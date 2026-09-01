//! Byte offsets on one side, line and character on the other.
//!
//! The front end counts bytes. LSP counts lines, and within a line it counts
//! UTF-16 code units, which is neither bytes nor characters. Everything that
//! crosses between the two goes through here.

/// Where every line starts, as a byte offset.
pub struct LineIndex {
    starts: Vec<u32>,
    text: String,
}

impl LineIndex {
    pub fn new(text: &str) -> LineIndex {
        let mut starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i as u32 + 1);
            }
        }
        LineIndex { starts, text: text.to_string() }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The line a byte offset falls on, and how far into it, in UTF-16.
    pub fn position(&self, offset: u32) -> lsp_types::Position {
        let offset = offset.min(self.text.len() as u32);
        let line = match self.starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let start = self.starts[line] as usize;
        let character = self.text[start..offset as usize].chars().map(char::len_utf16).sum::<usize>();
        lsp_types::Position { line: line as u32, character: character as u32 }
    }

    /// The byte offset of a line and character, clamped to the text.
    pub fn offset(&self, at: lsp_types::Position) -> u32 {
        let Some(&start) = self.starts.get(at.line as usize) else {
            return self.text.len() as u32;
        };
        let rest = &self.text[start as usize..];
        let mut utf16 = 0usize;
        for (i, c) in rest.char_indices() {
            if utf16 >= at.character as usize || c == '\n' {
                return start + i as u32;
            }
            utf16 += c.len_utf16();
        }
        self.text.len() as u32
    }

    pub fn range(&self, lo: u32, hi: u32) -> lsp_types::Range {
        lsp_types::Range { start: self.position(lo), end: self.position(hi) }
    }

    /// Is this span written on one line? A semantic token may not straddle
    /// two, and a rua string may.
    pub fn one_line(&self, lo: u32, hi: u32) -> bool {
        self.position(lo).line == self.position(hi).line
    }
}
