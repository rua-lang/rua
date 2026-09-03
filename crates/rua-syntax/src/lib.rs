//! Front end: tokens, AST, parser, and the resolver that turns names into
//! frame slots. Nothing here knows what a value is at runtime.

pub mod ast;
pub mod check;
pub mod lower;
pub mod fmt;
pub mod lexer;
pub mod parser;
pub mod resolve;

pub use ast::{BinOp, Binding, Block, Expr, FuncDef, Name, Span, Stat, UnOp, UpvalSrc};

/// Something the front end could not make sense of, and where.
#[derive(Debug, Clone)]
pub struct SyntaxError {
    pub message: String,
    pub line: u32,
    /// The bytes at fault. An editor underlines these; a terminal that has
    /// only the line still has the line.
    pub span: ast::Span,
}

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for SyntaxError {}

impl SyntaxError {
    pub fn new(message: impl Into<String>, line: u32, span: ast::Span) -> SyntaxError {
        SyntaxError { message: message.into(), line, span }
    }
}

/// Parse and resolve a chunk, returning it with the frame size it needs.
pub fn compile(src: &str) -> Result<(Block, usize), SyntaxError> {
    let parsed = parser::parse(src)?;
    // `v.len()` becomes `Vec2::len(v)` where the shape is known, before
    // anything else looks at the tree
    let parsed = lower::lower(&parsed);
    Ok(resolve::resolve_chunk(&parsed))
}
