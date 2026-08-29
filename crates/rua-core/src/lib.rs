//! The rua runtime: values, tables, the interpreter, and the standard library.
//!
//! ```
//! let mut vm = rua_core::Vm::new();
//! assert_eq!(vm.eval("6 * 7").unwrap()[0], rua_core::Value::Num(42.0));
//! ```

pub mod bytecode;
pub mod cffi;
pub mod compile;
pub mod hash;
pub mod interp;
pub mod stdlib;
pub mod value;
pub mod vm;

pub use interp::{MethodTable, Vm};
pub use value::{CellRef, Error, Function, Key, Native, Res, Table, Value};
