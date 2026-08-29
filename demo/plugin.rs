//! A Rust cdylib that rua scripts load through `ffi.cdef`.
//!
//! Rust's own ABI is unstable, so — exactly like every other language that
//! links to Rust — the boundary is `extern "C"`.
//!
//! Build: rustc -O --crate-type cdylib demo/plugin.rs -o demo/libruaplugin.so

use std::cell::RefCell;
use std::ffi::{c_char, c_double, c_int, CStr, CString};

#[no_mangle]
pub extern "C" fn rust_add(a: c_double, b: c_double) -> c_double {
    a + b
}

#[no_mangle]
pub extern "C" fn rust_fib(n: c_int) -> c_int {
    let (mut a, mut b) = (0i64, 1i64);
    for _ in 0..n {
        (a, b) = (b, a + b);
    }
    a as c_int
}

/// Takes a C string from rua, hands one back. The returned pointer is valid
/// until the next call on this thread — the usual C convention.
#[no_mangle]
pub extern "C" fn rust_shout(s: *const c_char) -> *const c_char {
    thread_local! {
        static OUT: RefCell<CString> = RefCell::new(CString::default());
    }
    let input = if s.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(s) }.to_string_lossy().to_uppercase()
    };
    OUT.with(|out| {
        *out.borrow_mut() = CString::new(input).unwrap_or_default();
        out.borrow().as_ptr()
    })
}
