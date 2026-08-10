//! Parses a large body of real-world Tcl and asserts we handle all of it.
//!
//! tcllib is valid Tcl by construction, which makes it a true oracle: any parse
//! error we report on it is our bug, not theirs. The corpus path is supplied by the
//! Nix dev shell / check as `TCL_LSP_CORPUS`; without it the test skips, so a plain
//! `cargo test` on a machine with no tcllib still passes.

use std::path::{Path, PathBuf};
use tcl_syntax::{outline, Script};

fn tcl_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            tcl_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "tcl") {
            out.push(path);
        }
    }
}

#[test]
fn parses_the_tcllib_corpus_without_errors() {
    let Ok(root) = std::env::var("TCL_LSP_CORPUS") else {
        eprintln!("TCL_LSP_CORPUS unset; skipping corpus test");
        return;
    };

    let mut files = Vec::new();
    tcl_files(Path::new(&root), &mut files);
    assert!(
        files.len() > 100,
        "expected a substantial corpus under {root}, found {} files",
        files.len()
    );

    let mut failures = Vec::new();
    let mut symbols = 0usize;
    let mut parsed = 0usize;

    for path in &files {
        // A handful of files in any large corpus are test fixtures in other
        // encodings; those are not our concern.
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        parsed += 1;

        let script = Script::new(&text);
        let result = outline(&script);
        symbols += count(&result.symbols);

        // Reconstruction invariant: commands must be reported in ascending,
        // non-overlapping order and stay inside the buffer. This one check catches
        // essentially every class of offset bug.
        let mut prev_end = 0usize;
        for cmd in script.commands().flatten() {
            assert!(
                cmd.range.start >= prev_end,
                "{}: command at {:?} overlaps the previous one ending at {prev_end}",
                path.display(),
                cmd.range
            );
            assert!(
                cmd.range.end <= script.len(),
                "{}: command range {:?} runs past the buffer ({})",
                path.display(),
                cmd.range,
                script.len()
            );
            prev_end = cmd.range.end;
        }

        // tcllib is valid Tcl, so any hard syntax error is ours.
        if let Some(err) = result.errors.iter().find(|e| !e.incomplete) {
            failures.push(format!("{}: {}", path.display(), err.message));
        }
    }

    eprintln!("corpus: {parsed} files parsed, {symbols} symbols found");
    assert!(parsed > 100, "too few readable files in the corpus");
    assert!(
        symbols > 1000,
        "expected thousands of symbols across tcllib, found {symbols}"
    );
    assert!(
        failures.is_empty(),
        "{} files failed to parse:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn count(symbols: &[tcl_syntax::Symbol]) -> usize {
    symbols.iter().map(|s| 1 + count(&s.children)).sum()
}
