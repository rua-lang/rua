//! C ABI. Link against the cdylib and include `rua.h`:
//!
//! ```c
//! rua_State *S = rua_new();
//! rua_eval(S, "return 6 * 7");
//! printf("%g\n", rua_result_number(S));
//! rua_close(S);
//! ```
//!
//! Every function here is safe to call from C with the documented contract:
//! pointers must be non-NULL (except where noted) and strings NUL terminated.

use rua_core::{Native, Res, Value, Vm};
use std::ffi::{c_char, c_double, c_int, CStr, CString};
use std::rc::Rc;

pub struct RuaState {
    vm: Vm,
    last_error: Option<CString>,
    last_string: Option<CString>,
    results: Vec<Value>,
}

/// A C function callable from rua: takes an array of numbers, returns a number.
pub type RuaNumFn = extern "C" fn(args: *const c_double, n: c_int) -> c_double;

fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

fn state<'a>(p: *mut RuaState) -> Option<&'a mut RuaState> {
    if p.is_null() {
        None
    } else {
        Some(unsafe { &mut *p })
    }
}

/// Create a VM with the standard library. Free it with `rua_close`.
#[no_mangle]
pub extern "C" fn rua_new() -> *mut RuaState {
    Box::into_raw(Box::new(RuaState {
        vm: Vm::new(),
        last_error: None,
        last_string: None,
        results: Vec::new(),
    }))
}

/// Destroy a VM created by `rua_new`. NULL is ignored.
#[no_mangle]
pub extern "C" fn rua_close(s: *mut RuaState) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s) });
    }
}

fn finish(s: &mut RuaState, r: Res<Vec<Value>>) -> c_int {
    match r {
        Ok(vals) => {
            s.results = vals;
            s.last_error = None;
            0
        }
        Err(e) => {
            s.results.clear();
            s.last_error = CString::new(e.to_string()).ok();
            -1
        }
    }
}

/// Run source text. Returns 0 on success, -1 on error (see `rua_error`).
#[no_mangle]
pub extern "C" fn rua_eval(s: *mut RuaState, src: *const c_char) -> c_int {
    let Some(s) = state(s) else { return -1 };
    let Some(src) = cstr(src) else { return finish(s, Err("rua_eval: bad source pointer".into())) };
    let r = s.vm.eval(src);
    finish(s, r)
}

/// Run a file. Returns 0 on success, -1 on error.
#[no_mangle]
pub extern "C" fn rua_dofile(s: *mut RuaState, path: *const c_char) -> c_int {
    let Some(s) = state(s) else { return -1 };
    let Some(path) = cstr(path) else { return finish(s, Err("rua_dofile: bad path".into())) };
    let r = s.vm.eval_file(path);
    finish(s, r)
}

/// The last error message, or NULL. Valid until the next rua call on `s`.
#[no_mangle]
pub extern "C" fn rua_error(s: *mut RuaState) -> *const c_char {
    match state(s).and_then(|s| s.last_error.as_ref()) {
        Some(e) => e.as_ptr(),
        None => std::ptr::null(),
    }
}

/// Number of values returned by the last `rua_eval`/`rua_call`.
#[no_mangle]
pub extern "C" fn rua_result_count(s: *mut RuaState) -> c_int {
    state(s).map(|s| s.results.len() as c_int).unwrap_or(0)
}

/// Result `i` as a number (0 when absent or not numeric).
#[no_mangle]
pub extern "C" fn rua_result_number(s: *mut RuaState, i: c_int) -> c_double {
    state(s)
        .and_then(|s| s.results.get(i.max(0) as usize).cloned())
        .and_then(|v| v.as_num().ok())
        .unwrap_or(0.0)
}

/// Result `i` as a string. Valid until the next rua call on `s`.
#[no_mangle]
pub extern "C" fn rua_result_string(s: *mut RuaState, i: c_int) -> *const c_char {
    let Some(s) = state(s) else { return std::ptr::null() };
    let v = match s.results.get(i.max(0) as usize) {
        Some(v) => v.to_string(),
        None => return std::ptr::null(),
    };
    s.last_string = CString::new(v).ok();
    match &s.last_string {
        Some(c) => c.as_ptr(),
        None => std::ptr::null(),
    }
}

/// Set a global to a number.
#[no_mangle]
pub extern "C" fn rua_set_number(s: *mut RuaState, name: *const c_char, v: c_double) {
    if let (Some(s), Some(name)) = (state(s), cstr(name)) {
        s.vm.set_global(name, Value::Num(v));
    }
}

/// Set a global to a string.
#[no_mangle]
pub extern "C" fn rua_set_string(s: *mut RuaState, name: *const c_char, v: *const c_char) {
    if let (Some(s), Some(name), Some(v)) = (state(s), cstr(name), cstr(v)) {
        s.vm.set_global(name, Value::str(v));
    }
}

/// Read a global as a number (0 when absent).
#[no_mangle]
pub extern "C" fn rua_get_number(s: *mut RuaState, name: *const c_char) -> c_double {
    match (state(s), cstr(name)) {
        (Some(s), Some(name)) => s.vm.get_global(name).as_num().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Expose a C function to scripts as a global taking and returning numbers.
#[no_mangle]
pub extern "C" fn rua_register(s: *mut RuaState, name: *const c_char, f: RuaNumFn) {
    let (Some(s), Some(name)) = (state(s), cstr(name)) else { return };
    let owned = name.to_string();
    let native = Native {
        name: owned.clone(),
        f: Box::new(move |_vm, args| {
            let nums: Res<Vec<c_double>> = args.iter().map(|a| a.as_num()).collect();
            let nums = nums?;
            Ok(vec![Value::Num(f(nums.as_ptr(), nums.len() as c_int))])
        }),
    };
    s.vm.set_global(&owned, Value::Native(Rc::new(native)));
}

/// Call a global function with `n` numeric arguments; result in `out`.
/// Returns 0 on success, -1 on error.
#[no_mangle]
pub extern "C" fn rua_call(
    s: *mut RuaState,
    name: *const c_char,
    args: *const c_double,
    n: c_int,
    out: *mut c_double,
) -> c_int {
    let Some(s) = state(s) else { return -1 };
    let Some(name) = cstr(name) else { return finish(s, Err("rua_call: bad name".into())) };
    let n = n.max(0) as usize;
    let slice: &[c_double] = if n == 0 || args.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(args, n) }
    };
    let f = s.vm.get_global(name);
    let argv: Vec<Value> = slice.iter().map(|x| Value::Num(*x)).collect();
    let r = s.vm.call(&f, argv);
    let ok = finish(s, r);
    if ok == 0 && !out.is_null() {
        let v = s.results.first().and_then(|v| v.as_num().ok()).unwrap_or(0.0);
        unsafe { *out = v };
    }
    ok
}

/// Turn the JIT on (1) or off (0).
#[no_mangle]
pub extern "C" fn rua_jit(s: *mut RuaState, on: c_int) {
    if let Some(s) = state(s) {
        s.vm.jit.enabled = on != 0;
    }
}
