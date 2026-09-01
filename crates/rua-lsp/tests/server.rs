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
    // the names only: punctuation's kind is not what this is about
    let kinds: Vec<(String, SemanticTokenType)> = world
        .token_kinds(&uri)
        .into_iter()
        .filter(|(t, _)| t.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .collect();
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

/// A token an editor colours may not straddle two lines, and a rua string or
/// block comment may. Those are cut at the line ends rather than dropped.
#[test]
fn a_token_spanning_lines_is_cut_at_the_line_ends() {
    let (world, uri) = open("let s = \"one\ntwo\"\n/* a\n   b */\n");
    let kinds = world.token_kinds(&uri);
    assert!(!kinds.iter().any(|(t, _)| t.contains('\n')), "{kinds:?}");
    // both halves of each survive
    let text: Vec<&str> = kinds.iter().map(|(t, _)| t.as_str()).collect();
    assert!(text.contains(&"\"one"), "{text:?}");
    assert!(text.contains(&"two\""), "{text:?}");
    assert!(text.contains(&"/* a"), "{text:?}");
}

/// A `#!` line is stepped over before the lexer starts, and it is still a
/// comment to everyone who reads the file: it colours as one, and nothing is
/// offered inside it.
#[test]
fn a_shebang_is_a_comment() {
    let (world, uri) = open("#!/usr/bin/env rua\nlet a = 1\n");
    let kinds = world.token_kinds(&uri);
    assert_eq!(
        kinds.first().map(|(t, k)| (t.as_str(), k.clone())),
        Some(("#!/usr/bin/env rua", SemanticTokenType::COMMENT)),
        "{kinds:?}"
    );
    assert!(labels(&world, &uri, at(0, 10)).is_empty(), "nothing to complete in a shebang");
    // and the line under it is ordinary code
    assert!(!labels(&world, &uri, at(1, 9)).is_empty());
}

/// Comments colour through the server too, so an editor with no grammar for
/// rua still greys them.
#[test]
fn comments_are_coloured() {
    let (world, uri) = open("let a = 1 // why\n");
    let kinds = world.token_kinds(&uri);
    assert!(
        kinds.iter().any(|(t, k)| t == "// why" && *k == SemanticTokenType::COMMENT),
        "{kinds:?}"
    );
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

// ---- completion knows where the cursor is ----------------------------------

fn labels(world: &rua_lsp::World, uri: &Url, at: Position) -> Vec<String> {
    match world.complete_at(uri, at) {
        Some(CompletionResponse::Array(items)) => items.into_iter().map(|i| i.label).collect(),
        _ => Vec::new(),
    }
}

/// After a `.` the keywords are noise: `break` is not a method.
#[test]
fn a_method_position_offers_methods_and_not_keywords() {
    let (world, uri) = open("let t = []\nt.\n");
    let got = labels(&world, &uri, at(1, 2));
    assert!(!got.is_empty(), "something should be offered");
    for keyword in ["break", "return", "let", "fn", "while"] {
        assert!(!got.contains(&keyword.to_string()), "`{keyword}` offered after a `.`");
    }
    // the runtime's own table and string methods
    assert!(got.contains(&"push".to_string()), "{got:?}");
    assert!(got.contains(&"len".to_string()), "{got:?}");
    // and not the globals either, which are not reached through a dot
    assert!(!got.contains(&"print".to_string()));
}

/// After `mod::` only that module's names, and nothing at all for a name that
/// is not a module — better silence than every global pretending to be in it.
#[test]
fn a_module_position_offers_only_that_module() {
    let (world, uri) = open("fs::\n");
    let got = labels(&world, &uri, at(0, 4));
    assert!(got.contains(&"read".to_string()), "{got:?}");
    assert!(!got.contains(&"print".to_string()), "a global is not inside `fs`");
    assert!(!got.contains(&"break".to_string()), "a keyword is not inside `fs`");

    let (world, uri) = open("notamodule::\n");
    assert!(labels(&world, &uri, at(0, 12)).is_empty(), "nothing is known to be in it");
}

/// Inside a comment nothing is worth offering, and a comment being typed
/// counts from its first character to the cursor.
#[test]
fn nothing_is_offered_inside_a_comment() {
    let (world, uri) = open("// a note about b\nlet b = 1\n/* and\n   a longer one */\n");
    assert!(labels(&world, &uri, at(0, 10)).is_empty(), "inside a line comment");
    assert!(labels(&world, &uri, at(0, 17)).is_empty(), "at the end of one");
    assert!(labels(&world, &uri, at(3, 8)).is_empty(), "inside a block comment");
    // but the line between them is ordinary code
    assert!(!labels(&world, &uri, at(1, 9)).is_empty(), "the code between them");
}

/// A string is text, not code — until the `{}` in it, which is code again.
#[test]
fn a_string_is_text_but_its_interpolation_is_not() {
    let (world, uri) = open("let who = \"you\"\nlet s = \"hello there\"\nlet t = \"hi {who}\"\n");
    assert!(labels(&world, &uri, at(1, 14)).is_empty(), "inside the text");
    // inside `{` ... `}` the ordinary names come back
    let inside = labels(&world, &uri, at(2, 16));
    assert!(inside.contains(&"who".to_string()), "{inside:?}");
    assert!(inside.contains(&"print".to_string()), "globals too");
}

/// `require` is given a file name, so that is what to offer — and the file
/// doing the requiring is not one of them.
#[test]
fn require_offers_the_files_beside_this_one() {
    let dir = std::env::temp_dir().join(format!("rua-req-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    for f in ["json.rua", "http.rua", "notes.txt"] {
        std::fs::write(dir.join(f), "").unwrap();
    }
    std::fs::write(dir.join("lib").join("vec2.rua"), "").unwrap();
    let uri = Url::from_file_path(dir.join("main.rua")).unwrap();
    let mut world = rua_lsp::World::new();
    world.open(uri.clone(), "let j = require(\"\")\n");

    let got = labels(&world, &uri, at(0, 17));
    assert!(got.contains(&"json".to_string()), "{got:?}");
    assert!(got.contains(&"http".to_string()), "{got:?}");
    assert!(got.contains(&"lib/".to_string()), "a directory to descend into");
    assert!(!got.iter().any(|g| g.ends_with(".rua")), "require adds the extension itself");
    assert!(!got.contains(&"notes".to_string()), "not a rua file");
    assert!(!got.contains(&"print".to_string()), "a global is not a module");

    // and inside that directory
    world.open(uri.clone(), "let v = require(\"lib/\")\n");
    let inner = labels(&world, &uri, at(0, 21));
    assert!(inner.contains(&"lib/vec2".to_string()), "{inner:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name being invented has nothing to complete to.
#[test]
fn naming_something_new_offers_nothing() {
    let (world, uri) = open("fn \nlet \nlet mut \n");
    assert!(labels(&world, &uri, at(0, 3)).is_empty(), "after `fn`");
    assert!(labels(&world, &uri, at(1, 4)).is_empty(), "after `let`");
    assert!(labels(&world, &uri, at(2, 8)).is_empty(), "after `let mut`");
}

/// Half a name is still that position: `fs::re` is a module position, and
/// `t.pu` a method one.
#[test]
fn a_name_being_typed_keeps_the_position_it_is_in() {
    let (world, uri) = open("fs::re\n");
    let got = labels(&world, &uri, at(0, 6));
    assert!(got.contains(&"read".to_string()), "{got:?}");
    assert!(!got.contains(&"print".to_string()));

    let (world, uri) = open("let t = []\nt.pu\n");
    let got = labels(&world, &uri, at(1, 4));
    assert!(got.contains(&"push".to_string()), "{got:?}");
    assert!(!got.contains(&"break".to_string()));
}

/// At the start of a statement everything is fair game.
#[test]
fn an_expression_position_offers_keywords_globals_and_local_names() {
    let (world, uri) = open("fn helper(a) { a }\nlet total = 1\n\n");
    let got = labels(&world, &uri, at(2, 0));
    for want in ["let", "while", "print", "fs", "helper", "total"] {
        assert!(got.contains(&want.to_string()), "`{want}` missing from {got:?}");
    }
    // a field name reached through a dot is not a name in scope
    let (world, uri) = open("let p = #{}\nlet n = p.width\n\n");
    let got = labels(&world, &uri, at(2, 0));
    assert!(!got.contains(&"width".to_string()), "a field is not a variable");
}

// ---- signature help --------------------------------------------------------

/// While a call is being written, say what it takes and which one is being
/// written now.
#[test]
fn signature_help_names_the_parameters_and_the_one_being_written() {
    let (world, uri) = open("fn add(left, right) { left + right }\nlet n = add(1, 2)\n");
    let sig = world.signature_at(&uri, at(1, 12)).expect("inside the call");
    assert_eq!(sig.signatures[0].label, "add(left, right)");
    assert_eq!(sig.active_parameter, Some(0), "on the first argument");

    let second = world.signature_at(&uri, at(1, 15)).expect("after the comma");
    assert_eq!(second.active_parameter, Some(1), "on the second");
}

/// A call with nothing around it is not a call, and neither is a function the
/// runtime provides — those are closures with no parameter names to read, and
/// inventing some would be worse than saying nothing.
#[test]
fn signature_help_says_nothing_it_does_not_know() {
    let (world, uri) = open("fn add(a, b) { a + b }\nlet n = 1\nprint(n)\n");
    assert!(world.signature_at(&uri, at(1, 9)).is_none(), "not inside a call");
    assert!(world.signature_at(&uri, at(2, 6)).is_none(), "`print` is the runtime's");
}

/// Calls inside calls: the innermost one is the one being written.
#[test]
fn signature_help_follows_a_call_inside_a_call() {
    let (world, uri) = open("fn outer(a) { a }\nfn inner(x, y) { x }\nlet n = outer(inner(1, 2))\n");
    let sig = world.signature_at(&uri, at(2, 24)).expect("inside inner");
    assert_eq!(sig.signatures[0].label, "inner(x, y)");
    assert_eq!(sig.active_parameter, Some(1));
    let out = world.signature_at(&uri, at(2, 14)).expect("inside outer");
    assert_eq!(out.signatures[0].label, "outer(a)");
}

/// Formatting comes back as one edit over the whole file, and nothing at all
/// when there is nothing to change.
#[test]
fn formatting_replaces_the_document_or_says_nothing() {
    let (world, uri) = open("fn f( a ){a+1}\n");
    let edits = world.format(&uri).expect("an edit");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "fn f(a) { a + 1 }\n");

    let (world, uri) = open("fn f(a) { a + 1 }\n");
    assert!(world.format(&uri).unwrap().is_empty(), "already laid out");

    let (world, uri) = open("let x = @\n");
    assert!(world.format(&uri).is_err(), "a file that does not lex is left alone");
}

// ---- completion reads the types that were written --------------------------

fn detail(world: &rua_lsp::World, uri: &Url, at: Position, label: &str) -> String {
    match world.complete_at(uri, at) {
        Some(CompletionResponse::Array(items)) => items
            .into_iter()
            .find(|i| i.label == label)
            .and_then(|i| i.detail)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// The one that otherwise sends you to the source: what does this argument
/// want? The parameter's name, its type, and where it sits in the call.
#[test]
fn an_argument_position_says_what_the_argument_is_for() {
    let src = "type Point = #{ x: number, y: number }\n\
               fn place(spot: Point, label: string) -> boolean { spot.x > 0 }\n\
               let ok = place(";
    let (world, uri) = open(src);
    let got = labels(&world, &uri, at(2, 15));
    assert_eq!(got.first().map(String::as_str), Some("spot"), "the parameter comes first");
    let d = detail(&world, &uri, at(2, 15), "spot");
    assert!(d.contains("first of"), "{d}");
    assert!(d.contains("place(spot: Point, label: string) -> boolean"), "{d}");
    assert!(d.contains("— Point"), "the type it wants: {d}");
    assert!(d.contains("#{ x: number, y: number }"), "followed down to the shape: {d}");
}

/// The second argument is the second one, however long the first was.
#[test]
fn the_argument_being_written_is_the_one_described() {
    let src = "fn pair(left: number, right: string) -> nil { }\nlet x = pair(f(1, 2), ";
    let (world, uri) = open(src);
    let d = detail(&world, &uri, at(1, 22), "right");
    assert!(d.contains("second of"), "{d}");
    assert!(d.ends_with("string"), "{d}");
}

/// Filling in a shape offers the fields that shape says it has, and nothing
/// else — no keywords, no globals, no guessing.
#[test]
fn a_typed_map_offers_the_fields_its_type_declares() {
    let src = "type Colour = #{ red: number, green: number, blue: number }\nlet c: Colour = #{ ";
    let (world, uri) = open(src);
    let got = labels(&world, &uri, at(1, 19));
    assert_eq!(got, vec!["red", "green", "blue"], "in the order they were declared");
    assert_eq!(detail(&world, &uri, at(1, 19), "red"), "number");
    // and it writes the key, not just the name
    match world.complete_at(&uri, at(1, 19)) {
        Some(CompletionResponse::Array(items)) => {
            assert_eq!(items[0].insert_text.as_deref(), Some("red: "));
        }
        other => panic!("{other:?}"),
    }
}

/// Where a type is written, only types are offered.
#[test]
fn a_type_position_offers_types_and_nothing_else() {
    let (world, uri) = open("type Point = #{ x: number }\nlet p: ");
    let got = labels(&world, &uri, at(1, 7));
    assert!(got.contains(&"number".to_string()), "{got:?}");
    assert!(got.contains(&"Point".to_string()), "a type this file declared");
    assert!(!got.contains(&"let".to_string()), "a keyword is not a type");
    assert!(!got.contains(&"print".to_string()), "a global is not a type");
    assert_eq!(detail(&world, &uri, at(1, 7), "Point"), "#{ x: number }", "shown expanded");

    // the same after `->`
    let (world, uri) = open("type Point = #{ x: number }\nfn f() -> ");
    assert!(labels(&world, &uri, at(1, 10)).contains(&"Point".to_string()));
}

/// A `:` writes a type in a binding and a value in a map literal, and the two
/// must not be confused.
#[test]
fn a_colon_in_a_map_is_not_a_type_position() {
    let (world, uri) = open("let total = 1\nlet m = #{ count: ");
    let got = labels(&world, &uri, at(1, 18));
    assert!(got.contains(&"total".to_string()), "a value goes here: {got:?}");
    assert!(!got.contains(&"boolean".to_string()), "not a type position");
}

/// A name is worth more with the type it was given beside it.
#[test]
fn names_are_offered_with_the_types_they_were_written_with() {
    let src = "type Point = #{ x: number }\n\
               fn scale(p: Point, by: number) -> Point { p }\n\
               let origin: Point = #{ x: 1 }\n\
               let count = 2\n";
    let (world, uri) = open(src);
    assert_eq!(detail(&world, &uri, at(4, 0), "origin"), "Point");
    assert_eq!(
        detail(&world, &uri, at(4, 0), "scale"),
        "scale(p: Point, by: number) -> Point",
        "a function is offered as its signature"
    );
    // one with no annotation still appears, just without a type to show
    assert_eq!(detail(&world, &uri, at(4, 0), "count"), "this file");
}

// ---- generics, and what they let the editor say ----------------------------

const HANDLER: &str = "type Body = #{ path: string, bytes: number }\n\
                       type Reply = #{ status: number }\n\
                       type Handler<T, U> = fn(route: string, data: T) -> U\n\
                       fn serve(on: string, handle: Handler<Body, Reply>, retries: number) -> boolean {\n\
                       retries > 0\n\
                       }\n\
                       let listener: Handler<Body, Reply> = |r, d| { #{ status: 200 } }\n";

/// The thing this is all for: typing a call and being told what goes there,
/// at every argument, including part way through writing one.
#[test]
fn every_argument_of_a_call_says_what_it_is_for() {
    let src = format!("{HANDLER}let a = serve(\"x\", listener, ret");
    let (world, uri) = open(&src);
    let line = 7;
    for (col, want, kind) in [
        (14u32, "on", "first"),
        (19, "handle", "second"),
        (29, "retries", "third"),
        (32, "retries", "third"),
    ] {
        let got = labels(&world, &uri, at(line, col));
        assert_eq!(got.first().map(String::as_str), Some(want), "at column {col}");
        let d = detail(&world, &uri, at(line, col), want);
        assert!(d.starts_with(kind), "{d}");
    }
}

/// A generic is followed down to what it stands for, because the name says
/// less than the shape does.
#[test]
fn a_generic_is_filled_in_where_it_is_used() {
    let src = format!("{HANDLER}let a = serve(\"x\", ");
    let (world, uri) = open(&src);
    let d = detail(&world, &uri, at(7, 19), "handle");
    assert!(d.contains("Handler<Body, Reply>"), "{d}");
    assert!(
        d.contains("fn(route: string, data: Body) -> Reply"),
        "the parameters filled in: {d}"
    );
}

/// Hover is the other half of the same answer.
#[test]
fn hover_says_what_a_name_and_a_type_and_a_function_are() {
    let (world, uri) = open(HANDLER);
    // a function: its whole signature, and a line per parameter
    let f = world.hover_at(&uri, at(3, 4)).expect("hover on `serve`");
    assert!(f.contains("fn serve(on: string, handle: Handler<Body, Reply>, retries: number) -> boolean"), "{f}");
    assert!(f.contains("`handle`"), "{f}");
    assert!(f.contains("fn(route: string, data: Body) -> Reply"), "filled in: {f}");

    // a name: the type written beside it, and what that stands for
    let n = world.hover_at(&uri, at(6, 5)).expect("hover on `listener`");
    assert!(n.contains("listener: Handler<Body, Reply>"), "{n}");
    assert!(n.contains("fn(route: string, data: Body) -> Reply"), "{n}");

    // a generic type: its parameters and its body, as written
    let t = world.hover_at(&uri, at(2, 6)).expect("hover on `Handler`");
    assert!(t.contains("type Handler<T, U> = fn(route: string, data: T) -> U"), "{t}");

    // a shape
    let b = world.hover_at(&uri, at(0, 6)).expect("hover on `Body`");
    assert!(b.contains("type Body = #{ path: string, bytes: number }"), "{b}");
}
