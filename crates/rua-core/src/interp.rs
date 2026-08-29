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
    fn into_error(self) -> Error {
        match self {
            Signal::Err(e) => *e,
            Signal::Break => Error("`break` outside of a loop"),
            Signal::Continue => Error("`continue` outside of a loop"),
            Signal::Return => Error("`return` outside of a function"),
        }
    }
}

type Eval<T> = Result<T, Signal>;

fn bad<T>(msg: impl Into<String>) -> Eval<T> {
    Err(Signal::Err(Box::new(Error(msg.into()))))
}

/// Iterations between checks on a running loop.
const LOOP_BATCH: u32 = 1000;

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
}

impl RtCtxHolder {
    fn new(hooks: RtHooks, callees: Vec<Callee>) -> RtCtxHolder {
        let callees = callees.into_boxed_slice();
        let ctx = Box::new(RtCtx {
            len: hooks.len,
            get: hooks.get,
            span: hooks.span,
            callees: callees.as_ptr(),
        });
        RtCtxHolder { ctx, callees }
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
    /// Every local lives here: one contiguous run of slots per active call.
    stack: Vec<Value>,
    base: usize,
    /// Argument vectors, recycled: a call per node adds up fast.
    pool: Vec<Vec<Value>>,
    /// Where a `return` leaves its values on the way out.
    rets: Vec<Value>,
    /// The statement being executed, and the call stack above it, so that an
    /// error can say where it happened.
    line: u32,
    frames: Vec<(Rc<str>, u32)>,
    /// Modules already loaded by `require`, keyed by canonical path.
    pub modules: HashMap<String, Value>,
    /// Compiled functions that inlined a call to a global, so that assigning to
    /// that global can throw their machine code away.
    jit_deps: HashMap<u32, Vec<std::rc::Weak<Function>>>,
    /// Functions the JIT is busy with, so callee-first compilation terminates.
    compiling: std::collections::HashSet<usize>,
    /// What the JIT knows about each loop it has seen, keyed by loop id.
    loops: HashMap<u32, LoopEntry>,
    /// `string`, `math` and `table`, kept to hand for method dispatch.
    libs: [Option<Rc<RefCell<Table>>>; 3],
    upvals: Rc<Vec<CellRef>>,
    depth: usize,
}

const MAX_DEPTH: usize = 200;

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
            pool: Vec::new(),
            rets: Vec::new(),
            line: 0,
            frames: Vec::new(),
            modules: HashMap::new(),
            jit_deps: HashMap::new(),
            compiling: std::collections::HashSet::new(),
            loops: HashMap::new(),
            libs: [None, None, None],
            upvals: Rc::new(Vec::new()),
            depth: 0,
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

    /// Store a global, and drop the machine code of anything that compiled a
    /// direct call to its old value.
    fn write_global(&mut self, slot: u32, v: Value) {
        let same = matches!((&self.gvals[slot as usize], &v), (Value::Func(a), Value::Func(b)) if Rc::ptr_eq(a, b));
        self.gvals[slot as usize] = v;
        if same {
            return;
        }
        if let Some(dependents) = self.jit_deps.remove(&slot) {
            for weak in dependents {
                if let Some(f) = weak.upgrade() {
                    f.jit.set(None);
                    f.jit_state.set(JitState::Cold);
                    f.hits.set(0);
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
        F: Fn(&mut Vm, Vec<Value>) -> Res<Vec<Value>> + 'static,
    {
        let v = Value::Native(Rc::new(Native { name: name.to_string(), f: Box::new(f) }));
        self.set_global(name, v);
    }

    /// Run source text; the value of the final expression is returned.
    pub fn eval(&mut self, src: &str) -> Res<Vec<Value>> {
        let (block, n_slots) = rua_syntax::compile(src).map_err(|e| Error {
            message: e.message,
            line: e.line,
            located: e.line > 0,
            where_: None,
            trace: Vec::new(),
        })?;
        let saved_base = self.base;
        let saved_upvals = std::mem::replace(&mut self.upvals, Rc::new(Vec::new()));
        self.base = self.stack.len();
        self.stack.resize(self.base + n_slots, Value::Nil);
        let out = self.exec_block(&block);
        self.stack.truncate(self.base);
        self.base = saved_base;
        self.upvals = saved_upvals;
        match out {
            Ok(vals) => Ok(vals),
            Err(Signal::Return) => Ok(self.take_rets()),
            Err(other) => Err(other.into_error()),
        }
    }

    pub fn eval_file(&mut self, path: &str) -> Res<Vec<Value>> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| Error(format!("cannot read {path}: {e}")))?;
        self.eval(&src)
    }

    pub fn call(&mut self, f: &Value, args: Vec<Value>) -> Res<Vec<Value>> {
        self.call_value(f, args).map_err(|s| s.into_error())
    }

    // ---- blocks and statements ----------------------------------------------

    /// Runs a block and yields its tail value(s), or none.
    fn exec_block(&mut self, block: &Block) -> Eval<Vec<Value>> {
        for (i, st) in block.stats.iter().enumerate() {
            self.line = block.lines.get(i).copied().unwrap_or(self.line);
            self.exec(st).map_err(|e| self.locate(e))?;
        }
        match &block.tail {
            Some(e) => {
                self.line = if block.tail_line > 0 { block.tail_line } else { self.line };
                let out = self.eval_multi(e);
                out.map_err(|e| self.locate(e))
            }
            None => Ok(Vec::new()),
        }
    }

    /// Stamp an error with the line it happened on, once.
    fn locate(&self, sig: Signal) -> Signal {
        match sig {
            Signal::Err(e) if !e.located => Signal::Err(Box::new(Error {
                message: e.message.clone(),
                located: true,
                line: self.line,
                where_: self
                    .frames
                    .last()
                    .map(|(n, _)| n.clone())
                    .filter(|n| !n.is_empty()),
                trace: self.frames.clone(),
            })),
            other => other,
        }
    }

    /// A block in statement position: its tail value, if any, is discarded.
    fn exec_block_unit(&mut self, block: &Block) -> Eval<()> {
        let vals = self.exec_block(block)?;
        self.recycle(vals);
        Ok(())
    }

    fn slot(&mut self, b: Binding, v: Value) {
        let i = self.base + b.slot as usize;
        // a captured local gets a fresh cell every time it is declared, so a
        // closure made in a loop captures that iteration's variable
        self.stack[i] = if b.cell { Value::Cell(Rc::new(RefCell::new(v))) } else { v };
    }

    fn exec(&mut self, st: &Stat) -> Eval<()> {
        match st {
            Stat::Expr(e) => {
                let vals = self.eval_multi(e)?;
                self.recycle(vals);
            }
            Stat::LetSlots(bindings, exprs) => {
                if bindings.len() == 1 && exprs.len() == 1 {
                    let v = self.eval_expr(&exprs[0])?;
                    self.slot(bindings[0], v);
                } else {
                    let vals = self.explist(exprs)?;
                    for (i, b) in bindings.iter().enumerate() {
                        let v = vals.get(i).cloned().unwrap_or(Value::Nil);
                        self.slot(*b, v);
                    }
                    self.recycle(vals);
                }
            }
            Stat::FnSlot(binding, f) => {
                // bind first (with a cell when captured), then build the
                // closure: that is what lets a function call itself
                self.slot(*binding, Value::Nil);
                let v = self.eval_expr(f)?;
                let i = self.base + binding.slot as usize;
                match &self.stack[i] {
                    Value::Cell(c) => *c.borrow_mut() = v,
                    _ => self.stack[i] = v,
                }
            }
            Stat::Assign(targets, exprs) => {
                if targets.len() == 1 && exprs.len() == 1 {
                    let v = self.eval_expr(&exprs[0])?;
                    self.assign_to(&targets[0], v)?;
                } else {
                    let vals = self.explist(exprs)?;
                    for (i, t) in targets.iter().enumerate() {
                        let v = vals.get(i).cloned().unwrap_or(Value::Nil);
                        self.assign_to(t, v)?;
                    }
                    self.recycle(vals);
                }
            }
            Stat::OpAssign(target, op, e) => {
                let rhs = self.eval_expr(e)?;
                // `i += 1` on a plain local: read, add and store in place
                if let Expr::Local(b, _) = target {
                    if !b.cell {
                        let i = self.base + b.slot as usize;
                        if let (Value::Num(x), Value::Num(y)) = (&self.stack[i], &rhs) {
                            let (x, y) = (*x, *y);
                            if let Some(v) = fast_num_op(*op, x, y) {
                                self.stack[i] = Value::Num(v);
                                return Ok(());
                            }
                        }
                    }
                }
                let old = self.eval_expr(target)?;
                let v = arith(*op, old, rhs)?;
                self.assign_to(target, v)?;
            }
            Stat::While(id, cond, body) => {
                if self.run_compiled_loop(*id, st)? {
                    return Ok(());
                }
                let mut spins = 0u32;
                while self.eval_expr(cond)?.truthy() {
                    match self.exec_block_unit(body) {
                        Err(Signal::Break) => break,
                        Err(Signal::Continue) | Ok(()) => {}
                        Err(other) => return Err(other),
                    }
                    spins += 1;
                    // hot enough to be worth compiling? hand the rest over
                    if spins % LOOP_BATCH == 0 && self.consider_loop(*id, st)? {
                        return Ok(());
                    }
                }
            }
            Stat::Loop(id, body) => {
                if self.run_compiled_loop(*id, st)? {
                    return Ok(());
                }
                let mut spins = 0u32;
                loop {
                    match self.exec_block_unit(body) {
                        Err(Signal::Break) => break,
                        Err(Signal::Continue) | Ok(()) => {}
                        Err(other) => return Err(other),
                    }
                    spins += 1;
                    if spins % LOOP_BATCH == 0 && self.consider_loop(*id, st)? {
                        return Ok(());
                    }
                }
            }
            Stat::ForRange { id, binding, start, end, inclusive, body, .. } => {
                let b = binding.expect("resolved");
                let mut i = self.eval_expr(start)?.as_num()?;
                let last = self.eval_expr(end)?.as_num()?;
                // already compiled? start it where this loop would start
                self.slot(b, Value::Num(i));
                if self.run_compiled_loop(*id, st)? {
                    return Ok(());
                }
                let mut spins = 0u32;
                while if *inclusive { i <= last } else { i < last } {
                    self.slot(b, Value::Num(i));
                    match self.exec_block(body) {
                        Err(Signal::Break) => break,
                        Err(Signal::Continue) | Ok(_) => {}
                        Err(other) => return Err(other),
                    }
                    i += 1.0;
                    spins += 1;
                    if spins % LOOP_BATCH == 0 {
                        // the compiled form picks the counter up where it is
                        self.slot(b, Value::Num(i));
                        if self.consider_loop(*id, st)? {
                            return Ok(());
                        }
                    }
                }
            }
            Stat::ForIn { bindings, iter, body, .. } => {
                let it = match self.eval_expr(iter)? {
                    // a table iterates its values, as a Rust `for` over a Vec
                    Value::Table(t) => crate::stdlib::value_iterator(t),
                    other => other,
                };
                loop {
                    let empty = self.take_vec(0);
                    let vals = self.call_value(&it, empty)?;
                    if matches!(vals.first(), None | Some(Value::Nil)) {
                        self.recycle(vals);
                        break;
                    }
                    for (i, b) in bindings.iter().enumerate() {
                        let v = vals.get(i).cloned().unwrap_or(Value::Nil);
                        self.slot(*b, v);
                    }
                    self.recycle(vals);
                    match self.exec_block(body) {
                        Err(Signal::Break) => break,
                        Err(Signal::Continue) | Ok(_) => {}
                        Err(other) => return Err(other),
                    }
                }
            }
            Stat::Return(exprs) => {
                let vals = self.explist(exprs)?;
                let old = std::mem::replace(&mut self.rets, vals);
                self.recycle(old);
                return Err(Signal::Return);
            }
            Stat::Break => return Err(Signal::Break),
            Stat::Continue => return Err(Signal::Continue),
            Stat::Let(..) | Stat::FnDecl(..) => {
                return bad("internal: unresolved statement reached the interpreter")
            }
        }
        Ok(())
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
                (Value::Table(t), Kind::Table) => {
                    regs.push(RtArg::table(Rc::as_ptr(t) as *mut std::ffi::c_void))
                }
                // a local does not hold what the compiled code expects: stay
                // interpreted this time round
                _ => return false,
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
        RtCtxHolder::new(hooks(), callees)
    }

    /// Loops may call already compiled global functions directly, too.
    fn self_ref_for_loop(&self) -> SelfRef {
        let mut compiled_globals = HashMap::new();
        for (name, slot) in &self.gnames {
            if let Value::Func(g) = &self.gvals[*slot as usize] {
                if let Some(code) = g.jit.get() {
                    compiled_globals.insert(name.to_string(), (code.address(), code.arity()));
                }
            }
        }
        SelfRef { upval: None, global: None, compiled_globals, hooks: hooks() }
    }

    fn assign_to(&mut self, target: &Expr, v: Value) -> Eval<()> {
        match target {
            Expr::Local(b, _) => {
                let i = self.base + b.slot as usize;
                if b.cell {
                    match &self.stack[i] {
                        Value::Cell(c) => *c.borrow_mut() = v,
                        _ => self.stack[i] = Value::Cell(Rc::new(RefCell::new(v))),
                    }
                } else {
                    self.stack[i] = v;
                }
                Ok(())
            }
            Expr::Upval(i, _) => {
                *self.upvals[*i as usize].borrow_mut() = v;
                Ok(())
            }
            Expr::Global(name, cache) => {
                let i = match cache.get() {
                    Some(i) => i,
                    None => {
                        let i = self.global_slot(name);
                        cache.set(i);
                        i
                    }
                };
                self.write_global(i, v);
                Ok(())
            }
            Expr::Index(obj, key) => {
                let o = self.eval_expr(obj)?;
                let k = Key::from_value(&self.eval_expr(key)?)?;
                match o {
                    Value::Table(t) => {
                        t.borrow_mut().set(k, v);
                        Ok(())
                    }
                    other => bad(format!("cannot index a {} value", other.type_name())),
                }
            }
            _ => bad("cannot assign to this expression"),
        }
    }

    // ---- expressions --------------------------------------------------------

    /// Hand a finished vector back for reuse.
    fn recycle(&mut self, mut v: Vec<Value>) {
        if self.pool.len() < 32 {
            v.clear();
            self.pool.push(v);
        }
    }

    /// Collect the values a `return` left behind.
    fn take_rets(&mut self) -> Vec<Value> {
        let empty = self.take_vec(0);
        std::mem::replace(&mut self.rets, empty)
    }

    fn take_vec(&mut self, cap: usize) -> Vec<Value> {
        match self.pool.pop() {
            Some(mut v) => {
                v.reserve(cap);
                v
            }
            None => Vec::with_capacity(cap),
        }
    }

    /// Evaluates a list, expanding the final call so `let (a, b) = f()` works.
    fn explist(&mut self, exprs: &[Expr]) -> Eval<Vec<Value>> {
        let mut out = self.take_vec(exprs.len());
        for (i, e) in exprs.iter().enumerate() {
            if i + 1 == exprs.len() {
                out.extend(self.eval_multi(e)?);
            } else {
                out.push(self.eval_expr(e)?);
            }
        }
        Ok(out)
    }

    fn eval_multi(&mut self, e: &Expr) -> Eval<Vec<Value>> {
        match e {
            Expr::Call(f, args) => {
                let fv = self.eval_expr(f)?;
                let argv = self.explist(args)?;
                self.call_value(&fv, argv)
            }
            Expr::Method(obj, name, args) => {
                let o = self.eval_expr(obj)?;
                let m = self.method(&o, name)?;
                let mut argv = self.take_vec(args.len() + 1);
                argv.push(o);
                argv.extend(self.explist(args)?);
                self.call_value(&m, argv)
            }
            Expr::Do(b) => self.exec_block(b),
            Expr::Match(subject, arms) => {
                let subject = self.eval_expr(subject)?;
                for arm in arms {
                    if !self.arm_matches(arm, &subject)? {
                        continue;
                    }
                    return self.exec_block(&arm.body);
                }
                Ok(Vec::new())
            }
            Expr::If(arms, els) => {
                for (cond, body) in arms {
                    if self.eval_expr(cond)?.truthy() {
                        return self.exec_block(body);
                    }
                }
                match els {
                    Some(b) => self.exec_block(b),
                    None => Ok(Vec::new()),
                }
            }
            other => Ok(vec![self.eval_expr(other)?]),
        }
    }

    /// Does this arm accept the subject? Binding patterns bind on the way.
    fn arm_matches(&mut self, arm: &Arm, subject: &Value) -> Eval<bool> {
        let mut hit = false;
        for p in &arm.patterns {
            match p {
                Pattern::Wild => hit = true,
                Pattern::Bind(_, Some(b)) => {
                    self.slot(*b, subject.clone());
                    hit = true;
                }
                Pattern::Bind(name, None) => {
                    return bad(format!("internal: unresolved pattern `{name}`"))
                }
                Pattern::Lit(e) => {
                    if self.eval_expr(e)? == *subject {
                        hit = true;
                    }
                }
            }
            if hit {
                break;
            }
        }
        if !hit {
            return Ok(false);
        }
        match &arm.guard {
            Some(g) => Ok(self.eval_expr(g)?.truthy()),
            None => Ok(true),
        }
    }

    fn eval_expr(&mut self, e: &Expr) -> Eval<Value> {
        Ok(match e {
            Expr::Num(n) => Value::Num(*n),
            Expr::Local(b, _) => {
                let v = &self.stack[self.base + b.slot as usize];
                if b.cell {
                    match v {
                        Value::Cell(c) => c.borrow().clone(),
                        other => other.clone(),
                    }
                } else {
                    v.clone()
                }
            }
            Expr::Upval(i, _) => self.upvals[*i as usize].borrow().clone(),
            Expr::Global(name, cache) => {
                let i = match cache.get() {
                    Some(i) => i,
                    None => {
                        let i = self.global_slot(name);
                        cache.set(i);
                        i
                    }
                };
                self.gvals[i as usize].clone()
            }
            Expr::Str(s) => Value::Str(s.clone()),
            Expr::Nil => Value::Nil,
            Expr::Bool(b) => Value::Bool(*b),
            Expr::Call(..) | Expr::Method(..) | Expr::Do(_) | Expr::If(..) | Expr::Match(..) => {
                // one value wanted: take it and hand the vector back
                let mut vals = self.eval_multi(e)?;
                let out = if vals.is_empty() { Value::Nil } else { vals.swap_remove(0) };
                self.recycle(vals);
                out
            }
            Expr::Index(obj, key) => {
                let o = self.eval_expr(obj)?;
                let k = self.eval_expr(key)?;
                self.index(&o, &k)?
            }
            Expr::Func(def) => {
                // capture now: a closure owns its upvalue cells
                let mut cells = Vec::with_capacity(def.upvals.len());
                for src in &def.upvals {
                    cells.push(match src {
                        UpvalSrc::ParentLocal(slot) => {
                            match &self.stack[self.base + *slot as usize] {
                                Value::Cell(c) => c.clone(),
                                other => Rc::new(RefCell::new(other.clone())),
                            }
                        }
                        UpvalSrc::ParentUpval(i) => self.upvals[*i as usize].clone(),
                    });
                }
                Value::Func(Rc::new(Function {
                    def: def.clone(),
                    param_kinds: RefCell::new(Vec::new()),
                    rt: RefCell::new(None),
                    upvals: Rc::new(cells),
                    hits: std::cell::Cell::new(0),
                    jit: std::cell::Cell::new(None),
                    jit_state: std::cell::Cell::new(JitState::Cold),
                }))
            }
            Expr::Array(items) => {
                let mut t = Table::new();
                let n = items.len();
                for (i, item) in items.iter().enumerate() {
                    if i + 1 == n {
                        for v in self.eval_multi(item)? {
                            t.push(v);
                        }
                    } else {
                        let v = self.eval_expr(item)?;
                        t.push(v);
                    }
                }
                Value::table(t)
            }
            Expr::Map(items) => {
                let mut t = Table::new();
                for (k, v) in items {
                    let kk = Key::from_value(&self.eval_expr(k)?)?;
                    let vv = self.eval_expr(v)?;
                    t.set(kk, vv);
                }
                Value::table(t)
            }
            Expr::Range(a, b, inclusive) => {
                let start = self.eval_expr(a)?.as_num()?;
                let end = self.eval_expr(b)?.as_num()?;
                crate::stdlib::range_iterator(start, end, *inclusive)
            }
            Expr::Un(op, a) => {
                let v = self.eval_expr(a)?;
                match op {
                    UnOp::Neg => Value::Num(-v.as_num()?),
                    UnOp::Not => Value::Bool(!v.truthy()),
                }
            }
            Expr::Bin(op, a, b) => {
                match op {
                    BinOp::And => {
                        let l = self.eval_expr(a)?;
                        return Ok(if l.truthy() { self.eval_expr(b)? } else { l });
                    }
                    BinOp::Or => {
                        let l = self.eval_expr(a)?;
                        return Ok(if l.truthy() { l } else { self.eval_expr(b)? });
                    }
                    _ => {}
                }
                let l = self.eval_expr(a)?;
                let r = self.eval_expr(b)?;
                // the overwhelmingly common case, kept off the generic path
                if let (Value::Num(x), Value::Num(y)) = (&l, &r) {
                    let (x, y) = (*x, *y);
                    return Ok(match op {
                        BinOp::Add => Value::Num(x + y),
                        BinOp::Sub => Value::Num(x - y),
                        BinOp::Mul => Value::Num(x * y),
                        BinOp::Div => Value::Num(x / y),
                        BinOp::Rem => Value::Num(x - (x / y).floor() * y),
                        BinOp::Lt => Value::Bool(x < y),
                        BinOp::Le => Value::Bool(x <= y),
                        BinOp::Gt => Value::Bool(x > y),
                        BinOp::Ge => Value::Bool(x >= y),
                        BinOp::Eq => Value::Bool(x == y),
                        BinOp::Ne => Value::Bool(x != y),
                        BinOp::And | BinOp::Or => unreachable!("short circuited above"),
                    });
                }
                arith(*op, l, r)?
            }
            Expr::Var(name) => {
                return bad(format!("internal: unresolved name `{name}`"));
            }
        })
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
    fn method(&mut self, o: &Value, name: &Rc<str>) -> Res<Value> {
        let key = Key::Str(name.clone());
        let kind = match o {
            Value::Table(t) => {
                let own = t.borrow().get(&key);
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
        match self.lib_member(kind, &key) {
            Value::Nil => err(format!("no method `{name}` on a {} value", o.type_name())),
            v => Ok(v),
        }
    }

    fn call_value(&mut self, f: &Value, args: Vec<Value>) -> Eval<Vec<Value>> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return bad("stack overflow");
        }
        let out = self.call_inner(f, args);
        self.depth -= 1;
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
        if self.compiling.insert(func.def.id) {
            for name in rua_jit::called_globals(&func.def) {
                let callee = self.get_global(&name);
                if let Value::Func(g) = callee {
                    if g.jit_state.get() == JitState::Cold && !self.compiling.contains(&g.def.id) {
                        self.try_compile(&g);
                    }
                }
            }
        }
        self.compiling.remove(&func.def.id);
        // Which upvalue, if any, currently holds this very function? That is
        // what `fn fib(n) { .. fib(n - 1) .. }` looks like after resolution,
        // and it is the only call the JIT is willing to compile.
        let upval = func
            .upvals
            .iter()
            .position(|c| matches!(&*c.borrow(), Value::Func(g) if Rc::ptr_eq(g, func)))
            .map(|i| i as u16);
        // a top level `fn` is a global, and recursion goes through that name
        let global = match self.get_global(&func.def.name) {
            Value::Func(g) if Rc::ptr_eq(&g, func) => Some(func.def.name.clone()),
            _ => None,
        };
        // globals that already hold compiled functions can be called directly
        let mut compiled_globals = HashMap::new();
        for (name, slot) in &self.gnames {
            if let Value::Func(g) = &self.gvals[*slot as usize] {
                if let Some(code) = g.jit.get() {
                    compiled_globals.insert(name.to_string(), (code.address(), code.arity()));
                }
            }
        }
        let req = SelfRef { upval, global, compiled_globals, hooks: hooks() };
        match self.jit.compile(&func.def, req) {
            Ok(out) => {
                let ctx = self.build_ctx(&out.inlined);
                func.jit.set(Some(out.code));
                *func.param_kinds.borrow_mut() = out.param_kinds;
                *func.rt.borrow_mut() = Some(ctx);
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
                Ok((n.f)(self, args)?)
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
                                return Ok(vec![Value::Num(n)]);
                            }
                        }
                    }
                }
                let def = &func.def;
                self.frames.push((Rc::from(def.name.as_str()), self.line));
                let saved_line = self.line;
                let saved_base = self.base;
                let saved_upvals = std::mem::replace(&mut self.upvals, func.upvals.clone());
                self.base = self.stack.len();
                self.stack.resize(self.base + def.n_slots, Value::Nil);
                let mut args = args;
                for (i, b) in def.param_bindings.iter().enumerate() {
                    let v = args.get_mut(i).map(std::mem::take).unwrap_or(Value::Nil);
                    self.slot(*b, v);
                }
                args.clear();
                if self.pool.len() < 32 {
                    self.pool.push(args);
                }
                let out = self.exec_block(&def.body);
                self.stack.truncate(self.base);
                self.base = saved_base;
                self.upvals = saved_upvals;
                self.frames.pop();
                self.line = saved_line;
                match out {
                    Ok(vals) => Ok(vals),
                    Err(Signal::Return) => Ok(self.take_rets()),
                    Err(other) => Err(other),
                }
            }
            Value::Cell(c) => {
                let inner = c.borrow().clone();
                self.call_inner(&inner, args)
            }
            other => bad(format!("cannot call a {} value", other.type_name())),
        }
    }
}

/// The numeric part of `arith`, without the `Value` wrapping.
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
            (Value::Table(t), Kind::Table) => {
                RtArg::table(Rc::as_ptr(t) as *mut std::ffi::c_void)
            }
            _ => return None,
        });
    }
    Some(out)
}

fn fast_num_op(op: BinOp, x: f64, y: f64) -> Option<f64> {
    Some(match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::Div => x / y,
        BinOp::Rem => x - (x / y).floor() * y,
        _ => return None,
    })
}

pub fn arith(op: BinOp, l: Value, r: Value) -> Res<Value> {
    use BinOp::*;
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
        And | Or => unreachable!("handled by short circuit"),
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

fn hooks() -> RtHooks {
    RtHooks {
        len: rua_rt_len as *const () as usize,
        get: rua_rt_get as *const () as usize,
        span: rua_rt_span as *const () as usize,
    }
}
