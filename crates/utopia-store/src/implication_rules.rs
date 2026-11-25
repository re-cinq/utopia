//! Implication rules (the fifth slice of 0044 decision 3): a statement of a certain shape, or something carrying a certain category word, implies a fact about another
//! property. The object is either the statement's own object, or is read out of the object's text by a reading.
//!
//! Same lifecycle as bindings: the aligner proposes (`propose`, agent), a person approves or rejects (`decide_with_delivery`),
//! and an agent's proposal does not overwrite a person's decision. Proposals carry a fingerprint (0053), so when the inputs change the aligner proposes again. Execution happens in `materialize`
//! **with no model call**: readings are first computed by the `read_phrases` job into `phrase_readings`, and materialization only looks up the cache;
//! a reading missing from the cache is skipped in this round, and is picked up once the cache fills in.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// The kinds of reading. The strings go into the database and into prompts, so these are constants rather than an enum; the model executes them,
/// and this module only defines "what to ask". Adding one is adding a line -- shown to the model together with its description.
pub const READINGS: &[(&str, &str)] = &[
    (
        "country_of_nationality",
        "the country a nationality or demonym names (British → United Kingdom, 法国 → France)",
    ),
    (
        "country_of_place",
        "the country a place belongs to (Piedmont region of Virginia → United States, Lyon → France)",
    ),
    (
        "year_of_phrase",
        "the year a phrase gives, as a four-digit number (\"the summer of 1952\" → 1952)",
    ),
];

pub fn reading_is_known(reading: &str) -> bool {
    READINGS.iter().any(|(k, _)| *k == reading)
}

/// Normalize a reading's input: same rule as for phrases and category words (collapse whitespace to one, lowercase, trim both ends)
pub fn normalize(s: &str) -> String {
    crate::phrase_bindings::normalize(s)
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Rule {
    pub id: Uuid,
    pub trigger: String,
    pub phrase: String,
    pub subject_type_id: Option<Uuid>,
    pub object_type_id: Option<Uuid>,
    pub object_is_value: bool,
    pub conclude_property_id: Uuid,
    pub reading: Option<String>,
    pub status: String,
    pub votes: Option<serde_json::Value>,
    pub decided_by: String,
    pub basis: Option<String>,
    pub statement_count: i32,
    pub examples: Vec<String>,
    pub decided_at: DateTime<Utc>,
}

/// A rule the aligner proposed.
pub struct Proposal<'a> {
    /// phrase | kind_word
    pub trigger: &'a str,
    pub phrase: &'a str,
    pub subject_type_id: Option<Uuid>,
    pub object_type_id: Option<Uuid>,
    pub object_is_value: bool,
    pub conclude_property_id: Uuid,
    pub reading: Option<&'a str>,
    /// proposed (needs a person's approval) | rejected (the model said this shape implies nothing -- recorded so it is not asked again every round)
    pub status: &'a str,
    pub votes: &'a serde_json::Value,
    pub basis: &'a str,
    pub statement_count: i64,
    pub examples: &'a [String],
}

/// Record the aligner's proposal; a rule that a person has already decided is left as is (returns None). The agent's old row
/// is overwritten by the new proposal -- the fingerprint changes only when the aligner proposes again, so what gets overwritten is a stale view.
pub async fn propose(pool: &PgPool, kb_id: Uuid, p: &Proposal<'_>) -> AppResult<Option<Uuid>> {
    if !matches!(p.trigger, "phrase" | "kind_word") {
        return Err(AppError::Validation(format!(
            "unknown trigger {:?}",
            p.trigger
        )));
    }
    if !matches!(p.status, "proposed" | "rejected") {
        return Err(AppError::Validation(format!(
            "a proposal is proposed or rejected, not {:?}",
            p.status
        )));
    }
    if let Some(r) = p.reading {
        if !reading_is_known(r) {
            return Err(AppError::Validation(format!("unknown reading {r:?}")));
        }
    }
    let phrase = normalize(p.phrase);
    if phrase.is_empty() {
        return Err(AppError::Validation(
            "an empty phrase implies nothing".into(),
        ));
    }
    let id = Uuid::now_v7();
    let row: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO implication_rules
             (id, kb_id, trigger, phrase, subject_type_id, object_type_id, object_is_value,
              conclude_property_id, reading, status, votes, decided_by, basis, statement_count, examples)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'agent', $12, $13, $14)
         ON CONFLICT (kb_id, trigger, phrase, subject_type_id, object_type_id, object_is_value,
                      conclude_property_id, reading) DO UPDATE
            SET status = EXCLUDED.status, votes = EXCLUDED.votes, basis = EXCLUDED.basis,
                statement_count = EXCLUDED.statement_count, examples = EXCLUDED.examples,
                decided_at = now()
          WHERE implication_rules.decided_by = 'agent'
         RETURNING id",
    )
    .bind(id)
    .bind(kb_id)
    .bind(p.trigger)
    .bind(&phrase)
    .bind(p.subject_type_id)
    .bind(if p.object_is_value { None } else { p.object_type_id })
    .bind(p.object_is_value)
    .bind(p.conclude_property_id)
    .bind(p.reading)
    .bind(p.status)
    .bind(p.votes)
    .bind(p.basis)
    .bind(i32::try_from(p.statement_count).unwrap_or(i32::MAX))
    .bind(p.examples)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

pub async fn list(pool: &PgPool, kb_id: Uuid, status: Option<&str>) -> AppResult<Vec<Rule>> {
    Ok(sqlx::query_as(
        "SELECT id, trigger, phrase, subject_type_id, object_type_id, object_is_value,
                conclude_property_id, reading, status, votes, decided_by, basis,
                statement_count, examples, decided_at
           FROM implication_rules
          WHERE kb_id = $1 AND ($2::text IS NULL OR status = $2)
          ORDER BY decided_at, phrase",
    )
    .bind(kb_id)
    .bind(status)
    .fetch_all(pool)
    .await?)
}

pub async fn get(pool: &PgPool, kb_id: Uuid, id: Uuid) -> AppResult<Option<Rule>> {
    Ok(sqlx::query_as(
        "SELECT id, trigger, phrase, subject_type_id, object_type_id, object_is_value,
                conclude_property_id, reading, status, votes, decided_by, basis,
                statement_count, examples, decided_at
           FROM implication_rules WHERE kb_id = $1 AND id = $2",
    )
    .bind(kb_id)
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// A person approves or rejects a rule, committed in the **same transaction** as its follow-up work (the same rule as 0051).
/// On approval, if it needs a reading: queue `read_phrases` first to fill the cache (when it finishes, it queues materialization itself);
/// if it needs no reading, or was rejected: queue materialization directly -- a rejection also needs a recompute, so implied rows get retracted.
pub async fn decide_with_delivery(
    pool: &PgPool,
    kb_id: Uuid,
    id: Uuid,
    approve: bool,
    votes: &serde_json::Value,
) -> AppResult<Option<i64>> {
    let mut tx = pool.begin().await?;
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "UPDATE implication_rules
            SET status = $3, votes = $4, decided_by = 'person', decided_at = now()
          WHERE kb_id = $1 AND id = $2
      RETURNING reading",
    )
    .bind(kb_id)
    .bind(id)
    .bind(if approve { "approved" } else { "rejected" })
    .bind(votes)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((reading,)) = row else {
        tx.rollback().await?;
        return Ok(None);
    };
    let kind = if approve && reading.is_some() {
        READ_KIND
    } else {
        crate::phrase_bindings::MATERIALIZE_KIND
    };
    let payload = serde_json::json!({ "kb_id": kb_id });
    let job = if kind == crate::phrase_bindings::MATERIALIZE_KIND {
        crate::jobs::enqueue_queued_once_tx(&mut tx, kind, payload, 3).await?
    } else {
        crate::jobs::enqueue_with_max_attempts_tx(&mut tx, kind, payload, 3).await?
    };
    tx.commit().await?;
    Ok(Some(job))
}

/// The kind of job that fills the reading cache.
pub const READ_KIND: &str = "read_phrases";

/// One word waiting to be read: which kind of reading, and what to read.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PendingReading {
    pub reading: String,
    pub phrase: String,
}

/// Among approved rules, the (reading, word) pairs that have no cache entry yet. Phrase rules read the statement's object (an entity name or a literal value),
/// category-word rules read the category word itself. A pair that was answered "cannot be read" also counts as cached and is not listed again.
pub async fn pending_readings(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<PendingReading>> {
    let sql = format!(
        "SELECT DISTINCT r.reading, {phrase} AS phrase
           FROM implication_rules r
           JOIN facts s ON s.kb_id = r.kb_id AND s.layer = 'open' AND s.invalidated_at IS NULL
           JOIN entities se ON se.id = s.subject_id
      LEFT JOIN entities oe ON oe.id = s.object_id
          WHERE r.kb_id = $1 AND r.status = 'approved' AND r.trigger = 'phrase' AND r.reading IS NOT NULL
            AND {rule_match}
            AND NOT EXISTS (SELECT 1 FROM phrase_readings pr
                             WHERE pr.kb_id = r.kb_id AND pr.reading = r.reading AND pr.phrase = {phrase})
         UNION
         SELECT DISTINCT r.reading, r.phrase
           FROM implication_rules r
          WHERE r.kb_id = $1 AND r.status = 'approved' AND r.trigger = 'kind_word' AND r.reading IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM phrase_readings pr
                             WHERE pr.kb_id = r.kb_id AND pr.reading = r.reading AND pr.phrase = r.phrase)",
        phrase = object_text_sql(),
        rule_match = RULE_MATCH,
    );
    Ok(sqlx::query_as(&sql).bind(kb_id).fetch_all(pool).await?)
}

/// The statement's object word, normalized: an entity name, or the value of a literal. For use by readings.
pub(crate) fn object_text_sql() -> &'static str {
    "lower(btrim(regexp_replace(coalesce(oe.canonical_name, s.object_value ->> 'value', s.object_value #>> '{}', ''), '\\s+', ' ', 'g')))"
}

/// Phrase rules match the statement's signature (the same MATCH as binding in materialize, with b replaced by r)
pub(crate) const RULE_MATCH: &str =
    "r.phrase = lower(btrim(regexp_replace(s.phrase, '\\s+', ' ', 'g')))
       AND r.subject_type_id IS NOT DISTINCT FROM se.type_id
       AND r.object_is_value = (s.object_id IS NULL)
       AND (s.object_id IS NULL OR r.object_type_id IS NOT DISTINCT FROM oe.type_id)";

/// Record one reading's answer: a thing in the library, a literal value, or both empty (could not be read -- also recorded, so it is not asked again).
pub async fn record_reading(
    pool: &PgPool,
    kb_id: Uuid,
    reading: &str,
    phrase: &str,
    entity_id: Option<Uuid>,
    value: Option<&serde_json::Value>,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO phrase_readings (kb_id, reading, phrase, entity_id, value)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (kb_id, reading, phrase) DO UPDATE
            SET entity_id = EXCLUDED.entity_id, value = EXCLUDED.value, answered_at = now()",
    )
    .bind(kb_id)
    .bind(reading)
    .bind(normalize(phrase))
    .bind(entity_id)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// If a reading yields a name: if the library has such a thing, that is it; if not, create one with that name.
/// The name fact is recorded per 0041 (`names::record`) -- this is the bridge for recall, so a country a reading yields can be recognized next time.
pub async fn resolve_or_create_named(pool: &PgPool, kb_id: Uuid, name: &str) -> AppResult<Uuid> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::Validation("an empty name names nothing".into()));
    }
    if let Some(id) = crate::resolution::existing_by_name(pool, kb_id, name).await? {
        return Ok(id);
    }
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(kb_id)
        .bind(name)
        .execute(pool)
        .await?;
    crate::names::record(pool, kb_id, id, name, None, None).await?;
    Ok(id)
}

#[cfg(test)]
#[path = "implication_rules_tests.rs"]
mod tests;
