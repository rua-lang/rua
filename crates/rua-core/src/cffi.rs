//! The bridge between rua values and `rua_ffi`'s C side.

use crate::interp::Vm;
use crate::value::*;
use rua_ffi::{CArg, CRet, CType, Signature};
use std::os::raw::c_void;
use std::rc::Rc;

fn to_arg(ty: CType, v: &Value) -> Res<CArg> {
    Ok(match ty {
        CType::Ptr | CType::CStr => match v {
            Value::Nil => CArg::null(),
            Value::Ptr(p) => CArg::ptr(*p),
            Value::Str(s) => CArg::string(s).map_err(Error)?,
            other => {
                return err(format!("ffi: cannot pass a {} as a pointer", other.type_name()))
            }
        },
        CType::U8 if matches!(v, Value::Bool(_)) => {
            CArg::num(ty, if v.truthy() { 1.0 } else { 0.0 }).map_err(Error)?
        }
        _ => CArg::num(ty, v.as_num()?).map_err(Error)?,
    })
}

fn from_ret(r: CRet) -> Vec<Value> {
    match r {
        CRet::Void => Vec::new(),
        CRet::Num(n) => vec![Value::Num(n)],
        CRet::Ptr(p) => vec![Value::Ptr(p)],
        CRet::Str(s) => vec![Value::str(s)],
        CRet::Null => vec![Value::Nil],
    }
}

/// Wrap a C function as a rua value that scripts can call.
pub fn make_callable(sig: Signature, addr: *mut c_void) -> Value {
    let name = sig.name.clone();
    let f = move |_vm: &mut Vm, args: &[Value]| -> Res<Vec<Value>> {
        let prepared: Res<Vec<CArg>> = sig
            .params
            .iter()
            .zip(args.iter())
            .map(|(t, v)| to_arg(*t, v))
            .collect();
        let prepared = prepared?;
        if prepared.len() != sig.params.len() {
            return err(format!(
                "{}: expected {} argument(s), got {}",
                sig.name,
                sig.params.len(),
                args.len()
            ));
        }
        // SAFETY: the address came from dlsym for the symbol the script
        // declared, and the signature is the one it declared for it.
        let out = unsafe { rua_ffi::call(&sig, addr, &prepared) }.map_err(Error)?;
        Ok(from_ret(out))
    };
    Value::Native(Rc::new(Native::new(name, f)))
}
