//! Graph repository: ontology, entity resolution (P2 slice 1: same name and same type within a KB merged into one), the fact ledger, and graph queries.

use sqlx::PgPool;
use std::collections::HashMap;
use std::collections::HashSet;
use utopia_core::models::{
    ChunkFactView, EntityFact, EntityHistoryEvent, EntityType, EvidenceView, FactQualifier,
    FactReviewItem, GraphChange, GraphEdge, GraphNode, RelationType, StatementQualifier,
};
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// Projection of the existing facts for the same assertion: (id, valid_from, valid_to).
type FactSpanRow = (
    Uuid,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
);

// Creating a KB no longer seeds any relations, and there is no longer `ensure_default_ontology`.
//
// There used to be ten seed relations here, a Chinese wording table, a `localized` function that picks the wording by language,
// and a seeding function that ran at KB creation, on first ontology read, and before every extraction. They were retired in three steps:
//
// - `related_to` (0010): a code-level fallback that, put into the prompt, became an escape hatch
// - the other eight (`#125`): none of them carried a signature, and even after loading an ontology package they were not replaced by a same-named one
//   (`worksFor` and `works_at` have keys that do not match, so they coexisted as two edges),
//   and their axiom slot was always false, so the consistency check could never find a contradiction on them
// - `mapped_to` (0011): it describes "how this number is computed", not "what exists in the world",
//   and has moved to `concept_mappings`
//
// So the remaining function was just traversing an empty table. **From the first day a KB is created, the ontology holds only**
// **the vocabularies the user imported themselves** — the second half of the same change that removed the built-in entity classes in 0009.

/// All classes in the KB. The executor is generic: class-word alignment has to read it in the same snapshot transaction, before the model is called (#795).
pub async fn entity_types<'e>(
    pool: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    kb_id: Uuid,
) -> AppResult<Vec<EntityType>> {
    Ok(
        // Another SELECT *: parents live in the join table, which `*` cannot reach.
        // This is the third time with this same trap — the SQL sits inside a string, so cargo check stays fully green,
        // and the first request is the one that reports `no column found`
        sqlx::query_as(
            "SELECT t.*,
                    ARRAY(SELECT p.parent_id FROM entity_type_parents p
                          WHERE p.child_id = t.id) AS parents,
                    (SELECT p.parent_id FROM entity_type_parents p
                      WHERE p.child_id = t.id AND p.is_primary) AS primary_parent
             FROM entity_types t WHERE t.kb_id = $1 ORDER BY t.created_at",
        )
        .bind(kb_id)
        .fetch_all(pool)
        .await?,
    )
}

pub async fn relation_types(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<RelationType>> {
    // Do not use SELECT *: domain/range live in the join table, which `*` cannot reach,
    // and sqlx only reports "no column found" at runtime — the compiler cannot see into an SQL string
    Ok(sqlx::query_as(
        "SELECT r.*,
                ARRAY(SELECT d.entity_type_id FROM relation_type_domains d
                      WHERE d.relation_type_id = r.id) AS domains,
                ARRAY(SELECT g.entity_type_id FROM relation_type_ranges g
                      WHERE g.relation_type_id = r.id) AS ranges,
                ARRAY(SELECT q.qualifier_type_id FROM relation_type_qualifiers q
                      WHERE q.relation_type_id = r.id) AS qualifiers
         FROM relation_types r WHERE r.kb_id = $1 ORDER BY r.created_at",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}

/// Writes a fact. Returns (fact id, whether it was newly created).
///
/// Repeated observations of the same assertion (same subject, predicate, object) do not each open their own entry:
/// - a live row with the same valid_from already exists → reuse it (evidence accumulates on the same row)
/// - a new observation **with no time**, and the same assertion already has an open row → the weaker statement is folded into it ("belongs to Nebula Technology"
///   folded into "belongs to Nebula Technology since 2021-02"), rather than producing a second, undated duplicate
/// - a new observation **with a time**, and the same assertion already has a bare, timeless row → time refinement: once the new row is stored,
///   the bare row is invalidated and the new row is placed on its supersedes chain (invalidate and rewrite, so the epistemic history stays complete), and the evidence is copied along with the row
/// - both carry times but they differ → conservatively coexist (they may really be two intervals, e.g. leaving and later rejoining)
///
/// The object of a fact: an entity (relation) or a literal value (attribute or question mapping). The same folding and time-refinement logic applies.
#[derive(Debug, Clone, Copy)]
pub enum FactObject<'a> {
    Entity(Uuid),
    Value(&'a serde_json::Value),
}

/// The result of writing an attribute value onto an edge (0037).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualifierWrite {
    /// The edge did not have this attribute before; it has been written on
    Set,
    /// Already present with the same value: another observation, nothing needs to change
    Same,
    /// Already present with a **different** value. Not overwritten — the first writer keeps it, and the caller records an entry so it is visible.
    /// Two inconsistent observations in the ledger have always been two rows plus a conflict; we have not reached the step of creating a separate row here
    Conflict,
}

/// Writes a literal attribute value onto an edge. **Attributes are not part of a fact's dedup key**: hearing it again with an amount
/// is the same edge gaining the amount, not a second edge.
/// Whether two attribute values are the same: numbers compare as numbers (`65` and `65.0` are the same number — older KBs store integers,
/// while newly written ones are floats); everything else compares structurally
fn qualifier_values_agree(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a, b) {
        (serde_json::Value::Number(x), serde_json::Value::Number(y)) => x.as_f64() == y.as_f64(),
        (serde_json::Value::Object(x), serde_json::Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| qualifier_values_agree(v, w)))
        }
        (serde_json::Value::Array(x), serde_json::Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(v, w)| qualifier_values_agree(v, w))
        }
        _ => a == b,
    }
}

pub async fn upsert_fact_qualifier(
    pool: &PgPool,
    fact_id: Uuid,
    qualifier_type_id: Uuid,
    value: &serde_json::Value,
) -> AppResult<QualifierWrite> {
    let existing: Option<(serde_json::Value,)> = sqlx::query_as(
        "SELECT value FROM fact_qualifiers WHERE fact_id = $1 AND qualifier_type_id = $2",
    )
    .bind(fact_id)
    .bind(qualifier_type_id)
    .fetch_optional(pool)
    .await?;
    match existing {
        Some((v,)) if qualifier_values_agree(&v, value) => Ok(QualifierWrite::Same),
        Some(_) => Ok(QualifierWrite::Conflict),
        None => {
            sqlx::query(
                "INSERT INTO fact_qualifiers (fact_id, qualifier_type_id, value)
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(fact_id)
            .bind(qualifier_type_id)
            .bind(value)
            .execute(pool)
            .await?;
            Ok(QualifierWrite::Set)
        }
    }
}

/// The one line after `fact_qualifiers` joins the definition to the entity name
#[derive(sqlx::FromRow)]
struct QualifierRow {
    fact_id: Uuid,
    qualifier_type_id: Uuid,
    key: String,
    label: String,
    value: Option<serde_json::Value>,
    entity_id: Option<Uuid>,
    entity_name: Option<String>,
}

/// The attributes each fact in a batch carries, fetched by fact id. Both ways of reading edges (the panel and the canvas) pass through here after loading their rows
pub async fn fact_qualifiers_for(
    pool: &PgPool,
    fact_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<FactQualifier>>> {
    let mut out: HashMap<Uuid, Vec<FactQualifier>> = HashMap::new();
    if fact_ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<QualifierRow> = sqlx::query_as(
        "SELECT q.fact_id, q.qualifier_type_id, r.key, r.label, q.value, q.entity_id,
                    e.canonical_name AS entity_name
             FROM fact_qualifiers q
             JOIN relation_types r ON r.id = q.qualifier_type_id
             LEFT JOIN entities e ON e.id = q.entity_id
             WHERE q.fact_id = ANY($1)
             ORDER BY q.fact_id, r.key",
    )
    .bind(fact_ids)
    .fetch_all(pool)
    .await?;
    for r in rows {
        out.entry(r.fact_id).or_default().push(FactQualifier {
            qualifier_type_id: r.qualifier_type_id,
            key: r.key,
            label: r.label,
            value: r.value,
            entity_id: r.entity_id,
            entity_name: r.entity_name,
        });
    }
    Ok(out)
}

/// Writes an attribute keyed by role word onto an open statement, recorded on the block that makes the statement (0070 decision 1).
/// The same block saying it again writes no new row; a statement from another block gets its own row, and the old row is left alone. Exactly one of value and entity must be given
pub async fn add_statement_qualifier(
    pool: &PgPool,
    fact_id: Uuid,
    chunk_id: Uuid,
    role: &str,
    value: Option<&serde_json::Value>,
    entity_id: Option<Uuid>,
) -> AppResult<()> {
    if value.is_some() == entity_id.is_some() {
        return Err(AppError::invalid(
            "qualifier_shape",
            "a statement qualifier carries exactly one of a value or an entity",
        ));
    }
    let role = role.trim();
    if role.is_empty() {
        return Err(AppError::invalid(
            "qualifier_role",
            "a statement qualifier needs the document's role word",
        ));
    }
    sqlx::query(
        "INSERT INTO statement_qualifiers (fact_id, role, value, entity_id, chunk_id)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (fact_id, role, chunk_id) DO NOTHING",
    )
    .bind(fact_id)
    .bind(role)
    .bind(value)
    .bind(entity_id)
    .bind(chunk_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The role-word attributes of each open statement in a batch, fetched by fact id; the entity cell is joined to its display name.
/// Only takes the rows stated at the `as_of` moment (0070 decisions 2, 3); the same value recorded on several blocks is returned once, along with those blocks
pub async fn statement_qualifiers_for(
    pool: &PgPool,
    fact_ids: &[Uuid],
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<HashMap<Uuid, Vec<StatementQualifier>>> {
    let mut out: HashMap<Uuid, Vec<StatementQualifier>> = HashMap::new();
    if fact_ids.is_empty() {
        return Ok(out);
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        fact_id: Uuid,
        role: String,
        value: Option<serde_json::Value>,
        entity_id: Option<Uuid>,
        entity_name: Option<String>,
        chunk_ids: Vec<Uuid>,
    }
    let as_of_param = as_of.map(|_| 2);
    let sql = format!(
        "WITH rw_f AS (SELECT rw_ee.fact_id, {ends} FROM {evidence}
                        WHERE rw_ee.fact_id = ANY($1) GROUP BY rw_ee.fact_id)
         SELECT q.fact_id, q.role, q.value, q.entity_id, e.canonical_name AS entity_name,
                array_agg(q.chunk_id ORDER BY q.chunk_id) AS chunk_ids
         FROM statement_qualifiers q
         LEFT JOIN rw_f ON rw_f.fact_id = q.fact_id
         JOIN chunks rw_c ON rw_c.id = q.chunk_id
         JOIN documents rw_d ON rw_d.id = rw_c.document_id
         LEFT JOIN entities e ON e.id = q.entity_id
         WHERE q.fact_id = ANY($1) AND {speaks}
         GROUP BY q.fact_id, q.role, q.value, q.entity_id, e.canonical_name
         ORDER BY q.fact_id, q.role, chunk_ids",
        ends = crate::record_axis::role_word_fact_ends(as_of_param),
        evidence = crate::record_axis::ROLE_WORD_EVIDENCE,
        speaks = crate::record_axis::role_word_speaks_given("rw_c", "rw_d", "rw_f", as_of_param),
    );
    let mut query = sqlx::query_as::<_, Row>(&sql).bind(fact_ids);
    if let Some(at) = as_of {
        query = query.bind(at);
    }
    for r in query.fetch_all(pool).await? {
        out.entry(r.fact_id).or_default().push(StatementQualifier {
            role: r.role,
            value: r.value,
            entity_id: r.entity_id,
            entity_name: r.entity_name,
            chunk_ids: r.chunk_ids,
        });
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_fact(
    pool: &PgPool,
    kb_id: Uuid,
    subject_id: Uuid,
    // None = no matching relation in the ontology. The original meaning is not lost — it is in the evidence's proposed_predicate,
    // and at display time it is recovered via fact_surface_predicate() (see `facts.predicate_id`)
    predicate_id: Option<Uuid>,
    object_id: Uuid,
    validity: Validity<'_>,
    confidence: f32,
) -> AppResult<(Uuid, bool)> {
    insert_fact_inner(
        pool,
        kb_id,
        subject_id,
        predicate_id,
        FactObject::Entity(object_id),
        validity,
        confidence,
    )
    .await
}

/// A fact's position on the **world axis**: each end's moment and granularity.
///
/// Packed into a struct rather than four parallel arguments: there are two `Option<DateTime>` and two `Option<&str>`,
/// and swapping adjacent arguments of the same type compiles silently, whereas here it would flip a fact's start and end.
///
/// The three states of the end (guarded by the database constraint `facts_to_precision_matches_date`):
///
/// | Meaning | `to` | `to_precision` |
/// |---|---|---|
/// | Still ongoing | `None` | `None` |
/// | **Ended, day unknown** | `None` | `Some("unknown")` |
/// | Ended at a known time | `Some(t)` | `Some(one of WORLD_PRECISIONS)` |
///
/// The second row was added later. Before it, `to = None` carried both "still ongoing" and "ended, date unknown", so
/// a sentence such as "former CEO of Weta Digital", where **the end is clear but the date is missing**,
/// could only be written as the former, and the graph would assert something the source says has already ended.
#[derive(Debug, Clone, Copy, Default)]
pub struct Validity<'a> {
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub from_precision: Option<&'a str>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    pub to_precision: Option<&'a str>,
    /// The day this observation's evidence is from — the document's date (0022). `None` means right now: a fact a person writes now,
    /// the person is the evidence. Stored as `attested_from`; an observation that says it ended on an unknown day is stored as
    /// `attested_to` (#393, each end has its own anchor); when the same assertion is observed again, it only moves earlier.
    /// A fact with no start is in force from its own start, and one that ended on an unknown day is in force up to that end — **it is not a start**, so
    /// it is not written into `from` (0003 rejected filling document dates into the date column)
    pub attested_at: Option<chrono::DateTime<chrono::Utc>>,
    /// How the start was obtained (0045 slice 3): `A` stated by the document, `B` computed from the document's own anchor,
    /// `C` has a time phrase but the anchor cannot be reached. `None` = not passed through time resolution (entered by a person, or computed by a rule); treated as A.
    /// The temporal engine reads this to decide whether this row may close its predecessor
    pub from_grade: Option<&'a str>,
    /// The end is the moment just after the period the text states (`facts.end_after_period`, #994): "left in 2024" closes
    /// the span that started 2024-03-01, stored as 2025-01-01, while the text says 2024. Looking at the value alone cannot tell it apart from "until 2025".
    /// Normalised at the write entry point (`drop_period_mark_unless`), and kept only on rows that can carry it ([`period_mark_fits`])
    pub end_after_period: bool,
}

/// `valid_to_precision` means "ended, but the day is not known".
pub const ENDED_UNKNOWN: &str = "unknown";

/// The precision ladder for the world axis (0024): from year down to second, and no further — no source states sub-second precision; the record axis keeps
/// microseconds because that is our own clock. The end side also has `ENDED_UNKNOWN`. The database CHECK is this same table, and
/// the extraction-side `parse_time`, the `time_text` shown to the model, and the exported `rdf::world_time` are all built from it
pub const WORLD_PRECISIONS: [&str; 6] = ["year", "month", "day", "hour", "minute", "second"];

/// Truncates the value to its precision: year precision is January 1st at 00:00, second precision is a whole second. **The stored value and its precision say the same thing**
/// (0024 item 2; the database has the same CHECK); with no precision (anchors, derived bounds) it is returned as-is.
pub fn truncate_to(
    t: chrono::DateTime<chrono::Utc>,
    precision: Option<&str>,
) -> chrono::DateTime<chrono::Utc> {
    use chrono::{Datelike, NaiveDate, NaiveTime, TimeZone, Timelike};
    let n = t.naive_utc();
    let (d, time) = (n.date(), n.time());
    let (date, time) = match precision {
        Some("year") => (
            NaiveDate::from_ymd_opt(d.year(), 1, 1).unwrap_or(d),
            NaiveTime::MIN,
        ),
        Some("month") => (
            NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d),
            NaiveTime::MIN,
        ),
        Some("day") => (d, NaiveTime::MIN),
        Some("hour") => (
            d,
            NaiveTime::from_hms_opt(time.hour(), 0, 0).unwrap_or(time),
        ),
        Some("minute") => (
            d,
            NaiveTime::from_hms_opt(time.hour(), time.minute(), 0).unwrap_or(time),
        ),
        Some("second") => (
            d,
            NaiveTime::from_hms_opt(time.hour(), time.minute(), time.second()).unwrap_or(time),
        ),
        _ => return t,
    };
    chrono::Utc.from_utc_datetime(&date.and_time(time))
}

/// The far end of a bucket: the value plus one precision unit. `2024-03-15` (day) → `2024-03-16`; `2024-03` (month)
/// → `2024-04`. An event holds within the bucket it names (0031), and the end read back is that; with no precision
/// (anchors) it is returned as-is — an anchor is a moment, not a bucket
pub fn bucket_end(
    t: chrono::DateTime<chrono::Utc>,
    precision: Option<&str>,
) -> chrono::DateTime<chrono::Utc> {
    use chrono::{Duration, Months};
    match precision {
        Some("year") => t.checked_add_months(Months::new(12)).unwrap_or(t),
        Some("month") => t.checked_add_months(Months::new(1)).unwrap_or(t),
        Some("day") => t + Duration::days(1),
        Some("hour") => t + Duration::hours(1),
        Some("minute") => t + Duration::minutes(1),
        Some("second") => t + Duration::seconds(1),
        _ => t,
    }
}

/// The temporal semantics of a relation (`relation_types.temporal`, 0031). States have an interval; events are a moment — both ends are written
/// with the same value, and hold within the bucket it names; eternal ones carry no date and hold at every moment.
///
/// This column has been there since the first graph-layer migration, and the UI has always offered it; but until 0031 only the state-driven
/// engine existed, so event and eternal were written and read back as intervals anyway
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Temporal {
    #[default]
    State,
    Event,
    Eternal,
}

impl Temporal {
    /// Unrecognised values are treated as states: the database CHECK only lets these three in, so here we no longer raise an error
    pub fn parse(s: &str) -> Self {
        match s {
            "event" => Self::Event,
            "eternal" => Self::Eternal,
            _ => Self::State,
        }
    }
}

/// The predicate's temporal semantics. No predicate (0010) defaults to state — the one of the three that loses no information, consistent with
/// the judgement made when importing an ontology
pub async fn predicate_temporal<'e>(
    pool: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    predicate_id: Option<Uuid>,
) -> AppResult<Temporal> {
    let Some(id) = predicate_id else {
        return Ok(Temporal::State);
    };
    let t: Option<String> = sqlx::query_scalar("SELECT temporal FROM relation_types WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(t.as_deref().map(Temporal::parse).unwrap_or_default())
}

impl<'a> Validity<'a> {
    /// The start end is known; the end end is unknown or not applicable.
    pub fn starting(
        from: Option<chrono::DateTime<chrono::Utc>>,
        from_precision: Option<&'a str>,
    ) -> Self {
        Self {
            from,
            from_precision,
            to: None,
            to_precision: None,
            attested_at: None,
            from_grade: None,
            end_after_period: false,
        }
    }

    /// The day of the document this observation came from.
    pub fn attested(mut self, at: Option<chrono::DateTime<chrono::Utc>>) -> Self {
        self.attested_at = at;
        self
    }

    /// Both ends are truncated to their own precision (0024). Every write path runs this before entering the DB, saying the same thing as the DB CHECK
    pub fn truncated(mut self) -> Self {
        self.from = self.from.map(|t| truncate_to(t, self.from_precision));
        self.to = self.to.map(|t| truncate_to(t, self.to_precision));
        self
    }

    /// Normalised by the predicate's temporal semantics (0031). Every write path runs this before entering the DB, the same as the read-side `world_axis`
    /// — one and the same rule.
    ///
    /// - Events: the moment is written at **both ends**. If the source gives a start, use the start; if it only gives an end, that is when it happened;
    ///   given a span, take the start — an acquisition does not run until next year. With no date, both ends are empty; "ended on an unknown day" is
    ///   meaningless for a moment, so it is stripped too. Two equal ends is a shape the row can explain by itself:
    ///   a reader who ignores the predicate at most reads it as a one-day state, not as "constant from that day on"
    /// - Eternal: all dates are stripped. A date in the source is about something else, not about when this relation holds
    /// - State: as-is — **equal ends are not written** (#966). The world axis reads a state as `from <= T < to`, and a span whose start and end
    ///   are the same value holds at no moment at all; writing it would only turn "joined that day" into "was never there". A statement
    ///   about a moment, under a state property, reads as start, end, or neither — the binding decides (`phrase_bindings.marks`),
    ///   not guessed here; this only enforces the invariant, so no write path can store a state that holds at no time
    ///
    /// Callers truncate first, then normalise: two ends that only become equal after truncation to precision (two times of day on the same date) are rejected too.
    /// A row with no predicate is read as a state (0010) — that is only a reading, not a declaration: callers do not pass it through this check
    pub fn under(mut self, temporal: Temporal) -> AppResult<Self> {
        match temporal {
            Temporal::State => {
                if self.from.is_some() && self.from == self.to {
                    return Err(empty_state_span());
                }
            }
            Temporal::Eternal => {
                self.from = None;
                self.from_precision = None;
                self.to = None;
                self.to_precision = None;
            }
            Temporal::Event => {
                let moment = self
                    .from
                    .map(|t| (t, self.from_precision))
                    .or_else(|| self.to.map(|t| (t, self.to_precision)));
                let (t, p) = match moment {
                    Some((t, p)) => (Some(t), p),
                    None => (None, None),
                };
                self.from = t;
                self.from_precision = p;
                self.to = t;
                self.to_precision = p;
            }
        }
        Ok(self)
    }

    /// The source says it ended, but not which day.
    pub fn ended_when_unknown(mut self) -> Self {
        self.to = None;
        self.to_precision = Some(ENDED_UNKNOWN);
        self
    }

    /// Rows that cannot carry the "the end refers to the period before it" mark ([`period_mark_fits`]) have the mark dropped here: the two
    /// write entry points (`insert_fact_on`, `correct_interval`) each run it after [`Validity::under`].
    /// Events normalised to a moment, eternal ones with dates stripped, and rows with no predicate that skip `under` all end up here. The end that enters a write entry point is
    /// the one stated, not one the timeline drew.
    pub(crate) fn drop_period_mark_unless(mut self, declared_state: bool) -> Self {
        self.end_after_period &= period_mark_fits(declared_state, self.from, self.to, false);
        self
    }

    /// Whether this assertion no longer holds — **both kinds of ending count**.
    ///
    /// The criterion is written here rather than scattered as `valid_to.is_some()` in many places: that style would mistake
    /// "ended but on an unknown day" for "still ongoing", and that is exactly what the per-end precision was meant to fix.
    pub fn has_ended(&self) -> bool {
        self.to.is_some() || self.to_precision == Some(ENDED_UNKNOWN)
    }
}

/// Whether an end can carry the mark saying "it refers to the period before it" (`end_after_period`, #994): the property is declared
/// as a state, has a start, the end has a date, and it is not drawn from the timeline.
///
/// The same rule as the constraint `facts_end_after_period_needs_a_stated_end` (whether a property is a state constraint is invisible to it,
/// so it is checked here). Everything on the Rust side that decides this calls it: the write-entry normalisation (`Validity::drop_period_mark_unless`),
/// before `temporal::rewrite_end_tx` places the mark, before `temporal::mark_end_after_period` fills in the mark, and the routing for changing an interval.
/// Rows without a start are not marked in the first slice: the end is moved to the end of the period only if the start falls within that period. The second slice lets
/// time resolution write such an end itself, and the start clause is then dropped here and in the constraint together
pub fn period_mark_fits(
    declared_state: bool,
    from: Option<chrono::DateTime<chrono::Utc>>,
    to: Option<chrono::DateTime<chrono::Utc>>,
    end_derived: bool,
) -> bool {
    declared_state && from.is_some() && to.is_some() && !end_derived
}

/// A state whose start and end are equal (#966): writing, changing an interval, and closing all reject it, saying the same thing
pub fn empty_state_span() -> AppError {
    AppError::invalid(
        "empty_state_span",
        "A state that ends where it starts holds at no moment: give an end after the start, \
         or leave it open.",
    )
}

#[allow(clippy::too_many_arguments)]
async fn insert_fact_inner(
    pool: &PgPool,
    kb_id: Uuid,
    subject_id: Uuid,
    // None = no matching relation in the ontology. The original meaning is not lost — it is in the evidence's proposed_predicate,
    // and at display time it is recovered via fact_surface_predicate() (see `facts.predicate_id`)
    predicate_id: Option<Uuid>,
    object: FactObject<'_>,
    validity: Validity<'_>,
    confidence: f32,
) -> AppResult<(Uuid, bool)> {
    let mut conn = pool.acquire().await?;
    insert_fact_on(
        &mut conn,
        kb_id,
        subject_id,
        predicate_id,
        object,
        validity,
        confidence,
    )
    .await
}

/// The same insertion semantics on a caller-owned connection/transaction.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_fact_on(
    conn: &mut sqlx::PgConnection,
    kb_id: Uuid,
    subject_id: Uuid,
    // None = no matching relation in the ontology. The original meaning is not lost — it is in the evidence's proposed_predicate,
    // and at display time it is recovered via fact_surface_predicate() (see `facts.predicate_id`)
    predicate_id: Option<Uuid>,
    object: FactObject<'_>,
    validity: Validity<'_>,
    confidence: f32,
) -> AppResult<(Uuid, bool)> {
    // Normalised by the predicate's temporal semantics (0031): events have both ends at the same moment, and eternal ones have no date. It is written here rather than in each
    // writer — extraction, confirmation, and facts a person enters all pass through this one gate
    let temporal = predicate_temporal(&mut *conn, predicate_id).await?;
    // Only attributes declared as state enforce "the two ends are not equal" (#966); rows with no predicate read as state only as a reading, and are still written as before
    let declared_state = predicate_id.is_some() && temporal == Temporal::State;
    let validity = if predicate_id.is_some() {
        validity.truncated().under(temporal)?
    } else {
        validity.truncated()
    }
    .drop_period_mark_unless(declared_state);
    let same_sql = match object {
        FactObject::Entity(_) => {
            "SELECT id, valid_from, valid_to, valid_to_precision FROM facts
             WHERE kb_id = $1 AND subject_id = $2 AND predicate_id = $3 AND object_id = $4
               AND invalidated_at IS NULL"
        }
        FactObject::Value(_) => {
            "SELECT id, valid_from, valid_to, valid_to_precision FROM facts
             WHERE kb_id = $1 AND subject_id = $2 AND predicate_id = $3 AND object_value = $4
               AND object_id IS NULL AND invalidated_at IS NULL"
        }
    };
    let mut q = sqlx::query_as(same_sql)
        .bind(kb_id)
        .bind(subject_id)
        .bind(predicate_id);
    q = match object {
        FactObject::Entity(id) => q.bind(id),
        FactObject::Value(v) => q.bind(v),
    };
    let mut same: Vec<FactSpanRow> = q.fetch_all(&mut *conn).await?;
    // A row whose start and end are equal under a state property (written before #966) holds at no moment: it is neither closed nor merged into.
    // Otherwise, whichever of "joined that day" and "since that day" is recorded first would leave the ledger in two shapes — closing the open span at a moment,
    // or merging the open span into a moment. Such rows are invalidated and recomputed by materialisation; here we simply do not let them affect other observations
    if declared_state {
        same.retain(|(_, vf, vt, _)| !(vf.is_some() && vf == vt));
    }
    // An observation that says "ended, day unknown" hits an **open row** of the same assertion (0022 / #393): close it.
    // Do not merge into it — merging would throw away the one piece of information this brings, that it ended (the exact-duplicate path for the same valid_from does that);
    // nor create a separate row — a separate row leaves two accounts that disagree, and the open one is still read as "still the case"
    // (that is exactly how the #345 question got stuck).
    // The correction goes via supersede: the old row is invalidated, the new row's end stays empty with precision 'unknown', and `attested_to` anchors to the document that states the end;
    // the start is unchanged — a dated start is kept as the date, and a bare row with no date keeps its own
    // `attested_from` (the first evidence). Two anchors, so a bare row can still be closed
    if validity.to.is_none() && validity.to_precision == Some(ENDED_UNKNOWN) {
        let open = same
            .iter()
            .filter(|(_, vf, vt, vtp)| {
                vt.is_none() && vtp.is_none() && validity.from.is_none_or(|f| Some(f) == *vf)
            })
            .max_by_key(|(_, vf, _, _)| *vf);
        if let Some((open, _, _, _)) = open {
            if let Some(closed) =
                crate::temporal::close_with_unknown_end(&mut *conn, *open, validity.attested_at)
                    .await?
            {
                return Ok((closed, true));
            }
        }
        // An "ended, day unknown" heard again on an already-closed row: the same thing, so reuse that row.
        // The anchor only moves earlier — if an earlier document says it ended, it ended earlier. If the row's end was computed by the engine,
        // and the text now states it, change it to the stated one, so it is no longer recomputed along the timeline (#679 third-round review)
        if let Some((ended, _, _, _)) = same.iter().find(|(_, vf, vt, vtp)| {
            vt.is_none()
                && vtp.as_deref() == Some(ENDED_UNKNOWN)
                && validity.from.is_none_or(|f| Some(f) == *vf)
        }) {
            if let Some(stated) = crate::temporal::state_derived_end(
                &mut *conn,
                *ended,
                None,
                false,
                validity.attested_at,
            )
            .await?
            {
                return Ok((stated, true));
            }
            attest_earlier(&mut *conn, *ended, validity.attested_at, true).await?;
            return Ok((*ended, false));
        }
    }
    /* **An observation that something ended on some day (no start, but an end) hits an open row of the same assertion: close it, do not open a separate row.**
Opening a separate row leaves two accounts that disagree, and the open one is still read as "still the case" — measured: "removed from the blacklist"
and "resigned from the board" each gained an extra `- → date` row, while the original stayed open. Events have no open row
(both ends at the same moment), so only states take this path. Corrections go via supersede (invalidate and rewrite, with the evidence and the attributes on the edge
moving with the row), the same path as #393's closing on "unknown day"; an open row whose start is later than the period the end describes is not part of this span */
    if temporal == Temporal::State && validity.from.is_none() {
        if let Some(to) = validity.to {
            let precision = validity.to_precision.unwrap_or("day");
            // The end names a period but is compared as a moment: "left in 2024" is stored as 2024-01-01. A state-declared
            // open row that starts within this period (in post since 2024-03-01) has its end land at the end of the period
            // (2025-01-01), rather than opening a separate row `- → 2024` and leaving the open span open forever; one that starts before the period
            // is still closed at the start of the period (0053, revised 2026-09-27). The same applies to one that starts and ends on the same day: it is closed at the end of that day,
            // not at its own start — that would be a state that holds at no time (#966)
            let period_end = declared_state.then(|| bucket_end(to, Some(precision)));
            // Whether it closes at that moment, and whether the end has therefore moved to the end of the period: if it has, the row records that the end refers to the period before it
            // (`end_after_period`, #994), so that readers can print the 2024 the text actually said
            let closes_at = |from: Option<chrono::DateTime<chrono::Utc>>| match (from, period_end) {
                (Some(f), Some(end)) if to <= f && f < end => (end, true),
                _ => (to, false),
            };
            // Already closed at that moment: the same thing, so reuse that row (an engine-computed end is changed to the stated one, as above)
            if let Some((ended, from, _, _)) = same
                .iter()
                .find(|(_, vf, vt, _)| *vt == Some(closes_at(*vf).0))
            {
                let (at, moved) = closes_at(*from);
                if let Some(stated) = crate::temporal::state_derived_end(
                    &mut *conn,
                    *ended,
                    Some((at, precision)),
                    moved,
                    validity.attested_at,
                )
                .await?
                {
                    return Ok((stated, true));
                }
                // A stated end is restated in another way: the row was closed by "until 2025", and this time "left in 2024",
                // the same end. Mark it, and it does not matter which of the two sentences arrives first. The above only compares moments: an end that a person changed,
                // Ends stated at other precisions are not changed; `mark_end_after_period` locks that row, then checks again.
                // Marking it still folds into that row, and no new row is reported: reporting a new row would have materialisation record this statement's
                // `from_statement_id` on it, the rule would record `implied`, and a row the person closed would become computed; when this statement is
                // withdrawn, it is invalidated along with it
                let marked = if moved {
                    crate::temporal::mark_end_after_period(&mut *conn, *ended, precision).await?
                } else {
                    None
                };
                let row = marked.unwrap_or(*ended);
                attest_earlier(&mut *conn, row, validity.attested_at, true).await?;
                return Ok((row, false));
            }
            let open = same
                .iter()
                .filter(|(_, vf, vt, vtp)| {
                    vt.is_none()
                        && vtp.is_none()
                        && vf.is_none_or(|f| match period_end {
                            Some(end) => f < end,
                            None => f <= to,
                        })
                })
                .max_by_key(|(_, vf, _, _)| *vf);
            if let Some((open, from, _, _)) = open {
                let (at, moved) = closes_at(*from);
                if let Some(closed) =
                    crate::temporal::close_stated(&mut *conn, *open, at, precision, moved).await?
                {
                    return Ok((closed, true));
                }
            }
        }
    }
    // Exact duplicate: same valid_from → reuse. Same start, **this time with an end, and that row still open** → close it
    // ("served as director from 2020-01-10", then "served as director from 2020-01-10 to 2024-04-30")
    if let Some((existing, _, vt, vtp)) = same.iter().find(|(_, vf, _, _)| *vf == validity.from) {
        // If the end this time states the period before it (the mark carried by a row produced by a correction is inherited from the old row), carry it along when closing;
        // the reused row keeps its own end and mark as they are
        if temporal == Temporal::State && vt.is_none() && vtp.is_none() {
            if let Some(to) = validity.to {
                if let Some(closed) = crate::temporal::close_stated(
                    &mut *conn,
                    *existing,
                    to,
                    validity.to_precision.unwrap_or("day"),
                    validity.end_after_period,
                )
                .await?
                {
                    return Ok((closed, true));
                }
            }
        }
        // That row is already closed, and this observation also states an end: if the end was computed by the engine, change it to the one the text states
        if temporal == Temporal::State && (vt.is_some() || vtp.is_some()) && validity.has_ended() {
            let stated_to = validity
                .to
                .map(|to| (to, validity.to_precision.unwrap_or("day")));
            if let Some(stated) = crate::temporal::state_derived_end(
                &mut *conn,
                *existing,
                stated_to,
                validity.end_after_period,
                validity.attested_at,
            )
            .await?
            {
                return Ok((stated, true));
            }
        }
        attest_earlier(
            &mut *conn,
            *existing,
            validity.attested_at,
            validity.has_ended(),
        )
        .await?;
        return Ok((*existing, false));
    }
    // Weakened statement: the new observation has no time, and the same assertion already has an open row → fold into it (take the open row with the latest start).
    // Events have no notion of "open" — their two ends are always the same moment — so an undated re-observation folds into the existing
    // moment (0031): having once heard "acquired in March", hearing "acquired" again with no date is not a second acquisition
    if validity.from.is_none() && !validity.has_ended() {
        if let Some((existing, _, _, _)) = same
            .iter()
            .filter(|(_, _, vt, _)| vt.is_none() || temporal == Temporal::Event)
            .max_by_key(|(_, vf, _, _)| *vf)
        {
            attest_earlier(&mut *conn, *existing, validity.attested_at, false).await?;
            return Ok((*existing, false));
        }
        // No open row, but this observation's document date falls within some **already closed** row: it speaks of that span, not of a
        // new span — the "director Li Wenbo" in a penalty decision has a date within his term. Creating a separate bare row would be
        // read as "still the case", even though the term has already closed. A document date after the span still creates a separate row: it may really be a
        // new span (re-appointment), and when in doubt, do not merge
        if let Some(at) = validity.attested_at {
            if let Some((existing, _, _, _)) = same
                .iter()
                .find(|(_, vf, vt, _)| vt.is_some_and(|t| at <= t) && vf.is_none_or(|f| f <= at))
            {
                attest_earlier(&mut *conn, *existing, validity.attested_at, false).await?;
                return Ok((*existing, false));
            }
        }
    }
    // Time-refinement candidates: existing rows with no start (bare rows, or rows that know only the end — the document stating the ending may arrive first under parallel extraction),
    // when this observation carries a start → after storing, invalidate that row and chain it. For a row that knows only its end,
    // the end travels with it: if this observation states no end, keep the row's; if it does, it must be the same one
    let mut validity = validity;
    // For a state-declared row, the inherited end states a period (as above): if this start falls within the period, the end takes the period's
    // end; if it falls after the period, that end describes an earlier span, so there is no refinement and each stays in its own row (0053, revised 2026-09-27)
    let refine_target = match validity.from {
        Some(from) => same.iter().find_map(|(id, vf, vt, vtp)| {
            if vf.is_some() {
                return None;
            }
            let (end, moved) = match (validity.to, *vt) {
                (None, Some(t)) if declared_state && t <= from => {
                    let end = bucket_end(t, Some(vtp.as_deref().unwrap_or("day")));
                    if from >= end {
                        return None;
                    }
                    (Some(end), true)
                }
                (Some(to), Some(t)) if to != t => return None,
                (_, vt) => (vt, false),
            };
            Some((*id, end, vtp.clone(), moved))
        }),
        None => None,
    };
    if let Some((_, Some(vt), vtp, moved)) = &refine_target {
        if validity.to.is_none() {
            validity.to = Some(*vt);
            validity.to_precision = vtp.as_deref().map(|p| match p {
                "year" => "year",
                "month" => "month",
                "day" => "day",
                "hour" => "hour",
                "minute" => "minute",
                "second" => "second",
                _ => ENDED_UNKNOWN,
            });
            // The inherited end has been moved to the end of the period: the same mark as on the closing path, so the two orders of statements write the same row
            validity.end_after_period = *moved;
        }
    }
    let refine_target = refine_target.map(|(id, _, _, _)| id);

    let id = Uuid::now_v7();
    let insert_sql = match object {
        FactObject::Entity(_) => {
            // An observation that says it ended on an unknown day: the document that states the ending is its own document, so attested_to lands on it too
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                                valid_from, valid_from_precision,
                                valid_to, valid_to_precision, confidence,
                                attested_from, attested_to, valid_from_grade, end_after_period)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, COALESCE($11, now()),
                     CASE WHEN $9::text = 'unknown' THEN COALESCE($11, now()) END, $12, $13)"
        }
        FactObject::Value(_) => {
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value,
                                valid_from, valid_from_precision,
                                valid_to, valid_to_precision, confidence,
                                attested_from, attested_to, valid_from_grade, end_after_period)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, COALESCE($11, now()),
                     CASE WHEN $9::text = 'unknown' THEN COALESCE($11, now()) END, $12, $13)"
        }
    };
    let mut ins = sqlx::query(insert_sql)
        .bind(id)
        .bind(kb_id)
        .bind(subject_id)
        .bind(predicate_id);
    ins = match object {
        FactObject::Entity(oid) => ins.bind(oid),
        FactObject::Value(v) => ins.bind(v),
    };
    ins.bind(validity.from)
        .bind(validity.from_precision)
        .bind(validity.to)
        .bind(validity.to_precision)
        .bind(confidence)
        .bind(validity.attested_at)
        .bind(validity.from_grade)
        .bind(validity.end_after_period)
        .execute(&mut *conn)
        .await?;

    // Time refinement: a bare row (a timeless, open-ended row of the same assertion) is replaced by this observation, which carries a time — invalidate and chain, with the evidence moving with the row
    if let Some(old_id) = refine_target {
        sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
            .bind(old_id)
            .execute(&mut *conn)
            .await?;
        sqlx::query("UPDATE facts SET supersedes = $2 WHERE id = $1")
            .bind(id)
            .bind(old_id)
            .execute(&mut *conn)
            .await?;
        sqlx::query(
            // The surface predicate moves with the evidence: what is being refined is the time, not what the text said. The quote's offsets move along too
            "INSERT INTO fact_evidence (fact_id, chunk_id, quote, proposed_predicate, document_id, doc_version,
                                        quote_start, quote_end)
             SELECT $1, chunk_id, quote, proposed_predicate, document_id, doc_version,
                    quote_start, quote_end
             FROM fact_evidence WHERE fact_id = $2
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(old_id)
        .execute(&mut *conn)
        .await?;
        // Attributes on the edge also move with the row (0037): the amount and title already on the bare row are not lost just because the time was refined
        sqlx::query(
            "INSERT INTO fact_qualifiers (fact_id, qualifier_type_id, value, entity_id)
             SELECT $1, qualifier_type_id, value, entity_id
             FROM fact_qualifiers WHERE fact_id = $2
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(old_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok((id, true))
}

/// The same assertion observed again: the anchor only moves earlier (0022). An earlier document is earlier evidence;
/// a later one changes nothing — a fact holds from the moment it has evidence, and being mentioned later does not push it
/// back. `None` (right now) also does not move it: right now is never earlier than any existing evidence.
///
/// `ended`: this observation says "it ended". Only it being earlier evidence of an ending moves the end anchor;
/// an observation that says it holds moves only the start anchor. Previously both moved together, so a later-arriving, earlier-dated "holds" folded into a row
/// "ended, day unknown", and the end anchor moved onto itself, shrinking the interval to empty (the #875 replay)
async fn attest_earlier<'e>(
    pool: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    fact_id: Uuid,
    at: Option<chrono::DateTime<chrono::Utc>>,
    ended: bool,
) -> AppResult<()> {
    if let Some(at) = at {
        // attested_to exists only on rows whose end is unknown; NULL stays NULL
        sqlx::query(
            // LEAST skips NULL — an open row's attested_to is NULL, so a plain least would conjure an end anchor out of nothing for it, hitting a CHECK.
            // NULL stays NULL
            "UPDATE facts SET attested_from = least(attested_from, $2),
                              attested_to = CASE WHEN attested_to IS NULL OR NOT $3 THEN attested_to
                                                 ELSE least(attested_to, $2) END
              WHERE id = $1",
        )
        .bind(fact_id)
        .bind(at)
        .bind(ended)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// The witness of a statement: the moment the text in the section it sits in speaks, together with that date's name (0064 decision 3).
///
/// Moves only earlier (same as [`attest_earlier`]): if the same sentence was stated in an earlier document, the witness is that earlier document.
/// The name follows the date — when the date does not move, neither does the name. Materialised typed rows computed by rules have no provenance of their own,
/// so they follow the statement (`materialize::sync_typed_attestation`; the caller invokes it once after writing a document's statements)
pub async fn attest_statement(
    pool: &PgPool,
    fact_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
    by: &str,
) -> AppResult<bool> {
    let moved = sqlx::query(
        "UPDATE facts
            SET attested_from = $2, attested_by = $3
          WHERE id = $1 AND layer = 'open' AND invalidated_at IS NULL
            AND (attested_from IS NULL OR attested_from > $2
                 OR (attested_from = $2 AND attested_by IS DISTINCT FROM $3))",
    )
    .bind(fact_id)
    .bind(at)
    .bind(by)
    .execute(pool)
    .await?;
    Ok(moved.rows_affected() > 0)
}

/// An evidence row records the moment its original text speaks, together with that date's name; a section that states no date records empty.
///
/// [`attest_statement`] only moves a fact's witness earlier, and does not keep track of which evidence supplied it or what day each other piece of evidence was. Evidence
/// whose document is deleted, or whose deletion is undone, requires the fact's witness to be recomputed from the remaining evidence (`documents::reattest_tx`),
/// and that depends on each entry recorded here
pub async fn witness_evidence(
    pool: &PgPool,
    fact_id: Uuid,
    chunk_id: Uuid,
    witness: Option<(chrono::DateTime<chrono::Utc>, &str)>,
) -> AppResult<()> {
    let (at, by) = witness.unzip();
    sqlx::query(
        "UPDATE fact_evidence SET attested_at = $3, attested_by = $4
          WHERE fact_id = $1 AND chunk_id = $2
            AND (attested_at IS DISTINCT FROM $3 OR attested_by IS DISTINCT FROM $4)",
    )
    .bind(fact_id)
    .bind(chunk_id)
    .bind(at)
    .bind(by)
    .execute(pool)
    .await?;
    Ok(())
}

/// Facts with a literal-value object (object_value channel, first consumer of the question mapping).
/// Dedup: live facts with the same (S,P) and a fully equal object_value are stored only once.
#[allow(clippy::too_many_arguments)]
pub async fn insert_value_fact(
    pool: &PgPool,
    kb_id: Uuid,
    subject_id: Uuid,
    // None = no matching relation in the ontology. The original meaning is not lost — it is in the evidence's proposed_predicate,
    // and at display time it is recovered via fact_surface_predicate() (see `facts.predicate_id`)
    predicate_id: Option<Uuid>,
    object_value: &serde_json::Value,
    validity: Validity<'_>,
    confidence: f32,
) -> AppResult<(Uuid, bool)> {
    insert_fact_inner(
        pool,
        kb_id,
        subject_id,
        predicate_id,
        FactObject::Value(object_value),
        validity,
        confidence,
    )
    .await
}

/// An open statement (0044 slice 1, #729): what the document says, in its own words.
/// Returns (fact id, whether newly created).
///
/// It writes one `layer = 'open'` row in `facts`: the phrase is written as-is on `phrase`, with `predicate_id`
/// is empty (`facts_open_statement_shape` guards this). Deduplication looks for a match by (KB, subject, phrase, object) among live
/// open rows — today rows with an empty predicate are deduplicated by (s, p, o), and a NULL predicate never matches, so every
/// observation inserts a row; once the phrase is part of the key, hearing the same sentence again is the same row, and evidence accumulates on it.
/// On a match it only moves `attested_from` earlier ([`attest_earlier`]).
///
/// **Does not go through the [`insert_fact_inner`] gate**: it does not consult `predicate_temporal`, does not pass
/// `Validity::under`, writes no `valid_*`, and does not touch the timeline. An open statement has no position on the world axis yet —
/// the time it mentions is kept as the literal text (`time_mentions`); reading the text as a date is for the later slices of 0045.
/// Only the record axis lives here: `attested_from` is the date the source system gave the document (callers pass it only when
/// `doc_time_source IN ('content', 'source')`, #714), and is left empty when there is none — **we do not fill in now**
/// (0064 decision 5): the moment the document is processed is not the date the document states. The date the document itself states is written
/// by [`attest_statement`] after extraction.
///
/// The caller then writes `proposed_predicate = phrase` onto the evidence ([`add_evidence_located`]),
/// so every read path that can already accept an empty predicate (`fact_surface_predicate`) displays it by phrase, unchanged.
pub async fn insert_open_statement(
    pool: &PgPool,
    kb_id: Uuid,
    subject_id: Uuid,
    phrase: &str,
    object: FactObject<'_>,
    attested_at: Option<chrono::DateTime<chrono::Utc>>,
    confidence: f32,
) -> AppResult<(Uuid, bool)> {
    let phrase = phrase.trim();
    if phrase.is_empty() {
        return Err(AppError::invalid(
            "phrase_missing",
            "an open statement keeps the document's relation phrase",
        ));
    }
    let same_sql = match object {
        FactObject::Entity(_) => {
            "SELECT id FROM facts
             WHERE layer = 'open' AND kb_id = $1 AND subject_id = $2 AND phrase = $3
               AND object_id = $4 AND invalidated_at IS NULL
             ORDER BY recorded_at LIMIT 1"
        }
        FactObject::Value(_) => {
            "SELECT id FROM facts
             WHERE layer = 'open' AND kb_id = $1 AND subject_id = $2 AND phrase = $3
               AND object_value = $4 AND object_id IS NULL AND invalidated_at IS NULL
             ORDER BY recorded_at LIMIT 1"
        }
    };
    let mut q = sqlx::query_scalar(same_sql)
        .bind(kb_id)
        .bind(subject_id)
        .bind(phrase);
    q = match object {
        FactObject::Entity(id) => q.bind(id),
        FactObject::Value(v) => q.bind(v),
    };
    let same: Option<Uuid> = q.fetch_optional(pool).await?;
    if let Some(existing) = same {
        // When the open statement is stored, it is not yet known whether this mention says it holds or ended (time words are read in 0045's task),
        // so both anchors move together as before
        attest_earlier(pool, existing, attested_at, true).await?;
        return Ok((existing, false));
    }

    let id = Uuid::now_v7();
    let insert_sql = match object {
        FactObject::Entity(_) => {
            "INSERT INTO facts (id, kb_id, subject_id, layer, phrase, object_id,
                                confidence, attested_from)
             VALUES ($1, $2, $3, 'open', $4, $5, $6, $7)"
        }
        FactObject::Value(_) => {
            "INSERT INTO facts (id, kb_id, subject_id, layer, phrase, object_value,
                                confidence, attested_from)
             VALUES ($1, $2, $3, 'open', $4, $5, $6, $7)"
        }
    };
    let mut ins = sqlx::query(insert_sql)
        .bind(id)
        .bind(kb_id)
        .bind(subject_id)
        .bind(phrase);
    ins = match object {
        FactObject::Entity(oid) => ins.bind(oid),
        FactObject::Value(v) => ins.bind(v),
    };
    ins.bind(confidence).bind(attested_at).execute(pool).await?;
    Ok((id, true))
}

/// Writes the solved result onto an open statement's world axis (0045 decision 2: the model reads, the code computes).
///
/// Only changes rows where `layer = 'open'` and that are live — hardcoded in the WHERE, not left to the caller to check; typed rows still go
/// through the [`insert_fact_inner`] gate (the predicate's temporal semantics, deduplication, the timeline); invalidated rows are history and are not changed.
/// The value is first truncated to its precision ([`truncate_to`]); the stored value and its precision must say the same thing for the database CHECK to let it through.
/// The three end states use the same table as [`Validity`]: `(None, None)` still ongoing,
/// `(None, Some("unknown"))` ended on an unknown day, `(Some(t), Some(precision))` ended at some time.
/// An ended-on-unknown-day must have its own anchor (`facts_ended_unknown_has_anchor`, #393): the one stating the ending
/// is this statement's own document, and the anchor is `attested_from`; if the document does not say which day it is (0064 decision 5),
/// the anchor is the moment the ledger recorded it; the other two states clear `attested_to`
///
/// If this row does not exist, is not an open row, or has been invalidated, nothing is changed and `not_an_open_statement` is returned
pub async fn set_open_validity(
    pool: &PgPool,
    fact_id: Uuid,
    from: Option<chrono::DateTime<chrono::Utc>>,
    from_precision: Option<&str>,
    to: Option<chrono::DateTime<chrono::Utc>>,
    to_precision: Option<&str>,
    // How the start was obtained (0045 slice 3). **Write it even when it cannot be anchored (`C`)**: then both ends are empty,
    // and only this column can say "this sentence has a time phrase that we could not place on the axis"; materialisation carries it
    // over to typed rows, so the temporal engine does not let a guessed position rewrite the predecessor's history
    grade: Option<&str>,
) -> AppResult<()> {
    let from = from.map(|t| truncate_to(t, from_precision));
    let to = to.map(|t| truncate_to(t, to_precision));
    let done = sqlx::query(
        "UPDATE facts
            SET valid_from = $2, valid_from_precision = $3,
                valid_to = $4, valid_to_precision = $5, valid_from_grade = $6,
                attested_to = CASE WHEN $4 IS NULL AND $5 = 'unknown'
                                   THEN COALESCE(attested_to, attested_from, recorded_at) END
          WHERE id = $1 AND layer = 'open' AND invalidated_at IS NULL",
    )
    .bind(fact_id)
    .bind(from)
    .bind(from_precision)
    .bind(to)
    .bind(to_precision)
    .bind(grade)
    .execute(pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::invalid(
            "not_an_open_statement",
            "only a live open statement takes its valid time from resolution",
        ));
    }
    Ok(())
}

/// `proposed`: the predicate the model actually proposed in this block. When it matches the ontology it equals the key;
/// when a predicate outside the ontology does not land on a relation, it is the only thing still carrying the original meaning — the fact row keeps only
/// "associated with", and the source's "runs on" survives only through here.
pub async fn add_evidence(
    pool: &PgPool,
    fact_id: Uuid,
    chunk_id: Uuid,
    quote: Option<&str>,
    proposed: Option<&str>,
) -> AppResult<()> {
    add_evidence_located(pool, fact_id, chunk_id, quote, proposed, None).await
}

/// [`add_evidence`], plus the position of the quote in `chunks.text` (0044 slice 1).
///
/// `span` is a `(start, end)` of character offsets (not bytes), **computed by searching the text on the server, never taken from**
/// **the numbers the model reported**; if it cannot be located, pass `None` and both columns stay empty. Writing the same (fact, chunk) pair again fills in the offsets only when
/// the original value is empty, following the same rule as the surface predicate: whatever was recorded first is what counts.
pub async fn add_evidence_located(
    pool: &PgPool,
    fact_id: Uuid,
    chunk_id: Uuid,
    quote: Option<&str>,
    proposed: Option<&str>,
    span: Option<(i32, i32)>,
) -> AppResult<()> {
    // Evidence records its version when written: which version of which document it came from (the basis for S3 version reconciliation and the "evidence stale" check)
    // On conflict, the surface predicate is filled in rather than skipping the whole row: a re-extraction mostly hits existing (fact, chunk) pairs,
    // and DO NOTHING would leave this column forever empty for stored evidence. It is filled only when the original is empty, never overwritten —
    // for the same fact in the same chunk, the wording recorded first is its wording
    //
    // **Evidence lands on the live row** (#679 third-round review). Between storing the row and writing the evidence, timeline recomputation may already have
    // rewritten this row (changing its end, changing its id): the evidence copied during the rewrite does not include this one, so writing it onto the old row would lose it.
    // First take a `FOR SHARE` lock on this row — the transaction rewriting it holds `FOR UPDATE`, so this waits for it to commit;
    // if the row obtained is invalidated, follow supersedes to the row it was rewritten into. If it was rejected and has no successor, the evidence is still recorded on it
    let mut tx = pool.begin().await?;
    let mut target = fact_id;
    loop {
        let Some((kb_id, false)) = sqlx::query_as::<_, (Uuid, bool)>(
            "SELECT kb_id, invalidated_at IS NULL FROM facts WHERE id = $1 FOR SHARE",
        )
        .bind(target)
        .fetch_optional(&mut *tx)
        .await?
        else {
            break;
        };
        let next: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM facts WHERE supersedes = $1 AND kb_id = $2
              ORDER BY invalidated_at IS NULL DESC, recorded_at DESC LIMIT 1",
        )
        .bind(target)
        .bind(kb_id)
        .fetch_optional(&mut *tx)
        .await?;
        match next {
            Some(next) => target = next,
            None => break,
        }
    }
    sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, quote, proposed_predicate, document_id, doc_version,
                                    quote_start, quote_end)
         SELECT $1, $2, $3, left($4, 120), c.document_id, c.doc_version, $5, $6
         FROM chunks c WHERE c.id = $2
         ON CONFLICT (fact_id, chunk_id) DO UPDATE
           SET proposed_predicate = COALESCE(fact_evidence.proposed_predicate, EXCLUDED.proposed_predicate),
               quote_start = COALESCE(fact_evidence.quote_start, EXCLUDED.quote_start),
               quote_end = COALESCE(fact_evidence.quote_end, EXCLUDED.quote_end)",
    )
    .bind(target)
    .bind(chunk_id)
    .bind(quote)
    .bind(proposed)
    .bind(span.map(|(s, _)| s))
    .bind(span.map(|(_, e)| e))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The node-fetching statement shared by all graph queries.
///
/// **LEFT JOIN, not JOIN** (0009). An entity whose type has not been determined is still a node on the graph: it has a name,
/// facts and evidence; only a label is missing. An inner join would make it vanish entirely — the facts are still in the DB,
/// yet the graph cannot find it, the hardest kind of data loss to notice.
///
/// key and label stay NULL, **colour and shape take defaults**: the former is identity, and if there is none it should say so;
/// the latter is something the canvas must have, and without it nothing can be rendered. The grey dot is exactly what "not yet decided" looks like.
/// `as_of`: the position that binds the record-axis parameter (`None` = answers for now; the write path and the "present" view use this).
/// Degrees follow the canvas — during replay they count the edges connected to this node **at that time**, otherwise the number in the corner
/// would not match the graph in front of you.
/// `owner`: bound **only when a moment was actually passed** (#336). Once `fact_owner_at` wraps the column, the index on subject/object of `facts`
/// is no longer usable, and "now" is the path every graph draw takes
fn node_sql(as_of: Option<usize>, owner: Option<usize>) -> String {
    let held = crate::record_axis::facts_held_at("f", as_of);
    // Subject and object also flip: an entity merged away in March still has its own facts in February (#336)
    let subject = crate::record_axis::owner_at("f", "subject_id", owner, false);
    let object = crate::record_axis::owner_at("f", "object_id", owner, true);
    // Name facts do not count towards degree (0041): every entity has at least one name, and counting them would make every node one size bigger
    let not_name = crate::names::not_a_name("f");
    format!(
        "SELECT e.id, e.canonical_name AS name, t.key AS type_key,
        t.label AS type_label,
        coalesce(t.color, '#94a3b8') AS color,
        coalesce(t.shape, 'circle') AS shape,
        e.disambiguator,
        (SELECT count(*) FROM facts f
         WHERE f.kb_id = e.kb_id AND ({subject} = e.id OR {object} = e.id) AND {held} AND {not_name}) AS degree
     FROM entities e LEFT JOIN entity_types t ON t.id = e.type_id"
    )
}

/// Whole-graph overview: the top N entities by degree, and the edges between them.
/// `at`: the world axis — returns only edges **valid** at moment T (starting no later than T or unknown, and ending after T or open).
/// `as_of`: the record axis (0019) — returns only the edges **we hold** at moment T; an assertion changed in March should still appear
/// at its pre-March position. The two parameters stay separate all the way to the API: folding them into one control would answer
/// "March's world, with today's knowledge" when asked "March's world, with March's knowledge", and both would
/// look plausible on screen.
/// Graph overview: the `limit` nodes with the highest degree, and the edges between them.
///
/// **Also returns the total.** How many are drawn is a rendering matter; how many are in the KB is a knowledge-base matter, and previously both
/// were shown as the same number in the interface — a KB of tens of thousands of entities always read 150 in the corner, and that
/// was the cap, not the scale. The rendering cap itself is reasonable (nobody can make sense of ten thousand points); what misled was
/// presenting it as the total.
pub async fn overview(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
    at: Option<chrono::DateTime<chrono::Utc>>,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<(Vec<GraphNode>, Vec<GraphEdge>, i64, i64)> {
    let nodes: Vec<GraphNode> = sqlx::query_as(&format!(
        "{} WHERE e.kb_id = $1 AND {visible} ORDER BY degree DESC, e.created_at LIMIT $2",
        node_sql(as_of.map(|_| 3), as_of.map(|_| 3)),
        visible = crate::record_axis::entity_visible_at("e", as_of.map(|_| 3)),
    ))
    .bind(kb_id)
    .bind(limit)
    .bind(as_of)
    .fetch_all(pool)
    .await?;

    let ids: Vec<Uuid> = nodes.iter().map(|n| n.id).collect();
    let edges = edges_among(pool, kb_id, &ids, at, as_of).await?;

    // The total is counted on the same basis as the canvas: merged entities are not counted, invalidated facts are not counted,
    // and attribute facts (whose object is a literal) cannot be drawn as edges, so they are not counted either. If the basis differed,
    // the 325 in "150 / 325" would not match the number the user sees elsewhere — **and the same holds during replay**:
    // the edge count follows the record axis, otherwise a graph rewound to March would show today's edge count.
    //
    // The node count also rewinds (#336): an entity's moment lives on `entity_merges`, not on the entity row —
    // the one merged away in March should appear on the canvas in February, and be counted in this total.
    let total_nodes: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM entities e WHERE e.kb_id = $1 AND {visible}",
        visible = crate::record_axis::entity_visible_at("e", as_of.map(|_| 2)),
    ))
    .bind(kb_id)
    .bind(as_of)
    .fetch_one(pool)
    .await?;
    let total_edges: i64 = sqlx::query_scalar(&format!(
        // Both sides need `object_id IS NOT NULL`: what is counted is **the edges that can be drawn**. After the derived table was widened
        // (0021), literal-value conclusions also live in this table; counting them in would make the edge count shown in the status bar
        // larger than what is on the canvas, and the extra ones could never be found.
        "SELECT (SELECT count(*) FROM facts f
                  WHERE f.kb_id = $1 AND {facts_held} AND f.object_id IS NOT NULL)
              + (SELECT count(*) FROM derived_facts d
                  WHERE d.kb_id = $1 AND {derived_held} AND d.object_id IS NOT NULL)",
        facts_held = crate::record_axis::facts_held_at("f", as_of.map(|_| 2)),
        derived_held = crate::record_axis::derived_held_at("d", as_of.map(|_| 2)),
    ))
    .bind(kb_id)
    .bind(as_of)
    .fetch_one(pool)
    .await?;
    Ok((nodes, edges, total_nodes, total_edges))
}

/// This open statement is already represented by a live typed row (0044 decision 1: the typed graph is the view of the open graph computed by binding).
/// The canvas and the entity panel use this to draw one edge per statement: if a typed row exists, draw it;
/// its label is the attribute's name; otherwise draw the original wording. **Both are kept in the ledger**; this only governs the display.
/// `$f` is replaced with the table alias at the call site
const REPRESENTED_BY_TYPED: &str = "EXISTS (SELECT 1 FROM typed_fact_sources src
                    JOIN facts ty ON ty.id = src.fact_id
                   WHERE src.statement_id = $f.id AND ty.invalidated_at IS NULL)";

fn represented_by_typed(alias: &str) -> String {
    REPRESENTED_BY_TYPED.replace("$f", alias)
}

/// The original wording of the statements behind a typed row, deduplicated and joined (several statements may compute the same row; see 0068).
fn said_as(alias: &str) -> String {
    format!(
        "(SELECT string_agg(DISTINCT st.phrase, ' · ')
            FROM typed_fact_sources src JOIN facts st ON st.id = src.statement_id
           WHERE src.fact_id = {alias}.id)"
    )
}

async fn edges_among(
    pool: &PgPool,
    kb_id: Uuid,
    ids: &[Uuid],
    at: Option<chrono::DateTime<chrono::Utc>>,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<GraphEdge>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // **Derived edges are explicitly UNIONed in.** They live in `derived_facts`, not in `facts` —
    // so every read path that wants to see inferred results has to write it out as here. Forgetting to write it means
    // derived edges are invisible, rather than being taken as someone's assertions (which is exactly what the split table buys).
    //
    // The graph wants them, because "this edge was inferred" is exactly one of the things the user should see; the `derived`
    // flag lets the interface draw the difference, and lets a person filter them out as a whole.
    //
    // The third segment is a **ghost edge** (0017 §3): a derivation that was inferred but never landed, living in the `detail` of `axiom_violations`.
    // Its id is the id of the violation; `derived` and `blocked` are both true,
    // and the interface uses that to make it follow the derived toggle and draw it in the contested colour, blended into the background.
    //
    // The assertion segment gets one extra bit, `contested`: an open violation or a temporal conflict points at it.
    // When a derivation collides with an assertion, the one hit is left; right is only the last premise, and has no dispute of its own
    let mut edges: Vec<GraphEdge> = sqlx::query_as(&format!(
        "SELECT f.id, {subject} AS source, {object} AS target,
                COALESCE(r.key, fact_surface_predicate(f.id)) AS predicate,
                COALESCE(r.label, fact_surface_predicate(f.id)) AS label,
                {said_as} AS said_as,
                r.id IS NULL AS inferred, FALSE AS derived, NULL::text AS rule,
                ARRAY[]::uuid[] AS premises,
                f.valid_from, f.valid_to,
                {holds_from} AS holds_from, {holds_to} AS holds_to, f.confidence,
                (EXISTS (SELECT 1 FROM axiom_violations v
                          WHERE {violation_open}
                            AND (v.left_fact = f.id
                                 OR (v.right_fact = f.id AND v.kind <> 'derived_contradiction')))
                 OR EXISTS (SELECT 1 FROM fact_conflicts c
                             WHERE {conflict_open}
                               AND (c.old_fact_id = f.id OR c.new_fact_id = f.id))
                ) AS contested,
                FALSE AS blocked
         FROM facts f LEFT JOIN relation_types r ON r.id = f.predicate_id
         WHERE f.kb_id = $1 AND {facts_held} AND f.object_id IS NOT NULL
           AND {subject} = ANY($2) AND {object} = ANY($2)
           AND {facts_hold}
           AND NOT {represented}
         UNION ALL
         SELECT d.id, d.subject_id AS source, d.object_id AS target,
                r.key AS predicate, r.label AS label, NULL::text AS said_as,
                FALSE AS inferred, TRUE AS derived, ru.kind AS rule,
                ARRAY(SELECT fd.premise_fact_id FROM fact_derivations fd
                       WHERE fd.derived_fact_id = d.id
                         AND fd.premise_fact_id IS NOT NULL
                       ORDER BY fd.seq) AS premises,
                d.valid_from, d.valid_to,
                d.valid_from AS holds_from, d.valid_to AS holds_to, d.confidence,
                FALSE AS contested, FALSE AS blocked
         FROM derived_facts d JOIN relation_types r ON r.id = d.predicate_id
                              JOIN rules ru ON ru.id = d.rule_id
         WHERE d.kb_id = $1 AND {derived_held}
           AND d.subject_id = ANY($2) AND d.object_id = ANY($2)
           AND {derived_hold}
         UNION ALL
         SELECT v.id,
                (v.detail->>'subject_id')::uuid AS source,
                (v.detail->>'object_id')::uuid AS target,
                v.detail->>'predicate' AS predicate, v.detail->>'predicate' AS label,
                NULL::text AS said_as,
                FALSE AS inferred, TRUE AS derived, v.detail->>'rule' AS rule,
                v.path AS premises,
                (v.detail->>'valid_from')::timestamptz AS valid_from,
                (v.detail->>'valid_to')::timestamptz AS valid_to,
                (v.detail->>'valid_from')::timestamptz AS holds_from,
                (v.detail->>'valid_to')::timestamptz AS holds_to,
                0::real AS confidence,
                TRUE AS contested, TRUE AS blocked
         FROM axiom_violations v
         WHERE v.kb_id = $1 AND v.kind = 'derived_contradiction' AND {violation_open}
           AND (v.detail->>'subject_id')::uuid = ANY($2)
           AND (v.detail->>'object_id')::uuid = ANY($2)
           AND {ghost_hold}",
        // World axis (0022): all three segments are assembled from world_axis, so the meaning of NULL is no longer hand-written at the read point
        facts_hold = crate::world_axis::facts_hold_at("f", 3),
        said_as = said_as("f"),
        represented = represented_by_typed("f"),
        derived_hold = crate::world_axis::derived_hold_at("d", 3),
        ghost_hold = crate::world_axis::interval_holds_at(
            "(v.detail->>'valid_from')::timestamptz",
            "(v.detail->>'valid_to')::timestamptz",
            3,
        ),
        holds_from = crate::world_axis::facts_holds_from("f"),
        holds_to = crate::world_axis::facts_holds_to("f"),
        facts_held = crate::record_axis::facts_held_at("f", as_of.map(|_| 4)),
        derived_held = crate::record_axis::derived_held_at("d", as_of.map(|_| 4)),
        violation_open = crate::record_axis::violation_open_at("v", as_of.map(|_| 4)),
        conflict_open = crate::record_axis::conflict_open_at("c", as_of.map(|_| 4)),
        // Derived edges do not rewind with the timeline: the engine infers them from the assertions at the time, and their subject and object were never rewritten by merges
        subject = crate::record_axis::owner_at("f", "subject_id", as_of.map(|_| 4), false),
        object = crate::record_axis::owner_at("f", "object_id", as_of.map(|_| 4), true),
    ))
    .bind(kb_id)
    .bind(ids)
    .bind(at)
    .bind(as_of)
    .fetch_all(pool)
    .await?;
    // Attributes on edges are fetched in one go by id from the two other tables (0037 by ontology property, 0061 by role word) and attached
    {
        let ids: Vec<Uuid> = edges.iter().map(|x| x.id).collect();
        let (mut typed, mut worded) = tokio::try_join!(
            fact_qualifiers_for(pool, &ids),
            statement_qualifiers_for(pool, &ids, as_of)
        )?;
        for x in edges.iter_mut() {
            x.qualifiers = typed.remove(&x.id).unwrap_or_default();
            x.statement_qualifiers = worded.remove(&x.id).unwrap_or_default();
        }
    }
    Ok(edges)
}

/// Neighbourhood expansion (BFS, at most 2 hops, node count capped).
pub async fn neighborhood(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    hops: u8,
    at: Option<chrono::DateTime<chrono::Utc>>,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<(Vec<GraphNode>, Vec<GraphEdge>)> {
    const MAX_NODES: usize = 300;
    let mut seen: HashSet<Uuid> = HashSet::from([entity_id]);
    let mut frontier: Vec<Uuid> = vec![entity_id];

    for _ in 0..hops.clamp(1, 2) {
        if frontier.is_empty() || seen.len() >= MAX_NODES {
            break;
        }
        // Fanning out also goes through the record axis: neighbours are looked up using the edges **at that time**, otherwise the replayed graph would grow
        // nodes that are only connected today
        let touching: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(&format!(
            "SELECT {subject}, {object} FROM facts f
             WHERE f.kb_id = $1 AND {facts_held} AND f.object_id IS NOT NULL
               AND ({subject} = ANY($2) OR {object} = ANY($2))",
            facts_held = crate::record_axis::facts_held_at("f", as_of.map(|_| 3)),
            subject = crate::record_axis::owner_at("f", "subject_id", as_of.map(|_| 3), false),
            object = crate::record_axis::owner_at("f", "object_id", as_of.map(|_| 3), true),
        ))
        .bind(kb_id)
        .bind(&frontier)
        .bind(as_of)
        .fetch_all(pool)
        .await?;

        let mut next = Vec::new();
        for (s, o) in touching {
            for id in [Some(s), o].into_iter().flatten() {
                if seen.len() >= MAX_NODES {
                    break;
                }
                if seen.insert(id) {
                    next.push(id);
                }
            }
        }
        frontier = next;
    }

    let ids: Vec<Uuid> = seen.into_iter().collect();
    let nodes: Vec<GraphNode> = sqlx::query_as(&format!(
        "{} WHERE e.kb_id = $1 AND e.id = ANY($2) AND {visible}",
        node_sql(as_of.map(|_| 3), as_of.map(|_| 3)),
        visible = crate::record_axis::entity_visible_at("e", as_of.map(|_| 3)),
    ))
    .bind(kb_id)
    .bind(&ids)
    .bind(as_of)
    .fetch_all(pool)
    .await?;
    let edges = edges_among(pool, kb_id, &ids, at, as_of).await?;
    Ok((nodes, edges))
}

/// Cosine distance between the context profile of this batch of entities and a vector; those without a profile are not in the results.
/// The graph tool compares them with the user's question: among several with the same name, whoever's profile is closest to the question is most likely the one being asked about
pub async fn profile_distances(
    pool: &PgPool,
    kb_id: Uuid,
    ids: &[Uuid],
    embedding: &[f32],
) -> AppResult<Vec<(Uuid, f64)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(Uuid, f64)> = sqlx::query_as(
        "SELECT e.id, (e.profile_embedding <=> $3)::float8
           FROM entities e
          WHERE e.kb_id = $1 AND e.id = ANY($2) AND e.profile_embedding IS NOT NULL",
    )
    .bind(kb_id)
    .bind(ids)
    .bind(pgvector::Vector::from(embedding.to_vec()))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Find entities by name. **Also returns the total** — "decline to merge" by its nature produces many same-named entities,
/// and with a fixed ten, the one being sought may not be among those ten at all, and the interface gives no sign of it.
///
/// **Exact-name matches come first** (canonical name or a current alias), then by degree. Ordering by degree alone would mean
/// asking for "Apple" while the KB holds an "Apple Store …" with nine more facts pushes the entity called Apple to tenth place,
/// and the chat and MCP tools only read the first eight — it is not found, and reading facts by name reads another entity's facts.
pub async fn search_entities(
    pool: &PgPool,
    kb_id: Uuid,
    text: &str,
    limit: i64,
    offset: i64,
    // Record axis (0019): if given, replay as of **that time** — list the entities visible at that time (a merged-away entity is still present before the merge, and entities created later are not),
    // with degree counted by who held facts at that time, consistent with the canvas during replay
    as_of: Option<chrono::DateTime<chrono::Utc>>,
    // Only exact name matches (canonical name or current alias); no substring scan
    exact: bool,
) -> AppResult<(Vec<GraphNode>, i64)> {
    let pattern = format!("%{}%", text.trim());
    let named = crate::names::has_name_like("e", 2);
    // The same-name key uses the same form as recall (the use of `has_name_in` in `resolution`): after normalisation, lowercased
    let folded_name = vec![crate::resolution::normalize_name(text).to_lowercase()];
    // Without replay the SQL has no moment parameter, identical to before; only during replay is one more bound
    let visible = |param: usize| crate::record_axis::entity_visible_at("e", as_of.map(|_| param));
    let rewind = as_of.map(|_| 6);
    let same_name = crate::names::has_name_in("e", 1, 5);
    // For exact matches, write canonical name and alias as one set of ids, each going through its own index (`entities_kb_name_idx`,
    // `facts_value_text_idx`). Written as `OR`, one side is a subquery, and the planner can only scan the entities in the KB.
    // The canonical-name index only holds entities that have not been merged: without replay this is the same as `visible`, so include it; during replay
    // the merged-away entity was still there at that time, so it cannot be included, and that branch falls back to scanning the KB. `$2` is bound but unused, so parameter positions do not shift
    let exactly_named = |names: usize| {
        format!(
            "e.id IN (SELECT x.id FROM entities x
                       WHERE x.kb_id = $1 AND lower(x.canonical_name) = ANY(${names}){unmerged}
                      UNION ALL {aliases})",
            unmerged = if as_of.is_none() {
                " AND x.merged_into IS NULL"
            } else {
                ""
            },
            aliases = crate::names::named_ids(1, names),
        )
    };
    let matches = if exact {
        exactly_named(5)
    } else {
        format!("(e.canonical_name ILIKE $2 OR {named})")
    };
    let sql = format!(
        "{} WHERE e.kb_id = $1 AND {visible}
         AND {matches}
         ORDER BY (lower(e.canonical_name) = ANY($5) OR {same_name}) DESC,
                  degree DESC, e.canonical_name, e.id LIMIT $3 OFFSET $4",
        node_sql(rewind, rewind),
        visible = visible(6),
    );
    let mut nodes_query = sqlx::query_as::<_, GraphNode>(&sql)
        .bind(kb_id)
        .bind(&pattern)
        .bind(limit)
        .bind(offset)
        .bind(&folded_name);
    if let Some(t) = as_of {
        nodes_query = nodes_query.bind(t);
    }
    let nodes: Vec<GraphNode> = nodes_query.fetch_all(pool).await?;
    // The name list for the count clause is in `$3`, and the moment has moved to `$4`
    let count_matches = if exact {
        exactly_named(3)
    } else {
        format!("(e.canonical_name ILIKE $2 OR {named})")
    };
    let count_sql = format!(
        "SELECT count(*) FROM entities e
          WHERE e.kb_id = $1 AND {visible}
            AND {count_matches}",
        visible = visible(4),
    );
    let mut count_query = sqlx::query_as::<_, (i64,)>(&count_sql)
        .bind(kb_id)
        .bind(&pattern)
        .bind(&folded_name);
    if let Some(t) = as_of {
        count_query = count_query.bind(t);
    }
    let (total,) = count_query.fetch_one(pool).await?;
    Ok((nodes, total))
}

/// A bare entity node, without facts: for when you only want to know whether it exists in this KB and what it is called.
/// `entity_detail` reads all facts together, so a hub entity would be hundreds of rows
pub async fn entity_node(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Option<GraphNode>> {
    Ok(sqlx::query_as(&format!(
        "{} WHERE e.kb_id = $1 AND e.id = $2",
        node_sql(as_of.map(|_| 3), as_of.map(|_| 3))
    ))
    .bind(kb_id)
    .bind(entity_id)
    .bind(as_of)
    .fetch_optional(pool)
    .await?)
}

/// Remarks written by people in this KB when changing intervals (the `fact.time_corrected` audit is recorded on the row that was changed, #970): each changed row
/// takes the most recent one. Fetched once at the start of the query; only rows carrying the person-changed mark ([`corrected_ends_sql`]) follow
/// their own supersedes chain to it. The fetch uses the partial index `audit_events_time_corrected_idx` (migration
/// 0097): it takes only this one kind of action, and does not read the KB's other audit events. `$1` is the KB id
pub(crate) const TIME_CORRECTED_TARGETS: &str = "time_corrected_targets AS MATERIALIZED (
         SELECT DISTINCT ON (target_id) target_id, detail->>'note' AS note FROM audit_events
          WHERE kb_id = $1 AND action = 'fact.time_corrected' AND target_id IS NOT NULL
          ORDER BY target_id, created_at DESC)";

/// Which end of the row a person changed: `start` / `end` / `both`; NULL means not changed (`facts.corrected_ends`,
/// written in the same transaction as the correction row, and carried along with the row when it is closed or moved, #970). When the end is
/// derived by the timeline (`end_derived`), the end is no longer described as person-changed: that end was later set by the timeline
pub(crate) fn corrected_ends_sql(alias: &str) -> String {
    format!(
        "(CASE WHEN {alias}.end_derived AND {alias}.corrected_ends = 'end' THEN NULL
               WHEN {alias}.end_derived AND {alias}.corrected_ends = 'both' THEN 'start'
               ELSE {alias}.corrected_ends END)"
    )
}

/// The remark written when a person changed the interval (#970 step 2): that of the most recent correction on the chain. If that correction wrote no remark, it is empty —
/// a remark from an earlier correction describes the interval that it replaced
pub(crate) fn correction_note_sql(alias: &str) -> String {
    format!(
        "(WITH RECURSIVE chain(id, depth) AS (
              SELECT {alias}.supersedes, 1 WHERE {alias}.supersedes IS NOT NULL
              UNION ALL
              SELECT p.supersedes, chain.depth + 1 FROM facts p JOIN chain ON p.id = chain.id
               WHERE p.supersedes IS NOT NULL)
          SELECT t.note FROM chain JOIN time_corrected_targets t ON t.target_id = chain.id
           ORDER BY chain.depth LIMIT 1)"
    )
}

/// The row the timeline attached when it closed `f` (#970 step 2), joined into the query as `closer`.
///
/// The same judgement the engine uses to close it (`temporal::desired_ends`): same predicate, same timeline — functional
/// by subject, inverse functional by object — and the start is exactly this row's end, with a different value. One whose start cannot be anchored, or which is
/// only described by the graph and not taken by the engine to close, is not recognised here either; when several rows fit, take the smallest id, as the engine's ordering does.
/// Only looks at rows with `end_derived`; a successor with no start (ended on an unknown day) cannot be found.
///
/// The taking-over end: the same object with a new subject (a project with a new lead) is a new subject; the same subject with a new
/// object, and an attribute fact is a new value. `subject` / `object` are the owners of `f` computed on the record axis, and `as_of` is the parameter position
pub(crate) fn closed_by_join(subject: &str, object: &str, as_of: Option<usize>) -> String {
    let n_subject = crate::record_axis::owner_at("n", "subject_id", as_of, false);
    let n_object = crate::record_axis::owner_at("n", "object_id", as_of, true);
    let n_held = crate::record_axis::facts_held_at("n", as_of);
    let described = crate::temporal::described_sql("n");
    format!(
        "LEFT JOIN LATERAL (
             SELECT n.id AS closed_by_id,
                    CASE WHEN rn.inverse_functional AND {n_object} = {object}
                         THEN n_s.canonical_name ELSE n_o.canonical_name END AS closed_by,
                    CASE WHEN rn.inverse_functional AND {n_object} = {object}
                         THEN NULL ELSE n.object_value END AS closed_by_value
               FROM facts n
               JOIN relation_types rn ON rn.id = n.predicate_id
               LEFT JOIN entities n_s ON n_s.id = {n_subject}
               LEFT JOIN entities n_o ON n_o.id = {n_object}
              WHERE f.end_derived AND f.valid_to IS NOT NULL
                AND n.kb_id = f.kb_id AND n.predicate_id = f.predicate_id AND n.id <> f.id
                AND {n_held}
                AND n.valid_from = f.valid_to
                AND n.valid_from_grade IS DISTINCT FROM 'C'
                AND NOT {described}
                AND ((rn.functional AND {n_subject} = {subject}
                      AND ({n_object} IS DISTINCT FROM {object}
                           OR n.object_value IS DISTINCT FROM f.object_value))
                  OR (rn.inverse_functional AND {n_object} = {object}
                      AND {n_subject} <> {subject}))
              ORDER BY n.id LIMIT 1) closer ON true"
    )
}

/// Entity detail: node information + fact timeline.
pub async fn entity_detail(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    at: Option<chrono::DateTime<chrono::Utc>>,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<(GraphNode, Vec<EntityFact>)> {
    let node = entity_node(pool, kb_id, entity_id, as_of)
        .await?
        .ok_or(AppError::NotFound)?;

    let mut facts: Vec<EntityFact> = sqlx::query_as(&format!(
        "WITH {targets}
         SELECT f.id, {said_as} AS said_as, f.recorded_at, f.invalidated_at, f.supersedes,
                ARRAY(SELECT DISTINCT fe.document_id FROM fact_evidence fe
                      WHERE fe.fact_id = f.id AND fe.document_id IS NOT NULL
                      ORDER BY fe.document_id) AS document_ids,
                CASE WHEN {subject} = $2 THEN 'out' ELSE 'in' END AS direction,
                COALESCE(r.key, fact_surface_predicate(f.id)) AS predicate_key,
                COALESCE(r.label, fact_surface_predicate(f.id)) AS predicate_label,
                r.id IS NULL AS inferred, r.temporal,
                CASE WHEN {subject} = $2 THEN {object} ELSE {subject} END AS other_id,
                o.canonical_name AS other_name, ot.label AS other_type, f.object_value,
                f.valid_from, f.valid_from_precision, f.valid_to, f.valid_to_precision,
                {holds_from} AS holds_from, {holds_to} AS holds_to, f.attested_by, f.confidence,
                (SELECT count(*) FROM fact_evidence fe WHERE fe.fact_id = f.id) AS evidence_count,
                (EXISTS (SELECT 1 FROM fact_evidence fe WHERE fe.fact_id = f.id)
                 AND NOT EXISTS (SELECT 1 FROM fact_evidence fe
                                 JOIN chunks c ON c.id = fe.chunk_id
                                 WHERE fe.fact_id = f.id AND {chunk_live})
                 AND NOT {awaits} AND NOT {follows_its_statement}
                ) AS stale,
                (f.supersedes IS NOT NULL) AS corrected,
                f.end_derived, f.end_after_period, {corrected_ends} AS corrected_ends,
                {corrected_ends} IS NOT NULL AS time_corrected,
                closer.closed_by_id, closer.closed_by, closer.closed_by_value,
                CASE WHEN {corrected_ends} IS NOT NULL THEN {correction_note} END
                    AS correction_note,
                (SELECT MAX(COALESCE(d.doc_time, d.created_at))
                 FROM fact_evidence fe JOIN documents d ON d.id = fe.document_id
                 WHERE fe.fact_id = f.id) AS last_evidence_time,
                COALESCE(
                    (SELECT jsonb_build_object(
                                'kind', v.kind, 'ref_id', v.id,
                                'derived', CASE WHEN v.kind = 'derived_contradiction'
                                    THEN (v.detail->>'subject') || ' · '
                                         || (v.detail->>'predicate') || ' · '
                                         || (v.detail->>'object') END)
                       FROM axiom_violations v
                      WHERE {violation_open}
                        AND (v.left_fact = f.id
                             OR (v.right_fact = f.id AND v.kind <> 'derived_contradiction'))
                      ORDER BY v.detected_at DESC LIMIT 1),
                    (SELECT jsonb_build_object('kind', 'temporal_conflict', 'ref_id', c.id)
                       FROM fact_conflicts c
                      WHERE {conflict_open}
                        AND (c.old_fact_id = f.id OR c.new_fact_id = f.id)
                      ORDER BY c.created_at DESC LIMIT 1)
                ) AS contested
         FROM facts f
         LEFT JOIN relation_types r ON r.id = f.predicate_id
         LEFT JOIN entities o
           ON o.id = CASE WHEN {subject} = $2 THEN {object} ELSE {subject} END
         LEFT JOIN entity_types ot ON ot.id = o.type_id
         {closer}
         WHERE f.kb_id = $1 AND {facts_held} AND {facts_hold}
           AND NOT {represented}
           AND ({subject} = $2 OR {object} = $2)
           AND {not_name}
         ORDER BY f.valid_from NULLS LAST, f.recorded_at, f.id",
        targets = TIME_CORRECTED_TARGETS,
        corrected_ends = corrected_ends_sql("f"),
        correction_note = correction_note_sql("f"),
        closer = closed_by_join(
            &crate::record_axis::owner_at("f", "subject_id", as_of.map(|_| 3), false),
            &crate::record_axis::owner_at("f", "object_id", as_of.map(|_| 3), true),
            as_of.map(|_| 3),
        ),
        not_name = crate::names::not_a_name("f"),
        said_as = said_as("f"),
        represented = represented_by_typed("f"),
        facts_held = crate::record_axis::facts_held_at("f", as_of.map(|_| 3)),
        facts_hold = crate::world_axis::facts_hold_at("f", 4),
        holds_from = crate::world_axis::facts_holds_from("f"),
        holds_to = crate::world_axis::facts_holds_to("f"),
        subject = crate::record_axis::owner_at("f", "subject_id", as_of.map(|_| 3), false),
        object = crate::record_axis::owner_at("f", "object_id", as_of.map(|_| 3), true),
        awaits = match as_of {
            Some(_) => "false",
            None => crate::review::awaits_a_new_version(),
        },
        follows_its_statement = crate::review::FOLLOWS_ITS_STATEMENT,
        chunk_live = crate::record_axis::chunk_live_at("c", as_of.map(|_| 3)),
        violation_open = crate::record_axis::violation_open_at("v", as_of.map(|_| 3)),
        conflict_open = crate::record_axis::conflict_open_at("c", as_of.map(|_| 3)),
    ))
    .bind(kb_id)
    .bind(entity_id)
    .bind(as_of)
    .bind(at)
    .fetch_all(pool)
    .await?;
    // The other two tables for attributes on edges (0037 by ontology property, 0061 by role word) are fetched back in one go by id and filled in
    {
        let ids: Vec<Uuid> = facts.iter().map(|x| x.id).collect();
        let (mut typed, mut worded) = tokio::try_join!(
            fact_qualifiers_for(pool, &ids),
            statement_qualifiers_for(pool, &ids, as_of)
        )?;
        for x in facts.iter_mut() {
            x.qualifiers = typed.remove(&x.id).unwrap_or_default();
            x.statement_qualifiers = worded.remove(&x.id).unwrap_or_default();
        }
    }

    Ok((node, facts))
}

/// Manually corrects an entity's type or name. Returns (snapshot before the change, state after) — the caller records the audit ledger from this.
///
/// A wrong type judgement or a garbled name previously could only be fixed by re-extracting the whole KB, a sledgehammer. Extraction gives a first guess, not a verdict.
///
/// Same name is not blocked: two entities of the same class with the same name are a legitimate product of "decline to merge" (two Zhang Weis),
/// blocking would stop the second from being recorded. Collisions are found by the caller, who then suggests a merge; see `same_name_peers`.
pub async fn update_entity(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    type_id: Option<Uuid>,
    canonical_name: Option<&str>,
) -> AppResult<(GraphNode, GraphNode)> {
    let before: GraphNode = sqlx::query_as(&format!(
        "{} WHERE e.kb_id = $1 AND e.id = $2 AND e.merged_into IS NULL",
        node_sql(None, None)
    ))
    .bind(kb_id)
    .bind(entity_id)
    .fetch_optional(pool)
    .await?
    .ok_or(AppError::NotFound)?;

    let new_name = match canonical_name {
        Some(raw) => {
            let n = raw.trim();
            if n.is_empty() {
                return Err(AppError::invalid(
                    "entity_name_required",
                    "Name cannot be empty",
                ));
            }
            // The same cap as on the extraction side: anything crossing this line is most likely a whole sentence that was taken as a name
            if n.chars().count() > 100 {
                return Err(AppError::invalid(
                    "entity_name_too_long",
                    "Name is too long (max 100)",
                ));
            }
            Some(n)
        }
        None => None,
    };

    if let Some(t) = type_id {
        let exists: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM entity_types WHERE id = $1 AND kb_id = $2")
                .bind(t)
                .bind(kb_id)
                .fetch_optional(pool)
                .await?;
        if exists.is_none() {
            return Err(AppError::invalid(
                "unknown_entity_type",
                "No such entity type in this KB",
            ));
        }
    }

    sqlx::query(
        // The type is marked human only when it is changed — this endpoint is also used to rename, and renaming alone should not quietly stamp
        // the type's source as manual. `$3 IS NULL` in this interface means 'no type was supplied this time',
        // not 'clear the type': the routing layer requires at least one of the two fields, and cannot express the three states.
        //
        // So there is one thing we cannot do today: since 0009, 'no type' can be a person's decision
        //(looked at it, and the ontology has no suitable class), and this interface cannot express that. Adding it would need the request body to distinguish
        // 'not supplied' from 'explicitly cleared'; that is a separate piece of work
        "UPDATE entities
         SET type_id = COALESCE($3, type_id),
             canonical_name = COALESCE($4, canonical_name),
             type_source = CASE WHEN $3::uuid IS NULL THEN type_source ELSE 'human' END,
             updated_at = now()
         WHERE id = $1 AND kb_id = $2 AND merged_into IS NULL",
    )
    .bind(entity_id)
    .bind(kb_id)
    .bind(type_id)
    .bind(new_name)
    .execute(pool)
    .await?;

    // A name changed by a person is also a name fact (0041). The old name is not invalidated: earlier documents still call it the old name, and
    // recall relies on it to recognise them; it is simply no longer the one shown in the interface
    if let Some(n) = new_name {
        crate::names::record(pool, kb_id, entity_id, n, None, None).await?;
    }

    // The disambiguation suffix depends on the name grouping and the type label (the type label is its fallback value), and both have just changed.
    // Renaming must refresh two groups: the old name's group may drop to 1 (the suffix should be cleared), and the new name's group may rise to 2.
    if let Some(n) = new_name.filter(|n| !n.eq_ignore_ascii_case(&before.name)) {
        crate::resolution::refresh_disambiguators(pool, kb_id, &before.name).await?;
        crate::resolution::refresh_disambiguators(pool, kb_id, n).await?;
    } else if type_id.is_some() {
        crate::resolution::refresh_disambiguators(pool, kb_id, &before.name).await?;
    }

    let after: GraphNode = sqlx::query_as(&format!(
        "{} WHERE e.kb_id = $1 AND e.id = $2",
        node_sql(None, None)
    ))
    .bind(kb_id)
    .bind(entity_id)
    .fetch_one(pool)
    .await?;
    Ok((before, after))
}

/// Other live entities with the same name as the given entity (case-insensitive) — used to suggest "merge?" after a rename.
/// Reports only, does not block: whether they really are the same is for a person to judge.
/// The same-name column must follow the slider on the panel (0019 / #307).
///
/// With no time given, falls back to today: merged-away entities are not counted, and edges up to and including yesterday are all counted, consistent with the present.
/// With a time given: attach `entity_visible_at`, so a "Zhang Wei" merged away in March reappears in the same-name list in February —
/// and that is exactly what the panel wants to tell the person
pub async fn same_name_peers(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<GraphNode>> {
    let visible = crate::record_axis::entity_visible_at("e", as_of.map(|_| 3));
    sqlx::query_as(&format!(
        "{} WHERE e.kb_id = $1 AND {visible} AND e.id <> $2
           AND lower(e.canonical_name) = (SELECT lower(canonical_name) FROM entities WHERE id = $2)
         ORDER BY degree DESC LIMIT 10",
        // Degree also rewinds to who held the facts at the time: a merge moves facts onto the target, and filtering on the record axis alone, without rewinding subject and object,
        // would show 0 for the merged-away one even before the merge (inconsistent with the canvas and the panel)
        node_sql(as_of.map(|_| 3), as_of.map(|_| 3)),
    ))
    .bind(kb_id)
    .bind(entity_id)
    .bind(as_of)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// Low-confidence live facts (the review page).
pub async fn low_confidence_facts(
    pool: &PgPool,
    kb_id: Uuid,
    below: f32,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<FactReviewItem>> {
    let rows: Vec<FactReviewItem> = sqlx::query_as(
        "SELECT f.id, s.canonical_name AS subject_name, COALESCE(r.label, fact_surface_predicate(f.id)) AS predicate_label,
                COALESCE(o.canonical_name, f.object_value->>'summary') AS object_name,
                f.valid_from, f.valid_to, f.confidence,
                (SELECT count(*) FROM fact_evidence fe WHERE fe.fact_id = f.id) AS evidence_count,
                (SELECT fe.quote FROM fact_evidence fe
                 WHERE fe.fact_id = f.id AND fe.quote IS NOT NULL LIMIT 1) AS quote
         FROM facts f
         JOIN entities s ON s.id = f.subject_id
         LEFT JOIN relation_types r ON r.id = f.predicate_id
         LEFT JOIN entities o ON o.id = f.object_id
         WHERE f.kb_id = $1 AND f.invalidated_at IS NULL AND f.confidence < $2
         ORDER BY f.confidence, f.recorded_at DESC, f.id DESC
         LIMIT $3 OFFSET $4",
    )
    .bind(kb_id)
    .bind(below)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Current facts whose evidence has all stayed on the old version ("knowledge a newer document version no longer re-confirms", S3 third slice).
/// The check is purely derived from chunk liveness — the claim mechanism guarantees that evidence for unchanged passages is not wrongly harmed;
/// it never deletes automatically (not mentioned again ≠ no longer true): the right to delete or close lies with the person in Review.
///
/// **The "current" condition must not be dropped.** Closing is invalidate + rewrite, and the correction row carries the original evidence (`temporal::close_superseded`);
/// if only evidence liveness were checked, a just-closed row would immediately fall back into this bucket — that exit would then not exist.
/// The WHERE is `review::unconfirmed_fact` (0022: "ended on an unknown day" is not open; RC-005: typed rows are judged by their statements).
pub async fn stale_facts(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<FactReviewItem>> {
    let rows: Vec<FactReviewItem> = sqlx::query_as(&format!(
        "SELECT f.id, s.canonical_name AS subject_name, COALESCE(r.label, fact_surface_predicate(f.id)) AS predicate_label,
                COALESCE(o.canonical_name, f.object_value->>'summary') AS object_name,
                f.valid_from, f.valid_to, f.confidence,
                (SELECT count(*) FROM fact_evidence fe WHERE fe.fact_id = f.id) AS evidence_count,
                (SELECT fe.quote FROM fact_evidence fe
                 WHERE fe.fact_id = f.id AND fe.quote IS NOT NULL LIMIT 1) AS quote
         FROM facts f
         JOIN entities s ON s.id = f.subject_id
         LEFT JOIN relation_types r ON r.id = f.predicate_id
         LEFT JOIN entities o ON o.id = f.object_id
         WHERE f.kb_id = $1 AND f.invalidated_at IS NULL AND {unconfirmed}
         ORDER BY f.recorded_at DESC, f.id DESC
         LIMIT $2 OFFSET $3",
        unconfirmed = crate::review::unconfirmed_fact(),
    ))
    .bind(kb_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Manually confirms a low-confidence fact: raises its confidence to 1.0.
pub async fn confirm_fact(pool: &PgPool, kb_id: Uuid, fact_id: Uuid) -> AppResult<()> {
    let res = sqlx::query(
        "UPDATE facts SET confidence = 1.0 WHERE id = $1 AND kb_id = $2 AND invalidated_at IS NULL",
    )
    .bind(fact_id)
    .bind(kb_id)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    // There used to be a section here: when confirming a `mapped_to` fact, the old mapping with the same (concept, source) was invalidated.
    // Mappings have moved out of the ledger (0011; `concept_mappings` manages uniqueness itself), so that SQL matched zero rows always, and was deleted.
    Ok(())
}

/// Manually rejects a fact: invalidates it (the ledger is append-only, no DELETE).
///
/// When the rejected item is an open statement, a typed recomputation is queued afterwards (#1366): the typed rows it materialised leave only after recomputation
/// (steps 1 and 2 of `materialize` look at each supporter via `typed_fact_sources`, and a row still supported by other live statements stays),
/// whereas the rejection itself only writes `invalidated_at`; without queuing, this row would wait until the next reconciliation.
/// Invalidate first, then queue: the other way round, recomputation could finish before the invalidation commits and read the statement as still alive. If already queued, nothing is repeated.
/// A failed queue is only logged: the invalidation has already committed, and an error here would make a retry of this rejection return 404, while the next reconciliation would recompute anyway
pub async fn reject_fact(pool: &PgPool, kb_id: Uuid, fact_id: Uuid) -> AppResult<()> {
    if !crate::temporal::retract(pool, kb_id, fact_id).await? {
        return Err(AppError::NotFound);
    }
    if let Err(e) = queue_typed_recompute(pool, kb_id, fact_id).await {
        tracing::warn!(%kb_id, %fact_id, error = %e, "否决后的类型化重算没能入队");
    }
    Ok(())
}

/// When the rejected item is an open statement, queue a typed recomputation; typed rows, or facts that no longer exist, are not queued.
async fn queue_typed_recompute(pool: &PgPool, kb_id: Uuid, fact_id: Uuid) -> AppResult<()> {
    let open: Option<bool> = sqlx::query_scalar("SELECT layer = 'open' FROM facts WHERE id = $1")
        .bind(fact_id)
        .fetch_optional(pool)
        .await?;
    if open == Some(true) {
        crate::jobs::enqueue_unless_queued(
            pool,
            crate::phrase_bindings::MATERIALIZE_KIND,
            serde_json::json!({ "kb_id": kb_id }),
        )
        .await?;
    }
    Ok(())
}

/// Reverse evidence chain: which live facts each chunk of this document extracted (the right-hand panel of the document viewer).
pub async fn document_extractions(
    pool: &PgPool,
    document_id: Uuid,
) -> AppResult<Vec<ChunkFactView>> {
    let rows: Vec<ChunkFactView> = sqlx::query_as(
        "SELECT fe.chunk_id, f.id AS fact_id,
                f.subject_id, s.canonical_name AS subject,
                -- 开放陈述的短语在 facts 本身，不依赖旧证据的 proposed_predicate。
                CASE WHEN f.layer='open' THEN f.phrase
                     ELSE COALESCE(r.label, fact_surface_predicate(f.id)) END AS predicate,
                r.id IS NULL AS inferred,
                (f.layer='open' AND nullif(btrim(f.phrase),'') IS NOT NULL) AS is_open_statement,
                -- 宾语是一样东西就用它的名字，是字面值（名字、金额、百分比）就用那个值。
                -- 只取 canonical_name 的话，每一条值宾语的陈述在阅读页右栏都是主语加短语、
                -- 后面空着一片——「Hugging Face, Inc. known as」后面什么都没有
                f.object_id, COALESCE(o.canonical_name, f.object_value #>> '{value}') AS object,
                f.valid_from, f.valid_to, f.confidence
         FROM fact_evidence fe
         JOIN chunks c ON c.id = fe.chunk_id AND c.document_id = $1
              AND c.superseded_at IS NULL
         JOIN facts f ON f.id = fe.fact_id AND f.invalidated_at IS NULL
         JOIN entities s ON s.id = f.subject_id
         LEFT JOIN relation_types r ON r.id = f.predicate_id
         LEFT JOIN entities o ON o.id = f.object_id
         ORDER BY c.seq, f.recorded_at",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The first still-valid evidence chunk for each fact (#935). Chat registers it in this turn's source list and prints its `[n]` on the fact row;
/// the reader clicks it and gets the original sentence.
///
/// Valid = the chunk is the current version and the document has not been deleted; if `as_of` is given, judge at that moment, consistent with the moment the facts were read
/// (record_axis). If several are valid, take the first chunk of the earliest document that stated it. A fact with no valid evidence at all
/// is not in the results — better to give no number than one pointing at a chunk that was already an old version or from a deleted document at the moment of reading
/// (when `as_of` is given, the cited passage is the one still present at that moment, even if it was later replaced or deleted)
pub async fn first_live_evidence(
    pool: &PgPool,
    kb_id: Uuid,
    fact_ids: &[Uuid],
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<(Uuid, utopia_core::models::ChunkView, Option<String>)>> {
    if fact_ids.is_empty() {
        return Ok(Vec::new());
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        fact_id: Uuid,
        id: Uuid,
        document_id: Uuid,
        seq: i32,
        text: String,
        filename: String,
        quote: Option<String>,
    }
    // Fetched together with that evidence's quote (follow-up to #968): the number opens that chunk, and the quote is the sentence in the chunk that states this fact.
    // With several pieces of evidence in one chunk, take the earliest sentence in the chunk; only once the ordering stops here is it deterministic
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT DISTINCT ON (fe.fact_id) fe.fact_id, c.id, c.document_id, c.seq, c.text, d.filename,
                NULLIF(btrim(fe.quote), '') AS quote
           FROM fact_evidence fe
           JOIN chunks c ON c.id = fe.chunk_id
           JOIN documents d ON d.id = c.document_id
          WHERE fe.fact_id = ANY($1) AND d.kb_id = $2 AND {chunk_live} AND {document_live}
          ORDER BY fe.fact_id, COALESCE(d.doc_time, d.created_at), d.id, c.seq, c.id",
        chunk_live = crate::record_axis::chunk_live_at("c", as_of.map(|_| 3)),
        document_live = crate::record_axis::document_live_at("d", as_of.map(|_| 3)),
    ))
    .bind(fact_ids)
    .bind(kb_id)
    .bind(as_of)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.fact_id,
                utopia_core::models::ChunkView {
                    id: r.id,
                    document_id: r.document_id,
                    seq: r.seq,
                    text: r.text,
                    filename: r.filename,
                },
                r.quote,
            )
        })
        .collect())
}

/// Evidence replay path: does not filter out superseded — its job is precisely to show old versions.
/// stale = the evidence version lags the document's current version (the UI marks it "from v{n}").
/// NotFound when the fact is not in this KB: the route validated only the kb in the path, and the fact id is supplied by the caller.
pub async fn fact_evidence(
    pool: &PgPool,
    kb_id: Uuid,
    fact_id: Uuid,
) -> AppResult<Vec<EvidenceView>> {
    sqlx::query("SELECT 1 FROM facts WHERE id = $1 AND kb_id = $2")
        .bind(fact_id)
        .bind(kb_id)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound)?;
    let rows: Vec<EvidenceView> = sqlx::query_as(
        "SELECT fe.quote, fe.proposed_predicate, fe.chunk_id, c.document_id, d.filename, c.seq,
                c.doc_version,
                c.doc_version < COALESCE(
                    (SELECT MAX(version) FROM document_versions dv
                     WHERE dv.document_id = c.document_id), 1) AS stale,
                d.deleted_at IS NOT NULL AS document_deleted,
                c.origin, c.origin_model, c.anchor
         FROM fact_evidence fe
         JOIN chunks c ON c.id = fe.chunk_id
         JOIN documents d ON d.id = c.document_id
         WHERE fe.fact_id = $1",
    )
    .bind(fact_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Empties the whole graph layer of a KB (the settlement semantics of "Rebuild graph"): entities / facts / evidence / pending review / conflicts / merges
/// records are all deleted; the ontology (class and relation definitions) and documents / chunks / embeddings are kept.
///
/// Two things are deliberately kept: the decision ledger (audit_events, snapshots are self-contained, so records remain readable after the graph is gone) and the verdict
/// cache (resolution_verdicts, after a rebuild a same-name pair reappears and hits directly, saving a batch of LLM calls).
/// Returns (number of entities deleted, number of facts deleted).
pub async fn purge_graph(pool: &PgPool, kb_id: Uuid) -> AppResult<(i64, i64)> {
    let mut tx = pool.begin().await?;
    let (entity_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM entities WHERE kb_id = $1")
        .bind(kb_id)
        .fetch_one(&mut *tx)
        .await?;
    let (fact_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM facts WHERE kb_id = $1")
        .bind(kb_id)
        .fetch_one(&mut *tx)
        .await?;

    // Most FKs are CASCADE, but two self-references are NO ACTION: dereference first, then delete; the order is written out explicitly
    // (this block is itself the definition of "what the graph layer consists of")
    for sql in [
        "DELETE FROM fact_conflicts WHERE kb_id = $1",
        "DELETE FROM resolution_reviews WHERE kb_id = $1",
        "DELETE FROM entity_merges WHERE kb_id = $1",
        "UPDATE facts SET supersedes = NULL WHERE kb_id = $1",
        "DELETE FROM fact_evidence WHERE fact_id IN (SELECT id FROM facts WHERE kb_id = $1)",
        "DELETE FROM facts WHERE kb_id = $1",
        "UPDATE entities SET merged_into = NULL WHERE kb_id = $1",
        "DELETE FROM entities WHERE kb_id = $1",
        // Unmatched statistics are re-accumulated by extraction
        "DELETE FROM ontology_misses WHERE kb_id = $1",
    ] {
        sqlx::query(sql).bind(kb_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok((entity_count, fact_count))
}

/// Cognitive change history of an entity (record time axis).
///
/// The fundamental difference from entity_detail: there `invalidated_at IS NULL`, answering only "what is believed now";
/// here nothing is filtered, answering "when did we believe this, and when did we change our mind". The data has always been there — the ledger is
/// append-only; a correction inserts a new row and invalidates the old row, never overwriting.
///
/// One fact row produces at most two events: the write (asserted / corrected) and the invalidation (rejected).
/// An invalidation that has a successor correction row is not recorded separately — that death is already explained by the successor's corrected event.
///
/// Attribution: in the audit ledger, the target of fact.close is the **old row that was closed**, while the correction row is a different, newly inserted row,
/// so the lookup is by COALESCE(supersedes, id); a conflict verdict's target is a conflict row, which takes one more hop.
/// If no audit record is found, the engine did it automatically (extraction write or temporal reconciliation), and actor is NULL.
pub async fn entity_history(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<EntityHistoryEvent>, i64)> {
    const EVENTS: &str = "
        WITH ef AS (
            SELECT f.*,
                   CASE WHEN f.subject_id = $2 THEN 'out' ELSE 'in' END AS direction,
                   CASE WHEN f.subject_id = $2 THEN f.object_id ELSE f.subject_id END AS other_id
            FROM facts f
            WHERE f.kb_id = $1 AND (f.subject_id = $2 OR f.object_id = $2)
        ),
        ev AS (
            SELECT ef.*, ef.recorded_at AS at,
                   CASE WHEN ef.supersedes IS NULL THEN 'asserted' ELSE 'corrected' END AS kind
            FROM ef
            UNION ALL
            -- 作废且无后继 = 被推翻……除非它是被并进了另一条断言。那种情形下
            -- 内容一字未少，说成「撤回」就是界面在陈述一件没发生的事
            SELECT ef.*, ef.invalidated_at AS at,
                   CASE WHEN EXISTS (SELECT 1 FROM fact_adoptions fa
                                     WHERE fa.old_fact_id = ef.id AND fa.mode = 'merged'
                                       AND fa.reverted_at IS NULL)
                        THEN 'merged' ELSE 'rejected' END AS kind
            FROM ef
            WHERE ef.invalidated_at IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM facts s WHERE s.kb_id = $1 AND s.supersedes = ef.id)
        ),
        -- 改类不是事实：没有谓词、没有对方、没有方向。它来自 entity_retypes，
        -- 一行最多产出两个事件——改动本身，以及撤销。
        --
        -- **撤销过的照样显示。** 读成「改过、又撤了」，不是没发生过。同一类错
        -- 这仓库栽过两次（#37；并入被读成撤回），所以这不是防御性编程
        rt AS (
            SELECT r.created_at AS at, 'retyped' AS kind, r.actor_id,
                   tf.label AS from_type_label, tt.label AS to_type_label
            FROM entity_retypes r
            LEFT JOIN entity_types tf ON tf.id = r.from_type_id
            JOIN entity_types tt ON tt.id = r.to_type_id
            WHERE r.kb_id = $1 AND r.entity_id = $2
            UNION ALL
            SELECT r.reverted_at, 'retype_reverted', r.actor_id, tf.label, tt.label
            FROM entity_retypes r
            LEFT JOIN entity_types tf ON tf.id = r.from_type_id
            JOIN entity_types tt ON tt.id = r.to_type_id
            WHERE r.kb_id = $1 AND r.entity_id = $2 AND r.reverted_at IS NOT NULL
        ),
        /* A merge is also a change of understanding about this entity, and the largest one: from then on it and another
entity are one and the same thing. **Previously this axis could not see it** — it only saw the facts that the merge invalidated along the way
(`merged`), so the interface reports the result, not what happened. Since #337
`entity_merges` has its own clock (created_at / reverted_at), which can simply be wired in.

The two directions are recorded separately: `merged_in` = someone else merged into it (facts moved to its name),
`merged_away` = it merged into someone else (this id no longer exists on its own from then on). Saying both in one breath
would make "who absorbed whom" unreadable, and rollback works by direction. */
        mg AS (
            SELECT m.created_at AS at,
                   CASE WHEN m.target_id = $2 THEN 'merged_in' ELSE 'merged_away' END AS kind,
                   m.merged_by AS actor_id,
                   CASE WHEN m.target_id = $2 THEN m.source_id ELSE m.target_id END AS other_id
            FROM entity_merges m
            WHERE m.kb_id = $1 AND (m.source_id = $2 OR m.target_id = $2)
            UNION ALL
            -- 撤销的归因**不能借合并那一行的 merged_by**：撤的人常常不是当初合的人，
            -- 而自动合并那一行本来就是 NULL。撤销写审计（`merge.revert`，target 是
            -- 这次合并），从那儿取才对得上人
            SELECT m.reverted_at, 'merge_reverted',
                   (SELECT a.actor_id FROM audit_events a
                     WHERE a.kb_id = $1 AND a.action = 'merge.revert'
                       AND a.target_id = m.id
                     ORDER BY a.created_at DESC LIMIT 1),
                   CASE WHEN m.target_id = $2 THEN m.source_id ELSE m.target_id END
            FROM entity_merges m
            WHERE m.kb_id = $1 AND (m.source_id = $2 OR m.target_id = $2)
              AND m.reverted_at IS NOT NULL
        )";
    let rows: Vec<EntityHistoryEvent> = sqlx::query_as(&format!(
        "{EVENTS}
         SELECT * FROM (
         SELECT ev.id AS fact_id, ev.at, ev.kind, ev.direction,
                COALESCE(r.label, fact_surface_predicate(ev.id)) AS predicate_label, o.canonical_name AS other_name,
                ev.object_value, ev.valid_from, ev.valid_from_precision,
                ev.valid_to, ev.valid_to_precision,
                ev.confidence, act.actor_name, act.action,
                src.document_id, src.filename, src.quote,
                NULL::text AS from_type_label, NULL::text AS to_type_label
         FROM ev
         LEFT JOIN relation_types r ON r.id = ev.predicate_id
         LEFT JOIN entities o ON o.id = ev.other_id
         LEFT JOIN LATERAL (
             SELECT u.display_name AS actor_name, a.action
             FROM audit_events a
             LEFT JOIN users u ON u.id = a.actor_id
             WHERE a.kb_id = $1
               -- 断言由抽取写入，从来不是人的决定：归因只问修正与推翻这两类事件，
               -- 否则后发生的人工裁决会被错安到当初那条断言头上
               AND ev.kind <> 'asserted'
               AND a.action = ANY(CASE ev.kind
                     WHEN 'corrected' THEN
                       ARRAY['fact.close', 'conflict.close_old', 'ontology.predicate_adopted',
                             -- 人工改区间（302）。少了这一条，人做的修正在
                             -- 这条轴上归给「engine」——一个决策账本把人的
                             -- 决定记成机器的，比不记还坏
                             'fact.time_corrected', 'conflict.retime_new']
                     -- 并入只可能由采纳造成，不会是 Review 里的拒绝
                     WHEN 'merged' THEN ARRAY['ontology.predicate_adopted']
                     ELSE ARRAY['fact.reject', 'conflict.reject_new',
                                'ontology.adoption_reverted'] END)
               AND (a.target_id = COALESCE(ev.supersedes, ev.id)
                    OR (a.action <> 'conflict.retime_new'
                        AND a.target_id IN (SELECT c.id FROM fact_conflicts c
                                            WHERE c.old_fact_id = COALESCE(ev.supersedes, ev.id)
                                               OR c.new_fact_id = ev.id))
                    -- 重定时改的是冲突里的新事实，修正行替的是它，上面那一跳对不上；
                    -- 审计里记着它写下的那一行，直接认。旧事实若因此被对账重画，
                    -- 那是引擎做的，不该算到这次决定头上
                    OR (a.action = 'conflict.retime_new'
                        AND a.detail->>'corrected_fact_id' = ev.id::text)
                    -- 采纳与撤销都记在关系类型上、一次动作改一批事实，
                    -- 靠 fact_adoptions 精确关联到具体哪几条（corrected 事件
                    -- 是新行、merged 是旧行，两头都认）
                    OR (a.action IN ('ontology.predicate_adopted',
                                     'ontology.adoption_reverted')
                        AND EXISTS (SELECT 1 FROM fact_adoptions fa
                                    WHERE fa.predicate_id = a.target_id
                                      AND (fa.new_fact_id = ev.id
                                           OR fa.old_fact_id = ev.id))))
             ORDER BY a.created_at DESC LIMIT 1
         ) act ON true
         LEFT JOIN LATERAL (
             SELECT d.id AS document_id, d.filename, fe.quote
             FROM fact_evidence fe
             JOIN chunks c ON c.id = fe.chunk_id
             JOIN documents d ON d.id = c.document_id
             WHERE fe.fact_id = ev.id
             ORDER BY fe.doc_version DESC NULLS LAST LIMIT 1
         ) src ON true
         UNION ALL
         SELECT NULL::uuid, rt.at, rt.kind, NULL::text,
                NULL::text, NULL::text,
                NULL::jsonb, NULL::timestamptz, NULL::text,
                NULL::timestamptz, NULL::text,
                NULL::real, u.display_name, NULL::text,
                NULL::uuid, NULL::text, NULL::text,
                rt.from_type_label, rt.to_type_label
         FROM rt LEFT JOIN users u ON u.id = rt.actor_id
         UNION ALL
         -- 合并事件：没有谓词、没有区间、没有证据行，对方是另一个实体。
         -- 被合并掉的那一头仍留在 entities 里（revert_merge 要按原路搬回去），
         -- 所以这里拿得到名字；LEFT JOIN 只是防库被清理过
         SELECT NULL::uuid, mg.at, mg.kind, NULL::text,
                NULL::text, o.canonical_name,
                NULL::jsonb, NULL::timestamptz, NULL::text,
                NULL::timestamptz, NULL::text,
                NULL::real, u.display_name, NULL::text,
                NULL::uuid, NULL::text, NULL::text,
                NULL::text, NULL::text
         FROM mg
         LEFT JOIN entities o ON o.id = mg.other_id
         LEFT JOIN users u ON u.id = mg.actor_id
         ) x
         -- 改类型与合并那两支没有 fact_id，同一刻的几行只靠它排不出先后（#646）：
         -- 再按种类，最后按整行——两行连整行都一样，谁先谁后看不出差别
         ORDER BY x.at DESC, x.fact_id, x.kind, x::text
         LIMIT $3 OFFSET $4"
    ))
    .bind(kb_id)
    .bind(entity_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    // The total includes the reclassification and merge branches too, otherwise pagination would be short by a chunk
    let (total,): (i64,) = sqlx::query_as(&format!(
        "{EVENTS} SELECT (SELECT count(*) FROM ev) + (SELECT count(*) FROM rt)                        + (SELECT count(*) FROM mg)"
    ))
    .bind(kb_id)
    .bind(entity_id)
    .fetch_one(pool)
    .await?;
    Ok((rows, total))
}

/// Cognitive changes across the whole KB within a recorded-time window.
///
/// **The window opens on the epistemic axis**: `since`/`until` compare recorded_at and invalidated_at,
/// not valid_from/valid_to. This is the only and entire difference from `entity_facts(at)` —
/// that one asks "what the world was like at some moment", this one asks "what we changed our minds about over some period". Both query the same table,
/// using different columns; mixing them up silently produces a wrong answer that looks plausible.
///
/// Event derivation has the same origin as `entity_history` (see the comments there): one fact row produces at most two events,
/// and a death already superseded by a correction is not recorded twice.
#[allow(clippy::too_many_arguments)]
pub async fn graph_changes(
    pool: &PgPool,
    kb_id: Uuid,
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
    entity_id: Option<Uuid>,
    kinds: Option<&[String]>,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<GraphChange>> {
    // Each of the two branches opens its window on **its own time column**, rather than union-then-filter:
    // a fact written in February and overturned in August should have neither event appear in the "March–April" window
    const EVENTS: &str = "
        WITH ev AS (
            SELECT f.id, f.subject_id, f.predicate_id, f.object_id, f.object_value, f.supersedes,
                   f.valid_from, f.valid_from_precision, f.valid_to, f.valid_to_precision, f.confidence,
                   f.recorded_at AS at,
                   CASE WHEN f.supersedes IS NULL THEN 'asserted' ELSE 'corrected' END AS kind
            FROM facts f
            WHERE f.kb_id = $1 AND f.recorded_at >= $2 AND f.recorded_at < $3
              AND ($4::uuid IS NULL OR f.subject_id = $4 OR f.object_id = $4)
            UNION ALL
            SELECT f.id, f.subject_id, f.predicate_id, f.object_id, f.object_value, f.supersedes,
                   f.valid_from, f.valid_from_precision, f.valid_to, f.valid_to_precision, f.confidence,
                   f.invalidated_at AS at,
                   CASE WHEN EXISTS (SELECT 1 FROM fact_adoptions fa
                                     WHERE fa.old_fact_id = f.id AND fa.mode = 'merged'
                                       AND fa.reverted_at IS NULL)
                        THEN 'merged' ELSE 'rejected' END AS kind
            FROM facts f
            WHERE f.kb_id = $1 AND f.invalidated_at >= $2 AND f.invalidated_at < $3
              AND NOT EXISTS (SELECT 1 FROM facts s WHERE s.kb_id = $1 AND s.supersedes = f.id)
              AND ($4::uuid IS NULL OR f.subject_id = $4 OR f.object_id = $4)
        )";
    Ok(sqlx::query_as(&format!(
        "{EVENTS}
         SELECT ev.id AS fact_id, ev.at, ev.kind,
                ev.subject_id, s.canonical_name AS subject_name,
                COALESCE(r.label, fact_surface_predicate(ev.id)) AS predicate_label, o.canonical_name AS object_name,
                ev.object_value, ev.valid_from, ev.valid_from_precision,
                ev.valid_to, ev.valid_to_precision,
                ev.confidence, src.document_id, src.filename, src.quote, src.quote_origin,
                po.canonical_name AS previous_object_name, previous.object_value AS previous_object_value
         FROM ev
         LEFT JOIN relation_types r ON r.id = ev.predicate_id
         JOIN entities s ON s.id = ev.subject_id
         LEFT JOIN entities o ON o.id = ev.object_id
         -- 前一次断言可能不在本次窗口里，不能靠相邻事件猜订正方向（#1348）。
         LEFT JOIN facts previous ON ev.kind = 'corrected' AND previous.id = ev.supersedes
                                    AND previous.kb_id = $1
         LEFT JOIN entities po ON po.id = previous.object_id
         LEFT JOIN LATERAL (
             SELECT d.id AS document_id, d.filename, fe.quote, c.origin AS quote_origin
             FROM fact_evidence fe
             JOIN chunks c ON c.id = fe.chunk_id
             JOIN documents d ON d.id = c.document_id
             WHERE fe.fact_id = ev.id
             ORDER BY fe.doc_version DESC NULLS LAST LIMIT 1
         ) src ON true
         WHERE ($5::text[] IS NULL OR ev.kind = ANY($5))
           -- 实体的本名那条名字事实不算一次变化（0041）：每建一个实体就多一行「X known as X」，
           -- 限量的变更清单会被它挤满。新读到的别名、改名照样列出来
           AND NOT (coalesce(r.builtin AND r.key = 'known_as', false)
                    AND lower(ev.object_value->>'value') = lower(s.canonical_name))
         -- 同一事实可以在同一时刻写入并作废；事件种类也得参与排序才能稳定续页。
         ORDER BY ev.at DESC, ev.id, ev.kind
         LIMIT $6 OFFSET $7"
    ))
    .bind(kb_id)
    .bind(since)
    .bind(until)
    .bind(entity_id)
    .bind(kinds)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

#[cfg(test)]
mod temporal_shape_tests {
    use super::*;

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        s.parse().unwrap()
    }

    /// The far end of a bucket is one precision unit added; without precision (anchors) it is returned as-is
    #[test]
    fn a_bucket_ends_one_unit_later() {
        let d = at("2024-03-15T00:00:00Z");
        assert_eq!(bucket_end(d, Some("day")), at("2024-03-16T00:00:00Z"));
        assert_eq!(
            bucket_end(at("2024-03-01T00:00:00Z"), Some("month")),
            at("2024-04-01T00:00:00Z")
        );
        assert_eq!(
            bucket_end(at("2024-12-01T00:00:00Z"), Some("month")),
            at("2025-01-01T00:00:00Z"),
            "跨年"
        );
        assert_eq!(
            bucket_end(at("2024-01-01T00:00:00Z"), Some("year")),
            at("2025-01-01T00:00:00Z")
        );
        assert_eq!(
            bucket_end(at("2024-03-15T14:32:00Z"), Some("minute")),
            at("2024-03-15T14:33:00Z")
        );
        assert_eq!(bucket_end(d, None), d);
    }

    /// Event: the start takes priority; only if there is no start does the end take the end; a span takes the start; "ended on an unknown day" is stripped
    #[test]
    fn an_event_collapses_to_one_moment() {
        let span = Validity {
            from: Some(at("2024-03-15T00:00:00Z")),
            from_precision: Some("day"),
            to: Some(at("2025-01-01T00:00:00Z")),
            to_precision: Some("day"),
            attested_at: None,
            from_grade: None,
            end_after_period: false,
        }
        .under(Temporal::Event)
        .unwrap();
        assert_eq!(span.from, Some(at("2024-03-15T00:00:00Z")));
        assert_eq!(span.to, Some(at("2024-03-15T00:00:00Z")));
        assert_eq!(
            (span.from_precision, span.to_precision),
            (Some("day"), Some("day"))
        );

        let end_only = Validity {
            from: None,
            from_precision: None,
            to: Some(at("2024-05-01T00:00:00Z")),
            to_precision: Some("month"),
            attested_at: None,
            from_grade: None,
            end_after_period: false,
        }
        .under(Temporal::Event)
        .unwrap();
        assert_eq!(end_only.from, Some(at("2024-05-01T00:00:00Z")));
        assert_eq!(end_only.from_precision, Some("month"));

        let unknown = Validity::default()
            .ended_when_unknown()
            .under(Temporal::Event)
            .unwrap();
        assert_eq!(
            (unknown.from, unknown.to, unknown.to_precision),
            (None, None, None)
        );
        assert!(!unknown.has_ended(), "一刻没有「结束」可言");
    }

    /// Eternal: dates are stripped; state as-is
    #[test]
    fn an_eternal_fact_keeps_no_dates_and_a_state_keeps_its_own() {
        let dated = Validity::starting(Some(at("1990-01-01T00:00:00Z")), Some("year"))
            .attested(Some(at("2024-04-01T00:00:00Z")));
        let eternal = dated.under(Temporal::Eternal).unwrap();
        assert_eq!((eternal.from, eternal.from_precision), (None, None));
        assert_eq!(
            eternal.attested_at,
            Some(at("2024-04-01T00:00:00Z")),
            "证据日期照记——读出侧不用它，账本仍知道"
        );
        let state = dated.under(Temporal::State).unwrap();
        assert_eq!(state.from, Some(at("1990-01-01T00:00:00Z")));
    }

    /// A state whose two ends are equal is rejected (#966): a span with equal start and end holds at no moment. An event is still written as a single moment;
    /// two ends that become equal only after truncation to precision are rejected too
    #[test]
    fn a_state_that_ends_where_it_starts_is_refused() {
        let moment = Validity {
            from: Some(at("2023-06-01T00:00:00Z")),
            from_precision: Some("day"),
            to: Some(at("2023-06-01T00:00:00Z")),
            to_precision: Some("day"),
            attested_at: None,
            from_grade: None,
            end_after_period: false,
        };
        let refused = moment
            .under(Temporal::State)
            .expect_err("an empty state span");
        assert!(
            format!("{refused:?}").contains("empty_state_span"),
            "{refused:?}"
        );
        assert!(moment.under(Temporal::Event).is_ok());
        let same_day = Validity {
            from: Some(at("2023-06-01T09:00:00Z")),
            to: Some(at("2023-06-01T17:00:00Z")),
            ..moment
        };
        assert!(same_day.truncated().under(Temporal::State).is_err());
        assert!(Validity {
            to: None,
            to_precision: None,
            ..moment
        }
        .under(Temporal::State)
        .is_ok());
    }
}
