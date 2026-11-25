//! Ledger side of binding relation phrases to properties by signature (the second slice of 0044 decision 3, see 0066).
//!
//! Open-statement relations are the document's own phrases (`facts.phrase`), and they do not choose a property. This module does three things: it counts
//! the signatures present in a library (phrase × subject class × object class or "value"), each with a few example sentences; it records what each signature was judged to be
//! (the binding) and which ones have gone stale; and it gives the next slice (materialization) a table of "signature → property, direction". The judgment
//! itself -- asking the model, and binding only when two votes agree -- lives in `phrase_alignment` on the server; this module knows nothing about the model.
//!
//! Phrases are normalized the same way as category words: whitespace collapsed to one space, trimmed at both ends, lowercased; [`normalize`] and [`PHRASE_SQL`]
//! must say the same thing.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::HashMap;
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// SQL-side normalization, matching [`normalize`]. `$col` is replaced with a column name by the caller.
const PHRASE_SQL: &str = "lower(btrim(regexp_replace($col, '\\s+', ' ', 'g')))";

pub(crate) fn phrase_sql(col: &str) -> String {
    PHRASE_SQL.replace("$col", col)
}

/// Normalize one phrase: collapse whitespace to one space, trim both ends, lowercase. "Acquired " and "acquired" are one phrase.
pub fn normalize(phrase: &str) -> String {
    phrase
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// One signature: everything the model is shown when judging which property it should bind to.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PhraseSignature {
    /// The normalized phrase
    pub phrase: String,
    /// The subject's class (empty while the category word is not yet bound to a class)
    pub subject_type_id: Option<Uuid>,
    pub subject_type_key: Option<String>,
    /// The object's class; empty when the object is a literal value, in which case `object_is_value` is true
    pub object_type_id: Option<Uuid>,
    pub object_type_key: Option<String>,
    pub object_is_value: bool,
    /// The number of live open statements under this signature
    pub count: i64,
    /// Examples: at most 3 "subject —phrase→ object" entries, each with its own quote
    pub examples: Vec<String>,
    pub quotes: Vec<String>,
    /// Statement identities in the same order as the live examples and quotes.
    pub example_statement_ids: Vec<Uuid>,
}

/// Every distinct signature in the library: live open statements, grouped by phrase, the classes at both ends, and whether the object is a literal value.
/// When adopting an existing property this is read inside its own transaction (it does not take another connection), so it accepts any executor
pub async fn signatures<'e>(
    pool: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    kb_id: Uuid,
) -> AppResult<Vec<PhraseSignature>> {
    // A statement can have several pieces of evidence. Before counting and taking examples, pick one stable reference per statement,
    // preferring evidence with a full position, so a statement with lots of evidence does not crowd out the others.
    let sql = format!(
        "WITH live AS (
             SELECT f.id, {phrase} AS phrase,
                    s.type_id AS subject_type_id, o.type_id AS object_type_id,
                    (f.object_id IS NULL) AS object_is_value,
                    s.canonical_name AS subject_name,
                    coalesce(o.canonical_name, f.object_value #>> '{{value}}', '') AS object_name,
                    f.phrase AS spelling, fe.chunk_id, fe.quote_start, fe.quote_end, f.recorded_at
             FROM facts f
             JOIN entities s ON s.id = f.subject_id
             LEFT JOIN entities o ON o.id = f.object_id
             LEFT JOIN LATERAL (
                 SELECT chunk_id, quote_start, quote_end FROM fact_evidence
                 WHERE fact_id = f.id
                 ORDER BY (quote_start IS NULL OR quote_end IS NULL), chunk_id
                 LIMIT 1
             ) fe ON true
             WHERE f.kb_id = $1 AND f.layer = 'open' AND f.invalidated_at IS NULL
               AND f.phrase IS NOT NULL AND btrim(f.phrase) <> ''
         ),
         -- 每组的计数和前 3 条例句在一趟里聚出来。从前每组的例句、引文各是一个关联子查询，
         -- 每个都把全部陈述重扫一遍，耗时随开放陈述数平方增长：3.6 万条时一趟约 200 秒（#1096）。
         -- 计数也在这里取，不另聚一份再按四个键用 IS NOT DISTINCT FROM 接回来——那种连接
         -- Postgres 做不了哈希；分块的原文只为留下的那三条去读
         ranked AS (
             SELECT l.*,
                    row_number() OVER (PARTITION BY l.phrase, l.subject_type_id, l.object_type_id, l.object_is_value
                                       ORDER BY l.recorded_at, l.id) AS rn
             FROM live l
         ),
         grouped AS (
             SELECT r.phrase, r.subject_type_id, r.object_type_id, r.object_is_value, count(*) AS count,
                    array_agg(r.subject_name || ' —' || r.spelling || '→ ' || r.object_name ORDER BY r.rn)
                        FILTER (WHERE r.rn <= 3) AS examples,
                    array_agg(coalesce(substr(c.text, r.quote_start + 1, r.quote_end - r.quote_start), '') ORDER BY r.rn)
                        FILTER (WHERE r.rn <= 3) AS quotes,
                    array_agg(r.id ORDER BY r.rn) FILTER (WHERE r.rn <= 3) AS example_statement_ids
             FROM ranked r
             LEFT JOIN chunks c ON c.id = r.chunk_id AND r.rn <= 3
             GROUP BY r.phrase, r.subject_type_id, r.object_type_id, r.object_is_value
         )
         SELECT g.phrase, g.subject_type_id, st.key AS subject_type_key,
                g.object_type_id, ot.key AS object_type_key, g.object_is_value, g.count,
                coalesce(g.examples, '{{}}') AS examples, coalesce(g.quotes, '{{}}') AS quotes,
                coalesce(g.example_statement_ids, '{{}}'::uuid[]) AS example_statement_ids
         FROM grouped g
         LEFT JOIN entity_types st ON st.id = g.subject_type_id
         LEFT JOIN entity_types ot ON ot.id = g.object_type_id
         -- 计数和短语相同、两端的类不同的签名，顺序也得定下来：对齐按这个顺序分批送模型
         ORDER BY g.count DESC, g.phrase, g.subject_type_id, g.object_type_id, g.object_is_value",
        phrase = phrase_sql("f.phrase")
    );
    Ok(sqlx::query_as(&sql).bind(kb_id).fetch_all(pool).await?)
}

/// Which signatures the live open statements in one document fall on (keys match [`PhraseSignature::key`]). After a pushed
/// document is extracted, the ontology judges only these signatures (#1242); other signatures belong to the aligner.
///
/// Evidence is found by `fact_evidence.document_id` (`fact_evidence_document_idx`), not through `chunks`:
/// `fact_evidence.chunk_id` has no index, so finding it through chunks scans all the evidence; on a library with 8.3 million rows that took 12 seconds per
/// document, and during a push dozens ran at once and pinned the database's CPU (#1471). Going by the document column through the index
/// takes 0.16 seconds, with the same rows -- the evidence's document_id is set at write time to equal its chunk's document.
pub async fn signature_keys_of_document(
    pool: &PgPool,
    kb_id: Uuid,
    document_id: Uuid,
) -> AppResult<Vec<(String, Option<Uuid>, Option<Uuid>, bool)>> {
    let sql = format!(
        "SELECT DISTINCT {phrase}, s.type_id, o.type_id, (f.object_id IS NULL)
           FROM facts f
           JOIN entities s ON s.id = f.subject_id
      LEFT JOIN entities o ON o.id = f.object_id
          WHERE f.kb_id = $1 AND f.layer = 'open' AND f.invalidated_at IS NULL
            AND f.phrase IS NOT NULL AND btrim(f.phrase) <> ''
            AND EXISTS (SELECT 1 FROM fact_evidence fe
                         WHERE fe.fact_id = f.id AND fe.document_id = $2)",
        phrase = phrase_sql("f.phrase")
    );
    Ok(sqlx::query_as(&sql)
        .bind(kb_id)
        .bind(document_id)
        .fetch_all(pool)
        .await?)
}

/// Fetch one bound signature by id (used when a person decides it in the queue): phrase, the classes at both ends, whether the object is a literal value,
/// examples as recorded at judging time, and the same statement count.
pub async fn signature_of(
    pool: &PgPool,
    kb_id: Uuid,
    id: Uuid,
) -> AppResult<Option<PhraseSignature>> {
    Ok(sqlx::query_as(
        "SELECT b.phrase, b.subject_type_id, st.key AS subject_type_key,
                b.object_type_id, ot.key AS object_type_key, b.object_is_value,
                b.statement_count::bigint AS count, b.examples, '{}'::text[] AS quotes,
                '{}'::uuid[] AS example_statement_ids
           FROM phrase_bindings b
      LEFT JOIN entity_types st ON st.id = b.subject_type_id
      LEFT JOIN entity_types ot ON ot.id = b.object_type_id
          WHERE b.kb_id = $1 AND b.id = $2",
    )
    .bind(kb_id)
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// What a signature was judged to be.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Binding {
    pub phrase: String,
    pub subject_type_id: Option<Uuid>,
    pub object_type_id: Option<Uuid>,
    pub object_is_value: bool,
    /// Has a value when `status = 'bound'`
    pub relation_type_id: Option<Uuid>,
    /// forward / reverse; has a value when `status = 'bound'`
    pub direction: Option<String>,
    /// bound / none / undecided
    pub status: String,
    pub decided_at: DateTime<Utc>,
    /// agent / person / ontology (the phrase is exactly a declared relation, #1242)
    pub decided_by: String,
    /// Fingerprint of the inputs at judging time; NULL = a judgment from before this column existed
    pub basis: Option<String>,
    /// For a statement that states only a single moment: which end of the state that moment marks, start / end / none (#966).
    /// NULL = unknown: judgments from before this column, or the property is not a state
    pub marks: Option<String>,
    /// The time the aligner asked about marks; NULL = never asked. Once asked with no value it is not asked again, it waits for a person (0053 revision)
    pub marks_asked_at: Option<DateTime<Utc>>,
}

impl Binding {
    /// The key that matches a signature: phrase + classes at both ends + whether the object is a literal value
    pub fn key(&self) -> (String, Option<Uuid>, Option<Uuid>, bool) {
        (
            self.phrase.clone(),
            self.subject_type_id,
            self.object_type_id,
            self.object_is_value,
        )
    }
}

impl PhraseSignature {
    pub fn key(&self) -> (String, Option<Uuid>, Option<Uuid>, bool) {
        (
            self.phrase.clone(),
            self.subject_type_id,
            self.object_type_id,
            self.object_is_value,
        )
    }
}

/// All bindings in the library.
pub async fn bindings(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<Binding>> {
    Ok(sqlx::query_as(
        "SELECT phrase, subject_type_id, object_type_id, object_is_value,
                relation_type_id, direction, status, decided_at, decided_by, basis, marks,
                marks_asked_at
         FROM phrase_bindings WHERE kb_id = $1 ORDER BY phrase",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}

/// The fingerprint of the inputs a judgment saw (0053): the ancestor closure of both ends' classes (including itself), whether the object is a literal value,
/// the candidate properties hit by inheritance, and each one's `updated_at`. The worker recomputes this for live signatures each round; a mismatch with the stored value means stale --
/// it compares the **current inputs**, not a timestamp, so adding or removing parent edges, and
/// edits made while a model request is in flight (#795), are all seen; a timestamp would not see them.
///
/// This is only a cache-invalidation key, not for any security purpose: FNV-1a 64-bit is enough, and there is no need to pull in a hash dependency for it.
pub fn basis_of(
    subject_closure: &[Uuid],
    object_closure: &[Uuid],
    object_is_value: bool,
    candidates: &[(Uuid, DateTime<Utc>)],
) -> String {
    let sorted = |ids: &[Uuid]| {
        let mut v: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
        v.sort();
        v.join(",")
    };
    let mut cands: Vec<String> = candidates
        .iter()
        .map(|(id, at)| format!("{id}@{}", at.to_rfc3339()))
        .collect();
    cands.sort();
    let text = format!(
        "s={};o={};v={};c={}",
        sorted(subject_closure),
        sorted(object_closure),
        object_is_value,
        cands.join(",")
    );
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// The last time each property was changed in the library, for the fingerprint. Read once per round, not part of a view -- the view is for the page,
/// and the page does not need this number
pub async fn property_versions(
    pool: &PgPool,
    kb_id: Uuid,
) -> AppResult<HashMap<Uuid, DateTime<Utc>>> {
    let rows: Vec<(Uuid, DateTime<Utc>)> =
        sqlx::query_as("SELECT id, updated_at FROM relation_types WHERE kb_id = $1")
            .bind(kb_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

/// A binding that no longer holds: the property it bound to was edited after the judgment; or the judgment was none / undecided and since then a property in the library was created or changed.
/// A negative judgment selected no property, and a new definition of an existing property may now match it.
/// If the property or class was deleted, the row has already gone via cascade.
pub async fn stale(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<Binding>> {
    Ok(sqlx::query_as(
        "SELECT b.phrase, b.subject_type_id, b.object_type_id, b.object_is_value,
                b.relation_type_id, b.direction, b.status, b.decided_at, b.decided_by, b.basis,
                b.marks, b.marks_asked_at
         FROM phrase_bindings b
         LEFT JOIN relation_types r ON r.id = b.relation_type_id
         WHERE b.kb_id = $1
           AND ((b.status = 'bound' AND r.updated_at > b.decided_at)
                OR (b.status IN ('none', 'undecided')
                    AND b.decided_at < (SELECT max(updated_at) FROM relation_types
                                        WHERE kb_id = $1)))
         ORDER BY b.phrase",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}

/// What a judgment writes.
pub struct Decision<'a> {
    pub relation_type_id: Option<Uuid>,
    /// forward / reverse
    pub direction: Option<&'a str>,
    /// bound / none / undecided
    pub status: &'a str,
    pub votes: &'a serde_json::Value,
    /// agent / person / ontology
    pub decided_by: &'a str,
    /// The fingerprint of the inputs at judging time ([`basis_of`]): an agent's judgment must carry it, a person's does not -- a person does not re-judge against a fingerprint
    pub basis: Option<&'a str>,
    /// start / end / none: how a single-moment statement reads under a state property (#966). Only bound carries it;
    /// None = unknown, and such a statement does not count as a typed row
    pub marks: Option<&'a str>,
    /// This judgment asked the marks: the aligner asks this together with the judgment when it binds a signature to a state property (0053 revision). It records the time asked,
    /// so a signature without a value is not asked again and goes to the alignment queue for a person. Only bound carries it
    pub marks_asked: bool,
}

/// Record a signature's judgment (replacing any existing one). Returns whether it was written.
///
/// **A person's judgment is never overwritten by an agent**: if the existing row was judged by a person and this one is from an agent or the ontology, the row is kept as is and false is returned.
/// The reverse is allowed: a person may change an agent's or the ontology's judgment.
pub async fn decide(
    pool: &PgPool,
    kb_id: Uuid,
    sig: &PhraseSignature,
    d: Decision<'_>,
) -> AppResult<bool> {
    validate_decision(sig, &d)?;
    // The observed regression outcome belongs to this decision, not to a later read that
    // could see a different person's binding. Commit both together, as delivery already does.
    let mut tx = pool.begin().await?;
    let changed = decide_on(&mut tx, kb_id, sig, d).await?;
    tx.commit().await?;
    Ok(changed)
}

fn validate_decision(sig: &PhraseSignature, d: &Decision<'_>) -> AppResult<String> {
    if !matches!(d.status, "bound" | "none" | "undecided") {
        return Err(AppError::Validation(format!(
            "unknown binding status {:?}",
            d.status
        )));
    }
    let bound = d.status == "bound";
    if bound != d.relation_type_id.is_some() || bound != d.direction.is_some() {
        return Err(AppError::Validation(
            "a bound signature needs a property and a direction and an unbound one must not have them".into(),
        ));
    }
    if let Some(dir) = d.direction {
        if !matches!(dir, "forward" | "reverse") {
            return Err(AppError::Validation(format!("unknown direction {dir:?}")));
        }
    }
    if (d.marks.is_some() || d.marks_asked) && !bound {
        return Err(AppError::Validation(
            "only a bound signature says which end a moment marks".into(),
        ));
    }
    if let Some(marks) = d.marks {
        if !matches!(marks, "start" | "end" | "none") {
            return Err(AppError::Validation(format!("unknown marks {marks:?}")));
        }
    }
    if !matches!(d.decided_by, "agent" | "person" | "ontology") {
        return Err(AppError::Validation(format!(
            "unknown decider {:?}",
            d.decided_by
        )));
    }
    let phrase = normalize(&sig.phrase);
    if phrase.is_empty() {
        return Err(AppError::Validation("an empty phrase binds nothing".into()));
    }
    Ok(phrase)
}

/// The kind of recompute job queued alongside a person's judgment when it is saved. `main` dispatches on it; the payload carries only `kb_id` --
/// the job reads the **current** binding and does not replay the properties from judgment time (0051: replaying an old payload would overwrite a later person).
pub const MATERIALIZE_KIND: &str = "materialize_typed";

/// A person's judgment and its own recompute job are committed in **one transaction** (0051).
///
/// Why not "save the judgment, then see whether a worker is running": an alignment in progress may already have done its last read, so this judgment would have nothing
/// to project typed rows for it -- it is accepted, yet never projected.
/// One job per judgment, and the job only becomes visible after the judgment commits, so whichever job runs reads the latest binding,
/// and the last judgment is always recomputed. The cost is N judgments and N recomputes, most of the later ones empty runs (0051
/// measured: 100 judgments converge in 593 ms in total); "skip queuing if one is already running" would save exactly the one that gets lost.
///
/// Returns the job id; when an agent may not overwrite a person (the rule in `decide_on`), nothing is written and `None` is returned.
pub async fn decide_with_delivery(
    pool: &PgPool,
    kb_id: Uuid,
    sig: &PhraseSignature,
    d: Decision<'_>,
) -> AppResult<Option<i64>> {
    decide_with_delivery_budget(pool, kb_id, sig, d, 3).await
}

/// The budget is its own parameter only so a test can check that a failed enqueue rolls back the judgment too: 0 is rejected by `enqueue`,
/// which is exactly a real failure occurring after the judgment write. Production always passes 3.
async fn decide_with_delivery_budget(
    pool: &PgPool,
    kb_id: Uuid,
    sig: &PhraseSignature,
    d: Decision<'_>,
    max_attempts: i32,
) -> AppResult<Option<i64>> {
    let mut tx = pool.begin().await?;
    if !decide_on(&mut tx, kb_id, sig, d).await? {
        tx.rollback().await?;
        return Ok(None);
    }
    let id = crate::jobs::enqueue_queued_once_tx(
        &mut tx,
        MATERIALIZE_KIND,
        serde_json::json!({ "kb_id": kb_id }),
        max_attempts,
    )
    .await?;
    tx.commit().await?;
    Ok(Some(id))
}

/// Write on the caller's transaction connection, so related durable work shares its commit.
pub async fn decide_on(
    connection: &mut sqlx::PgConnection,
    kb_id: Uuid,
    sig: &PhraseSignature,
    d: Decision<'_>,
) -> AppResult<bool> {
    let phrase = validate_decision(sig, &d)?;
    let res = sqlx::query(
        "INSERT INTO phrase_bindings
             (id, kb_id, phrase, subject_type_id, object_type_id, object_is_value,
              relation_type_id, direction, status, votes, statement_count, examples,
              decided_at, decided_by, basis, marks, marks_asked_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, now(), $13, $14, $15,
                 CASE WHEN $16 THEN now() END)
         ON CONFLICT (kb_id, phrase, subject_type_id, object_type_id, object_is_value) DO UPDATE
            SET relation_type_id = EXCLUDED.relation_type_id,
                direction = EXCLUDED.direction,
                status = EXCLUDED.status,
                votes = EXCLUDED.votes,
                statement_count = EXCLUDED.statement_count,
                examples = EXCLUDED.examples,
                decided_at = now(),
                decided_by = EXCLUDED.decided_by,
                basis = EXCLUDED.basis,
                marks = EXCLUDED.marks,
                marks_asked_at = EXCLUDED.marks_asked_at
          WHERE NOT (phrase_bindings.decided_by = 'person' AND EXCLUDED.decided_by <> 'person')",
    )
    .bind(Uuid::now_v7())
    .bind(kb_id)
    .bind(&phrase)
    .bind(sig.subject_type_id)
    .bind(if sig.object_is_value {
        None
    } else {
        sig.object_type_id
    })
    .bind(sig.object_is_value)
    .bind(d.relation_type_id)
    .bind(d.direction)
    .bind(d.status)
    .bind(d.votes)
    .bind(i32::try_from(sig.count).unwrap_or(i32::MAX))
    .bind(&sig.examples)
    .bind(d.decided_by)
    .bind(d.basis)
    .bind(d.marks)
    .bind(d.marks_asked)
    .execute(&mut *connection)
    .await?;
    let changed = res.rows_affected() > 0;
    if changed {
        crate::ontology_regressions::record_for_signature(connection, kb_id, sig).await?;
    }
    Ok(changed)
}

/// The result of asking marks for signatures bound before this column existed (0053 revision 2026-09-27). This ask only asks which end a single-moment statement marks,
/// and does not re-judge the binding: `value` is the value when both votes accept the binding's property and direction and it says the same value; that value is written.
/// Otherwise it is None and the binding is left as it is. Both outcomes record the time asked: anything asked is not asked again, and columns without a value go to the alignment queue for a person.
///
/// Only writes if the binding from the time of asking still stands: still agent-bound, property and direction unchanged, still no value. If a person has judged or re-judged it in the meantime,
/// this answer does not count, and it returns false
pub async fn record_marks(
    pool: &PgPool,
    kb_id: Uuid,
    asked: &Binding,
    value: Option<&str>,
) -> AppResult<bool> {
    if let Some(marks) = value {
        if !matches!(marks, "start" | "end" | "none") {
            return Err(AppError::Validation(format!("unknown marks {marks:?}")));
        }
    }
    let res = sqlx::query(
        "UPDATE phrase_bindings
            SET marks = $8, marks_asked_at = now()
          WHERE kb_id = $1 AND phrase = $2
            AND subject_type_id IS NOT DISTINCT FROM $3
            AND object_type_id IS NOT DISTINCT FROM $4
            AND object_is_value = $5
            AND status = 'bound' AND decided_by = 'agent' AND marks IS NULL
            AND relation_type_id = $6 AND direction = $7",
    )
    .bind(kb_id)
    .bind(&asked.phrase)
    .bind(asked.subject_type_id)
    .bind(asked.object_type_id)
    .bind(asked.object_is_value)
    .bind(asked.relation_type_id)
    .bind(asked.direction.as_deref())
    .bind(value)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

#[cfg(test)]
#[path = "phrase_bindings_delivery_tests.rs"]
mod delivery_tests;

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn a_phrase_is_one_phrase_however_spaced_or_cased() {
        assert_eq!(normalize("  Was   Designed By "), "was designed by");
        assert_eq!(normalize("Revenue"), "revenue");
        assert_eq!(normalize("细化解读"), "细化解读");
        assert_eq!(normalize("   "), "");
    }
}
