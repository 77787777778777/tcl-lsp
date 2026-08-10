;;; tcl-lsp-tests.el --- Tests for tcl-lsp.el -*- lexical-binding: t; -*-

;;; Commentary:

;; Two tiers.  The unit tests need nothing but this file and run everywhere.
;; The integration test drives a real `tcl-lsp' process and is skipped unless
;; one is on PATH, which `checks.emacs' arranges.
;;
;; Integration goes through eglot rather than lsp-mode on purpose: eglot's
;; request path is synchronous (`jsonrpc-request'), so a headless test can
;; assert on real responses without polling or a deferred library.  lsp-mode is
;; covered by asserting the pieces a session depends on, which is the honest
;; limit of what is deterministic in batch mode.

;;; Code:

(require 'ert)
(require 'json)
(require 'project)
(require 'tcl-lsp)

;;; Discovery

(ert-deftest tcl-lsp-test-explicit-path-wins ()
  "An explicit setting beats anything on PATH."
  (let ((tcl-lsp-server-path "/explicit/tcl-lsp"))
    (should (equal (tcl-lsp-server-program) "/explicit/tcl-lsp"))))

(ert-deftest tcl-lsp-test-exec-path-beats-bundled ()
  "A server on PATH wins over the bundled one.
This is what lets a project's direnv environment override the package."
  (let* ((dir (make-temp-file "tcl-lsp-test" t))
         (fake (expand-file-name "tcl-lsp" dir)))
    (unwind-protect
        (progn
          (with-temp-file fake (insert "#!/bin/sh\n"))
          (set-file-modes fake #o755)
          (let ((exec-path (cons dir exec-path))
                (tcl-lsp-server-path nil)
                (tcl-lsp-bundled-server-path "/bundled/tcl-lsp"))
            (should (equal (tcl-lsp-server-program) fake))))
      (delete-directory dir t))))

(ert-deftest tcl-lsp-test-falls-back-to-bundled ()
  "With nothing on PATH, the bundled server is used — but only if it exists."
  (let ((exec-path nil)
        (tcl-lsp-server-path nil))
    (let ((tcl-lsp-bundled-server-path "/definitely/not/here/tcl-lsp"))
      (should-not (tcl-lsp-server-program)))
    (let ((tcl-lsp-bundled-server-path (executable-find "sh")))
      (should (equal (tcl-lsp-server-program) (executable-find "sh"))))))

(ert-deftest tcl-lsp-test-missing-server-is-a-clear-error ()
  "A missing server produces an actionable message, not a cryptic one."
  (let ((exec-path nil)
        (tcl-lsp-server-path nil)
        (tcl-lsp-bundled-server-path nil))
    (should-not (tcl-lsp-server-available-p))
    (let ((err (should-error (tcl-lsp-server-command) :type 'user-error)))
      (should (string-match-p "direnv" (error-message-string err))))))

;;; Settings

(defun tcl-lsp-test--json (object)
  "Encode OBJECT the way it would go on the wire."
  (json-encode object))

(ert-deftest tcl-lsp-test-unconfigured-settings-say-nothing ()
  "Out of the box the client sends no settings at all.

This is the important one.  `initializationOptions' outranks the environment,
so any key sent unconditionally would override `TCL_LSP_*' from the user's
shell, their .envrc, or the paths the Nix wrapper baked in."
  (let ((tcl-lsp-tcl-version nil)
        (tcl-lsp-nagelfar-enable nil) (tcl-lsp-nagelfar-path nil)
        (tcl-lsp-nagelfar-syntax-db nil)
        (tcl-lsp-tclint-enable nil) (tcl-lsp-tclint-path nil)
        (tcl-lsp-tclfmt-enable nil) (tcl-lsp-tclfmt-path nil))
    (should (null (tcl-lsp-settings)))
    (should (null (tcl-lsp-initialization-options)))))

(ert-deftest tcl-lsp-test-enabled-tool-is-true ()
  "Turning a tool on sends JSON true."
  (let* ((tcl-lsp-nagelfar-enable t)
         (json (tcl-lsp-test--json (tcl-lsp-settings))))
    (should (string-match-p "\"nagelfar\":true" json))))

(ert-deftest tcl-lsp-test-disabled-tool-is-false ()
  "Turning a tool off sends JSON false.
`:json-false' rather than nil, which would omit the key and mean the
opposite."
  (let* ((tcl-lsp-nagelfar-enable :json-false)
         (json (tcl-lsp-test--json (tcl-lsp-settings))))
    (should (string-match-p "\"nagelfar\":false" json))))

(ert-deftest tcl-lsp-test-version-is-sent-when-set ()
  "An explicit Tcl version reaches the server."
  (let* ((tcl-lsp-tcl-version "9.0")
         (json (tcl-lsp-test--json (tcl-lsp-settings))))
    (should (string-match-p "\"tclVersion\":\"9.0\"" json))))

(ert-deftest tcl-lsp-test-tool-with-path-becomes-an-object ()
  "Setting a path switches that tool to the object form the server accepts."
  (let* ((tcl-lsp-tclint-path "/usr/bin/tclint")
         (tcl-lsp-tclint-enable t)
         (json (tcl-lsp-test--json (tcl-lsp-settings))))
    (should (string-match-p "\"tclint\":{" json))
    (should (string-match-p "\"path\":\"/usr/bin/tclint\"" json))
    (should (string-match-p "\"enable\":true" json))))

(ert-deftest tcl-lsp-test-nagelfar-syntax-db-is-nested ()
  "The syntax database is only read from nagelfar's object form."
  (let* ((tcl-lsp-nagelfar-syntax-db "/db/syntaxdb90.tcl")
         (json (tcl-lsp-test--json (tcl-lsp-settings))))
    (should (string-match-p "\"syntaxDb\":\"/db/syntaxdb90.tcl\"" json))))

(ert-deftest tcl-lsp-test-initialization-options-are-wrapped ()
  "Settings go out under a `tclLsp' key."
  (let* ((tcl-lsp-tcl-version "8.6")
         (json (tcl-lsp-test--json (tcl-lsp-initialization-options))))
    (should (string-match-p "\"tclLsp\":{" json))))

;;; Activation

(ert-deftest tcl-lsp-test-claims-tm-and-itcl-but-not-test ()
  "The .test extension is left alone unless explicitly opted into.
It is claimed by many unrelated ecosystems."
  (let ((auto-mode-alist nil))
    (tcl-lsp--register-auto-modes)
    (should (eq 'tcl-mode (cdr (assoc "\\.tm\\'" auto-mode-alist))))
    (should (eq 'tcl-mode (cdr (assoc "\\.itcl\\'" auto-mode-alist))))
    (should-not (assoc "\\.test\\'" auto-mode-alist)))
  (let ((auto-mode-alist nil)
        (tcl-lsp-claim-test-extension t))
    (tcl-lsp--register-auto-modes)
    (should (eq 'tcl-mode (cdr (assoc "\\.test\\'" auto-mode-alist))))))

(ert-deftest tcl-lsp-test-recognises-indexed-extensions ()
  "The watcher notices every extension the server indexes, including .test.
The server indexes .test even when Emacs does not open it in `tcl-mode'."
  (dolist (file '("a.tcl" "b.tm" "c.test" "d.itcl"))
    (should (tcl-lsp--tcl-file-p file)))
  (should-not (tcl-lsp--tcl-file-p "e.rs"))
  (should-not (tcl-lsp--tcl-file-p nil)))

;;; Diagnostic formatting

(ert-deftest tcl-lsp-test-flymake-formatter-names-the-source ()
  "Findings are attributable to the tool that produced them."
  (should (equal (tcl-lsp-flymake-message-formatter
                  '(:message "Unknown variable \"nam\"" :source? "nagelfar"))
                 "[nagelfar] Unknown variable \"nam\""))
  (should (equal (tcl-lsp-flymake-message-formatter
                  '(:message "expression is not braced"
                    :source? "tcl-lsp" :code? "unbraced-expr"))
                 "[tcl-lsp] expression is not braced (unbraced-expr)"))
  (should (equal (tcl-lsp-flymake-message-formatter '(:message "bare"))
                 "bare")))

;;; lsp-mode wiring
;;
;; Asserting the pieces rather than driving a session: lsp-mode's startup is
;; asynchronous and its own integration tests need `deferred', which is not
;; worth pulling in here.

(ert-deftest tcl-lsp-test-lsp-client-is-registered ()
  "The client registers itself, claims Tcl buffers, and binds the empty command."
  (skip-unless (require 'lsp-mode nil t))
  (let ((client (gethash 'tcl-lsp lsp-clients)))
    (should client)
    (should (equal "tcl" (cdr (assoc 'tcl-mode lsp-language-id-configuration))))
    ;; The server's reference lens carries an empty command; without a handler
    ;; for it lsp-mode would fall through to workspace/executeCommand, which
    ;; this server does not implement.
    (should (gethash "" (lsp--client-action-handlers client)))))

(ert-deftest tcl-lsp-test-connection-command-is-lazy ()
  "The command is a function, so it resolves per buffer.
A string would be resolved when the client is registered, before any
buffer-local direnv environment exists."
  (skip-unless (require 'lsp-mode nil t))
  (should (functionp #'tcl-lsp-server-command)))

(ert-deftest tcl-lsp-test-outranks-an-existing-tcl-client ()
  "This client wins when another is already registered for Tcl.
Config that predates this package commonly registers a Tcl server of its
own, and `lsp--find-clients' keeps exactly one non-add-on client: the
highest priority.  Ours declares 1 against the struct default of 0, so it
is chosen without the user having to remove anything."
  (skip-unless (require 'lsp-mode nil t))
  (let ((lsp-clients (copy-hash-table lsp-clients)))
    (lsp-register-client
     (make-lsp-client
      :new-connection (lsp-stdio-connection "true")
      :activation-fn (lsp-activate-on "tcl")
      :server-id 'tcl-lsp-test-rival))
    (let* ((clients (list (gethash 'tcl-lsp-test-rival lsp-clients)
                          (gethash 'tcl-lsp lsp-clients)))
           (winner (car (sort clients
                              (lambda (a b)
                                (> (lsp--client-priority a)
                                   (lsp--client-priority b)))))))
      (should (eq 'tcl-lsp (lsp--client-server-id winner))))))

;;; Integration, through eglot

(defvar tcl-lsp-test--workspace nil)

(defun tcl-lsp-test--make-workspace ()
  "Create a throwaway Tcl project and return its directory."
  (let ((dir (make-temp-file "tcl-lsp-ws" t)))
    (with-temp-file (expand-file-name "lib.tcl" dir)
      (insert "package provide testlib 1.0\n"
              "# Trims whitespace.\n"
              "namespace eval util {\n"
              "    proc trim {s} { string trim $s }\n"
              "}\n"))
    (with-temp-file (expand-file-name "main.tcl" dir)
      (insert "util::trim hello\n"
              "set c [expr $a + $b]\n"))
    ;; A root marker, so the server indexes the whole directory.
    (with-temp-file (expand-file-name "pkgIndex.tcl" dir) (insert "\n"))
    dir))

(defmacro tcl-lsp-test--with-session (file &rest body)
  "Open FILE in a temporary workspace with eglot running, then evaluate BODY."
  (declare (indent 1))
  `(progn
     (skip-unless (and (tcl-lsp-server-available-p) (require 'eglot nil t)))
     (let* ((dir (tcl-lsp-test--make-workspace))
            (default-directory dir)
            (buffer (find-file-noselect (expand-file-name ,file dir))))
       (unwind-protect
           (with-current-buffer buffer
             (tcl-mode)
             (let ((eglot-sync-connect 30)
                   (eglot-autoshutdown t))
               ;; The public entry point, so this does not depend on eglot's
               ;; internal connect arity.
               (eglot '(tcl-mode)
                      (cons 'transient dir)
                      'eglot-lsp-server
                      (tcl-lsp-server-command)
                      '("tcl")
                      t)
               (should (eglot-current-server))
               ,@body))
         (ignore-errors
           (with-current-buffer buffer
             (when (eglot-current-server) (eglot-shutdown (eglot-current-server)))))
         (kill-buffer buffer)
         (delete-directory dir t)))))

(ert-deftest tcl-lsp-test-server-starts-and-reports-itself ()
  "A real session comes up and identifies as tcl-lsp."
  (tcl-lsp-test--with-session "main.tcl"
    (let ((info (eglot--server-info (eglot-current-server))))
      (should (equal "tcl-lsp" (plist-get info :name))))))

(ert-deftest tcl-lsp-test-diagnostics-arrive ()
  "Diagnostics for the fixture reach the client.

Asserted at the notification, not through flymake: flymake decides when to run
on its own schedule, which batch mode does not drive, so going through it would
test Emacs's timers rather than this client.  Whether the server produces the
right diagnostics is already covered by the Tcl end-to-end suite."
  (let (published)
    (advice-add
     'eglot-handle-notification :before
     (lambda (_server method &rest args)
       ;; eglot passes the method as a plain symbol; accept the keyword too so
       ;; this does not depend on that detail.
       (when (memq method '(textDocument/publishDiagnostics
                            :textDocument/publishDiagnostics))
         (push (plist-get args :diagnostics) published)))
     '((name . tcl-lsp-test-capture)))
    (unwind-protect
        (tcl-lsp-test--with-session "main.tcl"
          (let ((deadline (+ (float-time) 20)))
            (while (and (null published) (< (float-time) deadline))
              (accept-process-output nil 0.1)))
          (should published)
          ;; `set c [expr $a + $b]' in the fixture: the server flags the
          ;; unbraced expression, which is its own diagnostic rather than a
          ;; parse error.
          (should (string-match-p "braced" (format "%s" published))))
      (advice-remove 'eglot-handle-notification 'tcl-lsp-test-capture))))

(ert-deftest tcl-lsp-test-definition-crosses-files ()
  "Definition resolves into a file that was never opened.
This only works if the workspace root reached the server, since it indexes
exactly once at startup."
  (tcl-lsp-test--with-session "main.tcl"
    (goto-char (point-min))
    (search-forward "trim")
    (let ((locations (eglot--request (eglot-current-server)
                                     :textDocument/definition
                                     (eglot--TextDocumentPositionParams))))
      (should locations)
      (should (string-match-p
               "lib\\.tcl"
               (format "%s" locations))))))

(ert-deftest tcl-lsp-test-completion-offers-builtins-and-workspace ()
  "Completion includes both a workspace proc and a Tcl builtin."
  (tcl-lsp-test--with-session "main.tcl"
    (goto-char (point-max))
    (insert "\n")
    (let* ((response (eglot--request (eglot-current-server)
                                     :textDocument/completion
                                     (eglot--TextDocumentPositionParams)))
           (text (format "%s" response)))
      (should (string-match-p "trim" text))
      (should (string-match-p "lsort" text)))))

(provide 'tcl-lsp-tests)
;;; tcl-lsp-tests.el ends here
