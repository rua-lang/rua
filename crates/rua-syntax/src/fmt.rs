//! Lay out rua source.
//!
//! This formatter moves whitespace and nothing else. It walks the tokens and
//! the comments in source order and decides what goes between them; it never
//! reorders, never joins or splits a line the author wrote, and never looks
//! inside a token. A formatter built on the tree would have to put the
//! comments back afterwards, which is where formatters lose them.
//!
//! What follows from that is worth stating: formatting cannot change what a
//! program means, and the test suite holds it to that by lexing the result
//! and comparing the tokens with the ones it started from.

use crate::ast::Span;
use crate::lexer::{Lexer, Tok};
use crate::SyntaxError;

/// One thing to print: a token, or a comment the lexer would have skipped.
struct Piece {
    text: String,
    tok: Option<Tok>,
    /// How many line breaks stood before it in the source.
    breaks: usize,
    /// What a lone `|` is doing here, since rua writes three things with it.
    bar: Bar,
    /// The spaces that stood before it on its own line. A comment aligned in
    /// a column with the ones above it was put there on purpose.
    gap: usize,
    /// If it begins a line, the column it began at.
    col: usize,
}

/// A `|` opens a closure's parameters, closes them, or separates the
/// alternatives of a match pattern. Which one decides where the spaces go,
/// and it cannot be told from the token alone.
#[derive(Clone, Copy, PartialEq)]
enum Bar {
    No,
    Open,
    Close,
    Alt,
}

/// Lay out a chunk of rua.
///
/// A file that does not lex is returned unchanged as an error: there is no
/// safe way to lay out bytes nobody can read.
pub fn format(src: &str) -> Result<String, SyntaxError> {
    let scan = Lexer::scan(src);
    if let Some(e) = scan.errors.first() {
        return Err(e.clone());
    }
    let pieces = interleave(src, &scan);
    // The lexer steps over a `#!` line before it starts, so it is in neither
    // the tokens nor the comments. It is still the first line of the file.
    let shebang = match src.starts_with("#!") {
        true => &src[..src.find('\n').unwrap_or(src.len())],
        false => "",
    };
    let body = render(&pieces);
    if shebang.is_empty() {
        return Ok(body);
    }
    Ok(format!("{}\n{}", shebang.trim_end(), body))
}

/// Tokens and comments in the order they were written.
fn interleave(src: &str, scan: &crate::lexer::Scan) -> Vec<Piece> {
    let mut all: Vec<(Span, Option<Tok>)> = Vec::new();
    for t in &scan.tokens {
        if t.tok == Tok::Eof {
            continue;
        }
        all.push((t.span, Some(t.tok.clone())));
    }
    for c in &scan.comments {
        all.push((*c, None));
    }
    all.sort_by_key(|(s, _)| s.lo);

    let mut out: Vec<Piece> = Vec::with_capacity(all.len());
    let mut prev_end = 0usize;
    for (span, tok) in all {
        let between = &src[prev_end..span.lo as usize];
        out.push(Piece {
            text: src[span.lo as usize..span.hi as usize].to_string(),
            tok,
            breaks: between.bytes().filter(|b| *b == b'\n').count(),
            bar: Bar::No,
            gap: between.len() - between.trim_end_matches(' ').len(),
            col: match between.rfind('\n') {
                Some(nl) => between[nl + 1..].chars().filter(|c| *c == ' ').count(),
                None => 0,
            },
        });
        prev_end = span.hi as usize;
    }
    classify_bars(&mut out);
    out
}

/// Decide what each lone `|` is. `||` is its own token, so the only choices
/// are a closure's two bars and a pattern's alternative — and the opening bar
/// is the one that stands where a value could start.
fn classify_bars(pieces: &mut [Piece]) {
    let mut inside = false;
    for i in 0..pieces.len() {
        if !matches!(pieces[i].tok, Some(Tok::Pipe)) {
            continue;
        }
        pieces[i].bar = if inside {
            inside = false;
            Bar::Close
        } else {
            let before = pieces[..i].iter().rev().find(|p| p.tok.is_some()).and_then(|p| p.tok.as_ref());
            let opens = !matches!(
                before,
                Some(
                    Tok::Name(_)
                        | Tok::Num(_)
                        | Tok::Str(_)
                        | Tok::RParen
                        | Tok::RBracket
                        | Tok::RBrace
                        | Tok::True
                        | Tok::False
                        | Tok::Nil
                )
            );
            if opens {
                inside = true;
                Bar::Open
            } else {
                Bar::Alt
            }
        };
    }
}

const INDENT: &str = "    ";

fn render(pieces: &[Piece]) -> String {
    // Indentation is a property of lines, not of brackets. `push(#{` opens
    // two and still means one step in: what a reader indents for is the line
    // that was left open, however many brackets it took to leave it open.
    let mut out = String::new();
    let mut level: usize = 0;
    // which brackets are open where a line begins: a block indents, an
    // argument list may be lined up instead
    let mut open: Vec<Tok> = Vec::new();
    for line in lines(pieces) {
        let first = &pieces[line.start];
        if line.start > 0 {
            let blanks = (first.breaks - 1).min(1);
            for _ in 0..=blanks {
                out.push('\n');
            }
        }
        // a line that begins by closing what an earlier line opened belongs
        // with the line that opened it
        let closes_first = matches!(
            first.tok,
            Some(Tok::RBrace | Tok::RBracket | Tok::RParen)
        );
        let block = level.saturating_sub(closes_first as usize);
        // Inside a call's arguments or an array, a line further right than
        // the block would put it was lined up with something on purpose —
        // under the bracket it belongs to, usually. A line further left was
        // not, and gets the block indent.
        let aligned = matches!(open.last(), Some(Tok::LParen | Tok::LBracket))
            && !closes_first
            && first.col > block * INDENT.len();
        if aligned {
            for _ in 0..first.col {
                out.push(' ');
            }
        } else {
            for _ in 0..block {
                out.push_str(INDENT);
            }
        }
        for i in line.start..line.end {
            if i > line.start {
                let p = &pieces[i];
                if p.tok.is_none() && p.gap > 1 {
                    // a trailing comment keeps the column it was written in
                    for _ in 0..p.gap {
                        out.push(' ');
                    }
                } else if needs_space(pieces, i) {
                    out.push(' ');
                }
            }
            out.push_str(&pieces[i].text);
        }
        for i in line.start..line.end {
            match pieces[i].tok {
                Some(Tok::LBrace) => open.push(Tok::LBrace),
                Some(Tok::LParen) => open.push(Tok::LParen),
                Some(Tok::LBracket) => open.push(Tok::LBracket),
                Some(Tok::RBrace | Tok::RParen | Tok::RBracket) => {
                    open.pop();
                }
                _ => {}
            }
        }
        match line.net.cmp(&0) {
            std::cmp::Ordering::Greater => level += 1,
            std::cmp::Ordering::Less => level = level.saturating_sub(1),
            std::cmp::Ordering::Equal => {}
        }
    }
    while out.ends_with('\n') || out.ends_with(' ') {
        out.pop();
    }
    out.push('\n');
    out
}

/// One line of output: which pieces are on it, and whether it leaves more
/// brackets open than it closes.
struct Line {
    start: usize,
    end: usize,
    net: i32,
}

fn lines(pieces: &[Piece]) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut start = 0;
    for i in 0..pieces.len() {
        if i > 0 && pieces[i].breaks > 0 {
            out.push(Line { start, end: i, net: net_brackets(&pieces[start..i]) });
            start = i;
        }
    }
    if start < pieces.len() {
        out.push(Line { start, end: pieces.len(), net: net_brackets(&pieces[start..]) });
    }
    out
}

fn net_brackets(line: &[Piece]) -> i32 {
    line.iter()
        .map(|p| match p.tok {
            Some(Tok::LBrace | Tok::LBracket | Tok::LParen) => 1,
            Some(Tok::RBrace | Tok::RBracket | Tok::RParen) => -1,
            _ => 0,
        })
        .sum()
}

/// Does a space belong between these two?
fn needs_space(pieces: &[Piece], i: usize) -> bool {
    let (prev, cur) = (&pieces[i - 1], &pieces[i]);
    // a comment keeps its distance from the code before it, and code keeps
    // its distance from a comment
    let (Some(a), Some(b)) = (prev.tok.as_ref(), cur.tok.as_ref()) else {
        return true;
    };
    // nothing hugs a `.` or a `::`
    if matches!(a, Tok::Dot | Tok::ColonColon) || matches!(b, Tok::Dot | Tok::ColonColon) {
        return false;
    }
    // `f(`, `t[`, `name(` — a call or an index sits against its target
    if matches!(b, Tok::LParen | Tok::LBracket)
        && matches!(a, Tok::Name(_) | Tok::RParen | Tok::RBracket)
    {
        return false;
    }
    // `#{` is one thing written as two tokens
    if matches!(a, Tok::Hash) {
        return false;
    }
    // an opening bracket hugs what follows, a closing one what precedes
    if matches!(a, Tok::LParen | Tok::LBracket) || matches!(b, Tok::RParen | Tok::RBracket) {
        return false;
    }
    // `{}` and `#{}` hold nothing, so there is nothing to put a space around
    if matches!(a, Tok::LBrace) && matches!(b, Tok::RBrace) {
        return false;
    }
    // `,` `;` `:` sit against what they follow
    if matches!(b, Tok::Comma | Tok::Semi | Tok::Colon) {
        return false;
    }
    // a range runs from one end straight to the other
    if matches!(a, Tok::DotDot | Tok::DotDotEq) || matches!(b, Tok::DotDot | Tok::DotDotEq) {
        return false;
    }
    // `|a, b|` — the bars of a closure hug their parameters, and an
    // alternative in a pattern is spaced like the operator it reads as
    if prev.bar == Bar::Open || cur.bar == Bar::Close {
        return false;
    }
    if cur.bar == Bar::Open {
        return !matches!(a, Tok::LParen | Tok::LBracket);
    }
    // a prefix `-` or `!` belongs to what it negates
    if prefix_operator(pieces, i - 1) {
        return false;
    }
    // `{` after `}` on the same line, `else {`, and so on: everything left
    // gets one space
    true
}

/// Is the token at `i` a prefix `-` or `!` rather than a binary one?
fn prefix_operator(pieces: &[Piece], i: usize) -> bool {
    match pieces[i].tok {
        Some(Tok::Bang) => true,
        Some(Tok::Minus) => {
            let before = i.checked_sub(1).and_then(|j| pieces[j].tok.as_ref());
            // a value before it makes it a subtraction
            !matches!(
                before,
                Some(
                    Tok::Name(_)
                        | Tok::Num(_)
                        | Tok::Str(_)
                        | Tok::RParen
                        | Tok::RBracket
                        | Tok::RBrace
                        | Tok::True
                        | Tok::False
                        | Tok::Nil
                )
            )
        }
        _ => false,
    }
}
