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
const LOOP_HOT: u64 = 50_000;

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
    /// Kept alive because the context points at its interior.
    #[allow(dead_code)]
    depth: Rc<std::cell::Cell<i64>>,
}

impl RtCtxHolder {
    fn new(hooks: RtHooks, callees: Vec<Callee>, depth: Rc<std::cell::Cell<i64>>) -> RtCtxHolder {
        let callees = callees.into_boxed_slice();
        let ctx = Box::new(RtCtx {
            len: hooks.len,
            get: hooks.get,
            span: hooks.span,
            push: hooks.push,
            set: hooks.set,
            callees: callees.as_ptr(),
            // `Cell<i64>` is a transparent wrapper, so this is the counter
            depth: depth.as_ptr(),
            max_depth: MAX_DEPTH,
        });
        RtCtxHolder { ctx, callees, depth }
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
    pool: Vec<Vec<Value>>,
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
    pub(crate) upvals: Rc<Vec<CellRef>>,
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
            pool: Vec::new(),
            multi: Vec::new(),
            multi_at: None,
            line: 0,
            frames: Vec::new(),
            modules: HashMap::new(),
            jit_deps: HashMap::new(),
            loop_deps: HashMap::new(),
            compiling: std::collections::HashSet::new(),
            retired_ctx: Vec::new(),
            loops: HashMap::new(),
            libs: [None, None, None],
            upvals: Rc::new(Vec::new()),
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
        let v = Value::Native(Rc::new(Native { name: name.to_string(), f: Box::new(f) }));
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
        // a chunk is a call like any other: one file loading another must not
        // walk off the end of the stack
        self.enter_depth().map_err(|s| s.into_error())?;
        let out = self.eval(&src);
        self.leave_depth();
        out
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
                    let ctx = self.build_ctx(&compiled.inlined);
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
            match (&self.stack[self.base + *slot as usize], kind) {
                (Value::Num(n), Kind::Num) => regs.push(RtArg::num(*n)),
                (Value::Table(t), Kind::Table | Kind::TableOut) => {
                    regs.push(RtArg::table(Rc::as_ptr(t) as *mut std::ffi::c_void))
                }
                // a local does not hold what the compiled code expects: stay
                // interpreted this time round
                _ => return false,
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
        unsafe { (compiled.code)(regs.as_mut_ptr(), ctx, &mut ok) };
        if ok == 0 {
            return false; // trapped: nothing was written, so just interpret
        }
        for (i, (slot, kind)) in compiled.slots.iter().zip(&compiled.kinds).enumerate() {
            if *kind == Kind::Num {
                self.stack[self.base + *slot as usize] = Value::Num(regs[i].num);
            }
        }
        true
    }

    /// Build the context a piece of compiled code runs with: the runtime hook
    /// addresses, and the entry points of the functions it calls directly.
    fn build_ctx(&self, inlined: &[String]) -> RtCtxHolder {
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
        RtCtxHolder::new(hooks(), callees, self.depth.clone())
    }

    /// Globals that already hold compiled code, with what their parameters
    /// have to be — a direct call can only pass numbers.
    fn compiled_globals(&self) -> HashMap<String, (usize, Vec<Kind>)> {
        let mut out = HashMap::new();
        for (name, slot) in &self.gnames {
            if let Value::Func(g) = &self.gvals[*slot as usize] {
                if let Some(code) = g.jit.get() {
                    out.insert(name.to_string(), (code.address(), g.param_kinds.borrow().clone()));
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
        let need = base + n_regs;
        if self.stack.len() < need {
            self.stack.resize(need, Value::Nil);
        }
        self.top = need;
        base
    }

    /// Give a frame's registers back, dropping whatever they held.
    ///
    /// The frame reaches from `base` to the top of the stack: a call that
    /// opened a frame above this one closed it again on the way out, so `top`
    /// is where this frame ends and the callee's register count need not be
    /// carried around to find it.
    pub(crate) fn close_frame(&mut self, base: usize) {
        for slot in &mut self.stack[base..self.top] {
            Value::put(slot, Value::Nil);
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

    pub(crate) fn enter_depth(&mut self) -> Eval<bool> {
        let d = self.depth.get() + 1;
        self.depth.set(d);
        if d > MAX_DEPTH {
            self.depth.set(d - 1);
            return bad("stack overflow");
        }
        Ok(true)
    }

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
        // a trap means the compiled code met something it does not handle; it
        // has written nothing, so the interpreter can simply run the call
        // SAFETY: this is the context built for this code.
        let Some(n) = (unsafe { code.call(&rt_args, ctx) }) else { return false };
        let v = if func.returns_nil.get() { Value::Nil } else { Value::Num(n) };
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
    pub(crate) fn recycle_vec(&mut self, mut v: Vec<Value>) {
        if self.pool.len() < 32 {
            v.clear();
            self.pool.push(v);
        }
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

    pub(crate) fn global_at(&self, slot: u32) -> Value {
        self.gvals[slot as usize].clone()
    }

    pub(crate) fn store_global(&mut self, slot: u32, v: Value) {
        self.write_global(slot, v);
    }

    /// Build a closure over the current frame: captured locals are shared
    /// through their cells, and upvalues are passed straight down.
    pub(crate) fn make_closure(&mut self, proto: Rc<crate::bytecode::Proto>) -> Value {
        let mut cells = Vec::with_capacity(proto.def.upvals.len());
        for src in &proto.def.upvals {
            cells.push(match src {
                UpvalSrc::ParentLocal(slot) => match &self.stack[self.base + *slot as usize] {
                    Value::Cell(c) => c.clone(),
                    other => Rc::new(RefCell::new(other.clone())),
                },
                UpvalSrc::ParentUpval(i) => self.upvals[*i as usize].clone(),
            });
        }
        Value::Func(Rc::new(Function {
            proto,
            param_kinds: RefCell::new(Vec::new()),
            rt: RefCell::new(None),
            returns_nil: std::cell::Cell::new(false),
            upvals: Rc::new(cells),
            hits: std::cell::Cell::new(0),
            jit: std::cell::Cell::new(None),
            jit_state: std::cell::Cell::new(JitState::Cold),
        }))
    }

    pub(crate) fn take_vec(&mut self, cap: usize) -> Vec<Value> {
        match self.pool.pop() {
            Some(mut v) => {
                v.reserve(cap);
                v
            }
            None => Vec::with_capacity(cap),
        }
    }

    /// `a[k]` and `a::k`: a plain lookup, no method fallback.
    pub fn index(&mut self, o: &Value, k: &Value) -> Res<Value> {
        match o {
            Value::Table(t) => Ok(t.borrow().get(&Key::from_value(k)?)),
            Value::Str(_) => Ok(self.lib_member(MethodTable::Str, &Key::from_value(k)?)),
            Value::Num(_) => Ok(self.lib_member(MethodTable::Math, &Key::from_value(k)?)),
            Value::Cell(c) => {
                let inner = c.borrow().clone();
                self.index(&inner, k)
            }
            other => err(format!("cannot index a {} value", other.type_name())),
        }
    }

    fn lib_member(&self, kind: MethodTable, key: &Key) -> Value {
        match self.lib(kind) {
            Some(t) => t.borrow().get(key),
            None => Value::Nil,
        }
    }

    /// `a.m(..)`: the receiver's own field first, then its type's library —
    /// this is what makes `[3,1,2].sort()` and `"ab".upper()` work.
    pub(crate) fn method(&mut self, o: &Value, name: &RStr) -> Res<Value> {
        let kind = match o {
            Value::Table(t) => {
                let own = match t.borrow().get_field(name) {
                    Some(v) => v,
                    None => t.borrow().get(&Key::Str(name.clone())),
                };
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
        let key = Key::Str(name.clone());
        match self.lib_member(kind, &key) {
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
                let ctx = self.build_ctx(&out.inlined);
                func.jit.set(Some(out.code));
                *func.param_kinds.borrow_mut() = out.param_kinds;
                func.returns_nil.set(out.returns_nil);
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
                            // a trap means the compiled code met something it
                            // does not handle; it has written nothing, so the
                            // interpreter can simply run the call instead
                            // SAFETY: this is the context built for this code.
                            if let Some(n) = unsafe { code.call(&rt_args, ctx) } {
                                self.recycle_vec(args);
                                return Ok(vec![Value::Num(n)]);
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

pub fn arith(op: crate::bytecode::BinKind, l: Value, r: Value) -> Res<Value> {
    use crate::bytecode::BinKind::*;
    Ok(match op {
        // `+` concatenates when either side is a string, as in Rust's String + &str
        Add => match (&l, &r) {
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
            let ord = match (&l, &r) {
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

fn hooks() -> RtHooks {
    RtHooks {
        len: rua_rt_len as *const () as usize,
        get: rua_rt_get as *const () as usize,
        span: rua_rt_span as *const () as usize,
        push: rua_rt_push as *const () as usize,
        set: rua_rt_set as *const () as usize,
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
fn compiled_args(args: &[Value], kinds: &[Kind]) -> Option<Vec<RtArg>> {
    if args.len() != kinds.len() {
        return None;
    }
    let mut out = Vec::with_capacity(args.len());
    for (v, kind) in args.iter().zip(kinds) {
        out.push(match (v, kind) {
            (Value::Num(n), Kind::Num) => RtArg::num(*n),
            (Value::Table(t), Kind::Table | Kind::TableOut) => {
                RtArg::table(Rc::as_ptr(t) as *mut std::ffi::c_void)
            }
            _ => return None,
        });
    }
    // Compiled code reads one table through a view of its array part and
    // appends to another. If those are the same table, the view would go
    // stale as it is written, so this call stays with the interpreter.
    if kinds.contains(&Kind::TableOut) {
        for (i, ki) in kinds.iter().enumerate() {
            if *ki != Kind::TableOut {
                continue;
            }
            for (j, kj) in kinds.iter().enumerate() {
                if *kj == Kind::Table && out[i].table == out[j].table {
                    return None;
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
