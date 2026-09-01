//! The rua language server.
//!
//! Everything it says comes from the same front end the interpreter runs on:
//! the lexer for highlighting, the parser and its recovery for diagnostics,
//! and a live `Vm` for what the standard library contains. A second, drifting
//! description of the language is the thing this exists to avoid.

use rua_lsp::analysis;

use lsp_server::{Connection, ExtractError, Message, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use lsp_types::*;

fn main() -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
    // stdio is the transport an editor starts us with; anything printed to
    // stdout that is not a message would corrupt it, so logging goes to stderr
    eprintln!("rua-lsp {}", env!("CARGO_PKG_VERSION"));
    let (connection, io_threads) = Connection::stdio();
    let caps = serde_json::to_value(server_capabilities())?;
    let init = connection.initialize(caps)?;
    let _: InitializeParams = serde_json::from_value(init)?;
    serve(&connection)?;
    io_threads.join()?;
    Ok(())
}

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        // whole documents: a rua file is small, and re-reading one is a
        // fraction of a millisecond
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".to_string(), ":".to_string()]),
            ..Default::default()
        }),
        document_symbol_provider: Some(OneOf::Left(true)),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        // `prepareProvider` is what lets the editor grey the command out on a
        // name this server will not rename, rather than offering and failing
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        semantic_tokens_provider: Some(
            SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: analysis::TOKEN_TYPES.to_vec(),
                    token_modifiers: Vec::new(),
                },
                full: Some(SemanticTokensFullOptions::Bool(true)),
                ..Default::default()
            }),
        ),
        ..Default::default()
    }
}

fn serve(connection: &Connection) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
    let mut world = analysis::World::new();
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                let response = answer(&mut world, req);
                connection.sender.send(Message::Response(response))?;
            }
            Message::Notification(note) => {
                if let Some(uri) = world.apply(&note) {
                    let diagnostics = world.diagnostics(&uri);
                    let params = PublishDiagnosticsParams {
                        uri: uri.clone(),
                        diagnostics,
                        version: None,
                    };
                    connection.sender.send(Message::Notification(lsp_server::Notification {
                        method: lsp_types::notification::PublishDiagnostics::METHOD.to_string(),
                        params: serde_json::to_value(params)?,
                    }))?;
                }
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

/// One request, answered. A request this server does not implement gets an
/// empty answer rather than an error: an editor asks about everything.
fn answer(world: &mut analysis::World, req: Request) -> Response {
    let id = req.id.clone();
    match req.method.as_str() {
        request::HoverRequest::METHOD => {
            reply(id, cast::<request::HoverRequest>(req).map(|(_, p)| world.hover(&p)))
        }
        request::Completion::METHOD => {
            reply(id, cast::<request::Completion>(req).map(|(_, p)| world.complete(&p)))
        }
        request::DocumentSymbolRequest::METHOD => reply(
            id,
            cast::<request::DocumentSymbolRequest>(req).map(|(_, p)| world.symbols(&p)),
        ),
        request::SemanticTokensFullRequest::METHOD => reply(
            id,
            cast::<request::SemanticTokensFullRequest>(req).map(|(_, p)| world.semantic_tokens(&p)),
        ),
        request::GotoDefinition::METHOD => reply(
            id,
            cast::<request::GotoDefinition>(req).map(|(_, p)| {
                let at = p.text_document_position_params;
                world
                    .definition_at(&at.text_document.uri, at.position)
                    .map(GotoDefinitionResponse::Scalar)
            }),
        ),
        request::References::METHOD => reply(
            id,
            cast::<request::References>(req).map(|(_, p)| {
                let at = p.text_document_position;
                world.references_at(&at.text_document.uri, at.position)
            }),
        ),
        request::DocumentHighlightRequest::METHOD => reply(
            id,
            cast::<request::DocumentHighlightRequest>(req).map(|(_, p)| {
                let at = p.text_document_position_params;
                let out: Vec<DocumentHighlight> = world
                    .references_at(&at.text_document.uri, at.position)
                    .into_iter()
                    .map(|l| DocumentHighlight { range: l.range, kind: None })
                    .collect();
                out
            }),
        ),
        request::PrepareRenameRequest::METHOD => reply(
            id,
            cast::<request::PrepareRenameRequest>(req).map(|(_, p)| {
                world
                    .prepare_rename_at(&p.text_document.uri, p.position)
                    .map(PrepareRenameResponse::Range)
            }),
        ),
        request::Rename::METHOD => {
            let out = cast::<request::Rename>(req).and_then(|(_, p)| {
                let at = p.text_document_position;
                world.rename_at(&at.text_document.uri, at.position, &p.new_name)
            });
            reply(id, out)
        }
        _ => Response { id, result: Some(serde_json::Value::Null), error: None },
    }
}

fn reply<T: serde::Serialize>(id: RequestId, out: Result<T, String>) -> Response {
    match out.and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string())) {
        Ok(result) => Response { id, result: Some(result), error: None },
        Err(message) => Response {
            id,
            result: None,
            error: Some(lsp_server::ResponseError {
                code: lsp_server::ErrorCode::InvalidParams as i32,
                message,
                data: None,
            }),
        },
    }
}

fn cast<R>(req: Request) -> Result<(RequestId, R::Params), String>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    req.extract(R::METHOD).map_err(|e| match e {
        ExtractError::MethodMismatch(r) => format!("not {}: {}", R::METHOD, r.method),
        ExtractError::JsonError { error, .. } => error.to_string(),
    })
}
