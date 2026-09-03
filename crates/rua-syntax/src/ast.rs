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

/// A type, as it was written.
///
/// Written types are kept as written — `Named` carries whatever word stood
/// there, and resolving it to something known is a later pass's business.
/// The arguments beside it are empty today and are where `Map<K, V>` will go,
/// so that adding generics is filling them in rather than reshaping this.
#[derive(Debug, Clone)]
pub enum Type {
    /// `number`, `string`, a name declared with `type`, or a parameter of one.
    Named(Rc<str>, Vec<Type>, Span),
    /// `[T]`
    Array(Box<Type>, Span),
    /// `#{ x: number, y: number }` — a shape, not a name for one.
    Record(Vec<(Name, Type)>, Span),
    /// `fn(A, B) -> C`, and `fn(route: string, data: T) -> U` when the
    /// arguments are worth naming — which is most of the time, since what an
    /// argument is for is the thing a reader goes to the source to find out.
    Fn(Vec<(Option<Name>, Type)>, Option<Box<Type>>, Span),
}

impl Type {
    pub fn span(&self) -> Span {
        match self {
            Type::Named(_, _, s) | Type::Array(_, s) | Type::Record(_, s) | Type::Fn(_, _, s) => *s,
        }
    }
}

impl std::fmt::Display for Type {
    /// The way it was written, which is what an editor shows.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Named(n, args, _) => {
                write!(f, "{n}")?;
                if let Some((first, rest)) = args.split_first() {
                    write!(f, "<{first}")?;
                    for a in rest {
                        write!(f, ", {a}")?;
                    }
                    write!(f, ">")?;
                }
                Ok(())
            }
            Type::Array(t, _) => write!(f, "[{t}]"),
            Type::Record(fields, _) => {
                write!(f, "#{{ ")?;
                for (i, (name, t)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{name}: {t}")?;
                }
                write!(f, " }}")
            }
            Type::Fn(args, ret, _) => {
                write!(f, "fn(")?;
                for (i, (name, a)) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    match name {
                        Some(n) => write!(f, "{n}: {a}")?,
                        None => write!(f, "{a}")?,
                    }
                }
                write!(f, ")")?;
                match ret {
                    Some(r) => write!(f, " -> {r}"),
                    None => Ok(()),
                }
            }
        }
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
    /// The type written beside it, if one was. It rides here because a name
    /// is written wherever a binding is — a parameter, a `let`, a field — and
    /// putting it anywhere else would mean reshaping all of those.
    pub ty: Option<Type>,
}

impl Name {
    pub fn new(text: impl Into<Rc<str>>, span: Span) -> Name {
        Name { text: text.into(), span, ty: None }
    }

    pub fn typed(text: impl Into<Rc<str>>, span: Span, ty: Option<Type>) -> Name {
        Name { text: text.into(), span, ty }
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
    /// `type Name = T`, and `type Handler<T, U> = ..` when it takes
    /// parameters. Nothing runs it; it is read by the checker and by the
    /// editor, and the compiler steps over it.
    TypeAlias(Name, Vec<Name>, Type),
    /// `impl Name { fn m(self, ..) { .. } }` — methods belonging to a shape.
    ///
    /// The resolver puts each one on the type's own table, so `Vec2::len(v)`
    /// is what it becomes and what anybody may write by hand. A `v.len()`
    /// whose receiver is known becomes the same call.
    Impl(Name, Vec<(Name, Expr)>),
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
    /// What it hands back, if that was written.
    pub ret: Option<Type>,
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
        FuncDef::typed(name, params, body, None, line)
    }

    pub fn typed(
        name: String,
        params: Vec<Name>,
        body: Block,
        ret: Option<Type>,
        line: u32,
    ) -> Self {
        FuncDef {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            name,
            params,
            body,
            ret,
            line,
            n_slots: 0,
            param_bindings: Vec::new(),
            upvals: Vec::new(),
        }
    }
}
