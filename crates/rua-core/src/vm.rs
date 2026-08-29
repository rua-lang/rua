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

impl Vm {
    /// Run a compiled function with `args`, and hand back what it returned.
    pub(crate) fn run(&mut self, func: &Rc<Function>, args: Vec<Value>) -> Eval<Vec<Value>> {
        let proto = func.proto.clone();
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

        let out = self.execute(&proto);
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
        let proto = func.proto.clone();
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

        let out = self.execute(&proto);
        let copied = match out {
            Ok((_, MULTI)) => {
                // the values are in the multi buffer, not in registers
                let first = self.multi_first();
                self.stack[ret_to] = first;
                Ok(true)
            }
            Ok((rbase, n)) => {
                let start = self.base + rbase as usize;
                let want = if nres == MULTI { n } else { nres };
                for i in 0..want as usize {
                    let v = if (i as u16) < n {
                        self.stack[start + i].clone()
                    } else {
                        Value::Nil
                    };
                    self.stack[ret_to + i] = v;
                }
                if nres == MULTI {
                    // a spread needs them as a list as well
                    let mut vals = self.take_vec(n as usize);
                    for i in 0..n as usize {
                        vals.push(self.stack[start + i].clone());
                    }
                    self.set_multi(vals);
                }
                Ok(false)
            }
            Err(e) => Err(e),
        };

        self.close_frame(self.base, proto.n_regs);
        self.base = saved_base;
        self.upvals = saved_upvals;
        copied.map(|_| ())
    }

    #[inline]
    fn reg(&self, r: Reg) -> Value {
        self.stack[self.base + r as usize].clone()
    }

    #[inline]
    fn set_reg(&mut self, r: Reg, v: Value) {
        self.stack[self.base + r as usize] = v;
    }

    /// Run one function's code. The result is where its return values are:
    /// a register and a count, or [`MULTI`] for "in the multi buffer".
    fn execute(&mut self, proto: &Rc<Proto>) -> Eval<(Reg, u16)> {
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
                    let (x, y) = (
                        &self.stack[self.base + a as usize],
                        &self.stack[self.base + b as usize],
                    );
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
                    let x = &self.stack[self.base + a as usize];
                    let v = match (x, &proto.consts[k as usize]) {
                        (Value::Num(x), Value::Num(y)) => num_op(kind, *x, *y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(kind, x, y).map_err(|e| self.at(proto, pc, e))?
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
                    if !self.stack[self.base + cond as usize].truthy() {
                        pc = to as usize;
                    }
                }
                Op::JumpIfTrue { cond, to } => {
                    if self.stack[self.base + cond as usize].truthy() {
                        pc = to as usize;
                    }
                }
                Op::Call { base, nargs, nres } => {
                    self.set_line(proto.lines[pc - 1]);
                    let callee = self.reg(base);
                    self.dispatch(&callee, base, nargs, nres)
                        .map_err(|e| self.here(proto, pc, e))?;
                }
                Op::Method { base, name, nargs, nres } => {
                    self.set_line(proto.lines[pc - 1]);
                    let recv = self.reg(base + 1);
                    let name = match &proto.consts[name as usize] {
                        Value::Str(s) => s.clone(),
                        other => Rc::from(other.to_string().as_str()),
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
                            other => Rc::from(other.to_string().as_str()),
                        };
                        self.method(&recv, &name).map_err(|e| self.at(proto, pc, e))?
                    };
                    let vals =
                        self.call_value(&callee, args).map_err(|e| self.here(proto, pc, e))?;
                    self.place(base, nres, vals);
                }
                Op::Ret { base, n } => return Ok((base, n)),
                Op::NewTable { dst } => self.set_reg(dst, Value::table(Table::new())),
                Op::GetIndex { dst, obj, key } => {
                    let (o, k) = (self.reg(obj), self.reg(key));
                    let v = self.index(&o, &k).map_err(|e| self.at(proto, pc, e))?;
                    self.set_reg(dst, v);
                }
                Op::SetIndex { obj, key, val } => {
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
        // a plain rua function needs no argument vector at all
        if let Value::Func(func) = callee {
            let func = func.clone();
            if self.enter_depth()? {
                let arg_start = self.base + base as usize + 1;
                let ret_to = self.base + base as usize;
                let out = self.call_compiled_or_run(&func, arg_start, nargs, ret_to, nres);
                self.leave_depth();
                return out;
            }
        }
        let args = self.take_args(base + 1, nargs);
        let vals = self.call_value(callee, args)?;
        self.place(base, nres, vals);
        Ok(())
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
    fn at(&self, proto: &Rc<Proto>, pc: usize, e: Error) -> Signal {
        self.locate_at(proto.lines.get(pc.saturating_sub(1)).copied().unwrap_or(0), e)
    }

    /// The same, for an error that already travelled up from a call.
    fn here(&self, proto: &Rc<Proto>, pc: usize, e: Signal) -> Signal {
        let line = proto.lines.get(pc.saturating_sub(1)).copied().unwrap_or(0);
        self.locate_signal(line, e)
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
