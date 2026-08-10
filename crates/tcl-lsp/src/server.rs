//! Server state and request dispatch.

use std::collections::HashMap;
use std::ops::Range;

use anyhow::Result;
use lsp_server::{Connection, ExtractError, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    notification::{
        DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument, DidOpenTextDocument,
        DidSaveTextDocument, Notification as _, PublishDiagnostics,
    },
    request::{
        Completion, DocumentHighlightRequest, DocumentLinkRequest, DocumentSymbolRequest,
        FoldingRangeRequest, Formatting, GotoDefinition, HoverRequest, References, Request as _,
        SelectionRangeRequest, SignatureHelpRequest, WorkspaceSymbolRequest,
    },
    *,
};
use tcl_analysis::{kb, uri_to_path, Index};
use tcl_syntax::{Document, LineIndex, LinePos, PositionEncoding, Symbol, SymbolKind as TclKind};

use crate::external;

pub struct Server {
    /// Buffers the editor has open. These override whatever is on disk.
    docs: HashMap<String, Document>,
    index: Index,
    /// Documentation for Tcl's and Tk's own commands.
    kb: kb::Kb,
    encoding: PositionEncoding,
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
    connection.initialize_finish(id, serde_json::to_value(init_result)?)?;

    // Which command set to analyse against. Independent of the libtcl this binary
    // is linked to — 8.6 and 9.0 parse alike — so it is a per-workspace setting.
    let target = std::env::var("TCL_LSP_TCL_VERSION")
        .map(|v| kb::Target::parse(&v))
        .unwrap_or_default();
    let kb = kb::builtin(target);
    eprintln!("loaded {} builtin commands for {target:?}", kb.len());

    let mut server = Server {
        docs: HashMap::new(),
        index: Index::new(),
        kb,
        encoding,
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
            for f in external::nagelfar(doc.text())
                .into_iter()
                .chain(external::tclint(doc.text()))
            {
                diags.push(finding_to_diagnostic(f, li, self.encoding));
            }
        }
        send_diagnostics(conn, uri, doc.version(), diags)
    }

    fn own_diagnostics(&self, doc: &Document) -> Vec<Diagnostic> {
        let outline = doc.outline();
        let text = doc.text();
        outline
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
            .collect()
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
            None => match self.kb.get(&word) {
                Some(builtin) => builtin.hover_markdown(),
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
        match external::tclfmt(doc.text()) {
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
