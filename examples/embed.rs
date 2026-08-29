//! Embedding rua in a Rust program: `cargo run --example embed`.

use rua::{Value, Vm};

fn main() -> Result<(), rua::Error> {
    let mut vm = Vm::new();

    // expose a Rust closure to scripts (natives are `Fn`, so own state in a Cell)
    let calls = std::cell::Cell::new(0usize);
    vm.register("hypot", move |_vm, args| {
        calls.set(calls.get() + 1);
        let (a, b) = (args[0].as_num()?, args[1].as_num()?);
        Ok(vec![Value::Num((a * a + b * b).sqrt())])
    });

    // push a value in
    vm.set_global("scale", Value::Num(3.0));

    // run a script
    vm.eval(
        r#"
        fn area(r) {
            math::pi * r * r * scale
        }
        print("hypot(3,4) =", hypot(3, 4));
    "#,
    )?;

    // call back into rua
    let area = vm.get_global("area");
    let out = vm.call(&area, vec![Value::Num(2.0)])?;
    println!("area(2)     = {}", out[0]);

    // and read a value back out
    vm.eval("answer = 6 * 7;")?;
    println!("answer      = {}", vm.get_global("answer"));

    // errors are values, not panics
    match vm.eval("1 + [1, 2]") {
        Ok(_) => println!("unexpected"),
        Err(e) => println!("error       = {e}"),
    }
    Ok(())
}
