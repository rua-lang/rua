//! Front end: tokens, AST, parser, and the resolver that turns names into
//! frame slots. Nothing here knows what a value is at runtime.

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod resolve;

pub use ast::{BinOp, Binding, Block, Expr, FuncDef, Stat, UnOp, UpvalSrc};

/// Something the front end could not make sense of, and where.
#[derive(Debug, Clone)]
pub struct SyntaxError {
    pub message: String,
    pub line: u32,
}

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for SyntaxError {}

impl From<String> for SyntaxError {
    /// The parser reports `line N: message`; split that back apart.
    fn from(s: String) -> Self {
        if let Some(rest) = s.strip_prefix("line ") {
            if let Some((num, msg)) = rest.split_once(": ") {
                if let Ok(line) = num.parse() {
                    return SyntaxError { message: msg.to_string(), line };
                }
            }
        }
        SyntaxError { message: s, line: 0 }
    }
}

/// Parse and resolve a chunk, returning it with the frame size it needs.
pub fn compile(src: &str) -> Result<(Block, usize), SyntaxError> {
    let parsed = parser::parse(src).map_err(SyntaxError::from)?;
    Ok(resolve::resolve_chunk(&parsed))
}
