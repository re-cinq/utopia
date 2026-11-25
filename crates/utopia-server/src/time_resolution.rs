//! Time mentions are resolved per document (0045 decisions 2-5): the model reads, the code computes.
//!
//! Extraction records only time **words** (`time_mentions`: the verbatim text, chunk, offsets, start and end). This is the task that comes after it:
//!
//! 1. **Document time context** (decision 3; since 0064 each chunk reports it during extraction, so no model is asked here). Previously the opening was handed to the model, which transcribed the document's own dates
//!    (signature date, filing date, the reporting year of a bulletin) and the periods the document names ("fiscal 2027" and its start and end); a date word counts only if it can be checked against the opening.
//!    The result is stored in `documents.time_context`; the document's own date goes into `doc_time`
//!    (source `content`). The upload time never enters (#714).
//! 2. **Interpretation** (decision 2). Each time word in the document, with the sentence it sits in, is handed to the model along with the context; the model
//!    returns only the shape, the reference (the stated numeric part / an anchor plus offset / a named period / none) and the granularity, and **never computes a date**.
//!    The interpretation is stored back on the mention as is; a wrong one is visible and can be changed.
//! 3. **Compute**. Calendar arithmetic, period-to-interval, truncation by granularity, and intervals implied by the shape are all computed here from structured fields,
//!    with no time word written in the code (decision 8). A stated date is grade A; one computed from an anchor the document gives itself is
//!    grade B; one that cannot be anchored is grade C -- grade C writes nothing and waits for an anchor (decisions 4, 6).
//! 4. The result lands on the open statement's `valid_from` / `valid_to` (the first axis of decision 5), which is what puts it on the timeline.
//!
//! The exception appears only on pushed statements (0054 source): when `when` / `ended` hold a full date or a time with a zone,
//! the payload is already a structured absolute value -- a structured timestamp is not a time word (0045 decision 8, #1089), so the code
//! reads it directly as the interpretation without sending it to a model; without a chat model, what the code can read is interpreted, what it cannot read stays C, and the task finishes as usual.

use crate::extraction::chat_retrying_rate_limits_at;
use crate::llm_util;
use crate::state::AppState;
use chrono::{DateTime, Duration, Months, NaiveDate, NaiveTime, TimeZone, Utc};
use std::collections::{HashMap, HashSet};
use utopia_extract::time::{
    build_interpretation_messages, headings_at, now_in_force, parse_interpretation_response,
    Anchor, DateParts, Direction, DocumentDating, Granularity, Interpretation, MentionInput,
    NamedPeriod, Offset, Reference, Shape, TimeContext, Unit,
};
use utopia_store::graph::{truncate_to, ENDED_UNKNOWN, WORLD_PRECISIONS};
use uuid::Uuid;

/// How many mentions one batch of interpretation asks about.
const BATCH: usize = 40;

/// The result of one parse: the position on the world axis and its grade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Resolved {
    from: Option<DateTime<Utc>>,
    from_p: Option<&'static str>,
    to: Option<DateTime<Utc>>,
    to_p: Option<&'static str>,
    /// A: stated; B: computed from the document's own anchors; C: cannot be anchored
    grade: &'static str,
}

const UNRESOLVED: Resolved = Resolved {
    from: None,
    from_p: None,
    to: None,
    to_p: None,
    grade: "C",
};

/// Numeric part -> moment and precision. Precision is the level the word actually reaches: a word giving only the year is year precision.
fn parts_to_time(p: &DateParts) -> Option<(DateTime<Utc>, &'static str)> {
    // A stated quarter starts from its first month (calendar quarter; fiscal quarters go through named periods)
    let month = p.month.or(p.quarter.map(|q| (q - 1) * 3 + 1));
    let p = &DateParts { month, ..p.clone() };
    let date = NaiveDate::from_ymd_opt(p.year, p.month.unwrap_or(1), p.day.unwrap_or(1))?;
    let time = NaiveTime::from_hms_opt(
        p.hour.unwrap_or(0),
        p.minute.unwrap_or(0),
        p.second.unwrap_or(0),
    )?;
    let precision = if p.month.is_none() {
        "year"
    } else if p.day.is_none() {
        "month"
    } else if p.hour.is_none() {
        "day"
    } else if p.minute.is_none() {
        "hour"
    } else if p.second.is_none() {
        "minute"
    } else {
        "second"
    };
    Some((Utc.from_utc_datetime(&date.and_time(time)), precision))
}

/// The interval's end. A quarter written alone ends at its last month (July to September); on the end side of "to the third quarter"
/// it is also the last month, and it must not stop at the quarter's first month.
fn parts_to_end(p: &DateParts) -> Option<(DateTime<Utc>, &'static str)> {
    let (t, precision) = parts_to_time(p)?;
    if p.quarter.is_some() {
        Some((t.checked_add_months(Months::new(2))?, "month"))
    } else {
        Some((t, precision))
    }
}

fn granularity_str(g: Granularity) -> &'static str {
    match g {
        Granularity::Year => "year",
        Granularity::Month => "month",
        Granularity::Day => "day",
        Granularity::Hour => "hour",
        Granularity::Minute => "minute",
        Granularity::Second => "second",
    }
}

/// Of two precision levels, take the coarser: the word only reaches the year, so even if the model says "day" it can only be year -- precision cannot be finer than the word
fn coarser(a: &'static str, b: &'static str) -> &'static str {
    let rank = |p: &str| WORLD_PRECISIONS.iter().position(|x| *x == p).unwrap_or(0);
    if rank(a) <= rank(b) {
        a
    } else {
        b
    }
}

/// Anchor plus offset (decision 2's arithmetic). Months, quarters and years add and subtract by month (end-of-month clamps automatically); weeks and finer by duration.
fn shift(t: DateTime<Utc>, offset: &Offset) -> Option<DateTime<Utc>> {
    let count = offset.count.unsigned_abs();
    let forward = (offset.direction == Direction::After) == (offset.count >= 0);
    let months = |n: u64| -> Option<DateTime<Utc>> {
        let m = Months::new(u32::try_from(n).ok()?);
        if forward {
            t.checked_add_months(m)
        } else {
            t.checked_sub_months(m)
        }
    };
    let span = |d: Duration| -> Option<DateTime<Utc>> {
        if forward {
            t.checked_add_signed(d)
        } else {
            t.checked_sub_signed(d)
        }
    };
    match offset.unit {
        Unit::Year => months(count.checked_mul(12)?),
        Unit::Quarter => months(count.checked_mul(3)?),
        Unit::Month => months(count),
        Unit::Week => span(Duration::weeks(i64::try_from(count).ok()?)),
        Unit::Day => span(Duration::days(i64::try_from(count).ok()?)),
        Unit::Hour => span(Duration::hours(i64::try_from(count).ok()?)),
        Unit::Minute => span(Duration::minutes(i64::try_from(count).ok()?)),
        Unit::Second => span(Duration::seconds(i64::try_from(count).ok()?)),
    }
}

/// A moment and its precision (one end of the world axis).
type Bound = (DateTime<Utc>, &'static str);

/// A quarter: the three months containing `t` (by fiscal quarter if a fiscal year-end date is given, otherwise by calendar quarter),
/// the start is its first month, the end is its last month (end precision to the month). "This quarter" and "last quarter" both come from here
fn quarter_around(t: DateTime<Utc>, fiscal_year_end: Option<(u32, u32)>) -> Option<(Bound, Bound)> {
    use chrono::Datelike;
    // A fiscal year counts from the month after its end date: a fiscal year ending on January 25 has quarter-start months 2, 5, 8, 11
    let first_month = fiscal_year_end.map(|(m, _)| m % 12 + 1).unwrap_or(1);
    let month0 = (t.month() as i32 - first_month as i32).rem_euclid(12); // month index within the fiscal year (0-based)
    let start_month0 = month0 - month0 % 3;
    let start = t.checked_sub_months(Months::new(u32::try_from(month0 - start_month0).ok()?))?;
    let start = truncate_to(start, Some("month"));
    let end = start.checked_add_months(Months::new(2))?;
    Some(((start, "month"), (end, "month")))
}

/// The start and end of a period (the stated numeric part). A quarter written at the end is that quarter's last month:
/// "first quarter to third quarter" ends in September, as in [`parts_to_end`].
fn period_bounds(from: &DateParts, to: &DateParts) -> Option<(Bound, Bound)> {
    Some((parts_to_time(from)?, parts_to_end(to)?))
}

/// The shape determines which segment of the world axis the word occupies (decision 7: shape is stored separately; here only the interval is computed).
/// Point: one instant, both ends the same value (the event style of 0031); from: the start; to: the end; interval: both ends;
/// "until": the observed start; duration: no position; ended but not known when: end precision unknown
fn place(
    shape: Shape,
    at: Option<(DateTime<Utc>, &'static str)>,
    until: Option<(DateTime<Utc>, &'static str)>,
    grade: &'static str,
) -> Resolved {
    let trunc = |(t, p): (DateTime<Utc>, &'static str)| (truncate_to(t, Some(p)), p);
    let at = at.map(trunc);
    let until = until.map(trunc);
    let mut r = Resolved {
        grade,
        ..UNRESOLVED
    };
    match shape {
        Shape::Point => {
            if let Some((t, p)) = at {
                r.from = Some(t);
                r.from_p = Some(p);
                r.to = Some(t);
                r.to_p = Some(p);
            }
        }
        Shape::Since | Shape::AsOf => {
            if let Some((t, p)) = at {
                r.from = Some(t);
                r.from_p = Some(p);
            }
        }
        Shape::Until => {
            if let Some((t, p)) = until.or(at) {
                r.to = Some(t);
                r.to_p = Some(p);
            }
        }
        Shape::Interval => {
            if let Some((t, p)) = at {
                r.from = Some(t);
                r.from_p = Some(p);
            }
            if let Some((t, p)) = until {
                r.to = Some(t);
                r.to_p = Some(p);
            }
        }
        Shape::Duration => return UNRESOLVED,
        Shape::EndedUnknown => {
            r.to_p = Some(ENDED_UNKNOWN);
        }
    }
    if r.from.is_none() && r.to.is_none() && r.to_p.is_none() {
        return UNRESOLVED;
    }
    r
}

/// The position of an anchor on the world axis: a document date, another mention, or a named period.
fn anchor_time(
    anchor: &Anchor,
    ctx: &DocumentDating,
    earlier: &HashMap<i64, Resolved>,
) -> Option<(Bound, Option<Bound>)> {
    match anchor {
        Anchor::Document => ctx.date.as_ref().and_then(parts_to_time).map(|t| (t, None)),
        Anchor::Mention { id } => {
            let r = earlier.get(id)?;
            let from = r.from.zip(r.from_p)?;
            Some((from, r.to.zip(r.to_p)))
        }
        Anchor::Period { name } => {
            let p = ctx
                .periods
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(name))?;
            let (from, to) = period_bounds(&p.from, &p.to)?;
            Some((from, Some(to)))
        }
    }
}

/// One interpretation -> position on the world axis. `earlier` are the mentions already computed in the same document (anchors point at them).
fn resolve_one(
    interp: &Interpretation,
    ctx: &DocumentDating,
    earlier: &HashMap<i64, Resolved>,
) -> Resolved {
    let g = granularity_str(interp.granularity);
    // "Ended, not known which day" is stated by the word itself (that tier of #393): there is no date to compute, but the ending itself is grade A
    if interp.shape == Shape::EndedUnknown {
        return place(Shape::EndedUnknown, None, None, "A");
    }
    match &interp.reference {
        Reference::Absolute { from, to } => {
            let Some((t, p)) = parts_to_time(from) else {
                return UNRESOLVED;
            };
            let until = to.as_ref().and_then(parts_to_end);
            // A quarter is an interval of three months: it runs to its last month
            if let (Some(_), None) = (from.quarter, &until) {
                let last = t.checked_add_months(Months::new(2)).map(|m| (m, "month"));
                let shape = match interp.shape {
                    Shape::Point | Shape::Duration => Shape::Interval,
                    s => s,
                };
                return place(shape, Some((t, "month")), last, "A");
            }
            place(interp.shape, Some((t, coarser(p, g))), until, "A")
        }
        Reference::Anchored { anchor, offset } => {
            let Some(((t, p), until)) = anchor_time(anchor, ctx, earlier) else {
                return UNRESOLVED;
            };
            // "until" / "to" said against a period ("year-end" against the reporting year): it speaks of the end of the period, not its start.
            // This is a semantic of the shape, not word recognition
            if offset.is_none()
                && matches!(anchor, Anchor::Period { .. })
                && matches!(interp.shape, Shape::AsOf | Shape::Until)
            {
                if let Some(end) = until {
                    return place(interp.shape, Some(end), None, "B");
                }
            }
            let (t, until) = match offset {
                Some(o) => {
                    let Some(moved) = shift(t, o) else {
                        return UNRESOLVED;
                    };
                    // Offsets in units of "year" or "month" (this year, last year, last month) refer to that whole span:
                    // start and end are the same bucket, and the precision is that unit
                    if matches!(o.unit, Unit::Year | Unit::Month)
                        && matches!(interp.shape, Shape::Point | Shape::Interval)
                    {
                        let unit_p = if o.unit == Unit::Year {
                            "year"
                        } else {
                            "month"
                        };
                        let start = truncate_to(moved, Some(unit_p));
                        return place(Shape::Point, Some((start, unit_p)), None, "B");
                    }
                    // Offsets in units of "quarter" (this quarter, last quarter) refer to that whole quarter, not a specific day
                    if o.unit == Unit::Quarter {
                        let Some((from, to)) = quarter_around(moved, ctx.fiscal_year_end) else {
                            return UNRESOLVED;
                        };
                        let shape = match interp.shape {
                            Shape::Point | Shape::Since | Shape::AsOf | Shape::Duration => {
                                Shape::Interval
                            }
                            s => s,
                        };
                        return place(shape, Some(from), Some(to), "B");
                    }
                    (moved, until.and_then(|(u, _)| shift(u, o).map(|m| (m, g))))
                }
                None => (t, until),
            };
            // A moment computed from an anchor takes its precision from the granularity the word states, but cannot be finer than the anchor itself
            place(interp.shape, Some((t, coarser(p, g))), until, "B")
        }
        Reference::Period { name, from, to } => {
            // A period with only one end written ("three months until June 30"): the end that is written stays as written; the other end is not guessed
            match (from, to) {
                (None, Some(t)) => {
                    return place(Shape::Until, None, parts_to_time(t), "A");
                }
                (Some(f), None) => {
                    return place(Shape::Since, parts_to_time(f), None, "A");
                }
                _ => {}
            }
            let (bounds, grade) = match (from, to) {
                (Some(f), Some(t)) => (period_bounds(f, t), "A"),
                _ => (
                    ctx.periods
                        .iter()
                        .find(|p| p.name.eq_ignore_ascii_case(name))
                        .and_then(|p| period_bounds(&p.from, &p.to)),
                    "B",
                ),
            };
            let Some((from, to)) = bounds else {
                return UNRESOLVED;
            };
            // A period is itself an interval; shapes like "until period end" are computed as their shape
            let shape = match interp.shape {
                Shape::Point | Shape::Since | Shape::Duration => Shape::Interval,
                s => s,
            };
            place(shape, Some(from), Some(to), grade)
        }
        Reference::None => UNRESOLVED,
    }
}

/// Computes the interpretations received; each interpretation uses its own section context.
fn resolve_interpretations(
    interpretations: &[Interpretation],
    context_for: impl Fn(i64) -> DocumentDating,
) -> HashMap<i64, Resolved> {
    let mut resolved = HashMap::new();
    let mut pending: Vec<&Interpretation> = interpretations.iter().collect();
    while !pending.is_empty() {
        let before = pending.len();
        pending.retain(|interp| {
            let r = resolve_one(interp, &context_for(interp.id), &resolved);
            if r == UNRESOLVED {
                return true;
            }
            resolved.insert(interp.id, r);
            false
        });
        // Each round unlocks at least one before continuing; cycles, self-references, and missing or unusable anchors are left as C once no progress is made.
        // It cannot be a fixed number of passes: a model reply can write an arbitrarily long dependency chain backwards.
        if pending.len() == before {
            break;
        }
    }
    for interp in pending {
        resolved.insert(interp.id, UNRESOLVED);
    }
    resolved
}

/// When computing, the document's date is replaced by the start point of the section the mention sits in.
fn context_at(context: &DocumentDating, now: Option<usize>) -> DocumentDating {
    let mut scoped = context.clone();
    if let Some(entry) = now.and_then(|i| context.entries.get(i)) {
        scoped.date = Some(entry.from.clone());
        scoped.date_words = Some(entry.words.clone());
    }
    scoped
}

/// When a pushed statement writes `when` / `ended` as a full date or a time with a zone, it is read here as an interpretation
/// (#1089; 0045 decision 8: a structured timestamp is not a time word). The shape is a point, the reference is the absolute value as written,
/// and the granularity is the level the word reaches -- consistent with the answer the model would give for the same word. `from` stores the parts
/// converted to UTC: `DateParts` has no zone, and the offset is already folded into the value in `parse_time`; an interpretation that stored UTC parts
/// computes to the same value again, whereas storing the local wall-clock time would read back as a different moment.
///
/// Accepts only what `parse_time` reads at "day" or finer: `YYYY-MM-DD`, and
/// `YYYY-MM-DDTHH[:MM[:SS]]` with `Z` or an offset. A bare year, year-month, or any unreadable word returns `None`,
/// and takes the model path as before -- this cut does not give the code the job of recognizing time words.
fn stated_reading(text: &str, id: i64) -> Option<Interpretation> {
    let (t, precision) = utopia_extract::parse_time(text)?;
    use chrono::{Datelike, Timelike};
    let (granularity, hour, minute, second) = match precision {
        "day" => (Granularity::Day, None, None, None),
        "hour" => (Granularity::Hour, Some(t.hour()), None, None),
        "minute" => (Granularity::Minute, Some(t.hour()), Some(t.minute()), None),
        "second" => (
            Granularity::Second,
            Some(t.hour()),
            Some(t.minute()),
            Some(t.second()),
        ),
        // Bare years and year-months are outside this cut (the limit of #1089); return None so the model reads them
        _ => return None,
    };
    Some(Interpretation {
        id,
        shape: Shape::Point,
        reference: Reference::Absolute {
            from: DateParts {
                year: t.year(),
                quarter: None,
                month: Some(t.month()),
                day: Some(t.day()),
                hour,
                minute,
                second,
            },
            to: None,
        },
        granularity,
    })
}

/// A statement's start and end: `when` gives the start (a point or interval also gives the end), `ended` gives the end.
fn combine(when: Option<Resolved>, ended: Option<Resolved>) -> Resolved {
    let mut r = when.unwrap_or(UNRESOLVED);
    if let Some(e) = ended {
        let to = e.to.or(e.from);
        let to_p = e.to_p.or(e.from_p);
        if to.is_some() || to_p.is_some() {
            r.to = to;
            r.to_p = to_p;
        }
        if r.grade == "C" {
            r.grade = e.grade;
        }
    }
    r
}

/// Resolve all time mentions of one document (task `resolve_time`). Idempotent: running again overwrites the previous result,
/// so when an anchor arrives late (the document's date changed, or a later chunk gave a period), requeueing once is enough (decision 4's first step)
pub async fn resolve_document(state: &AppState, document_id: Uuid) -> anyhow::Result<()> {
    let pool = &state.pool;
    let doc = utopia_store::documents::get(pool, document_id).await?;
    if doc.deleted_at.is_some() {
        return Ok(());
    }
    let kb = utopia_store::kbs::get(pool, doc.kb_id).await?;
    let settings = utopia_store::settings::get(pool, kb.workspace_id).await?;
    let client = settings.as_ref().and_then(llm_util::chat_client);
    // Pushed statements (0054): a full date or a time with a zone written in `when` / `ended` is read by the code directly (#1089),
    // and interpreted without a chat model; other documents are as before -- with no chat model, there is no one to read the words for this step
    let pushed = crate::pipeline::source_kind(state, doc.source_id)
        .await?
        .as_deref()
        == Some("statements");
    if !pushed && client.is_none() {
        anyhow::bail!("Chat model not configured; cannot resolve time");
    }

    // 1. Document time context: what each chunk reported during extraction (0064). No model is asked here
    let context: DocumentDating = match doc.time_context.clone() {
        Some(json) => {
            let context: DocumentDating = serde_json::from_value(json)?;
            // The whole document's start point is still recorded as the document's date (library sorting and the engine's ordering of rows without a start both read it)
            if crate::extraction_open::dated_at(&doc).is_none() {
                if let Some((t, _)) = context.date.as_ref().and_then(parts_to_time) {
                    utopia_store::documents::set_content_date(pool, document_id, t).await?;
                }
            }
            context
        }
        // Extraction reported no date at all: the document does not say which day it is. Do not ask a second time about the opening (0064 decision 1) --
        // that would ask the same question of a document where extraction reported "none", and in the measurement sixteen of twenty documents were asked
        // pointlessly; documents that state a date had it reported by extraction
        None => DocumentDating::default(),
    };

    // The date read from the opening before this cut, stored on the document (date only, no entries): it is the start point of the whole,
    // and like the one extraction reports it goes into the entries; the lookups below for statement evidence and relative time words both search the entries
    let mut context = context;
    if context.entries.is_empty() {
        if let (Some(date), Some(words)) = (context.date.clone(), context.date_words.clone()) {
            context.entries.push(utopia_extract::time::TimeEntry {
                kind: "now".into(),
                name: String::new(),
                words,
                from: date,
                to: None,
                scope: Vec::new(),
                chunk: None,
                char_start: None,
            });
        }
    }
    attest_statements(state, &doc, &context).await?;

    // Periods reported by extraction (0064) are merged into the document's named periods: interpretation refers to them by name
    for e in context
        .entries
        .clone()
        .iter()
        .filter(|e| e.kind == "period")
    {
        let name = if e.name.is_empty() { &e.words } else { &e.name };
        if let (Some(to), false) = (
            e.to.clone(),
            context
                .periods
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(name)),
        ) {
            context.periods.push(NamedPeriod {
                name: name.clone(),
                from: e.from.clone(),
                to,
            });
        }
    }
    let context = context;

    // 2. Mentions: the same words in the same sentence in the same document are interpreted only once
    let mentions = utopia_store::time_mentions::for_document(pool, document_id).await?;
    if mentions.is_empty() {
        return Ok(());
    }
    // The start point governing each mention's section (0064 decision 4): read the section it sits in from the heading in its chunk's body,
    // then find the deepest-scope `now` among the dates the document reported. Two weekly reports in one document each have their own report date,
    // and "last week" counts from the date of the section it sits in. A document with no entries (old, or pushed) still uses the document's date as before
    let chunk_text: HashMap<Uuid, String> =
        sqlx::query_as::<_, (Uuid, String)>("SELECT id, text FROM chunks WHERE document_id = $1")
            .bind(document_id)
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect();
    let now_of = |m: &utopia_store::time_mentions::MentionToResolve| -> Option<usize> {
        let text = chunk_text.get(&m.chunk_id)?;
        let path = headings_at(text, usize::try_from(m.char_start).unwrap_or(0));
        let found = now_in_force(&context.entries, &path)?;
        context.entries.iter().position(|e| std::ptr::eq(e, found))
    };
    type Key = (String, String, Option<usize>);
    let mut distinct: Vec<Key> = Vec::new();
    let mut index: HashMap<Key, usize> = HashMap::new();
    let mut key_of: HashMap<Uuid, Key> = HashMap::new();
    // Pushed statements (0054): a full date or a time with a zone written in the payload is read by the code as an interpretation (#1089).
    // The interpretation goes along with the other mentions in both directions, but it never enters the list the model is asked about -- a re-ask could only get this answer,
    // and the model must not overwrite what the code has read; the criterion is that the document comes from a `statements` source, not that the word looks like a date,
    // the same words in ordinary documents are still handed to the model
    let mut presolved: HashMap<usize, Interpretation> = HashMap::new();
    for m in &mentions {
        let key = (m.text.clone(), m.sentence.clone(), now_of(m));
        key_of.insert(m.id, key.clone());
        if index.contains_key(&key) {
            continue;
        }
        index.insert(key.clone(), distinct.len());
        distinct.push(key);
        if pushed {
            if let Some(interp) = stated_reading(&m.text, (distinct.len() - 1) as i64) {
                presolved.insert(distinct.len() - 1, interp);
            }
        }
    }
    let ctx = TimeContext {
        date: context.date.as_ref(),
        date_words: context.date_words.as_deref(),
        periods: &context.periods,
        fiscal_year_end: context.fiscal_year_end,
    };
    let mut interpretations: Vec<Interpretation> = presolved.values().cloned().collect();
    let mut received = HashSet::new();
    let mut malformed = 0usize;
    // Ask two rounds: the first asks all, the second asks only those the first did not answer. Mentions the model missed used to keep the word with no interpretation,
    // and nobody asked again ("last week" on the measurement bench was like this three times). Those the code has already read are not asked
    let mut todo: Vec<usize> = (0..distinct.len())
        .filter(|i| !presolved.contains_key(i))
        .collect();
    if let (Some(settings), Some(client)) = (settings.as_ref(), client.as_ref()) {
        for round in 0..2 {
            if round == 1 {
                todo.retain(|i| !received.contains(&(*i as i64)));
                if todo.is_empty() {
                    break;
                }
                tracing::info!(%document_id, unanswered = todo.len(), "时间解释有没答到的，再问一次");
            }
            for batch in todo.clone().chunks(BATCH) {
                let inputs: Vec<MentionInput<'_>> = batch
                    .iter()
                    .map(|&i| MentionInput {
                        id: i as i64,
                        text: &distinct[i].0,
                        sentence: &distinct[i].1,
                    })
                    .collect();
                let ids: Vec<i64> = inputs.iter().map(|m| m.id).collect();
                let messages = build_interpretation_messages(&ctx, &inputs);
                let reply = match chat_retrying_rate_limits_at(
                    state,
                    settings,
                    client,
                    &messages,
                    Some(0.0),
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(%document_id, error = %e, "时间解释调用失败，这一批留作未解析");
                        continue;
                    }
                };
                let (interps, skipped) = match parse_interpretation_response(&reply.text, &ids) {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!(%document_id, error = %e, "时间解释回复解析失败，这一批留作未解析");
                        continue;
                    }
                };
                malformed += skipped;
                if skipped > 0 {
                    // Bad entries must show which one is bad and what is wrong with it: reporting only a count, "last week" with no date three times over could not be explained
                    tracing::warn!(%document_id, skipped, reply = %reply.text.chars().take(600).collect::<String>(), "时间解释里有读不了的条目");
                }
                received.extend(interps.iter().map(|interp| interp.id));
                interpretations.extend(interps);
            }
        }
    }
    // Receiving an interpretation and computing a date are two different things: the follow-up answers only what was not received, while computation looks at all batches and the interpretations from follow-up.
    // That way downstream does not need to ask the model again, and can still use anchors that only arrive in a later batch or a follow-up answer.
    let computed = resolve_interpretations(&interpretations, |id| {
        let now = usize::try_from(id)
            .ok()
            .and_then(|i| distinct.get(i))
            .and_then(|key| key.2);
        context_at(&context, now)
    });
    let mut resolved: HashMap<usize, (Interpretation, Resolved)> = HashMap::new();
    for interp in interpretations {
        if let Ok(i) = usize::try_from(interp.id) {
            let r = computed[&interp.id];
            resolved.insert(i, (interp, r));
        }
    }

    // 3. Write the mentions back, then merge starts and ends per statement
    let mut per_fact: HashMap<Uuid, (Option<Resolved>, Option<Resolved>)> = HashMap::new();
    let (mut a, mut b, mut c) = (0usize, 0usize, 0usize);
    for m in &mentions {
        let Some(&i) = key_of.get(&m.id).and_then(|k| index.get(k)) else {
            continue;
        };
        let Some((interp, r)) = resolved.get(&i) else {
            // A pushed document has no chat model to ask (#1089): mentions the code cannot read are recorded as C -- "has a mention but it could not be placed on the axis"
            // and "no time word at all" must be distinguishable; the task finishes as usual, rather than leaving a task that fails forever over an unconfigured model
            // The interpretation column is left empty: nobody read it, so do not fabricate one
            if pushed && client.is_none() {
                utopia_store::time_mentions::set_resolution(
                    pool, m.id, "C", None, None, None, None,
                )
                .await?;
                c += 1;
                let slot = per_fact.entry(m.fact_id).or_insert((None, None));
                if m.role == "ended" {
                    slot.1 = Some(UNRESOLVED);
                } else {
                    slot.0 = Some(UNRESOLVED);
                }
            }
            // Had a model but no answer came back: keep the word, write no grade (ask again on the next rerun)
            continue;
        };
        utopia_store::time_mentions::set_interpretation(
            pool,
            m.id,
            serde_json::to_value(interp.shape)?
                .as_str()
                .unwrap_or("point"),
            &serde_json::to_value(&interp.reference)?,
            granularity_str(interp.granularity),
        )
        .await?;
        utopia_store::time_mentions::set_resolution(
            pool, m.id, r.grade, r.from, r.from_p, r.to, r.to_p,
        )
        .await?;
        match r.grade {
            "A" => a += 1,
            "B" => b += 1,
            _ => c += 1,
        }
        let slot = per_fact.entry(m.fact_id).or_insert((None, None));
        if m.role == "ended" {
            slot.1 = Some(*r);
        } else {
            slot.0 = Some(*r);
        }
    }
    let mut dated = 0usize;
    for (fact_id, (when, ended)) in per_fact {
        let r = combine(when, ended);
        // The grade of a start comes only from the start's own mention. combine borrows the end's grade when the start cannot be anchored, to report a count;
        // here it does not borrow: the engine asks "how did this row's start come to be" (0045 cut 3)
        let grade = when.map(|w| w.grade);
        let placed = r.from.is_some() || r.to.is_some() || r.to_p.is_some();
        // Even if nothing was computed, write grade C: only this column can say "this sentence has a time word, and we could not place it on the axis"; otherwise
        // it looks identical in the database to "the whole sentence has no time word"
        if !placed && grade != Some("C") {
            continue;
        }
        utopia_store::graph::set_open_validity(
            pool, fact_id, r.from, r.from_p, r.to, r.to_p, grade,
        )
        .await?;
        if placed {
            dated += 1;
        }
    }
    tracing::info!(
        %document_id,
        mentions = mentions.len(),
        grade_a = a,
        grade_b = b,
        grade_c = c,
        malformed,
        statements_dated = dated,
        "时间提及解析完成"
    );
    if dated > 0 {
        state.emit_graph(doc.kb_id);
    }
    Ok(())
}

/// Name of the witness: the name of the date plus its words ("reporting date 2026-09-04"). Without a name, just the words; on a cover page
/// a bare date, the model often fills in the date's own name, so it is written only once
fn witness_name(name: &str, words: &str) -> String {
    let (name, words) = (name.trim(), words.trim());
    if name.is_empty() || name == words {
        words.to_string()
    } else {
        format!("{name} {words}")
    }
}

/// Each statement in this document is witnessed by the moment its section's text speaks (0064 decision 3).
///
/// Which section a statement is in is found from the position of its citation within its chunk, and the heading in that chunk's body; what governs it is the deepest-scope `now`.
/// If there is none, write nothing -- the witness is left empty, and the statement holds at any point in time (decision 5).
/// This is "as of", not the start: "the market is global" in a report says that is how it was that day, not that it has been so since that day
async fn attest_statements(
    state: &AppState,
    doc: &utopia_core::models::Document,
    context: &DocumentDating,
) -> anyhow::Result<()> {
    let pool = &state.pool;
    if !context.entries.iter().any(|e| e.kind == "now") {
        // This pass of the document stated no date at all: the earlier evidence recorded from the last pass no longer counts either
        sqlx::query(
            "UPDATE fact_evidence SET attested_at = NULL, attested_by = NULL
              WHERE document_id = $1 AND attested_at IS NOT NULL",
        )
        .bind(doc.id)
        .execute(pool)
        .await?;
        return Ok(());
    }
    let rows: Vec<(Uuid, Uuid, String, Option<i32>)> = sqlx::query_as(
        "SELECT fe.fact_id, fe.chunk_id, c.text, fe.quote_start
           FROM fact_evidence fe
           JOIN facts f ON f.id = fe.fact_id
           JOIN chunks c ON c.id = fe.chunk_id
          WHERE fe.document_id = $1 AND c.superseded_at IS NULL
            AND f.layer = 'open' AND f.invalidated_at IS NULL",
    )
    .bind(doc.id)
    .fetch_all(pool)
    .await?;
    let mut moved = 0usize;
    for (fact_id, chunk_id, text, start) in rows {
        let path = headings_at(&text, usize::try_from(start.unwrap_or(0)).unwrap_or(0));
        let witness = now_in_force(&context.entries, &path).and_then(|now| {
            let (at, _) = parts_to_time(&now.from)?;
            Some((at, witness_name(&now.name, &now.words)))
        });
        // Each piece of evidence records the date of its own section (blank if none was stated): in fact only the earliest is kept, and when the document holding the evidence is deleted
        // the remaining evidence must be recomputed, so each one must know which day it belongs to
        utopia_store::graph::witness_evidence(
            pool,
            fact_id,
            chunk_id,
            witness.as_ref().map(|(at, by)| (*at, by.as_str())),
        )
        .await?;
        let Some((at, by)) = witness else {
            continue;
        };
        if utopia_store::graph::attest_statement(pool, fact_id, at, &by).await? {
            moved += 1;
        }
    }
    // Typed rows already materialized by rules follow their statements
    let typed = utopia_store::materialize::sync_typed_attestation(pool, doc.kb_id, None).await?;
    // Rows whose witness changed take a new place on the uniqueness timeline: recompute only the timelines those rows sit in, not the whole database
    // -- re-running every timeline across the database in a transaction for each document would be thousands of transactions per document once the database grows.
    // Commit the sync first, then take the timeline lock; do not wait for the advisory lock while holding an UPDATE row lock (same order as reconciliation after a materialization commit)
    let reconciled = utopia_store::temporal::reconcile_moved_facts(pool, doc.kb_id, &typed).await?;
    let typed = typed.len();
    if moved > 0 || typed > 0 {
        tracing::info!(document_id = %doc.id, statements = moved, typed,
            corrected = reconciled.corrected.len(), conflicts = reconciled.conflicts,
            "陈述按它所在那一节的日期作了证");
        state.emit_graph(doc.kb_id);
    }
    Ok(())
}

#[cfg(test)]
#[path = "time_context_tests.rs"]
mod context_tests;

#[cfg(test)]
#[path = "time_attestation_tests.rs"]
mod attestation_tests;

#[cfg(test)]
#[path = "time_resolution_pushed_tests.rs"]
mod pushed_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// A value written in the push payload (#1089): a full date or a time with a zone is read as a point-wise absolute interpretation, with precision following the words;
    /// a bare year, year-month, or unreadable word returns None -- the code does not take over recognizing time words
    #[test]
    fn a_stated_value_reads_at_the_precision_it_writes() {
        let read = |s: &str| {
            let interp = stated_reading(s, 0)?;
            let Reference::Absolute { from, to } = interp.reference else {
                return None;
            };
            assert_eq!(interp.shape, Shape::Point);
            assert!(to.is_none());
            Some((from, interp.granularity))
        };
        let date = |y, m, d, h, mi, s| DateParts {
            year: y,
            quarter: None,
            month: Some(m),
            day: Some(d),
            hour: h,
            minute: mi,
            second: s,
        };
        assert_eq!(
            read("2026-09-23"),
            Some((date(2026, 9, 23, None, None, None), Granularity::Day))
        );
        assert_eq!(
            read("2026-09-23T08Z"),
            Some((date(2026, 9, 23, Some(8), None, None), Granularity::Hour))
        );
        assert_eq!(
            read("2026-09-23T08:14Z"),
            Some((
                date(2026, 9, 23, Some(8), Some(14), None),
                Granularity::Minute
            ))
        );
        assert_eq!(
            read("2026-09-23T08:14:03Z"),
            Some((
                date(2026, 9, 23, Some(8), Some(14), Some(3)),
                Granularity::Second
            ))
        );
        // The offset is folded into the value: 08:14 at +08:00 is 00:14 UTC
        assert_eq!(
            read("2026-09-23T08:14:03+08:00"),
            Some((
                date(2026, 9, 23, Some(0), Some(14), Some(3)),
                Granularity::Second
            ))
        );
        // Words outside this cut: bare years, year-months, natural language
        for s in ["2026", "2026-09", "last Tuesday", "ended", ""] {
            assert!(read(s).is_none(), "{s:?} is not code-resolved");
        }
    }

    #[test]
    fn a_bare_date_is_named_once() {
        assert_eq!(
            witness_name("提报日期", "2026年9月4日"),
            "提报日期 2026年9月4日"
        );
        assert_eq!(witness_name("", "2024年10月10日"), "2024年10月10日");
        assert_eq!(
            witness_name("2024年10月10日", " 2024年10月10日"),
            "2024年10月10日"
        );
    }
    use utopia_extract::time::NamedPeriod;

    fn parts(y: i32, m: Option<u32>, d: Option<u32>) -> DateParts {
        DateParts {
            quarter: None,
            year: y,
            month: m,
            day: d,
            hour: None,
            minute: None,
            second: None,
        }
    }
    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    fn ctx_dated(y: i32, m: u32, d: u32) -> DocumentDating {
        DocumentDating {
            entries: Vec::new(),
            date: Some(parts(y, Some(m), Some(d))),
            date_words: Some("x".into()),
            periods: vec![NamedPeriod {
                name: "fiscal 2027".into(),
                from: parts(2026, Some(1), Some(26)),
                to: parts(2027, Some(1), Some(25)),
            }],
            fiscal_year_end: Some((1, 25)),
            skipped: 0,
        }
    }

    fn day_point(date: &str, grade: &'static str) -> Resolved {
        Resolved {
            from: Some(at(date)),
            from_p: Some("day"),
            to: Some(at(date)),
            to_p: Some("day"),
            grade,
        }
    }

    fn absolute_day(id: i64) -> Interpretation {
        Interpretation {
            id,
            shape: Shape::Point,
            reference: Reference::Absolute {
                from: parts(2024, Some(1), Some(1)),
                to: None,
            },
            granularity: Granularity::Day,
        }
    }

    fn next_day(id: i64, anchor_id: i64) -> Interpretation {
        Interpretation {
            reference: Reference::Anchored {
                anchor: Anchor::Mention { id: anchor_id },
                offset: Some(Offset {
                    count: 1,
                    unit: Unit::Day,
                    direction: Direction::After,
                }),
            },
            ..absolute_day(id)
        }
    }

    #[test]
    fn mention_dependencies_resolve_in_all_reply_orders() {
        let items = [
            serde_json::json!([0, "point", {"kind": "absolute", "from": {"y": 2024, "m": 1, "d": 1}}, "day"]),
            serde_json::json!([1, "point", {"kind": "anchored", "anchor": {"kind": "mention", "id": 0}, "offset": {"count": 1, "unit": "day", "direction": "after"}}, "day"]),
            serde_json::json!([2, "point", {"kind": "anchored", "anchor": {"kind": "mention", "id": 1}, "offset": {"count": 1, "unit": "day", "direction": "after"}}, "day"]),
        ];
        let expected = HashMap::from([
            (0, day_point("2024-01-01T00:00:00Z", "A")),
            (1, day_point("2024-01-02T00:00:00Z", "B")),
            (2, day_point("2024-01-03T00:00:00Z", "B")),
        ]);
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let reply = serde_json::json!({"m": order.map(|i| items[i].clone())});
            let (interpretations, skipped) =
                parse_interpretation_response(&reply.to_string(), &[0, 1, 2]).unwrap();
            assert_eq!(skipped, 0);
            assert_eq!(interpretations.len(), 3);
            let resolved = resolve_interpretations(&interpretations, |_| DocumentDating::default());
            assert_eq!(resolved, expected, "reply order: {order:?}");
        }
    }

    #[test]
    fn a_long_reverse_chain_does_not_follow_numeric_ids_or_a_fixed_pass_limit() {
        // Numbering interleaves top and bottom, unrelated to dependency order; the chain length also exceeds one batch's size.
        let ids: Vec<i64> = (0..81).map(|depth| 500 + (depth * 37) % 101).collect();
        let mut interpretations = vec![absolute_day(ids[0])];
        for pair in ids.windows(2) {
            interpretations.push(next_day(pair[1], pair[0]));
        }
        interpretations.reverse();
        let resolved = resolve_interpretations(&interpretations, |_| DocumentDating::default());
        assert_eq!(resolved.len(), ids.len());
        for (depth, id) in ids.iter().enumerate() {
            let date = at("2024-01-01T00:00:00Z") + Duration::days(depth as i64);
            assert_eq!(
                resolved[id],
                Resolved {
                    from: Some(date),
                    from_p: Some("day"),
                    to: Some(date),
                    to_p: Some("day"),
                    grade: if depth == 0 { "A" } else { "B" },
                },
                "depth {depth}, id {id}"
            );
        }
    }

    #[test]
    fn independent_chains_resolve_while_cycles_and_missing_anchors_stay_unresolved() {
        let second_root = Interpretation {
            reference: Reference::Absolute {
                from: parts(2025, Some(6), Some(10)),
                to: None,
            },
            ..absolute_day(20)
        };
        let interpretations = [
            next_day(2, 1),
            next_day(30, 30), // self-reference
            next_day(31, 32),
            next_day(32, 31),  // cycle
            next_day(33, 999), // missing interpretation
            next_day(34, 31),  // downstream of a dependency cycle
            next_day(22, 21),
            next_day(1, 0),
            next_day(21, 20),
            second_root,
            absolute_day(0),
        ];
        let resolved = resolve_interpretations(&interpretations, |_| ctx_dated(2030, 1, 1));
        assert_eq!(
            resolved,
            HashMap::from([
                (0, day_point("2024-01-01T00:00:00Z", "A")),
                (1, day_point("2024-01-02T00:00:00Z", "B")),
                (2, day_point("2024-01-03T00:00:00Z", "B")),
                (20, day_point("2025-06-10T00:00:00Z", "A")),
                (21, day_point("2025-06-11T00:00:00Z", "B")),
                (22, day_point("2025-06-12T00:00:00Z", "B")),
                (30, UNRESOLVED),
                (31, UNRESOLVED),
                (32, UNRESOLVED),
                (33, UNRESOLVED),
                (34, UNRESOLVED),
            ])
        );
    }

    #[test]
    fn received_interpretations_can_use_anchors_from_later_batches_and_the_makeup_reply() {
        let first_reply = r#"{"m": [
            [2, "point", {"kind": "anchored", "anchor": {"kind": "mention", "id": 1}, "offset": {"count": 1, "unit": "day", "direction": "after"}}, "day"],
            [1, "point", {"kind": "anchored", "anchor": {"kind": "mention", "id": 40}, "offset": {"count": 1, "unit": "day", "direction": "after"}}, "day"]
        ]}"#;
        let (mut interpretations, skipped) =
            parse_interpretation_response(first_reply, &[0, 1, 2]).unwrap();
        assert_eq!(skipped, 0);
        let before = resolve_interpretations(&interpretations, |_| DocumentDating::default());
        assert_eq!(before, HashMap::from([(1, UNRESOLVED), (2, UNRESOLVED)]));
        assert!(!before.contains_key(&0), "未回答与已回答但未解析的提及不同");

        let later_batch = r#"{"m": [
            [40, "point", {"kind": "anchored", "anchor": {"kind": "mention", "id": 0}, "offset": {"count": 1, "unit": "day", "direction": "after"}}, "day"]
        ]}"#;
        let (later, skipped) = parse_interpretation_response(later_batch, &[40]).unwrap();
        assert_eq!(skipped, 0);
        interpretations.extend(later);
        let still_waiting =
            resolve_interpretations(&interpretations, |_| DocumentDating::default());
        assert_eq!(still_waiting[&2], UNRESOLVED);

        let makeup_reply = r#"{"m": [
            [0, "point", {"kind": "absolute", "from": {"y": 2024, "m": 1, "d": 1}}, "day"]
        ]}"#;
        let (makeup, skipped) = parse_interpretation_response(makeup_reply, &[0]).unwrap();
        assert_eq!(skipped, 0);
        interpretations.extend(makeup);
        let after = resolve_interpretations(&interpretations, |_| DocumentDating::default());
        assert_eq!(
            after,
            HashMap::from([
                (0, day_point("2024-01-01T00:00:00Z", "A")),
                (40, day_point("2024-01-02T00:00:00Z", "B")),
                (1, day_point("2024-01-03T00:00:00Z", "B")),
                (2, day_point("2024-01-04T00:00:00Z", "B")),
            ])
        );
    }

    #[test]
    fn dependency_resolution_keeps_each_mentions_section_context() {
        let mut context = ctx_dated(2030, 1, 1);
        for (section, year) in [("First report", 2024), ("Second report", 2025)] {
            context.entries.push(utopia_extract::time::TimeEntry {
                kind: "now".into(),
                name: String::new(),
                words: year.to_string(),
                from: parts(year, Some(1), Some(1)),
                to: None,
                scope: vec![section.into()],
                chunk: None,
                char_start: None,
            });
        }
        let document_anchor = |id| Interpretation {
            reference: Reference::Anchored {
                anchor: Anchor::Document,
                offset: None,
            },
            ..absolute_day(id)
        };
        // Even numbers are in the first section, odd in the second; each of the two chains counts from its own section's date.
        let interpretations = [
            next_day(4, 2),
            next_day(5, 3),
            next_day(2, 0),
            next_day(3, 1),
            document_anchor(1),
            document_anchor(0),
        ];
        let resolved = resolve_interpretations(&interpretations, |id| {
            context_at(&context, Some((id % 2) as usize))
        });
        assert_eq!(
            resolved,
            HashMap::from([
                (0, day_point("2024-01-01T00:00:00Z", "B")),
                (2, day_point("2024-01-02T00:00:00Z", "B")),
                (4, day_point("2024-01-03T00:00:00Z", "B")),
                (1, day_point("2025-01-01T00:00:00Z", "B")),
                (3, day_point("2025-01-02T00:00:00Z", "B")),
                (5, day_point("2025-01-03T00:00:00Z", "B")),
            ])
        );
    }

    #[test]
    fn dependency_resolution_preserves_dateless_endings_and_unusable_anchors() {
        let interpretations = [
            next_day(1, 0),
            Interpretation {
                shape: Shape::Until,
                ..absolute_day(0)
            },
            next_day(3, 2),
            Interpretation {
                shape: Shape::EndedUnknown,
                ..next_day(2, 2)
            },
        ];
        let resolved = resolve_interpretations(&interpretations, |_| DocumentDating::default());
        assert_eq!(
            resolved,
            HashMap::from([
                (
                    0,
                    Resolved {
                        from: None,
                        from_p: None,
                        ..day_point("2024-01-01T00:00:00Z", "A")
                    }
                ),
                (1, UNRESOLVED),
                (
                    2,
                    Resolved {
                        to_p: Some(ENDED_UNKNOWN),
                        grade: "A",
                        ..UNRESOLVED
                    }
                ),
                (3, UNRESOLVED),
            ])
        );
    }

    #[test]
    fn parts_give_the_precision_the_words_state() {
        assert_eq!(parts_to_time(&parts(2024, None, None)).unwrap().1, "year");
        assert_eq!(
            parts_to_time(&parts(2024, Some(3), None)).unwrap().1,
            "month"
        );
        let (t, p) = parts_to_time(&parts(2011, Some(3), Some(4))).unwrap();
        assert_eq!((t, p), (at("2011-03-04T00:00:00Z"), "day"));
        assert!(parts_to_time(&parts(2024, Some(13), None)).is_none());
    }

    #[test]
    fn an_absolute_date_is_grade_a_and_a_point_has_two_equal_ends() {
        let i = Interpretation {
            id: 0,
            shape: Shape::Point,
            reference: Reference::Absolute {
                from: parts(2011, Some(3), Some(4)),
                to: None,
            },
            granularity: Granularity::Day,
        };
        let r = resolve_one(&i, &ctx_dated(2026, 9, 2), &HashMap::new());
        assert_eq!(r.grade, "A");
        assert_eq!(r.from, Some(at("2011-03-04T00:00:00Z")));
        assert_eq!(r.to, r.from);
        assert_eq!((r.from_p, r.to_p), (Some("day"), Some("day")));
    }

    #[test]
    fn a_bare_year_stays_a_year_even_if_the_model_says_day() {
        let i = Interpretation {
            id: 0,
            shape: Shape::Since,
            reference: Reference::Absolute {
                from: parts(1987, None, None),
                to: None,
            },
            granularity: Granularity::Day,
        };
        let r = resolve_one(&i, &ctx_dated(2026, 9, 2), &HashMap::new());
        assert_eq!(
            (r.from, r.from_p, r.to),
            (Some(at("1987-01-01T00:00:00Z")), Some("year"), None)
        );
    }

    #[test]
    fn today_and_last_year_anchor_to_the_document_as_grade_b() {
        let today = Interpretation {
            id: 0,
            shape: Shape::Point,
            reference: Reference::Anchored {
                anchor: Anchor::Document,
                offset: None,
            },
            granularity: Granularity::Day,
        };
        let r = resolve_one(&today, &ctx_dated(2026, 9, 2), &HashMap::new());
        assert_eq!((r.grade, r.from), ("B", Some(at("2026-09-02T00:00:00Z"))));
        let last_year = Interpretation {
            id: 1,
            shape: Shape::Interval,
            reference: Reference::Anchored {
                anchor: Anchor::Document,
                offset: Some(Offset {
                    count: 1,
                    unit: Unit::Year,
                    direction: Direction::Before,
                }),
            },
            granularity: Granularity::Year,
        };
        let r = resolve_one(&last_year, &ctx_dated(2026, 9, 2), &HashMap::new());
        assert_eq!(
            (r.grade, r.from, r.from_p),
            ("B", Some(at("2025-01-01T00:00:00Z")), Some("year"))
        );
        // No document date means the anchor cannot be found
        let undated = DocumentDating::default();
        assert_eq!(resolve_one(&today, &undated, &HashMap::new()), UNRESOLVED);
    }

    #[test]
    fn a_named_period_is_an_interval_from_the_context() {
        let i = Interpretation {
            id: 0,
            shape: Shape::Point,
            reference: Reference::Period {
                name: "Fiscal 2027".into(),
                from: None,
                to: None,
            },
            granularity: Granularity::Day,
        };
        let r = resolve_one(&i, &ctx_dated(2026, 9, 2), &HashMap::new());
        assert_eq!(r.grade, "B");
        assert_eq!(r.from, Some(at("2026-01-26T00:00:00Z")));
        assert_eq!(r.to, Some(at("2027-01-25T00:00:00Z")));
        // A period that states its own start and end within the text is grade A
        let stated = Interpretation {
            reference: Reference::Period {
                name: "three months ended July 26, 2026".into(),
                from: Some(parts(2026, Some(4), Some(27))),
                to: Some(parts(2026, Some(7), Some(26))),
            },
            ..i
        };
        assert_eq!(
            resolve_one(&stated, &DocumentDating::default(), &HashMap::new()).grade,
            "A"
        );
    }

    #[test]
    fn a_stated_quarter_is_the_three_months_it_names() {
        let i = Interpretation {
            id: 0,
            shape: Shape::Point,
            reference: Reference::Absolute {
                from: DateParts {
                    quarter: Some(3),
                    ..parts(2025, None, None)
                },
                to: None,
            },
            granularity: Granularity::Month,
        };
        let r = resolve_one(&i, &DocumentDating::default(), &HashMap::new());
        assert_eq!(r.grade, "A");
        assert_eq!(r.from, Some(at("2025-07-01T00:00:00Z")));
        assert_eq!(r.from_p, Some("month"));
        assert_eq!(r.to, Some(at("2025-09-01T00:00:00Z")));
        assert_eq!(r.to_p, Some("month"));
    }

    #[test]
    fn an_interval_from_one_quarter_to_another_ends_in_the_last_month() {
        let quarter = |q| DateParts {
            quarter: Some(q),
            ..parts(2025, None, None)
        };
        let i = Interpretation {
            id: 0,
            shape: Shape::Interval,
            reference: Reference::Absolute {
                from: quarter(1),
                to: Some(quarter(3)),
            },
            granularity: Granularity::Month,
        };
        let r = resolve_one(&i, &DocumentDating::default(), &HashMap::new());
        assert_eq!(
            (r.from, r.to, r.from_p, r.to_p, r.grade),
            (
                Some(at("2025-01-01T00:00:00Z")),
                Some(at("2025-09-01T00:00:00Z")),
                Some("month"),
                Some("month"),
                "A"
            )
        );
    }

    #[test]
    fn two_years_later_follows_the_mention_it_names() {
        let mut earlier = HashMap::new();
        earlier.insert(
            7,
            Resolved {
                from: Some(at("2019-01-01T00:00:00Z")),
                from_p: Some("year"),
                to: Some(at("2019-01-01T00:00:00Z")),
                to_p: Some("year"),
                grade: "A",
            },
        );
        let i = Interpretation {
            id: 8,
            shape: Shape::Point,
            reference: Reference::Anchored {
                anchor: Anchor::Mention { id: 7 },
                offset: Some(Offset {
                    count: 2,
                    unit: Unit::Year,
                    direction: Direction::After,
                }),
            },
            granularity: Granularity::Year,
        };
        let r = resolve_one(&i, &DocumentDating::default(), &earlier);
        assert_eq!(
            (r.grade, r.from, r.from_p),
            ("B", Some(at("2021-01-01T00:00:00Z")), Some("year"))
        );
    }

    #[test]
    fn an_ending_without_a_date_and_a_when_plus_ended_combine() {
        let ended = Interpretation {
            id: 0,
            shape: Shape::EndedUnknown,
            reference: Reference::None,
            granularity: Granularity::Day,
        };
        let e = resolve_one(&ended, &DocumentDating::default(), &HashMap::new());
        assert_eq!((e.to, e.to_p, e.grade), (None, Some(ENDED_UNKNOWN), "A"));
        let since = Resolved {
            from: Some(at("2020-01-01T00:00:00Z")),
            from_p: Some("year"),
            to: None,
            to_p: None,
            grade: "A",
        };
        let until = Resolved {
            from: Some(at("2024-06-01T00:00:00Z")),
            from_p: Some("month"),
            to: None,
            to_p: None,
            grade: "A",
        };
        let r = combine(Some(since), Some(until));
        assert_eq!(
            (r.from, r.to, r.to_p),
            (since.from, until.from, Some("month"))
        );
        assert_eq!(combine(None, Some(e)).to_p, Some(ENDED_UNKNOWN));
    }

    #[test]
    fn this_quarter_is_the_whole_quarter_and_the_fiscal_calendar_moves_it() {
        let this_quarter = Interpretation {
            id: 0,
            shape: Shape::Point,
            reference: Reference::Anchored {
                anchor: Anchor::Document,
                offset: Some(Offset {
                    count: 0,
                    unit: Unit::Quarter,
                    direction: Direction::After,
                }),
            },
            granularity: Granularity::Month,
        };
        // Calendar quarter: 2 September is in the third quarter (July-September)
        let calendar = DocumentDating {
            fiscal_year_end: None,
            ..ctx_dated(2026, 9, 2)
        };
        let r = resolve_one(&this_quarter, &calendar, &HashMap::new());
        assert_eq!(
            (r.from, r.to),
            (
                Some(at("2026-07-01T00:00:00Z")),
                Some(at("2026-09-01T00:00:00Z"))
            )
        );
        assert_eq!(
            (r.from_p, r.to_p, r.grade),
            (Some("month"), Some("month"), "B")
        );
        // Fiscal year ends on January 25: quarter-start months are 2, 5, 8, 11, and 2 September is in the quarter of August-October
        let r = resolve_one(&this_quarter, &ctx_dated(2026, 9, 2), &HashMap::new());
        assert_eq!(
            (r.from, r.to),
            (
                Some(at("2026-08-01T00:00:00Z")),
                Some(at("2026-10-01T00:00:00Z"))
            )
        );
        // The previous quarter
        let last_quarter = Interpretation {
            reference: Reference::Anchored {
                anchor: Anchor::Document,
                offset: Some(Offset {
                    count: 1,
                    unit: Unit::Quarter,
                    direction: Direction::Before,
                }),
            },
            ..this_quarter
        };
        let r = resolve_one(&last_quarter, &calendar, &HashMap::new());
        assert_eq!(r.from, Some(at("2026-04-01T00:00:00Z")));
    }

    #[test]
    fn as_of_a_period_means_its_end_and_a_whole_year_is_one_bucket() {
        let year_end = Interpretation {
            id: 0,
            shape: Shape::AsOf,
            reference: Reference::Anchored {
                anchor: Anchor::Period {
                    name: "fiscal 2027".into(),
                },
                offset: None,
            },
            granularity: Granularity::Day,
        };
        let r = resolve_one(&year_end, &ctx_dated(2026, 9, 2), &HashMap::new());
        assert_eq!((r.from, r.grade), (Some(at("2027-01-25T00:00:00Z")), "B"));
        let whole_year = Interpretation {
            id: 1,
            shape: Shape::Interval,
            reference: Reference::Anchored {
                anchor: Anchor::Document,
                offset: Some(Offset {
                    count: 0,
                    unit: Unit::Year,
                    direction: Direction::After,
                }),
            },
            granularity: Granularity::Year,
        };
        let r = resolve_one(&whole_year, &ctx_dated(2024, 1, 1), &HashMap::new());
        assert_eq!(
            (r.from, r.to, r.from_p, r.to_p),
            (
                Some(at("2024-01-01T00:00:00Z")),
                Some(at("2024-01-01T00:00:00Z")),
                Some("year"),
                Some("year")
            )
        );
    }

    #[test]
    fn a_period_with_one_stated_bound_writes_only_that_bound() {
        let ended = Interpretation {
            id: 0,
            shape: Shape::Interval,
            reference: Reference::Period {
                name: "three months ended June 30, 2019".into(),
                from: None,
                to: Some(parts(2019, Some(6), Some(30))),
            },
            granularity: Granularity::Day,
        };
        let r = resolve_one(&ended, &DocumentDating::default(), &HashMap::new());
        assert_eq!(
            (r.from, r.to, r.grade),
            (None, Some(at("2019-06-30T00:00:00Z")), "A")
        );
    }

    #[test]
    fn a_period_from_one_quarter_to_another_ends_in_the_last_month() {
        let quarter = |q| DateParts {
            quarter: Some(q),
            ..parts(2025, None, None)
        };
        let i = Interpretation {
            id: 0,
            shape: Shape::Interval,
            reference: Reference::Period {
                name: "Q1 to Q3 2025".into(),
                from: Some(quarter(1)),
                to: Some(quarter(3)),
            },
            granularity: Granularity::Month,
        };
        let r = resolve_one(&i, &DocumentDating::default(), &HashMap::new());
        assert_eq!(
            (r.from, r.to, r.from_p, r.to_p, r.grade),
            (
                Some(at("2025-01-01T00:00:00Z")),
                Some(at("2025-09-01T00:00:00Z")),
                Some("month"),
                Some("month"),
                "A"
            )
        );
    }

    #[test]
    fn month_arithmetic_clamps_to_the_month_end() {
        let t = at("2024-03-31T00:00:00Z");
        let back = shift(
            t,
            &Offset {
                count: 1,
                unit: Unit::Month,
                direction: Direction::Before,
            },
        )
        .unwrap();
        assert_eq!(back, at("2024-02-29T00:00:00Z"));
        let q = shift(
            t,
            &Offset {
                count: 1,
                unit: Unit::Quarter,
                direction: Direction::After,
            },
        )
        .unwrap();
        assert_eq!(q, at("2024-06-30T00:00:00Z"));
    }
}
