//! Safe bindings to Tcl's own parser.
//!
//! Tcl has no grammar independent of its implementation. How a script splits into
//! commands and words — brace nesting, quoting, backslash-newline continuation,
//! `$` and `[` substitution boundaries — is *defined by* `Tcl_ParseCommand`. Any
//! reimplementation is an approximation, so this crate calls the real thing.
//!
//! # Threading
//!
//! Every entry point here passes a NULL `Tcl_Interp *`. Tcl's parser accepts that
//! (it simply records no error message) and `Tcl_CommandComplete` takes no interp at
//! all. Because no interpreter is created, there is no per-interp mutable state to
//! protect and no thread affinity to respect, so these functions are ordinary pure
//! functions and are safe to call concurrently. This is what lets the language
//! server call them straight from async request handlers.

#![warn(clippy::undocumented_unsafe_blocks)]

use std::ops::Range;

#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code
)]
mod sys {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

/// The kind of a parse token, mirroring Tcl's `TCL_TOKEN_*` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// A word that requires substitution; composed of `num_components` sub-tokens.
    Word,
    /// A word with no substitution — a literal.
    SimpleWord,
    /// Literal text within a word.
    Text,
    /// A backslash escape sequence.
    Backslash,
    /// A `[...]` command substitution.
    Command,
    /// A `$name` / `${name}` variable substitution.
    Variable,
    /// A sub-expression (only produced by expression parsing).
    SubExpr,
    /// An operator (only produced by expression parsing).
    Operator,
    /// A `{*}` expansion word.
    ExpandWord,
    /// A token type this crate does not know about.
    Unknown(u32),
}

impl TokenKind {
    fn from_raw(raw: u32) -> Self {
        match raw {
            x if x == sys::TCL_TOKEN_WORD => TokenKind::Word,
            x if x == sys::TCL_TOKEN_SIMPLE_WORD => TokenKind::SimpleWord,
            x if x == sys::TCL_TOKEN_TEXT => TokenKind::Text,
            x if x == sys::TCL_TOKEN_BS => TokenKind::Backslash,
            x if x == sys::TCL_TOKEN_COMMAND => TokenKind::Command,
            x if x == sys::TCL_TOKEN_VARIABLE => TokenKind::Variable,
            x if x == sys::TCL_TOKEN_SUB_EXPR => TokenKind::SubExpr,
            x if x == sys::TCL_TOKEN_OPERATOR => TokenKind::Operator,
            x if x == sys::TCL_TOKEN_EXPAND_WORD => TokenKind::ExpandWord,
            other => TokenKind::Unknown(other),
        }
    }

    /// Whether this token is a literal whose text can be read directly.
    pub fn is_literal(self) -> bool {
        matches!(self, TokenKind::Text | TokenKind::SimpleWord)
    }
}

/// One parse token, with byte offsets absolute to the [`Script`] it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub size: usize,
    /// Number of following tokens that make up this one. Zero for leaf tokens.
    pub num_components: usize,
}

impl Token {
    pub fn range(&self) -> Range<usize> {
        self.start..self.start + self.size
    }
}

/// A single parsed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Byte range of the comment block preceding this command, if any.
    pub comment: Option<Range<usize>>,
    /// Byte range of the command, **including its terminating newline or `;`**
    /// (this is Tcl's own `commandSize` convention), so `range.end` is exactly
    /// where the next command begins.
    pub range: Range<usize>,
    /// Number of words, as Tcl counts them.
    pub num_words: usize,
    /// Flat token array. Structure is recovered via [`Token::num_components`];
    /// use [`Command::words`] to walk it word by word.
    pub tokens: Vec<Token>,
}

impl Command {
    /// Splits the flat token array into one slice per word.
    ///
    /// Tcl emits words as a head token followed by `num_components` component
    /// tokens; this walks that structure so callers do not have to.
    pub fn words(&self) -> Vec<&[Token]> {
        let mut out = Vec::with_capacity(self.num_words);
        let mut i = 0;
        while i < self.tokens.len() {
            let span = 1 + self.tokens[i].num_components;
            let end = (i + span).min(self.tokens.len());
            out.push(&self.tokens[i..end]);
            i = end;
        }
        out
    }
}

/// Why a parse failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Tcl parse error at byte {offset}{}", if *.incomplete { " (incomplete input)" } else { "" })]
pub struct ParseError {
    /// Byte offset at which the parser gave up.
    pub offset: usize,
    /// True when the input merely ends mid-construct — e.g. an unclosed brace.
    /// Editors hit this constantly while typing, so it is not a real error.
    pub incomplete: bool,
}

/// A NUL-terminated copy of a script, ready for repeated parsing.
///
/// Holding the buffer means the cost of copying and NUL-terminating is paid once
/// per document version rather than once per command.
pub struct Script {
    /// Script bytes followed by a trailing NUL that is not counted in `len`.
    buf: Vec<u8>,
    len: usize,
}

impl Script {
    pub fn new(text: &str) -> Self {
        let mut buf = Vec::with_capacity(text.len() + 1);
        buf.extend_from_slice(text.as_bytes());
        let len = buf.len();
        buf.push(0);
        Script { buf, len }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Returns the text of a byte range, if it is valid UTF-8.
    pub fn text(&self, range: Range<usize>) -> Option<&str> {
        let end = range.end.min(self.len);
        let start = range.start.min(end);
        std::str::from_utf8(&self.buf[start..end]).ok()
    }

    /// Parses the single command beginning at `offset`.
    ///
    /// `nested` mirrors Tcl's parameter: set it when parsing inside `[...]`, so that
    /// an unmatched `]` terminates the command.
    pub fn parse_command_at(&self, offset: usize, nested: bool) -> Result<Command, ParseError> {
        self.parse_command_within(offset, self.len, nested)
    }

    /// Parses the command at `offset`, treating `end` as the end of the input.
    ///
    /// Bounding the parse matters when descending into a nested script: a `proc`
    /// body ends at its closing brace, and telling Tcl the buffer continues to the
    /// end of the file makes it run past that brace and report a spurious error.
    pub fn parse_command_within(
        &self,
        offset: usize,
        end: usize,
        nested: bool,
    ) -> Result<Command, ParseError> {
        let end = end.min(self.len);
        if offset > end {
            return Err(ParseError {
                offset,
                incomplete: false,
            });
        }

        let base = self.buf.as_ptr();
        let mut parse = std::mem::MaybeUninit::<sys::Tcl_Parse>::zeroed();

        // SAFETY: `base` points to `self.buf`, which is NUL-terminated and lives for
        // the duration of this call. We hand Tcl a pointer `offset` bytes into it and
        // a length that stops at the real end, so it never reads past the buffer. The
        // interp argument is NULL, which Tcl documents as "do not report errors" and
        // which keeps this call free of interpreter state.
        let rc = unsafe {
            sys::Tcl_ParseCommand(
                std::ptr::null_mut(),
                base.add(offset).cast::<std::os::raw::c_char>(),
                (end - offset) as _,
                nested as _,
                parse.as_mut_ptr(),
            )
        };

        // `Tcl_Parse` is self-referential: on a small command `tokenPtr` points at the
        // struct's own `staticTokens` array. Moving or copying the struct would leave
        // `tokenPtr` dangling into the old location, and `Tcl_FreeParse` — which frees
        // iff `tokenPtr != staticTokens` — would then try to free stack memory. So the
        // struct stays put and everything below goes through a raw pointer to it.
        let guard = FreeParse(parse.as_mut_ptr());

        // SAFETY: Tcl_ParseCommand initialises the struct on every path that can be
        // reached with a non-NULL `start` (it calls TclParseInit first), including
        // failures, where it still sets `incomplete` and `term`.
        let p = unsafe { &*guard.0 };

        if rc != sys::TCL_OK as i32 {
            let term_offset = if p.term.is_null() {
                offset
            } else {
                // SAFETY: when non-null, `term` points within `self.buf`, so the
                // difference is a valid in-bounds byte offset.
                unsafe { p.term.cast::<u8>().offset_from(base).max(0) as usize }
            };
            return Err(ParseError {
                offset: term_offset,
                incomplete: p.incomplete != 0,
            });
        }
        let to_offset = |ptr: *const std::os::raw::c_char| -> usize {
            if ptr.is_null() {
                return 0;
            }
            // SAFETY: all pointers Tcl stores in Tcl_Parse point into the buffer we
            // supplied, so the offset is in bounds.
            unsafe { ptr.cast::<u8>().offset_from(base).max(0) as usize }
        };

        let comment = if p.commentStart.is_null() || p.commentSize == 0 {
            None
        } else {
            let start = to_offset(p.commentStart);
            Some(start..start + p.commentSize as usize)
        };

        let cmd_start = to_offset(p.commandStart);
        let mut tokens = Vec::with_capacity(p.numTokens as usize);
        for i in 0..p.numTokens as usize {
            // SAFETY: `tokenPtr` addresses at least `numTokens` initialised tokens,
            // per Tcl_ParseCommand's contract.
            let t = unsafe { &*p.tokenPtr.add(i) };
            tokens.push(Token {
                kind: TokenKind::from_raw(t.type_ as u32),
                start: to_offset(t.start),
                size: t.size as usize,
                num_components: t.numComponents as usize,
            });
        }

        Ok(Command {
            comment,
            range: cmd_start..cmd_start + p.commandSize as usize,
            num_words: p.numWords as usize,
            tokens,
        })
    }

    /// Iterates every top-level command in the script.
    pub fn commands(&self) -> CommandIter<'_> {
        self.commands_in(0..self.len)
    }

    /// Iterates the commands inside a byte range.
    ///
    /// This is how nested scripts — a `proc` body, a `namespace eval` body — are
    /// walked. Tcl reports token offsets absolute to the whole buffer, including for
    /// the `TEXT` token holding a braced body's interior, so a body can be re-parsed
    /// in place with no offset remapping at all.
    pub fn commands_in(&self, range: Range<usize>) -> CommandIter<'_> {
        let end = range.end.min(self.len);
        CommandIter {
            script: self,
            offset: range.start.min(end),
            end,
            done: false,
        }
    }
}

/// Releases a `Tcl_Parse` on drop, including on panic.
///
/// Holds a *pointer* rather than the struct itself. `Tcl_Parse` is self-referential —
/// `tokenPtr` points at its own inline `staticTokens` array for commands with few
/// enough tokens — so taking it by value would leave `tokenPtr` addressing the moved-from
/// location, and `Tcl_FreeParse` (which frees precisely when `tokenPtr != staticTokens`)
/// would then call `free()` on a stack address.
struct FreeParse(*mut sys::Tcl_Parse);

impl Drop for FreeParse {
    fn drop(&mut self) {
        // SAFETY: `self.0` points at a `Tcl_Parse` that Tcl_ParseCommand initialised and
        // that outlives this guard (it is a local in the calling frame, declared before
        // the guard and therefore dropped after it). Tcl_FreeParse is the documented
        // release routine, is a no-op when the static token slots sufficed, and Drop
        // guarantees it runs exactly once.
        unsafe { sys::Tcl_FreeParse(self.0) };
    }
}

/// Yields each top-level command in a [`Script`].
///
/// Iteration stops at the first hard parse error; an incomplete trailing command
/// (common while typing) is reported once and then ends iteration.
pub struct CommandIter<'a> {
    script: &'a Script,
    offset: usize,
    end: usize,
    done: bool,
}

impl Iterator for CommandIter<'_> {
    type Item = Result<Command, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.offset >= self.end {
            return None;
        }
        match self
            .script
            .parse_command_within(self.offset, self.end, false)
        {
            Ok(cmd) => {
                let next = cmd.range.end;
                // Tcl returns a zero-word command for trailing whitespace/comments.
                // Guard against a non-advancing offset so this can never spin.
                if next <= self.offset {
                    self.done = true;
                    if cmd.num_words == 0 {
                        return None;
                    }
                } else {
                    self.offset = next;
                }
                if cmd.num_words == 0 {
                    return self.next();
                }
                Some(Ok(cmd))
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// Whether `script` is a complete command — no unclosed brace, bracket or quote.
///
/// This is Tcl's own `info complete`, and is the correct way to decide whether the
/// user has finished typing before running analysis that assumes well-formed input.
pub fn command_complete(script: &str) -> bool {
    let c = std::ffi::CString::new(script);
    match c {
        // SAFETY: `s` is a valid NUL-terminated C string for the duration of the call.
        // Tcl_CommandComplete only reads it and takes no interpreter.
        Ok(s) => unsafe { sys::Tcl_CommandComplete(s.as_ptr()) != 0 },
        // An interior NUL cannot appear in a real script; treat it as incomplete
        // rather than panicking on untrusted editor input.
        Err(_) => false,
    }
}

/// The Tcl version these bindings were generated against, e.g. `"8.6"`.
pub const fn tcl_generation() -> u32 {
    if cfg!(tcl9) {
        9
    } else {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmds(src: &str) -> Vec<Command> {
        Script::new(src).commands().map(|c| c.unwrap()).collect()
    }

    #[test]
    fn parses_a_simple_command() {
        let c = cmds("puts hello");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].num_words, 2);
        assert_eq!(c[0].range, 0..10);
    }

    #[test]
    fn splits_multiple_commands() {
        let c = cmds("set a 1\nset b 2\n");
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].num_words, 3);
        assert_eq!(c[1].num_words, 3);
    }

    #[test]
    fn semicolons_separate_commands() {
        assert_eq!(cmds("set a 1; set b 2").len(), 2);
    }

    /// The payoff for using the real parser: none of this is tractable with regexes.
    #[test]
    fn handles_nested_braces_and_quotes() {
        let src = r#"proc f {a b} { if {$a > $b} { puts "a; wins {not a command}" } }"#;
        let c = cmds(src);
        assert_eq!(c.len(), 1, "the whole proc is one command");
        assert_eq!(c[0].num_words, 4);
    }

    #[test]
    fn backslash_newline_continues_a_command() {
        let c = cmds("set x \\\n  1\nset y 2\n");
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn records_comments() {
        let c = cmds("# a comment\nputs hi\n");
        assert_eq!(c.len(), 1);
        assert!(c[0].comment.is_some(), "comment should be attached");
    }

    #[test]
    fn word_offsets_are_absolute() {
        let script = Script::new("set alpha 1\nset beta 2\n");
        let all: Vec<_> = script.commands().map(|c| c.unwrap()).collect();
        let second = &all[1];
        let words = second.words();
        // First word of the second command is `set`, at byte 12.
        assert_eq!(words[0][0].start, 12);
        assert_eq!(script.text(words[1][0].range()), Some("beta"));
    }

    #[test]
    fn detects_substitution_tokens() {
        let script = Script::new("puts $x[f]");
        let c: Vec<_> = script.commands().map(|r| r.unwrap()).collect();
        let kinds: Vec<_> = c[0].tokens.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&TokenKind::Variable));
        assert!(kinds.contains(&TokenKind::Command));
    }

    #[test]
    fn expand_word_is_recognised() {
        let script = Script::new("puts {*}$args");
        let c: Vec<_> = script.commands().map(|r| r.unwrap()).collect();
        assert!(c[0].tokens.iter().any(|t| t.kind == TokenKind::ExpandWord));
    }

    #[test]
    fn unclosed_brace_reports_incomplete() {
        let script = Script::new("proc f {a b} { puts hi");
        let err = script
            .commands()
            .find_map(|r| r.err())
            .expect("expected an incomplete parse");
        assert!(
            err.incomplete,
            "unclosed brace should be flagged incomplete"
        );
    }

    #[test]
    fn command_complete_matches_info_complete() {
        assert!(command_complete("puts hi"));
        assert!(command_complete("proc f {} { puts hi }"));
        assert!(!command_complete("proc f {} { puts hi"));
        assert!(!command_complete("set x \"unterminated"));
        assert!(!command_complete("set x [f"));
    }

    #[test]
    fn empty_and_whitespace_scripts_yield_nothing() {
        assert!(cmds("").is_empty());
        assert!(cmds("   \n\t\n").is_empty());
        assert!(cmds("# only a comment\n").is_empty());
    }

    #[test]
    fn utf8_offsets_are_byte_offsets() {
        // "é" is two bytes; the parser works in bytes, and callers must too.
        let script = Script::new("set é 1");
        let c: Vec<_> = script.commands().map(|r| r.unwrap()).collect();
        let words = c[0].words();
        assert_eq!(script.text(words[1][0].range()), Some("é"));
    }

    // ---------------------------------------------------------------------
    // Regression tests for bugs in the reference implementation this project
    // learned from (github.com/jdc8/lsp, `tclp.tcl`). Each one is a real defect
    // that was found in its C shim; they are pinned here so we cannot repeat them.
    // ---------------------------------------------------------------------

    /// The shim passed the *full* script length together with an *offset* pointer,
    /// so Tcl was told the buffer ran `offset` bytes past its real end.
    #[test]
    fn parse_at_offset_uses_remaining_length() {
        let script = Script::new("set a 1\nset b 2\n");
        let cmd = script.parse_command_at(8, false).unwrap();
        // Tcl counts the terminator (newline or `;`) as part of the command, so this
        // is 8..16 rather than 8..15. That is what makes `range.end` directly usable
        // as the offset of the next command.
        assert_eq!(cmd.range, 8..16);
        assert_eq!(script.text(cmd.words()[1][0].range()), Some("b"));

        // Parsing from the very last byte must stay in bounds.
        let tail = script.parse_command_at(script.len(), false).unwrap();
        assert_eq!(tail.num_words, 0);
    }

    /// The shim zeroed a token's start offset whenever its size was 0, so empty
    /// words reported position 0 instead of where they actually are.
    #[test]
    fn empty_braced_word_has_correct_start() {
        let src = "set x {}";
        let script = Script::new(src);
        let cmd: Vec<_> = script.commands().map(|r| r.unwrap()).collect();
        let third = cmd[0].words()[2];
        let text = third
            .iter()
            .find(|t| t.kind == TokenKind::Text)
            .expect("empty braces still produce a TEXT token");
        assert_eq!(text.size, 0, "the word is empty");
        assert_eq!(
            text.start,
            src.find('}').unwrap(),
            "but its position is not 0"
        );
    }

    /// The shim never checked the return code, then read the uninitialised
    /// `Tcl_Parse`, yielding garbage token counts and offsets.
    #[test]
    fn parse_error_returns_err_not_garbage() {
        let script = Script::new("set x \"unterminated");
        let err = script.parse_command_at(0, false).unwrap_err();
        assert!(err.incomplete);
        assert!(
            err.offset <= script.len(),
            "error offset must stay in bounds"
        );
    }

    /// The shim recorded comments at `commandStart` rather than `commentStart`.
    #[test]
    fn comment_range_is_the_comment_not_the_command() {
        let src = "# hello there\nset x 1";
        let script = Script::new(src);
        let cmd: Vec<_> = script.commands().map(|r| r.unwrap()).collect();
        let comment = cmd[0].comment.clone().expect("comment recorded");
        assert_eq!(comment.start, 0);
        assert!(script.text(comment).unwrap().starts_with("# hello"));
        assert_eq!(cmd[0].range.start, 14, "command starts after the comment");
    }

    /// Tcl keeps 20 tokens inline in `Tcl_Parse` and heap-allocates beyond that.
    /// This exercises the spill path, which is where a mishandled `Tcl_FreeParse`
    /// corrupts the allocator.
    #[test]
    fn spills_past_the_static_token_array() {
        let words: Vec<String> = (0..60).map(|i| format!("w{i}")).collect();
        let src = format!("puts {}", words.join(" "));
        let script = Script::new(&src);
        let cmd: Vec<_> = script.commands().map(|r| r.unwrap()).collect();
        assert_eq!(cmd[0].num_words, 61);
        assert!(cmd[0].tokens.len() > 20, "must exceed NUM_STATIC_TOKENS");
        assert_eq!(script.text(cmd[0].words()[60][0].range()), Some("w59"));
    }

    /// Descending into a nested body must stop at that body's end. Without a bound,
    /// Tcl reads on past the closing brace and reports a spurious error — which is
    /// exactly what a corpus run over tcllib surfaced.
    #[test]
    fn nested_parses_are_bounded_by_the_body() {
        let src = "proc outer {} {\n  set a 1\n  set b 2\n}\nset after 3\n";
        let script = Script::new(src);
        // The body's interior. Note the first `{` in the source is the empty
        // argument list, so match the brace that opens a block instead.
        let open = src.find("{\n").unwrap() + 1;
        let close = src.rfind('}').unwrap();

        let inner: Vec<_> = script.commands_in(open..close).collect();
        assert!(
            inner.iter().all(|r| r.is_ok()),
            "body must parse cleanly: {inner:?}"
        );
        let inner: Vec<_> = inner.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(inner.len(), 2, "body holds exactly two commands");
        assert!(
            inner.iter().all(|c| c.range.end <= close),
            "no command may extend past the body"
        );
    }

    /// Guards the threading claim in the module docs: with a NULL interp these are
    /// pure functions, so concurrent use must not corrupt anything.
    #[test]
    fn parsing_is_thread_safe() {
        let src = "proc f {a} { expr {$a * 2} }\nset y [f 21]\n";
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        let s = Script::new(src);
                        let n = s.commands().filter(|r| r.is_ok()).count();
                        assert_eq!(n, 2);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("worker panicked");
        }
    }
}
