# tcl-lsp.el

An Emacs client for [`tcl-lsp`](../..), supporting **lsp-mode** (the fuller
experience) and **eglot**, sharing one implementation of server discovery and
configuration.

## Install

### straight.el / use-package

The package lives in a subdirectory of the server's repository, so the recipe
needs `:files`:

```elisp
(use-package tcl-lsp
  :straight (tcl-lsp :type git :host github
                     :repo "pillowtrucker/tcl-lsp"
                     :files ("editors/emacs/tcl-lsp.el"))
  :after lsp-mode
  :hook (tcl-mode . tcl-lsp-mode))
```

or, hacking on it locally:

```elisp
(use-package tcl-lsp
  :straight (tcl-lsp :local-repo "/home/you/tcl-lsp-flake"
                     :files ("editors/emacs/tcl-lsp.el"))
  :after lsp-mode
  :hook (tcl-mode . tcl-lsp-mode))
```

Then start the server the same way you start every other one:

```elisp
;; in your lsp-mode :hook list, alongside (rust-mode . lsp-deferred) etc.
(tcl-mode . lsp-deferred)
```

Use `lsp-deferred`, not `lsp` — see [direnv](#direnv-and-envrc) below.

### Nix

`nix build .#emacs-tcl-lsp` produces an Emacs package with the server's store
path baked in, so it works with no configuration:

```nix
environment.systemPackages = [
  (pkgs.emacsWithPackages (epkgs: [
    inputs.tcl-lsp.packages.${system}.emacs-tcl-lsp
    epkgs.lsp-mode
    epkgs.lsp-ui
  ]))
];
```

There is no `epkgs.tcl-lsp` overlay attribute, and that is deliberate. The
client bakes a specific `tcl-lsp` store path in as its fallback, so an overlay
would build it against *your* nixpkgs and quietly bake in a different server
than this flake pins. Referring to the flake package keeps the two in step.

### home-manager

`homeManagerModules.default` exists and installs both halves:

```nix
{
  imports = [ inputs.tcl-lsp.homeManagerModules.default ];
  programs.tcl-lsp.enable = true;
}
```

`programs.tcl-lsp.package` selects the server (`tcl-lsp-tcl9` for a Tcl 9.0
runtime), and `programs.tcl-lsp.emacs.enable` — defaulting to whatever
`programs.emacs.enable` is — adds the client and lsp-mode to
`programs.emacs.extraPackages`.

**This module is best-effort.** The author does not use home-manager, so it is
covered by `checks.hm-module`, which evaluates it against a stub of the two
options it writes to, but it has never been through a real activation.

## direnv and envrc

The server is found in this order, resolved **per buffer, when the session
starts**:

1. `tcl-lsp-server-path`, if you set it
2. `executable-find "tcl-lsp"` — so a project's own build wins
3. the store path baked in by the Nix build, if there is one

Step 2 is why this works with `envrc`: `envrc-mode` sets `exec-path`
buffer-locally, and the client passes a *function* to `lsp-stdio-connection`
rather than a string, so resolution happens in your buffer after envrc has
applied. A string would be resolved once, when the client is registered.

Two consequences worth knowing:

- **Use `lsp-deferred`, not `lsp`.** `tcl-mode-hook` runs *before*
  `after-change-major-mode-hook`, which is where `envrc-global-mode` enables
  itself. `lsp-deferred` waits for idle, by which time envrc has run.
- **After `direnv allow`, restart the workspace.** The session caches the
  command it resolved. `M-x lsp-workspace-restart`.

`M-x tcl-lsp-which-server` reports which binary this buffer would use and why.
That is the first thing to run when something looks wrong.

## Configuration

Every setting is unset by default, and unset means *the client says nothing*.
That matters: `initializationOptions` outranks the environment, so a client
that always sent its defaults would silently override `TCL_LSP_*` variables
from your shell, your `.envrc`, or the paths the Nix wrapper baked in.

| Variable | Effect |
|---|---|
| `tcl-lsp-server-path` | explicit server binary |
| `tcl-lsp-tcl-version` | `"8.6"` or `"9.0"` — the command set to analyse against |
| `tcl-lsp-nagelfar-enable` | `t` / `:json-false` / nil |
| `tcl-lsp-nagelfar-path`, `tcl-lsp-nagelfar-syntax-db` | nagelfar's binary and database |
| `tcl-lsp-tclint-enable`, `tcl-lsp-tclint-path` | tclint |
| `tcl-lsp-tclfmt-enable`, `tcl-lsp-tclfmt-path` | formatting |
| `tcl-lsp-claim-test-extension` | open `.test` in `tcl-mode` (off; the extension is contested) |
| `tcl-lsp-watch-files` | tell the server about files changed outside Emacs |
| `tcl-lsp-apply-lsp-ui-presets` | Tcl-only lsp-ui tweaks, buffer-local (off) |

The enable flags are deliberately **tri-state**: nil leaves the decision to the
server, and only `t` and `:json-false` are transmitted.

The analysis target is independent of the libtcl the binary links against —
8.6 and 9.0 parse identically, so an 8.6 build analyses a 9.0 project fine.

## Keys

`tcl-lsp-mode` binds a `C-c t` prefix, chosen to avoid both lsp-mode's `C-c l`
and `tcl-mode`'s own `C-c C-…` bindings.

| Key | Command |
|---|---|
| `C-c t h` | type hierarchy (TclOO / itcl / snit) |
| `C-c t c` | call hierarchy |
| `C-c t v` | toggle the Tcl version, live |
| `C-c t n` | toggle nagelfar, live |
| `C-c t l` | toggle tclint, live |
| `C-c t w` | which server am I using? |

The toggles send `workspace/didChangeConfiguration`; the server reloads its
command database and re-publishes diagnostics for every open file, so the
effect is immediate.

Type and call hierarchy need `lsp-treemacs`.

## What each client supports

eglot's limits here are structural, not missing configuration — it never sends
the requests, so no amount of client config reaches them. Checked against
**eglot 1.17.30**, the version bundled with Emacs 30.2; note that is older than
the 1.24 in `emacsPackages.eglot`, because Emacs prefers its built-in copy
unless you install eglot explicitly.

| | lsp-mode | eglot |
|---|---|---|
| Diagnostics, completion, hover, signature help | yes | yes |
| Definition, references, document highlight | yes | yes |
| Document + workspace symbols, rename, code actions | yes | yes |
| Formatting, on-type formatting, inlay hints | yes | yes |
| **Semantic tokens** | yes | no |
| **Code lens** (reference counts) | yes | no |
| **Call / type hierarchy** | yes | no |
| **Folding, selection ranges, document links** | yes | no |

The server advertises all of these regardless; eglot simply ignores the ones it
has no request for. Its full request set is the 23 `textDocument/*` methods in
its source, and `foldingRange`, `selectionRange` and `documentLink` are not
among them — `eglot-ignored-server-capabilities` mentions the latter two, but
only as things to suppress in the *advertised* capabilities.

## Three things this client does that a generic setup does not

- **Code lenses work.** The server sends `"3 references"` with an *empty*
  command string, because only a client knows how to show references. Without
  a binding, lsp-mode falls through to `workspace/executeCommand`, which this
  server does not implement. Handled via an `:action-handlers` entry keyed by
  `""`.
- **The index stays fresh.** The server scans the workspace once at startup and
  never registers file watchers, so lsp-mode never watches anything. This
  package runs its own `filenotify` watcher and sends
  `workspace/didChangeWatchedFiles`, so a file added from a shell is visible to
  goto-definition without restarting.
- **Declarations are not painted as types.** lsp-mode layers semantic-token
  *modifier* faces over type faces, and its default for `declaration` inherits
  `font-lock-type-face`. Since the server marks every proc, namespace, class,
  method and parameter as a declaration, the stock result is that every
  definition in the buffer looks like a type. `tcl-lsp-declaration-face` is
  empty by default to undo that; customise it if you want declarations marked.

Relatedly, the server's `keyword` token means *a command Tcl itself provides*
and `function` means *a proc you wrote*. Tcl has no reserved words, so
`keyword` maps to `font-lock-builtin-face`, not `font-lock-keyword-face`.

## Telling the diagnostic sources apart

Findings come from three places — `tcl-lsp`, `nagelfar` and `tclint` — with
different precision: tcl-lsp and tclint carry exact ranges, nagelfar only a
line.

lsp-mode files the LSP `source` into flycheck's `group` slot and the `code`
into `id`; flycheck displays neither by default, so reach it with
`flycheck-error-group`. Flymake users can get the source prefixed into the
message:

```elisp
(setq lsp-diagnostics-flymake-message-formatter
      #'tcl-lsp-flymake-message-formatter)
```

which renders `[nagelfar] Unknown variable "nam"`.

## Tests

```sh
nix build .#checks.x86_64-linux.emacs
```

Byte-compiles with warnings as errors, runs `checkdoc`, and drives a real
`tcl-lsp` process. The integration tests go through eglot because its request
path is synchronous, which makes headless assertions deterministic; the
lsp-mode side asserts the wiring a session depends on.
