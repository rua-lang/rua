# rua for VS Code

Editor support for rua, a small scripting language with Rust-shaped syntax
and Lua-shaped semantics. It lives in the `editors/vscode` directory of the
rua repository.

The extension is two halves that work independently:

| | comes from | works without |
| --- | --- | --- |
| Syntax highlighting, comment toggling, bracket matching, auto-closing, indentation | the bundled TextMate grammar and language configuration | anything installed |
| Diagnostics, hover, completion, document outline, semantic highlighting | the `rua-lsp` language server | — needs the binary on `PATH` |

Go-to-definition and rename are not implemented yet: they need the resolver to
report where each name was declared, which it does not do so far.

If `rua-lsp` is not installed, the extension says so once in its output
channel and carries on: highlighting, folding, `Ctrl+/`, bracket matching and
indentation all keep working. Nothing here is blocked on the server.

## What the grammar knows

The grammar is derived from `crates/rua-syntax/src/lexer.rs` and
`crates/rua-syntax/src/parser.rs`, not from a family resemblance to Rust:

- The sixteen keywords the lexer actually recognises: `break`, `continue`,
  `else`, `false`, `fn`, `for`, `if`, `in`, `let`, `loop`, `match`, `mut`,
  `nil`, `return`, `true`, `while`. There are no others — no `struct`, no
  `impl`, no `pub`, no `use`.
- `//` line comments and `/* */` block comments, **nested**: the lexer counts
  depth, so `/* a /* b */ c */` is one comment and the grammar treats it as one.
- Strings in double quotes only. They may span lines — the lexer scans to the
  next quote and does not stop at a newline. `\n`, `\t`, `\r` and `\0` are
  translated; every other escape yields the character itself.
- String interpolation. `{expr}` is highlighted as an embedded rua expression,
  so `"{vec2::len(a)}"` colours `vec2`, `::` and `len` the way it would outside
  the string. `{{` and `}}` are literal braces, and `{}` / `{:.2}` are left
  alone as `format` placeholders — which is exactly the split
  `parser.rs::interpolate` makes. `{expr:spec}` gets the expression highlighted
  and the spec after the `:` marked separately.
  Note that `\{` is *not* an escape: escapes are resolved before interpolation
  runs, so the backslash disappears and `{` still opens an interpolation. The
  grammar deliberately reproduces that.
- Numbers: `0x` hex, decimals, `_` separators anywhere in the digits, and
  exponents. A `.` only continues a number when a digit follows it, so `0..10`
  highlights as two numbers around a range operator rather than as `0.` and
  `.10`.
- The `#{ … }` map literal, with its keys — bare names, strings, numbers and
  `[computed]` — distinguished from the values, and nested maps and blocks
  handled without the outer literal ending early.
- `::` versus `.`: `vec2::make` is a namespace reaching a field, `xs.push` is a
  method on a receiver. The nine runtime modules (`math`, `string`, `table`,
  `os`, `io`, `fs`, `net`, `ffi`, `jit`) and the registered globals (`print`,
  `format`, `require`, `error`, `assert`, `str`, `num`, `try`, `global`,
  `globals`, `dofile`) get a built-in scope.
- Every operator in the `Tok` enum, compound assignments included: `+ - * / %`,
  `+= -= *= /= %=`, `== != < > <= >=`, `&& || !`, `= -> => .. ..= | :: #`.
- A leading `#!` line, which the lexer skips for the shell.

The grammar stops at lexical facts. It will not tell you whether a name is a
local or a global, whether a call has the right arity, or whether a variable is
used — those need a resolver, and they arrive with the language server.

## Building

```sh
cd editors/vscode
npm install
npm run compile      # tsc -p ./  ->  out/extension.js
```

`npm run watch` recompiles on save. `npm run typecheck` checks without emitting.

## Running it

**From this checkout, without installing.** Open `editors/vscode` in VS Code and
press <kbd>F5</kbd>. That launches an Extension Development Host with the
extension loaded; open any `.rua` file in it.

**Installed locally.** Package it into a `.vsix` and install that:

```sh
npm install
npx vsce package             # produces rua-0.1.0.vsix
code --install-extension rua-0.1.0.vsix
```

**Or symlink it**, which skips packaging entirely:

```sh
ln -s "$PWD" ~/.vscode/extensions/rua
```

VS Code picks it up on the next restart. Remember to `npm run compile` first,
or the manifest's `main` will point at nothing and only the grammar will load.

## The language server

The extension looks for a binary called `rua-lsp`. It resolves the name itself
before spawning anything, so a missing server produces a sentence rather than a
raw `ENOENT`:

```
rua language server not found: `rua-lsp` is not on PATH and is not an
executable file.
```

Install it and the extension picks it up on **rua: Restart Language Server**
(or on the next window). A bare name is looked up on `PATH`; a value containing
a path separator is used as given, with `~` expanded and a relative path
resolved against the first workspace folder.

### Settings

| setting | default | what it does |
| --- | --- | --- |
| `rua.server.enabled` | `true` | Run the server at all. Set to `false` to keep highlighting and nothing else. |
| `rua.server.path` | `"rua-lsp"` | Executable to launch. Bare name → `PATH` lookup; path → used as given. |
| `rua.server.args` | `[]` | Extra arguments for the server. |
| `rua.server.env` | `{}` | Extra environment variables for the server process. |
| `rua.trace.server` | `"off"` | `messages` or `verbose` to log the LSP traffic. |

Changing any of the launch settings restarts the server, so a change is not
silently ignored until the next window.

### Commands

- **rua: Restart Language Server** — stop and start it, re-resolving the path.
- **rua: Show Language Server Output** — open the channel with the launch log,
  the server's stderr, and the trace if it is on.

The client speaks LSP over stdio and requests notifications for `**/*.rua`, so
a server that wants to watch files gets told about them.

## Layout

```
package.json                     manifest: language, grammar, settings, commands
language-configuration.json      comments, brackets, auto-closing, indentation
syntaxes/rua.tmLanguage.json     the TextMate grammar
src/extension.ts                 the LSP client
```

## License

MIT, with the rest of rua.
