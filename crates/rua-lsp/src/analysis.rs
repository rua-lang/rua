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
    SemanticTokenType::COMMENT,
    SemanticTokenType::KEYWORD,
    SemanticTokenType::STRING,
    SemanticTokenType::NUMBER,
    SemanticTokenType::OPERATOR,
    SemanticTokenType::VARIABLE,
    SemanticTokenType::FUNCTION,
    SemanticTokenType::NAMESPACE,
    SemanticTokenType::PROPERTY,
    SemanticTokenType::TYPE,
];

const COMMENT: u32 = 0;
const KEYWORD: u32 = 1;
const STRING: u32 = 2;
const NUMBER: u32 = 3;
const OPERATOR: u32 = 4;
const VARIABLE: u32 = 5;
const FUNCTION: u32 = 6;
const NAMESPACE: u32 = 7;
const PROPERTY: u32 = 8;
const TYPE: u32 = 9;

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
        self.describe_in(&toks, i, index.text())
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
        let mut out = Vec::new();
        for (span, kind) in self.coloured(index) {
            let text = index.text()[span.lo as usize..span.hi as usize].to_string();
            out.push((text, TOKEN_TYPES[kind as usize].clone()));
        }
        out
    }

    /// The function whose call the cursor is inside, and which argument it is
    /// on — what an editor shows while a call is being written.
    ///
    /// Only functions written in this file: the runtime's own are closures
    /// with no parameter names to read, and inventing them would be worse
    /// than saying nothing.
    pub fn signature_at(&self, uri: &Url, at: Position) -> Option<SignatureHelp> {
        let index = self.docs.get(uri)?;
        let offset = index.offset(at);
        let scan = Lexer::scan(index.text());
        let toks = &scan.tokens;
        // walk back to the `(` this cursor is inside, counting the commas
        // that belong to it on the way
        let last = toks.iter().rposition(|t| t.span.hi <= offset && t.tok != Tok::Eof)?;
        let (mut depth, mut argument, mut open) = (0i32, 0u32, None);
        for i in (0..=last).rev() {
            match toks[i].tok {
                Tok::RParen | Tok::RBracket | Tok::RBrace => depth += 1,
                Tok::LBracket | Tok::LBrace if depth > 0 => depth -= 1,
                Tok::LParen if depth > 0 => depth -= 1,
                Tok::LParen => {
                    open = Some(i);
                    break;
                }
                Tok::Comma if depth == 0 => argument += 1,
                // a call does not run past the end of a statement
                Tok::Semi | Tok::Fn if depth == 0 => return None,
                _ => {}
            }
        }
        let open = open?;
        let Some(Tok::Name(callee)) = open.checked_sub(1).map(|i| &toks[i].tok) else {
            return None;
        };
        let (params, at_span) = self.parameters_of(&scan, callee)?;
        let label = format!("{callee}({})", params.join(", "));
        let _ = at_span;
        Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                label: label.clone(),
                documentation: None,
                parameters: Some(
                    params
                        .iter()
                        .map(|p| ParameterInformation {
                            label: ParameterLabel::Simple(p.clone()),
                            documentation: None,
                        })
                        .collect(),
                ),
                active_parameter: Some(argument.min(params.len().saturating_sub(1) as u32)),
            }],
            active_signature: Some(0),
            active_parameter: Some(argument),
        })
    }

    /// The parameters of a `fn` written in this file, read from the tokens so
    /// that a file which does not yet parse still answers.
    fn parameters_of(
        &self,
        scan: &rua_syntax::lexer::Scan,
        name: &str,
    ) -> Option<(Vec<String>, rua_syntax::ast::Span)> {
        let toks = &scan.tokens;
        for i in 0..toks.len() {
            if toks[i].tok != Tok::Fn {
                continue;
            }
            let Some(Tok::Name(n)) = toks.get(i + 1).map(|t| &t.tok) else { continue };
            if n != name || toks.get(i + 2).map(|t| &t.tok) != Some(&Tok::LParen) {
                continue;
            }
            let mut params = Vec::new();
            let mut j = i + 3;
            while let Some(t) = toks.get(j) {
                match &t.tok {
                    Tok::RParen => return Some((params, toks[i + 1].span)),
                    Tok::Name(p) => params.push(p.clone()),
                    Tok::Comma => {}
                    _ => return Some((params, toks[i + 1].span)),
                }
                j += 1;
            }
            return Some((params, toks[i + 1].span));
        }
        None
    }

    /// Lay out a whole document.
    ///
    /// One edit replacing everything: the formatter moves whitespace between
    /// tokens, and working out which of those moves is a minimal edit would
    /// cost more than the editor spends applying the whole thing.
    pub fn format(&self, uri: &Url) -> Result<Vec<TextEdit>, String> {
        let index = self.docs.get(uri).ok_or("no such document")?;
        let text = index.text();
        let out = rua_syntax::fmt::format(text).map_err(|e| {
            format!("line {}: {} — a file that does not parse is left alone", e.line, e.message)
        })?;
        if out == text {
            return Ok(Vec::new());
        }
        let end = index.position(text.len() as u32);
        Ok(vec![TextEdit {
            range: Range { start: Position { line: 0, character: 0 }, end },
            new_text: out,
        }])
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
        let (block, syntax) = rua_syntax::parser::parse_recover(index.text());
        let mut out: Vec<Diagnostic> = syntax
            .iter()
            .map(|e| Diagnostic {
                range: span_range(index, e.span, e.line),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("rua".to_string()),
                message: e.message.clone(),
                ..Default::default()
            })
            .collect();
        // Only once it parses. A tree the parser had to guess at says little
        // about types, and a wrong answer here is worse than none.
        if syntax.is_empty() {
            out.extend(rua_syntax::check::check(&block).iter().map(|e| Diagnostic {
                range: span_range(index, e.span, e.line),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("rua".to_string()),
                message: e.message.clone(),
                ..Default::default()
            }));
        }
        out
    }

    pub fn semantic_tokens(&self, p: &SemanticTokensParams) -> Option<SemanticTokensResult> {
        let index = self.docs.get(&p.text_document.uri)?;
        let mut data = Vec::new();
        let (mut last_line, mut last_start) = (0u32, 0u32);
        for (span, kind) in self.coloured(index) {
            let at = index.position(span.lo);
            let end = index.position(span.hi);
            let delta_line = at.line - last_line;
            let delta_start = if delta_line == 0 { at.character - last_start } else { at.character };
            data.push(SemanticToken {
                delta_line,
                delta_start,
                length: end.character - at.character,
                token_type: kind,
                token_modifiers_bitset: 0,
            });
            (last_line, last_start) = (at.line, at.character);
        }
        Some(SemanticTokensResult::Tokens(SemanticTokens { result_id: None, data }))
    }

    /// Every stretch of the document worth colouring, in order.
    ///
    /// A token an editor colours may not straddle two lines and a rua string
    /// or block comment may, so anything that does is cut at the line ends.
    fn coloured(&self, index: &LineIndex) -> Vec<(rua_syntax::ast::Span, u32)> {
        use rua_syntax::ast::Span;
        let scan = Lexer::scan(index.text());
        let types = type_positions(&scan.tokens);
        let mut out: Vec<(Span, u32)> = Vec::new();
        for (i, t) in scan.tokens.iter().enumerate() {
            // a name standing where a type goes is a type, whatever it would
            // have been in a value
            if let (Some(kind), Tok::Name(_)) = (types[i], &t.tok) {
                out.push((t.span, kind));
                continue;
            }
            if let Some(kind) = token_kind(&scan.tokens, i) {
                out.push((t.span, kind));
            }
        }
        // the `#!` line comes back among these, which is what makes it a
        // comment to an editor as well as to a reader
        for c in &scan.comments {
            out.push((*c, COMMENT));
        }
        out.sort_by_key(|(s, _)| s.lo);
        out.retain(|(s, _)| !s.is_empty());
        // cut anything that crosses a line break into one piece per line
        let mut split = Vec::with_capacity(out.len());
        for (span, kind) in out {
            if index.one_line(span.lo, span.hi) {
                split.push((span, kind));
                continue;
            }
            let text = index.text();
            let mut lo = span.lo;
            while lo < span.hi {
                let nl = text[lo as usize..span.hi as usize]
                    .find('\n')
                    .map(|n| lo + n as u32)
                    .unwrap_or(span.hi);
                if nl > lo {
                    split.push((Span::new(lo, nl), kind));
                }
                lo = nl + 1;
            }
        }
        split
    }

    /// The functions in a file, for the outline.    /// The functions in a file, for the outline. Read from the tokens rather
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
        let text = self.describe_in(&toks, i, index.text())?;
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
        self.describe_in(toks, i, "")
    }

    /// The same, with the document's text so the types written in it can be
    /// read. This is what makes a program document itself: the annotation is
    /// the documentation, and hovering is how you read it.
    fn describe_in(&self, toks: &[Lexed], i: usize, text: &str) -> Option<String> {
        if let Some(word) = keyword_doc(&toks[i].tok) {
            return Some(word.to_string());
        }
        let Tok::Name(name) = &toks[i].tok else { return None };
        if !text.is_empty() {
            if let Some(said) = self.what_the_file_says(name, toks, i, text) {
                return Some(said);
            }
        }
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

    /// What this file itself says about a name: the type written beside it,
    /// the signature it was declared with, or the shape it stands for.
    fn what_the_file_says(
        &self,
        name: &str,
        toks: &[Lexed],
        i: usize,
        text: &str,
    ) -> Option<String> {
        let (block, _) = rua_syntax::parser::parse_recover(text);
        let types = crate::types::Types::read(&block);

        // a type's own name: show what it stands for, with its parameters
        if let Some(body) = types.alias(name) {
            let params = types.type_params(name);
            let head = if params.is_empty() {
                name.to_string()
            } else {
                let ps: Vec<String> = params.iter().map(|p| p.to_string()).collect();
                format!("{name}<{}>", ps.join(", "))
            };
            return Some(format!("```rust
type {head} = {body}
```"));
        }

        // a function written here: its whole signature, which is the whole
        // of what a reader would have opened the file for
        if let Some(sig) = types.function(name) {
            let mut out = format!("```rust
fn {}
```", sig.label());
            let described: Vec<String> = sig
                .params
                .iter()
                .filter_map(|p| {
                    let t = p.ty.as_ref()?;
                    Some(format!("- `{p}` — {}", self.explain(&types, t)))
                })
                .collect();
            if !described.is_empty() {
                out.push_str("

");
                out.push_str(&described.join("
"));
            }
            return Some(out);
        }

        // a name with a type written beside it, and what that type stands for
        let decl = rua_syntax::resolve::occurrences(&block)
            .into_iter()
            .find(|o| o.span == toks[i].span)
            .and_then(|o| o.decl)?;
        let ty = types.at(decl)?;
        Some(format!("```rust
{name}: {ty}
```

{}", self.explain(&types, ty)))
    }

    /// A type, followed down to the shape it stands for when that says more
    /// than the name does.
    fn explain(&self, types: &crate::types::Types, ty: &rua_syntax::ast::Type) -> String {
        let filled = types.instantiate(ty);
        let shown = filled.to_string();
        if shown == ty.to_string() {
            return shown;
        }
        format!("`{ty}` is `{shown}`")
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
            // where a type is written, only types make sense
            Where::TypeName => {
                let types = self.types_of(index.text());
                let mut out: Vec<CompletionItem> = BUILTIN_TYPES
                    .iter()
                    .map(|t| item(t, CompletionItemKind::TYPE_PARAMETER, "built in"))
                    .collect();
                let mut names = types.alias_names();
                names.sort();
                for n in names {
                    let detail = types.alias(&n).map(|t| t.to_string()).unwrap_or_default();
                    out.push(item(&n, CompletionItemKind::STRUCT, &detail));
                }
                out
            }
            // `let o: Point = #{ ` — the fields Point says it has
            Where::Field(shape) => {
                let types = self.types_of(index.text());
                let Some(alias) = types.alias(&shape) else { return Some(CompletionResponse::Array(Vec::new())) };
                let Some(fields) = types.fields(alias) else {
                    return Some(CompletionResponse::Array(Vec::new()))
                };
                fields
                    .iter()
                    .map(|(name, t)| {
                        let mut it = item(name, CompletionItemKind::FIELD, &t.to_string());
                        // writing the key is writing `name: `
                        it.insert_text = Some(format!("{name}: "));
                        it
                    })
                    .collect()
            }
            // inside a call: say what this argument is for, then the names
            Where::Argument(callee, index_of) => {
                let types = self.types_of(index.text());
                let mut out = Vec::new();
                if let Some(sig) = types.function(&callee) {
                    if let Some(p) = sig.params.get(index_of) {
                        let detail = match &p.ty {
                            // follow a generic down to what it stands for:
                            // `Handler<Body, Reply>` says less than the
                            // function it turns out to be
                            Some(t) => {
                                let filled = types.instantiate(t);
                                let shown = filled.to_string();
                                if shown == t.to_string() {
                                    format!("{} of {} — {t}", ordinal(index_of), sig.label())
                                } else {
                                    format!(
                                        "{} of {} — {t} = {shown}",
                                        ordinal(index_of),
                                        sig.label()
                                    )
                                }
                            }
                            None => format!("{} of {}", ordinal(index_of), sig.label()),
                        };
                        let mut it = item(p, CompletionItemKind::VALUE, &detail);
                        it.preselect = Some(true);
                        it.sort_text = Some(format!("0{p}"));
                        out.push(it);
                    }
                }
                out.extend(self.in_scope(&scan, index.text()));
                out
            }
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
            Where::Expression => self.in_scope(&scan, index.text()),
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

    /// The types this file declares and the annotations it wrote down.
    fn types_of(&self, text: &str) -> crate::types::Types {
        let (block, _) = rua_syntax::parser::parse_recover(text);
        crate::types::Types::read(&block)
    }

    /// Keywords, what the runtime provides, and the names this file declares
    /// — each with the type it was given, when one was written.
    fn in_scope(&self, scan: &rua_syntax::lexer::Scan, text: &str) -> Vec<CompletionItem> {
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
                }
            }
        }
        // a name is worth more with the type it was given beside it, and a
        // function is worth more as its signature
        let (block, _) = rua_syntax::parser::parse_recover(text);
        let types = crate::types::Types::read(&block);
        let written: std::collections::HashMap<String, String> =
            rua_syntax::resolve::occurrences(&block)
                .into_iter()
                .filter_map(|o| {
                    let decl = o.decl?;
                    Some((o.name.to_string(), types.at(decl)?.to_string()))
                })
                .collect();
        for n in seen {
            let (kind, detail) = match (types.function(&n), written.get(&n)) {
                (Some(sig), _) => (CompletionItemKind::FUNCTION, sig.label()),
                (_, Some(t)) => (CompletionItemKind::VARIABLE, t.clone()),
                _ => (CompletionItemKind::VARIABLE, "this file".to_string()),
            };
            out.push(item(&n, kind, &detail));
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
        // a type is written after `:`, after `->`, and after `type X =`
        if self.writing_a_type(toks, b) {
            return Where::TypeName;
        }
        if let Some(w) = self.inside_a_call_or_record(toks, b, typing) {
            return w;
        }
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

    /// Is the token after `b` a type rather than a value?
    fn writing_a_type(&self, toks: &[Lexed], b: usize) -> bool {
        match &toks[b].tok {
            // `-> here`
            Tok::Arrow => true,
            // `type X = here`
            Tok::Assign => {
                b >= 2 && toks[b - 2].tok == Tok::Type
            }
            // `let x: here`, `fn f(a: here)`, `#{ x: here }` in a type — a
            // `:` in a value is a map key, whose value is a value
            Tok::Colon => self.colon_introduces_a_type(toks, b),
            // part way into writing one
            Tok::Comma | Tok::LBracket | Tok::Lt => {
                b > 0 && self.writing_a_type(toks, b - 1)
            }
            _ => false,
        }
    }

    /// A `:` writes a type in a binding and a value in a map literal. Which
    /// one it is depends on what opened before it.
    fn colon_introduces_a_type(&self, toks: &[Lexed], colon: usize) -> bool {
        // walk out to whatever bracket this sits inside
        let (mut depth, mut i) = (0i32, colon);
        while i > 0 {
            i -= 1;
            match toks[i].tok {
                Tok::RParen | Tok::RBrace | Tok::RBracket => depth += 1,
                Tok::LParen if depth == 0 => {
                    // `fn f(a: T)` — a parameter list, if a `fn` opened it
                    return i > 0 && matches!(toks[i - 1].tok, Tok::Name(_))
                        && i > 1
                        && toks[i - 2].tok == Tok::Fn;
                }
                Tok::LBrace if depth == 0 => {
                    // `#{ x: T }` inside a `type` is a shape; in a value it
                    // is a map, and its values are values
                    return i > 0
                        && toks[i - 1].tok == Tok::Hash
                        && self.in_a_type_declaration(toks, i);
                }
                Tok::LBracket if depth == 0 => return false,
                Tok::LParen | Tok::LBrace | Tok::LBracket => depth -= 1,
                // a `let x: T` never crosses a statement
                Tok::Semi | Tok::Let | Tok::Type if depth == 0 => {
                    return toks[i].tok != Tok::Semi;
                }
                _ => {}
            }
        }
        false
    }

    /// Is this position inside a `type X = ...`?
    fn in_a_type_declaration(&self, toks: &[Lexed], from: usize) -> bool {
        for i in (0..from).rev() {
            match toks[i].tok {
                Tok::Type => return true,
                Tok::Semi | Tok::Let | Tok::Fn | Tok::RBrace => return false,
                _ => {}
            }
        }
        false
    }

    /// Inside a call's arguments, or inside a `#{ }` whose shape is known.
    fn inside_a_call_or_record(
        &self,
        toks: &[Lexed],
        b: usize,
        typing: Option<usize>,
    ) -> Option<Where> {
        let from = typing.unwrap_or(b + 1);
        let (mut depth, mut i) = (0i32, from);
        while i > 0 {
            i -= 1;
            match &toks[i].tok {
                Tok::RParen | Tok::RBrace | Tok::RBracket => depth += 1,
                Tok::LBrace if depth == 0 && i > 0 && toks[i - 1].tok == Tok::Hash => {
                    // `#{ ` — the shape comes from what it is being given to
                    return self.shape_of_map_at(toks, i - 1).map(Where::Field);
                }
                Tok::LParen if depth == 0 => {
                    let Some(Tok::Name(callee)) = i.checked_sub(1).map(|j| &toks[j].tok) else {
                        return None;
                    };
                    // only the commas of this call: `pair(f(1, 2), ` is on
                    // its second argument, not its fourth
                    let (mut inner, mut commas) = (0i32, 0usize);
                    for t in &toks[i + 1..from] {
                        match t.tok {
                            Tok::LParen | Tok::LBrace | Tok::LBracket => inner += 1,
                            Tok::RParen | Tok::RBrace | Tok::RBracket => inner -= 1,
                            Tok::Comma if inner == 0 => commas += 1,
                            _ => {}
                        }
                    }
                    return Some(Where::Argument(callee.clone(), commas));
                }
                Tok::LParen | Tok::LBrace | Tok::LBracket => depth -= 1,
                Tok::Semi | Tok::Fn if depth == 0 => return None,
                _ => {}
            }
        }
        None
    }

    /// The name of the shape a `#{` is being written as: from the annotation
    /// on the binding it is assigned to, or the parameter it is passed as.
    fn shape_of_map_at(&self, toks: &[Lexed], hash: usize) -> Option<String> {
        // `let o: Point = #{`
        if hash >= 2 && toks[hash - 1].tok == Tok::Assign {
            for i in (0..hash - 1).rev() {
                match &toks[i].tok {
                    Tok::Name(t) if i > 0 && toks[i - 1].tok == Tok::Colon => {
                        return Some(t.clone())
                    }
                    Tok::Let => return None,
                    Tok::Semi => return None,
                    _ => {}
                }
            }
        }
        None
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
    /// Where a type is written: after a `:`, after a `->`, or in a `type`.
    TypeName,
    /// Inside `#{ }` where the shape is known — the fields it should have.
    Field(String),
    /// An argument of a call: which function, and which parameter.
    Argument(String, usize),
    /// Anywhere a value could be written.
    Expression,
}

/// `first`, `second`, `third` … for saying which argument is being written.
fn ordinal(i: usize) -> &'static str {
    const WORDS: &[&str] = &[
        "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth",
    ];
    WORDS.get(i).copied().unwrap_or("later argument")
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

/// Which tokens stand where a type is written.
///
/// A type begins after `type X =`, after a `->`, and after a `:` that
/// introduces one rather than a map's value. It ends where the thing holding
/// it does: at the `=` of a `let`, at the `,` before the next parameter, at
/// the `{` that opens a body, or at the bracket that closes around it.
fn type_positions(toks: &[Lexed]) -> Vec<Option<u32>> {
    let mut out: Vec<Option<u32>> = vec![None; toks.len()];
    let (mut depth, mut base, mut inside) = (0i32, 0i32, false);
    let mut i = 0;
    while i < toks.len() {
        let tok = &toks[i].tok;
        // `type X` and `type X<T, U>` — the name and its parameters are types
        if *tok == Tok::Type {
            let mut j = i + 1;
            if matches!(toks.get(j).map(|t| &t.tok), Some(Tok::Name(_))) {
                out[j] = Some(TYPE);
                j += 1;
            }
            if toks.get(j).map(|t| &t.tok) == Some(&Tok::Lt) {
                j += 1;
                while j < toks.len() && toks[j].tok != Tok::Gt {
                    if matches!(toks[j].tok, Tok::Name(_)) {
                        out[j] = Some(TYPE);
                    }
                    j += 1;
                }
                j += 1;
            }
            if toks.get(j).map(|t| &t.tok) == Some(&Tok::Assign) {
                inside = true;
                base = depth;
                i = j + 1;
                continue;
            }
            i = j;
            continue;
        }
        if !inside {
            let opens = match tok {
                Tok::Arrow => true,
                Tok::Colon => colon_before_a_type(toks, i),
                _ => false,
            };
            if opens {
                inside = true;
                base = depth;
                i += 1;
                continue;
            }
        }
        // a bare `{` inside a type is not part of it: it opens a body
        if inside && *tok == Tok::LBrace && (i == 0 || toks[i - 1].tok != Tok::Hash) {
            inside = false;
        }
        let closing = matches!(tok, Tok::RParen | Tok::RBracket | Tok::RBrace | Tok::Gt);
        match tok {
            Tok::LParen | Tok::LBracket | Tok::LBrace | Tok::Lt => depth += 1,
            _ if closing => depth -= 1,
            _ => {}
        }
        if inside {
            match tok {
                // the type is over: a value follows, or the next parameter
                Tok::Assign | Tok::Semi => inside = false,
                Tok::Comma if depth == base => inside = false,
                // a bracket that closes back to where the type began ends it
                _ if closing && depth <= base => inside = false,
                // `x: number` inside a shape names a field, and `route:
                // string` inside a function type names an argument — neither
                // is a type, however much it looks like one
                Tok::Name(_)
                    if toks.get(i + 1).map(|t| &t.tok) == Some(&Tok::Colon) =>
                {
                    out[i] = Some(PROPERTY)
                }
                Tok::Name(_) => out[i] = Some(TYPE),
                _ => {}
            }
        }
        i += 1;
    }
    out
}

/// Does this `:` introduce a type, or a map literal's value?
fn colon_before_a_type(toks: &[Lexed], colon: usize) -> bool {
    let (mut depth, mut i) = (0i32, colon);
    while i > 0 {
        i -= 1;
        match toks[i].tok {
            Tok::RParen | Tok::RBrace | Tok::RBracket => depth += 1,
            // `fn f(a: T)` — a parameter list belongs to a `fn`
            Tok::LParen if depth == 0 => {
                return i >= 2
                    && matches!(toks[i - 1].tok, Tok::Name(_))
                    && toks[i - 2].tok == Tok::Fn;
            }
            // `#{ x: T }` is a shape only inside a type
            Tok::LBrace if depth == 0 => return false,
            Tok::LBracket if depth == 0 => return false,
            Tok::LParen | Tok::LBrace | Tok::LBracket => depth -= 1,
            Tok::Let if depth == 0 => return true,
            Tok::Semi if depth == 0 => return false,
            _ => {}
        }
    }
    false
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
        | Tok::True | Tok::Type | Tok::While => KEYWORD,
        Tok::LParen | Tok::RParen | Tok::LBrace | Tok::RBrace | Tok::LBracket | Tok::RBracket
        | Tok::Semi | Tok::Comma | Tok::Hash => return None,
        _ => OPERATOR,
    })
}

const KEYWORDS: &[&str] = &[
    "break", "continue", "else", "false", "fn", "for", "if", "in", "let", "loop", "match", "mut",
    "nil", "return", "true", "type", "while",
];

/// The types that need no declaring, offered where a type is written.
const BUILTIN_TYPES: &[&str] =
    &["number", "string", "boolean", "nil", "table", "function", "any"];

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
        Tok::Type => "`type` — give a name to a shape: `type Point = #{ x: number, y: number }`.",
        Tok::True | Tok::False => "A boolean.",
        _ => return None,
    })
}
