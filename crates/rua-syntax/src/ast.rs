//! AST. Rust-shaped: blocks carry a tail expression, `if` is an expression.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The cached slot of a global. `NONE` until the runtime fills it in.
#[derive(Debug, Default)]
pub struct GlobalCache(pub Cell<u32>);

pub const NO_SLOT: u32 = u32::MAX;

impl GlobalCache {
    pub fn new() -> GlobalCache {
        GlobalCache(Cell::new(NO_SLOT))
    }

    pub fn get(&self) -> Option<u32> {
        match self.0.get() {
            NO_SLOT => None,
            i => Some(i),
        }
    }

    pub fn set(&self, i: u32) {
        self.0.set(i);
    }
}

// A clone starts cold: the copy has to look its global up once.
impl Clone for GlobalCache {
    fn clone(&self) -> Self {
        GlobalCache::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add, Sub, Mul, Div, Rem,
    Eq, Ne, Lt, Le, Gt, Ge,
    And, Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp { Neg, Not }

/// Where something is in the source, as byte offsets into it.
///
/// Lines are enough to say where an error happened when the reader is a
/// person looking at a terminal. An editor wants the token: to underline it,
/// to say what is under the cursor, to rename it. Both are kept, since one
/// does not follow from the other without the source text in hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub lo: u32,
    pub hi: u32,
}

impl Span {
    pub fn new(lo: u32, hi: u32) -> Span {
        Span { lo, hi }
    }

    /// From the start of this one to the end of that one.
    pub fn to(self, end: Span) -> Span {
        Span { lo: self.lo, hi: end.hi.max(self.hi) }
    }

    pub fn contains(self, at: u32) -> bool {
        at >= self.lo && at < self.hi
    }

    pub fn len(self) -> u32 {
        self.hi.saturating_sub(self.lo)
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
}

/// A name as it was written, and where.
///
/// Names are what an editor asks about — where is this declared, what else
/// refers to it, rename all of them — and none of those questions can be
/// answered without knowing which bytes each one occupies. It derefs to the
/// text, so code that only cares what the name is reads as it did before.
#[derive(Debug, Clone)]
pub struct Name {
    pub text: Rc<str>,
    pub span: Span,
}

impl Name {
    pub fn new(text: impl Into<Rc<str>>, span: Span) -> Name {
        Name { text: text.into(), span }
    }
}

impl std::ops::Deref for Name {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        &*self.text == other
    }
}

/// `{ stmts...; tail }` — the tail expression is the block's value.
///
/// `lines` runs parallel to `stats`, so a runtime error can say where it
/// happened without every node carrying a span.
#[derive(Debug, Clone, Default)]
pub struct Block {
    pub stats: Vec<Stat>,
    pub lines: Vec<u32>,
    pub tail: Option<Box<Expr>>,
    pub tail_line: u32,
}

impl Block {
    pub fn is_empty(&self) -> bool {
        self.stats.is_empty() && self.tail.is_none()
    }
}

/// Where a closure's upvalue comes from when the closure is created.
#[derive(Debug, Clone, Copy)]
pub enum UpvalSrc {
    /// A cell sitting in the enclosing frame.
    ParentLocal(u16),
    /// An upvalue the enclosing closure already holds.
    ParentUpval(u16),
}

/// A local binding: its frame slot, and whether it lives in a cell because
/// some nested closure captures it.
#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub slot: u16,
    pub cell: bool,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Nil,
    Bool(bool),
    Num(f64),
    Str(Rc<str>),
    /// Before resolution. `resolve` rewrites these into the three below.
    Var(Name),
    /// A local in the current frame. The name rides along for the JIT.
    Local(Binding, Rc<str>),
    /// An upvalue captured from an enclosing function.
    Upval(u16, Rc<str>),
    /// A global, plus a one entry inline cache: globals get a stable index the
    /// first time they are touched, so later accesses are an array read.
    Global(Rc<str>, GlobalCache),
    /// `a[k]`, `a.k` and `a::k` all land here.
    Index(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    /// `obj.m(a)` — passes `obj` as the first argument, like Rust's `self`.
    Method(Box<Expr>, Rc<str>, Vec<Expr>),
    /// `fn` items and `|a, b| ...` closures.
    Func(Rc<FuncDef>),
    /// `[1, 2, 3]`
    Array(Vec<Expr>),
    /// `#{ k: v }`
    Map(Vec<(Expr, Expr)>),
    /// `a..b` / `a..=b`
    Range(Box<Expr>, Box<Expr>, bool),
    If(Vec<(Expr, Block)>, Option<Block>),
    /// `match subject { pattern => body, ... }`
    Match(Box<Expr>, Vec<Arm>),
    Do(Block),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Un(UnOp, Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum Stat {
    /// `let a = e;` (and `let mut a = e;` — everything is mutable anyway)
    Let(Vec<Name>, Vec<Expr>),
    /// `let` after resolution.
    LetSlots(Vec<Binding>, Vec<Expr>),
    /// `fn name(..) {..}` — the name is bound *before* the body is resolved,
    /// so the function can call itself.
    FnDecl(Name, Expr),
    /// `fn` after resolution.
    FnSlot(Binding, Expr),
    Assign(Vec<Expr>, Vec<Expr>),
    /// A compound assignment such as `x += 1`.
    OpAssign(Expr, BinOp, Expr),
    Expr(Expr),
    /// The `u32` is a stable id, so the runtime can count a loop's iterations
    /// and hand the hot ones to the JIT.
    While(u32, Expr, Block),
    Loop(u32, Block),
    /// `for i in 0..n { }` — kept separate from `ForIn` because it compiles.
    ForRange {
        id: u32,
        var: Name,
        binding: Option<Binding>,
        start: Expr,
        end: Expr,
        inclusive: bool,
        body: Block,
    },
    /// `for (k, v) in t.iter() { }`
    ForIn { id: u32, vars: Vec<Name>, bindings: Vec<Binding>, iter: Expr, body: Block },
    Return(Vec<Expr>),
    Break,
    Continue,
}

/// One `match` arm: patterns, an optional guard, and what to evaluate.
#[derive(Debug, Clone)]
pub struct Arm {
    pub patterns: Vec<Pattern>,
    pub guard: Option<Expr>,
    pub body: Block,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    /// `_`
    Wild,
    /// A literal to compare the subject against.
    Lit(Expr),
    /// A name, which binds the subject. `binding` is filled in by `resolve`.
    Bind(Name, Option<Binding>),
}

#[derive(Debug)]
pub struct FuncDef {
    pub id: usize,
    pub name: String,
    pub params: Vec<Name>,
    pub body: Block,
    pub line: u32,
    /// Filled in by `resolve`: frame size, parameter bindings, and where each
    /// captured upvalue comes from.
    pub n_slots: usize,
    pub param_bindings: Vec<Binding>,
    pub upvals: Vec<UpvalSrc>,
}

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
static NEXT_LOOP: AtomicUsize = AtomicUsize::new(0);

/// A fresh loop id.
pub fn next_loop_id() -> u32 {
    NEXT_LOOP.fetch_add(1, Ordering::Relaxed) as u32
}

impl FuncDef {
    pub fn new(name: String, params: Vec<Name>, body: Block, line: u32) -> Self {
        FuncDef {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            name,
            params,
            body,
            line,
            n_slots: 0,
            param_bindings: Vec::new(),
            upvals: Vec::new(),
        }
    }
}
