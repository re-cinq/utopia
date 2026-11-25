//! Full-database export as RDF (0020).
//!
//! **Send as you query**: once a page of facts is serialized, the accumulated bytes are sent out; the whole file is never assembled in memory.
//! A database of a hundred thousand facts is exactly the kind that most needs exporting, and also exactly the kind where
//! "first assemble one String" would kill the service.
//!
//! **One snapshot**: the health check, the vocabulary and every per-page query run in the same read-only REPEATABLE READ
//! transaction. If each page took its own connection, a rule committed after the vocabulary was sent would leave
//! a `wasGeneratedBy` pointing at it in a derived page -- a reference to an ontology that does not exist would appear in the graph.
//!
//! An error midway can only truncate: the HTTP headers have already gone out. So the error goes to the log, and the client gets
//! a file cut short; this is better than assemble-then-send, which cannot be served at all on the same database.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use utopia_core::AppError;
use uuid::Uuid;

use super::documents_routes::require_reader_kb;
use crate::auth::SessionOrToken;
use crate::error::ApiResult;
use crate::rdf::{self, Format, Names, SharedBuf, Sink};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct ExportQuery {
    /// `turtle` (default) or `jsonld`
    #[serde(default)]
    pub format: Option<String>,
    /// Public base address used to mint IRIs. If not given, a URN is used -- stable, and it does not pretend to know where it is deployed.
    #[serde(default)]
    pub base: Option<String>,
}

pub async fn export(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path(kb_id): Path<Uuid>,
    Query(q): Query<ExportQuery>,
) -> ApiResult<Response> {
    let kb = require_reader_kb(&state, &reader, kb_id).await?;
    let format = Format::parse(q.format.as_deref()).ok_or_else(|| {
        AppError::Validation("Unknown `format` (expected turtle or jsonld)".into())
    })?;
    let names = Names::new(kb_id, q.base.as_deref()).map_err(AppError::Validation)?;

    // Export is an action that takes the **whole database off this machine**: the ledger must record it (same reasoning as 0014);
    // for a token export, also record which token (0014 decision 7).
    // It must finish writing and release the connection before the snapshot transaction: the transaction below holds one for the whole stream's duration,
    // and holding it while going back for another connection would starve concurrent exports on the smallest pool (two connections) of each other
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        reader.user.id,
        "kb.exported",
        "knowledge_base",
        Some(kb_id),
        utopia_store::audit::via_token(
            serde_json::json!({ "format": format.extension() }),
            reader.token_id(),
        ),
    )
    .await;

    // The whole export holds one connection in a read-only REPEATABLE READ transaction: the preflight below and every page query in the stream
    // read the same snapshot, and the transaction lives until the stream ends
    let mut tx = state.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;

    // Provenance chains that cross databases are always refused (0070 blocks new rows, here we block existing bad rows): better to send not a single byte
    // than attach another database's objects to this database's IRIs -- the file would look complete but dangle.
    // Stop this before the stream starts: the client gets a deterministic error, not a truncated file
    utopia_store::export::provenance_integrity(&mut tx, kb_id).await?;
    // Same discipline for epistemic state (0062): cross-database references, stored values outside the vocabulary, and criteria that open
    // violations cannot resolve are all stopped before the first byte is sent
    utopia_store::export::contest_integrity(&mut tx, kb_id).await?;

    let stream = async_stream::try_stream! {
        let buf = SharedBuf::default();
        let mut sink = Sink::new(format, buf.clone());

        let classes = utopia_store::export::classes(&mut tx, kb_id).await.map_err(io)?;
        let relations = utopia_store::export::relations(&mut tx, kb_id).await.map_err(io)?;
        let vocab = rdf::vocabulary(&names, &classes, &relations);
        for c in &classes {
            rdf::emit_class(&mut sink, &vocab, c)?;
        }
        for r in &relations {
            rdf::emit_relation(&mut sink, &vocab, r)?;
        }
        yield axum::body::Bytes::from(buf.take());

        let mut after = None;
        loop {
            let page = utopia_store::export::documents_page(&mut tx, kb_id, after).await.map_err(io)?;
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            for d in &page {
                rdf::emit_document(&mut sink, &names, d)?;
            }
            yield axum::body::Bytes::from(buf.take());
        }

        let mut after = None;
        loop {
            let page = utopia_store::export::entities_page(&mut tx, kb_id, after).await.map_err(io)?;
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            for e in &page {
                rdf::emit_entity(&mut sink, &names, &vocab, e)?;
            }
            yield axum::body::Bytes::from(buf.take());
        }

        // Current triples are judged as of the moment of export; the whole file uses one now:
        // if the current time were fetched per chunk, the first and second halves of the same file would be written with two different nows
        let now = chrono::Utc::now();
        let mut after = None;
        loop {
            let page = utopia_store::export::facts_page(&mut tx, kb_id, after).await.map_err(io)?;
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            for f in &page {
                rdf::emit_fact(&mut sink, &names, &vocab, f, now)?;
            }
            yield axum::body::Bytes::from(buf.take());
        }

        let mut after = None;
        loop {
            let page = utopia_store::export::derived_page(&mut tx, kb_id, after).await.map_err(io)?;
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            for d in &page {
                rdf::emit_derived(&mut sink, &names, &vocab, d)?;
            }
            yield axum::body::Bytes::from(buf.take());
        }

        // Epistemic state (0062): disputes and violations point at fact IRIs, so they come after facts and derivations
        let mut after = None;
        loop {
            let page = utopia_store::export::fact_conflicts_page(&mut tx, kb_id, after).await.map_err(io)?;
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            for c in &page {
                rdf::emit_fact_conflict(&mut sink, &names, c)?;
            }
            yield axum::body::Bytes::from(buf.take());
        }

        let mut after = None;
        loop {
            let page = utopia_store::export::axiom_violations_page(&mut tx, kb_id, after).await.map_err(io)?;
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            for v in &page {
                rdf::emit_axiom_violation(&mut sink, &names, &vocab, v)?;
            }
            yield axum::body::Bytes::from(buf.take());
        }

        sink.finish()?;
        yield axum::body::Bytes::from(buf.take());
    };

    let filename = format!("{}.{}", slug(&kb.name), format.extension());
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(format.content_type()),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok((
        StatusCode::OK,
        headers,
        Body::from_stream(Box::pin(stream)
            as std::pin::Pin<
                Box<
                    dyn futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>>
                        + Send,
                >,
            >),
    )
        .into_response())
}

/// An error from the database mid-stream can only become an io error -- the response headers have already been sent, so the status code cannot be changed.
fn io(e: AppError) -> std::io::Error {
    tracing::error!(error = %e, "export interrupted");
    std::io::Error::other(e.to_string())
}

/// Short name used for the file name. Non-ASCII characters do not go into the Content-Disposition filename,
/// so a database name in Chinese would turn into a string of question marks there
fn slug(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "export".into()
    } else {
        trimmed
    }
}
