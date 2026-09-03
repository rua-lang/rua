//! Recursive descent + precedence climbing over the Rust-shaped grammar.
//!
//! The one rule worth stating out loud: a block's last expression, written
//! without a trailing `;`, is the block's value — as in Rust.

use crate::ast::*;
use crate::lexer::{Lexed, Lexer, Tok};
use crate::SyntaxError;
use std::rc::Rc;

pub struct Parser {
    toks: Vec<Lexed>,
    pos: usize,
    anon: usize,
    /// How many loops we are inside, so `break` outside one is a syntax error.
    loops: usize,
    /// Nesting depth, so that pathological input reports an error instead of
    /// running the parser out of stack.
    depth: usize,
    /// Keep going after a statement that would not parse, instead of stopping
    /// at the first one. An editor wants every error at once, and a tree of
    /// what it could read; a command line wants the first error and nothing
    /// else, since the rest are usually consequences of it.
    recover: bool,
    errors: Vec<SyntaxError>,
}

/// Deep enough for any real program, shallow enough to stay on the stack.
const MAX_NESTING: usize = 160;

type PResult<T> = Result<T, SyntaxError>;

/// Parse as much as there is, and report everything that went wrong.
///
/// Always returns a tree: what could not be read is missing from it, and the
/// errors say what and where. This is what an editor asks for.
pub fn parse_recover(src: &str) -> (Block, Vec<SyntaxError>) {
    let (toks, mut errors) = Lexer::tokenize_all(src);
    let mut p = Parser {
        toks,
        pos: 0,
        anon: 0,
        loops: 0,
        depth: 0,
        recover: true,
        errors: Vec::new(),
    };
    let block = p.block_body(Tok::Eof).unwrap_or_default();
    errors.append(&mut p.errors);
    errors.sort_by_key(|e| (e.span.lo, e.line));
    (block, errors)
}

pub fn parse(src: &str) -> PResult<Block> {
    let toks = Lexer::tokenize(src)?;
    let mut p = Parser { toks, pos: 0, anon: 0, loops: 0, depth: 0, recover: false, errors: Vec::new() };
    let b = p.block_body(Tok::Eof)?;
    p.expect(Tok::Eof)?;
    Ok(b)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn line(&self) -> u32 {
        self.toks[self.pos].line
    }
    /// Where the token the parser is looking at was written.
    fn span(&self) -> Span {
        self.toks[self.pos].span
    }
    /// An error about the token in hand, which is the one at fault whenever
    /// the parser is surprised by what it found.
    fn err<T>(&self, message: impl Into<String>) -> PResult<T> {
        Err(SyntaxError::new(message, self.line(), self.span()))
    }
    /// The same, about a token already consumed.
    fn err_at<T>(&self, message: impl Into<String>, line: u32, span: Span) -> PResult<T> {
        Err(SyntaxError::new(message, line, span))
    }
    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }
    fn accept(&mut self, t: Tok) -> bool {
        if *self.peek() == t {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: Tok) -> PResult<()> {
        if *self.peek() == t {
            self.bump();
            Ok(())
        } else {
            self.err(format!("expected {t}, found {}", self.peek()))
        }
    }
    fn name(&mut self) -> PResult<Name> {
        let line = self.line();
        let span = self.span();
        match self.bump() {
            Tok::Name(n) => Ok(Name::new(n, span)),
            other => self.err_at(format!("expected a name, found {other}"), line, span),
        }
    }

    /// A loop body, where `break` and `continue` are allowed.
    fn loop_block(&mut self) -> PResult<Block> {
        self.loops += 1;
        let b = self.block();
        self.loops -= 1;
        b
    }

    /// `{ ... }`
    fn block(&mut self) -> PResult<Block> {
        self.expect(Tok::LBrace)?;
        let b = self.block_body(Tok::RBrace)?;
        self.expect(Tok::RBrace)?;
        Ok(b)
    }

    fn block_body(&mut self, end: Tok) -> PResult<Block> {
        let mut stats = Vec::new();
        let mut lines = Vec::new();
        let mut tail = None;
        let mut tail_line = 0;
        while *self.peek() != end && *self.peek() != Tok::Eof {
            if self.accept(Tok::Semi) {
                continue;
            }
            let line = self.line();
            let before = self.pos;
            let item = match self.statement() {
                Ok(item) => item,
                Err(e) if self.recover => {
                    self.errors.push(e);
                    // the statement may have failed without reading anything,
                    // and a loop that reads nothing does not end
                    if self.pos == before {
                        self.bump();
                    }
                    self.sync(&end);
                    continue;
                }
                Err(e) => return Err(e),
            };
            match item {
                Item::Stat(s) => {
                    stats.push(s);
                    lines.push(line);
                }
                Item::Value(e) => {
                    // an expression with no `;`: the block's value if it is last
                    if *self.peek() == end || *self.peek() == Tok::Eof {
                        tail = Some(Box::new(e));
                        tail_line = line;
                        break;
                    }
                    stats.push(Stat::Expr(e));
                    lines.push(line);
                }
            }
        }
        Ok(Block { stats, lines, tail, tail_line })
    }

    /// Read forward to somewhere a statement could begin, so that one mistake
    /// costs one error rather than every line after it.
    fn sync(&mut self, end: &Tok) {
        loop {
            match self.peek() {
                Tok::Eof => return,
                Tok::RBrace => return,
                t if t == end => return,
                // a `;` ends the wreckage; step over it and read on
                Tok::Semi => {
                    self.bump();
                    return;
                }
                Tok::Let | Tok::Fn | Tok::If | Tok::While | Tok::For | Tok::Loop
                | Tok::Return | Tok::Match | Tok::Break | Tok::Continue => return,
                _ => {
                    let before = self.pos;
                    self.bump();
                    if self.pos == before {
                        return;
                    }
                }
            }
        }
    }

    fn statement(&mut self) -> PResult<Item> {
        let line = self.line();
        match self.peek().clone() {
            Tok::Let => {
                self.bump();
                let names = self.let_pattern()?;
                let exprs = if self.accept(Tok::Assign) {
                    self.exprlist()?
                } else {
                    Vec::new()
                };
                self.accept(Tok::Semi);
                Ok(Item::Stat(Stat::Let(names, exprs)))
            }
            // `impl Vec2 { fn len(self) -> number { .. } }`
            Tok::Impl => {
                self.bump();
                let name = self.name()?;
                // `impl Box<T>` — the names its methods may use, which stand
                // for whatever the shape was given when it was written down
                let mut params = Vec::new();
                if self.accept(Tok::Lt) {
                    while *self.peek() != Tok::Gt && *self.peek() != Tok::Eof {
                        params.push(self.name()?);
                        if !self.accept(Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::Gt)?;
                }
                self.expect(Tok::LBrace)?;
                let mut methods = Vec::new();
                while *self.peek() != Tok::RBrace && *self.peek() != Tok::Eof {
                    let line = self.line();
                    self.expect(Tok::Fn)?;
                    let m = self.name()?;
                    let f = self.funcbody(format!("{name}::{m}"), line)?;
                    methods.push((m, f));
                    self.accept(Tok::Semi);
                }
                self.expect(Tok::RBrace)?;
                Ok(Item::Stat(Stat::Impl(name, params, methods)))
            }
            // `type Point = #{ x: number, y: number }`
            Tok::Type => {
                self.bump();
                let name = self.name()?;
                // `type Handler<T, U> = ..` — the names its body may use
                let mut params = Vec::new();
                if self.accept(Tok::Lt) {
                    while *self.peek() != Tok::Gt && *self.peek() != Tok::Eof {
                        params.push(self.name()?);
                        if !self.accept(Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::Gt)?;
                }
                self.expect(Tok::Assign)?;
                let t = self.ty()?;
                self.accept(Tok::Semi);
                Ok(Item::Stat(Stat::TypeAlias(name, params, t)))
            }
            Tok::Fn => {
                self.bump();
                let name = self.name()?;
                let f = self.funcbody(name.to_string(), line)?;
                // `fn f() {}` binds a local, so recursion and shadowing work
                Ok(Item::Stat(Stat::FnDecl(name, f)))
            }
            Tok::Return => {
                self.bump();
                let exprs = if matches!(self.peek(), Tok::Semi | Tok::RBrace | Tok::Eof) {
                    Vec::new()
                } else {
                    self.exprlist()?
                };
                self.accept(Tok::Semi);
                Ok(Item::Stat(Stat::Return(exprs)))
            }
            Tok::Break | Tok::Continue => {
                let span = self.span();
                let word = if *self.peek() == Tok::Break { "break" } else { "continue" };
                let stat = if word == "break" { Stat::Break } else { Stat::Continue };
                self.bump();
                self.accept(Tok::Semi);
                if self.loops == 0 {
                    return self.err_at(format!("`{word}` outside of a loop"), line, span);
                }
                Ok(Item::Stat(stat))
            }
            Tok::While => {
                self.bump();
                let cond = self.expr_no_struct()?;
                let body = self.loop_block()?;
                Ok(Item::Stat(Stat::While(next_loop_id(), cond, body)))
            }
            Tok::Loop => {
                self.bump();
                let body = self.loop_block()?;
                Ok(Item::Stat(Stat::Loop(next_loop_id(), body)))
            }
            Tok::For => {
                self.bump();
                let vars = self.let_pattern()?;
                self.expect(Tok::In)?;
                let iter = self.expr_no_struct()?;
                let body = self.loop_block()?;
                Ok(Item::Stat(match iter {
                    // `for i in a..b` is a counted loop: the JIT can compile it
                    Expr::Range(start, end, inclusive) if vars.len() == 1 => Stat::ForRange {
                        id: next_loop_id(),
                        var: vars.into_iter().next().unwrap(),
                        binding: None,
                        start: *start,
                        end: *end,
                        inclusive,
                        body,
                    },
                    other => Stat::ForIn {
                        id: next_loop_id(),
                        vars,
                        bindings: Vec::new(),
                        iter: other,
                        body,
                    },
                }))
            }
            // A block-shaped expression in statement position is a statement,
            // as in Rust: `if c { 1 }` followed by `(f)()` is two statements,
            // not a call of the `if`.
            Tok::If | Tok::Match | Tok::LBrace => {
                let e = match self.peek() {
                    Tok::If => self.if_expr()?,
                    Tok::Match => self.match_expr()?,
                    _ => Expr::Do(self.block()?),
                };
                self.accept(Tok::Semi);
                if self.block_ends_here() {
                    return Ok(Item::Value(e));
                }
                Ok(Item::Stat(Stat::Expr(e)))
            }
            _ => {
                let span = self.span();
                let e = self.expr()?;
                // assignment?
                if let Some(op) = compound_op(self.peek()) {
                    self.bump();
                    let v = self.expr()?;
                    self.accept(Tok::Semi);
                    check_target(&e, line, span)?;
                    return Ok(Item::Stat(Stat::OpAssign(e, op, v)));
                }
                if self.accept(Tok::Assign) {
                    let vals = self.exprlist()?;
                    self.accept(Tok::Semi);
                    check_target(&e, line, span)?;
                    return Ok(Item::Stat(Stat::Assign(vec![e], vals)));
                }
                if self.accept(Tok::Semi) {
                    return Ok(Item::Stat(Stat::Expr(e)));
                }
                // a block-shaped expression can stand alone as a statement
                if matches!(e, Expr::If(..) | Expr::Do(_)) && !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    return Ok(Item::Stat(Stat::Expr(e)));
                }
                Ok(Item::Value(e))
            }
        }
    }

    /// `x`, `mut x`, or `(a, b)` — the shapes `let` and `for` accept.
    /// Is the next token the end of the enclosing block?
    fn block_ends_here(&self) -> bool {
        matches!(self.peek(), Tok::RBrace | Tok::Eof)
    }

    /// A type, as written. Everything here is optional in the grammar: a
    /// program with no types in it parses exactly as it did.
    fn ty(&mut self) -> PResult<Type> {
        let start = self.span();
        match self.peek().clone() {
            // `[T]`
            Tok::LBracket => {
                self.bump();
                let inner = self.ty()?;
                let end = self.span();
                self.expect(Tok::RBracket)?;
                Ok(Type::Array(Box::new(inner), start.to(end)))
            }
            // `#{ x: number, y: number }`
            Tok::Hash => {
                self.bump();
                self.expect(Tok::LBrace)?;
                let mut fields = Vec::new();
                while *self.peek() != Tok::RBrace && *self.peek() != Tok::Eof {
                    let name = self.name()?;
                    self.expect(Tok::Colon)?;
                    fields.push((name, self.ty()?));
                    if !self.accept(Tok::Comma) {
                        break;
                    }
                }
                let end = self.span();
                self.expect(Tok::RBrace)?;
                Ok(Type::Record(fields, start.to(end)))
            }
            // `fn(A, B) -> C`
            Tok::Fn => {
                self.bump();
                self.expect(Tok::LParen)?;
                let mut args = Vec::new();
                while *self.peek() != Tok::RParen && *self.peek() != Tok::Eof {
                    // `route: string` names the argument; `string` alone does
                    // not, and both are allowed
                    let named = matches!(self.peek(), Tok::Name(_))
                        && self.toks.get(self.pos + 1).map(|t| &t.tok) == Some(&Tok::Colon);
                    let name = if named {
                        let n = self.name()?;
                        self.expect(Tok::Colon)?;
                        Some(n)
                    } else {
                        None
                    };
                    args.push((name, self.ty()?));
                    if !self.accept(Tok::Comma) {
                        break;
                    }
                }
                let mut end = self.span();
                self.expect(Tok::RParen)?;
                let ret = if self.accept(Tok::Arrow) {
                    let r = self.ty()?;
                    end = r.span();
                    Some(Box::new(r))
                } else {
                    None
                };
                Ok(Type::Fn(args, ret, start.to(end)))
            }
            // `nil` is a type as well as a value
            Tok::Nil => {
                self.bump();
                Ok(Type::Named("nil".into(), Vec::new(), start))
            }
            Tok::Name(n) => {
                self.bump();
                // `Map<K, V>` — nothing generic exists yet, and the shape is
                // here so that adding it is not a change to this
                let mut args = Vec::new();
                let mut end = start;
                if *self.peek() == Tok::Lt {
                    self.bump();
                    while *self.peek() != Tok::Gt && *self.peek() != Tok::Eof {
                        args.push(self.ty()?);
                        if !self.accept(Tok::Comma) {
                            break;
                        }
                    }
                    end = self.span();
                    self.expect(Tok::Gt)?;
                }
                Ok(Type::Named(n.into(), args, start.to(end)))
            }
            other => self.err(format!("expected a type, found {other}")),
        }
    }

    /// `: T` after a name, when one was written.
    fn annotation(&mut self) -> PResult<Option<Type>> {
        if self.accept(Tok::Colon) {
            return Ok(Some(self.ty()?));
        }
        Ok(None)
    }

    fn let_pattern(&mut self) -> PResult<Vec<Name>> {
        self.accept(Tok::Mut);
        if self.accept(Tok::LParen) {
            let mut names = Vec::new();
            loop {
                self.accept(Tok::Mut);
                let mut n = self.name()?;
                n.ty = self.annotation()?;
                names.push(n);
                if !self.accept(Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen)?;
            Ok(names)
        } else {
            let mut n = self.name()?;
            n.ty = self.annotation()?;
            Ok(vec![n])
        }
    }

    fn funcbody(&mut self, name: String, line: u32) -> PResult<Expr> {
        self.expect(Tok::LParen)?;
        let mut params = Vec::new();
        if !self.accept(Tok::RParen) {
            loop {
                self.accept(Tok::Mut);
                let mut p = self.name()?;
                p.ty = self.annotation()?;
                params.push(p);
                if !self.accept(Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen)?;
        }
        let ret = if self.accept(Tok::Arrow) { Some(self.ty()?) } else { None };
        // `break` does not cross a function boundary
        let outer_loops = std::mem::take(&mut self.loops);
        let body = self.block();
        self.loops = outer_loops;
        let body = body?;
        Ok(Expr::Func(Rc::new(FuncDef::typed(name, params, body, ret, line))))
    }

    fn closure(&mut self) -> PResult<Expr> {
        let line = self.line();
        let mut params = Vec::new();
        if self.accept(Tok::OrOr) {
            // `||` — no parameters
        } else {
            self.expect(Tok::Pipe)?;
            if !self.accept(Tok::Pipe) {
                loop {
                    self.accept(Tok::Mut);
                    params.push(self.name()?);
                    if !self.accept(Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::Pipe)?;
            }
        }
        self.anon += 1;
        let name = String::new(); // anonymous: nothing useful to put in an error
        let body = if *self.peek() == Tok::LBrace {
            self.block()?
        } else {
            Block {
                stats: Vec::new(),
                lines: Vec::new(),
                tail: Some(Box::new(self.expr()?)),
                tail_line: line,
            }
        };
        Ok(Expr::Func(Rc::new(FuncDef::new(name, params, body, line))))
    }

    fn exprlist(&mut self) -> PResult<Vec<Expr>> {
        let mut v = vec![self.expr()?];
        while self.accept(Tok::Comma) {
            v.push(self.expr()?);
        }
        Ok(v)
    }

    fn expr(&mut self) -> PResult<Expr> {
        self.binexpr(0, true)
    }

    /// Conditions and `for` iterables: a `{` here opens the body, not a map.
    fn expr_no_struct(&mut self) -> PResult<Expr> {
        self.binexpr(0, false)
    }

    fn binexpr(&mut self, limit: u8, structs: bool) -> PResult<Expr> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            self.depth -= 1;
            return self.err("expression nests too deeply");
        }
        let out = self.binexpr_inner(limit, structs);
        self.depth -= 1;
        out
    }

    fn binexpr_inner(&mut self, limit: u8, structs: bool) -> PResult<Expr> {
        let mut left = match self.peek().clone() {
            Tok::Bang => {
                self.bump();
                Expr::Un(UnOp::Not, Box::new(self.binexpr(UNARY_PRI, structs)?))
            }
            Tok::Minus => {
                self.bump();
                Expr::Un(UnOp::Neg, Box::new(self.binexpr(UNARY_PRI, structs)?))
            }
            _ => self.simple(structs)?,
        };
        loop {
            // ranges sit between comparison and `+`, and never chain
            if matches!(self.peek(), Tok::DotDot | Tok::DotDotEq) && RANGE_PRI > limit {
                let inclusive = *self.peek() == Tok::DotDotEq;
                self.bump();
                let end = self.binexpr(RANGE_PRI, structs)?;
                left = Expr::Range(Box::new(left), Box::new(end), inclusive);
                continue;
            }
            let Some((op, lp, rp)) = binop(self.peek()) else { break };
            if lp <= limit {
                break;
            }
            self.bump();
            let right = self.binexpr(rp, structs)?;
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn simple(&mut self, structs: bool) -> PResult<Expr> {
        match self.peek().clone() {
            Tok::Pipe | Tok::OrOr => self.closure(),
            Tok::Fn => {
                let line = self.line();
                self.bump();
                self.anon += 1;
                self.funcbody(String::new(), line)
            }
            _ => self.suffixed(structs),
        }
    }

    fn array(&mut self) -> PResult<Expr> {
        self.expect(Tok::LBracket)?;
        let mut items = Vec::new();
        while !self.accept(Tok::RBracket) {
            items.push(self.expr()?);
            if !self.accept(Tok::Comma) {
                self.expect(Tok::RBracket)?;
                break;
            }
        }
        Ok(Expr::Array(items))
    }

    /// `#{ name: "x", [k]: v }`
    fn map(&mut self) -> PResult<Expr> {
        self.expect(Tok::Hash)?;
        self.expect(Tok::LBrace)?;
        let mut items = Vec::new();
        while !self.accept(Tok::RBrace) {
            let key = match self.peek().clone() {
                Tok::LBracket => {
                    self.bump();
                    let k = self.expr()?;
                    self.expect(Tok::RBracket)?;
                    k
                }
                Tok::Str(s) => {
                    self.bump();
                    Expr::Str(s.into())
                }
                Tok::Num(n) => {
                    self.bump();
                    Expr::Num(n)
                }
                _ => Expr::Str(self.name()?.text),
            };
            self.expect(Tok::Colon)?;
            items.push((key, self.expr()?));
            if !self.accept(Tok::Comma) {
                self.expect(Tok::RBrace)?;
                break;
            }
        }
        Ok(Expr::Map(items))
    }

    /// `match x { 0 => "zero", n if n > 9 => "big", _ => "other" }`
    fn match_expr(&mut self) -> PResult<Expr> {
        self.expect(Tok::Match)?;
        let subject = self.expr_no_struct()?;
        self.expect(Tok::LBrace)?;
        let mut arms = Vec::new();
        while !self.accept(Tok::RBrace) {
            let mut patterns = vec![self.pattern()?];
            while self.accept(Tok::Pipe) {
                patterns.push(self.pattern()?);
            }
            let guard = if self.accept(Tok::If) { Some(self.expr_no_struct()?) } else { None };
            self.expect(Tok::FatArrow)?;
            let body = if *self.peek() == Tok::LBrace {
                self.block()?
            } else {
                let line = self.line();
                Block {
                    stats: Vec::new(),
                    lines: Vec::new(),
                    tail: Some(Box::new(self.expr()?)),
                    tail_line: line,
                }
            };
            arms.push(Arm { patterns, guard, body });
            if !self.accept(Tok::Comma) {
                self.expect(Tok::RBrace)?;
                break;
            }
        }
        Ok(Expr::Match(Box::new(subject), arms))
    }

    fn pattern(&mut self) -> PResult<Pattern> {
        let span = self.span();
        let line = self.line();
        Ok(match self.peek().clone() {
            Tok::Name(n) if n == "_" => {
                self.bump();
                Pattern::Wild
            }
            Tok::Name(n) => {
                let at = self.span();
                self.bump();
                Pattern::Bind(Name::new(n, at), None)
            }
            Tok::Num(_) | Tok::Str(_) | Tok::True | Tok::False | Tok::Nil => {
                Pattern::Lit(self.primary(false)?)
            }
            Tok::Minus => {
                let span = self.span();
                self.bump();
                match self.bump() {
                    Tok::Num(n) => Pattern::Lit(Expr::Num(-n)),
                    other => {
                        return self.err_at(
                            format!("expected a number, found {other}"),
                            line,
                            span,
                        )
                    }
                }
            }
            other => return self.err_at(format!("{other} is not a pattern"), line, span),
        })
    }

    fn if_expr(&mut self) -> PResult<Expr> {
        self.expect(Tok::If)?;
        let mut arms = Vec::new();
        let cond = self.expr_no_struct()?;
        arms.push((cond, self.block()?));
        let mut els = None;
        while self.accept(Tok::Else) {
            if *self.peek() == Tok::If {
                self.bump();
                let c = self.expr_no_struct()?;
                arms.push((c, self.block()?));
            } else {
                els = Some(self.block()?);
                break;
            }
        }
        Ok(Expr::If(arms, els))
    }

    fn primary(&mut self, structs: bool) -> PResult<Expr> {
        match self.peek().clone() {
            Tok::Num(n) => {
                self.bump();
                Ok(Expr::Num(n))
            }
            Tok::Str(text) => {
                let line = self.line();
                let span = self.span();
                self.bump();
                interpolate(&text, line, span)
            }
            Tok::Nil => {
                self.bump();
                Ok(Expr::Nil)
            }
            Tok::True => {
                self.bump();
                Ok(Expr::Bool(true))
            }
            Tok::False => {
                self.bump();
                Ok(Expr::Bool(false))
            }
            Tok::Name(n) => {
                let at = self.span();
                self.bump();
                Ok(Expr::Var(Name::new(n, at)))
            }
            Tok::LParen => {
                self.bump();
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::LBracket => self.array(),
            Tok::Hash if structs => self.map(),
            Tok::If => self.if_expr(),
            Tok::Match => self.match_expr(),
            Tok::LBrace if structs => Ok(Expr::Do(self.block()?)),
            other => self.err(format!("unexpected {other}")),
        }
    }

    fn suffixed(&mut self, structs: bool) -> PResult<Expr> {
        let mut e = self.primary(structs)?;
        loop {
            match self.peek().clone() {
                // `a.b(x)` is a method call: `a` becomes the first argument
                Tok::Dot => {
                    self.bump();
                    let k = self.name()?;
                    if *self.peek() == Tok::LParen {
                        let args = self.callargs()?;
                        e = Expr::Method(Box::new(e), k.text, args);
                    } else {
                        e = Expr::Index(Box::new(e), Box::new(Expr::Str(k.text)));
                    }
                }
                // `a::b(x)` is a plain path call, no receiver
                Tok::ColonColon => {
                    self.bump();
                    let k = self.name()?;
                    e = Expr::Index(Box::new(e), Box::new(Expr::Str(k.text)));
                }
                Tok::LBracket => {
                    self.bump();
                    let k = self.expr()?;
                    self.expect(Tok::RBracket)?;
                    e = Expr::Index(Box::new(e), Box::new(k));
                }
                Tok::LParen => {
                    let args = self.callargs()?;
                    e = Expr::Call(Box::new(e), args);
                }
                _ => return Ok(e),
            }
        }
    }

    fn callargs(&mut self) -> PResult<Vec<Expr>> {
        self.expect(Tok::LParen)?;
        if self.accept(Tok::RParen) {
            return Ok(Vec::new());
        }
        let args = self.exprlist()?;
        self.expect(Tok::RParen)?;
        Ok(args)
    }
}

/// `"a {x} b"` becomes `"a " + x + " b"`, and `{x:.2}` routes through
/// `format`. `{{` and `}}` are literal braces.
fn interpolate(text: &str, line: u32, span: Span) -> PResult<Expr> {
    // A string with neither brace has nothing to say about either. Skipping
    // only on `{` made `}}` mean two braces here and one in a string that
    // happened to contain a `{` elsewhere, which is the kind of rule you meet
    // while writing JSON out of a script.
    if !text.contains(['{', '}']) {
        return Ok(Expr::Str(text.into()));
    }
    let mut parts: Vec<Expr> = Vec::new();
    let mut literal = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            // `{}` and `{:.2}` are `format` placeholders, not interpolation
            '{' if matches!(chars.peek(), Some('}') | Some(':')) => {
                literal.push('{');
                for c in chars.by_ref() {
                    literal.push(c);
                    if c == '}' {
                        break;
                    }
                }
            }
            '{' => {
                let mut src = String::new();
                let mut depth = 0usize;
                let mut closed = false;
                for c in chars.by_ref() {
                    match c {
                        '}' if depth == 0 => {
                            closed = true;
                            break;
                        }
                        '(' | '[' | '{' => {
                            depth += 1;
                            src.push(c);
                        }
                        ')' | ']' | '}' => {
                            depth = depth.saturating_sub(1);
                            src.push(c);
                        }
                        c => src.push(c),
                    }
                }
                if !closed {
                    // The most likely cause is a `{` that was meant to be a
                    // brace rather than the start of an interpolation, which
                    // is what writing JSON or CSS out of a script is made of.
                    let hint = "write `{{` for a literal brace";
                    return Err(SyntaxError::new(
                        format!("unclosed `{{` in a string ({hint})"),
                        line,
                        span,
                    ));
                }
                if !literal.is_empty() {
                    parts.push(Expr::Str(std::mem::take(&mut literal).into()));
                }
                let (expr_src, spec) = split_spec(&src);
                let inner = parse_fragment(&expr_src, line, span)?;
                parts.push(match spec {
                    // `{x:.2}` is `format("{:.2}", x)`
                    Some(spec) => Expr::Call(
                        // the global, so a local called `format` cannot capture it
                        Box::new(Expr::Global("format".into(), GlobalCache::new())),
                        vec![Expr::Str(format!("{{:{spec}}}").into()), inner],
                    ),
                    None => inner,
                });
            }
            c => literal.push(c),
        }
    }
    if !literal.is_empty() || parts.is_empty() {
        parts.push(Expr::Str(literal.into()));
    }
    // start from a string so that `+` concatenates rather than adds
    let mut out = match parts.first() {
        Some(Expr::Str(_)) => parts.remove(0),
        _ => Expr::Str("".into()),
    };
    for p in parts {
        out = Expr::Bin(BinOp::Add, Box::new(out), Box::new(p));
    }
    Ok(out)
}

/// Split `expr:spec`, ignoring the `::` path operator and anything nested.
fn split_spec(src: &str) -> (String, Option<String>) {
    let bytes: Vec<char> = src.chars().collect();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => {
                if bytes.get(i + 1) == Some(&':') {
                    i += 2;
                    continue;
                }
                let spec: String = bytes[i + 1..].iter().collect();
                let expr: String = bytes[..i].iter().collect();
                return (expr, Some(spec));
            }
            _ => {}
        }
        i += 1;
    }
    (src.to_string(), None)
}

/// Parse one expression out of a fragment of source, for interpolation.
fn parse_fragment(src: &str, line: u32, span: Span) -> PResult<Expr> {
    let toks = Lexer::tokenize(src).map_err(|e| {
        SyntaxError::new(format!("in `{{{src}}}`: {}", e.message), line, span)
    })?;
    let mut p = Parser { toks, pos: 0, anon: 0, loops: 0, depth: 0, recover: false, errors: Vec::new() };
    let e = p
        .expr()
        .map_err(|e| SyntaxError::new(format!("in `{{{src}}}`: {}", e.message), line, span))?;
    if *p.peek() != Tok::Eof {
        return Err(SyntaxError::new(
            format!("`{{{src}}}` is not a single expression"),
            line,
            span,
        ));
    }
    Ok(e)
}

enum Item {
    Stat(Stat),
    /// An expression with no trailing `;`.
    Value(Expr),
}

fn check_target(e: &Expr, line: u32, span: Span) -> PResult<()> {
    match e {
        Expr::Var(_) | Expr::Index(..) => Ok(()),
        _ => Err(SyntaxError::new("cannot assign to this expression", line, span)),
    }
}

fn compound_op(t: &Tok) -> Option<BinOp> {
    Some(match t {
        Tok::PlusEq => BinOp::Add,
        Tok::MinusEq => BinOp::Sub,
        Tok::StarEq => BinOp::Mul,
        Tok::SlashEq => BinOp::Div,
        Tok::PercentEq => BinOp::Rem,
        _ => return None,
    })
}

const UNARY_PRI: u8 = 9;
const RANGE_PRI: u8 = 4;

/// (op, left priority, right priority).
fn binop(t: &Tok) -> Option<(BinOp, u8, u8)> {
    Some(match t {
        Tok::OrOr => (BinOp::Or, 1, 1),
        Tok::AndAnd => (BinOp::And, 2, 2),
        Tok::Lt => (BinOp::Lt, 3, 3),
        Tok::Gt => (BinOp::Gt, 3, 3),
        Tok::Le => (BinOp::Le, 3, 3),
        Tok::Ge => (BinOp::Ge, 3, 3),
        Tok::Ne => (BinOp::Ne, 3, 3),
        Tok::EqEq => (BinOp::Eq, 3, 3),
        Tok::Plus => (BinOp::Add, 6, 6),
        Tok::Minus => (BinOp::Sub, 6, 6),
        Tok::Star => (BinOp::Mul, 7, 7),
        Tok::Slash => (BinOp::Div, 7, 7),
        Tok::Percent => (BinOp::Rem, 7, 7),
        _ => return None,
    })
}
