//! AST to bytecode.
//!
//! The resolver has already turned names into frame slots, so this pass is
//! mostly about laying out registers: locals keep their slots, and anything
//! else the compiler needs is a temporary above them.

use crate::bytecode::*;
use crate::value::Value;
use rua_syntax::ast::*;
use std::rc::Rc;

pub fn compile_chunk(body: &Block, n_slots: usize, def: Rc<FuncDef>) -> Rc<Proto> {
    let mut f = FnCompiler::new(def, n_slots);
    f.block_ret(body);
    Rc::new(f.finish())
}

/// Where `break` and `continue` should jump to, once we know.
struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
}

struct FnCompiler {
    def: Rc<FuncDef>,
    code: Vec<Op>,
    lines: Vec<u32>,
    consts: Vec<Value>,
    protos: Vec<Rc<Proto>>,
    globals: Vec<GlobalRef>,
    hints: usize,
    free: Reg,
    max_regs: Reg,
    loops: Vec<LoopCtx>,
    line: u32,
}

impl FnCompiler {
    fn new(def: Rc<FuncDef>, n_slots: usize) -> FnCompiler {
        FnCompiler {
            def,
            code: Vec::new(),
            lines: Vec::new(),
            consts: Vec::new(),
            protos: Vec::new(),
            globals: Vec::new(),
            hints: 0,
            free: n_slots as Reg,
            max_regs: n_slots as Reg,
            loops: Vec::new(),
            line: 0,
        }
    }

    fn finish(self) -> Proto {
        let params = self
            .def
            .param_bindings
            .iter()
            .map(|b| ParamSlot { reg: b.slot, cell: b.cell })
            .collect();
        Proto {
            name: Rc::from(self.def.name.as_str()),
            def: self.def,
            code: self.code,
            lines: self.lines,
            consts: self.consts,
            protos: self.protos,
            globals: self.globals,
            n_regs: self.max_regs as usize + 1,
            hints: (0..self.hints).map(|_| std::cell::Cell::new(0)).collect(),
            params,
        }
    }

    // ---- registers and constants -------------------------------------------

    fn alloc(&mut self) -> Reg {
        let r = self.free;
        self.free += 1;
        self.max_regs = self.max_regs.max(self.free);
        r
    }

    fn mark(&self) -> Reg {
        self.free
    }

    fn release(&mut self, mark: Reg) {
        self.free = mark;
    }

    /// A register holding `e`. A plain local already is one, which is what
    /// keeps the common `a + b` down to a single instruction.
    fn operand(&mut self, e: &Expr) -> Reg {
        match e {
            Expr::Local(b, _) if !b.cell => b.slot,
            other => {
                let r = self.alloc();
                self.expr(other, r);
                r
            }
        }
    }

    /// The constant slot of a literal index — `t[3]` or `t.field`.
    fn const_key(&mut self, e: &Expr) -> Option<u16> {
        match e {
            Expr::Num(n) => Some(self.constant(Value::Num(*n))),
            Expr::Str(s) => Some(self.constant(Value::Str(s.clone()))),
            _ => None,
        }
    }

    /// The constant index of a literal number, when there is one: the right
    /// hand side of most arithmetic is a literal.
    fn literal(&mut self, e: &Expr) -> Option<u16> {
        match e {
            Expr::Num(n) => Some(self.constant(Value::Num(*n))),
            _ => None,
        }
    }

    fn constant(&mut self, v: Value) -> u16 {
        if let Some(i) = self.consts.iter().position(|c| c == &v) {
            return i as u16;
        }
        self.consts.push(v);
        (self.consts.len() - 1) as u16
    }

    fn global(&mut self, name: &Rc<str>) -> u16 {
        if let Some(i) = self.globals.iter().position(|g| &g.name == name) {
            return i as u16;
        }
        self.globals.push(GlobalRef::new(name.clone()));
        (self.globals.len() - 1) as u16
    }

    fn emit(&mut self, op: Op, line: u32) -> usize {
        self.code.push(op);
        self.lines.push(if line > 0 { line } else { self.line });
        self.code.len() - 1
    }

    fn here(&self) -> u32 {
        self.code.len() as u32
    }

    /// Point a previously emitted jump at the current position.
    fn patch(&mut self, at: usize) {
        let target = self.here();
        match &mut self.code[at] {
            Op::Jump { to } | Op::JumpIfFalse { to, .. } | Op::JumpIfTrue { to, .. } => {
                *to = target
            }
            Op::IterNext { exit, .. } => *exit = target,
            other => panic!("cannot patch {other:?}"),
        }
    }

    // ---- statements ---------------------------------------------------------

    /// Compile a block; if `dst` is given, its tail value lands there.
    fn block(&mut self, b: &Block, dst: Option<Reg>) {
        for (i, st) in b.stats.iter().enumerate() {
            self.line = b.lines.get(i).copied().unwrap_or(self.line);
            self.stat(st);
        }
        match (&b.tail, dst) {
            (Some(e), Some(dst)) => {
                if b.tail_line > 0 {
                    self.line = b.tail_line;
                }
                self.expr(e, dst);
            }
            (Some(e), None) => {
                if b.tail_line > 0 {
                    self.line = b.tail_line;
                }
                let mark = self.mark();
                let tmp = self.alloc();
                self.expr(e, tmp);
                self.release(mark);
            }
            (None, Some(dst)) => {
                self.emit(Op::Nil { dst }, 0);
            }
            (None, None) => {}
        }
    }

    fn stat(&mut self, st: &Stat) {
        match st {
            Stat::LetSlots(bindings, exprs) => self.bind(bindings, exprs),
            Stat::FnSlot(binding, f) => {
                // bind first so the closure can capture itself, then fill it in
                let mark = self.mark();
                let tmp = self.alloc();
                self.emit(Op::Nil { dst: tmp }, 0);
                self.store_binding(*binding, tmp);
                self.expr(f, tmp);
                if binding.cell {
                    self.emit(Op::SetCell { slot: binding.slot, src: tmp }, 0);
                } else {
                    self.emit(Op::Move { dst: binding.slot, src: tmp }, 0);
                }
                self.release(mark);
            }
            Stat::Assign(targets, exprs) => {
                let mark = self.mark();
                if targets.len() == 1 && exprs.len() == 1 {
                    self.assign(&targets[0], &exprs[0]);
                } else {
                    let base = self.free;
                    for e in exprs {
                        let r = self.alloc();
                        self.expr(e, r);
                    }
                    for (i, t) in targets.iter().enumerate() {
                        if i < exprs.len() {
                            self.store(t, base + i as Reg);
                        } else {
                            let r = self.alloc();
                            self.emit(Op::Nil { dst: r }, 0);
                            self.store(t, r);
                        }
                    }
                }
                self.release(mark);
            }
            Stat::OpAssign(target, op, e) => {
                let mark = self.mark();
                let kind = bin_kind(*op);
                // `x += 1` on a plain local updates that register in place
                if let Expr::Local(b, _) = target {
                    if !b.cell {
                        match self.literal(e) {
                            Some(k) => {
                                self.emit(Op::BinK { kind, dst: b.slot, a: b.slot, k }, 0);
                            }
                            None => {
                                let rhs = self.operand(e);
                                self.emit(Op::Bin { kind, dst: b.slot, a: b.slot, b: rhs }, 0);
                            }
                        }
                        self.release(mark);
                        return;
                    }
                }
                // `t[k] += v` evaluates `t` and `k` once, as an assignment to
                // the same place would
                if let Expr::Index(obj, key) = target {
                    let o = self.operand(obj);
                    let cur = self.alloc();
                    match self.const_key(key) {
                        Some(k) => {
                            self.emit(Op::GetIndexK { dst: cur, obj: o, k }, 0);
                            let rhs = self.operand(e);
                            self.emit(Op::Bin { kind, dst: cur, a: cur, b: rhs }, 0);
                            self.emit(Op::SetIndexK { obj: o, k, val: cur }, 0);
                        }
                        None => {
                            let k = self.operand(key);
                            self.emit(Op::GetIndex { dst: cur, obj: o, key: k }, 0);
                            let rhs = self.operand(e);
                            self.emit(Op::Bin { kind, dst: cur, a: cur, b: rhs }, 0);
                            self.emit(Op::SetIndex { obj: o, key: k, val: cur }, 0);
                        }
                    }
                    self.release(mark);
                    return;
                }
                let cur = self.alloc();
                self.expr(target, cur);
                let rhs = self.operand(e);
                self.emit(Op::Bin { kind, dst: cur, a: cur, b: rhs }, 0);
                self.store(target, cur);
                self.release(mark);
            }
            Stat::Expr(e) => {
                let mark = self.mark();
                let tmp = self.alloc();
                self.expr_discard(e, tmp);
                self.release(mark);
            }
            Stat::While(id, cond, body) => {
                let top = self.here();
                let mark = self.mark();
                let c = self.alloc();
                self.expr(cond, c);
                let exit = self.emit(Op::JumpIfFalse { cond: c, to: 0 }, 0);
                self.release(mark);
                self.loops.push(LoopCtx { breaks: vec![exit], continues: Vec::new() });
                self.block(body, None);
                let ctx = self.loops.pop().expect("pushed above");
                for at in ctx.continues {
                    self.patch_to(at, top);
                }
                let hint = self.loop_hint(*id);
                self.emit(Op::Jump { to: top }, 0);
                for at in ctx.breaks {
                    self.patch(at);
                }
                self.patch_hint(hint);
            }
            Stat::Loop(id, body) => {
                let top = self.here();
                self.loops.push(LoopCtx { breaks: Vec::new(), continues: Vec::new() });
                self.block(body, None);
                let ctx = self.loops.pop().expect("pushed above");
                for at in ctx.continues {
                    self.patch_to(at, top);
                }
                let hint = self.loop_hint(*id);
                self.emit(Op::Jump { to: top }, 0);
                for at in ctx.breaks {
                    self.patch(at);
                }
                self.patch_hint(hint);
            }
            Stat::ForRange { id, binding, start, end, inclusive, body, .. } => {
                let b = binding.expect("resolved");
                let mark = self.mark();
                // an uncaptured loop variable counts in its own slot: that is
                // what lets the JIT take the loop over mid-flight
                let counter = if b.cell { self.alloc() } else { b.slot };
                let limit = self.alloc();
                let step = self.alloc();
                self.expr(start, counter);
                self.expr(end, limit);
                let one = self.constant(Value::Num(1.0));
                self.emit(Op::Const { dst: step, k: one }, 0);

                let top = self.here();
                let test = self.alloc();
                let kind = if *inclusive { BinKind::Le } else { BinKind::Lt };
                self.emit(Op::Bin { kind, dst: test, a: counter, b: limit }, 0);
                let exit = self.emit(Op::JumpIfFalse { cond: test, to: 0 }, 0);
                self.release(test);
                if b.cell {
                    // a captured loop variable is fresh each turn, so a closure
                    // made in the body captures this iteration's value
                    self.store_binding(b, counter);
                }

                self.loops.push(LoopCtx { breaks: vec![exit], continues: Vec::new() });
                self.block(body, None);
                let ctx = self.loops.pop().expect("pushed above");
                let cont = self.here();
                for at in ctx.continues {
                    self.patch_to(at, cont);
                }
                self.emit(Op::Bin { kind: BinKind::Add, dst: counter, a: counter, b: step }, 0);
                let hint = self.loop_hint(*id);
                self.emit(Op::Jump { to: top }, 0);
                for at in ctx.breaks {
                    self.patch(at);
                }
                self.patch_hint(hint);
                self.release(mark);
            }
            Stat::ForIn { id, bindings, iter, body, .. } => {
                let mark = self.mark();
                let it = self.alloc();
                self.expr(iter, it);
                self.emit(Op::IterInit { dst: it, src: it }, 0);
                // values come out into a contiguous run, then go to their slots
                let vbase = self.free;
                for _ in bindings {
                    self.alloc();
                }
                let top = self.here();
                let next = self.emit(
                    Op::IterNext {
                        iter: it,
                        base: vbase,
                        count: bindings.len() as u16,
                        exit: 0,
                    },
                    0,
                );
                for (i, b) in bindings.iter().enumerate() {
                    self.store_binding(*b, vbase + i as Reg);
                }
                self.loops.push(LoopCtx { breaks: vec![next], continues: Vec::new() });
                self.block(body, None);
                let ctx = self.loops.pop().expect("pushed above");
                let cont = self.here();
                for at in ctx.continues {
                    self.patch_to(at, cont);
                }
                let hint = self.loop_hint(*id);
                self.emit(Op::Jump { to: top }, 0);
                for at in ctx.breaks {
                    self.patch(at);
                }
                self.patch_hint(hint);
                self.release(mark);
            }
            Stat::Return(exprs) => {
                let mark = self.mark();
                match exprs.len() {
                    0 => {
                        let r = self.alloc();
                        self.emit(Op::Ret { base: r, n: 0 }, 0);
                    }
                    1 if is_call(&exprs[0]) => {
                        // `return f()` passes every value f produced
                        let r = self.alloc();
                        self.call_expr(&exprs[0], r, MULTI);
                        self.emit(Op::Ret { base: r, n: MULTI }, 0);
                    }
                    n => {
                        let base = self.free;
                        for e in exprs {
                            let r = self.alloc();
                            self.expr(e, r);
                        }
                        self.emit(Op::Ret { base, n: n as u16 }, 0);
                    }
                }
                self.release(mark);
            }
            Stat::Break => {
                let at = self.emit(Op::Jump { to: 0 }, 0);
                match self.loops.last_mut() {
                    Some(l) => l.breaks.push(at),
                    None => self.code[at] = Op::Nil { dst: 0 }, // caught by the resolver
                }
            }
            Stat::Continue => {
                let at = self.emit(Op::Jump { to: 0 }, 0);
                match self.loops.last_mut() {
                    Some(l) => l.continues.push(at),
                    None => self.code[at] = Op::Nil { dst: 0 },
                }
            }
            Stat::Let(..) | Stat::FnDecl(..) => {
                unreachable!("the resolver rewrites these")
            }
        }
    }

    /// Emit a loop's back-edge hint, with a fresh counter slot.
    fn loop_hint(&mut self, id: u32) -> usize {
        let hint = self.hints as u16;
        self.hints += 1;
        self.emit(Op::LoopHint { id, hint, exit: 0 }, 0)
    }

    /// A loop hint points past the loop, for when the JIT takes it over.
    fn patch_hint(&mut self, at: usize) {
        let target = self.here();
        if let Op::LoopHint { exit, .. } = &mut self.code[at] {
            *exit = target;
        }
    }

    fn patch_to(&mut self, at: usize, target: u32) {
        match &mut self.code[at] {
            Op::Jump { to } | Op::JumpIfFalse { to, .. } | Op::JumpIfTrue { to, .. } => {
                *to = target
            }
            other => panic!("cannot patch {other:?}"),
        }
    }

    /// `let` and `for` bindings: evaluate, then move into the slot (as a fresh
    /// cell when something captures it).
    fn bind(&mut self, bindings: &[Binding], exprs: &[Expr]) {
        let mark = self.mark();
        if bindings.len() == 1 && exprs.len() == 1 {
            let b = bindings[0];
            if b.cell {
                let tmp = self.alloc();
                self.expr(&exprs[0], tmp);
                self.emit(Op::NewCell { slot: b.slot, src: tmp }, 0);
            } else {
                self.expr(&exprs[0], b.slot);
            }
        } else if exprs.len() == 1 && bindings.len() > 1 && is_call(&exprs[0]) {
            // `let (a, b) = f()`
            let base = self.free;
            for _ in bindings {
                self.alloc();
            }
            self.call_expr(&exprs[0], base, bindings.len() as u16);
            for (i, b) in bindings.iter().enumerate() {
                self.store_binding(*b, base + i as Reg);
            }
        } else {
            let base = self.free;
            for e in exprs {
                let r = self.alloc();
                self.expr(e, r);
            }
            for (i, b) in bindings.iter().enumerate() {
                if i < exprs.len() {
                    self.store_binding(*b, base + i as Reg);
                } else {
                    let r = self.alloc();
                    self.emit(Op::Nil { dst: r }, 0);
                    self.store_binding(*b, r);
                }
            }
        }
        self.release(mark);
    }

    fn store_binding(&mut self, b: Binding, src: Reg) {
        if b.cell {
            self.emit(Op::NewCell { slot: b.slot, src }, 0);
        } else if b.slot != src {
            self.emit(Op::Move { dst: b.slot, src }, 0);
        }
    }

    /// Assignment to an already existing place.
    fn store(&mut self, target: &Expr, src: Reg) {
        match target {
            Expr::Local(b, _) => {
                if b.cell {
                    self.emit(Op::SetCell { slot: b.slot, src }, 0);
                } else if b.slot != src {
                    self.emit(Op::Move { dst: b.slot, src }, 0);
                }
            }
            Expr::Upval(i, _) => {
                self.emit(Op::SetUpval { idx: *i, src }, 0);
            }
            Expr::Global(name, _) => {
                let g = self.global(name);
                self.emit(Op::SetGlobal { g, src }, 0);
            }
            Expr::Index(obj, key) => {
                let mark = self.mark();
                let o = self.operand(obj);
                match self.const_key(key) {
                    Some(k) => self.emit(Op::SetIndexK { obj: o, k, val: src }, 0),
                    None => {
                        let k = self.operand(key);
                        self.emit(Op::SetIndex { obj: o, key: k, val: src }, 0)
                    }
                };
                self.release(mark);
            }
            _ => unreachable!("the parser rejects other assignment targets"),
        }
    }

    fn assign(&mut self, target: &Expr, e: &Expr) {
        match target {
            // a plain local can be written in place
            Expr::Local(b, _) if !b.cell => self.expr(e, b.slot),
            _ => {
                let mark = self.mark();
                let tmp = self.alloc();
                self.expr(e, tmp);
                self.store(target, tmp);
                self.release(mark);
            }
        }
    }
}

impl FnCompiler {
    // ---- expressions --------------------------------------------------------

    /// Compile `e` so that its value ends up in `dst`.
    fn expr(&mut self, e: &Expr, dst: Reg) {
        match e {
            Expr::Nil => {
                self.emit(Op::Nil { dst }, 0);
            }
            Expr::Bool(b) => {
                let k = self.constant(Value::Bool(*b));
                self.emit(Op::Const { dst, k }, 0);
            }
            Expr::Num(n) => {
                let k = self.constant(Value::Num(*n));
                self.emit(Op::Const { dst, k }, 0);
            }
            Expr::Str(s) => {
                let k = self.constant(Value::Str(s.clone()));
                self.emit(Op::Const { dst, k }, 0);
            }
            Expr::Local(b, _) => {
                if b.cell {
                    self.emit(Op::GetCell { dst, slot: b.slot }, 0);
                } else if b.slot != dst {
                    self.emit(Op::Move { dst, src: b.slot }, 0);
                }
            }
            Expr::Upval(i, _) => {
                self.emit(Op::GetUpval { dst, idx: *i }, 0);
            }
            Expr::Global(name, _) => {
                let g = self.global(name);
                self.emit(Op::GetGlobal { dst, g }, 0);
            }
            Expr::Index(obj, key) => {
                let mark = self.mark();
                let o = self.operand(obj);
                match self.const_key(key) {
                    Some(k) => self.emit(Op::GetIndexK { dst, obj: o, k }, 0),
                    None => {
                        let k = self.operand(key);
                        self.emit(Op::GetIndex { dst, obj: o, key: k }, 0)
                    }
                };
                self.release(mark);
            }
            Expr::Call(..) | Expr::Method(..) => self.call_expr(e, dst, 1),
            Expr::Func(def) => {
                let proto = compile_function(def);
                self.protos.push(proto);
                let idx = (self.protos.len() - 1) as u16;
                self.emit(Op::Closure { dst, proto: idx }, 0);
            }
            Expr::Array(items) => {
                self.emit(Op::NewTable { dst }, 0);
                let mark = self.mark();
                let n = items.len();
                for (i, item) in items.iter().enumerate() {
                    if i + 1 == n && is_call(item) {
                        // the last element spreads, as in `[a, f()]`
                        let tmp = self.alloc();
                        self.call_expr(item, tmp, MULTI);
                        self.emit(Op::AppendMulti { obj: dst }, 0);
                    } else {
                        let tmp = self.alloc();
                        self.expr(item, tmp);
                        self.emit(Op::Append { obj: dst, val: tmp }, 0);
                    }
                    self.release(mark);
                }
            }
            Expr::Map(items) => {
                self.emit(Op::NewTable { dst }, 0);
                let mark = self.mark();
                for (k, v) in items {
                    let kr = self.alloc();
                    self.expr(k, kr);
                    let vr = self.alloc();
                    self.expr(v, vr);
                    self.emit(Op::SetIndex { obj: dst, key: kr, val: vr }, 0);
                    self.release(mark);
                }
            }
            Expr::Range(a, b, inclusive) => {
                let mark = self.mark();
                let ra = self.alloc();
                self.expr(a, ra);
                let rb = self.alloc();
                self.expr(b, rb);
                self.emit(Op::Range { dst, a: ra, b: rb, inclusive: *inclusive }, 0);
                self.release(mark);
            }
            Expr::Un(UnOp::Neg, a) => {
                self.expr(a, dst);
                self.emit(Op::Neg { dst, a: dst }, 0);
            }
            Expr::Un(UnOp::Not, a) => {
                self.expr(a, dst);
                self.emit(Op::Not { dst, a: dst }, 0);
            }
            Expr::Bin(BinOp::And, a, b) => {
                self.expr(a, dst);
                let skip = self.emit(Op::JumpIfFalse { cond: dst, to: 0 }, 0);
                self.expr(b, dst);
                self.patch(skip);
            }
            Expr::Bin(BinOp::Or, a, b) => {
                self.expr(a, dst);
                let skip = self.emit(Op::JumpIfTrue { cond: dst, to: 0 }, 0);
                self.expr(b, dst);
                self.patch(skip);
            }
            Expr::Bin(op, a, b) => {
                let mark = self.mark();
                let ra = self.operand(a);
                match self.literal(b) {
                    Some(k) => {
                        self.emit(Op::BinK { kind: bin_kind(*op), dst, a: ra, k }, 0);
                    }
                    None => {
                        let rb = self.operand(b);
                        self.emit(Op::Bin { kind: bin_kind(*op), dst, a: ra, b: rb }, 0);
                    }
                }
                self.release(mark);
            }
            Expr::Do(b) => self.block(b, Some(dst)),
            Expr::If(arms, els) => self.if_expr(arms, els.as_ref(), Some(dst)),
            Expr::Match(subject, arms) => self.match_expr(subject, arms, Some(dst)),
            Expr::Var(name) => unreachable!("unresolved name `{name}` reached the compiler"),
        }
    }

    /// Compile for effect: a statement level `if` needs no value, and asking
    /// for one would force both branches to produce something.
    fn expr_discard(&mut self, e: &Expr, scratch: Reg) {
        match e {
            Expr::If(arms, els) => self.if_expr(arms, els.as_ref(), None),
            Expr::Match(subject, arms) => self.match_expr(subject, arms, None),
            Expr::Do(b) => self.block(b, None),
            Expr::Call(..) | Expr::Method(..) => self.call_expr(e, scratch, 0),
            other => self.expr(other, scratch),
        }
    }

    fn if_expr(&mut self, arms: &[(Expr, Block)], els: Option<&Block>, dst: Option<Reg>) {
        let mut ends = Vec::new();
        for (i, (cond, body)) in arms.iter().enumerate() {
            let mark = self.mark();
            let c = self.alloc();
            self.expr(cond, c);
            let next = self.emit(Op::JumpIfFalse { cond: c, to: 0 }, 0);
            self.release(mark);
            self.block(body, dst);
            let is_last = i + 1 == arms.len() && els.is_none();
            if !is_last {
                ends.push(self.emit(Op::Jump { to: 0 }, 0));
            }
            self.patch(next);
        }
        match els {
            Some(b) => self.block(b, dst),
            // an `if` with no `else` is nil when the condition fails
            None => {
                if let Some(dst) = dst {
                    self.emit(Op::Nil { dst }, 0);
                }
            }
        }
        for at in ends {
            self.patch(at);
        }
    }

    fn match_expr(&mut self, subject: &Expr, arms: &[Arm], dst: Option<Reg>) {
        let mark = self.mark();
        let subj = self.alloc();
        self.expr(subject, subj);
        let mut ends = Vec::new();
        for arm in arms {
            // any pattern matching sends us to the body; the last failure falls
            // through to the next arm
            let mut hits = Vec::new();
            let mut misses = Vec::new();
            let mut always = false;
            for p in &arm.patterns {
                match p {
                    Pattern::Wild => always = true,
                    Pattern::Bind(_, Some(b)) => {
                        self.store_binding(*b, subj);
                        always = true;
                    }
                    Pattern::Bind(name, None) => {
                        unreachable!("unresolved pattern `{name}`")
                    }
                    Pattern::Lit(e) => {
                        let m = self.mark();
                        let lit = self.alloc();
                        self.expr(e, lit);
                        let test = self.alloc();
                        self.emit(Op::Bin { kind: BinKind::Eq, dst: test, a: subj, b: lit }, 0);
                        hits.push(self.emit(Op::JumpIfTrue { cond: test, to: 0 }, 0));
                        self.release(m);
                    }
                }
                if always {
                    break;
                }
            }
            if !always {
                // none of the literals matched: on to the next arm
                misses.push(self.emit(Op::Jump { to: 0 }, 0));
            }
            for at in hits {
                self.patch(at);
            }
            if let Some(guard) = &arm.guard {
                let m = self.mark();
                let g = self.alloc();
                self.expr(guard, g);
                misses.push(self.emit(Op::JumpIfFalse { cond: g, to: 0 }, 0));
                self.release(m);
            }
            self.block(&arm.body, dst);
            ends.push(self.emit(Op::Jump { to: 0 }, 0));
            for at in misses {
                self.patch(at);
            }
        }
        // nothing matched
        if let Some(dst) = dst {
            self.emit(Op::Nil { dst }, 0);
        }
        for at in ends {
            self.patch(at);
        }
        self.release(mark);
    }

    /// Calls put the callee at `base` and its arguments right after it, so the
    /// VM can hand the callee a contiguous window.
    fn call_expr(&mut self, e: &Expr, dst: Reg, nres: u16) {
        let mark = self.mark();
        // A call writes its results starting at the frame base, so when the
        // destination is scratch space we put the frame there and the results
        // land where they are wanted. Register allocation is a stack, so
        // anything still live sits below `dst` and is safe.
        let base = if dst >= self.def_locals() {
            self.free = dst;
            self.alloc()
        } else {
            debug_assert!(nres <= 1, "several results must go to scratch registers");
            self.alloc()
        };
        match e {
            Expr::Call(f, args) => {
                self.expr(f, base);
                let spread = self.args(args);
                let nargs = args.len() as u16;
                if spread {
                    self.emit(Op::CallSpread { base, nargs: nargs - 1, nres, method: u16::MAX }, 0);
                } else {
                    self.emit(Op::Call { base, nargs, nres }, 0);
                }
            }
            Expr::Method(obj, name, args) => {
                let recv = self.alloc();
                self.expr(obj, recv);
                let spread = self.args(args);
                let k = self.constant(Value::str(&**name));
                let nargs = args.len() as u16;
                if spread {
                    self.emit(
                        Op::CallSpread { base, nargs: nargs - 1 + 1, nres, method: k },
                        0,
                    );
                } else {
                    self.emit(Op::Method { base, name: k, nargs, nres }, 0);
                }
            }
            _ => unreachable!("not a call"),
        }
        if nres > 0 && base != dst {
            self.emit(Op::Move { dst, src: base }, 0);
        }
        // keep the result registers reserved: they are what the caller reads
        let used = match nres {
            MULTI | 0 => 1,
            n => n,
        };
        self.release(mark.max(base + used));
    }

    /// Compile a block that is a function body: its tail is what the function
    /// returns, and a call in tail position passes on *every* value it made.
    fn block_ret(&mut self, b: &Block) {
        for (i, st) in b.stats.iter().enumerate() {
            self.line = b.lines.get(i).copied().unwrap_or(self.line);
            self.stat(st);
        }
        if b.tail_line > 0 {
            self.line = b.tail_line;
        }
        match b.tail.as_deref() {
            None => {
                let r = self.alloc();
                self.emit(Op::Ret { base: r, n: 0 }, 0);
            }
            Some(e) if is_call(e) => {
                let mark = self.mark();
                let r = self.alloc();
                self.call_expr(e, r, MULTI);
                self.emit(Op::Ret { base: r, n: MULTI }, 0);
                self.release(mark);
            }
            Some(Expr::Do(inner)) => self.block_ret(inner),
            Some(Expr::If(arms, els)) => self.if_ret(arms, els.as_ref()),
            Some(e) => {
                let mark = self.mark();
                let r = self.alloc();
                self.expr(e, r);
                self.emit(Op::Ret { base: r, n: 1 }, 0);
                self.release(mark);
            }
        }
    }

    /// An `if` in tail position: every branch returns for itself, so a call in
    /// any of them keeps its extra values.
    fn if_ret(&mut self, arms: &[(Expr, Block)], els: Option<&Block>) {
        for (cond, body) in arms {
            let mark = self.mark();
            let c = self.alloc();
            self.expr(cond, c);
            let next = self.emit(Op::JumpIfFalse { cond: c, to: 0 }, 0);
            self.release(mark);
            self.block_ret(body);
            self.patch(next);
        }
        match els {
            Some(b) => self.block_ret(b),
            None => {
                let r = self.alloc();
                self.emit(Op::Ret { base: r, n: 0 }, 0);
            }
        }
    }

    /// Compile a call's arguments. A call in last position spreads: every
    /// value it produced becomes an argument, as in `print(f())`.
    fn args(&mut self, args: &[Expr]) -> bool {
        let n = args.len();
        let mut spread = false;
        for (i, a) in args.iter().enumerate() {
            let r = self.alloc();
            if i + 1 == n && is_call(a) {
                self.call_expr(a, r, MULTI);
                spread = true;
            } else {
                self.expr(a, r);
            }
        }
        spread
    }

    fn def_locals(&self) -> Reg {
        self.def.n_slots as Reg
    }
}

/// Compile a nested function into its own proto.
fn compile_function(def: &Rc<FuncDef>) -> Rc<Proto> {
    let mut f = FnCompiler::new(def.clone(), def.n_slots);
    f.block_ret(&def.body);
    Rc::new(f.finish())
}

fn bin_kind(op: BinOp) -> BinKind {
    match op {
        BinOp::Add => BinKind::Add,
        BinOp::Sub => BinKind::Sub,
        BinOp::Mul => BinKind::Mul,
        BinOp::Div => BinKind::Div,
        BinOp::Rem => BinKind::Rem,
        BinOp::Eq => BinKind::Eq,
        BinOp::Ne => BinKind::Ne,
        BinOp::Lt => BinKind::Lt,
        BinOp::Le => BinKind::Le,
        BinOp::Gt => BinKind::Gt,
        BinOp::Ge => BinKind::Ge,
        BinOp::And | BinOp::Or => unreachable!("short circuited in the compiler"),
    }
}

fn is_call(e: &Expr) -> bool {
    matches!(e, Expr::Call(..) | Expr::Method(..))
}
