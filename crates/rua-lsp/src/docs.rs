//! The documents the editor has open, as it last told us they were.

use crate::index::LineIndex;
use lsp_types::Url;
use std::collections::HashMap;

#[derive(Default)]
pub struct Docs {
    open: HashMap<Url, LineIndex>,
}

impl Docs {
    pub fn set(&mut self, uri: Url, text: &str) {
        self.open.insert(uri, LineIndex::new(text));
    }

    pub fn remove(&mut self, uri: &Url) {
        self.open.remove(uri);
    }

    pub fn get(&self, uri: &Url) -> Option<&LineIndex> {
        self.open.get(uri)
    }
}
