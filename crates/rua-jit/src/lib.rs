//! The JIT: hot rua functions are lowered to Rust source with `quote`, checked
//! with `syn`, compiled by `rustc -O` into a cdylib, and dlopen'd back in.
//!
//! This crate sees only the AST: it takes a resolved [`FuncDef`] and hands back
//! a function pointer. Deciding *when* to compile is the runtime's business.
//!
//! It is a *method* JIT over the numeric subset of the language: every value in
//! generated code is an `f64`, booleans are 1.0/0.0, and anything outside that
//! subset (tables, strings, closures, calls to other rua functions) makes the
//! function fall back to the interpreter forever.

use rua_syntax::ast::*;
use proc_macro2::{Literal, TokenStream};
use quote::{format_ident, quote};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;

/// One argument or register in compiled code: a number, or a table.
///
/// `table` is null for a plain number. Tables are passed as the address of the
/// runtime's `RefCell<Table>`, which compiled code only ever reads, and only
/// through the hooks below.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RtArg {
    pub num: f64,
    pub table: *mut std::ffi::c_void,
}

impl RtArg {
    pub fn num(n: f64) -> RtArg {
        RtArg { num: n, table: std::ptr::null_mut() }
    }

    pub fn table(p: *mut std::ffi::c_void) -> RtArg {
        RtArg { num: 0.0, table: p }
    }
}

/// Another compiled function this one calls directly: where its code is, and
/// the context to run it with.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Callee {
    pub entry: usize,
    pub ctx: *const RtCtx,
}

/// Everything compiled code needs from the runtime, passed in as a pointer
/// rather than baked into the generated source as constants.
///
/// That indirection is what lets compiled code be cached on disk: the source
/// no longer contains addresses, which change with every run.
#[repr(C)]
pub struct RtCtx {
    /// `fn(table, ok) -> f64` — the array length of a table.
    pub len: usize,
    /// `fn(table, index, ok) -> f64` — a numeric element, or a trap.
    pub get: usize,
    /// `fn(table, len_out, ok) -> *const f64` — a direct view of the array
    /// part, so element reads need no call at all.
    pub span: usize,
    /// `fn(table, value)` — append a number to a table.
    pub push: usize,
    /// `fn(table, index, value)` — write a number already inside the array
    /// part. In place, so the view a caller holds stays valid.
    pub set: usize,
    /// The functions this one calls directly, in the order it refers to them.
    pub callees: *const Callee,
    /// The interpreter's call depth counter, which compiled code keeps up to
    /// date so that recursion hits the same limit either way.
    pub depth: *mut i64,
    pub max_depth: i64,
}

/// The hook addresses, before they are put in an [`RtCtx`].
#[derive(Clone, Copy, Debug, Default)]
pub struct RtHooks {
    pub len: usize,
    pub get: usize,
    pub span: usize,
    pub push: usize,
    pub set: usize,
}

/// What a slot holds, as far as compiled code is concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Num,
    /// A table compiled code reads, through a view of its array part.
    Table,
    /// A table compiled code only ever appends to.
    TableOut,
}

/// A compiled entry point: arguments in, one number out.
///
/// `ok` starts at 1. Compiled code sets it to 0 and returns early when it meets
/// something it cannot handle — a table element that is not a number, say — and
/// the runtime then re-runs the call in the interpreter. That is sound because
/// compiled code never writes anything.
pub type Entry = unsafe extern "C" fn(*const RtArg, *const RtCtx, *mut i32) -> f64;

#[derive(Clone, Copy)]
pub struct JitFn {
    entry: Entry,
    arity: usize,
}

impl JitFn {
    /// How many arguments this entry point takes.
    pub fn arity(&self) -> usize {
        self.arity
    }

    /// The raw code address, for generating a direct call to it.
    pub fn address(&self) -> usize {
        self.entry as *const () as usize
    }

    /// Run it. `None` means the compiled code trapped and the caller should
    /// fall back to the interpreter.
    ///
    /// # Safety
    /// `ctx` must be the context built for this entry point, with live hook
    /// addresses and callee table.
    pub unsafe fn call(&self, args: &[RtArg], ctx: *const RtCtx) -> Option<f64> {
        let mut ok: i32 = 1;
        // SAFETY: `args` has the arity this entry point was compiled for, and
        // every table pointer in it is live for the duration of the call.
        let out = (self.entry)(args.as_ptr(), ctx, &mut ok);
        if ok == 0 {
            None
        } else {
            Some(out)
        }
    }
}

/// How a function can refer to itself, as verified by the runtime just before
/// compilation. Without this, recursion would look like an ordinary call to
/// something the compiler knows nothing about.
#[derive(Default, Clone, Debug)]
pub struct SelfRef {
    /// The upvalue index holding this function, for a local `fn`.
    pub upval: Option<u16>,
    /// The global name holding this function, for a top level `fn`.
    pub global: Option<String>,
    /// Globals that currently hold an already compiled function: calls to
    /// these become direct calls to their machine code. The runtime promises
    /// to throw the result away if any of them is reassigned. The kinds are
    /// the callee's parameters — a direct call can only pass numbers.
    pub compiled_globals: HashMap<String, (usize, Vec<Kind>)>,
    /// Addresses of the runtime hooks compiled code calls for table reads.
    pub hooks: RtHooks,
}

/// What a compilation produced, and what it assumed.
pub struct Compiled {
    pub code: JitFn,
    /// The function has no value: its `f64` result is meaningless and the
    /// runtime should hand back nil, as the interpreter does.
    pub returns_nil: bool,
    /// Globals whose current value was compiled in as a direct call.
    pub inlined: Vec<String>,
    /// What each parameter has to be for the compiled code to apply.
    pub param_kinds: Vec<Kind>,
}

pub struct Jit {
    pub enabled: bool,
    pub threshold: u32,
    pub dump: bool,
    pub compiled: usize,
    pub bailed: usize,
    pub last_error: Option<String>,
    dir: PathBuf,
    /// Bumped per compilation, so two versions of one function never collide.
    generation: u64,
    /// Reuse compiled code left on disk by an earlier run.
    pub cache: bool,
    pub cache_hits: usize,
}

type Lower<T> = Result<T, String>;

impl Jit {
    pub fn new() -> Jit {
        let on = std::env::var("RUA_JIT").map(|v| v != "0" && v != "off").unwrap_or(true);
        let threshold = std::env::var("RUA_JIT_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50);
        // Compiled code is cached on disk by a hash of the Rust it came from,
        // so running the same script twice pays rustc once.
        let dir = std::env::var("RUA_JIT_DIR").map(PathBuf::from).unwrap_or_else(|_| {
            std::env::var("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".cache")))
                .unwrap_or_else(|_| std::env::temp_dir())
                .join("rua-jit")
        });
        Jit {
            enabled: on,
            threshold,
            dump: std::env::var("RUA_JIT_DUMP").map(|v| v != "0").unwrap_or(false),
            compiled: 0,
            bailed: 0,
            last_error: None,
            dir,
            generation: 0,
            cache: std::env::var("RUA_JIT_CACHE").map(|v| v != "0").unwrap_or(true),
            cache_hits: 0,
        }
    }

    /// Compile one resolved function, or explain why it cannot be compiled.
    /// `self_ref` carries what the runtime has verified about the function's
    /// own name and about the other compiled functions it can call directly.
    pub fn compile(&mut self, def: &FuncDef, self_ref: SelfRef) -> Result<Compiled, String> {
        let out = self.compile_inner(def, self_ref);
        match &out {
            Ok(_) => self.compiled += 1,
            Err(why) => {
                self.bailed += 1;
                self.last_error = Some(format!("{}: {why}", def.name));
                if self.dump {
                    eprintln!("[rua-jit] skipped {}: {why}", def.name);
                }
            }
        }
        out
    }

    fn compile_inner(&mut self, def: &FuncDef, self_ref: SelfRef) -> Lower<Compiled> {
        if def.params.len() > 4 {
            return Err("more than 4 parameters".into());
        }
        // A compiled function produces one number, or nothing at all — a
        // procedure like `fn fill(t, n) { ... }` is worth compiling too. What
        // it may not do is produce a number down one path and nil down another,
        // since the two are different values to the interpreter.
        let ends_with_return =
            matches!(def.body.stats.last(), Some(Stat::Return(v)) if v.len() == 1);
        let returns_nil = def.body.tail.is_none() && !ends_with_return;
        if returns_nil && returns_a_value(&def.body) {
            return Err("some paths return a value and some do not".into());
        }
        let symbol = format!("rua_jit_{}", def.id);
        // The file is named after a hash of its contents (see `build`), which
        // is both the cache key and what keeps dlopen — which caches by path —
        // honest when a function is recompiled into different code.
        let file_stem = symbol.clone();
        let (src, inlined, param_kinds) =
            self.lower_function(def, &symbol, self_ref, returns_nil)?;

        let addr = self.build(&file_stem, &symbol, &src, &def.name)?;
        // SAFETY: `build` returned the address of the `extern "C"` entry point
        // generated just above, which has exactly the `Entry` signature.
        let entry = unsafe { std::mem::transmute::<*const (), Entry>(addr) };
        let code = JitFn { entry, arity: def.params.len() };
        Ok(Compiled { code, inlined, param_kinds, returns_nil })
    }

    /// Compile one hot loop into a function over its live numeric locals.
    ///
    /// This is on-stack replacement in its simplest form: the generated code
    /// re-tests the loop condition, so the interpreter can hand over control at
    /// any iteration and let the loop run to completion natively.
    pub fn compile_loop(&mut self, st: &Stat, self_ref: SelfRef) -> Result<CompiledLoop, String> {
        let out = self.compile_loop_inner(st, self_ref);
        match &out {
            Ok(_) => self.compiled += 1,
            Err(why) => {
                self.bailed += 1;
                self.last_error = Some(format!("loop: {why}"));
                if self.dump {
                    eprintln!("[rua-jit] skipped a hot loop: {why}");
                }
            }
        }
        out
    }

    fn compile_loop_inner(&mut self, st: &Stat, self_ref: SelfRef) -> Lower<CompiledLoop> {
        if contains_return(st) {
            return Err("the loop returns from its function".into());
        }
        let slots = loop_slots(st)?;
        if slots.is_empty() {
            return Err("the loop touches no locals".into());
        }
        let mut wrapper = Block::default();
        wrapper.stats.push(st.clone());
        let kinds = infer_kinds(&wrapper, &self_ref.compiled_globals)?;
        let kind_list: Vec<Kind> =
            slots.iter().map(|s| kinds.get(s).copied().unwrap_or(Kind::Num)).collect();
        let mut cx = Ctx {
            known: slots.iter().copied().collect(),
            self_symbol: String::new(),
            self_ref,
            arity: usize::MAX, // there is no self call from a loop body
            inlined: Vec::new(),
            self_params_numeric: true,
            writes: table_usage(&wrapper).traps_forbidden(),
            kinds,
            in_range: Vec::new(),
            loop_labels: Vec::new(),
            labels: 0,
            on_trap: quote! { return; },
        };
        // mid-loop entry: the induction variable already holds its current
        // value, so a counted loop lowers to its `while` form without the init
        let body = match st {
            Stat::ForRange { binding, start, end, inclusive, body, .. } => {
                let b = binding.ok_or("unresolved `for`")?;
                // The interpreter evaluated the bound once, before the loop.
                // Taking the loop over mid-flight re-evaluates it, so it may
                // only depend on things the body leaves alone.
                if let Expr::Local(lb, name) = end {
                    if writes_slot(body, lb.slot) {
                        return Err(format!("the loop bound `{name}` changes in the body"));
                    }
                }
                if let Expr::Method(obj, _, _) = end {
                    if let Expr::Local(lb, name) = &**obj {
                        if writes_slot(body, lb.slot) {
                            return Err(format!("the loop bound `{name}` changes in the body"));
                        }
                    }
                }
                let id = ident(b.slot);
                let e = cx.expr(end)?;
                let fact = cx.range_fact(b, start, end, *inclusive, body);
                if let Some(f) = fact {
                    cx.in_range.push(f);
                }
                let label = cx.fresh_label();
                cx.loop_labels.push(Some(label.clone()));
                let inner = cx.block(body, false);
                cx.loop_labels.pop();
                if fact.is_some() {
                    cx.in_range.pop();
                }
                let inner = inner?;
                let test = if *inclusive {
                    quote! { #id <= __end }
                } else {
                    quote! { #id < __end }
                };
                quote! {
                    let __end: f64 = #e;
                    while #test {
                        #label: { #inner }
                        #id += 1.0;
                    }
                }
            }
            other => {
                cx.loop_labels.push(None);
                let out = cx.stat(other);
                cx.loop_labels.pop();
                out?
            }
        };
        self.generation += 1;
        let symbol = format!("rua_loop_{}", self.generation);
        let name = format_ident!("{}", symbol);
        let loads = slots.iter().zip(&kind_list).enumerate().map(|(i, (slot, kind))| {
            let id = ident(*slot);
            let idx = Literal::usize_suffixed(i);
            match kind {
                Kind::Num => quote! { let mut #id: f64 = (*regs.add(#idx)).num; },
                Kind::TableOut => quote! { let #id: *mut c_void = (*regs.add(#idx)).table; },
                Kind::Table => {
                    let (ptr, len) = span_idents(*slot);
                    quote! {
                        let #id: *mut c_void = (*regs.add(#idx)).table;
                        let mut #len: usize = 0;
                        let #ptr: *const f64 = rua_span(rt, #id, &mut #len, ok);
                        if *ok == 0 { return; }
                    }
                }
            }
        });
        let stores = slots.iter().zip(&kind_list).enumerate().filter_map(|(i, (slot, kind))| {
            // a table is mutated in place through the runtime, so only numbers
            // have to travel back into the registers
            match kind {
                Kind::Num => {
                    let id = ident(*slot);
                    let i = Literal::usize_suffixed(i);
                    Some(quote! { (*regs.add(#i)).num = #id; })
                }
                Kind::Table | Kind::TableOut => None,
            }
        });
        let preamble = preamble();
        let file = quote! {
            #preamble

            /// # Safety
            /// `regs` points at one `RtArg` per local this loop uses, `rt` at
            /// the context built for it, and `ok` at an `i32` that starts
            /// non-zero.
            #[no_mangle]
            pub unsafe extern "C" fn #name(
                regs: *mut RtArg,
                rt: *const RtCtx,
                ok: *mut i32,
            ) {
                #(#loads)*
                #body
                #(#stores)*
            }
        };
        let parsed: syn::File =
            syn::parse2(file).map_err(|e| format!("generated Rust did not parse: {e}"))?;
        let src = prettyplease::unparse(&parsed);
        let code = self.build(&symbol, &symbol, &src, "a hot loop")?;
        // SAFETY: `build` returned the address of the `extern "C"` symbol above.
        let code: LoopFn = unsafe { std::mem::transmute::<*const (), LoopFn>(code) };
        Ok(CompiledLoop { code, slots, kinds: kind_list, inlined: cx.inlined })
    }

    /// Write the source out, run rustc over it, and dlopen the result.
    ///
    /// `stem` names the files and `symbol` names the entry point: they differ
    /// because recompiling a function keeps its symbol but must produce a file
    /// dlopen has never seen, since dlopen caches by path.
    fn build(&mut self, stem: &str, symbol: &str, src: &str, what: &str) -> Lower<*const ()> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        // the hash makes the file unique per generated source, which both keys
        // the cache and keeps dlopen (which caches by path) honest
        let stem = format!("{stem}_{:016x}", source_hash(src));
        // another process may be compiling the same function into this shared
        // directory, so the input file is private too
        let rs = self.dir.join(format!("{stem}_p{}.rs", std::process::id()));
        let so = self.dir.join(format!("lib{stem}.so"));
        if self.dump {
            eprintln!("[rua-jit] {what} ->\n{src}");
        }
        if so.exists() && self.cache {
            self.cache_hits += 1;
        } else {
            std::fs::write(&rs, src).map_err(|e| e.to_string())?;
            // compile to a private path and rename: another process (or an
            // interrupted one) must never leave a half written library behind
            let tmp = self.dir.join(format!("lib{stem}.{}.tmp", std::process::id()));
            let out = Command::new("rustc")
                .args([
                    "--edition", "2021", "-O", "-C", "debuginfo=0", "--crate-type", "cdylib",
                    // the file name carries a hash and a pid, which is not a
                    // legal crate name
                    "--crate-name", "rua_jit_unit", "-o",
                ])
                .arg(&tmp)
                .arg(&rs)
                .output()
                .map_err(|e| format!("cannot run rustc: {e}"))?;
            if !out.status.success() {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!(
                    "rustc failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            std::fs::rename(&tmp, &so).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&rs);
        }
        // SAFETY: a cdylib we just produced, whose symbol we just named. The
        // handle is leaked so the code pages outlive every call site.
        unsafe {
            let lib = libloading::Library::new(&so).map_err(|e| e.to_string())?;
            let sym: libloading::Symbol<*const ()> =
                lib.get(symbol.as_bytes()).map_err(|e| e.to_string())?;
            let addr = sym.try_as_raw_ptr().ok_or("the symbol has no address")? as *const ();
            std::mem::forget(lib);
            Ok(addr)
        }
    }

    fn lower_function(
        &self,
        def: &FuncDef,
        symbol: &str,
        self_ref: SelfRef,
        returns_nil: bool,
    ) -> Lower<(String, Vec<String>, Vec<Kind>)> {
        if def.param_bindings.iter().any(|b| b.cell) {
            return Err("a parameter is captured by a closure".into());
        }
        let kinds = infer_kinds(&def.body, &self_ref.compiled_globals)?;
        // only parameters may be tables: a local always holds a number
        for (slot, kind) in &kinds {
            if *kind == Kind::Table
                && !def.param_bindings.iter().any(|b| b.slot == *slot)
            {
                return Err("a table is held in a local, not a parameter".into());
            }
        }
        let param_kinds: Vec<Kind> = def
            .param_bindings
            .iter()
            .map(|b| kinds.get(&b.slot).copied().unwrap_or(Kind::Num))
            .collect();
        let mut cx = Ctx {
            known: def.param_bindings.iter().map(|b| b.slot).collect(),
            self_symbol: symbol.to_string(),
            self_ref,
            arity: def.params.len(),
            inlined: Vec::new(),
            self_params_numeric: param_kinds.iter().all(|k| *k == Kind::Num),
            writes: table_usage(&def.body).traps_forbidden(),
            kinds,
            in_range: Vec::new(),
            loop_labels: Vec::new(),
            labels: 0,
            on_trap: quote! { return 0.0; },
        };
        let body = if returns_nil {
            let inner = cx.block(&def.body, false)?;
            quote! { { #inner 0.0 } }
        } else {
            cx.block(&def.body, true)?
        };
        // unpack the argument array into locals of the right kind
        let prologue = def.param_bindings.iter().enumerate().map(|(i, b)| {
            let id = ident(b.slot);
            let idx = Literal::usize_suffixed(i);
            match cx.kinds.get(&b.slot).copied().unwrap_or(Kind::Num) {
                Kind::Num => quote! { let mut #id: f64 = (*args.add(#idx)).num; },
                Kind::TableOut => quote! { let #id: *mut c_void = (*args.add(#idx)).table; },
                Kind::Table => {
                    let (ptr, len) = span_idents(b.slot);
                    quote! {
                        let #id: *mut c_void = (*args.add(#idx)).table;
                        let mut #len: usize = 0;
                        let #ptr: *const f64 = rua_span(rt, #id, &mut #len, ok);
                        if *ok == 0 { return 0.0; }
                    }
                }
            }
        });
        let name = format_ident!("{}", symbol);
        let preamble = preamble();
        let file = quote! {
            #preamble

            /// # Safety
            /// `args` points at one `RtArg` per parameter, `rt` at the context
            /// built for this function, and `ok` at an `i32` that starts
            /// non-zero.
            #[no_mangle]
            pub unsafe extern "C" fn #name(
                args: *const RtArg,
                rt: *const RtCtx,
                ok: *mut i32,
            ) -> f64 {
                // Recursion here is real machine recursion, so it has to
                // respect the interpreter's limit rather than run the process
                // out of stack. Tripping it before anything else happens keeps
                // the trap safe: nothing has been written yet.
                let __depth = (*rt).depth;
                *__depth += 1;
                if *__depth > (*rt).max_depth {
                    *__depth -= 1;
                    *ok = 0;
                    return 0.0;
                }
                let __out = (|| -> f64 {
                    #(#prologue)*
                    #body
                })();
                *__depth -= 1;
                __out
            }
        };
        let parsed: syn::File =
            syn::parse2(file).map_err(|e| format!("generated Rust did not parse: {e}"))?;
        Ok((prettyplease::unparse(&parsed), cx.inlined, param_kinds))
    }
}

impl Default for Jit {
    fn default() -> Self {
        Jit::new()
    }
}

/// A stable hash of the generated source, used to name (and so to cache) the
/// shared object it compiles to.
fn source_hash(src: &str) -> u64 {
    // FNV-1a: tiny, deterministic across runs, and good enough to name a file
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in src.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// A compiled loop: it reads and writes the numeric locals it touches through
/// a register array, so the interpreter can jump into it mid-flight and pick
/// the values back up when it returns.
/// The pointer must address one [`RtArg`] per slot in [`CompiledLoop::slots`],
/// which is why calling this is unsafe. As with [`Entry`], `ok` going to 0
/// means the loop trapped and the interpreter should take over again.
pub type LoopFn = unsafe extern "C" fn(*mut RtArg, *const RtCtx, *mut i32);

/// A compiled loop plus the frame slots its register array mirrors.
pub struct CompiledLoop {
    pub code: LoopFn,
    pub slots: Vec<u16>,
    /// What each slot has to hold for the compiled code to be usable.
    pub kinds: Vec<Kind>,
    /// Globals it calls directly, in callee-table order.
    pub inlined: Vec<String>,
}

/// The globals this function calls. The runtime uses this to compile callees
/// first: a direct call can only be generated to code that already exists.
pub fn called_globals(def: &FuncDef) -> Vec<String> {
    let mut out = Vec::new();
    walk_block(&def.body, &mut out);
    out
}

fn walk_block(b: &Block, out: &mut Vec<String>) {
    for st in &b.stats {
        walk_stat(st, out);
    }
    if let Some(t) = &b.tail {
        walk_expr(t, out);
    }
}

fn walk_stat(st: &Stat, out: &mut Vec<String>) {
    match st {
        Stat::Let(_, es) | Stat::LetSlots(_, es) | Stat::Return(es) => {
            es.iter().for_each(|e| walk_expr(e, out))
        }
        Stat::FnDecl(_, e) | Stat::FnSlot(_, e) | Stat::Expr(e) => walk_expr(e, out),
        Stat::Assign(ts, es) => ts.iter().chain(es).for_each(|e| walk_expr(e, out)),
        Stat::OpAssign(t, _, e) => {
            walk_expr(t, out);
            walk_expr(e, out);
        }
        Stat::While(_, c, b) => {
            walk_expr(c, out);
            walk_block(b, out);
        }
        Stat::Loop(_, b) => walk_block(b, out),
        Stat::ForRange { start, end, body, .. } => {
            walk_expr(start, out);
            walk_expr(end, out);
            walk_block(body, out);
        }
        Stat::ForIn { iter, body, .. } => {
            walk_expr(iter, out);
            walk_block(body, out);
        }
        Stat::Break | Stat::Continue => {}
    }
}

fn walk_expr(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Call(f, args) => {
            if let Expr::Global(name, _) = &**f {
                out.push(name.to_string());
            }
            walk_expr(f, out);
            args.iter().for_each(|a| walk_expr(a, out));
        }
        Expr::Method(o, _, args) => {
            walk_expr(o, out);
            args.iter().for_each(|a| walk_expr(a, out));
        }
        Expr::Index(a, b) | Expr::Bin(_, a, b) | Expr::Range(a, b, _) => {
            walk_expr(a, out);
            walk_expr(b, out);
        }
        Expr::Un(_, a) => walk_expr(a, out),
        Expr::Array(items) => items.iter().for_each(|i| walk_expr(i, out)),
        Expr::Map(items) => items.iter().for_each(|(k, v)| {
            walk_expr(k, out);
            walk_expr(v, out);
        }),
        Expr::If(arms, els) => {
            for (c, b) in arms {
                walk_expr(c, out);
                walk_block(b, out);
            }
            if let Some(b) = els {
                walk_block(b, out);
            }
        }
        Expr::Match(subject, arms) => {
            walk_expr(subject, out);
            for arm in arms {
                for p in &arm.patterns {
                    if let Pattern::Lit(e) = p {
                        walk_expr(e, out);
                    }
                }
                if let Some(g) = &arm.guard {
                    walk_expr(g, out);
                }
                walk_block(&arm.body, out);
            }
        }
        Expr::Do(b) => walk_block(b, out),
        Expr::Func(_)
        | Expr::Var(_)
        | Expr::Local(..)
        | Expr::Upval(..)
        | Expr::Global(..)
        | Expr::Nil
        | Expr::Bool(_)
        | Expr::Num(_)
        | Expr::Str(_) => {}
    }
}

/// Work out what each slot holds: a table if it is indexed or asked for its
/// length, a number otherwise. Disagreement means the JIT stays out of it.
fn infer_kinds(
    b: &Block,
    callees: &HashMap<String, (usize, Vec<Kind>)>,
) -> Result<HashMap<u16, Kind>, String> {
    let mut kinds = HashMap::new();
    let mut bad = None;
    // a local passed straight to a compiled function takes that parameter's
    // kind, which is how a table reaches a helper
    kinds_block(b, &mut kinds, &mut bad, callees);
    match bad {
        Some(why) => Err(why),
        None => Ok(kinds),
    }
}

fn note(slot: u16, kind: Kind, kinds: &mut HashMap<u16, Kind>, bad: &mut Option<String>) {
    match kinds.insert(slot, kind) {
        Some(old) if old != kind => {
            *bad = Some(format!("a local is used as both {old:?} and {kind:?}"))
        }
        _ => {}
    }
}

/// What the inference walk needs to know about the functions being called.
type Callees = HashMap<String, (usize, Vec<Kind>)>;

fn kinds_block(
    b: &Block,
    kinds: &mut HashMap<u16, Kind>,
    bad: &mut Option<String>,
    callees: &Callees,
) {
    for st in &b.stats {
        kinds_stat(st, kinds, bad, callees);
    }
    if let Some(t) = &b.tail {
        kinds_expr(t, kinds, bad, callees);
    }
}

fn kinds_stat(
    st: &Stat,
    kinds: &mut HashMap<u16, Kind>,
    bad: &mut Option<String>,
    callees: &Callees,
) {
    match st {
        Stat::Let(_, es) | Stat::Return(es) => es.iter().for_each(|e| kinds_expr(e, kinds, bad, callees)),
        Stat::LetSlots(bs, es) => {
            for b in bs {
                note(b.slot, Kind::Num, kinds, bad);
            }
            es.iter().for_each(|e| kinds_expr(e, kinds, bad, callees));
        }
        Stat::FnDecl(_, e) | Stat::FnSlot(_, e) | Stat::Expr(e) => kinds_expr(e, kinds, bad, callees),
        Stat::Assign(ts, es) => {
            for t in ts {
                // `t[i] = v` writes in place, which the view survives, so the
                // table is still read through a span
                if let Expr::Index(obj, key) = t {
                    if let Expr::Local(b, _) = &**obj {
                        note(b.slot, Kind::Table, kinds, bad);
                        kinds_expr(key, kinds, bad, callees);
                        continue;
                    }
                }
                kinds_expr(t, kinds, bad, callees);
            }
            es.iter().for_each(|e| kinds_expr(e, kinds, bad, callees));
        }
        Stat::OpAssign(t, _, e) => {
            kinds_expr(t, kinds, bad, callees);
            kinds_expr(e, kinds, bad, callees);
        }
        Stat::While(_, c, b) => {
            kinds_expr(c, kinds, bad, callees);
            kinds_block(b, kinds, bad, callees);
        }
        Stat::Loop(_, b) => kinds_block(b, kinds, bad, callees),
        Stat::ForRange { binding, start, end, body, .. } => {
            if let Some(b) = binding {
                note(b.slot, Kind::Num, kinds, bad);
            }
            kinds_expr(start, kinds, bad, callees);
            kinds_expr(end, kinds, bad, callees);
            kinds_block(body, kinds, bad, callees);
        }
        Stat::ForIn { body, iter, .. } => {
            kinds_expr(iter, kinds, bad, callees);
            kinds_block(body, kinds, bad, callees);
        }
        Stat::Break | Stat::Continue => {}
    }
}

fn kinds_expr(
    e: &Expr,
    kinds: &mut HashMap<u16, Kind>,
    bad: &mut Option<String>,
    callees: &Callees,
) {
    match e {
        // `t[i]` and `t.len()` are what make a slot a table
        Expr::Index(obj, key) => {
            if let Expr::Local(b, _) = &**obj {
                note(b.slot, Kind::Table, kinds, bad);
            } else {
                kinds_expr(obj, kinds, bad, callees);
            }
            kinds_expr(key, kinds, bad, callees);
        }
        Expr::Method(obj, name, args) => {
            match (&**obj, &**name) {
                // `t.len()` works for a table either way, so it says nothing
                // about which kind this is
                (Expr::Local(_, _), "len") => {}
                (Expr::Local(b, _), "push") => note(b.slot, Kind::TableOut, kinds, bad),
                _ => kinds_expr(obj, kinds, bad, callees),
            }
            args.iter().for_each(|a| kinds_expr(a, kinds, bad, callees));
        }
        Expr::Local(b, _) => note(b.slot, Kind::Num, kinds, bad),
        Expr::Bin(_, a, b) | Expr::Range(a, b, _) => {
            kinds_expr(a, kinds, bad, callees);
            kinds_expr(b, kinds, bad, callees);
        }
        Expr::Un(_, a) => kinds_expr(a, kinds, bad, callees),
        Expr::Call(f, args) => {
            // an argument handed to a compiled function takes that
            // parameter's kind: that is how a table reaches a helper
            if let Expr::Global(name, _) = &**f {
                if let Some((_, param_kinds)) = callees.get(&**name) {
                    if param_kinds.len() == args.len() {
                        for (a, kind) in args.iter().zip(param_kinds) {
                            match (a, kind) {
                                (Expr::Local(b, _), Kind::Table | Kind::TableOut) => {
                                    note(b.slot, *kind, kinds, bad)
                                }
                                _ => kinds_expr(a, kinds, bad, callees),
                            }
                        }
                        return;
                    }
                }
            }
            kinds_expr(f, kinds, bad, callees);
            args.iter().for_each(|a| kinds_expr(a, kinds, bad, callees));
        }
        Expr::Array(items) => items.iter().for_each(|i| kinds_expr(i, kinds, bad, callees)),
        Expr::Map(items) => items.iter().for_each(|(k, v)| {
            kinds_expr(k, kinds, bad, callees);
            kinds_expr(v, kinds, bad, callees);
        }),
        Expr::If(arms, els) => {
            for (c, b) in arms {
                kinds_expr(c, kinds, bad, callees);
                kinds_block(b, kinds, bad, callees);
            }
            if let Some(b) = els {
                kinds_block(b, kinds, bad, callees);
            }
        }
        Expr::Match(subject, arms) => {
            kinds_expr(subject, kinds, bad, callees);
            for arm in arms {
                for p in &arm.patterns {
                    match p {
                        Pattern::Lit(e) => kinds_expr(e, kinds, bad, callees),
                        Pattern::Bind(_, Some(b)) => note(b.slot, Kind::Num, kinds, bad),
                        _ => {}
                    }
                }
                if let Some(g) = &arm.guard {
                    kinds_expr(g, kinds, bad, callees);
                }
                kinds_block(&arm.body, kinds, bad, callees);
            }
        }
        Expr::Do(b) => kinds_block(b, kinds, bad, callees),
        Expr::Func(_) | Expr::Upval(..) | Expr::Global(..) | Expr::Var(_) | Expr::Nil
        | Expr::Bool(_) | Expr::Num(_) | Expr::Str(_) => {}
    }
}

/// The frame slots a loop reads or writes. A captured local (a cell) means a
/// closure shares it, which the numeric subset cannot express.
fn loop_slots(st: &Stat) -> Result<Vec<u16>, String> {
    let mut out: Vec<u16> = Vec::new();
    let mut bad: Option<String> = None;
    collect_slots_stat(st, &mut out, &mut bad);
    match bad {
        Some(why) => Err(why),
        None => {
            out.sort_unstable();
            out.dedup();
            Ok(out)
        }
    }
}

fn collect_slots_stat(st: &Stat, out: &mut Vec<u16>, bad: &mut Option<String>) {
    match st {
        Stat::Let(_, es) | Stat::Return(es) => es.iter().for_each(|e| collect_slots(e, out, bad)),
        Stat::LetSlots(bs, es) => {
            for b in bs {
                if b.cell {
                    *bad = Some("a local in the loop is captured by a closure".into());
                }
                out.push(b.slot);
            }
            es.iter().for_each(|e| collect_slots(e, out, bad));
        }
        Stat::FnDecl(..) | Stat::FnSlot(..) => {
            *bad = Some("the loop defines a function".into());
        }
        Stat::Expr(e) => collect_slots(e, out, bad),
        Stat::Assign(ts, es) => ts.iter().chain(es).for_each(|e| collect_slots(e, out, bad)),
        Stat::OpAssign(t, _, e) => {
            collect_slots(t, out, bad);
            collect_slots(e, out, bad);
        }
        Stat::While(_, c, b) => {
            collect_slots(c, out, bad);
            collect_slots_block(b, out, bad);
        }
        Stat::Loop(_, b) => collect_slots_block(b, out, bad),
        Stat::ForRange { binding, start, end, body, .. } => {
            if let Some(b) = binding {
                if b.cell {
                    *bad = Some("the loop variable is captured by a closure".into());
                }
                out.push(b.slot);
            }
            collect_slots(start, out, bad);
            collect_slots(end, out, bad);
            collect_slots_block(body, out, bad);
        }
        Stat::ForIn { .. } => *bad = Some("`for ... in` over an iterator".into()),
        Stat::Break | Stat::Continue => {}
    }
}

fn collect_slots_block(b: &Block, out: &mut Vec<u16>, bad: &mut Option<String>) {
    for st in &b.stats {
        collect_slots_stat(st, out, bad);
    }
    if let Some(t) = &b.tail {
        collect_slots(t, out, bad);
    }
}

fn collect_slots(e: &Expr, out: &mut Vec<u16>, bad: &mut Option<String>) {
    match e {
        Expr::Local(b, name) => {
            if b.cell {
                *bad = Some(format!("`{name}` is captured by a closure"));
            }
            out.push(b.slot);
        }
        Expr::Func(_) => *bad = Some("the loop makes a closure".into()),
        Expr::Index(a, b) | Expr::Bin(_, a, b) | Expr::Range(a, b, _) => {
            collect_slots(a, out, bad);
            collect_slots(b, out, bad);
        }
        Expr::Un(_, a) => collect_slots(a, out, bad),
        Expr::Call(f, args) => {
            collect_slots(f, out, bad);
            args.iter().for_each(|a| collect_slots(a, out, bad));
        }
        Expr::Method(o, _, args) => {
            collect_slots(o, out, bad);
            args.iter().for_each(|a| collect_slots(a, out, bad));
        }
        Expr::Array(items) => items.iter().for_each(|i| collect_slots(i, out, bad)),
        Expr::Map(items) => items.iter().for_each(|(k, v)| {
            collect_slots(k, out, bad);
            collect_slots(v, out, bad);
        }),
        Expr::If(arms, els) => {
            for (c, b) in arms {
                collect_slots(c, out, bad);
                collect_slots_block(b, out, bad);
            }
            if let Some(b) = els {
                collect_slots_block(b, out, bad);
            }
        }
        Expr::Match(subject, arms) => {
            collect_slots(subject, out, bad);
            for arm in arms {
                for p in &arm.patterns {
                    match p {
                        Pattern::Lit(e) => collect_slots(e, out, bad),
                        Pattern::Bind(name, Some(b)) => {
                            if b.cell {
                                *bad = Some(format!("`{name}` is captured by a closure"));
                            }
                            out.push(b.slot);
                        }
                        _ => {}
                    }
                }
                if let Some(g) = &arm.guard {
                    collect_slots(g, out, bad);
                }
                collect_slots_block(&arm.body, out, bad);
            }
        }
        Expr::Do(b) => collect_slots_block(b, out, bad),
        Expr::Upval(..) | Expr::Global(..) | Expr::Var(_) | Expr::Nil | Expr::Bool(_)
        | Expr::Num(_) | Expr::Str(_) => {}
    }
}

/// A `return` inside a compiled loop would have to unwind the interpreter, so
/// loops containing one stay interpreted.
fn contains_return(st: &Stat) -> bool {
    let mut found = false;
    fn scan_stat(st: &Stat, found: &mut bool) {
        match st {
            Stat::Return(_) => *found = true,
            Stat::While(_, _, b) | Stat::Loop(_, b) => scan_block(b, found),
            Stat::ForRange { body, .. } | Stat::ForIn { body, .. } => scan_block(body, found),
            Stat::Expr(e) => scan_expr(e, found),
            _ => {}
        }
    }
    fn scan_block(b: &Block, found: &mut bool) {
        for st in &b.stats {
            scan_stat(st, found);
        }
        if let Some(t) = &b.tail {
            scan_expr(t, found);
        }
    }
    fn scan_expr(e: &Expr, found: &mut bool) {
        match e {
            Expr::If(arms, els) => {
                for (_, b) in arms {
                    scan_block(b, found);
                }
                if let Some(b) = els {
                    scan_block(b, found);
                }
            }
            Expr::Do(b) => scan_block(b, found),
            _ => {}
        }
    }
    scan_stat(st, &mut found);
    found
}

/// Everything the generated file needs before its entry point: the argument
/// type, the two arithmetic helpers, and thunks for the runtime hooks, whose
/// addresses are baked in as constants.
fn preamble() -> TokenStream {
    quote! {
        #![allow(
            unused_mut,
            unused_parens,
            unused_variables,
            unused_assignments,
            unreachable_code,
            dead_code
        )]

        use std::ffi::c_void;

        #[repr(C)]
        #[derive(Clone, Copy)]
        pub struct RtArg {
            pub num: f64,
            pub table: *mut c_void,
        }

        #[repr(C)]
        #[derive(Clone, Copy)]
        pub struct Callee {
            pub entry: usize,
            pub ctx: *const RtCtx,
        }

        #[repr(C)]
        pub struct RtCtx {
            pub len: usize,
            pub get: usize,
            pub span: usize,
            pub push: usize,
            pub set: usize,
            pub callees: *const Callee,
            pub depth: *mut i64,
            pub max_depth: i64,
        }

        #[inline(always)]
        fn rua_rem(a: f64, b: f64) -> f64 {
            a - (a / b).floor() * b
        }

        #[inline(always)]
        fn rua_bool(b: bool) -> f64 {
            if b { 1.0 } else { 0.0 }
        }

        /// # Safety
        /// `t` is a live table pointer, and `rt` the context we were called
        /// with.
        #[inline(always)]
        unsafe fn rua_len(rt: *const RtCtx, t: *mut c_void, ok: *mut i32) -> f64 {
            let f: unsafe extern "C" fn(*mut c_void, *mut i32) -> f64 =
                std::mem::transmute((*rt).len as *const ());
            f(t, ok)
        }

        /// # Safety
        /// As `rua_len`.
        #[inline(always)]
        unsafe fn rua_get(rt: *const RtCtx, t: *mut c_void, i: f64, ok: *mut i32) -> f64 {
            let f: unsafe extern "C" fn(*mut c_void, f64, *mut i32) -> f64 =
                std::mem::transmute((*rt).get as *const ());
            f(t, i, ok)
        }

        /// # Safety
        /// `t` is a live table pointer, and `rt` the context we were called
        /// with.
        #[inline(always)]
        unsafe fn rua_push(rt: *const RtCtx, t: *mut c_void, v: f64) {
            let f: unsafe extern "C" fn(*mut c_void, f64) = 
                std::mem::transmute((*rt).push as *const ());
            f(t, v)
        }

        /// # Safety
        /// `t` is a live table pointer, `i` an index inside its array part.
        #[inline(always)]
        unsafe fn rua_set(rt: *const RtCtx, t: *mut c_void, i: f64, v: f64, ok: *mut i32) {
            let f: unsafe extern "C" fn(*mut c_void, f64, f64, *mut i32) =
                std::mem::transmute((*rt).set as *const ());
            f(t, i, v, ok)
        }

        /// # Safety
        /// As `rua_len`. The view a read table hands out stays valid as long as
        /// nothing writes to *that* table, which the runtime checks before it
        /// uses this code.
        #[inline(always)]
        unsafe fn rua_span(
            rt: *const RtCtx,
            t: *mut c_void,
            len: *mut usize,
            ok: *mut i32,
        ) -> *const f64 {
            let f: unsafe extern "C" fn(*mut c_void, *mut usize, *mut i32) -> *const f64 =
                std::mem::transmute((*rt).span as *const ());
            f(t, len, ok)
        }
    }
}

/// Lowering context: which frame slots are in scope as plain f64 locals.
struct Ctx {
    known: HashSet<u16>,
    self_symbol: String,
    /// How this function refers to itself, if it does.
    self_ref: SelfRef,
    /// Globals compiled in as direct calls.
    inlined: Vec<String>,
    arity: usize,
    /// Whether every parameter of this function is a number, which is what a
    /// direct self call is able to pass.
    self_params_numeric: bool,
    /// What each slot holds: a number, or a table reached through the hooks.
    kinds: HashMap<u16, Kind>,
    /// True when this code appends to a table. Once it has written something,
    /// trapping back to the interpreter would run those writes twice, so every
    /// read has to be provably in range instead.
    writes: bool,
    /// Loop variables known to be a valid index into a given table, from
    /// `for i in 0..t.len()`.
    in_range: Vec<(u16, u16)>,
    /// For each enclosing loop, the label to break to for `continue`. A counted
    /// loop wraps its body in a labeled block so that `continue` still runs the
    /// increment; a `while` needs no label.
    loop_labels: Vec<Option<syn::Lifetime>>,
    labels: usize,
    /// How to leave this entry point when a table read traps. Functions return
    /// a number; loops return nothing.
    on_trap: TokenStream,
}

/// Does any path in this block return a value? A procedure may contain bare
/// `return`s, but not a mix of the two.
fn returns_a_value(b: &Block) -> bool {
    let mut found = false;
    value_returns_block(b, &mut found);
    found
}

fn value_returns_block(b: &Block, found: &mut bool) {
    for st in &b.stats {
        match st {
            Stat::Return(es) => {
                if !es.is_empty() {
                    *found = true;
                }
            }
            Stat::While(_, _, b) | Stat::Loop(_, b) => value_returns_block(b, found),
            Stat::ForRange { body, .. } | Stat::ForIn { body, .. } => {
                value_returns_block(body, found)
            }
            Stat::Expr(e) => value_returns_expr(e, found),
            _ => {}
        }
    }
    if let Some(t) = &b.tail {
        value_returns_expr(t, found);
    }
}

fn value_returns_expr(e: &Expr, found: &mut bool) {
    match e {
        Expr::Do(b) => value_returns_block(b, found),
        Expr::If(arms, els) => {
            for (_, b) in arms {
                value_returns_block(b, found);
            }
            if let Some(b) = els {
                value_returns_block(b, found);
            }
        }
        Expr::Match(_, arms) => arms.iter().for_each(|a| value_returns_block(&a.body, found)),
        _ => {}
    }
}

/// How this code uses tables, which decides whether it may trap.
///
/// A trap re-runs the whole call in the interpreter, so it is only safe while
/// re-running would produce the same result. Writing an element in place is
/// fine: the re-run recomputes the same value and writes it again — *unless*
/// the code also reads that table, in which case the partial writes would feed
/// back into the recomputation. Appending is never fine, because the re-run
/// would append a second time.
#[derive(Default)]
struct TableUse {
    read: HashSet<u16>,
    written: HashSet<u16>,
    pushes: bool,
}

impl TableUse {
    /// May compiled code bail out to the interpreter part way through?
    fn traps_forbidden(&self) -> bool {
        self.pushes || self.read.intersection(&self.written).next().is_some()
    }
}

fn table_usage(b: &Block) -> TableUse {
    let mut use_ = TableUse::default();
    usage_block(b, &mut use_);
    use_
}

fn usage_block(b: &Block, u: &mut TableUse) {
    for st in &b.stats {
        usage_stat(st, u);
    }
    if let Some(t) = &b.tail {
        usage_expr(t, u);
    }
}

fn usage_stat(st: &Stat, u: &mut TableUse) {
    match st {
        Stat::Assign(ts, es) => {
            for t in ts {
                if let Expr::Index(obj, key) = t {
                    if let Expr::Local(b, _) = &**obj {
                        u.written.insert(b.slot);
                    }
                    usage_expr(key, u);
                } else {
                    usage_expr(t, u);
                }
            }
            es.iter().for_each(|e| usage_expr(e, u));
        }
        Stat::OpAssign(t, _, e) => {
            // `t[i] += v` both reads and writes
            if let Expr::Index(obj, key) = t {
                if let Expr::Local(b, _) = &**obj {
                    u.written.insert(b.slot);
                    u.read.insert(b.slot);
                }
                usage_expr(key, u);
            } else {
                usage_expr(t, u);
            }
            usage_expr(e, u);
        }
        Stat::LetSlots(_, es) | Stat::Let(_, es) | Stat::Return(es) => {
            es.iter().for_each(|e| usage_expr(e, u))
        }
        Stat::FnDecl(_, e) | Stat::FnSlot(_, e) | Stat::Expr(e) => usage_expr(e, u),
        Stat::While(_, c, b) => {
            usage_expr(c, u);
            usage_block(b, u);
        }
        Stat::Loop(_, b) => usage_block(b, u),
        Stat::ForRange { start, end, body, .. } => {
            usage_expr(start, u);
            usage_expr(end, u);
            usage_block(body, u);
        }
        Stat::ForIn { iter, body, .. } => {
            usage_expr(iter, u);
            usage_block(body, u);
        }
        Stat::Break | Stat::Continue => {}
    }
}

fn usage_expr(e: &Expr, u: &mut TableUse) {
    match e {
        Expr::Index(obj, key) => {
            if let Expr::Local(b, _) = &**obj {
                u.read.insert(b.slot);
            } else {
                usage_expr(obj, u);
            }
            usage_expr(key, u);
        }
        Expr::Method(obj, name, args) => {
            if let Expr::Local(b, _) = &**obj {
                match &**name {
                    "push" => {
                        u.pushes = true;
                        u.written.insert(b.slot);
                    }
                    "len" => {}
                    _ => {}
                }
            } else {
                usage_expr(obj, u);
            }
            args.iter().for_each(|a| usage_expr(a, u));
        }
        Expr::Bin(_, a, b) | Expr::Range(a, b, _) => {
            usage_expr(a, u);
            usage_expr(b, u);
        }
        Expr::Un(_, a) => usage_expr(a, u),
        Expr::Call(f, args) => {
            usage_expr(f, u);
            args.iter().for_each(|a| usage_expr(a, u));
        }
        Expr::Array(items) => items.iter().for_each(|i| usage_expr(i, u)),
        Expr::Map(items) => items.iter().for_each(|(k, v)| {
            usage_expr(k, u);
            usage_expr(v, u);
        }),
        Expr::If(arms, els) => {
            for (c, b) in arms {
                usage_expr(c, u);
                usage_block(b, u);
            }
            if let Some(b) = els {
                usage_block(b, u);
            }
        }
        Expr::Match(subject, arms) => {
            usage_expr(subject, u);
            for a in arms {
                if let Some(g) = &a.guard {
                    usage_expr(g, u);
                }
                usage_block(&a.body, u);
            }
        }
        Expr::Do(b) => usage_block(b, u),
        _ => {}
    }
}

/// Does this block assign to a given frame slot anywhere inside it? A proof
/// that an index stays in range is only worth anything if nothing moves it.
fn writes_slot(b: &Block, slot: u16) -> bool {
    b.stats.iter().any(|st| writes_slot_stat(st, slot))
        || b.tail.as_deref().map(|e| writes_slot_expr(e, slot)).unwrap_or(false)
}

fn writes_slot_stat(st: &Stat, slot: u16) -> bool {
    let target_is = |e: &Expr| matches!(e, Expr::Local(b, _) if b.slot == slot);
    match st {
        Stat::LetSlots(bs, es) => {
            bs.iter().any(|b| b.slot == slot) || es.iter().any(|e| writes_slot_expr(e, slot))
        }
        Stat::Let(_, es) | Stat::Return(es) => es.iter().any(|e| writes_slot_expr(e, slot)),
        Stat::FnSlot(b, e) => b.slot == slot || writes_slot_expr(e, slot),
        Stat::FnDecl(_, e) | Stat::Expr(e) => writes_slot_expr(e, slot),
        Stat::Assign(ts, es) => {
            ts.iter().any(target_is) || es.iter().any(|e| writes_slot_expr(e, slot))
        }
        Stat::OpAssign(t, _, e) => target_is(t) || writes_slot_expr(e, slot),
        Stat::While(_, c, b) => writes_slot_expr(c, slot) || writes_slot(b, slot),
        Stat::Loop(_, b) => writes_slot(b, slot),
        Stat::ForRange { binding, start, end, body, .. } => {
            binding.map(|b| b.slot == slot).unwrap_or(false)
                || writes_slot_expr(start, slot)
                || writes_slot_expr(end, slot)
                || writes_slot(body, slot)
        }
        Stat::ForIn { bindings, iter, body, .. } => {
            bindings.iter().any(|b| b.slot == slot)
                || writes_slot_expr(iter, slot)
                || writes_slot(body, slot)
        }
        Stat::Break | Stat::Continue => false,
    }
}

fn writes_slot_expr(e: &Expr, slot: u16) -> bool {
    match e {
        Expr::Do(b) => writes_slot(b, slot),
        Expr::If(arms, els) => {
            arms.iter().any(|(c, b)| writes_slot_expr(c, slot) || writes_slot(b, slot))
                || els.as_ref().map(|b| writes_slot(b, slot)).unwrap_or(false)
        }
        Expr::Match(subject, arms) => {
            writes_slot_expr(subject, slot)
                || arms.iter().any(|a| {
                    a.patterns.iter().any(|p| matches!(p, Pattern::Bind(_, Some(b)) if b.slot == slot))
                        || a.guard.as_ref().map(|g| writes_slot_expr(g, slot)).unwrap_or(false)
                        || writes_slot(&a.body, slot)
                })
        }
        Expr::Bin(_, a, b) | Expr::Index(a, b) | Expr::Range(a, b, _) => {
            writes_slot_expr(a, slot) || writes_slot_expr(b, slot)
        }
        Expr::Un(_, a) => writes_slot_expr(a, slot),
        Expr::Call(f, args) => {
            writes_slot_expr(f, slot) || args.iter().any(|a| writes_slot_expr(a, slot))
        }
        Expr::Method(o, _, args) => {
            writes_slot_expr(o, slot) || args.iter().any(|a| writes_slot_expr(a, slot))
        }
        Expr::Array(items) => items.iter().any(|i| writes_slot_expr(i, slot)),
        Expr::Map(items) => {
            items.iter().any(|(k, v)| writes_slot_expr(k, slot) || writes_slot_expr(v, slot))
        }
        // a nested function could capture and write the slot, but a captured
        // local is a cell and never gets a range fact in the first place
        Expr::Func(_) => false,
        _ => false,
    }
}

/// The pointer and length holding a table slot's array view.
fn span_idents(slot: u16) -> (proc_macro2::Ident, proc_macro2::Ident) {
    (format_ident!("p{}", slot), format_ident!("n{}", slot))
}

/// Locals are named by frame slot, so shadowing cannot collide.
fn ident(slot: u16) -> proc_macro2::Ident {
    format_ident!("v{}", slot)
}

fn num(n: f64) -> TokenStream {
    let lit = Literal::f64_suffixed(n);
    quote! { #lit }
}

impl Ctx {
    /// `want_value`: does the surrounding Rust context need an f64 out of this
    /// block, or a `()` statement?
    fn block(&mut self, b: &Block, want_value: bool) -> Lower<TokenStream> {
        let saved = self.known.clone();
        let mut out = TokenStream::new();
        for st in &b.stats {
            out.extend(self.stat(st)?);
        }
        let tail = match (&b.tail, want_value) {
            (Some(e), true) => {
                let v = self.expr(e)?;
                quote! { #v }
            }
            (Some(e), false) => {
                let v = self.expr(e)?;
                quote! { let _ = #v; }
            }
            (None, true) => {
                // no tail: the block must end in `return <expr>`, which types as `!`
                match b.stats.last() {
                    Some(Stat::Return(v)) if v.len() == 1 => TokenStream::new(),
                    _ => return Err("a value is needed but the block produces none".into()),
                }
            }
            (None, false) => TokenStream::new(),
        };
        self.known = saved;
        Ok(quote! { { #out #tail } })
    }

    fn stat(&mut self, st: &Stat) -> Lower<TokenStream> {
        Ok(match st {
            Stat::LetSlots(bindings, exprs) => {
                if exprs.len() > bindings.len() {
                    return Err("extra values in a `let`".into());
                }
                if bindings.iter().any(|b| b.cell) {
                    return Err("a local captured by a closure".into());
                }
                // right hand side first, then the bindings: `let x = x` must
                // read the *outer* x
                let mut out = TokenStream::new();
                let mut tmps = Vec::new();
                for (i, e) in exprs.iter().enumerate() {
                    let v = self.expr(e)?;
                    let t = format_ident!("__t{}", i);
                    out.extend(quote! { let #t: f64 = #v; });
                    tmps.push(t);
                }
                for (i, b) in bindings.iter().enumerate() {
                    let id = ident(b.slot);
                    let Some(t) = tmps.get(i) else {
                        return Err("a binding with no value (that is nil, not 0)".into());
                    };
                    out.extend(quote! { let mut #id: f64 = #t; });
                    self.known.insert(b.slot);
                }
                out
            }
            // `t[i] = v` on a table whose index is proven in range
            Stat::Assign(targets, exprs)
                if targets.len() == 1 && exprs.len() == 1 && self.is_table_write(&targets[0]) =>
            {
                let Expr::Index(obj, key) = &targets[0] else { unreachable!("checked") };
                let slot = self.table_slot(obj)?;
                let id = ident(slot);
                let i = self.expr(key)?;
                let v = self.expr(&exprs[0])?;
                if self.proven_in_range(key, slot) {
                    quote! { unsafe { rua_set(rt, #id, #i, #v, ok) }; }
                } else if self.writes {
                    // this code may not bail out part way, so an index it
                    // cannot vouch for is not compilable
                    return Err("an unproven index in code that cannot trap".into());
                } else {
                    // the write may be out of range — growing the table, or
                    // landing in the keyed part — which the interpreter has to
                    // do instead
                    let trap = self.on_trap.clone();
                    quote! {
                        unsafe { rua_set(rt, #id, #i, #v, ok) };
                        if unsafe { *ok } == 0 { #trap }
                    }
                }
            }
            Stat::Assign(targets, exprs) => {
                if exprs.len() > targets.len() {
                    return Err("extra values in an assignment".into());
                }
                // `(a, b) = (b, a)` swaps: evaluate every value before storing
                let mut out = TokenStream::new();
                let mut tmps = Vec::new();
                for (i, e) in exprs.iter().enumerate() {
                    let v = self.expr(e)?;
                    let t = format_ident!("__a{}", i);
                    out.extend(quote! { let #t: f64 = #v; });
                    tmps.push(t);
                }
                for (i, t) in targets.iter().enumerate() {
                    let id = self.local(t)?;
                    let Some(v) = tmps.get(i) else {
                        return Err("an assignment with no value (that is nil, not 0)".into());
                    };
                    out.extend(quote! { #id = #v; });
                }
                out
            }
            Stat::OpAssign(target, op, e) => {
                let id = self.local(target)?;
                let v = self.expr(e)?;
                match op {
                    BinOp::Add => quote! { #id += #v; },
                    BinOp::Sub => quote! { #id -= #v; },
                    BinOp::Mul => quote! { #id *= #v; },
                    BinOp::Div => quote! { #id /= #v; },
                    BinOp::Rem => quote! { #id = rua_rem(#id, #v); },
                    _ => return Err("unsupported compound assignment".into()),
                }
            }
            Stat::Return(exprs) => match exprs.len() {
                // fine in a procedure, which the caller turns back into nil
                0 => quote! { return 0.0; },
                1 => {
                    let v = self.expr(&exprs[0])?;
                    quote! { return #v; }
                }
                _ => return Err("multiple return values".into()),
            },
            Stat::Break => quote! { break; },
            Stat::Continue => match self.loop_labels.last() {
                // in a counted loop, `continue` leaves the body block so that
                // the increment below it still runs
                Some(Some(label)) => quote! { break #label; },
                Some(None) => quote! { continue; },
                None => return Err("`continue` outside a loop".into()),
            },
            Stat::While(_, cond, body) => {
                let c = self.truthy(cond)?;
                self.loop_labels.push(None);
                let b = self.block(body, false);
                self.loop_labels.pop();
                let b = b?;
                quote! { while #c #b }
            }
            Stat::Loop(_, body) => {
                self.loop_labels.push(None);
                let b = self.block(body, false);
                self.loop_labels.pop();
                let b = b?;
                quote! { loop #b }
            }
            Stat::ForRange { binding, start, end, inclusive, body, .. } => {
                let binding = binding.ok_or("unresolved `for` loop")?;
                if binding.cell {
                    return Err("the loop variable is captured by a closure".into());
                }
                let s = self.expr(start)?;
                let e = self.expr(end)?;
                let saved = self.known.clone();
                self.known.insert(binding.slot);
                let id = ident(binding.slot);
                // `for i in 0..t.len()` makes `t[i]` provably in range, which
                // is what lets code that also writes read a table at all
                let fact = self.range_fact(binding, start, end, *inclusive, body);
                if let Some(f) = fact {
                    self.in_range.push(f);
                }
                let label = self.fresh_label();
                self.loop_labels.push(Some(label.clone()));
                let b = self.block(body, false);
                self.loop_labels.pop();
                if fact.is_some() {
                    self.in_range.pop();
                }
                let b = b?;
                self.known = saved;
                let test = if *inclusive {
                    quote! { #id <= __end }
                } else {
                    quote! { #id < __end }
                };
                quote! {
                    {
                        let __end: f64 = #e;
                        let mut #id: f64 = #s;
                        while #test {
                            #label: { #b }
                            #id += 1.0;
                        }
                    }
                }
            }
            Stat::Expr(e) => match e {
                // an `if` used as a statement stays a statement
                Expr::If(..) | Expr::Do(_) => {
                    let v = self.value_or_unit(e)?;
                    quote! { #v }
                }
                other => {
                    let v = self.expr(other)?;
                    quote! { let _ = #v; }
                }
            },
            Stat::ForIn { .. } => return Err("`for ... in` over an iterator".into()),
            Stat::FnSlot(..) | Stat::FnDecl(..) => return Err("nested function".into()),
            Stat::Let(..) => return Err("unresolved `let`".into()),
        })
    }

    /// The name behind an assignment target, if it is a compilable local.
    fn local(&mut self, target: &Expr) -> Lower<proc_macro2::Ident> {
        match target {
            Expr::Local(b, name) if !b.cell && self.known.contains(&b.slot) => {
                let _ = name;
                Ok(ident(b.slot))
            }
            Expr::Local(_, name) | Expr::Upval(_, name) | Expr::Global(name, _) => {
                Err(format!("assignment to `{name}`, which is not a plain local"))
            }
            _ => Err("assignment to a field".into()),
        }
    }

    /// `if`/blocks in statement position: lower the branches as `()` blocks.
    fn value_or_unit(&mut self, e: &Expr) -> Lower<TokenStream> {
        Ok(match e {
            Expr::Do(b) => self.block(b, false)?,
            Expr::If(arms, els) => self.if_chain(arms, els.as_ref(), false)?,
            Expr::Match(subject, arms) => self.match_chain(subject, arms, false)?,
            other => {
                let v = self.expr(other)?;
                quote! { let _ = #v; }
            }
        })
    }

    /// A `match` over numbers is an if/else chain on one temporary.
    fn match_chain(&mut self, subject: &Expr, arms: &[Arm], want_value: bool) -> Lower<TokenStream> {
        let subj = self.expr(subject)?;
        let mut chain = TokenStream::new();
        let mut closed = false;
        for arm in arms.iter().rev() {
            // build from the bottom up, so each arm's `else` is what follows it
            let mut binding = TokenStream::new();
            let mut test: Option<TokenStream> = None;
            for p in &arm.patterns {
                match p {
                    Pattern::Wild => test = None,
                    Pattern::Bind(_, Some(b)) => {
                        if b.cell {
                            return Err("a match binding is captured by a closure".into());
                        }
                        let id = ident(b.slot);
                        self.known.insert(b.slot);
                        binding = quote! { let #id: f64 = __m; };
                    }
                    Pattern::Bind(name, None) => {
                        return Err(format!("unresolved pattern `{name}`"))
                    }
                    Pattern::Lit(e) => {
                        let v = self.expr(e)?;
                        let one = quote! { (__m == #v) };
                        test = Some(match test.take() {
                            Some(prev) => quote! { (#prev || #one) },
                            None => one,
                        });
                    }
                }
            }
            let body = self.block(&arm.body, want_value)?;
            let guard = match &arm.guard {
                Some(g) => Some(self.truthy(g)?),
                None => None,
            };
            let cond = match (test, guard) {
                (Some(t), Some(g)) => Some(quote! { #t && #g }),
                (Some(t), None) => Some(t),
                (None, Some(g)) => Some(g),
                (None, None) => None,
            };
            chain = match cond {
                None => {
                    closed = true;
                    quote! { { #binding #body } }
                }
                Some(cond) => {
                    let rest = if chain.is_empty() {
                        if want_value {
                            return Err("`match` in value position has no catch-all arm".into());
                        }
                        TokenStream::new()
                    } else {
                        quote! { else #chain }
                    };
                    quote! { { #binding if #cond #body #rest } }
                }
            };
        }
        if want_value && !closed {
            return Err("`match` in value position has no catch-all arm".into());
        }
        Ok(quote! { { let __m: f64 = #subj; #chain } })
    }

    fn if_chain(
        &mut self,
        arms: &[(Expr, Block)],
        els: Option<&Block>,
        want_value: bool,
    ) -> Lower<TokenStream> {
        let (cond, body) = &arms[0];
        let c = self.truthy(cond)?;
        let b = self.block(body, want_value)?;
        let tail = if arms.len() > 1 {
            let rest = self.if_chain(&arms[1..], els, want_value)?;
            quote! { else #rest }
        } else if let Some(e) = els {
            let eb = self.block(e, want_value)?;
            quote! { else #eb }
        } else if want_value {
            return Err("`if` without `else` used as a value".into());
        } else {
            TokenStream::new()
        };
        Ok(quote! { if #c #b #tail })
    }

    /// A condition, which must be *provably* a boolean.
    ///
    /// Everything in compiled code is an `f64`, so a boolean and the number it
    /// would be encoded as are indistinguishable — and rua is Lua-shaped, where
    /// `0` is true. Rather than guess, anything that is not a comparison or a
    /// combination of comparisons sends the function back to the interpreter.
    fn truthy(&mut self, e: &Expr) -> Lower<TokenStream> {
        match e {
            Expr::Bool(b) => {
                let lit = *b;
                Ok(quote! { #lit })
            }
            Expr::Bin(op, a, b) => {
                if let Some(o) = cmp_op(*op) {
                    let (l, r) = (self.expr(a)?, self.expr(b)?);
                    return Ok(quote! { (#l #o #r) });
                }
                if matches!(op, BinOp::And | BinOp::Or) {
                    let (l, r) = (self.truthy(a)?, self.truthy(b)?);
                    let o = if *op == BinOp::And { quote!(&&) } else { quote!(||) };
                    return Ok(quote! { (#l #o #r) });
                }
                Err("a condition that is not a comparison".into())
            }
            Expr::Un(UnOp::Not, a) => {
                let v = self.truthy(a)?;
                Ok(quote! { (!#v) })
            }
            _ => Err("a condition that is not provably a boolean".into()),
        }
    }

    fn expr(&mut self, e: &Expr) -> Lower<TokenStream> {
        Ok(match e {
            Expr::Num(n) => num(*n),
            // `true`/`false` are booleans, not the numbers 1 and 0
            Expr::Bool(_) => return Err("a boolean used as a number".into()),
            Expr::Nil => return Err("nil".into()),
            Expr::Str(_) => return Err("strings".into()),
            // only plain frame locals compile; a captured local (a cell) means
            // some closure shares it, and closures are not compiled
            Expr::Local(b, name) => {
                if b.cell || !self.known.contains(&b.slot) {
                    return Err(format!("`{name}` is captured or declared outside this function"));
                }
                let id = ident(b.slot);
                quote! { #id }
            }
            Expr::Upval(_, name) => return Err(format!("upvalue `{name}`")),
            Expr::Global(name, _) => return Err(format!("global `{name}`")),
            Expr::Var(name) => return Err(format!("unresolved `{name}`")),
            Expr::Do(b) => self.block(b, true)?,
            Expr::If(arms, els) => self.if_chain(arms, els.as_ref(), true)?,
            Expr::Match(subject, arms) => self.match_chain(subject, arms, true)?,
            Expr::Un(op, a) => {
                if matches!(op, UnOp::Not) {
                    return Err("`!` as a value".into());
                }
                let v = self.expr(a)?;
                match op {
                    UnOp::Neg => quote! { (-(#v)) },
                    UnOp::Not => return Err("`!` as a value".into()),
                }
            }
            Expr::Bin(op, a, b) => {
                use BinOp::*;
                match op {
                    // `a && b` yields one of its operands, and rua counts 0 as
                    // true, so this cannot be expressed in f64-only code
                    And | Or => return Err("`&&`/`||` as a value".into()),
                    Rem => {
                        let (l, r) = (self.expr(a)?, self.expr(b)?);
                        quote! { rua_rem(#l, #r) }
                    }
                    Add | Sub | Mul | Div => {
                        let (l, r) = (self.expr(a)?, self.expr(b)?);
                        let o = match op {
                            Add => quote!(+),
                            Sub => quote!(-),
                            Mul => quote!(*),
                            _ => quote!(/),
                        };
                        quote! { (#l #o #r) }
                    }
                    // a comparison produces a boolean, which is a different
                    // value from the number 1 or 0 that would represent it
                    _ => return Err("a comparison used as a value".into()),
                }
            }
            Expr::Call(f, args) => self.call(f, args)?,
            // `t[i]`: read straight out of the array view fetched on entry
            Expr::Index(obj, key) if self.is_table(obj) => {
                let slot = self.table_slot(obj)?;
                if self.kind_of(slot) != Kind::Table {
                    return Err("reading a table that is also appended to".into());
                }
                let (ptr, len) = span_idents(slot);
                // `for i in 0..t.len()` proves `t[i]` is in range, which is
                // what lets a function that writes read at all
                if self.proven_in_range(key, slot) {
                    let i = self.expr(key)?;
                    quote! { unsafe { *#ptr.add((#i) as usize) } }
                } else {
                    if self.writes {
                        return Err("an unproven index in code that also writes".into());
                    }
                    let i = self.expr(key)?;
                    let trap = self.on_trap.clone();
                    quote! {
                        {
                            let __i = #i;
                            let __u = __i as usize;
                            if __i < 0.0 || __i.fract() != 0.0 || __u >= #len {
                                unsafe { *ok = 0; }
                                #trap
                            }
                            unsafe { *#ptr.add(__u) }
                        }
                    }
                }
            }
            Expr::Method(obj, name, args) if self.is_table(obj) => {
                let slot = self.table_slot(obj)?;
                match (&**name, args.len()) {
                    ("len", 0) => match self.kind_of(slot) {
                        // a read table already knows its length from the view
                        Kind::Table => {
                            let (_, len) = span_idents(slot);
                            quote! { (#len as f64) }
                        }
                        _ => {
                            let id = ident(slot);
                            let trap = self.on_trap.clone();
                            quote! {
                                {
                                    let __v = unsafe { rua_len(rt, #id, ok) };
                                    if unsafe { *ok } == 0 { #trap }
                                    __v
                                }
                            }
                        }
                    },
                    ("push", 1) => {
                        let id = ident(slot);
                        let v = self.expr(&args[0])?;
                        // `push` yields no value; as an expression it is nil,
                        // which the numeric subset cannot hold, so it is only
                        // ever compiled in statement position
                        quote! { { unsafe { rua_push(rt, #id, #v) }; 0.0 } }
                    }
                    _ => return Err(format!("`{name}` on a table")),
                }
            }
            Expr::Method(obj, name, args) => {
                // `x.sqrt()` and friends are f64 methods in the generated code
                let mut lowered = vec![self.expr(obj)?];
                for a in args {
                    lowered.push(self.expr(a)?);
                }
                let recv = lowered.remove(0);
                math_method(name, &recv, &lowered)?
            }
            Expr::Index(..) => return Err("indexing".into()),
            Expr::Func(_) => return Err("nested function".into()),
            Expr::Array(_) => return Err("array literal".into()),
            Expr::Map(_) => return Err("map literal".into()),
            Expr::Range(..) => return Err("range".into()),
        })
    }

    /// A fresh label for a counted loop's body block.
    fn fresh_label(&mut self) -> syn::Lifetime {
        self.labels += 1;
        syn::Lifetime::new(&format!("'body{}", self.labels), proc_macro2::Span::call_site())
    }

    /// Is this an assignment into a table this code holds?
    fn is_table_write(&self, target: &Expr) -> bool {
        matches!(target, Expr::Index(obj, _) if self.is_table(obj))
    }

    /// Is this expression a local the inference decided holds a table?
    fn is_table(&self, e: &Expr) -> bool {
        matches!(e, Expr::Local(b, _)
            if matches!(self.kinds.get(&b.slot), Some(Kind::Table) | Some(Kind::TableOut)))
    }

    fn kind_of(&self, slot: u16) -> Kind {
        self.kinds.get(&slot).copied().unwrap_or(Kind::Num)
    }

    /// Is `key` a loop variable we know indexes `table` safely?
    fn proven_in_range(&self, key: &Expr, table: u16) -> bool {
        match key {
            Expr::Local(b, _) => self.in_range.iter().any(|(v, t)| *v == b.slot && *t == table),
            _ => false,
        }
    }

    /// `for i in 0..t.len()` makes `i` a proven index into `t` for the body —
    /// but only while the body leaves both the counter and the table alone.
    /// Without that check the generated code would index an f64 view with an
    /// arbitrary number, which is an out of bounds read.
    fn range_fact(
        &self,
        binding: Binding,
        start: &Expr,
        end: &Expr,
        inclusive: bool,
        body: &Block,
    ) -> Option<(u16, u16)> {
        if inclusive || binding.cell {
            return None;
        }
        match start {
            Expr::Num(n) if *n == 0.0 => {}
            _ => return None,
        }
        // the end has to be exactly `t.len()` on a table we read
        if let Expr::Method(obj, name, args) = end {
            if &**name == "len" && args.is_empty() {
                if let Expr::Local(b, _) = &**obj {
                    if self.kind_of(b.slot) == Kind::Table
                        && !writes_slot(body, binding.slot)
                        && !writes_slot(body, b.slot)
                    {
                        return Some((binding.slot, b.slot));
                    }
                }
            }
        }
        None
    }

    fn table_slot(&self, e: &Expr) -> Lower<u16> {
        match e {
            Expr::Local(b, _) if self.known.contains(&b.slot) => Ok(b.slot),
            _ => Err("a table that is not a parameter".into()),
        }
    }

    fn call(&mut self, f: &Expr, args: &[Expr]) -> Lower<TokenStream> {
        // A callee can trap — on its own depth check, or on a table that is not
        // dense numbers — and a trap unwinds to the interpreter, which re-runs
        // the whole call. That is only safe while nothing has been written yet.
        if self.writes {
            return Err("a call from code that also writes".into());
        }
        // self recursion: call the very symbol we are generating
        let is_self = match f {
            Expr::Upval(i, _) => Some(*i) == self.self_ref.upval,
            Expr::Global(name, _) => self.self_ref.global.as_deref() == Some(&**name),
            _ => false,
        };
        if is_self && args.len() == self.arity {
            if !self.self_params_numeric {
                return Err("recursion in a function that takes a table".into());
            }
            let sym = format_ident!("{}", self.self_symbol);
            let a: Vec<_> = args.iter().map(|x| self.expr(x)).collect::<Lower<_>>()?;
            let trap = self.on_trap.clone();
            return Ok(quote! {
                {
                    let __args = [#(RtArg { num: #a, table: std::ptr::null_mut() }),*];
                    let __r = unsafe { #sym(__args.as_ptr(), rt, ok) };
                    if unsafe { *ok } == 0 { #trap }
                    __r
                }
            });
        }
        // a call to another already compiled function becomes a direct call to
        // its machine code, at the address the runtime handed us
        if let Expr::Global(name, _) = f {
            let entry = self.self_ref.compiled_globals.get(&**name).cloned();
            if let Some((_addr, kinds)) = entry {
                if kinds.len() == args.len() {
                    // A direct call hands over an `RtArg` array, so it can pass
                    // a table the caller already holds — but never one the
                    // callee would write, since the callee's aliasing check
                    // lives in the runtime call path this skips.
                    let mut cells = Vec::with_capacity(args.len());
                    let mut ok = true;
                    for (a, kind) in args.iter().zip(&kinds) {
                        match kind {
                            Kind::Num => match self.expr(a) {
                                Ok(v) => cells.push(quote! {
                                    RtArg { num: #v, table: std::ptr::null_mut() }
                                }),
                                Err(_) => {
                                    ok = false;
                                    break;
                                }
                            },
                            Kind::Table => match a {
                                Expr::Local(b, _)
                                    if self.kind_of(b.slot) == Kind::Table
                                        && self.known.contains(&b.slot) =>
                                {
                                    let id = ident(b.slot);
                                    cells.push(quote! { RtArg { num: 0.0, table: #id } });
                                }
                                _ => {
                                    ok = false;
                                    break;
                                }
                            },
                            Kind::TableOut => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok {
                        let trap = self.on_trap.clone();
                        let index = match self.inlined.iter().position(|n| n == &**name) {
                            Some(i) => i,
                            None => {
                                self.inlined.push(name.to_string());
                                self.inlined.len() - 1
                            }
                        };
                        let index = Literal::usize_suffixed(index);
                        return Ok(quote! {
                            {
                                // SAFETY: the runtime put a compiled function of
                                // this shape in this slot, and it discards this
                                // code if that global is reassigned.
                                let __c = unsafe { *(*rt).callees.add(#index) };
                                let __f: unsafe extern "C" fn(
                                    *const RtArg,
                                    *const RtCtx,
                                    *mut i32,
                                ) -> f64 = unsafe { std::mem::transmute(__c.entry as *const ()) };
                                let __args = [#(#cells),*];
                                let __r = unsafe { __f(__args.as_ptr(), __c.ctx, ok) };
                                if unsafe { *ok } == 0 { #trap }
                                __r
                            }
                        });
                    }
                }
            }
        }
        if let Expr::Upval(_, name) | Expr::Local(_, name) | Expr::Global(name, _) = f {
            return Err(format!("call to `{name}`"));
        }
        // `math::sqrt(x)` maps onto Rust's f64 intrinsics
        if let Expr::Index(obj, key) = f {
            if let (Expr::Global(o, _), Expr::Str(k)) = (&**obj, &**key) {
                if &**o == "math" {
                    let a: Vec<_> = args.iter().map(|x| self.expr(x)).collect::<Lower<_>>()?;
                    if a.is_empty() {
                        return Err(format!("math::{k} with no arguments"));
                    }
                    return math_method(k, &a[0], &a[1..]);
                }
            }
        }
        Err("call to a non-inlinable function".into())
    }
}

/// The math whitelist, shared by `math::sqrt(x)` and `x.sqrt()`.
fn math_method(name: &str, recv: &TokenStream, rest: &[TokenStream]) -> Lower<TokenStream> {
    let unary = |m: &str| -> Lower<TokenStream> {
        if !rest.is_empty() {
            return Err(format!("{name} takes no arguments"));
        }
        let m = format_ident!("{}", m);
        Ok(quote! { (#recv).#m() })
    };
    Ok(match name {
        "floor" => unary("floor")?,
        "ceil" => unary("ceil")?,
        "abs" => unary("abs")?,
        "sqrt" => unary("sqrt")?,
        "sin" => unary("sin")?,
        "cos" => unary("cos")?,
        "tan" => unary("tan")?,
        "exp" => unary("exp")?,
        "ln" | "log" => unary("ln")?,
        "round" => unary("round")?,
        "max" | "min" | "powf" | "pow" => {
            if rest.len() != 1 {
                return Err(format!("{name} takes one argument"));
            }
            let y = &rest[0];
            let m = format_ident!("{}", if name.starts_with("pow") { "powf" } else { name });
            quote! { (#recv).#m(#y) }
        }
        other => return Err(format!("`{other}` is not in the math whitelist")),
    })
}

fn cmp_op(op: BinOp) -> Option<TokenStream> {
    Some(match op {
        BinOp::Lt => quote!(<),
        BinOp::Le => quote!(<=),
        BinOp::Gt => quote!(>),
        BinOp::Ge => quote!(>=),
        BinOp::Eq => quote!(==),
        BinOp::Ne => quote!(!=),
        _ => return None,
    })
}
