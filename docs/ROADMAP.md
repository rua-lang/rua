# ROADMAP

[x] LSP — `crates/rua-lsp`. Diagnostics, hover, completion, the outline and
     semantic highlighting, go-to-definition, references, rename, signature
     help and formatting — all from the front end the interpreter runs on and
     a live `Vm` for what the standard library holds.
[x] Editor plugins — `editors/vscode`: a TextMate grammar that needs nothing
     installed, and a client for the server above. GitHub reads neither, so
     `.gitattributes` shows `.rua` as Rust there.
[x] TLS — `net::connect_tls`, certificates checked against the platform's
     own store (a bundled set when it has none). `examples/http.rua` speaks
     `https` and is a library now; `examples/fetch.rua` is the script.
[x] Types — written, read by the editor, checked (`rua --check`), and a
     value at run time: `typeis(v, T)` guards what comes in from outside.
[] Syscalls
[] Concurrency (async? tasks? threads?)