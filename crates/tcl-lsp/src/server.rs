//! Server state and request dispatch.

use std::collections::HashMap;
use std::ops::Range;

use anyhow::Result;
use lsp_server::{Connection, ExtractError, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    notification::{
        DidChangeConfiguration, DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument,
        DidOpenTextDocument, DidSaveTextDocument, Notification as _, PublishDiagnostics,
    },
    request::{
        CallHierarchyIncomingCalls, CallHierarchyOutgoingCalls, CallHierarchyPrepare,
        CodeActionRequest, CodeLensRequest, Completion, DocumentHighlightRequest,
        DocumentLinkRequest, DocumentSymbolRequest, FoldingRangeRequest, Formatting,
        GotoDefinition, HoverRequest, InlayHintRequest, OnTypeFormatting, PrepareRenameRequest,
        References, Rename, Request as _, SelectionRangeRequest, SemanticTokensFullRequest,
        SemanticTokensRangeRequest, SignatureHelpRequest, TypeHierarchyPrepare,
        TypeHierarchySubtypes, TypeHierarchySupertypes, WorkspaceSymbolRequest,
    },
    *,
};
use tcl_analysis::{kb, uri_to_path, Index};
use tcl_syntax::{Document, LineIndex, LinePos, PositionEncoding, Symbol, SymbolKind as TclKind};

use crate::config::Config;
use crate::external;

pub struct Server {
    /// Buffers the editor has open. These override whatever is on disk.
    docs: HashMap<String, Document>,
    index: Index,
    /// Documentation for Tcl and Tk own commands.
    kb: kb::Kb,
    config: Config,
    encoding: PositionEncoding,
    /// Project syntax database (nagelfar `-header` of the workspace's Tcl
    /// files), layered under the builtin db so cross-file procs resolve.
    project_db: Option<String>,
    /// Host-application command database (veles::call_llm and friends),
    /// layered under the project db so plugin code is not flagged for
    /// calling the API it runs against.
    host_db: Option<String>,
}

pub fn run(connection: &Connection) -> Result<()> {
    let (id, params) = connection.initialize_start()?;
    let params: InitializeParams = serde_json::from_value(params)?;

    // Position-encoding negotiation. Prefer UTF-8 (no conversion at all), but only
    // if the client offered it; the protocol's default is UTF-16 and assuming
    // otherwise silently misplaces every edit in a document with non-ASCII text.
    let offered = params
        .capabilities
        .general
        .as_ref()
        .and_then(|g| g.position_encodings.as_ref());
    let encoding = match offered {
        Some(list) if list.contains(&PositionEncodingKind::UTF8) => PositionEncoding::Utf8,
        _ => PositionEncoding::Utf16,
    };
    eprintln!("negotiated position encoding: {encoding:?}");

    let roots = workspace_roots(&params);

    let init_result = InitializeResult {
        capabilities: capabilities(encoding),
        server_info: Some(ServerInfo {
            name: "tcl-lsp".to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
    };
    // lsp-types 0.97 models every type-hierarchy request but omits
    // `typeHierarchyProvider` from ServerCapabilities, so the key is added to the
    // serialized value directly. Clients look for exactly this name.
    let mut init_value = serde_json::to_value(init_result)?;
    if let Some(caps) = init_value
        .get_mut("capabilities")
        .and_then(|c| c.as_object_mut())
    {
        caps.insert(
            "typeHierarchyProvider".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    connection.initialize_finish(id, init_value)?;

    // Environment first, then whatever the client sent with `initialize`.
    let mut config = Config::default();
    if let Some(opts) = params.initialization_options.as_ref() {
        config.merge(opts);
    }
    let kb = kb::builtin(config.tcl_version);
    eprintln!(
        "loaded {} builtin commands for {:?}",
        kb.len(),
        config.tcl_version
    );

    // Index the workspace up front so definition and references work in files the
    // user has not opened yet. The same file list builds the project syntax
    // database that nags nagelfar out of its "unknown command" noise.
    let files = collect_tcl_files(&roots);
    let project_db = match config.nagelfar.active() {
        Some(exe) => {
            let dir = std::env::temp_dir().join(format!("tcl-lsp-db-{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok();
            let db = dir.join("project.db");
            if external::build_header_db(exe, &files, &db) {
                eprintln!("project syntax db: {} file(s) indexed", files.len());
                Some(db.to_string_lossy().to_string())
            } else {
                eprintln!("project syntax db build failed — builtin db only");
                None
            }
        }
        None => None,
    };
    // The host API db layers under the project db: a plugin calling
    // `veles::call_llm` is calling a command the core registers at runtime,
    // and "unknown" there is noise, not a finding.
    let host_db = std::env::temp_dir()
        .join(format!("tcl-lsp-db-{}", std::process::id()))
        .join("host.db");
    let host_db =
        external::write_host_command_db(&host_db).then(|| host_db.to_string_lossy().to_string());
    let mut server = Server {
        docs: HashMap::new(),
        index: Index::new(),
        kb,
        config,
        encoding,
        project_db,
        host_db,
    };

    // Index the workspace up front so definition and references work in files the
    // user has not opened yet.
    for root in roots {
        let n = server.index.scan(&root);
        eprintln!("indexed {n} Tcl files under {}", root.display());
    }

    server.main_loop(connection)
}

fn workspace_roots(params: &InitializeParams) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(folders) = &params.workspace_folders {
        for f in folders {
            if let Some(p) = uri_to_path(f.uri.as_str()) {
                out.push(p);
            }
        }
    }
    if out.is_empty() {
        #[allow(deprecated)]
        if let Some(root) = params.root_uri.as_ref() {
            if let Some(p) = uri_to_path(root.as_str()) {
                out.push(p);
            }
        }
    }
    out
}

/// Every Tcl file under the workspace roots, for cross-file linting. The
/// walk is the server's only broadcast: nagelfar gets these as input
/// siblings, definitions from them resolving "unknown command" noise.
/// Bounded on BOTH axes so a hostile repository cannot make the lint pass
/// O(n²) in a degenerate tree: file count AND total bytes.
fn collect_tcl_files(roots: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    const MAX_FILES: usize = 400;
    const MAX_BYTES: u64 = 16 * 1024 * 1024;
    let mut out = Vec::new();
    let mut bytes = 0u64;
    let mut stack: Vec<std::path::PathBuf> = roots.to_vec();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let name = p.file_name().map(|n| n.to_string_lossy().to_string());
                let name = name.as_deref().unwrap_or("");
                if name == "target"
                    || name == ".git"
                    || name == "vendor"
                    || name == "node_modules"
                    || name.starts_with("result")
                {
                    continue;
                }
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "tcl") {
                if out.len() >= MAX_FILES || bytes >= MAX_BYTES {
                    continue;
                }
                if let Ok(meta) = std::fs::metadata(&p) {
                    bytes += meta.len();
                }
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn capabilities(encoding: PositionEncoding) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(match encoding {
            PositionEncoding::Utf8 => PositionEncodingKind::UTF8,
            PositionEncoding::Utf16 => PositionEncodingKind::UTF16,
        }),
        // Incremental, so a keystroke does not resend the whole file.
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                ..Default::default()
            },
        )),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
            first_trigger_character: "}".to_string(),
            more_trigger_character: None,
        }),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        document_link_provider: Some(DocumentLinkOptions {
            resolve_provider: Some(false),
            work_done_progress_options: Default::default(),
        }),
        signature_help_provider: Some(SignatureHelpOptions {
            // A space starts the next argument, which is when the active parameter
            // advances; `[` opens a nested command with its own signature.
            trigger_characters: Some(vec![" ".to_string(), "[".to_string()]),
            retrigger_characters: Some(vec![" ".to_string()]),
            work_done_progress_options: Default::default(),
        }),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: crate::semantic::TOKEN_TYPES.to_vec(),
                    token_modifiers: vec![SemanticTokenModifier::DECLARATION],
                },
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: Some(true),
                work_done_progress_options: Default::default(),
            },
        )),
        inlay_hint_provider: Some(OneOf::Left(true)),
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        code_lens_provider: Some(CodeLensOptions {
            resolve_provider: Some(false),
        }),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        completion_provider: Some(CompletionOptions {
            // `$` opens a variable completion; `:` catches `::` for namespaces.
            trigger_characters: Some(vec!["$".to_string(), ":".to_string()]),
            resolve_provider: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    }
}

impl Server {
    fn main_loop(&mut self, connection: &Connection) -> Result<()> {
        for msg in &connection.receiver {
            match msg {
                Message::Request(req) => {
                    if connection.handle_shutdown(&req)? {
                        return Ok(());
                    }
                    let resp = self.handle_request(req);
                    connection.sender.send(Message::Response(resp))?;
                }
                Message::Notification(note) => self.handle_notification(connection, note)?,
                Message::Response(_) => {}
            }
        }
        Ok(())
    }

    fn handle_request(&mut self, req: Request) -> Response {
        let id = req.id.clone();
        let result = match req.method.as_str() {
            DocumentSymbolRequest::METHOD => {
                cast::<DocumentSymbolRequest>(req).map(|(_, p)| self.document_symbols(&p))
            }
            GotoDefinition::METHOD => {
                cast::<GotoDefinition>(req).map(|(_, p)| self.goto_definition(&p))
            }
            References::METHOD => cast::<References>(req).map(|(_, p)| self.references(&p)),
            DocumentHighlightRequest::METHOD => {
                cast::<DocumentHighlightRequest>(req).map(|(_, p)| self.highlights(&p))
            }
            HoverRequest::METHOD => cast::<HoverRequest>(req).map(|(_, p)| self.hover(&p)),
            Completion::METHOD => cast::<Completion>(req).map(|(_, p)| self.completion(&p)),
            FoldingRangeRequest::METHOD => {
                cast::<FoldingRangeRequest>(req).map(|(_, p)| self.folding(&p))
            }
            SelectionRangeRequest::METHOD => {
                cast::<SelectionRangeRequest>(req).map(|(_, p)| self.selection_ranges(&p))
            }
            DocumentLinkRequest::METHOD => {
                cast::<DocumentLinkRequest>(req).map(|(_, p)| self.document_links(&p))
            }
            SignatureHelpRequest::METHOD => {
                cast::<SignatureHelpRequest>(req).map(|(_, p)| self.signature_help(&p))
            }
            PrepareRenameRequest::METHOD => {
                cast::<PrepareRenameRequest>(req).map(|(_, p)| self.prepare_rename(&p))
            }
            Rename::METHOD => cast::<Rename>(req).map(|(_, p)| self.rename(&p)),
            CallHierarchyPrepare::METHOD => {
                cast::<CallHierarchyPrepare>(req).map(|(_, p)| self.prepare_call_hierarchy(&p))
            }
            CallHierarchyIncomingCalls::METHOD => {
                cast::<CallHierarchyIncomingCalls>(req).map(|(_, p)| self.incoming_calls(&p))
            }
            CallHierarchyOutgoingCalls::METHOD => {
                cast::<CallHierarchyOutgoingCalls>(req).map(|(_, p)| self.outgoing_calls(&p))
            }
            CodeLensRequest::METHOD => {
                cast::<CodeLensRequest>(req).map(|(_, p)| self.code_lens(&p))
            }
            OnTypeFormatting::METHOD => {
                cast::<OnTypeFormatting>(req).map(|(_, p)| self.on_type_formatting(&p))
            }
            TypeHierarchyPrepare::METHOD => {
                cast::<TypeHierarchyPrepare>(req).map(|(_, p)| self.prepare_type_hierarchy(&p))
            }
            TypeHierarchySupertypes::METHOD => {
                cast::<TypeHierarchySupertypes>(req).map(|(_, p)| self.supertypes(&p))
            }
            TypeHierarchySubtypes::METHOD => {
                cast::<TypeHierarchySubtypes>(req).map(|(_, p)| self.subtypes(&p))
            }
            CodeActionRequest::METHOD => {
                cast::<CodeActionRequest>(req).map(|(_, p)| self.code_actions(&p))
            }
            InlayHintRequest::METHOD => {
                cast::<InlayHintRequest>(req).map(|(_, p)| self.inlay_hints(&p))
            }
            SemanticTokensFullRequest::METHOD => {
                cast::<SemanticTokensFullRequest>(req).map(|(_, p)| self.semantic_tokens(&p))
            }
            SemanticTokensRangeRequest::METHOD => {
                cast::<SemanticTokensRangeRequest>(req).map(|(_, p)| self.semantic_tokens_range(&p))
            }
            WorkspaceSymbolRequest::METHOD => {
                cast::<WorkspaceSymbolRequest>(req).map(|(_, p)| self.workspace_symbols(&p))
            }
            Formatting::METHOD => cast::<Formatting>(req).map(|(_, p)| self.formatting(&p)),
            other => {
                eprintln!("unhandled request: {other}");
                return Response::new_ok(id, serde_json::Value::Null);
            }
        };

        match result {
            Ok(value) => Response::new_ok(id, value),
            Err(err) => {
                eprintln!("request failed: {err}");
                // A malformed request must not take the server down.
                Response::new_ok(id, serde_json::Value::Null)
            }
        }
    }

    fn handle_notification(&mut self, conn: &Connection, note: Notification) -> Result<()> {
        match note.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let p: DidOpenTextDocumentParams = serde_json::from_value(note.params)?;
                let uri = p.text_document.uri.to_string();
                // The editor's buffer is the only source of truth. Never re-read the
                // file from disk: it would ignore every unsaved edit.
                let doc = Document::new(p.text_document.text, p.text_document.version);
                self.index.set_file(uri.clone(), doc.text());
                self.publish(conn, &uri, &doc, true)?;
                self.docs.insert(uri, doc);
            }
            DidChangeTextDocument::METHOD => {
                let p: DidChangeTextDocumentParams = serde_json::from_value(note.params)?;
                let uri = p.text_document.uri.to_string();
                if let Some(doc) = self.docs.get_mut(&uri) {
                    for change in p.content_changes {
                        match change.range {
                            Some(range) => {
                                let li = doc.line_index();
                                let start = li.offset(to_linepos(range.start), self.encoding);
                                let end = li.offset(to_linepos(range.end), self.encoding);
                                doc.edit(start..end, &change.text);
                            }
                            None => doc.replace(change.text),
                        }
                    }
                    doc.set_version(p.text_document.version);
                }
                if let Some(doc) = self.docs.get(&uri) {
                    self.index.set_file(uri.clone(), doc.text());
                }
                if let Some(doc) = self.docs.get(&uri) {
                    // External analysers are not run per keystroke; see `publish`.
                    self.publish(conn, &uri, doc, false)?;
                }
            }
            DidSaveTextDocument::METHOD => {
                let p: DidSaveTextDocumentParams = serde_json::from_value(note.params)?;
                let uri = p.text_document.uri.to_string();
                if let Some(doc) = self.docs.get(&uri) {
                    self.publish(conn, &uri, doc, true)?;
                }
            }
            DidCloseTextDocument::METHOD => {
                let p: DidCloseTextDocumentParams = serde_json::from_value(note.params)?;
                let uri = p.text_document.uri.to_string();
                self.docs.remove(&uri);
                // Keep the file in the index — it still exists on disk — but re-read
                // it, so the index reflects saved content rather than a discarded
                // buffer.
                if let Some(path) = uri_to_path(&uri) {
                    match std::fs::read_to_string(&path) {
                        Ok(text) => self.index.set_file(uri.clone(), &text),
                        Err(_) => self.index.remove(&uri),
                    }
                }
                send_diagnostics(conn, &uri, None, Vec::new())?;
            }
            DidChangeConfiguration::METHOD => {
                let p: DidChangeConfigurationParams = serde_json::from_value(note.params)?;
                let target_moved = self.config.merge(&p.settings);
                if target_moved {
                    self.kb = kb::builtin(self.config.tcl_version);
                    eprintln!(
                        "reloaded {} builtin commands for {:?}",
                        self.kb.len(),
                        self.config.tcl_version
                    );
                }
                // Diagnostics were produced under the old settings, so anything the
                // user has open has to be re-reported or it silently goes stale.
                let uris: Vec<String> = self.docs.keys().cloned().collect();
                for uri in uris {
                    if let Some(doc) = self.docs.get(&uri) {
                        self.publish(conn, &uri, doc, true)?;
                    }
                }
            }
            DidChangeWatchedFiles::METHOD => {
                let p: DidChangeWatchedFilesParams = serde_json::from_value(note.params)?;
                for change in p.changes {
                    let uri = change.uri.to_string();
                    if self.docs.contains_key(&uri) {
                        continue; // an open buffer wins over the file on disk
                    }
                    match uri_to_path(&uri).and_then(|p| std::fs::read_to_string(p).ok()) {
                        Some(text) => self.index.set_file(uri, &text),
                        None => self.index.remove(&uri),
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Publishes diagnostics for a document.
    ///
    /// `thorough` additionally runs the external analysers, which cost tens of
    /// milliseconds each. They run on open and on save, not on every keystroke.
    fn publish(&self, conn: &Connection, uri: &str, doc: &Document, thorough: bool) -> Result<()> {
        let mut diags = self.own_diagnostics(doc);
        if thorough {
            let li = doc.line_index();
            let mut findings = Vec::new();
            if let Some(exe) = self.config.nagelfar.active() {
                let mut dbs: Vec<String> = Vec::new();
                if let Some(db) = &self.config.nagelfar_db {
                    dbs.push(db.clone());
                }
                if let Some(p) = &self.project_db {
                    dbs.push(p.clone());
                }
                if let Some(h) = &self.host_db {
                    dbs.push(h.clone());
                }
                findings.extend(external::nagelfar(doc.text(), exe, &dbs));
            }
            if let Some(exe) = self.config.tclint.active() {
                findings.extend(external::tclint(doc.text(), exe));
            }
            for f in findings {
                diags.push(finding_to_diagnostic(f, li, self.encoding));
            }
        }
        send_diagnostics(conn, uri, doc.version(), diags)
    }

    fn own_diagnostics(&self, doc: &Document) -> Vec<Diagnostic> {
        let outline = doc.outline();
        let text = doc.text();
        let mut out: Vec<Diagnostic> = outline
            .errors
            .iter()
            .filter(|e| {
                // An incomplete construct at the very end of the buffer is what a
                // half-typed line looks like. Reporting it would put a red squiggle
                // under the cursor on nearly every keystroke.
                !(e.incomplete && text[e.range.start.min(text.len())..].trim().is_empty())
            })
            .map(|e| Diagnostic {
                range: to_range(doc.line_index(), e.range.clone(), self.encoding),
                severity: Some(if e.incomplete {
                    DiagnosticSeverity::WARNING
                } else {
                    DiagnosticSeverity::ERROR
                }),
                source: Some("tcl-lsp".to_string()),
                message: e.message.clone(),
                ..Default::default()
            })
            .collect();

        for bad in unknown_options(&outline, text, &self.kb) {
            let mut message = format!("`{}` is not an option of `{}`", bad.flag, bad.command);
            if let Some(near) = &bad.suggestion {
                message.push_str(&format!("; did you mean `{near}`?"));
            }
            out.push(Diagnostic {
                range: to_range(doc.line_index(), bad.range, self.encoding),
                severity: Some(DiagnosticSeverity::WARNING),
                code: Some(NumberOrString::String("unknown-option".into())),
                source: Some("tcl-lsp".to_string()),
                message,
                ..Default::default()
            });
        }

        for range in unbraced_expressions(&outline, text) {
            out.push(Diagnostic {
                range: to_range(doc.line_index(), range, self.encoding),
                severity: Some(DiagnosticSeverity::WARNING),
                code: Some(NumberOrString::String("unbraced-expr".into())),
                source: Some("tcl-lsp".to_string()),
                message: "expression is not braced: it will be substituted before \
                          evaluation, which is slower and can change the result"
                    .to_string(),
                ..Default::default()
            });
        }
        out
    }

    /// Quick fixes and refactorings at a position.
    fn code_actions(&self, p: &CodeActionParams) -> serde_json::Value {
        let uri = p.text_document.uri.to_string();
        let Some(doc) = self.docs.get(&uri) else {
            return serde_json::Value::Null;
        };
        let li = doc.line_index();
        let from = li.offset(to_linepos(p.range.start), self.encoding);
        let to = li.offset(to_linepos(p.range.end), self.encoding);
        let outline = doc.outline();
        let text = doc.text();

        let mut actions: Vec<CodeActionOrCommand> = Vec::new();

        // Brace an unbraced expression.
        for range in unbraced_expressions(&outline, text) {
            if range.end < from || range.start > to {
                continue;
            }
            let Some(inner) = text.get(range.clone()) else {
                continue;
            };
            let edit = TextEdit {
                range: to_range(li, range.clone(), self.encoding),
                new_text: format!("{{{inner}}}"),
            };
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Brace the expression".to_string(),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(WorkspaceEdit {
                    changes: Some(single_change(&uri, vec![edit])),
                    ..Default::default()
                }),
                is_preferred: Some(true),
                ..Default::default()
            }));
        }

        // Correct a misspelled option.
        for bad in unknown_options(&outline, text, &self.kb) {
            if bad.range.end < from || bad.range.start > to {
                continue;
            }
            let Some(near) = bad.suggestion else { continue };
            let Some(range) = self.range_in(&uri, bad.range) else {
                continue;
            };
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Change `{}` to `{near}`", bad.flag),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(WorkspaceEdit {
                    changes: Some(single_change(
                        &uri,
                        vec![TextEdit {
                            range,
                            new_text: near,
                        }],
                    )),
                    ..Default::default()
                }),
                is_preferred: Some(true),
                ..Default::default()
            }));
        }

        // Add a missing `package require` for a command defined in a file that
        // declares a package this one never requires.
        if let Some(action) = self.add_package_require(&uri, doc, &outline, from) {
            actions.push(CodeActionOrCommand::CodeAction(action));
        }

        json(actions)
    }

    fn add_package_require(
        &self,
        uri: &str,
        doc: &Document,
        outline: &tcl_syntax::Outline,
        offset: usize,
    ) -> Option<CodeAction> {
        let word = word_at(doc.text(), offset)?;
        let ns = namespace_at(&outline.symbols, offset);
        let hits = self.index.resolve(&word, &ns);
        let (def_uri, _) = hits.first()?;
        if *def_uri == uri {
            return None; // defined right here; nothing to require
        }
        let provider = self.index.file(def_uri)?;
        let package = provider.outline.provides.first()?.name.clone();

        let already = outline
            .links
            .iter()
            .any(|l| l.kind == tcl_syntax::LinkKind::PackageRequire && l.name == package);
        if already {
            return None;
        }

        // Insert after any existing `package require`, else at the very top.
        let insert_at = outline
            .links
            .iter()
            .filter(|l| l.kind == tcl_syntax::LinkKind::PackageRequire)
            .map(|l| line_end_offset(doc.text(), l.range.end))
            .max()
            .unwrap_or(0);
        let pos = to_position(doc.line_index(), insert_at, self.encoding);
        let edit = TextEdit {
            range: Range_ {
                start: pos,
                end: pos,
            },
            new_text: format!("package require {package}\n"),
        };
        Some(CodeAction {
            title: format!("Add `package require {package}`"),
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(WorkspaceEdit {
                changes: Some(single_change(uri, vec![edit])),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    // --- helpers ---------------------------------------------------------

    /// Converts a byte range in `uri` to an LSP range, using whichever line index
    /// is available — the open buffer's, or the indexed copy of a closed file.
    fn range_in(&self, uri: &str, range: Range<usize>) -> Option<Range_> {
        if let Some(doc) = self.docs.get(uri) {
            return Some(to_range(doc.line_index(), range, self.encoding));
        }
        let f = self.index.file(uri)?;
        Some(to_range(&f.line_index, range, self.encoding))
    }

    fn location(&self, uri: &str, range: Range<usize>) -> Option<Location> {
        let range = self.range_in(uri, range)?;
        Some(Location {
            uri: parse_uri(uri)?,
            range,
        })
    }

    /// The identifier under the cursor plus the namespace in force there.
    fn word_and_ns(&self, uri: &str, pos: Position) -> Option<(String, String)> {
        let doc = self.docs.get(uri)?;
        let offset = doc.line_index().offset(to_linepos(pos), self.encoding);
        let word = word_at(doc.text(), offset)?;
        let ns = self
            .index
            .file(uri)
            .map(|f| namespace_at(&f.outline.symbols, offset))
            .unwrap_or_else(|| "::".to_string());
        Some((word, ns))
    }

    // --- request handlers ------------------------------------------------

    fn document_symbols(&self, p: &DocumentSymbolParams) -> serde_json::Value {
        let uri = p.text_document.uri.to_string();
        let Some(doc) = self.docs.get(&uri) else {
            return serde_json::Value::Null;
        };
        let outline = doc.outline();
        let symbols: Vec<DocumentSymbol> = outline
            .symbols
            .iter()
            .map(|s| to_document_symbol(doc.line_index(), s, self.encoding))
            .collect();
        json(DocumentSymbolResponse::Nested(symbols))
    }

    fn workspace_symbols(&self, p: &WorkspaceSymbolParams) -> serde_json::Value {
        let mut out: Vec<SymbolInformation> = Vec::new();
        for (uri, def) in self.index.search(&p.query, 512) {
            let Some(location) = self.location(uri, def.name_range.clone()) else {
                continue;
            };
            #[allow(deprecated)]
            out.push(SymbolInformation {
                name: def.qname.clone(),
                kind: to_lsp_kind(def.kind),
                tags: None,
                deprecated: None,
                location,
                container_name: None,
            });
        }
        json(out)
    }

    fn goto_definition(&self, p: &GotoDefinitionParams) -> serde_json::Value {
        let uri = p
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let Some((word, ns)) = self.word_and_ns(&uri, p.text_document_position_params.position)
        else {
            return serde_json::Value::Null;
        };
        let locations: Vec<Location> = self
            .index
            .resolve(&word, &ns)
            .into_iter()
            .filter_map(|(u, d)| self.location(u, d.name_range.clone()))
            .collect();
        if locations.is_empty() {
            return serde_json::Value::Null;
        }
        json(GotoDefinitionResponse::Array(locations))
    }

    fn references(&self, p: &ReferenceParams) -> serde_json::Value {
        let uri = p.text_document_position.text_document.uri.to_string();
        let Some((word, ns)) = self.word_and_ns(&uri, p.text_document_position.position) else {
            return serde_json::Value::Null;
        };
        // Resolve first, so `greet` and `::util::greet` find the same use sites.
        let qname = self
            .index
            .resolve(&word, &ns)
            .first()
            .map(|(_, d)| d.qname.clone())
            .unwrap_or_else(|| tcl_syntax::qualify(&ns, &word));

        let mut out: Vec<Location> = self
            .index
            .references(&qname)
            .into_iter()
            .filter_map(|loc| self.location(&loc.uri, loc.range))
            .collect();

        if p.context.include_declaration {
            for (u, d) in self.index.resolve(&word, &ns) {
                if let Some(l) = self.location(u, d.name_range.clone()) {
                    out.push(l);
                }
            }
        }
        json(out)
    }

    fn highlights(&self, p: &DocumentHighlightParams) -> serde_json::Value {
        let uri = p
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let Some((word, _ns)) = self.word_and_ns(&uri, p.text_document_position_params.position)
        else {
            return serde_json::Value::Null;
        };
        let Some(f) = self.index.file(&uri) else {
            return serde_json::Value::Null;
        };
        let tail = word.rsplit("::").next().unwrap_or(&word);
        let mut out = Vec::new();
        for r in &f.outline.refs {
            if r.name == word || r.name.rsplit("::").next() == Some(tail) {
                if let Some(range) = self.range_in(&uri, r.range.clone()) {
                    out.push(DocumentHighlight {
                        range,
                        kind: Some(DocumentHighlightKind::READ),
                    });
                }
            }
        }
        for d in &f.defs {
            if d.name == tail {
                if let Some(range) = self.range_in(&uri, d.name_range.clone()) {
                    out.push(DocumentHighlight {
                        range,
                        kind: Some(DocumentHighlightKind::WRITE),
                    });
                }
            }
        }
        json(out)
    }

    fn hover(&self, p: &HoverParams) -> serde_json::Value {
        let uri = p
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let Some((word, ns)) = self.word_and_ns(&uri, p.text_document_position_params.position)
        else {
            return serde_json::Value::Null;
        };
        // A widget-path dispatch (`$w.bla.bla insert end x`) has no static
        // definition — the widget command is built at runtime — but the
        // subcommand word still deserves a hover. Say what is known: it is a
        // widget command, on which path, and which of the classic Tk widget
        // families document this subcommand. Checked only when nothing in the
        // workspace defines the word (see widget_command_hover's doc).
        if let Some(md) =
            self.widget_command_hover(&uri, p.text_document_position_params.position, &word, &ns)
        {
            return json(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: md,
                }),
                range: None,
            });
        }
        let hits = self.index.resolve(&word, &ns);
        let md = match hits.first() {
            // A definition in the user's own code wins over a builtin of the same
            // name, because redefining a builtin is legal and the user's version is
            // what will actually run.
            Some((_, def)) => {
                let mut md = format!("```tcl\n{}\n```", def.signature());
                if let Some(doc) = &def.doc {
                    md.push_str("\n\n");
                    md.push_str(doc);
                }
                md
            }
            None => match self
                .ensemble_subcommand_hover(&uri, p.text_document_position_params.position, &word)
                .or_else(|| self.kb.get(&word).map(|b| b.hover_markdown()))
            {
                Some(md) => md,
                None => return serde_json::Value::Null,
            },
        };
        json(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: md,
            }),
            range: None,
        })
    }

    /// Hover content for the subcommand word of an ensemble builtin —
    /// `string match`, `dict get`, `file tail` — where the cursor sits on
    /// the second word. `word_at` yields `match`, which is not itself a
    /// command, so the plain kb lookup misses; this re-reads the enclosing
    /// command, finds the head's page, and shows that subcommand's entry:
    /// its own signature and the prose of its `.TP` block.
    fn ensemble_subcommand_hover(&self, uri: &str, pos: Position, word: &str) -> Option<String> {
        let doc = self.docs.get(uri)?;
        let offset = doc.line_index().offset(to_linepos(pos), self.encoding);
        let text = doc.text();
        let enclosing = tcl_syntax::command_at(&tcl_syntax::Script::new(text), offset)?;
        // Word 1 of the command, head is word 0.
        if enclosing.word_index != 1 {
            return None;
        }
        let head = enclosing.name?;
        let cmd = self.kb.get(&head)?;
        // Ensembles accept unambiguous abbreviations (`string mat`), so the
        // cursor word may be a prefix: use it only when exactly one
        // subcommand starts with it.
        let exact = cmd.subcommands.iter().find(|s| s.name == word);
        let mut candidates = cmd
            .subcommands
            .iter()
            .filter(|s| !word.is_empty() && s.name.starts_with(word));
        let sub = match (exact, candidates.next(), candidates.next()) {
            (Some(s), _, _) => Some(s),
            (None, Some(one), None) => Some(one),
            _ => None,
        }?;
        let mut md = format!("```tcl\n{}\n```\n", sub.signature);
        if !sub.doc.is_empty() {
            md.push('\n');
            md.push_str(&sub.doc);
            md.push('\n');
        }
        Some(md)
    }

    /// Hover content for the subcommand word of a dynamic-head command
    /// (`$w.path insert end x` with the cursor on `insert`), or `None` when
    /// the position is not such a word.
    ///
    /// Deliberately LAST in hover's dispatch order: a dynamic head is also
    /// how TclOO object methods are called (`$c bump 3`), and when the
    /// workspace actually defines a matching method, the normal
    /// index-resolution path already produces its signature and docstring.
    /// This path only fires for words nothing in the workspace defines —
    /// where the best available answer is Tk's widget documentation.
    fn widget_command_hover(
        &self,
        uri: &str,
        pos: Position,
        word: &str,
        ns: &str,
    ) -> Option<String> {
        // TclOO first: if the workspace genuinely defines this name, hover
        // already showed it and the widget-doc fallback would be noise.
        //
        // "Genuinely" is load-bearing. `index.resolve` falls back to a
        // last-segment match, so a plain `proc configure` / `proc get` /
        // `proc insert` ANYWHERE in the tree would otherwise suppress the
        // Tk documentation for that subcommand across the whole workspace —
        // and those are the most common proc names there are. A TclOO
        // method IS the intended target of a `$obj sub ...` dispatch; a
        // proc reached only by tail-match almost never is.
        let defers = self.index.resolve(word, ns).iter().any(|(_, d)| {
            matches!(
                d.kind,
                TclKind::Method | TclKind::Constructor | TclKind::Destructor
            ) || d.qname == format!("::{word}")
                || (ns != "::" && !ns.is_empty() && d.qname == format!("{ns}::{word}"))
        });
        if defers {
            return None;
        }
        let doc = self.docs.get(uri)?;
        let offset = doc.line_index().offset(to_linepos(pos), self.encoding);
        let text = doc.text();
        // The enclosing command tells us whether the head is dynamic and which
        // word the cursor is on. `Enclosing.name` is None exactly when the
        // command word involves substitution.
        let enclosing = tcl_syntax::command_at(&tcl_syntax::Script::new(text), offset)?;
        if enclosing.name.is_some() {
            return None; // statically named command: the normal path handles it
        }
        if enclosing.word_index != 1 {
            return None; // hover only makes sense on the dispatch word itself
        }
        let sub = word_at(text, offset)?;
        if sub != word {
            return None;
        }
        let head_span = enclosing.command.words().first().and_then(|w| {
            w.first()
                .map(|t| t.range().start)
                .zip(w.last().map(|t| t.range().end))
        });
        let head = head_span
            .and_then(|(s, e)| text.get(s..e).map(|s| s.to_string()))
            .unwrap_or_default();
        Some(self.widget_command_markdown(&head, &sub))
    }

    /// Documentation for a widget-path dispatch (`$w insert end x`).
    ///
    /// The path is runtime-built, so the exact widget family is not statically
    /// known — but the head often names it (`.log`, `.entry.field`,
    /// `$tree`, a variable called `txt`), and when it does, the real man-page
    /// documentation for that family's subcommand is shown. Otherwise the
    /// subcommand is documented across the families that share it, with
    /// pointers to the pages whose `.TP` entries describe it.
    fn widget_command_markdown(&self, head: &str, sub: &str) -> String {
        // Widgets whose subcommand sets are worth checking, in specificity
        // order. `ttk::*` names its family directly; the classic pages too.
        const WIDGETS: &[&str] = &[
            "text",
            "entry",
            "listbox",
            "canvas",
            "menu",
            "menubutton",
            "ttk::treeview",
            "ttk::combobox",
            "ttk::notebook",
            "ttk::spinbox",
            "ttk::scale",
            "ttk::progressbar",
            "button",
            "checkbutton",
            "radiobutton",
            "label",
            "labelframe",
            "frame",
            "scrollbar",
            "scale",
            "spinbox",
            "ttk::button",
            "ttk::entry",
            "ttk::frame",
            "ttk::label",
            "ttk::labelframe",
            "ttk::menubutton",
            "ttk::radiobutton",
            "ttk::checkbutton",
            "ttk::scrollbar",
            "ttk::separator",
            "ttk::sizegrip",
        ];

        // Guess the family from the head: the variable or path tail matched
        // against the widget's own name (`txt`/`.log.text` → text,
        // `$tree`/`.main.tv` → ttk::treeview). Short variable names are
        // conventionally the widget's own name or an abbreviation of it.
        let tail = head
            .trim_start_matches('$')
            .rsplit(['.', ':'])
            .find(|s| !s.is_empty())
            .unwrap_or("");
        let lower = tail.to_ascii_lowercase();
        let family_guess = WIDGETS.iter().find(|w| {
            let short = w.rsplit("::").next().unwrap_or(w);
            lower.contains(short) || short.starts_with(&lower) && lower.len() >= 2
        });

        // Which families document this subcommand at all? Their SYNOPSIS
        // rows and `.TP` entries carry `pathName <sub> ...` from the man
        // pages. Group form (`tag option`) and its nested forms
        // (`tag add`, `tag bind`, …) share the word and all belong.
        let mut documenting: Vec<&str> = Vec::new();
        let mut entries: Vec<(String, String)> = Vec::new(); // (signature, doc)
        let mut page_description: Option<String> = None;
        for w in WIDGETS {
            let Some(cmd) = self.kb.get(w) else {
                continue;
            };
            let tps: Vec<&kb::Subcommand> =
                cmd.subcommands.iter().filter(|s| s.name == sub).collect();
            let syn = cmd
                .synopsis
                .iter()
                .find(|s| s.split_whitespace().nth(1) == Some(sub));
            if tps.is_empty() && syn.is_none() {
                continue;
            }
            documenting.push(w);
            let head_is_this = family_guess == Some(w);
            if !head_is_this {
                continue;
            }
            // The guessed family is the answer; show every form it has.
            for s in &tps {
                entries.push((s.signature.clone(), s.doc.clone()));
            }
            if entries.is_empty() {
                if let Some(s) = syn {
                    entries.push((s.clone(), String::new()));
                }
            }
            if !cmd.description.is_empty() {
                page_description = Some(cmd.description.clone());
            }
        }
        // No family named: show one entry per DISTINCT form. `configure` is
        // spelled the same on 18 pages (their option sets differ — the pages
        // say which); `insert` genuinely differs per widget, and each variant
        // earns its line.
        let mut fallback_entries: Vec<(String, String)> = Vec::new();
        if entries.is_empty() {
            for w in WIDGETS {
                if family_guess == Some(w) {
                    continue;
                }
                let Some(cmd) = self.kb.get(w) else {
                    continue;
                };
                for s in cmd.subcommands.iter().filter(|s| s.name == sub) {
                    let sig = s.signature.clone();
                    if !fallback_entries.iter().any(|(e, _)| e == &sig) {
                        fallback_entries.push((sig, s.doc.clone()));
                    }
                }
            }
            if fallback_entries.is_empty() {
                for w in WIDGETS {
                    let Some(cmd) = self.kb.get(w) else {
                        continue;
                    };
                    if let Some(s) = cmd
                        .synopsis
                        .iter()
                        .find(|s| s.split_whitespace().nth(1) == Some(sub))
                    {
                        fallback_entries.push((s.clone(), cmd.description.clone()));
                        break;
                    }
                }
            }
        }
        if entries.is_empty() {
            entries = fallback_entries;
        }

        let mut md = format!("**Widget command** `{sub}` on `{head}`.\n");
        match (family_guess, documenting.len()) {
            (Some(f), _) => {
                md.push_str(&format!(
                    "\nPath tail matches [`{f}`]; that page's documentation follows.\n"
                ));
            }
            (_, 0) => {
                md.push_str(&format!(
                    "\nNo Tk widget man page documents `{sub}` as a widget subcommand. If the \
                     head is an object handle (TclOO), see that class's `method {sub}` instead.\n"
                ));
            }
            (_, n) => {
                // The pages list is orientation, not the point; the entries
                // below are the documentation. Cap the list so a shared form
                // like `configure` (18 pages) does not bury it.
                let shown = if n > 6 {
                    format!("`{}`, and {} more", documenting[..6].join("`, `"), n - 6)
                } else {
                    format!("`{}`", documenting.join("`, `"))
                };
                md.push_str(&format!(
                    "\n`{sub}` is a widget subcommand on {n} pages: {shown}.\n"
                ));
            }
        }
        for (sig, doc) in &entries {
            md.push_str(&format!("\n```tcl\n{sig}\n```\n"));
            if !doc.is_empty() {
                md.push_str(doc);
                md.push('\n');
            }
        }
        if entries.is_empty() {
            if let Some(d) = page_description {
                md.push('\n');
                md.push_str(&d);
                md.push('\n');
            }
        }
        md
    }

    fn completion(&self, p: &CompletionParams) -> serde_json::Value {
        let uri = p.text_document_position.text_document.uri.to_string();
        let Some(doc) = self.docs.get(&uri) else {
            return serde_json::Value::Null;
        };
        let offset = doc
            .line_index()
            .offset(to_linepos(p.text_document_position.position), self.encoding);
        let text = doc.text();

        let c = self.index.completions_at(&uri, offset);
        let mut items: Vec<CompletionItem> = Vec::new();

        // Typing `-` inside a Tk widget command: offer that widget's own options,
        // which the command database carries from each man page's `.OP` entries.
        if let Some(opts) = self.option_completions(text, offset) {
            return json(CompletionResponse::Array(opts));
        }

        if wants_variable(text, offset) {
            for v in c.variables {
                items.push(CompletionItem {
                    label: v.name.clone(),
                    kind: Some(CompletionItemKind::VARIABLE),
                    ..Default::default()
                });
            }
        } else {
            let mut seen = std::collections::HashSet::new();
            for d in c.commands {
                if !seen.insert(d.qname.clone()) {
                    continue;
                }
                items.push(CompletionItem {
                    label: d.name.clone(),
                    kind: Some(match d.kind {
                        TclKind::Proc => CompletionItemKind::FUNCTION,
                        TclKind::Method => CompletionItemKind::METHOD,
                        TclKind::Class => CompletionItemKind::CLASS,
                        TclKind::Namespace => CompletionItemKind::MODULE,
                        _ => CompletionItemKind::VARIABLE,
                    }),
                    detail: Some(d.signature()),
                    documentation: d.doc.clone().map(Documentation::String),
                    // Workspace symbols sort above builtins: what the user wrote is
                    // more likely to be what they want than all 240 of Tcl's own.
                    sort_text: Some(format!("0{}", d.name)),
                    ..Default::default()
                });
            }
            for b in self.kb.iter() {
                if !seen.insert(b.name.clone()) {
                    continue;
                }
                items.push(CompletionItem {
                    label: b.name.clone(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    detail: b.synopsis.first().cloned().or(Some(b.summary.clone())),
                    documentation: Some(Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: b.hover_markdown(),
                    })),
                    sort_text: Some(format!("1{}", b.name)),
                    ..Default::default()
                });
            }
        }
        json(CompletionResponse::Array(items))
    }

    /// `-option` completions for the command being typed, if it documents any.
    ///
    /// Returns `None` — rather than an empty list — when this is not an option
    /// position, so the caller falls through to ordinary command completion.
    fn option_completions(&self, text: &str, offset: usize) -> Option<Vec<CompletionItem>> {
        // The word under the cursor has to actually start with a dash.
        let start = text[..offset.min(text.len())]
            .rfind(|c: char| c.is_whitespace() || c == '[' || c == '{')
            .map(|i| i + 1)
            .unwrap_or(0);
        if !text[start..offset.min(text.len())].starts_with('-') {
            return None;
        }

        let script = tcl_syntax::Script::new(text);
        let enclosing = tcl_syntax::command_at(&script, offset)?;
        let name = enclosing.name?;
        let cmd = self.kb.get(&name)?;
        if cmd.options.is_empty() {
            return None;
        }
        Some(
            cmd.options
                .iter()
                .map(|o| CompletionItem {
                    label: o.flag.clone(),
                    kind: Some(CompletionItemKind::PROPERTY),
                    detail: Some(if o.db_class.is_empty() {
                        name.clone()
                    } else {
                        format!("{} ({})", name, o.db_class)
                    }),
                    documentation: (!o.doc.is_empty())
                        .then(|| Documentation::String(o.doc.clone())),
                    ..Default::default()
                })
                .collect(),
        )
    }

    fn folding(&self, p: &FoldingRangeParams) -> serde_json::Value {
        let uri = p.text_document.uri.to_string();
        let Some(doc) = self.docs.get(&uri) else {
            return serde_json::Value::Null;
        };
        let outline = doc.outline();
        let li = doc.line_index();
        let mut out = Vec::new();
        collect_folds(&outline.symbols, li, self.encoding, &mut out);
        json(out)
    }

    /// Anchors a type-hierarchy session on the TclOO class under the cursor.
    fn prepare_type_hierarchy(&self, p: &TypeHierarchyPrepareParams) -> serde_json::Value {
        let uri = p
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let Some((word, ns)) = self.word_and_ns(&uri, p.text_document_position_params.position)
        else {
            return serde_json::Value::Null;
        };
        let items: Vec<TypeHierarchyItem> = self
            .index
            .resolve(&word, &ns)
            .iter()
            .filter(|(_, d)| d.kind == TclKind::Class)
            .filter_map(|(u, d)| self.type_item(u, d))
            .collect();
        if items.is_empty() {
            return serde_json::Value::Null;
        }
        json(items)
    }

    fn type_item(&self, uri: &str, def: &tcl_analysis::Def) -> Option<TypeHierarchyItem> {
        Some(TypeHierarchyItem {
            name: def.qname.clone(),
            kind: SymbolKind::CLASS,
            tags: None,
            detail: (!def.supers.is_empty())
                .then(|| format!("superclass {}", def.supers.join(" "))),
            uri: parse_uri(uri)?,
            range: self.range_in(uri, def.full_range.clone())?,
            selection_range: self.range_in(uri, def.name_range.clone())?,
            data: Some(serde_json::json!({ "qname": def.qname })),
        })
    }

    /// The classes a class derives from: its `superclass` and `mixin` entries.
    fn supertypes(&self, p: &TypeHierarchySupertypesParams) -> serde_json::Value {
        let Some(def) = self.class_of_item(&p.item) else {
            return serde_json::Value::Null;
        };
        // A superclass name is written inside the class body, so it resolves
        // relative to the class's own namespace, not the global one.
        let ns = container_of(&def.qname);
        let mut out = Vec::new();
        for name in &def.supers {
            for (u, d) in self.index.resolve(name, &ns) {
                if d.kind == TclKind::Class {
                    if let Some(item) = self.type_item(u, d) {
                        out.push(item);
                    }
                }
            }
        }
        json(out)
    }

    /// The classes that derive from a class.
    fn subtypes(&self, p: &TypeHierarchySubtypesParams) -> serde_json::Value {
        let Some(def) = self.class_of_item(&p.item) else {
            return serde_json::Value::Null;
        };
        let target = def.qname.clone();
        let mut out = Vec::new();
        for (uri, f) in self.index.files() {
            for candidate in f.defs.iter().filter(|d| d.kind == TclKind::Class) {
                let ns = container_of(&candidate.qname);
                let derives = candidate.supers.iter().any(|s| {
                    self.index
                        .resolve(s, &ns)
                        .first()
                        .is_some_and(|(_, d)| d.qname == target)
                });
                if derives {
                    if let Some(item) = self.type_item(uri, candidate) {
                        out.push(item);
                    }
                }
            }
        }
        json(out)
    }

    fn class_of_item(&self, item: &TypeHierarchyItem) -> Option<tcl_analysis::Def> {
        let qname = item
            .data
            .as_ref()
            .and_then(|d| d.get("qname"))
            .and_then(|v| v.as_str())
            .unwrap_or(&item.name);
        self.index
            .resolve(qname, "::")
            .into_iter()
            .find(|(_, d)| d.kind == TclKind::Class)
            .map(|(_, d)| d.clone())
    }

    /// Re-indents the current line as you type its closing brace.
    ///
    /// Deliberately narrow: only the line the trigger sits on, and only when it
    /// contains nothing but the brace and whitespace. Reformatting more than that
    /// while someone is mid-edit moves text under the cursor, which is worse than
    /// doing nothing.
    fn on_type_formatting(&self, p: &DocumentOnTypeFormattingParams) -> serde_json::Value {
        let uri = p.text_document_position.text_document.uri.to_string();
        let Some(doc) = self.docs.get(&uri) else {
            return serde_json::Value::Null;
        };
        if p.ch != "}" {
            return json(Vec::<TextEdit>::new());
        }
        let text = doc.text();
        let li = doc.line_index();
        let line = p.text_document_position.position.line;
        let start = li.offset(LinePos { line, character: 0 }, self.encoding);
        let end = li.offset(
            LinePos {
                line,
                character: u32::MAX,
            },
            self.encoding,
        );
        let Some(current) = text.get(start..end) else {
            return json(Vec::<TextEdit>::new());
        };
        if current.trim() != "}" {
            return json(Vec::<TextEdit>::new());
        }

        // Match the indentation of the line that opened this block. Tcl's own
        // parser knows where that is: the closing brace ends some command, and
        // that command's first line is the one to line up with.
        //
        // The brace's own offset is what to ask about, not the start of the line:
        // the leading whitespace still lies *inside* the block, so asking there
        // would find the last command of the body instead of the command the brace
        // closes.
        let script = tcl_syntax::Script::new(text);
        let brace_at = start + current.find('}').unwrap_or(0);
        let Some(open_line) = enclosing_open_line(&script, text, brace_at) else {
            return json(Vec::<TextEdit>::new());
        };
        let indent: String = open_line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let existing: String = current
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if indent == existing {
            return json(Vec::<TextEdit>::new());
        }
        let edit = TextEdit {
            range: Range_ {
                start: Position { line, character: 0 },
                end: to_position(li, start + existing.len(), self.encoding),
            },
            new_text: indent,
        };
        json(vec![edit])
    }

    /// Reference counts above each definition.
    fn code_lens(&self, p: &CodeLensParams) -> serde_json::Value {
        let uri = p.text_document.uri.to_string();
        let Some(f) = self.index.file(&uri) else {
            return serde_json::Value::Null;
        };
        let named: Vec<&tcl_analysis::Def> = f
            .defs
            .iter()
            .filter(|d| {
                matches!(
                    d.kind,
                    TclKind::Proc | TclKind::Method | TclKind::Class | TclKind::Namespace
                )
            })
            .collect();
        let counts = self
            .index
            .reference_counts(named.iter().map(|d| d.qname.as_str()));

        let mut out = Vec::new();
        for def in named {
            let n = counts.get(&def.qname).copied().unwrap_or(0);
            let Some(range) = self.range_in(&uri, def.name_range.clone()) else {
                continue;
            };
            out.push(CodeLens {
                range,
                command: Some(lsp_types::Command {
                    title: if n == 1 {
                        "1 reference".to_string()
                    } else {
                        format!("{n} references")
                    },
                    // Clients bind this to "show references" themselves; there is no
                    // server-side command to execute.
                    command: String::new(),
                    arguments: None,
                }),
                data: None,
            });
        }
        json(out)
    }

    /// Anchors a call-hierarchy session on the proc under the cursor.
    fn prepare_call_hierarchy(&self, p: &CallHierarchyPrepareParams) -> serde_json::Value {
        let uri = p
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let Some((word, ns)) = self.word_and_ns(&uri, p.text_document_position_params.position)
        else {
            return serde_json::Value::Null;
        };
        let hits = self.index.resolve(&word, &ns);
        let items: Vec<CallHierarchyItem> = hits
            .iter()
            .filter_map(|(u, d)| self.hierarchy_item(u, d))
            .collect();
        if items.is_empty() {
            return serde_json::Value::Null;
        }
        json(items)
    }

    fn hierarchy_item(&self, uri: &str, def: &tcl_analysis::Def) -> Option<CallHierarchyItem> {
        Some(CallHierarchyItem {
            name: def.qname.clone(),
            kind: to_lsp_kind(def.kind),
            tags: None,
            detail: def.detail.clone(),
            uri: parse_uri(uri)?,
            range: self.range_in(uri, def.full_range.clone())?,
            selection_range: self.range_in(uri, def.name_range.clone())?,
            // Carried through the round trip so the follow-up requests need no
            // position lookup of their own.
            data: Some(serde_json::json!({ "uri": uri, "qname": def.qname })),
        })
    }

    /// The qualified name a call refers to, or `None` if it resolves to nothing.
    ///
    /// Cheap tail comparison first: `resolve` scans every file, so running it on
    /// every call in a large workspace would be quadratic.
    fn call_target(&self, call: &tcl_syntax::Call, want_tail: Option<&str>) -> Option<String> {
        let tail = call.name.rsplit("::").next().unwrap_or(&call.name);
        if want_tail.is_some_and(|w| w != tail) {
            return None;
        }
        let hits = self.index.resolve(&call.name, &call.namespace);
        hits.first().map(|(_, d)| d.qname.clone())
    }

    fn incoming_calls(&self, p: &CallHierarchyIncomingCallsParams) -> serde_json::Value {
        let Some(target) = p.item.data.as_ref().and_then(|d| d.get("qname")) else {
            return serde_json::Value::Null;
        };
        let target = target.as_str().unwrap_or_default().to_string();
        let tail = target.rsplit("::").next().unwrap_or(&target).to_string();

        // Grouped by the proc the call site sits in.
        let mut grouped: HashMap<(String, String), Vec<Range<usize>>> = HashMap::new();
        for (uri, f) in self.index.files() {
            for call in &f.outline.calls {
                if self.call_target(call, Some(&tail)).as_deref() != Some(target.as_str()) {
                    continue;
                }
                let caller = enclosing_def(&f.outline.symbols, call.name_range.start)
                    .map(|s| s.qname.clone())
                    // A call at file scope has no enclosing proc; attribute it to
                    // the file itself rather than dropping it.
                    .unwrap_or_else(|| "<file scope>".to_string());
                grouped
                    .entry((uri.to_string(), caller))
                    .or_default()
                    .push(call.name_range.clone());
            }
        }

        let mut out = Vec::new();
        for ((uri, caller), ranges) in grouped {
            let from = match self.index.resolve(&caller, "::").first() {
                Some((u, d)) => self.hierarchy_item(u, d),
                None => self.file_scope_item(&uri),
            };
            let Some(from) = from else { continue };
            out.push(CallHierarchyIncomingCall {
                from,
                from_ranges: ranges
                    .into_iter()
                    .filter_map(|r| self.range_in(&uri, r))
                    .collect(),
            });
        }
        json(out)
    }

    fn outgoing_calls(&self, p: &CallHierarchyOutgoingCallsParams) -> serde_json::Value {
        let data = p.item.data.as_ref();
        let Some(uri) = data.and_then(|d| d.get("uri")).and_then(|v| v.as_str()) else {
            return serde_json::Value::Null;
        };
        let Some(f) = self.index.file(uri) else {
            return serde_json::Value::Null;
        };
        let Some(body) = self
            .range_of_item(&p.item, f)
            .or_else(|| Some(0..f.outline.symbols.first()?.full_range.end))
        else {
            return serde_json::Value::Null;
        };

        let mut grouped: HashMap<String, Vec<Range<usize>>> = HashMap::new();
        for call in &f.outline.calls {
            if !body.contains(&call.name_range.start) {
                continue;
            }
            let Some(qname) = self.call_target(call, None) else {
                continue;
            };
            grouped
                .entry(qname)
                .or_default()
                .push(call.name_range.clone());
        }

        let mut out = Vec::new();
        for (qname, ranges) in grouped {
            let Some((u, d)) = self.index.resolve(&qname, "::").first().copied() else {
                continue;
            };
            let Some(to) = self.hierarchy_item(u, d) else {
                continue;
            };
            out.push(CallHierarchyOutgoingCall {
                to,
                from_ranges: ranges
                    .into_iter()
                    .filter_map(|r| self.range_in(uri, r))
                    .collect(),
            });
        }
        json(out)
    }

    /// Byte range of a hierarchy item's body, recovered from its qualified name.
    fn range_of_item(
        &self,
        item: &CallHierarchyItem,
        f: &tcl_analysis::FileIndex,
    ) -> Option<Range<usize>> {
        f.defs
            .iter()
            .find(|d| d.qname == item.name)
            .map(|d| d.full_range.clone())
    }

    /// A stand-in item for calls made at file scope, outside any proc.
    fn file_scope_item(&self, uri: &str) -> Option<CallHierarchyItem> {
        let zero = Range_ {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
        };
        Some(CallHierarchyItem {
            name: "<file scope>".to_string(),
            kind: SymbolKind::FILE,
            tags: None,
            detail: None,
            uri: parse_uri(uri)?,
            range: zero,
            selection_range: zero,
            data: None,
        })
    }

    /// Parameter-name hints at call sites of user-defined procs.
    ///
    /// Not emitted for builtins: a man-page synopsis names arguments for a human
    /// reader (`?options?`), not in a form that reads well inline.
    fn inlay_hints(&self, p: &InlayHintParams) -> serde_json::Value {
        let uri = p.text_document.uri.to_string();
        let Some(doc) = self.docs.get(&uri) else {
            return serde_json::Value::Null;
        };
        let li = doc.line_index();
        let from = li.offset(to_linepos(p.range.start), self.encoding);
        let to = li.offset(to_linepos(p.range.end), self.encoding);

        let outline = doc.outline();
        let mut out: Vec<InlayHint> = Vec::new();
        for call in &outline.calls {
            if call.name_range.start < from || call.name_range.start > to {
                continue;
            }
            let hits = self.index.resolve(&call.name, &call.namespace);
            let Some((_, def)) = hits.first() else {
                continue;
            };
            let Some(detail) = def.detail.as_deref() else {
                continue;
            };
            let params = tcl_syntax::split_args(detail);
            for (i, arg) in call.args.iter().enumerate() {
                let Some(name) = params.get(i) else { break };
                // `args` swallows everything that follows, so a positional label
                // past that point would be a lie.
                if name == "args" {
                    break;
                }
                out.push(InlayHint {
                    position: to_position(li, arg.start, self.encoding),
                    label: InlayHintLabel::String(format!("{name}:")),
                    kind: Some(InlayHintKind::PARAMETER),
                    text_edits: None,
                    tooltip: None,
                    padding_left: None,
                    padding_right: Some(true),
                    data: None,
                });
            }
        }
        json(out)
    }

    fn semantic_tokens(&self, p: &SemanticTokensParams) -> serde_json::Value {
        let uri = p.text_document.uri.to_string();
        let Some(doc) = self.docs.get(&uri) else {
            return serde_json::Value::Null;
        };
        let data = crate::semantic::tokens(
            &doc.outline(),
            doc.text(),
            doc.line_index(),
            self.encoding,
            |name| self.kb.get(name).is_some(),
        );
        json(SemanticTokens {
            result_id: None,
            data,
        })
    }

    /// Range variant. The tokens are computed for the whole file and filtered,
    /// which keeps the delta encoding valid without a second code path.
    fn semantic_tokens_range(&self, p: &SemanticTokensRangeParams) -> serde_json::Value {
        let uri = p.text_document.uri.to_string();
        let Some(doc) = self.docs.get(&uri) else {
            return serde_json::Value::Null;
        };
        let all = crate::semantic::tokens(
            &doc.outline(),
            doc.text(),
            doc.line_index(),
            self.encoding,
            |name| self.kb.get(name).is_some(),
        );

        // Re-walk the deltas to absolute lines so the requested window can be cut
        // out, then re-encode relative to the first token kept.
        let (from, to) = (p.range.start.line, p.range.end.line);
        let mut abs_line = 0u32;
        let mut abs_start = 0u32;
        let mut out: Vec<SemanticToken> = Vec::new();
        let (mut prev_line, mut prev_start) = (0u32, 0u32);
        for t in all {
            abs_line += t.delta_line;
            abs_start = if t.delta_line == 0 {
                abs_start + t.delta_start
            } else {
                t.delta_start
            };
            if abs_line < from || abs_line > to {
                continue;
            }
            let delta_line = abs_line - prev_line;
            out.push(SemanticToken {
                delta_line,
                delta_start: if delta_line == 0 {
                    abs_start - prev_start
                } else {
                    abs_start
                },
                ..t
            });
            prev_line = abs_line;
            prev_start = abs_start;
        }
        json(SemanticTokens {
            result_id: None,
            data: out,
        })
    }

    /// Every site that renaming `word` would have to touch, as byte ranges.
    ///
    /// Only the *last segment* of a qualified name is returned. Renaming `trim` to
    /// `strip` must turn `util::trim` into `util::strip`, not replace the whole
    /// qualified name and break the reference.
    fn rename_sites(&self, word: &str, ns: &str) -> Vec<(String, Range<usize>)> {
        let hits = self.index.resolve(word, ns);
        let Some((_, first)) = hits.first() else {
            return Vec::new();
        };
        let qname = first.qname.clone();

        let mut sites: Vec<(String, Range<usize>)> = Vec::new();
        for (uri, def) in &hits {
            sites.push((uri.to_string(), tail_range(&def.name_range, &def.name)));
        }
        for loc in self.index.references(&qname) {
            let Some(f) = self.index.file(&loc.uri) else {
                continue;
            };
            // Recover the text as written, so the tail length is right.
            let written = f
                .outline
                .refs
                .iter()
                .find(|r| r.range == loc.range)
                .map(|r| r.name.clone())
                .unwrap_or_default();
            let tail = written.rsplit("::").next().unwrap_or(&written).to_string();
            if tail.is_empty() {
                continue;
            }
            sites.push((loc.uri.clone(), tail_range(&loc.range, &tail)));
        }
        sites.sort_by_key(|(uri, range)| (uri.clone(), range.start));
        sites.dedup();
        sites
    }

    /// Reports whether the symbol under the cursor can be renamed, and where.
    fn prepare_rename(&self, p: &TextDocumentPositionParams) -> serde_json::Value {
        let uri = p.text_document.uri.to_string();
        let Some((word, ns)) = self.word_and_ns(&uri, p.position) else {
            return serde_json::Value::Null;
        };
        // A builtin has no definition in the workspace, so there is nothing we could
        // consistently rewrite. Better to decline than to half-rename.
        if self.index.resolve(&word, &ns).is_empty() {
            return serde_json::Value::Null;
        }
        let tail = word.rsplit("::").next().unwrap_or(&word).to_string();
        let Some(doc) = self.docs.get(&uri) else {
            return serde_json::Value::Null;
        };
        let offset = doc
            .line_index()
            .offset(to_linepos(p.position), self.encoding);
        let Some(full) = word_range_at(doc.text(), offset) else {
            return serde_json::Value::Null;
        };
        let range = tail_range(&full, &tail);
        json(PrepareRenameResponse::RangeWithPlaceholder {
            range: to_range(doc.line_index(), range, self.encoding),
            placeholder: tail,
        })
    }

    // `WorkspaceEdit::changes` is defined by lsp-types as `HashMap<Uri, _>`, and
    // `Uri` caches its parsed components behind interior mutability. We never mutate
    // a key, and the map is built from plain strings below, so the lint has nothing
    // to bite on here — but the protocol type leaves no way to avoid it.
    #[allow(clippy::mutable_key_type)]
    fn rename(&self, p: &RenameParams) -> serde_json::Value {
        let uri = p.text_document_position.text_document.uri.to_string();
        let Some((word, ns)) = self.word_and_ns(&uri, p.text_document_position.position) else {
            return serde_json::Value::Null;
        };
        let new_name = p.new_name.trim();
        // A Tcl command name may hold almost anything, but a rename that introduces
        // whitespace or a separator would silently change the meaning of every call
        // site, so refuse rather than corrupt the code.
        if new_name.is_empty() || new_name.contains(char::is_whitespace) || new_name.contains("::")
        {
            eprintln!("refusing rename to {new_name:?}: not a simple command name");
            return serde_json::Value::Null;
        }

        // Grouped by URI *string*: `lsp_types::Uri` has interior mutability (it
        // caches its parsed components), which makes it unsound as a hash key.
        let mut by_uri: HashMap<String, Vec<TextEdit>> = HashMap::new();
        for (site_uri, range) in self.rename_sites(&word, &ns) {
            let Some(range) = self.range_in(&site_uri, range) else {
                continue;
            };
            by_uri.entry(site_uri).or_default().push(TextEdit {
                range,
                new_text: new_name.to_string(),
            });
        }
        let changes: HashMap<Uri, Vec<TextEdit>> = by_uri
            .into_iter()
            .filter_map(|(uri, edits)| parse_uri(&uri).map(|u| (u, edits)))
            .collect();
        if changes.is_empty() {
            return serde_json::Value::Null;
        }
        json(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        })
    }

    /// "Expand selection": the chain of constructs around each requested position.
    fn selection_ranges(&self, p: &SelectionRangeParams) -> serde_json::Value {
        let uri = p.text_document.uri.to_string();
        let Some(doc) = self.docs.get(&uri) else {
            return serde_json::Value::Null;
        };
        let script = tcl_syntax::Script::new(doc.text());
        let li = doc.line_index();

        let out: Vec<SelectionRange> = p
            .positions
            .iter()
            .map(|pos| {
                let offset = li.offset(to_linepos(*pos), self.encoding);
                let chain = tcl_syntax::selection_chain(&script, offset);
                // The protocol nests these outward, so build from the largest range
                // inwards and let each become the next one's parent.
                let mut parent: Option<Box<SelectionRange>> = None;
                for range in chain.iter().rev() {
                    parent = Some(Box::new(SelectionRange {
                        range: to_range(li, range.clone(), self.encoding),
                        parent,
                    }));
                }
                parent.map(|b| *b).unwrap_or(SelectionRange {
                    range: Range_ {
                        start: *pos,
                        end: *pos,
                    },
                    parent: None,
                })
            })
            .collect();
        json(out)
    }

    /// Links for `source` and `package require`.
    ///
    /// A `source` path is resolved relative to the file it appears in, which is what
    /// Tcl does for a relative path when the script is run from its own directory.
    /// A `package require` resolves through the index to whichever file declares the
    /// matching `package provide` — by declaration, not by filename.
    fn document_links(&self, p: &DocumentLinkParams) -> serde_json::Value {
        let uri = p.text_document.uri.to_string();
        let Some(f) = self.index.file(&uri) else {
            return serde_json::Value::Null;
        };
        let base_dir = uri_to_path(&uri).and_then(|p| p.parent().map(|d| d.to_path_buf()));

        let mut out = Vec::new();
        for link in &f.outline.links {
            let Some(range) = self.range_in(&uri, link.range.clone()) else {
                continue;
            };
            let (target, tooltip) = match link.kind {
                tcl_syntax::LinkKind::Source => {
                    let Some(dir) = base_dir.as_ref() else {
                        continue;
                    };
                    let path = dir.join(&link.name);
                    // Only offer a link that actually goes somewhere.
                    if !path.exists() {
                        continue;
                    }
                    (
                        tcl_analysis::path_to_uri(&path),
                        Some(format!("source {}", link.name)),
                    )
                }
                tcl_syntax::LinkKind::PackageRequire => match self.index.provider_of(&link.name) {
                    Some((provider, _)) => (
                        Some(provider.to_string()),
                        Some(format!("package provide {}", link.name)),
                    ),
                    None => continue,
                },
            };
            let Some(target) = target.and_then(|t| parse_uri(&t)) else {
                continue;
            };
            out.push(DocumentLink {
                range,
                target: Some(target),
                tooltip,
                data: None,
            });
        }
        json(out)
    }

    /// Signature help for the command being typed.
    fn signature_help(&self, p: &SignatureHelpParams) -> serde_json::Value {
        let uri = p
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let Some(doc) = self.docs.get(&uri) else {
            return serde_json::Value::Null;
        };
        let offset = doc.line_index().offset(
            to_linepos(p.text_document_position_params.position),
            self.encoding,
        );
        let script = tcl_syntax::Script::new(doc.text());
        let Some(enclosing) = tcl_syntax::command_at(&script, offset) else {
            return serde_json::Value::Null;
        };
        let Some(name) = enclosing.name else {
            // A dynamically-named command has no signature we can know.
            return serde_json::Value::Null;
        };
        // Word 0 is the command name itself; arguments start at 1.
        let active = enclosing.word_index.saturating_sub(1) as u32;

        let ns = namespace_at(&f_symbols(self.index.file(&uri)), offset);
        let sig = self
            .user_signature(&name, &ns)
            .or_else(|| self.builtin_signature(&name, &script, &enclosing.command));
        let Some(sig) = sig else {
            return serde_json::Value::Null;
        };

        let n_params = sig.parameters.as_ref().map(|p| p.len()).unwrap_or(0) as u32;
        json(SignatureHelp {
            signatures: vec![sig],
            active_signature: Some(0),
            active_parameter: Some(active.min(n_params.saturating_sub(1))),
        })
    }

    /// A signature built from a user-defined proc's argument list.
    fn user_signature(&self, name: &str, ns: &str) -> Option<SignatureInformation> {
        let hits = self.index.resolve(name, ns);
        let (_, def) = hits.first()?;
        let args = def.detail.clone().unwrap_or_default();
        let params: Vec<ParameterInformation> = tcl_syntax::split_args(&args)
            .into_iter()
            .map(|p| ParameterInformation {
                label: ParameterLabel::Simple(p),
                documentation: None,
            })
            .collect();
        Some(SignatureInformation {
            label: def.signature(),
            documentation: def.doc.clone().map(Documentation::String),
            parameters: Some(params),
            active_parameter: None,
        })
    }

    /// A signature from the man-page database, including ensemble subcommands.
    fn builtin_signature(
        &self,
        name: &str,
        script: &tcl_syntax::Script,
        cmd: &tcl_tclsys::Command,
    ) -> Option<SignatureInformation> {
        let builtin = self.kb.get(name)?;

        // `string compare ...` — prefer the subcommand's own signature when the
        // second word names one.
        let sub = cmd
            .words()
            .get(1)
            .and_then(|w| w.first())
            .and_then(|t| script.text(t.range()))
            .and_then(|s| self.kb.subcommand(name, s));

        let label = match sub {
            Some(s) => s.signature.clone(),
            None => builtin
                .synopsis
                .first()
                .cloned()
                .unwrap_or_else(|| builtin.name.clone()),
        };
        let doc = match sub {
            Some(s) if !s.doc.is_empty() => s.doc.clone(),
            _ => builtin.summary.clone(),
        };
        Some(SignatureInformation {
            parameters: Some(synopsis_parameters(&label)),
            label,
            documentation: Some(Documentation::String(doc)),
            active_parameter: None,
        })
    }

    /// Formats the buffer with `tclfmt`, if it is available.
    fn formatting(&self, p: &DocumentFormattingParams) -> serde_json::Value {
        let uri = p.text_document.uri.to_string();
        let Some(doc) = self.docs.get(&uri) else {
            return serde_json::Value::Null;
        };
        let Some(exe) = self.config.tclfmt.active() else {
            return json(Vec::<TextEdit>::new());
        };
        match external::tclfmt(doc.text(), exe) {
            Some(formatted) if formatted != doc.text() => {
                let end = doc.line_index().position(doc.text().len(), self.encoding);
                let edit = TextEdit {
                    range: Range_ {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: end.line,
                            character: end.character,
                        },
                    },
                    new_text: formatted,
                };
                json(vec![edit])
            }
            _ => json(Vec::<TextEdit>::new()),
        }
    }
}

// `lsp_types::Range` collides with `std::ops::Range`; alias it for clarity.
use lsp_types::Range as Range_;

fn json<T: serde::Serialize>(value: T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

fn parse_uri(s: &str) -> Option<Uri> {
    s.parse().ok()
}

/// True when the cursor sits inside a `$`-introduced variable reference.
fn wants_variable(text: &str, offset: usize) -> bool {
    let upto = &text[..offset.min(text.len())];
    match upto.rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':')) {
        Some(i) => upto.as_bytes()[i] == b'$',
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Conversions. Everything internal is a byte offset; this is the only boundary
// where LSP positions are produced or consumed.
// ---------------------------------------------------------------------------

fn to_linepos(p: Position) -> LinePos {
    LinePos {
        line: p.line,
        character: p.character,
    }
}

fn to_position(li: &LineIndex, offset: usize, enc: PositionEncoding) -> Position {
    let lp = li.position(offset, enc);
    Position {
        line: lp.line,
        character: lp.character,
    }
}

fn to_range(li: &LineIndex, range: Range<usize>, enc: PositionEncoding) -> Range_ {
    Range_ {
        start: to_position(li, range.start, enc),
        end: to_position(li, range.end, enc),
    }
}

fn finding_to_diagnostic(
    f: external::Finding,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Diagnostic {
    // nagelfar reports a line but no column, so the range covers to end of line;
    // tclint gives a column, so the range starts there.
    let start_char = f.column.unwrap_or(0);
    let line_end = li.offset(
        LinePos {
            line: f.line,
            character: u32::MAX,
        },
        enc,
    );
    let end = to_position(li, line_end, enc);
    Diagnostic {
        range: Range_ {
            start: Position {
                line: f.line,
                character: start_char,
            },
            end,
        },
        severity: Some(match f.severity {
            external::Severity::Error => DiagnosticSeverity::ERROR,
            external::Severity::Warning => DiagnosticSeverity::WARNING,
            external::Severity::Info => DiagnosticSeverity::INFORMATION,
        }),
        code: f.code.map(NumberOrString::String),
        source: Some(f.source.to_string()),
        message: f.message,
        ..Default::default()
    }
}

fn to_lsp_kind(kind: TclKind) -> SymbolKind {
    match kind {
        TclKind::Proc => SymbolKind::FUNCTION,
        TclKind::Namespace => SymbolKind::NAMESPACE,
        TclKind::Class => SymbolKind::CLASS,
        TclKind::Method => SymbolKind::METHOD,
        TclKind::Constructor | TclKind::Destructor => SymbolKind::CONSTRUCTOR,
        TclKind::Variable => SymbolKind::VARIABLE,
    }
}

fn to_document_symbol(li: &LineIndex, s: &Symbol, enc: PositionEncoding) -> DocumentSymbol {
    #[allow(deprecated)]
    DocumentSymbol {
        name: s.name.clone(),
        detail: s.detail.clone(),
        kind: to_lsp_kind(s.kind),
        tags: None,
        deprecated: None,
        range: to_range(li, s.full_range.clone(), enc),
        selection_range: to_range(li, s.name_range.clone(), enc),
        children: Some(
            s.children
                .iter()
                .map(|c| to_document_symbol(li, c, enc))
                .collect(),
        ),
    }
}

fn collect_folds(
    symbols: &[Symbol],
    li: &LineIndex,
    enc: PositionEncoding,
    out: &mut Vec<FoldingRange>,
) {
    for s in symbols {
        let start = li.position(s.full_range.start, enc);
        let end = li.position(s.full_range.end, enc);
        if end.line > start.line {
            out.push(FoldingRange {
                start_line: start.line,
                // The closing brace stays visible when collapsed.
                end_line: end.line.saturating_sub(1),
                kind: Some(FoldingRangeKind::Region),
                ..Default::default()
            });
        }
        collect_folds(&s.children, li, enc, out);
    }
}

/// The symbols of an indexed file, or nothing when it is not indexed.
fn f_symbols(file: Option<&tcl_analysis::FileIndex>) -> Vec<Symbol> {
    file.map(|f| f.outline.symbols.clone()).unwrap_or_default()
}

/// Splits a documented synopsis into parameter labels.
///
/// Man-page synopses read like `lsort ?options? list`, so each whitespace-separated
/// word after the command name is one parameter, `?…?` marking it optional.
fn synopsis_parameters(label: &str) -> Vec<ParameterInformation> {
    label
        .split_whitespace()
        .skip(1)
        .map(|w| ParameterInformation {
            label: ParameterLabel::Simple(w.to_string()),
            documentation: None,
        })
        .collect()
}

/// The text of the line that opened the block a closing brace at `offset` ends.
///
/// Found by asking Tcl's parser which command spans the brace — the command's own
/// first line is the one to align with — rather than by counting braces, which
/// would be fooled by a `}` inside a string.
fn enclosing_open_line<'a>(
    script: &tcl_syntax::Script,
    text: &'a str,
    offset: usize,
) -> Option<&'a str> {
    let cmd = tcl_syntax::command_at(script, offset)?.command;
    if cmd.range.start >= offset {
        return None;
    }
    let line_start = text[..cmd.range.start]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let line_end = text[line_start..]
        .find('\n')
        .map(|i| line_start + i)
        .unwrap_or(text.len());
    text.get(line_start..line_end)
}

/// The namespace a qualified name lives in: `::a::b::C` -> `::a::b`.
fn container_of(qname: &str) -> String {
    match qname.rfind("::") {
        Some(0) | None => "::".to_string(),
        Some(i) => qname[..i].to_string(),
    }
}

/// The innermost proc, method, constructor or destructor containing `offset`.
fn enclosing_def(symbols: &[Symbol], offset: usize) -> Option<&Symbol> {
    let mut best: Option<&Symbol> = None;
    fn rec<'a>(symbols: &'a [Symbol], offset: usize, best: &mut Option<&'a Symbol>) {
        for s in symbols {
            if !s.full_range.contains(&offset) {
                continue;
            }
            if matches!(
                s.kind,
                TclKind::Proc | TclKind::Method | TclKind::Constructor | TclKind::Destructor
            ) {
                *best = Some(s);
            }
            rec(&s.children, offset, best);
        }
    }
    rec(symbols, offset, &mut best);
    best
}

/// The innermost namespace or class whose body contains `offset`.
fn namespace_at(symbols: &[Symbol], offset: usize) -> String {
    fn rec(symbols: &[Symbol], offset: usize, current: &str, best: &mut String) {
        for s in symbols {
            if !s.full_range.contains(&offset) {
                continue;
            }
            let here = match s.kind {
                TclKind::Namespace | TclKind::Class => {
                    *best = s.qname.clone();
                    s.qname.as_str()
                }
                _ => current,
            };
            rec(&s.children, offset, here, best);
        }
    }
    let mut best = "::".to_string();
    rec(symbols, offset, "::", &mut best);
    best
}

/// An option flag the command does not accept.
struct BadOption {
    range: Range<usize>,
    flag: String,
    command: String,
    suggestion: Option<String>,
}

/// Finds `-option` arguments a command is documented not to accept.
///
/// Only commands whose option list is actually known are checked — `Kb::accepts_option`
/// returns `None` otherwise, and "cannot say" must never become "invalid". This is
/// only sound because `.SO` inheritance is resolved when the database is generated;
/// without that a `ttk::button` would appear to reject `-cursor`.
fn unknown_options(outline: &tcl_syntax::Outline, text: &str, kb: &kb::Kb) -> Vec<BadOption> {
    let mut out = Vec::new();
    for call in &outline.calls {
        let Some(cmd) = kb.get(&call.name) else {
            continue;
        };
        if cmd.options.is_empty() {
            continue;
        }
        for arg in &call.args {
            let Some(word) = text.get(arg.clone()) else {
                continue;
            };
            // `--` ends option processing, so nothing after it is a flag.
            if word == "--" {
                break;
            }
            // A leading dash followed by a letter: `-3` is a number, `$-x` is not
            // a literal, and a value that merely starts with `-` is rare enough
            // here that the trade is worth it.
            if !word.starts_with('-') || !word[1..].starts_with(|c: char| c.is_ascii_alphabetic()) {
                continue;
            }
            if kb.accepts_option(&call.name, word) != Some(false) {
                continue;
            }
            out.push(BadOption {
                range: arg.clone(),
                flag: word.to_string(),
                command: cmd.name.clone(),
                suggestion: nearest_option(cmd, word),
            });
        }
    }
    out
}

/// The documented option closest to `flag`, when one is close enough to be a
/// plausible typo rather than a wild guess.
fn nearest_option(cmd: &kb::Command, flag: &str) -> Option<String> {
    let (best, dist) = cmd
        .options
        .iter()
        .map(|o| (o.flag.clone(), edit_distance(&o.flag, flag)))
        .min_by_key(|(_, d)| *d)?;
    // A third of the length, so `-comand` suggests `-command` but `-zzz` suggests
    // nothing.
    (dist * 3 <= flag.len().max(1)).then_some(best)
}

/// Levenshtein distance, iterative with a single row.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Byte ranges of expression arguments that were written without braces.
///
/// `expr $a + $b` is substituted before evaluation: slower, and a value carrying
/// an operator changes what gets evaluated. Bracing is Tcl's standard advice. The
/// same applies to the condition of `if`, `while` and `for`.
fn unbraced_expressions(outline: &tcl_syntax::Outline, text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    for call in &outline.calls {
        let expr_arg = match call.name.trim_start_matches("::") {
            "expr" | "while" | "if" => 0,
            // `for start test next body`
            "for" => 1,
            _ => continue,
        };
        let Some(first) = call.args.get(expr_arg) else {
            continue;
        };
        // A braced word is already correct.
        if text.get(first.clone()).is_some_and(|s| s.starts_with('{')) {
            continue;
        }
        // `expr` concatenates all its arguments, so the whole tail is the
        // expression; the others take exactly one word.
        let end = if call.name.ends_with("expr") {
            call.args.last().map(|a| a.end).unwrap_or(first.end)
        } else {
            first.end
        };
        if end > first.start {
            out.push(first.start..end);
        }
    }
    out
}

/// Offset of the end of the line containing `offset`, just past its newline.
fn line_end_offset(text: &str, offset: usize) -> usize {
    match text[offset.min(text.len())..].find('\n') {
        Some(i) => offset + i + 1,
        None => text.len(),
    }
}

/// A one-file `WorkspaceEdit` change map.
#[allow(clippy::mutable_key_type)] // see the note on `Server::rename`
fn single_change(uri: &str, edits: Vec<TextEdit>) -> HashMap<Uri, Vec<TextEdit>> {
    let mut map = HashMap::new();
    if let Some(u) = parse_uri(uri) {
        map.insert(u, edits);
    }
    map
}

/// The sub-range covering just the final `::`-separated segment of a name.
fn tail_range(full: &Range<usize>, tail: &str) -> Range<usize> {
    let start = full.end.saturating_sub(tail.len()).max(full.start);
    start..full.end
}

/// Byte range of the identifier surrounding an offset.
fn word_range_at(text: &str, offset: usize) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b':';
    let mut start = offset.min(bytes.len());
    while start > 0 && is_word(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = offset.min(bytes.len());
    while end < bytes.len() && is_word(bytes[end]) {
        end += 1;
    }
    (start < end).then_some(start..end)
}

/// The Tcl identifier surrounding a byte offset, including `::` separators.
fn word_at(text: &str, offset: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b':';
    let mut start = offset.min(bytes.len());
    while start > 0 && is_word(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = offset.min(bytes.len());
    while end < bytes.len() && is_word(bytes[end]) {
        end += 1;
    }
    if start >= end {
        return None;
    }
    let word = text.get(start..end)?;
    if word.trim_matches(':').is_empty() {
        None
    } else {
        Some(word.to_string())
    }
}

fn send_diagnostics(
    conn: &Connection,
    uri: &str,
    version: impl Into<Option<i32>>,
    diagnostics: Vec<Diagnostic>,
) -> Result<()> {
    let Some(uri) = parse_uri(uri) else {
        return Ok(());
    };
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version: version.into(),
    };
    conn.sender.send(Message::Notification(Notification {
        method: PublishDiagnostics::METHOD.to_string(),
        params: serde_json::to_value(params)?,
    }))?;
    Ok(())
}

fn cast<R>(req: Request) -> Result<(RequestId, R::Params), ExtractError<Request>>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    req.extract(R::METHOD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcl_syntax::{outline, Script};

    /// A server with exactly one indexed/opened document, for testing the
    /// hover dispatch paths that consult both the index and the Tk docs.
    fn server_with(uri: &str, text: &str) -> Server {
        let mut index = Index::new();
        index.set_file(uri, text);
        let mut docs = HashMap::new();
        docs.insert(uri.to_string(), Document::new(text.to_string(), 1));
        Server {
            docs,
            index,
            kb: kb::builtin(kb::Target::Tcl86),
            config: Config::default(),
            encoding: PositionEncoding::Utf8,
            project_db: None,
            host_db: None,
        }
    }

    fn pos_at(text: &str, uri_needle: &str, line_index: &LineIndex, needle: &str) -> Position {
        let base = text.find(uri_needle).unwrap();
        let at = base + text[base..].find(needle).unwrap();
        to_position(line_index, at, PositionEncoding::Utf8)
    }

    #[test]
    fn widget_hover_defers_to_a_workspace_defined_method() {
        // `$c bump 3` dispatches on a dynamic head like a widget path does,
        // but `bump` IS defined here (a TclOO method). Hover must show the
        // method, not the Tk widget fallback — that was a regression.
        let text = "oo::class create Counter {\n    method bump {by} { return 1 }\n}\nset c [Counter new]\n$c bump 3\n";
        let uri = "file:///prio.tcl";
        let s = server_with(uri, text);
        let li = s.docs.get(uri).unwrap().line_index();
        let p = pos_at(text, "$c", li, "bump");
        assert!(
            s.widget_command_hover(uri, p, "bump", "::").is_none(),
            "a workspace-defined name must not fall through to widget docs"
        );
    }

    #[test]
    fn widget_hover_documents_a_real_widget_subcommand() {
        let text = "set log .main.f.log\n$log.text insert end hi\n";
        let uri = "file:///prio2.tcl";
        let s = server_with(uri, text);
        let li = s.docs.get(uri).unwrap().line_index();
        let p = pos_at(text, "$log", li, "insert");
        let md = s
            .widget_command_hover(uri, p, "insert", "::")
            .expect("`insert` is documented as a text-widget subcommand");
        assert!(md.contains("pathName insert index chars"), "{md}");
        assert!(md.contains("Inserts all of the"), "{md}");
    }

    #[test]
    fn widget_hover_declares_unknown_subcommands() {
        let text = "set w .top\n$w frobnicate x\n";
        let uri = "file:///prio3.tcl";
        let s = server_with(uri, text);
        let li = s.docs.get(uri).unwrap().line_index();
        let p = pos_at(text, "$w", li, "frobnicate");
        let md = s
            .widget_command_hover(uri, p, "frobnicate", "::")
            .expect("a dispatch word always gets a hover");
        assert!(md.contains("No Tk widget man page"), "{md}");
    }

    #[test]
    fn a_proc_named_like_a_subcommand_does_not_shadow_the_widget_doc() {
        // `index.resolve` tail-matches, so a plain `proc configure` (or
        // `get`, `insert`, `delete` — the universal widget subcommands are
        // also the most common proc names) used to suppress the Tk hover
        // for `$w configure` across the ENTIRE workspace. Only a real
        // method should defer.
        let text = concat!(
            "proc helper::configure {opts} { return 1 }\n",
            "set w .top\n",
            "$w configure -bg red\n",
        );
        let uri = "file:///coincide.tcl";
        let s = server_with(uri, text);
        let li = s.docs.get(uri).unwrap().line_index();
        let p = pos_at(text, "$w configure", li, "configure");
        let md = s
            .widget_command_hover(uri, p, "configure", "::")
            .expect("a coincidental proc must not shadow the widget doc");
        assert!(
            md.contains("**Widget command**"),
            "got the proc, not the widget: {md}"
        );
        assert!(!md.contains("helper::configure"), "{md}");
    }

    #[test]
    fn ensemble_subcommand_hover_shows_the_subcommand_not_the_page() {
        // `string match`: word_at yields `match`, which is no command of its
        // own; the ensemble path must find `string`'s page and show the
        // `match` entry — signature and prose — instead of nothing.
        let text = "set x [string match $pat $s]\n";
        let uri = "file:///ens.tcl";
        let s = server_with(uri, text);
        let li = s.docs.get(uri).unwrap().line_index();
        let p = pos_at(text, "string", li, "match");
        let md = s
            .ensemble_subcommand_hover(uri, p, "match")
            .expect("`match` is a `string` subcommand");
        assert!(md.contains("string match ?-nocase? pattern string"), "{md}");
        assert!(md.contains("See if pattern matches"), "{md}");
        assert!(s.ensemble_subcommand_hover(uri, p, "nope").is_none());
    }

    #[test]
    fn widget_hover_shows_every_form_of_a_group_subcommand() {
        // `tag` on a text widget is a GROUP: the man page lists nested forms
        // (`tag add`, `tag configure`, …) under the group .TP. The generator
        // used to drop them, so the hover quoted "The following forms of the
        // tag subcommand are currently supported:" and stopped. The forms
        // must be present now.
        let text = "set log .main.f.log\n$log tag names\n";
        let uri = "file:///group.tcl";
        let s = server_with(uri, text);
        let li = s.docs.get(uri).unwrap().line_index();
        let p = pos_at(text, "$log", li, "tag");
        let md = s
            .widget_command_hover(uri, p, "tag", "::")
            .expect("tag is a documented widget subcommand");
        assert!(md.contains("tag names"), "{md}");
        assert!(md.contains("tag configure"), "{md}");
        // And the intro must not end in a dangling "supported:" heading.
        assert!(
            !md.trim_end().ends_with("currently supported:"),
            "dangling intro: {md}"
        );
    }

    #[test]
    fn word_at_includes_namespace_qualifiers() {
        let text = "puts [::a::b::c 1]";
        let at = text.find("b::c").unwrap();
        assert_eq!(word_at(text, at).as_deref(), Some("::a::b::c"));
    }

    #[test]
    fn word_at_returns_none_in_whitespace() {
        assert_eq!(word_at("a  b", 2), None);
    }

    #[test]
    fn word_at_ignores_bare_colons() {
        assert_eq!(word_at("::", 1), None);
    }

    #[test]
    fn namespace_at_finds_the_innermost_scope() {
        let src = "namespace eval a {\n namespace eval b {\n  set x 1\n }\n}\n";
        let o = outline(&Script::new(src));
        let at = src.find("set x").unwrap();
        assert_eq!(namespace_at(&o.symbols, at), "::a::b");
    }

    #[test]
    fn namespace_at_is_global_outside_any_namespace() {
        let src = "set x 1\nnamespace eval a {\n set y 2\n}\n";
        let o = outline(&Script::new(src));
        assert_eq!(namespace_at(&o.symbols, 0), "::");
    }

    fn bad_options(src: &str) -> Vec<BadOption> {
        let o = outline(&Script::new(src));
        unknown_options(&o, src, &kb::builtin(kb::Target::Tcl86))
    }

    #[test]
    fn flags_an_option_a_widget_does_not_accept() {
        let bad = bad_options("ttk::button .b -nosuchoption x\n");
        assert_eq!(
            bad.len(),
            1,
            "got {:?}",
            bad.iter().map(|b| &b.flag).collect::<Vec<_>>()
        );
        assert_eq!(bad[0].flag, "-nosuchoption");
    }

    /// The whole reason option validation was unsafe before `.SO` inheritance was
    /// resolved: `-cursor` is documented on `ttk_widget`, not on `ttk_button`.
    #[test]
    fn accepts_options_inherited_through_the_so_block() {
        assert!(
            bad_options("ttk::button .b -cursor watch -takefocus 1 -style X\n").is_empty(),
            "inherited standard options must be accepted"
        );
        assert!(
            bad_options("button .b -font fixed -padx 2 -cursor watch\n").is_empty(),
            "classic widgets inherit from options(n)"
        );
    }

    #[test]
    fn says_nothing_about_commands_whose_options_are_undocumented() {
        // `lsort` takes `-unique` and friends, but they come from prose, not `.OP`.
        assert!(bad_options("lsort -unique -integer {1 2}\n").is_empty());
    }

    #[test]
    fn ignores_negative_numbers_and_the_double_dash() {
        assert!(bad_options("ttk::button .b -width -3\n").is_empty());
        assert!(bad_options("ttk::button .b -- -whatever\n").is_empty());
    }

    #[test]
    fn suggests_a_near_miss_but_not_a_wild_guess() {
        let bad = bad_options("ttk::button .b -commnd x\n");
        assert_eq!(bad[0].suggestion.as_deref(), Some("-command"));
        let far = bad_options("ttk::button .b -qqqqqqqq x\n");
        assert_eq!(far[0].suggestion, None, "no plausible correction");
    }

    /// The guard that makes option validation defensible: Tk's own library is
    /// correct Tcl/Tk by construction, so anything flagged there is our bug. A
    /// diagnostic users learn to distrust is worse than no diagnostic at all.
    #[test]
    fn reports_nothing_on_tk_own_library() {
        let Ok(root) = std::env::var("TCL_LSP_TK_LIBRARY") else {
            eprintln!("TCL_LSP_TK_LIBRARY unset; skipping the false-positive guard");
            return;
        };
        let Ok(entries) = std::fs::read_dir(&root) else {
            return;
        };
        let kb = kb::builtin(kb::Target::Tcl86);
        let mut findings = Vec::new();
        let mut files = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "tcl") {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                files += 1;
                let o = outline(&Script::new(&text));
                for bad in unknown_options(&o, &text, &kb) {
                    findings.push(format!(
                        "{}: {} on {}",
                        path.display(),
                        bad.flag,
                        bad.command
                    ));
                }
            }
        }
        assert!(files > 5, "expected Tk's library at {root}");
        assert!(
            findings.is_empty(),
            "{} false positives over {files} files:\n{}",
            findings.len(),
            findings.join("\n")
        );
    }

    #[test]
    fn edit_distance_is_symmetric_and_zero_for_equal() {
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("-command", "-commnd"), 1);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
    }

    #[test]
    fn variable_completion_triggers_after_a_dollar() {
        let text = "puts $na";
        assert!(wants_variable(text, text.len()));
        assert!(!wants_variable("puts na", 7));
    }

    #[test]
    fn variable_completion_does_not_trigger_mid_command() {
        assert!(!wants_variable("set x 1\nputs ", 13));
    }
}
