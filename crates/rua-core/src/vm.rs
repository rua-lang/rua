//! The virtual machine: one function's bytecode at a time.
//!
//! Registers are a window into `Vm::stack` starting at `Vm::base`, so a call is
//! a resize and two saved fields rather than an allocation. Control flow is
//! jumps, which is why `break`, `continue` and `return` need no unwinding
//! machinery here — only errors travel back up the Rust stack.

use crate::bytecode::*;
use crate::interp::{arith, Eval, Signal, Vm, LOOP_BATCH};
use crate::value::*;
use std::cell::RefCell;
use std::rc::Rc;

/// One suspended call: where to go back to when the current function returns.
///
/// rua calls used to recurse in Rust, which made script recursion depth a
/// function of the host stack — an unoptimised build overflowed at 150 nested
/// calls — and cost a stack frame's prologue per call. Calls now push one of
/// these and keep going round the same loop, the way Lua's `goto startfunc`
/// does.
pub(crate) struct CallFrame {
    /// The function to go back to, which also keeps its code alive.
    caller: Rc<Function>,
    base: usize,
    pc: usize,
    /// Where the callee's results belong, and how many are wanted.
    ret_to: usize,
    nres: u16,
    upvals: Rc<Vec<CellRef>>,
    /// The line the call was made from, for the traceback.
    line: u32,
    /// The callee's frame size, so its registers can be handed back on the way
    /// out — including when an error unwinds several frames at once.
    callee_regs: usize,
}

impl Vm {
    /// Run a compiled function with `args`, and hand back what it returned.
    pub(crate) fn run(&mut self, func: &Rc<Function>, args: Vec<Value>) -> Eval<Vec<Value>> {
        // borrowed, not cloned: the caller holds the function alive, and a
        // refcount round trip per call is visible in a profile
        let proto = &func.proto;
        let saved_base = self.base;
        let saved_upvals = std::mem::replace(&mut self.upvals, func.upvals.clone());
        self.base = self.open_frame(proto.n_regs);

        let mut args = args;
        for (i, p) in proto.params.iter().enumerate() {
            let v = args.get_mut(i).map(std::mem::take).unwrap_or(Value::Nil);
            let slot = self.base + p.reg as usize;
            self.stack[slot] = if p.cell {
                Value::Cell(Rc::new(RefCell::new(v)))
            } else {
                v
            };
        }
        args.clear();
        self.recycle_vec(args);

        let out = self.execute(func);
        let result = match out {
            Ok((_, MULTI)) => Ok(self.take_multi()),
            Ok((rbase, n)) => {
                let mut vals = self.take_vec(n as usize);
                let start = self.base + rbase as usize;
                for i in 0..n as usize {
                    vals.push(self.stack[start + i].clone());
                }
                Ok(vals)
            }
            Err(e) => Err(e),
        };

        self.close_frame(self.base, proto.n_regs);
        self.base = saved_base;
        self.upvals = saved_upvals;
        result
    }

    /// Call a rua function without building an argument vector: the arguments
    /// are already sitting in the caller's registers, and the results go
    /// straight back into them.
    pub(crate) fn run_into(
        &mut self,
        func: &Rc<Function>,
        arg_start: usize,
        nargs: u16,
        ret_to: usize,
        nres: u16,
    ) -> Eval<()> {
        let proto = &func.proto;
        let saved_base = self.base;
        let saved_upvals = std::mem::replace(&mut self.upvals, func.upvals.clone());
        self.base = self.open_frame(proto.n_regs);
        for (i, p) in proto.params.iter().enumerate() {
            let v = if (i as u16) < nargs {
                self.stack[arg_start + i].clone()
            } else {
                Value::Nil
            };
            let slot = self.base + p.reg as usize;
            self.stack[slot] = if p.cell {
                Value::Cell(Rc::new(RefCell::new(v)))
            } else {
                v
            };
        }

        let out = self.execute(func);
        let copied = match out {
            // The common case: a known number of results, straight across.
            // `open_frame` puts the callee's registers above the caller's top,
            // so the two windows are disjoint and no temporary is needed —
            // routing every return through a pooled vector was costing about
            // 150 host instructions per call.
            Ok((rbase, n)) if n != MULTI && nres != MULTI => {
                let start = self.base + rbase as usize;
                for i in 0..nres as usize {
                    let v = if (i as u16) < n {
                        self.stack[start + i].clone()
                    } else {
                        Value::Nil
                    };
                    self.stack[ret_to + i] = v;
                }
                Ok(())
            }
            Ok((rbase, n)) => {
                // A spread, which does need the values as a list.
                let mut vals = if n == MULTI {
                    self.take_multi()
                } else {
                    let start = self.base + rbase as usize;
                    let mut v = self.take_vec(n as usize);
                    for i in 0..n as usize {
                        v.push(self.stack[start + i].clone());
                    }
                    v
                };
                match nres {
                    // a spread reserves one register and reads the rest from
                    // the multi buffer
                    MULTI => {
                        self.stack[ret_to] = vals.first().cloned().unwrap_or(Value::Nil);
                        self.set_multi(vals);
                    }
                    want => {
                        for i in 0..want as usize {
                            self.stack[ret_to + i] =
                                vals.get(i).cloned().unwrap_or(Value::Nil);
                        }
                        vals.clear();
                        self.recycle_vec(std::mem::take(&mut vals));
                    }
                }
                Ok(())
            }
            Err(e) => Err(e),
        };

        self.close_frame(self.base, proto.n_regs);
        self.base = saved_base;
        self.upvals = saved_upvals;
        copied.map(|_| ())
    }

    /// Read a register.
    ///
    /// The compiler sizes every frame to `Proto::n_regs` and never emits a
    /// register outside it, so the index is in bounds by construction — and
    /// checking it again on every operand costs about 8% of the interpreter.
    #[inline]
    fn reg(&self, r: Reg) -> Value {
        self.at_reg(r).clone()
    }

    #[inline]
    fn at_reg(&self, r: Reg) -> &Value {
        debug_assert!((self.base + r as usize) < self.stack.len());
        // SAFETY: see above — the compiler guarantees the index.
        unsafe { self.stack.get_unchecked(self.base + r as usize) }
    }

    #[inline]
    fn set_reg(&mut self, r: Reg, v: Value) {
        debug_assert!((self.base + r as usize) < self.stack.len());
        let i = self.base + r as usize;
        // SAFETY: as in `at_reg`.
        unsafe { *self.stack.get_unchecked_mut(i) = v };
    }

    /// Run one function's code. The result is where its return values are:
    /// a register and a count, or [`MULTI`] for "in the multi buffer".
    fn execute(&mut self, entry: &Rc<Function>) -> Eval<(Reg, u16)> {
        let mut frames: Vec<CallFrame> = Vec::new();
        let out = self.run_frames(entry, &mut frames);
        if out.is_err() {
            // an error left calls suspended: give their registers back and put
            // the interpreter's state where the Rust caller expects it
            while let Some(fr) = frames.pop() {
                self.close_frame(self.base, fr.callee_regs);
                self.base = fr.base;
                self.upvals = fr.upvals;
                self.leave_depth();
                self.pop_frame();
            }
        }
        out
    }

    fn run_frames(
        &mut self,
        entry: &Rc<Function>,
        frames: &mut Vec<CallFrame>,
    ) -> Eval<(Reg, u16)> {
        let mut current = entry.clone();
        // SAFETY: `current` owns the proto and outlives every use of this
        // reference; it is refreshed whenever `current` changes.
        let mut proto: &Proto = unsafe { &*Rc::as_ptr(&current.proto) };
        let mut pc = 0usize;
        loop {
            let op = proto.code[pc];
            pc += 1;
            match op {
                Op::Const { dst, k } => {
                    let v = proto.consts[k as usize].clone();
                    self.set_reg(dst, v);
                }
                Op::Nil { dst } => self.set_reg(dst, Value::Nil),
                Op::Move { dst, src } => {
                    let v = self.reg(src);
                    self.set_reg(dst, v);
                }
                Op::GetGlobal { dst, g } => {
                    let slot = self.global_ref(proto, g);
                    let v = self.global_at(slot);
                    self.set_reg(dst, v);
                }
                Op::SetGlobal { g, src } => {
                    let slot = self.global_ref(proto, g);
                    let v = self.reg(src);
                    self.store_global(slot, v);
                }
                Op::GetUpval { dst, idx } => {
                    let v = self.upvals[idx as usize].borrow().clone();
                    self.set_reg(dst, v);
                }
                Op::SetUpval { idx, src } => {
                    let v = self.reg(src);
                    *self.upvals[idx as usize].borrow_mut() = v;
                }
                Op::GetCell { dst, slot } => {
                    let v = match &self.stack[self.base + slot as usize] {
                        Value::Cell(c) => c.borrow().clone(),
                        other => other.clone(),
                    };
                    self.set_reg(dst, v);
                }
                Op::SetCell { slot, src } => {
                    let v = self.reg(src);
                    match &self.stack[self.base + slot as usize] {
                        Value::Cell(c) => *c.borrow_mut() = v,
                        _ => self.set_reg(slot, Value::Cell(Rc::new(RefCell::new(v)))),
                    }
                }
                Op::NewCell { slot, src } => {
                    let v = self.reg(src);
                    self.set_reg(slot, Value::Cell(Rc::new(RefCell::new(v))));
                }
                Op::Bin { kind, dst, a, b } => {
                    let (x, y) = (self.at_reg(a), self.at_reg(b));
                    // the overwhelmingly common case, kept off the generic path
                    let v = if let (Value::Num(x), Value::Num(y)) = (x, y) {
                        num_op(kind, *x, *y)
                    } else {
                        let (x, y) = (x.clone(), y.clone());
                        arith(kind, x, y).map_err(|e| self.at(proto, pc, e))?
                    };
                    self.set_reg(dst, v);
                }
                Op::BinK { kind, dst, a, k } => {
                    let x = self.at_reg(a);
                    let v = match (x, &proto.consts[k as usize]) {
                        (Value::Num(x), Value::Num(y)) => num_op(kind, *x, *y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(kind, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    self.set_reg(dst, v);
                }
                // the specialised arithmetic: one branch, not two
                Op::Add { dst, a, b } => {
                    let v = match (self.at_reg(a), self.at_reg(b)) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x + y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Add, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    self.set_reg(dst, v);
                }
                Op::Sub { dst, a, b } => {
                    let v = match (self.at_reg(a), self.at_reg(b)) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x - y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Sub, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    self.set_reg(dst, v);
                }
                Op::Mul { dst, a, b } => {
                    let v = match (self.at_reg(a), self.at_reg(b)) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x * y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Mul, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    self.set_reg(dst, v);
                }
                Op::Div { dst, a, b } => {
                    let v = match (self.at_reg(a), self.at_reg(b)) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x / y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Div, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    self.set_reg(dst, v);
                }
                Op::AddK { dst, a, k } => {
                    let v = match (self.at_reg(a), &proto.consts[k as usize]) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x + y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Add, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    self.set_reg(dst, v);
                }
                Op::SubK { dst, a, k } => {
                    let v = match (self.at_reg(a), &proto.consts[k as usize]) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x - y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Sub, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    self.set_reg(dst, v);
                }
                Op::MulK { dst, a, k } => {
                    let v = match (self.at_reg(a), &proto.consts[k as usize]) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x * y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Mul, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    self.set_reg(dst, v);
                }
                Op::Neg { dst, a } => {
                    let v = self.reg(a);
                    let n = v.as_num().map_err(|e| self.at(proto, pc, e))?;
                    self.set_reg(dst, Value::Num(-n));
                }
                Op::Not { dst, a } => {
                    let v = Value::Bool(!self.reg(a).truthy());
                    self.set_reg(dst, v);
                }
                Op::Jump { to } => pc = to as usize,
                Op::JumpIfFalse { cond, to } => {
                    if !self.at_reg(cond).truthy() {
                        pc = to as usize;
                    }
                }
                Op::JumpIfTrue { cond, to } => {
                    if self.at_reg(cond).truthy() {
                        pc = to as usize;
                    }
                }
                Op::Call { base, nargs, nres } => {
                    self.set_line(proto.lines[pc - 1]);
                    // taking the callee by value avoids a second refcount:
                    // switching to it moves the handle into `current`
                    match self.reg(base) {
                        Value::Func(f) => {
                            match self.enter_frame(&f, base, nargs, nres, &current, pc, frames) {
                                Err(e) => return Err(self.here(proto, pc, e)),
                                Ok(true) => {
                                    current = f;
                                    proto = unsafe { &*Rc::as_ptr(&current.proto) };
                                    pc = 0;
                                }
                                Ok(false) => {}
                            }
                        }
                        callee => self
                            .dispatch(&callee, base, nargs, nres)
                            .map_err(|e| self.here(proto, pc, e))?,
                    }
                }
                Op::Method { base, name, nargs, nres } => {
                    self.set_line(proto.lines[pc - 1]);
                    let recv = self.reg(base + 1);
                    let name = match &proto.consts[name as usize] {
                        Value::Str(s) => s.clone(),
                        other => RStr::from(other.to_string()),
                    };
                    let m = self.method(&recv, &name).map_err(|e| self.at(proto, pc, e))?;
                    // the receiver is the first argument, as in Rust
                    self.dispatch(&m, base, nargs + 1, nres)
                        .map_err(|e| self.here(proto, pc, e))?;
                }
                Op::CallSpread { base, nargs, nres, method } => {
                    self.set_line(proto.lines[pc - 1]);
                    // fixed arguments, then everything the last call produced
                    let mut args = self.take_args(base + 1, nargs);
                    let extra = self.take_multi();
                    args.extend(extra.iter().cloned());
                    self.recycle_vec(extra);
                    let callee = if method == u16::MAX {
                        self.reg(base)
                    } else {
                        let recv = self.reg(base + 1);
                        let name = match &proto.consts[method as usize] {
                            Value::Str(s) => s.clone(),
                            other => RStr::from(other.to_string()),
                        };
                        self.method(&recv, &name).map_err(|e| self.at(proto, pc, e))?
                    };
                    let vals =
                        self.call_value(&callee, args).map_err(|e| self.here(proto, pc, e))?;
                    self.place(base, nres, vals);
                }
                Op::Ret { base, n } => match frames.pop() {
                    // the outermost function of this activation
                    None => return Ok((base, n)),
                    Some(fr) => {
                        let (caller, next_pc) = self.leave_frame(base, n, fr);
                        current = caller;
                        proto = unsafe { &*Rc::as_ptr(&current.proto) };
                        pc = next_pc;
                    }
                },
                Op::NewTable { dst } => self.set_reg(dst, Value::table(Table::new())),
                Op::GetIndex { dst, obj, key } => {
                    // `t[i]` on an array is the hot path of most real programs
                    let fast = match (self.at_reg(obj), self.at_reg(key)) {
                        (Value::Table(t), Value::Num(n)) => {
                            t.borrow().get_num(*n).cloned()
                        }
                        _ => None,
                    };
                    let v = match fast {
                        Some(v) => v,
                        None => {
                            let (o, k) = (self.reg(obj), self.reg(key));
                            self.index(&o, &k).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    self.set_reg(dst, v);
                }
                Op::GetIndexK { dst, obj, k } => {
                    let key = &proto.consts[k as usize];
                    let fast = match (self.at_reg(obj), key) {
                        (Value::Table(t), Value::Num(n)) => t.borrow().get_num(*n).cloned(),
                        (Value::Table(t), Value::Str(s)) => t.borrow().get_field(s),
                        _ => None,
                    };
                    let v = match fast {
                        Some(v) => v,
                        None => {
                            let (o, key) = (self.reg(obj), key.clone());
                            self.index(&o, &key).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    self.set_reg(dst, v);
                }
                Op::SetIndexK { obj, k, val } => {
                    let key = proto.consts[k as usize].clone();
                    let done = match (
                        &self.stack[self.base + obj as usize],
                        &key,
                        &self.stack[self.base + val as usize],
                    ) {
                        (Value::Table(t), Value::Num(n), v) => t.borrow_mut().set_num(*n, v),
                        _ => false,
                    };
                    if done {
                        continue;
                    }
                    let o = self.reg(obj);
                    let v = self.reg(val);
                    match o {
                        Value::Table(t) => {
                            let key = Key::from_value(&key).map_err(|e| self.at(proto, pc, e))?;
                            t.borrow_mut().set(key, v);
                        }
                        other => {
                            let e = Error(format!("cannot index a {} value", other.type_name()));
                            return Err(self.at(proto, pc, e));
                        }
                    }
                }
                Op::SetIndex { obj, key, val } => {
                    // an in-place write into the array part, likewise
                    let done = match (
                        &self.stack[self.base + obj as usize],
                        &self.stack[self.base + key as usize],
                        &self.stack[self.base + val as usize],
                    ) {
                        (Value::Table(t), Value::Num(n), v) => {
                            t.borrow_mut().set_num(*n, v)
                        }
                        _ => false,
                    };
                    if done {
                        continue;
                    }
                    let o = self.reg(obj);
                    let k = self.reg(key);
                    let v = self.reg(val);
                    match o {
                        Value::Table(t) => {
                            let key = Key::from_value(&k).map_err(|e| self.at(proto, pc, e))?;
                            t.borrow_mut().set(key, v);
                        }
                        other => {
                            let e = Error(format!("cannot index a {} value", other.type_name()));
                            return Err(self.at(proto, pc, e));
                        }
                    }
                }
                Op::Append { obj, val } => {
                    let v = self.reg(val);
                    if let Value::Table(t) = self.reg(obj) {
                        t.borrow_mut().push(v);
                    }
                }
                Op::AppendMulti { obj } => {
                    let vals = self.take_multi();
                    if let Value::Table(t) = self.reg(obj) {
                        let mut b = t.borrow_mut();
                        for v in &vals {
                            b.push(v.clone());
                        }
                    }
                    self.recycle_vec(vals);
                }
                Op::Closure { dst, proto: idx } => {
                    let child = proto.protos[idx as usize].clone();
                    let v = self.make_closure(child);
                    self.set_reg(dst, v);
                }
                Op::Range { dst, a, b, inclusive } => {
                    let start = self.reg(a).as_num().map_err(|e| self.at(proto, pc, e))?;
                    let end = self.reg(b).as_num().map_err(|e| self.at(proto, pc, e))?;
                    self.set_reg(dst, crate::stdlib::range_iterator(start, end, inclusive));
                }
                Op::IterInit { dst, src } => {
                    let v = match self.reg(src) {
                        // a table iterates its values, as a Rust `for` over a Vec
                        Value::Table(t) => crate::stdlib::value_iterator(t),
                        other => other,
                    };
                    self.set_reg(dst, v);
                }
                Op::IterNext { iter, base, count, exit } => {
                    let it = self.reg(iter);
                    let empty = self.take_vec(0);
                    let vals =
                        self.call_value(&it, empty).map_err(|e| self.here(proto, pc, e))?;
                    if matches!(vals.first(), None | Some(Value::Nil)) {
                        self.recycle_vec(vals);
                        pc = exit as usize;
                    } else {
                        for i in 0..count {
                            let v = vals.get(i as usize).cloned().unwrap_or(Value::Nil);
                            self.set_reg(base + i, v);
                        }
                        self.recycle_vec(vals);
                    }
                }
                Op::JumpIfNot { kind, a, b, to } => {
                    let (x, y) = (self.at_reg(a), self.at_reg(b));
                    let taken = if let (Value::Num(x), Value::Num(y)) = (x, y) {
                        num_cmp(kind, *x, *y)
                    } else {
                        let (x, y) = (x.clone(), y.clone());
                        arith(kind, x, y).map_err(|e| self.at(proto, pc, e))?.truthy()
                    };
                    if !taken {
                        pc = to as usize;
                    }
                }
                Op::JumpIfNotK { kind, a, k, to } => {
                    let (x, y) = (self.at_reg(a), &proto.consts[k as usize]);
                    let taken = match (x, y) {
                        (Value::Num(x), Value::Num(y)) => num_cmp(kind, *x, *y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(kind, x, y).map_err(|e| self.at(proto, pc, e))?.truthy()
                        }
                    };
                    if !taken {
                        pc = to as usize;
                    }
                }
                Op::JumpBack { to, id, hint, exit } => {
                    let counter = &proto.hints[hint as usize];
                    let n = counter.get().wrapping_add(1);
                    counter.set(n);
                    pc = if n % LOOP_BATCH == 0 && self.note_loop(proto, id) {
                        exit as usize
                    } else {
                        to as usize
                    };
                }
                Op::LoopHint { id, hint, exit } => {
                    // counting is a `Cell` bump; only every so often is it
                    // worth asking whether this loop deserves compiling
                    let counter = &proto.hints[hint as usize];
                    let n = counter.get().wrapping_add(1);
                    counter.set(n);
                    if n % LOOP_BATCH == 0 && self.note_loop(proto, id) {
                        pc = exit as usize;
                    }
                }
            }
        }
    }

    /// Make a call from registers: `callee`, then `nargs` arguments starting
    /// at `base + 1`, with results written back from `base`.
    fn dispatch(&mut self, callee: &Value, base: Reg, nargs: u16, nres: u16) -> Eval<()> {
        // a builtin reads the arguments straight out of the registers
        if let Value::Native(n) = callee {
            let n = n.clone();
            let start = self.base + base as usize + 1;
            let vals = {
                // SAFETY-free borrow dance: the native gets a snapshot of the
                // argument window, and natives never resize the register stack
                // out from under themselves (they call back through `call`,
                // which pushes a new frame above `top`).
                let args = self.stack[start..start + nargs as usize].to_vec();
                (n.f)(self, &args)?
            };
            self.place(base, nres, vals);
            return Ok(());
        }
        // a plain rua function needs no argument vector at all, and the callee
        // is already owned by our caller, so it needs no second refcount either
        if let Value::Func(func) = callee {
            if self.enter_depth()? {
                let arg_start = self.base + base as usize + 1;
                let ret_to = self.base + base as usize;
                let out = self.call_compiled_or_run(func, arg_start, nargs, ret_to, nres);
                self.leave_depth();
                return out;
            }
        }
        let args = self.take_args(base + 1, nargs);
        let vals = self.call_value(callee, args)?;
        self.place(base, nres, vals);
        Ok(())
    }

    /// Begin a call to a rua function from inside the loop.
    ///
    /// Returns whether the interpreter should switch to it: compiled code and
    /// anything the JIT can satisfy is handled here and needs no frame.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn enter_frame(
        &mut self,
        f: &Rc<Function>,
        base: Reg,
        nargs: u16,
        nres: u16,
        current: &Rc<Function>,
        pc: usize,
        frames: &mut Vec<CallFrame>,
    ) -> Eval<bool> {
        let arg_start = self.base + base as usize + 1;
        let ret_to = self.base + base as usize;
        self.enter_depth()?;
        if self.try_compiled_call(f, arg_start, nargs, ret_to, nres) {
            self.leave_depth();
            return Ok(false);
        }
        let callee_regs = f.proto.n_regs;
        frames.push(CallFrame {
            caller: current.clone(),
            base: self.base,
            pc,
            ret_to,
            nres,
            upvals: std::mem::replace(&mut self.upvals, f.upvals.clone()),
            line: self.line,
            callee_regs,
        });
        self.push_frame(Rc::as_ptr(&f.proto), self.line);
        if self.jit.enabled {
            self.current_fn = Some(f.clone());
        }
        let base = self.open_frame(callee_regs);
        for (i, p) in f.proto.params.iter().enumerate() {
            let v = if (i as u16) < nargs {
                self.stack[arg_start + i].clone()
            } else {
                Value::Nil
            };
            let slot = base + p.reg as usize;
            self.stack[slot] = if p.cell {
                Value::Cell(Rc::new(RefCell::new(v)))
            } else {
                v
            };
        }
        self.base = base;
        Ok(true)
    }

    /// Move a returning function's values into its caller's registers.
    #[inline(never)]
    fn return_values(&mut self, base: Reg, n: u16, ret_to: usize, nres: u16) {
        let start = self.base + base as usize;
        if n != MULTI && nres != MULTI {
            for i in 0..nres as usize {
                let v = if (i as u16) < n {
                    self.stack[start + i].clone()
                } else {
                    Value::Nil
                };
                self.stack[ret_to + i] = v;
            }
            return;
        }
        let mut vals = if n == MULTI {
            self.take_multi()
        } else {
            let mut v = self.take_vec(n as usize);
            for i in 0..n as usize {
                v.push(self.stack[start + i].clone());
            }
            v
        };
        match nres {
            MULTI => {
                self.stack[ret_to] = vals.first().cloned().unwrap_or(Value::Nil);
                self.set_multi(vals);
            }
            want => {
                for i in 0..want as usize {
                    self.stack[ret_to + i] = vals.get(i).cloned().unwrap_or(Value::Nil);
                }
                vals.clear();
                self.recycle_vec(vals);
            }
        }
    }

    /// Finish a call: hand the results back, give the registers up, and say
    /// where the caller left off.
    #[inline(never)]
    fn leave_frame(&mut self, base: Reg, n: u16, fr: CallFrame) -> (Rc<Function>, usize) {
        self.return_values(base, n, fr.ret_to, fr.nres);
        self.close_frame(self.base, fr.callee_regs);
        self.base = fr.base;
        self.upvals = fr.upvals;
        self.leave_depth();
        self.pop_frame();
        self.line = fr.line;
        (fr.caller, fr.pc)
    }

    /// Copy `n` registers out into a fresh (pooled) vector.
    fn take_args(&mut self, base: Reg, n: u16) -> Vec<Value> {
        let mut out = self.take_vec(n as usize);
        let start = self.base + base as usize;
        for i in 0..n as usize {
            out.push(self.stack[start + i].clone());
        }
        out
    }

    /// Put a call's results where the caller asked for them.
    fn place(&mut self, base: Reg, nres: u16, vals: Vec<Value>) {
        match nres {
            0 => self.recycle_vec(vals),
            MULTI => {
                let first = vals.first().cloned().unwrap_or(Value::Nil);
                self.set_reg(base, first);
                self.set_multi(vals);
            }
            n => {
                for i in 0..n {
                    let v = vals.get(i as usize).cloned().unwrap_or(Value::Nil);
                    self.set_reg(base + i, v);
                }
                self.recycle_vec(vals);
            }
        }
    }

    /// Note the line an instruction was on, so the error can say where it was.
    fn at(&self, proto: &Proto, pc: usize, e: Error) -> Signal {
        self.locate_at(proto.lines.get(pc.saturating_sub(1)).copied().unwrap_or(0), e)
    }

    /// The same, for an error that already travelled up from a call.
    fn here(&self, proto: &Proto, pc: usize, e: Signal) -> Signal {
        let line = proto.lines.get(pc.saturating_sub(1)).copied().unwrap_or(0);
        self.locate_signal(line, e)
    }
}

#[inline]
fn num_cmp(kind: BinKind, x: f64, y: f64) -> bool {
    match kind {
        BinKind::Lt => x < y,
        BinKind::Le => x <= y,
        BinKind::Gt => x > y,
        BinKind::Ge => x >= y,
        BinKind::Eq => x == y,
        BinKind::Ne => x != y,
        _ => unreachable!("only comparisons reach a fused branch"),
    }
}

#[inline]
fn num_op(kind: BinKind, x: f64, y: f64) -> Value {
    match kind {
        BinKind::Add => Value::Num(x + y),
        BinKind::Sub => Value::Num(x - y),
        BinKind::Mul => Value::Num(x * y),
        BinKind::Div => Value::Num(x / y),
        BinKind::Rem => Value::Num(x - (x / y).floor() * y),
        BinKind::Eq => Value::Bool(x == y),
        BinKind::Ne => Value::Bool(x != y),
        BinKind::Lt => Value::Bool(x < y),
        BinKind::Le => Value::Bool(x <= y),
        BinKind::Gt => Value::Bool(x > y),
        BinKind::Ge => Value::Bool(x >= y),
    }
}
