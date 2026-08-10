//! Locating the construct under the cursor.
//!
//! Both selection ranges and signature help need the same thing: the chain of
//! nested constructs containing a byte offset. Descending through Tcl's own parser
//! gets this exactly right where a bracket-counting heuristic would not — a `}`
//! inside a quoted string is not a scope boundary, and only the parser knows that.

use std::ops::Range;

use tcl_tclsys::{Command, Script, Token, TokenKind};

use crate::outline::{literal, word_range};

/// The interior of a word that actually contains further code or text.
///
/// Only `{...}`, `"..."` and `[...]` have an inside worth descending into. A bare
/// word like `compare` also has a TEXT sub-token spanning exactly itself, so
/// descending on that alone would recurse forever on the same range — which is
/// precisely what a naive implementation does.
fn interior(script: &Script, word: &[Token]) -> Option<Range<usize>> {
    let head = word.first()?;
    if !matches!(head.kind, TokenKind::Word | TokenKind::SimpleWord) {
        return None;
    }
    let text = script.text(head.range())?;
    let inner = match text.chars().next()? {
        '{' | '"' => word.get(1).map(|t| t.range())?,
        // A command substitution's token spans the brackets too, so strip them.
        '[' if text.ends_with(']') => head.start + 1..head.start + head.size - 1,
        _ => return None,
    };
    (inner.end > inner.start && inner != head.range()).then_some(inner)
}

/// One step of the containment chain.
pub struct Enclosing {
    /// The command containing the offset.
    pub command: Command,
    /// Its first word, if that is a static literal — i.e. the command's name.
    pub name: Option<String>,
    /// Index of the word the cursor sits in or immediately after. Word 0 is the
    /// command name, so argument *n* is word *n*.
    pub word_index: usize,
    /// Range of the word the cursor is in, when it is inside one.
    pub word_range: Option<Range<usize>>,
}

/// The innermost command containing `offset`, descending through nested bodies.
///
/// Returns `None` when the offset is outside any command, or when the text does
/// not parse — which is normal for a buffer mid-edit, and callers should treat as
/// "no information" rather than an error.
pub fn command_at(script: &Script, offset: usize) -> Option<Enclosing> {
    let mut search = 0..script.len();
    let mut found: Option<Enclosing> = None;

    // A body can nest arbitrarily; bound the descent for the same reason the
    // outline walker does.
    for _ in 0..100 {
        let Some(cmd) = command_containing(script, search.clone(), offset) else {
            break;
        };
        // Everything derived from `words` must be computed before `cmd` is moved
        // into the result, since the word slices borrow its token array.
        let words = cmd.words();
        let (word_index, in_word) = locate_word(&words, offset);
        let name = words.first().and_then(|w| literal(script, w));
        // Descend into a braced body only when the cursor is genuinely inside it.
        let descend = words
            .get(word_index)
            .and_then(|w| interior(script, w))
            .filter(|b| b.contains(&offset));
        drop(words);

        found = Some(Enclosing {
            name,
            word_index,
            word_range: in_word,
            command: cmd,
        });
        match descend {
            Some(body) if body.end > body.start => search = body,
            _ => break,
        }
    }
    found
}

/// The chain of ranges around `offset`, innermost first.
///
/// Editors bind this to "expand selection": each successive range is a strict
/// superset of the last, ending at the whole file.
pub fn selection_chain(script: &Script, offset: usize) -> Vec<Range<usize>> {
    // Collected outermost-first while descending, then reversed.
    let mut outer_first: Vec<Range<usize>> = Vec::new();
    let mut search = 0..script.len();

    for _ in 0..100 {
        let Some(cmd) = command_containing(script, search.clone(), offset) else {
            break;
        };
        push_range(&mut outer_first, cmd.range.clone());

        let words = cmd.words();
        let (idx, _) = locate_word(&words, offset);
        let Some(word) = words.get(idx) else { break };
        let Some(span) = word_range(word) else { break };
        if !span.contains(&offset) {
            break;
        }
        push_range(&mut outer_first, span.clone());

        match interior(script, word) {
            // A braced, quoted or bracketed word contributes a second, inner range:
            // the text without its delimiters. Descend and keep going.
            Some(inner) if inner.contains(&offset) => {
                push_range(&mut outer_first, inner.clone());
                search = inner;
            }
            _ => break,
        }
    }

    push_range(&mut outer_first, 0..script.len());
    outer_first.sort_by_key(|r| (r.end - r.start, r.start));
    outer_first.dedup();
    outer_first
}

fn push_range(out: &mut Vec<Range<usize>>, r: Range<usize>) {
    if r.end > r.start && !out.contains(&r) {
        out.push(r);
    }
}

fn command_containing(script: &Script, range: Range<usize>, offset: usize) -> Option<Command> {
    script
        .commands_in(range)
        .flatten()
        .find(|c| c.range.start <= offset && offset <= c.range.end)
}

/// Which word the cursor is in, or which one it is about to become.
///
/// A cursor in the whitespace after word *k* reports *k+1*, because that is the
/// argument the user is starting to type — which is what signature help needs.
fn locate_word(words: &[&[Token]], offset: usize) -> (usize, Option<Range<usize>>) {
    for (i, w) in words.iter().enumerate() {
        if let Some(span) = word_range(w) {
            if span.start <= offset && offset <= span.end {
                return (i, Some(span));
            }
        }
    }
    let passed = words
        .iter()
        .filter(|w| word_range(w).is_some_and(|s| s.end < offset))
        .count();
    (passed, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(src: &str, needle: &str) -> Vec<String> {
        let script = Script::new(src);
        let at = src.find(needle).expect("needle present");
        selection_chain(&script, at)
            .into_iter()
            .map(|r| src[r].to_string())
            .collect()
    }

    #[test]
    fn chain_grows_from_word_to_command_to_file() {
        let got = chain("puts hello\n", "hello");
        assert_eq!(got[0], "hello");
        assert!(got[1].starts_with("puts hello"));
        assert_eq!(got.last().unwrap(), "puts hello\n");
    }

    #[test]
    fn chain_descends_through_a_proc_body() {
        let src = "proc f {} {\n    set x 1\n}\n";
        let got = chain(src, "x 1");
        // Innermost is the word, then the command, then the body, then the proc.
        assert_eq!(got[0], "x");
        // A command's range includes its terminating newline, per Tcl's own
        // `commandSize` convention.
        assert!(
            got.iter().any(|r| r.trim_end() == "set x 1"),
            "expected the enclosing command in {got:?}"
        );
        assert!(
            got.iter()
                .any(|r| r.contains("set x 1") && r.contains('\n')),
            "expected the proc body in {got:?}"
        );
        assert_eq!(got.last().unwrap(), src);
    }

    #[test]
    fn chain_ranges_strictly_grow() {
        let src = "namespace eval a {\n proc f {} {\n  puts hi\n }\n}\n";
        let script = Script::new(src);
        let at = src.find("hi").unwrap();
        let ranges = selection_chain(&script, at);
        for pair in ranges.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            assert!(
                b.start <= a.start && b.end >= a.end && (b.end - b.start) > (a.end - a.start),
                "each range must strictly contain the previous: {a:?} then {b:?}"
            );
        }
    }

    /// A brace inside a quoted string is not a scope boundary. Only the real
    /// parser knows that, which is the whole reason this is not done with regexes.
    #[test]
    fn braces_inside_strings_do_not_break_the_chain() {
        let src = "proc f {} {\n    puts \"a } b\"\n}\n";
        let script = Script::new(src);
        let at = src.find("a } b").unwrap();
        let ranges = selection_chain(&script, at);
        assert_eq!(&src[ranges.last().unwrap().clone()], src);
    }

    #[test]
    fn command_at_reports_the_command_name() {
        let src = "puts [lsort $x]\n";
        let script = Script::new(src);
        let at = src.find("$x").unwrap();
        let e = command_at(&script, at).expect("a command");
        assert_eq!(e.name.as_deref(), Some("lsort"));
    }

    #[test]
    fn command_at_finds_the_innermost_command() {
        let src = "proc f {} {\n    lsort $items\n}\n";
        let script = Script::new(src);
        let at = src.find("$items").unwrap();
        let e = command_at(&script, at).expect("a command");
        assert_eq!(e.name.as_deref(), Some("lsort"));
        assert_eq!(e.word_index, 1, "the cursor is on the first argument");
    }

    #[test]
    fn word_index_counts_arguments() {
        // Distinct words, so a needle cannot accidentally match inside another.
        let src = "string compare alpha beta\n";
        let script = Script::new(src);
        for (needle, want) in [
            ("string", 0usize),
            ("compare", 1),
            ("alpha", 2),
            ("beta", 3),
        ] {
            let at = src.find(needle).unwrap();
            let e = command_at(&script, at).expect("a command");
            assert_eq!(e.word_index, want, "for {needle}");
        }
    }

    /// Typing a fresh argument: the cursor is past the last word, so the index is
    /// the argument about to be written, not the one before it.
    #[test]
    fn word_index_advances_in_trailing_whitespace() {
        let src = "lsort -unique ";
        let script = Script::new(src);
        let e = command_at(&script, src.len()).expect("a command");
        assert_eq!(e.word_index, 2);
    }

    #[test]
    fn no_command_in_an_empty_script() {
        let script = Script::new("");
        assert!(command_at(&script, 0).is_none());
        assert!(selection_chain(&script, 0).is_empty());
    }
}
