//! Closes facts that a source no longer states (RC-004).
//!
//! A source that pushes statements re-pushes them each time, and old chunks get superseded; a fact that is no longer stated survives only as evidence resting on the old chunks,
//! and the "pending confirmation" that lands in review ([`crate::review::unconfirmed_fact`]) never moves. Here they are closed in batches:
//! the end is the moment its last evidence was superseded, via [`crate::temporal::close_superseded`] — the same void + rewrite a person's "close" click in review does;
//! history is kept, nothing is revoked or rejected.

use sqlx::PgPool;
use utopia_core::AppResult;
use uuid::Uuid;

/// Result of a batch. `remaining` is how many can still be closed after this batch, the full count (starting from the source's documents, via
/// `fact_evidence_document_idx`); `skipped` is how many start after that moment, cannot be closed, and are left for a person
pub struct SourceClosing {
    pub closed: u64,
    pub skipped: i64,
    pub remaining: i64,
}

/// Pending facts in the scope `$2` (one source, or one document) of store `$1`: all evidence comes from the source's documents and all of it is superseded; excludes name facts
/// (`known_as` is written by Utopia itself); typed rows with a statement source are excluded — they follow their statement, and once the statement is closed,
/// materialization rebuilds them from the closed statement. `inverted`: the start (truncated to its precision) is later than the end,
/// the same comparison that `close_superseded` uses to refuse `interval_inverted`
fn candidates(scope: Scope) -> String {
    let key = scope.column();
    format!(
        "WITH ev AS (
           SELECT fe.fact_id, date_trunc('second', max(c.superseded_at), 'UTC') AS ended
             FROM documents d
             JOIN fact_evidence fe ON fe.document_id = d.id
             JOIN chunks c ON c.id = fe.chunk_id
            WHERE d.kb_id = $1 AND {key} = $2
            GROUP BY fe.fact_id
           HAVING bool_and(c.superseded_at IS NOT NULL))
         SELECT f.id, f.layer, ev.ended,
                COALESCE(date_trunc(CASE WHEN f.valid_from_precision IN
                                         ('year','month','day','hour','minute','second')
                                         THEN f.valid_from_precision ELSE 'microseconds' END,
                                    f.valid_from, 'UTC') > ev.ended, false) AS inverted
           FROM ev JOIN facts f ON f.id = ev.fact_id
          WHERE f.kb_id = $1 AND f.invalidated_at IS NULL AND {unconfirmed} AND {not_a_name}
            AND NOT EXISTS (SELECT 1 FROM fact_evidence fe
                              JOIN chunks c ON c.id = fe.chunk_id
                              JOIN documents d ON d.id = c.document_id
                             WHERE fe.fact_id = f.id AND {key} IS DISTINCT FROM $2)
            AND (f.layer = 'open'
                 OR NOT EXISTS (SELECT 1 FROM typed_fact_sources s WHERE s.fact_id = f.id))",
        unconfirmed = crate::review::unconfirmed_fact(),
        not_a_name = crate::names::not_a_name("f"),
        key = key,
    )
}

/// Closes at most `limit`, open statements first. Once a statement is closed, a typed recompute is queued (the same as rejecting a statement,
/// `graph::reject_fact`): the old statement is voided, the rows it materialized step down in the recompute, and are rebuilt from the closed statement
pub async fn close_of_source(
    pool: &PgPool,
    kb_id: Uuid,
    source_id: Uuid,
    limit: i64,
) -> AppResult<SourceClosing> {
    close_in(pool, kb_id, Scope::Source, source_id, limit).await
}

/// After a pushed entry (one document) is extracted into a new version, the facts it no longer states are closed (RC-006): all evidence rests on
/// chunks of this document that were superseded, with no evidence elsewhere. Closes them batch by batch until none remain; the number of rounds is bounded
pub async fn close_of_document(
    pool: &PgPool,
    kb_id: Uuid,
    document_id: Uuid,
) -> AppResult<SourceClosing> {
    let mut total = SourceClosing {
        closed: 0,
        skipped: 0,
        remaining: 1,
    };
    for _ in 0..MAX_DOCUMENT_ROUNDS {
        if total.remaining == 0 {
            break;
        }
        let batch = close_in(pool, kb_id, Scope::Document, document_id, DOCUMENT_BATCH).await?;
        total.closed += batch.closed;
        total.skipped = batch.skipped;
        total.remaining = batch.remaining;
    }
    Ok(total)
}

const DOCUMENT_BATCH: i64 = 1000;
const MAX_DOCUMENT_ROUNDS: usize = 50;

#[derive(Clone, Copy)]
enum Scope {
    Source,
    Document,
}

impl Scope {
    fn column(self) -> &'static str {
        match self {
            Scope::Source => "d.source_id",
            Scope::Document => "d.id",
        }
    }
}

async fn close_in(
    pool: &PgPool,
    kb_id: Uuid,
    scope: Scope,
    scope_id: Uuid,
    limit: i64,
) -> AppResult<SourceClosing> {
    let due: Vec<(Uuid, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(&format!(
        "SELECT id, layer, ended FROM ({}) c WHERE NOT inverted
          ORDER BY layer = 'typed', id LIMIT $3",
        candidates(scope)
    ))
    .persistent(false)
    .bind(kb_id)
    .bind(scope_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut closed = 0;
    let mut statements = false;
    for (fact_id, layer, ended) in &due {
        // None = voided or closed by someone else in the meantime, so not counted in this batch
        if crate::temporal::close_superseded(pool, *fact_id, *ended, "second")
            .await?
            .is_some()
        {
            closed += 1;
            statements |= layer == "open";
        }
    }
    if statements {
        crate::jobs::enqueue_unless_queued(
            pool,
            crate::phrase_bindings::MATERIALIZE_KIND,
            serde_json::json!({ "kb_id": kb_id }),
        )
        .await?;
    }
    let (remaining, skipped): (i64, i64) = sqlx::query_as(&format!(
        "SELECT count(*) FILTER (WHERE NOT inverted), count(*) FILTER (WHERE inverted)
           FROM ({}) c",
        candidates(scope)
    ))
    .persistent(false)
    .bind(kb_id)
    .bind(scope_id)
    .fetch_one(pool)
    .await?;
    Ok(SourceClosing {
        closed,
        skipped,
        remaining,
    })
}
