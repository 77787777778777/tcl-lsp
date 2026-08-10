//! Syntax-level services for Tcl: position mapping and structural outlines.
//!
//! All offsets produced here are **byte** offsets. Translation to LSP's
//! (line, character) coordinates happens only via [`LineIndex`], which is the one
//! place that knows about position encodings.

#![forbid(unsafe_code)]
#![deny(clippy::print_stdout, clippy::print_stderr)]

pub mod line_index;
pub mod outline;
pub mod selection;

pub use line_index::{LineIndex, LinePos, PositionEncoding};
pub use outline::{
    outline, qualify, split_args, Call, LinkKind, LinkRef, Outline, Provide, Ref, RefKind, Symbol,
    SymbolKind, SyntaxError, VarDef,
};
pub use selection::{command_at, selection_chain, Enclosing};
pub use tcl_tclsys::{command_complete, ParseError, Script};

/// One open document: its text, its line index, and its outline.
///
/// The text is a plain `String`, deliberately. Tcl's parser needs a *contiguous*
/// byte buffer, so a rope would have to be flattened on every parse, which defeats
/// its purpose. Applying an edit is one `String::replace_range` — a memmove that is
/// far cheaper than the analysis that follows it.
pub struct Document {
    text: String,
    line_index: LineIndex,
    version: i32,
}

impl Document {
    pub fn new(text: String, version: i32) -> Self {
        let line_index = LineIndex::new(&text);
        Document {
            text,
            line_index,
            version,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn line_index(&self) -> &LineIndex {
        &self.line_index
    }

    pub fn set_version(&mut self, version: i32) {
        self.version = version;
    }

    /// Replaces the whole document (a full-sync change).
    pub fn replace(&mut self, text: String) {
        self.text = text;
        self.line_index = LineIndex::new(&self.text);
    }

    /// Applies one incremental change over a byte range.
    pub fn edit(&mut self, range: std::ops::Range<usize>, replacement: &str) {
        let start = range.start.min(self.text.len());
        let end = range.end.clamp(start, self.text.len());
        // Keep edits on char boundaries; a client that sends a position inside a
        // multi-byte character would otherwise panic the server.
        let start = floor_char_boundary(&self.text, start);
        let end = floor_char_boundary(&self.text, end);
        self.text.replace_range(start..end, replacement);
        self.line_index = LineIndex::new(&self.text);
    }

    /// Parses the document and returns its outline.
    pub fn outline(&self) -> Outline {
        outline(&Script::new(&self.text))
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_edit_updates_text_and_index() {
        let mut doc = Document::new("set a 1\nset b 2\n".to_string(), 1);
        // Replace `b` with `beta`.
        doc.edit(12..13, "beta");
        assert_eq!(doc.text(), "set a 1\nset beta 2\n");
        assert_eq!(doc.line_index().line_count(), 3);
    }

    #[test]
    fn edits_near_multibyte_chars_do_not_panic() {
        let mut doc = Document::new("set é 1".to_string(), 1);
        doc.edit(5..6, "x"); // lands inside `é`
        assert!(doc.text().is_char_boundary(0));
    }

    #[test]
    fn outline_reflects_edits() {
        let mut doc = Document::new("proc a {} {}\n".to_string(), 1);
        assert_eq!(doc.outline().symbols.len(), 1);
        doc.edit(doc.text().len()..doc.text().len(), "proc b {} {}\n");
        assert_eq!(doc.outline().symbols.len(), 2);
    }
}
