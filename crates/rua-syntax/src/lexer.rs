//! Hand written lexer. Rust-shaped tokens.

use crate::ast::Span;
use crate::SyntaxError;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // literals
    Num(f64),
    Str(String),
    Name(String),
    // keywords
    Break, Continue, Else, False, Fn, For, If, In, Let, Loop, Match, Mut, Nil, Return, True, While,
    // operators
    Plus, Minus, Star, Slash, Percent,
    Bang, AndAnd, OrOr,
    EqEq, Ne, Le, Ge, Lt, Gt, Assign,
    // punctuation
    LParen, RParen, LBrace, RBrace, LBracket, RBracket,
    Semi, Comma, Colon, ColonColon, Dot, DotDot, DotDotEq, Pipe, Hash, Arrow, FatArrow,
    PlusEq, MinusEq, StarEq, SlashEq, PercentEq,
    Eof,
}

impl std::fmt::Display for Tok {
    /// How the token is written, so that an error names what the reader typed
    /// rather than what the enum happens to call it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Tok::Num(n) => return write!(f, "the number {n}"),
            Tok::Str(_) => return write!(f, "a string"),
            Tok::Name(n) => return write!(f, "`{n}`"),
            Tok::Eof => return write!(f, "end of file"),
            Tok::Break => "break",
            Tok::Continue => "continue",
            Tok::Else => "else",
            Tok::False => "false",
            Tok::Fn => "fn",
            Tok::For => "for",
            Tok::If => "if",
            Tok::In => "in",
            Tok::Let => "let",
            Tok::Loop => "loop",
            Tok::Match => "match",
            Tok::Mut => "mut",
            Tok::Nil => "nil",
            Tok::Return => "return",
            Tok::True => "true",
            Tok::While => "while",
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::Bang => "!",
            Tok::AndAnd => "&&",
            Tok::OrOr => "||",
            Tok::EqEq => "==",
            Tok::Ne => "!=",
            Tok::Le => "<=",
            Tok::Ge => ">=",
            Tok::Lt => "<",
            Tok::Gt => ">",
            Tok::Assign => "=",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::Semi => ";",
            Tok::Comma => ",",
            Tok::Colon => ":",
            Tok::ColonColon => "::",
            Tok::Dot => ".",
            Tok::DotDot => "..",
            Tok::DotDotEq => "..=",
            Tok::Pipe => "|",
            Tok::Hash => "#",
            Tok::Arrow => "->",
            Tok::FatArrow => "=>",
            Tok::PlusEq => "+=",
            Tok::MinusEq => "-=",
            Tok::StarEq => "*=",
            Tok::SlashEq => "/=",
            Tok::PercentEq => "%=",
        };
        write!(f, "`{s}`")
    }
}

#[derive(Debug, Clone)]
pub struct Lexed {
    pub tok: Tok,
    pub line: u32,
    /// The bytes this token was written with, trivia excluded.
    pub span: Span,
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: u32,
    /// Record where the comments were, for a reader that cares.
    keep_comments: bool,
    comments: Vec<Span>,
    /// The `#!` line, which is stepped over before lexing begins and is a
    /// comment to everyone who reads the file afterwards.
    shebang: Option<Span>,
}

/// A pass over the source, for somebody who wants all of it.
pub struct Scan {
    pub tokens: Vec<Lexed>,
    pub comments: Vec<Span>,
    pub errors: Vec<SyntaxError>,
}

pub type LexResult<T> = Result<T, SyntaxError>;

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        let bytes = src.as_bytes();
        // a leading `#!` line belongs to the shell, not to us
        let mut pos = 0;
        let mut shebang = None;
        if bytes.starts_with(b"#!") {
            while pos < bytes.len() && bytes[pos] != b'\n' {
                pos += 1;
            }
            shebang = Some(Span::new(0, pos as u32));
        }
        Lexer { src: bytes, pos, line: 1, keep_comments: false, comments: Vec::new(), shebang }
    }

    /// Everything a reader of the source might want: the tokens, where the
    /// comments were, and what could not be read.
    ///
    /// Comments are trivia to a compiler and not to an editor, which has to
    /// know that the cursor is inside one before it offers to complete a
    /// keyword there. The lexer already finds them; this writes them down
    /// rather than making somebody find them again.
    pub fn scan(src: &'a str) -> Scan {
        let mut lx = Lexer::new(src);
        lx.keep_comments = true;
        // the `#!` line was stepped over in `new`, before anything could be
        // recorded; it is the file's first comment
        let shebang = lx.shebang;
        let (tokens, errors) = lx.run();
        let mut comments = lx.comments;
        if let Some(s) = shebang {
            comments.insert(0, s);
        }
        Scan { tokens, comments, errors }
    }

    /// Every token, and everything that went wrong getting them.
    ///
    /// A file being typed into is not a finished one: it has a half-written
    /// string in it, or a character that means nothing yet. Giving up on the
    /// first of those leaves an editor with no tokens at all, which is the
    /// moment it most needs them. This lexes past what it cannot read.
    pub fn tokenize_all(src: &'a str) -> (Vec<Lexed>, Vec<SyntaxError>) {
        Lexer::new(src).run()
    }

    fn run(&mut self) -> (Vec<Lexed>, Vec<SyntaxError>) {
        let lx = self;
        let mut out = Vec::new();
        let mut errors = Vec::new();
        loop {
            let before = lx.pos;
            match lx.next_token() {
                Ok(t) => {
                    let eof = t.tok == Tok::Eof;
                    out.push(t);
                    if eof {
                        return (out, errors);
                    }
                }
                Err(e) => {
                    errors.push(e);
                    // An unterminated string or comment runs to the end, and
                    // there is nothing after it to read. Anything else leaves
                    // the offending byte behind it, and reading on is what
                    // finds the rest of the file's tokens.
                    if lx.pos >= lx.src.len() {
                        let at = lx.src.len() as u32;
                        out.push(Lexed {
                            tok: Tok::Eof,
                            line: lx.line,
                            span: Span::new(at, at),
                        });
                        return (out, errors);
                    }
                    if lx.pos == before {
                        lx.bump();
                    }
                }
            }
        }
    }

    pub fn tokenize(src: &'a str) -> LexResult<Vec<Lexed>> {
        let mut lx = Lexer::new(src);
        let mut out = Vec::new();
        loop {
            let t = lx.next_token()?;
            let eof = t.tok == Tok::Eof;
            out.push(t);
            if eof {
                return Ok(out);
            }
        }
    }

    fn peek(&self) -> u8 {
        *self.src.get(self.pos).unwrap_or(&0)
    }
    fn peek2(&self) -> u8 {
        *self.src.get(self.pos + 1).unwrap_or(&0)
    }
    fn bump(&mut self) -> u8 {
        let c = self.peek();
        self.pos += 1;
        if c == b'\n' {
            self.line += 1;
        }
        c
    }

    /// Remember a comment that ended where the cursor now is.
    fn note_comment(&mut self, from: usize) {
        if self.keep_comments {
            self.comments.push(Span::new(from as u32, self.pos as u32));
        }
    }

    fn skip_trivia(&mut self) -> LexResult<()> {
        loop {
            match (self.peek(), self.peek2()) {
                (b' ' | b'\t' | b'\r' | b'\n', _) => {
                    self.bump();
                }
                (b'/', b'/') => {
                    let at = self.pos;
                    while self.peek() != b'\n' && self.peek() != 0 {
                        self.bump();
                    }
                    self.note_comment(at);
                }
                (b'/', b'*') => {
                    let start = self.line;
                    let at = self.pos;
                    let opened = at;
                    self.bump();
                    self.bump();
                    let mut depth = 1;
                    while depth > 0 {
                        match (self.peek(), self.peek2()) {
                            (0, _) => {
                                return Err(SyntaxError::new(
                                    "unterminated /* comment",
                                    start,
                                    Span::new(at as u32, self.pos as u32),
                                ))
                            }
                            (b'/', b'*') => {
                                self.bump();
                                self.bump();
                                depth += 1;
                            }
                            (b'*', b'/') => {
                                self.bump();
                                self.bump();
                                depth -= 1;
                            }
                            _ => {
                                self.bump();
                            }
                        }
                    }
                    self.note_comment(opened);
                }
                _ => return Ok(()),
            }
        }
    }

    fn next_token(&mut self) -> LexResult<Lexed> {
        self.skip_trivia()?;
        let line = self.line;
        let lo = self.pos as u32;
        let tok = self.next_tok()?;
        Ok(Lexed { tok, line, span: Span::new(lo, self.pos as u32) })
    }

    /// The token itself. `next_token` is what knows where it started.
    fn next_tok(&mut self) -> LexResult<Tok> {
        let line = self.line;
        let mk = Ok;
        let c = self.peek();
        if c == 0 {
            return mk(Tok::Eof);
        }
        if c.is_ascii_digit() {
            return mk(self.number()?);
        }
        if c == b'_' || c.is_ascii_alphabetic() {
            return mk(self.name());
        }
        if c == b'"' {
            return mk(self.string()?);
        }
        self.bump();
        let tok = match c {
            b'+' => {
                if self.peek() == b'=' {
                    self.bump();
                    Tok::PlusEq
                } else {
                    Tok::Plus
                }
            }
            b'-' => match self.peek() {
                b'>' => {
                    self.bump();
                    Tok::Arrow
                }
                b'=' => {
                    self.bump();
                    Tok::MinusEq
                }
                _ => Tok::Minus,
            },
            b'*' => {
                if self.peek() == b'=' {
                    self.bump();
                    Tok::StarEq
                } else {
                    Tok::Star
                }
            }
            b'/' => {
                if self.peek() == b'=' {
                    self.bump();
                    Tok::SlashEq
                } else {
                    Tok::Slash
                }
            }
            b'%' => {
                if self.peek() == b'=' {
                    self.bump();
                    Tok::PercentEq
                } else {
                    Tok::Percent
                }
            }
            b'(' => Tok::LParen,
            b')' => Tok::RParen,
            b'{' => Tok::LBrace,
            b'}' => Tok::RBrace,
            b'[' => Tok::LBracket,
            b']' => Tok::RBracket,
            b';' => Tok::Semi,
            b',' => Tok::Comma,
            b'#' => Tok::Hash,
            b':' => {
                if self.peek() == b':' {
                    self.bump();
                    Tok::ColonColon
                } else {
                    Tok::Colon
                }
            }
            b'.' => {
                if self.peek() == b'.' {
                    self.bump();
                    if self.peek() == b'=' {
                        self.bump();
                        Tok::DotDotEq
                    } else {
                        Tok::DotDot
                    }
                } else {
                    Tok::Dot
                }
            }
            b'=' => match self.peek() {
                b'=' => {
                    self.bump();
                    Tok::EqEq
                }
                b'>' => {
                    self.bump();
                    Tok::FatArrow
                }
                _ => Tok::Assign,
            },
            b'!' => {
                if self.peek() == b'=' {
                    self.bump();
                    Tok::Ne
                } else {
                    Tok::Bang
                }
            }
            b'<' => {
                if self.peek() == b'=' {
                    self.bump();
                    Tok::Le
                } else {
                    Tok::Lt
                }
            }
            b'>' => {
                if self.peek() == b'=' {
                    self.bump();
                    Tok::Ge
                } else {
                    Tok::Gt
                }
            }
            b'&' => {
                if self.peek() == b'&' {
                    self.bump();
                    Tok::AndAnd
                } else {
                    return Err(SyntaxError::new(
                        "rua has no `&`; did you mean `&&`?",
                        line,
                        Span::new(self.pos as u32 - 1, self.pos as u32),
                    ));
                }
            }
            b'|' => {
                if self.peek() == b'|' {
                    self.bump();
                    Tok::OrOr
                } else {
                    Tok::Pipe
                }
            }
            other => {
                return Err(SyntaxError::new(
                    format!("unexpected character {:?}", other as char),
                    line,
                    Span::new(self.pos as u32 - 1, self.pos as u32),
                ))
            }
        };
        mk(tok)
    }

    fn number(&mut self) -> LexResult<Tok> {
        let start = self.pos;
        if self.peek() == b'0' && (self.peek2() | 32) == b'x' {
            self.bump();
            self.bump();
            while self.peek().is_ascii_hexdigit() || self.peek() == b'_' {
                self.bump();
            }
            let s: String = std::str::from_utf8(&self.src[start + 2..self.pos])
                .unwrap()
                .replace('_', "");
            let n = u64::from_str_radix(&s, 16).map_err(|e| {
                SyntaxError::new(
                    format!("bad hexadecimal number: {e}"),
                    self.line,
                    Span::new(start as u32, self.pos as u32),
                )
            })?;
            return Ok(Tok::Num(n as f64));
        }
        while self.peek().is_ascii_digit() || self.peek() == b'_' {
            self.bump();
        }
        // a `.` only continues the number when a digit follows: `0..10` is a range
        if self.peek() == b'.' && self.peek2().is_ascii_digit() {
            self.bump();
            while self.peek().is_ascii_digit() || self.peek() == b'_' {
                self.bump();
            }
        }
        if (self.peek() | 32) == b'e' && (self.peek2().is_ascii_digit() || self.peek2() == b'-' || self.peek2() == b'+') {
            self.bump();
            if self.peek() == b'+' || self.peek() == b'-' {
                self.bump();
            }
            while self.peek().is_ascii_digit() {
                self.bump();
            }
        }
        let s: String = std::str::from_utf8(&self.src[start..self.pos]).unwrap().replace('_', "");
        s.parse::<f64>().map(Tok::Num).map_err(|e| {
            SyntaxError::new(
                format!("bad number: {e}"),
                self.line,
                Span::new(start as u32, self.pos as u32),
            )
        })
    }

    fn name(&mut self) -> Tok {
        let start = self.pos;
        while self.peek() == b'_' || self.peek().is_ascii_alphanumeric() {
            self.bump();
        }
        let s = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        match s {
            "break" => Tok::Break,
            "continue" => Tok::Continue,
            "else" => Tok::Else,
            "false" => Tok::False,
            "fn" => Tok::Fn,
            "for" => Tok::For,
            "if" => Tok::If,
            "in" => Tok::In,
            "let" => Tok::Let,
            "loop" => Tok::Loop,
            "match" => Tok::Match,
            "mut" => Tok::Mut,
            "nil" => Tok::Nil,
            "return" => Tok::Return,
            "true" => Tok::True,
            "while" => Tok::While,
            _ => Tok::Name(s.to_string()),
        }
    }

    fn string(&mut self) -> LexResult<Tok> {
        let start = self.pos;
        let quote = self.bump();
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = self.bump();
            match c {
                0 => {
                    return Err(SyntaxError::new(
                        "unterminated string",
                        self.line,
                        Span::new(start as u32, self.pos as u32),
                    ))
                }
                b'\\' => {
                    let e = self.bump();
                    out.push(match e {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        b'0' => 0,
                        other => other,
                    });
                }
                c if c == quote => {
                    return Ok(Tok::Str(String::from_utf8_lossy(&out).into_owned()))
                }
                c => out.push(c),
            }
        }
    }
}
