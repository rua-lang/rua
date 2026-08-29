//! Hand written lexer. Rust-shaped tokens.

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

#[derive(Debug, Clone)]
pub struct Lexed {
    pub tok: Tok,
    pub line: u32,
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: u32,
}

pub type LexResult<T> = Result<T, String>;

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        let bytes = src.as_bytes();
        // a leading `#!` line belongs to the shell, not to us
        let mut pos = 0;
        if bytes.starts_with(b"#!") {
            while pos < bytes.len() && bytes[pos] != b'\n' {
                pos += 1;
            }
        }
        Lexer { src: bytes, pos, line: 1 }
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

    fn skip_trivia(&mut self) -> LexResult<()> {
        loop {
            match (self.peek(), self.peek2()) {
                (b' ' | b'\t' | b'\r' | b'\n', _) => {
                    self.bump();
                }
                (b'/', b'/') => {
                    while self.peek() != b'\n' && self.peek() != 0 {
                        self.bump();
                    }
                }
                (b'/', b'*') => {
                    let start = self.line;
                    self.bump();
                    self.bump();
                    let mut depth = 1;
                    while depth > 0 {
                        match (self.peek(), self.peek2()) {
                            (0, _) => return Err(format!("line {start}: unterminated /* comment")),
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
                }
                _ => return Ok(()),
            }
        }
    }

    fn next_token(&mut self) -> LexResult<Lexed> {
        self.skip_trivia()?;
        let line = self.line;
        let mk = |tok| Ok(Lexed { tok, line });
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
                    return Err(format!("line {line}: rua has no `&`; did you mean `&&`?"));
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
            other => return Err(format!("line {line}: unexpected character {:?}", other as char)),
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
            let n = u64::from_str_radix(&s, 16).map_err(|e| e.to_string())?;
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
        s.parse::<f64>().map(Tok::Num).map_err(|e| format!("line {}: bad number: {e}", self.line))
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
        let quote = self.bump();
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = self.bump();
            match c {
                0 => return Err(format!("line {}: unterminated string", self.line)),
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
