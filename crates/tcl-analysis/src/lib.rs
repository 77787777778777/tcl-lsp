//! Workspace-wide index and name resolution.
//!
//! Holds one entry per Tcl file — open in the editor or merely on disk — so that
//! go-to-definition, references and workspace symbols work across a project rather
//! than only across the buffers that happen to be open.
//!
//! Files are keyed by an opaque string (the LSP client's document URI). This crate
//! deliberately knows nothing about LSP types or position encodings; it deals in
//! byte offsets and hands back a [`LineIndex`] so the caller can convert.

#![forbid(unsafe_code)]
#![deny(clippy::print_stdout, clippy::print_stderr)]

pub mod kb;

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use tcl_syntax::{outline, LineIndex, Outline, RefKind, Script, Symbol, SymbolKind, VarDef};

/// A definition, flattened out of the nested outline for fast lookup.
#[derive(Debug, Clone)]
pub struct Def {
    pub kind: SymbolKind,
    pub name: String,
    pub qname: String,
    pub name_range: Range<usize>,
    pub full_range: Range<usize>,
    pub detail: Option<String>,
    pub doc: Option<String>,
}

impl Def {
    /// A one-line signature, e.g. `::util::greet {name}`.
    pub fn signature(&self) -> String {
        match &self.detail {
            Some(args) => format!("{} {{{}}}", self.qname, args),
            None => self.qname.clone(),
        }
    }
}

/// Everything known about one file.
pub struct FileIndex {
    pub line_index: LineIndex,
    pub outline: Outline,
    pub defs: Vec<Def>,
}

/// A location within the workspace, in byte offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loc {
    pub uri: String,
    pub range: Range<usize>,
}

#[derive(Default)]
pub struct Index {
    files: HashMap<String, FileIndex>,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn file(&self, uri: &str) -> Option<&FileIndex> {
        self.files.get(uri)
    }

    pub fn contains(&self, uri: &str) -> bool {
        self.files.contains_key(uri)
    }

    pub fn remove(&mut self, uri: &str) {
        self.files.remove(uri);
    }

    /// Indexes (or re-indexes) one file from its current text.
    pub fn set_file(&mut self, uri: impl Into<String>, text: &str) {
        let script = Script::new(text);
        let outline = outline(&script);
        let mut defs = Vec::new();
        flatten(&outline.symbols, &mut defs);
        self.files.insert(
            uri.into(),
            FileIndex {
                line_index: LineIndex::new(text),
                outline,
                defs,
            },
        );
    }

    /// Resolves a possibly-qualified name to its definitions.
    ///
    /// Follows Tcl's own lookup rules: a name starting with `::` is absolute and
    /// must match exactly; otherwise it is searched for in the current namespace
    /// and then in the global namespace — and, importantly, *not* in the
    /// namespaces in between.
    pub fn resolve(&self, name: &str, namespace: &str) -> Vec<(&str, &Def)> {
        let mut candidates: Vec<String> = Vec::new();
        if name.starts_with("::") {
            candidates.push(name.to_string());
        } else {
            if namespace != "::" && !namespace.is_empty() {
                candidates.push(format!("{namespace}::{name}"));
            }
            candidates.push(format!("::{name}"));
        }

        for cand in &candidates {
            let hits = self.exact(cand);
            if !hits.is_empty() {
                return hits;
            }
        }

        // Nothing matched by qualified name. Fall back to matching the final
        // segment, which is what makes navigation useful when the caller's
        // namespace context is not recoverable from the cursor position alone.
        let tail = name.rsplit("::").next().unwrap_or(name);
        let mut out = Vec::new();
        for (uri, f) in &self.files {
            for d in &f.defs {
                if d.name == tail {
                    out.push((uri.as_str(), d));
                }
            }
        }
        out
    }

    fn exact(&self, qname: &str) -> Vec<(&str, &Def)> {
        let mut out = Vec::new();
        for (uri, f) in &self.files {
            for d in &f.defs {
                if d.qname == qname {
                    out.push((uri.as_str(), d));
                }
            }
        }
        out
    }

    /// Every use site of a name, across the workspace.
    ///
    /// Matching is on the final segment plus, when the reference is qualified, the
    /// full name — so `::util::greet` and a bare `greet` inside `namespace eval
    /// util` both count, without pulling in an unrelated `other::greet`.
    pub fn references(&self, qname: &str) -> Vec<Loc> {
        let tail = qname.rsplit("::").next().unwrap_or(qname);
        let mut out = Vec::new();
        for (uri, f) in &self.files {
            for r in &f.outline.refs {
                if r.kind != RefKind::Command {
                    continue;
                }
                let resolved = tcl_syntax::qualify(&r.namespace, &r.name);
                let matches = if r.name.starts_with("::") {
                    r.name == qname
                } else {
                    resolved == qname || r.name == tail
                };
                if matches {
                    out.push(Loc {
                        uri: uri.clone(),
                        range: r.range.clone(),
                    });
                }
            }
        }
        out
    }

    /// Fuzzy-ish search over qualified names, for `workspace/symbol`.
    pub fn search(&self, query: &str, limit: usize) -> Vec<(&str, &Def)> {
        let q = query.to_lowercase();
        let mut out = Vec::new();
        for (uri, f) in &self.files {
            for d in &f.defs {
                if q.is_empty() || d.qname.to_lowercase().contains(&q) {
                    out.push((uri.as_str(), d));
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }

    /// Names visible for completion at a point in a file: every definition in the
    /// workspace, plus the variables whose scope covers the cursor.
    pub fn completions_at(&self, uri: &str, offset: usize) -> Completions<'_> {
        let vars = self
            .file(uri)
            .map(|f| {
                let mut v: Vec<&VarDef> = f
                    .outline
                    .variables
                    .iter()
                    .filter(|v| {
                        v.scope.contains(&offset) || v.scope.end >= offset && v.scope.start == 0
                    })
                    .collect();
                v.sort_by(|a, b| a.name.cmp(&b.name));
                v.dedup_by(|a, b| a.name == b.name);
                v
            })
            .unwrap_or_default();
        Completions {
            commands: self.files.values().flat_map(|f| f.defs.iter()).collect(),
            variables: vars,
        }
    }

    /// Indexes every `.tcl` file under `root`. Returns how many were added.
    pub fn scan(&mut self, root: &Path) -> usize {
        let mut files = Vec::new();
        collect_tcl_files(root, &mut files, 0);
        let mut n = 0;
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(uri) = path_to_uri(&path) else {
                continue;
            };
            self.set_file(uri, &text);
            n += 1;
        }
        n
    }
}

/// Completion candidates in scope at a point.
pub struct Completions<'a> {
    pub commands: Vec<&'a Def>,
    pub variables: Vec<&'a VarDef>,
}

fn flatten(symbols: &[Symbol], out: &mut Vec<Def>) {
    for s in symbols {
        out.push(Def {
            kind: s.kind,
            name: s.name.clone(),
            qname: s.qname.clone(),
            name_range: s.name_range.clone(),
            full_range: s.full_range.clone(),
            detail: s.detail.clone(),
            doc: s.doc.clone(),
        });
        flatten(&s.children, out);
    }
}

/// Directories that never contain project sources worth indexing.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".direnv", "result"];

fn collect_tcl_files(root: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    // A workspace scan must not follow a pathological tree forever.
    if depth > 24 || out.len() > 20_000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name.starts_with('.') && name != "." || SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            collect_tcl_files(&path, out, depth + 1);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("tcl" | "tm" | "test" | "itcl")
        ) {
            out.push(path);
        }
    }
}

/// Builds a `file://` URI the same way LSP clients do.
pub fn path_to_uri(path: &Path) -> Option<String> {
    let s = path.to_str()?;
    let mut out = String::from("file://");
    for b in s.bytes() {
        match b {
            b'/' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b if b.is_ascii_alphanumeric() => out.push(b as char),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    Some(out)
}

/// Inverse of [`path_to_uri`], tolerant of the percent-encoding clients use.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let mut it = rest.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hi = it.next()?;
            let lo = it.next()?;
            let hex = |c: u8| char::from(c).to_digit(16).map(|d| d as u8);
            bytes.push(hex(hi)? * 16 + hex(lo)?);
        } else {
            bytes.push(b);
        }
    }
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(files: &[(&str, &str)]) -> Index {
        let mut idx = Index::new();
        for (uri, text) in files {
            idx.set_file(*uri, text);
        }
        idx
    }

    #[test]
    fn resolves_across_files() {
        let idx = index(&[
            (
                "file:///a.tcl",
                "namespace eval util { proc greet {n} {} }\n",
            ),
            ("file:///b.tcl", "util::greet hi\n"),
        ]);
        let hits = idx.resolve("util::greet", "::");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "file:///a.tcl");
        assert_eq!(hits[0].1.qname, "::util::greet");
    }

    #[test]
    fn absolute_names_do_not_match_a_different_namespace() {
        let idx = index(&[(
            "file:///a.tcl",
            "namespace eval a { proc f {} {} }\nnamespace eval b { proc f {} {} }\n",
        )]);
        let hits = idx.resolve("::a::f", "::");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1.qname, "::a::f");
    }

    /// Tcl searches the current namespace, then global — never the levels between.
    #[test]
    fn relative_lookup_prefers_the_current_namespace() {
        let idx = index(&[(
            "file:///a.tcl",
            "proc f {} {}\nnamespace eval a { proc f {} {} }\n",
        )]);
        let in_a = idx.resolve("f", "::a");
        assert_eq!(in_a.len(), 1);
        assert_eq!(in_a[0].1.qname, "::a::f");

        let at_global = idx.resolve("f", "::");
        assert_eq!(at_global.len(), 1);
        assert_eq!(at_global[0].1.qname, "::f");
    }

    #[test]
    fn finds_references_across_files() {
        let idx = index(&[
            ("file:///a.tcl", "proc greet {} {}\ngreet\n"),
            ("file:///b.tcl", "greet\ngreet\n"),
        ]);
        let refs = idx.references("::greet");
        assert_eq!(refs.len(), 3, "one call in a.tcl, two in b.tcl");
    }

    #[test]
    fn search_matches_qualified_names() {
        let idx = index(&[("file:///a.tcl", "namespace eval util { proc trim {} {} }\n")]);
        assert_eq!(idx.search("trim", 10).len(), 1);
        assert_eq!(idx.search("util", 10).len(), 2, "namespace and proc");
        assert!(idx.search("nope", 10).is_empty());
    }

    #[test]
    fn removing_a_file_drops_its_symbols() {
        let mut idx = index(&[("file:///a.tcl", "proc gone {} {}\n")]);
        assert_eq!(idx.resolve("gone", "::").len(), 1);
        idx.remove("file:///a.tcl");
        assert!(idx.resolve("gone", "::").is_empty());
    }

    #[test]
    fn reindexing_replaces_old_symbols() {
        let mut idx = index(&[("file:///a.tcl", "proc old {} {}\n")]);
        idx.set_file("file:///a.tcl", "proc new {} {}\n");
        assert!(idx.resolve("old", "::").is_empty());
        assert_eq!(idx.resolve("new", "::").len(), 1);
    }

    #[test]
    fn completions_include_workspace_procs_and_scoped_vars() {
        let src = "proc helper {} {}\nproc f {alpha} {\n  set beta 1\n  \n}\nset gamma 2\n";
        let mut idx = Index::new();
        idx.set_file("file:///a.tcl", src);
        // Offset inside f's body, on the blank line.
        let at = src.find("  \n}").unwrap() + 2;
        let c = idx.completions_at("file:///a.tcl", at);
        let cmds: Vec<_> = c.commands.iter().map(|d| d.name.as_str()).collect();
        assert!(cmds.contains(&"helper"));
        let vars: Vec<_> = c.variables.iter().map(|v| v.name.as_str()).collect();
        assert!(vars.contains(&"alpha"), "parameter in scope: {vars:?}");
        assert!(vars.contains(&"beta"), "local in scope: {vars:?}");
    }

    #[test]
    fn uri_roundtrip_handles_spaces() {
        let p = Path::new("/tmp/some dir/a.tcl");
        let uri = path_to_uri(p).unwrap();
        assert!(uri.contains("%20"), "space must be encoded: {uri}");
        assert_eq!(uri_to_path(&uri).unwrap(), p);
    }

    #[test]
    fn signature_renders_arguments() {
        let idx = index(&[("file:///a.tcl", "proc greet {name greeting} {}\n")]);
        let hits = idx.resolve("greet", "::");
        assert_eq!(hits[0].1.signature(), "::greet {name greeting}");
    }
}
