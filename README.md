# tcl-lsp

A language server for **Tcl** and **Tk**, written in Rust.

Most Tcl tooling approximates the language with regular expressions or a hand-written
grammar. That does not work well, because Tcl has no grammar independent of its
implementation — quoting, brace nesting, backslash continuation and substitution rules are
*defined by* `Tcl_ParseCommand`. So this server links against **libtcl itself** and uses
Tcl's own parser as the authority on how a script splits into commands and words, with
[tree-sitter-tcl] layered on top for fast, error-tolerant incremental reparsing while you
are mid-keystroke.

Status: **early development.** See [Roadmap](#roadmap).

## Supported Tcl versions

Tcl/Tk **8.6** is the primary, default target — both as the runtime the server links
against and as the command set it analyses your code against. Tcl/Tk **9.0** is supported
as an optional build variant (`packages.tcl-lsp-tcl9`).

These are not interchangeable at the C level: `Tcl_Token` changed its `size` and
`numComponents` fields from `int` to `Tcl_Size` in 9.0, so the FFI bindings are generated
per-version with bindgen rather than hand-written.

## Features

| Working today | Notes |
|---|---|
| Diagnostics | own parser, plus nagelfar and tclint |
| Completion | workspace procs, Tcl/Tk builtins, in-scope variables after `$`, and Tk widget `-options` |
| Hover | user procs with their doc comment; builtins from the man pages |
| Go to definition | across the whole workspace, namespace-qualified |
| Find references | command call sites across files |
| Document highlight | uses and definitions in the current file |
| Document + workspace symbols | procs, namespaces, TclOO classes, methods, variables |
| Folding ranges | proc, namespace and class bodies |
| Selection ranges | expand-selection: word → command → body → definition → file |
| Document links | `source` paths, and `package require` → its `package provide` |
| Signature help | user procs from their argument list; builtins and ensemble subcommands from the man pages |
| Rename | definition plus every call site; rewrites only the last segment of a qualified name |
| Semantic tokens | full and range; user procs distinguished from Tcl's own commands |
| Inlay hints | parameter names at call sites of user procs |
| Code actions | brace an unbraced expression; add a missing `package require` |
| Formatting | via `tclfmt` |
| Incremental sync | buffer is the source of truth, never the file on disk |
| Position encoding | negotiated; UTF-8 preferred, UTF-16 correct |

The workspace is indexed on startup, so definitions resolve in files you have never
opened. Not yet implemented: on-type formatting, code lens, and call or type hierarchies.
See [Roadmap](#roadmap).

Name resolution follows Tcl's real rules — a `::`-prefixed name is absolute, and a bare
name is looked up in the current namespace and then the global one, never in the levels
between.

Diagnostics come from three independently toggleable sources:

| Source | Provides |
|---|---|
| built-in analysis | parse errors, and unbraced expressions in `expr`/`if`/`while`/`for` |
| [nagelfar] | unknown variables, bad `expr`, wrong argument counts, invalid builtin options |
| [tclint] | style and lint rules (already reports precise columns) |

Formatting is delegated to `tclfmt` (part of tclint). External tools run out-of-process and
are never linked, so their licences do not affect this project.

## Building

```sh
nix build              # the server, against Tcl/Tk 8.6
nix build .#tcl-lsp-tcl9   # optional Tcl/Tk 9.0 variant
nix flake check        # build + clippy + rustfmt + tests
nix develop            # dev shell with rust, tcl 8.6, nagelfar, tclint
```

## Editor setup

<details>
<summary>Neovim (nvim-lspconfig)</summary>

```lua
vim.lsp.config.tcl_lsp = {
  cmd = { 'tcl-lsp' },
  filetypes = { 'tcl' },
  root_markers = { 'pkgIndex.tcl', 'tclIndex', '.git' },
}
vim.lsp.enable('tcl_lsp')
```
</details>

<details>
<summary>Helix (languages.toml)</summary>

```toml
[language-server.tcl-lsp]
command = "tcl-lsp"

[[language]]
name = "tcl"
language-servers = ["tcl-lsp"]
```
</details>

<details>
<summary>Emacs (eglot)</summary>

```elisp
(add-to-list 'eglot-server-programs '(tcl-mode . ("tcl-lsp")))
```
</details>

<details>
<summary>VS Code / Zed</summary>

Point the client at the `tcl-lsp` binary over stdio. A dedicated extension is not yet
published.
</details>

## Configuration

Sent via `workspace/didChangeConfiguration` under the `tclLsp` key:

Configuration is currently read from the environment; a `workspace/didChangeConfiguration`
mapping is planned. The Nix wrapper sets each of these as a *default*, so your own
environment always wins.

| Variable | Default | Meaning |
|---|---|---|
| `TCL_LSP_TCL_VERSION` | `8.6` | command set to analyse against (`8.6` or `9.0`) |
| `TCL_LSP_NAGELFAR` | baked store path | the `nagelfar` binary |
| `TCL_LSP_NAGELFAR_DB` | `syntaxdb86.tcl` | its syntax database |
| `TCL_LSP_NAGELFAR_ENABLE` | on | set to `0` to disable |
| `TCL_LSP_TCLINT` | baked store path | the `tclint` binary |
| `TCL_LSP_TCLINT_ENABLE` | on | set to `0` to disable |
| `TCL_LSP_TCLFMT` | baked store path | the `tclfmt` binary |

Note the analysis target is independent of the libtcl the binary is *linked* against: 8.6
and 9.0 parse identically, so an 8.6 build can correctly analyse a 9.0 project.

### Regenerating the command database

The Tcl/Tk builtin documentation is generated from the man pages of the pinned toolchain
and committed, so `cargo build` needs neither Tcl nor Nix. After a nixpkgs bump:

```sh
nix run .#regen-cmddb
```

`nix flake check` fails if the committed copies have drifted from the pinned man pages.

## Roadmap

- **Phase 1 — done.** Incremental sync, diagnostics, document symbols, definition, hover,
  completion, formatting.
- **Phase 2 — done.** References, highlights, workspace symbols, folding, selection
  ranges, document links, signature help.
- **Phase 3 — mostly done.** Rename, semantic tokens, inlay hints and code actions have
  landed; on-type formatting remains.
- **Phase 4** — call/type hierarchy, code lens, `snit`/`itcl`. Tk `-option` *completion*
  has landed; validating them is deliberately still open, because a widget also inherits
  the standard options listed via `.SO`, which the generator does not yet resolve — so
  flagging an unknown option today would produce false positives.

## Prior art

- [jdc8/lsp] — a Tcl LSP written *in* Tcl. Its critcl shim exposing `Tcl_ParseCommand` to
  script level is the idea this project builds on. Kept locally under `lsp/` (gitignored)
  purely for reference; none of its code is used here.
- `tclsp`, shipped inside [tclint] — a Python server offering diagnostics and formatting.

## Licence

Dual-licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your
option.

[tree-sitter-tcl]: https://github.com/tree-sitter-grammars/tree-sitter-tcl
[nagelfar]: https://nagelfar.sourceforge.net/
[tclint]: https://github.com/nmoroze/tclint
[jdc8/lsp]: https://github.com/jdc8/lsp
