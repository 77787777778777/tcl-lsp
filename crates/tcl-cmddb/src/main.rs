//! Generates the Tcl/Tk command database from the official man pages.
//!
//! The man pages shipped with a Tcl/Tk build are the authoritative, versioned
//! description of that build's command set, and they are already installed in the
//! Nix store. Parsing them gives an 8.6 database and a 9.0 database that track the
//! toolchain automatically, with no hand-maintained list to fall out of date.
//!
//! Only the small subset of troff that Tcl's `man.macros` actually uses is
//! handled — see `SUPPORTED MACROS` below.
//!
//! Usage:
//!   tcl-cmddb --version 8.6 --tcl-man DIR [--tk-man DIR] --out FILE

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tcl_analysis::kb::{Command, Kb, OptionSpec, Subcommand};

fn main() -> Result<()> {
    let mut version = String::new();
    let mut tcl_man: Option<PathBuf> = None;
    let mut tk_man: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--version" => version = args.next().unwrap_or_default(),
            "--tcl-man" => tcl_man = args.next().map(PathBuf::from),
            "--tk-man" => tk_man = args.next().map(PathBuf::from),
            "--out" => out = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                eprintln!("usage: tcl-cmddb --version V --tcl-man DIR [--tk-man DIR] --out FILE");
                return Ok(());
            }
            other => bail!("unknown argument: {other}"),
        }
    }

    let Some(tcl_man) = tcl_man else {
        bail!("--tcl-man is required")
    };
    let Some(out) = out else {
        bail!("--out is required")
    };
    if version.is_empty() {
        bail!("--version is required");
    }

    let mut commands = Vec::new();
    collect(&tcl_man, "tcl", &mut commands)?;
    if let Some(tk) = &tk_man {
        collect(tk, "tk", &mut commands)?;
    }
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands.dedup_by(|a, b| a.name == b.name);

    let kb = Kb::new(version, commands);

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&out, kb.to_json()).with_context(|| format!("writing {}", out.display()))?;
    eprintln!("wrote {} commands to {}", kb.len(), out.display());
    Ok(())
}

fn collect(dir: &Path, package: &str, out: &mut Vec<Command>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(text) = read_maybe_gzip(&path) else {
            continue;
        };
        if let Some(cmd) = parse_page(&text, package) {
            out.push(cmd);
        }
    }
    Ok(())
}

/// Reads a man page, transparently handling the `.gz` that Nix installs.
fn read_maybe_gzip(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if name.ends_with(".gz") {
        // Shelling out to gzip avoids a dependency for a build-time-only tool.
        let out = std::process::Command::new("gzip")
            .arg("-dc")
            .arg(path)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else if name.ends_with(".n") || name.ends_with(".1") || name.ends_with(".3") {
        let mut s = String::new();
        std::fs::File::open(path)
            .ok()?
            .read_to_string(&mut s)
            .ok()?;
        Some(s)
    } else {
        None
    }
}

/// Strips the troff font and escape sequences Tcl's man pages use.
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // Font changes: \fB \fI \fR \fP, and the \f(CW two-letter form.
            Some('f') => {
                if chars.next() == Some('(') {
                    chars.next();
                    chars.next();
                }
            }
            // \s±N size changes.
            Some('s') => {
                if matches!(chars.peek(), Some('+') | Some('-')) {
                    chars.next();
                }
                while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                    chars.next();
                }
            }
            Some('-') => out.push('-'),
            Some('&') | Some('%') => {}
            Some('e') => out.push('\\'),
            Some('|') | Some('^') => {}
            Some(' ') => out.push(' '),
            Some('(') => {
                // \(bu and friends; drop the two-letter name.
                chars.next();
                chars.next();
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out.trim().to_string()
}

/// Extracts one command from a man page, or `None` if the page documents none.
fn parse_page(text: &str, package: &str) -> Option<Command> {
    let mut cmd = Command {
        package: package.to_string(),
        ..Default::default()
    };

    #[derive(PartialEq)]
    enum Section {
        None,
        Name,
        Synopsis,
        Description,
        Other,
    }
    let mut section = Section::None;
    let mut description = String::new();
    let mut pending_op: Option<OptionSpec> = None;
    let mut pending_tp: Option<Subcommand> = None;
    let mut want_tp_signature = false;

    for raw in text.lines() {
        let line = raw.trim_end();

        // Comments.
        if line.starts_with(".\\\"") || line.starts_with("'\\\"") {
            continue;
        }

        if let Some(rest) = line.strip_prefix(".TH ") {
            // `.TH lsort n 8.5 Tcl "Tcl Built-In Commands"`
            let name = clean(rest.split_whitespace().next().unwrap_or(""));
            if !name.is_empty() {
                cmd.name = name;
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix(".SH ") {
            flush_op(&mut pending_op, &mut cmd);
            flush_tp(&mut pending_tp, &mut cmd);
            let head = rest.trim().trim_matches('"').to_ascii_uppercase();
            section = match head.as_str() {
                "NAME" => Section::Name,
                "SYNOPSIS" => Section::Synopsis,
                "DESCRIPTION" => Section::Description,
                _ => Section::Other,
            };
            continue;
        }

        // `.OP \-command command Command` introduces a Tk widget option.
        if let Some(rest) = line.strip_prefix(".OP ") {
            flush_op(&mut pending_op, &mut cmd);
            flush_tp(&mut pending_tp, &mut cmd);
            let parts: Vec<String> = rest.split_whitespace().map(clean).collect();
            pending_op = Some(OptionSpec {
                flag: parts.first().cloned().unwrap_or_default(),
                db_name: parts.get(1).cloned().unwrap_or_default(),
                db_class: parts.get(2).cloned().unwrap_or_default(),
                doc: String::new(),
            });
            continue;
        }

        // `.TP` starts a tagged paragraph; in DESCRIPTION these are subcommands.
        if line.starts_with(".TP") {
            flush_op(&mut pending_op, &mut cmd);
            flush_tp(&mut pending_tp, &mut cmd);
            if section == Section::Description {
                want_tp_signature = true;
            }
            continue;
        }

        // Other macros carry no content we need.
        if line.starts_with('.') {
            if line.starts_with(".BE") {
                flush_op(&mut pending_op, &mut cmd);
            }
            continue;
        }

        if line.trim().is_empty() {
            continue;
        }

        if want_tp_signature {
            want_tp_signature = false;
            let sig = clean(line);
            // A subcommand signature starts with the command name, e.g.
            // `string cat ?string1?`. Anything else is ordinary prose.
            if let Some(rest) = sig.strip_prefix(&format!("{} ", cmd.name)) {
                if let Some(word) = rest.split_whitespace().next() {
                    if word
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                        && !word.starts_with('?')
                    {
                        pending_tp = Some(Subcommand {
                            name: word.to_string(),
                            signature: sig,
                            doc: String::new(),
                        });
                        continue;
                    }
                }
            }
            continue;
        }

        match section {
            Section::Name => {
                // `lsort \- Sort the elements of a list`
                let cleaned = clean(line);
                if let Some((names, summary)) = cleaned.split_once(" - ") {
                    if cmd.name.is_empty() {
                        cmd.name = names.split(',').next().unwrap_or("").trim().to_string();
                    }
                    cmd.summary = summary.trim().to_string();
                }
            }
            Section::Synopsis => {
                let s = clean(line);
                if !s.is_empty() {
                    cmd.synopsis.push(s);
                }
            }
            Section::Description => {
                if let Some(sub) = pending_tp.as_mut() {
                    append_prose(&mut sub.doc, &clean(line));
                } else if description.len() < 600 {
                    append_prose(&mut description, &clean(line));
                }
            }
            _ => {
                if let Some(op) = pending_op.as_mut() {
                    append_prose(&mut op.doc, &clean(line));
                }
            }
        }

        // `.OP` paragraphs live in an ARGUMENTS/WIDGET-SPECIFIC OPTIONS section,
        // which is `Section::Other` above, but may also appear in DESCRIPTION.
        if section == Section::Description {
            if let Some(op) = pending_op.as_mut() {
                append_prose(&mut op.doc, &clean(line));
            }
        }
    }

    flush_op(&mut pending_op, &mut cmd);
    flush_tp(&mut pending_tp, &mut cmd);
    cmd.description = description.trim().to_string();

    if cmd.name.is_empty() || cmd.name.contains(' ') {
        return None;
    }
    // Skip C API pages (`Tcl_*`) and pure overview pages with nothing usable.
    if cmd.name.starts_with("Tcl_") || cmd.name.starts_with("Tk_") {
        return None;
    }
    if cmd.summary.is_empty() && cmd.synopsis.is_empty() {
        return None;
    }
    Some(cmd)
}

fn append_prose(buf: &mut String, line: &str) {
    if line.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push(' ');
    }
    buf.push_str(line);
}

fn flush_op(pending: &mut Option<OptionSpec>, cmd: &mut Command) {
    if let Some(mut op) = pending.take() {
        op.doc = truncate(&op.doc, 300);
        if !op.flag.is_empty() {
            cmd.options.push(op);
        }
    }
}

fn flush_tp(pending: &mut Option<Subcommand>, cmd: &mut Command) {
    if let Some(mut sub) = pending.take() {
        sub.doc = truncate(&sub.doc, 300);
        if !cmd.subcommands.iter().any(|s| s.name == sub.name) {
            cmd.subcommands.push(sub);
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", s[..end].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_troff_font_escapes() {
        assert_eq!(
            clean(r"\fBlsort \fR?\fIoptions\fR? \fIlist\fR"),
            "lsort ?options? list"
        );
        assert_eq!(clean(r"a \- b"), "a - b");
        assert_eq!(clean(r"\&text"), "text");
    }

    const LSORT: &str = r#".TH lsort n 8.5 Tcl "Tcl Built-In Commands"
.SH NAME
lsort \- Sort the elements of a list
.SH SYNOPSIS
\fBlsort \fR?\fIoptions\fR? \fIlist\fR
.BE
.SH DESCRIPTION
.PP
This command sorts the elements of \fIlist\fR, returning a new
list in sorted order.
"#;

    #[test]
    fn parses_a_simple_command_page() {
        let c = parse_page(LSORT, "tcl").expect("a command");
        assert_eq!(c.name, "lsort");
        assert_eq!(c.summary, "Sort the elements of a list");
        assert_eq!(c.synopsis, vec!["lsort ?options? list"]);
        assert!(c.description.starts_with("This command sorts"));
        assert_eq!(c.package, "tcl");
    }

    const STRING: &str = r#".TH string n 8.1 Tcl "Tcl Built-In Commands"
.SH NAME
string \- Manipulate strings
.SH SYNOPSIS
\fBstring \fIoption arg \fR?\fIarg ...?\fR
.BE
.SH DESCRIPTION
.PP
Performs one of several string operations.
.TP
\fBstring cat\fR ?\fIstring1\fR? ?\fIstring2...\fR?
.
Concatenate the given strings just like placing them directly next to each other.
.TP
\fBstring compare\fR ?\fB\-nocase\fR? \fIstring1 string2\fR
.
Perform a character-by-character comparison.
"#;

    #[test]
    fn extracts_ensemble_subcommands() {
        let c = parse_page(STRING, "tcl").expect("a command");
        assert_eq!(c.name, "string");
        let names: Vec<&str> = c.subcommands.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["cat", "compare"]);
        assert!(c.subcommands[0].doc.starts_with("Concatenate"));
        assert_eq!(
            c.subcommands[0].signature,
            "string cat ?string1? ?string2...?"
        );
    }

    const TTK_BUTTON: &str = r#".TH ttk::button n 8.5 Tk "Tk Themed Widget"
.SH NAME
ttk::button \- Widget that issues a command when pressed
.SH SYNOPSIS
\fBttk::button\fR \fIpathName \fR?\fIoptions\fR?
.BE
.SH "WIDGET OPTIONS"
.OP \-command command Command
A script to evaluate when the widget is invoked.
.OP \-default default Default
May be set to one of \fBnormal\fR, \fBactive\fR, or \fBdisabled\fR.
"#;

    #[test]
    fn extracts_tk_widget_options() {
        let c = parse_page(TTK_BUTTON, "tk").expect("a command");
        assert_eq!(c.name, "ttk::button");
        let flags: Vec<&str> = c.options.iter().map(|o| o.flag.as_str()).collect();
        assert_eq!(flags, vec!["-command", "-default"]);
        assert_eq!(c.options[0].db_name, "command");
        assert_eq!(c.options[0].db_class, "Command");
        assert!(c.options[0].doc.starts_with("A script to evaluate"));
    }

    #[test]
    fn skips_c_api_pages() {
        let page = ".TH Tcl_ParseCommand 3 8.1 Tcl\n.SH NAME\nTcl_ParseCommand \\- parse\n";
        assert!(parse_page(page, "tcl").is_none());
    }

    #[test]
    fn skips_pages_with_no_usable_content() {
        assert!(parse_page(".TH nothing n\n", "tcl").is_none());
    }
}
