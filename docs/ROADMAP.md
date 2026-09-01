# ROADMAP

[x] LSP — `crates/rua-lsp`. Diagnostics, hover, completion, the outline and
     semantic highlighting, all from the front end the interpreter runs on and
     a live `Vm` for what the standard library holds. Go-to-definition and
     rename are still out: they want the resolver to report where each name
     was declared, and it does not yet.
[x] Editor plugins — `editors/vscode`: a TextMate grammar that needs nothing
     installed, and a client for the server above. GitHub reads neither, so
     `.gitattributes` shows `.rua` as Rust there.
[x] TLS — `net::connect_tls`, certificates checked against the platform's
     own store (a bundled set when it has none). `examples/http.rua` speaks
     `https` and is a library now; `examples/fetch.rua` is the script.
[] Syscalls
[] Concurrency (async? tasks? threads?)