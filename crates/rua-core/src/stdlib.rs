//! The standard library: a handful of globals plus the `math`, `string`,
//! `table`, `os`, `io`, `ffi` and `jit` modules.
//!
//! Module functions double as methods: `t.push(1)` and `"ab".upper()` are
//! `table::push(t, 1)` and `string::upper("ab")` — the receiver is just the
//! first argument, as in Rust.

use crate::cffi;
use rua_ffi as ffi;
use crate::interp::Vm;
use crate::value::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

thread_local! {
    static START: Instant = Instant::now();
    static SEED: Cell<u64> = const { Cell::new(0x2545F4914F6CDD1D) };
}

fn native<F>(name: &str, f: F) -> Value
where
    F: Fn(&mut Vm, &[Value]) -> Res<Vec<Value>> + 'static,
{
    Value::Native(Rc::new(Native { name: name.to_string(), f: Box::new(f) }))
}

/// Everything after the first argument, without panicking when there is none.
fn rest(args: &[Value]) -> &[Value] {
    args.get(1..).unwrap_or(&[])
}

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Nil)
}

fn num_arg(args: &[Value], i: usize) -> Res<f64> {
    arg(args, i).as_num()
}

fn str_arg(args: &[Value], i: usize) -> Res<Rc<str>> {
    arg(args, i).as_str()
}

fn table_arg(args: &[Value], i: usize) -> Res<Rc<RefCell<Table>>> {
    match arg(args, i) {
        Value::Table(t) => Ok(t),
        other => err(format!("expected a table, got {}", other.type_name())),
    }
}

fn one(v: Value) -> Res<Vec<Value>> {
    Ok(vec![v])
}

fn module(vm: &mut Vm, name: &str, entries: Vec<(&str, Value)>) -> Rc<RefCell<Table>> {
    let t = Rc::new(RefCell::new(Table::new()));
    for (k, v) in entries {
        t.borrow_mut().set_str(k, v);
    }
    vm.set_global(name, Value::Table(t.clone()));
    t
}

pub fn install(vm: &mut Vm) {
    use crate::interp::MethodTable;
    base(vm);
    let m = math(vm);
    vm.set_method_table(MethodTable::Math, m);
    let s = string(vm);
    vm.set_method_table(MethodTable::Str, s);
    let t = table_lib(vm);
    vm.set_method_table(MethodTable::Table, t);
    os_io(vm);
    ffi_lib(vm);
    jit_lib(vm);
    vm.set_global("VERSION", Value::str("rua 0.1"));
}

fn base(vm: &mut Vm) {
    vm.register("print", |_vm, args| {
        let line: Vec<String> = args.iter().map(|v| v.to_string()).collect();
        println!("{}", line.join(" "));
        Ok(Vec::new())
    });
    vm.register("format", |_vm, args| {
        one(Value::str(format_impl(&str_arg(args, 0)?, rest(args))?))
    });
    vm.register("type", |_vm, args| one(Value::str(arg(args, 0).type_name())));
    vm.register("str", |_vm, args| one(Value::str(arg(args, 0).to_string())));
    vm.register("num", |_vm, args| {
        one(match arg(args, 0).as_num() {
            Ok(n) => Value::Num(n),
            Err(_) => Value::Nil,
        })
    });
    vm.register("error", |_vm, args| Err(Error(arg(args, 0).to_string())));
    vm.register("assert", |_vm, args| {
        if arg(args, 0).truthy() {
            Ok(args.to_vec())
        } else {
            Err(Error(match args.get(1) {
                Some(m) => m.to_string(),
                None => "assertion failed".to_string(),
            }))
        }
    });
    // `try(f, args...)` -> (ok, value_or_message)
    vm.register("try", |vm, args| {
        let Some(f) = args.first().cloned() else {
            return err("try needs a function");
        };
        match vm.call(&f, args[1..].to_vec()) {
            Ok(mut vals) => {
                let mut out = vec![Value::Bool(true)];
                out.append(&mut vals);
                Ok(out)
            }
            Err(e) => Ok(vec![Value::Bool(false), Value::str(e.to_string())]),
        }
    });
    vm.register("len", |_vm, args| match arg(args, 0) {
        Value::Table(t) => one(Value::Num(t.borrow().len() as f64)),
        Value::Str(s) => one(Value::Num(s.len() as f64)),
        other => err(format!("len: unexpected {}", other.type_name())),
    });
    // dynamic access to the global namespace, since it is not a table
    vm.register("global", |vm, args| {
        let name = str_arg(args, 0)?;
        match args.len() {
            0 | 1 => one(vm.get_global(&name)),
            _ => {
                vm.set_global(&name, arg(args, 1));
                Ok(Vec::new())
            }
        }
    });
    vm.register("globals", |vm, _args| {
        let mut t = Table::new();
        for n in vm.global_names() {
            t.push(Value::Str(n));
        }
        one(Value::table(t))
    });
    vm.register("dofile", |vm, args| {
        let path = str_arg(args, 0)?;
        vm.eval_file(&path)
    });
    // `let m = require("lib.rua")` — runs the file once and returns its value,
    // which is whatever expression the file ends with
    vm.register("require", |vm, args| {
        let path = str_arg(args, 0)?;
        let key = std::fs::canonicalize(&*path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string());
        if let Some(cached) = vm.modules.get(&key) {
            return one(cached.clone());
        }
        // insert before running, so a cycle sees nil instead of looping
        vm.modules.insert(key.clone(), Value::Nil);
        let out = match vm.eval_file(&path) {
            Ok(out) => out,
            Err(e) => {
                // a module that failed must be loadable again later
                vm.modules.remove(&key);
                return Err(e);
            }
        };
        let v = out.into_iter().next().unwrap_or(Value::Nil);
        vm.modules.insert(key, v.clone());
        one(v)
    });
}

/// The iterator protocol: a function called with no arguments that yields the
/// next value(s), or nil when it is done. `for` speaks exactly this.
pub fn iterator<F>(name: &str, f: F) -> Value
where
    F: Fn() -> Option<Vec<Value>> + 'static,
{
    native(name, move |_vm, _args| Ok(f().unwrap_or_else(|| vec![Value::Nil])))
}

/// `for v in t` walks a table's values, in key order.
pub fn value_iterator(t: Rc<RefCell<Table>>) -> Value {
    let keys = t.borrow().keys();
    let i = Cell::new(0usize);
    iterator("values", move || {
        let idx = i.get();
        let k = keys.get(idx)?.clone();
        i.set(idx + 1);
        Some(vec![t.borrow().get(&k)])
    })
}

pub fn range_iterator(start: f64, end: f64, inclusive: bool) -> Value {
    let i = Cell::new(start);
    iterator("range", move || {
        let cur = i.get();
        let done = if inclusive { cur > end } else { cur >= end };
        if done {
            return None;
        }
        i.set(cur + 1.0);
        Some(vec![Value::Num(cur)])
    })
}

fn math(vm: &mut Vm) -> Rc<RefCell<Table>> {
    let f1 = |name: &'static str, f: fn(f64) -> f64| {
        native(name, move |_vm, args| one(Value::Num(f(num_arg(args, 0)?))))
    };
    module(
        vm,
        "math",
        vec![
            ("pi", Value::Num(std::f64::consts::PI)),
            ("e", Value::Num(std::f64::consts::E)),
            ("inf", Value::Num(f64::INFINITY)),
            ("floor", f1("floor", f64::floor)),
            ("ceil", f1("ceil", f64::ceil)),
            ("round", f1("round", f64::round)),
            ("abs", f1("abs", f64::abs)),
            ("sqrt", f1("sqrt", f64::sqrt)),
            ("sin", f1("sin", f64::sin)),
            ("cos", f1("cos", f64::cos)),
            ("tan", f1("tan", f64::tan)),
            ("exp", f1("exp", f64::exp)),
            ("ln", f1("ln", f64::ln)),
            ("log", f1("log", f64::ln)),
            ("pow", native("pow", |_vm, args| {
                one(Value::Num(num_arg(args, 0)?.powf(num_arg(args, 1)?)))
            })),
            ("max", native("max", |_vm, args| {
                let mut m = num_arg(args, 0)?;
                for a in rest(args) {
                    m = m.max(a.as_num()?);
                }
                one(Value::Num(m))
            })),
            ("min", native("min", |_vm, args| {
                let mut m = num_arg(args, 0)?;
                for a in rest(args) {
                    m = m.min(a.as_num()?);
                }
                one(Value::Num(m))
            })),
            ("random", native("random", |_vm, args| {
                let r = next_random();
                one(Value::Num(match args.len() {
                    0 => r,
                    1 => (r * num_arg(args, 0)?).floor(),
                    _ => {
                        let (lo, hi) = (num_arg(args, 0)?, num_arg(args, 1)?);
                        (r * (hi - lo)).floor() + lo
                    }
                }))
            })),
            ("seed", native("seed", |_vm, args| {
                let seed = num_arg(args, 0)? as u64 | 1;
                SEED.with(|c| c.set(seed));
                Ok(Vec::new())
            })),
        ],
    )
}

fn next_random() -> f64 {
    SEED.with(|c| {
        let mut x = c.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        c.set(x);
        (x >> 11) as f64 / (1u64 << 53) as f64
    })
}

fn string(vm: &mut Vm) -> Rc<RefCell<Table>> {
    module(
        vm,
        "string",
        vec![
            ("len", native("len", |_vm, args| {
                one(Value::Num(str_arg(args, 0)?.len() as f64))
            })),
            // zero based, end exclusive: "hello".slice(1, 3) == "el"
            ("slice", native("slice", |_vm, args| {
                let s = str_arg(args, 0)?;
                let b = s.as_bytes();
                let start = clamp_index(args.get(1), 0.0, b.len());
                let end = clamp_index(args.get(2), b.len() as f64, b.len());
                if start >= end {
                    return one(Value::str(""));
                }
                one(Value::str(String::from_utf8_lossy(&b[start..end])))
            })),
            ("upper", native("upper", |_vm, args| {
                one(Value::str(str_arg(args, 0)?.to_uppercase()))
            })),
            ("lower", native("lower", |_vm, args| {
                one(Value::str(str_arg(args, 0)?.to_lowercase()))
            })),
            ("trim", native("trim", |_vm, args| {
                one(Value::str(str_arg(args, 0)?.trim()))
            })),
            ("repeat", native("repeat", |_vm, args| {
                let s = str_arg(args, 0)?;
                let n = num_arg(args, 1)?.max(0.0) as usize;
                // a script asking for a petabyte of text gets an error, not an
                // allocator abort
                if s.len().saturating_mul(n) > MAX_STRING {
                    return err("repeat: the result would be too large");
                }
                one(Value::str(s.repeat(n)))
            })),
            ("reverse", native("reverse", |_vm, args| {
                one(Value::str(str_arg(args, 0)?.chars().rev().collect::<String>()))
            })),
            ("find", native("find", |_vm, args| {
                let (s, pat) = (str_arg(args, 0)?, str_arg(args, 1)?);
                one(match s.find(&*pat) {
                    Some(i) => Value::Num(i as f64),
                    None => Value::Nil,
                })
            })),
            ("contains", native("contains", |_vm, args| {
                let (s, pat) = (str_arg(args, 0)?, str_arg(args, 1)?);
                one(Value::Bool(s.contains(&*pat)))
            })),
            ("starts_with", native("starts_with", |_vm, args| {
                let (s, pat) = (str_arg(args, 0)?, str_arg(args, 1)?);
                one(Value::Bool(s.starts_with(&*pat)))
            })),
            ("ends_with", native("ends_with", |_vm, args| {
                let (s, pat) = (str_arg(args, 0)?, str_arg(args, 1)?);
                one(Value::Bool(s.ends_with(&*pat)))
            })),
            ("replace", native("replace", |_vm, args| {
                let (s, from, to) = (str_arg(args, 0)?, str_arg(args, 1)?, str_arg(args, 2)?);
                one(Value::str(s.replace(&*from, &to)))
            })),
            ("split", native("split", |_vm, args| {
                let (s, sep) = (str_arg(args, 0)?, str_arg(args, 1)?);
                let mut t = Table::new();
                for part in s.split(&*sep) {
                    t.push(Value::str(part));
                }
                one(Value::table(t))
            })),
            ("chars", native("chars", |_vm, args| {
                let s = str_arg(args, 0)?;
                let mut t = Table::new();
                for c in s.chars() {
                    t.push(Value::str(c.to_string()));
                }
                one(Value::table(t))
            })),
            ("byte", native("byte", |_vm, args| {
                let s = str_arg(args, 0)?;
                let i = num_arg(args, 1).unwrap_or(0.0).max(0.0) as usize;
                one(match s.as_bytes().get(i) {
                    Some(b) => Value::Num(*b as f64),
                    None => Value::Nil,
                })
            })),
            ("format", native("format", |_vm, args| {
                one(Value::str(format_impl(&str_arg(args, 0)?, rest(args))?))
            })),
        ],
    )
}

fn clamp_index(v: Option<&Value>, default: f64, len: usize) -> usize {
    let raw = v.and_then(|x| x.as_num().ok()).unwrap_or(default);
    let raw = if raw < 0.0 { len as f64 + raw } else { raw };
    raw.max(0.0).min(len as f64) as usize
}

/// The most padding or precision a format spec may ask for. Without a cap a
/// script can ask for gigabytes of spaces.
const MAX_FORMAT_WIDTH: usize = 4096;

/// The largest string a builtin will construct: 256MB.
const MAX_STRING: usize = 256 << 20;

/// Rust-flavoured formatting: `{}` placeholders, `{:.2}` precision, `{:x}`,
/// and `{{` for a literal brace.
fn format_impl(fmt: &str, args: &[Value]) -> Res<String> {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    let mut next = 0usize;
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut spec = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    spec.push(c);
                }
                let v = args.get(next).cloned().unwrap_or(Value::Nil);
                next += 1;
                let spec = spec.trim_start_matches(':');
                out.push_str(&match spec {
                    "" => v.to_string(),
                    "x" => format!("{:x}", v.as_num()? as i64),
                    "X" => format!("{:X}", v.as_num()? as i64),
                    "b" => format!("{:b}", v.as_num()? as i64),
                    "e" => format!("{:e}", v.as_num()?),
                    s if s.starts_with('.') => {
                        let p: usize = s[1..]
                            .parse()
                            .map_err(|_| Error(format!("format: bad precision `{s}`")))?;
                        if p > MAX_FORMAT_WIDTH {
                            return err(format!("format: precision {p} is too large"));
                        }
                        format!("{:.*}", p, v.as_num()?)
                    }
                    s => {
                        // a bare width, optionally right aligned with `>`
                        let (right, digits) = match s.strip_prefix('>') {
                            Some(rest) => (true, rest),
                            None => (false, s),
                        };
                        let w: usize = digits
                            .parse()
                            .map_err(|_| Error(format!("format: unsupported spec `{s}`")))?;
                        if w > MAX_FORMAT_WIDTH {
                            return err(format!("format: width {w} is too large"));
                        }
                        let text = v.to_string();
                        if text.len() >= w {
                            text
                        } else if right {
                            format!("{}{}", " ".repeat(w - text.len()), text)
                        } else {
                            format!("{}{}", text, " ".repeat(w - text.len()))
                        }
                    }
                });
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

fn table_lib(vm: &mut Vm) -> Rc<RefCell<Table>> {
    module(
        vm,
        "table",
        vec![
            ("len", native("len", |_vm, args| {
                one(Value::Num(table_arg(args, 0)?.borrow().len() as f64))
            })),
            ("push", native("push", |_vm, args| {
                let t = table_arg(args, 0)?;
                for v in rest(args) {
                    t.borrow_mut().push(v.clone());
                }
                Ok(Vec::new())
            })),
            ("pop", native("pop", |_vm, args| {
                let t = table_arg(args, 0)?;
                let n = t.borrow().len();
                if n == 0 {
                    return one(Value::Nil);
                }
                let mut b = t.borrow_mut();
                let v = b.get_index(n - 1);
                b.set_index(n - 1, Value::Nil);
                one(v)
            })),
            ("insert", native("insert", |_vm, args| {
                let t = table_arg(args, 0)?;
                let pos = num_arg(args, 1)?.max(0.0) as usize;
                let v = arg(args, 2);
                let mut b = t.borrow_mut();
                let n = b.len();
                let mut i = n;
                while i > pos {
                    let prev = b.get_index(i - 1);
                    b.set_index(i, prev);
                    i -= 1;
                }
                b.set_index(pos.min(n), v);
                Ok(Vec::new())
            })),
            ("remove", native("remove", |_vm, args| {
                let t = table_arg(args, 0)?;
                let mut b = t.borrow_mut();
                let n = b.len();
                if n == 0 {
                    return one(Value::Nil);
                }
                let pos = num_arg(args, 1).unwrap_or((n - 1) as f64).max(0.0) as usize;
                if pos >= n {
                    return one(Value::Nil);
                }
                let out = b.get_index(pos);
                for i in pos..n - 1 {
                    let next = b.get_index(i + 1);
                    b.set_index(i, next);
                }
                b.set_index(n - 1, Value::Nil);
                one(out)
            })),
            ("contains", native("contains", |_vm, args| {
                let t = table_arg(args, 0)?;
                let needle = arg(args, 1);
                let b = t.borrow();
                one(Value::Bool(b.keys().iter().any(|k| b.get(k) == needle)))
            })),
            ("join", native("join", |_vm, args| {
                let t = table_arg(args, 0)?;
                let sep = args.get(1).map(|v| v.to_string()).unwrap_or_default();
                let b = t.borrow();
                let parts: Vec<String> = (0..b.len()).map(|i| b.get_index(i).to_string()).collect();
                one(Value::str(parts.join(&sep)))
            })),
            ("keys", native("keys", |_vm, args| {
                let t = table_arg(args, 0)?;
                let mut out = Table::new();
                for k in t.borrow().keys() {
                    out.push(k.to_value());
                }
                one(Value::table(out))
            })),
            ("values", native("values", |_vm, args| {
                let t = table_arg(args, 0)?;
                let b = t.borrow();
                let mut out = Table::new();
                for k in b.keys() {
                    out.push(b.get(&k));
                }
                one(Value::table(out))
            })),
            // `for (k, v) in t.iter()`
            ("iter", native("iter", |_vm, args| {
                let t = table_arg(args, 0)?;
                let keys = t.borrow().keys();
                let i = Cell::new(0usize);
                one(iterator("table iterator", move || {
                    let idx = i.get();
                    let k = keys.get(idx)?.clone();
                    i.set(idx + 1);
                    let v = t.borrow().get(&k);
                    Some(vec![k.to_value(), v])
                }))
            })),
            ("sort", native("sort", |vm, args| {
                let t = table_arg(args, 0)?;
                let cmp = arg(args, 1);
                let mut items: Vec<Value> = {
                    let b = t.borrow();
                    (0..b.len()).map(|i| b.get_index(i)).collect()
                };
                // insertion sort: n is small in scripts, and the comparator can fail
                for i in 1..items.len() {
                    let mut j = i;
                    while j > 0 {
                        let less = match &cmp {
                            Value::Nil => {
                                let (a, b) = (items[j].clone(), items[j - 1].clone());
                                match (&a, &b) {
                                    (Value::Str(x), Value::Str(y)) => x < y,
                                    _ => a.as_num()? < b.as_num()?,
                                }
                            }
                            f => vm
                                .call(f, vec![items[j].clone(), items[j - 1].clone()])?
                                .first()
                                .map(|v| v.truthy())
                                .unwrap_or(false),
                        };
                        if !less {
                            break;
                        }
                        items.swap(j, j - 1);
                        j -= 1;
                    }
                }
                let mut b = t.borrow_mut();
                for (i, v) in items.into_iter().enumerate() {
                    b.set_index(i, v);
                }
                Ok(Vec::new())
            })),
        ],
    )
}

fn os_io(vm: &mut Vm) {
    module(
        vm,
        "os",
        vec![
            ("clock", native("clock", |_vm, _args| {
                one(Value::Num(START.with(|s| s.elapsed().as_secs_f64())))
            })),
            ("time", native("time", |_vm, _args| {
                let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                one(Value::Num(t as f64))
            })),
            ("getenv", native("getenv", |_vm, args| {
                one(match std::env::var(&*str_arg(args, 0)?) {
                    Ok(v) => Value::str(v),
                    Err(_) => Value::Nil,
                })
            })),
            ("exit", native("exit", |_vm, args| {
                std::process::exit(num_arg(args, 0).unwrap_or(0.0) as i32)
            })),
        ],
    );
    module(
        vm,
        "io",
        vec![
            ("write", native("write", |_vm, args| {
                use std::io::Write;
                let s: Vec<String> = args.iter().map(|v| v.to_string()).collect();
                print!("{}", s.concat());
                let _ = std::io::stdout().flush();
                Ok(Vec::new())
            })),
            ("read", native("read", |_vm, _args| {
                let mut line = String::new();
                match std::io::stdin().read_line(&mut line) {
                    Ok(0) => one(Value::Nil),
                    Ok(_) => one(Value::str(line.trim_end_matches('\n'))),
                    Err(e) => err(format!("io::read: {e}")),
                }
            })),
        ],
    );
}

fn ffi_lib(vm: &mut Vm) {
    let process = ffi::this_process().map(Value::Ptr).unwrap_or(Value::Nil);
    module(
        vm,
        "ffi",
        vec![
            ("C", process),
            ("load", native("load", |_vm, args| {
                one(Value::Ptr(ffi::load_library(&str_arg(args, 0)?).map_err(Error)?))
            })),
            ("cdef", native("cdef", |vm, args| {
                // ffi::cdef(lib, "decl") or ffi::cdef("decl") against the process
                let (handle, decl) = match arg(args, 0) {
                    Value::Ptr(p) => (p, str_arg(args, 1)?),
                    Value::Str(s) => match vm.index(&vm.get_global("ffi"), &Value::str("C"))? {
                        Value::Ptr(p) => (p, s),
                        _ => return err("ffi::cdef: no process handle"),
                    },
                    other => {
                        return err(format!(
                            "ffi::cdef: expected a library, got {}",
                            other.type_name()
                        ))
                    }
                };
                let sig = ffi::parse_decl(&decl).map_err(Error)?;
                let addr = ffi::symbol(handle, &sig.name).map_err(Error)?;
                one(cffi::make_callable(sig, addr))
            })),
            ("string", native("string", |_vm, args| match arg(args, 0) {
                Value::Ptr(p) => {
                    // SAFETY: the script asserts this points at a C string.
                    one(Value::str(unsafe { ffi::read_string(p) }))
                }
                Value::Str(s) => one(Value::Str(s)),
                Value::Nil => one(Value::Nil),
                other => err(format!("ffi::string: expected cdata, got {}", other.type_name())),
            })),
        ],
    );
}

fn jit_lib(vm: &mut Vm) {
    module(
        vm,
        "jit",
        vec![
            ("on", native("on", |vm, _args| {
                vm.jit.enabled = true;
                Ok(Vec::new())
            })),
            ("off", native("off", |vm, _args| {
                vm.jit.enabled = false;
                Ok(Vec::new())
            })),
            ("threshold", native("threshold", |vm, args| {
                vm.jit.threshold = num_arg(args, 0)?.max(1.0) as u32;
                Ok(Vec::new())
            })),
            ("status", native("status", |vm, _args| {
                Ok(vec![
                    Value::Bool(vm.jit.enabled),
                    Value::Num(vm.jit.compiled as f64),
                    Value::Num(vm.jit.bailed as f64),
                    match &vm.jit.last_error {
                        Some(e) => Value::str(e),
                        None => Value::Nil,
                    },
                ])
            })),
        ],
    );
}
