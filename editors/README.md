# Editors

Two pieces, and they are independent. The **grammar** colours a file with
nothing installed. The **language server** adds what only the real front end
can know: every syntax error at once, `fs::` completing to the names `fs`
actually holds, and go-to-definition that knows which `x` you meant.

```sh
cargo install --path crates/rua-lsp     # puts `rua-lsp` on your PATH
```

Everything below assumes that has been run. Check it with `rua-lsp --version`;
the server speaks LSP over stdin and stdout, so running it by hand just sits
there, which is correct.

## What the server does

| | |
| --- | --- |
| Diagnostics | every syntax error in the file, not the first — the parser reads on after each one |
| Go to definition | the declaration a name resolves to, shadowing and closures included |
| References, highlight | every mention of *that* variable, not every mention of the spelling |
| Rename | the same set, rewritten; refused on a name this file did not declare |
| Hover | what a keyword does; what a module holds |
| Completion | reads the position *and* the types: the parameter a call wants, the fields a shape declares, only types where a type goes, and every name with the type it was written with |
| Outline | the functions in the file, even while it does not parse |
| Formatting | whitespace between tokens, and nothing else — it cannot lose a comment or change what a program says |
| Semantic highlighting | a name coloured by whether it is a call, a module or a field |

## VS Code

```sh
cd editors/vscode
npm install && npm run compile
npx @vscode/vsce package --allow-missing-repository
code --install-extension rua-0.1.0.vsix
```

The packaged `.vsix` carries its own copy of `vscode-languageclient`, so it
needs nothing else at run time. `rua.server.path` points at the binary if it
is not on `PATH`; `rua.server.enabled` turns the server off and leaves the
colours.

## fresh

Two files and a config block. The grammar first — fresh highlights with
[syntect], which reads `.sublime-syntax` and **not** TextMate
`.tmLanguage.json`, so it cannot share the one VS Code uses:

```sh
cp editors/fresh/rua/grammars/syntax.sublime-syntax ~/.config/fresh/grammars/rua.sublime-syntax
```

```jsonc
// ~/.config/fresh/config.json
{
  "lsp_enabled": true,
  "languages": {
    "rua": {
      "extensions": ["rua"],
      "comment_prefix": "//",
      "auto_indent": true,
      // named for TextMate, but the loader takes the file's extension and
      // accepts only .sublime-syntax
      "textmate_grammar": "/home/you/.config/fresh/grammars/rua.sublime-syntax",
      // fresh's own format command runs this, not the server
      "formatter": { "command": "rua", "args": ["--fmt"], "stdin": true }
    }
  },
  "lsp": {
    "rua": [
      {
        "command": "rua-lsp",
        "enabled": true,
        "name": "rua",
        "auto_start": true,
        "root_markers": ["Cargo.toml", ".git"],
        "env": { "RUA_LSP_LOG": "info" }
      }
    ]
  }
}
```

Grammars are built once at startup, so **restart fresh**, and reopen any
buffer that was open before — it keeps the syntax it was given.

What each of fresh's commands reaches:

| fresh | what answers |
| --- | --- |
| diagnostics in the gutter, as you type | the server, unasked |
| `Go to Definition`, `Find References`, `Rename`, `Go to Implementation` | the server |
| `Signature Help` | the server, for functions written in the file |
| `Completion` | the server |
| hover under the mouse | the server |
| `Format buffer` | **`rua --fmt`, not the server** — fresh has no `lsp_format` action, so its format command runs the configured formatter |
| `Code Actions` | nothing yet; the server offers none |

Do not put a `"grammar"` key in `languages.rua`: naming a built-in there
overrides the file above.

[syntect]: https://github.com/trishume/syntect

## Neovim

No plugin needed; the built-in client is enough.

```lua
-- ~/.config/nvim/init.lua
vim.filetype.add({ extension = { rua = "rua" } })

vim.api.nvim_create_autocmd("FileType", {
  pattern = "rua",
  callback = function(args)
    vim.lsp.start({
      name = "rua",
      cmd = { "rua-lsp" },
      root_dir = vim.fs.root(args.buf, { "Cargo.toml", ".git" }),
    })
  end,
})
```

Neovim 0.11 binds `grn` to rename, `grr` to references, `gO` to the outline
and `K` to hover on its own; the definition is on `<C-]>`, through the
`tagfunc` it sets. `:checkhealth vim.lsp` shows whether the server attached.

## Zed

Zed wants an extension to register a language, but it will run a server for a
language it already knows. Until there is a rua extension, treat `.rua` as
Rust for colours — the same trick `.gitattributes` uses for GitHub:

```json
// ~/.config/zed/settings.json
{ "file_types": { "Rust": ["*.rua"] } }
```

## Formatting without an editor

```sh
rua --fmt file.rua              # to standard output
rua --fmt --write *.rua         # in place, printing what it changed
cat a.rua | rua --fmt           # with no file, a filter
```

The same formatter the server uses. It moves whitespace between tokens and
never looks inside one, so it cannot lose a comment, reorder anything, or
change what a program means — the test suite lays out every `.rua` file in
the repository and checks the tokens come back identical. A file that does
not lex is reported and left alone.

VS Code formats through the server: its Format Document is wired to whatever
the server advertises, and the extension names itself the default formatter
for rua so nothing stops to ask which one to use.

fresh does not. Its `Format buffer` runs the command named in
`languages.rua.formatter` — there is no `lsp_format` action — so that is
where `rua --fmt` goes, and `"format_on_save": true` beside it does the rest.

## When it does not work

The server writes what it is doing to standard error, which is where every
editor keeps it — fresh under `logs/lsp/`, VS Code in its output channel.

```
[   0.001] info  opened lisp.rua (17435 bytes)
[   0.001] info  lisp.rua: no problems (1.2ms)
[   0.002] error textDocument/rename failed: `print` is not declared in this
                 file, so renaming it here would leave the declaration behind
```

`RUA_LSP_LOG=debug` adds a line per request with how long it took;
`RUA_LSP_LOG=off` silences everything. Pass it through the editor's own
setting for the server's environment — `rua.server.env` in VS Code, `env` in
a fresh `lsp` entry.

## What was checked here

The VS Code extension compiles and packages on this machine, and the `.vsix`
carries its own `vscode-languageclient`. The `fresh` configuration above is
the one running here, and fresh's own log shows the server attaching:
`LSP server 'rua' initialized for language: rua`. The grammar pack is
installed but its colours have not been seen, since that needs a restart of
somebody's editor. Neovim and Zed are not installed on this machine, so those
two are written from their documentation and not from a run.

## Anything else

The server is an ordinary LSP over stdio: run `rua-lsp`, speak the protocol.
Nothing in it is editor-specific.
