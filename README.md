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
  --dump-bytecode  print the bytecode the compiler generates
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

## The standard library

Enough to write something real, and no more than that.

```rust
// files: whole ones, because that is what a script wants
let text = fs::read("notes.md")
for line in fs::lines("notes.md") { print(line) }
fs::write("out.json", body)
fs::append("log.txt", "done\n")
if fs::exists(p) && !fs::is_dir(p) { print(fs::size(p)) }
for name in fs::list(".") { }                  // sorted, so runs compare

// sockets: TCP, both ends
let s = net::connect("example.com:80")
net::timeout(s, 10)                            // a read that waits forever hangs forever
net::write(s, "GET / HTTP/1.0\r\n\r\n")
print(net::read_line(s))                       // or net::read(s) for the rest
net::close(s)

let srv = net::listen("127.0.0.1:8080")        // port 0 asks for a free one
let c = net::accept(srv)                       // blocks

// running things, and the terminal
let (code, out, err) = os::run("git rev-parse HEAD")
let all = io::read_all()                       // stdin, for the end of a pipe
io::write("no newline")

// and a library beside your script
let json = require("json")                     // finds json.rua next to the file
print(json::write(json::parse(text)))          // `::` reaches a field, `.` calls a method
```

A socket is a number the runtime owns rather than an object, since there is no
type here to hang one on: closing it is explicit, and a handle that has been
closed is an error rather than somebody else's connection. There is no TLS, so
`https` is out — the runtime has no crypto and pretending otherwise would be
worse than saying so.

`examples/json.rua` is a JSON reader and writer written in rua, and
`examples/http.rua` is an HTTP client and a static file server, also in rua.
Both exist because writing them is how the gaps above were found.

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

**Tables.** A parameter that is indexed, appended to, or asked for its
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
flag and returns, and the interpreter runs the call instead.

**Arrays of arrays** work the same way one level down. `let b = bodies[i]`
binds an element, and the element's address and the view of its numbers are
fetched there — once per binding rather than once per access — so `b[3]` is a
load. `t[k][j]` without the binding compiles too, fetching the view at the
access, which is worth it exactly when the element changes every time round the
loop. The runtime checks on the way in that every element is a table of numbers
long enough for the constant indexes the body uses, so the body needs no test
of its own.

**Compiled code can make an array, fill it, and return it.** `let t = []`
allocates through the runtime, which owns the table for the length of the call
— so a trap drops it, exactly as an in-place write is discarded and an append
truncated. A table that escapes into a caller's array, or is returned, is
handed over at the end.

**Writes go through the view, and a trap takes them back.** While compiled code
runs, the numeric view *is* the table: a write is one store, and when the call
ends the runtime copies the view back over the array part. If the call trapped
instead, it throws the view away and every write goes with it, so the
interpreter can run the call again from the start. An append is recorded with
the length the table had before it and truncated back the same way.

Everything compiled code does to a table therefore undoes itself, and the rule
that shaped the whole compiler — never trap once you have written — is gone
with it. A function that reads and writes the same table, indexes it somewhere
the compiler cannot prove, recurses through another compiled function, or
builds an array while doing it, all compile.

**A hot loop is entered at once, not every thousandth turn.** Counting is how a
loop is found; once found, every iteration left in it belongs to the compiled
code. The counter doubles as the flag, so the check costs the same cell read.

**A local that only ever holds a boolean** is carried as 0.0/1.0 and tested
against zero. Which locals those are is a property of the scope, not the
register: the compiler reuses one register for a flag in one block and a number
in another.

**A small callee is compiled into its caller's object.** A call between
compiled functions otherwise goes through a pointer in the runtime's table,
which nothing can inline across shared objects. A callee that calls nothing
itself, takes only numbers and produces a value is emitted beside its caller
and called by name, so `rustc` inlines it — spectral norm's kernel is three
lines called n squared times, and the call cost more than the arithmetic.

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
entirely. The objects are stripped (about 300KB each rather than 4MB) and the
directory is pruned oldest-first to 128MB. `RUA_JIT_CACHE=0` turns the cache
off, `RUA_JIT_DIR` moves it, `RUA_JIT_CACHE_MB` resizes it.

## Speed

`bench/` holds eight programs written twice, once in rua and once in Lua —
n-body, binary trees, spectral norm, fannkuch, n-queens, matrix multiply, word
frequency, and a Scheme interpreter. `bench/run.sh` runs each under every
engine and **refuses to print a timing until they all produce byte-identical
output**: a fast wrong answer is not a benchmark. The JIT's disk cache is
warmed first, so these numbers exclude `rustc`.

The Scheme is the one to look at hardest. The other seven are loops over
numbers and arrays, which is the shape a JIT likes and an interpreter is least
embarrassed by. `bench/lisp.rua` is a language: reader, evaluator with proper
tail calls, closures and `set!`, thirty primitives, and a workload written in
the Scheme it implements — Takeuchi, fib, merge sort, n-queens, an association
list, the Y combinator. It spends its time on symbol lookup through chained
environments, cons-cell allocation, string dispatch on special forms and deep
recursion, and none of that is anything `rustc` can be handed.

| | rua interp | rua + JIT | lua 5.4 | luajit | vs luajit |
|---|---|---|---|---|---|
| n-body | 1.04s | **0.034s** | 0.488s | 0.042s | **0.80x** |
| n-queens | 0.14s | **0.022s** | 0.062s | 0.020s | **1.10x** |
| fannkuch | 0.60s | **0.050s** | 0.271s | 0.042s | **1.19x** |
| spectral norm | 1.15s | **0.035s** | 0.624s | 0.025s | 1.40x |
| matrix multiply | 0.48s | **0.025s** | 0.168s | 0.017s | 1.47x |
| binary trees | 2.61s | 2.59s | 3.281s | 1.430s | 1.81x |
| word frequency | 0.11s | 0.11s | 0.059s | 0.035s | 3.11x |
| Scheme interpreter | 1.99s | 2.01s | 1.376s | 0.564s | 3.52x |

**Six of the eight beat Lua 5.4**, and n-body is faster than LuaJIT. The five
the compiler takes are 0.80x to 1.47x of it; the three it cannot are 1.8x to
3.5x, and those are the interpreter.

Read that honestly. **Where the JIT applies it is decisive**, and **where it
does not, rua is its interpreter** — which now beats Lua 5.4 on binary trees
and is within 1.5x on the other two.

What keeps the last three out of the compiler is one thing: **values that are
not numbers or arrays of them.**

* **Strings.** Word frequency builds an array of words and joins them; the
  Scheme's reader walks a string a byte at a time.
* **Maps.** Binary trees is made of `#{ left: .., right: .. }` — keyed
  entries, where the compiler understands the array part.
* **Closures**, which the Scheme's evaluator is built on.

Making and returning *arrays* is no longer on that list: matrix multiply
builds a matrix a row at a time and hands it back, and compiles.


### What the interpreter's time actually goes on

Every claim here was measured by making the change and running the suite, and
a good many plausible ones were thrown away for measuring zero or worse.

* **Dispatch is not the problem.** The `match` costs about 8% of the
  instructions at a 0.89% branch-miss rate, with an IPC of 2.5. It is
  throughput bound, so the only lever is executing fewer instructions.
* **Reference counting is worth 1.2x, not the 1.3x this file used to claim.**
  A probe that removes it outright — leaking everything — measures the whole
  remaining prize at 1.17x to 1.23x, and part of even that is `free`, which a
  collector inherits rather than deletes. What was reachable without one was a
  long list of handles the work never needed: receivers, index objects,
  constant keys and arithmetic operands that were counted in and back out to be
  looked at. Those are gone.
* **A call is where an interpreted program lives.** Loading a callee from a
  global into a register that the next instruction takes straight back out is
  one instruction now; a call names the register its result belongs in; a
  spread consumes the results where they already sit; binding parameters is
  skipped when nothing is captured.
* **Two allocations per object was one too many.** A table with fields paid for
  an `Rc` and a `Vec` whose minimum capacity is four entries. The keyed part
  keeps two inline, and the fields only a large or compiled-over table needs
  moved behind one box: binary trees went from 1.51 mallocs per node to 1.01.
* **The allocator itself was 15%** of binary trees. The `rua` binary brings its
  own; the library crates do not, because an embedder's allocator is theirs.
* **Values that are only compared should not be copied**, and **writing a
  register should not call out to a destructor** — both were worth ~10% when
  they were found, and both are still the shape of the remaining cost.

Ideas that were implemented, measured, and reverted, because a list of those is
worth as much as the other one: an inline cache on dynamic indexing (an
environment lookup thrashes it, so the check is pure overhead); inlining string
equality into the comparison handlers; splitting the return path into fast and
slow halves; outlining call handling to shrink the dispatch loop; raising the
threshold at which a table builds a hash index; a machine-integer twin for loop
counters; and `-C target-cpu=native`, which one measurement liked and a careful
one did not — it costs 16% on spectral norm here.

What is left is the value representation. Every register write releases a real
handle and every read of a heap value takes one; removing that is a POD value
with a tracing GC, which is a different interpreter rather than a patch to this
one — and now worth 1.2x, most of it on the two benchmarks the compiler cannot
take.

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
