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
    /// `fn(table, index, ptr_out, len_out, ok) -> table` — the table at
    /// `t[i]`, and a view of its array part.
    pub inner: usize,
    /// The same two, for code that writes: the view is writable, and the
    /// runtime keeps or discards what was written when the call ends.
    pub span_mut: usize,
    pub inner_mut: usize,
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
    pub inner: usize,
    pub span_mut: usize,
    pub inner_mut: usize,
}

/// What a slot holds, as far as compiled code is concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Num,
    /// A table compiled code reads, through a view of its array part.
    Table,
    /// A table compiled code only ever appends to.
    TableOut,
    /// A local that only ever holds a boolean, carried as 0.0/1.0.
    Bool,
    /// A slot the region defines before it reads: whatever it holds on the way
    /// in is not looked at.
    Dead,
    /// A table whose elements are themselves tables — an array of bodies, a
    /// matrix.
    ///
    /// `checked` says the runtime has to walk it on the way in and confirm
    /// every element is a table of numbers at least `min` long. That is what
    /// lets a body that writes read `b[3]` with no test of its own, since it
    /// may not trap once it has written. A body that only reads can trap
    /// safely, so it checks as it goes and the walk is skipped — which matters
    /// when the compiled code is an inner loop entered thousands of times.
    Tables { checked: bool, min: u32 },
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
        if def.params.len() > 16 {
            return Err("more than 16 parameters".into());
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
                    let what = match st {
                        Stat::Loop(id, _) => format!("loop #{id}"),
                        Stat::While(id, _, _) => format!("while #{id}"),
                        Stat::ForRange { id, .. } => format!("for #{id}"),
                        _ => "loop".to_string(),
                    };
                    eprintln!("[rua-jit] skipped a hot {what}: {why}");
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
        let Kinds { mut kinds, inner_of } = infer_kinds(&wrapper, &self_ref.compiled_globals)?;
        // A loop may take a flag from outside and hand it back: the runtime
        // marshals those as booleans either way, so unlike a function's
        // parameters they need not be excluded.
        let bools = boolean_locals(&wrapper, &HashSet::new());
        let usage = table_usage(&wrapper);
        let writes = usage.traps_forbidden();
        let mutable_views = usage.writes_tables();
        relax_checks(&mut kinds, writes);
        // in a loop there are no parameters, so nothing is stable by that route
        let stable: HashSet<u16> = HashSet::new();
        let mut dead = dead_on_entry(&wrapper);
        // The loop being compiled is entered at its back edge, not at its
        // head, so its own counter is live there however the body reads it.
        // Nested loops inside the region do start from the top, and theirs
        // really is dead.
        if let Stat::ForRange { binding: Some(b), .. } = st {
            dead.remove(&b.slot);
        }
        let kind_list: Vec<Kind> = slots
            .iter()
            .map(|s| match kinds.get(s).copied() {
                // a slot defined before it is read, or one that only ever
                // holds a flag, is not the plain number the walk called it
                Some(Kind::Num) | None if dead.contains(s) => Kind::Dead,
                Some(Kind::Num) | None if bools.contains(s) => Kind::Bool,
                Some(k) => k,
                None => Kind::Num,
            })
            .collect();
        let mut cx = Ctx {
            known: slots.iter().copied().collect(),
            self_symbol: String::new(),
            self_ref,
            arity: usize::MAX, // there is no self call from a loop body
            inlined: Vec::new(),
            self_param_kinds: Vec::new(),
            writes,
            mutable_views,
            bools,
            kinds,
            inner_of,
            len_of: length_locals(&wrapper),
            in_range: Vec::new(),
            bounded: Vec::new(),
            hoisted: Vec::new(),
            stable_params: stable,
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
                Kind::Num | Kind::Bool | Kind::Dead => {
                    quote! { let mut #id: f64 = (*regs.add(#idx)).num; }
                }
                Kind::TableOut => quote! { let #id: *mut c_void = (*regs.add(#idx)).table; },
                Kind::Tables { .. } => {
                    let (_, len) = span_idents(*slot);
                    quote! {
                        let #id: *mut c_void = (*regs.add(#idx)).table;
                        let #len: usize = {
                            let n = rua_len(rt, #id, ok);
                            if *ok == 0 { return; }
                            n as usize
                        };
                    }
                }
                Kind::Table => {
                    let (ptr, len) = span_idents(*slot);
                    let fetch = if mutable_views {
                        quote! { let #ptr: *mut f64 = rua_span_mut(rt, #id, &mut #len, ok); }
                    } else {
                        quote! { let #ptr: *const f64 = rua_span(rt, #id, &mut #len, ok); }
                    };
                    quote! {
                        let #id: *mut c_void = (*regs.add(#idx)).table;
                        let mut #len: usize = 0;
                        #fetch
                        if *ok == 0 { return; }
                    }
                }
            }
        });
        let stores = slots.iter().zip(&kind_list).enumerate().filter_map(|(i, (slot, kind))| {
            // a table is mutated in place through the runtime, so only numbers
            // have to travel back into the registers
            match kind {
                Kind::Num | Kind::Bool | Kind::Dead => {
                    let id = ident(*slot);
                    let i = Literal::usize_suffixed(i);
                    Some(quote! { (*regs.add(#i)).num = #id; })
                }
                Kind::Table | Kind::TableOut | Kind::Tables { .. } => None,
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

    /// Keep the on-disk cache from growing without limit.
    ///
    /// Every distinct version of every compiled function leaves an object
    /// behind, and a session that edits code as it runs produces a lot of them.
    /// Oldest go first; anything still mapped by a running process stays valid
    /// until it unmaps, and a later run simply recompiles what it needs.
    fn prune_cache(&self) {
        let cap = std::env::var("RUA_JIT_CACHE_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(128)
            * 1024
            * 1024;
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        let mut total = 0u64;
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            total += meta.len();
            files.push((modified, meta.len(), e.path()));
        }
        if total <= cap {
            return;
        }
        // drop the oldest until comfortably under, so this does not run every
        // time a single object is added
        files.sort_by_key(|(t, _, _)| *t);
        let target = cap / 2;
        for (_, len, path) in files {
            if total <= target {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
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
                    "--edition", "2021", "-O", "-C", "debuginfo=0",
                    // an unstripped object is 4MB of symbol table for a
                    // function of a dozen instructions
                    "-C", "strip=symbols",
                    // generated code cannot panic, and unwinding out of it
                    // across the FFI boundary would be undefined anyway
                    "-C", "panic=abort",
                    "--crate-type", "cdylib",
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
            self.prune_cache();
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
        let Kinds { mut kinds, inner_of } = infer_kinds(&def.body, &self_ref.compiled_globals)?;
        let bools = boolean_locals(
            &def.body,
            &def.param_bindings.iter().map(|b| b.slot).collect(),
        );
        let usage = table_usage(&def.body);
        let writes = usage.traps_forbidden();
        let mutable_views = usage.writes_tables();
        relax_checks(&mut kinds, writes);
        // Only parameters may be tables — except an element of an array of
        // arrays, which is a local by construction: `let b = bodies[i]`.
        for (slot, kind) in &kinds {
            if matches!(kind, Kind::Table | Kind::Tables { .. })
                && !inner_of.contains_key(slot)
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
        let stable: HashSet<u16> = def
            .param_bindings
            .iter()
            .filter(|b| !writes_slot(&def.body, b.slot))
            .map(|b| b.slot)
            .collect();
        let mut cx = Ctx {
            known: def.param_bindings.iter().map(|b| b.slot).collect(),
            self_symbol: symbol.to_string(),
            self_ref,
            arity: def.params.len(),
            inlined: Vec::new(),
            self_param_kinds: param_kinds.clone(),
            writes,
            mutable_views,
            bools,
            kinds,
            inner_of,
            len_of: length_locals(&def.body),
            in_range: Vec::new(),
            bounded: Vec::new(),
            hoisted: Vec::new(),
            stable_params: stable,
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
                Kind::Num | Kind::Bool | Kind::Dead => {
                    quote! { let mut #id: f64 = (*args.add(#idx)).num; }
                }
                Kind::TableOut => quote! { let #id: *mut c_void = (*args.add(#idx)).table; },
                // An array of arrays arrives as an address and a length: the
                // views of its elements are fetched where they are bound,
                // which is once per element rather than once per access.
                Kind::Tables { .. } => {
                    let (_, len) = span_idents(b.slot);
                    quote! {
                        let #id: *mut c_void = (*args.add(#idx)).table;
                        let #len: usize = {
                            let n = rua_len(rt, #id, ok);
                            if *ok == 0 { return 0.0; }
                            n as usize
                        };
                    }
                }
                Kind::Table => {
                    let (ptr, len) = span_idents(b.slot);
                    let fetch = if cx.mutable_views {
                        quote! { let #ptr: *mut f64 = rua_span_mut(rt, #id, &mut #len, ok); }
                    } else {
                        quote! { let #ptr: *const f64 = rua_span(rt, #id, &mut #len, ok); }
                    };
                    quote! {
                        let #id: *mut c_void = (*args.add(#idx)).table;
                        let mut #len: usize = 0;
                        #fetch
                        if *ok == 0 { return 0.0; }
                    }
                }
            }
        });
        // one length check per (table, bound) pair, all before any body code
        let checks: Vec<TokenStream> = cx
            .hoisted
            .iter()
            .map(|(slot, bound)| {
                let (_, len) = span_idents(*slot);
                quote! {
                    if (#bound).ceil() > (#len as f64) {
                        *ok = 0;
                        return 0.0;
                    }
                }
            })
            .collect();
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
                    #(#checks)*
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
/// What the slots of a function hold: the kind of each, and which of them are
/// elements of an array of arrays, bound by `let b = t[i]`.
struct Kinds {
    kinds: HashMap<u16, Kind>,
    /// Element slot to the array-of-arrays slot it was bound from.
    inner_of: HashMap<u16, u16>,
}

fn infer_kinds(
    b: &Block,
    callees: &HashMap<String, (usize, Vec<Kind>)>,
) -> Result<Kinds, String> {
    let mut kinds = HashMap::new();
    let mut bad = None;
    // `let b = t[i]` says nothing on its own: `b` is a number if it is used as
    // one and an inner table if it is indexed, and which it is decides whether
    // `t` is a flat table or an array of arrays. So the walk records the link
    // and the answer is settled once everything has been seen.
    let mut links: Vec<(u16, u16)> = Vec::new();
    // How long an element has to be, recorded by the same walk so that no
    // constant index can slip past it. The lowering checks what it emits
    // against this too, so a miss would refuse the function rather than read
    // off the end of one.
    let mut longest: HashMap<(u16, bool), u32> = HashMap::new();
    // a local passed straight to a compiled function takes that parameter's
    // kind, which is how a table reaches a helper
    kinds_block(b, &mut kinds, &mut bad, callees, &mut links, &mut longest);

    let mut inner_of = HashMap::new();
    for (inner, outer) in &links {
        match kinds.get(inner) {
            Some(Kind::Table) => {
                inner_of.insert(*inner, *outer);
                note(*outer, Kind::Tables { checked: true, min: 0 }, &mut kinds, &mut bad);
            }
            // an ordinary `let x = t[i]`: a number out of a flat table
            _ => {
                note(*inner, Kind::Num, &mut kinds, &mut bad);
                note(*outer, Kind::Table, &mut kinds, &mut bad);
            }
        }
    }

    // Compiled code reads `b[3]` with no test of its own, so every element of
    // the array has to be at least that long; the runtime checks it on the way
    // in, where trapping is still safe.
    for (inner, outer) in &inner_of {
        let need = longest.get(&(*inner, false)).copied().unwrap_or(0);
        if let Some(Kind::Tables { min: have, .. }) = kinds.get(outer).copied() {
            kinds.insert(*outer, Kind::Tables { checked: true, min: have.max(need) });
        }
    }
    // and the same demand written the other way, as `t[k][3]`
    for ((slot, on_elements), need) in &longest {
        if !*on_elements {
            continue;
        }
        if let Some(Kind::Tables { min: have, .. }) = kinds.get(slot).copied() {
            kinds.insert(*slot, Kind::Tables { checked: true, min: have.max(*need) });
        }
    }

    match bad {
        Some(why) => Err(why),
        None => Ok(Kinds { kinds, inner_of }),
    }
}

/// `t[3]` needs `t` to have four elements, and `t[k][3]` needs every element
/// of `t` to have four. Remember the largest such demand, kept apart by which
/// of the two it is.
/// Locals bound once, at the top of a function, to `t.len()` and never
/// touched again: the table they measure.
///
/// That is the shape of `let n = t.len()` above a loop over `0..n`, and it is
/// what lets the loop's reads be proven on entry — where the length can be
/// named directly, and where trapping is still safe because nothing has been
/// written yet. Anything else that writes the local disqualifies it, which is
/// checked by taking the binding out and asking whether anything is left.
fn length_locals(body: &Block) -> HashMap<u16, u16> {
    let mut out = HashMap::new();
    for (i, st) in body.stats.iter().enumerate() {
        let Stat::LetSlots(bs, es) = st else { continue };
        let ([b], [Expr::Method(obj, name, args)]) = (&bs[..], &es[..]) else { continue };
        if b.cell || &**name != "len" || !args.is_empty() {
            continue;
        }
        let Expr::Local(t, _) = &**obj else { continue };
        let mut rest = body.clone();
        rest.stats.remove(i);
        if writes_slot(&rest, b.slot) {
            continue;
        }
        out.insert(b.slot, t.slot);
    }
    out
}

/// A body that only reads can trap wherever it likes, so it checks the shape
/// of an array of arrays as it goes rather than making the runtime walk it on
/// every entry.
fn relax_checks(kinds: &mut HashMap<u16, Kind>, writes: bool) {
    if writes {
        return;
    }
    for kind in kinds.values_mut() {
        if let Kind::Tables { checked, .. } = kind {
            *checked = false;
        }
    }
}

/// Locals that only ever hold a boolean.
///
/// Compiled code has nothing but `f64`, and rua's `0` is true, so a boolean
/// and the number encoding it are otherwise indistinguishable — which is why a
/// condition has to be provably one. A local assigned nothing but conditions
/// is an exception: it can be held as 0.0/1.0 and tested against zero, which
/// is what a flag like `let done = false` needs. Every other use of it fails
/// to compile, so nothing can read it as a number by mistake.
/// Slots a region defines before it reads: their value on the way in cannot
/// matter.
///
/// On-stack replacement enters a loop at the top of its body, so a local the
/// body binds before using is dead at that moment — whatever the register
/// happens to hold, a boolean left by the last iteration or nothing at all.
/// Without this the loop is compiled and then refused at every entry, because
/// the register does not hold the number the slot's kind promises.
fn dead_on_entry(body: &Block) -> HashSet<u16> {
    let mut first: Vec<(u16, bool)> = Vec::new();
    dead_block(body, &mut first);
    let mut seen = HashSet::new();
    let mut dead = HashSet::new();
    for (slot, is_bind) in first {
        if seen.insert(slot) && is_bind {
            dead.insert(slot);
        }
    }
    dead
}

fn dead_note_reads(e: &Expr, out: &mut Vec<(u16, bool)>) {
    match e {
        Expr::Local(b, _) => out.push((b.slot, false)),
        Expr::Do(b) => dead_block(b, out),
        Expr::If(arms, els) => {
            for (c, b) in arms {
                dead_note_reads(c, out);
                dead_block(b, out);
            }
            if let Some(b) = els {
                dead_block(b, out);
            }
        }
        Expr::Match(subject, arms) => {
            dead_note_reads(subject, out);
            for arm in arms {
                dead_block(&arm.body, out);
            }
        }
        Expr::Bin(_, a, b) | Expr::Range(a, b, _) | Expr::Index(a, b) => {
            dead_note_reads(a, out);
            dead_note_reads(b, out);
        }
        Expr::Un(_, a) => dead_note_reads(a, out),
        Expr::Call(f, args) => {
            dead_note_reads(f, out);
            args.iter().for_each(|a| dead_note_reads(a, out));
        }
        Expr::Method(o, _, args) => {
            dead_note_reads(o, out);
            args.iter().for_each(|a| dead_note_reads(a, out));
        }
        _ => {}
    }
}

fn dead_block(b: &Block, out: &mut Vec<(u16, bool)>) {
    for st in &b.stats {
        dead_stat(st, out);
    }
    if let Some(t) = &b.tail {
        dead_note_reads(t, out);
    }
}

fn dead_stat(st: &Stat, out: &mut Vec<(u16, bool)>) {
    match st {
        // the value is read first, and only then does the name come into being
        Stat::LetSlots(bs, es) => {
            es.iter().for_each(|e| dead_note_reads(e, out));
            bs.iter().for_each(|b| out.push((b.slot, true)));
        }
        Stat::Assign(ts, es) => {
            es.iter().for_each(|e| dead_note_reads(e, out));
            // an assignment is not a binding: the name was already there
            ts.iter().for_each(|t| dead_note_reads(t, out));
        }
        Stat::OpAssign(t, _, e) => {
            dead_note_reads(t, out);
            dead_note_reads(e, out);
        }
        Stat::While(_, c, b) => {
            dead_note_reads(c, out);
            dead_block(b, out);
        }
        Stat::Loop(_, b) => dead_block(b, out),
        Stat::ForRange { binding, start, end, body, .. } => {
            dead_note_reads(start, out);
            dead_note_reads(end, out);
            if let Some(b) = binding {
                out.push((b.slot, true));
            }
            dead_block(body, out);
        }
        Stat::ForIn { bindings, iter, body, .. } => {
            dead_note_reads(iter, out);
            bindings.iter().for_each(|b| out.push((b.slot, true)));
            dead_block(body, out);
        }
        Stat::FnDecl(_, e) | Stat::FnSlot(_, e) | Stat::Expr(e) => dead_note_reads(e, out),
        Stat::Let(_, es) | Stat::Return(es) => {
            es.iter().for_each(|e| dead_note_reads(e, out))
        }
        Stat::Break | Stat::Continue => {}
    }
}

fn boolean_locals(body: &Block, outside: &HashSet<u16>) -> HashSet<u16> {
    let mut assigned: HashMap<u16, bool> = HashMap::new();
    let mut spoiled: HashSet<u16> = HashSet::new();
    let mut bound: HashSet<u16> = HashSet::new();
    bool_block(body, &mut assigned, &mut spoiled, &mut bound);
    assigned
        .into_iter()
        .filter(|(slot, all_bool)| {
            // declared here, or not something that arrives from outside
            *all_bool && !spoiled.contains(slot) && (bound.contains(slot) || !outside.contains(slot))
        })
        .map(|(slot, _)| slot)
        .collect()
}

fn bool_note(slot: u16, e: &Expr, assigned: &mut HashMap<u16, bool>) {
    let is_bool = bool_valued(e);
    let at = assigned.entry(slot).or_insert(true);
    *at &= is_bool;
}

/// Is this expression one the compiler can see is a boolean?
fn bool_valued(e: &Expr) -> bool {
    match e {
        Expr::Bool(_) => true,
        Expr::Bin(op, a, b) => {
            cmp_op(*op).is_some()
                || (matches!(op, BinOp::And | BinOp::Or) && bool_valued(a) && bool_valued(b))
        }
        Expr::Un(UnOp::Not, a) => bool_valued(a),
        _ => false,
    }
}

fn bool_block(b: &Block, assigned: &mut HashMap<u16, bool>, spoiled: &mut HashSet<u16>, bound: &mut HashSet<u16>) {
    for st in &b.stats {
        bool_stat(st, assigned, spoiled, bound);
    }
    if let Some(t) = &b.tail {
        bool_expr(t, assigned, spoiled, bound);
    }
}

fn bool_stat(st: &Stat, assigned: &mut HashMap<u16, bool>, spoiled: &mut HashSet<u16>, bound: &mut HashSet<u16>) {
    match st {
        Stat::LetSlots(bs, es) => {
            for (i, b) in bs.iter().enumerate() {
                bound.insert(b.slot);
                match es.get(i) {
                    Some(e) => bool_note(b.slot, e, assigned),
                    None => {
                        spoiled.insert(b.slot);
                    }
                }
            }
            for e in es {
                bool_expr(e, assigned, spoiled, bound);
            }
        }
        Stat::Assign(ts, es) => {
            for (i, t) in ts.iter().enumerate() {
                if let Expr::Local(b, _) = t {
                    match es.get(i) {
                        Some(e) => bool_note(b.slot, e, assigned),
                        None => {
                            spoiled.insert(b.slot);
                        }
                    }
                }
            }
            for e in es {
                bool_expr(e, assigned, spoiled, bound);
            }
        }
        // arithmetic on it, or a loop counting through it, and it is a number
        Stat::OpAssign(t, _, e) => {
            if let Expr::Local(b, _) = t {
                spoiled.insert(b.slot);
            }
            bool_expr(e, assigned, spoiled, bound);
        }
        Stat::ForRange { binding, body, .. } => {
            if let Some(b) = binding {
                spoiled.insert(b.slot);
            }
            bool_block(body, assigned, spoiled, bound);
        }
        Stat::ForIn { bindings, body, .. } => {
            for b in bindings {
                spoiled.insert(b.slot);
            }
            bool_block(body, assigned, spoiled, bound);
        }
        Stat::While(_, c, b) => {
            bool_expr(c, assigned, spoiled, bound);
            bool_block(b, assigned, spoiled, bound);
        }
        Stat::Loop(_, b) => bool_block(b, assigned, spoiled, bound),
        Stat::FnDecl(_, e) | Stat::FnSlot(_, e) | Stat::Expr(e) => {
            bool_expr(e, assigned, spoiled, bound)
        }
        Stat::Let(_, es) | Stat::Return(es) => {
            for e in es {
                bool_expr(e, assigned, spoiled, bound);
            }
        }
        Stat::Break | Stat::Continue => {}
    }
}

fn bool_expr(e: &Expr, assigned: &mut HashMap<u16, bool>, spoiled: &mut HashSet<u16>, bound: &mut HashSet<u16>) {
    match e {
        Expr::Do(b) => bool_block(b, assigned, spoiled, bound),
        Expr::If(arms, els) => {
            for (c, b) in arms {
                bool_expr(c, assigned, spoiled, bound);
                bool_block(b, assigned, spoiled, bound);
            }
            if let Some(b) = els {
                bool_block(b, assigned, spoiled, bound);
            }
        }
        Expr::Match(subject, arms) => {
            bool_expr(subject, assigned, spoiled, bound);
            for arm in arms {
                bool_block(&arm.body, assigned, spoiled, bound);
            }
        }
        Expr::Bin(_, a, b) | Expr::Range(a, b, _) => {
            bool_expr(a, assigned, spoiled, bound);
            bool_expr(b, assigned, spoiled, bound);
        }
        Expr::Un(_, a) => bool_expr(a, assigned, spoiled, bound),
        Expr::Index(a, b) => {
            bool_expr(a, assigned, spoiled, bound);
            bool_expr(b, assigned, spoiled, bound);
        }
        Expr::Call(f, args) => {
            bool_expr(f, assigned, spoiled, bound);
            for a in args {
                bool_expr(a, assigned, spoiled, bound);
            }
        }
        Expr::Method(o, _, args) => {
            bool_expr(o, assigned, spoiled, bound);
            for a in args {
                bool_expr(a, assigned, spoiled, bound);
            }
        }
        _ => {}
    }
}

fn note_const_index(what: (u16, bool), key: &Expr, longest: &mut HashMap<(u16, bool), u32>) {
    if let Expr::Num(n) = key {
        if *n >= 0.0 && n.fract() == 0.0 && *n < u32::MAX as f64 {
            let need = *n as u32 + 1;
            let at = longest.entry(what).or_insert(0);
            *at = (*at).max(need);
        }
    }
}

fn note(slot: u16, kind: Kind, kinds: &mut HashMap<u16, Kind>, bad: &mut Option<String>) {
    match kinds.insert(slot, kind) {
        Some(old) if old != kind => {
            *bad = Some(format!("slot {slot} is used as both {old:?} and {kind:?}"))
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
    links: &mut Vec<(u16, u16)>,
    longest: &mut HashMap<(u16, bool), u32>,
) {
    for st in &b.stats {
        kinds_stat(st, kinds, bad, callees, links, longest);
    }
    if let Some(t) = &b.tail {
        kinds_expr(t, kinds, bad, callees, links, longest);
    }
}

fn kinds_stat(
    st: &Stat,
    kinds: &mut HashMap<u16, Kind>,
    bad: &mut Option<String>,
    callees: &Callees,
    links: &mut Vec<(u16, u16)>,
    longest: &mut HashMap<(u16, bool), u32>,
) {
    match st {
        Stat::Let(_, es) | Stat::Return(es) => es.iter().for_each(|e| kinds_expr(e, kinds, bad, callees, links, longest)),
        Stat::LetSlots(bs, es) => {
            // `let b = t[i]`: what `b` is decides what `t` is, and neither is
            // known yet
            if let ([b], [Expr::Index(obj, key)]) = (&bs[..], &es[..]) {
                if let Expr::Local(t, _) = &**obj {
                    links.push((b.slot, t.slot));
                    kinds_expr(key, kinds, bad, callees, links, longest);
                    return;
                }
            }
            for b in bs {
                note(b.slot, Kind::Num, kinds, bad);
            }
            es.iter().for_each(|e| kinds_expr(e, kinds, bad, callees, links, longest));
        }
        Stat::FnDecl(_, e) | Stat::FnSlot(_, e) | Stat::Expr(e) => kinds_expr(e, kinds, bad, callees, links, longest),
        Stat::Assign(ts, es) => {
            for t in ts {
                // `t[i] = v` writes in place, which the view survives, so the
                // table is still read through a span
                if let Expr::Index(obj, key) = t {
                    if let Expr::Local(b, _) = &**obj {
                        note(b.slot, Kind::Table, kinds, bad);
                        note_const_index((b.slot, false), key, longest);
                        kinds_expr(key, kinds, bad, callees, links, longest);
                        continue;
                    }
                }
                kinds_expr(t, kinds, bad, callees, links, longest);
            }
            es.iter().for_each(|e| kinds_expr(e, kinds, bad, callees, links, longest));
        }
        Stat::OpAssign(t, _, e) => {
            kinds_expr(t, kinds, bad, callees, links, longest);
            kinds_expr(e, kinds, bad, callees, links, longest);
        }
        Stat::While(_, c, b) => {
            kinds_expr(c, kinds, bad, callees, links, longest);
            kinds_block(b, kinds, bad, callees, links, longest);
        }
        Stat::Loop(_, b) => kinds_block(b, kinds, bad, callees, links, longest),
        Stat::ForRange { binding, start, end, body, .. } => {
            if let Some(b) = binding {
                note(b.slot, Kind::Num, kinds, bad);
            }
            kinds_expr(start, kinds, bad, callees, links, longest);
            kinds_expr(end, kinds, bad, callees, links, longest);
            kinds_block(body, kinds, bad, callees, links, longest);
        }
        Stat::ForIn { body, iter, .. } => {
            kinds_expr(iter, kinds, bad, callees, links, longest);
            kinds_block(body, kinds, bad, callees, links, longest);
        }
        Stat::Break | Stat::Continue => {}
    }
}

fn kinds_expr(
    e: &Expr,
    kinds: &mut HashMap<u16, Kind>,
    bad: &mut Option<String>,
    callees: &Callees,
    links: &mut Vec<(u16, u16)>,
    longest: &mut HashMap<(u16, bool), u32>,
) {
    match e {
        // `t[i]` and `t.len()` are what make a slot a table
        Expr::Index(obj, key) => {
            match &**obj {
                Expr::Local(b, _) => {
                    note(b.slot, Kind::Table, kinds, bad);
                    note_const_index((b.slot, false), key, longest);
                }
                // `t[k][j]` reaches an element without binding it: `t` is an
                // array of arrays either way
                Expr::Index(outer, k) if matches!(&**outer, Expr::Local(..)) => {
                    if let Expr::Local(b, _) = &**outer {
                        note(b.slot, Kind::Tables { checked: true, min: 0 }, kinds, bad);
                        note_const_index((b.slot, true), key, longest);
                    }
                    kinds_expr(k, kinds, bad, callees, links, longest);
                }
                _ => kinds_expr(obj, kinds, bad, callees, links, longest),
            }
            kinds_expr(key, kinds, bad, callees, links, longest);
        }
        Expr::Method(obj, name, args) => {
            match (&**obj, &**name) {
                // `t.len()` works for a table either way, so it says nothing
                // about which kind this is
                (Expr::Local(_, _), "len") => {}
                (Expr::Local(b, _), "push") => note(b.slot, Kind::TableOut, kinds, bad),
                _ => kinds_expr(obj, kinds, bad, callees, links, longest),
            }
            args.iter().for_each(|a| kinds_expr(a, kinds, bad, callees, links, longest));
        }
        Expr::Local(b, _) => note(b.slot, Kind::Num, kinds, bad),
        Expr::Bin(_, a, b) | Expr::Range(a, b, _) => {
            kinds_expr(a, kinds, bad, callees, links, longest);
            kinds_expr(b, kinds, bad, callees, links, longest);
        }
        Expr::Un(_, a) => kinds_expr(a, kinds, bad, callees, links, longest),
        Expr::Call(f, args) => {
            // an argument handed to a compiled function takes that
            // parameter's kind: that is how a table reaches a helper
            if let Expr::Global(name, _) = &**f {
                match callees.get(&**name) {
                    Some((_, param_kinds)) if param_kinds.len() == args.len() => {
                        for (a, kind) in args.iter().zip(param_kinds) {
                            match (a, kind) {
                                (Expr::Local(b, _), Kind::Table | Kind::TableOut) => {
                                    note(b.slot, *kind, kinds, bad)
                                }
                                _ => kinds_expr(a, kinds, bad, callees, links, longest),
                            }
                        }
                        return;
                    }
                    _ => {
                        // An unknown callee — a function not compiled yet,
                        // including this one calling itself — says nothing
                        // about its arguments. Guessing "number" here would
                        // contradict how they are used elsewhere.
                        for a in args {
                            if !matches!(a, Expr::Local(..)) {
                                kinds_expr(a, kinds, bad, callees, links, longest);
                            }
                        }
                        return;
                    }
                }
            }
            kinds_expr(f, kinds, bad, callees, links, longest);
            args.iter().for_each(|a| kinds_expr(a, kinds, bad, callees, links, longest));
        }
        Expr::Array(items) => items.iter().for_each(|i| kinds_expr(i, kinds, bad, callees, links, longest)),
        Expr::Map(items) => items.iter().for_each(|(k, v)| {
            kinds_expr(k, kinds, bad, callees, links, longest);
            kinds_expr(v, kinds, bad, callees, links, longest);
        }),
        Expr::If(arms, els) => {
            for (c, b) in arms {
                kinds_expr(c, kinds, bad, callees, links, longest);
                kinds_block(b, kinds, bad, callees, links, longest);
            }
            if let Some(b) = els {
                kinds_block(b, kinds, bad, callees, links, longest);
            }
        }
        Expr::Match(subject, arms) => {
            kinds_expr(subject, kinds, bad, callees, links, longest);
            for arm in arms {
                for p in &arm.patterns {
                    match p {
                        Pattern::Lit(e) => kinds_expr(e, kinds, bad, callees, links, longest),
                        Pattern::Bind(_, Some(b)) => note(b.slot, Kind::Num, kinds, bad),
                        _ => {}
                    }
                }
                if let Some(g) = &arm.guard {
                    kinds_expr(g, kinds, bad, callees, links, longest);
                }
                kinds_block(&arm.body, kinds, bad, callees, links, longest);
            }
        }
        Expr::Do(b) => kinds_block(b, kinds, bad, callees, links, longest),
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
            pub inner: usize,
            pub span_mut: usize,
            pub inner_mut: usize,
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
        /// As `rua_span`. What is written through this view is kept, or
        /// thrown away, when the call that fetched it ends.
        #[inline(always)]
        unsafe fn rua_span_mut(
            rt: *const RtCtx,
            t: *mut c_void,
            len: *mut usize,
            ok: *mut i32,
        ) -> *mut f64 {
            let f: unsafe extern "C" fn(*mut c_void, *mut usize, *mut i32) -> *mut f64 =
                std::mem::transmute((*rt).span_mut as *const ());
            f(t, len, ok)
        }

        /// # Safety
        /// As `rua_inner`, for code that writes to the element.
        #[inline(always)]
        unsafe fn rua_inner_mut(
            rt: *const RtCtx,
            t: *mut c_void,
            i: f64,
            ptr: *mut *mut f64,
            len: *mut usize,
            ok: *mut i32,
        ) -> *mut c_void {
            let f: unsafe extern "C" fn(
                *mut c_void,
                f64,
                *mut *mut f64,
                *mut usize,
                *mut i32,
            ) -> *mut c_void = std::mem::transmute((*rt).inner_mut as *const ());
            f(t, i, ptr, len, ok)
        }

        /// # Safety
        /// `t` is a live table of tables and `i` an index inside it. The
        /// element's view stays valid for the same reason a top level one
        /// does: nothing reshapes it while this code runs.
        #[inline(always)]
        unsafe fn rua_inner(
            rt: *const RtCtx,
            t: *mut c_void,
            i: f64,
            ptr: *mut *const f64,
            len: *mut usize,
            ok: *mut i32,
        ) -> *mut c_void {
            let f: unsafe extern "C" fn(
                *mut c_void,
                f64,
                *mut *const f64,
                *mut usize,
                *mut i32,
            ) -> *mut c_void = std::mem::transmute((*rt).inner as *const ());
            f(t, i, ptr, len, ok)
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
    /// Locals that only ever hold a boolean, held as 0.0/1.0.
    bools: HashSet<u16>,
    /// Does this code write through its views? They are fetched writable if
    /// so, and the runtime keeps or discards what was written.
    mutable_views: bool,
    /// Slots bound by `let b = t[i]` where the elements are tables, and the
    /// array they came from. Their view is fetched where they are bound rather
    /// than on entry.
    inner_of: HashMap<u16, u16>,
    /// Locals bound once to `t.len()` and never written: the table they
    /// measure. A loop bounded by one of those is bounded by the table.
    len_of: HashMap<u16, u16>,
    self_symbol: String,
    /// How this function refers to itself, if it does.
    self_ref: SelfRef,
    /// Globals compiled in as direct calls.
    inlined: Vec<String>,
    arity: usize,
    /// Whether every parameter of this function is a number, which is what a
    /// direct self call is able to pass.
    /// What this function's own parameters are, for a recursive call.
    self_param_kinds: Vec<Kind>,
    /// What each slot holds: a number, or a table reached through the hooks.
    kinds: HashMap<u16, Kind>,
    /// True when this code appends to a table. Once it has written something,
    /// trapping back to the interpreter would run those writes twice, so every
    /// read has to be provably in range instead.
    writes: bool,
    /// Loop variables known to be a valid index into a given table, from
    /// `for i in 0..t.len()`.
    in_range: Vec<(u16, u16)>,
    /// Loop variables running `0..bound` where the bound is a parameter or a
    /// literal — something that can be compared against a table's length once,
    /// on entry, instead of on every read.
    bounded: Vec<(u16, TokenStream)>,
    /// The checks that hoisting produced: `(table slot, bound)`.
    hoisted: Vec<(u16, TokenStream)>,
    /// Parameters the body never assigns, whose value at entry is their value
    /// throughout.
    stable_params: HashSet<u16>,
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
    ///
    /// An in-place write goes through the numeric view, and the runtime throws
    /// that view away if the call traps, so those writes undo themselves. An
    /// append does not: it has already changed the array part.
    fn traps_forbidden(&self) -> bool {
        self.pushes
    }

    /// Does this code write to a table at all? Those it writes are handed to
    /// it as writable views.
    fn writes_tables(&self) -> bool {
        !self.written.is_empty()
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
        // Which locals are flags is a property of the scope, not of the
        // register: the compiler reuses a register for a flag in one block and
        // a number in another, and both are correct where they stand.
        let saved_bools = self.bools.clone();
        let mut out = TokenStream::new();
        for st in &b.stats {
            out.extend(self.stat(st)?);
        }
        let tail = match (&b.tail, want_value) {
            (Some(e), true) => {
                let v = self.expr(e)?;
                quote! { #v }
            }
            // A block's last expression is its value, but in statement
            // position there is no value to want: `for j in .. { if c { .. } }`
            // ends in an `if` that produces nothing, and that is fine.
            (Some(e), false) => self.value_or_unit(e)?,
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
        self.bools = saved_bools;
        Ok(quote! { { #out #tail } })
    }

    fn stat(&mut self, st: &Stat) -> Lower<TokenStream> {
        Ok(match st {
            // `let b = t[i]` where the elements of `t` are tables: fetch the
            // element's address and the view of its numbers here, once, rather
            // than at every access through it
            Stat::LetSlots(bindings, exprs)
                if bindings.len() == 1 && self.inner_of.contains_key(&bindings[0].slot) =>
            {
                let b = bindings[0];
                if b.cell {
                    return Err("an element of an array captured by a closure".into());
                }
                let Some(Expr::Index(obj, key)) = exprs.first() else {
                    return Err("an element binding with no index".into());
                };
                let outer = self.table_slot(obj)?;
                if !self.proven_in_range(key, outer) {
                    return Err("an unproven index into an array of arrays".into());
                }
                let outer_id = ident(outer);
                let id = ident(b.slot);
                let (ptr, len) = span_idents(b.slot);
                let i = self.expr(key)?;
                let trap = self.on_trap.clone();
                self.known.insert(b.slot);
                // The runtime checked on the way in that every element is a
                // table of numbers, and the index is proven, so this cannot
                // fail; the check is what makes that a wrong answer rather
                // than a wild pointer if it ever did.
                let fetch = if self.mutable_views {
                    quote! {
                        let mut #ptr: *mut f64 = std::ptr::null_mut();
                        let #id: *mut c_void =
                            unsafe { rua_inner_mut(rt, #outer_id, #i, &mut #ptr, &mut #len, ok) };
                    }
                } else {
                    quote! {
                        let mut #ptr: *const f64 = std::ptr::null();
                        let #id: *mut c_void =
                            unsafe { rua_inner(rt, #outer_id, #i, &mut #ptr, &mut #len, ok) };
                    }
                };
                quote! {
                    let mut #len: usize = 0;
                    #fetch
                    if unsafe { *ok } == 0 { #trap }
                }
            }
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
                    // a flag is held as 0.0/1.0, and read back as a condition
                    let v = if bool_valued(e) {
                        let c = self.truthy(e)?;
                        quote! { rua_bool(#c) }
                    } else {
                        self.expr(e)?
                    };
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
                    // this binding decides what the register means from here
                    match exprs.get(i).map(bool_valued) {
                        Some(true) => self.bools.insert(b.slot),
                        _ => self.bools.remove(&b.slot),
                    };
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
                    let (ptr, _) = span_idents(slot);
                    let _ = &id;
                    quote! { unsafe { *#ptr.add((#i) as usize) = #v; } }
                } else if self.writes {
                    // this code may not bail out part way, so an index it
                    // cannot vouch for is not compilable
                    return Err("an unproven index in code that cannot trap".into());
                } else if self.mutable_views {
                    // The index cannot be vouched for, so it is tested here:
                    // inside the array part the write is a store through the
                    // view, and anywhere else — growing the table, or landing
                    // in the keyed part — is the interpreter's business.
                    let (ptr, len) = span_idents(slot);
                    let trap = self.on_trap.clone();
                    let _ = &id;
                    quote! {
                        {
                            let __i = #i;
                            let __u = __i as usize;
                            let __v = #v;
                            if __i < 0.0 || __i.fract() != 0.0 || __u >= #len {
                                unsafe { *ok = 0; }
                                #trap
                            }
                            unsafe { *#ptr.add(__u) = __v; }
                        }
                    }
                } else {
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
                    let v = match targets.get(i) {
                        Some(Expr::Local(b, _)) if self.bools.contains(&b.slot) => {
                            let c = self.truthy(e)?;
                            quote! { rua_bool(#c) }
                        }
                        _ => self.expr(e)?,
                    };
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
            // `t[i] -= 1` is the read, the operation and the write. The index
            // is evaluated twice, so it has to be something plain — which is
            // what it is in practice, a counter or a literal.
            Stat::OpAssign(target @ Expr::Index(_, key), op, e)
                if self.is_table_write(target)
                    && matches!(&**key, Expr::Local(..) | Expr::Num(_)) =>
            {
                let expanded = Stat::Assign(
                    vec![target.clone()],
                    vec![Expr::Bin(*op, Box::new(target.clone()), Box::new(e.clone()))],
                );
                return self.stat(&expanded);
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
                let bound = self.hoistable_bound(binding, start, end, *inclusive, body);
                if let Some(b) = bound.clone() {
                    self.bounded.push((binding.slot, b));
                }
                let label = self.fresh_label();
                self.loop_labels.push(Some(label.clone()));
                let b = self.block(body, false);
                self.loop_labels.pop();
                if fact.is_some() {
                    self.in_range.pop();
                }
                if bound.is_some() {
                    self.bounded.pop();
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
            // a local that only ever held a condition
            Expr::Local(b, _) if self.bools.contains(&b.slot) && self.known.contains(&b.slot) => {
                let id = ident(b.slot);
                Ok(quote! { (#id != 0.0) })
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
                if self.bools.contains(&b.slot) {
                    return Err(format!("`{name}` holds a boolean, used as a number"));
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
            // `t[k][j]`: an element reached without being bound. The view is
            // fetched here, so this is worth an inner loop only when the
            // element changes every time round it — which is exactly when a
            // binding would not have helped either.
            Expr::Index(obj, key) if self.elem_index(obj).is_some() => {
                let (slot, k) = self.elem_index(obj).expect("checked");
                let Kind::Tables { min, .. } = self.kind_of(slot) else {
                    return Err("indexing twice into something that is not an array of arrays".into());
                };
                let outer = ident(slot);
                let trap = self.on_trap.clone();
                // In code that writes, a trap here would re-run a call that
                // has already changed something, so both indexes have to be
                // settled in advance: the outer one proven, the inner one a
                // constant the runtime checked every element against.
                if self.writes {
                    if !self.proven_in_range(&k, slot) {
                        return Err("an unproven index into an array of arrays".into());
                    }
                    match &**key {
                        Expr::Num(n) if *n >= 0.0 && n.fract() == 0.0 && (*n as u32) < min => {}
                        _ => return Err("an unproven index inside an array of arrays".into()),
                    }
                }
                let ki = self.expr(&k)?;
                let j = self.expr(key)?;
                quote! {
                    {
                        let mut __el: usize = 0;
                        let mut __ep: *const f64 = std::ptr::null();
                        unsafe { rua_inner(rt, #outer, #ki, &mut __ep, &mut __el, ok) };
                        if unsafe { *ok } == 0 { #trap }
                        let __j = #j;
                        let __u = __j as usize;
                        if __j < 0.0 || __j.fract() != 0.0 || __u >= __el {
                            unsafe { *ok = 0; }
                            #trap
                        }
                        unsafe { *__ep.add(__u) }
                    }
                }
            }
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
                        // a read table already knows its length from the view,
                        // and an array of arrays read its length on entry
                        Kind::Table | Kind::Tables { .. } => {
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

    /// `t[k]` where `t` is an array of arrays: the slot and the index.
    fn elem_index(&self, e: &Expr) -> Option<(u16, Expr)> {
        let Expr::Index(obj, key) = e else { return None };
        let Expr::Local(b, _) = &**obj else { return None };
        match self.kind_of(b.slot) {
            Kind::Tables { .. } if self.known.contains(&b.slot) => Some((b.slot, (**key).clone())),
            _ => None,
        }
    }

    /// Is this expression a local the inference decided holds a table?
    fn is_table(&self, e: &Expr) -> bool {
        matches!(e, Expr::Local(b, _)
            if matches!(
                self.kinds.get(&b.slot),
                Some(Kind::Table) | Some(Kind::TableOut) | Some(Kind::Tables { .. })
            ))
    }

    fn kind_of(&self, slot: u16) -> Kind {
        self.kinds.get(&slot).copied().unwrap_or(Kind::Num)
    }

    /// Is `key` a loop variable we know indexes `table` safely?
    fn proven_in_range(&mut self, key: &Expr, table: u16) -> bool {
        // A constant index is settled before the body runs. Inside an element
        // of an array of arrays the runtime checked every element's length on
        // the way in; on a flat table one length check at entry does it, and
        // entry is before any write, so trapping there is safe.
        if let Expr::Num(n) = key {
            if !(*n >= 0.0 && n.fract() == 0.0 && *n < u32::MAX as f64) {
                return false;
            }
            let need = *n as u32 + 1;
            if let Some(outer) = self.inner_of.get(&table).copied() {
                return matches!(self.kind_of(outer), Kind::Tables { min, .. } if need <= min);
            }
            let bound = Literal::f64_suffixed(need as f64);
            let bound = quote! { #bound };
            if !self.hoisted.iter().any(|(t, b)| *t == table && b.to_string() == bound.to_string())
            {
                self.hoisted.push((table, bound));
            }
            return true;
        }
        let Expr::Local(b, _) = key else { return false };
        if self.in_range.iter().any(|(v, t)| *v == b.slot && *t == table) {
            return true;
        }
        // `for i in 0..n` reading `t[i]`: comparing `n` against the length of
        // `t` once, on entry, proves every read in the loop. Entry is before
        // any write, so trapping there is safe even for code that writes.
        let bound = self
            .bounded
            .iter()
            .find(|(v, _)| *v == b.slot)
            .map(|(_, bound)| bound.clone());
        match bound {
            Some(bound) => {
                if !self.hoisted.iter().any(|(t, b)| *t == table && b.to_string() == bound.to_string())
                {
                    self.hoisted.push((table, bound));
                }
                true
            }
            None => false,
        }
    }

    /// The bound of `for i in 0..n`, when it is something we can re-evaluate on
    /// entry: a literal, or a parameter the body leaves alone.
    fn hoistable_bound(
        &mut self,
        binding: Binding,
        start: &Expr,
        end: &Expr,
        inclusive: bool,
        body: &Block,
    ) -> Option<TokenStream> {
        if inclusive || binding.cell || writes_slot(body, binding.slot) {
            return None;
        }
        match start {
            Expr::Num(n) if *n >= 0.0 => {}
            _ => return None,
        }
        match end {
            Expr::Num(_) => self.expr(end).ok(),
            Expr::Local(b, _) if self.stable_params.contains(&b.slot) => self.expr(end).ok(),
            // `let n = t.len()` and then `for i in 0..n`: the bound is the
            // table's length, which entry can name without the local being in
            // scope there
            Expr::Local(b, _) => {
                let t = *self.len_of.get(&b.slot)?;
                match self.kind_of(t) {
                    Kind::Table | Kind::Tables { .. } => {
                        let (_, len) = span_idents(t);
                        Some(quote! { (#len as f64) })
                    }
                    _ => None,
                }
            }
            _ => None,
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
        // `math::sqrt(x)` is an f64 intrinsic, not a call: it cannot trap, so
        // it is allowed even where a real call is not.
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
            let sym = format_ident!("{}", self.self_symbol);
            // A table passes to the recursive call the way it does to any
            // other compiled one: as the address this frame already holds.
            // Handing it straight back is what lets a recursive function that
            // works on arrays compile at all.
            let kinds = self.self_param_kinds.clone();
            let mut cells = Vec::with_capacity(args.len());
            for (a, kind) in args.iter().zip(&kinds) {
                match kind {
                    Kind::Num => {
                        let v = self.expr(a)?;
                        cells.push(quote! { RtArg { num: #v, table: std::ptr::null_mut() } });
                    }
                    _ => match a {
                        Expr::Local(b, _)
                            if self.kind_of(b.slot) == *kind && self.known.contains(&b.slot) =>
                        {
                            let id = ident(b.slot);
                            cells.push(quote! { RtArg { num: 0.0, table: #id } });
                        }
                        _ => return Err("a table argument that is not the one held here".into()),
                    },
                }
            }
            let trap = self.on_trap.clone();
            return Ok(quote! {
                {
                    let __args = [#(#cells),*];
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
                            // an array of arrays passes the same way a flat
                            // table does: the address, and the callee checks
                            // its own requirements on the way in
                            Kind::Tables { .. } => match a {
                                Expr::Local(b, _)
                                    if self.kind_of(b.slot) == *kind
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
                            // a flag is never a parameter, so a call never
                            // passes one
                            Kind::Bool | Kind::Dead | Kind::TableOut => {
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
