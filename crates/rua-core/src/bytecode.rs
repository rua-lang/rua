//! The instruction set the VM runs.
//!
//! Registers are a window into one shared value stack. The resolver has already
//! given every local a slot, so slots `0..n_slots` are exactly the locals and
//! everything above them is scratch space the compiler hands out for
//! subexpressions.

use crate::value::Value;
use rua_syntax::ast::FuncDef;
use std::cell::Cell;
use std::rc::Rc;

/// A register index within the current frame.
pub type Reg = u16;

/// "As many values as there are", for calls and returns that pass along
/// whatever a callee produced.
pub const MULTI: u16 = u16::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinKind {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy)]
pub enum Op {
    /// `dst = consts[k]`
    Const { dst: Reg, k: u16 },
    Nil { dst: Reg },
    Move { dst: Reg, src: Reg },

    /// `dst = globals[g]`, where `g` indexes this chunk's global name table.
    GetGlobal { dst: Reg, g: u16 },
    SetGlobal { g: u16, src: Reg },
    GetUpval { dst: Reg, idx: u16 },
    SetUpval { idx: u16, src: Reg },
    /// Read through a captured local's cell.
    GetCell { dst: Reg, slot: Reg },
    SetCell { slot: Reg, src: Reg },
    /// Put a *fresh* cell in a slot: a closure made in a loop captures that
    /// iteration's variable, not the one next door.
    NewCell { slot: Reg, src: Reg },

    Bin { kind: BinKind, dst: Reg, a: Reg, b: Reg },
    /// The same, with a constant on the right: `i + 1`, `x < n`, `x % 5`.
    BinK { kind: BinKind, dst: Reg, a: Reg, k: u16 },

    // The four arithmetic operations that dominate every profile, given their
    // own opcodes so the VM branches once on the instruction rather than a
    // second time on the operation. A rewrite pass produces these from `Bin`
    // and `BinK`; everything else keeps the generic form, because specialising
    // the whole table measured *worse* — the jump table stops fitting.
    Add { dst: Reg, a: Reg, b: Reg },
    Sub { dst: Reg, a: Reg, b: Reg },
    Mul { dst: Reg, a: Reg, b: Reg },
    Div { dst: Reg, a: Reg, b: Reg },
    AddK { dst: Reg, a: Reg, k: u16 },
    SubK { dst: Reg, a: Reg, k: u16 },
    MulK { dst: Reg, a: Reg, k: u16 },
    Neg { dst: Reg, a: Reg },
    Not { dst: Reg, a: Reg },

    Jump { to: u32 },
    JumpIfFalse { cond: Reg, to: u32 },
    JumpIfTrue { cond: Reg, to: u32 },
    /// Compare and branch in one: jump when `a <kind> b` is false. Loop
    /// conditions are the whole reason this exists.
    JumpIfNot { kind: BinKind, a: Reg, b: Reg, to: u32 },
    /// The same against a constant: `x == 0`, `t[i] == nil`. Comparing with a
    /// literal is most of what conditions do.
    JumpIfNotK { kind: BinKind, a: Reg, k: u16, to: u32 },
    /// A loop's back edge: count the iteration for the JIT and jump. `exit` is
    /// where to continue if the JIT takes the loop over.
    JumpBack { to: u32, id: u32, hint: u16, exit: u32 },
    /// A counted loop's back edge: `i += 1`, test against the limit, and jump
    /// back into the body if it still holds — the three instructions a `for i
    /// in a..b` used to spend per iteration, which is twice what Lua spends.
    /// Falling through is the loop's exit, so it needs no exit field.
    ForLoop { counter: Reg, limit: Reg, to: u32, id: u32, hint: u16, le: bool },

    /// Callee at `base`, arguments at `base+1..base+1+nargs`. Results land at
    /// `base`; `nres` of them, or all of them when it is [`MULTI`].
    Call { base: Reg, nargs: u16, nres: u16 },
    /// `obj.m(..)`: the receiver is at `base+1` and becomes the first argument.
    Method { base: Reg, name: u16, nargs: u16, nres: u16 },
    /// A call whose last argument was itself a call: the fixed arguments are
    /// followed by every value that call produced, as in `print(f())`.
    CallSpread { base: Reg, nargs: u16, nres: u16, method: u16 },
    /// Return `n` values starting at `base`, or the pending multi-value set.
    Ret { base: Reg, n: u16 },

    NewTable { dst: Reg },
    /// `dst = obj[key]`
    GetIndex { dst: Reg, obj: Reg, key: Reg },
    /// `dst = obj[k]` for a constant index: `t[3]`, `node.left`. Most indexing
    /// in real code is this shape, and it saves loading the key into a
    /// register first.
    GetIndexK { dst: Reg, obj: Reg, k: u16, ic: u16 },
    /// `obj[key] = val`
    SetIndex { obj: Reg, key: Reg, val: Reg },
    /// `obj[k] = val` for a constant index.
    SetIndexK { obj: Reg, k: u16, val: Reg },
    /// `t.push(v)`, for array literals.
    Append { obj: Reg, val: Reg },
    /// Append everything the last call produced, for `[a, f()]`.
    AppendMulti { obj: Reg },

    Closure { dst: Reg, proto: u16 },
    Range { dst: Reg, a: Reg, b: Reg, inclusive: bool },

    /// Turn a value into something `for` can pull from.
    IterInit { dst: Reg, src: Reg },
    /// Pull the next values into `base..base+count`, jumping to `exit` when the
    /// iterator is done.
    IterNext { iter: Reg, base: Reg, count: u16, exit: u32 },

    /// The back edge of a loop: the loop's id, a counter slot, and where to
    /// continue if the JIT takes the loop over.
    LoopHint { id: u32, hint: u16, exit: u32 },
}

/// One compiled function: its code, and everything the code refers to.
pub struct Proto {
    pub def: Rc<FuncDef>,
    /// The function's name, ready to push onto a traceback without allocating.
    pub name: Rc<str>,
    pub code: Vec<Op>,
    /// One line number per instruction, for error messages.
    pub lines: Vec<u32>,
    pub consts: Vec<Value>,
    pub protos: Vec<Rc<Proto>>,
    /// Global names this chunk touches, each with the slot it resolved to.
    pub globals: Vec<GlobalRef>,
    pub n_regs: usize,
    /// One iteration counter per loop, so counting costs a `Cell` bump rather
    /// than a hash lookup.
    pub hints: Vec<Cell<u32>>,
    /// One slot per constant field read, remembering where that field was
    /// found last time. Objects built the same way have their fields in the
    /// same order, so the guess is nearly always right and the scan is skipped.
    pub caches: Vec<Cell<u32>>,
    /// Parameters, in order, with the register each lands in.
    pub params: Vec<ParamSlot>,
}

#[derive(Debug)]
pub struct GlobalRef {
    pub name: Rc<str>,
    /// Filled in the first time it is touched; global slots never move.
    pub slot: Cell<u32>,
}

impl GlobalRef {
    pub fn new(name: Rc<str>) -> GlobalRef {
        GlobalRef { name, slot: Cell::new(u32::MAX) }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ParamSlot {
    pub reg: Reg,
    pub cell: bool,
}

impl std::fmt::Debug for Proto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "proto {} ({} ops)", self.def.name, self.code.len())
    }
}

#[cfg(test)]
mod size {
    #[test]
    fn op_is_sixteen_bytes() {
        // Every instruction is fetched through this, so a variant that pushes
        // the enum wider makes the whole interpreter slower.
        assert_eq!(std::mem::size_of::<super::Op>(), 16);
    }
}
