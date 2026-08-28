//! Structural outline of a Tcl file: definitions, references and variables.
//!
//! Built directly on Tcl's own parser, so brace nesting and quoting are handled
//! exactly. Bodies are re-parsed in place — Tcl reports token offsets absolute to
//! the whole buffer, including the interior of a braced word, so descending into a
//! `proc` or `namespace eval` body needs no offset arithmetic. Nested parses are
//! bounded by the body's end; without that the parser runs past the closing brace.

use std::ops::Range;
use tcl_tclsys::{Command, Script, Token, TokenKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Proc,
    Namespace,
    Class,
    Method,
    Constructor,
    Destructor,
    Variable,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub kind: SymbolKind,
    /// The name exactly as written.
    pub name: String,
    /// Fully qualified name, always absolute (`::a::b::c`).
    pub qname: String,
    /// Range of the name token, for goto-definition and rename.
    pub name_range: Range<usize>,
    /// Range of the whole definition, for folding and documentSymbol.
    pub full_range: Range<usize>,
    /// Signature detail, e.g. a proc's argument list.
    pub detail: Option<String>,
    /// For a TclOO class: the names given to `superclass` and `mixin`, as written.
    pub supers: Vec<String>,
    /// Leading `#` comment block, if any — free documentation for hover.
    pub doc: Option<String>,
    pub children: Vec<Symbol>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// A word in command position.
    Command,
    /// A `$name` substitution.
    Variable,
}

/// A use site, as opposed to a definition.
#[derive(Debug, Clone)]
pub struct Ref {
    pub kind: RefKind,
    /// The name as written; may be namespace-qualified.
    pub name: String,
    pub range: Range<usize>,
    /// Namespace in force at this point, for resolving a relative name.
    pub namespace: String,
}

/// A variable binding introduced by `set`, `variable`, `global`, `foreach`, …
#[derive(Debug, Clone)]
pub struct VarDef {
    pub name: String,
    pub range: Range<usize>,
    /// The region in which this binding is visible — a proc body, or the whole
    /// file for globals. Completion uses this to offer only what is in scope.
    pub scope: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// `source path` — a file pulled into this one.
    Source,
    /// `package require name` — satisfied by whichever file provides it.
    PackageRequire,
}

/// A navigable reference to another file.
#[derive(Debug, Clone)]
pub struct LinkRef {
    pub kind: LinkKind,
    /// The literal path or package name. Only static literals are recorded; a
    /// computed `source [file join $dir x.tcl]` is deliberately skipped.
    pub name: String,
    pub range: Range<usize>,
}

/// A call site with its argument positions, for parameter-name inlay hints.
///
/// Only recorded where the walker knows it is in genuine script context, so an
/// expression like `if {$a ne {}}` never contributes phantom calls.
#[derive(Debug, Clone)]
pub struct Call {
    pub name: String,
    pub name_range: Range<usize>,
    /// Byte range of each argument word, excluding the command name itself.
    pub args: Vec<Range<usize>>,
    pub namespace: String,
}

/// A `package provide NAME` declaration, which is what a `require` resolves to.
#[derive(Debug, Clone)]
pub struct Provide {
    pub name: String,
    pub range: Range<usize>,
}

/// A syntax error, reported at a byte range.
#[derive(Debug, Clone)]
pub struct SyntaxError {
    pub range: Range<usize>,
    pub message: String,
    /// True when the input merely stops mid-construct, which is the normal state
    /// of a buffer being typed into. Callers should not surface these as errors
    /// at the end of the document.
    pub incomplete: bool,
}

/// Everything we learn from one file.
#[derive(Debug, Clone, Default)]
pub struct Outline {
    pub symbols: Vec<Symbol>,
    pub refs: Vec<Ref>,
    pub variables: Vec<VarDef>,
    pub links: Vec<LinkRef>,
    pub provides: Vec<Provide>,
    /// Comment blocks, for semantic highlighting.
    pub comments: Vec<Range<usize>>,
    pub calls: Vec<Call>,
    pub errors: Vec<SyntaxError>,
}

/// Joins a namespace prefix and a possibly-qualified name into an absolute qname.
///
/// Implements Tcl's rule that a name beginning with `::` is absolute and ignores
/// the enclosing namespace entirely. This is what the reference prototype lacked:
/// it compared bare words with `string wordstart`, so `::a::b::c` could never
/// resolve.
pub fn qualify(prefix: &str, name: &str) -> String {
    if let Some(stripped) = name.strip_prefix("::") {
        format!("::{stripped}")
    } else if prefix == "::" || prefix.is_empty() {
        format!("::{name}")
    } else {
        format!("{prefix}::{name}")
    }
}

/// The namespace a definition of `name` lands in, given the enclosing namespace.
fn container_of(prefix: &str, name: &str) -> String {
    let q = qualify(prefix, name);
    match q.rfind("::") {
        Some(0) | None => "::".to_string(),
        Some(i) => q[..i].to_string(),
    }
}

fn last_segment(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Reads a word as a literal string, if it is one.
///
/// Returns `None` when the word involves substitution (`$x`, `[f]`), which is the
/// single most important gate in the whole analyzer: a name that is not a static
/// literal cannot be resolved, and must never be guessed at.
pub(crate) fn literal(script: &Script, word: &[Token]) -> Option<String> {
    let head = word.first()?;
    match head.kind {
        TokenKind::SimpleWord => {
            let inner = word.get(1)?;
            script.text(inner.range()).map(str::to_owned)
        }
        TokenKind::Text => script.text(head.range()).map(str::to_owned),
        TokenKind::Word => {
            // A compound word is literal only if every component is text or an escape.
            let mut out = String::new();
            for t in &word[1..] {
                match t.kind {
                    TokenKind::Text | TokenKind::Backslash => out.push_str(script.text(t.range())?),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// The byte range of a word's *contents*, i.e. inside any braces or quotes.
///
/// Braced bodies come back from Tcl as ONE head token (Word/SimpleWord/
/// ExpandWord) whose sub-token stream is whatever Tcl needed to express its
/// contents. When the body contains backslash-processed sequences — a
/// `\`-newline continuation, an escaped brace — those surface as Backslash
/// children SPLITTING the Text, so "the first sub-token's range" is only the
/// head chunk. That truncated range made every braced body containing a line
/// continuation re-parse as "unterminated", costing a false warning per
/// body. The interior is the head token minus its outer braces: read the
/// bytes instead of trusting child-token boundaries. (Empty bodies yield an
/// empty range.)
pub(crate) fn body_range(script: &Script, word: &[Token]) -> Option<Range<usize>> {
    let head = word.first()?;
    match head.kind {
        TokenKind::SimpleWord | TokenKind::Word | TokenKind::ExpandWord => {
            let r = head.range();
            // A braced word starts with `{` (`{*` for expansion); anything else
            // (a quoted word can't appear here, but do not guess) keeps the
            // exact pre-fix behaviour.
            if script.as_bytes().get(r.start) != Some(&b'{') {
                return word.get(1).map(|t| t.range());
            }
            let start = r.start + 1;
            let end = r.end.checked_sub(1)?;
            Some(start..end)
        }
        TokenKind::Text => Some(head.range()),
        _ => None,
    }
}

/// The range of a word's first token, used to anchor names.
pub(crate) fn word_range(word: &[Token]) -> Option<Range<usize>> {
    word.first().map(|t| t.range())
}

fn doc_of(script: &Script, cmd: &Command) -> Option<String> {
    let range = cmd.comment.clone()?;
    let raw = script.text(range)?;
    let mut out = String::new();
    for line in raw.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix('#') else {
            continue;
        };
        out.push_str(rest.trim_start());
        out.push('\n');
    }
    let out = out.trim_end().to_string();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// What kind of script we are looking at; the same word means different things
/// inside an `oo::class` body than at script level.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Script,
    ClassBody(Dialect),
}

/// Which object system a class body is written in. They share member keywords
/// but not their argument shapes: TclOO `variable a b c` names three variables,
/// while itcl `variable name ?init?` names one and gives it a value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    TclOo,
    Itcl,
    Snit,
}

/// Builds the outline for a whole file.
pub fn outline(script: &Script) -> Outline {
    let mut w = Walker {
        script,
        refs: Vec::new(),
        variables: Vec::new(),
        links: Vec::new(),
        provides: Vec::new(),
        comments: Vec::new(),
        calls: Vec::new(),
        errors: Vec::new(),
        depth: 0,
        pending_supers: Vec::new(),
    };
    let mut symbols = Vec::new();
    let whole = 0..script.len();
    w.walk(whole.clone(), "::", Mode::Script, whole, &mut symbols);
    Outline {
        symbols,
        refs: w.refs,
        variables: w.variables,
        links: w.links,
        provides: w.provides,
        comments: w.comments,
        calls: w.calls,
        errors: w.errors,
    }
}

struct Walker<'a> {
    script: &'a Script,
    refs: Vec<Ref>,
    variables: Vec<VarDef>,
    links: Vec<LinkRef>,
    provides: Vec<Provide>,
    comments: Vec<Range<usize>>,
    calls: Vec<Call>,
    errors: Vec<SyntaxError>,
    depth: usize,
    /// `superclass`/`mixin` names seen while walking the current class body.
    pending_supers: Vec<String>,
}

/// Hard ceiling on nesting. Real Tcl never approaches this; it exists so that a
/// command shape added later cannot turn a bad recursion into a stack overflow
/// that takes the whole server down.
const MAX_DEPTH: usize = 100;

impl Walker<'_> {
    fn walk(
        &mut self,
        range: Range<usize>,
        ns: &str,
        mode: Mode,
        scope: Range<usize>,
        into: &mut Vec<Symbol>,
    ) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.depth += 1;
        self.walk_inner(range, ns, mode, scope, into);
        self.depth -= 1;
    }

    fn walk_inner(
        &mut self,
        range: Range<usize>,
        ns: &str,
        mode: Mode,
        scope: Range<usize>,
        into: &mut Vec<Symbol>,
    ) {
        for result in self.script.commands_in(range) {
            let cmd = match result {
                Ok(c) => c,
                Err(e) => {
                    let at = e.offset.min(self.script.len());
                    self.errors.push(SyntaxError {
                        range: at..(at + 1).min(self.script.len()).max(at),
                        message: if e.incomplete {
                            "unterminated command: missing a closing brace, bracket or quote".into()
                        } else {
                            "syntax error".into()
                        },
                        incomplete: e.incomplete,
                    });
                    break;
                }
            };

            if let Some(c) = cmd.comment.clone() {
                if !self.comments.contains(&c) {
                    self.comments.push(c);
                }
            }
            self.collect_variable_refs(&cmd, ns);

            // Descend into `[...]` substitutions. Unlike a braced word — where only
            // the command's shape says whether it holds a script or an expression —
            // a COMMAND token is unambiguously a script, because Tcl's own parser
            // classified it as one. Without this, everything inside `[expr ...]` or
            // `[myproc $x]` is invisible to references, hints and highlighting.
            for t in &cmd.tokens {
                if t.kind == TokenKind::Command && t.size >= 2 {
                    let inner = t.start + 1..t.start + t.size - 1;
                    if inner.end > inner.start {
                        let mut kids = Vec::new();
                        self.walk(inner, ns, Mode::Script, scope.clone(), &mut kids);
                        into.append(&mut kids);
                    }
                }
            }

            let words = cmd.words();
            let Some(head) = words.first().and_then(|w| literal(self.script, w)) else {
                continue; // dynamically-named command; nothing statically knowable
            };

            // Record the command itself as a reference, so find-references and
            // call hierarchy have use sites to work from.
            if let Some(r) = word_range(words[0]) {
                self.refs.push(Ref {
                    kind: RefKind::Command,
                    name: head.clone(),
                    range: r.clone(),
                    namespace: ns.to_string(),
                });
                self.calls.push(Call {
                    name: head.clone(),
                    name_range: r,
                    args: words.iter().skip(1).filter_map(|w| word_range(w)).collect(),
                    namespace: ns.to_string(),
                });
            }

            // itcl writes access control in front of the member it applies to
            // (`public method m {} {}`), so step past it and treat what follows as
            // the real command.
            let bare = head.trim_start_matches("::").to_string();
            let (head, words) = if matches!(mode, Mode::ClassBody(_))
                && matches!(bare.as_str(), "public" | "protected" | "private")
                && words.len() > 2
            {
                match words.get(1).and_then(|w| literal(self.script, w)) {
                    Some(inner) => (
                        inner.trim_start_matches("::").to_string(),
                        words[1..].to_vec(),
                    ),
                    None => (bare, words),
                }
            } else {
                (bare, words)
            };
            let head = head.as_str();

            match (mode, head) {
                (Mode::Script, "proc") => self.proc_def(&cmd, &words, ns, into),
                (Mode::Script, "namespace") => self.namespace_def(&cmd, &words, ns, into),
                // `oo::class create Name {body}` — name and body are words 2 and 3.
                (Mode::Script, "oo::class") => self.class_def(&cmd, &words, ns, into),
                // itcl and snit take the name directly: `itcl::class Name {body}`.
                (
                    Mode::Script,
                    "itcl::class" | "snit::type" | "snit::widget" | "snit::widgetadaptor",
                ) => {
                    let dialect = if head.starts_with("itcl") {
                        Dialect::Itcl
                    } else {
                        Dialect::Snit
                    };
                    self.megawidget_def(&cmd, &words, ns, dialect, into)
                }
                (Mode::ClassBody(_), "method") => {
                    if let Some(mut sym) = self.simple_def(&cmd, &words, ns, SymbolKind::Method) {
                        sym.detail = words.get(2).and_then(|w| literal(self.script, w));
                        if let Some(body) = words.get(3).and_then(|w| body_range(self.script, w)) {
                            self.bind_params(&words, 2, body.clone());
                            let mut kids = Vec::new();
                            self.walk(body.clone(), ns, Mode::Script, body, &mut kids);
                            sym.children = kids;
                        }
                        into.push(sym);
                    }
                }
                (Mode::ClassBody(_), "constructor") => {
                    let mut sym = self.anon_def(&cmd, ns, "constructor", SymbolKind::Constructor);
                    if let Some(body) = words.get(2).and_then(|w| body_range(self.script, w)) {
                        self.bind_params(&words, 1, body.clone());
                        self.walk(body.clone(), ns, Mode::Script, body, &mut sym.children);
                    }
                    into.push(sym);
                }
                (Mode::ClassBody(_), "destructor") => {
                    let mut sym = self.anon_def(&cmd, ns, "destructor", SymbolKind::Destructor);
                    if let Some(body) = words.get(1).and_then(|w| body_range(self.script, w)) {
                        self.walk(body.clone(), ns, Mode::Script, body, &mut sym.children);
                    }
                    into.push(sym);
                }
                // `superclass A B` / `mixin M` inside a class body. Recorded on the
                // enclosing class, which the caller patches in after the walk.
                // `typemethod` (snit) and a class-level `proc` (itcl) are members
                // just like `method`, differing only in what they are bound to.
                (Mode::ClassBody(_), "typemethod" | "proc") => {
                    if let Some(mut sym) = self.simple_def(&cmd, &words, ns, SymbolKind::Method) {
                        sym.detail = words.get(2).and_then(|w| literal(self.script, w));
                        if let Some(body) = words.get(3).and_then(|w| body_range(self.script, w)) {
                            self.bind_params(&words, 2, body.clone());
                            let mut kids = Vec::new();
                            self.walk(body.clone(), ns, Mode::Script, body, &mut kids);
                            sym.children = kids;
                        }
                        into.push(sym);
                    }
                }
                // snit's `option -name ?default?`, plus the several flavours of
                // class-level storage in itcl and snit.
                (Mode::ClassBody(_), "typevariable" | "common" | "component" | "option") => {
                    if let (Some(name), Some(r)) = (
                        words.get(1).and_then(|w| literal(self.script, w)),
                        words.get(1).and_then(|w| word_range(w)),
                    ) {
                        into.push(Symbol {
                            kind: SymbolKind::Variable,
                            qname: qualify(ns, &name),
                            name,
                            name_range: r,
                            full_range: cmd.range.clone(),
                            detail: None,
                            supers: Vec::new(),
                            doc: None,
                            children: Vec::new(),
                        });
                    }
                }
                (Mode::ClassBody(_), "superclass" | "mixin" | "inherit") => {
                    for w in words.iter().skip(1) {
                        if let Some(name) = literal(self.script, w) {
                            if !name.starts_with('-') {
                                self.pending_supers.push(name);
                            }
                        }
                    }
                }
                // TclOO's `variable a b c` declares three; itcl's and snit's
                // `variable name ?value?` declares one and initialises it.
                (Mode::ClassBody(Dialect::Itcl | Dialect::Snit), "variable") => {
                    if let (Some(name), Some(r)) = (
                        words.get(1).and_then(|w| literal(self.script, w)),
                        words.get(1).and_then(|w| word_range(w)),
                    ) {
                        into.push(Symbol {
                            kind: SymbolKind::Variable,
                            qname: qualify(ns, &name),
                            name,
                            name_range: r,
                            full_range: cmd.range.clone(),
                            detail: None,
                            supers: Vec::new(),
                            doc: None,
                            children: Vec::new(),
                        });
                    }
                }
                (Mode::ClassBody(_), "variable") => {
                    for w in words.iter().skip(1) {
                        if let (Some(name), Some(r)) = (literal(self.script, w), word_range(w)) {
                            into.push(Symbol {
                                kind: SymbolKind::Variable,
                                qname: qualify(ns, &name),
                                name,
                                name_range: r,
                                full_range: cmd.range.clone(),
                                detail: None,
                                supers: Vec::new(),
                                doc: None,
                                children: Vec::new(),
                            });
                        }
                    }
                }
                // Variable-binding commands. Recording these is what makes variable
                // completion possible; the scope decides where they are offered.
                (Mode::Script, "set" | "variable" | "global" | "incr" | "append" | "lappend") => {
                    if let (Some(name), Some(r)) = (
                        words.get(1).and_then(|w| literal(self.script, w)),
                        words.get(1).and_then(|w| word_range(w)),
                    ) {
                        self.bind(name, r, scope.clone());
                    }
                }
                (Mode::Script, "foreach" | "lmap") => {
                    // `foreach {a b} $list {...}` — the first word is a name list.
                    if let Some(list) = words.get(1).and_then(|w| literal(self.script, w)) {
                        let base = words.get(1).and_then(|w| word_range(w));
                        for name in list.split_whitespace() {
                            let r = base.clone().unwrap_or(cmd.range.clone());
                            self.bind(name.to_string(), r, scope.clone());
                        }
                    }
                    // The body is the last *braced* word. Requiring braces matters:
                    // without it a malformed one-word `foreach` makes word 0 its own
                    // body, and the recursion never shrinks.
                    if let Some(body) = words
                        .iter()
                        .skip(2)
                        .rfind(|w| is_braced(self.script, w))
                        .and_then(|w| body_range(self.script, w))
                    {
                        let mut kids = Vec::new();
                        self.walk(body, ns, Mode::Script, scope.clone(), &mut kids);
                        into.append(&mut kids);
                    }
                }
                // `source ?-encoding enc? filename`
                (Mode::Script, "source") => {
                    if let Some(w) = words.last() {
                        if let (Some(name), Some(r)) = (literal(self.script, w), word_range(w)) {
                            if words.len() > 1 && !name.starts_with('-') {
                                self.links.push(LinkRef {
                                    kind: LinkKind::Source,
                                    name,
                                    range: r,
                                });
                            }
                        }
                    }
                }
                (Mode::Script, "package") => {
                    let sub = words.get(1).and_then(|w| literal(self.script, w));
                    let arg = words.get(2);
                    if let (Some(sub), Some(w)) = (sub, arg) {
                        if let (Some(name), Some(r)) = (literal(self.script, w), word_range(w)) {
                            match sub.as_str() {
                                "require" => self.links.push(LinkRef {
                                    kind: LinkKind::PackageRequire,
                                    name,
                                    range: r,
                                }),
                                "provide" => self.provides.push(Provide { name, range: r }),
                                _ => {}
                            }
                        }
                    }
                }
                (Mode::Script, "lassign") => {
                    for w in words.iter().skip(2) {
                        if let (Some(name), Some(r)) = (literal(self.script, w), word_range(w)) {
                            self.bind(name, r, scope.clone());
                        }
                    }
                }
                // Control flow. Only genuine *script* arguments are descended into.
                // Treating every braced word as a script is wrong: `if {$a ne {}}`
                // is an expression, and parsing it as a script is a syntax error.
                (Mode::Script, _) => {
                    for i in script_args(head, &words, self.script) {
                        let Some(w) = words.get(i) else { continue };
                        if !is_braced(self.script, w) {
                            continue;
                        }
                        if let Some(body) = body_range(self.script, w) {
                            let mut kids = Vec::new();
                            self.walk(body, ns, Mode::Script, scope.clone(), &mut kids);
                            into.append(&mut kids);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn bind(&mut self, name: String, range: Range<usize>, scope: Range<usize>) {
        if name.is_empty() || name.contains('$') {
            return;
        }
        self.variables.push(VarDef { name, range, scope });
    }

    /// Records a proc's or method's formal parameters as bindings in its body.
    fn bind_params(&mut self, words: &[&[Token]], arg_index: usize, body: Range<usize>) {
        let Some(args) = words.get(arg_index).and_then(|w| literal(self.script, w)) else {
            return;
        };
        let range = words
            .get(arg_index)
            .and_then(|w| word_range(w))
            .unwrap_or(body.clone());
        // Each element may be a bare name or `{name default}`.
        for part in split_args(&args) {
            self.bind(part, range.clone(), body.clone());
        }
    }

    fn collect_variable_refs(&mut self, cmd: &Command, ns: &str) {
        let toks = &cmd.tokens;
        for (i, t) in toks.iter().enumerate() {
            if t.kind != TokenKind::Variable {
                continue;
            }
            // A VARIABLE token is followed by its name as a TEXT component.
            if let Some(nt) = toks.get(i + 1) {
                if nt.kind == TokenKind::Text {
                    if let Some(name) = self.script.text(nt.range()) {
                        self.refs.push(Ref {
                            kind: RefKind::Variable,
                            name: name.to_string(),
                            range: nt.range(),
                            namespace: ns.to_string(),
                        });
                    }
                }
            }
        }
    }

    fn proc_def(&mut self, cmd: &Command, words: &[&[Token]], ns: &str, into: &mut Vec<Symbol>) {
        let Some(mut sym) = self.simple_def(cmd, words, ns, SymbolKind::Proc) else {
            return;
        };
        sym.detail = words.get(2).and_then(|w| literal(self.script, w));
        if let Some(body) = words.get(3).and_then(|w| body_range(self.script, w)) {
            // `proc a::b::c` defines into `::a::b`, so the body's enclosing namespace
            // is the name's qualifier, not the surrounding scope.
            let proc_name = literal(self.script, words[1]).unwrap_or_default();
            let inner = container_of(ns, &proc_name);
            self.bind_params(words, 2, body.clone());
            let mut kids = Vec::new();
            self.walk(body.clone(), &inner, Mode::Script, body, &mut kids);
            sym.children = kids;
        }
        into.push(sym);
    }

    fn namespace_def(
        &mut self,
        cmd: &Command,
        words: &[&[Token]],
        ns: &str,
        into: &mut Vec<Symbol>,
    ) {
        // Only `namespace eval` introduces a scope; `export`/`import`/etc. do not.
        if words
            .get(1)
            .and_then(|w| literal(self.script, w))
            .as_deref()
            != Some("eval")
        {
            return;
        }
        let Some(name) = words.get(2).and_then(|w| literal(self.script, w)) else {
            return;
        };
        let qname = qualify(ns, &name);
        let mut sym = Symbol {
            kind: SymbolKind::Namespace,
            name: last_segment(&name).to_string(),
            qname: qname.clone(),
            name_range: words
                .get(2)
                .and_then(|w| word_range(w))
                .unwrap_or(cmd.range.clone()),
            full_range: cmd.range.clone(),
            detail: None,
            supers: Vec::new(),
            doc: doc_of(self.script, cmd),
            children: Vec::new(),
        };
        if let Some(body) = words.get(3).and_then(|w| body_range(self.script, w)) {
            let mut kids = Vec::new();
            self.walk(body.clone(), &qname, Mode::Script, body, &mut kids);
            sym.children = kids;
        }
        into.push(sym);
    }

    fn class_def(&mut self, cmd: &Command, words: &[&[Token]], ns: &str, into: &mut Vec<Symbol>) {
        if words
            .get(1)
            .and_then(|w| literal(self.script, w))
            .as_deref()
            != Some("create")
        {
            return;
        }
        self.class_like(cmd, words, ns, 2, 3, Dialect::TclOo, into);
    }

    /// `itcl::class Name {body}` / `snit::type Name {body}` — the name is word 1
    /// and the body word 2, where TclOO puts them at 2 and 3 after `create`.
    fn megawidget_def(
        &mut self,
        cmd: &Command,
        words: &[&[Token]],
        ns: &str,
        dialect: Dialect,
        into: &mut Vec<Symbol>,
    ) {
        self.class_like(cmd, words, ns, 1, 2, dialect, into);
    }

    /// Shared body for every class-like definition, differing only in which words
    /// hold the name and the body.
    fn class_like(
        &mut self,
        cmd: &Command,
        words: &[&[Token]],
        ns: &str,
        name_at: usize,
        body_at: usize,
        dialect: Dialect,
        into: &mut Vec<Symbol>,
    ) {
        let Some(name) = words.get(name_at).and_then(|w| literal(self.script, w)) else {
            return;
        };
        let qname = qualify(ns, &name);
        let mut sym = Symbol {
            kind: SymbolKind::Class,
            name: last_segment(&name).to_string(),
            qname: qname.clone(),
            name_range: words
                .get(name_at)
                .and_then(|w| word_range(w))
                .unwrap_or(cmd.range.clone()),
            full_range: cmd.range.clone(),
            detail: None,
            supers: Vec::new(),
            doc: doc_of(self.script, cmd),
            children: Vec::new(),
        };
        if let Some(body) = words.get(body_at).and_then(|w| body_range(self.script, w)) {
            let mut kids = Vec::new();
            let outer_supers = std::mem::take(&mut self.pending_supers);
            self.walk(
                body.clone(),
                &qname,
                Mode::ClassBody(dialect),
                body,
                &mut kids,
            );
            sym.children = kids;
            // Collected by the ClassBody arm while walking, then handed back so a
            // nested class does not steal its parent superclass list.
            sym.supers = std::mem::replace(&mut self.pending_supers, outer_supers);
        }
        into.push(sym);
    }

    /// A `name args body` style definition: `proc` and `method`.
    fn simple_def(
        &self,
        cmd: &Command,
        words: &[&[Token]],
        ns: &str,
        kind: SymbolKind,
    ) -> Option<Symbol> {
        let name = literal(self.script, words.get(1)?)?;
        Some(Symbol {
            kind,
            qname: qualify(ns, &name),
            name: last_segment(&name).to_string(),
            name_range: word_range(words[1])?,
            full_range: cmd.range.clone(),
            detail: None,
            supers: Vec::new(),
            doc: doc_of(self.script, cmd),
            children: Vec::new(),
        })
    }

    /// A definition with no name of its own: `constructor` / `destructor`.
    fn anon_def(&self, cmd: &Command, ns: &str, name: &str, kind: SymbolKind) -> Symbol {
        let head_range = cmd
            .tokens
            .first()
            .map(|t| t.range())
            .unwrap_or(cmd.range.clone());
        Symbol {
            kind,
            name: name.to_string(),
            qname: qualify(ns, name),
            name_range: head_range,
            full_range: cmd.range.clone(),
            detail: None,
            supers: Vec::new(),
            doc: doc_of(self.script, cmd),
            children: Vec::new(),
        }
    }
}

/// Which argument positions of `head` hold a script body.
///
/// This is deliberately a whitelist. Tcl gives no syntactic way to tell a script
/// from an expression or a value — `while {$i < 5} {incr i}` has two braced words
/// and only the second is code — so the distinction has to be modelled per command.
/// Anything not listed here is simply not descended into, which costs a few missed
/// nested symbols but never invents a syntax error.
fn script_args(head: &str, words: &[&[Token]], script: &Script) -> Vec<usize> {
    let n = words.len();
    match head {
        // `if cond body ?elseif cond body?... ?else body?`
        "if" => {
            let mut out = vec![2];
            let mut i = 3;
            while i < n {
                match literal(script, words[i]).as_deref() {
                    Some("elseif") => {
                        out.push(i + 2);
                        i += 3;
                    }
                    Some("then") => i += 1,
                    Some("else") => {
                        out.push(i + 1);
                        i += 2;
                    }
                    // An `if` written without the `else` keyword.
                    _ => {
                        out.push(i);
                        i += 1;
                    }
                }
            }
            out
        }
        "while" => vec![2],
        // `for start test next body` — everything but the test is code.
        "for" => vec![1, 3, 4],
        "catch" => vec![1],
        // `try body ?on code var body?... ?trap pat var body?... ?finally body?`
        "try" => {
            let mut out = vec![1];
            let mut i = 2;
            while i < n {
                match literal(script, words[i]).as_deref() {
                    Some("on") | Some("trap") => {
                        out.push(i + 3);
                        i += 4;
                    }
                    Some("finally") => {
                        out.push(i + 1);
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            out
        }
        // `uplevel ?level? script` / `namespace inscope ns script`
        "uplevel" => vec![n.saturating_sub(1)],
        "time" | "coroutine" => vec![1],
        _ => Vec::new(),
    }
}

/// True when a word was written in braces, i.e. it is a script rather than a value.
pub(crate) fn is_braced(script: &Script, word: &[Token]) -> bool {
    word.first()
        .and_then(|t| script.text(t.range()))
        .is_some_and(|s| s.starts_with('{'))
}

/// Splits a Tcl argument list, where an element may be `name` or `{name default}`.
pub fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for ch in args.chars() {
        match ch {
            '{' => {
                depth += 1;
                if depth == 1 {
                    continue;
                }
                cur.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    push_arg(&mut out, &mut cur);
                    continue;
                }
                cur.push(ch);
            }
            c if c.is_whitespace() && depth == 0 => push_arg(&mut out, &mut cur),
            c => cur.push(c),
        }
    }
    push_arg(&mut out, &mut cur);
    out
}

fn push_arg(out: &mut Vec<String>, cur: &mut String) {
    if cur.is_empty() {
        return;
    }
    // `{name default}` binds only `name`.
    let name = cur.split_whitespace().next().unwrap_or("").to_string();
    if !name.is_empty() {
        out.push(name);
    }
    cur.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Outline {
        outline(&Script::new(src))
    }

    fn qnames(o: &Outline) -> Vec<String> {
        fn rec(s: &[Symbol], out: &mut Vec<String>) {
            for sym in s {
                out.push(sym.qname.clone());
                rec(&sym.children, out);
            }
        }
        let mut v = Vec::new();
        rec(&o.symbols, &mut v);
        v
    }

    #[test]
    fn finds_top_level_procs() {
        let o = parse("proc greet {name} { puts hi }\nproc bye {} {}\n");
        assert_eq!(qnames(&o), vec!["::greet", "::bye"]);
        assert_eq!(o.symbols[0].detail.as_deref(), Some("name"));
    }

    #[test]
    fn nests_procs_inside_namespaces() {
        let o = parse("namespace eval util { proc trim {s} {} }\n");
        assert_eq!(qnames(&o), vec!["::util", "::util::trim"]);
    }

    #[test]
    fn handles_nested_namespaces() {
        let o = parse("namespace eval a { namespace eval b { proc c {} {} } }\n");
        assert_eq!(qnames(&o), vec!["::a", "::a::b", "::a::b::c"]);
    }

    #[test]
    fn absolute_names_ignore_the_enclosing_namespace() {
        let o = parse("namespace eval a { proc ::top {} {} }\n");
        assert_eq!(qnames(&o), vec!["::a", "::top"]);
    }

    #[test]
    fn qualified_proc_names_resolve_to_their_namespace() {
        let o = parse("proc net::http::get {url} {}\n");
        assert_eq!(qnames(&o), vec!["::net::http::get"]);
        assert_eq!(o.symbols[0].name, "get");
    }

    #[test]
    fn finds_tcloo_members() {
        let o = parse(
            "oo::class create Shape {\n  variable sides\n  constructor {n} {}\n  \
             method area {} {}\n  destructor {}\n}\n",
        );
        assert_eq!(
            qnames(&o),
            vec![
                "::Shape",
                "::Shape::sides",
                "::Shape::constructor",
                "::Shape::area",
                "::Shape::destructor"
            ]
        );
    }

    // --- itcl and snit -----------------------------------------------------

    #[test]
    fn finds_itcl_class_members() {
        let o = parse(
            "itcl::class Shape {\n  inherit Base\n  constructor {args} {}\n  destructor {}\n  \
             public method area {} {}\n  protected method helper {x} {}\n  \
             private variable sides 3\n  common registry\n  proc make {} {}\n}\n",
        );
        assert_eq!(
            qnames(&o),
            vec![
                "::Shape",
                "::Shape::constructor",
                "::Shape::destructor",
                "::Shape::area",
                "::Shape::helper",
                "::Shape::sides",
                "::Shape::registry",
                "::Shape::make",
            ]
        );
    }

    /// itcl spells inheritance `inherit`, not `superclass`.
    #[test]
    fn itcl_inherit_is_a_supertype() {
        let o = parse("itcl::class Derived {\n  inherit Base Other\n}\n");
        assert_eq!(o.symbols[0].supers, vec!["Base", "Other"]);
    }

    #[test]
    fn access_modifiers_do_not_hide_the_member() {
        let o = parse("itcl::class C {\n  public method m {a b} {}\n}\n");
        let m = &o.symbols[0].children[0];
        assert_eq!(m.name, "m");
        assert_eq!(m.kind, SymbolKind::Method);
        assert_eq!(m.detail.as_deref(), Some("a b"));
    }

    #[test]
    fn finds_snit_type_members() {
        let o = parse(
            "snit::type Server {\n  option -port 80\n  variable sock\n  typevariable count\n  \
             component logger\n  constructor {args} {}\n  method start {} {}\n  \
             typemethod reset {} {}\n}\n",
        );
        assert_eq!(
            qnames(&o),
            vec![
                "::Server",
                "::Server::-port",
                "::Server::sock",
                "::Server::count",
                "::Server::logger",
                "::Server::constructor",
                "::Server::start",
                "::Server::reset",
            ]
        );
    }

    #[test]
    fn snit_widget_and_widgetadaptor_are_classes() {
        for form in ["snit::widget", "snit::widgetadaptor"] {
            let o = parse(&format!("{form} W {{\n  method draw {{}} {{}}\n}}\n"));
            assert_eq!(o.symbols[0].kind, SymbolKind::Class, "{form}");
            assert_eq!(qnames(&o), vec!["::W", "::W::draw"], "{form}");
        }
    }

    #[test]
    fn megawidget_bodies_are_still_walked_for_nested_procs() {
        let o = parse("snit::type T {\n  method m {} {\n    set local 1\n  }\n}\n");
        assert!(o.variables.iter().any(|v| v.name == "local"));
    }

    #[test]
    fn records_superclasses_and_mixins() {
        let o = parse(
            "oo::class create Base {}\noo::class create Mix {}\n\
             oo::class create Derived {\n  superclass Base\n  mixin Mix\n  method m {} {}\n}\n",
        );
        let d = o.symbols.iter().find(|s| s.name == "Derived").unwrap();
        assert_eq!(d.supers, vec!["Base", "Mix"]);
        let b = o.symbols.iter().find(|s| s.name == "Base").unwrap();
        assert!(b.supers.is_empty());
    }

    #[test]
    fn superclasses_do_not_leak_between_classes() {
        let o = parse(
            "oo::class create A {\n superclass X\n}\noo::class create B {\n method m {} {}\n}\n",
        );
        let b = o.symbols.iter().find(|s| s.name == "B").unwrap();
        assert!(b.supers.is_empty(), "B inherited A's list: {:?}", b.supers);
    }

    #[test]
    fn attaches_leading_comments_as_docs() {
        let o = parse("# Greets a person.\n# Twice.\nproc greet {n} {}\n");
        assert_eq!(
            o.symbols[0].doc.as_deref(),
            Some("Greets a person.\nTwice.")
        );
    }

    #[test]
    fn skips_dynamically_named_definitions() {
        let o = parse("proc [genName] {} {}\nproc real {} {}\n");
        assert_eq!(qnames(&o), vec!["::real"]);
    }

    #[test]
    fn namespace_export_is_not_a_scope() {
        let o = parse("namespace eval a { namespace export * \n proc b {} {} }\n");
        assert_eq!(qnames(&o), vec!["::a", "::a::b"]);
    }

    #[test]
    fn braces_in_strings_do_not_confuse_the_walker() {
        let o = parse("proc f {} { puts \"a } b\" }\nproc g {} {}\n");
        assert_eq!(qnames(&o), vec!["::f", "::g"]);
    }

    #[test]
    fn reports_unterminated_input_as_incomplete() {
        let o = parse("proc f {} {\n");
        assert!(o.errors.iter().any(|e| e.incomplete));
    }

    #[test]
    fn name_ranges_point_at_the_name() {
        let src = "proc greet {n} {}";
        let o = parse(src);
        assert_eq!(&src[o.symbols[0].name_range.clone()], "greet");
    }

    // --- references -------------------------------------------------------

    /// A `[...]` substitution is a script, and Tcl's parser has already said so.
    /// Everything inside one used to be invisible to references and hints.
    #[test]
    fn descends_into_command_substitutions() {
        let o = parse("proc helper {} {}\nset x [helper]\nputs [expr {1 + 2}]\n");
        let names: Vec<&str> = o.calls.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"helper"), "got {names:?}");
        assert!(names.contains(&"expr"), "got {names:?}");
        assert!(
            o.refs.iter().any(|r| r.name == "helper"),
            "the call inside [...] must count as a reference"
        );
    }

    #[test]
    fn nested_substitutions_are_visited_once() {
        let o = parse("puts [lindex [split $s] 0]\n");
        let names: Vec<&str> = o.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names.iter().filter(|n| **n == "split").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "lindex").count(), 1);
    }

    #[test]
    fn records_command_references() {
        let o = parse("proc greet {} {}\ngreet\ngreet\n");
        let calls = o
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Command && r.name == "greet")
            .count();
        assert_eq!(
            calls, 2,
            "two call sites, excluding the definition's own name"
        );
    }

    #[test]
    fn records_variable_references_with_ranges() {
        let src = "set name x\nputs $name\n";
        let o = parse(src);
        let r = o
            .refs
            .iter()
            .find(|r| r.kind == RefKind::Variable)
            .expect("a variable reference");
        assert_eq!(r.name, "name");
        assert_eq!(&src[r.range.clone()], "name");
    }

    #[test]
    fn references_carry_their_namespace() {
        let o = parse("namespace eval a { helper }\n");
        let r = o
            .refs
            .iter()
            .find(|r| r.name == "helper")
            .expect("command ref");
        assert_eq!(r.namespace, "::a");
    }

    // --- variables --------------------------------------------------------

    #[test]
    fn binds_proc_parameters_within_the_body() {
        let o = parse("proc f {a {b 2} args} { set c 3 }\n");
        let names: Vec<_> = o.variables.iter().map(|v| v.name.as_str()).collect();
        for want in ["a", "b", "args", "c"] {
            assert!(names.contains(&want), "missing binding {want} in {names:?}");
        }
    }

    #[test]
    fn parameter_scope_is_the_proc_body() {
        let src = "proc f {a} { set b 1 }\nset outer 2\n";
        let o = parse(src);
        let a = o.variables.iter().find(|v| v.name == "a").unwrap();
        let outer = o.variables.iter().find(|v| v.name == "outer").unwrap();
        assert!(a.scope.end < src.len(), "parameter scope stops at the body");
        assert!(
            !a.scope.contains(&outer.range.start),
            "a global set is outside the proc's scope"
        );
    }

    #[test]
    fn binds_foreach_and_lassign_targets() {
        let o = parse("foreach {k v} $pairs { puts $k }\nlassign $l p q\n");
        let names: Vec<_> = o.variables.iter().map(|v| v.name.as_str()).collect();
        for want in ["k", "v", "p", "q"] {
            assert!(names.contains(&want), "missing {want} in {names:?}");
        }
    }

    // --- links ------------------------------------------------------------

    #[test]
    fn records_source_and_package_links() {
        let o = parse("source lib/util.tcl\npackage require json 1.2\npackage provide mine 1.0\n");
        let src = o
            .links
            .iter()
            .find(|l| l.kind == LinkKind::Source)
            .expect("a source link");
        assert_eq!(src.name, "lib/util.tcl");
        let req = o
            .links
            .iter()
            .find(|l| l.kind == LinkKind::PackageRequire)
            .expect("a package require");
        assert_eq!(req.name, "json");
        assert_eq!(o.provides.len(), 1);
        assert_eq!(o.provides[0].name, "mine");
    }

    #[test]
    fn source_link_skips_the_encoding_option() {
        let o = parse("source -encoding utf-8 a.tcl\n");
        assert_eq!(o.links.len(), 1);
        assert_eq!(o.links[0].name, "a.tcl");
    }

    /// A computed path cannot be resolved, so it must not be offered as a link.
    #[test]
    fn skips_computed_source_paths() {
        let o = parse("source [file join $dir x.tcl]\n");
        assert!(o.links.is_empty(), "got {:?}", o.links);
    }

    #[test]
    fn link_ranges_point_at_the_argument() {
        let src = "source lib/util.tcl\n";
        let o = parse(src);
        assert_eq!(&src[o.links[0].range.clone()], "lib/util.tcl");
    }

    #[test]
    fn descends_into_control_flow_bodies() {
        let o = parse("if {1} {\n  proc inner {} {}\n}\n");
        assert!(
            qnames(&o).contains(&"::inner".to_string()),
            "a proc defined inside `if` must still be found"
        );
    }

    /// Tcl gives no syntactic hint that `{$a ne {}}` is an expression rather than a
    /// script. Parsing it as a script is a hard syntax error — this shape appears
    /// throughout tcllib and is why script positions are modelled per command.
    #[test]
    fn expression_conditions_are_never_parsed_as_scripts() {
        let o = parse("if {($a ne {}) && ($b ne {})} {\n  proc inner {} {}\n}\n");
        assert!(
            o.errors.is_empty(),
            "the condition must not yield errors: {:?}",
            o.errors
        );
        assert!(qnames(&o).contains(&"::inner".to_string()));
    }

    #[test]
    fn descends_into_else_and_elseif_bodies() {
        let o = parse(
            "if {$a} {\n proc t {} {}\n} elseif {$b} {\n proc ei {} {}\n} else {\n proc e {} {}\n}\n",
        );
        let names = qnames(&o);
        for want in ["::t", "::ei", "::e"] {
            assert!(
                names.contains(&want.to_string()),
                "missing {want} in {names:?}"
            );
        }
    }

    #[test]
    fn descends_into_for_and_catch_bodies_but_not_the_test() {
        let o = parse(
            "for {set i 0} {$i < 3} {incr i} {\n proc body {} {}\n}\ncatch {\n proc c {} {}\n}\n",
        );
        let names = qnames(&o);
        assert!(names.contains(&"::body".to_string()));
        assert!(names.contains(&"::c".to_string()));
        assert!(o.errors.is_empty(), "{:?}", o.errors);
    }

    #[test]
    fn does_not_treat_expr_conditions_as_scripts() {
        // `{1}` is the condition, not a body; walking it must not invent symbols.
        let o = parse("while {1} { set x 1 }\n");
        assert!(qnames(&o).is_empty());
        assert!(o.variables.iter().any(|v| v.name == "x"));
    }

    #[test]
    fn braced_bodies_with_line_continuations_walk_fully() {
        // THE false-positive bug: a braced body whose interior contains
        // backslash-sequences (`\`-newline continuations) tokenizes as
        // [Word [Text][Backslash][Text]...]. body_range used to return the
        // FIRST Text child's range, truncating the body at the first
        // continuation, so every following re-parse ended mid-bracket and
        // reported "unterminated command". The interior is the head token
        // minus its braces — parse it whole.
        let o = parse(
            "proc case_a {} {\n    _collapsible $w k [dict create \\\n        a 1 \\\n        b 2]\n    proc inner {} { puts hi }\n}\n",
        );
        assert!(o.errors.is_empty(), "no synthetic errors: {:?}", o.errors);
        assert_eq!(qnames(&o), vec!["::case_a", "::inner"]);
    }
}
