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

// ---- go to definition, references and rename -------------------------------

fn at(line: u32, character: u32) -> Position {
    Position { line, character }
}

/// The declaration of a local, from a mention of it.
#[test]
fn definition_finds_where_a_local_came_into_scope() {
    let (world, uri) = open("fn f(a) {\n    let total = a + 1\n    total * 2\n}\n");
    // the `total` on the third line
    let decl = world.definition_at(&uri, at(2, 5)).expect("a definition");
    assert_eq!(decl.range.start, at(1, 8), "the `total` in the `let`");
    assert_eq!(decl.range.end, at(1, 13));
    // and the parameter, from its use
    let param = world.definition_at(&uri, at(1, 16)).expect("a definition");
    assert_eq!(param.range.start, at(0, 5), "the `a` in the parameter list");
}

/// A top level `fn` is a global, and this file is where it was written, so a
/// call to it can still be followed — including from above the definition.
#[test]
fn definition_finds_a_function_defined_later_in_the_file() {
    let (world, uri) = open("fn main() {\n    helper(1)\n}\nfn helper(x) { x }\n");
    let decl = world.definition_at(&uri, at(1, 4)).expect("a definition");
    assert_eq!(decl.range.start, at(3, 3), "the name in `fn helper`");
}

/// The thing this has to get right. Two locals spelled the same are not the
/// same variable, and neither renaming nor go-to-definition may confuse them.
#[test]
fn shadowing_is_not_confused_by_a_shared_spelling() {
    let src = "fn f() {\n    let x = 1\n    {\n        let x = 2\n        print(x)\n    }\n    print(x)\n}\n";
    let (world, uri) = open(src);
    // the `x` printed inside the inner block belongs to the inner `let`
    let inner = world.definition_at(&uri, at(4, 14)).expect("a definition");
    assert_eq!(inner.range.start, at(3, 12), "the inner `let x`");
    // the one printed after the block belongs to the outer
    let outer = world.definition_at(&uri, at(6, 10)).expect("a definition");
    assert_eq!(outer.range.start, at(1, 8), "the outer `let x`");
    // and each has exactly two mentions: its declaration and its one use
    assert_eq!(world.references_at(&uri, at(4, 14)).len(), 2);
    assert_eq!(world.references_at(&uri, at(6, 10)).len(), 2);
}

/// Renaming touches every mention of one variable and nothing that merely
/// shares its name.
#[test]
fn rename_changes_one_variable_and_leaves_the_other_alone() {
    let src = "fn f() {\n    let x = 1\n    {\n        let x = 2\n        print(x)\n    }\n    print(x)\n}\n";
    let (world, uri) = open(src);
    let edit = world.rename_at(&uri, at(6, 10), "outer").expect("a rename");
    let edits = &edit.changes.expect("changes")[&uri];
    assert_eq!(edits.len(), 2, "the outer declaration and its one use");
    let mut lines: Vec<u32> = edits.iter().map(|e| e.range.start.line).collect();
    lines.sort();
    assert_eq!(lines, vec![1, 6], "not the inner `x` on lines 3 and 4");
    assert!(edits.iter().all(|e| e.new_text == "outer"));
}

/// A closure that reads a variable from around it is reading the same
/// variable, so renaming has to follow it in there.
#[test]
fn rename_follows_a_name_captured_by_a_closure() {
    let src = "fn f() {\n    let count = 0\n    let bump = || { count + 1 }\n    bump()\n}\n";
    let (world, uri) = open(src);
    let refs = world.references_at(&uri, at(1, 8));
    assert_eq!(refs.len(), 2, "the declaration and the use inside the closure");
    let edit = world.rename_at(&uri, at(1, 8), "n").expect("a rename");
    let edits = &edit.changes.expect("changes")[&uri];
    assert_eq!(edits.len(), 2);
    assert!(edits.iter().any(|e| e.range.start.line == 2), "the one inside the closure");
}

/// Renaming what this file did not declare would leave the declaration
/// behind, so the server declines rather than half-doing it.
#[test]
fn a_name_from_outside_the_file_is_not_renamed() {
    let (world, uri) = open("print(1)\n");
    assert!(world.prepare_rename_at(&uri, at(0, 2)).is_none(), "`print` is not ours");
    let refused = world.rename_at(&uri, at(0, 2), "shout");
    assert!(refused.is_err(), "{refused:?}");
    assert!(refused.unwrap_err().contains("not declared in this file"));
}

/// A rename has to produce something rua would lex as a name.
#[test]
fn a_new_name_that_is_not_a_name_is_refused() {
    let (world, uri) = open("fn f() {\n    let x = 1\n    x\n}\n");
    for bad in ["2x", "a b", "", "let", "x-y"] {
        let out = world.rename_at(&uri, at(1, 8), bad);
        assert!(out.is_err(), "`{bad}` should be refused");
    }
    assert!(world.rename_at(&uri, at(1, 8), "_ok2").is_ok());
}

/// A parameter is declared by the function, and renaming it takes the body
/// with it.
#[test]
fn rename_covers_a_parameter_and_its_uses() {
    let (world, uri) = open("fn add(a, b) {\n    a + b + a\n}\n");
    let edit = world.rename_at(&uri, at(0, 7), "left").expect("a rename");
    let edits = &edit.changes.expect("changes")[&uri];
    assert_eq!(edits.len(), 3, "the parameter and its two uses");
}

/// Highlighting under the cursor is the same question as references, so it
/// has to answer the same way when a name is shadowed.
#[test]
fn references_of_a_loop_variable_stay_inside_the_loop() {
    let src = "fn f(xs) {\n    for i in 0..3 { print(i) }\n    let i = 9\n    i\n}\n";
    let (world, uri) = open(src);
    let loop_var = world.references_at(&uri, at(1, 8));
    assert_eq!(loop_var.len(), 2, "the loop's `i` and the one printed");
    let after = world.references_at(&uri, at(3, 4));
    assert_eq!(after.len(), 2, "the later `let i` and the tail that reads it");
    assert!(loop_var.iter().all(|l| l.range.start.line == 1));
}
