//! What the server knows, and where it learned it.
//!
//! Diagnostics come from the parser's recovery, highlighting from the lexer,
//! and the standard library from a live `Vm` — asked what globals it has
//! rather than told. A second description of rua kept here would drift from
//! the one that runs.

use crate::docs::Docs;
use crate::{log_debug, log_info};
use crate::index::LineIndex;
use lsp_types::*;
use rua_syntax::lexer::{Lexed, Lexer, Tok};

/// The token kinds this server colours, in the order the editor is told about
/// them — the numbers on the wire are indices into this.
pub const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,
    SemanticTokenType::STRING,
    SemanticTokenType::NUMBER,
    SemanticTokenType::OPERATOR,
    SemanticTokenType::VARIABLE,
    SemanticTokenType::FUNCTION,
    SemanticTokenType::NAMESPACE,
    SemanticTokenType::PROPERTY,
];

const KEYWORD: u32 = 0;
const STRING: u32 = 1;
const NUMBER: u32 = 2;
const OPERATOR: u32 = 3;
const VARIABLE: u32 = 4;
const FUNCTION: u32 = 5;
const NAMESPACE: u32 = 6;
const PROPERTY: u32 = 7;

pub struct World {
    docs: Docs,
    /// A runtime, only ever asked what it contains. Building one is what makes
    /// the completion list the real standard library.
    vm: rua_core::Vm,
}

impl World {
    pub fn new() -> World {
        let vm = rua_core::Vm::new();
        log_info!("standard library: {} globals", vm.global_names().len());
        World { docs: Docs::default(), vm }
    }

    /// Put a document in, as `didOpen` does. The tests drive the server this
    /// way; an editor goes through `apply`.
    pub fn open(&mut self, uri: Url, text: &str) {
        self.docs.set(uri, text);
    }

    /// Hover, as its text. `hover` wraps this for the protocol.
    pub fn hover_at(&self, uri: &Url, at: Position) -> Option<String> {
        let index = self.docs.get(uri)?;
        let offset = index.offset(at);
        let (toks, _) = Lexer::tokenize_all(index.text());
        let i = toks.iter().position(|t| t.span.contains(offset))?;
        self.describe(&toks, i)
    }

    /// The names in the outline, in order.
    pub fn symbol_names(&self, uri: &Url) -> Vec<String> {
        match self.symbols(&DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }) {
            Some(DocumentSymbolResponse::Nested(v)) => v.into_iter().map(|s| s.name).collect(),
            _ => Vec::new(),
        }
    }

    /// Each coloured token's text and the kind it was given, for tests: the
    /// wire format is deltas, which say nothing on their own.
    pub fn token_kinds(&self, uri: &Url) -> Vec<(String, SemanticTokenType)> {
        let Some(index) = self.docs.get(uri) else { return Vec::new() };
        let (toks, _) = Lexer::tokenize_all(index.text());
        let mut out = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            let Some(kind) = token_kind(&toks, i) else { continue };
            if !index.one_line(t.span.lo, t.span.hi) || t.span.is_empty() {
                continue;
            }
            // only the ones a reader would call a name: the rest are keywords
            // and punctuation, and their text is their kind
            if !matches!(t.tok, Tok::Name(_)) {
                continue;
            }
            let text = index.text()[t.span.lo as usize..t.span.hi as usize].to_string();
            out.push((text, TOKEN_TYPES[kind as usize].clone()));
        }
        out
    }

    /// Every mention of a name in a document, and what each refers to.
    ///
    /// The resolver works this out to compile the file at all; asking it
    /// rather than working it out again here is what keeps the editor's idea
    /// of scope and the interpreter's from ever differing.
    fn occurrences(&self, uri: &Url) -> Vec<rua_syntax::resolve::Occurrence> {
        let Some(index) = self.docs.get(uri) else { return Vec::new() };
        // the tree the parser could recover is enough: a file with a mistake
        // in it still has names in the rest of it
        let (block, _) = rua_syntax::parser::parse_recover(index.text());
        rua_syntax::resolve::occurrences(&block)
    }

    /// The name written under the cursor, as the resolver understood it.
    fn at(&self, uri: &Url, at: Position) -> Option<rua_syntax::resolve::Occurrence> {
        let index = self.docs.get(uri)?;
        let offset = index.offset(at);
        self.occurrences(uri)
            .into_iter()
            // a cursor resting just past the last character of a name is
            // still on it, which is where it lands after typing one
            .find(|o| o.span.contains(offset) || o.span.hi == offset)
    }

    /// Where the name under the cursor was declared.
    pub fn definition_at(&self, uri: &Url, at: Position) -> Option<Location> {
        let index = self.docs.get(uri)?;
        let decl = self.at(uri, at)?.decl?;
        Some(Location { uri: uri.clone(), range: index.range(decl.lo, decl.hi) })
    }

    /// Every mention of the same thing — which is not every mention of the
    /// same spelling: two locals in different scopes share a name and are not
    /// the same variable.
    pub fn references_at(&self, uri: &Url, at: Position) -> Vec<Location> {
        let Some(index) = self.docs.get(uri) else { return Vec::new() };
        let Some(target) = self.at(uri, at) else { return Vec::new() };
        self.same_thing(uri, &target)
            .into_iter()
            .map(|o| Location { uri: uri.clone(), range: index.range(o.span.lo, o.span.hi) })
            .collect()
    }

    /// The occurrences that mean what this one means. A declaration in this
    /// file is the identity; a global with none is matched by name, since
    /// there is nothing else to go on.
    fn same_thing(
        &self,
        uri: &Url,
        target: &rua_syntax::resolve::Occurrence,
    ) -> Vec<rua_syntax::resolve::Occurrence> {
        let all = self.occurrences(uri);
        match target.decl {
            Some(decl) => all.into_iter().filter(|o| o.decl == Some(decl)).collect(),
            None => all
                .into_iter()
                .filter(|o| o.name == target.name && o.kind == target.kind)
                .collect(),
        }
    }

    /// May the name under the cursor be renamed, and which bytes are it?
    ///
    /// A name declared somewhere this file cannot see — `print`, or a global
    /// another file defines — is refused: renaming every mention here would
    /// leave the declaration behind and quietly break the program.
    pub fn prepare_rename_at(&self, uri: &Url, at: Position) -> Option<Range> {
        let index = self.docs.get(uri)?;
        let target = self.at(uri, at)?;
        target.decl?;
        Some(index.range(target.span.lo, target.span.hi))
    }

    /// Rename every mention of one thing.
    pub fn rename_at(&self, uri: &Url, at: Position, to: &str) -> Result<WorkspaceEdit, String> {
        if !is_name(to) {
            return Err(format!("`{to}` is not a name"));
        }
        let index = self.docs.get(uri).ok_or("no such document")?;
        let target = self.at(uri, at).ok_or("there is no name here")?;
        if target.decl.is_none() {
            return Err(format!(
                "`{}` is not declared in this file, so renaming it here would \
                 leave the declaration behind",
                target.name
            ));
        }
        let edits: Vec<TextEdit> = self
            .same_thing(uri, &target)
            .into_iter()
            .map(|o| TextEdit {
                range: index.range(o.span.lo, o.span.hi),
                new_text: to.to_string(),
            })
            .collect();
        let mut changes = std::collections::HashMap::new();
        changes.insert(uri.clone(), edits);
        Ok(WorkspaceEdit { changes: Some(changes), ..Default::default() })
    }

    /// Take in what the editor says changed. Answers the document's uri when
    /// its text moved, since that is when diagnostics are worth resending.
    pub fn apply(&mut self, note: &lsp_server::Notification) -> Option<Url> {
        use lsp_types::notification::Notification as _;
        match note.method.as_str() {
            notification::DidOpenTextDocument::METHOD => {
                let p: DidOpenTextDocumentParams =
                    serde_json::from_value(note.params.clone()).ok()?;
                log_info!(
                    "opened {} ({} bytes)",
                    crate::log::short(&p.text_document.uri),
                    p.text_document.text.len()
                );
                self.docs.set(p.text_document.uri.clone(), &p.text_document.text);
                Some(p.text_document.uri)
            }
            notification::DidChangeTextDocument::METHOD => {
                let p: DidChangeTextDocumentParams =
                    serde_json::from_value(note.params.clone()).ok()?;
                // full sync: the last change carries the whole document
                let text = p.content_changes.last()?.text.clone();
                log_debug!("changed {} ({} bytes)", crate::log::short(&p.text_document.uri), text.len());
                self.docs.set(p.text_document.uri.clone(), &text);
                Some(p.text_document.uri)
            }
            notification::DidCloseTextDocument::METHOD => {
                let p: DidCloseTextDocumentParams =
                    serde_json::from_value(note.params.clone()).ok()?;
                log_info!("closed {}", crate::log::short(&p.text_document.uri));
                self.docs.remove(&p.text_document.uri);
                // one last empty set, so the editor drops what it was showing
                Some(p.text_document.uri)
            }
            _ => None,
        }
    }

    /// Everything wrong with a document, which is everything the parser could
    /// not read — it reads on after each, so this is all of them and not the
    /// first one.
    pub fn diagnostics(&self, uri: &Url) -> Vec<Diagnostic> {
        let Some(index) = self.docs.get(uri) else { return Vec::new() };
        let (_, errors) = rua_syntax::parser::parse_recover(index.text());
        errors
            .iter()
            .map(|e| Diagnostic {
                range: span_range(index, e.span, e.line),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("rua".to_string()),
                message: e.message.clone(),
                ..Default::default()
            })
            .collect()
    }

    pub fn semantic_tokens(&self, p: &SemanticTokensParams) -> Option<SemanticTokensResult> {
        let index = self.docs.get(&p.text_document.uri)?;
        let (toks, _) = Lexer::tokenize_all(index.text());
        let mut data = Vec::new();
        let (mut last_line, mut last_start) = (0u32, 0u32);
        for (i, t) in toks.iter().enumerate() {
            let Some(kind) = token_kind(&toks, i) else { continue };
            // a token the editor colours may not straddle two lines, and a rua
            // string is allowed to
            if !index.one_line(t.span.lo, t.span.hi) || t.span.is_empty() {
                continue;
            }
            let at = index.position(t.span.lo);
            let end = index.position(t.span.hi);
            let delta_line = at.line - last_line;
            let delta_start = if delta_line == 0 { at.character - last_start } else { at.character };
            data.push(SemanticToken {
                delta_line,
                delta_start,
                length: end.character - at.character,
                token_type: kind,
                token_modifiers_bitset: 0,
            });
            last_line = at.line;
            last_start = at.character;
        }
        Some(SemanticTokensResult::Tokens(SemanticTokens { result_id: None, data }))
    }

    /// The functions in a file, for the outline. Read from the tokens rather
    /// than the tree, so that a file which does not parse still has one.
    pub fn symbols(&self, p: &DocumentSymbolParams) -> Option<DocumentSymbolResponse> {
        let index = self.docs.get(&p.text_document.uri)?;
        let (toks, _) = Lexer::tokenize_all(index.text());
        let mut out = Vec::new();
        for pair in toks.windows(2) {
            if pair[0].tok != Tok::Fn {
                continue;
            }
            let Tok::Name(name) = &pair[1].tok else { continue };
            let range = index.range(pair[0].span.lo, pair[1].span.hi);
            #[allow(deprecated)]
            out.push(DocumentSymbol {
                name: name.clone(),
                detail: None,
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range,
                selection_range: index.range(pair[1].span.lo, pair[1].span.hi),
                children: None,
            });
        }
        Some(DocumentSymbolResponse::Nested(out))
    }

    pub fn hover(&self, p: &HoverParams) -> Option<Hover> {
        let uri = &p.text_document_position_params.text_document.uri;
        let index = self.docs.get(uri)?;
        let at = index.offset(p.text_document_position_params.position);
        let (toks, _) = Lexer::tokenize_all(index.text());
        let i = toks.iter().position(|t| t.span.contains(at))?;
        let text = self.describe(&toks, i)?;
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: text,
            }),
            range: Some(index.range(toks[i].span.lo, toks[i].span.hi)),
        })
    }

    /// What to say about the token under the cursor.
    fn describe(&self, toks: &[Lexed], i: usize) -> Option<String> {
        if let Some(word) = keyword_doc(&toks[i].tok) {
            return Some(word.to_string());
        }
        let Tok::Name(name) = &toks[i].tok else { return None };
        // `fs::read` — the module in front says which table to look in
        let qualified = i >= 2 && toks[i - 1].tok == Tok::ColonColon;
        if qualified {
            if let Tok::Name(module) = &toks[i - 2].tok {
                if let Some(members) = self.members(module) {
                    return Some(if members.iter().any(|m| m == name) {
                        format!("```rust\n{module}::{name}\n```\n\nfrom the `{module}` module.")
                    } else {
                        format!("`{name}` is not in `{module}`.")
                    });
                }
            }
        }
        if let Some(mut members) = self.members(name) {
            members.sort();
            return Some(format!(
                "```rust\n{name}\n```\n\nA module of {} names:\n\n{}",
                members.len(),
                members.iter().map(|m| format!("- `{name}::{m}`")).collect::<Vec<_>>().join("\n")
            ));
        }
        if self.is_global(name) {
            return Some(format!("```rust\n{name}\n```\n\nA name the runtime provides."));
        }
        None
    }

    pub fn complete(&self, p: &CompletionParams) -> Option<CompletionResponse> {
        let uri = &p.text_document_position.text_document.uri;
        self.complete_at(uri, p.text_document_position.position)
    }

    /// What is worth offering where the cursor is.
    ///
    /// Most of the work is deciding that, not listing names: after a `.` the
    /// keywords are noise, inside a comment everything is, and inside
    /// `require("` the answer is a list of files rather than of names.
    pub fn complete_at(&self, uri: &Url, at: Position) -> Option<CompletionResponse> {
        let index = self.docs.get(uri)?;
        let offset = index.offset(at);
        let scan = Lexer::scan(index.text());
        let items = match self.where_is(&scan, index.text(), offset) {
            Where::Comment | Where::Text | Where::Naming => Vec::new(),
            Where::Member(module) => match self.members(&module) {
                Some(mut names) => {
                    names.sort();
                    names
                        .into_iter()
                        .map(|n| item(&n, CompletionItemKind::FUNCTION, &module))
                        .collect()
                }
                // not a module this runtime has: better to say nothing than to
                // offer every global as though it were inside it
                None => Vec::new(),
            },
            Where::Method => self.methods(&scan),
            Where::Require(prefix) => self.modules_beside(uri, &prefix),
            Where::Expression => self.in_scope(&scan),
        };
        Some(CompletionResponse::Array(items))
    }

    /// The methods a `.` could be followed by: what the runtime answers for
    /// strings, tables and numbers, plus the field names this file uses.
    fn methods(&self, scan: &rua_syntax::lexer::Scan) -> Vec<CompletionItem> {
        use rua_core::MethodTable;
        let mut out = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for (kind, what) in [
            (MethodTable::Str, "string"),
            (MethodTable::Table, "table"),
            (MethodTable::Math, "number"),
        ] {
            let mut names: Vec<String> =
                self.vm.method_names(kind).iter().map(|n| n.to_string()).collect();
            names.sort();
            for n in names {
                if seen.iter().any(|s| *s == n) {
                    continue;
                }
                seen.push(n.clone());
                out.push(item(&n, CompletionItemKind::METHOD, what));
            }
        }
        // a field is not a method, but `t.` is where both are written
        for (i, t) in scan.tokens.iter().enumerate() {
            if i == 0 || scan.tokens[i - 1].tok != Tok::Dot {
                continue;
            }
            if let Tok::Name(n) = &t.tok {
                if !seen.iter().any(|s| s == n) {
                    seen.push(n.clone());
                    out.push(item(n, CompletionItemKind::FIELD, "used in this file"));
                }
            }
        }
        out
    }

    /// What `require` could be given: the rua files beside this one.
    fn modules_beside(&self, uri: &Url, prefix: &str) -> Vec<CompletionItem> {
        let Ok(path) = uri.to_file_path() else { return Vec::new() };
        let Some(dir) = path.parent() else { return Vec::new() };
        // `require("lib/thing")` is written with a directory in it, so the
        // part before the last slash says where to look
        let (sub, _) = prefix.rsplit_once('/').unwrap_or(("", prefix));
        let looking = if sub.is_empty() { dir.to_path_buf() } else { dir.join(sub) };
        let Ok(entries) = std::fs::read_dir(&looking) else { return Vec::new() };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let full = if sub.is_empty() { name.clone() } else { format!("{sub}/{name}") };
            if e.path().is_dir() && !name.starts_with('.') {
                out.push(item(&format!("{full}/"), CompletionItemKind::FOLDER, "directory"));
            } else if let Some(stem) = name.strip_suffix(".rua") {
                // `require` adds the extension itself, so it is not written
                let label =
                    if sub.is_empty() { stem.to_string() } else { format!("{sub}/{stem}") };
                if Some(&*label) != path.file_stem().and_then(|s| s.to_str()) {
                    out.push(item(&label, CompletionItemKind::MODULE, "a file beside this one"));
                }
            }
        }
        out.sort_by(|a, b| a.label.cmp(&b.label));
        out
    }

    /// Keywords, what the runtime provides, and the names this file declares.
    fn in_scope(&self, scan: &rua_syntax::lexer::Scan) -> Vec<CompletionItem> {
        let mut out: Vec<CompletionItem> = Vec::new();
        for k in KEYWORDS {
            out.push(item(k, CompletionItemKind::KEYWORD, "keyword"));
        }
        for g in self.vm.global_names() {
            let kind = if self.members(&g).is_some() {
                CompletionItemKind::MODULE
            } else {
                CompletionItemKind::FUNCTION
            };
            out.push(item(&g, kind, "runtime"));
        }
        // names written here, minus the ones reached through a `.` or a `::`,
        // which are somebody else's fields rather than names in scope
        let mut seen: Vec<String> = Vec::new();
        for (i, t) in scan.tokens.iter().enumerate() {
            let after_dot = i > 0
                && matches!(scan.tokens[i - 1].tok, Tok::Dot | Tok::ColonColon);
            if after_dot {
                continue;
            }
            if let Tok::Name(n) = &t.tok {
                if !seen.iter().any(|s| s == n) && !self.is_global(n) {
                    seen.push(n.clone());
                    out.push(item(n, CompletionItemKind::VARIABLE, "this file"));
                }
            }
        }
        out
    }

    /// Work out where the cursor is standing.
    fn where_is(&self, scan: &rua_syntax::lexer::Scan, text: &str, at: u32) -> Where {
        // a comment swallows everything in it, and the cursor may be at the
        // very end of one that is still being typed
        if scan.comments.iter().any(|c| c.contains(at) || c.hi == at) {
            return Where::Comment;
        }
        let toks = &scan.tokens;
        // inside a string literal, which the lexer hands back whole
        if let Some(i) = toks.iter().position(|t| {
            matches!(t.tok, Tok::Str(_)) && at > t.span.lo && at <= t.span.hi
        }) {
            let body = &text[toks[i].span.lo as usize..at as usize];
            if !in_interpolation(body) {
                // `require("…")` is the one string whose contents are a name
                let opens_require = i >= 2
                    && toks[i - 1].tok == Tok::LParen
                    && matches!(&toks[i - 2].tok, Tok::Name(n) if n == "require");
                return if opens_require {
                    Where::Require(body.trim_start_matches('"').to_string())
                } else {
                    Where::Text
                };
            }
            // inside `{...}`: an expression like any other
        }
        // the token being typed, if the cursor is in the middle of a name
        let typing = toks
            .iter()
            .position(|t| matches!(t.tok, Tok::Name(_)) && at > t.span.lo && at <= t.span.hi);
        let before = match typing {
            Some(i) => i.checked_sub(1),
            None => toks.iter().rposition(|t| t.span.hi <= at && t.tok != Tok::Eof),
        };
        let Some(b) = before else { return Where::Expression };
        match &toks[b].tok {
            Tok::ColonColon => match b.checked_sub(1).map(|j| &toks[j].tok) {
                Some(Tok::Name(module)) => Where::Member(module.clone()),
                _ => Where::Expression,
            },
            Tok::Dot => Where::Method,
            // `fn here`, `let here`, `let mut here` — a name being invented
            Tok::Fn | Tok::Let => Where::Naming,
            Tok::Mut if b > 0 && toks[b - 1].tok == Tok::Let => Where::Naming,
            _ => Where::Expression,
        }
    }

    fn is_global(&self, name: &str) -> bool {
        !matches!(self.vm.get_global(name), rua_core::Value::Nil)
    }

    /// The names in a module, if that global holds a table of them.
    fn members(&self, name: &str) -> Option<Vec<String>> {
        match self.vm.get_global(name) {
            rua_core::Value::Table(t) => {
                let keys = t.borrow().keys();
                Some(keys.iter().map(|k| k.to_value().to_string()).collect())
            }
            _ => None,
        }
    }
}

/// Where the cursor is, which is what decides whether a suggestion is help
/// or noise. `break` is a fine thing to offer at the start of a statement and
/// nonsense after a `.`.
#[derive(Debug, PartialEq)]
enum Where {
    /// Inside a comment. Nothing belongs here.
    Comment,
    /// Inside a string, outside any `{}` in it.
    Text,
    /// Inside the string `require` is being given.
    Require(String),
    /// After `mod::`.
    Member(String),
    /// After a `.`.
    Method,
    /// Naming something that does not exist yet, after `fn` or `let`.
    Naming,
    /// Anywhere a value could be written.
    Expression,
}

/// Is this something rua would lex as one name?
fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        && !KEYWORDS.contains(&s)
}

/// Is the end of this string's text inside a `{...}`?
///
/// `{{` and `}}` are how a literal brace is written, so neither opens or
/// closes one.
fn in_interpolation(body: &str) -> bool {
    let bytes = body.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'{' if bytes.get(i + 1) == Some(&b'{') => i += 1,
            b'}' if bytes.get(i + 1) == Some(&b'}') => i += 1,
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    depth > 0
}

fn item(label: &str, kind: CompletionItemKind, detail: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        detail: Some(detail.to_string()),
        ..Default::default()
    }
}

/// A diagnostic's range: the token if the error named one, the whole line if
/// it could not.
fn span_range(index: &LineIndex, span: rua_syntax::ast::Span, line: u32) -> Range {
    if !span.is_empty() {
        return index.range(span.lo, span.hi);
    }
    let line = line.saturating_sub(1);
    Range {
        start: Position { line, character: 0 },
        end: Position { line, character: u32::MAX },
    }
}

/// Which colour a token gets. What it is lexically decides most of it; the
/// tokens either side decide whether a name is a call, a module or a field.
fn token_kind(toks: &[Lexed], i: usize) -> Option<u32> {
    let next = toks.get(i + 1).map(|t| &t.tok);
    let prev = i.checked_sub(1).and_then(|j| toks.get(j)).map(|t| &t.tok);
    Some(match &toks[i].tok {
        Tok::Eof => return None,
        Tok::Num(_) => NUMBER,
        Tok::Str(_) => STRING,
        Tok::Name(_) => match (prev, next) {
            (Some(Tok::Fn), _) => FUNCTION,
            (_, Some(Tok::ColonColon)) => NAMESPACE,
            (Some(Tok::Dot), _) => PROPERTY,
            (_, Some(Tok::LParen)) => FUNCTION,
            _ => VARIABLE,
        },
        Tok::Break | Tok::Continue | Tok::Else | Tok::False | Tok::Fn | Tok::For | Tok::If
        | Tok::In | Tok::Let | Tok::Loop | Tok::Match | Tok::Mut | Tok::Nil | Tok::Return
        | Tok::True | Tok::While => KEYWORD,
        Tok::LParen | Tok::RParen | Tok::LBrace | Tok::RBrace | Tok::LBracket | Tok::RBracket
        | Tok::Semi | Tok::Comma | Tok::Hash => return None,
        _ => OPERATOR,
    })
}

const KEYWORDS: &[&str] = &[
    "break", "continue", "else", "false", "fn", "for", "if", "in", "let", "loop", "match", "mut",
    "nil", "return", "true", "while",
];

fn keyword_doc(t: &Tok) -> Option<&'static str> {
    Some(match t {
        Tok::Fn => "`fn` — a function. The last expression in its body is its value.",
        Tok::Let => "`let` — bind a name in this scope.",
        Tok::If => "`if` — a condition. It is an expression, so it has a value.",
        Tok::Else => "`else` — the other branch of an `if`.",
        Tok::Match => "`match` — choose by pattern. An expression, like `if`.",
        Tok::While => "`while` — repeat while a condition holds.",
        Tok::For => "`for` — walk a range, an array or an iterator.",
        Tok::Loop => "`loop` — repeat until something `break`s.",
        Tok::Return => "`return` — leave a function with a value.",
        Tok::Break => "`break` — leave the innermost loop.",
        Tok::Continue => "`continue` — start the innermost loop's next turn.",
        Tok::Nil => "`nil` — the absence of a value.",
        Tok::In => "`in` — what a `for` walks.",
        Tok::Mut => "`mut` — allowed on a `let`, and means nothing: every binding is assignable.",
        Tok::True | Tok::False => "A boolean.",
        _ => return None,
    })
}
