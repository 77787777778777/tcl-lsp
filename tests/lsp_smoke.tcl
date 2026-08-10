#!/usr/bin/env tclsh
# End-to-end smoke test: drives tcl-lsp over real stdio JSON-RPC and checks the
# responses. Deliberately written in Tcl so it exercises the server the way an
# editor would, with no Rust test harness in between.

set bin [lindex $argv 0]

proc send {ch obj} {
    # Content-Length is a BYTE count, not a character count.
    set payload [encoding convertto utf-8 $obj]
    puts -nonewline $ch "Content-Length: [string length $payload]\r\n\r\n$payload"
    flush $ch
}

proc recv {ch} {
    set len 0
    while {[gets $ch line] >= 0} {
        set line [string trimright $line "\r"]
        if {$line eq ""} break
        if {[regexp {^Content-Length:\s*(\d+)$} $line -> n]} { set len $n }
    }
    if {$len == 0} { return "" }
    return [encoding convertfrom utf-8 [read $ch $len]]
}

# A throwaway workspace, so the initial scan has something real to index and
# cross-file navigation can be exercised.
set ws [file normalize [file join [pwd] tcl-lsp-e2e-ws]]
file mkdir $ws
set f [open [file join $ws lib.tcl] w]
puts $f "package provide utillib 1.0\n# Trims whitespace.\nnamespace eval util {\n    proc trim {s} { string trim \$s }\n}"
close $f

# A second file that links to the first, for documentLink.
set f [open [file join $ws main.tcl] w]
puts $f "source lib.tcl\npackage require utillib\nutil::trim x"
close $f

set srv [open "|$bin 2>/dev/null" r+]
fconfigure $srv -translation binary -encoding binary -blocking 1

# --- initialize, declaring UTF-8 support so the server should negotiate it
send $srv "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"processId\":null,\"rootUri\":\"file://$ws\",\"capabilities\":{\"general\":{\"positionEncodings\":\[\"utf-8\",\"utf-16\"\]}}}}"
set init [recv $srv]
if {![string match {*"positionEncoding":"utf-8"*} $init]} {
    puts "FAIL: expected utf-8 encoding negotiation, got: $init"
    exit 1
}
puts "ok  negotiated utf-8 position encoding"
foreach cap {documentSymbolProvider definitionProvider hoverProvider workspaceSymbolProvider
             documentFormattingProvider referencesProvider documentHighlightProvider
             foldingRangeProvider completionProvider selectionRangeProvider
             documentLinkProvider signatureHelpProvider renameProvider
             semanticTokensProvider inlayHintProvider} {
    if {![string match "*$cap*" $init]} { puts "FAIL: missing capability $cap"; exit 1 }
}
puts "ok  advertises the full capability set"

send $srv {{"jsonrpc":"2.0","method":"initialized","params":{}}}

# --- open a document containing a namespace, a proc, and a TclOO class
set src "# Greets a person.\nnamespace eval util {\n    proc greet {name} {\n        puts \"hello \$name\"\n    }\n}\noo::class create Shape {\n    method area {} {}\n}\n"
set doc [string map [list \n \\n \" \\\"] $src]
send $srv "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didOpen\",\"params\":{\"textDocument\":{\"uri\":\"file:///t.tcl\",\"languageId\":\"tcl\",\"version\":1,\"text\":\"$doc\"}}}"

# diagnostics notification for a clean file
set diag [recv $srv]
if {![string match {*publishDiagnostics*} $diag]} { puts "FAIL: no diagnostics notification"; exit 1 }
if {![string match {*"diagnostics":\[\]*} $diag]} { puts "FAIL: clean file produced diagnostics: $diag" ; exit 1 }
puts "ok  clean file yields no diagnostics"

# --- documentSymbol
send $srv {{"jsonrpc":"2.0","id":2,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":"file:///t.tcl"}}}}
set syms [recv $srv]
foreach want {util greet Shape area} {
    if {![string match "*\"$want\"*" $syms]} { puts "FAIL: documentSymbol missing $want"; exit 1 }
}
puts "ok  documentSymbol reports namespace, proc, class and method"

# --- hover over `greet` should show its qualified name and the doc comment
send $srv {{"jsonrpc":"2.0","id":3,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///t.tcl"},"position":{"line":2,"character":10}}}}
set hov [recv $srv]
if {![string match {*::util::greet*} $hov]} { puts "FAIL: hover lacks qualified name: $hov"; exit 1 }
puts "ok  hover resolves the namespace-qualified name"

# --- go to definition of `greet`
send $srv {{"jsonrpc":"2.0","id":4,"method":"textDocument/definition","params":{"textDocument":{"uri":"file:///t.tcl"},"position":{"line":2,"character":10}}}}
set def [recv $srv]
if {![string match {*file:///t.tcl*} $def]} { puts "FAIL: definition not found: $def"; exit 1 }
puts "ok  definition resolves"

# --- introduce a syntax error and confirm diagnostics appear from the BUFFER
# Quoted with "" rather than {} because the payload's braces are deliberately
# unbalanced -- that is the whole point of the test.
set broken "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didChange\",\"params\":{\"textDocument\":{\"uri\":\"file:///t.tcl\",\"version\":2},\"contentChanges\":\[{\"text\":\"proc broken {} {\\nputs oops\\n\"}\]}}"
send $srv $broken
set diag2 [recv $srv]
if {[string match {*"diagnostics":\[\]*} $diag2]} { puts "FAIL: broken buffer produced no diagnostics: $diag2"; exit 1 }
puts "ok  unterminated brace reported from the unsaved buffer"

# --- restore a good buffer for the remaining feature checks
set src2 "namespace eval util {\n    proc trim {s} {}\n}\nproc caller {} {\n    util::trim x\n    util::trim y\n}\n"
set doc2 [string map [list \n \\n \" \\\"] $src2]
send $srv "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didChange\",\"params\":{\"textDocument\":{\"uri\":\"file:///t.tcl\",\"version\":3},\"contentChanges\":\[{\"text\":\"$doc2\"}\]}}"
recv $srv

# --- references: two call sites of util::trim
send $srv {{"jsonrpc":"2.0","id":5,"method":"textDocument/references","params":{"textDocument":{"uri":"file:///t.tcl"},"position":{"line":4,"character":12},"context":{"includeDeclaration":false}}}}
set refs [recv $srv]
set n [regexp -all {"uri"} $refs]
if {$n < 2} { puts "FAIL: expected >=2 references, got $n: $refs"; exit 1 }
puts "ok  references finds both call sites"

# --- documentHighlight
send $srv {{"jsonrpc":"2.0","id":6,"method":"textDocument/documentHighlight","params":{"textDocument":{"uri":"file:///t.tcl"},"position":{"line":4,"character":12}}}}
set hl [recv $srv]
if {![string match {*"range"*} $hl]} { puts "FAIL: no highlights: $hl"; exit 1 }
puts "ok  documentHighlight returns ranges"

# --- foldingRange
send $srv {{"jsonrpc":"2.0","id":7,"method":"textDocument/foldingRange","params":{"textDocument":{"uri":"file:///t.tcl"}}}}
set fold [recv $srv]
if {![string match {*startLine*} $fold]} { puts "FAIL: no folding ranges: $fold"; exit 1 }
puts "ok  foldingRange covers proc and namespace bodies"

# --- completion offers workspace procs
send $srv {{"jsonrpc":"2.0","id":8,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///t.tcl"},"position":{"line":5,"character":4}}}}
set comp [recv $srv]
if {![string match {*"trim"*} $comp]} { puts "FAIL: completion missing trim: $comp"; exit 1 }
puts "ok  completion offers workspace procs"

# --- workspace/symbol reaches the file that was only ever scanned from disk
send $srv {{"jsonrpc":"2.0","id":10,"method":"workspace/symbol","params":{"query":"trim"}}}
set wsym [recv $srv]
if {![string match {*lib.tcl*} $wsym]} {
    puts "FAIL: workspace symbols did not include the unopened lib.tcl: $wsym"; exit 1
}
puts "ok  workspace scan indexed a file that was never opened"

# --- selectionRange: expanding from a word outwards
send $srv {{"jsonrpc":"2.0","id":13,"method":"textDocument/selectionRange","params":{"textDocument":{"uri":"file:///t.tcl"},"positions":[{"line":4,"character":14}]}}}
set sel [recv $srv]
if {![string match {*"parent"*} $sel]} {
    puts "FAIL: selectionRange returned no nesting: $sel"; exit 1
}
puts "ok  selectionRange nests word inside command inside body"

# --- signatureHelp for a user-defined proc
send $srv {{"jsonrpc":"2.0","id":14,"method":"textDocument/signatureHelp","params":{"textDocument":{"uri":"file:///t.tcl"},"position":{"line":4,"character":15}}}}
set sig [recv $srv]
if {![string match {*::util::trim*} $sig]} {
    puts "FAIL: no signature for util::trim: $sig"; exit 1
}
if {![string match {*activeParameter*} $sig]} {
    puts "FAIL: signature help lacks an active parameter: $sig"; exit 1
}
puts "ok  signatureHelp shows a user proc's parameters"

# --- documentLink on a real file: `source` and `package require`
set mainuri "file://$ws/main.tcl"
set mainsrc [string map [list \n \\n \" \\\"] "source lib.tcl\npackage require utillib\nutil::trim x\n"]
send $srv "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didOpen\",\"params\":{\"textDocument\":{\"uri\":\"$mainuri\",\"languageId\":\"tcl\",\"version\":1,\"text\":\"$mainsrc\"}}}"
recv $srv
send $srv "{\"jsonrpc\":\"2.0\",\"id\":16,\"method\":\"textDocument/documentLink\",\"params\":{\"textDocument\":{\"uri\":\"$mainuri\"}}}"
set links [recv $srv]
if {![string match {*lib.tcl*} $links]} {
    puts "FAIL: no documentLink for `source lib.tcl`: $links"; exit 1
}
set nlinks [regexp -all {"target"} $links]
if {$nlinks < 2} {
    puts "FAIL: expected links for both source and package require, got $nlinks: $links"; exit 1
}
puts "ok  documentLink resolves source paths and package require"

# --- prepareRename offers just the final segment of a qualified name
send $srv {{"jsonrpc":"2.0","id":18,"method":"textDocument/prepareRename","params":{"textDocument":{"uri":"file:///t.tcl"},"position":{"line":4,"character":12}}}}
set prep [recv $srv]
if {![string match {*"placeholder":"trim"*} $prep]} {
    puts "FAIL: prepareRename should offer 'trim', got: $prep"; exit 1
}
puts "ok  prepareRename targets the last segment of a qualified name"

# --- rename rewrites the definition and every call site
send $srv {{"jsonrpc":"2.0","id":19,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///t.tcl"},"position":{"line":4,"character":12},"newName":"strip"}}}
set ren [recv $srv]
if {![string match {*"newText":"strip"*} $ren]} {
    puts "FAIL: rename produced no edits: $ren"; exit 1
}
set nedits [regexp -all {"newText"} $ren]
if {$nedits < 3} {
    puts "FAIL: expected the definition plus two call sites, got $nedits: $ren"; exit 1
}
puts "ok  rename rewrites the definition and all call sites"

# --- rename refuses a name that would change the meaning of call sites
send $srv {{"jsonrpc":"2.0","id":20,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///t.tcl"},"position":{"line":4,"character":12},"newName":"bad name"}}}
set badren [recv $srv]
if {![string match {*"result":null*} $badren]} {
    puts "FAIL: rename should refuse a name with whitespace: $badren"; exit 1
}
puts "ok  rename refuses a name containing whitespace"

# --- semanticTokens
send $srv {{"jsonrpc":"2.0","id":21,"method":"textDocument/semanticTokens/full","params":{"textDocument":{"uri":"file:///t.tcl"}}}}
set sem [recv $srv]
if {![string match {*"data"*} $sem]} { puts "FAIL: no semantic tokens: $sem"; exit 1 }
# Five integers per token, so the array length must be a multiple of five.
if {![regexp {"data":\[([^\]]*)\]} $sem -> nums]} {
    puts "FAIL: could not read the token array: $sem"; exit 1
}
set count [llength [split $nums ,]]
if {$count == 0 || $count % 5 != 0} {
    puts "FAIL: token array length $count is not a multiple of 5"; exit 1
}
puts "ok  semanticTokens returns a well-formed delta-encoded array"

# --- inlayHint shows parameter names at call sites
send $srv {{"jsonrpc":"2.0","id":22,"method":"textDocument/inlayHint","params":{"textDocument":{"uri":"file:///t.tcl"},"range":{"start":{"line":0,"character":0},"end":{"line":10,"character":0}}}}}
set hints [recv $srv]
if {![string match {*"s:"*} $hints]} {
    puts "FAIL: expected a parameter hint 's:' for util::trim, got: $hints"; exit 1
}
puts "ok  inlayHint labels arguments with parameter names"

# --- unbraced expr is diagnosed, and offered a quick fix
set exprsrc "set a 1\nset b 2\nset c \[expr \$a + \$b\]\n"
set exprdoc [string map [list \n \\n \" \\\"] $exprsrc]
send $srv "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didOpen\",\"params\":{\"textDocument\":{\"uri\":\"file:///expr.tcl\",\"languageId\":\"tcl\",\"version\":1,\"text\":\"$exprdoc\"}}}"
set ediag [recv $srv]
if {![string match {*unbraced-expr*} $ediag]} {
    puts "FAIL: unbraced expr not diagnosed: $ediag"; exit 1
}
puts "ok  unbraced expr is diagnosed"

send $srv {{"jsonrpc":"2.0","id":23,"method":"textDocument/codeAction","params":{"textDocument":{"uri":"file:///expr.tcl"},"range":{"start":{"line":2,"character":13},"end":{"line":2,"character":13}},"context":{"diagnostics":[]}}}}
set ca [recv $srv]
if {![string match {*Brace the expression*} $ca]} {
    puts "FAIL: no brace-the-expression quick fix: $ca"; exit 1
}
if {![string match {*\{$a + $b\}*} $ca]} {
    puts "FAIL: quick fix did not wrap the whole expression: $ca"; exit 1
}
puts "ok  codeAction offers to brace the expression"

# --- Tk widget option completion, from the man pages' .OP entries
set tksrc "ttk::button .b -\n"
set tkdoc [string map [list \n \\n \" \\\"] $tksrc]
send $srv "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didOpen\",\"params\":{\"textDocument\":{\"uri\":\"file:///tk.tcl\",\"languageId\":\"tcl\",\"version\":1,\"text\":\"$tkdoc\"}}}"
recv $srv
send $srv {{"jsonrpc":"2.0","id":24,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///tk.tcl"},"position":{"line":0,"character":16}}}}
set tkcomp [recv $srv]
if {![string match {*"-command"*} $tkcomp]} {
    puts "FAIL: no Tk option completion for ttk::button: $tkcomp"; exit 1
}
puts "ok  completion offers Tk widget options"

# --- hover over a Tcl builtin comes from the generated command database
set bsrc "lsort \[list 3 1 2\]\n"
set bdoc2 [string map [list \n \\n \" \\\"] $bsrc]
send $srv "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didOpen\",\"params\":{\"textDocument\":{\"uri\":\"file:///builtin.tcl\",\"languageId\":\"tcl\",\"version\":1,\"text\":\"$bdoc2\"}}}"
recv $srv
send $srv {{"jsonrpc":"2.0","id":11,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///builtin.tcl"},"position":{"line":0,"character":2}}}}
set bhov [recv $srv]
if {![string match {*Sort the elements of a list*} $bhov]} {
    puts "FAIL: no builtin documentation for lsort: $bhov"; exit 1
}
puts "ok  hover documents Tcl builtins from the man-page database"

send $srv {{"jsonrpc":"2.0","id":12,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///builtin.tcl"},"position":{"line":0,"character":5}}}}
set bcomp [recv $srv]
foreach want {lsort foreach string} {
    if {![string match "*\"$want\"*" $bcomp]} {
        puts "FAIL: completion missing builtin $want"; exit 1
    }
}
puts "ok  completion offers Tcl builtins"

# --- signatureHelp for a builtin comes from the man-page synopsis
send $srv {{"jsonrpc":"2.0","id":17,"method":"textDocument/signatureHelp","params":{"textDocument":{"uri":"file:///builtin.tcl"},"position":{"line":0,"character":6}}}}
set bsig [recv $srv]
if {![string match {*lsort*} $bsig]} {
    puts "FAIL: no builtin signature for lsort: $bsig"; exit 1
}
puts "ok  signatureHelp documents builtins from their synopsis"

# --- external analysers, if they are configured. Opening a file runs them; a
# typo'd variable is something only nagelfar catches, not our own parser.
if {[info exists ::env(TCL_LSP_NAGELFAR)]} {
    set bad "proc greet {name} {\n    puts \"hi \$nam\"\n}\n"
    set bdoc [string map [list \n \\n \" \\\"] $bad]
    send $srv "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/didOpen\",\"params\":{\"textDocument\":{\"uri\":\"file:///bad.tcl\",\"languageId\":\"tcl\",\"version\":1,\"text\":\"$bdoc\"}}}"
    set bdiag [recv $srv]
    if {![string match {*nagelfar*} $bdiag]} {
        puts "FAIL: no nagelfar-sourced diagnostic: $bdiag"; exit 1
    }
    if {![string match {*Unknown variable*} $bdiag]} {
        puts "FAIL: nagelfar did not report the unknown variable: $bdiag"; exit 1
    }
    puts "ok  nagelfar diagnostics surface through publishDiagnostics"
} else {
    puts "--  skipped external analyser check (TCL_LSP_NAGELFAR unset)"
}

# --- didChangeConfiguration takes effect: switching off nagelfar must clear
# the diagnostics it produced, without disturbing our own.
if {[info exists ::env(TCL_LSP_NAGELFAR)]} {
    send $srv {{"jsonrpc":"2.0","method":"workspace/didChangeConfiguration","params":{"settings":{"tclLsp":{"diagnostics":{"nagelfar":false,"tclint":false}}}}}}
    # The server republishes every open document; find the one for bad.tcl.
    set seen 0
    for {set i 0} {$i < 8} {incr i} {
        set msg [recv $srv]
        if {[string match {*bad.tcl*} $msg]} {
            if {[string match {*nagelfar*} $msg]} {
                puts "FAIL: nagelfar diagnostics survived being switched off: $msg"; exit 1
            }
            set seen 1
            break
        }
    }
    if {!$seen} { puts "FAIL: no republished diagnostics after config change"; exit 1 }
    puts "ok  didChangeConfiguration disables a diagnostic backend"
}

send $srv {{"jsonrpc":"2.0","id":9,"method":"shutdown","params":null}}
recv $srv
send $srv {{"jsonrpc":"2.0","method":"exit","params":null}}
catch {close $srv}
file delete -force $ws
puts "ALL CHECKS PASSED"
