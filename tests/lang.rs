use rua::{Value, Vm};

fn n(src: &str) -> f64 {
    let out = Vm::new().eval(src).expect("eval failed");
    out[0].as_num().expect("not a number")
}

fn s(src: &str) -> String {
    Vm::new().eval(src).expect("eval failed")[0].to_string()
}

#[test]
fn arithmetic_and_precedence() {
    assert_eq!(n("1 + 2 * 3"), 7.0);
    assert_eq!(n("(1 + 2) * 3"), 9.0);
    assert_eq!(n("-2 * 2"), -4.0);
    assert_eq!(n("7 % 3"), 1.0);
    assert_eq!(n("-7 % 3"), 2.0); // floor modulo
    assert_eq!(n("10 / 4"), 2.5);
    assert_eq!(n("math::pow(2, 10)"), 1024.0);
    assert_eq!(n("2.0.max(7.0)"), 7.0);
}

#[test]
fn blocks_have_values() {
    assert_eq!(n("{ let a = 2; a * 3 }"), 6.0);
    assert_eq!(n("if true { 1 } else { 2 }"), 1.0);
    assert_eq!(n("if false { 1 } else if true { 2 } else { 3 }"), 2.0);
    assert_eq!(s("let x = 5; if x > 3 { \"big\" } else { \"small\" }"), "big");
    // a trailing `;` discards the value
    assert!(Vm::new().eval("let x = 1;").unwrap().is_empty());
}

#[test]
fn strings() {
    assert_eq!(s(r#""a" + "b" + 1"#), "ab1");
    assert_eq!(s(r#""rua".upper()"#), "RUA");
    assert_eq!(s(r#""hello".slice(1, 3)"#), "el");
    assert_eq!(s(r#""hello".slice(-3)"#), "llo");
    assert_eq!(n(r#""hello".len()"#), 5.0);
    assert_eq!(s(r#"format("{}/{}/{:.2}", 7, "x", 1.5)"#), "7/x/1.50");
    assert_eq!(s(r#""{} {}".format("hi", 2)"#), "hi 2");
    assert_eq!(n(r#""hello".find("ll")"#), 2.0);
    assert_eq!(s(r#""a,b,c".split(",").join("-")"#), "a-b-c");
    assert_eq!(s(r#"" pad ".trim()"#), "pad");
}

#[test]
fn arrays_are_zero_based() {
    assert_eq!(n("[1, 2, 3].len()"), 3.0);
    assert_eq!(n("[10, 20, 30][0]"), 10.0);
    assert_eq!(n("let t = []; t.push(1); t.push(2); t.len()"), 2.0);
    assert_eq!(s("let t = [3, 1, 2]; t.sort(); t.join(\"\")"), "123");
    assert_eq!(n("let t = [1, 2, 3]; t.remove(0); t[0]"), 2.0);
    assert_eq!(n("let t = [1, 2, 3]; t.pop()"), 3.0);
    assert_eq!(s("[1, 2, 3].join(\",\")"), "1,2,3");
}

#[test]
fn maps() {
    assert_eq!(n("#{ a: 5 }.a"), 5.0);
    assert_eq!(n("let m = #{}; m.x = 7; m.x"), 7.0);
    assert_eq!(n("let m = #{ a: 1, b: 2 }; m.keys().len()"), 2.0);
    assert_eq!(
        n("let m = #{ a: 1, b: 2 }; let s = 0; for (k, v) in m.iter() { s += v; } s"),
        3.0
    );
}

#[test]
fn methods_on_maps() {
    let src = r#"
        fn point(x, y) {
            let p = #{ x: x, y: y };
            p.len = |self| math::sqrt(self.x * self.x + self.y * self.y);
            p
        }
        point(3, 4).len()
    "#;
    assert_eq!(n(src), 5.0);
}

#[test]
fn closures_capture() {
    let src = r#"
        fn counter() {
            let n = 0;
            || { n += 1; n }
        }
        let c = counter();
        c(); c();
        c()
    "#;
    assert_eq!(n(src), 3.0);
}

#[test]
fn multiple_returns_destructure() {
    let mut vm = Vm::new();
    let out = vm.eval("fn f() { return 1, 2, 3; } f()").unwrap();
    assert_eq!(out.len(), 3);
    assert_eq!(out[2], Value::Num(3.0));
    assert_eq!(n("fn f() { return 4, 5; } let (a, b) = f(); b"), 5.0);
}

#[test]
fn control_flow() {
    assert_eq!(n("let s = 0; for i in 1..=10 { s += i; } s"), 55.0);
    assert_eq!(n("let s = 0; for i in 0..5 { s += i; } s"), 10.0);
    assert_eq!(n("let i = 0; while i < 5 { i += 1; } i"), 5.0);
    assert_eq!(n("let i = 0; loop { i += 1; if i > 2 { break; } } i"), 3.0);
    assert_eq!(n("let s = 0; for i in 0..10 { if i % 2 == 0 { continue; } s += i; } s"), 25.0);
    assert_eq!(n("fn f() { for i in 0..10 { if i == 3 { return i; } } } f()"), 3.0);
    assert_eq!(s("1 < 2 && \"yes\" || \"no\""), "yes");
    assert_eq!(s("nil || \"fallback\""), "fallback");
    assert_eq!(s("!false"), "true");
}

#[test]
fn errors_are_values() {
    let mut vm = Vm::new();
    let out = vm.eval(r#"try(|| error("boom"))"#).unwrap();
    assert_eq!(out[0], Value::Bool(false));
    // the message carries the line it came from
    assert!(out[1].to_string().contains("boom"), "got {}", out[1]);
    assert!(out[1].to_string().starts_with("line 1:"), "got {}", out[1]);
    assert!(vm.eval("nosuch()").is_err());
    assert!(vm.eval("let let = 3").is_err());
    assert_eq!(n("let (ok, v) = try(|| 1); if ok { v } else { 0 }"), 1.0);
}

#[test]
fn rust_can_register_functions() {
    let mut vm = Vm::new();
    vm.register("triple", |_vm, args| Ok(vec![Value::Num(args[0].as_num()? * 3.0)]));
    assert_eq!(vm.eval("triple(14)").unwrap()[0], Value::Num(42.0));

    let f = vm.eval("|a, b| a * b").unwrap().remove(0);
    let out = vm.call(&f, vec![Value::Num(6.0), Value::Num(7.0)]).unwrap();
    assert_eq!(out[0], Value::Num(42.0));
}

/// The JIT must not change results: same program, compiled and interpreted.
#[test]
fn jit_matches_the_interpreter() {
    let src = r#"
        fn collatz(n) {
            let steps = 0;
            while n > 1 {
                if n % 2 == 0 { n = n / 2; } else { n = 3 * n + 1; }
                steps += 1;
            }
            steps
        }
        let total = 0;
        for i in 1..=300 { total += collatz(i); }
        total
    "#;

    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let a = interp.eval(src).unwrap()[0].as_num().unwrap();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 5;
    let b = jitted.eval(src).unwrap()[0].as_num().unwrap();

    assert_eq!(a, b);
    assert!(jitted.jit.compiled >= 1, "expected collatz to be compiled");
}

#[test]
fn jit_handles_recursion_loops_and_math() {
    let mut vm = Vm::new();
    vm.jit.threshold = 3;
    let out = vm
        .eval(
            r#"
        fn fib(n) {
            if n < 2 { return n; }
            fib(n - 1) + fib(n - 2)
        }
        fn hyp(a, b) { math::sqrt(a * a + b * b) }
        fn tri(n) {
            let s = 0;
            for i in 0..=n { s += i; }
            s
        }
        let h = 0;
        for i in 0..20 { h = hyp(3, 4); }
        let t = 0;
        for i in 0..20 { t = tri(10); }
        return fib(20), h, t;
    "#,
        )
        .unwrap();
    assert_eq!(out[0], Value::Num(6765.0));
    assert_eq!(out[1], Value::Num(5.0));
    assert_eq!(out[2], Value::Num(55.0));
    assert!(vm.jit.compiled >= 3, "fib, hyp and tri should all compile");
}

/// Non-numeric functions must stay on the interpreter, silently.
#[test]
fn jit_bails_out_without_breaking_anything() {
    let mut vm = Vm::new();
    vm.jit.threshold = 2;
    let out = vm
        .eval(
            r#"
        fn join(a, b) { a + b }        // compiles as numeric, but is called on strings
        fn label(t) { t.join("-") }    // cannot compile at all: a string method
        let s = "";
        let last = "";
        for i in 0..10 {
            s = join("x", s);
            last = label([i, i + 1]);
        }
        return s, last;
    "#,
        )
        .unwrap();
    assert_eq!(out[0].to_string(), "xxxxxxxxxx");
    assert_eq!(out[1].to_string(), "9-10");
    assert!(vm.jit.bailed >= 1, "`label` should have been rejected by the JIT");
}

/// A function that can fall through returns nil, which the numeric JIT cannot
/// express — it must stay interpreted.
#[test]
fn jit_leaves_nil_returning_functions_alone() {
    let mut vm = Vm::new();
    vm.jit.threshold = 2;
    let out = vm
        .eval(
            r#"
        fn maybe(n) {
            if n > 100 { return n; }
        }
        let last = 1;
        for i in 0..10 { last = maybe(i); }
        type(last)
    "#,
        )
        .unwrap();
    assert_eq!(out[0].to_string(), "nil");
}

#[test]
fn ffi_calls_into_libc_and_libm() {
    let mut vm = Vm::new();
    let out = vm
        .eval(
            r#"
        let cos = ffi::cdef(ffi::load("m"), "double cos(double x)");
        let strlen = ffi::cdef("size_t strlen(const char *s)");
        return cos(0), strlen("hello");
    "#,
        )
        .unwrap();
    assert_eq!(out[0], Value::Num(1.0));
    assert_eq!(out[1], Value::Num(5.0));
}

#[test]
fn ffi_declarations_parse() {
    use rua::ffi::{parse_decl, CType};
    let sig = parse_decl("const char *getenv(const char *name)").unwrap();
    assert_eq!(sig.name, "getenv");
    assert_eq!(sig.ret, CType::CStr);
    assert_eq!(sig.params, vec![CType::CStr]);

    let sig = parse_decl("void *malloc(size_t)").unwrap();
    assert_eq!(sig.ret, CType::Ptr);
    assert_eq!(sig.params, vec![CType::U64]);

    let sig = parse_decl("int rand(void)").unwrap();
    assert_eq!(sig.params, Vec::new());
    assert_eq!(sig.ret, CType::I32);

    assert!(parse_decl("not a declaration").is_err());
}

/// A top level `fn` is a global: that is how an embedder finds it, and how a
/// recursive call finds itself.
#[test]
fn top_level_functions_are_globals() {
    let mut vm = Vm::new();
    vm.jit.threshold = 3;
    vm.eval("fn fib(n) { if n < 2 { return n; } fib(n - 1) + fib(n - 2) }").unwrap();

    let f = vm.get_global("fib");
    assert!(matches!(f, Value::Func(_)), "fib should be a global function");
    for _ in 0..5 {
        let out = vm.call(&f, vec![Value::Num(20.0)]).unwrap();
        assert_eq!(out[0], Value::Num(6765.0));
    }
    assert!(vm.jit.compiled >= 1, "recursion through a global should still compile");

    // a `fn` inside a block stays local
    let out = vm
        .eval("let outer = { fn inner(x) { x * 2 } inner(21) }; return outer, type(inner);")
        .unwrap();
    assert_eq!(out[0], Value::Num(42.0));
    assert_eq!(out[1].to_string(), "nil");
}

#[test]
fn errors_say_where_they_happened() {
    let mut vm = Vm::new();
    let e = vm
        .eval("let x = 1;\nlet y = 2;\nfn boom(v) { v.nope() }\nboom(x);")
        .unwrap_err();
    assert_eq!(e.line, 3, "got {e}");
    assert_eq!(e.where_.as_deref(), Some("boom"), "got {e}");
    assert!(e.to_string().starts_with("line 3:"), "got {e}");
}

#[test]
fn require_runs_a_file_once() {
    let dir = std::env::temp_dir().join("rua-require-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("m.rua");
    std::fs::write(&path, "counter = (counter || 0) + 1;\n#{ n: counter }\n").unwrap();

    let mut vm = Vm::new();
    let src = format!(
        r#"
        let a = require("{p}");
        let b = require("{p}");
        return a.n, b.n, a == b, counter;
    "#,
        p = path.display()
    );
    let out = vm.eval(&src).unwrap();
    assert_eq!(out[0], Value::Num(1.0));
    assert_eq!(out[1], Value::Num(1.0));
    assert_eq!(out[2], Value::Bool(true), "the same module value comes back");
    assert_eq!(out[3], Value::Num(1.0), "the file runs once");
}

/// Compiled functions call each other's machine code directly, and that has to
/// come undone if the callee is replaced.
#[test]
fn compiled_functions_call_each_other() {
    let mut vm = Vm::new();
    vm.jit.threshold = 2;
    let out = vm
        .eval(
            r#"
        fn square(x) { x * x }
        fn hyp(a, b) { math::sqrt(square(a) + square(b)) }
        let last = 0;
        for i in 0..10 { last = hyp(3, 4); }
        last
    "#,
        )
        .unwrap();
    assert_eq!(out[0], Value::Num(5.0));
    assert!(vm.jit.compiled >= 2, "square and hyp should both compile");

    // replacing `square` must invalidate the code that inlined it
    let out = vm
        .eval(
            r#"
        square = |x| x * x * 10;
        let after = 0;
        for i in 0..10 { after = hyp(3, 4); }
        after
    "#,
        )
        .unwrap();
    // sqrt(9*10 + 16*10), not the old sqrt(9 + 16)
    assert_eq!(out[0], Value::Num(250f64.sqrt()), "hyp must see the new square");
}

/// Compiling a function compiles the helpers it calls, so that a whole cluster
/// of numeric functions ends up as machine code no matter what order it warms
/// up in.
#[test]
fn callees_are_compiled_before_their_callers() {
    let mut vm = Vm::new();
    vm.jit.threshold = 1;
    let out = vm
        .eval(
            r#"
        fn sq(x) { x * x }
        fn dist(x1, y1, x2, y2) { math::sqrt(sq(x2 - x1) + sq(y2 - y1)) }
        fn total(n) {
            let s = 0;
            let i = 0;
            while i < n { s += dist(0, 0, i, i); i += 1; }
            s
        }
        total(100)
    "#,
        )
        .unwrap();
    let expected: f64 = (0..100).map(|i| ((2 * i * i) as f64).sqrt()).sum();
    assert!((out[0].as_num().unwrap() - expected).abs() < 1e-6);
    assert_eq!(vm.jit.compiled, 3, "sq, dist and total should all compile");
    assert_eq!(vm.jit.bailed, 0);
}

/// A loop inside a function that is only ever called once still gets compiled:
/// the interpreter hands control over mid-loop and picks the locals back up.
#[test]
fn hot_loops_compile_while_they_run() {
    let src = r#"
        fn work(n) {
            let s = 0;
            let i = 0;
            while i < n {
                if i % 3 == 0 { s += i * 2; } else { s -= 1; }
                i += 1;
            }
            s
        }
        work(200000)
    "#;

    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let expected = interp.eval(src).unwrap()[0].as_num().unwrap();

    let mut jitted = Vm::new();
    // the function is called once, so only loop compilation can kick in
    jitted.jit.threshold = 1000;
    
    let got = jitted.eval(src).unwrap()[0].as_num().unwrap();

    assert_eq!(expected, got);
    assert!(jitted.jit.compiled >= 1, "the hot loop should have compiled");
}

#[test]
fn compiled_loops_respect_break_and_nesting() {
    let src = r#"
        fn f() {
            let total = 0;
            for i in 0..200000 {
                let j = 0;
                while j < 50 {
                    total += 1;
                    j += 1;
                    if j > 40 { break; }
                }
                if i > 150000 { break; }
            }
            total
        }
        f()
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let expected = interp.eval(src).unwrap()[0].as_num().unwrap();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 1000;
    assert_eq!(jitted.eval(src).unwrap()[0].as_num().unwrap(), expected);
}

#[test]
fn match_expressions() {
    assert_eq!(s(r#"match 0 { 0 => "zero", _ => "other" }"#), "zero");
    assert_eq!(s(r#"match 7 { 1 | 2 | 3 => "small", _ => "other" }"#), "other");
    assert_eq!(s(r#"match -4 { x if x < 0 => "negative", _ => "positive" }"#), "negative");
    assert_eq!(n("match 9 { x => x * 2 }"), 18.0);
    assert_eq!(s(r#"match "b" { "a" => "first", "b" => "second", _ => "?" }"#), "second");
    // no arm matches: the match is nil
    assert_eq!(s("type(match 5 { 1 => 1 })"), "nil");
    // patterns bind per arm and do not leak
    assert_eq!(n("let x = 1; match 42 { x => x } + x"), 43.0);
}

#[test]
fn match_compiles_when_it_is_numeric() {
    let src = r#"
        fn classify(n) {
            match n % 3 {
                0 => 10,
                1 => 20,
                x => 30 + x,
            }
        }
        let total = 0;
        for i in 0..30 { total += classify(i); }
        total
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let expected = interp.eval(src).unwrap()[0].as_num().unwrap();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 3;
    assert_eq!(jitted.eval(src).unwrap()[0].as_num().unwrap(), expected);
    assert!(jitted.jit.compiled >= 1, "a numeric match should compile");
}

#[test]
fn string_interpolation() {
    assert_eq!(s(r#"let name = "rua"; "hi {name}""#), "hi rua");
    assert_eq!(s(r#"let a = 2; "{a} + {a} = {a + a}""#), "2 + 2 = 4");
    assert_eq!(s(r#""{math::pi:.2}""#), "3.14");
    assert_eq!(s(r#"let t = [1, 2]; "n={t.len()}""#), "n=2");
    assert_eq!(s(r#""{{literal}}""#), "{literal}");
    assert_eq!(s(r#""no braces here""#), "no braces here");
    assert!(Vm::new().eval(r#""{unclosed"#).is_err());
}

#[test]
fn errors_carry_a_traceback() {
    let mut vm = Vm::new();
    let e = vm
        .eval("fn inner(v) { v.missing() }\nfn outer(v) { inner(v) }\nouter([1]);")
        .unwrap_err();
    let trace = e.traceback();
    assert_eq!(trace.len(), 2, "got {trace:?}");
    // innermost first, the way a backtrace reads
    assert!(trace[0].contains("inner called from outer, line 2"), "got {trace:?}");
    assert!(trace[1].contains("outer called from top level, line 3"), "got {trace:?}");
}

#[test]
fn tables_iterate_directly() {
    assert_eq!(n("let s = 0; for v in [1, 2, 3] { s += v; } s"), 6.0);
    assert_eq!(n("let s = 0; for (i, v) in [4, 5].iter() { s += i * v; } s"), 5.0);
    assert_eq!(s("let m = #{ a: 1, b: 2 }; m.keys().join(\",\")"), "a,b");
    assert_eq!(n("let m = #{ a: 1, b: 2 }; let s = 0; for v in m { s += v; } s"), 3.0);
}

#[test]
fn semicolons_are_optional() {
    let src = r#"
        let a = 1
        let b = 2
        fn add(x, y) {
            let s = x + y
            s
        }
        add(a, b)
    "#;
    assert_eq!(n(src), 3.0);
    // but a trailing `;` still discards the value, as in Rust
    assert_eq!(s("fn f() { 1; } type(f())"), "nil");
}

/// Compiled code reads tables through a cached view of their array part. That
/// view has to disappear the moment the table changes.
#[test]
fn compiled_table_reads_see_writes() {
    let mut vm = Vm::new();
    vm.jit.threshold = 2;
    let out = vm
        .eval(
            r#"
        fn total(a) { let s = 0; for i in 0..a.len() { s += a[i] } s }
        let xs = [1, 2, 3]
        let first = 0
        for i in 0..5 { first = total(xs) }      // warm it up so it compiles

        xs.push(10)
        let after_push = total(xs)
        xs[0] = 100
        let after_write = total(xs)
        return first, after_push, after_write
    "#,
        )
        .unwrap();
    assert_eq!(out[0], Value::Num(6.0));
    assert_eq!(out[1], Value::Num(16.0), "a push must be visible");
    assert_eq!(out[2], Value::Num(115.0), "an element write must be visible");
    assert!(vm.jit.compiled >= 1, "the function should have compiled");
}

/// A table that is not a dense run of numbers traps out of compiled code, and
/// the interpreter finishes the call instead — with the same answer.
#[test]
fn compiled_table_reads_deoptimise() {
    let src = r#"
        fn total(a) { let s = 0; for i in 0..a.len() { s += a[i] } s }
        let xs = [1, 2, 3]
        let warm = 0
        for i in 0..5 { warm = total(xs) }
        xs.push("!")                             // no longer all numbers
        total(xs)
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let expected = interp.eval(src).unwrap()[0].to_string();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 2;
    assert_eq!(jitted.eval(src).unwrap()[0].to_string(), expected);
}

#[test]
fn compiled_table_reads_match_the_interpreter() {
    let src = r#"
        fn dot(a, b) {
            let s = 0
            for i in 0..a.len() { s += a[i] * b[i] }
            s
        }
        fn norm(a) { math::sqrt(dot(a, a)) }
        let xs = []
        let ys = []
        for i in 0..50 { xs.push(i % 7); ys.push(i % 11) }
        let d = 0
        let n = 0
        for k in 0..20 { d = dot(xs, ys); n = norm(xs) }
        return d, n
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let a = interp.eval(src).unwrap();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 3;
    let b = jitted.eval(src).unwrap();
    assert_eq!(a[0], b[0]);
    assert_eq!(a[1], b[1]);
    assert!(jitted.jit.compiled >= 2, "dot and norm should compile");
}

/// An index that is out of range, or not a whole number, traps rather than
/// inventing a value.
#[test]
fn compiled_index_bounds_are_checked() {
    let src = r#"
        fn at(a, i) { a[i] }
        let xs = [10, 20, 30]
        let warm = 0
        for k in 0..5 { warm = at(xs, 1) }
        return at(xs, 1), type(at(xs, 9)), type(at(xs, 0.5))
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let a = interp.eval(src).unwrap();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 2;
    let b = jitted.eval(src).unwrap();
    assert_eq!(b[0], Value::Num(20.0));
    assert_eq!(a[1].to_string(), b[1].to_string(), "out of range");
    assert_eq!(a[2].to_string(), b[2].to_string(), "fractional index");
}
