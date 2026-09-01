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
    Value::Native(Rc::new(Native::new(name, f)))
}

/// A builtin of one argument: it reads a register and writes a register, with
/// no vector at either end. Most of the standard library is this shape.
fn unary<F>(name: &str, f: F) -> Value
where
    F: Fn(&Value) -> Res<Value> + Clone + 'static,
{
    Value::Native(Rc::new(Native::unary(name, f)))
}

/// A builtin with a two-argument form, keeping its general one.
fn native2<F, G>(name: &str, f: F, fast: G) -> Value
where
    F: Fn(&mut Vm, &[Value]) -> Res<Vec<Value>> + 'static,
    G: Fn(&Value, &Value) -> Res<Value> + 'static,
{
    Value::Native(Rc::new(Native::with_fast2(name, f, fast)))
}

/// A builtin of two arguments, in the same shape.
fn binary<F>(name: &str, f: F) -> Value
where
    F: Fn(&Value, &Value) -> Res<Value> + Clone + 'static,
{
    Value::Native(Rc::new(Native::binary(name, f)))
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

fn str_arg(args: &[Value], i: usize) -> Res<RStr> {
    arg(args, i).as_str()
}

fn table_arg(args: &[Value], i: usize) -> Res<Rc<RefCell<Table>>> {
    match arg(args, i) {
        Value::Table(t) => Ok(t),
        other => err(format!("expected a table, got {}", other.type_name())),
    }
}

fn one(v: Value) -> Res<Vec<Value>> {
    crate::interp::one_value(v)
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
    fs_lib(vm);
    net_lib(vm);
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
    // The names are made once. `type(x)` is a test, not a string operation,
    // and a script that leans on it — an interpreter written in rua, say —
    // should not allocate and hash a fresh string every time it asks.
    let type_names: Vec<Value> = Value::TYPE_NAMES.iter().map(|n| Value::str(n)).collect();
    vm.register_unary("type", move |v| Ok(type_names[v.type_index()].clone()));
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
    vm.register_unary("len", |v| match v {
        Value::Table(t) => Ok(Value::Num(t.borrow().len() as f64)),
        Value::Str(s) => Ok(Value::Num(s.len() as f64)),
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
            t.push(Value::str(&*n));
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
        // beside the file asking, then the working directory, with or without
        // the extension
        let path = RStr::from(vm.resolve_path(&str_arg(args, 0)?));
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
        unary(name, move |v| Ok(Value::Num(f(v.as_num()?))))
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
            ("len", unary("len", |v| Ok(Value::Num(v.as_str()?.len() as f64)))),
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
            ("byte", binary("byte", |s, i| {
                let s = s.as_str()?;
                let i = i.as_num().unwrap_or(0.0).max(0.0) as usize;
                Ok(match s.as_bytes().get(i) {
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
                out.push_str(&format_one(spec.trim_start_matches(':'), &v)?);
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

/// One placeholder: `[[fill]align][width][.precision][kind]`, as Rust writes
/// it — `{:>8}`, `{:<12}`, `{:^5}`, `{:.3}`, `{:>9.2}`, `{:x}`.
///
/// A script's output is mostly a table of numbers beside their names, and
/// lining those up needs a width and a side to pad on. Alignment defaults the
/// way Rust's does: numbers right, everything else left.
fn format_one(spec: &str, v: &Value) -> Res<String> {
    let mut rest = spec;

    // fill and alignment: `>` or `-^` or nothing
    let mut fill = ' ';
    let mut align = None;
    let chars: Vec<char> = rest.chars().collect();
    if chars.len() >= 2 && matches!(chars[1], '<' | '^' | '>') {
        fill = chars[0];
        align = Some(chars[1]);
        rest = &rest[chars[0].len_utf8() + 1..];
    } else if let Some(first) = chars.first() {
        if matches!(first, '<' | '^' | '>') {
            align = Some(*first);
            rest = &rest[1..];
        }
    }

    // `{:08.3}` — a leading zero on the width is a fill, as in Rust
    if align.is_none() && rest.starts_with('0') && rest.len() > 1 {
        fill = '0';
        align = Some('>');
        rest = &rest[1..];
    }

    // width, then precision, then what to render it as
    let width_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    let width: usize = match &rest[..width_end] {
        "" => 0,
        digits => digits
            .parse()
            .map_err(|_| Error(format!("format: bad width in `{spec}`")))?,
    };
    rest = &rest[width_end..];

    let mut precision = None;
    if let Some(after) = rest.strip_prefix('.') {
        let end = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
        precision = Some(
            after[..end]
                .parse::<usize>()
                .map_err(|_| Error(format!("format: bad precision in `{spec}`")))?,
        );
        rest = &after[end..];
    }
    if width > MAX_FORMAT_WIDTH || precision.unwrap_or(0) > MAX_FORMAT_WIDTH {
        return err(format!("format: `{spec}` asks for more room than is sensible"));
    }

    let numeric = matches!(v, Value::Num(_));
    let body = match rest {
        "" => match precision {
            // a precision on a string is how much of it to keep, as in Rust
            Some(p) if !numeric => v.to_string().chars().take(p).collect(),
            Some(p) => format!("{:.*}", p, v.as_num()?),
            None => v.to_string(),
        },
        "x" => format!("{:x}", v.as_num()? as i64),
        "X" => format!("{:X}", v.as_num()? as i64),
        "b" => format!("{:b}", v.as_num()? as i64),
        "o" => format!("{:o}", v.as_num()? as i64),
        "e" => format!("{:e}", v.as_num()?),
        // `f` is what a C or Python habit reaches for, and it means the same
        // thing here as no kind at all
        "f" => format!("{:.*}", precision.unwrap_or(6), v.as_num()?),
        other => return err(format!("format: unsupported spec `{other}` in `{spec}`")),
    };

    let pad = width.saturating_sub(body.chars().count());
    if pad == 0 {
        return Ok(body);
    }
    Ok(match align.unwrap_or(if numeric { '>' } else { '<' }) {
        '>' => format!("{}{}", fill.to_string().repeat(pad), body),
        '^' => {
            let left = pad / 2;
            format!(
                "{}{}{}",
                fill.to_string().repeat(left),
                body,
                fill.to_string().repeat(pad - left)
            )
        }
        _ => format!("{}{}", body, fill.to_string().repeat(pad)),
    })
}

/// A unix time as `YYYY-MM-DD HH:MM:SS`, in UTC.
///
/// UTC because the alternative is the zone database, and a script that wants
/// a stamp on a line of output wants one it can compare, not one that moves
/// twice a year. The calendar arithmetic is Howard Hinnant's: days since the
/// epoch to a civil date, without a table of month lengths.
fn utc_string(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02}")
}

fn table_lib(vm: &mut Vm) -> Rc<RefCell<Table>> {
    module(
        vm,
        "table",
        vec![
            ("len", unary("len", |v| match v {
                Value::Table(t) => Ok(Value::Num(t.borrow().len() as f64)),
                other => err(format!("len: expected a table, got {}", other.type_name())),
            })),
            // `t.push(v)` is the shape that matters; the general form still
            // takes as many values as it is given
            ("push", native2(
                "push",
                |_vm, args| {
                    let t = table_arg(args, 0)?;
                    for v in rest(args) {
                        t.borrow_mut().push(v.clone());
                    }
                    Ok(Vec::new())
                },
                |t, v| match t {
                    Value::Table(t) => {
                        t.borrow_mut().push(v.clone());
                        Ok(Value::Nil)
                    }
                    other => err(format!("push: expected a table, got {}", other.type_name())),
                },
            )),
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
                                    (Value::Str(x), Value::Str(y)) => **x < **y,
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
            // `os::date()` now, or `os::date(t)` for a time from `os::time`
            ("date", native("date", |_vm, args| {
                let t = match args.first() {
                    Some(v) => v.as_num()?,
                    None => SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as f64,
                };
                one(Value::str(utc_string(t as i64)))
            })),
            // `let (code, out, err) = os::run("ls -l")`
            ("run", native("run", |_vm, args| {
                let cmd = str_arg(args, 0)?;
                // through a shell, because the whole point is to write what
                // you would have typed
                let shell = if cfg!(windows) { "cmd" } else { "sh" };
                let flag = if cfg!(windows) { "/C" } else { "-c" };
                match std::process::Command::new(shell).arg(flag).arg(&*cmd).output() {
                    Ok(out) => Ok(vec![
                        Value::Num(out.status.code().unwrap_or(-1) as f64),
                        Value::str(String::from_utf8_lossy(&out.stdout)),
                        Value::str(String::from_utf8_lossy(&out.stderr)),
                    ]),
                    Err(e) => err(format!("os::run {cmd}: {e}")),
                }
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
            ("read_all", native("read_all", |_vm, _args| {
                use std::io::Read;
                let mut text = String::new();
                match std::io::stdin().read_to_string(&mut text) {
                    Ok(_) => one(Value::str(text)),
                    Err(e) => err(format!("io::read_all: {e}")),
                }
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

/// Files. `io` is the terminal — this is the disk.
///
/// Whole files rather than handles: a script reaches for a file to read it, to
/// write it, or to walk its lines, and a handle is machinery in the way of all
/// three. Anything that fails says what it was doing and to which path, since
/// a script that cannot open a file wants to print that, not a number.
fn fs_lib(vm: &mut Vm) {
    module(
        vm,
        "fs",
        vec![
            ("read", unary("read", |p| {
                let p = p.as_str()?;
                match std::fs::read_to_string(&*p) {
                    Ok(text) => Ok(Value::str(text)),
                    Err(e) => err(format!("fs::read {p}: {e}")),
                }
            })),
            ("lines", unary("lines", |p| {
                let p = p.as_str()?;
                match std::fs::read_to_string(&*p) {
                    Ok(text) => {
                        let mut t = Table::new();
                        // a trailing newline ends the last line, it does not
                        // start an empty one
                        for line in text.strip_suffix('\n').unwrap_or(&text).split('\n') {
                            t.push(Value::str(line.strip_suffix('\r').unwrap_or(line)));
                        }
                        if text.is_empty() {
                            t = Table::new();
                        }
                        Ok(Value::table(t))
                    }
                    Err(e) => err(format!("fs::lines {p}: {e}")),
                }
            })),
            ("write", binary("write", |p, text| {
                let p = p.as_str()?;
                match std::fs::write(&*p, text.to_string()) {
                    Ok(()) => Ok(Value::Nil),
                    Err(e) => err(format!("fs::write {p}: {e}")),
                }
            })),
            ("append", binary("append", |p, text| {
                use std::io::Write;
                let p = p.as_str()?;
                let opened = std::fs::OpenOptions::new().create(true).append(true).open(&*p);
                match opened.and_then(|mut f| f.write_all(text.to_string().as_bytes())) {
                    Ok(()) => Ok(Value::Nil),
                    Err(e) => err(format!("fs::append {p}: {e}")),
                }
            })),
            ("exists", unary("exists", |p| {
                Ok(Value::Bool(std::path::Path::new(&*p.as_str()?).exists()))
            })),
            ("is_dir", unary("is_dir", |p| {
                Ok(Value::Bool(std::path::Path::new(&*p.as_str()?).is_dir()))
            })),
            ("size", unary("size", |p| {
                let p = p.as_str()?;
                match std::fs::metadata(&*p) {
                    Ok(m) => Ok(Value::Num(m.len() as f64)),
                    Err(e) => err(format!("fs::size {p}: {e}")),
                }
            })),
            ("mkdir", unary("mkdir", |p| {
                let p = p.as_str()?;
                // the parents too, since a script asking for `out/logs` wants
                // `out` as well and nobody has ever wanted the other answer
                match std::fs::create_dir_all(&*p) {
                    Ok(()) => Ok(Value::Nil),
                    Err(e) => err(format!("fs::mkdir {p}: {e}")),
                }
            })),
            ("rename", binary("rename", |from, to| {
                let (from, to) = (from.as_str()?, to.as_str()?);
                match std::fs::rename(&*from, &*to) {
                    Ok(()) => Ok(Value::Nil),
                    Err(e) => err(format!("fs::rename {from} -> {to}: {e}")),
                }
            })),
            ("remove", unary("remove", |p| {
                let p = p.as_str()?;
                match std::fs::remove_file(&*p) {
                    Ok(()) => Ok(Value::Nil),
                    Err(e) => err(format!("fs::remove {p}: {e}")),
                }
            })),
            ("list", unary("list", |p| {
                let p = p.as_str()?;
                let entries = std::fs::read_dir(&*p)
                    .map_err(|e| Error(format!("fs::list {p}: {e}")))?;
                let mut names: Vec<String> = Vec::new();
                for e in entries {
                    let e = e.map_err(|e| Error(format!("fs::list {p}: {e}")))?;
                    names.push(e.file_name().to_string_lossy().into_owned());
                }
                // a directory has no order of its own; give the script one
                names.sort();
                let mut t = Table::new();
                for n in names {
                    t.push(Value::str(n));
                }
                Ok(Value::table(t))
            })),
        ],
    );
}

/// How many sockets a script may have open at once, and how many times one
/// slot may be reused. Both fit in a handle a `f64` holds exactly.
const SOCKET_SLOTS: u64 = 1 << 26;

thread_local! {
    /// The sockets a script has open, by handle.
    ///
    /// A socket is a number rather than an object because rua has no type to
    /// hang one on: the runtime owns it, and closing is explicit rather than
    /// something that happens when a number goes out of scope.
    ///
    /// A closed slot is reused, or a server that accepts a million
    /// connections keeps a million dead ones — but the handle carries the
    /// generation the slot was at, so a handle held past its close is an
    /// error rather than whoever is using that slot now.
    static SOCKETS: RefCell<Sockets> = const { RefCell::new(Sockets::new()) };
}

struct Sockets {
    slots: Vec<Slot>,
    free: Vec<usize>,
}

struct Slot {
    generation: u64,
    sock: Option<Sock>,
}

impl Sockets {
    const fn new() -> Sockets {
        Sockets { slots: Vec::new(), free: Vec::new() }
    }

    fn insert(&mut self, sock: Sock) -> Res<Value> {
        let i = match self.free.pop() {
            Some(i) => {
                self.slots[i].sock = Some(sock);
                i
            }
            None => {
                if self.slots.len() as u64 >= SOCKET_SLOTS {
                    return err("net: too many sockets open at once");
                }
                self.slots.push(Slot { generation: 0, sock: Some(sock) });
                self.slots.len() - 1
            }
        };
        let handle = self.slots[i].generation * SOCKET_SLOTS + i as u64 + 1;
        Ok(Value::Num(handle as f64))
    }

    fn at(&mut self, handle: &Value) -> Option<&mut Sock> {
        let h = handle.as_num().ok()?;
        if h < 1.0 || h.fract() != 0.0 {
            return None;
        }
        let h = h as u64 - 1;
        let slot = self.slots.get_mut((h % SOCKET_SLOTS) as usize)?;
        // the generation is what tells a live handle from one that named this
        // slot before it was closed and handed out again
        (slot.generation == h / SOCKET_SLOTS).then(|| slot.sock.as_mut())?
    }

    fn close(&mut self, handle: &Value) -> bool {
        let Some(h) = handle.as_num().ok().filter(|h| *h >= 1.0) else { return false };
        let h = h as u64 - 1;
        let i = (h % SOCKET_SLOTS) as usize;
        let Some(slot) = self.slots.get_mut(i) else { return false };
        if slot.generation != h / SOCKET_SLOTS || slot.sock.is_none() {
            return false;
        }
        slot.sock = None;
        slot.generation += 1;
        // a slot whose generations are spent is retired rather than wrapped,
        // since wrapping would make an ancient handle valid again
        if slot.generation < SOCKET_SLOTS {
            self.free.push(i);
        }
        true
    }
}

enum Sock {
    /// Buffered, so that reading a line is a line and not a guess.
    Stream(std::io::BufReader<std::net::TcpStream>),
    Listener(std::net::TcpListener),
}

fn put_sock(s: Sock) -> Res<Value> {
    SOCKETS.with(|all| all.borrow_mut().insert(s))
}

/// Do something with an open socket, by handle.
fn with_sock<T>(h: &Value, what: &str, f: impl FnOnce(&mut Sock) -> Res<T>) -> Res<T> {
    SOCKETS.with(|all| match all.borrow_mut().at(h) {
        Some(sock) => f(sock),
        None => err(format!("net::{what}: not an open socket")),
    })
}

fn stream_of<'a>(s: &'a mut Sock, what: &str) -> Res<&'a mut std::io::BufReader<std::net::TcpStream>> {
    match s {
        Sock::Stream(s) => Ok(s),
        Sock::Listener(_) => err(format!("net::{what}: that is a listener, not a connection")),
    }
}

/// TCP, as much of it as a script needs: connect, listen, accept, read,
/// write, close.
fn net_lib(vm: &mut Vm) {
    use std::io::{BufRead, Read, Write};
    module(
        vm,
        "net",
        vec![
            ("connect", unary("connect", |addr| {
                let addr = addr.as_str()?;
                match std::net::TcpStream::connect(&*addr) {
                    Ok(s) => put_sock(Sock::Stream(std::io::BufReader::new(s))),
                    Err(e) => err(format!("net::connect {addr}: {e}")),
                }
            })),
            ("listen", unary("listen", |addr| {
                let addr = addr.as_str()?;
                match std::net::TcpListener::bind(&*addr) {
                    Ok(l) => put_sock(Sock::Listener(l)),
                    Err(e) => err(format!("net::listen {addr}: {e}")),
                }
            })),
            // blocks until someone connects
            ("accept", unary("accept", |h| {
                let stream = with_sock(h, "accept", |s| match s {
                    Sock::Listener(l) => match l.accept() {
                        Ok((s, _)) => Ok(s),
                        Err(e) => err(format!("net::accept: {e}")),
                    },
                    Sock::Stream(_) => err("net::accept: that is a connection, not a listener"),
                })?;
                put_sock(Sock::Stream(std::io::BufReader::new(stream)))
            })),
            ("write", binary("write", |h, text| {
                let text = text.to_string();
                with_sock(h, "write", |s| {
                    let s = stream_of(s, "write")?;
                    match s.get_mut().write_all(text.as_bytes()).and_then(|()| s.get_mut().flush()) {
                        Ok(()) => Ok(Value::Num(text.len() as f64)),
                        Err(e) => err(format!("net::write: {e}")),
                    }
                })
            })),
            // one line, without its newline; nil when the peer is done
            ("read_line", unary("read_line", |h| {
                with_sock(h, "read_line", |s| {
                    let s = stream_of(s, "read_line")?;
                    let mut line = String::new();
                    match s.read_line(&mut line) {
                        Ok(0) => Ok(Value::Nil),
                        Ok(_) => Ok(Value::str(line.trim_end_matches(['\n', '\r']))),
                        Err(e) => err(format!("net::read_line: {e}")),
                    }
                })
            })),
            // everything the peer sends until it closes
            ("read", unary("read", |h| {
                with_sock(h, "read", |s| {
                    let s = stream_of(s, "read")?;
                    let mut buf = Vec::new();
                    match s.read_to_end(&mut buf) {
                        Ok(_) => Ok(Value::str(String::from_utf8_lossy(&buf))),
                        Err(e) => err(format!("net::read: {e}")),
                    }
                })
            })),
            ("close", unary("close", |h| {
                if SOCKETS.with(|all| all.borrow_mut().close(h)) {
                    Ok(Value::Nil)
                } else {
                    err("net::close: not an open socket")
                }
            })),
            // a read that waits forever is a script that hangs forever
            ("timeout", binary("timeout", |h, secs| {
                let secs = secs.as_num()?;
                with_sock(h, "timeout", |s| {
                    let d = if secs <= 0.0 {
                        None
                    } else {
                        Some(std::time::Duration::from_secs_f64(secs))
                    };
                    let s = stream_of(s, "timeout")?;
                    s.get_ref()
                        .set_read_timeout(d)
                        .and_then(|()| s.get_ref().set_write_timeout(d))
                        .map_err(|e| Error(format!("net::timeout: {e}")))?;
                    Ok(Value::Nil)
                })
            })),
            ("address", unary("address", |h| {
                with_sock(h, "address", |s| {
                    let a = match s {
                        Sock::Stream(s) => s.get_ref().peer_addr(),
                        Sock::Listener(l) => l.local_addr(),
                    };
                    match a {
                        Ok(a) => Ok(Value::str(a.to_string())),
                        Err(e) => err(format!("net::address: {e}")),
                    }
                })
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
