//! World-axis predicate (0022): `holds_at(T)` — **which facts hold at time T**.
//!
//! The write side has long distinguished "still ongoing" from "ended, date unknown", and never invents a start point for the original text.
//! The read side, however, reads both back as "holds at any time": `valid_from IS NULL` is read as having always been so,
//! and `valid_to IS NULL` without checking the precision beside it is read as still true to this day. Ask about a moment before the evidence appeared,
//! or about a moment the source says has ended but gives no date for, and the graph will answer with confidence — citing exactly
//! the row that says it should not hold (#345, #352).
//!
//! **The predicate is assembled only here**, for the same reason as `record_axis`: a defense scattered at every read point, when one is missed,
//! fails silently. The frontend no longer computes it again itself — edges and facts carry `holds_from` / `holds_to`
//! (the read-out interval projected by the same expressions as here), and the slider only filters by them.
//!
//! An unknown end **reads up to the evidence**, and each end has its own anchor (#393): `attested_from` is the date of the earliest
//! document among this row's observations — no start point → holds from there; `attested_to` is the date of the document that **states the end**,
//! present only on rows that "ended, date unknown" — holds up to it. One anchor cannot cover both ends:
//! when a bare row with no start is closed by a "no longer serving" statement, the start must use the first evidence and the end must use the one that states the ending.
//! The asymmetry between the two ends is still deliberate: an open end reads as "until someone says it ended" — an ending arrives as a record
//! and closes the row; a missing start has no such corrector, and nobody will say "in 2023 it had not started yet".
//!
//! A NULL `at` on the world axis means **every moment** (the canvas draws history, the slider narrows it), unlike the record axis
//! where "NULL means now" applies: nobody holds a belief later than this moment, while a graph with no moment covers all
//! time.

/// Temporal semantics of the predicate (0031). A row cannot itself say whether it is a state, an event, or a constant — its predicate says so;
/// a row without a predicate (0010) reads as a state, the same judgment as the write side's `predicate_temporal`
fn temporal_of(alias: &str) -> String {
    format!("(SELECT r.temporal FROM relation_types r WHERE r.id = {alias}.predicate_id)")
}

/// `facts`: the read-out lower bound — use the start if the source gave one, otherwise from the earliest evidence.
/// Constants have no lower bound (NULL means open): the evidence-date gate is about "from when is it known", and a constant does not hold from some time.
///
/// Rows with neither a start nor a witness (0064 decision 5): the document did not say when it holds, nor which day it is itself from,
/// so the lower bound is open — it holds at any point. Event rows without a date are excepted, see [`facts_hold_at`]
pub fn facts_holds_from(alias: &str) -> String {
    format!(
        "CASE WHEN {temporal} = 'eternal' THEN NULL \
              ELSE COALESCE({alias}.valid_from, {alias}.attested_from) END",
        temporal = temporal_of(alias),
    )
}

/// `facts`: the read-out upper bound — use the end if the source gave one; if it says it ended but not when, up to the earliest document that states
/// it; otherwise open (NULL).
///
/// An event (0031) holds within the bucket it names: the upper bound is that moment plus one unit of precision — `2024-03-15`
/// runs through `2024-03-16`, `2024-03` through the end of April. An event without a date has both bounds anchored,
/// and the interval is empty: it happened, but when is unknown, so no moment counts as holding (how 0022 treats the unknown).
/// Event rows written before 0031 have an empty end, and are read by their start's bucket — no backfill needed. Constants have no upper bound
pub fn facts_holds_to(alias: &str) -> String {
    format!(
        "CASE WHEN {temporal} = 'eternal' THEN NULL \
              WHEN {temporal} = 'event' THEN \
                   CASE WHEN {alias}.valid_from IS NULL THEN {alias}.attested_from \
                        ELSE COALESCE({alias}.valid_to, {alias}.valid_from) \
                             + ('1 ' || COALESCE(NULLIF({alias}.valid_to_precision, 'unknown'), \
                                                 {alias}.valid_from_precision))::interval END \
              WHEN {alias}.valid_to IS NOT NULL THEN {alias}.valid_to \
              WHEN {alias}.valid_to_precision = 'unknown' THEN {alias}.attested_to END",
        temporal = temporal_of(alias),
    )
}

/// `facts`: asserts holding at time T. A `$param` that is NULL means no filter.
/// Both ends may be open (constants), so this is pure interval containment
///
/// An event without a date happened but when is unknown, so no moment counts as holding (0022). With a witness, its two ends are both the witness,
/// so the interval is empty; without a witness (0064 decision 5) both ends are NULL, which reads as an interval open at both ends, so it is blocked
/// separately — `infinity` is not used as an endpoint, since those two values would still have to be projected to the reader. Rows without a predicate (open statements)
/// have NULL temporal semantics and read as state: compare with `IS NOT DISTINCT FROM`, otherwise the whole predicate is NULL, and
/// placed in WHERE it filters out every open statement
pub fn facts_hold_at(alias: &str, param: usize) -> String {
    format!(
        "(${param}::timestamptz IS NULL \
          OR (NOT ({temporal} IS NOT DISTINCT FROM 'event' AND {alias}.valid_from IS NULL \
                   AND {alias}.attested_from IS NULL) \
              AND {holds}))",
        temporal = temporal_of(alias),
        holds = interval_holds_at(&facts_holds_from(alias), &facts_holds_to(alias), param),
    )
}

/// Pure interval containment, a NULL end being open. Derived rows and ghost edges (0017 §3, with the interval in `detail`)
/// both use it — their ends are not what the source states; the engine computes them from the premises.
pub fn interval_holds_at(from: &str, to: &str, param: usize) -> String {
    format!(
        "(${param}::timestamptz IS NULL \
          OR (({from} IS NULL OR {from} <= ${param}) AND ({to} IS NULL OR {to} > ${param})))"
    )
}

/// `derived_facts`: derived holding at time T. Both ends are written by the evaluator intersecting the intervals **read out** of the premises
/// (0022 rule 4: a premise with no start is counted from its anchor, and one that ended but not when runs to its anchor), so here
/// it is pure containment — derived rows do not need an anchor themselves.
pub fn derived_hold_at(alias: &str, param: usize) -> String {
    interval_holds_at(
        &format!("{alias}.valid_from"),
        &format!("{alias}.valid_to"),
        param,
    )
}
