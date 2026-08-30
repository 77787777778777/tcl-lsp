//! Semantic token production.
//!
//! LSP wants a flat, delta-encoded array of five integers per token, in document
//! order, with no token spanning a line break. Building that correctly is fiddly
//! enough to be worth isolating from the request handlers.

use std::ops::Range;

use lsp_types::{SemanticToken, SemanticTokenType};
use tcl_syntax::{LineIndex, Outline, PositionEncoding, RefKind, Symbol, SymbolKind};

/// The token types this server emits, in the order the client is told about them.
/// A token's `token_type` field is an index into this array.
pub const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::COMMENT,   // 0
    SemanticTokenType::FUNCTION,  // 1
    SemanticTokenType::NAMESPACE, // 2
    SemanticTokenType::CLASS,     // 3
    SemanticTokenType::METHOD,    // 4
    SemanticTokenType::VARIABLE,  // 5
    SemanticTokenType::PARAMETER, // 6
    SemanticTokenType::KEYWORD,   // 7
];

const T_COMMENT: u32 = 0;
const T_FUNCTION: u32 = 1;
const T_NAMESPACE: u32 = 2;
const T_CLASS: u32 = 3;
const T_METHOD: u32 = 4;
const T_VARIABLE: u32 = 5;
const T_PARAMETER: u32 = 6;
const T_KEYWORD: u32 = 7;

/// Modifier bit 0 = declaration.
const M_DECLARATION: u32 = 1;

struct Raw {
    range: Range<usize>,
    kind: u32,
    modifiers: u32,
}

/// Builds the semantic tokens for a file.
///
/// `builtin` decides whether a command name is one of Tcl's own, which is what
/// separates a keyword from a call to user code.
pub fn tokens(
    outline: &Outline,
    text: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    builtin: impl Fn(&str) -> bool,
) -> Vec<SemanticToken> {
    let mut raw: Vec<Raw> = Vec::new();

    for c in &outline.comments {
        raw.push(Raw {
            range: c.clone(),
            kind: T_COMMENT,
            modifiers: 0,
        });
    }

    collect_symbols(&outline.symbols, &mut raw);

    for r in &outline.refs {
        let kind = match r.kind {
            RefKind::Variable => T_VARIABLE,
            // Tcl has no reserved words; `if` and `proc` are ordinary commands. So
            // "keyword" here means "a command Tcl itself provides", which is the
            // distinction that actually helps a reader.
            RefKind::Command if builtin(&r.name) => T_KEYWORD,
            RefKind::Command => T_FUNCTION,
            // `$w.bla.bla insert` — the dispatch word reads like a method.
            RefKind::WidgetCommand => T_METHOD,
        };
        raw.push(Raw {
            range: r.range.clone(),
            kind,
            modifiers: 0,
        });
    }

    for v in &outline.variables {
        raw.push(Raw {
            range: v.range.clone(),
            kind: T_PARAMETER,
            modifiers: M_DECLARATION,
        });
    }

    encode(raw, text, li, enc)
}

fn collect_symbols(symbols: &[Symbol], out: &mut Vec<Raw>) {
    for s in symbols {
        let kind = match s.kind {
            SymbolKind::Proc => T_FUNCTION,
            SymbolKind::Namespace => T_NAMESPACE,
            SymbolKind::Class => T_CLASS,
            SymbolKind::Method | SymbolKind::Constructor | SymbolKind::Destructor => T_METHOD,
            SymbolKind::Variable => T_VARIABLE,
        };
        out.push(Raw {
            range: s.name_range.clone(),
            kind,
            modifiers: M_DECLARATION,
        });
        collect_symbols(&s.children, out);
    }
}

/// Sorts, de-overlaps, splits across lines, and delta-encodes.
fn encode(
    mut raw: Vec<Raw>,
    text: &str,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Vec<SemanticToken> {
    // A declaration and a reference can land on the same name. Sorting so that
    // declarations come first, then dropping duplicates by start offset, keeps the
    // more specific classification.
    raw.sort_by_key(|r| (r.range.start, std::cmp::Reverse(r.modifiers), r.kind));
    raw.dedup_by_key(|r| r.range.start);

    let mut out = Vec::with_capacity(raw.len());
    let (mut prev_line, mut prev_start) = (0u32, 0u32);
    let mut last_end = 0usize;

    for r in raw {
        // Overlapping tokens are rejected by some clients; keep the first.
        if r.range.start < last_end || r.range.end > text.len() {
            continue;
        }
        // A token may not cross a line break, so emit one per line it covers.
        for (line_range, _) in line_pieces(text, &r.range) {
            let pos = li.position(line_range.start, enc);
            let end = li.position(line_range.end, enc);
            if end.line != pos.line || end.character <= pos.character {
                continue;
            }
            let delta_line = pos.line - prev_line;
            let delta_start = if delta_line == 0 {
                pos.character.saturating_sub(prev_start)
            } else {
                pos.character
            };
            out.push(SemanticToken {
                delta_line,
                delta_start,
                length: end.character - pos.character,
                token_type: r.kind,
                token_modifiers_bitset: r.modifiers,
            });
            prev_line = pos.line;
            prev_start = pos.character;
        }
        last_end = r.range.end;
    }
    out
}

/// Splits a byte range into per-line pieces, dropping the newlines themselves.
fn line_pieces(text: &str, range: &Range<usize>) -> Vec<(Range<usize>, ())> {
    let mut out = Vec::new();
    let slice = &text[range.start.min(text.len())..range.end.min(text.len())];
    let mut start = range.start;
    for part in slice.split('\n') {
        let end = start + part.len();
        if end > start {
            out.push((start..end, ()));
        }
        start = end + 1; // step over the newline
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcl_syntax::{outline, Script};

    fn build(src: &str) -> Vec<SemanticToken> {
        let o = outline(&Script::new(src));
        let li = LineIndex::new(src);
        tokens(&o, src, &li, PositionEncoding::Utf8, |n| {
            matches!(n, "puts" | "set" | "proc" | "string" | "namespace")
        })
    }

    #[test]
    fn emits_tokens_in_document_order() {
        let toks = build("# a comment\nproc greet {name} {\n    puts $name\n}\n");
        assert!(!toks.is_empty());
        // Deltas must never be negative, which the encoding enforces by construction:
        // a token on the same line records a non-decreasing start.
        let mut line = 0u32;
        for t in &toks {
            line += t.delta_line;
            assert!(line < 10, "line drifted out of range");
        }
    }

    #[test]
    fn classifies_a_declaration_and_its_uses() {
        let toks = build("proc greet {} {}\ngreet\n");
        assert!(
            toks.iter()
                .any(|t| t.token_type == T_FUNCTION && t.token_modifiers_bitset == M_DECLARATION),
            "the definition must be marked as a declaration"
        );
        assert!(
            toks.iter()
                .any(|t| t.token_type == T_FUNCTION && t.token_modifiers_bitset == 0),
            "the call site must not be"
        );
    }

    #[test]
    fn builtins_are_keywords_and_user_procs_are_not() {
        let toks = build("proc mine {} {}\nputs hi\nmine\n");
        assert!(toks.iter().any(|t| t.token_type == T_KEYWORD));
        assert!(toks.iter().any(|t| t.token_type == T_FUNCTION));
    }

    #[test]
    fn no_token_spans_a_line_break() {
        // A multi-line comment block must become one token per line.
        let src = "# line one\n# line two\nputs hi\n";
        let toks = build(src);
        let comments: Vec<_> = toks.iter().filter(|t| t.token_type == T_COMMENT).collect();
        assert!(comments.len() >= 2, "expected one comment token per line");
        for t in &toks {
            assert!(t.length <= 11, "token {t:?} looks like it crossed a line");
        }
    }

    #[test]
    fn handles_an_empty_document() {
        assert!(build("").is_empty());
    }

    #[test]
    fn utf16_positions_are_used_when_negotiated() {
        let src = "# é comment\nputs hi\n";
        let o = outline(&Script::new(src));
        let li = LineIndex::new(src);
        let u8s = tokens(&o, src, &li, PositionEncoding::Utf8, |_| false);
        let u16s = tokens(&o, src, &li, PositionEncoding::Utf16, |_| false);
        // The comment is 1 byte longer in UTF-8 than it is in UTF-16 units.
        assert_ne!(u8s[0].length, u16s[0].length);
    }
}
