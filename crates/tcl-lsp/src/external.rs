//! Optional out-of-process analysers.
//!
//! `nagelfar` and `tclint` are run as subprocesses, never linked — which matters
//! because nagelfar is GPL-licensed. Each is looked up through an environment
//! variable that the Nix wrapper sets to an absolute store path (as a *default*, so
//! a user can override it), falling back to a bare name found on `PATH`.
//!
//! Every tool runs against the editor's buffer, written to a temporary file, so
//! diagnostics reflect unsaved edits rather than what is on disk.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Severity as reported by an external tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// One finding, with 0-based line and optional 0-based column.
#[derive(Debug, Clone)]
pub struct Finding {
    pub line: u32,
    pub column: Option<u32>,
    pub severity: Severity,
    pub message: String,
    pub code: Option<String>,
    pub source: &'static str,
}

/// Writes `source` to a scratch file so a tool that has no stdin mode can read it.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(source: &str, tag: &str) -> Option<Scratch> {
        let dir = std::env::temp_dir().join(format!("tcl-lsp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{tag}.tcl"));
        let mut f = std::fs::File::create(&path).ok()?;
        f.write_all(source.as_bytes()).ok()?;
        f.flush().ok()?;
        Some(Scratch { path })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn run(exe: &str, args: &[&std::ffi::OsStr]) -> Option<String> {
    let out = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    // These tools exit non-zero precisely when they have something to report, so
    // the status is not a failure signal; only an empty result is.
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(text)
}

/// Runs nagelfar over the buffer.
///
/// Output with `-H` is `<file>: <line>: <S> <message>`, where `<S>` is `E`, `W` or
/// `N`. Some messages continue onto unprefixed following lines; those are folded
/// into the preceding finding rather than dropped.
///
/// `dbs` are syntax databases layered with `-s` (repeatable): the builtin Tcl/Tk
/// one FIRST, then a project database built from the workspace's files with
/// `nagelfar -header`. The project database is what kills "Unknown command" for
/// procs defined in sibling files — nagelfar does NOT resolve those from bare
/// file arguments, only from databases.
pub fn nagelfar(source: &str, exe: &str, dbs: &[String]) -> Vec<Finding> {
    let Some(scratch) = Scratch::new(source, "nagelfar") else {
        return Vec::new();
    };

    let mut args: Vec<&std::ffi::OsStr> = vec!["-H".as_ref()];
    for db in dbs {
        args.push("-s".as_ref());
        args.push(db.as_ref());
    }
    args.push(scratch.path.as_os_str());

    let Some(text) = run(exe, &args) else {
        return Vec::new();
    };
    if text.contains("No syntax database") {
        // Misconfigured rather than clean; say so once, on stderr.
        eprintln!("nagelfar: no syntax database found; set TCL_LSP_NAGELFAR_DB");
        return Vec::new();
    }

    let mut out: Vec<Finding> = Vec::new();
    for line in text.lines() {
        match parse_nagelfar_line(line) {
            Some(f) => out.push(f),
            None => {
                // A continuation of the previous message, e.g. the second line of
                // `Bad expression: ...\nin expression "1 +_@_"`.
                if let Some(last) = out.last_mut() {
                    let t = line.trim();
                    if !t.is_empty() && !t.starts_with("Checking file") {
                        last.message.push('\n');
                        last.message.push_str(t);
                    }
                }
            }
        }
    }
    out
}

/// Builds a nagelfar syntax database from the given files (`nagelfar -header`),
/// returned as the db path on success. This is the ONE place project cross-file
/// definitions become visible to the linter. Bounded by the caller's file caps;
/// a header build is one subprocess that finishes in a second or two for any
/// realistic project.
pub fn build_header_db(
    exe: &str,
    files: &[std::path::PathBuf],
    out_path: &std::path::Path,
) -> bool {
    if files.is_empty() {
        return false;
    }
    let mut cmd = Command::new(exe);
    cmd.arg("-header")
        .arg(out_path)
        .args(files)
        .stdin(Stdio::null());
    match cmd.output() {
        Ok(o) => o.status.success() && out_path.metadata().map(|m| m.len() > 0).unwrap_or(false),
        Err(_) => false,
    }
}

/// Syntax entries for the host application's own commands, written as a small
/// companion database so nagelfar knows them.
///
/// A plugin speaks its host's API: `veles::call_llm`, `veles::json_get`,
/// `veles::hook`. Those are registered at runtime by the Rust core
/// (`Tcl_CreateObjCommand`), invisible to any static analyser, so nagelfar
/// flags every call with "Unknown command" — pure noise under real code.
/// The entries are the same shape the builtin db uses; `{x x?}`-style means
/// "known command, arbitrary word-shaped args".
pub const HOST_COMMAND_DB: &str = r#"# Host-application commands (veles-agent core, registered at runtime).
set ::syntax(veles::call_llm) {x x x? x?}
set ::syntax(veles::json_get) {x x x*}
set ::syntax(veles::hook) {x x}
set ::syntax(veles::routes) 1
"#;

/// Writes [`HOST_COMMAND_DB`] to `out_path`, returning whether it landed.
/// A failure is not fatal — the linter just keeps flagging host commands.
pub fn write_host_command_db(out_path: &std::path::Path) -> bool {
    std::fs::write(out_path, HOST_COMMAND_DB)
        .map(|_| out_path.metadata().map(|m| m.len() > 0).unwrap_or(false))
        .unwrap_or(false)
}

fn parse_nagelfar_line(line: &str) -> Option<Finding> {
    // `<file>: <line>: <S> <message>`
    let (_, rest) = line.split_once(": ")?;
    let (num, rest) = rest.split_once(": ")?;
    let lineno: u32 = num.trim().parse().ok()?;
    let mut chars = rest.trim_start().splitn(2, ' ');
    let sev = chars.next()?;
    let msg = chars.next()?.trim();
    let severity = match sev {
        "E" => Severity::Error,
        "W" => Severity::Warning,
        "N" => Severity::Info,
        _ => return None,
    };
    Some(Finding {
        // nagelfar counts lines from 1; LSP from 0.
        line: lineno.saturating_sub(1),
        column: None,
        severity,
        message: msg.to_string(),
        code: None,
        source: "nagelfar",
    })
}

/// Runs tclint over the buffer.
///
/// Output is `<file>:<line>:<col>: <message>`, optionally ending in
/// `[violation-code]`. Unlike nagelfar it reports real columns.
pub fn tclint(source: &str, exe: &str) -> Vec<Finding> {
    let Some(scratch) = Scratch::new(source, "tclint") else {
        return Vec::new();
    };
    let Some(text) = run(exe, &[scratch.path.as_os_str()]) else {
        return Vec::new();
    };
    text.lines().filter_map(parse_tclint_line).collect()
}

fn parse_tclint_line(line: &str) -> Option<Finding> {
    // Split off `<file>:<line>:<col>: ` — the path itself may contain colons, so
    // work backwards from the message instead of splitting left to right.
    let (head, message) = {
        let mut parts = line.splitn(4, ':');
        let _file = parts.next()?;
        let l = parts.next()?;
        let c = parts.next()?;
        let rest = parts.next()?;
        ((l, c), rest.trim().to_string())
    };
    let lineno: u32 = head.0.trim().parse().ok()?;
    let col: u32 = head.1.trim().parse().ok()?;

    let (message, code) = match (message.rfind('['), message.rfind(']')) {
        (Some(a), Some(b)) if b > a && b == message.len() - 1 => (
            message[..a].trim().to_string(),
            Some(message[a + 1..b].to_string()),
        ),
        _ => (message, None),
    };

    Some(Finding {
        line: lineno.saturating_sub(1),
        column: Some(col.saturating_sub(1)),
        severity: if message.starts_with("syntax error") {
            Severity::Error
        } else {
            Severity::Warning
        },
        message,
        code,
        source: "tclint",
    })
}

/// Formats `source` with `tclfmt`, returning the formatted text.
///
/// `tclfmt` has neither a stdin mode nor a range mode, so the buffer goes through
/// a temporary file and range formatting is necessarily whole-file.
pub fn tclfmt(source: &str, exe: &str) -> Option<String> {
    let scratch = Scratch::new(source, "tclfmt")?;
    let out = Command::new(exe)
        .arg(&scratch.path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    // tclfmt prints diagnostics instead of source when the file does not parse.
    if text.is_empty() || text.contains("syntax error:") {
        return None;
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nagelfar_output() {
        let f = parse_nagelfar_line("/tmp/x.tcl: 2: E Unknown variable \"nam\"").unwrap();
        assert_eq!(f.line, 1, "1-based input becomes 0-based");
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(f.message, "Unknown variable \"nam\"");
        assert_eq!(f.source, "nagelfar");
    }

    #[test]
    fn maps_nagelfar_severities() {
        assert_eq!(
            parse_nagelfar_line("/t.tcl: 9: W Something")
                .unwrap()
                .severity,
            Severity::Warning
        );
        assert_eq!(
            parse_nagelfar_line("/t.tcl: 9: N Something")
                .unwrap()
                .severity,
            Severity::Info
        );
    }

    #[test]
    fn ignores_nagelfar_banner_lines() {
        assert!(parse_nagelfar_line("Checking file /tmp/x.tcl").is_none());
    }

    #[test]
    fn parses_tclint_output_with_columns() {
        let f = parse_tclint_line("/tmp/x.tcl:3:21: syntax error: bad expression").unwrap();
        assert_eq!(f.line, 2);
        assert_eq!(f.column, Some(20));
        assert_eq!(f.severity, Severity::Error);
    }

    #[test]
    fn extracts_tclint_violation_codes() {
        let f = parse_tclint_line("/x.tcl:2:3: too many args for puts [command-args]").unwrap();
        assert_eq!(f.code.as_deref(), Some("command-args"));
        assert_eq!(f.message, "too many args for puts");
        assert_eq!(f.severity, Severity::Warning);
    }

    #[test]
    fn ignores_unparseable_lines() {
        assert!(parse_tclint_line("not a diagnostic").is_none());
        assert!(parse_tclint_line("").is_none());
    }
}
