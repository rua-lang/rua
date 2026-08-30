# rua

A small scripting language with Rust's syntax, Lua's shape, and a JIT that is
just `rustc`.

```rust
fn fib(n) {
    if n < 2 { return n }        // a block's last expression is its value
    fib(n - 1) + fib(n - 2)
}

let xs = [3, 1, 2]               // semicolons optional
xs.sort()
for (i, v) in xs.iter() { print("{i}: {v}") }        // interpolation

let size = match xs.len() { 0 => "empty", 1 | 2 => "small", n => "{n} items" }

let cos = ffi::cdef(ffi::load("m"), "double cos(double x)")
print(cos(0))                    // 1
```

Three things make it interesting:

1. **It JITs through `rustc`.** Hot numeric functions are lowered to Rust source
   with `quote`, checked with `syn`, compiled by `rustc -O` into a cdylib, and
   dlopen'd back into the running process.
2. **It speaks C.** Scripts call C libraries with `ffi::cdef`, the way LuaJIT
   does, and C programs embed the VM through `include/rua.h`.
3. **It speaks Rust.** `Vm::register` exposes Rust closures to scripts, and
   `extern "C"` Rust cdylibs load through the same `ffi::cdef` door as C.

## Quick start

```sh
cargo build --release --workspace
./target/release/rua examples/hello.rua
./target/release/rua                        # REPL (top level `let` binds a global there)
./target/release/rua -e 'print(6 * 7)'
```

```
rua [options] [script.rua] [args...]
  -e CHUNK      evaluate CHUNK
  -i            REPL after the script
  --no-jit      interpret everything
  --jit N       compile a function after N calls (default 50)
  --dump-jit    print the Rust the JIT generates
```

## The language

Rust's surface, dynamically typed, everything an `f64` or a reference:

```rust
// items, closures, blocks-as-values
fn counter() {
    let n = 0;
    || { n += 1; n }                  // closures capture by reference
}
let next = counter();
print(next(), next(), next());        // 1 2 3

let kind = if n % 2 == 0 { "even" } else { "odd" };

// arrays (zero based) and maps
let xs = [3, 1, 2];
xs.push(4);
xs.sort();
print(xs.len(), xs.join(", "), xs[0]);

let p = #{ x: 3, y: 4 };
p.len = |self| math::sqrt(self.x * self.x + self.y * self.y);
print(p.len());                       // 5

// loops
for i in 0..10 { if i % 2 == 0 { continue; } print(i); }
for (k, v) in p.iter() { print(k, v); }
while i < 10 { i += 1; }
loop { break; }

// multiple returns, destructured
fn divmod(a, b) { return math::floor(a / b), a % b }
let (q, r) = divmod(17, 5)

// match, with alternatives, bindings and guards
let kind = match n {
    0 => "zero",
    1 | 2 | 3 => "small",
    x if x < 0 => "negative",
    x => "big: {x}",
}

// errors are values
let (ok, why) = try(|| error("boom"));
```

`.` is a method call and passes the receiver; `::` is a plain path, no
receiver — exactly the Rust distinction. That is why `"ab".upper()` and
`math::sqrt(2)` both read right: module functions *are* the methods, with the
receiver as their first argument.

Multiple files, since one file is rarely enough:

```rust
// vec2.rua — a file's last expression is what `require` hands back
fn make(x, y) { #{ x: x, y: y } }
fn len(v) { math::sqrt(v.x * v.x + v.y * v.y) }
#{ make: make, len: len }
```

```rust
let vec2 = require("vec2.rua");     // runs once, cached by path
print(vec2::len(vec2::make(3, 4))); // 5
```

A top level `fn` is a global, the way a Rust item belongs to its module — that
is how `require`, and an embedder, find it. A `fn` inside a block is a local.
Scripts can start with `#!/usr/bin/env rua` and be run directly.

When something goes wrong, the error says where:

```
rua: examples/x.rua:12: no method `nope` on a table value (in render)
```

Present: numbers, strings, booleans, `nil`, arrays, maps, closures, multiple
returns, `match`, string interpolation, `if`/`while`/`loop`/`for`, ranges
(`0..n`, `0..=n`), `break`, `continue`, `try`, `require`, and the `math`
`string` `table` `os` `io` modules. Semicolons are optional; the one place a
`;` changes meaning is at the end of a block, where it discards the value
instead of returning it. `let mut` parses and is ignored — everything is
mutable. So does `-> T` on a function.

Absent on purpose: static types, traits, borrow checking, destructuring
patterns beyond `let (a, b)`, integer/float distinction, string patterns,
coroutines. This is a scripting language that reads like Rust, not Rust.

## The JIT

Three things get compiled, all of them through `rustc`:

**Hot functions.** Every function counts its calls. Past the threshold (50, or
`--jit N`) the compiler looks at it, and if the whole body lives in the *numeric
subset* — arithmetic, comparisons, `&&`/`||`/`!`, locals, `if`, loops,
`math::*`, calls to other compiled functions, self recursion — it lowers it to
Rust. This:

```rust
fn collatz(n) {
    let steps = 0;
    while n > 1 {
        if n % 2 == 0 { n = n / 2; } else { n = 3 * n + 1; }
        steps += 1;
    }
    steps
}
```

becomes this (`--dump-jit` output, verbatim):

```rust
#[no_mangle]
pub extern "C" fn rua_jit_0(mut v0: f64) -> f64 {
    let __t0: f64 = 0f64;
    let mut v1: f64 = __t0;
    while (v0 > 1f64) {
        if (rua_rem(v0, 2f64) == 0f64) {
            let __a0: f64 = (v0 / 2f64);
            v0 = __a0;
        } else {
            let __a0: f64 = ((3f64 * v0) + 1f64);
            v0 = __a0;
        }
        v1 += 1f64;
    }
    v1
}
```

**Hot loops, mid-flight.** A loop that runs 50,000 iterations across its life is
compiled into a function over the locals it touches, and the interpreter hands
control to it *while the loop is running*, then picks the locals back up. That
is on-stack replacement, and it is what makes a script's top level, or a `main`
that is only ever called once, run at compiled speed.

**Tables.** A parameter that is only ever indexed or asked for its
length is passed as a table. Compiled code gets a contiguous view of its array
part — built on demand, thrown away by any write to the table, so it can never
go stale — and reads elements straight out of it with a bounds check:

```rust
fn dot(a, b) {
    let s = 0
    for i in 0..a.len() { s += a[i] * b[i] }    // 4M reads: 0.028s, vs 0.063s in Lua
    s
}
```

If an element turns out not to be a number, compiled code *traps*: it sets a
flag and returns, and the interpreter runs the call instead — sound because
nothing has been written yet.

Compiled code may also *append* to a table, which is what makes array building
compile. That needs the trap never to happen after a write, so a function that
appends may only read through an index the compiler can prove is in range
(`t[i]` inside `for i in 0..t.len()` over that same table, where the body
assigns neither), may not call another compiled function (whose trap would
unwind past the writes), and is skipped entirely when the table it writes is
the same one it reads.

Everything compiled is an `f64`, and rua is Lua-shaped — every number is true,
including `0` — so a boolean and the number encoding it are indistinguishable in
compiled code. A condition therefore has to be *provably* boolean (a comparison,
or `&&`/`||`/`!` over comparisons); anything else keeps the function
interpreted.

**Calls between compiled functions.** Compiling a function first compiles the
helpers it calls, then emits direct calls to their machine code — no interpreter
in between. If you later reassign one of those globals, every function that
inlined it has its code thrown away and recompiles:

```rust
fn square(x) { x * x }
fn hyp(a, b) { math::sqrt(square(a) + square(b)) }   // calls square directly

square = |x| x * x * 10;                             // hyp is recompiled
```

Every value in compiled code is an `f64`; booleans are 1.0/0.0. Anything outside
the subset — strings, tables, closures, captured locals, calls to interpreted
functions, a body that can fall through and return nil — falls back to the
interpreter. Results are identical either way, and `jit::status()` reports what
compiled, what didn't, and why.

Compiled code is cached in `~/.cache/rua-jit` (or `$XDG_CACHE_HOME`), keyed by a
hash of the generated Rust, so the second run of a script skips `rustc`
entirely. `RUA_JIT_CACHE=0` turns that off, `RUA_JIT_DIR` moves it.

## Speed

`bench/` holds seven programs written twice, once in rua and once in Lua —
n-body, binary trees, spectral norm, fannkuch, n-queens, matrix multiply and
word frequency. `bench/run.sh` runs each under every engine and **refuses to
print a timing until they all produce byte-identical output**: a fast wrong
answer is not a benchmark. The JIT's disk cache is warmed first, so these
numbers exclude `rustc`.

| | rua interp | rua + JIT | lua 5.4 | luajit |
|---|---|---|---|---|
| spectral norm | 1.85s | **0.12s** | 0.62s | 0.03s |
| n-queens | 0.17s | 0.17s | 0.06s | 0.02s |
| matrix multiply | 0.44s | 0.45s | 0.18s | 0.02s |
| fannkuch | 0.67s | 0.66s | 0.27s | 0.04s |
| word frequency | 0.21s | 0.22s | 0.07s | 0.04s |
| n-body | 1.51s | 1.54s | 0.49s | 0.04s |
| binary trees | 5.30s | 5.30s | 3.29s | 1.42s |

Read that honestly. **Where the JIT applies it is decisive** — spectral norm is
5x faster than Lua 5.4 and 20x faster than rua's own interpreter, because its
kernels are exactly what the compiler accepts: numbers and flat arrays. **Where
it does not apply, rua is its interpreter**, and that is 2–4x slower than Lua
5.4 across the board.

What keeps the other six out of the compiler is worth being precise about:

* **Nested tables.** `bodies[i][3]` and `b[k][j]` — n-body and matrix multiply
  are built on arrays of arrays, and compiled code only understands a flat one.
* **Indices it cannot vouch for.** n-queens indexes `diag1[row + c]`. Proving
  that in range needs value-range analysis; a real JIT would emit a guard and
  deoptimise, which rua cannot do after a write (see the JIT section).
* **Multiple return values**, which fannkuch's kernel uses.
* **Strings, maps and allocation** — word frequency and binary trees are made of
  exactly the things an f64-only compiler has nothing to say about.

The interpreter is 1.6–3.1x slower than Lua 5.4. An audit that prototyped and
measured each candidate cause — rather than reasoning about them — found that
most of the obvious suspects are not the problem:

* **Dispatch is not the problem.** The `match` costs 5.3% of cycles at a 0.010%
  branch-miss rate (Lua's own is 0.024%). Threaded dispatch with computed goto
  is not worth doing here, and an earlier version of this file said otherwise.
* **String interning and cached hashes buy nothing** — both were implemented and
  measured at zero. What string keys actually cost was the owned `Key`
  temporary and its refcount round trip, which is now gone.
* **Instruction count is not the problem either.** rua runs about 1.3x Lua's
  bytecode operations, but 2.4x the machine instructions per operation.

What is left is the value representation. Every register write drops what was
there and every read of a heap value bumps a reference count; Lua's values are
the same 16 bytes but garbage collected, so a copy is a plain move. Removing
that — a POD value with a tracing GC — measured as a further 1.3x. It is a
different interpreter, not a patch to this one. What this one does instead is
hand the hot numeric parts to `rustc`.

## FFI: calling C

```rust
let m = ffi::load("m");                                   // dlopen libm
let cos = ffi::cdef(m, "double cos(double x)");
print(cos(0));                                            // 1

let strlen = ffi::cdef("size_t strlen(const char *s)");   // defaults to ffi::C
let getenv = ffi::cdef("char *getenv(const char *name)");
print(strlen("hello"), getenv("HOME"));
```

Declarations are real C declarations, parsed from a small subset: the scalar
types, `void`, and one level of pointer. `char *` crosses the boundary as a rua
string, any other pointer as opaque `cdata`. Calls go through libffi, so the
signature you write is the signature that is used — a wrong declaration is a
wrong declaration, exactly as in LuaJIT.

## FFI: calling Rust

Rust's own ABI is unstable, so the boundary is `extern "C"` — the same door:

```rust
#[no_mangle]
pub extern "C" fn rust_add(a: f64, b: f64) -> f64 { a + b }
```

```rust
let plugin = ffi::load("./demo/libruaplugin.so");
let add = ffi::cdef(plugin, "double rust_add(double a, double b)");
print(add(1.5, 2.25));                                    // 3.75
```

`sh demo/run.sh` builds and runs this, plus the C embedding demo below.

## Embedding in Rust

```rust
use rua::{Value, Vm};

let mut vm = Vm::new();
vm.register("hypot", |_vm, args| {
    let (a, b) = (args[0].as_num()?, args[1].as_num()?);
    Ok(vec![Value::Num((a * a + b * b).sqrt())])
});
vm.set_global("scale", Value::Num(3.0));
vm.eval("fn area(r) { math::pi * r * r * scale }")?;

let area = vm.get_global("area");          // top level `fn` is a global
let out = vm.call(&area, vec![Value::Num(2.0)])?;
```

Full version: `cargo run --example embed`.

## Embedding in C

```c
#include "rua.h"

rua_State *S = rua_new();
rua_register(S, "hypot", c_hypot);          /* double f(const double*, int) */
rua_eval(S, "return hypot(3, 4);");
printf("%g\n", rua_result_number(S, 0));    /* 5 */
rua_close(S);
```

```sh
cargo build --release --workspace
cc demo/embed.c -I include -L target/release -lrua -lm -o embed
LD_LIBRARY_PATH=target/release ./embed
```

## Layout

A workspace, because the pieces genuinely do not need each other:

```
crates/rua-syntax    lexer, AST, parser, resolver          (no dependencies)
crates/rua-jit       AST -> quote/syn -> rustc -> dlopen   (depends on syntax)
crates/rua-ffi       C declaration parser, libffi calls    (knows nothing of rua values)
crates/rua-core      values, bytecode compiler, VM, stdlib (depends on the three above)
crates/rua-capi      the rua_* C ABI, built as librua.so
crates/rua-cli       the `rua` command
.                    the `rua` facade crate: re-exports the rest
```

The direction of the arrows is the point: the JIT sees only the AST and returns
a function pointer, so the runtime owns every policy decision about when to
compile. The FFI crate never sees a `Value`; `rua-core/src/cffi.rs` is the
twenty lines that translate.

Inside `rua-core`, source becomes bytecode in `compile.rs` (registers are the
resolver's slots, plus scratch above them) and runs in `vm.rs`. The AST stays
alive next to the bytecode, because that is what the JIT reads.

## Tests

```sh
cargo test --workspace     # 51 tests: language, JIT-equals-interpreter, FFI, modules
sh bench/run.sh            # the benchmark suite, with cross-engine output checks
```

The JIT tests run the same program with and without compilation and assert the
results match. That is the invariant that matters, and it is what an adversarial
multi-agent review pass was pointed at: it found 39 real bugs, including an
out-of-bounds read the range proof allowed, a use-after-free in the compiled
call graph, and three ways compiled and interpreted code could disagree. Every
one of them has a regression test here now.

## License

MIT
