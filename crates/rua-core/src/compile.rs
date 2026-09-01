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
    /// How many inline caches this function needs, one per constant field read.
    caches: usize,
    free: Reg,
    max_regs: Reg,
    loops: Vec<LoopCtx>,
    line: u32,
}

/// The register a plain local already lives in. A captured one does not
/// count: it is read through its cell, which takes an instruction.
fn plain_reg(e: &Expr) -> Option<Reg> {
    match e {
        Expr::Local(b, _) if !b.cell => Some(b.slot),
        _ => None,
    }
}

/// The pieces of a string interpolation, if this is one: a left leaning chain
/// of `+` rooted in a string literal, so every step of it concatenates.
///
/// Only the left spine is flattened. In `"a" + (b + c)` the right hand side is
/// its own expression and may well be arithmetic, which it stays.
fn concat_parts(e: &Expr) -> Option<Vec<&Expr>> {
    let mut parts = Vec::new();
    let mut cur = e;
    while let Expr::Bin(BinOp::Add, a, b) = cur {
        parts.push(&**b);
        cur = a;
    }
    if !matches!(cur, Expr::Str(_)) {
        return None;
    }
    parts.push(cur);
    parts.reverse();
    // two parts is one allocation either way, and `n` is a byte
    (3..=255).contains(&parts.len()).then_some(parts)
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
            caches: 0,
            free: n_slots as Reg,
            max_regs: n_slots as Reg,
            loops: Vec::new(),
            line: 0,
        }
    }

    /// Turn the generic arithmetic instructions into their specialised forms.
    ///
    /// Done here rather than at the emit sites so that the compiler stays
    /// written in terms of one `Bin`, and the specialisation is a single table
    /// to read and change.
    fn specialise(code: &mut [Op]) {
        for op in code.iter_mut() {
            *op = match *op {
                Op::Bin { kind: BinKind::Add, dst, a, b } => Op::Add { dst, a, b },
                Op::Bin { kind: BinKind::Sub, dst, a, b } => Op::Sub { dst, a, b },
                Op::Bin { kind: BinKind::Mul, dst, a, b } => Op::Mul { dst, a, b },
                Op::Bin { kind: BinKind::Div, dst, a, b } => Op::Div { dst, a, b },
                Op::BinK { kind: BinKind::Add, dst, a, k } => Op::AddK { dst, a, k },
                Op::BinK { kind: BinKind::Sub, dst, a, k } => Op::SubK { dst, a, k },
                Op::BinK { kind: BinKind::Mul, dst, a, k } => Op::MulK { dst, a, k },
                other => other,
            };
        }
    }

    fn finish(mut self) -> Proto {
        Self::specialise(&mut self.code);
        let params: Vec<ParamSlot> = self
            .def
            .param_bindings
            .iter()
            .map(|b| ParamSlot { reg: b.slot, cell: b.cell })
            .collect();
        let plain_params = params.iter().all(|p| !p.cell);
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
            caches: (0..self.caches).map(|_| std::cell::Cell::new(0)).collect(),
            params,
            plain_params,
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
            Expr::Str(s) => Some(self.constant(Value::str(&**s))),
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
            Op::Jump { to }
            | Op::JumpIfFalse { to, .. }
            | Op::JumpIfTrue { to, .. }
            | Op::JumpIfNot { to, .. }
            | Op::JumpIfNotK { to, .. }
            | Op::JumpBack { to, .. } => *to = target,
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
                            let ic = self.next_cache();
                            self.emit(Op::GetIndexK { dst: cur, obj: o, k, ic }, 0);
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
                let mut exits = Vec::new();
                // `while true` is how a trampoline is written, and testing a
                // constant every time round cost two instructions an iteration
                if !matches!(cond, Expr::Bool(true)) {
                    self.cond_jump(cond, true, &mut exits);
                }
                self.loops.push(LoopCtx { breaks: exits, continues: Vec::new() });
                self.block(body, None);
                let ctx = self.loops.pop().expect("pushed above");
                for at in ctx.continues {
                    self.patch_to(at, top);
                }
                let hint = self.next_hint();
                let back = self.emit(Op::JumpBack { to: top, id: *id, hint, exit: 0 }, 0);
                for at in ctx.breaks {
                    self.patch(at);
                }
                self.patch_hint(back);
            }
            Stat::Loop(id, body) => {
                let top = self.here();
                self.loops.push(LoopCtx { breaks: Vec::new(), continues: Vec::new() });
                self.block(body, None);
                let ctx = self.loops.pop().expect("pushed above");
                for at in ctx.continues {
                    self.patch_to(at, top);
                }
                let hint = self.next_hint();
                let back = self.emit(Op::JumpBack { to: top, id: *id, hint, exit: 0 }, 0);
                for at in ctx.breaks {
                    self.patch(at);
                }
                self.patch_hint(back);
            }
            Stat::ForRange { id, binding, start, end, inclusive, body, .. } => {
                let b = binding.expect("resolved");
                let mark = self.mark();
                // an uncaptured loop variable counts in its own slot: that is
                // what lets the JIT take the loop over mid-flight
                let counter = if b.cell { self.alloc() } else { b.slot };
                let limit = self.alloc();
                self.expr(start, counter);
                self.expr(end, limit);

                // The entry test is its own instruction; every iteration after
                // the first is tested by the back edge, which does the step and
                // the jump in the same breath.
                let kind = if *inclusive { BinKind::Le } else { BinKind::Lt };
                let exit = self.emit(Op::JumpIfNot { kind, a: counter, b: limit, to: 0 }, 0);
                let top = self.here();
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
                let hint = self.next_hint();
                self.emit(
                    Op::ForLoop { counter, limit, to: top, id: *id, hint, le: *inclusive },
                    0,
                );
                for at in ctx.breaks {
                    self.patch(at);
                }
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
                let hint = self.next_hint();
                let back = self.emit(Op::JumpBack { to: top, id: *id, hint, exit: 0 }, 0);
                for at in ctx.breaks {
                    self.patch(at);
                }
                self.patch_hint(back);
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
                    1 => {
                        // as in `block_ret`: `return x` for a plain local is
                        // the register it already lives in
                        if let Expr::Local(b, _) = &exprs[0] {
                            if !b.cell {
                                self.emit(Op::Ret { base: b.slot, n: 1 }, 0);
                                self.release(mark);
                                return;
                            }
                        }
                        let r = self.alloc();
                        self.expr(&exprs[0], r);
                        self.emit(Op::Ret { base: r, n: 1 }, 0);
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

    /// A fresh iteration counter slot for a loop.
    /// A fresh inline cache slot for one constant field read.
    fn next_cache(&mut self) -> u16 {
        let at = self.caches as u16;
        self.caches += 1;
        at
    }

    fn next_hint(&mut self) -> u16 {
        let hint = self.hints as u16;
        self.hints += 1;
        hint
    }

    /// A back edge points past the loop, for when the JIT takes it over.
    fn patch_hint(&mut self, at: usize) {
        let target = self.here();
        if let Op::JumpBack { exit, .. } = &mut self.code[at] {
            *exit = target;
        }
    }

    /// Compile `cond` as a branch taken when it is false, fusing the compare
    /// into the jump where it is one.
    /// Compile a condition straight into branches.
    ///
    /// `if a < b && c == 0 { .. }` should be two compare-and-jumps, not two
    /// booleans built in registers and then tested. Recursing through `&&`,
    /// `||` and `!` the way Lua's `luaK_goiffalse` does keeps every operand out
    /// of a register. The returned sites all jump when the condition fails
    /// (`when_false`) or when it holds; the caller patches them.
    fn cond_jump(&mut self, cond: &Expr, when_false: bool, out: &mut Vec<usize>) {
        match cond {
            Expr::Bin(BinOp::And, a, b) => {
                if when_false {
                    // either half failing fails the whole thing
                    self.cond_jump(a, true, out);
                    self.cond_jump(b, true, out);
                } else {
                    let mut skip = Vec::new();
                    self.cond_jump(a, true, &mut skip);
                    self.cond_jump(b, false, out);
                    for at in skip {
                        self.patch(at);
                    }
                }
            }
            Expr::Bin(BinOp::Or, a, b) => {
                if when_false {
                    let mut holds = Vec::new();
                    self.cond_jump(a, false, &mut holds);
                    self.cond_jump(b, true, out);
                    for at in holds {
                        self.patch(at);
                    }
                } else {
                    self.cond_jump(a, false, out);
                    self.cond_jump(b, false, out);
                }
            }
            Expr::Un(UnOp::Not, a) => self.cond_jump(a, !when_false, out),
            _ => {
                let at = self.cond_leaf(cond, when_false);
                out.push(at);
            }
        }
    }

    /// One comparison, or one value tested for truth.
    fn cond_leaf(&mut self, cond: &Expr, when_false: bool) -> usize {
        if let Expr::Bin(op, a, b) = cond {
            // jumping when the comparison is *true* is the same as jumping
            // when its opposite is false, so one instruction covers both
            let kind = if when_false { compare_kind(*op) } else { compare_kind(*op).map(invert) };
            if let Some(kind) = kind {
                let mark = self.mark();
                let ra = self.operand(a);
                let at = match self.const_operand(b) {
                    Some(k) => self.emit(Op::JumpIfNotK { kind, a: ra, k, to: 0 }, 0),
                    None => {
                        let rb = self.operand(b);
                        self.emit(Op::JumpIfNot { kind, a: ra, b: rb, to: 0 }, 0)
                    }
                };
                self.release(mark);
                return at;
            }
        }
        let mark = self.mark();
        let c = self.alloc();
        self.expr(cond, c);
        self.release(mark);
        if when_false {
            self.emit(Op::JumpIfFalse { cond: c, to: 0 }, 0)
        } else {
            self.emit(Op::JumpIfTrue { cond: c, to: 0 }, 0)
        }
    }

    /// A literal that a comparison can take directly.
    fn const_operand(&mut self, e: &Expr) -> Option<u16> {
        let v = match e {
            Expr::Num(n) => Value::Num(*n),
            Expr::Str(s) => Value::str(&**s),
            Expr::Bool(b) => Value::Bool(*b),
            Expr::Nil => Value::Nil,
            _ => return None,
        };
        Some(self.constant(v))
    }

    fn patch_to(&mut self, at: usize, target: u32) {
        match &mut self.code[at] {
            Op::Jump { to }
            | Op::JumpIfFalse { to, .. }
            | Op::JumpIfTrue { to, .. }
            | Op::JumpIfNot { to, .. }
            | Op::JumpIfNotK { to, .. } => *to = target,
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
                let k = self.constant(Value::str(&**s));
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
                    Some(k) => {
                        let ic = self.next_cache();
                        self.emit(Op::GetIndexK { dst, obj: o, k, ic }, 0)
                    }
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
                    // `#{ a: 1 }` has a literal key, which the store takes
                    // directly rather than through a register
                    match self.const_key(k) {
                        Some(key) => {
                            let vr = self.alloc();
                            self.expr(v, vr);
                            self.emit(Op::SetIndexK { obj: dst, k: key, val: vr }, 0);
                        }
                        None => {
                            let kr = self.alloc();
                            self.expr(k, kr);
                            let vr = self.alloc();
                            self.expr(v, vr);
                            self.emit(Op::SetIndex { obj: dst, key: kr, val: vr }, 0);
                        }
                    }
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
            Expr::Bin(BinOp::Add, ..) if concat_parts(e).is_some() => {
                let parts = concat_parts(e).unwrap();
                let mark = self.mark();
                let base = self.alloc();
                self.expr(parts[0], base);
                for p in &parts[1..] {
                    let r = self.alloc();
                    self.expr(p, r);
                }
                self.emit(Op::Concat { dst, base, n: parts.len() as u8 }, 0);
                self.release(mark);
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
            let mut next = Vec::new();
            self.cond_jump(cond, true, &mut next);
            self.block(body, dst);
            let is_last = i + 1 == arms.len() && els.is_none();
            if !is_last {
                ends.push(self.emit(Op::Jump { to: 0 }, 0));
            }
            for at in next {
                self.patch(at);
            }
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
                self.cond_jump(guard, true, &mut misses);
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
        // A single result on its way to a named local goes there directly;
        // everything else lands at the frame base and is moved afterwards.
        // A spread has no room in its instruction to say so, so it does not.
        let direct = if nres == 1 && base != dst { dst } else { base };
        let mut out = base;
        match e {
            Expr::Call(f, args) => {
                // A call to a global needs no register for its callee: the
                // call reads the global itself. A spread still does, since it
                // goes the long way round.
                let global = match &**f {
                    Expr::Global(name, _) => Some(self.global(name)),
                    _ => None,
                };
                match global {
                    // one argument, already in a register: the call takes it
                    // from there rather than being handed it by a move
                    Some(g) if args.len() == 1 && plain_reg(&args[0]).is_some() => {
                        let a = plain_reg(&args[0]).expect("just checked");
                        self.alloc(); // the window still reserves the slot
                        out = direct;
                        self.emit(Op::CallGlobal1 { base, g, a, nres, dst: direct }, 0);
                    }
                    Some(g) => {
                        let spread = self.args(args);
                        let nargs = args.len() as u16;
                        if spread {
                            self.emit(Op::GetGlobal { dst: base, g }, 0);
                            self.emit(
                                Op::CallSpread { base, nargs: nargs - 1, nres, method: u16::MAX },
                                0,
                            );
                        } else {
                            out = direct;
                            self.emit(
                                Op::CallGlobal { base, g, nargs, nres, dst: direct },
                                0,
                            );
                        }
                    }
                    None => {
                        self.expr(f, base);
                        let spread = self.args(args);
                        let nargs = args.len() as u16;
                        if spread {
                            self.emit(
                                Op::CallSpread { base, nargs: nargs - 1, nres, method: u16::MAX },
                                0,
                            );
                        } else {
                            out = direct;
                            self.emit(Op::Call { base, nargs, nres, dst: direct }, 0);
                        }
                    }
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
                    out = direct;
                    self.emit(Op::Method { base, name: k, nargs, nres, dst: direct }, 0);
                }
            }
            _ => unreachable!("not a call"),
        }
        if nres > 0 && base != dst && out != dst {
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
            // A function whose tail is a plain local returns that register:
            // copying it into a scratch one first is an instruction and a
            // reference count for nothing.
            Some(Expr::Local(b, _)) if !b.cell => {
                self.emit(Op::Ret { base: b.slot, n: 1 }, 0);
            }
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
            let mut next = Vec::new();
            self.cond_jump(cond, true, &mut next);
            self.block_ret(body);
            for at in next {
                self.patch(at);
            }
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
        let mut i = 0;
        while i < n {
            let a = &args[i];
            // two locals in a row are one instruction, which is most of what
            // gathering arguments is
            if i + 2 <= n {
                if let (Some(s0), Some(s1)) = (plain_reg(a), plain_reg(&args[i + 1])) {
                    let d0 = self.alloc();
                    let d1 = self.alloc();
                    self.emit(Op::Move2 { d0, s0, d1, s1 }, 0);
                    i += 2;
                    continue;
                }
            }
            let r = self.alloc();
            if i + 1 == n && is_call(a) {
                self.call_expr(a, r, MULTI);
                spread = true;
            } else {
                self.expr(a, r);
            }
            i += 1;
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

/// The comparison this operator is, if it is one.
fn compare_kind(op: BinOp) -> Option<BinKind> {
    match op {
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne => Some(bin_kind(op)),
        _ => None,
    }
}

/// The comparison that is true exactly when this one is false.
fn invert(kind: BinKind) -> BinKind {
    match kind {
        BinKind::Lt => BinKind::Ge,
        BinKind::Ge => BinKind::Lt,
        BinKind::Le => BinKind::Gt,
        BinKind::Gt => BinKind::Le,
        BinKind::Eq => BinKind::Ne,
        BinKind::Ne => BinKind::Eq,
        other => other,
    }
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
