;;; tcl-lsp.el --- Emacs client for the tcl-lsp language server -*- lexical-binding: t; -*-

;; Copyright (C) 2026 tcl-lsp contributors

;; Author: tcl-lsp contributors
;; Version: 0.1.0
;; Package-Requires: ((emacs "29.1"))
;; Keywords: languages, tcl, tools
;; URL: https://github.com/pillowtrucker/tcl-lsp
;; SPDX-License-Identifier: MIT OR Apache-2.0

;;; Commentary:

;; A client for `tcl-lsp', a language server for Tcl and Tk that links against
;; libtcl and uses Tcl's own parser.
;;
;; Supports both `lsp-mode' (the fuller experience) and `eglot', sharing one
;; implementation of server discovery and configuration.  Neither is a hard
;; dependency: whichever is present gets wired up.
;;
;; Quick start with lsp-mode:
;;
;;     (require 'tcl-lsp)
;;     (add-hook 'tcl-mode-hook #'lsp-deferred)
;;
;; or with eglot:
;;
;;     (require 'tcl-lsp)
;;     (add-hook 'tcl-mode-hook #'eglot-ensure)
;;
;; Three server behaviours need client-side handling, and this package provides
;; it; see the commentary at each definition for why:
;;
;;   - code lenses arrive with an empty command, to be bound by the client;
;;   - the server never registers file watchers, so nothing reindexes on its own;
;;   - the workspace is indexed once at startup, so the root has to be right.

;;; Code:

(require 'cl-lib)
(require 'filenotify)

(declare-function lsp-register-client "lsp-mode")
(declare-function lsp-stdio-connection "lsp-mode")
(declare-function lsp-activate-on "lsp-mode")
(declare-function lsp-configuration-section "lsp-mode")
(declare-function lsp-find-references "lsp-mode")
(declare-function lsp-notify "lsp-mode")
(declare-function lsp-workspace-root "lsp-mode")
(declare-function lsp-workspaces "lsp-mode")
(declare-function lsp--workspace-client "lsp-mode")
(declare-function lsp--client-server-id "lsp-mode")
(declare-function lsp--info "lsp-mode")
(declare-function lsp--warn "lsp-mode")
(declare-function make-lsp-client "lsp-mode")
(declare-function lsp-session "lsp-mode")
(declare-function lsp--session-workspaces "lsp-mode")
(declare-function lsp--workspace-root "lsp-mode")
(declare-function lsp--path-to-uri "lsp-mode")
(declare-function lsp-treemacs-call-hierarchy "lsp-treemacs")
(declare-function lsp-treemacs-type-hierarchy "lsp-treemacs")

(defvar lsp-ui-sideline-show-diagnostics)
(defvar lsp-ui-sideline-show-code-actions)
(defvar lsp-ui-sideline-show-hover)

(defgroup tcl-lsp nil
  "Client for the tcl-lsp language server."
  :group 'tools
  :prefix "tcl-lsp-"
  :link '(url-link "https://github.com/pillowtrucker/tcl-lsp"))


;;; Server discovery

(defconst tcl-lsp-bundled-server-path nil
  "Absolute path to a `tcl-lsp' binary baked in at package build time.
The Nix package substitutes a store path here so the client works with no
configuration.  Nil when the package was not built by Nix.")

(defcustom tcl-lsp-server-path nil
  "Explicit path to the `tcl-lsp' executable.
When nil the server is looked up on the variable `exec-path', then falls back to
`tcl-lsp-bundled-server-path'."
  :type '(choice (const :tag "Search exec-path, then the bundled server" nil)
                 (file :tag "Explicit path" :must-match t))
  :group 'tcl-lsp)

(defcustom tcl-lsp-server-args nil
  "Extra command-line arguments passed to the server."
  :type '(repeat string)
  :group 'tcl-lsp)

(defun tcl-lsp-server-program ()
  "Return the path to the `tcl-lsp' executable, or nil if none is found.

Resolution order is deliberate.  An explicit setting wins; otherwise the
variable `exec-path' is searched *before* the bundled path, so a project
providing its own build — through direnv, envrc, or a Nix dev shell — takes
precedence over whatever this package was built against.

This must be called from the buffer the server will serve, and late enough
that a buffer-local environment is in effect.  That is why the lsp-mode client
below registers a function rather than a string: a string is resolved when the
client is registered, long before any buffer exists."
  (or tcl-lsp-server-path
      (executable-find "tcl-lsp")
      (and tcl-lsp-bundled-server-path
           (file-executable-p tcl-lsp-bundled-server-path)
           tcl-lsp-bundled-server-path)))

(defun tcl-lsp-server-command ()
  "Return the full command list to start the server."
  (if-let* ((program (tcl-lsp-server-program)))
      (cons program tcl-lsp-server-args)
    (user-error
     "Cannot find the `tcl-lsp' executable.  Set `tcl-lsp-server-path', or \
enter a shell that provides it (for this project, `direnv allow')")))

(defun tcl-lsp-server-available-p ()
  "Return non-nil when a `tcl-lsp' executable can be found."
  (and (tcl-lsp-server-program) t))

;;;###autoload
(defun tcl-lsp-which-server ()
  "Report which `tcl-lsp' executable this buffer would use, and why.
Useful when a direnv environment is not being picked up as expected."
  (interactive)
  (let ((program (tcl-lsp-server-program)))
    (message "tcl-lsp: %s"
             (cond
              ((null program) "not found on exec-path or bundled")
              ((equal program tcl-lsp-server-path)
               (format "%s (from `tcl-lsp-server-path')" program))
              ((equal program tcl-lsp-bundled-server-path)
               (format "%s (bundled; exec-path has none)" program))
              (t (format "%s (from exec-path)" program))))))


;;; Which buffers get a server

(defcustom tcl-lsp-claim-test-extension nil
  "Whether to open .test files in `tcl-mode'.

Off by default, deliberately.  The server does index .test files as tcltest
suites, but that extension is claimed by many unrelated ecosystems, and taking
it globally would hijack their files.  Turn this on in projects where .test
really does mean Tcl."
  :type 'boolean
  :group 'tcl-lsp)

(defconst tcl-lsp-file-extensions '("tcl" "tm" "itcl")
  "Extensions always associated with `tcl-mode' by this package.
A subset of what the server indexes; see `tcl-lsp-claim-test-extension'.")

(defun tcl-lsp--register-auto-modes ()
  "Associate Tcl file extensions with `tcl-mode'."
  (dolist (ext (append tcl-lsp-file-extensions
                       (and tcl-lsp-claim-test-extension '("test"))))
    (add-to-list 'auto-mode-alist
                 (cons (concat "\\." (regexp-quote ext) "\\'") 'tcl-mode))))

(defconst tcl-lsp-root-files '("pkgIndex.tcl" "tclIndex" ".git")
  "Files marking a Tcl project root.

Getting this right matters more than usual: the server scans and indexes the
workspace exactly once, at startup, and never re-scans.  A root that is too
narrow leaves cross-file definitions and references silently empty.")


;;; Settings, shared by both clients
;;
;; The server accepts either a {"tclLsp": {...}} wrapper or the bare object, so
;; lsp-mode's ordinary payload works unchanged, and eglot can send the same
;; structure.  Each tool accepts a bare boolean (enable), a bare string (path),
;; or an object with `path' and `enable'.

(defcustom tcl-lsp-tcl-version nil
  "Tcl command set to analyse against, or nil to leave it to the server.

Independent of the libtcl the server binary links to: 8.6 and 9.0 parse
identically, so an 8.6 build analyses a 9.0 project correctly.  Changing this
makes the server reload its builtin command database and re-publish
diagnostics for every open file.

Left unset by default so that `TCL_LSP_TCL_VERSION' — from your shell, a
.envrc, or the Nix wrapper — keeps working.  Anything sent from here outranks
the environment."
  :type '(choice (const :tag "Leave to the server" nil)
                 (const "8.6")
                 (const "9.0"))
  :group 'tcl-lsp)

(defcustom tcl-lsp-nagelfar-enable nil
  "Whether to run the nagelfar backend.
It contributes unknown-variable, arity and builtin-option diagnostics.

Three states, not two.  nil says nothing and lets the server decide from its
own environment; only t and `:json-false' are transmitted.  A plain boolean
here would mean this package silently overrode `TCL_LSP_NAGELFAR_ENABLE'
every time Emacs started a server."
  :type '(choice (const :tag "Leave to the server" nil)
                 (const :tag "On" t)
                 (const :tag "Off" :json-false))
  :group 'tcl-lsp)

(defcustom tcl-lsp-nagelfar-path nil
  "Path to the nagelfar executable, or nil to use the server's default."
  :type '(choice (const :tag "Server default" nil) file)
  :group 'tcl-lsp)

(defcustom tcl-lsp-nagelfar-syntax-db nil
  "Path to a nagelfar syntax database, or nil to use the server's default.
nagelfar cannot run without one; the server's Nix wrapper supplies a
version-matched database."
  :type '(choice (const :tag "Server default" nil) file)
  :group 'tcl-lsp)

(defcustom tcl-lsp-tclint-enable nil
  "Whether to run the tclint backend.
Unlike nagelfar it reports precise columns.
Tri-state; see `tcl-lsp-nagelfar-enable'."
  :type (quote (choice (const :tag "Leave to the server" nil)
                 (const :tag "On" t)
                 (const :tag "Off" :json-false)))
  :group (quote tcl-lsp))

(defcustom tcl-lsp-tclint-path nil
  "Path to the tclint executable, or nil to use the server's default."
  :type '(choice (const :tag "Server default" nil) file)
  :group 'tcl-lsp)

(defcustom tcl-lsp-tclfmt-enable nil
  "Whether formatting is available.
Formatting is delegated to tclfmt; with this off, formatting is a no-op.
Tri-state; see `tcl-lsp-nagelfar-enable'."
  :type (quote (choice (const :tag "Leave to the server" nil)
                 (const :tag "On" t)
                 (const :tag "Off" :json-false)))
  :group (quote tcl-lsp))

(defcustom tcl-lsp-tclfmt-path nil
  "Path to the tclfmt executable, or nil to use the server's default."
  :type '(choice (const :tag "Server default" nil) file)
  :group 'tcl-lsp)

(defun tcl-lsp--tool-setting (enable path &optional extra)
  "Build one tool setting from ENABLE, PATH and EXTRA, or nil to say nothing.

Returns the most compact shape the server accepts: a bare boolean when only
ENABLE is meaningful, otherwise an alist carrying `path' and `enable'.  EXTRA
is an alist of additional keys folded into the object form.

Returns nil when nothing has been configured, so the key is omitted entirely.
That matters: `initializationOptions' outranks the environment, so a setting
sent unconditionally would silently override a `TCL_LSP_*' variable from the
user's shell or from the Nix wrapper."
  (cond
   ((and (null enable) (null path) (null extra)) nil)
   ((and (null path) (null extra)) enable)
   (t (append (when path `((path . ,path)))
              (when enable `((enable . ,enable)))
              extra))))

(defun tcl-lsp-settings ()
  "Return the `tclLsp' settings object, as an alist ready for JSON encoding.
Keys the user has not configured are absent, leaving the server's own
defaults — environment variables, or the paths the Nix wrapper baked in — in
force."
  (let ((settings
         `((tclVersion . ,tcl-lsp-tcl-version)
           (nagelfar . ,(tcl-lsp--tool-setting
                         tcl-lsp-nagelfar-enable
                         tcl-lsp-nagelfar-path
                         (when tcl-lsp-nagelfar-syntax-db
                           `((syntaxDb . ,tcl-lsp-nagelfar-syntax-db)))))
           (tclint . ,(tcl-lsp--tool-setting
                       tcl-lsp-tclint-enable tcl-lsp-tclint-path))
           (tclfmt . ,(tcl-lsp--tool-setting
                       tcl-lsp-tclfmt-enable tcl-lsp-tclfmt-path)))))
    (cl-remove-if (lambda (pair) (null (cdr pair))) settings)))

(defun tcl-lsp-initialization-options ()
  "Return the wrapped settings, or nil when there is nothing to say.
Nil rather than an empty section: an empty alist encodes as JSON null, and
sending `{\"tclLsp\": null}' is a confusing way to express \"no opinion\"."
  (when-let* ((settings (tcl-lsp-settings)))
    `((tclLsp . ,settings))))


;;; Reindexing on changes made outside Emacs
;;
;; The server scans the workspace once, at startup, and reindexes only when told
;; about a change.  It never sends `client/registerCapability', and lsp-mode only
;; watches files a server has *registered* interest in, so out of the box nothing
;; watches anything and a file added from a shell stays invisible to
;; goto-definition until the session restarts.  Hence our own watcher.

(defcustom tcl-lsp-watch-files t
  "Whether to tell the server about Tcl files changed outside Emacs."
  :type 'boolean
  :group 'tcl-lsp)

(defcustom tcl-lsp-watch-ignored-directories
  '(".git" ".direnv" ".hg" ".svn" "target" "node_modules" "result")
  "Directory names never descended into when watching a workspace."
  :type '(repeat string)
  :group 'tcl-lsp)

(defcustom tcl-lsp-watch-debounce 0.5
  "Seconds to coalesce filesystem events before notifying the server."
  :type 'number
  :group 'tcl-lsp)

(defvar tcl-lsp--watches nil
  "Alist of (ROOT . DESCRIPTORS) for workspaces currently being watched.")

(defvar tcl-lsp--pending-changes nil
  "Alist of (ROOT . EVENTS) awaiting the debounce timer.")

(defvar tcl-lsp--debounce-timer nil
  "Timer coalescing filesystem events.")

(defconst tcl-lsp--change-type '((created . 1) (changed . 2) (deleted . 3))
  "Map from `filenotify' actions to LSP FileChangeType values.")

(defun tcl-lsp--watchable-directories (root)
  "Return ROOT and its subdirectories worth watching."
  (let ((dirs (list root)))
    (dolist (entry (ignore-errors (directory-files root t "\\`[^.]" t)) dirs)
      (when (and (file-directory-p entry)
                 (not (member (file-name-nondirectory entry)
                              tcl-lsp-watch-ignored-directories)))
        (setq dirs (append dirs (tcl-lsp--watchable-directories entry)))))))

(defun tcl-lsp--tcl-file-p (file)
  "Return non-nil when FILE is one the server would index."
  (member (file-name-extension (or file "")) '("tcl" "tm" "test" "itcl")))

(defvar lsp--cur-workspace)

(defun tcl-lsp--notify (workspace method params)
  "Send notification METHOD with PARAMS to WORKSPACE.
Equivalent to lsp-mode's `with-lsp-workspace', which is only a binding of
`lsp--cur-workspace'.  Inlining it keeps a macro out of this file's
compile-time dependencies, so the package byte-compiles cleanly whether or not
lsp-mode is on the load path."
  (let ((lsp--cur-workspace workspace))
    (lsp-notify method params)))

(defun tcl-lsp--flush-changes ()
  "Send each coalesced change to the server."
  (setq tcl-lsp--debounce-timer nil)
  (dolist (entry tcl-lsp--pending-changes)
    (when-let* ((events (cdr entry)))
      (dolist (workspace (tcl-lsp--workspaces-for-root (car entry)))
        (ignore-errors
          (tcl-lsp--notify workspace "workspace/didChangeWatchedFiles"
                           `(:changes ,(vconcat (nreverse events))))))))
  (setq tcl-lsp--pending-changes nil))

(defun tcl-lsp--workspaces-for-root (root)
  "Return live tcl-lsp workspaces rooted at ROOT."
  (cl-remove-if-not
   (lambda (workspace)
     (and (eq 'tcl-lsp (lsp--client-server-id (lsp--workspace-client workspace)))
          (equal root (lsp--workspace-root workspace))))
   (lsp--session-workspaces (lsp-session))))

(defun tcl-lsp--handle-file-event (root event)
  "Queue EVENT under ROOT for the debounce timer."
  (let ((action (nth 1 event))
        (file (nth 2 event)))
    (when (and (tcl-lsp--tcl-file-p file)
               (assq action tcl-lsp--change-type))
      (let ((cell (or (assoc root tcl-lsp--pending-changes)
                      (car (push (cons root nil) tcl-lsp--pending-changes)))))
        (setcdr cell (cons (list :uri (lsp--path-to-uri file)
                                 :type (alist-get action tcl-lsp--change-type))
                           (cdr cell))))
      (when tcl-lsp--debounce-timer (cancel-timer tcl-lsp--debounce-timer))
      (setq tcl-lsp--debounce-timer
            (run-with-timer tcl-lsp-watch-debounce nil #'tcl-lsp--flush-changes)))))

(defun tcl-lsp--start-watching (workspace)
  "Watch the root of WORKSPACE so edits to Tcl files reach the server."
  (when-let* (((and tcl-lsp-watch-files (file-notify-valid-p nil)))
              (root (lsp--workspace-root workspace))
              ((eq 'tcl-lsp (lsp--client-server-id
                             (lsp--workspace-client workspace))))
              ((not (assoc root tcl-lsp--watches))))
    (let (descriptors)
      (dolist (dir (tcl-lsp--watchable-directories root))
        (when-let* ((descriptor
                     (ignore-errors
                       (file-notify-add-watch
                        dir '(change)
                        (lambda (event) (tcl-lsp--handle-file-event root event))))))
          (push descriptor descriptors)))
      (push (cons root descriptors) tcl-lsp--watches)
      (lsp--info "tcl-lsp: watching %d directories under %s"
                 (length descriptors) root))))

(defun tcl-lsp--stop-watching (workspace)
  "Stop watching WORKSPACE's root."
  (when-let* ((root (ignore-errors (lsp--workspace-root workspace)))
              (entry (assoc root tcl-lsp--watches)))
    (mapc (lambda (d) (ignore-errors (file-notify-rm-watch d))) (cdr entry))
    (setq tcl-lsp--watches (delq entry tcl-lsp--watches))))


;;; Code lenses
;;
;; The server sends a lens titled "N references" whose `command' is the empty
;; string, because only the client knows how to show references.  lsp-mode would
;; otherwise fall through to `workspace/executeCommand', which this server does
;; not implement.  Registering an action handler under "" intercepts it first;
;; see `lsp--execute-command'.

(defun tcl-lsp--code-lens-action (_command)
  "Show references for the symbol at point.
Bound to the server's reference-count lens."
  (interactive)
  (call-interactively #'lsp-find-references))


;;; Semantic tokens
;;
;; Every token type the server emits already has a default face in lsp-mode, so
;; highlighting works untouched.  The one distinction worth drawing out is the
;; server's own: "keyword" means a command Tcl itself provides, and "function"
;; means a proc defined in your code.  Seeing which is which at a glance is the
;; point of the feature, so keywords are mapped to the builtin face.

(defface tcl-lsp-declaration-face '((t nil))
  "Face layered over a symbol's own face where it is declared.

Empty on purpose.  lsp-mode applies modifier faces *over* type faces
\(`add-face-text-property' without APPEND\), and its default for `declaration'
inherits `font-lock-type-face'.  The server marks every proc, namespace,
class, method and parameter as a declaration, so accepting that default paints
every definition in the buffer as though it were a type.  Customise this if
you want declarations distinguished — `:weight bold' is a reasonable choice."
  :group 'tcl-lsp)

(defcustom tcl-lsp-semantic-token-face-overrides
  '(("keyword" . font-lock-builtin-face))
  "Overrides applied to lsp-mode's default semantic token faces.

An alist of (TOKEN-TYPE . FACE); token types are those in the server's legend:
comment, function, namespace, class, method, variable, parameter, keyword.

`keyword' is mapped to `font-lock-builtin-face' rather than
`font-lock-keyword-face' because Tcl has no reserved words — `if' and `proc'
are ordinary commands.  What the server means by `keyword' is \"a command Tcl
itself provides\", as against `function', which is a proc you wrote; builtin
versus function-name is the face pair that actually renders that contrast."
  :type '(alist :key-type string :value-type face)
  :group 'tcl-lsp)


;;; The minor mode
;;
;; Commands lsp-mode does not bind conveniently, plus live configuration
;; toggles.  Setting any of the `lsp-defcustom' variables below pushes a
;; `workspace/didChangeConfiguration'; the server reloads its command database
;; and re-publishes diagnostics for every open file, so changes are immediate.

(defvar tcl-lsp-mode-map
  (let ((map (make-sparse-keymap))
        (prefix (make-sparse-keymap)))
    ;; `C-c t', staying clear of lsp-mode's own `C-c l'.
    (define-key prefix (kbd "h") #'tcl-lsp-type-hierarchy)
    (define-key prefix (kbd "c") #'tcl-lsp-call-hierarchy)
    (define-key prefix (kbd "v") #'tcl-lsp-toggle-tcl-version)
    (define-key prefix (kbd "n") #'tcl-lsp-toggle-nagelfar)
    (define-key prefix (kbd "l") #'tcl-lsp-toggle-tclint)
    (define-key prefix (kbd "w") #'tcl-lsp-which-server)
    (define-key map (kbd "C-c t") prefix)
    map)
  "Keymap for `tcl-lsp-mode'.")

;;;###autoload
(define-minor-mode tcl-lsp-mode
  "Minor mode adding Tcl-specific language server commands.

\\{tcl-lsp-mode-map}"
  :lighter " TclLSP"
  :keymap tcl-lsp-mode-map
  :group 'tcl-lsp
  (when tcl-lsp-mode
    (tcl-lsp--apply-lsp-ui-presets)))

(defun tcl-lsp-sync-configuration ()
  "Push the current settings to every running tcl-lsp server.

Sent as one `workspace/didChangeConfiguration' carrying the whole `tclLsp'
object rather than a diff: the server merges partial updates, so sending
everything is both simpler and idempotent."
  (interactive)
  (when (fboundp 'lsp-workspaces)
    (dolist (workspace (tcl-lsp--all-workspaces))
      (ignore-errors
        (tcl-lsp--notify workspace "workspace/didChangeConfiguration"
                         `(:settings ,(tcl-lsp-initialization-options)))))))

(defun tcl-lsp--all-workspaces ()
  "Return every live tcl-lsp workspace."
  (when (fboundp 'lsp-session)
    (cl-remove-if-not
     (lambda (workspace)
       (eq 'tcl-lsp (lsp--client-server-id (lsp--workspace-client workspace))))
     (lsp--session-workspaces (lsp-session)))))

(defun tcl-lsp--set-and-report (symbol value description)
  "Set SYMBOL to VALUE, push the new configuration, and report DESCRIPTION."
  (set-default symbol value)
  (tcl-lsp-sync-configuration)
  (message "tcl-lsp: %s" description))

;;;###autoload
(defun tcl-lsp-toggle-tcl-version ()
  "Switch the analysis target between Tcl 8.6 and 9.0."
  (interactive)
  (let ((next (if (equal tcl-lsp-tcl-version "8.6") "9.0" "8.6")))
    (tcl-lsp--set-and-report
     'tcl-lsp-tcl-version next (format "analysing against Tcl %s" next))))

;;;###autoload
(defun tcl-lsp-toggle-nagelfar ()
  "Turn the nagelfar diagnostic backend on or off."
  (interactive)
  (let ((next (not tcl-lsp-nagelfar-enable)))
    (tcl-lsp--set-and-report
     'tcl-lsp-nagelfar-enable next
     (format "nagelfar %s" (if next "enabled" "disabled")))))

;;;###autoload
(defun tcl-lsp-toggle-tclint ()
  "Turn the tclint diagnostic backend on or off."
  (interactive)
  (let ((next (not tcl-lsp-tclint-enable)))
    (tcl-lsp--set-and-report
     'tcl-lsp-tclint-enable next
     (format "tclint %s" (if next "enabled" "disabled")))))

;;;###autoload
(defun tcl-lsp-type-hierarchy ()
  "Show the type hierarchy for the TclOO, itcl or snit class at point."
  (interactive)
  (if (fboundp 'lsp-treemacs-type-hierarchy)
      (call-interactively #'lsp-treemacs-type-hierarchy)
    (user-error "Type hierarchy needs `lsp-treemacs'")))

;;;###autoload
(defun tcl-lsp-call-hierarchy ()
  "Show the incoming call tree for the proc at point."
  (interactive)
  (if (fboundp 'lsp-treemacs-call-hierarchy)
      (call-interactively #'lsp-treemacs-call-hierarchy)
    (user-error "Call hierarchy needs `lsp-treemacs'")))


;;; lsp-mode client

(with-eval-after-load 'lsp-mode
  (require 'lsp-semantic-tokens nil t)

  (defvar lsp-language-id-configuration)
  (add-to-list 'lsp-language-id-configuration '(tcl-mode . "tcl"))

  (lsp-register-client
   (make-lsp-client
    ;; A function, not a string: resolved per buffer when the session starts,
    ;; which is what lets a direnv or envrc environment win.
    :new-connection (lsp-stdio-connection #'tcl-lsp-server-command
                                          #'tcl-lsp-server-available-p)
    :activation-fn (lsp-activate-on "tcl")
    :server-id 'tcl-lsp
    :priority 1
    :initialization-options #'tcl-lsp-initialization-options
    :action-handlers (let ((handlers (make-hash-table :test 'equal)))
                       (puthash "" #'tcl-lsp--code-lens-action handlers)
                       handlers)
    :semantic-tokens-faces-overrides
    (list :types tcl-lsp-semantic-token-face-overrides
          ;; Neutralise `declaration'; see `tcl-lsp-declaration-face'.
          :modifiers '(("declaration" . tcl-lsp-declaration-face)))))

  (add-hook 'lsp-after-initialize-hook
            (lambda ()
              (when-let* ((workspace (car (lsp-workspaces))))
                (tcl-lsp--start-watching workspace))))
  (add-hook 'lsp-after-uninitialized-functions #'tcl-lsp--stop-watching))


;;; Telling the three diagnostic sources apart
;;
;; Findings arrive from `tcl-lsp' itself, from nagelfar and from tclint, and they
;; differ in precision: tcl-lsp and tclint carry exact ranges, nagelfar only a
;; line.  Knowing which tool is complaining is what tells you which one to
;; silence.
;;
;; lsp-mode files the LSP `source' into flycheck's `group' slot and the `code'
;; into `id' (see `lsp-diagnostics--flycheck-start'); flycheck shows neither by
;; default, so flycheck users can reach it with `flycheck-error-group'.  Flymake
;; users get a documented hook, which the formatter below plugs into.

(defun tcl-lsp-flymake-message-formatter (diagnostic)
  "Format DIAGNOSTIC for flymake, prefixed with the tool that reported it.
Suitable as `lsp-diagnostics-flymake-message-formatter'."
  (let ((message (or (plist-get diagnostic :message) ""))
        (source (plist-get diagnostic :source?))
        (code (plist-get diagnostic :code?)))
    (concat (when source (format "[%s] " source))
            message
            (when code (format " (%s)" code)))))

;;; lsp-ui
;;
;; Off by default, and buffer-local when on.  Your `lsp-ui' settings are global
;; and deliberate; a Tcl client has no business changing how sidelines behave in
;; your Rust buffers, and little business overriding them in Tcl ones either.
;; Enable this only if you want the tweaks below in Tcl buffers specifically.

(defcustom tcl-lsp-apply-lsp-ui-presets nil
  "Whether to apply Tcl-specific `lsp-ui' tweaks in Tcl buffers.

When non-nil, `tcl-lsp-mode' sets these buffer-locally, leaving your global
`lsp-ui' configuration untouched:

  `lsp-ui-sideline-show-diagnostics'  on  — the point of the server
  `lsp-ui-sideline-show-code-actions' on  — quick fixes are worth surfacing
  `lsp-ui-sideline-show-hover'        off — nagelfar spans whole lines, so
                                            hover text in the sideline is noisy"
  :type 'boolean
  :group 'tcl-lsp)

(defun tcl-lsp--apply-lsp-ui-presets ()
  "Apply the `lsp-ui' tweaks buffer-locally, if enabled."
  (when (and tcl-lsp-apply-lsp-ui-presets (featurep 'lsp-ui))
    (setq-local lsp-ui-sideline-show-diagnostics t)
    (setq-local lsp-ui-sideline-show-code-actions t)
    (setq-local lsp-ui-sideline-show-hover nil)))


;;; eglot
;;
;; eglot 1.24 implements no semantic tokens, call hierarchy or type hierarchy, so
;; those capabilities are simply unavailable there.  Everything else — including
;; inlay hints — works.  Discovery and settings are shared with lsp-mode.

(with-eval-after-load 'eglot
  (defvar eglot-server-programs)
  (add-to-list 'eglot-server-programs
               `((tcl-mode) . ,(lambda (&rest _) (tcl-lsp-server-command)))))

;;;###autoload
(defun tcl-lsp-eglot-workspace-configuration (&rest _)
  "Return settings for eglot, in the shape `eglot-workspace-configuration' wants."
  (tcl-lsp-initialization-options))

;;;###autoload
(defun tcl-lsp-setup ()
  "Associate Tcl file extensions with `tcl-mode' and enable `tcl-lsp-mode'."
  (interactive)
  (tcl-lsp--register-auto-modes)
  (add-hook 'tcl-mode-hook #'tcl-lsp-mode))

;;;###autoload
(with-eval-after-load 'tcl
  (tcl-lsp--register-auto-modes))

(provide 'tcl-lsp)
;;; tcl-lsp.el ends here
