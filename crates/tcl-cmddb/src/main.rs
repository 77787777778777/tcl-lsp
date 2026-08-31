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
    let mut inherited: Vec<Inherited> = Vec::new();
    collect(&tcl_man, "tcl", &mut commands, &mut inherited)?;
    if let Some(tk) = &tk_man {
        collect(tk, "tk", &mut commands, &mut inherited)?;
    }
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands.dedup_by(|a, b| a.name == b.name);

    resolve_standard_options(&mut commands, &inherited);
    apply_option_aliases(&mut commands);

    let kb = Kb::new(version, commands);

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&out, kb.to_json()).with_context(|| format!("writing {}", out.display()))?;
    eprintln!("wrote {} commands to {}", kb.len(), out.display());
    Ok(())
}

/// A widget's `.SO` block: the options it inherits, and the page they come from.
struct Inherited {
    command: String,
    /// Page the options are documented on: `options` unless `.SO` names another.
    from_page: String,
    flags: Vec<String>,
}

fn collect(
    dir: &Path,
    package: &str,
    out: &mut Vec<Command>,
    inherited: &mut Vec<Inherited>,
) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(text) = read_maybe_gzip(&path) else {
            continue;
        };
        if let Some((cmd, inh)) = parse_page(&text, package) {
            if let Some(mut inh) = inh {
                inh.command = cmd.name.clone();
                inherited.push(inh);
            }
            out.push(cmd);
        }
    }
    Ok(())
}

/// Copies each `.SO`-inherited option's documentation in from the page that
/// defines it, so a widget's option list is complete rather than only listing
/// what its own page happens to spell out.
///
/// Without this a `ttk::button` appears not to accept `-cursor`, which it plainly
/// does — the reason validating options was not safe before.
fn resolve_standard_options(commands: &mut [Command], inherited: &[Inherited]) {
    // Page name -> its documented options. `.SO` names a page (`ttk_widget`),
    // whose command is spelled with `::` (`ttk::widget`).
    let by_page: std::collections::HashMap<String, Vec<OptionSpec>> = commands
        .iter()
        .map(|c| (c.name.replace("::", "_"), c.options.clone()))
        .collect();

    let mut additions: Vec<(String, Vec<OptionSpec>)> = Vec::new();
    for inh in inherited {
        let source = by_page.get(&inh.from_page);
        let mut add = Vec::new();
        for flag in &inh.flags {
            let mut spec = source
                .and_then(|opts| opts.iter().find(|o| &o.flag == flag))
                .cloned()
                .unwrap_or_else(|| OptionSpec {
                    flag: flag.clone(),
                    ..Default::default()
                });
            spec.standard = true;
            add.push(spec);
        }
        additions.push((inh.command.clone(), add));
    }

    for (name, add) in additions {
        let Some(cmd) = commands.iter_mut().find(|c| c.name == name) else {
            continue;
        };
        for spec in add {
            if !cmd.options.iter().any(|o| o.flag == spec.flag) {
                cmd.options.push(spec);
            }
        }
        cmd.options.sort_by(|a, b| a.flag.cmp(&b.flag));
    }
}

/// Propagates documented option aliases across every command that takes them.
///
/// Tk records aliases inline on one page — `.OP "\-background or \-bg"` — but a
/// widget that documents `-background` on its own page never mentions `-bg`, even
/// though it accepts it. Two flags naming the same option-database entry are the
/// same option, so wherever one is valid the other is too.
fn apply_option_aliases(commands: &mut [Command]) {
    use std::collections::{BTreeSet, HashMap};

    let mut groups: HashMap<(String, String), BTreeSet<String>> = HashMap::new();
    for c in commands.iter() {
        for o in &c.options {
            if o.db_name.is_empty() {
                continue;
            }
            groups
                .entry((o.db_name.clone(), o.db_class.clone()))
                .or_default()
                .insert(o.flag.clone());
        }
    }
    // Only groups that actually name an option more than one way are aliases.
    groups.retain(|_, flags| flags.len() > 1);

    for c in commands.iter_mut() {
        let mut add: Vec<OptionSpec> = Vec::new();
        for o in &c.options {
            let Some(group) = groups.get(&(o.db_name.clone(), o.db_class.clone())) else {
                continue;
            };
            for flag in group {
                let known = c.options.iter().any(|x| &x.flag == flag)
                    || add.iter().any(|x| &x.flag == flag);
                if !known {
                    add.push(OptionSpec {
                        flag: flag.clone(),
                        db_name: o.db_name.clone(),
                        db_class: o.db_class.clone(),
                        doc: o.doc.clone(),
                        standard: o.standard,
                    });
                }
            }
        }
        c.options.extend(add);
        c.options.sort_by(|a, b| a.flag.cmp(&b.flag));
    }
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
fn parse_page(text: &str, package: &str) -> Option<(Command, Option<Inherited>)> {
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
    let mut pending_op: Option<PendingOp> = None;
    let mut pending_tp: Option<Subcommand> = None;
    let mut want_tp_signature = false;
    // `.SO ?page?` opens a block naming the options this widget inherits.
    let mut in_so = false;
    let mut so_page = String::new();
    let mut so_flags: Vec<String> = Vec::new();

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
                // Widget pages document their subcommands under this heading:
                // `.TP` rows of `pathName <sub> ...` + prose. Those are exactly
                // what a hover on a `$w insert` dispatch word wants to show.
                "WIDGET COMMAND" => Section::Description,
                _ => Section::Other,
            };
            continue;
        }

        // `.SO ?page?` … `.SE` lists the standard options a widget inherits. The
        // names appear inline; their documentation lives on the named page, or on
        // `options(n)` when no page is given.
        if let Some(rest) = line.strip_prefix(".SO") {
            flush_op(&mut pending_op, &mut cmd);
            in_so = true;
            let named = rest.trim();
            so_page = if named.is_empty() {
                "options".to_string()
            } else {
                named.to_string()
            };
            continue;
        }
        if line.starts_with(".SE") {
            in_so = false;
            continue;
        }
        if in_so {
            for word in clean(line).split_whitespace() {
                if word.starts_with('-') {
                    so_flags.push(word.to_string());
                }
            }
            continue;
        }

        // `.OP \-command command Command` introduces a Tk widget option.
        if let Some(rest) = line.strip_prefix(".OP ") {
            flush_op(&mut pending_op, &mut cmd);
            flush_tp(&mut pending_tp, &mut cmd);
            let parts = split_fields(rest);
            // The first field may name an alias too, quoted:
            //   .OP "\-borderwidth or \-bd" borderWidth BorderWidth
            let flags: Vec<String> = parts
                .first()
                .map(|f| {
                    f.split(" or ")
                        .map(|s| clean(s.trim()))
                        .filter(|s| s.starts_with('-'))
                        .collect()
                })
                .unwrap_or_default();
            pending_op = (!flags.is_empty()).then(|| PendingOp {
                flags,
                db_name: parts.get(1).map(|s| clean(s)).unwrap_or_default(),
                db_class: parts.get(2).map(|s| clean(s)).unwrap_or_default(),
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
            // `string cat ?string1?`. Widget pages instead write
            // `pathName bbox index` — `pathName` is their stand-in for the
            // widget's own name. Anything else is ordinary prose.
            let path_prefix = format!("{} ", cmd.name);
            let path_name_prefix = "pathName ".to_string();
            let (from_name, rest) = if let Some(r) = sig.strip_prefix(&path_prefix) {
                (true, r)
            } else if let Some(r) = sig.strip_prefix(&path_name_prefix) {
                (false, r)
            } else {
                continue;
            };
            if let Some(word) = rest.split_whitespace().next() {
                if word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    && !word.starts_with('?')
                {
                    let _ = from_name;
                    pending_tp = Some(Subcommand {
                        name: word.to_string(),
                        signature: sig,
                        doc: String::new(),
                    });
                    continue;
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
                    // The literal `.` line that fills troff blank lines is not prose.
                    let c = clean(line);
                    if c != "." {
                        append_prose(&mut sub.doc, &c);
                    }
                } else if description.len() < 1600 {
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
    let inherited = (!so_flags.is_empty()).then(|| Inherited {
        command: String::new(), // filled in by the caller, which knows the name
        from_page: so_page,
        flags: so_flags,
    });
    Some((cmd, inherited))
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

/// An `.OP` entry being accumulated. One entry can name several flags, because
/// Tk documents its aliases inline: `-borderwidth or -bd`.
struct PendingOp {
    flags: Vec<String>,
    db_name: String,
    db_class: String,
    doc: String,
}

/// Splits a macro's arguments on whitespace, keeping `"quoted groups"` together.
fn split_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn flush_op(pending: &mut Option<PendingOp>, cmd: &mut Command) {
    let Some(op) = pending.take() else { return };
    let doc = truncate(&op.doc, 300);
    for flag in op.flags {
        if flag.is_empty() || cmd.options.iter().any(|o| o.flag == flag) {
            continue;
        }
        cmd.options.push(OptionSpec {
            flag,
            db_name: op.db_name.clone(),
            db_class: op.db_class.clone(),
            doc: doc.clone(),
            standard: false,
        });
    }
}

fn flush_tp(pending: &mut Option<Subcommand>, cmd: &mut Command) {
    if let Some(mut sub) = pending.take() {
        sub.doc = truncate(&sub.doc, 300);
        // Dedup by identity, not just name: a group form and its nested
        // forms share the dispatch word (`tag option ?arg...?` plus
        // `tag add ...`, `tag bind ...` from the `.RS` list). The forms ARE
        // the documentation for group commands; dropping them left hovers
        // quoting "The following forms of the tag subcommand are
        // supported:" and nothing after it.
        if !cmd
            .subcommands
            .iter()
            .any(|s| s.name == sub.name && s.signature == sub.signature)
        {
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
        let c = parse_page(LSORT, "tcl").expect("a command").0;
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
        let c = parse_page(STRING, "tcl").expect("a command").0;
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
        let c = parse_page(TTK_BUTTON, "tk").expect("a command").0;
        assert_eq!(c.name, "ttk::button");
        let flags: Vec<&str> = c.options.iter().map(|o| o.flag.as_str()).collect();
        assert_eq!(flags, vec!["-command", "-default"]);
        assert_eq!(c.options[0].db_name, "command");
        assert_eq!(c.options[0].db_class, "Command");
        assert!(c.options[0].doc.starts_with("A script to evaluate"));
    }

    const FRAME: &str = r#".TH frame n 8.4 Tk "Tk Built-In Commands"
.SH NAME
frame \- Create and manipulate frame widgets
.SH SYNOPSIS
\fBframe\fR \fIpathName \fR?\fIoptions\fR?
.SO
\-borderwidth	\-cursor	\-takefocus
.SE
.SH "WIDGET-SPECIFIC OPTIONS"
.OP \-background background Background
Background colour of the frame.
"#;

    const OPTIONS: &str = r#".TH options n 4.4 Tk "Tk Built-In Commands"
.SH NAME
options \- Standard options supported by widgets
.SH SYNOPSIS
standard options
.SH DESCRIPTION
.OP "\-background or \-bg" background Background
Specifies the normal background colour.
.OP "\-borderwidth or \-bd" borderWidth BorderWidth
Specifies a non-negative value for the border width.
.OP \-cursor cursor Cursor
Specifies the mouse cursor.
"#;

    /// The `.OP` first field may be quoted and name an alias.
    #[test]
    fn parses_quoted_option_aliases() {
        let c = parse_page(OPTIONS, "tk").expect("a command").0;
        let flags: Vec<&str> = c.options.iter().map(|o| o.flag.as_str()).collect();
        assert!(flags.contains(&"-background"), "got {flags:?}");
        assert!(
            flags.contains(&"-bg"),
            "the alias must be recorded too: {flags:?}"
        );
        assert!(flags.contains(&"-borderwidth") && flags.contains(&"-bd"));
        // Both names describe the same option-database entry.
        let bg = c.options.iter().find(|o| o.flag == "-bg").unwrap();
        assert_eq!(bg.db_name, "background");
    }

    #[test]
    fn records_the_standard_options_a_widget_inherits() {
        let (_, inh) = parse_page(FRAME, "tk").expect("a command");
        let inh = inh.expect("frame has a .SO block");
        assert_eq!(inh.from_page, "options");
        assert_eq!(inh.flags, vec!["-borderwidth", "-cursor", "-takefocus"]);
    }

    #[test]
    fn so_with_a_named_page_records_that_page() {
        let page = ".TH ttk::button n 8.5 Tk\n.SH NAME\nttk::button \\- x\n.SH SYNOPSIS\nx\n.SO ttk_widget\n\\-cursor\t\\-style\n.SE\n";
        let (_, inh) = parse_page(page, "tk").expect("a command");
        assert_eq!(inh.expect("a .SO block").from_page, "ttk_widget");
    }

    /// End to end: a widget must end up accepting the options it inherits *and*
    /// their aliases, which is what makes validating them safe.
    #[test]
    fn inheritance_and_aliases_combine() {
        let (frame, finh) = parse_page(FRAME, "tk").expect("a command");
        let (options, _) = parse_page(OPTIONS, "tk").expect("a command");
        let mut commands = vec![frame, options];
        let mut inh = finh.expect("a .SO block");
        inh.command = "frame".to_string();

        resolve_standard_options(&mut commands, &[inh]);
        apply_option_aliases(&mut commands);

        let frame = commands.iter().find(|c| c.name == "frame").unwrap();
        let flags: Vec<&str> = frame.options.iter().map(|o| o.flag.as_str()).collect();
        for want in [
            "-background",
            "-bg",
            "-borderwidth",
            "-bd",
            "-cursor",
            "-takefocus",
        ] {
            assert!(
                flags.contains(&want),
                "frame should accept {want}: {flags:?}"
            );
        }
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
