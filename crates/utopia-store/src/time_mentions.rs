//! Time phrases a statement mentions (0044 first slice, #729) and how they are read (0045).
//!
//! **A time mention is the document's own words; it is never a date the model computed.** "last winter", "within 30 days of signing the contract"
//! "Q3": what gets stored is these words and their character offsets in `chunks.text`, not some model's written
//! `TIMESTAMPTZ`. The words are kept and the reading can change; if a reading was wrong, fix the reading rather than deleting the document and re-extracting.
//!
//! Reading has two layers (0045 decision 2: the model reads, code computes):
//!
//! - **Interpretation** (`shape` / `reference` / `granularity`) is given by the model: shape (point, interval, until,
//!   duration, end with no date given), reference (an absolute value as written, or an anchor plus offset, or none), and the ladder rung the words name.
//!   The field values are not checked here: they are the vocabulary of the extraction contract, and the database just stores them.
//! - **Solved values** (`grade` / `resolved_*`) are computed by code from the interpretation: each end's moment and precision, the ladder and
//!   `facts` share one table (0024), with the end side having one extra `unknown`. A is an absolute date stated in the text, B is computed from the document's own anchor,
//!   C could not be computed (all four columns empty: what cannot be computed waits, decision 4). `resolved_at`
//!   is the most recent computation time: re-compute when the anchor arrives later.
//!
//! A statement's `when` and `ended` are each one mention (`role`); the same words can appear twice. The statement's position on the world
//! axis (`facts.valid_*`) is written from the solved values by [`crate::graph::set_open_validity`], not written here.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::HashMap;
use utopia_core::AppResult;
use uuid::Uuid;

/// A time mention: which statement, which chunk, the words copied, their character offset in the chunk, and their reading (if any)
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct TimeMention {
    pub id: Uuid,
    pub fact_id: Uuid,
    pub chunk_id: Uuid,
    /// The words copied verbatim
    pub text: String,
    /// Character offset in `chunks.text` (not bytes)
    pub char_start: i32,
    /// Which slot of the statement it comes from: `when` / `ended`
    pub role: String,
    /// The model's interpretation: shape, reference, granularity. Empty if not yet read
    pub shape: Option<String>,
    pub reference: Option<serde_json::Value>,
    pub granularity: Option<String>,
    /// Code's solved values: A absolute, B computed from an anchor, C not computed. Empty if not yet computed
    pub grade: Option<String>,
    pub resolved_from: Option<DateTime<Utc>>,
    pub resolved_from_precision: Option<String>,
    pub resolved_to: Option<DateTime<Utc>>,
    /// The end side has one extra `unknown`: ended, date unknown
    pub resolved_to_precision: Option<String>,
}

const COLUMNS: &str =
    "id, fact_id, chunk_id, text, char_start, role, shape, reference, granularity,
                       grade, resolved_from, resolved_from_precision, resolved_to,
                       resolved_to_precision";

/// Record a time mention. `char_start` is a character offset, **searched out by the server within the chunk**, not taken from the model's reported value;
/// `role` is which slot of the statement it comes from (`when` / `ended`). The same statement at the same position in the same chunk,
/// for the same slot, has only one row; recording again returns that row's id
pub async fn record(
    pool: &PgPool,
    kb_id: Uuid,
    fact_id: Uuid,
    chunk_id: Uuid,
    text: &str,
    char_start: i32,
    role: &str,
) -> AppResult<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO time_mentions (id, kb_id, fact_id, chunk_id, text, char_start, role)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (fact_id, chunk_id, char_start, role) DO UPDATE SET text = time_mentions.text
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(kb_id)
    .bind(fact_id)
    .bind(chunk_id)
    .bind(text)
    .bind(char_start)
    .bind(role)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Time phrases each statement in a batch mentions, fetched by fact id; within a chunk sorted by position of occurrence
pub async fn for_facts(
    pool: &PgPool,
    fact_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<TimeMention>>> {
    let mut out: HashMap<Uuid, Vec<TimeMention>> = HashMap::new();
    if fact_ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<TimeMention> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM time_mentions
          WHERE fact_id = ANY($1)
          ORDER BY fact_id, chunk_id, char_start, role"
    ))
    .bind(fact_ids)
    .fetch_all(pool)
    .await?;
    for m in rows {
        out.entry(m.fact_id).or_default().push(m);
    }
    Ok(out)
}

/// A pending mention awaiting reading: the words, position, slot, and the sentence it sits in; the input when solving walks through the document in its context
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct MentionToResolve {
    pub id: Uuid,
    pub fact_id: Uuid,
    pub chunk_id: Uuid,
    pub text: String,
    pub char_start: i32,
    pub role: String,
    /// The quote of this statement in this chunk (`fact_evidence.quote`); when the evidence has no quote, fall back to the whole chunk's words
    pub sentence: String,
}

/// All pending mentions in one document: each live open statement's every mention on this document's live chunks,
/// in chunk order, then position within the chunk; solving reads the document forward so anchors can carry from one chunk to the next (0045 decision 3).
/// Chunks replaced by a new version are not included: their words are no longer this document's words
pub async fn for_document(pool: &PgPool, document_id: Uuid) -> AppResult<Vec<MentionToResolve>> {
    Ok(sqlx::query_as(
        "SELECT m.id, m.fact_id, m.chunk_id, m.text, m.char_start, m.role,
                COALESCE(fe.quote, c.text) AS sentence
           FROM time_mentions m
           JOIN chunks c ON c.id = m.chunk_id
           JOIN facts f ON f.id = m.fact_id
           LEFT JOIN fact_evidence fe ON fe.fact_id = m.fact_id AND fe.chunk_id = m.chunk_id
          WHERE c.document_id = $1 AND c.superseded_at IS NULL
            AND f.layer = 'open' AND f.invalidated_at IS NULL
          ORDER BY c.seq, m.char_start, m.role",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await?)
}

/// The model's interpretation of this mention (0045 decision 2): shape, reference, granularity, stored as given. The values are the vocabulary of the extraction contract,
/// which the database does not check; date arithmetic is not done here
pub async fn set_interpretation(
    pool: &PgPool,
    id: Uuid,
    shape: &str,
    reference: &serde_json::Value,
    granularity: &str,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE time_mentions SET shape = $2, reference = $3, granularity = $4 WHERE id = $1",
    )
    .bind(id)
    .bind(shape)
    .bind(reference)
    .bind(granularity)
    .execute(pool)
    .await?;
    Ok(())
}

/// Code's solved value for this mention: grade and each end's moment and precision; `resolved_at` records this moment. Values are first truncated to precision
/// ([`crate::graph::truncate_to`]), so the stored value and precision say the same thing, and the database's CHECK accepts it.
/// Grade C has all four columns empty; the end side `(None, Some("unknown"))` means ended, date unknown
pub async fn set_resolution(
    pool: &PgPool,
    id: Uuid,
    grade: &str,
    from: Option<DateTime<Utc>>,
    from_precision: Option<&str>,
    to: Option<DateTime<Utc>>,
    to_precision: Option<&str>,
) -> AppResult<()> {
    let from = from.map(|t| crate::graph::truncate_to(t, from_precision));
    let to = to.map(|t| crate::graph::truncate_to(t, to_precision));
    sqlx::query(
        "UPDATE time_mentions
            SET grade = $2, resolved_from = $3, resolved_from_precision = $4,
                resolved_to = $5, resolved_to_precision = $6, resolved_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(grade)
    .bind(from)
    .bind(from_precision)
    .bind(to)
    .bind(to_precision)
    .execute(pool)
    .await?;
    Ok(())
}
