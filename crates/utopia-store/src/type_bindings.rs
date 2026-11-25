//! The ledger side of binding category words to classes (piece 1 of decisions 3–4 of 0044, see 0065).
//!
//! Open extraction only records the document's own category words (`entities.specific_type`) and does not pick a class. This does three things:
//! counts which category words a KB has, and for each its examples and relation phrases (its signature); records what each category word was judged to be
//! (the binding) and judges which ones have gone stale; writes the bound class onto entities under that category word (`type_source =
//! 'aligned'`), or, when no class matches, falls back to the old flow and proposes "add a class". The judgment itself — asking the model,
//! two votes must agree before binding — lives in the server's `type_alignment`; this module knows nothing about the model.
//!
//! Category words are normalized the same way on the Rust and SQL sides: whitespace folded to one space, trimmed at both ends, lowercased.
//! [`normalize`] and [`KIND_WORD_SQL`] must say the same thing, otherwise the words `signatures` counts
//! cannot be found by `apply`.
//!
//! An agent's judgment records the input it saw (`basis`, the category-word half of 0053, #795): for each candidate class shown to the model, its
//! id, `updated_at`, and ancestor closure ([`ClassSnapshot::basis`]). Stale means the fingerprint recomputed from the current classes no longer matches; moments are not compared, since the judgment is written after the model answers,
//! so a definition edited during that time is invisible to timestamps, and adding or removing a parent edge does not touch `updated_at`. When the judgment is written, the fingerprint is recomputed in the same transaction, and
//! a reply whose fingerprint does not match is
//! not accepted ([`decide_and_apply_if_current`]).

use chrono::{DateTime, Utc};
use sqlx::{Executor, PgPool, Postgres, Transaction};
use std::collections::HashMap;
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// SQL-side normalization: matches [`normalize`]. `$col` is replaced by the column name at the call site.
const KIND_WORD_SQL: &str = "lower(btrim(regexp_replace($col, '\\s+', ' ', 'g')))";

fn kind_word_sql(col: &str) -> String {
    KIND_WORD_SQL.replace("$col", col)
}

/// Normalize a category word: whitespace folded to one space, trimmed at both ends, lowercased. "Company " and "company" are one word.
pub fn normalize(kind_word: &str) -> String {
    kind_word
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// A category word's signature: everything shown to the model when judging which class it should bind to.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct KindWordSignature {
    /// The normalized word
    pub kind_word: String,
    /// Spellings seen in documents (at most 3, by occurrence count)
    pub words: Vec<String>,
    /// The number of live entities under this word
    pub count: i64,
    /// Example names (at most 3; named ones first, then by creation order)
    pub examples: Vec<String>,
    /// The most common relation phrases among open statements with these entities as subject (at most 3)
    pub phrases: Vec<String>,
}

/// Each distinct category word in the KB: live (`merged_into` is empty) entities carrying the category word.
pub async fn signatures<'e>(
    pool: impl Executor<'e, Database = Postgres>,
    kb_id: Uuid,
) -> AppResult<Vec<KindWordSignature>> {
    let sql = format!(
        "WITH live AS (
             SELECT e.id, e.canonical_name, e.description, e.created_at,
                    btrim(regexp_replace(e.specific_type, '\\s+', ' ', 'g')) AS spelling,
                    {kind} AS kind_word
             FROM entities e
             WHERE e.kb_id = $1 AND e.merged_into IS NULL
               AND e.specific_type IS NOT NULL AND btrim(e.specific_type) <> ''
         ),
         -- 写法、例名、关系短语的前 3 个各在一趟里聚出来。从前每个词各跑三个关联子查询，
         -- 每个都把全部实体（短语那个还连着陈述）重扫一遍，耗时随库的大小平方增长（#1096）
         grouped AS (
             SELECT kind_word, count(*) AS count,
                    (array_agg(canonical_name ORDER BY (description IS NOT NULL), created_at, id))[1:3] AS examples
             FROM live GROUP BY kind_word
         ),
         spellings AS (
             SELECT kind_word, spelling,
                    row_number() OVER (PARTITION BY kind_word ORDER BY count(*) DESC, spelling) AS rn
             FROM live GROUP BY kind_word, spelling
         ),
         words AS (
             SELECT kind_word, array_agg(spelling ORDER BY rn) AS words
             FROM spellings WHERE rn <= 3 GROUP BY kind_word
         ),
         phrase_counts AS (
             SELECT l.kind_word, f.phrase,
                    row_number() OVER (PARTITION BY l.kind_word ORDER BY count(*) DESC, f.phrase) AS rn
             FROM facts f JOIN live l ON l.id = f.subject_id
             WHERE f.kb_id = $1 AND f.layer = 'open' AND f.invalidated_at IS NULL
               AND f.phrase IS NOT NULL
             GROUP BY l.kind_word, f.phrase
         ),
         phrases AS (
             SELECT kind_word, array_agg(phrase ORDER BY rn) AS phrases
             FROM phrase_counts WHERE rn <= 3 GROUP BY kind_word
         )
         SELECT g.kind_word,
                g.count,
                coalesce(w.words, '{{}}') AS words,
                g.examples,
                coalesce(p.phrases, '{{}}') AS phrases
         FROM grouped g
         LEFT JOIN words w ON w.kind_word = g.kind_word
         LEFT JOIN phrases p ON p.kind_word = g.kind_word
         ORDER BY g.count DESC, g.kind_word",
        kind = kind_word_sql("e.specific_type")
    );
    Ok(sqlx::query_as(&sql).bind(kb_id).fetch_all(pool).await?)
}

/// What a category word was judged to be.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Binding {
    pub kind_word: String,
    /// Has a value when `status = 'bound'`
    pub type_id: Option<Uuid>,
    /// bound / none / undecided
    pub status: String,
    pub decided_at: DateTime<Utc>,
    /// agent / person
    pub decided_by: String,
    /// The fingerprint of the input the agent judged with ([`ClassSnapshot::basis`]). Human judgments do not carry one — humans are not re-judged by fingerprint;
    /// agent judgments from before this column existed are also empty, and each of them is judged once more
    pub basis: Option<String>,
}

/// All bindings in the KB, in word order.
pub async fn bindings<'e>(
    pool: impl Executor<'e, Database = Postgres>,
    kb_id: Uuid,
) -> AppResult<Vec<Binding>> {
    Ok(sqlx::query_as(
        "SELECT kind_word, type_id, status, decided_at, decided_by, basis
         FROM type_bindings WHERE kb_id = $1 ORDER BY kind_word",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}

/// The classes in a KB at this moment: each class's `updated_at` and direct parent. The fingerprint is computed from it. A snapshot is read at the start, in the transaction before the model is called,
/// and another is read in the transaction that writes the judgment; if the two computed values differ, the candidate classes changed
/// during the answering.
#[derive(Debug, Clone, Default)]
pub struct ClassSnapshot {
    versions: HashMap<Uuid, DateTime<Utc>>,
    parents: HashMap<Uuid, Vec<Uuid>>,
}

/// Read a KB's [`ClassSnapshot`].
pub async fn class_snapshot<'e>(
    pool: impl Executor<'e, Database = Postgres>,
    kb_id: Uuid,
) -> AppResult<ClassSnapshot> {
    let rows: Vec<(Uuid, DateTime<Utc>, Vec<Uuid>)> = sqlx::query_as(
        "SELECT t.id, t.updated_at,
                ARRAY(SELECT p.parent_id FROM entity_type_parents p WHERE p.child_id = t.id)
         FROM entity_types t WHERE t.kb_id = $1",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    let mut snapshot = ClassSnapshot::default();
    for (id, at, parents) in rows {
        snapshot.versions.insert(id, at);
        snapshot.parents.insert(id, parents);
    }
    Ok(snapshot)
}

impl ClassSnapshot {
    /// A category word's judgment fingerprint: for each candidate class shown to the model, its id, `updated_at`, and ancestor closure.
    /// The order of candidates does not count (the second vote is given in reverse anyway); a candidate that no longer exists is recorded as gone, which also counts as changed.
    /// Adding or removing a parent edge does not touch `updated_at`, so the closure is included separately.
    pub fn basis(&self, candidates: &[Uuid]) -> String {
        let mut parts: Vec<String> = candidates
            .iter()
            .map(|id| match self.versions.get(id) {
                Some(at) => {
                    let up: Vec<String> = self.ancestors(*id).iter().map(Uuid::to_string).collect();
                    format!("{id}@{}^{}", at.to_rfc3339(), up.join(","))
                }
                None => format!("{id}@gone"),
            })
            .collect();
        parts.sort();
        parts.dedup();
        fingerprint(&parts.join(";"))
    }

    /// All ancestors of a class (excluding itself), sorted. Multiple inheritance and diamonds are treated as a union, with each ancestor entered once;
    /// the editor does not allow cycles, so a visited one is not walked again, and even if there were a cycle the walk still terminates.
    fn ancestors(&self, id: Uuid) -> Vec<Uuid> {
        let mut seen: Vec<Uuid> = Vec::new();
        let mut stack: Vec<Uuid> = self.parents.get(&id).cloned().unwrap_or_default();
        while let Some(p) = stack.pop() {
            if p == id || seen.contains(&p) {
                continue;
            }
            seen.push(p);
            if let Some(up) = self.parents.get(&p) {
                stack.extend_from_slice(up);
            }
        }
        seen.sort();
        seen
    }
}

/// The same hash as `phrase_bindings::basis_of`: FNV-1a 64-bit. It is only the key for cache invalidation, not a security
/// use, and not worth pulling in a hash dependency for; nor do we touch that side — if its value changes, all phrase judgments would need re-judging.
fn fingerprint(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// A binding no longer holds: the class it binds to was changed after the judgment; or, after a judgment of none / undecided, a class was
/// created or modified in the library. A negative judgment has no selected class, and a new definition on an existing class may also make it match.
/// If the bound class was deleted, the row has already gone via cascade, so it does not appear here.
///
/// The alignment worker no longer reads this: timestamps do not see edits made during the model's answering (#795), so staleness is judged by fingerprint
/// ([`ClassSnapshot::basis`]). Kept for callers that only need a coarse signal.
pub async fn stale(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT b.kind_word
         FROM type_bindings b
         LEFT JOIN entity_types t ON t.id = b.type_id
         WHERE b.kb_id = $1
           AND ((b.status = 'bound' AND t.updated_at > b.decided_at)
                OR (b.status IN ('none', 'undecided')
                    AND b.decided_at < (SELECT max(updated_at) FROM entity_types
                                        WHERE kb_id = $1)))
         ORDER BY b.kind_word",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}

/// Records a category word's judgment (replacing if present). Returns whether it was written.
///
/// **A human's judgment is not overwritten by an agent**: if the existing row is a human's and this one is the agent's, it stays as-is and returns false.
/// The reverse is allowed: a human may change an agent's. When `words` is empty, the existing spellings are kept — a human deciding in the UI may not have
/// the signature at hand. A judgment written here carries no fingerprint; agent judgments go through [`decide_and_apply_if_current`].
#[allow(clippy::too_many_arguments)]
pub async fn decide<'e>(
    pool: impl Executor<'e, Database = Postgres>,
    kb_id: Uuid,
    kind_word: &str,
    words: &[String],
    type_id: Option<Uuid>,
    status: &str,
    votes: &serde_json::Value,
    decided_by: &str,
) -> AppResult<bool> {
    decide_with_basis(
        pool, kb_id, kind_word, words, type_id, status, votes, decided_by, None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn decide_with_basis<'e>(
    pool: impl Executor<'e, Database = Postgres>,
    kb_id: Uuid,
    kind_word: &str,
    words: &[String],
    type_id: Option<Uuid>,
    status: &str,
    votes: &serde_json::Value,
    decided_by: &str,
    basis: Option<&str>,
) -> AppResult<bool> {
    if !matches!(status, "bound" | "none" | "undecided") {
        return Err(AppError::Validation(format!(
            "unknown binding status {status:?}"
        )));
    }
    if (status == "bound") != type_id.is_some() {
        return Err(AppError::Validation(
            "a bound kind word needs a class and an unbound one must not have one".into(),
        ));
    }
    if !matches!(decided_by, "agent" | "person") {
        return Err(AppError::Validation(format!(
            "unknown decider {decided_by:?}"
        )));
    }
    let kind_word = normalize(kind_word);
    if kind_word.is_empty() {
        return Err(AppError::Validation(
            "an empty kind word binds nothing".into(),
        ));
    }
    let res = sqlx::query(
        "INSERT INTO type_bindings
             (id, kb_id, kind_word, words, type_id, status, votes, decided_at, decided_by, basis)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now(), $8, $9)
         ON CONFLICT (kb_id, kind_word) DO UPDATE
            SET words = CASE WHEN cardinality(EXCLUDED.words) = 0
                             THEN type_bindings.words ELSE EXCLUDED.words END,
                type_id = EXCLUDED.type_id,
                status = EXCLUDED.status,
                votes = EXCLUDED.votes,
                decided_at = now(),
                decided_by = EXCLUDED.decided_by,
                basis = EXCLUDED.basis
          WHERE NOT (type_bindings.decided_by = 'person' AND EXCLUDED.decided_by = 'agent')",
    )
    .bind(Uuid::now_v7())
    .bind(kb_id)
    .bind(&kind_word)
    .bind(words)
    .bind(type_id)
    .bind(status)
    .bind(votes)
    .bind(decided_by)
    .bind(basis)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Store a decision and its entity projection in one transaction.
/// The binding row stays locked until the projection is written, so an older
/// agent cannot apply its class after a person's newer decision has committed.
/// A rejected agent decision changes neither the binding nor the entities.
#[allow(clippy::too_many_arguments)]
pub async fn decide_and_apply(
    pool: &PgPool,
    kb_id: Uuid,
    kind_word: &str,
    words: &[String],
    type_id: Option<Uuid>,
    status: &str,
    votes: &serde_json::Value,
    decided_by: &str,
) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    let written = write_decision_and_projection(
        &mut tx, kb_id, kind_word, words, type_id, status, votes, decided_by, None,
    )
    .await?;
    tx.commit().await?;
    Ok(written)
}

/// What became of an agent decision offered to [`decide_and_apply_if_current`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    /// The decision and its projection are written, with the basis.
    Written,
    /// A person decided this kind word; the agent's answer does not replace it.
    KeptPerson,
    /// The candidate classes changed while the model was answering. The answer
    /// is about inputs that no longer exist, so nothing is written.
    Moved,
}

/// Accept an agent decision only for the inputs it actually read (#795).
///
/// `basis` is what the run computed from its snapshot before calling the model.
/// The candidate classes are locked `FOR SHARE` before the binding row is written:
/// a class delete takes the class row first and then cascades to the binding, so
/// taking them in the same order cannot deadlock with it. The basis is then
/// recomputed from the rows as they are now; a mismatch means the model answered
/// about a definition or hierarchy that has since changed, and nothing is written.
///
/// A parent edge or a new class committed after this check is not blocked. The
/// stored basis then no longer matches the current inputs, so the next run finds
/// the decision stale and asks again.
#[allow(clippy::too_many_arguments)]
pub async fn decide_and_apply_if_current(
    pool: &PgPool,
    kb_id: Uuid,
    kind_word: &str,
    words: &[String],
    type_id: Option<Uuid>,
    status: &str,
    votes: &serde_json::Value,
    basis: &str,
    candidates: &[Uuid],
) -> AppResult<Acceptance> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT id FROM entity_types WHERE kb_id = $1 AND id = ANY($2) ORDER BY id FOR SHARE",
    )
    .bind(kb_id)
    .bind(candidates)
    .fetch_all(&mut *tx)
    .await?;
    if class_snapshot(&mut *tx, kb_id).await?.basis(candidates) != basis {
        tx.rollback().await?;
        return Ok(Acceptance::Moved);
    }
    let written = write_decision_and_projection(
        &mut tx,
        kb_id,
        kind_word,
        words,
        type_id,
        status,
        votes,
        "agent",
        Some(basis),
    )
    .await?;
    tx.commit().await?;
    Ok(if written {
        Acceptance::Written
    } else {
        Acceptance::KeptPerson
    })
}

/// The review request may wait briefly for a concurrent writer, but must not
/// pin a connection indefinitely. This is per lock acquisition, not a request
/// deadline, and does not change the background aligner's waiting policy.
///
/// The decision, its projection and the phrase alignment it makes necessary
/// commit together: a changed class moves the phrase signatures of every entity
/// under this kind word, and a job queued after the commit could be lost with
/// the process in between.
pub async fn decide_and_apply_human(
    pool: &PgPool,
    kb_id: Uuid,
    kind_word: &str,
    type_id: Option<Uuid>,
    votes: &serde_json::Value,
) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    let result = async {
        sqlx::query("SET LOCAL lock_timeout = '2s'")
            .execute(&mut *tx)
            .await?;
        let written = decide_and_apply_human_tx(&mut tx, kb_id, kind_word, type_id, votes).await?;
        if written {
            crate::jobs::enqueue_unless_queued_tx(
                &mut tx,
                "align_phrases",
                serde_json::json!({ "kb_id": kb_id }),
            )
            .await?;
        }
        Ok::<bool, AppError>(written)
    }
    .await;
    match result {
        Ok(written) => {
            tx.commit().await?;
            Ok(written)
        }
        Err(error) => {
            // Finish rollback before returning a retryable response or reusing
            // the connection. Preserve both errors if cleanup itself fails.
            if let Err(rollback) = tx.rollback().await {
                return Err(AppError::Other(anyhow::Error::new(error).context(format!(
                    "rolling back human kind-word decision: {rollback}"
                ))));
            }
            Err(busy_on_lock_timeout(error))
        }
    }
}

/// A person's decision and its entity projection, written as [`decide_and_apply_human`]
/// writes them but on the caller's transaction, so they commit or roll back with the
/// caller's other writes. The caller queues the phrase alignment the changed classes
/// need in the same transaction, and sets its own lock timeout and maps it with
/// [`busy_on_lock_timeout`].
pub async fn decide_and_apply_human_tx(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    kind_word: &str,
    type_id: Option<Uuid>,
    votes: &serde_json::Value,
) -> AppResult<bool> {
    write_decision_and_projection(
        tx,
        kb_id,
        kind_word,
        &[],
        type_id,
        if type_id.is_some() { "bound" } else { "none" },
        votes,
        "person",
        None,
    )
    .await
}

/// A lock wait that ran out (55P03 under `lock_timeout`) means another operation
/// is updating the kind word: answer the retryable 409 the review page knows.
pub fn busy_on_lock_timeout(error: AppError) -> AppError {
    if matches!(&error, AppError::Db(sqlx::Error::Database(e))
        if e.code().as_deref() == Some("55P03"))
    {
        return AppError::CodedConflict {
            code: "alignment_busy",
            message:
                "This kind word is being updated by another operation. Please try again shortly."
                    .into(),
        };
    }
    error
}

#[allow(clippy::too_many_arguments)]
async fn write_decision_and_projection(
    connection: &mut sqlx::PgConnection,
    kb_id: Uuid,
    kind_word: &str,
    words: &[String],
    type_id: Option<Uuid>,
    status: &str,
    votes: &serde_json::Value,
    decided_by: &str,
    basis: Option<&str>,
) -> AppResult<bool> {
    let written = decide_with_basis(
        &mut *connection,
        kb_id,
        kind_word,
        words,
        type_id,
        status,
        votes,
        decided_by,
        basis,
    )
    .await?;
    if written {
        match type_id {
            Some(id) => {
                apply(&mut *connection, kb_id, kind_word, id).await?;
            }
            None => {
                unapply(&mut *connection, kb_id, kind_word).await?;
            }
        }
    }
    Ok(written)
}

/// Writes the bound class onto every live entity under this category word whose class a human has not set. Returns the number changed.
///
/// An entity already on this class is not a change (`IS DISTINCT FROM`: `type_id` may be NULL). Once it has a class,
/// the "suggest a class" is no longer a suggestion, so `proposed_type` is cleared along with it — otherwise the ontology page would keep asking to create a class
/// for a word that already has one.
pub async fn apply<'e>(
    pool: impl Executor<'e, Database = Postgres>,
    kb_id: Uuid,
    kind_word: &str,
    type_id: Uuid,
) -> AppResult<u64> {
    let sql = format!(
        "UPDATE entities
            SET type_id = $3, type_source = 'aligned', proposed_type = NULL, updated_at = now()
          WHERE kb_id = $1 AND merged_into IS NULL AND type_source <> 'human'
            AND specific_type IS NOT NULL AND {kind} = $2
            AND type_id IS DISTINCT FROM $3",
        kind = kind_word_sql("specific_type")
    );
    let res = sqlx::query(&sql)
        .bind(kb_id)
        .bind(normalize(kind_word))
        .bind(type_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// When a binding becomes none or goes stale: entities aligned to it through it lose that class. Only `aligned` rows are touched —
/// classes that extraction judged, the engine guessed, or a human picked are not what this binding gave. Returns the number changed.
pub async fn unapply<'e>(
    pool: impl Executor<'e, Database = Postgres>,
    kb_id: Uuid,
    kind_word: &str,
) -> AppResult<u64> {
    let sql = format!(
        "UPDATE entities
            SET type_id = NULL, type_source = 'extracted', updated_at = now()
          WHERE kb_id = $1 AND merged_into IS NULL AND type_source = 'aligned'
            AND specific_type IS NOT NULL AND {kind} = $2",
        kind = kind_word_sql("specific_type")
    );
    let res = sqlx::query(&sql)
        .bind(kb_id)
        .bind(normalize(kind_word))
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Fills in, across the KB, each bound class onto entities under its category word that are not yet on that class. Returns the number changed.
///
/// [`apply`] projects only once, at the moment a judgment lands, and cannot see entities created later. Extraction reads the binding table (`bound_map`) once when it starts, so when
/// "company" is bound partway through one document, the enterprise class is bound, and the companies created in the second half of that document
/// become entities with no class; when several documents upload concurrently, the first one's judgment counts as "partway through" for the others. A subject with no class
/// cannot be a candidate for any attribute with a declared domain, so all its attribute statements come out as "none" — the six registered-capital, founding-date
/// and headcount rows in the table of the fourth test document were lost exactly this way (2026-10-22).
///
/// Run once after each document's extraction and before each category-word alignment round starts. Idempotent: entities already on that class are not changed. The same rule as [`apply`]
/// applies: entities a human set a class for are left alone (`type_source = 'human'`), and `proposed_type` is cleared along with it
pub async fn apply_bound<'e>(
    pool: impl Executor<'e, Database = Postgres>,
    kb_id: Uuid,
) -> AppResult<u64> {
    let sql = format!(
        "UPDATE entities e
            SET type_id = b.type_id, type_source = 'aligned', proposed_type = NULL, updated_at = now()
           FROM type_bindings b
          WHERE e.kb_id = $1 AND b.kb_id = e.kb_id
            AND b.status = 'bound' AND b.type_id IS NOT NULL
            AND e.merged_into IS NULL AND e.type_source <> 'human'
            AND e.specific_type IS NOT NULL AND {kind} = b.kind_word
            AND e.type_id IS DISTINCT FROM b.type_id",
        kind = kind_word_sql("e.specific_type")
    );
    let res = sqlx::query(&sql).bind(kb_id).execute(pool).await?;
    Ok(res.rows_affected())
}

/// Binding table used during extraction: normalized category word → class id, bound ones only.
pub async fn bound_map(pool: &PgPool, kb_id: Uuid) -> AppResult<HashMap<String, Uuid>> {
    let rows: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT kind_word, type_id FROM type_bindings
         WHERE kb_id = $1 AND status = 'bound' AND type_id IS NOT NULL",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// For category words with no matching class: records the class name the model proposed on the entity's `proposed_type`; type resolution reads it.
///
/// Written only the first time, at most 60 characters; proposes only live entities that have no class yet and that a human has not
/// set — a human-set entity would not be claimed on adoption, so counting it in "will be reclassified N" would be false.
/// Returns the number of rows written.
pub async fn propose(
    pool: &PgPool,
    kb_id: Uuid,
    kind_word: &str,
    spelling: &str,
) -> AppResult<u64> {
    let spelling = spelling.trim();
    if spelling.is_empty() {
        return Err(AppError::Validation(
            "a proposal needs the document's spelling".into(),
        ));
    }
    let sql = format!(
        "UPDATE entities SET proposed_type = left($3, 60)
          WHERE kb_id = $1 AND merged_into IS NULL AND type_id IS NULL
            AND type_source <> 'human' AND proposed_type IS NULL
            AND specific_type IS NOT NULL AND {kind} = $2",
        kind = kind_word_sql("specific_type")
    );
    let res = sqlx::query(&sql)
        .bind(kb_id)
        .bind(normalize(kind_word))
        .bind(spelling)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
#[path = "type_bindings_catch_up_tests.rs"]
mod catch_up_tests;

#[cfg(test)]
mod tests {
    use super::{normalize, ClassSnapshot};
    use chrono::{DateTime, Utc};
    use uuid::Uuid;

    #[test]
    fn a_kind_word_is_one_word_however_spaced_or_cased() {
        assert_eq!(
            normalize("  Stockholder   Proposal "),
            "stockholder proposal"
        );
        assert_eq!(normalize("Company"), "company");
        assert_eq!(normalize("指标"), "指标");
        assert_eq!(normalize("   "), "");
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("timestamp")
    }

    /// organization ⊂ legal_entity; agent has no parent class for now
    fn snapshot() -> (ClassSnapshot, [Uuid; 3]) {
        let ids = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
        let [org, legal, _] = ids;
        let mut s = ClassSnapshot::default();
        for id in ids {
            s.versions.insert(id, at("2026-09-26T00:00:00Z"));
            s.parents.insert(id, Vec::new());
        }
        s.parents.insert(org, vec![legal]);
        (s, ids)
    }

    #[test]
    fn a_basis_names_what_was_shown_not_the_order_it_was_shown_in() {
        let (s, [org, legal, _]) = snapshot();
        assert_eq!(s.basis(&[org, legal]), s.basis(&[legal, org]));
        assert_eq!(s.basis(&[org, legal]), s.basis(&[org, legal, org]));
        assert_ne!(s.basis(&[org, legal]), s.basis(&[org]));
    }

    #[test]
    fn an_edit_a_grandparent_edge_or_a_deleted_candidate_changes_the_basis() {
        let (s, [org, legal, agent]) = snapshot();
        let before = s.basis(&[org]);

        let mut edited = s.clone();
        edited.versions.insert(org, at("2026-09-26T00:00:01Z"));
        assert_ne!(before, edited.basis(&[org]), "an edit moves updated_at");

        // Adding or removing a parent edge does not touch updated_at: if org's grandparent changes, org's fingerprint must change too
        let mut rooted = s.clone();
        rooted.parents.insert(legal, vec![agent]);
        assert_ne!(
            before,
            rooted.basis(&[org]),
            "a grandparent is in the closure"
        );

        let mut gone = s.clone();
        gone.versions.remove(&org);
        gone.parents.remove(&org);
        assert_ne!(
            before,
            gone.basis(&[org]),
            "a deleted candidate is a change"
        );
    }

    #[test]
    fn a_diamond_or_a_cycle_still_gives_one_closure() {
        let (mut s, [org, legal, agent]) = snapshot();
        // Diamond: org → legal, org → agent, legal → agent
        s.parents.insert(org, vec![legal, agent]);
        s.parents.insert(legal, vec![agent]);
        assert_eq!(s.ancestors(org), {
            let mut v = vec![legal, agent];
            v.sort();
            v
        });
        // The editor does not allow cycles; if one exists anyway, it still terminates and does not count itself as an ancestor
        s.parents.insert(agent, vec![org]);
        assert!(!s.ancestors(org).contains(&org));
        let _ = s.basis(&[org, legal, agent]);
    }
}
