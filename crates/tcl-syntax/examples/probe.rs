//! Debug helper. Prints where parsing or outlining of a file goes wrong.
//!
//! `cargo run -p tcl-syntax --example probe -- FILE_OR_DIR`
//!
//! Given a directory it walks every `.tcl` file, printing each name *before*
//! processing it, so a crash identifies its own culprit.

use std::path::{Path, PathBuf};

fn main() {
    let arg = std::env::args().nth(1).expect("usage: probe FILE_OR_DIR");
    let path = PathBuf::from(arg);
    if path.is_dir() {
        let mut files = Vec::new();
        collect(&path, &mut files);
        for f in files {
            println!("{}", f.display());
            let _ = one(&f);
        }
    } else {
        one(&path);
    }
}

fn collect(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "tcl") {
            out.push(p);
        }
    }
}

fn one(path: &Path) -> Option<()> {
    let text = std::fs::read_to_string(path).ok()?;
    let script = tcl_syntax::Script::new(&text);

    let mut n = 0usize;
    for r in script.commands() {
        match r {
            Ok(_) => n += 1,
            Err(e) => {
                let at = e.offset.min(text.len());
                let start = text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let end = text[at..].find('\n').map(|i| at + i).unwrap_or(text.len());
                let line = text[..at].matches('\n').count() + 1;
                println!(
                    "  parse error at byte {at} (line {line}), incomplete={}",
                    e.incomplete
                );
                println!("  >>> {:?}", &text[start..end.min(start + 200)]);
                return Some(());
            }
        }
    }
    let o = tcl_syntax::outline(&script);
    for e in &o.errors {
        let at = e.range.start.min(text.len());
        let start = text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let end = text[at..].find('\n').map(|i| at + i).unwrap_or(text.len());
        let line = text[..at].matches('\n').count() + 1;
        println!(
            "  OUTLINE error at byte {at} (line {line}) incomplete={}: {}",
            e.incomplete, e.message
        );
        println!("  >>> {:?}", &text[start..end.min(start + 220)]);
    }
    println!(
        "  ok: {n} commands, {} symbols, {} refs, {} vars",
        o.symbols.len(),
        o.refs.len(),
        o.variables.len()
    );
    Some(())
}
