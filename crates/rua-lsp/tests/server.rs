//! The analysis the server answers with, exercised without a transport.
//!
//! The protocol itself is `lsp-server`'s business. What is worth testing is
//! that the answers come from the real front end: that a file with two
//! mistakes reports two, that the module list is the runtime's own, and that
//! the ranges point at tokens rather than lines.

use lsp_types::*;

/// Drive the server the way an editor does: open a document, then ask.
fn open(text: &str) -> (rua_lsp::World, Url) {
    let uri = Url::parse("file:///test.rua").expect("a url");
    let mut world = rua_lsp::World::new();
    world.open(uri.clone(), text);
    (world, uri)
}

#[test]
fn every_mistake_is_reported_not_only_the_first() {
    let (world, uri) = open("let a = 1\nlet = 2\nlet c = 3 + * 4\nlet d = 5\n");
    let found = world.diagnostics(&uri);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].message.contains("expected a name"));
    assert!(found[1].message.contains("unexpected `*`"));
    // the second is on the third line, and points at the `*` itself
    assert_eq!(found[1].range.start.line, 2);
    assert_eq!(found[1].range.start.character, 12);
    assert_eq!(found[1].range.end.character, 13, "one character wide");
}

#[test]
fn a_file_that_parses_has_nothing_to_report() {
    let (world, uri) = open("fn add(a, b) { a + b }\nprint(add(1, 2))\n");
    assert!(world.diagnostics(&uri).is_empty());
}

/// The module list is whatever the runtime has, not a copy of it kept here.
/// `fs::open` and `fs::lines` are recent; a hand-written list would not have
/// them.
#[test]
fn completion_after_a_module_is_that_modules_own_names() {
    let (world, uri) = open("fs::\n");
    let at = Position { line: 0, character: 4 };
    let items = match world.complete_at(&uri, at) {
        Some(CompletionResponse::Array(items)) => items,
        other => panic!("expected a list, got {other:?}"),
    };
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    for want in ["read", "write", "lines", "open", "exists"] {
        assert!(labels.contains(&want), "`fs::{want}` missing from {labels:?}");
    }
    assert!(!labels.contains(&"print"), "a global is not a member of `fs`");
}

#[test]
fn hover_on_a_module_lists_what_is_in_it() {
    let (world, uri) = open("fs::read(\"x\")\n");
    let text = world.hover_at(&uri, Position { line: 0, character: 0 }).expect("hover on `fs`");
    assert!(text.contains("module"), "{text}");
    assert!(text.contains("fs::read"), "{text}");
}

#[test]
fn hover_on_a_keyword_says_what_it_does() {
    let (world, uri) = open("fn f() { 1 }\n");
    let text = world.hover_at(&uri, Position { line: 0, character: 1 }).expect("hover on `fn`");
    assert!(text.contains("function"), "{text}");
}

#[test]
fn the_outline_lists_functions_even_when_the_file_does_not_parse() {
    let (world, uri) = open("fn one() { 1 }\nlet = broken\nfn two(a) { a }\n");
    let names = world.symbol_names(&uri);
    assert_eq!(names, vec!["one", "two"], "both, despite the mistake between them");
}

/// Semantic tokens are what the lexer saw, so a name is coloured by what it
/// is used as rather than by a regular expression's guess.
#[test]
fn semantic_tokens_tell_a_call_from_a_module_from_a_field() {
    let (world, uri) = open("fs::read(x.name)\n");
    let kinds = world.token_kinds(&uri);
    // `fs` before `::` is a namespace; `read` before `(` is a function;
    // `name` after `.` is a property; `x` is a plain variable
    assert_eq!(
        kinds,
        vec![
            ("fs".to_string(), SemanticTokenType::NAMESPACE),
            ("read".to_string(), SemanticTokenType::FUNCTION),
            ("x".to_string(), SemanticTokenType::VARIABLE),
            ("name".to_string(), SemanticTokenType::PROPERTY),
        ]
    );
}

/// A string may run over a line break, and a token an editor colours may not.
#[test]
fn a_token_spanning_lines_is_left_to_the_grammar() {
    let (world, uri) = open("let s = \"one\ntwo\"\nlet n = 1\n");
    // the multi-line string is skipped rather than emitted with a length that
    // runs off the end of its line
    let kinds = world.token_kinds(&uri);
    assert!(!kinds.iter().any(|(t, _)| t.contains('\n')), "{kinds:?}");
}
