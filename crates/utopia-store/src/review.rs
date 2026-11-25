//! The **real count** of the review queue.
//!
//! Previously the left-column badge read the length of the array the API returned, but the API always returned at most 100 items, so a
//! database with 164 low-confidence facts showed 100. Once those 100 were cleared, the remaining 64 would surface again
//! and look as if they had appeared out of nowhere.
//!
//! **Counting and fetching are two different things, and must be done separately.** Fetching has a cap (ten per page, paging for the next),
//! counting does not: `count(*)` uses the same WHERE as the list, and the same index.
//!
//! The eight COUNTs are combined into one query rather than sent eight times: they all hit the same kb, and one round trip fills
//! the left column all at once, whereas sending them separately would make the left column jump in steps when switching knowledge bases.

use sqlx::PgPool;
use utopia_core::models::ReviewCounts;
use utopia_core::AppResult;
use uuid::Uuid;

/// The low-confidence threshold. **Shares one constant with `review_routes`**: writing one number in each place
/// would eventually fork into "the badge says 12, click through and find 9".
pub const LOW_CONFIDENCE_BELOW: f32 = 0.75;

/// A typed row is judged the same as its statement (RC-005), see [`unconfirmed_fact`].
pub const FOLLOWS_ITS_STATEMENT: &str = "EXISTS (SELECT 1 FROM typed_fact_sources ts
                   JOIN facts st ON st.id = ts.statement_id
                  WHERE ts.fact_id = f.id
                    AND st.invalidated_at IS NULL
                    AND st.valid_to IS NULL AND st.valid_to_precision IS NULL
                    AND EXISTS (SELECT 1 FROM fact_evidence se
                                  JOIN chunks sc ON sc.id = se.chunk_id
                                 WHERE se.fact_id = st.id
                                   AND sc.superseded_at IS NULL))";

/// A document a fact cites has a current chunk that no one has read yet: a new version was written, not yet extracted. Before it speaks,
/// what it might have dropped cannot be called "gone". Only wait for versions that will still be read: still ingesting, or queued and being extracted.
/// Versions where extraction is off, skipped or failed will never be read, so waiting on them would wait forever. The alias is fixed to `f`.
///
/// The document is found from the evidence's chunk (`fe.chunk_id`), not `fe.document_id`: the latter may be empty.
///
/// **Only the present.** `extracted_at` has no history: a forced re-extraction clears it and writes it as now, so whether a chunk
/// had been read at time T cannot be known after the fact. So replay (0019) does not ask this, and only checks whether the evidence chunk
/// had been superseded by then, which re-extraction cannot undo.
pub fn awaits_a_new_version() -> &'static str {
    "EXISTS (SELECT 1 FROM fact_evidence fe
               JOIN chunks cited ON cited.id = fe.chunk_id
               JOIN documents d ON d.id = cited.document_id
               JOIN chunks c ON c.document_id = d.id
              WHERE fe.fact_id = f.id AND c.superseded_at IS NULL AND c.extracted_at IS NULL
                AND (d.graph_status IN ('queued', 'extracting')
                     OR d.status IN ('pending', 'parsing', 'indexing', 'embedding')))"
}

/// The criterion for "awaiting confirmation", written as an SQL fragment, shared by `counts` and the overview (`review_summary`):
/// evidence exists, but every chunk it sits in was replaced by a new version, **and this fact still holds**. The alias is fixed to `f`.
///
/// The upper-bound clause is the precondition for the "close" exit: closing is void + rewrite, and the correction row copies the evidence along
/// (`temporal::close_superseded`); without it, the just-closed row still satisfies the criterion, so the queue could never be cleared,
/// and clicking again would just hit "interval already closed". The criterion says the same thing as 0022: "end date unknown" is not open.
///
/// Last point: a new version written but not yet read does not count as awaiting confirmation (see [`awaits_a_new_version`]).
///
/// One more: for typed rows (RC-005), the evidence was copied from the statement at the moment of materialization; if the statement is later said again on a new chunk,
/// the copy does not grow with it. So rows with a statement source are judged the same as their statement: if the source statement is current, open,
/// and its evidence sits on a live chunk, the row does not count as awaiting confirmation. Goes via `typed_fact_sources` and the `fact_evidence` primary key.
pub fn unconfirmed_fact() -> String {
    format!(
        "f.valid_to IS NULL AND f.valid_to_precision IS NULL
               AND EXISTS (SELECT 1 FROM fact_evidence fe WHERE fe.fact_id = f.id)
               AND NOT EXISTS (SELECT 1 FROM fact_evidence fe
                                 JOIN chunks c ON c.id = fe.chunk_id
                                WHERE fe.fact_id = f.id
                                  AND c.superseded_at IS NULL)
               AND NOT {awaits}
               AND NOT {follows}",
        awaits = awaits_a_new_version(),
        follows = FOLLOWS_ITS_STATEMENT,
    )
}

pub async fn counts(pool: &PgPool, kb_id: Uuid) -> AppResult<ReviewCounts> {
    let sql = format!(
        "SELECT
           (SELECT count(*) FROM pending_facts WHERE kb_id = $1) AS pending,
           (SELECT count(*) FROM resolution_reviews
             WHERE kb_id = $1 AND status = 'pending') AS duplicates,
           (SELECT count(*) FROM resolution_reviews rr
             JOIN entities a ON a.id = rr.left_id JOIN entities b ON b.id = rr.right_id
             WHERE rr.kb_id = $1 AND rr.status = 'pending' AND {same}) AS duplicates_same_type,
           (SELECT count(*) FROM resolution_reviews rr
             JOIN entities a ON a.id = rr.left_id JOIN entities b ON b.id = rr.right_id
             WHERE rr.kb_id = $1 AND rr.status = 'pending' AND {conflict}) AS duplicates_type_conflict,
           (SELECT count(*) FROM fact_conflicts
             WHERE kb_id = $1 AND status = 'open') AS conflicts,
           (SELECT count(*) FROM facts f
             WHERE f.kb_id = $1 AND f.invalidated_at IS NULL AND {unconfirmed}) AS unconfirmed,
           (SELECT count(*) FROM facts
             WHERE kb_id = $1 AND invalidated_at IS NULL
               AND confidence < $2 AND derived_by_rule IS NULL) AS lowconf,
           (SELECT count(*) FROM concept_mappings
             WHERE kb_id = $1 AND status = 'proposed') AS mappings,
           (SELECT count(*) FROM axiom_violations
             WHERE kb_id = $1 AND status = 'open') AS violations,
           (SELECT count(*) FROM ontology_defects
             WHERE kb_id = $1 AND status = 'open') AS defects,
           (SELECT count(*) FROM entity_merges WHERE kb_id = $1) AS merges,
           (SELECT count(*) FROM agent_decisions
             WHERE kb_id = $1 AND status = 'proposed') AS agent,
           (SELECT count(*) FROM agent_decisions WHERE kb_id = $1) AS agent_rows,
           EXISTS (SELECT 1 FROM jobs j WHERE j.kind = 'govern' AND j.status = 'running'
                     AND j.payload->>'kb_id' = $1::text) AS agent_running,
           (SELECT count(*) FROM resolution_reviews rr
             WHERE rr.kb_id = $1 AND rr.status = 'pending'
               AND NOT EXISTS (SELECT 1 FROM agent_decisions d
                                WHERE d.target_kind = 'review' AND d.target_id = rr.id
                                  AND d.status = 'proposed')) AS agent_queue,
           (SELECT count(*) FROM (SELECT 1 FROM phrase_bindings b WHERE b.kb_id = $1 AND {waiting_phrase}
                                  UNION ALL SELECT 1 FROM type_bindings WHERE kb_id = $1 AND status = 'undecided') a) AS alignment,
           (SELECT count(*) FROM errata_actions WHERE kb_id = $1 AND status = 'held') AS errata",
        unconfirmed = unconfirmed_fact(),
        waiting_phrase = crate::alignment_queue::WAITING_PHRASE,
        same = crate::resolution::TypeFilter::Same.clause(),
        conflict = crate::resolution::TypeFilter::Conflict.clause(),
    );
    Ok(sqlx::query_as(&sql)
        .bind(kb_id)
        .bind(LOW_CONFIDENCE_BELOW)
        .fetch_one(pool)
        .await?)
}
