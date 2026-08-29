//! FFI: the C side of calling C (and `extern "C"` Rust) from rua.
//!
//! This crate knows nothing about rua values. It parses a small subset of C
//! declarations, resolves symbols, and calls them through libffi; the runtime
//! converts its own values to and from [`CArg`] and [`CRet`].
//!
//! ```
//! let sig = rua_ffi::parse_decl("double cos(double x)").unwrap();
//! assert_eq!(sig.name, "cos");
//! ```

use libffi::middle::{arg, Cif, CodePtr, Ret, Type};
use std::ffi::{CStr, CString};
use std::os::raw::c_void;

/// Anything here fails with a message, never a panic.
pub type Res<T> = Result<T, String>;

fn err<T>(msg: impl Into<String>) -> Res<T> {
    Err(msg.into())
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CType {
    Void,
    I8, U8, I16, U16, I32, U32, I64, U64,
    F32, F64,
    Ptr,
    CStr,
}

impl CType {
    pub(crate) fn ffi(self) -> Type {
        match self {
            CType::Void => Type::void(),
            CType::I8 => Type::i8(),
            CType::U8 => Type::u8(),
            CType::I16 => Type::i16(),
            CType::U16 => Type::u16(),
            CType::I32 => Type::i32(),
            CType::U32 => Type::u32(),
            CType::I64 => Type::i64(),
            CType::U64 => Type::u64(),
            CType::F32 => Type::f32(),
            CType::F64 => Type::f64(),
            CType::Ptr | CType::CStr => Type::pointer(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Signature {
    pub name: String,
    pub ret: CType,
    pub params: Vec<CType>,
}

/// Parse one C function declaration, e.g. `int puts(const char *s)`.
pub fn parse_decl(decl: &str) -> Res<Signature> {
    let toks = ctokens(decl);
    let lparen = toks
        .iter()
        .position(|t| t == "(")
        .ok_or_else(|| format!("`{decl}` is not a function declaration"))?;
    let rparen = toks
        .iter()
        .rposition(|t| t == ")")
        .ok_or_else(|| format!("`{decl}` is missing `)`"))?;
    if lparen == 0 {
        return err("declaration has no name");
    }
    let name = toks[lparen - 1].clone();
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return err(format!("`{name}` is not a valid function name"));
    }
    let ret = parse_type(&toks[..lparen - 1])?;

    let mut params = Vec::new();
    let inner = &toks[lparen + 1..rparen];
    if !(inner.is_empty() || inner == ["void"]) {
        for chunk in inner.split(|t| t == ",") {
            // drop a trailing parameter name if there is one
            let mut c = chunk.to_vec();
            if c.len() > 1 && c.last().map(|t| is_name(t)).unwrap_or(false) && !is_type_word(c.last().unwrap()) {
                c.pop();
            }
            params.push(parse_type(&c)?);
        }
    }
    Ok(Signature { name, ret, params })
}

fn is_name(t: &str) -> bool {
    t.chars().next().map(|c| c.is_alphabetic() || c == '_').unwrap_or(false)
        && t.chars().all(|c| c.is_alphanumeric() || c == '_')
}

fn is_type_word(t: &str) -> bool {
    matches!(
        t,
        "void" | "char" | "short" | "int" | "long" | "float" | "double" | "signed" | "unsigned"
            | "size_t" | "ssize_t" | "bool" | "_Bool" | "int8_t" | "uint8_t" | "int16_t"
            | "uint16_t" | "int32_t" | "uint32_t" | "int64_t" | "uint64_t" | "intptr_t"
            | "uintptr_t" | "const" | "*"
    )
}

fn ctokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '(' | ')' | ',' | '*' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(ch.to_string());
            }
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn parse_type(toks: &[String]) -> Res<CType> {
    let stars = toks.iter().filter(|t| *t == "*").count();
    let words: Vec<&str> = toks
        .iter()
        .map(|s| s.as_str())
        .filter(|t| *t != "*" && *t != "const" && *t != "struct")
        .collect();
    let unsigned = words.contains(&"unsigned");
    let base: Vec<&str> = words.into_iter().filter(|w| *w != "signed" && *w != "unsigned").collect();
    let joined = base.join(" ");
    if stars > 0 {
        return Ok(if joined == "char" { CType::CStr } else { CType::Ptr });
    }
    Ok(match joined.as_str() {
        "void" | "" => CType::Void,
        "char" | "int8_t" => if unsigned { CType::U8 } else { CType::I8 },
        "uint8_t" | "bool" | "_Bool" => CType::U8,
        "short" | "short int" | "int16_t" => if unsigned { CType::U16 } else { CType::I16 },
        "uint16_t" => CType::U16,
        "int" | "int32_t" => if unsigned { CType::U32 } else { CType::I32 },
        "uint32_t" => CType::U32,
        "long" | "long int" | "long long" | "long long int" | "int64_t" | "ssize_t" | "intptr_t" => {
            if unsigned { CType::U64 } else { CType::I64 }
        }
        "uint64_t" | "size_t" | "uintptr_t" => CType::U64,
        "float" => CType::F32,
        "double" | "long double" => CType::F64,
        other => return err(format!("unsupported C type `{other}`")),
    })
}

/// One prepared argument. It owns its storage, so the address libffi is given
/// stays valid for the whole call.
pub enum CArg {
    I(i64),
    U(u64),
    I32(i32),
    U32(u32),
    I16(i16),
    U16(u16),
    I8(i8),
    U8(u8),
    F32(f32),
    F64(f64),
    P(*mut c_void),
    S(CString, *mut c_void),
}

impl CArg {
    /// A number, narrowed to whatever the declaration asked for.
    pub fn num(ty: CType, n: f64) -> Res<CArg> {
        Ok(match ty {
            CType::F64 => CArg::F64(n),
            CType::F32 => CArg::F32(n as f32),
            CType::I8 => CArg::I8(n as i8),
            CType::U8 => CArg::U8(n as u8),
            CType::I16 => CArg::I16(n as i16),
            CType::U16 => CArg::U16(n as u16),
            CType::I32 => CArg::I32(n as i32),
            CType::U32 => CArg::U32(n as u32),
            CType::I64 => CArg::I(n as i64),
            CType::U64 => CArg::U(n as u64),
            CType::Ptr | CType::CStr => CArg::P(n as usize as *mut c_void),
            CType::Void => return err("void is not a valid argument type"),
        })
    }

    pub fn ptr(p: *mut c_void) -> CArg {
        CArg::P(p)
    }

    pub fn null() -> CArg {
        CArg::P(std::ptr::null_mut())
    }

    /// A string, copied into a NUL terminated buffer that lives until the call
    /// returns.
    pub fn string(s: &str) -> Res<CArg> {
        let c = CString::new(s.as_bytes().to_vec())
            .map_err(|_| "string contains a NUL byte".to_string())?;
        let p = c.as_ptr() as *mut c_void;
        Ok(CArg::S(c, p))
    }
}

/// What a C function handed back.
#[derive(Debug)]
pub enum CRet {
    Void,
    Num(f64),
    Ptr(*mut c_void),
    Str(String),
    Null,
}

/// Call `addr` as if it had the signature `sig`.
///
/// # Safety
///
/// `addr` must really be a function with that signature. A wrong declaration
/// is a wrong declaration, exactly as in LuaJIT's ffi.
pub unsafe fn call(sig: &Signature, addr: *mut c_void, args: &[CArg]) -> Res<CRet> {
    if args.len() != sig.params.len() {
        return err(format!(
            "{}: expected {} argument(s), got {}",
            sig.name,
            sig.params.len(),
            args.len()
        ));
    }
    let ffi_args: Vec<_> = args
        .iter()
        .map(|a| match a {
            CArg::I(x) => arg(x),
            CArg::U(x) => arg(x),
            CArg::I32(x) => arg(x),
            CArg::U32(x) => arg(x),
            CArg::I16(x) => arg(x),
            CArg::U16(x) => arg(x),
            CArg::I8(x) => arg(x),
            CArg::U8(x) => arg(x),
            CArg::F32(x) => arg(x),
            CArg::F64(x) => arg(x),
            CArg::P(x) => arg(x),
            CArg::S(_, p) => arg(p),
        })
        .collect();
    let cif = Cif::new(sig.params.iter().map(|t| t.ffi()), sig.ret.ffi());
    let code = CodePtr(addr);
    Ok(match sig.ret {
        CType::Void => {
            cif.call_return_into(code, &ffi_args, Ret::void());
            CRet::Void
        }
        CType::F64 => {
            let mut r = 0f64;
            cif.call_return_into(code, &ffi_args, Ret::new(&mut r));
            CRet::Num(r)
        }
        CType::F32 => {
            let mut r = 0f32;
            cif.call_return_into(code, &ffi_args, Ret::new(&mut r));
            CRet::Num(r as f64)
        }
        CType::Ptr => {
            let mut r: *mut c_void = std::ptr::null_mut();
            cif.call_return_into(code, &ffi_args, Ret::new(&mut r));
            if r.is_null() {
                CRet::Null
            } else {
                CRet::Ptr(r)
            }
        }
        CType::CStr => {
            let mut r: *mut c_void = std::ptr::null_mut();
            cif.call_return_into(code, &ffi_args, Ret::new(&mut r));
            if r.is_null() {
                CRet::Null
            } else {
                CRet::Str(CStr::from_ptr(r as *const i8).to_string_lossy().into_owned())
            }
        }
        // libffi widens every integer return into an ffi_arg slot
        int => {
            let mut r = 0u64;
            cif.call_return_into(code, &ffi_args, Ret::new(&mut r));
            CRet::Num(match int {
                CType::I8 => r as u8 as i8 as f64,
                CType::U8 => r as u8 as f64,
                CType::I16 => r as u16 as i16 as f64,
                CType::U16 => r as u16 as f64,
                CType::I32 => r as u32 as i32 as f64,
                CType::U32 => r as u32 as f64,
                CType::I64 => r as i64 as f64,
                _ => r as f64,
            })
        }
    })
}

/// Read a NUL terminated string from a pointer.
///
/// # Safety
///
/// `p` must point at a NUL terminated string.
pub unsafe fn read_string(p: *mut c_void) -> String {
    CStr::from_ptr(p as *const i8).to_string_lossy().into_owned()
}

/// dlopen a library and leak the handle: symbols stay valid for the process.
pub fn load_library(name: &str) -> Res<*mut c_void> {
    let candidates = if name.contains(".so") || name.contains('/') || name.contains(".dylib") {
        vec![name.to_string()]
    } else {
        vec![
            format!("lib{name}.so"),
            format!("lib{name}.so.6"),
            format!("lib{name}.dylib"),
            name.to_string(),
        ]
    };
    let mut last = String::new();
    for c in &candidates {
        // SAFETY: dlopen runs the library's initialisers; the user asked for it.
        match unsafe { libloading::Library::new(c) } {
            Ok(lib) => {
                let boxed: &'static libloading::Library = Box::leak(Box::new(lib));
                return Ok(boxed as *const _ as *mut c_void);
            }
            Err(e) => last = e.to_string(),
        }
    }
    err(format!("cannot load `{name}`: {last}"))
}

/// The current process, including everything already linked into it.
pub fn this_process() -> Res<*mut c_void> {
    // dlopen(NULL): a handle to our own image, plus everything linked into it.
    let lib = libloading::os::unix::Library::this();
    let lib: libloading::Library = lib.into();
    let boxed: &'static libloading::Library = Box::leak(Box::new(lib));
    Ok(boxed as *const _ as *mut c_void)
}

pub fn symbol(handle: *mut c_void, name: &str) -> Res<*mut c_void> {
    // SAFETY: `handle` is one of our leaked `Library` boxes, valid for the
    // lifetime of the process.
    let lib: &libloading::Library = unsafe { &*(handle as *const libloading::Library) };
    unsafe {
        let sym: libloading::Symbol<*mut c_void> = lib
            .get(name)
            .map_err(|e| format!("symbol `{name}` not found: {e}"))?;
        sym.try_as_raw_ptr()
            .ok_or_else(|| format!("symbol `{name}` has no address"))
    }
}
