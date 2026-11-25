//! Facts awaiting a person's nod (see `docs/decisions/0015`; the table is in `migrations/0018`).
//!
//! A triple extracted from a memory lands here first, **not in `facts`**: not on the graph, not used in retrieval,
//! not used in inference. Once a person nods, after seeing "source sentence on top, triple below" in the conversation or on the Review page,
//! it goes into the ledger via the same path as extraction (`insert_fact` + evidence + temporal reconciliation).
//!
//! **Why a separate table rather than a column on `facts`**: over fifty queries filter by `invalidated_at IS NULL`
//! to pull live facts, and missing the filter in just one place lets an unconfirmed fact slip into the graph, and the whole reason this table exists
//! is to prevent exactly that. Forgetting to read it has one consequence: "the pending queue is invisible",
//! and that is the right direction to fail in. 0013's `derived_facts` makes the same call.
//!
//! **Only blocks interactive single-item writes.** Bulk ingestion still writes optimistically plus after-the-fact review; that path does not go through here.
//!
//! **Two shapes** (0044 first slice, migration 0062): an empty `phrase` is the old ontology-backed shape (predicate, computed
//! `valid_*`); nodding goes through `insert_fact` + temporal reconciliation; a non-empty `phrase` is an open statement: the document's own
//! relation phrase, attributes keyed by role word, the time phrases copied verbatim, and the quote's position in the block; nodding goes
//! through `insert_open_statement`, which does not consult the ontology and does not touch the world axis.

use sqlx::PgPool;
use utopia_core::models::PendingFactView;
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

use crate::graph::{FactObject, Validity};

/// A proposal handed over by the extractor. Fields line up with `insert_fact` / `insert_value_fact`,
/// with extras `proposed_predicate` (the model's own words), `chunk_id` (the memory sentence), `proposed_by` (who said it).
pub struct Proposal<'a> {
    pub kb_id: Uuid,
    pub subject_id: Uuid,
    /// None = the ontology has no matching relation (0010). **This empty is exactly what the person needs to see**
    pub predicate_id: Option<Uuid>,
    pub object_id: Option<Uuid>,
    pub object_value: Option<&'a serde_json::Value>,
    pub proposed_predicate: Option<&'a str>,
    pub validity: Validity<'a>,
    pub confidence: f32,
    pub chunk_id: Uuid,
    pub proposed_by: Option<Uuid>,
    /// Which token said it, for a statement proposed via MCP (0026). Empty for web chat
    pub proposed_token: Option<Uuid>,
    /// Some = an open statement (0044): the document's own relation phrase, as written. Once nodded it becomes
    /// `facts.layer = 'open'`, with no predicate and no `valid_*` written. None = old ontology-backed shape
    pub phrase: Option<&'a str>,
    /// Attributes keyed by document role words: `[{"role": "amount", "value": "$2 million"} |
    /// {"role": "to", "entity_id": "<uuid>"}]`. Only meaningful when `phrase` is non-empty
    pub qualifiers: Option<&'a serde_json::Value>,
    /// Time phrases the statement mentions, copied verbatim, never a date: `[{"text": "March 4, 2011", "char_start": 143}]`,
    /// the offset is a character offset in `chunks.text`. Only meaningful when `phrase` is non-empty
    pub time_words: Option<&'a serde_json::Value>,
    /// The quote's character offset `(start, end)` in `chunks.text`, computed by the server searching the text, never taken from what the model
    /// reported; if not located, pass None. Only meaningful when `phrase` is non-empty
    pub quote_span: Option<(i32, i32)>,
}

/// Where the proposal goes. None of the three "not proposed" states is an error: re-extracting a memory sentence computes the same triple again,
/// and without blocking, every extraction would erase the person's decision once more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Proposed(Uuid),
    /// The graph already has the same live fact: no need to ask again
    AlreadyAsserted,
    /// Already waiting in the queue
    AlreadyPending,
    /// The person rejected this triple (`rejected_facts`)
    Rejected,
}

pub async fn propose(pool: &PgPool, p: Proposal<'_>) -> AppResult<Outcome> {
    // Open-statement phrases are kept as written but stripped of leading and trailing whitespace; `insert_open_statement` does the same, so an empty phrase is blocked here,
    // so a row that would only fail at nod time never enters the queue
    let phrase = p.phrase.map(str::trim);
    if phrase == Some("") {
        return Err(AppError::invalid(
            "phrase_missing",
            "an open statement keeps the document's relation phrase",
        ));
    }
    // "Already on the graph": an open statement is looked up among live open rows by (subject, phrase, object); the ontology-backed form
    // still by (subject, predicate, object)
    let asserted: Option<(i32,)> = match phrase {
        Some(phrase) => {
            sqlx::query_as(
                "SELECT 1 FROM facts
                  WHERE kb_id = $1 AND subject_id = $2
                    AND layer = 'open' AND phrase = $3
                    AND object_id IS NOT DISTINCT FROM $4
                    AND object_value IS NOT DISTINCT FROM $5
                    AND invalidated_at IS NULL
                  LIMIT 1",
            )
            .bind(p.kb_id)
            .bind(p.subject_id)
            .bind(phrase)
            .bind(p.object_id)
            .bind(p.object_value)
            .fetch_optional(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT 1 FROM facts
                  WHERE kb_id = $1 AND subject_id = $2
                    AND predicate_id IS NOT DISTINCT FROM $3
                    AND object_id IS NOT DISTINCT FROM $4
                    AND object_value IS NOT DISTINCT FROM $5
                    AND invalidated_at IS NULL
                  LIMIT 1",
            )
            .bind(p.kb_id)
            .bind(p.subject_id)
            .bind(p.predicate_id)
            .bind(p.object_id)
            .bind(p.object_value)
            .fetch_optional(pool)
            .await?
        }
    };
    if asserted.is_some() {
        return Ok(Outcome::AlreadyAsserted);
    }
    let pending: Option<(i32,)> = sqlx::query_as(
        "SELECT 1 FROM pending_facts
          WHERE kb_id = $1 AND subject_id = $2
            AND predicate_id IS NOT DISTINCT FROM $3
            AND object_id IS NOT DISTINCT FROM $4
            AND object_value IS NOT DISTINCT FROM $5
            AND phrase IS NOT DISTINCT FROM $6
          LIMIT 1",
    )
    .bind(p.kb_id)
    .bind(p.subject_id)
    .bind(p.predicate_id)
    .bind(p.object_id)
    .bind(p.object_value)
    .bind(phrase)
    .fetch_optional(pool)
    .await?;
    if pending.is_some() {
        return Ok(Outcome::AlreadyPending);
    }
    // The rejection record is only looked up by (subject, predicate / phrase, object entity). **Literal-value facts are not looked up**: `rejected_facts`
    // has no object_value column, so blocking by (subject, predicate) would widen "salary 28000 was rejected" into
    // "the salary attribute is never mentioned again". Better to ask once more than to reject a new value on someone's behalf.
    // The phrase is part of the key: a different phrase between the same pair of entities is a different sentence, and must not be hit by the last sentence's rejection
    if let Some(object_id) = p.object_id {
        let rejected: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM rejected_facts
              WHERE kb_id = $1 AND subject_id = $2
                AND predicate_id IS NOT DISTINCT FROM $3
                AND object_id = $4
                AND phrase IS NOT DISTINCT FROM $5
              LIMIT 1",
        )
        .bind(p.kb_id)
        .bind(p.subject_id)
        .bind(p.predicate_id)
        .bind(object_id)
        .bind(phrase)
        .fetch_optional(pool)
        .await?;
        if rejected.is_some() {
            return Ok(Outcome::Rejected);
        }
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO pending_facts
             (id, kb_id, subject_id, predicate_id, object_id, object_value, proposed_predicate,
              valid_from, valid_from_precision, valid_to, valid_to_precision,
              confidence, chunk_id, proposed_by, proposed_token,
              phrase, qualifiers, time_words, quote_start, quote_end)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                 $16, $17, $18, $19, $20)",
    )
    .bind(id)
    .bind(p.kb_id)
    .bind(p.subject_id)
    .bind(p.predicate_id)
    .bind(p.object_id)
    .bind(p.object_value)
    .bind(p.proposed_predicate)
    .bind(p.validity.from)
    .bind(p.validity.from_precision)
    .bind(p.validity.to)
    .bind(p.validity.to_precision)
    .bind(p.confidence)
    .bind(p.chunk_id)
    .bind(p.proposed_by)
    .bind(p.proposed_token)
    .bind(phrase)
    .bind(p.qualifiers)
    .bind(p.time_words)
    .bind(p.quote_span.map(|(start, _)| start))
    .bind(p.quote_span.map(|(_, end)| end))
    .execute(pool)
    .await?;
    Ok(Outcome::Proposed(id))
}

/// A pending item for a person to see: `utopia_core::models::PendingFactView`.
const VIEW_SELECT: &str = "\
    SELECT p.id, p.subject_id, s.canonical_name AS subject_name,
           p.predicate_id, r.label AS predicate_label, p.proposed_predicate,
           p.object_id, o.canonical_name AS object_name, p.object_value,
           p.valid_from, p.valid_from_precision, p.valid_to, p.valid_to_precision,
           p.confidence, p.chunk_id, c.text AS quote,
           p.proposed_by, u.display_name AS proposed_by_name,
           t.name AS proposed_token_name, p.created_at,
           p.phrase, p.qualifiers, p.time_words, p.quote_start, p.quote_end
      FROM pending_facts p
      JOIN entities s ON s.id = p.subject_id
      LEFT JOIN relation_types r ON r.id = p.predicate_id
      LEFT JOIN entities o ON o.id = p.object_id
      JOIN chunks c ON c.id = p.chunk_id
      LEFT JOIN users u ON u.id = p.proposed_by
      LEFT JOIN personal_tokens t ON t.id = p.proposed_token";

pub async fn list(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<PendingFactView>> {
    Ok(sqlx::query_as(&format!(
        "{VIEW_SELECT} WHERE p.kb_id = $1 ORDER BY p.created_at DESC, p.id DESC LIMIT $2 OFFSET $3"
    ))
    .bind(kb_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

/// All pending items extracted from one memory sentence: the conversation card fetches by this.
pub async fn for_chunk(
    pool: &PgPool,
    kb_id: Uuid,
    chunk_id: Uuid,
) -> AppResult<Vec<PendingFactView>> {
    Ok(sqlx::query_as(&format!(
        "{VIEW_SELECT} WHERE p.kb_id = $1 AND p.chunk_id = $2 ORDER BY p.created_at"
    ))
    .bind(kb_id)
    .bind(chunk_id)
    .fetch_all(pool)
    .await?)
}

async fn get(pool: &PgPool, kb_id: Uuid, id: Uuid) -> AppResult<PendingFactView> {
    sqlx::query_as(&format!("{VIEW_SELECT} WHERE p.kb_id = $1 AND p.id = $2"))
        .bind(kb_id)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound)
}

/// A self-contained snapshot the decision ledger needs: after the row is deleted, the ledger can still read what was confirmed or rejected at the time.
fn snapshot(v: &PendingFactView) -> serde_json::Value {
    serde_json::json!({
        "subject": v.subject_name,
        "predicate": v.predicate_label,
        "proposed_predicate": v.proposed_predicate,
        "phrase": v.phrase,
        "object": v.object_name,
        "object_value": v.object_value,
        "qualifiers": v.qualifiers,
        "time_words": v.time_words,
        "valid_from": v.valid_from,
        "valid_to": v.valid_to,
        "confidence": v.confidence,
        "quote": v.quote,
        "proposed_by": v.proposed_by_name,
    })
}

pub struct Confirmed {
    pub fact_id: Uuid,
    /// The document that memory sentence lives in: after nodding, time parsing is redone against it (0045)
    pub document_id: Uuid,
    /// false = the same live fact is already on the graph; this time only evidence was added
    pub created: bool,
    /// Number of rows where temporal reconciliation was uncertain and went into `fact_conflicts`. Open statements are not reconciled, always 0
    pub conflicts: u32,
    pub snapshot: serde_json::Value,
}

/// Person nods: enters the ledger via the extraction path (lands the fact, attaches evidence, temporal reconciliation), then is removed from the queue.
///
/// **Confidence is not touched.** A person's stance is not expressed as a float (a lesson from 0011); it lives in the audit ledger.
///
/// No transaction: `insert_fact` dedupes live facts by (subject, predicate, object), so if interrupted and nodded again
/// it only adds evidence and never lands a second one; the row is deleted last. Idempotent, so no lingering risk.
pub async fn confirm(pool: &PgPool, kb_id: Uuid, id: Uuid) -> AppResult<Confirmed> {
    let v = get(pool, kb_id, id).await?;
    if let Some(phrase) = v.phrase.clone() {
        return confirm_open(pool, kb_id, &v, &phrase).await;
    }
    // The anchor is the date of the document that memory sentence lives in (0022), the same source as the extraction path. A fact with no start
    // holds from the moment it has evidence, and the evidence is this sentence
    let attested_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT d.doc_time FROM chunks c JOIN documents d ON d.id = c.document_id
          WHERE c.id = $1",
    )
    .bind(v.chunk_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    let validity = Validity {
        from: v.valid_from,
        from_precision: v.valid_from_precision.as_deref(),
        to: v.valid_to,
        to_precision: v.valid_to_precision.as_deref(),
        attested_at,
        // A pending fact was proposed by a person or agent, its date is given directly and did not go through time parsing
        from_grade: None,
        end_after_period: false,
    };
    let (fact_id, created) = match (v.object_id, v.object_value.as_ref()) {
        (Some(object_id), _) => {
            crate::graph::insert_fact(
                pool,
                kb_id,
                v.subject_id,
                v.predicate_id,
                object_id,
                validity,
                v.confidence,
            )
            .await?
        }
        (None, Some(value)) => {
            crate::graph::insert_value_fact(
                pool,
                kb_id,
                v.subject_id,
                v.predicate_id,
                value,
                validity,
                v.confidence,
            )
            .await?
        }
        (None, None) => return Err(no_object()),
    };
    // Evidence points back to the memory sentence. The quoted sentence is the whole sentence: an episode is only ever one sentence
    crate::graph::add_evidence(
        pool,
        fact_id,
        v.chunk_id,
        Some(&v.quote),
        v.proposed_predicate.as_deref(),
    )
    .await?;

    // Same temporal reconciliation as extraction: for state relations with a uniqueness constraint, a new fact closes the old one.
    // This is exactly what memory should do: after "Mira handed it to Devin", Mira's fact should close.
    // Merging into an existing assertion also works: one more observation, and the timeline's shape may change with it (#679)
    let mut conflicts = 0u32;
    if let Some(pid) = v.predicate_id {
        let meta: Option<(bool, bool, String)> = sqlx::query_as(
            "SELECT functional, inverse_functional, temporal FROM relation_types WHERE id = $1",
        )
        .bind(pid)
        .fetch_optional(pool)
        .await?;
        if let Some((functional, inverse_functional, temporal)) = meta {
            if temporal == "state" {
                let mut directions = Vec::new();
                if functional {
                    directions.push(crate::temporal::Uniqueness::SubjectSide);
                }
                // The object-side uniqueness only means something for entity objects; a literal value has no "who is being pointed at"
                if inverse_functional && v.object_id.is_some() {
                    directions.push(crate::temporal::Uniqueness::ObjectSide);
                }
                for dir in directions {
                    let report = crate::temporal::reconcile_new_fact(
                        pool,
                        kb_id,
                        fact_id,
                        v.subject_id,
                        pid,
                        v.object_id,
                        v.object_value.as_ref(),
                        dir,
                        validity,
                        v.confidence,
                    )
                    .await?;
                    conflicts += report.conflicts;
                }
            }
        }
    }
    sqlx::query("DELETE FROM pending_facts WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(Confirmed {
        fact_id,
        document_id: document_of(pool, v.chunk_id).await?,
        created,
        conflicts,
        snapshot: snapshot(&v),
    })
}

/// Person nods to an open statement (0044): enters the ledger via the open extraction path (`insert_open_statement`,
/// evidence with offsets, role-word attributes, time phrases), then is removed from the queue.
///
/// **Does not consult the ontology and does not reconcile over time.** An open statement has no predicate, and `relation_types` has no
/// functional / temporal entry to look up for it; the time it mentions is copied words, with no position on the world axis yet,
/// and no `valid_*` to close anyone with. Reading the words as dates and then reconciling is for later slices of 0045.
///
/// Like the ontology-backed path, this has no transaction: `insert_open_statement` dedupes by (subject, phrase, object) among the live
/// open rows; evidence, attributes and time phrases are all ON CONFLICT idempotent, so if interrupted and nodded again it will not land a second row.
async fn confirm_open(
    pool: &PgPool,
    kb_id: Uuid,
    v: &PendingFactView,
    phrase: &str,
) -> AppResult<Confirmed> {
    // The record-axis anchor only accepts a date the document states itself (#714): `doc_time` is taken only when `doc_time_source` is content / source;
    // upload time and file mtime do not count. Memory documents are 'none', so it is empty here, and
    // it lands as now: if a person nods right now, the person is the evidence. The extraction path has no such default (0064 decision 5):
    // there no person is present, and what the document does not say is absent
    let attested_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT d.doc_time FROM chunks c JOIN documents d ON d.id = c.document_id
          WHERE c.id = $1 AND d.doc_time_source IN ('content', 'source')",
    )
    .bind(v.chunk_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    let object = match (v.object_id, v.object_value.as_ref()) {
        (Some(object_id), _) => FactObject::Entity(object_id),
        (None, Some(value)) => FactObject::Value(value),
        (None, None) => return Err(no_object()),
    };
    let (fact_id, created) = crate::graph::insert_open_statement(
        pool,
        kb_id,
        v.subject_id,
        phrase,
        object,
        attested_at.or_else(|| Some(chrono::Utc::now())),
        v.confidence,
    )
    .await?;
    // Evidence points back to the memory sentence, with the quote's position in the block; `proposed_predicate = phrase`, so all
    // read paths that already tolerate an empty predicate show it by phrase without changing a character.
    // The quote is **the sentence**, not the whole block: the quote on the pending item is the whole block (with timestamp prefix),
    // and with offsets, the sentence is cut out by offsets, so the offsets say the same thing as the quote (the evidence table's quote keeps only 120 chars)
    let span = v.quote_start.zip(v.quote_end);
    let sentence: String = match span {
        Some((start, end)) if start >= 0 && end > start => v
            .quote
            .chars()
            .skip(start as usize)
            .take((end - start) as usize)
            .collect(),
        _ => v.quote.clone(),
    };
    crate::graph::add_evidence_located(
        pool,
        fact_id,
        v.chunk_id,
        Some(&sentence),
        Some(phrase),
        span,
    )
    .await?;
    for q in json_items(v.qualifiers.as_ref()) {
        let role = q
            .get("role")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                AppError::invalid(
                    "qualifier_role",
                    "a statement qualifier needs the document's role word",
                )
            })?;
        // An entry with entity_id points to an entity; otherwise value is the value in the document, stored as JSON as-is
        let entity_id = q
            .get("entity_id")
            .and_then(serde_json::Value::as_str)
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| {
                AppError::invalid(
                    "qualifier_entity",
                    "a statement qualifier's entity_id must be a uuid",
                )
            })?;
        let value = if entity_id.is_some() {
            None
        } else {
            q.get("value")
        };
        crate::graph::add_statement_qualifier(pool, fact_id, v.chunk_id, role, value, entity_id)
            .await?;
    }
    for t in json_items(v.time_words.as_ref()) {
        let (Some(text), Some(char_start)) = (
            t.get("text").and_then(serde_json::Value::as_str),
            t.get("char_start")
                .and_then(serde_json::Value::as_i64)
                .and_then(|n| i32::try_from(n).ok()),
        ) else {
            return Err(AppError::invalid(
                "time_word_shape",
                "a time word is the document's text and its char offset in the chunk",
            ));
        };
        // Which slot of the statement it came from; items recorded before 0062 lack this field, and they are all `when`
        let role = t
            .get("role")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("when");
        crate::time_mentions::record(pool, kb_id, fact_id, v.chunk_id, text, char_start, role)
            .await?;
    }
    sqlx::query("DELETE FROM pending_facts WHERE id = $1")
        .bind(v.id)
        .execute(pool)
        .await?;
    Ok(Confirmed {
        fact_id,
        document_id: document_of(pool, v.chunk_id).await?,
        created,
        conflicts: 0,
        snapshot: snapshot(v),
    })
}

fn no_object() -> AppError {
    AppError::invalid(
        "pending_fact_has_no_object",
        "a pending fact must have an object entity or a literal value",
    )
}

/// A JSON object column; not an array (or none) is empty
fn json_items(v: Option<&serde_json::Value>) -> impl Iterator<Item = &serde_json::Value> {
    v.and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
}

/// The document that a chunk sits in.
async fn document_of(pool: &PgPool, chunk_id: Uuid) -> AppResult<Uuid> {
    Ok(
        sqlx::query_scalar("SELECT document_id FROM chunks WHERE id = $1")
            .bind(chunk_id)
            .fetch_one(pool)
            .await?,
    )
}

/// Person rejects: recorded in `rejected_facts` (the next re-extraction checks it first), then removed from the queue.
/// The memory sentence itself is kept: the person did say that sentence, it just produced no usable fact.
/// An open statement records the phrase along with it: what is rejected is "this sentence", not everything between that pair of entities
pub async fn reject(
    pool: &PgPool,
    kb_id: Uuid,
    id: Uuid,
    rejected_by: Option<Uuid>,
) -> AppResult<serde_json::Value> {
    let v = get(pool, kb_id, id).await?;
    sqlx::query(
        "INSERT INTO rejected_facts (kb_id, subject_id, predicate_id, object_id, rejected_by, phrase)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(kb_id)
    .bind(v.subject_id)
    .bind(v.predicate_id)
    .bind(v.object_id)
    .bind(rejected_by)
    .bind(&v.phrase)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM pending_facts WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(snapshot(&v))
}
