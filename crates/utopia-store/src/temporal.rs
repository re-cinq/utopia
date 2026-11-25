//! Temporal engine (S3): contradiction detection for functional state relations, and automatic closing.
//!
//! Principles:
//! - Pure rule-based judgment, zero LLM — ambiguity has already been absorbed upstream (entity resolution merges, the ontology marks functional)
//! - Closing goes through "void + rewrite" rather than editing in place: the old assertion's invalidated_at records "when it was corrected",
//!   and the correction closes the interval and links back to the old row through a supersedes chain — so "replaying the past with what was known then" holds
//! - The closing point uses world time only: the successor's valid_from; when the successor has no start, the predecessor is written as "ended, date unknown",
//!   anchored on the successor's fact attestation (the 0022 shape, the 0064 section date). The attestation date is never written into the date column
//! - When unsure (missing time / simultaneous start / low confidence), never close forcibly; go into fact_conflicts for a person to decide
//!
//! **A timeline is a single whole.** All existing rows of the same (store, holder, predicate, uniqueness direction) are arranged in time order
//! into one column. The end of a row is either stated in the source or by a person and is never recomputed; or it is derived by the engine (`facts.end_derived`,
//! 0057): open rows and rows the engine closed both end at the nearest later start of another value. Facts stored or observed again
//! A merge that moves rows in or retracts them, or a document deletion or restore, recomputes this column **once** to the shape the rows now present imply — the result
//! depends only on which rows exist, not on the order they arrived (#679).
//!
//! When a relation is unique on both sides (functional and inverse functional, "a person leads only one project at a time, and a
//! project has only one leader at a time"), a row sits on both timelines at once: its end is pushed by each side, and the earlier one wins.
//!
//! The recompute runs in one transaction, holding the advisory lock of this timeline. A transaction that moves several timelines (retraction of a merge, document
//! deletion) first takes all these locks in a fixed order before changing any row: the store reconciliation also takes locks first and then row locks, in the same order,
//! so no one holds a row lock while waiting for an advisory lock. For a relation unique on both sides, one lock per predicate — the recompute of one side has to read the other.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use utopia_core::models::ConflictView;
use utopia_core::AppResult;
use uuid::Uuid;

/// The fact's own attestation, from the same source as the world-axis lower bound (0064). Different sections of the same document version can have different dates;
/// NULL explicitly means there is no time, and must not fall back to the document date. Typed rows get this attestation synced from the source statement.
const DATED_AT: &str = "f.attested_from";

/// Whether this row's source includes text described from a picture, projected through the `facts` alias `f` (0040 decision 4).
/// A typed row has no source of its own; its source is that of the statements that materialized it (0044 slice 2)
const DESCRIBED: &str = "(EXISTS (SELECT 1 FROM fact_evidence fe
                                    JOIN chunks ch ON ch.id = fe.chunk_id
                                   WHERE fe.fact_id = f.id AND ch.origin = 'described')
                          OR EXISTS (SELECT 1 FROM typed_fact_sources ts
                                       JOIN fact_evidence fe ON fe.fact_id = ts.statement_id
                                       JOIN chunks ch ON ch.id = fe.chunk_id
                                      WHERE ts.fact_id = f.id AND ch.origin = 'described'))";

/// Replace [`DESCRIBED`] with another alias: when reading, find the successor of a row that was closed (#970), the same rows the engine excludes
pub(crate) fn described_sql(alias: &str) -> String {
    DESCRIBED.replace("= f.id", &format!("= {alias}.id"))
}

/// Uniqueness direction: functional = subject side (Zhang San reports_to only one person at a time);
/// inverse functional = object side (a project has only one person leading it at a time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Uniqueness {
    SubjectSide,
    ObjectSide,
}

impl Uniqueness {
    fn other(self) -> Self {
        match self {
            Self::SubjectSide => Self::ObjectSide,
            Self::ObjectSide => Self::SubjectSide,
        }
    }
}

/// One uniqueness timeline: who (the holder), on which predicate and which direction, can have only one value at a time
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timeline {
    pub holder: Uuid,
    pub predicate_id: Uuid,
    pub side: Uniqueness,
    /// Predicate unique on both sides: every row on this line is also on a line on the other side
    pub both_sides: bool,
}

impl Timeline {
    fn lock_key(&self, kb_id: Uuid) -> String {
        if self.both_sides {
            format!("timeline:{kb_id}:{}:both", self.predicate_id)
        } else {
            format!(
                "timeline:{kb_id}:{}:{}:{:?}",
                self.holder, self.predicate_id, self.side
            )
        }
    }
}

/// Reconciliation result: the ids of the correction rows produced by automatic closing (the caller records them as needed), and the number of conflicts newly sent for human review this time.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    pub corrected: Vec<Uuid>,
    pub conflicts: u32,
}

/// A row on the timeline
#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: Uuid,
    subject_id: Uuid,
    object_id: Option<Uuid>,
    object_value: Option<serde_json::Value>,
    valid_from: Option<DateTime<Utc>>,
    valid_from_precision: Option<String>,
    valid_to: Option<DateTime<Utc>>,
    valid_to_precision: Option<String>,
    attested_to: Option<DateTime<Utc>>,
    end_derived: bool,
    dated_at: Option<DateTime<Utc>>,
    /// How the start came about: `A` stated explicitly, `B` computed by the document's own anchor, `C` has a time expression but the anchor cannot be placed;
    /// `None` = did not go through time resolution (written by a person, or computed by rule), treated as A
    valid_from_grade: Option<String>,
    /// Whether this row's source includes text described from a picture (0040 decision 4)
    described: bool,
}

/// The end of a row
#[derive(Debug, Clone, PartialEq)]
enum End {
    /// Still continuing
    Open,
    /// Ended at this moment, with precision
    At(DateTime<Utc>, String),
    /// Ended, date unknown; the reading side reads up to the anchor
    Unknown(Option<DateTime<Utc>>),
}

impl End {
    /// The moment it reads up to; an open row has none
    fn instant(&self) -> Option<DateTime<Utc>> {
        match self {
            End::Open => None,
            End::At(at, _) => Some(*at),
            End::Unknown(anchor) => *anchor,
        }
    }

    /// The earlier of the two ends; at the same moment, the one with a written date beats the anchor
    fn earlier(self, other: End) -> End {
        match (self.instant(), other.instant()) {
            (None, _) => other,
            (_, None) => self,
            (Some(a), Some(b)) if b < a => other,
            (Some(a), Some(b)) if a == b && matches!(other, End::At(..)) => other,
            _ => self,
        }
    }
}

impl Row {
    /// Whether this row's start cannot be anchored: it has a time expression, but the code could not place it on the world axis (level C). **A row with no level
    /// is not counted** — its date was given directly (written by a person, computed by rule, every row before this column), not
    /// guessed from elsewhere. A row that cannot be anchored still enters the timeline (it is ordered by its attestation date), it just is not allowed to rewrite history
    fn start_unanchored(&self) -> bool {
        self.valid_from_grade.as_deref() == Some("C")
    }

    /// The moment used for ordering: the start. With no start, the fact's attestation — but this holds only for rows that are still open, or closed by the engine:
    /// their evidence says it still held that day. A row the source says has already ended — its evidence date only says "it ended before that day",
    /// and sorting by it would let a value that ended long ago close the current value (#679 third review round)
    fn key(&self) -> Option<DateTime<Utc>> {
        match self.valid_from {
            Some(start) => Some(start),
            None if self.is_open() || self.end_derived => self.dated_at,
            None => None,
        }
    }
    fn is_open(&self) -> bool {
        self.valid_to.is_none() && self.valid_to_precision.is_none()
    }
    /// The end is set by the engine from the timeline: open, or closed by the engine
    fn recomputable(&self) -> bool {
        self.is_open() || self.end_derived
    }
    /// The moment of the end: a date; the anchor of "ended, date unknown" (the reading side likewise reads up to it)
    fn end(&self) -> Option<DateTime<Utc>> {
        match (self.valid_to, self.valid_to_precision.as_deref()) {
            (Some(to), _) => Some(to),
            (None, Some(crate::graph::ENDED_UNKNOWN)) => self.attested_to,
            _ => None,
        }
    }
    /// The end currently written
    fn current_end(&self) -> End {
        match (self.valid_to, self.valid_to_precision.as_deref()) {
            (Some(to), p) => End::At(to, p.unwrap_or("day").to_string()),
            (None, Some(crate::graph::ENDED_UNKNOWN)) => End::Unknown(self.attested_to),
            _ => End::Open,
        }
    }
    /// What the end of the preceding segment is when it stops at the start of this row: with a start it is that moment; with no start it is "ended,
    /// date unknown", anchored on this row's attestation
    fn end_before(&self) -> End {
        match self.valid_from {
            Some(start) => {
                let precision = self.valid_from_precision.as_deref().unwrap_or("day");
                End::At(
                    crate::graph::truncate_to(start, Some(precision)),
                    precision.to_string(),
                )
            }
            None => End::Unknown(self.dated_at),
        }
    }
    /// Still holds at the moment `at`
    fn holds_at(&self, at: DateTime<Utc>) -> bool {
        self.is_open() || self.end().is_some_and(|end| end > at)
    }
    /// The holder of this row on the `side`
    fn holder(&self, side: Uniqueness) -> Option<Uuid> {
        match side {
            Uniqueness::SubjectSide => Some(self.subject_id),
            Uniqueness::ObjectSide => self.object_id,
        }
    }
}

/// Whether two rows state the same value (same side: subject side compares objects, object side compares subjects)
fn same_value(side: Uniqueness, a: &Row, b: &Row) -> bool {
    match side {
        Uniqueness::SubjectSide => a.object_id == b.object_id && a.object_value == b.object_value,
        Uniqueness::ObjectSide => a.subject_id == b.subject_id,
    }
}

async fn lock_timeline(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    timeline: Timeline,
) -> AppResult<()> {
    lock_timelines(tx, kb_id, &[timeline]).await
}

/// If a lock covers more timelines than this, take the predicate-level exclusive lock instead (see [`lock_timelines`])
const BULK_TIMELINES: usize = 256;

/// Takes the advisory locks of several timelines, **in a fixed order**. Every transaction that moves several timelines goes through here:
/// everyone queues in the same order, so nobody holds one and waits for another. Taking the same lock twice in one transaction is harmless.
///
/// **Locks come in two levels.** Normally take the predicate-level shared lock first, then the timeline-level exclusive lock — timelines of different holders
/// on the same predicate are unrelated, so they still run in parallel. When too many timelines must be locked at once (deleting one document that gave
/// thousands of entities one attribute each), take the exclusive locks of those predicates instead of locking one by one: locking 8000 timelines one by one would take
/// 8000 locks, and at twenty thousand Postgres's lock table (`max_locks_per_transaction`) simply cannot hold them, and the
/// document could not be deleted at all (#679 fourth review round). Predicate locks come first, timeline locks after, each sorted on its own,
/// so the two levels never wait on each other in reverse
pub async fn lock_timelines(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    timelines: &[Timeline],
) -> AppResult<()> {
    if timelines.is_empty() {
        return Ok(());
    }
    let mut predicates: Vec<String> = timelines
        .iter()
        .map(|t| format!("predicate:{kb_id}:{}", t.predicate_id))
        .collect();
    predicates.sort();
    predicates.dedup();
    let bulk = timelines.len() > BULK_TIMELINES;
    let predicate_lock = if bulk {
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))"
    } else {
        "SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 0))"
    };
    for key in predicates {
        sqlx::query(predicate_lock)
            .bind(key)
            .execute(&mut **tx)
            .await?;
    }
    if bulk {
        return Ok(());
    }
    let mut keys: Vec<String> = timelines.iter().map(|t| t.lock_key(kb_id)).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(key)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn load_timeline(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    timeline: Timeline,
) -> AppResult<Vec<Row>> {
    let holder_column = match timeline.side {
        Uniqueness::SubjectSide => "f.subject_id",
        Uniqueness::ObjectSide => "f.object_id",
    };
    let rows = sqlx::query_as(&format!(
        "SELECT f.id, f.subject_id, f.object_id, f.object_value,
                f.valid_from, f.valid_from_precision, f.valid_to, f.valid_to_precision,
                f.attested_to, f.end_derived, {DATED_AT} AS dated_at,
                f.valid_from_grade, {DESCRIBED} AS described
         FROM facts f
         WHERE f.kb_id = $1 AND {holder_column} = $2 AND f.predicate_id = $3
           AND f.invalidated_at IS NULL
         ORDER BY f.recorded_at
         FOR UPDATE OF f"
    ))
    .bind(kb_id)
    .bind(timeline.holder)
    .bind(timeline.predicate_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows)
}

/// All the uniqueness timelines these facts sit on (counting only temporal = state, predicates declaring uniqueness).
///
/// `swap = (from, to)`: facts whose subject or object is `from`, counted together with the line they land on once swapped to `to` —
/// a retraction of a merge must take the locks on both sides, before and after the move, before moving any row
pub async fn timelines_of<'e, E>(
    executor: E,
    kb_id: Uuid,
    fact_ids: &[Uuid],
    // For the timelines on these holders, also record a copy swapped onto the later entity (both sides must be locked for a merge and for its retraction)
    swap: Option<(&[Uuid], Uuid)>,
) -> AppResult<Vec<Timeline>>
where
    E: sqlx::PgExecutor<'e>,
{
    #[derive(sqlx::FromRow)]
    struct Placed {
        subject_id: Uuid,
        object_id: Option<Uuid>,
        predicate_id: Uuid,
        functional: bool,
        inverse_functional: bool,
    }
    if fact_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placed: Vec<Placed> = sqlx::query_as(
        "SELECT DISTINCT f.subject_id, f.object_id, f.predicate_id, r.functional, r.inverse_functional
         FROM facts f JOIN relation_types r ON r.id = f.predicate_id
         WHERE f.kb_id = $1 AND f.id = ANY($2)
           AND r.temporal = 'state' AND (r.functional OR r.inverse_functional)",
    )
    .bind(kb_id)
    .bind(fact_ids)
    .fetch_all(executor)
    .await?;
    let swapped = |id: Uuid| {
        swap.filter(|(from, _)| from.contains(&id))
            .map(|(_, to)| to)
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for p in placed {
        let both_sides = p.functional && p.inverse_functional;
        let mut holders = Vec::new();
        if p.functional {
            holders.push((p.subject_id, Uniqueness::SubjectSide));
            holders.extend(swapped(p.subject_id).map(|to| (to, Uniqueness::SubjectSide)));
        }
        // Object-side uniqueness only makes sense for entity objects (a literal value is not "occupied")
        if let (true, Some(object)) = (p.inverse_functional, p.object_id) {
            holders.push((object, Uniqueness::ObjectSide));
            holders.extend(swapped(object).map(|to| (to, Uniqueness::ObjectSide)));
        }
        for (holder, side) in holders {
            let timeline = Timeline {
                holder,
                predicate_id: p.predicate_id,
                side,
                both_sides,
            };
            if seen.insert(timeline) {
                out.push(timeline);
            }
        }
    }
    Ok(out)
}

/// Reconciliation along the given uniqueness direction after a state fact is **observed**. Call it for newly stored facts, and
/// also for facts observed again (another copy of evidence for the same assertion, whose date may be earlier, so the shape of the timeline changes); call it after
/// writing the evidence. The caller is responsible for judging that the relation really has uniqueness in that direction and temporal = state.
///
/// The object can be an entity (object_id) or a literal value (object_value, an attribute fact) —
/// the test for "different object" is a comparison of the (object_id, object_value) pair: a salary changing from 30k to 35k
/// takes the same closing path as "switching from Zhang San to Li Si".
///
/// The time and confidence of this fact are read from the store; the parameters are used only to decide which timeline to act on
#[allow(clippy::too_many_arguments)]
pub async fn reconcile_new_fact(
    pool: &PgPool,
    kb_id: Uuid,
    new_fact_id: Uuid,
    subject_id: Uuid,
    predicate_id: Uuid,
    object_id: Option<Uuid>,
    _object_value: Option<&serde_json::Value>,
    direction: Uniqueness,
    _new_validity: crate::graph::Validity<'_>,
    _new_confidence: f32,
) -> AppResult<ReconcileReport> {
    let holder = match direction {
        Uniqueness::SubjectSide => subject_id,
        Uniqueness::ObjectSide => match object_id {
            Some(o) => o,
            None => return Ok(ReconcileReport::default()),
        },
    };
    let both_sides: bool = sqlx::query_scalar(
        "SELECT functional AND inverse_functional FROM relation_types WHERE id = $1",
    )
    .bind(predicate_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or(false);
    let timeline = Timeline {
        holder,
        predicate_id,
        side: direction,
        both_sides,
    };
    let mut report = ReconcileReport::default();
    let mut tx = pool.begin().await?;
    lock_timeline(&mut tx, kb_id, timeline).await?;
    arrive(&mut tx, kb_id, timeline, new_fact_id, &[], &mut report).await?;
    tidy(&mut tx, kb_id, timeline, &mut report).await?;
    tx.commit().await?;
    Ok(report)
}

/// A fact arrives on its timeline; record what the recompute cannot judge and hand it to a person (the caller already holds the lock):
///
/// - starts at the same moment as another value and both still hold at that moment: it is unclear who succeeds whom
/// - both still open, and one side cannot state a time (no start and no attestation): there is no basis to say which came first.
///   Whichever side arrives first, it is the same conflicting pair; neither side is closed on anyone's behalf
///
/// `settled`: facts that already arrived in the same batch are not paired with them again
async fn arrive(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    timeline: Timeline,
    fact_id: Uuid,
    settled: &[Uuid],
    report: &mut ReconcileReport,
) -> AppResult<()> {
    let rows = load_timeline(tx, kb_id, timeline).await?;
    let Some(new) = rows.iter().find(|r| r.id == fact_id) else {
        return Ok(());
    };
    let others = rows
        .iter()
        .filter(|r| r.id != new.id && !same_value(timeline.side, r, new))
        .filter(|r| !settled.contains(&r.id));
    for other in others {
        let reason = match (new.key(), other.key()) {
            (Some(at), Some(theirs)) if at == theirs && new.holds_at(at) && other.holds_at(at) => {
                "simultaneous"
            }
            (None, _) | (_, None) if new.is_open() && other.is_open() => "no_time",
            _ => continue,
        };
        if record_conflict_tx(tx, kb_id, other.id, new.id, reason).await? {
            report.conflicts += 1;
        }
    }
    Ok(())
}

/// What must change in a recompute of one timeline (the shape the unit tests check)
#[cfg(test)]
#[derive(Debug, Default, PartialEq)]
struct Plan {
    /// (row, the end it should have)
    ends: Vec<(Uuid, End)>,
    /// (row, the nearest successor that a figure description did not allow to close it): handed to a person
    held: Vec<(Uuid, Uuid)>,
}

/// The end each row on a timeline should have (a pure function, does not touch the store): contains only the rows whose end is set by the engine.
///
/// Rows that can be placed on the timeline (with a start, or with an attestation) are ordered by moment; rows whose end is written are left alone. The rest:
/// each row — open, or closed by the engine — ends at the start of the nearest later row with a different value; if there is no such row after it,
/// it stays open. When the successor's start cannot be anchored (level C), skip it: its place on the axis is given by the attestation, not by
/// what the sentence says (0045 slice 3). A successor that is a figure description is also skipped; that pair is handed to a person (0040 decision 4).
///
/// Each row's end depends only on the moments, values and the origin of the starts of the rows; rewriting an end does not change these three, so one computation
/// gives the final state and does not need to run round by round; the order in which rows came in does not affect the result
fn desired_ends(side: Uniqueness, rows: &[Row]) -> (HashMap<Uuid, End>, Vec<(Uuid, Uuid)>) {
    let mut keyed: Vec<&Row> = rows.iter().filter(|r| r.key().is_some()).collect();
    // Rows starting at the same moment: those with a written start go first, so a successor takes it and the predecessor ends at a date rather than an anchor
    keyed.sort_by_key(|r| (r.key(), r.valid_from.is_none(), r.id));
    // Untimed rows cannot keep an end that the old engine inferred from the document date alone; an end written explicitly by a person is still not recomputed.
    let mut ends: HashMap<Uuid, End> = rows
        .iter()
        .filter(|row| row.recomputable() && row.key().is_none())
        .map(|row| (row.id, End::Open))
        .collect();
    let mut held = Vec::new();
    for (i, row) in keyed.iter().enumerate() {
        if !row.recomputable() {
            continue;
        }
        let mut end = End::Open;
        let mut doubtful = None;
        for later in keyed[i + 1..]
            .iter()
            .filter(|later| later.key() > row.key() && !same_value(side, row, later))
        {
            // Start cannot be anchored: do not close, and do not record a contradiction either — there is nothing to judge for this pair; the pending mention goes to the time
            // anchor queue (0045: level C neither closes timelines nor opens contradictions)
            if later.start_unanchored() {
                continue;
            }
            // A fact described from a figure can enter the graph and be cited, but may not close a correct old value on its own: a bar chart
            // misreading a number is common, and no row in the store would say this closing relied on a picture (0040 decision 4)
            if later.described {
                doubtful.get_or_insert(later.id);
                continue;
            }
            end = later.end_before();
            break;
        }
        if let Some(later) = doubtful {
            held.push((row.id, later));
        }
        ends.insert(row.id, end);
    }
    (ends, held)
}

/// Recompute plan for one side's timeline: the rows whose desired end differs from the one now written
#[cfg(test)]
fn plan_timeline(side: Uniqueness, rows: &[Row]) -> Plan {
    let (ends, held) = desired_ends(side, rows);
    Plan {
        ends: diff(rows, ends),
        held,
    }
}

/// Among the desired ends, those that differ from the current ones, in the order of the rows on the timeline
fn diff(rows: &[Row], mut ends: HashMap<Uuid, End>) -> Vec<(Uuid, End)> {
    rows.iter()
        .filter_map(|r| {
            let end = ends.remove(&r.id)?;
            (r.current_end() != end).then_some((r.id, end))
        })
        .collect()
}

/// Recomputes a timeline to what [`desired_ends`] says (the caller already holds the lock). For a relation unique on both sides,
/// each row is pushed once more along its other side's timeline, and the earlier end wins
async fn tidy(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    timeline: Timeline,
    report: &mut ReconcileReport,
) -> AppResult<()> {
    let rows = load_timeline(tx, kb_id, timeline).await?;
    let (mut ends, mut held) = desired_ends(timeline.side, &rows);
    if timeline.both_sides {
        let other = timeline.side.other();
        let holders: BTreeSet<Uuid> = rows.iter().filter_map(|r| r.holder(other)).collect();
        for holder in holders {
            let across = Timeline {
                holder,
                side: other,
                ..timeline
            };
            let across_rows = load_timeline(tx, kb_id, across).await?;
            let (across_ends, across_held) = desired_ends(other, &across_rows);
            for (id, end) in across_ends {
                if let Some(mine) = ends.remove(&id) {
                    ends.insert(id, mine.earlier(end));
                }
            }
            held.extend(
                across_held
                    .into_iter()
                    .filter(|(row, _)| ends.contains_key(row)),
            );
        }
    }
    let mut rewritten: HashMap<Uuid, Uuid> = HashMap::new();
    for (id, end) in diff(&rows, ends) {
        // A row that was reopened is simply open; its end is not derived from anyone
        let derived = end != End::Open;
        if let Some(corrected) = rewrite_end_tx(tx, id, &end, derived, false, false).await? {
            rewritten.insert(id, corrected);
            report.corrected.push(corrected);
        }
    }
    for (row, later) in held {
        let row = rewritten.get(&row).copied().unwrap_or(row);
        let later = rewritten.get(&later).copied().unwrap_or(later);
        if record_conflict_tx(tx, kb_id, row, later, "described_evidence").await? {
            report.conflicts += 1;
        }
    }
    Ok(())
}

/// After two entities merge into one, which uniqueness relations **would still** have two values holding at the same time (the execution gate, 0027).
///
/// Each side holding a different value is not a contradiction by itself: a lease whose landlord was first HPBB1 and later became BBHQ1, with each name holding one,
/// together form a timeline of successive succession. Here the rows of both sides are treated as one timeline; the end of each row is derived the way the engine would
/// order them ([`desired_ends`]), then checked for whether different values overlap at some moment — the ones that cannot be ordered
/// (starting at the same moment, no time stated, confidence too low to succeed, two intervals written in the sources that intersect) are the contradictions the consistency
/// check would report after the merge. Only entity-object edges are considered, on the same basis as the consistency check (`utopia_reason::check`).
/// Returns the predicate labels, sorted by label
pub async fn merge_would_overlap(
    pool: &PgPool,
    kb_id: Uuid,
    a: Uuid,
    b: Uuid,
) -> AppResult<Vec<String>> {
    #[derive(sqlx::FromRow)]
    struct Shared {
        predicate_id: Uuid,
        label: String,
        functional: bool,
        inverse_functional: bool,
        temporal: String,
    }
    let shared: Vec<Shared> = sqlx::query_as(
        "SELECT DISTINCT r.id AS predicate_id, r.label, r.functional, r.inverse_functional,
                r.temporal
         FROM facts x
         JOIN facts y ON y.predicate_id = x.predicate_id AND y.kb_id = x.kb_id
         JOIN relation_types r ON r.id = x.predicate_id
         WHERE x.kb_id = $1
           AND x.invalidated_at IS NULL AND y.invalidated_at IS NULL
           AND x.object_id IS NOT NULL AND y.object_id IS NOT NULL
           AND ((r.functional AND x.subject_id = $2 AND y.subject_id = $3
                 AND x.object_id <> y.object_id)
             OR (r.inverse_functional AND x.object_id = $2 AND y.object_id = $3
                 AND x.subject_id <> y.subject_id))",
    )
    .bind(kb_id)
    .bind(a)
    .bind(b)
    .fetch_all(pool)
    .await?;

    let mut labels = Vec::new();
    for s in shared {
        let mut sides = Vec::new();
        if s.functional {
            sides.push(Uniqueness::SubjectSide);
        }
        if s.inverse_functional {
            sides.push(Uniqueness::ObjectSide);
        }
        for side in sides {
            let holder_column = match side {
                Uniqueness::SubjectSide => "f.subject_id",
                Uniqueness::ObjectSide => "f.object_id",
            };
            let mut rows: Vec<Row> = sqlx::query_as(&format!(
                "SELECT f.id, f.subject_id, f.object_id, f.object_value,
                        f.valid_from, f.valid_from_precision, f.valid_to, f.valid_to_precision,
                        f.attested_to, f.end_derived, {DATED_AT} AS dated_at,
                        f.valid_from_grade, {DESCRIBED} AS described
                 FROM facts f
                 WHERE f.kb_id = $1 AND {holder_column} = ANY($2) AND f.predicate_id = $3
                   AND f.invalidated_at IS NULL AND f.object_id IS NOT NULL"
            ))
            .bind(kb_id)
            .bind(vec![a, b])
            .bind(s.predicate_id)
            .fetch_all(pool)
            .await?;
            // **Only the conflicts the merge brings in are counted.** Values one side holds are all already held by the other: merged, they are still the same few values;
            // the same object becomes one row, and a conflict can only exist if that side already had it, merged or not. Measured on the lease chain:
            // A lease already has two landlords without dates, and another spelling carries only one of them, so the 0.98 merge is blocked by
            // "two landlords", although those two were already there before the merge
            let values_of = |holder: Uuid| -> HashSet<Uuid> {
                rows.iter()
                    .filter_map(|r| match side {
                        Uniqueness::SubjectSide => (r.subject_id == holder).then_some(r.object_id?),
                        Uniqueness::ObjectSide => {
                            (r.object_id == Some(holder)).then_some(r.subject_id)
                        }
                    })
                    .collect()
            };
            let (held_by_a, held_by_b) = (values_of(a), values_of(b));
            if held_by_a.is_subset(&held_by_b) || held_by_b.is_subset(&held_by_a) {
                continue;
            }
            // The shape after merging into one: the holder side is the same entity on both
            for row in &mut rows {
                match side {
                    Uniqueness::SubjectSide => row.subject_id = a,
                    Uniqueness::ObjectSide => row.object_id = Some(a),
                }
            }
            if still_overlaps(side, &rows, s.temporal == "state") {
                labels.push(s.label.clone());
                break;
            }
        }
    }
    labels.sort();
    labels.dedup();
    Ok(labels)
}

/// The [start, end) of a row after it has been placed; `None` means that side is unbounded
type Span = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// Once a timeline has been placed by the engine, whether different values ever hold at the same moment (a pure function). `placed`: the predicate is
/// a state, which the engine places; events and constants are not placed, and are compared by the ends written on the row
fn still_overlaps(side: Uniqueness, rows: &[Row], placed: bool) -> bool {
    let ends = if placed {
        desired_ends(side, rows).0
    } else {
        HashMap::new()
    };
    // [start, end) of each row after placement: the start is the moment used for ordering, and a row with no stated time starts from the infinite past;
    // the end is inferred by the engine, or written in the source
    let spans: Vec<Span> = rows
        .iter()
        .map(|r| {
            let end = ends.get(&r.id).map(End::instant).unwrap_or_else(|| r.end());
            (r.key(), end)
        })
        .collect();
    for i in 0..rows.len() {
        for j in i + 1..rows.len() {
            if same_value(side, &rows[i], &rows[j]) {
                continue;
            }
            let ((si, ei), (sj, ej)) = (spans[i], spans[j]);
            let starts_before_end =
                |s: Option<DateTime<Utc>>, e: Option<DateTime<Utc>>| match (s, e) {
                    (Some(s), Some(e)) => s < e,
                    _ => true,
                };
            if starts_before_end(si, ej) && starts_before_end(sj, ei) {
                return true;
            }
        }
    }
    false
}

/// Recomputes several timelines each (the caller has already taken the locks via [`lock_timelines`]). Called after a retraction of a merge, or after a document
/// delete or restore has changed the rows in its own transaction
pub async fn tidy_timelines_tx(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    timelines: &[Timeline],
) -> AppResult<ReconcileReport> {
    let mut report = ReconcileReport::default();
    for timeline in timelines {
        tidy(tx, kb_id, *timeline, &mut report).await?;
    }
    Ok(report)
}

/// Reconciliation after a merge moves facts: a fact whose subject and object were swapped is equivalent to "a newly stored observation" —
/// once two objects fold into one, the uniqueness invariant is first able to see them collide. Also used after a person has edited an interval.
/// The correction row ids returned are recorded by the caller in the merge ledger, for audit
pub async fn reconcile_moved_facts(
    pool: &PgPool,
    kb_id: Uuid,
    fact_ids: &[Uuid],
) -> AppResult<ReconcileReport> {
    reconcile_facts(pool, kb_id, fact_ids).await
}

/// Every timeline the facts in a batch sit on: each fact in the batch "arrives" on its timeline one by one (recording what to hand to a person), and the whole
/// line is recomputed once. One transaction per timeline. Also called after materialization turns statements into typed rows (#899):
/// those rows are "newly stored observations" and reconcile just like ones written by extraction or confirmation
pub(crate) async fn reconcile_facts(
    pool: &PgPool,
    kb_id: Uuid,
    fact_ids: &[Uuid],
) -> AppResult<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let batch: HashSet<Uuid> = fact_ids.iter().copied().collect();
    for timeline in timelines_of(pool, kb_id, fact_ids, None).await? {
        let mut tx = pool.begin().await?;
        lock_timeline(&mut tx, kb_id, timeline).await?;
        let rows = load_timeline(&mut tx, kb_id, timeline).await?;
        let mut arriving: Vec<&Row> = rows.iter().filter(|r| batch.contains(&r.id)).collect();
        arriving.sort_by_key(|r| (r.key().is_none(), r.key(), r.id));
        let arriving: Vec<Uuid> = arriving.iter().map(|r| r.id).collect();
        for (i, id) in arriving.iter().enumerate() {
            arrive(&mut tx, kb_id, timeline, *id, &arriving[..i], &mut report).await?;
        }
        tidy(&mut tx, kb_id, timeline, &mut report).await?;
        tx.commit().await?;
    }
    Ok(report)
}

/// Declaration came late: recompute every timeline on which the existing facts of a predicate sit (#341).
///
/// In a store the ontology grew by itself, nobody declared uniqueness, so a successor does not close its predecessor — three people managing one
/// project at the same time. Once a person adds the declaration, this reconciles the rows already on the ledger. It goes through the same
/// path as storing (void + rewrite, with the supersedes chain back to the old row), so even with the record axis rolled back before the declaration, three
/// open rows still show; uncertain ones (missing time / simultaneous start / low confidence) still go to human review, not closed forcibly.
///
/// Predicates with no declaration are refused: the engine does not infer on a person's behalf (bootstrap_ontology.rs explains why).
pub async fn reconcile_predicate(
    pool: &PgPool,
    kb_id: Uuid,
    predicate_id: Uuid,
) -> AppResult<ReconcileReport> {
    let declared: Option<(bool, bool, String)> = sqlx::query_as(
        "SELECT functional, inverse_functional, temporal FROM relation_types
         WHERE kb_id = $1 AND id = $2",
    )
    .bind(kb_id)
    .bind(predicate_id)
    .fetch_optional(pool)
    .await?;
    let Some((functional, inverse_functional, temporal)) = declared else {
        return Err(utopia_core::AppError::NotFound);
    };
    if temporal != "state" {
        return Err(utopia_core::AppError::invalid(
            "not_a_state",
            "only a state relation has intervals to close",
        ));
    }
    if !functional && !inverse_functional {
        return Err(utopia_core::AppError::invalid(
            "not_unique",
            "declare the relation functional or inverse-functional first; the engine does not infer it",
        ));
    }
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM facts
         WHERE kb_id = $1 AND predicate_id = $2 AND invalidated_at IS NULL
           AND (object_id IS NOT NULL OR object_value IS NOT NULL)",
    )
    .bind(kb_id)
    .bind(predicate_id)
    .fetch_all(pool)
    .await?;
    reconcile_facts(pool, kb_id, &ids).await
}

/// Retracts a fact (a person judged it an extraction error): it never existed, and the timeline is recomputed from the remaining rows — the predecessor
/// that it had closed reconnects. Locks the timelines first, then voids (see the module header). Returns whether a row was retracted; an already voided one does not count
pub async fn retract<'a>(
    pool: impl sqlx::Acquire<'a, Database = Postgres>,
    kb_id: Uuid,
    fact_id: Uuid,
) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    let timelines = timelines_of(&mut *tx, kb_id, &[fact_id], None).await?;
    lock_timelines(&mut tx, kb_id, &timelines).await?;
    let retracted = sqlx::query(
        "UPDATE facts SET invalidated_at = now()
         WHERE id = $1 AND kb_id = $2 AND invalidated_at IS NULL",
    )
    .bind(fact_id)
    .bind(kb_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if retracted {
        tidy_timelines_tx(&mut tx, kb_id, &timelines).await?;
    }
    tx.commit().await?;
    Ok(retracted)
}

/// Void + rewrite with a **stated** end: the old row records invalidated_at (the epistemic axis), and the correction row that closes the interval
/// (world axis) is inserted with the evidence references copied along with it. A person's decision to close it goes through here, refusing an end earlier than the start; writing from the source
/// goes through `close_stated`. This end is no longer recomputed by the engine. Returns the correction row id; `None` = already voided, nothing changed.
pub async fn close_superseded<'a>(
    pool: impl sqlx::Acquire<'a, Database = Postgres>,
    fact_id: Uuid,
    valid_to: DateTime<Utc>,
    valid_to_precision: &str,
) -> AppResult<Option<Uuid>> {
    let valid_to = crate::graph::truncate_to(valid_to, Some(valid_to_precision));
    let mut tx = pool.begin().await?;
    let row: Option<(Option<DateTime<Utc>>, Option<String>)> = sqlx::query_as(
        "SELECT valid_from, valid_from_precision FROM facts
         WHERE id = $1 AND invalidated_at IS NULL FOR UPDATE",
    )
    .bind(fact_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((from, from_precision)) = row else {
        return Ok(None);
    };
    // Compare only after locking the start, so concurrent corrections cannot slip past the check; both ends are truncated to their own precision (#1294).
    // Placed at the store entry of review, it also governs closing where a conflict defaults to the new fact's start, without changing the rule for stated source text.
    if from
        .is_some_and(|from| valid_to < crate::graph::truncate_to(from, from_precision.as_deref()))
    {
        return Err(utopia_core::AppError::invalid(
            "interval_inverted",
            "The start of the interval is after its end.",
        ));
    }
    // Those closed by date (closed in review, close_at for a conflict, fact_closed for a violation) give the exact moment of the end, so they are not marked
    let end = End::At(valid_to, valid_to_precision.to_string());
    let corrected = rewrite_end_tx(&mut tx, fact_id, &end, false, false, false).await?;
    tx.commit().await?;
    Ok(corrected)
}

/// [`close_superseded`], plus whether this end is the moment just after the period the source stated (`end_after_period`,
/// #994): the write path closes "left in 2024" marked at 2025-01-01, so the reader can print 2024
pub(crate) async fn close_stated<'a>(
    pool: impl sqlx::Acquire<'a, Database = Postgres>,
    fact_id: Uuid,
    valid_to: DateTime<Utc>,
    valid_to_precision: &str,
    after_period: bool,
) -> AppResult<Option<Uuid>> {
    // The closing point is truncated to its precision (0024): a month-precision close is the 1st of that month at 00:00
    let end = End::At(
        crate::graph::truncate_to(valid_to, Some(valid_to_precision)),
        valid_to_precision.to_string(),
    );
    let mut tx = pool.begin().await?;
    let corrected = rewrite_end_tx(&mut tx, fact_id, &end, false, false, after_period).await?;
    tx.commit().await?;
    Ok(corrected)
}

/// Void + rewrite into "ended, date unknown" (0022 / #393): the old row records invalidated_at, the correction row's end is still
/// NULL with precision 'unknown'; `attested_to` is anchored on **the document that stated the end** — when read it means "up to that point";
/// `attested_from` is inherited from the old row — a bare row with no start relies on it to record the first piece of evidence, read as "from that time on".
/// This is an end stated in the source, and the engine does not recompute it. Evidence references are copied along with the row. Returns the correction row id; `None` = this is no longer
/// an open row, nothing changed.
pub async fn close_with_unknown_end<'a>(
    pool: impl sqlx::Acquire<'a, Database = Postgres>,
    fact_id: Uuid,
    attested_at: Option<DateTime<Utc>>,
) -> AppResult<Option<Uuid>> {
    let mut tx = pool.begin().await?;
    let corrected = rewrite_end_tx(
        &mut tx,
        fact_id,
        &End::Unknown(attested_at),
        false,
        true,
        false,
    )
    .await?;
    tx.commit().await?;
    Ok(corrected)
}

/// The source stated the end of a row that the **engine closed**: rewritten to what the source says, and not recomputed after that (#679 third review round).
/// `valid_to` None is "ended, date unknown", anchored on `attested_at`; `after_period`: what the source said
/// is the period just before that moment (#994). Returns the correction row id; if the row is already voided, or its end was already stated,
/// returns `None`
pub async fn state_derived_end<'a>(
    pool: impl sqlx::Acquire<'a, Database = Postgres>,
    fact_id: Uuid,
    valid_to: Option<(DateTime<Utc>, &str)>,
    after_period: bool,
    attested_at: Option<DateTime<Utc>>,
) -> AppResult<Option<Uuid>> {
    let end = match valid_to {
        Some((at, precision)) => End::At(
            crate::graph::truncate_to(at, Some(precision)),
            precision.to_string(),
        ),
        None => End::Unknown(attested_at),
    };
    let mut tx = pool.begin().await?;
    let derived: Option<bool> = sqlx::query_scalar(
        "SELECT end_derived FROM facts WHERE id = $1 AND invalidated_at IS NULL FOR UPDATE",
    )
    .bind(fact_id)
    .fetch_optional(&mut *tx)
    .await?;
    let corrected = match derived {
        Some(true) => rewrite_end_tx(&mut tx, fact_id, &end, false, false, after_period).await?,
        _ => None,
    };
    tx.commit().await?;
    Ok(corrected)
}

/// A stated end was restated in another way: a row closed by "until 2025" is now read as "left in 2024".
/// The moment is the same, so it is rewritten into a marked row (#994); the reverse order is already marked on the closing path, and both orders
/// write the same row.
///
/// The caller (`insert_fact_on`) only compared the moment; here the row is locked and two things are checked: the precision matches what this time says —
/// a day-precision 2025-01-01 stated is that day, not the end of 2024; and the end was not edited by a person
/// (`corrected_ends` is empty or only the start) — a person changing the displayed 2024 to 2025 means they want 2025.
/// Already marked rows are not marked again. Whether that row can carry the mark (ends the engine drew, and rows with no start cannot) is read out and judged by
/// `graph::period_mark_fits`. Returns the correction row id; returns `None` if no marking is needed
pub(crate) async fn mark_end_after_period<'a>(
    pool: impl sqlx::Acquire<'a, Database = Postgres>,
    fact_id: Uuid,
    precision: &str,
) -> AppResult<Option<Uuid>> {
    let mut tx = pool.begin().await?;
    #[allow(clippy::type_complexity)]
    let locked_row: Option<(Option<DateTime<Utc>>, Option<DateTime<Utc>>, bool, bool)> =
        sqlx::query_as(
            "SELECT f.valid_from, f.valid_to, f.end_derived,
                    COALESCE(r.temporal = 'state', false) AS declared_state
               FROM facts f LEFT JOIN relation_types r ON r.id = f.predicate_id
              WHERE f.id = $1 AND f.invalidated_at IS NULL
                AND f.valid_to_precision = $2 AND NOT f.end_after_period
                AND (f.corrected_ends IS NULL OR f.corrected_ends = 'start')
              FOR UPDATE OF f",
        )
        .bind(fact_id)
        .bind(precision)
        .fetch_optional(&mut *tx)
        .await?;
    let marked = match locked_row {
        // The start half, which a person edited, stays a person's: the rewrite's CASE carries 'start' across unchanged
        Some((from, Some(at), end_derived, declared_state))
            if crate::graph::period_mark_fits(declared_state, from, Some(at), end_derived) =>
        {
            let end = End::At(at, precision.to_string());
            rewrite_end_tx(&mut tx, fact_id, &end, false, false, true).await?
        }
        _ => None,
    };
    tx.commit().await?;
    Ok(marked)
}

/// Void + rewrite a row's end. `derived`: this end was inferred by the engine from the timeline (0057), and is recomputed with the
/// timeline later; `require_open`: change only rows that are still open; `after_period`: the end is the moment just after the period the source stated
/// (#994).
///
/// First `FOR UPDATE` lock on the old row: of two parallel rewrites, only one sees it still alive, and the other gets `None`,
/// so they do not each insert a correction row; anyone adding evidence to this row must also wait for it (`graph::add_evidence`), so evidence does not land
/// on the old row just voided. Conflicts not yet judged move over to the correction row: the problem remains, only this row has a new end
async fn rewrite_end_tx(
    tx: &mut Transaction<'_, Postgres>,
    fact_id: Uuid,
    end: &End,
    derived: bool,
    require_open: bool,
    after_period: bool,
) -> AppResult<Option<Uuid>> {
    let alive: Option<(Option<DateTime<Utc>>, Option<Uuid>)> = sqlx::query_as(
        "SELECT valid_from, predicate_id FROM facts
         WHERE id = $1 AND invalidated_at IS NULL
           AND (NOT $2 OR (valid_to IS NULL AND valid_to_precision IS NULL))
         FOR UPDATE",
    )
    .bind(fact_id)
    .bind(require_open)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((from, predicate)) = alive else {
        return Ok(None);
    };
    // A row declared as a state, closed at its own start, is a state that holds at no moment (#966, the same invariant as
    // `Validity::under`). A person closing a fact goes through here; the engine closes a row only at a strictly later start,
    // and the write path does not use an open row's own start to close it
    if let End::At(at, _) = end {
        if predicate.is_some()
            && from == Some(*at)
            && crate::graph::predicate_temporal(&mut **tx, predicate).await?
                == crate::graph::Temporal::State
        {
            return Err(crate::graph::empty_state_span());
        }
    }
    let (valid_to, precision, anchor) = match end {
        End::Open => (None, None, None),
        End::At(at, precision) => (Some(*at), Some(precision.as_str()), None),
        End::Unknown(anchor) => (None, Some(crate::graph::ENDED_UNKNOWN), *anchor),
    };
    // The mark is placed only on ends that can carry it (`graph::period_mark_fits`): ends drawn by the timeline, ends of "unknown date", and reopened ends, or rows with no start or whose attribute is not a state, never carry it — the start and predicate were read under the lock above. Look up
    // the attribute only when marking is needed: rewrites that do not mark (timeline redraws, closing to "unknown date")
    // do not add an extra query
    let after_period = after_period && {
        let declared_state = predicate.is_some()
            && crate::graph::predicate_temporal(&mut **tx, predicate).await?
                == crate::graph::Temporal::State;
        crate::graph::period_mark_fits(declared_state, from, valid_to, derived)
    };
    let corrected = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, layer, phrase, object_id, object_value,
                            valid_from, valid_from_precision, valid_from_grade,
                            valid_to, valid_to_precision, confidence, supersedes,
                            attested_from, attested_by, attested_to, end_derived,
                            from_statement_id, implied, corrected_ends, end_after_period)
         SELECT $1, kb_id, subject_id, predicate_id, layer, phrase, object_id, object_value,
                valid_from, valid_from_precision, valid_from_grade, $3, $4, confidence, id,
                attested_from, attested_by,
                CASE WHEN $4::text = 'unknown' THEN COALESCE($5, now()) END, $6,
                from_statement_id, implied,
                -- 终点在这里被重新写下（文档说了终点，或时间线推了一个），人改过的终点就不在了；
                -- 起点那一半照旧
                CASE corrected_ends WHEN 'both' THEN 'start' WHEN 'end' THEN NULL
                     ELSE corrected_ends END,
                $7
         FROM facts WHERE id = $2",
    )
    .bind(corrected)
    .bind(fact_id)
    .bind(valid_to)
    .bind(precision)
    .bind(anchor)
    .bind(derived)
    .bind(after_period)
    .execute(&mut **tx)
    .await?;
    carry_open_conflicts(tx, fact_id, corrected).await?;
    sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
        .bind(fact_id)
        .execute(&mut **tx)
        .await?;
    copy_evidence(tx, fact_id, corrected).await?;
    copy_qualifiers(tx, fact_id, corrected).await?;
    copy_materialization_links(tx, fact_id, corrected).await?;
    Ok(Some(corrected))
}

/// A row materialized and then rewritten is still a materialized row (#899): the statements and rule it was computed from
/// (`typed_fact_sources` / `implied_fact_sources`) are copied with the correction row, and `from_statement_id` and
/// `implied` are carried through in the INSERT. Without them, the next materialization round sees no live typed row for the statement,
/// and computes one again — the old row's sources are cleared by materialization itself (it deletes only those attached to voided rows)
async fn copy_materialization_links(
    tx: &mut Transaction<'_, Postgres>,
    from: Uuid,
    to: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO typed_fact_sources (fact_id, statement_id)
         SELECT $2, statement_id FROM typed_fact_sources WHERE fact_id = $1
         ON CONFLICT DO NOTHING",
    )
    .bind(from)
    .bind(to)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO implied_fact_sources (fact_id, rule_id, statement_id, entity_id)
         SELECT $2, rule_id, statement_id, entity_id FROM implied_fact_sources WHERE fact_id = $1
         ON CONFLICT DO NOTHING",
    )
    .bind(from)
    .bind(to)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Void + rewrite a row's holder: the subject or object swapped for another entity, the rest unchanged (evidence and edge attributes travel with the row).
/// Used when a retraction of a merge sends a row that was rewritten after the merge back to its source entity — changing the subject in place means the record-axis replay of the merge window
/// could not find where it was attached at that time (0027 only recognizes rows on the merge ledger). Returns the new row id; `None` = already voided
pub async fn rehome_tx(
    tx: &mut Transaction<'_, Postgres>,
    fact_id: Uuid,
    subject_id: Option<Uuid>,
    object_id: Option<Uuid>,
) -> AppResult<Option<Uuid>> {
    let alive: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM facts WHERE id = $1 AND invalidated_at IS NULL FOR UPDATE")
            .bind(fact_id)
            .fetch_optional(&mut **tx)
            .await?;
    if alive.is_none() {
        return Ok(None);
    }
    let moved = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, layer, phrase, object_id, object_value,
                            valid_from, valid_from_precision, valid_to, valid_to_precision,
                            confidence, derived_by_rule, supersedes,
                            attested_from, attested_by, attested_to, end_derived,
                            from_statement_id, implied, corrected_ends, end_after_period)
         SELECT $1, kb_id, COALESCE($3, subject_id), predicate_id, layer, phrase,
                COALESCE($4, object_id), object_value, valid_from, valid_from_precision, valid_to, valid_to_precision,
                confidence, derived_by_rule, id, attested_from, attested_by,
                attested_to, end_derived, from_statement_id, implied, corrected_ends, end_after_period
         FROM facts WHERE id = $2",
    )
    .bind(moved)
    .bind(fact_id)
    .bind(subject_id)
    .bind(object_id)
    .execute(&mut **tx)
    .await?;
    // Open conflicts move to the new row first: once the old row is voided, the 0051 trigger withdraws them, and a later recompute would not
    // record the same pair starting that day again — after retraction the two rows overlap, yet the review queue is empty (#679 fourth review round)
    carry_open_conflicts(tx, fact_id, moved).await?;
    sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
        .bind(fact_id)
        .execute(&mut **tx)
        .await?;
    copy_evidence(tx, fact_id, moved).await?;
    copy_qualifiers(tx, fact_id, moved).await?;
    copy_materialization_links(tx, fact_id, moved).await?;
    Ok(Some(moved))
}

/// Open conflicts move from the old row to the correction row (once the old row is voided, the 0051 trigger withdraws them).
/// If the correction row already has the same pair, keep the old one, which is withdrawn with the void
async fn carry_open_conflicts(
    tx: &mut Transaction<'_, Postgres>,
    from: Uuid,
    to: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE fact_conflicts c SET old_fact_id = $2
          WHERE c.status = 'open' AND c.old_fact_id = $1
            AND NOT EXISTS (SELECT 1 FROM fact_conflicts d
                             WHERE d.old_fact_id = $2 AND d.new_fact_id = c.new_fact_id)",
    )
    .bind(from)
    .bind(to)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE fact_conflicts c SET new_fact_id = $2
          WHERE c.status = 'open' AND c.new_fact_id = $1
            AND NOT EXISTS (SELECT 1 FROM fact_conflicts d
                             WHERE d.new_fact_id = $2 AND d.old_fact_id = c.old_fact_id)",
    )
    .bind(from)
    .bind(to)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Edge attributes are copied onto the correction row (0037): what is corrected is the time interval, the amount and title on the edge stay as they are —
/// without carrying them, closing a stint would lose its title
async fn copy_qualifiers(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    from: Uuid,
    to: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO fact_qualifiers (fact_id, qualifier_type_id, value, entity_id)
         SELECT $1, qualifier_type_id, value, entity_id
         FROM fact_qualifiers WHERE fact_id = $2
         ON CONFLICT DO NOTHING",
    )
    .bind(to)
    .bind(from)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Evidence references are copied onto the correction row. The surface predicate is carried too: what is corrected is the time interval, not what the source said.
/// Chapter dates are carried as well (0104): after an open statement is reattached and its interval changed, deleting a document still leaves the evidence whose attestation remains
/// to recompute (`documents::reattest_tx`); a new row whose evidence carries no chapter date falls back to the document's own date
async fn copy_evidence(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    from: Uuid,
    to: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, quote, proposed_predicate, document_id, doc_version,
                                    quote_start, quote_end, attested_at, attested_by)
         SELECT $1, chunk_id, quote, proposed_predicate, document_id, doc_version,
                quote_start, quote_end, attested_at, attested_by
         FROM fact_evidence WHERE fact_id = $2
         ON CONFLICT DO NOTHING",
    )
    .bind(to)
    .bind(from)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A person's manual correction of a fact's validity interval. The same mechanism as automatic closing (void + rewrite); the difference is only that
/// both ends come from the parameters, rather than inheriting the old row's start.
///
/// Extraction read "first half of 2023" as 1 January, and before this one could only delete the document and extract it again:
/// a wrong name can be changed in Review, but a wrong time had no entry point, and the whole timeline was skewed on that one value.
///
/// **Not an in-place UPDATE.** Editing in place would erase the correction itself, and that is exactly what the record axis must replay
/// (0019) — after the change, asking for March and asking for September should give different intervals. That is also the whole difference between this function
/// and an `UPDATE facts SET valid_from = …`.
///
/// A person changes the time, not the source: a materialized row, once edited, still comes from the same statements, the same rule as the rewrite of
/// the timeline ([`copy_materialization_links`]). If that were not carried, the statement would no longer be represented by a live typed row,
/// and on screen it would come back as the original wording with the original interval, and the next materialization would compute a row for it again.
///
/// Which end was changed is recorded on the correction row (`corrected_ends`, #970): an end whose value or precision (an end still carrying a mark) changed
/// is unioned with what the old row already had, in the same transaction as the correction — the fact row says "this date was edited by a person", no longer relying on an audit after the fact.
/// A person submits a whole interval, so after editing both ends are the person's (`end_derived` is cleared, the timeline no longer redraws it): the original
/// is the end inferred by the timeline, and a person leaving its value unchanged still pins it, so it also counts as the person's; an end that was the source's own stays the source's
/// if it is not changed
///
/// Whether the end is the moment just after the period the source stated (`end_after_period`, #994) is given by the caller; here it is only normalized:
/// the correction route decides it from the request, and never converts on a person's behalf
///
/// Returns the correction row id; `None` means this row was concurrently rewritten or voided, so nothing was done this time.
pub async fn correct_interval(
    pool: &PgPool,
    fact_id: Uuid,
    validity: crate::graph::Validity<'_>,
) -> AppResult<Option<Uuid>> {
    // A person's edit of an interval is also normalized by the predicate's temporal semantics (0031): filling in a span for an event still lands on its moment
    let predicate: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT predicate_id FROM facts WHERE id = $1")
            .bind(fact_id)
            .fetch_optional(pool)
            .await?;
    let predicate = predicate.flatten();
    let temporal = crate::graph::predicate_temporal(pool, predicate).await?;
    // Equal ends for a state are refused only for attributes declared as states (#966): an open statement and a row with an empty predicate read as a state is only
    // a reading; "that day" is still written with both ends equal
    let validity = match predicate {
        Some(_) => validity.truncated().under(temporal)?,
        None => validity.truncated(),
    }
    .drop_period_mark_unless(predicate.is_some() && temporal == crate::graph::Temporal::State);
    let mut tx = pool.begin().await?;
    let corrected = Uuid::now_v7();
    // A change of mark also counts as a change of end (#994): a person changing the displayed 2024 to 2025 leaves the stored moment alone, but says another year
    let inserted: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, layer, phrase, object_id, object_value,
                            valid_from, valid_from_precision,
                            valid_to, valid_to_precision, confidence, supersedes,
                            attested_from, attested_by, attested_to,
                            from_statement_id, implied, corrected_ends, end_after_period)
         SELECT $1, kb_id, subject_id, predicate_id, layer, phrase, object_id, object_value,
                $3, $4, $5, $6, confidence, id,
                attested_from, attested_by,
                CASE WHEN $6::text = 'unknown' THEN COALESCE(attested_to, now()) END,
                from_statement_id, implied,
                CASE WHEN start_marked AND end_marked THEN 'both'
                     WHEN start_marked THEN 'start'
                     WHEN end_marked THEN 'end' END,
                $7
         FROM (SELECT f.*,
                      c.start_changed OR COALESCE(f.corrected_ends IN ('start', 'both'), false)
                          AS start_marked,
                      c.end_changed OR f.end_derived
                          OR COALESCE(f.corrected_ends IN ('end', 'both'), false)
                          AS end_marked
                 FROM facts f,
                      LATERAL (SELECT f.valid_from IS DISTINCT FROM $3::timestamptz
                                          OR f.valid_from_precision IS DISTINCT FROM $4::text
                                          AS start_changed,
                                      f.valid_to IS DISTINCT FROM $5::timestamptz
                                          OR f.valid_to_precision IS DISTINCT FROM $6::text
                                          OR f.end_after_period IS DISTINCT FROM $7::boolean
                                          AS end_changed) c
                WHERE f.id = $2 AND f.invalidated_at IS NULL) old
         RETURNING id",
    )
    .bind(corrected)
    .bind(fact_id)
    .bind(validity.from)
    .bind(validity.from_precision)
    .bind(validity.to)
    .bind(validity.to_precision)
    .bind(validity.end_after_period)
    .fetch_optional(&mut *tx)
    .await?;
    // Already corrected concurrently: do not act again (same defence as close_superseded)
    if inserted.is_none() {
        tx.rollback().await?;
        return Ok(None);
    }
    sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
        .bind(fact_id)
        .execute(&mut *tx)
        .await?;
    copy_evidence(&mut tx, fact_id, corrected).await?;
    copy_qualifiers(&mut tx, fact_id, corrected).await?;
    copy_materialization_links(&mut tx, fact_id, corrected).await?;
    tx.commit().await?;
    Ok(Some(corrected))
}

/// Records a pair of conflicts. Returns whether this pair is recorded for the first time: the timeline sees the same pair again on every recompute,
/// and one already in the queue (or already judged by a person) is no longer counted as a new conflict
async fn record_conflict_tx(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    old_fact_id: Uuid,
    new_fact_id: Uuid,
    reason: &str,
) -> AppResult<bool> {
    // A pair a person already judged "keep both" is not asked again just because the two rows were later rewritten (got new ids):
    // walk up the supersedes chain to each row's predecessor; if that pair was judged, do not record it again (#679 third review round)
    let inserted = sqlx::query(
        "INSERT INTO fact_conflicts (id, kb_id, old_fact_id, new_fact_id, reason)
         SELECT $1, $2, $3, $4, $5
         WHERE NOT EXISTS (
             WITH RECURSIVE a(id) AS (
                     SELECT $3::uuid
                     UNION SELECT f.supersedes FROM facts f JOIN a ON f.id = a.id
                      WHERE f.supersedes IS NOT NULL),
                 b(id) AS (
                     SELECT $4::uuid
                     UNION SELECT f.supersedes FROM facts f JOIN b ON f.id = b.id
                      WHERE f.supersedes IS NOT NULL)
             SELECT 1 FROM fact_conflicts c
              WHERE c.status = 'resolved' AND c.resolution = 'kept_both'
                AND ((c.old_fact_id IN (SELECT id FROM a) AND c.new_fact_id IN (SELECT id FROM b))
                  OR (c.old_fact_id IN (SELECT id FROM b) AND c.new_fact_id IN (SELECT id FROM a))))
           -- 同一对反过来记过的也算记过：重算按时间线顺序看，谁是「旧」谁是「新」
           -- 随到达顺序变，不去重的话一次全量重算会翻出一堆方向相反的重复
           AND NOT EXISTS (SELECT 1 FROM fact_conflicts r
                            WHERE r.old_fact_id = $4 AND r.new_fact_id = $3 AND r.status = 'open')
         ON CONFLICT (old_fact_id, new_fact_id) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(kb_id)
    .bind(old_fact_id)
    .bind(new_fact_id)
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    Ok(inserted.rows_affected() > 0)
}

/// Review page's conflict list (both facts with names and intervals).
/// Lazy cleanup: a conflict where either side has been voided (rejected, or rewritten by another closing) is meaningless,
/// so it is auto-queued and marked stale — to prevent a wrong judgment on a zombie conflict (e.g. closing Eve on the rejected Ivan).
pub async fn list_conflicts(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<ConflictView>> {
    // **There used to be an UPDATE here.** Before reading, it changed conflicts with "one side already voided" to
    // resolved / stale / now() — the cleanup was right, but the place was wrong: `resolved_at` then recorded the
    // moment someone opened this page, and in a store nobody opens, stale conflicts stayed open forever. Retirement is now pinned to where voiding happens
    // (the 0051 trigger); here it only reads
    let rows: Vec<ConflictView> = sqlx::query_as(
        "SELECT c.id, c.reason, c.created_at, r.label AS predicate_label,
                c.old_fact_id, os.canonical_name AS old_subject,
                oo.canonical_name AS old_object, fo.valid_from AS old_valid_from, fo.valid_from_precision AS old_valid_from_precision,
                c.new_fact_id, ns.canonical_name AS new_subject,
                no_.canonical_name AS new_object, fn_.valid_from AS new_valid_from, fn_.valid_from_precision AS new_valid_from_precision,
                fn_.confidence AS new_confidence
         FROM fact_conflicts c
         JOIN facts fo ON fo.id = c.old_fact_id
         JOIN facts fn_ ON fn_.id = c.new_fact_id
         JOIN entities os ON os.id = fo.subject_id
         JOIN entities ns ON ns.id = fn_.subject_id
         JOIN relation_types r ON r.id = fo.predicate_id
         LEFT JOIN entities oo ON oo.id = fo.object_id
         LEFT JOIN entities no_ ON no_.id = fn_.object_id
         WHERE c.kb_id = $1 AND c.status = 'open'
         ORDER BY c.created_at DESC, c.id DESC
         LIMIT $2 OFFSET $3",
    )
    .bind(kb_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

mod conflicts;
pub use conflicts::{resolve_conflict, resolve_review_conflict, ConflictDecision};

/// A holder with **more than two open values** on one end of a predicate — the shape the ledger takes when uniqueness was not declared (or was declared late)
/// (#341). This is grounds for a proposal shown to people, not a verdict: when `declared` is false it says
/// "this looks like it should be declared"; when true it says "declared, but these rows have not been reconciled yet".
#[derive(Debug, Clone)]
pub struct UniquenessCandidate {
    pub predicate_id: Uuid,
    pub key: String,
    pub label: String,
    /// relation | attribute
    pub kind: String,
    /// "subject" (→ functional) or "object" (→ inverse_functional)
    pub side: &'static str,
    /// Whether uniqueness on this end is already declared
    pub declared: bool,
    /// Number of holders with more than two open values
    pub holders: usize,
    /// Number of open facts on those holders
    pub open_facts: usize,
    /// Number of intervals reconciliation would close (an estimate, by the rules at storage time: the earlier start stops at the successor's start)
    pub would_close: usize,
    /// Number of pairs reconciliation would send to human review (missing time / simultaneous start / low confidence)
    pub would_review: usize,
    /// The first few holders and their open values, sorted chronologically
    pub examples: Vec<HolderExample>,
}

#[derive(Debug, Clone)]
pub struct HolderExample {
    pub holder: String,
    pub values: Vec<OpenValue>,
}

#[derive(Debug, Clone)]
pub struct OpenValue {
    pub fact_id: Uuid,
    pub name: String,
    pub valid_from: Option<DateTime<Utc>>,
    pub confidence: f32,
    /// How the start came about (same as [`Row::start_unanchored`])
    pub valid_from_grade: Option<String>,
}

/// Each candidate carries a few examples.
const EXAMPLE_HOLDERS: usize = 3;

pub async fn uniqueness_candidates(
    pool: &PgPool,
    kb_id: Uuid,
) -> AppResult<Vec<UniquenessCandidate>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        predicate_id: Uuid,
        key: String,
        label: String,
        kind: String,
        declared: bool,
        holder_name: String,
        other_name: Option<String>,
        object_value: Option<serde_json::Value>,
        valid_from: Option<DateTime<Utc>>,
        valid_from_grade: Option<String>,
        confidence: f32,
    }
    // Each end is queried separately. `crowded` first counts the distinct values per (predicate, holder), then fetches in one batch
    // the open facts on those holders — the estimate needs the start and confidence of each adjacent pair of values,
    // and a count alone is not enough. Open = no end on the world axis, the same invariant used by reconciliation at storage time
    let subject_side: Vec<Row> = sqlx::query_as(
        "WITH open AS (
             SELECT f.id, f.predicate_id, f.subject_id AS holder, f.object_id, f.object_value,
                    COALESCE(f.object_id::text, f.object_value::text) AS value_key,
                    f.valid_from, f.valid_from_grade, f.confidence, f.recorded_at
             FROM facts f JOIN relation_types r ON r.id = f.predicate_id
             WHERE f.kb_id = $1 AND f.invalidated_at IS NULL
               AND f.valid_to IS NULL AND f.valid_to_precision IS NULL
               AND r.temporal = 'state'
               -- 名字不算：一个实体有两个名字是常态，不是「这个属性该唯一」的证据（0041）
               AND NOT (r.builtin AND r.key = 'known_as')
               AND (f.object_id IS NOT NULL OR f.object_value IS NOT NULL)
         ),
         crowded AS (
             SELECT predicate_id, holder FROM open
             GROUP BY predicate_id, holder HAVING count(DISTINCT value_key) >= 2
         )
         SELECT o.id, o.predicate_id, r.key, r.label, r.kind, r.functional AS declared,
                h.canonical_name AS holder_name, e.canonical_name AS other_name,
                o.object_value, o.valid_from, o.valid_from_grade, o.confidence
         FROM open o
         JOIN crowded c ON c.predicate_id = o.predicate_id AND c.holder = o.holder
         JOIN relation_types r ON r.id = o.predicate_id
         JOIN entities h ON h.id = o.holder
         LEFT JOIN entities e ON e.id = o.object_id
         ORDER BY r.key, h.canonical_name, o.valid_from ASC NULLS LAST, o.recorded_at ASC",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    let object_side: Vec<Row> = sqlx::query_as(
        "WITH open AS (
             SELECT f.id, f.predicate_id, f.object_id AS holder, f.subject_id,
                    f.valid_from, f.valid_from_grade, f.confidence, f.recorded_at
             FROM facts f JOIN relation_types r ON r.id = f.predicate_id
             WHERE f.kb_id = $1 AND f.invalidated_at IS NULL
               AND f.valid_to IS NULL AND f.valid_to_precision IS NULL
               AND r.temporal = 'state' AND r.kind = 'relation'
               AND f.object_id IS NOT NULL
         ),
         crowded AS (
             SELECT predicate_id, holder FROM open
             GROUP BY predicate_id, holder HAVING count(DISTINCT subject_id) >= 2
         )
         SELECT o.id, o.predicate_id, r.key, r.label, r.kind, r.inverse_functional AS declared,
                h.canonical_name AS holder_name, e.canonical_name AS other_name,
                NULL::jsonb AS object_value, o.valid_from, o.valid_from_grade, o.confidence
         FROM open o
         JOIN crowded c ON c.predicate_id = o.predicate_id AND c.holder = o.holder
         JOIN relation_types r ON r.id = o.predicate_id
         JOIN entities h ON h.id = o.holder
         JOIN entities e ON e.id = o.subject_id
         ORDER BY r.key, h.canonical_name, o.valid_from ASC NULLS LAST, o.recorded_at ASC",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    for (side, rows) in [("subject", subject_side), ("object", object_side)] {
        // Rows are already sorted by (predicate, holder, chronology), so cutting into segments gives the groups
        let mut i = 0;
        while i < rows.len() {
            let pred = rows[i].predicate_id;
            let mut cand = UniquenessCandidate {
                predicate_id: pred,
                key: rows[i].key.clone(),
                label: rows[i].label.clone(),
                kind: rows[i].kind.clone(),
                side,
                declared: rows[i].declared,
                holders: 0,
                open_facts: 0,
                would_close: 0,
                would_review: 0,
                examples: Vec::new(),
            };
            while i < rows.len() && rows[i].predicate_id == pred {
                let holder = rows[i].holder_name.clone();
                let mut values = Vec::new();
                while i < rows.len()
                    && rows[i].predicate_id == pred
                    && rows[i].holder_name == holder
                {
                    let r = &rows[i];
                    values.push(OpenValue {
                        fact_id: r.id,
                        name: r
                            .other_name
                            .clone()
                            .or_else(|| r.object_value.as_ref().map(literal_name))
                            .unwrap_or_else(|| "?".to_string()),
                        valid_from: r.valid_from,
                        valid_from_grade: r.valid_from_grade.clone(),
                        confidence: r.confidence,
                    });
                    i += 1;
                }
                let (close, review) = plan_closures(&values);
                cand.holders += 1;
                cand.open_facts += values.len();
                cand.would_close += close;
                cand.would_review += review;
                if cand.examples.len() < EXAMPLE_HOLDERS {
                    cand.examples.push(HolderExample { holder, values });
                }
            }
            out.push(cand);
        }
    }
    Ok(out)
}

/// For one holder's open values sorted chronologically, what reconciliation would do with them: (number closed, number sent to review).
///
/// Dry-runs the process of `reconcile_predicate` without storing anything: each is treated as "a newly stored observation", and compared with
/// every other still-open one — the earlier start stops at the successor's start (only if its own start is anchored may it rewrite history);
/// two with no start, or starting the same day, cannot say who succeeds whom, so they go to review and both stay open; a successor with no start
/// stops at the predecessor's start. A holder whose three have no start yields three conflict pairs, consistent with the engine.
/// This is an estimate — only the result of a real run counts
fn plan_closures(values: &[OpenValue]) -> (usize, usize) {
    let mut open = vec![true; values.len()];
    let mut close = 0;
    let mut review = 0;
    for i in 0..values.len() {
        if !open[i] {
            continue;
        }
        let new = &values[i];
        for j in (i + 1)..values.len() {
            if !open[j] {
                continue;
            }
            let old = &values[j];
            match (old.valid_from, new.valid_from) {
                (_, None) => review += 1,
                (Some(of), Some(nf)) if of == nf => review += 1,
                (Some(of), Some(nf)) if nf < of => {
                    if new.valid_from_grade.as_deref() == Some("C") {
                        review += 1;
                    } else {
                        close += 1;
                        open[i] = false;
                        break;
                    }
                }
                (_, Some(_)) => {
                    if new.valid_from_grade.as_deref() == Some("C") {
                        review += 1;
                    } else {
                        close += 1;
                        open[j] = false;
                    }
                }
            }
        }
    }
    (close, review)
}

/// How a literal value looks to a person: `{value, unit}` → "28000 CNY", a bare scalar is read as itself.
fn literal_name(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(o) => {
            let value = match o.get("value") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => return v.to_string(),
            };
            match o
                .get("unit")
                .and_then(|u| u.as_str())
                .filter(|u| !u.is_empty())
            {
                Some(unit) => format!("{value} {unit}"),
                None => value,
            }
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn v(from: Option<(i32, u32, u32)>, grade: Option<&str>) -> OpenValue {
        OpenValue {
            fact_id: Uuid::now_v7(),
            name: String::new(),
            valid_from: from.map(|(y, m, d)| Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()),
            valid_from_grade: grade.map(str::to_string),
            confidence: 0.9,
        }
    }

    fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
    }

    /// A row on the timeline: the value, the start, and the evidence that carries its own date when there is no start
    fn row(value: &str, from: Option<DateTime<Utc>>, dated: Option<DateTime<Utc>>) -> Row {
        Row {
            id: Uuid::now_v7(),
            subject_id: Uuid::nil(),
            object_id: None,
            object_value: Some(serde_json::json!({ "value": value })),
            valid_from: from,
            valid_from_precision: from.map(|_| "day".to_string()),
            valid_to: None,
            valid_to_precision: None,
            attested_to: None,
            end_derived: false,
            dated_at: dated,
            valid_from_grade: None,
            described: false,
        }
    }

    /// Applies the plan to the rows (rewriting into new ids is unrelated here; ends are swapped in place), until the plan is empty
    fn settle(rows: &mut [Row]) {
        let plan = plan_timeline(Uniqueness::SubjectSide, rows);
        for (id, end) in plan.ends {
            let r = rows.iter_mut().find(|r| r.id == id).unwrap();
            r.end_derived = end != End::Open;
            (r.valid_to, r.valid_to_precision, r.attested_to) = match end {
                End::Open => (None, None, None),
                End::At(t, p) => (Some(t), Some(p), None),
                End::Unknown(a) => (None, Some("unknown".into()), a),
            };
        }
        assert_eq!(
            plan_timeline(Uniqueness::SubjectSide, rows).ends,
            vec![],
            "一次算完就是最终的样子"
        );
    }

    fn shape(rows: &[Row]) -> Vec<(String, End)> {
        let mut out: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.object_value.as_ref().unwrap()["value"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                    r.current_end(),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The same set of rows gives the same computed timeline, whatever order they came in
    #[test]
    fn a_timeline_does_not_depend_on_the_order_its_rows_arrive() {
        let base = vec![
            row("v1", Some(day(2020, 2, 18)), None),
            row("v2", Some(day(2020, 3, 17)), None),
            row("v3", None, Some(day(2020, 4, 14))),
            row("v4", Some(day(2020, 5, 26)), None),
            row("v5", None, Some(day(2020, 6, 8))),
        ];
        let mut first = base.clone();
        settle(&mut first);
        assert_eq!(
            shape(&first),
            vec![
                ("v1".into(), End::At(day(2020, 3, 17), "day".into())),
                ("v2".into(), End::Unknown(Some(day(2020, 4, 14)))),
                ("v3".into(), End::At(day(2020, 5, 26), "day".into())),
                ("v4".into(), End::Unknown(Some(day(2020, 6, 8)))),
                ("v5".into(), End::Open),
            ]
        );
        for rotation in 1..base.len() {
            let mut rows = base.clone();
            rows.rotate_left(rotation);
            rows.reverse();
            settle(&mut rows);
            assert_eq!(shape(&rows), shape(&first));
        }
    }

    /// The later segment moved or went away, so the ends the engine drew change with it; a stated end does not move
    #[test]
    fn a_derived_end_follows_what_comes_next_and_a_stated_one_stays() {
        let mut rows = vec![
            row("A", Some(day(2016, 5, 16)), None),
            row("B", None, Some(day(2020, 8, 13))),
        ];
        settle(&mut rows);
        assert_eq!(rows[0].current_end(), End::Unknown(Some(day(2020, 8, 13))));
        // A later file says B starts 31 July: A ends that day
        rows[1].valid_from = Some(day(2020, 7, 31));
        rows[1].valid_from_precision = Some("day".into());
        settle(&mut rows);
        assert_eq!(
            rows[0].current_end(),
            End::At(day(2020, 7, 31), "day".into())
        );
        // B is retracted: A is open again
        rows.pop();
        settle(&mut rows);
        assert_eq!(rows[0].current_end(), End::Open);
        assert!(!rows[0].end_derived);

        // The source states C runs to the end of 2019, and D from June 2019 onward: two sources disagree, so we do not pick for a person
        let mut stated = vec![
            row("C", Some(day(2018, 1, 1)), None),
            row("D", Some(day(2019, 6, 1)), None),
        ];
        stated[0].valid_to = Some(day(2019, 12, 31));
        stated[0].valid_to_precision = Some("day".into());
        assert_eq!(
            plan_timeline(Uniqueness::SubjectSide, &stated),
            Plan::default()
        );
    }

    /// Gate before merging: two values that can be ordered as successors do not count as overlapping; only those that cannot be ordered do
    #[test]
    fn two_values_overlap_only_when_the_engine_cannot_order_them() {
        let succession = vec![
            row("HPBB1", Some(day(2016, 5, 16)), None),
            row("BBHQ1", None, Some(day(2020, 8, 13))),
        ];
        assert!(!still_overlaps(Uniqueness::SubjectSide, &succession, true));

        let same_day = vec![
            row("P", Some(day(2020, 1, 1)), None),
            row("Q", Some(day(2020, 1, 1)), None),
        ];
        assert!(still_overlaps(Uniqueness::SubjectSide, &same_day, true));

        let no_time = vec![row("K", None, None), row("N", Some(day(2020, 1, 1)), None)];
        assert!(still_overlaps(Uniqueness::SubjectSide, &no_time, true));

        // Two stated intervals from the sources intersect: the engine does not pick for a person
        let mut stated = vec![
            row("C", Some(day(2018, 1, 1)), None),
            row("D", Some(day(2019, 6, 1)), None),
        ];
        stated[0].valid_to = Some(day(2019, 12, 31));
        stated[0].valid_to_precision = Some("day".into());
        assert!(still_overlaps(Uniqueness::SubjectSide, &stated, true));

        // Events and constants do not go through the engine's ordering: two values without a stated time still count as colliding, as before
        let eternal = vec![row("X", None, None), row("Y", None, None)];
        assert!(still_overlaps(Uniqueness::SubjectSide, &eternal, false));
    }

    /// A successor whose start cannot be anchored may not close its predecessor, and opens no contradiction either (0045 slice 3: level C does neither);
    /// the predecessor stops at the first successor after it whose start can be anchored
    #[test]
    fn an_unanchored_successor_closes_nothing_and_opens_no_conflict() {
        let mut rows = vec![
            row("A", Some(day(2020, 1, 1)), None),
            row("B", Some(day(2020, 2, 1)), None),
            row("C", Some(day(2020, 3, 1)), None),
        ];
        rows[1].valid_from_grade = Some("C".to_string());
        let plan = plan_timeline(Uniqueness::SubjectSide, &rows);
        assert_eq!(
            plan.ends,
            vec![
                (rows[0].id, End::At(day(2020, 3, 1), "day".into())),
                (rows[1].id, End::At(day(2020, 3, 1), "day".into())),
            ]
        );
        assert!(
            plan.held.is_empty(),
            "a pair that cannot be anchored has nothing to judge"
        );
    }

    /// When reading, the successor that closes a row is found with the same "rendered from a figure description" criterion the engine uses; only the alias is swapped (#970)
    #[test]
    fn the_described_check_takes_another_alias() {
        let n = described_sql("n");
        assert!(!n.contains("= f.id"), "{n}");
        assert_eq!(n.matches("= n.id").count(), 2, "{n}");
    }

    /// A successor that is a figure description may not close its predecessor on its own; that pair is handed to a person (0040 decision 4);
    /// the predecessor stops at the first successor after it that is not a description
    #[test]
    fn a_described_successor_is_held_and_the_next_stated_one_closes() {
        let mut rows = vec![
            row("A", Some(day(2020, 1, 1)), None),
            row("B", Some(day(2020, 2, 1)), None),
            row("C", Some(day(2020, 3, 1)), None),
        ];
        rows[1].described = true;
        let plan = plan_timeline(Uniqueness::SubjectSide, &rows);
        assert_eq!(
            plan.ends,
            vec![
                (rows[0].id, End::At(day(2020, 3, 1), "day".into())),
                (rows[1].id, End::At(day(2020, 3, 1), "day".into())),
            ]
        );
        assert_eq!(plan.held, vec![(rows[0].id, rows[1].id)]);
    }

    #[test]
    fn a_chain_of_three_closes_twice() {
        let values = [
            v(Some((2023, 2, 1)), None),
            v(Some((2024, 7, 5)), None),
            v(Some((2025, 9, 1)), None),
        ];
        assert_eq!(plan_closures(&values), (2, 0));
    }

    #[test]
    fn what_the_engine_would_not_close_goes_to_review() {
        // Starting the same day
        assert_eq!(
            plan_closures(&[v(Some((2024, 1, 1)), None), v(Some((2024, 1, 1)), None)]),
            (0, 1)
        );
        // Both have no start
        assert_eq!(plan_closures(&[v(None, None), v(None, None)]), (0, 1));
        // The one with the earlier start has no anchor, so it may not rewrite history
        assert_eq!(
            plan_closures(&[
                v(Some((2023, 1, 1)), Some("C")),
                v(Some((2024, 1, 1)), None)
            ]),
            (0, 1)
        );
        // The successor has no start: it stops at the predecessor's start (at storage time, "an old fact with no start also applies")
        assert_eq!(
            plan_closures(&[v(Some((2023, 1, 1)), None), v(None, None)]),
            (1, 0)
        );
        // An unpaired one
        assert_eq!(plan_closures(&[v(Some((2023, 1, 1)), None)]), (0, 0));
        // All three have no start: every pair is unclear, three conflict pairs, consistent with the engine
        assert_eq!(
            plan_closures(&[v(None, None), v(None, None), v(None, None)]),
            (0, 3)
        );
        // The one with a start closes the two without a start: they all stop at its start
        assert_eq!(
            plan_closures(&[v(Some((2023, 1, 1)), None), v(None, None), v(None, None)]),
            (2, 0)
        );
    }

    #[test]
    fn a_literal_reads_as_a_value_with_its_unit() {
        assert_eq!(
            literal_name(&serde_json::json!({ "value": 28000, "unit": "CNY" })),
            "28000 CNY"
        );
        assert_eq!(
            literal_name(&serde_json::json!({ "value": "Staff Engineer" })),
            "Staff Engineer"
        );
        assert_eq!(literal_name(&serde_json::json!(32000)), "32000");
        assert_eq!(literal_name(&serde_json::json!("plain")), "plain");
    }
}
