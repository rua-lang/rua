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
}

impl fmt::Debug for Native {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native {}", self.name)
    }
}

pub use rua_jit::JitFn;

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
    Str(Rc<str>),
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
    pub fn str(s: impl AsRef<str>) -> Value {
        Value::Str(Rc::from(s.as_ref()))
    }

    pub fn table(t: Table) -> Value {
        Value::Table(Rc::new(RefCell::new(t)))
    }

    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

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

    pub fn as_str(&self) -> Res<Rc<str>> {
        match self {
            Value::Str(s) => Ok(s.clone()),
            Value::Num(_) | Value::Bool(_) | Value::Nil => Ok(Rc::from(self.to_string().as_str())),
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
    Str(Rc<str>),
    /// f64 bits, so that 1.0 and 1 hash alike.
    Num(u64),
    Bool(bool),
    Ptr(usize),
}

impl Key {
    pub fn from_value(v: &Value) -> Res<Key> {
        Ok(match v {
            Value::Str(s) => Key::Str(s.clone()),
            Value::Num(n) => Key::Num(n.to_bits()),
            Value::Bool(b) => Key::Bool(*b),
            Value::Table(t) => Key::Ptr(Rc::as_ptr(t) as usize),
            Value::Func(t) => Key::Ptr(Rc::as_ptr(t) as usize),
            Value::Native(t) => Key::Ptr(Rc::as_ptr(t) as usize),
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
        }
    }
}

/// A table is an array part plus a hash part, as in Lua: `t[0..n]` lives in a
/// `Vec` (O(1) index and push), everything else in a map that remembers its
/// insertion order so iteration is predictable.
#[derive(Default, Debug)]
pub struct Table {
    arr: Vec<Value>,
    map: FxMap<Key, Value>,
    order: Vec<Key>,
    /// A plain `f64` copy of the array part, built on demand for compiled code
    /// so that it can read elements without calling back into the runtime.
    /// Any mutation throws it away.
    nums: Option<Vec<f64>>,
}

/// The array index a key denotes, if it denotes one.
fn array_index(k: &Key) -> Option<usize> {
    match k {
        Key::Num(bits) => {
            let n = f64::from_bits(*bits);
            if n >= 0.0 && n.fract() == 0.0 && n < usize::MAX as f64 {
                Some(n as usize)
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
        self.map.get(k).cloned().unwrap_or(Value::Nil)
    }

    pub fn get_str(&self, k: &str) -> Value {
        self.get(&Key::Str(Rc::from(k)))
    }

    /// A contiguous `f64` view of the array part, or `None` if any element is
    /// not a number. Compiled code reads through this directly; it is dropped
    /// by any write to the table, so it can never go stale.
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
        self.nums = None;
        if let Some(i) = array_index(&k) {
            if i < self.arr.len() {
                self.arr[i] = v;
                // a nil at the end shrinks the array part
                while matches!(self.arr.last(), Some(Value::Nil)) {
                    self.arr.pop();
                }
                return;
            }
            if i == self.arr.len() && !matches!(v, Value::Nil) {
                self.arr.push(v);
                self.absorb_from_map();
                return;
            }
        }
        if let Value::Nil = v {
            if self.map.remove(&k).is_some() {
                self.order.retain(|x| x != &k);
            }
            return;
        }
        if self.map.insert(k.clone(), v).is_none() {
            self.order.push(k);
        }
    }

    /// After growing the array part, pull in any keys that now sit next to it.
    fn absorb_from_map(&mut self) {
        loop {
            let next = Key::Num((self.arr.len() as f64).to_bits());
            match self.map.remove(&next) {
                Some(v) => {
                    self.order.retain(|x| x != &next);
                    self.arr.push(v);
                }
                None => return,
            }
        }
    }

    pub fn set_str(&mut self, k: &str, v: Value) {
        self.set(Key::Str(Rc::from(k)), v);
    }

    pub fn set_index(&mut self, i: usize, v: Value) {
        self.set(Key::Num((i as f64).to_bits()), v);
    }

    /// Array length: the dense `0..n` run.
    pub fn len(&self) -> usize {
        self.arr.len()
    }

    pub fn is_empty(&self) -> bool {
        self.arr.is_empty() && self.map.is_empty()
    }

    pub fn push(&mut self, v: Value) {
        if let Value::Nil = v {
            return;
        }
        self.nums = None;
        self.arr.push(v);
        self.absorb_from_map();
    }

    /// Array indices first, then the other keys in insertion order.
    pub fn keys(&self) -> Vec<Key> {
        let mut out: Vec<Key> = (0..self.arr.len())
            .map(|i| Key::Num((i as f64).to_bits()))
            .collect();
        out.extend(self.order.iter().cloned());
        out
    }
}
