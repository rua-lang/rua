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
    assert_eq!(n("let x = 1; let m = match 42 { x => x }; m + x"), 43.0);
    // a block-shaped expression in statement position is a statement, as in
    // Rust: this is `match ...` followed by `+ x`, which is not an expression
    assert!(Vm::new().eval("let x = 1; match 42 { x => x } + x").is_err());
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

/// Compiled code may append to a table. That is only sound because every read
/// it makes is provably in range, so it can never trap *after* writing.
#[test]
fn compiled_pushes_match_the_interpreter() {
    let src = r#"
        fn build(out, n) {
            let i = 0
            while i < n { out.push(i * 2); i += 1 }
            out.len()
        }
        fn double(src, dst) {
            for i in 0..src.len() { dst.push(src[i] * 3) }
            dst.len()
        }
        let a = []
        for k in 0..5 { build(a, 20) }
        let b = []
        for k in 0..5 { double(a, b) }
        return a.len(), b.len(), a.join(","), b.join(",")
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let want = interp.eval(src).unwrap();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 2;
    let got = jitted.eval(src).unwrap();
    for i in 0..4 {
        assert_eq!(want[i].to_string(), got[i].to_string(), "result {i}");
    }
    assert!(jitted.jit.compiled >= 2, "both writers should compile");
}

/// Reading and writing the *same* table would leave compiled code looking at a
/// stale view, so that call has to stay interpreted.
#[test]
fn compiled_writes_bail_on_aliasing() {
    let src = r#"
        fn double(src, dst) {
            for i in 0..src.len() { dst.push(src[i] * 2) }
            dst.len()
        }
        let warm_a = [1, 2, 3]
        let warm_b = []
        for k in 0..5 { double(warm_a, warm_b) }      // compiles here
        let c = [1, 2]
        double(c, c)                                   // same table both ways
        c.join(",")
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let want = interp.eval(src).unwrap()[0].to_string();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 2;
    assert_eq!(jitted.eval(src).unwrap()[0].to_string(), want);
}

/// A push of something that is not a number is outside the numeric subset, so
/// the function stays interpreted and still works.
#[test]
fn compiled_writes_reject_non_numbers() {
    let mut vm = Vm::new();
    vm.jit.threshold = 2;
    let out = vm
        .eval(
            r#"
        fn label(out, n) {
            let i = 0
            while i < n { out.push("v{i}"); i += 1 }
            out.len()
        }
        let t = []
        for k in 0..5 { label(t, 2) }
        return t.len(), t.join(",")
    "#,
        )
        .unwrap();
    assert_eq!(out[0], Value::Num(10.0));
    assert_eq!(out[1].to_string(), "v0,v1,v0,v1,v0,v1,v0,v1,v0,v1");
}

// ---- regressions found by the review pass ---------------------------------

/// A call that spreads three or more values used to overwrite the callee's own
/// return registers while copying them out.
#[test]
fn spreading_a_multi_return_keeps_its_values() {
    let mut vm = Vm::new();
    let out = vm.eval("fn three() { return 7, 8, 9 } fn pass() { three() } return pass()").unwrap();
    assert_eq!(out.len(), 3);
    assert_eq!(
        out.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","),
        "7,8,9"
    );
    // and forwarding into a destructuring binding fills every slot
    assert_eq!(
        n("fn g() { return 1, 2, 3 } fn f() { return g() } let (a, b, c) = f(); a * 100 + b * 10 + c"),
        123.0
    );
}

/// The JIT proved `t[i]` in range from `for i in 0..t.len()`. If the body moves
/// `i`, that proof is void — it used to emit an unchecked read.
#[test]
fn a_moved_loop_variable_voids_the_range_proof() {
    let src = r#"
        fn sum(a) {
            let s = 0
            for i in 0..a.len() { i = 5; s += a[i] }
            s
        }
        let xs = [1, 2, 3]
        xs[5] = 9                     // in the hash part: a.len() is still 3
        let r = 0
        for k in 0..60 { r = sum(xs) }
        r
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let want = interp.eval(src).unwrap()[0].as_num().unwrap();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 5;
    assert_eq!(jitted.eval(src).unwrap()[0].as_num().unwrap(), want);
}

/// rua is Lua-shaped: every number is true, including 0. Compiled code cannot
/// tell a boolean from the number that would encode it, so it must not try.
#[test]
fn zero_is_true_in_compiled_code_too() {
    let src = r#"
        fn f(x) { if x { 1 } else { 2 } }
        fn g(x) { if !x { 1 } else { 2 } }
        let a = 0
        let b = 0
        for i in 0..60 { a = f(0); b = g(0) }
        return a, b
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let want = interp.eval(src).unwrap();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 5;
    let got = jitted.eval(src).unwrap();
    assert_eq!(want[0], got[0]);
    assert_eq!(want[1], got[1]);
}

/// `continue` in a counted loop still has to run the increment.
#[test]
fn continue_in_a_compiled_counted_loop_terminates() {
    let src = r#"
        fn odd_sum(n) {
            let s = 0
            for i in 0..n { if i % 2 == 0 { continue } s += i }
            s
        }
        let r = 0
        for k in 0..60 { r = odd_sum(10) }
        r
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let want = interp.eval(src).unwrap()[0].as_num().unwrap();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 5;
    assert_eq!(jitted.eval(src).unwrap()[0].as_num().unwrap(), want);
    assert_eq!(want, 25.0);
}

/// Reassigning a global has to reach the code of callers two levels up, which
/// are jumping straight at the old machine code.
#[test]
fn invalidation_reaches_indirect_callers() {
    let src = r#"
        fn k(x) { x * 2 }
        fn g(x) { k(x) + 1 }
        fn h(x) { g(x) + 1 }
        let r = 0
        for i in 0..80 { r = h(1) }
        k = |x| x * 100
        for i in 0..80 { r = g(1) }        // recompiles g
        return h(1), g(1), k(1)
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let want = interp.eval(src).unwrap();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 20;
    let got = jitted.eval(src).unwrap();
    for i in 0..3 {
        assert_eq!(want[i], got[i], "value {i}");
    }
}

/// A constant index inside a compiled loop is proved once, on the way in.
/// Until the loop emitted that proof it indexed the view without it, and read
/// past the end of a table that turned out to be shorter.
#[test]
fn a_hot_loop_proves_its_constant_indexes_on_entry() {
    let src = r#"
        let t = [7.0]
        let s = 0.0
        let i = 0
        while i < 400 {
            if i == 399 { s += t[3] } else { s += t[0] }
            i += 1
        }
        s
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    assert!(interp.eval(src).is_err(), "the interpreter refuses a nil");

    let mut jitted = Vm::new();
    jitted.jit.threshold = 5;
    assert!(jitted.eval(src).is_err(), "and compiled code must refuse it too");
}

/// `for k in 0..n` indexing `m[k][0]` is proved against the number of element
/// views, once, on entry — so a bound longer than the array traps there
/// rather than walking off the end of it.
#[test]
fn a_proven_outer_index_is_checked_against_the_views_it_uses() {
    let src = r#"
        fn sum(m, n) {
            let s = 0.0
            for k in 0..n { s += m[k][0] }
            s
        }
        let m = []
        for i in 0..4 { let r = []; r.push(i + 1.0); r.push(0.0); m.push(r) }
        let t = 0.0
        for i in 0..400 { t += sum(m, 4) }
        let (ok, why) = try(|| sum(m, 6))
        return t, ok
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let want = interp.eval(src).unwrap();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 5;
    let got = jitted.eval(src).unwrap();
    assert_eq!(want[1], Value::Bool(false), "the interpreter should refuse");
    assert_eq!(got[0], want[0], "the sums agree");
    assert_eq!(got[1], want[1], "and so does the refusal");
}

/// Compiled recursion is real machine recursion, so it has to stop where the
/// interpreter stops instead of running the process out of stack.
#[test]
fn compiled_recursion_respects_the_depth_limit() {
    let src = r#"
        fn down(n) { if n < 1 { return 0 } down(n - 1) + 1 }
        let warm = 0
        for k in 0..60 { warm = down(5) }
        let (ok, why) = try(|| down(100000))
        return ok, why
    "#;
    let mut interp = Vm::new();
    interp.jit.enabled = false;
    let want = interp.eval(src).unwrap();

    let mut jitted = Vm::new();
    jitted.jit.threshold = 5;
    let got = jitted.eval(src).unwrap();
    assert_eq!(want[0], Value::Bool(false), "the interpreter should refuse");
    assert_eq!(got[0], want[0], "and so should compiled code");
    assert!(got[1].to_string().contains("stack overflow"), "got {}", got[1]);
}

#[test]
fn builtins_do_not_panic_on_odd_arguments() {
    let mut vm = Vm::new();
    // no format string: nothing to format, and above all no panic
    assert_eq!(vm.eval("format()").unwrap()[0].to_string(), "nil");
    assert!(vm.eval(r#"format("{:.999999999}", 1)"#).is_err());
    assert!(vm.eval(r#""ab".repeat(100000000000)"#).is_err());
    assert!(vm.eval("math::max()").is_err());
    assert_eq!(vm.eval("let t = []; t.push(); t.len()").unwrap()[0], Value::Num(0.0));
    // a nil written into an array literal keeps its place
    assert_eq!(vm.eval("[1, nil, 2].len()").unwrap()[0], Value::Num(3.0));
}

#[test]
fn the_parser_rejects_what_it_cannot_mean() {
    assert!(Vm::new().eval("break").is_err(), "`break` outside a loop");
    assert!(Vm::new().eval("fn f() { continue }").is_err(), "`continue` outside a loop");
    // a block-shaped expression in statement position does not swallow the
    // next line
    assert_eq!(n("let x = 3\nif x > 2 { 1 }\n(x)\n"), 3.0);
    // and a local called `format` does not capture an interpolation's call
    assert_eq!(s(r#"let format = 7; "{math::pi:.2} {format}""#), "3.14 7");
    // deep nesting is an error, not a crash
    let deep = format!("let x = {}1{}", "(".repeat(500), ")".repeat(500));
    assert!(Vm::new().eval(&deep).is_err());
}

/// `t[k] += v` evaluates the place once, as `t[k] = t[k] + v` written by hand
/// would not.
#[test]
fn compound_assignment_evaluates_its_place_once() {
    let mut vm = Vm::new();
    let out = vm
        .eval(
            r#"
        let calls = 0
        fn key() { calls = calls + 1; 0 }
        let t = [10]
        t[key()] += 5
        return t[0], calls
    "#,
        )
        .unwrap();
    assert_eq!(out[0], Value::Num(15.0));
    assert_eq!(out[1], Value::Num(1.0), "the index expression ran twice");
}

#[test]
fn a_failed_require_can_be_retried() {
    let dir = std::env::temp_dir().join("rua-require-retry");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("m.rua");
    std::fs::write(&path, "error(\"nope\")\n").unwrap();

    let mut vm = Vm::new();
    let src = format!(r#"let (ok, _) = try(|| require("{p}")); ok"#, p = path.display());
    assert_eq!(vm.eval(&src).unwrap()[0], Value::Bool(false));

    // the module is fixed, and loading it again must actually load it
    std::fs::write(&path, "#{ n: 7 }\n").unwrap();
    let src = format!(r#"require("{p}").n"#, p = path.display());
    assert_eq!(vm.eval(&src).unwrap()[0], Value::Num(7.0));
}

/// A table or function used as a key stays alive and comes back as itself —
/// it used to be stored as a bare address, which both dangled and handed
/// scripts a forged `cdata` pointer out of `keys()`.
#[test]
fn object_keys_keep_their_identity() {
    let mut vm = Vm::new();
    let out = vm
        .eval(
            r#"
        let m = #{}
        let k = [1, 2]
        m[k] = "value"
        let back = m.keys()[0]
        return type(back), m[k], m[back], back.len()
    "#,
        )
        .unwrap();
    assert_eq!(out[0].to_string(), "table", "a table key comes back a table");
    assert_eq!(out[1].to_string(), "value");
    assert_eq!(out[2].to_string(), "value", "the key still finds its entry");
    assert_eq!(out[3], Value::Num(2.0));
}

/// Conditions compile straight to branches now, through `&&`, `||` and `!`.
/// That rewrite has to preserve short-circuiting, rua's truthiness (every
/// number is true, including 0) and the value forms of the same operators.
#[test]
fn conditions_keep_their_semantics() {
    // short circuit: the right side must not run
    assert_eq!(
        s(r#"
        let calls = []
        fn t(name, v) { calls.push(name); v }
        if t("a", false) && t("b", true) { }
        if t("c", true) || t("d", true) { }
        calls.join(",")
    "#),
        "a,c"
    );
    // truthiness in condition position
    assert_eq!(s(r#"if 0 { "t" } else { "f" }"#), "t");
    assert_eq!(s(r#"if "" { "t" } else { "f" }"#), "t");
    assert_eq!(s(r#"if nil { "t" } else { "f" }"#), "f");
    assert_eq!(s(r#"if !nil { "t" } else { "f" }"#), "t");
    assert_eq!(s(r#"if !0 { "t" } else { "f" }"#), "f");
    // chains, with and without constants
    assert_eq!(s(r#"let n = 5; if n > 1 && n < 10 && n != 7 { "in" } else { "out" }"#), "in");
    assert_eq!(s(r#"let n = 5; if !(n > 1) || n == 5 { "y" } else { "n" }"#), "y");
    assert_eq!(s(r#"let n = 5; if n < 1 || n > 9 { "y" } else { "n" }"#), "n");
    // as values they still yield an operand, not a boolean
    assert_eq!(s(r#"false || "fallback""#), "fallback");
    assert_eq!(s(r#"0 && "zero is true""#), "zero is true");
    // in a while condition and a match guard
    assert_eq!(n("let i = 0; while i < 3 && true { i += 1 } i"), 3.0);
    assert_eq!(s(r#"match 4 { x if x > 1 && x < 9 => "mid", _ => "out" }"#), "mid");
    // comparison against a nil constant
    assert_eq!(s(r#"let t = #{}; if t.missing == nil { "absent" } else { "present" }"#), "absent");
}

/// Calls no longer recurse in Rust, so how deep a rua program may recurse is a
/// policy limit rather than a property of the host stack — and it holds in an
/// unoptimised build, where frames are far larger.
#[test]
fn deep_recursion_is_bounded_by_policy_not_the_host_stack() {
    let mut vm = Vm::new();
    vm.jit.enabled = false;
    assert_eq!(
        vm.eval("fn down(n) { if n < 1 { return 0 } down(n - 1) + 1 } down(900)").unwrap()[0],
        Value::Num(900.0)
    );
    // and past the limit it is still a catchable error, not a crash
    let out = vm
        .eval("fn down(n) { if n < 1 { return 0 } down(n - 1) + 1 } try(|| down(50000))")
        .unwrap();
    assert_eq!(out[0], Value::Bool(false));
    assert!(out[1].to_string().contains("stack overflow"), "got {}", out[1]);
}

/// An error raised several calls deep has to leave the interpreter's own state
/// — register stack, frame stack, depth counter — exactly as it found it, or
/// the next call runs in a corrupted window.
#[test]
fn an_error_unwinds_every_suspended_call() {
    let mut vm = Vm::new();
    let out = vm
        .eval(
            r#"
        fn deep(n) { if n < 1 { error("bottom") } deep(n - 1) }
        let (ok, why) = try(|| deep(50))
        // the VM must still work perfectly afterwards
        fn sum(n) { let s = 0; for i in 0..n { s += i } s }
        return ok, why, sum(100), deep
    "#,
        )
        .unwrap();
    assert_eq!(out[0], Value::Bool(false));
    assert!(out[1].to_string().contains("bottom"), "got {}", out[1]);
    assert_eq!(out[2], Value::Num(4950.0), "the VM still runs correctly after unwinding");
    // repeated failures must not leak frames either
    for _ in 0..100 {
        let r = vm.eval("let (ok, _) = try(|| deep(30)); ok").unwrap();
        assert_eq!(r[0], Value::Bool(false));
    }
    assert_eq!(vm.eval("sum(10)").unwrap()[0], Value::Num(45.0));
}

/// A method call whose argument spreads another call's results reads its
/// receiver from the same register the argument list is collected from. When
/// arguments started being moved rather than cloned, that emptied the receiver
/// before it was read — and every test here still passed, because none of them
/// wrote `t.push(f())` where `f` returns more than one value.
#[test]
fn method_call_spreading_a_call_keeps_its_receiver() {
    assert_eq!(
        s(r#"
        fn two() { return "a", "b" }
        let t = []
        t.push(two())
        t.push(two())
        t.join(",")
        "#),
        "a,b,a,b"
    );
    // the receiver as a local, with the spread argument built from it
    assert_eq!(
        n(r#"
        fn parts(s) { return s, s }
        let t = []
        let word = "x"
        t.push(parts(word))
        t.len()
        "#),
        2.0
    );
}

/// Compiled code writes through a view of a table's numbers, and a trap throws
/// that view away so the interpreter can run the call again from the start.
/// If the writes survived a trap, the re-run would apply them twice — which is
/// exactly what this checks, since the value written depends on what is there.
#[test]
fn a_trap_undoes_what_compiled_code_wrote() {
    let mut vm = Vm::new();
    vm.jit.threshold = 2;
    let out = vm
        .eval(
            r#"
        fn step(t, k) {
            t[0] = t[0] + 1
            t[k] = 0
            t[0]
        }
        let t = [0, 0, 0, 0]
        // in range: compiled after the first couple of calls
        for i in 0..20 { step(t, 1) }
        // out of range: the compiled code traps part way, having already
        // written t[0], and the interpreter runs the whole call again
        step(t, 99)
        return t[0];
        "#,
        )
        .unwrap();
    assert!(vm.jit.compiled >= 1, "`step` should have been compiled");
    assert_eq!(out[0].as_num().unwrap(), 21.0, "t[0] counted once per call");
}

/// A loop hot enough to be compiled is taken over *at its back edge*, with the
/// counter part way through — so the counter is live there, whatever an
/// analysis of the body says about it being assigned before it is read. Get
/// that wrong and the loop starts again from zero, which is a wrong answer
/// rather than a crash: this counts what it should have counted.
#[test]
fn a_loop_taken_over_mid_flight_keeps_its_counter() {
    let mut vm = Vm::new();
    let out = vm
        .eval(
            r#"
        let total = 0
        for i in 0..300000 { total += i }
        return total;
        "#,
        )
        .unwrap();
    assert_eq!(out[0].as_num().unwrap(), 44999850000.0);
}

/// A nested function's own parameters are not captures. They shared a
/// namespace with the enclosing frame's locals, so `fn advance(bodies, dt)`
/// beside a `let bodies` put that array in a heap cell — which slows every
/// read of it and stops the compiler taking any loop that touches it.
#[test]
fn a_parameter_is_not_a_capture_of_the_same_name() {
    let mut vm = Vm::new();
    let out = vm
        .eval(
            r#"
        let k = 2
        fn scale(v) { v * k }          // this one really does capture k
        fn first(a) { a[0] }           // `a` here is its own, not the outer one
        let a = [scale(3), 2, 1]
        let total = 0
        for i in 0..200000 { total += first(a) }
        return total;
        "#,
        )
        .unwrap();
    assert_eq!(out[0].as_num().unwrap(), 1200000.0);
    assert!(
        vm.jit.compiled >= 1,
        "the loop reads a plain local and should compile: {:?}",
        vm.jit.last_error
    );
}

/// The scan that decides captures has to follow scope, not just names.
#[test]
fn a_name_read_before_it_is_shadowed_is_still_a_capture() {
    assert_eq!(
        s(r#"
        let x = "outer"
        fn f() { let y = x; let x = "inner"; y + "/" + x }
        f()
        "#),
        "outer/inner"
    );
}

/// Compiled code reads an array of arrays through views of every element,
/// cached against an epoch that has to move whenever any table's storage does.
/// Growing the outer array, and growing an element, both move something; a
/// debug build checks the cached views against the tables on every hit, so
/// this fails loudly rather than reading freed memory.
#[test]
fn cached_element_views_do_not_outlive_the_arrays_they_view() {
    let mut vm = Vm::new();
    vm.jit.threshold = 2;
    let out = vm
        .eval(
            r#"
        fn total(rows, n) {
            let s = 0
            for i in 0..n {
                let r = rows[i]
                s += r[0] + r[1]
            }
            s
        }
        let rows = [[1, 2], [3, 4]]
        let acc = 0
        for k in 0..40 { acc += total(rows, 2) }
        rows.push([5, 6])                      // the outer array moves
        for k in 0..40 { acc += total(rows, 3) }
        let first = rows[0]
        for k in 0..40 { first.push(k) }       // an element moves
        for k in 0..40 { acc += total(rows, 3) }
        return acc;
        "#,
        )
        .unwrap();
    assert_eq!(out[0].as_num().unwrap(), 40.0 * 10.0 + 80.0 * 21.0);
}

/// Compiled code reads an array of arrays through views of its elements, and
/// appends to a table through the runtime. If the table it appends to *is* one
/// of those elements, the append can move the storage the view points at — so
/// that call has to stay with the interpreter. The answer is the same either
/// way; what this pins is that it is still an answer.
#[test]
fn appending_to_an_element_of_an_array_it_reads_stays_interpreted() {
    let mut vm = Vm::new();
    vm.jit.threshold = 2;
    let out = vm
        .eval(
            r#"
        fn grow(rows, out, n) {
            let s = 0
            for i in 0..n {
                out.push(1)          // this can move the element's storage
                let r = rows[0]      // and this reads it again afterwards
                s += r[0]
            }
            s
        }
        let rows = [[1, 2], [3, 4]]
        let total = 0
        // `out` is an element of `rows`, so the views the compiled code holds
        // of the elements are exactly what the appends move
        for k in 0..40 { total += grow(rows, rows[0], 2) }
        return total + rows[0].len();
        "#,
        )
        .unwrap();
    assert_eq!(out[0].as_num().unwrap(), 40.0 * 2.0 + 82.0);
}

/// The index check compiled code uses on an unproven index is one comparison
/// against the length and one round trip through the machine integer. That has
/// to say the same thing as the interpreter about every awkward float: not a
/// number, negative, fractional, larger than the integers, and negative zero —
/// which is a real index, being equal to zero.
#[test]
fn a_compiled_index_check_agrees_with_the_interpreter() {
    let src = r#"
        fn read(t, i) { let s = 0; for k in 0..3 { s += t[i] } s }
        let t = [10, 20, 30]
        for k in 0..40 { read(t, 0) }
        let out = []
        out.push(if try(|| read(t, 0 / 0)) { 1 } else { 0 })
        out.push(if try(|| read(t, 0 - 1)) { 1 } else { 0 })
        out.push(if try(|| read(t, 1.5)) { 1 } else { 0 })
        out.push(if try(|| read(t, 1e30)) { 1 } else { 0 })
        out.push(read(t, -0.0))
        out.push(read(t, 2))
        out.join(",")
    "#;
    let mut jitted = Vm::new();
    jitted.jit.threshold = 2;
    let with = jitted.eval(src).unwrap()[0].to_string();

    let mut plain = Vm::new();
    plain.jit.enabled = false;
    let without = plain.eval(src).unwrap()[0].to_string();

    assert_eq!(with, "0,0,0,0,30,90");
    assert_eq!(with, without, "the compiler and the interpreter must agree");
}

/// Compiled code can make an array and push it into one of its caller's. If
/// the call then bails out, the interpreter runs the whole thing again — so
/// the append has to have been undone, or the row is there twice.
#[test]
fn a_row_pushed_before_a_trap_is_not_pushed_twice() {
    let mut vm = Vm::new();
    vm.jit.threshold = 3;
    let out = vm
        .eval(
            r#"
        fn addrow(m, k, t) {
            let r = []
            r.push(k)
            m.push(r)      // a table this code made, escaping into the caller's
            t[k]           // and past the end of `t` on the later calls
        }
        let t = []
        for i in 0..100 { t.push(i) }
        let m = []
        for i in 0..120 { try(|| addrow(m, i, t)) }
        return m.len(), m[0][0], m[119][0];
        "#,
        )
        .unwrap();
    let got: Vec<f64> = out.iter().map(|v| v.as_num().unwrap()).collect();
    assert_eq!(got, vec![120.0, 0.0, 119.0], "one row per call, in order");
}

/// A script that cannot read or write a file cannot do very much. This is the
/// whole of `fs` on one file, including what it says when the file is not
/// there — a script wants to print that, not a number.
#[test]
fn a_script_can_read_and_write_files() {
    let dir = std::env::temp_dir().join(format!("rua-fs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("notes.txt");
    let p = path.to_string_lossy().replace('\\', "/");
    let mut vm = Vm::new();
    let out = vm
        .eval(&format!(
            r#"
        let p = "{p}"
        fs::write(p, "alpha\nbeta\n")
        fs::append(p, "gamma\n")
        let ls = fs::lines(p)
        let missing = try(|| fs::read(p + ".nope"))
        let size = fs::size(p)
        fs::remove(p)
        return ls.len(), ls[2], missing, size, fs::exists(p);
        "#
        ))
        .unwrap_or_else(|e| panic!("{e}"));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out[0].as_num().unwrap(), 3.0, "three lines, no empty fourth");
    assert_eq!(out[1].to_string(), "gamma");
    assert_eq!(out[2].to_string(), "false", "a missing file is an error, not nil");
    assert_eq!(out[3].as_num().unwrap(), 17.0);
    assert_eq!(out[4].to_string(), "false", "removed");
}

/// `{` starts an interpolation, so a literal brace is doubled — and `}}` has
/// to mean one brace whether or not the same string happens to contain a `{`.
/// Writing JSON out of a script is where that rule gets met.
#[test]
fn braces_in_strings_escape_the_same_way_everywhere() {
    assert_eq!(s(r#""}}""#), "}");
    assert_eq!(s(r#""{{}}""#), "{}");
    assert_eq!(s(r#""}""#), "}");
    assert_eq!(s(r#""{{""#), "{");
    assert_eq!(s(r#"let n = 2; "{n} {{n}}""#), "2 {n}");
    // and the format placeholders still are what they are
    assert_eq!(s(r#"let n = 2; "{}" .format(n)"#), "2");
}

/// What a script prints is mostly a table of numbers beside their names, and
/// lining those up needs a width and a side to pad on. The spec is Rust's.
#[test]
fn format_can_line_a_column_up() {
    assert_eq!(s(r#""[{:>8}]".format("right")"#), "[   right]");
    assert_eq!(s(r#""[{:<8}]".format("left")"#), "[left    ]");
    assert_eq!(s(r#""[{:^9}]".format("mid")"#), "[   mid   ]");
    assert_eq!(s(r#""[{:-^9}]".format("mid")"#), "[---mid---]");
    assert_eq!(s(r#""[{:>7.1}]".format(3.14159)"#), "[    3.1]");
    assert_eq!(s(r#""[{:08.3}]".format(3.14159)"#), "[0003.142]");
    assert_eq!(s(r#""[{:.3}]".format("truncated")"#), "[tru]");
    // a number pads right and a string pads left, unasked, as in Rust
    assert_eq!(s(r#""[{:6}|{:6}]".format(42, "ab")"#), "[    42|ab    ]");
    assert_eq!(s(r#""[{:x}|{:o}|{:b}]".format(255, 64, 5)"#), "[ff|100|101]");
}
