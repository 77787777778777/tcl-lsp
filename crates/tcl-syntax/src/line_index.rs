//! Byte offsets ↔ line/character positions, in both position encodings.
//!
//! Everything inside this server works in **byte offsets**, because that is what
//! Tcl's parser reports. LSP instead speaks (line, character), where "character"
//! means UTF-16 code units by default and UTF-8 bytes only if the client agreed to
//! it. Getting that wrong produces edits that land in the wrong place in any
//! document containing non-ASCII text, so all conversion is funnelled through here.

/// Which unit the client counts `character` in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PositionEncoding {
    /// UTF-16 code units — the LSP default, assumed when nothing is negotiated.
    #[default]
    Utf16,
    /// UTF-8 bytes, which needs no conversion at all. Preferred when offered.
    Utf8,
}

/// A (line, character) position, in whichever encoding is in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LinePos {
    pub line: u32,
    pub character: u32,
}

/// Maps between byte offsets and line/character positions for one document version.
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// Byte offset at which each line starts. Always begins with 0.
    line_starts: Vec<u32>,
    /// Lines containing non-ASCII bytes, with their UTF-8 byte length per char.
    /// Lines absent from here are pure ASCII, where all three unit counts coincide
    /// and conversion is a plain subtraction.
    wide_lines: Vec<(u32, Vec<WideChar>)>,
    len: u32,
}

#[derive(Debug, Clone, Copy)]
struct WideChar {
    /// Byte offset of the character, relative to the start of its line.
    start: u32,
    /// Its length in UTF-8 bytes.
    len_utf8: u32,
    /// Its length in UTF-16 code units (1, or 2 for anything above the BMP).
    len_utf16: u32,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts = vec![0u32];
        let mut wide_lines: Vec<(u32, Vec<WideChar>)> = Vec::new();
        let mut cur_line = 0u32;
        let mut line_start = 0usize;
        let mut cur_wide: Vec<WideChar> = Vec::new();

        for (i, ch) in text.char_indices() {
            if ch == '\n' {
                if !cur_wide.is_empty() {
                    wide_lines.push((cur_line, std::mem::take(&mut cur_wide)));
                }
                line_starts.push(i as u32 + 1);
                cur_line += 1;
                line_start = i + 1;
                continue;
            }
            if !ch.is_ascii() {
                cur_wide.push(WideChar {
                    start: (i - line_start) as u32,
                    len_utf8: ch.len_utf8() as u32,
                    len_utf16: ch.len_utf16() as u32,
                });
            }
        }
        if !cur_wide.is_empty() {
            wide_lines.push((cur_line, cur_wide));
        }

        LineIndex {
            line_starts,
            wide_lines,
            len: text.len() as u32,
        }
    }

    pub fn line_count(&self) -> u32 {
        self.line_starts.len() as u32
    }

    fn wide_for(&self, line: u32) -> Option<&[WideChar]> {
        self.wide_lines
            .binary_search_by_key(&line, |(l, _)| *l)
            .ok()
            .map(|i| self.wide_lines[i].1.as_slice())
    }

    /// Byte offset → position.
    pub fn position(&self, offset: usize, enc: PositionEncoding) -> LinePos {
        let offset = (offset as u32).min(self.len);
        // The line whose start is the greatest value <= offset.
        let line = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        } as u32;
        let line_start = self.line_starts[line as usize];
        let col_bytes = offset - line_start;

        let character = match (enc, self.wide_for(line)) {
            // ASCII line, or UTF-8 encoding: bytes are the unit.
            (PositionEncoding::Utf8, _) | (_, None) => col_bytes,
            (PositionEncoding::Utf16, Some(wides)) => {
                let mut units = col_bytes;
                for w in wides {
                    if w.start >= col_bytes {
                        break;
                    }
                    units = units - w.len_utf8 + w.len_utf16;
                }
                units
            }
        };
        LinePos { line, character }
    }

    /// Position → byte offset. Out-of-range input is clamped rather than rejected,
    /// because clients legitimately send end-of-line positions past the last column.
    pub fn offset(&self, pos: LinePos, enc: PositionEncoding) -> usize {
        let line = pos.line.min(self.line_count().saturating_sub(1));
        let line_start = self.line_starts[line as usize];
        let line_end = self
            .line_starts
            .get(line as usize + 1)
            .copied()
            .unwrap_or(self.len);
        let max_bytes = line_end.saturating_sub(line_start);

        let col_bytes = match (enc, self.wide_for(line)) {
            (PositionEncoding::Utf8, _) | (_, None) => pos.character,
            (PositionEncoding::Utf16, Some(wides)) => {
                let mut remaining = pos.character;
                let mut bytes = 0u32;
                for w in wides {
                    // Advance through the ASCII run before this wide char.
                    let ascii = w.start - bytes;
                    if remaining <= ascii {
                        break;
                    }
                    remaining -= ascii;
                    bytes = w.start;
                    if remaining < w.len_utf16 {
                        break; // inside a surrogate pair; clamp to its start
                    }
                    remaining -= w.len_utf16;
                    bytes += w.len_utf8;
                }
                bytes + remaining
            }
        };
        (line_start + col_bytes.min(max_bytes)) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PositionEncoding::{Utf16, Utf8};

    fn pos(line: u32, character: u32) -> LinePos {
        LinePos { line, character }
    }

    #[test]
    fn ascii_roundtrip() {
        let text = "set a 1\nset b 2\n";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position(8, Utf16), pos(1, 0));
        assert_eq!(idx.position(12, Utf16), pos(1, 4));
        assert_eq!(idx.offset(pos(1, 4), Utf16), 12);
        assert_eq!(idx.offset(pos(0, 0), Utf8), 0);
    }

    /// `é` is 2 UTF-8 bytes but 1 UTF-16 unit, so the two encodings disagree and
    /// a client that negotiated UTF-16 must not be handed byte columns.
    #[test]
    fn bmp_char_differs_between_encodings() {
        let text = "set é 1";
        let idx = LineIndex::new(text);
        let after = text.find('1').unwrap(); // byte 7
        assert_eq!(idx.position(after, Utf8).character, 7);
        assert_eq!(idx.position(after, Utf16).character, 6);
        assert_eq!(idx.offset(pos(0, 6), Utf16), after);
        assert_eq!(idx.offset(pos(0, 7), Utf8), after);
    }

    /// An emoji is 4 UTF-8 bytes and 2 UTF-16 units (a surrogate pair).
    #[test]
    fn astral_char_is_two_utf16_units() {
        let text = "puts 😀x";
        let idx = LineIndex::new(text);
        let x = text.find('x').unwrap(); // byte 9
        assert_eq!(idx.position(x, Utf8).character, 9);
        assert_eq!(idx.position(x, Utf16).character, 7);
        assert_eq!(idx.offset(pos(0, 7), Utf16), x);
    }

    #[test]
    fn multiple_wide_chars_accumulate() {
        let text = "ééé|";
        let idx = LineIndex::new(text);
        let bar = text.find('|').unwrap(); // byte 6
        assert_eq!(idx.position(bar, Utf16).character, 3);
        assert_eq!(idx.offset(pos(0, 3), Utf16), bar);
    }

    #[test]
    fn wide_chars_on_later_lines() {
        let text = "ascii\nsét x\nmore";
        let idx = LineIndex::new(text);
        let x = text.find('x').unwrap();
        let p = idx.position(x, Utf16);
        assert_eq!(p.line, 1);
        assert_eq!(idx.offset(p, Utf16), x);
    }

    #[test]
    fn positions_past_end_of_line_are_clamped() {
        let idx = LineIndex::new("ab\ncd\n");
        assert_eq!(idx.offset(pos(0, 999), Utf16), 3);
        assert_eq!(idx.offset(pos(999, 0), Utf16), 6);
    }

    #[test]
    fn every_char_boundary_roundtrips() {
        let text = "a é b 😀 c\nsecond é line\n";
        let idx = LineIndex::new(text);
        for (off, _) in text.char_indices() {
            for enc in [Utf8, Utf16] {
                assert_eq!(
                    idx.offset(idx.position(off, enc), enc),
                    off,
                    "at {off} in {enc:?}"
                );
            }
        }
    }
}
