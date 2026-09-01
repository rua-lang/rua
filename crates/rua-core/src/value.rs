//! Values, tables, environments. Reference counted, like Lua's GC but simpler
//! (cycles leak; that is the price of `Rc` and it is a fine price here).

use rua_syntax::ast::FuncDef;
use crate::interp::Vm;
use std::cell::{Cell, RefCell};
use crate::hash::FxMap;
use std::fmt;
use std::os::raw::c_void;
use std::rc::Rc;

/// A runtime error, with the line it came from once the interpreter has had a
/// chance to stamp it.
#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    /// The line, once known. `located` says whether it has been stamped.
    pub line: u32,
    pub located: bool,
    /// The function it happened in, if it was not top level code.
    pub where_: Option<Rc<str>>,
    /// The call stack when it happened, innermost last: the function called,
    /// and the line its caller called it from.
    pub trace: Vec<(Rc<str>, u32)>,
}

impl Error {
    /// The call stack, formatted one frame per line, innermost first.
    pub fn traceback(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (i, (name, line)) in self.trace.iter().enumerate().rev() {
            let caller = match i {
                0 => "top level".to_string(),
                _ => {
                    let (n, _) = &self.trace[i - 1];
                    if n.is_empty() {
                        "a closure".to_string()
                    } else {
                        n.to_string()
                    }
                }
            };
            let callee = if name.is_empty() { "a closure" } else { name };
            out.push(format!("{callee} called from {caller}, line {line}"));
        }
        out
    }
}

/// `Error("...")`, the way it reads everywhere in the standard library.
#[allow(non_snake_case)]
pub fn Error(message: impl Into<String>) -> Error {
    Error {
        message: message.into(),
        line: 0,
        located: false,
        where_: None,
        trace: Vec::new(),
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.located {
            write!(f, "line {}: ", self.line)?;
        }
        write!(f, "{}", self.message)?;
        if let Some(w) = &self.where_ {
            write!(f, " (in {w})")?;
        }
        Ok(())
    }
}
impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error(s)
    }
}
impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error(s.to_string())
    }
}

pub type Res<T> = Result<T, Error>;

pub fn err<T>(msg: impl Into<String>) -> Res<T> {
    Err(Error(msg.into()))
}

/// A function callable from Rust: this is the "Rust ABI" side of the FFI.
///
/// Arguments arrive as a slice of the VM's registers, so calling a builtin
/// copies nothing.
pub struct Native {
    pub name: String,
    pub f: Box<dyn Fn(&mut Vm, &[Value]) -> Res<Vec<Value>>>,
    /// The same builtin as a plain function of two arguments, when it is one.
    /// `t.push(v)` and `s.byte(i)` are this shape, and a method call counts
    /// its receiver as the first.
    pub fast2: Option<Box<dyn Fn(&Value, &Value) -> Res<Value>>>,
    /// The same builtin as a plain function of one argument, when it is one.
    ///
    /// The general shape — a slice in, a vector out — costs two pooled vectors
    /// and a copy each way for what is usually `type(x)` or `t.len()`. This
    /// form reads the argument out of the register and writes the result into
    /// another, and it is what most calls to a builtin actually are.
    pub fast1: Option<Box<dyn Fn(&Value) -> Res<Value>>>,
}

impl Native {
    pub fn new(
        name: impl Into<String>,
        f: impl Fn(&mut Vm, &[Value]) -> Res<Vec<Value>> + 'static,
    ) -> Native {
        Native { name: name.into(), f: Box::new(f), fast1: None, fast2: None }
    }

    /// A builtin of one argument, which needs nothing from the VM.
    pub fn unary(
        name: impl Into<String>,
        f: impl Fn(&Value) -> Res<Value> + Clone + 'static,
    ) -> Native {
        let g = f.clone();
        Native {
            name: name.into(),
            f: Box::new(move |_vm, args| {
                let v = g(args.first().unwrap_or(&Value::Nil))?;
                crate::interp::one_value(v)
            }),
            fast1: Some(Box::new(f)),
            fast2: None,
        }
    }

    /// A builtin that takes any number of arguments but has a two-argument
    /// form worth taking directly, which is what `t.push(v)` is.
    pub fn with_fast2(
        name: impl Into<String>,
        f: impl Fn(&mut Vm, &[Value]) -> Res<Vec<Value>> + 'static,
        fast: impl Fn(&Value, &Value) -> Res<Value> + 'static,
    ) -> Native {
        Native {
            name: name.into(),
            f: Box::new(f),
            fast1: None,
            fast2: Some(Box::new(fast)),
        }
    }

    /// A builtin of two arguments, which needs nothing from the VM.
    pub fn binary(
        name: impl Into<String>,
        f: impl Fn(&Value, &Value) -> Res<Value> + Clone + 'static,
    ) -> Native {
        let g = f.clone();
        Native {
            name: name.into(),
            f: Box::new(move |_vm, args| {
                let a = args.first().unwrap_or(&Value::Nil);
                let b = args.get(1).unwrap_or(&Value::Nil);
                let v = g(a, b)?;
                crate::interp::one_value(v)
            }),
            fast1: None,
            fast2: Some(Box::new(f)),
        }
    }
}

impl fmt::Debug for Native {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native {}", self.name)
    }
}

pub use rua_jit::JitFn;

/// A string value.
///
/// `Rc<str>` would be the obvious handle, but it is a *fat* pointer, and
/// `Value` is a union of everything — so one 16-byte variant would push every
/// value in the system to 24 bytes: every register, every array element, every
/// argument. Wrapping the fat pointer in a thin one keeps `Value` at 16, which
/// measures as a ~10% interpreter speedup across the benchmark suite. The cost
/// is one extra load to reach the bytes, which is cheap next to moving a third
/// more memory on every register write.
///
/// The handle also carries the string's hash. A table lookup with a string key
/// is the inner loop of any program that uses tables as objects, and hashing
/// the bytes again at every one of them is pure repetition: the bytes cannot
/// change. Computing it at creation costs one pass over a string that was just
/// copied anyway, and leaves every later lookup a single load.
#[derive(Clone)]
pub struct RStr(Rc<StrData>);

#[derive(Debug)]
pub struct StrData {
    hash: u64,
    s: Box<str>,
}

/// Short strings are interned, as they are in Lua: equal bytes mean one
/// allocation, so `==` on two symbols is a pointer comparison and a table
/// lookup keyed by a name never compares bytes at all. A program that uses
/// tables as objects — or an interpreter, where every step compares symbols —
/// does that constantly.
///
/// Long strings are left alone: interning them would compare the bytes on
/// every creation, which is the cost it exists to avoid.
const INTERN_MAX: usize = 40;

thread_local! {
    static INTERN: RefCell<Interner> = RefCell::new(Interner::default());
}

/// Strings by hash. Entries are dropped once the interner is the only holder,
/// which is checked when the table has doubled since the last sweep — the
/// reference count is the liveness information a mark phase would go looking
/// for.
#[derive(Default)]
struct Interner {
    map: FxMap<u64, Vec<RStr>>,
    live: usize,
    sweep_at: usize,
}

impl Interner {
    fn intern(&mut self, hash: u64, s: &str) -> RStr {
        let bucket = self.map.entry(hash).or_default();
        for r in bucket.iter() {
            if &*r.0.s == s {
                return r.clone();
            }
        }
        let fresh = RStr(Rc::new(StrData {
            hash,
            s: Box::from(s),
        }));
        bucket.push(fresh.clone());
        self.live += 1;
        if self.live > self.sweep_at {
            self.sweep();
        }
        fresh
    }

    fn sweep(&mut self) {
        let mut live = 0;
        self.map.retain(|_, bucket| {
            bucket.retain(|r| Rc::strong_count(&r.0) > 1);
            live += bucket.len();
            !bucket.is_empty()
        });
        self.live = live;
        self.sweep_at = (live * 2).max(512);
    }
}

impl RStr {
    pub fn new(s: &str) -> RStr {
        let hash = crate::hash::str_hash(s);
        if s.len() > INTERN_MAX {
            return RStr(Rc::new(StrData {
                hash,
                s: Box::from(s),
            }));
        }
        // during thread teardown the table is gone; an uninterned string is
        // still a correct string
        INTERN
            .try_with(|i| i.borrow_mut().intern(hash, s))
            .unwrap_or_else(|_| {
                RStr(Rc::new(StrData {
                    hash,
                    s: Box::from(s),
                }))
            })
    }

    pub fn as_str(&self) -> &str {
        &self.0.s
    }

    /// The hash of the bytes, computed when the string was made.
    #[inline]
    pub fn hash_bits(&self) -> u64 {
        self.0.hash
    }

    /// Do these two handles point at the same allocation? A cheap pre-test
    /// before comparing bytes.
    pub fn same(&self, other: &RStr) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl std::ops::Deref for RStr {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0.s
    }
}

impl PartialEq for RStr {
    fn eq(&self, other: &Self) -> bool {
        // same allocation, then same hash, and only then the bytes: two
        // different strings almost never reach the third test
        self.same(other) || (self.0.hash == other.0.hash && self.0.s == other.0.s)
    }
}
impl Eq for RStr {}

impl std::hash::Hash for RStr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.0.hash)
    }
}

impl fmt::Display for RStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self)
    }
}

impl fmt::Debug for RStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl From<&str> for RStr {
    fn from(s: &str) -> RStr {
        RStr::new(s)
    }
}

impl From<String> for RStr {
    fn from(s: String) -> RStr {
        RStr::new(&s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JitState {
    Cold,
    Compiled,
    /// Tried and cannot: not a numeric-only function, or rustc said no.
    Blocked,
}

/// A shared, mutable variable: what a captured local becomes.
pub type CellRef = Rc<RefCell<Value>>;

pub struct Function {
    /// The compiled body, which also carries the AST the JIT reads.
    pub proto: Rc<crate::bytecode::Proto>,
    /// What the compiled entry point expects of each argument, when there is
    /// one. Empty until the JIT has had a look.
    pub param_kinds: RefCell<Vec<rua_jit::Kind>>,
    /// The context compiled code runs with: hook addresses and callees.
    pub rt: RefCell<Option<crate::interp::RtCtxHolder>>,
    /// Whether the compiled entry point's result means nothing, because the
    /// function is a procedure.
    pub returns_nil: Cell<bool>,
    /// Captured variables, in the order `FuncDef::upvals` describes.
    pub upvals: Rc<Vec<CellRef>>,
    pub hits: Cell<u32>,
    pub jit: Cell<Option<JitFn>>,
    pub jit_state: Cell<JitState>,
}

impl Function {
    /// The syntax this function was compiled from, for the JIT.
    pub fn def(&self) -> &Rc<FuncDef> {
        &self.proto.def
    }
}

impl fmt::Debug for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "function {}", self.proto.def.name)
    }
}

#[derive(Clone, Default)]
pub enum Value {
    #[default]
    Nil,
    Bool(bool),
    Num(f64),
    Str(RStr),
    Table(Rc<RefCell<Table>>),
    Func(Rc<Function>),
    Native(Rc<Native>),
    /// Raw C pointer (`ffi::load` handles, `ffi::string` buffers, cdata).
    Ptr(*mut c_void),
    /// A captured local. Never visible to scripts: it only ever sits in a
    /// frame slot that the resolver marked as captured.
    Cell(CellRef),
}

impl Value {
    /// Overwrite a slot with a new value.
    ///
    /// `*slot = v` is the same thing, but it always calls `Value`'s drop glue,
    /// which is an out-of-line call: too big for LLVM to inline into every
    /// register write in the interpreter. Most values written over are numbers
    /// or nil, which need no drop at all, so this tests the tag first and only
    /// calls the glue when there is something to release. It is worth about a
    /// tenth of the interpreter.
    #[inline(always)]
    pub fn put(slot: &mut Value, v: Value) {
        match slot {
            Value::Nil | Value::Num(_) | Value::Bool(_) | Value::Ptr(_) => {
                // SAFETY: the old value owns nothing, so overwriting the bytes
                // leaks nothing; `v` is moved in and not dropped here.
                unsafe { std::ptr::write(slot, v) }
            }
            _ => *slot = v,
        }
    }

    pub fn str(s: impl AsRef<str>) -> Value {
        Value::Str(RStr::new(s.as_ref()))
    }

    pub fn table(t: Table) -> Value {
        Value::Table(Rc::new(RefCell::new(t)))
    }

    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    /// The index of this value's type name in [`Value::TYPE_NAMES`].
    #[inline]
    pub fn type_index(&self) -> usize {
        match self {
            Value::Nil => 0,
            Value::Bool(_) => 1,
            Value::Num(_) => 2,
            Value::Str(_) => 3,
            Value::Table(_) => 4,
            Value::Func(_) | Value::Native(_) => 5,
            Value::Ptr(_) => 6,
            Value::Cell(c) => c.borrow().type_index(),
        }
    }

    pub const TYPE_NAMES: [&'static str; 7] = [
        "nil", "boolean", "number", "string", "table", "function", "cdata",
    ];

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::Str(_) => "string",
            Value::Table(_) => "table",
            Value::Func(_) | Value::Native(_) => "function",
            Value::Ptr(_) => "cdata",
            Value::Cell(c) => c.borrow().type_name(),
        }
    }

    pub fn as_num(&self) -> Res<f64> {
        match self {
            Value::Num(n) => Ok(*n),
            Value::Str(s) => s
                .trim()
                .parse::<f64>()
                .map_err(|_| Error(format!("cannot convert {:?} to a number", &**s))),
            other => err(format!("attempt to use a {} as a number", other.type_name())),
        }
    }

    pub fn as_str(&self) -> Res<RStr> {
        match self {
            Value::Str(s) => Ok(s.clone()),
            Value::Num(_) | Value::Bool(_) | Value::Nil => Ok(RStr::from(self.to_string())),
            other => err(format!("attempt to use a {} as a string", other.type_name())),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Nil, Value::Nil) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Num(a), Value::Num(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Table(a), Value::Table(b)) => Rc::ptr_eq(a, b),
            (Value::Func(a), Value::Func(b)) => Rc::ptr_eq(a, b),
            (Value::Native(a), Value::Native(b)) => Rc::ptr_eq(a, b),
            (Value::Ptr(a), Value::Ptr(b)) => a == b,
            (Value::Cell(a), b) => &*a.borrow() == b,
            (a, Value::Cell(b)) => a == &*b.borrow(),
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Nil => write!(f, "nil"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    write!(f, "{}", *n as i64)
                } else {
                    write!(f, "{n}")
                }
            }
            Value::Str(s) => write!(f, "{s}"),
            Value::Table(t) => write!(f, "table: {:p}", Rc::as_ptr(t)),
            Value::Func(x) => write!(f, "function: {}", x.proto.def.name),
            Value::Native(x) => write!(f, "function: builtin {}", x.name),
            Value::Ptr(p) => write!(f, "cdata: {p:p}"),
            Value::Cell(c) => write!(f, "{}", c.borrow()),
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub enum Key {
    Str(RStr),
    /// f64 bits, so that 1.0 and 1 hash alike.
    Num(u64),
    Bool(bool),
    /// A raw C pointer used as a key.
    Ptr(usize),
    /// A table or function used as a key: identity is its address, but the
    /// value is kept so the entry holds it alive and hands the *same* thing
    /// back — an address alone would be both a dangling identity and, once
    /// returned to a script, a forged `cdata` pointer.
    Obj(ObjKey),
}

/// A table or function as a table key: hashed and compared by identity.
#[derive(Clone, Debug)]
pub struct ObjKey(pub Value);

impl ObjKey {
    fn addr(&self) -> usize {
        match &self.0 {
            Value::Table(t) => Rc::as_ptr(t) as usize,
            Value::Func(f) => Rc::as_ptr(f) as usize,
            Value::Native(n) => Rc::as_ptr(n) as usize,
            other => other as *const Value as usize,
        }
    }
}

impl PartialEq for ObjKey {
    fn eq(&self, other: &Self) -> bool {
        self.addr() == other.addr()
    }
}
impl Eq for ObjKey {}

impl std::hash::Hash for ObjKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr().hash(state)
    }
}

impl Key {
    pub fn from_value(v: &Value) -> Res<Key> {
        Ok(match v {
            Value::Str(s) => Key::Str(s.clone()),
            Value::Num(n) => Key::Num(n.to_bits()),
            Value::Bool(b) => Key::Bool(*b),
            Value::Table(_) | Value::Func(_) | Value::Native(_) => Key::Obj(ObjKey(v.clone())),
            Value::Ptr(p) => Key::Ptr(*p as usize),
            Value::Cell(c) => return Key::from_value(&c.borrow()),
            Value::Nil => return err("table index is nil"),
        })
    }

    pub fn to_value(&self) -> Value {
        match self {
            Key::Str(s) => Value::Str(s.clone()),
            Key::Num(b) => Value::Num(f64::from_bits(*b)),
            Key::Bool(b) => Value::Bool(*b),
            Key::Ptr(p) => Value::Ptr(*p as *mut c_void),
            Key::Obj(o) => o.0.clone(),
        }
    }
}

/// A table is an array part plus a keyed part, as in Lua: `t[0..n]` lives in a
/// `Vec` (O(1) index and push), everything else in an insertion-ordered list of
/// pairs. Object-shaped tables — a handful of named fields — are the common
/// case, and for those a linear scan beats hashing and costs one allocation
/// instead of three. A hash index appears only once a table outgrows that.
#[derive(Default, Debug)]
pub struct Table {
    arr: Vec<Value>,
    /// Keyed entries, in insertion order.
    pairs: Vec<(Key, Value)>,
    /// Key to index into `pairs`, built once `pairs` gets long.
    index: Option<Box<FxMap<Key, usize>>>,
    /// The views of every element, for compiled code that walks an array of
    /// arrays, with the shape epoch they were built at. Rebuilding them at
    /// every call was a third of n-body.
    spans: Option<(u64, Box<[rua_jit::RtSpan]>)>,
    /// A plain `f64` copy of the array part, built on demand for compiled code
    /// so that it can read elements without calling back into the runtime.
    /// Any mutation that could invalidate it throws it away.
    nums: Option<Vec<f64>>,
}

thread_local! {
    /// Bumped whenever any table's storage moves or changes shape. Compiled
    /// code holds raw views into tables; this is how anything cached about
    /// them knows it is still true.
    static SHAPE_EPOCH: Cell<u64> = const { Cell::new(1) };
}

pub fn shape_epoch() -> u64 {
    SHAPE_EPOCH.with(|e| e.get())
}

pub fn bump_shape_epoch() {
    SHAPE_EPOCH.with(|e| e.set(e.get().wrapping_add(1)));
}

/// Above this many keyed entries, a table builds a hash index.
const INDEX_THRESHOLD: usize = 8;

/// The array index a key denotes, if it denotes one.
fn array_index(k: &Key) -> Option<usize> {
    match k {
        Key::Num(bits) => {
            let n = f64::from_bits(*bits);
            let i = n as usize;
            if i as f64 == n {
                Some(i)
            } else {
                None
            }
        }
        _ => None,
    }
}

impl Table {
    pub fn new() -> Table {
        Table::default()
    }

    pub fn get(&self, k: &Key) -> Value {
        if let Some(i) = array_index(k) {
            if i < self.arr.len() {
                return self.arr[i].clone();
            }
        }
        match self.find(k) {
            Some(i) => self.pairs[i].1.clone(),
            None => Value::Nil,
        }
    }

    /// Read `t.field` without building an owned `Key` first.
    ///
    /// The obvious `get(&Key::Str(name.clone()))` costs a refcount round trip
    /// and a 32-byte temporary with a destructor, on an operation that is
    /// otherwise a pointer comparison. Object-shaped tables are small, so the
    /// scan is short; a table big enough to have built an index falls back.
    ///
    /// `t.name`, remembering where it was found.
    ///
    /// A field read is a scan or a hash probe; both are avoidable, because the
    /// same line of a program reads the same field of objects built the same
    /// way. The cache holds a position, and checking it is one pointer
    /// comparison — the names are interned, so equal names are one allocation.
    #[inline]
    pub fn get_field_cached(&self, name: &RStr, at: &std::cell::Cell<u32>) -> Value {
        if let Some((Key::Str(ks), v)) = self.pairs.get(at.get() as usize) {
            if ks.same(name) {
                return v.clone();
            }
        }
        match self.probe_str(name) {
            Some(i) => {
                at.set(i as u32);
                self.pairs[i].1.clone()
            }
            None => Value::Nil,
        }
    }

    pub fn get_field(&self, name: &RStr) -> Option<Value> {
        Some(match self.probe_str(name) {
            Some(i) => self.pairs[i].1.clone(),
            None => Value::Nil,
        })
    }

    /// Where a string key lives in `pairs`.
    ///
    /// The hash index is keyed by `Key`, and building one from a name means an
    /// `Rc` clone: an increment on the way in and a decrement on the way out,
    /// at every field read in the program. This borrows the handle instead —
    /// a bitwise copy that is never dropped and never leaves this function, so
    /// nothing owns it and nothing double-frees.
    #[inline]
    fn probe_str(&self, name: &RStr) -> Option<usize> {
        match &self.index {
            Some(ix) => {
                let borrowed = std::mem::ManuallyDrop::new(Key::Str(
                    // SAFETY: `name` outlives `borrowed`, which is never
                    // dropped, stored or handed out.
                    unsafe { std::ptr::read(name) },
                ));
                ix.get(&borrowed).copied()
            }
            None => self.pairs.iter().position(|(k, _)| match k {
                Key::Str(ks) => ks == name,
                _ => false,
            }),
        }
    }

    /// Write `t.field = v` in the same way. Returns whether it happened; a nil
    /// value goes the long way round, since removing an entry moves the rest.
    #[inline]
    pub fn set_field(&mut self, name: &RStr, v: &Value) -> bool {
        if matches!(v, Value::Nil) {
            return false;
        }
        if let Some(i) = self.probe_str(name) {
            self.pairs[i].1 = v.clone();
            return true;
        }
        self.pairs.push((Key::Str(name.clone()), v.clone()));
        let last = self.pairs.len() - 1;
        match &mut self.index {
            Some(ix) => {
                ix.insert(Key::Str(name.clone()), last);
            }
            None if self.pairs.len() > INDEX_THRESHOLD => self.reindex(),
            None => {}
        }
        true
    }

    /// Where `k` lives in `pairs`, by index or by scan.
    #[inline]
    fn find(&self, k: &Key) -> Option<usize> {
        match &self.index {
            Some(ix) => ix.get(k).copied(),
            None => self.pairs.iter().position(|(key, _)| key == k),
        }
    }

    pub fn get_str(&self, k: &str) -> Value {
        self.get(&Key::Str(RStr::new(k)))
    }

    /// Read `t[n]` when `n` is an exact index into the array part.
    ///
    /// This is the hot path for every `t[i]` in the language, so it avoids
    /// building a `Key` and avoids `fract()` — the cast round trip is both the
    /// integer test and the index.
    #[inline]
    pub fn get_num(&self, n: f64) -> Option<&Value> {
        let i = n as usize;
        if i as f64 == n {
            return self.arr.get(i);
        }
        None
    }

    /// Write `t[n] = v` in place when `n` indexes the array part and the write
    /// cannot change its shape. Returns whether it happened.
    #[inline]
    pub fn set_num(&mut self, n: f64, v: &Value) -> bool {
        let i = n as usize;
        if i as f64 != n || i >= self.arr.len() || matches!(v, Value::Nil) {
            return false;
        }
        match (&mut self.nums, v) {
            (Some(cache), Value::Num(x)) => cache[i] = *x,
            (slot @ Some(_), _) => *slot = None,
            (None, _) => {}
        }
        self.arr[i] = v.clone();
        true
    }

    /// A contiguous `f64` view of the array part, or `None` if any element is
    /// not a number. Compiled code reads through this directly; it is dropped
    /// by any write to the table, so it can never go stale.
    /// A view of the numeric array part that compiled code writes *through*.
    ///
    /// While compiled code runs, this cache is the authority: writing to it
    /// costs one store rather than a call back into the runtime. The array
    /// part is left alone, which is what makes a trap recoverable — throwing
    /// the cache away throws every compiled write away with it.
    pub fn nums_span_mut(&mut self) -> Option<(*mut f64, usize)> {
        self.nums_span()?;
        // what is written through it no longer matches the array part
        let cache = self.nums.as_mut()?;
        Some((cache.as_mut_ptr(), cache.len()))
    }

    /// Compiled code finished: copy what it wrote back over the array part.
    pub fn commit_nums(&mut self) {
        if let Some(cache) = &self.nums {
            for (slot, n) in self.arr.iter_mut().zip(cache) {
                *slot = Value::Num(*n);
            }
        }
    }

    /// The element views built earlier, if nothing has moved since.
    ///
    /// Correctness rests on the epoch moving whenever any table's storage
    /// does, which is a rule someone has to remember. A debug build checks it
    /// instead: a stale view fails a test rather than reading freed memory.
    pub fn cached_spans(&self, epoch: u64) -> Option<(*const rua_jit::RtSpan, usize)> {
        match &self.spans {
            Some((at, views)) if *at == epoch => {
                debug_assert!(self.views_still_true(views), "a cached element view went stale");
                Some((views.as_ptr(), views.len()))
            }
            _ => None,
        }
    }

    #[cfg(debug_assertions)]
    fn views_still_true(&self, views: &[rua_jit::RtSpan]) -> bool {
        if views.len() != self.arr.len() {
            return false;
        }
        self.arr.iter().zip(views).all(|(v, span)| match v {
            Value::Table(t) => match t.try_borrow() {
                Ok(inner) => match &inner.nums {
                    Some(cache) => cache.as_ptr() == span.ptr && cache.len() == span.len,
                    None => false,
                },
                Err(_) => false,
            },
            _ => false,
        })
    }

    #[cfg(not(debug_assertions))]
    fn views_still_true(&self, _views: &[rua_jit::RtSpan]) -> bool {
        true
    }

    /// Keep a freshly built set of views, handing back the old one so that
    /// compiled code still reading through it is not left holding nothing.
    pub fn replace_spans(
        &mut self,
        epoch: u64,
        views: Box<[rua_jit::RtSpan]>,
    ) -> Option<Box<[rua_jit::RtSpan]>> {
        let old = self.spans.take().map(|(_, v)| v);
        self.spans = Some((epoch, views));
        old
    }

    /// Compiled code bailed out: throw away what it wrote.
    pub fn discard_nums(&mut self) {
        self.nums = None;
        bump_shape_epoch();
    }

    /// Is the array part all there is? An append then adds one slot and takes
    /// nothing from the keyed part, so undoing it is a truncation.
    pub fn is_plain_array(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Undo appends compiled code made, back to the length it started at.
    pub fn truncate_arr(&mut self, n: usize) {
        if n < self.arr.len() {
            bump_shape_epoch();
            self.arr.truncate(n);
            if let Some(cache) = &mut self.nums {
                cache.truncate(n);
            }
        }
    }

    pub fn nums_span(&mut self) -> Option<(*const f64, usize)> {
        if self.nums.is_none() {
            let mut out = Vec::with_capacity(self.arr.len());
            for v in &self.arr {
                match v {
                    Value::Num(n) => out.push(*n),
                    _ => return None,
                }
            }
            self.nums = Some(out);
            // the view is a fresh allocation, so anything holding the old one
            // is looking at nothing
            bump_shape_epoch();
        }
        let cache = self.nums.as_ref().expect("just filled in");
        Some((cache.as_ptr(), cache.len()))
    }

    /// The element at a numeric index, if it is a number in the array part.
    /// This is the hot path for compiled code, so it does the index maths
    /// itself rather than going through a `Key`.
    #[inline]
    pub fn num_at(&self, i: f64) -> Option<f64> {
        if i < 0.0 || i.fract() != 0.0 {
            return None;
        }
        match self.arr.get(i as usize) {
            Some(Value::Num(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn get_index(&self, i: usize) -> Value {
        self.arr.get(i).cloned().unwrap_or_else(|| self.get(&Key::Num((i as f64).to_bits())))
    }

    pub fn set(&mut self, k: Key, v: Value) {
        if let Some(i) = array_index(&k) {
            // an in-place numeric write can stay in the view too
            match (&mut self.nums, &v) {
                (Some(cache), Value::Num(n)) if i < cache.len() => cache[i] = *n,
                (slot @ Some(_), _) => {
                    *slot = None;
                    bump_shape_epoch();
                }
                (None, _) => {}
            }
            if i < self.arr.len() {
                self.arr[i] = v;
                // a nil at the end shrinks the array part
                while matches!(self.arr.last(), Some(Value::Nil)) {
                    self.arr.pop();
                    self.nums = None;
                    bump_shape_epoch();
                }
                return;
            }
            if i == self.arr.len() && !matches!(v, Value::Nil) {
                self.push(v);
                return;
            }
        }
        // a write outside the array part cannot touch the view

        if let Value::Nil = v {
            if let Some(i) = self.find(&k) {
                self.pairs.remove(i);
                // the indices after it just moved
                self.index = None;
                self.reindex();
            }
            return;
        }
        match self.find(&k) {
            Some(i) => self.pairs[i].1 = v,
            None => {
                self.pairs.push((k.clone(), v));
                match &mut self.index {
                    Some(ix) => {
                        ix.insert(k, self.pairs.len() - 1);
                    }
                    None if self.pairs.len() > INDEX_THRESHOLD => self.reindex(),
                    None => {}
                }
            }
        }
    }

    /// Build (or rebuild) the hash index, once a table is big enough to want
    /// one.
    fn reindex(&mut self) {
        if self.pairs.len() <= INDEX_THRESHOLD {
            return;
        }
        let mut ix = FxMap::default();
        for (i, (k, _)) in self.pairs.iter().enumerate() {
            ix.insert(k.clone(), i);
        }
        self.index = Some(Box::new(ix));
    }

    /// After growing the array part, pull in any keys that now sit next to it.
    fn absorb_from_map(&mut self) {
        // the common case is a pure array, where there is nothing to absorb
        if self.pairs.is_empty() {
            return;
        }
        // anything pulled in from the keyed part may not be a number
        loop {
            let next = Key::Num((self.arr.len() as f64).to_bits());
            match self.find(&next) {
                Some(i) => {
                    self.nums = None;
                    let (_, v) = self.pairs.remove(i);
                    self.arr.push(v);
                    self.index = None;
                    self.reindex();
                    bump_shape_epoch();
                }
                None => return,
            }
        }
    }

    pub fn set_str(&mut self, k: &str, v: Value) {
        self.set(Key::Str(RStr::new(k)), v);
    }

    pub fn set_index(&mut self, i: usize, v: Value) {
        self.set(Key::Num((i as f64).to_bits()), v);
    }

    /// Array length: the dense `0..n` run.
    pub fn len(&self) -> usize {
        self.arr.len()
    }

    pub fn is_empty(&self) -> bool {
        self.arr.is_empty() && self.pairs.is_empty()
    }

    /// Append, including a nil: `[1, nil, 2]` keeps its three slots, so that
    /// `len()` and iteration agree with what was written.
    pub fn push(&mut self, v: Value) {
        // Only a *move* invalidates what compiled code holds. A push that fits
        // in the space already there moves nothing, and a matrix multiply
        // pushing a row at a time would otherwise throw away the views of
        // every other row on each one.
        let room = self.arr.capacity();
        let room_nums = self.nums.as_ref().map_or(usize::MAX, |c| c.capacity());
        // Keep the numeric view in step rather than dropping it. Filling an
        // array and then reading it is the common shape, and rebuilding the
        // view on the next read costs more than the whole fill.
        match (&mut self.nums, &v) {
            (Some(cache), Value::Num(n)) => cache.push(*n),
            (slot @ Some(_), _) => *slot = None,
            (None, _) => {}
        }
        self.arr.push(v);
        if self.arr.capacity() != room
            || self.nums.as_ref().map_or(usize::MAX, |c| c.capacity()) != room_nums
        {
            bump_shape_epoch();
        }
        self.absorb_from_map();
    }

    /// Array indices first, then the other keys in insertion order.
    pub fn keys(&self) -> Vec<Key> {
        let mut out: Vec<Key> = (0..self.arr.len())
            .map(|i| Key::Num((i as f64).to_bits()))
            .collect();
        out.extend(self.pairs.iter().map(|(k, _)| k.clone()));
        out
    }
}

#[cfg(test)]
mod size {
    /// `Value` must stay 16 bytes: it is copied on every register write, and
    /// widening it costs about 10% of the interpreter (measured).
    #[test]
    fn value_is_two_words() {
        assert_eq!(std::mem::size_of::<super::Value>(), 16);
    }
}
