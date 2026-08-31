//! The virtual machine: one function's bytecode at a time.
//!
//! Registers are a window into `Vm::stack` starting at `Vm::base`, so a call is
//! a resize and two saved fields rather than an allocation. Control flow is
//! jumps, which is why `break`, `continue` and `return` need no unwinding
//! machinery here — only errors travel back up the Rust stack.

use crate::bytecode::*;
use crate::interp::{arith, Eval, Signal, Vm, LOOP_BATCH, LOOP_READY};
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
    /// Where the caller's registers and results are. These are stack offsets,
    /// and the stack is bounded by the recursion limit times a frame size, so
    /// they need no more than 32 bits — the record is built and copied on
    /// every single call, and half of it was padding and spare range.
    base: u32,
    pc: u32,
    ret_to: u32,
    /// The line the call was made from, for the traceback.
    line: u32,
    /// How many results the caller wants.
    nres: u16,
    /// The end of the caller's frame. The callee's frame starts on top of the
    /// arguments and may be shorter, so this is what says where the caller's
    /// registers stop when it is time to release them.
    top: u32,
}

impl Vm {
    /// Run a compiled function with `args`, and hand back what it returned.
    pub(crate) fn run(&mut self, func: &Rc<Function>, args: Vec<Value>) -> Eval<Vec<Value>> {
        // borrowed, not cloned: the caller holds the function alive, and a
        // refcount round trip per call is visible in a profile
        let proto = &func.proto;
        let saved_base = self.base;
        self.base = self.open_frame(proto.n_regs);

        let mut args = args;
        for (i, p) in proto.params.iter().enumerate() {
            let v = args.get_mut(i).map(std::mem::take).unwrap_or(Value::Nil);
            let slot = self.base + p.reg as usize;
            let v = if p.cell {
                Value::Cell(Rc::new(RefCell::new(v)))
            } else {
                v
            };
            Value::put(&mut self.stack[slot], v);
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
                    vals.push(std::mem::take(&mut self.stack[start + i]));
                }
                Ok(vals)
            }
            Err(e) => Err(e),
        };

        self.close_frame(self.base);
        self.base = saved_base;
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
        self.base = self.open_frame(proto.n_regs);
        for (i, p) in proto.params.iter().enumerate() {
            let v = if (i as u16) < nargs {
                std::mem::take(&mut self.stack[arg_start + i])
            } else {
                Value::Nil
            };
            let slot = self.base + p.reg as usize;
            let v = if p.cell {
                Value::Cell(Rc::new(RefCell::new(v)))
            } else {
                v
            };
            Value::put(&mut self.stack[slot], v);
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
                    Value::put(&mut self.stack[ret_to + i], v);
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

        self.close_frame(self.base);
        self.base = saved_base;
        copied.map(|_| ())
    }

    #[inline]
    fn set_reg(&mut self, r: Reg, v: Value) {
        debug_assert!((self.base + r as usize) < self.stack.len());
        let i = self.base + r as usize;
        // SAFETY: as in `at_reg`.
        unsafe { Value::put(self.stack.get_unchecked_mut(i), v) };
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
                self.close_frame(self.base);
                self.base = fr.base as usize;
                self.top = fr.top as usize;
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

        // The registers of the running frame, and the running function's code,
        // as raw pointers.
        //
        // Reading a register through `self.stack[self.base + r]` looks free and
        // is not: `self` is behind a mutable reference that every handler may
        // write through, so the compiler must reload the vector's pointer and
        // the frame's base from memory at every single operand -- three loads
        // and a bounds check to reach two words that did not move. Holding them
        // here costs one refresh per call and return, which is where they
        // actually change.
        //
        // The invariant is checked on every instruction in a debug build, so a
        // handler that grows the stack or switches frames without saying so
        // fails the test suite rather than reading freed memory.
        let mut regs: *mut Value = unsafe { self.stack.as_mut_ptr().add(self.base) };
        let mut code: *const Op = proto.code.as_ptr();
        macro_rules! resync {
            () => {{
                regs = unsafe { self.stack.as_mut_ptr().add(self.base) };
                code = proto.code.as_ptr();
            }};
        }
        macro_rules! at {
            ($r:expr) => {
                // SAFETY: the compiler sizes every frame to `n_regs` and never
                // emits a register outside it
                unsafe { &*regs.add($r as usize) }
            };
        }
        macro_rules! get {
            ($r:expr) => {
                at!($r).clone()
            };
        }
        macro_rules! set {
            ($r:expr, $v:expr) => {{
                let v = $v;
                unsafe { Value::put(&mut *regs.add($r as usize), v) }
            }};
        }
        loop {
            debug_assert_eq!(regs, unsafe {
                self.stack.as_mut_ptr().add(self.base)
            });
            debug_assert_eq!(code, proto.code.as_ptr());
            debug_assert!(pc < proto.code.len());
            // SAFETY: `pc` only ever moves to a jump target the compiler
            // emitted, and every path out of the code ends in `Ret`
            let op = unsafe { *code.add(pc) };
            pc += 1;
            match op {
                Op::Const { dst, k } => {
                    let v = proto.consts[k as usize].clone();
                    set!(dst, v);
                }
                Op::Nil { dst } => set!(dst, Value::Nil),
                Op::Move { dst, src } => {
                    let v = get!(src);
                    set!(dst, v);
                }
                Op::GetGlobal { dst, g } => {
                    let slot = self.global_ref(proto, g);
                    let v = self.global_at(slot);
                    set!(dst, v);
                }
                Op::SetGlobal { g, src } => {
                    let slot = self.global_ref(proto, g);
                    let v = get!(src);
                    self.store_global(slot, v);
                }
                Op::GetUpval { dst, idx } => {
                    let v = current.upvals[idx as usize].borrow().clone();
                    set!(dst, v);
                }
                Op::SetUpval { idx, src } => {
                    let v = get!(src);
                    *current.upvals[idx as usize].borrow_mut() = v;
                }
                Op::GetCell { dst, slot } => {
                    let v = match at!(slot) {
                        Value::Cell(c) => c.borrow().clone(),
                        other => other.clone(),
                    };
                    set!(dst, v);
                }
                Op::SetCell { slot, src } => {
                    let v = get!(src);
                    match at!(slot) {
                        Value::Cell(c) => *c.borrow_mut() = v,
                        _ => set!(slot, Value::Cell(Rc::new(RefCell::new(v)))),
                    }
                }
                Op::NewCell { slot, src } => {
                    let v = get!(src);
                    set!(slot, Value::Cell(Rc::new(RefCell::new(v))));
                }
                Op::Bin { kind, dst, a, b } => {
                    let (x, y) = (at!(a), at!(b));
                    // the overwhelmingly common case, kept off the generic path
                    let v = if let (Value::Num(x), Value::Num(y)) = (x, y) {
                        num_op(kind, *x, *y)
                    } else if let Some(b) = crate::interp::equality(kind, x, y) {
                        Value::Bool(b)
                    } else {
                        let (x, y) = (x.clone(), y.clone());
                        arith(kind, x, y).map_err(|e| self.at(proto, pc, e))?
                    };
                    set!(dst, v);
                }
                Op::BinK { kind, dst, a, k } => {
                    let x = at!(a);
                    let v = match (x, &proto.consts[k as usize]) {
                        (Value::Num(x), Value::Num(y)) => num_op(kind, *x, *y),
                        (x, y) => match crate::interp::equality(kind, x, y) {
                            Some(b) => Value::Bool(b),
                            None => {
                                let (x, y) = (x.clone(), y.clone());
                                arith(kind, x, y).map_err(|e| self.at(proto, pc, e))?
                            }
                        },
                    };
                    set!(dst, v);
                }
                // the specialised arithmetic: one branch, not two
                Op::Add { dst, a, b } => {
                    let v = match (at!(a), at!(b)) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x + y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Add, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    set!(dst, v);
                }
                Op::Sub { dst, a, b } => {
                    let v = match (at!(a), at!(b)) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x - y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Sub, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    set!(dst, v);
                }
                Op::Mul { dst, a, b } => {
                    let v = match (at!(a), at!(b)) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x * y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Mul, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    set!(dst, v);
                }
                Op::Div { dst, a, b } => {
                    let v = match (at!(a), at!(b)) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x / y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Div, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    set!(dst, v);
                }
                Op::AddK { dst, a, k } => {
                    let v = match (at!(a), &proto.consts[k as usize]) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x + y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Add, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    set!(dst, v);
                }
                Op::SubK { dst, a, k } => {
                    let v = match (at!(a), &proto.consts[k as usize]) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x - y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Sub, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    set!(dst, v);
                }
                Op::MulK { dst, a, k } => {
                    let v = match (at!(a), &proto.consts[k as usize]) {
                        (Value::Num(x), Value::Num(y)) => Value::Num(x * y),
                        (x, y) => {
                            let (x, y) = (x.clone(), y.clone());
                            arith(BinKind::Mul, x, y).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    set!(dst, v);
                }
                Op::Neg { dst, a } => {
                    let v = get!(a);
                    let n = v.as_num().map_err(|e| self.at(proto, pc, e))?;
                    set!(dst, Value::Num(-n));
                }
                Op::Not { dst, a } => {
                    let v = Value::Bool(!get!(a).truthy());
                    set!(dst, v);
                }
                Op::Jump { to } => pc = to as usize,
                Op::JumpIfFalse { cond, to } => {
                    if !at!(cond).truthy() {
                        pc = to as usize;
                    }
                }
                Op::JumpIfTrue { cond, to } => {
                    if at!(cond).truthy() {
                        pc = to as usize;
                    }
                }
                Op::Call { base, nargs, nres } => {
                    self.set_line(proto.lines[pc - 1]);
                    // The callee register is dead once the call is under way
                    // — the results land on top of it — so the handle is moved
                    // out rather than copied: no reference count on the way in
                    // and none on the way out.
                    match unsafe { std::mem::take(&mut *regs.add(base as usize)) } {
                        Value::Func(f) => {
                            match self.enter_frame(&f, base, nargs, nres, &current, pc, frames) {
                                Err(e) => return Err(self.here(proto, pc, e)),
                                Ok(true) => {
                                    current = f;
                                    proto = unsafe { &*Rc::as_ptr(&current.proto) };
                                    pc = 0;
                                    resync!();
                                }
                                // a native or compiled callee: it ran to
                                // completion, and may have grown the stack
                                Ok(false) => resync!(),
                            }
                        }
                        callee => {
                            self.dispatch(&callee, base, nargs, nres)
                                .map_err(|e| self.here(proto, pc, e))?;
                            resync!();
                        }
                    }
                }
                Op::Method { base, name, nargs, nres } => {
                    self.set_line(proto.lines[pc - 1]);
                    let recv = get!(base + 1);
                    // the name is a constant of this function: borrow it
                    let m = match &proto.consts[name as usize] {
                        Value::Str(s) => self.method(&recv, s),
                        other => self.method(&recv, &RStr::from(other.to_string())),
                    }
                    .map_err(|e| self.at(proto, pc, e))?;
                    // the receiver is the first argument, as in Rust
                    self.dispatch(&m, base, nargs + 1, nres)
                        .map_err(|e| self.here(proto, pc, e))?;
                    resync!();
                }
                Op::CallSpread { base, nargs, nres, method } => {
                    self.set_line(proto.lines[pc - 1]);
                    // The callee is found first: for a method call the
                    // receiver is also the first argument, and collecting the
                    // arguments moves it out of its register.
                    let callee = if method == u16::MAX {
                        get!(base)
                    } else {
                        let recv = get!(base + 1);
                        match &proto.consts[method as usize] {
                            Value::Str(s) => self.method(&recv, s),
                            other => self.method(&recv, &RStr::from(other.to_string())),
                        }
                        .map_err(|e| self.at(proto, pc, e))?
                    };
                    // fixed arguments, then everything the last call produced
                    let mut args = self.take_args(base + 1, nargs);
                    let extra = self.take_multi();
                    args.extend(extra.iter().cloned());
                    self.recycle_vec(extra);
                    let vals =
                        self.call_value(&callee, args).map_err(|e| self.here(proto, pc, e))?;
                    self.place(base, nres, vals);
                    resync!();
                }
                Op::Ret { base, n } => match frames.pop() {
                    // the outermost function of this activation
                    None => return Ok((base, n)),
                    Some(fr) => {
                        let (caller, next_pc) = self.leave_frame(base, n, fr);
                        current = caller;
                        proto = unsafe { &*Rc::as_ptr(&current.proto) };
                        pc = next_pc;
                        resync!();
                    }
                },
                Op::NewTable { dst } => set!(dst, Value::table(Table::new())),
                Op::GetIndex { dst, obj, key } => {
                    // `t[i]` on an array is the hot path of most real programs
                    let fast = match (at!(obj), at!(key)) {
                        (Value::Table(t), Value::Num(n)) => {
                            t.borrow().get_num(*n).cloned()
                        }
                        _ => None,
                    };
                    let v = match fast {
                        Some(v) => v,
                        None => {
                            let (o, k) = (get!(obj), get!(key));
                            self.index(&o, &k).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    set!(dst, v);
                }
                Op::GetIndexK { dst, obj, k, ic } => {
                    let key = &proto.consts[k as usize];
                    let fast = match (at!(obj), key) {
                        (Value::Table(t), Value::Num(n)) => t.borrow().get_num(*n).cloned(),
                        (Value::Table(t), Value::Str(s)) => {
                            let at = &proto.caches[ic as usize];
                            Some(t.borrow().get_field_cached(s, at))
                        }
                        _ => None,
                    };
                    let v = match fast {
                        Some(v) => v,
                        None => {
                            let (o, key) = (get!(obj), key.clone());
                            self.index(&o, &key).map_err(|e| self.at(proto, pc, e))?
                        }
                    };
                    set!(dst, v);
                }
                Op::SetIndexK { obj, k, val } => {
                    let key = proto.consts[k as usize].clone();
                    let done = match (
                        at!(obj),
                        &key,
                        at!(val),
                    ) {
                        (Value::Table(t), Value::Num(n), v) => t.borrow_mut().set_num(*n, v),
                        (Value::Table(t), Value::Str(s), v) => t.borrow_mut().set_field(s, v),
                        _ => false,
                    };
                    if done {
                        continue;
                    }
                    let o = get!(obj);
                    let v = get!(val);
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
                        at!(obj),
                        at!(key),
                        at!(val),
                    ) {
                        (Value::Table(t), Value::Num(n), v) => {
                            t.borrow_mut().set_num(*n, v)
                        }
                        // `t[name] = v` with the name in a register
                        (Value::Table(t), Value::Str(s), v) => t.borrow_mut().set_field(s, v),
                        _ => false,
                    };
                    if done {
                        continue;
                    }
                    let o = get!(obj);
                    let k = get!(key);
                    let v = get!(val);
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
                    let v = get!(val);
                    if let Value::Table(t) = get!(obj) {
                        t.borrow_mut().push(v);
                    }
                }
                Op::AppendMulti { obj } => {
                    let vals = self.take_multi();
                    if let Value::Table(t) = get!(obj) {
                        let mut b = t.borrow_mut();
                        for v in &vals {
                            b.push(v.clone());
                        }
                    }
                    self.recycle_vec(vals);
                }
                Op::Closure { dst, proto: idx } => {
                    let child = proto.protos[idx as usize].clone();
                    let v = self.make_closure(child, &current.upvals);
                    set!(dst, v);
                }
                Op::Range { dst, a, b, inclusive } => {
                    let start = get!(a).as_num().map_err(|e| self.at(proto, pc, e))?;
                    let end = get!(b).as_num().map_err(|e| self.at(proto, pc, e))?;
                    set!(dst, crate::stdlib::range_iterator(start, end, inclusive));
                }
                Op::IterInit { dst, src } => {
                    let v = match get!(src) {
                        // a table iterates its values, as a Rust `for` over a Vec
                        Value::Table(t) => crate::stdlib::value_iterator(t),
                        other => other,
                    };
                    set!(dst, v);
                }
                Op::IterNext { iter, base, count, exit } => {
                    let it = get!(iter);
                    let empty = self.take_vec(0);
                    let vals =
                        self.call_value(&it, empty).map_err(|e| self.here(proto, pc, e))?;
                    resync!();
                    if matches!(vals.first(), None | Some(Value::Nil)) {
                        self.recycle_vec(vals);
                        pc = exit as usize;
                    } else {
                        for i in 0..count {
                            let v = vals.get(i as usize).cloned().unwrap_or(Value::Nil);
                            set!(base + i, v);
                        }
                        self.recycle_vec(vals);
                    }
                }
                Op::JumpIfNot { kind, a, b, to } => {
                    let (x, y) = (at!(a), at!(b));
                    let taken = if let (Value::Num(x), Value::Num(y)) = (x, y) {
                        num_cmp(kind, *x, *y)
                    } else if let Some(b) = crate::interp::equality(kind, x, y) {
                        b
                    } else {
                        let (x, y) = (x.clone(), y.clone());
                        arith(kind, x, y).map_err(|e| self.at(proto, pc, e))?.truthy()
                    };
                    if !taken {
                        pc = to as usize;
                    }
                }
                Op::JumpIfNotK { kind, a, k, to } => {
                    let (x, y) = (at!(a), &proto.consts[k as usize]);
                    let taken = match (x, y) {
                        (Value::Num(x), Value::Num(y)) => num_cmp(kind, *x, *y),
                        (x, y) => match crate::interp::equality(kind, x, y) {
                            Some(b) => b,
                            None => {
                                let (x, y) = (x.clone(), y.clone());
                                arith(kind, x, y).map_err(|e| self.at(proto, pc, e))?.truthy()
                            }
                        },
                    };
                    if !taken {
                        pc = to as usize;
                    }
                }
                Op::ForLoop { counter, limit, to, id, hint, le } => {
                    // step and test in one, and falling through is the exit
                    let step = match (at!(counter), at!(limit)) {
                        (Value::Num(i), Value::Num(l)) => {
                            let next = i + 1.0;
                            Some((next, if le { next <= *l } else { next < *l }))
                        }
                        _ => None,
                    };
                    let going = match step {
                        Some((next, going)) => {
                            set!(counter, Value::Num(next));
                            going
                        }
                        // a loop variable the body turned into something that
                        // is not a number: let the ordinary paths say so
                        None => {
                            let kind = if le { BinKind::Le } else { BinKind::Lt };
                            let next = arith(BinKind::Add, get!(counter), Value::Num(1.0))
                                .map_err(|e| self.at(proto, pc, e))?;
                            let going = arith(kind, next.clone(), get!(limit))
                                .map_err(|e| self.at(proto, pc, e))?
                                .truthy();
                            set!(counter, next);
                            going
                        }
                    };
                    if going {
                        let hits = &proto.hints[hint as usize];
                        let n = hits.get();
                        // when the JIT takes the loop over it runs it out, and
                        // the instruction after this one is where that ends
                        let taken = if n == LOOP_READY {
                            let ran = self.enter_loop(id);
                            if !ran {
                                // its locals are not what the compiled code
                                // wants this time: go back to counting
                                proto.hints[hint as usize].set(0);
                            }
                            ran
                        } else {
                            let n = n.wrapping_add(1);
                            hits.set(n);
                            let ran = n % LOOP_BATCH == 0
                                && self.note_loop(proto, &current, id);
                            if ran {
                                proto.hints[hint as usize].set(LOOP_READY);
                            }
                            ran
                        };
                        if !taken {
                            pc = to as usize;
                        }
                        resync!();
                    }
                }
                Op::JumpBack { to, id, hint, exit } => {
                    let counter = &proto.hints[hint as usize];
                    let n = counter.get();
                    let taken = if n == LOOP_READY {
                        let ran = self.enter_loop(id);
                        if !ran {
                            proto.hints[hint as usize].set(0);
                        }
                        ran
                    } else {
                        let n = n.wrapping_add(1);
                        counter.set(n);
                        let ran = n % LOOP_BATCH == 0 && self.note_loop(proto, &current, id);
                        if ran {
                            proto.hints[hint as usize].set(LOOP_READY);
                        }
                        ran
                    };
                    pc = if taken { exit as usize } else { to as usize };
                    resync!();
                }
                Op::LoopHint { id, hint, exit } => {
                    // counting is a `Cell` bump; only every so often is it
                    // worth asking whether this loop deserves compiling
                    let counter = &proto.hints[hint as usize];
                    let n = counter.get().wrapping_add(1);
                    counter.set(n);
                    if n % LOOP_BATCH == 0 && self.note_loop(proto, &current, id) {
                        pc = exit as usize;
                    }
                    resync!();
                }
            }
        }
    }

    /// Make a call from registers: `callee`, then `nargs` arguments starting
    /// at `base + 1`, with results written back from `base`.
    fn dispatch(&mut self, callee: &Value, base: Reg, nargs: u16, nres: u16) -> Eval<()> {
        // a builtin reads the arguments straight out of the registers
        if let Value::Native(n) = callee {
            // the common builtin: one argument in a register, one result into
            // another, and no vectors at either end
            if nres <= 1 {
                let start = self.base + base as usize + 1;
                if nargs == 1 {
                    if let Some(fast) = &n.fast1 {
                        let a = self.stack[start].clone();
                        let v = fast(&a)?;
                        if nres == 1 {
                            self.set_reg(base, v);
                        }
                        self.multi_at = None;
                        return Ok(());
                    }
                } else if nargs == 2 {
                    if let Some(fast) = &n.fast2 {
                        let a = self.stack[start].clone();
                        let b = self.stack[start + 1].clone();
                        let v = fast(&a, &b)?;
                        if nres == 1 {
                            self.set_reg(base, v);
                        }
                        self.multi_at = None;
                        return Ok(());
                    }
                }
            }
            let n = n.clone();
            let start = self.base + base as usize + 1;
            // The native gets the argument window as a slice. The vector
            // comes from the pool and goes back to it, so an ordinary builtin
            // call allocates nothing at all: the values are moved out of the
            // registers, which the compiler has finished with.
            let mut args = self.take_vec(nargs as usize);
            for i in 0..nargs as usize {
                args.push(std::mem::take(&mut self.stack[start + i]));
            }
            let out = (n.f)(self, &args);
            self.recycle_vec(args);
            let vals = out?;
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
        // A function the JIT has refused is the common case — every function
        // that is not numeric, and every function at all when the JIT is off —
        // and asking about it is a flag read, not a call.
        if f.jit_state.get() != JitState::Blocked
            && self.try_compiled_call(f, arg_start, nargs, ret_to, nres)
        {
            self.leave_depth();
            return Ok(false);
        }
        frames.push(CallFrame {
            caller: current.clone(),
            base: self.base as u32,
            pc: pc as u32,
            ret_to: ret_to as u32,
            line: self.line,
            nres,
            top: self.top as u32,
        });
        self.push_frame(Rc::as_ptr(&f.proto), self.line);
        // The frame opens on the arguments, so an argument that is already in
        // the right register — which every one of them is, since parameters
        // take the first registers in order — needs no move at all.
        let base = arg_start;
        self.open_frame_at(base, f.proto.n_regs);
        self.bind_params(&f.proto, base, nargs);
        self.base = base;
        Ok(true)
    }

    /// Settle the parameters of a frame that opened on its arguments.
    ///
    /// Nothing to do for the common call: the arguments are already in the
    /// registers the parameters name. What is left is the missing arguments,
    /// which are nil, and the captured ones, which have to become cells.
    #[inline]
    fn bind_params(&mut self, proto: &Proto, base: usize, nargs: u16) {
        for (i, p) in proto.params.iter().enumerate() {
            debug_assert_eq!(
                p.reg as usize, i,
                "parameters take the first registers, in order"
            );
            let slot = base + p.reg as usize;
            if (i as u16) >= nargs {
                Value::put(&mut self.stack[slot], Value::Nil);
            }
            if p.cell {
                let v = std::mem::take(&mut self.stack[slot]);
                self.stack[slot] = Value::Cell(Rc::new(RefCell::new(v)));
            }
        }
    }

    /// Move a returning function's values into its caller's registers.
    #[inline(never)]
    fn return_values(&mut self, base: Reg, n: u16, ret_to: usize, nres: u16) -> usize {
        let start = self.base + base as usize;
        if n != MULTI && nres != MULTI {
            // the frame these come from is about to be released, so they are
            // moved rather than copied; the destination is always below the
            // source, so nothing is overwritten before it is read
            for i in 0..nres as usize {
                let v = if (i as u16) < n {
                    std::mem::take(&mut self.stack[start + i])
                } else {
                    Value::Nil
                };
                Value::put(&mut self.stack[ret_to + i], v);
            }
            return nres as usize;
        }
        // Whatever is being returned, find where it is: the returning
        // function's registers, or the results of the call it ended with,
        // which are usually still in registers themselves.
        let from = if n == MULTI {
            match self.multi_in_regs() {
                Some((at, k)) => Some((at, k)),
                None => None,
            }
        } else {
            Some((start, n))
        };

        // A caller that wants a spread reserved one register and reads the rest
        // from the buffer, so results can only stay in registers when there is
        // at most one of them — or when the caller asked for a fixed count, and
        // reserved exactly that many.
        let fits = from.is_some_and(|(_, count)| nres != MULTI || count <= 1);
        if let (true, Some((src, count))) = (fits, from) {
            // A register to register move: the source frame sits above the
            // destination, so the two windows never overlap.
            let want = if nres == MULTI { count } else { nres };
            for i in 0..want as usize {
                let v = if (i as u16) < count {
                    self.stack[src + i].clone()
                } else {
                    Value::Nil
                };
                Value::put(&mut self.stack[ret_to + i], v);
            }
            if nres == MULTI {
                self.set_multi_at(ret_to, want);
            } else {
                self.multi_at = None;
            }
            return want as usize;
        }

        // The remaining cases need the values as a list: more results than the
        // caller reserved room for, or results a native produced.
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
                let first = vals.first().cloned().unwrap_or(Value::Nil);
                Value::put(&mut self.stack[ret_to], first);
                self.set_multi(vals);
                1
            }
            want => {
                for i in 0..want as usize {
                    let v = vals.get_mut(i).map(std::mem::take).unwrap_or(Value::Nil);
                    Value::put(&mut self.stack[ret_to + i], v);
                }
                vals.clear();
                self.recycle_vec(vals);
                want as usize
            }
        }
    }

    /// Finish a call: hand the results back, give the registers up, and say
    /// where the caller left off.
    #[inline(never)]
    fn leave_frame(&mut self, base: Reg, n: u16, fr: CallFrame) -> (Rc<Function>, usize) {
        let written = self.return_values(base, n, fr.ret_to as usize, fr.nres);
        self.close_frame_from(self.base, fr.ret_to as usize + written);
        self.base = fr.base as usize;
        self.top = fr.top as usize;
        self.leave_depth();
        self.pop_frame();
        self.line = fr.line;
        (fr.caller, fr.pc as usize)
    }

    /// Move `n` registers out into a fresh (pooled) vector.
    ///
    /// The compiler always builds an argument list in fresh temporaries above
    /// every live local, and never reads them again, so the values can be moved
    /// rather than cloned: no reference count goes up here and none comes back
    /// down when the frame closes.
    fn take_args(&mut self, base: Reg, n: u16) -> Vec<Value> {
        let mut out = self.take_vec(n as usize);
        let start = self.base + base as usize;
        for i in 0..n as usize {
            out.push(std::mem::take(&mut self.stack[start + i]));
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
                // the results are on their way to the pool, so they are moved
                // out rather than copied: a builtin returning a string or a
                // table should not touch its reference count twice on the way
                // to the register it was asked for
                let mut vals = vals;
                for i in 0..n as usize {
                    let v = vals.get_mut(i).map(std::mem::take).unwrap_or(Value::Nil);
                    self.set_reg(base + i as u16, v);
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

#[cfg(test)]
mod frame_size {
    #[test]
    fn call_frame_is_small() {
        // The record is built and copied on every call; keep an eye on it.
        assert_eq!(std::mem::size_of::<super::CallFrame>(), 32);
    }
}
