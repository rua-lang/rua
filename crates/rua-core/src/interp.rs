//! Tree-walking interpreter. Hot functions get handed to the JIT (see jit.rs).

use rua_syntax::ast::*;
use rua_jit::{Callee, Jit, Kind, RtArg, RtCtx, RtHooks, SelfRef};
use std::collections::HashMap;
use crate::value::*;
use std::cell::RefCell;
use std::rc::Rc;

/// Non-local control flow. Errors travel the same channel so that `?` works.
pub enum Signal {
    /// Boxed so that `Result<Value, Signal>` stays small: this type is
    /// returned from every single expression evaluation.
    Err(Box<Error>),
    Break,
    Continue,
    /// The values are in `Vm::rets`: keeping them out of this type is what
    /// makes `Result<Value, Signal>` two words wide.
    Return,
}

impl From<Error> for Signal {
    fn from(e: Error) -> Self {
        Signal::Err(Box::new(e))
    }
}

impl From<String> for Signal {
    fn from(s: String) -> Self {
        Signal::Err(Box::new(Error(s)))
    }
}

impl Signal {
    pub(crate) fn into_error(self) -> Error {
        match self {
            Signal::Err(e) => *e,
            Signal::Break => Error("`break` outside of a loop"),
            Signal::Continue => Error("`continue` outside of a loop"),
            Signal::Return => Error("`return` outside of a function"),
        }
    }
}

pub type Eval<T> = Result<T, Signal>;

pub(crate) fn bad<T>(msg: impl Into<String>) -> Eval<T> {
    Err(Signal::Err(Box::new(Error(msg.into()))))
}

/// Iterations between checks on a running loop.
pub(crate) const LOOP_BATCH: u32 = 1000;

/// Total iterations, across every entry, before a loop is worth compiling.
///
/// Calling `rustc` costs tens of milliseconds, which buys roughly a million
/// interpreted iterations, so this waits for real evidence that the loop is
/// where the program lives. A loop that runs briefly but often usually sits in
/// a function that the ordinary call counter compiles anyway.
const LOOP_HOT: u64 = 5_000;

/// A loop counter set to this means "already compiled": the interpreter hands
/// the loop over at once rather than counting another thousand iterations
/// first. Counting is how a loop is *found*; once found, every iteration left
/// in it belongs to the compiled code.
pub(crate) const LOOP_READY: u32 = u32::MAX;

/// The state of one loop: hot enough yet, and the code if it compiled.
#[derive(Default)]
struct LoopEntry {
    iterations: u64,
    code: Option<rua_jit::CompiledLoop>,
    ctx: Option<RtCtxHolder>,
    blocked: bool,
}

/// Owns an [`RtCtx`] and the callee table it points at, so that the addresses
/// compiled code reads stay put for as long as the code can run.
pub struct RtCtxHolder {
    ctx: Box<RtCtx>,
    #[allow(dead_code)]
    callees: Box<[Callee]>,
    /// The field names compiled code reaches by. It holds handles into this,
    /// so the strings have to outlive the code and never move.
    #[allow(dead_code)]
    keys: Box<[RStr]>,
    #[allow(dead_code)]
    key_ptrs: Box<[*const std::ffi::c_void]>,
    /// Kept alive because the context points at its interior.
    #[allow(dead_code)]
    depth: Rc<std::cell::Cell<i64>>,
}

impl RtCtxHolder {
    fn new(
        hooks: RtHooks,
        callees: Vec<Callee>,
        keys: Vec<RStr>,
        depth: Rc<std::cell::Cell<i64>>,
    ) -> RtCtxHolder {
        let callees = callees.into_boxed_slice();
        let keys = keys.into_boxed_slice();
        let key_ptrs: Box<[*const std::ffi::c_void]> = keys
            .iter()
            .map(|k| k as *const RStr as *const std::ffi::c_void)
            .collect();
        let ctx = Box::new(RtCtx {
            len: hooks.len,
            get: hooks.get,
            span: hooks.span,
            push: hooks.push,
            set: hooks.set,
            inner: hooks.inner,
            span_mut: hooks.span_mut,
            inner_mut: hooks.inner_mut,
            spans: hooks.spans,
            spans_mut: hooks.spans_mut,
            note_append: hooks.note_append,
            new_table: hooks.new_table,
            push_table: hooks.push_table,
            field: hooks.field,
            set_field: hooks.set_field,
            keys: key_ptrs.as_ptr(),
            callees: callees.as_ptr(),
            // `Cell<i64>` is a transparent wrapper, so this is the counter
            depth: depth.as_ptr(),
            max_depth: MAX_DEPTH,
        });
        RtCtxHolder { ctx, callees, keys, key_ptrs, depth }
    }

    fn as_ptr(&self) -> *const RtCtx {
        &*self.ctx as *const RtCtx
    }
}

/// The three types that carry methods.
#[derive(Clone, Copy)]
pub enum MethodTable {
    Str = 0,
    Math = 1,
    Table = 2,
}

/// Slots in the library method cache. A power of two, and larger than the
/// number of builtin names a program calls in a loop.
const METHOD_IC: usize = 64;

/// A cached field read of a library table.
///
/// Deliberately its own function rather than a second call to
/// `Table::get_field_cached`: the interpreter's `t.field` handler wants that
/// one inlined into the dispatch loop, and giving the inliner a second, colder
/// call site is enough for it to stop — which costs every field read in the
/// program about 2%.
#[inline(never)]
fn cached_member(
    t: &Rc<RefCell<Table>>,
    name: &RStr,
    at: &std::cell::Cell<u32>,
) -> Value {
    t.borrow().get_field_cached(name, at)
}

pub struct Vm {
    /// Globals live in a flat array; `Expr::Global` caches the index it got.
    gvals: Vec<Value>,
    gnames: crate::hash::FxMap<Rc<str>, u32>,
    pub jit: Jit,
    /// Every local lives here: one contiguous run of registers per call.
    pub(crate) stack: Vec<Value>,
    pub(crate) base: usize,
    /// One past the highest register in use, which is where the next frame
    /// starts. `stack.len()` is the high-water mark, not the top.
    pub(crate) top: usize,
    /// Argument vectors, recycled: a call per node adds up fast.
    /// The values of the last call that produced "as many as there are".
    multi: Vec<Value>,
    /// Where the last multi-value call left its results, when they are still
    /// sitting in the registers it wrote them to: the stack index and how
    /// many. A call whose results feed straight into another call or a return
    /// — which is what a tail position is — then needs no vector at all.
    /// While this is set, `multi` is empty.
    pub(crate) multi_at: Option<(u32, u16)>,
    /// The function being executed, so that a hot loop can nominate it for
    /// compilation. Only tracked when the JIT is on, since that is the only
    /// The statement being executed, and the call stack above it, so that an
    /// error can say where it happened.
    pub(crate) line: u32,
    /// One entry per active call: the function, and the line it was called
    /// from. The pointer is valid because the call that pushed it is still on
    /// the Rust stack, holding the function alive.
    frames: Vec<(*const crate::bytecode::Proto, u32)>,
    /// Modules already loaded by `require`, keyed by canonical path.
    pub modules: HashMap<String, Value>,
    /// The directory of each file currently being loaded, innermost last.
    loading: Vec<Option<std::path::PathBuf>>,
    /// Compiled functions that inlined a call to a global, so that assigning to
    /// that global can throw their machine code away. Keyed by global slot, and
    /// walked transitively: a caller two levels up is calling that code too.
    jit_deps: HashMap<u32, Vec<std::rc::Weak<Function>>>,
    /// The same for compiled loops, which inline calls just as functions do.
    loop_deps: HashMap<u32, Vec<u32>>,
    /// Functions the JIT is busy with, so callee-first compilation terminates.
    compiling: std::collections::HashSet<usize>,
    /// Retired runtime contexts. Compiled code holds raw pointers to these, so
    /// they may never be freed while the process can still enter that code —
    /// which, since the shared objects are deliberately leaked, is forever.
    /// They are a few dozen bytes each and bounded by the number of compiles.
    retired_ctx: Vec<RtCtxHolder>,
    /// What the JIT knows about each loop it has seen, keyed by loop id.
    loops: HashMap<u32, LoopEntry>,
    /// `string`, `math` and `table`, kept to hand for method dispatch.
    libs: [Option<Rc<RefCell<Table>>>; 3],
    /// Where each library method was found last time, by name.
    ///
    /// `parts.push(x)` reaches its builtin through a hash probe of the library
    /// table, and a program that calls builtins in a loop does that probe on
    /// every iteration for a table that never changes. A position is checked
    /// by comparing the name stored there, which is one pointer comparison —
    /// names are interned — so a stale or colliding entry is a miss, never a
    /// wrong answer.
    method_ic: Box<[std::cell::Cell<u32>; METHOD_IC]>,
    /// Call depth, shared with compiled code through [`RtCtx`] so that native
    /// recursion is bounded by the same limit. Boxed because compiled code
    /// holds its address and the VM itself may move.
    depth: Rc<std::cell::Cell<i64>>,
}

/// How deep rua calls may nest.
///
/// A rua-to-rua call no longer costs a Rust stack frame — the VM keeps its own
/// frame stack — so this is a policy limit rather than a physical one. It still
/// has to hold for compiled code, which does recurse natively, and for natives
/// that call back into the interpreter.
const MAX_DEPTH: i64 = 1000;

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vm {
    /// A VM with the standard library loaded.
    pub fn new() -> Vm {
        let mut vm = Vm::bare();
        crate::stdlib::install(&mut vm);
        vm
    }

    /// A VM with nothing in it, for embedders who want a locked down sandbox.
    pub fn bare() -> Vm {
        Vm {
            gvals: Vec::new(),
            gnames: crate::hash::FxMap::default(),
            jit: Jit::new(),
            stack: Vec::with_capacity(256),
            base: 0,
            top: 0,
            multi: Vec::new(),
            multi_at: None,
            line: 0,
            frames: Vec::new(),
            modules: HashMap::new(),
            loading: Vec::new(),
            jit_deps: HashMap::new(),
            loop_deps: HashMap::new(),
            compiling: std::collections::HashSet::new(),
            retired_ctx: Vec::new(),
            loops: HashMap::new(),
            libs: [None, None, None],
            method_ic: Box::new([const { std::cell::Cell::new(u32::MAX) }; METHOD_IC]),
            depth: Rc::new(std::cell::Cell::new(0)),
        }
    }

    // ---- embedding API (the Rust side) --------------------------------------

    /// The slot a global lives in, creating it (as nil) if it is new. Slots
    /// are never reused, which is what makes the inline caches sound.
    fn global_slot(&mut self, name: &Rc<str>) -> u32 {
        if let Some(i) = self.gnames.get(name) {
            return *i;
        }
        let i = self.gvals.len() as u32;
        self.gvals.push(Value::Nil);
        self.gnames.insert(name.clone(), i);
        i
    }

    pub fn set_global(&mut self, name: &str, v: Value) {
        let i = match self.gnames.get(name) {
            Some(i) => *i,
            None => self.global_slot(&Rc::from(name)),
        };
        self.write_global(i, v);
    }

    /// Store a global, and drop the machine code of everything that compiled a
    /// call to its old value — including callers of those callers, since a
    /// direct call jumps at a fixed address that will not be revisited.
    fn write_global(&mut self, slot: u32, v: Value) {
        let same = matches!((&self.gvals[slot as usize], &v), (Value::Func(a), Value::Func(b)) if Rc::ptr_eq(a, b));
        self.gvals[slot as usize] = v;
        if same {
            return;
        }
        self.invalidate(slot);
    }

    /// Throw away every piece of compiled code that can reach this global,
    /// following the dependency edges to a fixed point.
    fn invalidate(&mut self, slot: u32) {
        let mut queue = vec![slot];
        let mut seen = std::collections::HashSet::new();
        while let Some(slot) = queue.pop() {
            if !seen.insert(slot) {
                continue;
            }
            for id in self.loop_deps.remove(&slot).unwrap_or_default() {
                if let Some(entry) = self.loops.get_mut(&id) {
                    entry.code = None;
                    entry.iterations = 0;
                    if let Some(ctx) = entry.ctx.take() {
                        self.retired_ctx.push(ctx);
                    }
                }
            }
            for weak in self.jit_deps.remove(&slot).unwrap_or_default() {
                let Some(f) = weak.upgrade() else { continue };
                if f.jit.get().is_none() {
                    continue;
                }
                f.jit.set(None);
                f.jit_state.set(JitState::Cold);
                f.hits.set(0);
                if let Some(ctx) = f.rt.borrow_mut().take() {
                    self.retired_ctx.push(ctx);
                }
                // whoever inlined *this* function is now calling dead code too
                let name = f.proto.name.clone();
                if let Some(s) = self.gnames.get(&*name).copied() {
                    queue.push(s);
                }
            }
        }
    }

    pub fn get_global(&self, name: &str) -> Value {
        match self.gnames.get(name) {
            Some(i) => self.gvals[*i as usize].clone(),
            None => Value::Nil,
        }
    }

    /// Every global that has been defined, in definition order.
    pub fn global_names(&self) -> Vec<Rc<str>> {
        let mut names: Vec<(u32, Rc<str>)> =
            self.gnames.iter().map(|(k, v)| (*v, k.clone())).collect();
        names.sort_by_key(|(i, _)| *i);
        names.into_iter().map(|(_, n)| n).collect()
    }

    /// Remember a type's method table: `string`, `math` or `table`.
    pub fn set_method_table(&mut self, kind: MethodTable, t: Rc<RefCell<Table>>) {
        self.libs[kind as usize] = Some(t);
    }

    fn lib(&self, kind: MethodTable) -> Option<&Rc<RefCell<Table>>> {
        self.libs[kind as usize].as_ref()
    }

    /// Register a Rust function as a rua global.
    pub fn register<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&mut Vm, &[Value]) -> Res<Vec<Value>> + 'static,
    {
        let v = Value::Native(Rc::new(Native::new(name, f)));
        self.set_global(name, v);
    }

    /// Expose a Rust function of one argument, which most builtins are.
    pub fn register_unary<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&Value) -> Res<Value> + Clone + 'static,
    {
        let v = Value::Native(Rc::new(Native::unary(name, f)));
        self.set_global(name, v);
    }

    /// Run source text; the value of the final expression is returned.
    /// Compile `src` and describe the bytecode, for `rua --dump-bytecode`.
    ///
    /// Reading the op mix of a real program is how the compiler's output gets
    /// better: a benchmark that turns out to spend a fifth of its ops moving
    /// registers around is telling you where to look.
    pub fn dump_bytecode(&mut self, src: &str) -> Res<String> {
        let (block, n_slots) = rua_syntax::compile(src).map_err(|e| Error {
            message: e.message,
            line: e.line,
            located: e.line > 0,
            where_: None,
            trace: Vec::new(),
        })?;
        let def = Rc::new(rua_syntax::ast::FuncDef {
            id: usize::MAX,
            name: String::from("<chunk>"),
            params: Vec::new(),
            body: block,
            line: 0,
            n_slots,
            param_bindings: Vec::new(),
            upvals: Vec::new(),
        });
        let proto = crate::compile::compile_chunk(&def.body, n_slots, def.clone());
        let mut out = String::new();
        write_proto(&mut out, &proto);
        Ok(out)
    }

    pub fn eval(&mut self, src: &str) -> Res<Vec<Value>> {
        let (block, n_slots) = rua_syntax::compile(src).map_err(|e| Error {
            message: e.message,
            line: e.line,
            located: e.line > 0,
            where_: None,
            trace: Vec::new(),
        })?;
        // A chunk is a function of no arguments, compiled the same way. It
        // keeps its syntax, so the JIT can still find the loops inside it.
        let def = Rc::new(rua_syntax::ast::FuncDef {
            id: usize::MAX,
            name: String::new(),
            params: Vec::new(),
            body: block,
            line: 0,
            n_slots,
            param_bindings: Vec::new(),
            upvals: Vec::new(),
        });
        let proto = crate::compile::compile_chunk(&def.body, n_slots, def.clone());
        let chunk = Rc::new(Function {
            proto,
            param_kinds: RefCell::new(Vec::new()),
            rt: RefCell::new(None),
            returns_nil: std::cell::Cell::new(false),
            returns_table: std::cell::Cell::new(false),
            upvals: Rc::new(Vec::new()),
            hits: std::cell::Cell::new(0),
            jit: std::cell::Cell::new(None),
            // a chunk runs once: never worth compiling as a whole
            jit_state: std::cell::Cell::new(JitState::Blocked),
        });
        let args = self.take_vec(0);
        self.run(&chunk, args).map_err(|s| s.into_error())
    }

    pub fn eval_file(&mut self, path: &str) -> Res<Vec<Value>> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| Error(format!("cannot read {path}: {e}")))?;
        // Where a file is is how it finds what it requires, so a library can
        // sit beside the script that uses it and both can be run from
        // anywhere.
        let dir = std::path::Path::new(path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf());
        self.loading.push(dir);
        // a chunk is a call like any other: one file loading another must not
        // walk off the end of the stack
        let depth = self.enter_depth().map_err(|s| s.into_error());
        let out = match depth {
            Ok(_) => {
                let out = self.eval(&src);
                self.leave_depth();
                out
            }
            Err(e) => Err(e),
        };
        self.loading.pop();
        out
    }

    /// Say where the script being run lives, so that what it requires can sit
    /// beside it. An embedder that has no file need not call this.
    pub fn set_script(&mut self, path: &str) {
        let dir = std::path::Path::new(path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf());
        self.loading.clear();
        self.loading.push(dir);
    }

    /// Where to look for something a script asks for by relative path: beside
    /// the file doing the asking, then the working directory.
    pub fn resolve_path(&self, path: &str) -> String {
        let given = std::path::Path::new(path);
        if given.is_absolute() || given.exists() {
            return path.to_string();
        }
        let with_ext = |p: std::path::PathBuf| {
            if p.exists() {
                return Some(p);
            }
            let ext = p.with_extension("rua");
            ext.exists().then_some(ext)
        };
        if let Some(Some(dir)) = self.loading.last() {
            if let Some(found) = with_ext(dir.join(given)) {
                return found.to_string_lossy().into_owned();
            }
        }
        match with_ext(given.to_path_buf()) {
            Some(found) => found.to_string_lossy().into_owned(),
            None => path.to_string(),
        }
    }

    pub fn call(&mut self, f: &Value, args: Vec<Value>) -> Res<Vec<Value>> {
        self.call_value(f, args).map_err(|s| s.into_error())
    }

    // ---- blocks and statements ----------------------------------------------

    /// Stamp an error with the line it happened on, once.
    /// Remember which line we are on, so a call frame can say where it came
    /// from in a traceback.
    #[inline]
    pub(crate) fn set_line(&mut self, line: u32) {
        self.line = line;
    }

    pub(crate) fn locate_at(&self, line: u32, e: Error) -> Signal {
        self.locate_signal(line, Signal::Err(Box::new(e)))
    }

    pub(crate) fn locate_signal(&self, line: u32, sig: Signal) -> Signal {
        match sig {
            Signal::Err(e) if !e.located => Signal::Err(Box::new(Error {
                message: e.message.clone(),
                located: true,
                line,
                where_: self.frames.last().map(|(p, _)| frame_name(*p)).filter(|n| !n.is_empty()),
                trace: self
                    .frames
                    .iter()
                    .map(|(p, line)| (frame_name(*p), *line))
                    .collect(),
            })),
            other => other,
        }
    }

    /// A loop has gone round another batch of iterations. Returns whether the
    /// JIT took it over and ran it to completion.
    pub(crate) fn note_loop(
        &mut self,
        proto: &crate::bytecode::Proto,
        running: &Rc<Function>,
        id: u32,
    ) -> bool {
        if !self.jit.enabled {
            return false;
        }
        // A function called a handful of times but looping hard inside is
        // exactly as worth compiling as one called ten thousand times. The call
        // counter cannot see that; the loop can. This activation stays
        // interpreted (or is taken over by the loop below), but the next call
        // gets the compiled function.
        if running.jit_state.get() == JitState::Cold {
            let f = running.clone();
            self.try_compile(&f);
        }
        {
            let entry = self.loops.entry(id).or_default();
            entry.iterations = entry.iterations.saturating_add(LOOP_BATCH as u64);
            if entry.blocked {
                return false;
            }
            if entry.code.is_none() && entry.iterations < LOOP_HOT {
                return false;
            }
        }
        // the JIT works from the syntax, so find the loop this hint came from
        let Some(stat) = find_loop(&proto.def.body, id).cloned() else {
            self.loops.entry(id).or_default().blocked = true;
            return false;
        };
        self.consider_loop(id, &stat).unwrap_or(false)
    }

    /// Hand a loop the JIT has already taken to its compiled code.
    pub(crate) fn enter_loop(&mut self, id: u32) -> bool {
        self.run_compiled_loop(id, &Stat::Break).unwrap_or(false)
    }

    /// Run a loop that is already compiled, if its locals are all numbers.
    /// Returns whether the compiled code ran (and so the loop is finished).
    fn run_compiled_loop(&mut self, id: u32, st: &Stat) -> Eval<bool> {
        if !self.loops.get(&id).map(|e| e.code.is_some()).unwrap_or(false) {
            return Ok(false);
        }
        let entry = self.loops.remove(&id).expect("checked above");
        let ran = self.enter_compiled_loop(&entry);
        self.loops.insert(id, entry);
        let _ = st;
        Ok(ran)
    }

    /// A loop has run hot: compile it, then jump straight into the machine code
    /// from the state the interpreter is in — on-stack replacement.
    fn consider_loop(&mut self, id: u32, st: &Stat) -> Eval<bool> {
        if !self.jit.enabled {
            return Ok(false);
        }
        {
            let entry = self.loops.entry(id).or_default();
            entry.iterations = entry.iterations.saturating_add(LOOP_BATCH as u64);
            if entry.blocked || entry.iterations < LOOP_HOT {
                return Ok(false);
            }
        }
        if self.loops[&id].code.is_none() {
            let req = self.self_ref_for_loop();
            match self.jit.compile_loop(st, req) {
                Ok(compiled) => {
                    let ctx = self.build_ctx(&compiled.inlined, &compiled.keys);
                    for name in &compiled.inlined {
                        if let Some(slot) = self.gnames.get(name.as_str()).copied() {
                            self.loop_deps.entry(slot).or_default().push(id);
                        }
                    }
                    let entry = self.loops.get_mut(&id).expect("present");
                    entry.ctx = Some(ctx);
                    entry.code = Some(compiled);
                }
                Err(_) => {
                    self.loops.get_mut(&id).expect("present").blocked = true;
                    return Ok(false);
                }
            }
        }
        self.run_compiled_loop(id, st)
    }

    /// Marshal the loop's locals into registers, run it, and copy them back.
    fn enter_compiled_loop(&mut self, entry: &LoopEntry) -> bool {
        let Some(compiled) = &entry.code else { return false };
        let mut regs = Vec::with_capacity(compiled.slots.len());
        for (slot, kind) in compiled.slots.iter().zip(&compiled.kinds) {
            // a local that does not hold what the compiled code expects
            // leaves this time round interpreted
            match rt_arg(&self.stack[self.base + *slot as usize], kind) {
                Some(a) => regs.push(a),
                None => return false,
            }
        }
        // as in `compiled_args`: never read a view of a table this code writes
        for (i, ki) in compiled.kinds.iter().enumerate() {
            if *ki != Kind::TableOut {
                continue;
            }
            for (j, kj) in compiled.kinds.iter().enumerate() {
                if *kj == Kind::Table && regs[i].table == regs[j].table {
                    return false;
                }
            }
        }
        let Some(ctx) = entry.ctx.as_ref().map(|c| c.as_ptr()) else { return false };
        let mut ok: i32 = 1;
        // SAFETY: `regs` has one entry per slot the loop was compiled for, of
        // the kind it was compiled for, every table pointer is live here, and
        // `ctx` is the context built for this loop.
        let mark = dirty_mark();
        unsafe { (compiled.code)(regs.as_mut_ptr(), ctx, &mut ok) };
        // as above: a trap gives up everything the loop wrote
        unsafe { settle_dirty(mark, ok != 0) };
        if ok == 0 {
            return false;
        }
        for (i, (slot, kind)) in compiled.slots.iter().zip(&compiled.kinds).enumerate() {
            match kind {
                Kind::Num | Kind::Dead => {
                    self.stack[self.base + *slot as usize] = Value::Num(regs[i].num)
                }
                // a flag goes back as the boolean it is: in rua every number
                // is true, so handing back 0.0 would not mean false
                Kind::Bool => {
                    self.stack[self.base + *slot as usize] = Value::Bool(regs[i].num != 0.0)
                }
                _ => {}
            }
        }
        true
    }

    /// Build the context a piece of compiled code runs with: the runtime hook
    /// addresses, and the entry points of the functions it calls directly.
    fn build_ctx(&self, inlined: &[String], keys: &[String]) -> RtCtxHolder {
        let callees: Vec<Callee> = inlined
            .iter()
            .map(|name| match self.get_global(name) {
                Value::Func(g) => match (g.jit.get(), g.rt.borrow().as_ref()) {
                    (Some(code), Some(ctx)) => {
                        Callee { entry: code.address(), ctx: ctx.as_ptr() }
                    }
                    // should not happen: callees are compiled first
                    _ => Callee { entry: 0, ctx: std::ptr::null() },
                },
                _ => Callee { entry: 0, ctx: std::ptr::null() },
            })
            .collect();
        let keys: Vec<RStr> = keys.iter().map(|k| RStr::new(k)).collect();
        RtCtxHolder::new(hooks(), callees, keys, self.depth.clone())
    }

    /// Globals that already hold compiled code, with what their parameters
    /// have to be — a direct call can only pass numbers.
    fn compiled_globals(&self) -> HashMap<String, rua_jit::Callable> {
        let mut out = HashMap::new();
        for (name, slot) in &self.gnames {
            if let Value::Func(g) = &self.gvals[*slot as usize] {
                // A direct call reads the callee's result out of the `f64` it
                // returns, and a table does not travel there.
                if g.returns_table.get() {
                    continue;
                }
                if let Some(code) = g.jit.get() {
                    out.insert(
                        name.to_string(),
                        rua_jit::Callable {
                            addr: code.address(),
                            kinds: g.param_kinds.borrow().clone(),
                            // the syntax, so a small callee can be compiled
                            // into the caller's object and inlined there
                            def: Some(g.def().clone()),
                        },
                    );
                }
            }
        }
        out
    }

    /// Loops may call already compiled global functions directly, too.
    fn self_ref_for_loop(&self) -> SelfRef {
        SelfRef {
            upval: None,
            global: None,
            compiled_globals: self.compiled_globals(),
            hooks: hooks(),
        }
    }

    /// Reserve a frame's registers. The stack only ever grows: frames clear
    /// their own registers on the way out, so a new frame starts on nils
    /// without paying to fill them again.
    pub(crate) fn open_frame(&mut self, n_regs: usize) -> usize {
        let base = self.top;
        self.open_frame_at(base, n_regs);
        base
    }

    /// Open a frame at a chosen place on the stack, rather than above
    /// everything.
    ///
    /// A call from inside the interpreter puts the callee's frame exactly on
    /// top of the arguments the caller built, which is where the callee's
    /// parameters live: the arguments are then already in place and the call
    /// copies nothing. Everything above a call's base is dead by construction
    /// — the compiler allocates it above every live local and temporary — so
    /// the two frames may overlap.
    pub(crate) fn open_frame_at(&mut self, base: usize, n_regs: usize) {
        let need = base + n_regs;
        if self.stack.len() < need {
            self.stack.resize(need, Value::Nil);
        }
        self.top = need;
    }

    /// Give a frame's registers back, dropping whatever they held.
    ///
    /// The frame reaches from `base` to the top of the stack: a call that
    /// opened a frame above this one closed it again on the way out, so `top`
    /// is where this frame ends and the callee's register count need not be
    /// carried around to find it.
    pub(crate) fn close_frame(&mut self, base: usize) {
        self.close_frame_from(base, base)
    }

    /// Release a frame, leaving the results already written into it alone.
    ///
    /// A callee's frame starts one register above where its results go, so
    /// everything the caller asked for beyond the first value lands inside the
    /// frame that is about to be released. Those registers belong to the
    /// caller now.
    pub(crate) fn close_frame_from(&mut self, base: usize, from: usize) {
        for slot in &mut self.stack[from.max(base)..self.top] {
            // A frame is sized for the widest path through it and most calls
            // take a narrow one, so most of the window being released is
            // already nil — it was left that way by whoever released it last.
            // Testing for that is a load and a branch; writing over it is a
            // load, a branch and two stores.
            if !matches!(slot, Value::Nil) {
                Value::put(slot, Value::Nil);
            }
        }
        self.top = base;
    }

    /// Enter a call, refusing to go deeper than the stack allows.
    /// Push a traceback entry for a call the VM is entering itself.
    pub(crate) fn push_frame(&mut self, proto: *const crate::bytecode::Proto, line: u32) {
        self.frames.push((proto, line));
    }

    pub(crate) fn pop_frame(&mut self) {
        self.frames.pop();
    }

    #[inline]
    pub(crate) fn enter_depth(&mut self) -> Eval<bool> {
        let d = self.depth.get() + 1;
        self.depth.set(d);
        if d > MAX_DEPTH {
            self.depth.set(d - 1);
            return bad("stack overflow");
        }
        Ok(true)
    }

    #[inline]
    pub(crate) fn leave_depth(&mut self) {
        self.depth.set(self.depth.get() - 1);
    }


    /// Try to satisfy a call with compiled code. Returns whether it did.
    ///
    /// Also does the profiling that decides when to compile, so it runs on
    /// every call whether or not the JIT is on.
    pub(crate) fn try_compiled_call(
        &mut self,
        func: &Rc<Function>,
        arg_start: usize,
        nargs: u16,
        ret_to: usize,
        nres: u16,
    ) -> bool {
        let hits = func.hits.get().saturating_add(1);
        func.hits.set(hits);
        if func.jit_state.get() == JitState::Cold && hits >= self.jit.threshold {
            self.try_compile(func);
        }
        let Some(code) = func.jit.get() else { return false };
        let kinds = func.param_kinds.borrow();
        if kinds.len() != nargs as usize {
            return false;
        }
        let Some(rt_args) =
            compiled_args(&self.stack[arg_start..arg_start + nargs as usize], &kinds)
        else {
            return false;
        };
        let Some(ctx) = func.rt.borrow().as_ref().map(|c| c.as_ptr()) else { return false };
        drop(kinds);
        // A trap means the compiled code met something it does not handle. The
        // interpreter then runs the call from the start, so anything compiled
        // code wrote has to go: it wrote through the numeric views, and
        // throwing those away leaves the tables as they were.
        // SAFETY: this is the context built for this code, and every table it
        // wrote through is alive until this call returns.
        let mark = dirty_mark();
        let Some((n, made)) = (unsafe { code.call(&rt_args, ctx) }) else {
            unsafe { settle_dirty(mark, false) };
            return false;
        };
        // A table the call made becomes a value here, while the scratch list
        // still holds it — settling is what lets go.
        let v = if func.returns_table.get() {
            if made.is_null() {
                unsafe { settle_dirty(mark, false) };
                return false;
            }
            Value::Table(unsafe { rc_of(made) })
        } else if func.returns_nil.get() {
            Value::Nil
        } else {
            Value::Num(n)
        };
        unsafe { settle_dirty(mark, true) };
        match nres {
            0 => {}
            crate::bytecode::MULTI => {
                self.stack[ret_to] = v.clone();
                let mut vals = self.take_vec(1);
                vals.push(v);
                self.set_multi(vals);
            }
            want => {
                self.stack[ret_to] = v;
                for i in 1..want as usize {
                    self.stack[ret_to + i] = Value::Nil;
                }
            }
        }
        true
    }

    /// Run a function, using its compiled code when that applies.
    pub(crate) fn call_compiled_or_run(
        &mut self,
        func: &Rc<Function>,
        arg_start: usize,
        nargs: u16,
        ret_to: usize,
        nres: u16,
    ) -> Eval<()> {
        if self.try_compiled_call(func, arg_start, nargs, ret_to, nres) {
            return Ok(());
        }
        self.frames.push((Rc::as_ptr(&func.proto), self.line));
        let out = self.run_into(func, arg_start, nargs, ret_to, nres);
        self.frames.pop();
        out
    }

    /// Hand a finished vector back for reuse.
    pub(crate) fn recycle_vec(&mut self, v: Vec<Value>) {
        recycle_vec(v)
    }

    /// Take the values of the last multi-value call, as a vector. Results
    /// left in registers are copied out here, which is why the paths that can
    /// consume them where they lie do so before calling this.
    pub(crate) fn take_multi(&mut self) -> Vec<Value> {
        if let Some((at, n)) = self.multi_at.take() {
            let mut out = self.take_vec(n as usize);
            for i in 0..n as usize {
                out.push(self.stack[at as usize + i].clone());
            }
            return out;
        }
        let empty = self.take_vec(0);
        std::mem::replace(&mut self.multi, empty)
    }

    pub(crate) fn set_multi(&mut self, vals: Vec<Value>) {
        self.multi_at = None;
        let old = std::mem::replace(&mut self.multi, vals);
        self.recycle_vec(old);
    }

    /// Publish results that are already in registers.
    pub(crate) fn set_multi_at(&mut self, at: usize, n: u16) {
        if !self.multi.is_empty() {
            let old = std::mem::take(&mut self.multi);
            self.recycle_vec(old);
        }
        self.multi_at = Some((at as u32, n));
    }

    /// Where the last call's results are, if they are still in registers.
    pub(crate) fn multi_in_regs(&self) -> Option<(usize, u16)> {
        self.multi_at.map(|(at, n)| (at as usize, n))
    }

    /// The global slot a chunk's reference points at, resolving it once.
    pub(crate) fn global_ref(&mut self, proto: &crate::bytecode::Proto, g: u16) -> u32 {
        let entry = &proto.globals[g as usize];
        match entry.slot.get() {
            u32::MAX => {
                let slot = self.global_slot(&entry.name);
                entry.slot.set(slot);
                slot
            }
            slot => slot,
        }
    }

    /// A resolved global, without taking a reference count on it. The pointer
    /// is only good until the next write to the global table, which is why the
    /// callers of this read through it and let go.
    #[inline]
    pub(crate) fn global_ptr(&self, slot: u32) -> *const Value {
        debug_assert!((slot as usize) < self.gvals.len());
        // SAFETY: as in `global_resolved`.
        unsafe { self.gvals.as_ptr().add(slot as usize) }
    }

    /// The same, for a slot that has already been resolved.
    ///
    /// Slots are handed out from `gvals`'s length and the table never shrinks,
    /// so a slot that once existed still does: the bounds check is a load and
    /// a branch on the hottest instruction in most programs.
    #[inline]
    pub(crate) fn global_resolved(&self, slot: u32) -> Value {
        debug_assert!((slot as usize) < self.gvals.len());
        // SAFETY: as above.
        unsafe { self.gvals.get_unchecked(slot as usize).clone() }
    }

    pub(crate) fn store_global(&mut self, slot: u32, v: Value) {
        self.write_global(slot, v);
    }

    /// Build a closure over the current frame: captured locals are shared
    /// through their cells, and upvalues are passed straight down.
    pub(crate) fn make_closure(
        &mut self,
        proto: Rc<crate::bytecode::Proto>,
        upvals: &[CellRef],
    ) -> Value {
        let mut cells = Vec::with_capacity(proto.def.upvals.len());
        for src in &proto.def.upvals {
            cells.push(match src {
                UpvalSrc::ParentLocal(slot) => match &self.stack[self.base + *slot as usize] {
                    Value::Cell(c) => c.clone(),
                    other => Rc::new(RefCell::new(other.clone())),
                },
                UpvalSrc::ParentUpval(i) => upvals[*i as usize].clone(),
            });
        }
        Value::Func(Rc::new(Function {
            proto,
            param_kinds: RefCell::new(Vec::new()),
            rt: RefCell::new(None),
            returns_nil: std::cell::Cell::new(false),
            returns_table: std::cell::Cell::new(false),
            upvals: Rc::new(cells),
            hits: std::cell::Cell::new(0),
            jit: std::cell::Cell::new(None),
            jit_state: std::cell::Cell::new(JitState::Cold),
        }))
    }

    pub(crate) fn take_vec(&mut self, cap: usize) -> Vec<Value> {
        take_vec(cap)
    }

    /// `a[k]` and `a::k`: a plain lookup, no method fallback.
    pub fn index(&self, o: &Value, k: &Value) -> Res<Value> {
        match o {
            Value::Table(t) => Ok(t.borrow().get(&Key::from_value(k)?)),
            // a library member is always reached by name
            Value::Str(_) | Value::Num(_) => {
                let kind = if matches!(o, Value::Str(_)) {
                    MethodTable::Str
                } else {
                    MethodTable::Math
                };
                Ok(match k {
                    Value::Str(name) => self.lib_member(kind, name),
                    _ => Value::Nil,
                })
            }
            Value::Cell(c) => {
                let inner = c.borrow().clone();
                self.index(&inner, k)
            }
            other => err(format!("cannot index a {} value", other.type_name())),
        }
    }

    fn lib_member(&self, kind: MethodTable, name: &RStr) -> Value {
        match self.lib(kind) {
            Some(t) => {
                let slot = (name.hash_bits() as usize).wrapping_add(kind as usize)
                    & (METHOD_IC - 1);
                cached_member(t, name, &self.method_ic[slot])
            }
            None => Value::Nil,
        }
    }

    /// `a.m(..)`: the receiver's own field first, then its type's library —
    /// this is what makes `[3,1,2].sort()` and `"ab".upper()` work.
    pub(crate) fn method(&self, o: &Value, name: &RStr) -> Res<Value> {
        let kind = match o {
            Value::Table(t) => {
                let own = t.borrow().get_field(name);
                if !matches!(own, Value::Nil) {
                    return Ok(own);
                }
                MethodTable::Table
            }
            Value::Str(_) => MethodTable::Str,
            Value::Num(_) => MethodTable::Math,
            other => {
                return err(format!("a {} value has no method `{name}`", other.type_name()))
            }
        };
        match self.lib_member(kind, name) {
            Value::Nil => err(format!("no method `{name}` on a {} value", o.type_name())),
            v => Ok(v),
        }
    }

    pub(crate) fn call_value(&mut self, f: &Value, args: Vec<Value>) -> Eval<Vec<Value>> {
        self.enter_depth()?;
        let out = self.call_inner(f, args);
        self.leave_depth();
        out
    }

    /// Hand a hot function to the JIT, and remember the answer either way.
    fn try_compile(&mut self, func: &Rc<Function>) {
        if !self.jit.enabled {
            func.jit_state.set(JitState::Blocked);
            return;
        }
        // Compile the helpers this function calls first: a direct call can only
        // be generated to machine code that already exists. The guard set stops
        // mutual recursion from looping here.
        if self.compiling.insert(func.def().id) {
            for name in rua_jit::called_globals(func.def()) {
                let callee = self.get_global(&name);
                if let Value::Func(g) = callee {
                    if g.jit_state.get() == JitState::Cold && !self.compiling.contains(&g.def().id) {
                        self.try_compile(&g);
                    }
                }
            }
        }
        self.compiling.remove(&func.def().id);
        // Which upvalue, if any, currently holds this very function? That is
        // what `fn fib(n) { .. fib(n - 1) .. }` looks like after resolution,
        // and it is the only call the JIT is willing to compile.
        let upval = func
            .upvals
            .iter()
            .position(|c| matches!(&*c.borrow(), Value::Func(g) if Rc::ptr_eq(g, func)))
            .map(|i| i as u16);
        // a top level `fn` is a global, and recursion goes through that name
        let global = match self.get_global(&func.def().name) {
            Value::Func(g) if Rc::ptr_eq(&g, func) => Some(func.def().name.clone()),
            _ => None,
        };
        // globals that already hold compiled functions can be called directly
        let compiled_globals = self.compiled_globals();
        let req = SelfRef { upval, global, compiled_globals, hooks: hooks() };
        match self.jit.compile(func.def(), req) {
            Ok(out) => {
                let ctx = self.build_ctx(&out.inlined, &out.keys);
                func.jit.set(Some(out.code));
                *func.param_kinds.borrow_mut() = out.param_kinds;
                func.returns_nil.set(out.returns_nil);
                func.returns_table.set(out.returns_table);
                if let Some(old) = func.rt.borrow_mut().replace(ctx) {
                    // someone else's compiled code may still call through it
                    self.retired_ctx.push(old);
                }
                func.jit_state.set(JitState::Compiled);
                for name in out.inlined {
                    if let Some(slot) = self.gnames.get(name.as_str()).copied() {
                        self.jit_deps.entry(slot).or_default().push(Rc::downgrade(func));
                    }
                }
            }
            Err(_) => func.jit_state.set(JitState::Blocked),
        }
    }

    fn call_inner(&mut self, f: &Value, args: Vec<Value>) -> Eval<Vec<Value>> {
        match f {
            Value::Native(n) => {
                let n = n.clone();
                let out = (n.f)(self, &args)?;
                self.recycle_vec(args);
                Ok(out)
            }
            Value::Func(func) => {
                // profile, then let the JIT have a look
                let hits = func.hits.get().saturating_add(1);
                func.hits.set(hits);
                if func.jit_state.get() == JitState::Cold && hits >= self.jit.threshold {
                    self.try_compile(func);
                }
                if let Some(code) = func.jit.get() {
                    if let Some(rt_args) = compiled_args(&args, &func.param_kinds.borrow()) {
                        let ctx = func.rt.borrow().as_ref().map(|c| c.as_ptr());
                        if let Some(ctx) = ctx {
                            // A trap means the compiled code met something it
                            // does not handle. Settling gives back everything
                            // it wrote and everything it made, so the
                            // interpreter can simply run the call instead.
                            // SAFETY: this is the context built for this code.
                            let mark = dirty_mark();
                            let out = unsafe { code.call(&rt_args, ctx) };
                            let v = match out {
                                // the table it made, while the scratch list
                                // still holds it
                                Some((_, made)) if func.returns_table.get() => {
                                    if made.is_null() {
                                        None
                                    } else {
                                        Some(Value::Table(unsafe { rc_of(made) }))
                                    }
                                }
                                Some(_) if func.returns_nil.get() => Some(Value::Nil),
                                Some((n, _)) => Some(Value::Num(n)),
                                None => None,
                            };
                            unsafe { settle_dirty(mark, v.is_some()) };
                            if let Some(v) = v {
                                self.recycle_vec(args);
                                return Ok(vec![v]);
                            }
                        }
                    }
                }
                self.frames.push((Rc::as_ptr(&func.proto), self.line));
                let out = self.run(func, args);
                self.frames.pop();
                out
            }
            Value::Cell(c) => {
                let inner = c.borrow().clone();
                self.call_inner(&inner, args)
            }
            other => bad(format!("cannot call a {} value", other.type_name())),
        }
    }

}

/// `==` and `!=` on two values that are not both numbers.
///
/// Equality never coerces and never fails, so it needs no ownership of its
/// operands — and the general path takes its arguments by value, which meant
/// a reference count up and back down on each side of every comparison
/// between two tables or two strings. A program that walks a data structure
/// does that in its inner loop.
#[inline]
pub fn equality(op: crate::bytecode::BinKind, l: &Value, r: &Value) -> Option<bool> {
    match op {
        crate::bytecode::BinKind::Eq => Some(l == r),
        crate::bytecode::BinKind::Ne => Some(l != r),
        _ => None,
    }
}

/// The general form of a binary operator: everything the specialised numeric
/// paths in the interpreter loop decline to handle.
///
/// It borrows both operands. Nothing here keeps either of them — a comparison
/// reads them, a concatenation copies the bytes out — and taking them by value
/// meant the loop cloned two registers, and dropped them again, on every
/// operation that was not two numbers.
pub fn arith(op: crate::bytecode::BinKind, l: &Value, r: &Value) -> Res<Value> {
    use crate::bytecode::BinKind::*;
    Ok(match op {
        // `+` concatenates when either side is a string, as in Rust's String + &str
        Add => match (l, r) {
            (Value::Str(a), b) => Value::str(format!("{a}{b}")),
            (a, Value::Str(b)) => Value::str(format!("{a}{b}")),
            _ => Value::Num(l.as_num()? + r.as_num()?),
        },
        Sub => Value::Num(l.as_num()? - r.as_num()?),
        Mul => Value::Num(l.as_num()? * r.as_num()?),
        Div => Value::Num(l.as_num()? / r.as_num()?),
        Rem => {
            let (a, b) = (l.as_num()?, r.as_num()?);
            Value::Num(a - (a / b).floor() * b)
        }
        Eq => Value::Bool(l == r),
        Ne => Value::Bool(l != r),
        Lt | Le | Gt | Ge => {
            let ord = match (l, r) {
                (Value::Str(a), Value::Str(b)) => a.cmp(b),
                _ => l
                    .as_num()?
                    .partial_cmp(&r.as_num()?)
                    .ok_or_else(|| Error("comparison with NaN"))?,
            };
            use std::cmp::Ordering::*;
            Value::Bool(match op {
                Lt => ord == Less,
                Le => ord != Greater,
                Gt => ord == Greater,
                _ => ord != Less,
            })
        }
    })
}

// ---- the runtime hooks compiled code calls back into ----------------------

/// Read the array length of a table.
///
/// # Safety
/// `t` must be a table pointer the runtime handed to compiled code, and `ok`
/// must point at a live `i32`.
#[no_mangle]
pub unsafe extern "C" fn rua_rt_len(t: *mut std::ffi::c_void, ok: *mut i32) -> f64 {
    if t.is_null() {
        *ok = 0;
        return 0.0;
    }
    let table = &*(t as *const RefCell<Table>);
    // Reading through `as_ptr` skips the borrow counter. Compiled code never
    // writes, and the interpreter holds no borrow across a call into it.
    (*table.as_ptr()).len() as f64
}

/// Read one numeric element of a table. Anything that is not a number — a
/// missing index, a string, a nested table — trips `ok`, and the runtime runs
/// the call in the interpreter instead.
///
/// # Safety
/// As [`rua_rt_len`].
#[no_mangle]
pub unsafe extern "C" fn rua_rt_get(t: *mut std::ffi::c_void, i: f64, ok: *mut i32) -> f64 {
    if t.is_null() {
        *ok = 0;
        return 0.0;
    }
    let table = &*(t as *const RefCell<Table>);
    // As in `rua_rt_len`: a read, with no borrow bookkeeping.
    match (*table.as_ptr()).num_at(i) {
        Some(n) => n,
        None => {
            *ok = 0;
            0.0
        }
    }
}

/// Hand compiled code a direct view of a table's array part.
///
/// The view is a cache inside the table, dropped by any write, and compiled
/// code never writes — so it cannot go stale underneath the caller.
///
/// # Safety
/// As [`rua_rt_len`]. `len` must point at a live `usize`.
#[no_mangle]
pub unsafe extern "C" fn rua_rt_span(
    t: *mut std::ffi::c_void,
    len: *mut usize,
    ok: *mut i32,
) -> *const f64 {
    if t.is_null() {
        *ok = 0;
        return std::ptr::null();
    }
    let table = &*(t as *const RefCell<Table>);
    match (*table.as_ptr()).nums_span() {
        Some((ptr, n)) => {
            *len = n;
            ptr
        }
        // not a dense array of numbers: back to the interpreter
        None => {
            *ok = 0;
            std::ptr::null()
        }
    }
}

/// Append a number to a table, for compiled code.
///
/// # Safety
/// As [`rua_rt_len`].
#[no_mangle]
pub unsafe extern "C" fn rua_rt_push(t: *mut std::ffi::c_void, v: f64) {
    debug_assert!(!t.is_null(), "compiled code pushed to a null table");
    if t.is_null() {
        return;
    }
    let table = &*(t as *const RefCell<Table>);
    (*table.as_ptr()).push(Value::Num(v));
}

/// Compiled code is about to append to this table: remember how long it is, so
/// that a trap can put it back. Said once on the way in rather than at every
/// append.
///
/// # Safety
/// As [`rua_rt_push`].
pub unsafe extern "C" fn rua_rt_note_append(t: *mut std::ffi::c_void, ok: *mut i32) {
    if t.is_null() {
        *ok = 0;
        return;
    }
    let table = t as *const RefCell<Table>;
    // Appending to a table that has keyed entries can pull one of them into
    // the array part, and undoing that is not a truncation. Compiled code
    // builds plain arrays; anything else is the interpreter's business.
    if !(*(*table).as_ptr()).is_plain_array() {
        *ok = 0;
        return;
    }
    note_append(table, (*(*table).as_ptr()).len());
}

/// Write a number already inside a table's array part, for compiled code. In
/// place, so a view handed out earlier stays valid.
///
/// # Safety
/// As [`rua_rt_len`].
#[no_mangle]
pub unsafe extern "C" fn rua_rt_set(t: *mut std::ffi::c_void, i: f64, v: f64, ok: *mut i32) {
    debug_assert!(!t.is_null(), "compiled code wrote to a null table");
    if t.is_null() {
        *ok = 0;
        return;
    }
    let table = &*(t as *const RefCell<Table>);
    // Only an in-place write into the array part: anything else would change
    // the table's shape, which the view compiled code holds cannot survive.
    if !(*table.as_ptr()).set_num(i, &Value::Num(v)) {
        *ok = 0;
    }
}

/// `t[i]` when the elements are themselves tables: the element's address, and
/// a view of its numbers.
///
/// The caller checked on the way in that every element is a table of numbers
/// long enough for the constant indexes the compiled body uses, and the index
/// is one the compiler proved, so this does not fail in practice. It still
/// reports rather than assumes: a wrong answer that the interpreter re-runs
/// beats handing compiled code a null pointer.
///
/// # Safety
/// `t` is a live table pointer, and `ptr`/`len`/`ok` are writable.
pub unsafe extern "C" fn rua_rt_inner(
    t: *mut std::ffi::c_void,
    i: f64,
    ptr: *mut *const f64,
    len: *mut usize,
    ok: *mut i32,
) -> *mut std::ffi::c_void {
    if t.is_null() {
        *ok = 0;
        return std::ptr::null_mut();
    }
    let table = &*(t as *const RefCell<Table>);
    let Some(Value::Table(elem)) = (*table.as_ptr()).get_num(i) else {
        *ok = 0;
        return std::ptr::null_mut();
    };
    let addr = Rc::as_ptr(elem) as *mut std::ffi::c_void;
    match (*elem.as_ptr()).nums_span() {
        Some((p, n)) => {
            *ptr = p;
            *len = n;
            addr
        }
        None => {
            *ok = 0;
            std::ptr::null_mut()
        }
    }
}

thread_local! {
    /// Tables compiled code is writing through their numeric view.
    ///
    /// The pointers are alive for as long as the call that handed them out:
    /// the caller holds every table it passed, and compiled code cannot drop
    /// one. When the call ends they are committed, or thrown away if it
    /// bailed — which is what lets compiled code write and still trap.
    static DIRTY: RefCell<Vec<Dirty>> = const { RefCell::new(Vec::new()) };
}

/// A table compiled code changed, and what it takes to put it back.
#[derive(Clone, Copy)]
struct Dirty {
    table: *const RefCell<Table>,
    /// The length it had before compiled code appended to it, if it did.
    appended_from: Option<usize>,
}

/// How much of the three scratch lists belongs to calls already under way.
fn dirty_mark() -> (usize, usize, usize) {
    (
        DIRTY.with(|d| d.borrow().len()),
        SPANS.with(|s| s.borrow().len()),
        MADE.with(|m| m.borrow().len()),
    )
}

fn note_dirty(t: *const RefCell<Table>) {
    DIRTY.with(|d| {
        let mut d = d.borrow_mut();
        if !d.iter().any(|e| e.table == t) {
            d.push(Dirty { table: t, appended_from: None });
        }
    });
}

/// Compiled code is about to append to a table: remember how long it was, so
/// that a trap can put it back.
fn note_append(t: *const RefCell<Table>, len: usize) {
    DIRTY.with(|d| {
        let mut d = d.borrow_mut();
        match d.iter_mut().find(|e| e.table == t) {
            Some(e) => e.appended_from = Some(e.appended_from.unwrap_or(len).min(len)),
            None => d.push(Dirty { table: t, appended_from: Some(len) }),
        }
    });
}

/// Finish the writes compiled code made: keep them, or undo them.
///
/// # Safety
/// Every pointer recorded since `mark` still points at a live table, which
/// holds while the call that handed it to compiled code has not returned.
unsafe fn settle_dirty(mark: (usize, usize, usize), keep: bool) {
    // the element views this call was given die with it, and only those
    SPANS.with(|s| s.borrow_mut().truncate(mark.1));
    DIRTY.with(|d| {
        let mut list = d.borrow_mut();
        for e in list.drain(mark.0..) {
            let table = &*e.table;
            if keep {
                (*table.as_ptr()).commit_nums();
            } else {
                // the view goes, taking every in-place write with it, and the
                // array part goes back to the length it had before the appends
                (*table.as_ptr()).discard_nums();
                if let Some(len) = e.appended_from {
                    (*table.as_ptr()).truncate_arr(len);
                }
            }
        }
    });
    // The tables this call made are the runtime's no longer. What escaped —
    // pushed into something, or handed back — has an owner by now; what did
    // not is dropped here, and so is everything a call that trapped made.
    MADE.with(|m| m.borrow_mut().truncate(mark.2));
}

thread_local! {
    /// Tables compiled code made for itself, with `let t = []`.
    ///
    /// Nothing else holds one until it escapes, so this list is what owns it
    /// meanwhile — and what makes creating a table as undoable as everything
    /// else compiled code does: a trap drops the whole lot.
    static MADE: RefCell<Vec<Rc<RefCell<Table>>>> = const { RefCell::new(Vec::new()) };
}

/// An empty table for compiled code to fill.
///
/// # Safety
/// Called only from compiled code, on the thread that entered it.
pub unsafe extern "C" fn rua_rt_new_table() -> *mut std::ffi::c_void {
    let t = Rc::new(RefCell::new(Table::new()));
    let addr = Rc::as_ptr(&t) as *mut std::ffi::c_void;
    MADE.with(|m| m.borrow_mut().push(t));
    addr
}

/// The reference behind a table pointer compiled code holds.
///
/// # Safety
/// `p` came from `Rc::as_ptr` on a table that is still alive: every table
/// address compiled code has was handed to it by the runtime, either from a
/// caller's argument or from `rua_rt_new_table`, and both outlive the call.
pub(crate) unsafe fn rc_of(p: *mut std::ffi::c_void) -> Rc<RefCell<Table>> {
    let p = p as *const RefCell<Table>;
    Rc::increment_strong_count(p);
    Rc::from_raw(p)
}

/// Append a table to a table, which is how a row reaches its matrix.
///
/// # Safety
/// As [`rc_of`], for both pointers.
pub unsafe extern "C" fn rua_rt_push_table(
    t: *mut std::ffi::c_void,
    e: *mut std::ffi::c_void,
) {
    debug_assert!(!t.is_null(), "compiled code pushed to a null table");
    debug_assert!(!e.is_null(), "compiled code pushed a null table");
    if t.is_null() || e.is_null() {
        return;
    }
    let table = &*(t as *const RefCell<Table>);
    let v = Value::Table(rc_of(e));
    (*table.as_ptr()).push(v);
}

/// The table at `t.name`, as an address.
///
/// A field holding nil answers null, which is what lets compiled code walk to
/// the end of a chain of them. Anything else — a number, a string, a closure —
/// traps: compiled code has nowhere to put it.
///
/// # Safety
/// As [`rc_of`] for `t`, and `key` is one of the handles the runtime put in
/// this code's context, which owns the string it points at.
pub unsafe extern "C" fn rua_rt_field(
    t: *mut std::ffi::c_void,
    key: *const std::ffi::c_void,
    ok: *mut i32,
) -> *mut std::ffi::c_void {
    debug_assert!(!t.is_null(), "compiled code read a field of a null table");
    if t.is_null() || key.is_null() {
        *ok = 0;
        return std::ptr::null_mut();
    }
    let table = &*(t as *const RefCell<Table>);
    let name = &*(key as *const RStr);
    match (*table.as_ptr()).get_field(name) {
        Value::Table(inner) => Rc::as_ptr(&inner) as *mut std::ffi::c_void,
        Value::Nil => std::ptr::null_mut(),
        _ => {
            *ok = 0;
            std::ptr::null_mut()
        }
    }
}

/// Write a table, or nil for a null, into `t.name`.
///
/// Only ever called on a table the compiled code made, so there is nothing to
/// undo: a call that traps drops the table and everything it holds.
///
/// # Safety
/// As [`rua_rt_field`], for both addresses.
pub unsafe extern "C" fn rua_rt_set_field(
    t: *mut std::ffi::c_void,
    key: *const std::ffi::c_void,
    v: *mut std::ffi::c_void,
) {
    debug_assert!(!t.is_null(), "compiled code wrote a field of a null table");
    if t.is_null() || key.is_null() {
        return;
    }
    let table = &*(t as *const RefCell<Table>);
    let name = &*(key as *const RStr);
    let value = if v.is_null() { Value::Nil } else { Value::Table(rc_of(v)) };
    (*table.as_ptr()).set_field(name, &value);
}

/// A view of a table's numbers that compiled code writes through.
///
/// # Safety
/// As `rua_rt_span`.
pub unsafe extern "C" fn rua_rt_span_mut(
    t: *mut std::ffi::c_void,
    len: *mut usize,
    ok: *mut i32,
) -> *mut f64 {
    if t.is_null() {
        *ok = 0;
        return std::ptr::null_mut();
    }
    let table = t as *const RefCell<Table>;
    match (*(*table).as_ptr()).nums_span_mut() {
        Some((ptr, n)) => {
            *len = n;
            note_dirty(table);
            ptr
        }
        None => {
            *ok = 0;
            std::ptr::null_mut()
        }
    }
}

/// `t[i]` when the elements are tables, for code that writes to them.
///
/// # Safety
/// As `rua_rt_inner`.
pub unsafe extern "C" fn rua_rt_inner_mut(
    t: *mut std::ffi::c_void,
    i: f64,
    ptr: *mut *mut f64,
    len: *mut usize,
    ok: *mut i32,
) -> *mut std::ffi::c_void {
    if t.is_null() {
        *ok = 0;
        return std::ptr::null_mut();
    }
    let table = &*(t as *const RefCell<Table>);
    let Some(Value::Table(elem)) = (*table.as_ptr()).get_num(i) else {
        *ok = 0;
        return std::ptr::null_mut();
    };
    let addr = Rc::as_ptr(elem);
    match (*elem.as_ptr()).nums_span_mut() {
        Some((p, n)) => {
            *ptr = p;
            *len = n;
            note_dirty(addr);
            addr as *mut std::ffi::c_void
        }
        None => {
            *ok = 0;
            std::ptr::null_mut()
        }
    }
}

thread_local! {
    /// The element views handed to compiled code, kept alive for as long as
    /// the call that asked for them.
    static SPANS: RefCell<Vec<Box<[rua_jit::RtSpan]>>> = const { RefCell::new(Vec::new()) };
}

/// A view of every element of an array of arrays, built once.
///
/// # Safety
/// `t` is a live table pointer; the array stays valid until the compiled call
/// that asked for it ends, which is when the runtime drops it.
pub unsafe extern "C" fn rua_rt_spans_mut(
    t: *mut std::ffi::c_void,
    len: *mut usize,
    ok: *mut i32,
) -> *const rua_jit::RtSpan {
    spans_of(t, len, ok, true)
}

/// A view of every element of an array of arrays, built once.
///
/// # Safety
/// As `rua_rt_spans`.
pub unsafe extern "C" fn rua_rt_spans(
    t: *mut std::ffi::c_void,
    len: *mut usize,
    ok: *mut i32,
) -> *const rua_jit::RtSpan {
    spans_of(t, len, ok, false)
}

/// # Safety
/// `t` is a live table pointer; the array stays valid until the compiled call
/// that asked for it ends.
unsafe fn spans_of(
    t: *mut std::ffi::c_void,
    len: *mut usize,
    ok: *mut i32,
    writable: bool,
) -> *const rua_jit::RtSpan {
    if t.is_null() {
        *ok = 0;
        return std::ptr::null();
    }
    let outer = &*(t as *const RefCell<Table>);
    // Nothing about an array of arrays changes between calls that only read
    // and write numbers in place, and rebuilding these views was a third of
    // n-body. The epoch moves whenever any table's storage does.
    if let Some(cached) = (*outer.as_ptr()).cached_spans(rua_core_shape_epoch()) {
        *len = cached.1;
        if writable {
            // The views are still good, but this call has to say which tables
            // it may write, so that they are committed or rolled back. One
            // borrow of the list for the whole array: reaching for the
            // thread local once per element was most of what was left of
            // n-body's call overhead.
            DIRTY.with(|d| {
                let mut d = d.borrow_mut();
                for k in 0..cached.1 {
                    if let Some(Value::Table(e)) = (*outer.as_ptr()).at(k) {
                        let t = Rc::as_ptr(e);
                        if !d.iter().any(|e| e.table == t) {
                            d.push(Dirty { table: t, appended_from: None });
                        }
                    }
                }
            });
        }
        return cached.0;
    }
    let n = (*outer.as_ptr()).len();
    let mut out: Vec<rua_jit::RtSpan> = Vec::with_capacity(n);
    for k in 0..n {
        let elem = match (*outer.as_ptr()).at(k) {
            Some(Value::Table(e)) => e.clone(),
            _ => {
                *ok = 0;
                return std::ptr::null();
            }
        };
        let view = if writable {
            note_dirty(Rc::as_ptr(&elem));
            (*elem.as_ptr()).nums_span_mut()
        } else {
            (*elem.as_ptr()).nums_span().map(|(p, l)| (p as *mut f64, l))
        };
        match view {
            Some((p, l)) => out.push(rua_jit::RtSpan { ptr: p, len: l }),
            None => {
                *ok = 0;
                return std::ptr::null();
            }
        }
    }
    *len = n;
    let boxed = out.into_boxed_slice();
    let addr = boxed.as_ptr();
    // keep the previous one alive: compiled code further up this call may
    // still be reading through it
    if let Some(old) = (*outer.as_ptr()).replace_spans(rua_core_shape_epoch(), boxed) {
        SPANS.with(|s| s.borrow_mut().push(old));
    }
    addr
}

fn rua_core_shape_epoch() -> u64 {
    crate::value::shape_epoch()
}

fn hooks() -> RtHooks {
    RtHooks {
        len: rua_rt_len as *const () as usize,
        get: rua_rt_get as *const () as usize,
        span: rua_rt_span as *const () as usize,
        push: rua_rt_push as *const () as usize,
        set: rua_rt_set as *const () as usize,
        inner: rua_rt_inner as *const () as usize,
        span_mut: rua_rt_span_mut as *const () as usize,
        inner_mut: rua_rt_inner_mut as *const () as usize,
        spans: rua_rt_spans as *const () as usize,
        spans_mut: rua_rt_spans_mut as *const () as usize,
        note_append: rua_rt_note_append as *const () as usize,
        new_table: rua_rt_new_table as *const () as usize,
        push_table: rua_rt_push_table as *const () as usize,
        field: rua_rt_field as *const () as usize,
        set_field: rua_rt_set_field as *const () as usize,
    }
}

/// The name of the function a traceback frame refers to.
///
/// # Safety
/// Frames are popped as their calls return, so a pointer in the vector always
/// belongs to a call still on the stack, which holds the proto alive.
fn frame_name(p: *const crate::bytecode::Proto) -> Rc<str> {
    unsafe { (*p).name.clone() }
}

/// Turn call arguments into the shape compiled code expects, or `None` when
/// they do not match what it was compiled for.
/// One value, as the argument compiled code expects for that kind — or `None`,
/// which sends the call back to the interpreter.
fn rt_arg(v: &Value, kind: &Kind) -> Option<RtArg> {
    Some(match (v, kind) {
        (Value::Num(n), Kind::Num) => RtArg::num(*n),
        // a flag travels as 0.0/1.0; an unassigned slot holds nil, and the
        // compiled code writes to it before it reads it
        (Value::Bool(b), Kind::Bool) => RtArg::num(if *b { 1.0 } else { 0.0 }),
        (Value::Nil, Kind::Bool) => RtArg::num(0.0),
        // the compiled code defines this slot before it reads it, so what the
        // register holds now is nobody's business
        (_, Kind::Dead) => RtArg::num(0.0),
        // A table compiled code reaches by name. It reads fields and never
        // writes them — only tables it made itself are written — so there is
        // no view to go stale and nothing to undo.
        (Value::Table(t), Kind::Fields) => RtArg::table(Rc::as_ptr(t) as *mut std::ffi::c_void),
        // the end of a chain of them travels as a null address
        (Value::Nil, Kind::Fields) => RtArg::table(std::ptr::null_mut()),
        (Value::Table(t), Kind::Table | Kind::TableOut) => {
            RtArg::table(Rc::as_ptr(t) as *mut std::ffi::c_void)
        }
        // An array of arrays: compiled code reads `b[3]` inside it with no test
        // of its own, so the shape is checked once, here, where refusing is
        // still free. Every element has to be a table whose array part is all
        // numbers and long enough.
        (Value::Table(t), Kind::Tables { checked: false, .. }) => {
            RtArg::table(Rc::as_ptr(t) as *mut std::ffi::c_void)
        }
        (Value::Table(t), Kind::Tables { checked: true, min }) => {
            let outer = t.try_borrow().ok()?;
            for k in 0..outer.len() {
                let elem = match outer.get_num(k as f64) {
                    Some(Value::Table(e)) => e.clone(),
                    _ => return None,
                };
                let mut inner = elem.try_borrow_mut().ok()?;
                match inner.nums_span() {
                    Some((_, n)) if n >= *min as usize => {}
                    _ => return None,
                }
            }
            RtArg::table(Rc::as_ptr(t) as *mut std::ffi::c_void)
        }
        _ => return None,
    })
}

fn compiled_args(args: &[Value], kinds: &[Kind]) -> Option<Vec<RtArg>> {
    if args.len() != kinds.len() {
        return None;
    }
    let mut out = Vec::with_capacity(args.len());
    for (v, kind) in args.iter().zip(kinds) {
        out.push(rt_arg(v, kind)?);
    }
    // Compiled code reads one table through a view of its array part and
    // appends to another. If those are the same table — or if the one it
    // appends to is an *element* of an array of arrays it reads — the view
    // goes stale the moment the append moves the storage, so this call stays
    // with the interpreter.
    if kinds.contains(&Kind::TableOut) {
        for (i, ki) in kinds.iter().enumerate() {
            if *ki != Kind::TableOut {
                continue;
            }
            for (j, kj) in kinds.iter().enumerate() {
                match kj {
                    Kind::Table if out[i].table == out[j].table => return None,
                    Kind::Tables { .. } => {
                        if out[i].table == out[j].table {
                            return None;
                        }
                        let Value::Table(outer) = &args[j] else { return None };
                        let outer = outer.try_borrow().ok()?;
                        for k in 0..outer.len() {
                            if let Some(Value::Table(e)) = outer.get_num(k as f64) {
                                if Rc::as_ptr(e) as *mut std::ffi::c_void == out[i].table {
                                    return None;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Some(out)
}

/// Find a loop statement by the id the compiler stamped on its back edge.
fn find_loop(block: &Block, id: u32) -> Option<&Stat> {
    for st in &block.stats {
        if let Some(found) = find_loop_stat(st, id) {
            return Some(found);
        }
    }
    match &block.tail {
        Some(e) => find_loop_expr(e, id),
        None => None,
    }
}

fn find_loop_stat(st: &Stat, id: u32) -> Option<&Stat> {
    let (this, body, extra) = match st {
        Stat::While(i, c, b) => (Some(*i), Some(b), Some(c)),
        Stat::Loop(i, b) => (Some(*i), Some(b), None),
        Stat::ForRange { id: i, body, end, .. } => (Some(*i), Some(body), Some(end)),
        Stat::ForIn { id: i, body, iter, .. } => (Some(*i), Some(body), Some(iter)),
        Stat::Expr(e) | Stat::FnSlot(_, e) => (None, None, Some(e)),
        Stat::LetSlots(_, es) | Stat::Return(es) => {
            return es.iter().find_map(|e| find_loop_expr(e, id))
        }
        Stat::Assign(ts, es) => return ts.iter().chain(es).find_map(|e| find_loop_expr(e, id)),
        Stat::OpAssign(t, _, e) => {
            return find_loop_expr(t, id).or_else(|| find_loop_expr(e, id))
        }
        _ => (None, None, None),
    };
    if this == Some(id) {
        return Some(st);
    }
    if let Some(b) = body {
        if let Some(found) = find_loop(b, id) {
            return Some(found);
        }
    }
    extra.and_then(|e| find_loop_expr(e, id))
}

fn find_loop_expr(e: &Expr, id: u32) -> Option<&Stat> {
    match e {
        Expr::Do(b) => find_loop(b, id),
        Expr::If(arms, els) => arms
            .iter()
            .find_map(|(c, b)| find_loop_expr(c, id).or_else(|| find_loop(b, id)))
            .or_else(|| els.as_ref().and_then(|b| find_loop(b, id))),
        Expr::Match(subject, arms) => find_loop_expr(subject, id).or_else(|| {
            arms.iter().find_map(|a| {
                a.guard
                    .as_ref()
                    .and_then(|g| find_loop_expr(g, id))
                    .or_else(|| find_loop(&a.body, id))
            })
        }),
        Expr::Bin(_, a, b) | Expr::Index(a, b) | Expr::Range(a, b, _) => {
            find_loop_expr(a, id).or_else(|| find_loop_expr(b, id))
        }
        Expr::Un(_, a) => find_loop_expr(a, id),
        Expr::Call(f, args) => {
            find_loop_expr(f, id).or_else(|| args.iter().find_map(|a| find_loop_expr(a, id)))
        }
        Expr::Method(o, _, args) => {
            find_loop_expr(o, id).or_else(|| args.iter().find_map(|a| find_loop_expr(a, id)))
        }
        Expr::Array(items) => items.iter().find_map(|i| find_loop_expr(i, id)),
        Expr::Map(items) => items
            .iter()
            .find_map(|(k, v)| find_loop_expr(k, id).or_else(|| find_loop_expr(v, id))),
        _ => None,
    }
}

/// One function's code, then the functions defined inside it.
fn write_proto(out: &mut String, proto: &crate::bytecode::Proto) {
    use std::fmt::Write;
    let name = if proto.name.is_empty() { "<closure>" } else { &proto.name };
    let _ = writeln!(
        out,
        "\nfunction {name}  ({} registers, {} ops, {} constants)",
        proto.n_regs,
        proto.code.len(),
        proto.consts.len()
    );
    for (i, op) in proto.code.iter().enumerate() {
        let _ = writeln!(out, "  {i:>4}  {op:?}");
    }
    for p in &proto.protos {
        write_proto(out, p);
    }
}

thread_local! {
    /// Argument and result vectors, kept rather than freed.
    ///
    /// Calling a builtin used to allocate twice — once for a copy of the
    /// argument registers, once for the results — and free both again, at
    /// every call. The standard library builds its results with `one`, which
    /// has no VM to hand, so the pool lives beside the interpreter rather than
    /// inside it and both sides can reach it.
    static VEC_POOL: RefCell<Vec<Vec<Value>>> = const { RefCell::new(Vec::new()) };
}

pub fn take_vec(cap: usize) -> Vec<Value> {
    VEC_POOL
        .try_with(|p| match p.borrow_mut().pop() {
            Some(mut v) => {
                v.reserve(cap);
                v
            }
            None => Vec::with_capacity(cap),
        })
        .unwrap_or_else(|_| Vec::with_capacity(cap))
}

pub fn recycle_vec(mut v: Vec<Value>) {
    let _ = VEC_POOL.try_with(|p| {
        let mut pool = p.borrow_mut();
        if pool.len() < 32 {
            v.clear();
            pool.push(v);
        }
    });
}

/// One value, in a vector from the pool: what most builtins return.
pub fn one_value(v: Value) -> Res<Vec<Value>> {
    let mut out = take_vec(1);
    out.push(v);
    Ok(out)
}
