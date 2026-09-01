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
| Completion | the module's real members after `fs::`, plus keywords, globals and names in the file |
| Outline | the functions in the file, even while it does not parse |
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

fresh highlights with [syntect], which reads `.sublime-syntax` and **not**
TextMate `.tmLanguage.json` — so it cannot share the grammar VS Code uses.
There is a syntect grammar for rua in `editors/fresh/rua/`, but fresh loads
user grammars through its package manager rather than from a directory, and
that path is not covered by `fresh --cmd help`. Until it is, borrow Rust's
built-in grammar, which is close because rua was shaped after it:

```jsonc
// ~/.config/fresh/config.json
{
  "lsp_enabled": true,
  "languages": {
    "rua": {
      "extensions": ["rua"],
      "grammar": "Rust",
      "comment_prefix": "//",
      "auto_indent": true
    }
  },
  "lsp": {
    "rua": [
      {
        "command": "rua-lsp",
        "enabled": true,
        "name": "rua",
        "auto_start": true,
        "root_markers": ["Cargo.toml", ".git"]
      }
    ]
  }
}
```

What Rust's grammar gets wrong is `#{`, `::` on a module, and the `{}` inside
strings; everything else — `fn`, `let`, `match`, comments, numbers, strings —
lands. The language server is unaffected either way, and it is the half that
knows what the names mean.

`fresh --cmd config show` prints the merged configuration back, which is the
quickest way to see that it took.

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

## What was checked here

The VS Code extension compiles and packages on this machine, and the `.vsix`
carries its own `vscode-languageclient`. The `fresh` configuration above is
the one running here — `fresh --cmd config show` reads it back — though
whether its LSP client attaches has not been watched from the inside. Neovim
and Zed are not installed on this machine, so those two are written from
their documentation and not from a run.

## Anything else

The server is an ordinary LSP over stdio: run `rua-lsp`, speak the protocol.
Nothing in it is editor-specific.
