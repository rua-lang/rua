//! rua — a small Rust-shaped scripting language.
//!
//! Rust syntax, dynamic types, and three ways in:
//!
//! * **Rust** — [`Vm::eval`] runs source, [`Vm::register`] exposes Rust
//!   closures to scripts.
//! * **C** — the `rua_*` functions in the `rua-capi` crate (see
//!   `include/rua.h`) embed the VM in a C program.
//! * **FFI** — scripts call out to C, or to `extern "C"` Rust, with
//!   `ffi::cdef`.
//!
//! Hot functions in the numeric subset of the language are compiled to native
//! code: rua lowers them to Rust with `quote`, checks the result with `syn`,
//! and hands it to `rustc -O` (see [`jit`]).
//!
//! This crate is a facade over the workspace:
//!
//! | crate | what it holds |
//! |---|---|
//! | [`syntax`] | lexer, AST, parser, resolver |
//! | [`core`] | values, tables, interpreter, standard library |
//! | [`jit`] | AST to Rust to `rustc` to machine code |
//! | [`ffi`] | C declaration parsing and libffi dispatch |
//! | `rua-capi` | the `rua_*` C ABI, built as `librua.so` |
//!
//! ```
//! let mut vm = rua::Vm::new();
//! vm.register("double", |_vm, args| {
//!     Ok(vec![rua::Value::Num(args[0].as_num()? * 2.0)])
//! });
//! let out = vm.eval("double(21)").unwrap();
//! assert_eq!(out[0], rua::Value::Num(42.0));
//! ```

pub use rua_core as core;
pub use rua_ffi as ffi;
pub use rua_jit as jit;
pub use rua_syntax as syntax;

pub use rua_core::{Error, Key, Native, Res, Table, Value, Vm};
pub use rua_syntax::{parser, resolve};

/// Parse and run a chunk in a fresh VM with the standard library.
pub fn eval(src: &str) -> Res<Vec<Value>> {
    Vm::new().eval(src)
}
