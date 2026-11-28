//! Open statements under a bound signature become typed facts (pieces 3 and 4 of decision 3 of 0044; see 0067, 0068).
//!
//! The typed graph is a view: one typed row = one triple, drawn from one or more open statements and their signature bindings.
//! The predicate is the attribute the binding gives; subject and object follow the binding's direction (reverse means the statement's object becomes the subject); literal values,
//! temporal axis, source time, and confidence come from the statement; evidence and qualifiers are each copied once; sources are recorded in `typed_fact_sources`,
//! with `from_statement_id` being the first. Statements carrying a mood qualifier do not count.
//!
//! Writing rows goes through the typed graph's own gate ([`crate::graph::insert_fact`] / [`crate::graph::insert_value_fact`]): the same assertion
//! with the same start reuses that row; a bare row is replaced by a timed observation and chained, and an "ended" marker closes the open row — two documents saying
//! the same thing make one edge on the timeline.
//!
//! Recomputation is a set operation, and the result is the same however many times it runs: first delete sources that no longer hold (statement voided, signature no longer bound,
//! bound to another attribute or reversed, the row itself voided), then void typed rows whose sources are all gone, and finally fill in the (statement, binding) pairs that should exist but do not
//! — a row with the same assertion absorbs the pair, and only when there is none is a new row created. No model call.
//!
//! The written rows are then **reconciled** (#899): on the write path, extraction and a confirmed state fact each recompute the timeline along the attribute's uniqueness
//! direction (`temporal::reconcile_new_fact`); a materialized row is the same kind of observation, so this step must not be skipped
//! — otherwise two values of a functional attribute would each stay open, and a later observation could not close the earlier one. Reconciliation runs
//! after the recomputation commits: each reconciliation opens its own transaction and takes its own locks, so it is not done inside the materialization transaction.

use sqlx::PgPool;
use utopia_core::AppResult;
use uuid::Uuid;

use crate::graph::{insert_fact_on, FactObject, Validity};
use serde_json::Value;

/// The condition for a statement and a binding to match: after phrase normalization they are equal, the classes at both ends are the same (empty matches empty), and whether the object is a literal matches.
/// `s` is the open statement (facts), `se`/`oe` are its two entities, `b` is the phrase_bindings
const MATCH: &str = "b.kb_id = s.kb_id
       AND b.phrase = lower(btrim(regexp_replace(s.phrase, '\\s+', ' ', 'g')))
       AND b.subject_type_id IS NOT DISTINCT FROM se.type_id
       AND b.object_is_value = (s.object_id IS NULL)
       AND (s.object_id IS NULL OR b.object_type_id IS NOT DISTINCT FROM oe.type_id)";

/// How a statement that states just one moment (start equals end) bound to a state attribute is read, see [`span_of`]: the binding says
/// `start` runs from that moment on, `end` runs up to that moment, and a binding that says `none` or says nothing reads the span named by that date
/// (0053 revision 2026-10-22). Previously `none` and unstated did not make typed rows (`MOMENT_UNREAD`), and every annual figure in four company
/// profiles was lost that way; copying the start and end would write a state that holds at no moment, so the end
/// moves to the end of the bucket, marked with `end_after_period`, so the reader can print the 2023 the original wrote
/// The attribute's datatype and unit; values are read by them ([`attribute_value`]). `rt` is the attribute bound to or implied by the rule,
/// when it is a relation the datatype is NULL and the value is written as-is
const SHAPE: &str = "CASE WHEN rt.kind = 'attribute' THEN rt.datatype END AS datatype, rt.unit";

/// What one round of recomputation wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Voided typed rows (their sources were all gone)
    pub retired: u64,
    /// Newly created typed rows
    pub added: u64,
    /// Statements merged into existing rows
    pub merged: u64,
    /// Implied rows derived by a rule (piece 5 of decision 3 of 0044), newly created
    pub implied: u64,
    /// The number of correction rows rewritten by reconciliation's automatic closing (#899)
    pub corrected: u64,
    /// The number of conflicts reconciliation could not decide and handed to a human
    pub conflicts: u32,
}

/// A (statement, binding) pair to materialize, plus what to copy from the statement.
#[derive(sqlx::FromRow)]
struct Due {
    statement: Uuid,
    property: Uuid,
    direction: String,
    subject_id: Uuid,
    object_id: Option<Uuid>,
    /// When the object is an entity, its name: bound to a text attribute, the name is the value (0044 revision 2026-10-22)
    object_name: Option<String>,
    object_value: Option<serde_json::Value>,
    valid_from: Option<chrono::DateTime<chrono::Utc>>,
    valid_from_precision: Option<String>,
    valid_from_grade: Option<String>,
    valid_to: Option<chrono::DateTime<chrono::Utc>>,
    valid_to_precision: Option<String>,
    attested_from: Option<chrono::DateTime<chrono::Utc>>,
    confidence: f32,
    /// Which end the binding marks a moment on (start / end / none, NULL unknown)
    marks: Option<String>,
    /// The temporal semantics of the attribute bound to
    temporal: Option<String>,
    /// Whether what is bound is a relation or an attribute
    kind: String,
    /// When the bound thing is an attribute, its datatype and unit; values are read by them ([`attribute_value`])
    datatype: Option<String>,
    unit: Option<String>,
}

/// Recomputes one KB.
///
/// The worker takes this path: it can wait as long as needed, because nobody is waiting in front of a screen. Human judgments do not take this path:
/// see `materialize_human`, which is another entry point with a time budget
pub async fn materialize(pool: &PgPool, kb_id: Uuid) -> AppResult<Outcome> {
    // A worker and a human review can recompute the same base concurrently.
    // Serialize before reading due statements, and use this connection for the
    // whole recompute so waiting runs cannot exhaust the pool with lock holders.
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('typed_materialize'), hashtext($1))")
        .bind(kb_id.to_string())
        .execute(&mut *tx)
        .await?;
    let (outcome, written) = materialize_in_tx(&mut tx, kb_id).await?;
    tx.commit().await?;
    reconcile_written(pool, kb_id, outcome, &written).await
}

/// The typed rows written by this round (newly created or merged) are reconciled along their own uniqueness timelines (#899). Only state rows and
/// predicates that declare uniqueness have timelines; `timelines_of` filters by itself, and the others spin idle here
async fn reconcile_written(
    pool: &PgPool,
    kb_id: Uuid,
    mut outcome: Outcome,
    written: &[Uuid],
) -> AppResult<Outcome> {
    if !written.is_empty() {
        let report = crate::temporal::reconcile_facts(pool, kb_id, written).await?;
        outcome.corrected = report.corrected.len() as u64;
        outcome.conflicts = report.conflicts;
    }
    Ok(outcome)
}

/// The entry point inside a task (0051): **try the lock, do not wait.** If it gets `typed_materialize`, it runs the full recomputation
/// on this connection and commits; if it cannot get the lock, it rolls back and returns `None`, and the caller marks it `Deferred` to come back later.
///
/// Why not wait for the lock as `materialize` does: waiting for the lock holds a connection from the pool, so several decisions clicked in a row each
/// queue a job holding a connection, and the pool empties (0051 §Alternatives). Nor is it like the old
/// human entry point with its 2-second wait budget: a job has nobody in front of a screen, so a timeout merely pushes the same recomputation
/// to the next retry; it is better not to wait at all. A human's single click commits only the decision and this job (same transaction,
/// `phrase_bindings::decide_with_delivery`); what the screen waits on is an event, not a lock.
pub async fn try_materialize(pool: &PgPool, kb_id: Uuid) -> AppResult<Option<Outcome>> {
    let mut tx = pool.begin().await?;
    let acquired: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext('typed_materialize'), hashtext($1))",
    )
    .bind(kb_id.to_string())
    .fetch_one(&mut *tx)
    .await?;
    if !acquired {
        tx.rollback().await?;
        return Ok(None);
    }
    let (outcome, written) = materialize_in_tx(&mut tx, kb_id).await?;
    tx.commit().await?;
    Ok(Some(
        reconcile_written(pool, kb_id, outcome, &written).await?,
    ))
}

async fn materialize_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kb_id: Uuid,
) -> AppResult<(Outcome, Vec<Uuid>)> {
    // The statements that scan the whole base (steps 0 to 3, the implication scans and the
    // attestation sync) are not cached: a cached statement switches to a generic plan after a few
    // runs, and a generic plan cannot see how many rows `kb_id = $1` selects. The source cleanup's
    // generic plan ran for hours on a production base where the plan for its own id takes two
    // seconds (#1298). The per-row statements in the loops stay cached.
    // The rows written this round (newly created and merged) are reconciled after commit
    let mut written: Vec<Uuid> = Vec::new();
    // 0. A materialized state row whose start equals its end (copied as-is before #966 wrote a moment) holds at no moment: void it; its statement
    //    is recomputed in step 3 with the reading the binding gives — "joined that day" and "since that day" settle again into a span starting that day.
    //    Only copies are recognized: it has a source statement whose two ends are exactly this moment (typed rows check typed_fact_sources; implied rows
    //    check implied_fact_sources). A row a human edited to have equal start and end also carries from_statement_id, implied, and
    //    source links (#967, #911) and looks like a computed one, but its source statement is not this moment, so it is left alone:
    //    the invariant blocks new ones from now on, and humans fix the old ones by hand. It runs before step 1: until the binding gives the reading, a statement at that moment does not count as a source, and step 1 would delete this link first.
    //    Only computed rows are touched (from_statement_id or
    //    implied, the same guard as step 2): a row a human wrote that absorbed a statement also has source links, but it is not computed
    let emptied = sqlx::query(
        "UPDATE facts t
            SET invalidated_at = now()
          WHERE t.kb_id = $1 AND t.layer = 'typed' AND t.invalidated_at IS NULL
            AND t.valid_from IS NOT NULL AND t.valid_to = t.valid_from
            AND (t.from_statement_id IS NOT NULL OR t.implied)
            AND EXISTS (SELECT 1 FROM relation_types r
                         WHERE r.id = t.predicate_id AND r.temporal = 'state')
            AND EXISTS (SELECT 1 FROM facts s
                         WHERE s.valid_from = t.valid_from AND s.valid_to = t.valid_from
                           AND (s.id IN (SELECT ts.statement_id FROM typed_fact_sources ts
                                          WHERE ts.fact_id = t.id)
                             OR s.id IN (SELECT i.statement_id FROM implied_fact_sources i
                                          WHERE i.fact_id = t.id)))",
    )
    .persistent(false)
    .bind(kb_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();

    // 1. Delete sources that no longer hold: the statement is dead, the row is dead, the signature no longer binds, the attribute or direction changed, or the statement now carries a mood
    sqlx::query(&source_cleanup_sql())
        .persistent(false)
        .bind(kb_id)
        .execute(&mut **tx)
        .await?;

    // 1b. Sources of implied rows: the rule no longer approves them, the statement that triggered them died or changed its signature, or the entity is gone or changed its category word:
    //     delete the source; same rule as 1, only the source table is different (implied_fact_sources)
    sqlx::query(&format!(
        "DELETE FROM implied_fact_sources i
          USING implication_rules r
          WHERE i.rule_id = r.id AND r.kb_id = $1
            AND (r.status <> 'approved'
                 OR (i.statement_id IS NOT NULL AND NOT EXISTS (
                        SELECT 1 FROM facts s
                          JOIN entities se ON se.id = s.subject_id
                     LEFT JOIN entities oe ON oe.id = s.object_id
                         WHERE s.id = i.statement_id AND s.layer = 'open' AND s.invalidated_at IS NULL
                           AND r.trigger = 'phrase' AND {RULE_MATCH}))
                 OR (i.entity_id IS NOT NULL AND NOT EXISTS (
                        SELECT 1 FROM entities e
                         WHERE e.id = i.entity_id AND e.merged_into IS NULL
                           AND r.trigger = 'kind_word' AND {KIND} = r.phrase)))",
        RULE_MATCH = crate::implication_rules::RULE_MATCH,
        KIND = kind_word_sql("e.specific_type"),
    ))
    .persistent(false)
    .bind(kb_id)
    .execute(&mut **tx)
    .await?;

    // 2. Void typed rows whose sources are all gone: only computed rows are touched (those with from_statement_id, or rule-derived); human-written rows are left alone
    let retired = sqlx::query(
        "UPDATE facts t
            SET invalidated_at = now()
          WHERE t.kb_id = $1 AND t.layer = 'typed' AND t.invalidated_at IS NULL
            AND (t.from_statement_id IS NOT NULL OR t.implied)
            AND NOT EXISTS (SELECT 1 FROM typed_fact_sources src WHERE src.fact_id = t.id)
            AND NOT EXISTS (SELECT 1 FROM implied_fact_sources i WHERE i.fact_id = t.id)",
    )
    .persistent(false)
    .bind(kb_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    let retired = retired + emptied;

    // 3. (statement, binding) pairs that should exist but do not: the statement is alive, the signature is bound, no mood, and no live row yet has it as a source
    //    with the same predicate. reverse only makes sense for a relation between two things (a literal cannot be a subject)
    let due: Vec<Due> = sqlx::query_as(&due_sql())
        .persistent(false)
        .bind(kb_id)
        .fetch_all(&mut **tx)
        .await?;

    let (mut added, mut merged) = (0u64, 0u64);
    // Bound values that cannot be read under their datatype: (statement, attribute, the original word), recorded as discarded rows at the end
    let mut unreadable: Vec<(Uuid, Uuid, String)> = Vec::new();
    for d in &due {
        let reverse = d.direction == "reverse";
        // A moment statement under a state attribute is read as the binding says (#966, 0053 revision 2026-10-22): a start is written as from that moment
        // on, an end as up to that moment (through the `insert_fact_on` "ended on some day" path, closing the open span),
        // and if neither or nothing is said, the attribute is read as the span named by that date, and a relation writes nothing (see `span_of`)
        let Some(validity) = span_of(
            d.kind == "attribute",
            d.temporal.as_deref() == Some("state"),
            d.marks.as_deref(),
            d.valid_from,
            d.valid_from_precision.as_deref(),
            d.valid_from_grade.as_deref(),
            d.valid_to,
            d.valid_to_precision.as_deref(),
            d.attested_from,
        ) else {
            continue;
        };
        // A text attribute bound to a statement whose object is an entity: the object's name is the value (0044 revision 2026-10-22)
        let named_value = (d.kind == "attribute")
            .then(|| {
                d.object_name
                    .as_deref()
                    .map(|n| serde_json::json!({ "value": n }))
            })
            .flatten();
        let object_value = named_value.as_ref().or(d.object_value.as_ref());
        let object_id = if d.kind == "attribute" {
            None
        } else {
            d.object_id
        };
        let (fact, new) = match (reverse, object_id, object_value) {
            (true, Some(object), _) => {
                insert_fact_on(
                    tx,
                    kb_id,
                    object,
                    Some(d.property),
                    FactObject::Entity(d.subject_id),
                    validity,
                    d.confidence,
                )
                .await?
            }
            (false, Some(object), _) => {
                insert_fact_on(
                    tx,
                    kb_id,
                    d.subject_id,
                    Some(d.property),
                    FactObject::Entity(object),
                    validity,
                    d.confidence,
                )
                .await?
            }
            (false, None, Some(value)) => {
                // A value that cannot be read as the attribute's datatype: the statement stays in the open graph and this pair is not written — but a row must be recorded
                // (`attr_datatype`); before, nothing was said, and a number missing from the graph went unexplained
                let Some(value) = attribute_value(value, d.datatype.as_deref(), d.unit.as_deref())
                else {
                    unreadable.push((d.statement, d.property, raw_text(value)));
                    continue;
                };
                insert_fact_on(
                    tx,
                    kb_id,
                    d.subject_id,
                    Some(d.property),
                    FactObject::Value(&value),
                    validity,
                    d.confidence,
                )
                .await?
            }
            _ => continue,
        };
        written.push(fact);
        if new {
            added += 1;
            sqlx::query("UPDATE facts SET from_statement_id = $2 WHERE id = $1 AND from_statement_id IS NULL")
                .bind(fact)
                .bind(d.statement)
                .execute(&mut **tx)
                .await?;
            // A new row replaces a bare row (time refinement, on the supersedes chain): the replaced row's sources move along with it,
            // so this round converges, instead of waiting for the next round to delete the old sources as "no longer holding" and re-add them
            sqlx::query(
                "INSERT INTO typed_fact_sources (fact_id, statement_id)
                 SELECT $1, src.statement_id
                   FROM facts n JOIN typed_fact_sources src ON src.fact_id = n.supersedes
                  WHERE n.id = $1
                 ON CONFLICT DO NOTHING",
            )
            .bind(fact)
            .execute(&mut **tx)
            .await?;
            sqlx::query(
                "DELETE FROM typed_fact_sources src
                  USING facts n
                  WHERE n.id = $1 AND src.fact_id = n.supersedes",
            )
            .bind(fact)
            .execute(&mut **tx)
            .await?;
        } else {
            merged += 1;
        }
        sqlx::query(
            "INSERT INTO typed_fact_sources (fact_id, statement_id) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(fact)
        .bind(d.statement)
        .execute(&mut **tx)
        .await?;
        // Evidence is copied every time: evidence is the same quotation from the same passage
        sqlx::query(
            "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, doc_version,
                                        proposed_predicate, quote_start, quote_end)
             SELECT $1, chunk_id, quote, document_id, doc_version, proposed_predicate,
                    quote_start, quote_end
               FROM fact_evidence WHERE fact_id = $2
             ON CONFLICT DO NOTHING",
        )
        .bind(fact)
        .bind(d.statement)
        .execute(&mut **tx)
        .await?;
        // Role words copy only the roles the row does not have yet (a revision of decision 1 of 0070): when a statement merged into an existing row says a different value for the same role,
        // copying it would make this row read two values at once; the other value is carried over by decision 4
        sqlx::query(
            "INSERT INTO statement_qualifiers (fact_id, role, value, entity_id, chunk_id)
             SELECT $1, q.role, q.value, q.entity_id, q.chunk_id
               FROM statement_qualifiers q
              WHERE q.fact_id = $2
                AND NOT EXISTS (SELECT 1 FROM statement_qualifiers n
                                 WHERE n.fact_id = $1 AND n.role = q.role)
             ON CONFLICT DO NOTHING",
        )
        .bind(fact)
        .bind(d.statement)
        .execute(&mut **tx)
        .await?;
    }
    // 3a. Computed implied rows from approved rules (piece 5 of decision 3 of 0044). Readings only check the cache: this round does not compute those missing from the cache;
    //     they are computed after `read_phrases` fills it in. Phrase rules trigger on statements, category-word rules trigger on entities
    let implied = imply_in_tx(tx, kb_id, &mut written, &mut unreadable).await?;
    record_unreadable(tx, kb_id, &unreadable).await?;
    // 3b. Witnesses follow the source statement (0064 decisions 3, 5): typed rows have no provenance of their own. The write entry used by materialization and implied-row
    //     computation fills in the current moment when a statement has no witness (the default for facts written by humans); here it is switched back to the statement's.
    //     Runs after 3a: rule-derived rows are written this way too.
    //     Existing rows whose witness moved, together with newly written rows, are reconciled after commit: the witness is the moment the timeline uses for ordering.
    written.extend(sync_typed_attestation(&mut **tx, kb_id, None).await?);
    Ok((
        Outcome {
            retired,
            added,
            merged,
            implied,
            corrected: 0,
            conflicts: 0,
        },
        written,
    ))
}

/// The witness sync as one statement. For the whole base (`given` false) it gathers every source of the base.
/// For given rows it gathers only those rows' sources: with the rows named on the outer `UPDATE` alone, Postgres
/// still aggregated every source of the base before it looked at them, and a recompute for seven facts read
/// the base's whole `facts` and all of `typed_fact_sources` (RC-012). The rows it moves are the same.
fn typed_attestation_sql(given: bool) -> String {
    let (of_materialized, of_implied, rows) = if given {
        (
            "WHERE ts.fact_id = ANY($2)",
            "WHERE i.fact_id = ANY($2)",
            "t.id = ANY($2)",
        )
    } else {
        ("", "", "$2::uuid[] IS NULL")
    };
    format!(
        "UPDATE facts t
            SET attested_from = src.at, attested_by = src.by
           FROM (SELECT u.fact_id,
                        min(u.attested_from) AS at,
                        (array_agg(u.attested_by ORDER BY u.attested_from NULLS LAST, u.statement_id))[1] AS by
                   FROM (SELECT ts.fact_id, ts.statement_id,
                                CASE WHEN k.invalidated_at IS NULL THEN k.attested_from END AS attested_from,
                                CASE WHEN k.invalidated_at IS NULL THEN k.attested_by END AS attested_by
                           FROM typed_fact_sources ts
                           JOIN facts k ON k.id = ts.statement_id AND k.kb_id = $1
                          {of_materialized}
                         UNION ALL
                         -- 规则的来源按规则的库筛：类别词规则的来源是实体，没有陈述
                         SELECT i.fact_id, i.statement_id, s.attested_from, s.attested_by
                           FROM implied_fact_sources i
                           JOIN implication_rules r ON r.id = i.rule_id AND r.kb_id = $1
                      LEFT JOIN facts s ON s.id = i.statement_id AND s.kb_id = $1
                                       AND s.invalidated_at IS NULL
                          {of_implied}) u
                  GROUP BY u.fact_id) src
          WHERE t.id = src.fact_id AND t.kb_id = $1 AND t.layer = 'typed'
            AND t.invalidated_at IS NULL
            AND {rows}
            AND (t.implied
                 OR EXISTS (SELECT 1 FROM typed_fact_sources ts WHERE ts.fact_id = t.id))
            AND (t.attested_from IS DISTINCT FROM src.at
                 OR t.attested_by IS DISTINCT FROM src.by)
          RETURNING t.id"
    )
}

/// The witness of a typed row = the earliest witness among its source statements, together with that date's name; if no source statement has a witness,
/// neither does it (it holds at any moment, 0064 decision 5). Source statements come in two places: those materialized into this row
/// (`typed_fact_sources`), and those a rule read this row from (`implied_fact_sources`); for category-word rules
/// the source is the entity, which carries no witness.
///
/// Rows moved by this: those materialized from statements, and those computed by rules (`implied`). A human-written row is left alone when rule conclusions merge into it:
/// its witness is the moment a human wrote it, and `attest_earlier` has already moved it earlier when merging
///
/// Returns the rows whose witness moved. The witness is the moment the unique timeline uses to order rows with no start (`temporal::DATED_AT`),
/// so after commit the caller must reconcile these rows (`temporal::reconcile_moved_facts`) — only the timelines these rows are in,
/// not the whole KB
///
/// A voided source statement bears no witness: materialization itself first removes them from the sources, but the document-deletion path
/// (`documents::reattest_tx`) runs before materialization, when the statement was just voided along with the document and the source table is not yet updated.
/// `only`: touch only these rows (on document deletion, only those citing that document, whose timelines are already locked); `None` is the whole KB
/// (and then only these rows' sources are read, see [`typed_attestation_sql`])
pub async fn sync_typed_attestation<'e>(
    ex: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    kb_id: Uuid,
    only: Option<&[Uuid]>,
) -> AppResult<Vec<Uuid>> {
    let moved = sqlx::query_scalar(&typed_attestation_sql(only.is_some()))
        .persistent(false)
        .bind(kb_id)
        .bind(only)
        .fetch_all(ex)
        .await?;
    Ok(moved)
}

/// The typed rows that follow these statements: materialized from them, or read from them by a rule.
/// A caller that moved the witness of a few statements syncs these rows instead of the whole base (RC-012).
pub async fn typed_rows_following<'e>(
    ex: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    statements: &[Uuid],
) -> AppResult<Vec<Uuid>> {
    if statements.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_scalar(
        "SELECT ts.fact_id FROM typed_fact_sources ts WHERE ts.statement_id = ANY($1)
         UNION
         SELECT i.fact_id FROM implied_fact_sources i WHERE i.statement_id = ANY($1)",
    )
    .bind(statements)
    .fetch_all(ex)
    .await?;
    Ok(rows)
}

#[derive(sqlx::FromRow)]
struct Implied {
    rule: Uuid,
    statement: Option<Uuid>,
    entity: Option<Uuid>,
    subject_id: Uuid,
    property: Uuid,
    object_id: Option<Uuid>,
    object_value: Option<serde_json::Value>,
    valid_from: Option<chrono::DateTime<chrono::Utc>>,
    valid_from_precision: Option<String>,
    valid_from_grade: Option<String>,
    valid_to: Option<chrono::DateTime<chrono::Utc>>,
    valid_to_precision: Option<String>,
    attested_from: Option<chrono::DateTime<chrono::Utc>>,
    confidence: f32,
    /// Temporal semantics of the conclusion attribute: a moment statement landing on a state attribute is read as a span (the rule has no marks)
    temporal: String,
    /// Whether the conclusion is an attribute or a relation: a moment statement is read as a span only on an attribute
    kind: String,
    datatype: Option<String>,
    unit: Option<String>,
}

async fn imply_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kb_id: Uuid,
    written: &mut Vec<Uuid>,
    unreadable: &mut Vec<(Uuid, Uuid, String)>,
) -> AppResult<u64> {
    // Phrase rules: live statements without mood under the signature; the object is the reading's answer (the entity or value from the cache),
    // with no reading, it is the statement's object. Those that already have a live implied row with this statement as the source are not computed again
    let by_statement: Vec<Implied> = sqlx::query_as(&format!(
        "SELECT r.id AS rule, s.id AS statement, NULL::uuid AS entity,
                s.subject_id, r.conclude_property_id AS property,
                CASE WHEN r.reading IS NULL THEN s.object_id ELSE pr.entity_id END AS object_id,
                CASE WHEN r.reading IS NULL THEN s.object_value ELSE pr.value END AS object_value,
                s.valid_from, s.valid_from_precision, s.valid_from_grade, s.valid_to, s.valid_to_precision,
                s.attested_from, s.confidence, rt.temporal, rt.kind, {SHAPE}
           FROM implication_rules r
           JOIN relation_types rt ON rt.id = r.conclude_property_id
           JOIN facts s ON s.kb_id = r.kb_id AND s.layer = 'open' AND s.invalidated_at IS NULL
           JOIN entities se ON se.id = s.subject_id
      LEFT JOIN entities oe ON oe.id = s.object_id
      LEFT JOIN phrase_readings pr ON r.reading IS NOT NULL AND pr.kb_id = r.kb_id
                                   AND pr.reading = r.reading AND pr.phrase = {TEXT}
          WHERE r.kb_id = $1 AND r.status = 'approved' AND r.trigger = 'phrase'
            AND {RULE_MATCH}
            AND {NO_MOOD}
            AND (r.reading IS NULL OR pr.entity_id IS NOT NULL OR pr.value IS NOT NULL)
            AND NOT EXISTS (SELECT 1 FROM errata_actions ea
                             WHERE ea.statement_id = s.id AND ea.predicate_id = r.conclude_property_id
                               AND ea.status = 'applied' AND ea.action IN ('retract', 'revise'))
            AND NOT EXISTS (SELECT 1 FROM implied_fact_sources i JOIN facts t ON t.id = i.fact_id
                             WHERE i.rule_id = r.id AND i.statement_id = s.id AND t.invalidated_at IS NULL)
          ORDER BY s.id",
        TEXT = crate::implication_rules::object_text_sql(),
        RULE_MATCH = crate::implication_rules::RULE_MATCH,
        NO_MOOD = no_mood_spoken(),
    ))
    .persistent(false)
    .bind(kb_id)
    .fetch_all(&mut **tx)
    .await?;
    // Category-word rules: live entities carrying this category word; the object must come from a reading (the category word itself has no object)
    let by_entity: Vec<Implied> = sqlx::query_as(&format!(
        "SELECT r.id AS rule, NULL::uuid AS statement, e.id AS entity,
                e.id AS subject_id, r.conclude_property_id AS property,
                pr.entity_id AS object_id, pr.value AS object_value,
                NULL::timestamptz AS valid_from, NULL::text AS valid_from_precision, NULL::text AS valid_from_grade,
                NULL::timestamptz AS valid_to, NULL::text AS valid_to_precision,
                NULL::timestamptz AS attested_from, 0.9::real AS confidence, rt.temporal, rt.kind, {SHAPE}
           FROM implication_rules r
           JOIN relation_types rt ON rt.id = r.conclude_property_id
           JOIN entities e ON e.kb_id = r.kb_id AND e.merged_into IS NULL AND {KIND} = r.phrase
           JOIN phrase_readings pr ON pr.kb_id = r.kb_id AND pr.reading = r.reading AND pr.phrase = r.phrase
          WHERE r.kb_id = $1 AND r.status = 'approved' AND r.trigger = 'kind_word' AND r.reading IS NOT NULL
            AND (pr.entity_id IS NOT NULL OR pr.value IS NOT NULL)
            AND NOT EXISTS (SELECT 1 FROM implied_fact_sources i JOIN facts t ON t.id = i.fact_id
                             WHERE i.rule_id = r.id AND i.entity_id = e.id AND t.invalidated_at IS NULL)
          ORDER BY e.id",
        KIND = kind_word_sql("e.specific_type"),
    ))
    .persistent(false)
    .bind(kb_id)
    .fetch_all(&mut **tx)
    .await?;

    let mut implied = 0u64;
    for d in by_statement.iter().chain(by_entity.iter()) {
        // Rules have no marks (#966): when a moment statement implies a state attribute, the conclusion for an attribute is read as a span named by that date,
        // and a relation's is not an implied row (0053 revision 2026-10-22, same as materialization)
        let Some(validity) = span_of(
            d.kind == "attribute",
            d.temporal == "state",
            None,
            d.valid_from,
            d.valid_from_precision.as_deref(),
            d.valid_from_grade.as_deref(),
            d.valid_to,
            d.valid_to_precision.as_deref(),
            d.attested_from,
        ) else {
            continue;
        };
        let value;
        let object = match (d.object_id, &d.object_value) {
            (Some(o), _) => FactObject::Entity(o),
            (None, Some(v)) => {
                let Some(read) = attribute_value(v, d.datatype.as_deref(), d.unit.as_deref())
                else {
                    // The source of a category-word rule is an entity, so there is no document to record; a phrase rule records it on the statement that triggered it
                    if let Some(statement) = d.statement {
                        unreadable.push((statement, d.property, raw_text(v)));
                    }
                    continue;
                };
                value = read;
                FactObject::Value(&value)
            }
            _ => continue,
        };
        // An entity does not imply itself: reading "Virginia" as "United States" is right, while reading "France" as "France"
        // is a self-loop
        if matches!(object, FactObject::Entity(o) if o == d.subject_id) {
            continue;
        }
        let (fact, new) = insert_fact_on(
            tx,
            kb_id,
            d.subject_id,
            Some(d.property),
            object,
            validity,
            d.confidence,
        )
        .await?;
        written.push(fact);
        if new {
            implied += 1;
            sqlx::query("UPDATE facts SET implied = TRUE WHERE id = $1")
                .bind(fact)
                .execute(&mut **tx)
                .await?;
        }
        sqlx::query(
            "INSERT INTO implied_fact_sources (fact_id, rule_id, statement_id, entity_id)
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(fact)
        .bind(d.rule)
        .bind(d.statement)
        .bind(d.entity)
        .execute(&mut **tx)
        .await?;
        // Evidence: a phrase rule copies the quotation of the statement that triggered it — the conclusion a reader draws from this sentence has this sentence as its evidence
        if let Some(statement) = d.statement {
            sqlx::query(
                "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, doc_version,
                                            proposed_predicate, quote_start, quote_end)
                 SELECT $1, chunk_id, quote, document_id, doc_version, proposed_predicate,
                        quote_start, quote_end
                   FROM fact_evidence WHERE fact_id = $2
                 ON CONFLICT DO NOTHING",
            )
            .bind(fact)
            .bind(statement)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(implied)
}

/// The span of a statement when it lands as a typed row. Different start and end are copied as-is; a moment with equal start and end lands on a state attribute, read as the binding says
/// (#966): `start` runs from that moment, `end` runs up to it (through the "ended on some day" path). `none`, or nothing said:
/// it lands on the **attribute** and is read as the span named by that date (0053 revision 2026-10-22) — "2023 revenue of 5.8 billion" is
/// a quantity for the year 2023: the start is that year, the end is the end of the bucket, marked with `end_after_period`, so the reader can print the 2023
/// the original wrote; landing on a relation still makes no row — "joined on 2023-06-01" read as a one-day tenure would shut the
/// open row that starts that same day on the next day (the write entry's rule: same start, an end now given, and the row still open means close it), which is a wrong end.
/// A moment without precision is not a bucket, so no span can be read from it, and it returns None too
#[allow(clippy::too_many_arguments)]
fn span_of<'a>(
    attribute: bool,
    state: bool,
    marks: Option<&str>,
    from: Option<chrono::DateTime<chrono::Utc>>,
    from_precision: Option<&'a str>,
    from_grade: Option<&'a str>,
    to: Option<chrono::DateTime<chrono::Utc>>,
    to_precision: Option<&'a str>,
    attested_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<Validity<'a>> {
    let moment = state && from.is_some() && from == to;
    Some(match (moment, marks) {
        (false, _) => Validity {
            from,
            from_precision,
            from_grade,
            to,
            to_precision,
            attested_at,
            end_after_period: false,
        },
        (true, Some("start")) => Validity {
            from,
            from_precision,
            from_grade,
            to: None,
            to_precision: None,
            attested_at,
            end_after_period: false,
        },
        (true, Some("end")) => Validity {
            from: None,
            from_precision: None,
            from_grade: None,
            to: from,
            to_precision: from_precision,
            attested_at,
            end_after_period: false,
        },
        (true, _) => {
            if !attribute {
                return None;
            }
            let start = from?;
            let end = crate::graph::bucket_end(start, from_precision);
            if end <= start {
                return None;
            }
            Validity {
                from,
                from_precision,
                from_grade,
                to: Some(end),
                to_precision: from_precision,
                attested_at,
                end_after_period: true,
            }
        }
    })
}

/// The original words of the value cell, given to discarded rows as a sample: `{"value": "over 2000 staff"}` → `over 2000 staff`
fn raw_text(raw: &Value) -> String {
    match raw.get("value").unwrap_or(raw) {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Values that cannot be read are recorded as `attr_datatype` discarded rows (0001: a discard is a row, not silence). Rewritten for the whole KB:
/// materialization recomputes each round, so rows left by a previous round that could not be read but can now be read because the attribute became text are retired. The document is taken from the statement's
/// evidence; statements with no evidence (human-written) have no document to attach to, so nothing is recorded
async fn record_unreadable(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kb_id: Uuid,
    unreadable: &[(Uuid, Uuid, String)],
) -> AppResult<()> {
    let statements: Vec<Uuid> = unreadable.iter().map(|u| u.0).collect();
    let properties: Vec<Uuid> = unreadable.iter().map(|u| u.1).collect();
    let raws: Vec<String> = unreadable.iter().map(|u| u.2.clone()).collect();
    let rows: Vec<crate::extraction_drops::Unreadable> =
        sqlx::query_as::<_, (Uuid, String, String)>(
            "SELECT fe.document_id, rt.key, se.canonical_name || ' → ' || x.raw
           FROM unnest($2::uuid[], $3::uuid[], $4::text[]) AS x(statement, property, raw)
           JOIN relation_types rt ON rt.id = x.property
           JOIN facts s ON s.id = x.statement
           JOIN entities se ON se.id = s.subject_id
           JOIN LATERAL (SELECT fe.document_id FROM fact_evidence fe
                          WHERE fe.fact_id = x.statement AND fe.document_id IS NOT NULL
                          ORDER BY fe.quote_start NULLS LAST LIMIT 1) fe ON true
          WHERE rt.kb_id = $1
          ORDER BY x.statement",
        )
        .persistent(false)
        .bind(kb_id)
        .bind(&statements)
        .bind(&properties)
        .bind(&raws)
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(
            |(document_id, detail, example)| crate::extraction_drops::Unreadable {
                document_id,
                detail,
                example,
            },
        )
        .collect();
    crate::extraction_drops::replace_reason_tx(
        tx,
        kb_id,
        crate::extraction_drops::reason::ATTR_DATATYPE,
        &rows,
    )
    .await
}

/// The shape of an open statement's value (`{"value": "about 230m"}`) when it lands on an attribute: the `value` cell is read by datatype and unit into
/// a number, a date, or a boolean ([`utopia_extract::attribute_value`]); other keys are copied as-is; a value bound to a relation is kept as-is.
///
/// **If it cannot be read, nothing is written** (None). Previously the original characters were copied: a "registered capital" declared as a number in units of ten-thousand yuan held
/// `"about 230m"`, exported as `"about 230m"^^xsd:decimal`, which matched nothing for comparison, reconciliation, or rules.
/// The statement still sits in the open graph, and none of the original characters are lost. It is read only here, not at the `insert_fact_on` gate: humans and corrections
/// write relative dates on date attributes (`relative`), which is a different kind of value
fn attribute_value(raw: &Value, datatype: Option<&str>, unit: Option<&str>) -> Option<Value> {
    let Some(datatype) = datatype else {
        return Some(raw.clone());
    };
    let read = utopia_extract::attribute_value(datatype, unit, raw.get("value").unwrap_or(raw))?;
    let mut out = if raw.is_object() {
        raw.clone()
    } else {
        serde_json::json!({})
    };
    out["value"] = read;
    Some(out)
}

/// Statement `s` has no speaking `mood` (0070 decision 2): a re-push dropped the mood, so a mood on an old chunk no longer blocks
fn no_mood_spoken() -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM statement_qualifiers q WHERE q.fact_id = s.id AND q.role = 'mood' AND {})",
        crate::record_axis::role_word_speaks_now("q")
    )
}

/// Normalization of category words (same as type_bindings): here it must match the entity's specific_type in SQL
fn kind_word_sql(col: &str) -> String {
    format!("lower(btrim(regexp_replace({col}, '\\s+', ' ', 'g')))")
}

/// The number of live typed rows derived from statements in the KB.
pub async fn count(pool: &PgPool, kb_id: Uuid) -> AppResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM facts
          WHERE kb_id = $1 AND layer = 'typed' AND from_statement_id IS NOT NULL
            AND invalidated_at IS NULL",
    )
    .bind(kb_id)
    .fetch_one(pool)
    .await?)
}

/// The statement for step 3: (statement, binding) pairs that should exist but do not.
/// Starts from the bound bindings, first takes only open statements whose wording matches (both CTEs compute it separately), then joins the entities.
/// Normalized wording joins that the planner cannot estimate the row count for, so for each binding it rereads all entities:
/// in a test, 300k entities read 9 million rows; in production 85–93 seconds. The condition and the result rows are unchanged.
fn due_sql() -> String {
    format!(
        "WITH bound AS MATERIALIZED (
             SELECT b.* FROM phrase_bindings b WHERE b.kb_id = $1 AND b.status = 'bound'),
         stmts AS MATERIALIZED (
             SELECT s.*, {NORM} AS norm
               FROM facts s
              WHERE s.kb_id = $1 AND s.layer = 'open' AND s.invalidated_at IS NULL
                AND {NORM} IN (SELECT phrase FROM bound))
         SELECT s.id AS statement, b.relation_type_id AS property, b.direction,
                s.subject_id, s.object_id, oe.canonical_name AS object_name, s.object_value,
                s.valid_from, s.valid_from_precision, s.valid_from_grade,
                s.valid_to, s.valid_to_precision,
                s.attested_from, s.confidence, b.marks,
                rt.temporal, rt.kind, {SHAPE}
           FROM stmts s
           JOIN bound b ON b.phrase = s.norm
           JOIN entities se ON se.id = s.subject_id
      LEFT JOIN entities oe ON oe.id = s.object_id
           JOIN relation_types rt ON rt.id = b.relation_type_id
          WHERE b.subject_type_id IS NOT DISTINCT FROM se.type_id
            AND b.object_is_value = (s.object_id IS NULL)
            AND (s.object_id IS NULL OR b.object_type_id IS NOT DISTINCT FROM oe.type_id)
            AND (b.direction = 'forward' OR s.object_id IS NOT NULL)
            AND {NO_MOOD}
            -- 勘误撤过的（陈述, 属性）不再算（0044 决定 7）：撤销要站得住
            AND NOT EXISTS (SELECT 1 FROM errata_actions ea
                             WHERE ea.statement_id = s.id AND ea.predicate_id = b.relation_type_id
                               AND ea.status = 'applied' AND ea.action IN ('retract', 'revise'))
            AND NOT EXISTS (SELECT 1 FROM typed_fact_sources src JOIN facts t ON t.id = src.fact_id
                             WHERE src.statement_id = s.id AND t.invalidated_at IS NULL
                               AND t.predicate_id = b.relation_type_id)
          ORDER BY s.id",
        NORM = kind_word_sql("s.phrase"),
        NO_MOOD = no_mood_spoken(),
    )
}

/// The statement for step 1: sources that still hold are computed once (a CTE), then the rest are deleted by an anti-join.
/// When written as a correlated NOT EXISTS, the planner estimated the binding match as one row and rescanned for each source:
/// on a KB with 690k sources it ran for twelve hours; written this way it takes two minutes. The condition and the deleted rows are unchanged.
fn source_cleanup_sql() -> String {
    format!(
        "WITH holding AS MATERIALIZED (
             SELECT src.fact_id, src.statement_id
               FROM typed_fact_sources src
               JOIN facts t ON t.id = src.fact_id AND t.kb_id = $1 AND t.invalidated_at IS NULL
               JOIN facts s ON s.id = src.statement_id AND s.kb_id = $1
                           AND s.invalidated_at IS NULL
               JOIN entities se ON se.id = s.subject_id
          LEFT JOIN entities oe ON oe.id = s.object_id
               JOIN phrase_bindings b ON {MATCH}
              WHERE b.status = 'bound'
                AND b.relation_type_id = t.predicate_id
                AND ((b.direction = 'forward' AND t.subject_id = s.subject_id)
                  OR (b.direction = 'reverse' AND t.subject_id = s.object_id))
                AND {NO_MOOD})
         DELETE FROM typed_fact_sources src
          USING facts t
          WHERE src.fact_id = t.id AND t.kb_id = $1
            AND NOT EXISTS (SELECT 1 FROM holding h
                             WHERE h.fact_id = src.fact_id
                               AND h.statement_id = src.statement_id)",
        NO_MOOD = no_mood_spoken(),
    )
}

#[cfg(test)]
#[path = "materialize_delivery_tests.rs"]
mod delivery_tests;

#[cfg(test)]
#[path = "materialize_plan_tests.rs"]
mod plan_tests;

#[cfg(test)]
#[path = "materialize_unreadable_tests.rs"]
mod unreadable_tests;

#[cfg(test)]
#[path = "materialize_period_tests.rs"]
mod period_tests;
