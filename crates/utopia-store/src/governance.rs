//! Governance (0025): the agent reads the ledger before deciding.
//!
//! Three things: **precedent** — what the people in this store have done for this pair, this name, or this type pair;
//! **queue** — repeated pairs awaiting a person are ordered first in, first out, and the head carries its cluster (other pairs with the same name or entity)
//! along; **gate** — a pure function that says whether this verdict is acted on automatically or written as a suggestion. The model call happens in
//! the server's `governance` task; nothing here touches the model.
//!
//! Precedent counts only human decisions (actor is not empty). Decisions the agent made itself do not count — otherwise it would cite itself,
//! and grow more confident by judging its own judgments. When a person accepts the agent's suggestion, it counts: that entry was signed by a person and goes through the human decision path.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgPool, Postgres};
use utopia_core::models::{AgentDecisionView, ReviewItem};
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

use crate::resolution::{assemble_reviews, ReviewRow};

/// Confidence threshold for acting on its own. Somewhat higher than the batch arbiter's 0.8: that tier looks only at this pair, whereas here it decides on behalf of a person
pub const AUTO_CONF: f32 = 0.85;
/// Threshold when history agrees: a person made the same decision for this pair, this name, or this type pair, so the agent's confidence may be lower
pub const SUPPORTED_CONF: f32 = 0.75;
/// How many human decisions a type pair's habit needs before it counts
pub const TYPE_PAIR_MIN: i64 = 5;
/// Maximum number of precedents per family that are included in the prompt
const PER_FAMILY: i64 = 8;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Precedent {
    pub event_id: Uuid,
    /// review.merge | review.keep | merge.manual | merge.revert
    pub action: String,
    pub left: String,
    pub right: String,
    pub at: DateTime<Utc>,
    /// The sentence written when a person decided (0026). **A precedent is only a precedent with its reason**: with only the outcome,
    /// one wrong merge would be read as "this kind should be merged" and the error would be laundered into policy. Old rows do not have this column, so it is empty
    pub why: Option<String>,
}

impl Precedent {
    /// Whether this entry merged two records or kept them apart
    pub fn merged(&self) -> bool {
        matches!(self.action.as_str(), "review.merge" | "merge.manual")
    }
}

#[derive(Debug, Clone, Default, Serialize, sqlx::FromRow)]
pub struct TypePairStats {
    pub merged: i64,
    pub kept: i64,
    pub reverted: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Precedents {
    /// The decisions a person made about these two names (regardless of left/right)
    pub same_pair: Vec<Precedent>,
    /// One name against the other: whether this name is a same-name magnet, or is always merged
    pub same_name: Vec<Precedent>,
    /// This store's habit for this type pair; if one side has no type, there is none
    pub type_pair: Option<TypePairStats>,
    /// A retraction involving either name: if there is one, only suggest, do not act
    pub reverts: Vec<Precedent>,
}

/// Two families of detail formats in the ledger: review.* records left/right, merge.* records source/target
const L: &str = "lower(COALESCE(detail->>'left', detail->>'source', ''))";
const R: &str = "lower(COALESCE(detail->>'right', detail->>'target', ''))";
const COLS: &str = "id AS event_id, action,
         COALESCE(detail->>'left', detail->>'source', '') AS \"left\",
         COALESCE(detail->>'right', detail->>'target', '') AS \"right\",
         created_at AS at,
         NULLIF(detail->>'why', '') AS why";

pub async fn precedents_for(
    pool: &PgPool,
    kb_id: Uuid,
    item: &ReviewItem,
) -> AppResult<Precedents> {
    let (a, b) = (
        item.left.name.to_lowercase(),
        item.right.name.to_lowercase(),
    );
    let is_pair = format!("(({L} = $2 AND {R} = $3) OR ({L} = $3 AND {R} = $2))");
    let touches = format!("({L} IN ($2, $3) OR {R} IN ($2, $3))");

    let same_pair: Vec<Precedent> = sqlx::query_as(&format!(
        "SELECT {COLS} FROM audit_events
         WHERE kb_id = $1 AND actor_id IS NOT NULL
           AND action IN ('review.merge', 'review.keep', 'merge.manual') AND {is_pair}
         ORDER BY created_at DESC LIMIT $4"
    ))
    .bind(kb_id)
    .bind(&a)
    .bind(&b)
    .bind(PER_FAMILY)
    .fetch_all(pool)
    .await?;

    let same_name: Vec<Precedent> = sqlx::query_as(&format!(
        "SELECT {COLS} FROM audit_events
         WHERE kb_id = $1 AND actor_id IS NOT NULL
           AND action IN ('review.merge', 'review.keep', 'merge.manual')
           AND {touches} AND NOT {is_pair}
         ORDER BY created_at DESC LIMIT $4"
    ))
    .bind(kb_id)
    .bind(&a)
    .bind(&b)
    .bind(PER_FAMILY)
    .fetch_all(pool)
    .await?;

    let reverts: Vec<Precedent> = sqlx::query_as(&format!(
        "SELECT {COLS} FROM audit_events
         WHERE kb_id = $1 AND action = 'merge.revert' AND {touches}
         ORDER BY created_at DESC LIMIT $4"
    ))
    .bind(kb_id)
    .bind(&a)
    .bind(&b)
    .bind(PER_FAMILY)
    .fetch_all(pool)
    .await?;

    let types: Vec<(Uuid, Option<Uuid>)> =
        sqlx::query_as("SELECT id, type_id FROM entities WHERE id = ANY($1)")
            .bind(vec![item.left.id, item.right.id])
            .fetch_all(pool)
            .await?;
    let type_of = |id: Uuid| types.iter().find(|t| t.0 == id).and_then(|t| t.1);
    let type_pair = match (type_of(item.left.id), type_of(item.right.id)) {
        (Some(ta), Some(tb)) => Some(type_pair_stats(pool, kb_id, ta, tb).await?),
        _ => None,
    };

    Ok(Precedents {
        same_pair,
        same_name,
        type_pair,
        reverts,
    })
}

/// This store's habit for this type pair: how many a person merged, how many they kept apart, how many they merged and later retracted
async fn type_pair_stats(
    pool: &PgPool,
    kb_id: Uuid,
    ta: Uuid,
    tb: Uuid,
) -> AppResult<TypePairStats> {
    let (merged, kept): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE rr.status = 'merged'),
                count(*) FILTER (WHERE rr.status = 'kept')
         FROM resolution_reviews rr
         JOIN entities a ON a.id = rr.left_id
         JOIN entities b ON b.id = rr.right_id
         WHERE rr.kb_id = $1 AND rr.decided_by IS NOT NULL
           AND ((a.type_id = $2 AND b.type_id = $3) OR (a.type_id = $3 AND b.type_id = $2))",
    )
    .bind(kb_id)
    .bind(ta)
    .bind(tb)
    .fetch_one(pool)
    .await?;
    let reverted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM entity_merges m
         JOIN entities s ON s.id = m.source_id
         JOIN entities t ON t.id = m.target_id
         WHERE m.kb_id = $1 AND m.reverted_at IS NOT NULL
           AND ((s.type_id = $2 AND t.type_id = $3) OR (s.type_id = $3 AND t.type_id = $2))",
    )
    .bind(kb_id)
    .bind(ta)
    .bind(tb)
    .fetch_one(pool)
    .await?;
    Ok(TypePairStats {
        merged,
        kept,
        reverted,
    })
}

/// Precedents rendered as lines in the prompt. In English: the reader is the model
pub fn render_lines(p: &Precedents) -> Vec<String> {
    let verb = |x: &Precedent| if x.merged() { "merged" } else { "kept apart" };
    let day = |t: &DateTime<Utc>| t.format("%Y-%m-%d").to_string();
    // If a person wrote a reason, include it: the model should learn "why", not "how it usually judges"
    let because = |x: &Precedent| {
        x.why
            .as_deref()
            .map(|w| format!("; they wrote: \"{w}\""))
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for x in &p.same_pair {
        out.push(format!(
            "this same pair was {} by a person on {}{}",
            verb(x),
            day(&x.at),
            because(x)
        ));
    }
    for x in &p.same_name {
        out.push(format!(
            "\"{}\" against \"{}\" was {} by a person on {}{}",
            x.left,
            x.right,
            verb(x),
            day(&x.at),
            because(x)
        ));
    }
    if let Some(t) = &p.type_pair {
        if t.merged + t.kept > 0 {
            out.push(format!(
                "for pairs of these two types in this base, people merged {}, kept {} apart, and later reverted {} merge(s)",
                t.merged, t.kept, t.reverted
            ));
        }
    }
    for x in &p.reverts {
        out.push(format!(
            "a merge of \"{}\" into \"{}\" was reverted by a person on {}{}",
            x.left,
            x.right,
            day(&x.at),
            because(x)
        ));
    }
    out
}

/// Shape of what lands in agent_decisions.precedents: each entry carries a family
pub fn precedents_json(p: &Precedents) -> serde_json::Value {
    let tag = |family: &str, xs: &[Precedent]| {
        xs.iter()
            .map(|x| {
                serde_json::json!({
                    "family": family, "event_id": x.event_id, "action": x.action,
                    "left": x.left, "right": x.right, "at": x.at, "why": x.why,
                })
            })
            .collect::<Vec<_>>()
    };
    let mut all = tag("same_pair", &p.same_pair);
    all.extend(tag("same_name", &p.same_name));
    all.extend(tag("revert", &p.reverts));
    if let Some(t) = &p.type_pair {
        all.push(serde_json::json!({
            "family": "type_pair", "merged": t.merged, "kept": t.kept, "reverted": t.reverted,
        }));
    }
    serde_json::Value::Array(all)
}

/// Shapes of names: the few kinds that can be recognized without the model
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameShape {
    /// Identical, ignoring case and a leading "the"
    Identical,
    /// One is the other with a leading qualifier removed: Google DeepMind / DeepMind
    Abbreviation,
    /// One is the other with a company suffix added: Apple Inc. / Apple, Thrive Capital / Thrive
    Suffix,
    /// One is the other with a version or edition added: Claude 4 / Claude, AlphaFold2 / AlphaFold, 2024 IMO / IMO
    Version,
    /// One is the other with one or two words added: DeepMind Health / DeepMind, Gemini Robotics-ER / Gemini Robotics
    Extension,
    /// One is a sentence that contains the other: Sam Altman's efforts / Sam Altman; or list / member
    Phrase,
    /// Neither contains the other
    Unrelated,
}

fn norm_name(s: &str) -> String {
    let s = s.trim().to_lowercase();
    let s = s.strip_prefix("the ").unwrap_or(&s).to_string();
    // An abbreviation in parentheses at the end is not a version: reinforcement learning (RL), Department of Defense (DoD)
    let s = match (s.rfind(" ("), s.ends_with(')')) {
        (Some(i), true)
            if s[i + 2..s.len() - 1].chars().all(|c| c.is_alphanumeric()) && s.len() - i <= 12 =>
        {
            s[..i].to_string()
        }
        _ => s,
    };
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

const CORPORATE_SUFFIX: &[&str] = &[
    "inc",
    "ltd",
    "llc",
    "plc",
    "corp",
    "corporation",
    "co",
    "company",
    "capital",
    "group",
    "platforms",
    "industries",
    "technologies",
    "holdings",
    "limited",
    "gmbh",
    "sa",
    "ag",
];

pub fn name_shape(a: &str, b: &str) -> NameShape {
    let (a, b) = (norm_name(a), norm_name(b));
    if a == b {
        return NameShape::Identical;
    }
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    // Must appear as a whole word: Time does not count inside Financial Times (followed by a letter), while AlphaFold counts inside AlphaFold2
    // (followed by a digit, which is a version)
    let word_at = |long: &str, short: &str| -> Option<usize> {
        let mut from = 0;
        while let Some(i) = long[from..].find(short) {
            let i = from + i;
            let before_ok = i == 0
                || !long[..i]
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_alphanumeric());
            // Followed by a letter is not a whole word — except for an extra plural s: sorting algorithms contains
            // sorting algorithm, and Times contains Time
            let rest = &long[i + short.len()..];
            let mut rest_chars = rest.chars();
            let after_ok = match (rest_chars.next(), rest_chars.next()) {
                (None, _) => true,
                (Some(c), _) if !c.is_alphabetic() => true,
                (Some('s'), None) => true,
                (Some('s'), Some(d)) => !d.is_alphanumeric(),
                _ => false,
            };
            if before_ok && after_ok {
                return Some(i);
            }
            // **Advance one character, not one byte.** When `i + 1` lands in the middle of a multi-byte character,
            // the next round's `long[from..]` panics outright — Chinese names blow up on contact, and this path
            // runs inside the arbiter, so the panic kills the whole task: the task row stays in running forever,
            // with no error and no retry, and the queue silently loses a slot (measured 2026-09-08)
            from = i + long[i..].chars().next().map_or(1, char::len_utf8);
        }
        None
    };
    let Some(pos) = word_at(long, short) else {
        return NameShape::Unrelated;
    };
    if short.is_empty() {
        return NameShape::Unrelated;
    }
    let words = |s: &str| {
        s.split(|c: char| !c.is_alphanumeric() && c != '\'')
            .filter(|w| !w.is_empty())
            .count()
    };
    let phrase_marker = |s: &str| {
        s.contains("'s ")
            || s.ends_with("'s")
            || [
                " of ",
                " by ",
                " from ",
                " for ",
                " against ",
                " with ",
                " to ",
                " in ",
                " on ",
                " at ",
                " and ",
                " or ",
                " led ",
                " into ",
            ]
            .iter()
            .any(|m| format!(" {s} ").contains(m))
    };
    if long.matches(", ").count() >= 2 {
        return NameShape::Phrase;
    }
    // Where the shorter one ends inside the longer: a plural s counts as part of it
    let end = {
        let rest = &long[pos + short.len()..];
        if rest.starts_with('s')
            && rest[1..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric())
        {
            pos + short.len() + 1
        } else {
            pos + short.len()
        }
    };
    if end == long.len() {
        let front = long[..pos].trim().trim_end_matches([',', '-', ':']);
        if front.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return NameShape::Version;
        }
        if phrase_marker(front) || words(front) > 2 {
            return NameShape::Phrase;
        }
        return NameShape::Abbreviation;
    }
    if pos == 0 {
        let tail = long[end..]
            .trim()
            .trim_start_matches([',', '-', ':', '.', 'v']);
        let tail = tail.trim();
        if tail.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return NameShape::Version;
        }
        let tail_words: Vec<&str> = tail
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        if !tail_words.is_empty() && tail_words.iter().all(|w| CORPORATE_SUFFIX.contains(w)) {
            return NameShape::Suffix;
        }
        // Two words starting the tail already make a sentence: DeepMind Health data sharing, OpenAI Ireland Ltd,
        // Meta Superintelligence Labs — none of which are the thing before them
        if phrase_marker(tail) || tail_words.len() > 1 {
            return NameShape::Phrase;
        }
        return NameShape::Extension;
    }
    NameShape::Phrase
}

/// Broad category of the type label: the extractor's labels are noisy on their own (the same company is Organization in one document and
/// Corporation in another), so only different broad categories count as a conflict. None = cannot tell which category, conflicts with nothing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeFamily {
    Person,
    Organization,
    Place,
    Event,
}

pub fn type_family(label: &str) -> Option<TypeFamily> {
    let l = label.to_lowercase();
    if l == "person" || l == "researcher" || l.ends_with("person") {
        return Some(TypeFamily::Person);
    }
    if l.contains("event") {
        return Some(TypeFamily::Event);
    }
    if [
        "organization",
        "corporation",
        "business",
        "store",
        "hospital",
        "university",
        "college",
        "school",
        "consortium",
        "ngo",
        "party",
        "agency",
        "company",
        "bank",
        "fund",
        "newspaper",
        "governmentoffice",
        "team",
    ]
    .iter()
    .any(|k| l.contains(k))
    {
        return Some(TypeFamily::Organization);
    }
    if [
        "place",
        "city",
        "country",
        "state",
        "continent",
        "administrativearea",
        "region",
        "landmark",
        "civicstructure",
        "locality",
    ]
    .iter()
    .any(|k| l.contains(k))
    {
        return Some(TypeFamily::Place);
    }
    None
}

/// Whether the two sides' type labels are different broad categories
pub fn types_conflict(a: Option<&str>, b: Option<&str>) -> bool {
    matches!(
        (a.and_then(type_family), b.and_then(type_family)),
        (Some(x), Some(y)) if x != y
    )
}

#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    /// Acts on its own: a merge can be undone, a split can be merged again
    Apply,
    /// Written as a suggestion, left for a person
    Propose,
}

impl Precedents {
    /// History agrees with this verdict: this pair was decided this way by a person, this name is decided this way every time, or this type pair has
    /// this habit (at least TYPE_PAIR_MIN entries; a merge habit also requires that nothing was retracted)
    pub fn supports(&self, same: bool) -> bool {
        let pair = self.same_pair.iter().any(|x| x.merged() == same);
        let name = !self.same_name.is_empty() && self.same_name.iter().all(|x| x.merged() == same);
        let habit = self.type_pair.as_ref().is_some_and(|t| {
            t.merged + t.kept >= TYPE_PAIR_MIN
                && if same {
                    t.merged >= t.kept && t.reverted == 0
                } else {
                    t.kept > t.merged
                }
        });
        pair || name || habit
    }
}

/// The agent judges by its own confidence; history is a reference (0025 decision 4, revised 2026-09-06). If history disagrees, it blocks:
/// a person split this pair but it was merged, merged but it was split, or a retraction involving either name. If history agrees, the threshold is lowered. Otherwise
/// the agent's own confidence applies. A type conflict never merges — that is a rule, not confidence
pub fn gate(
    same: Option<bool>,
    conf: f32,
    types_conflict: bool,
    shape: NameShape,
    p: &Precedents,
) -> Gate {
    let Some(same) = same else {
        return Gate::Propose;
    };
    if !p.reverts.is_empty() || p.same_pair.iter().any(|x| x.merged() != same) {
        return Gate::Propose;
    }
    // Two rules that do not rely on the model: different broad type categories never merge; a version tail or a sentence containing a name never merges automatically —
    // the model says "same" most often on these two, and they are almost never the same
    if same && (types_conflict || matches!(shape, NameShape::Version | NameShape::Phrase)) {
        return Gate::Propose;
    }
    let bar = if p.supports(same) {
        SUPPORTED_CONF
    } else {
        AUTO_CONF
    };
    if conf >= bar {
        Gate::Apply
    } else {
        Gate::Propose
    }
}

/// Whether a person has judged this same pair of entities: Some(true) merged, Some(false) kept apart, None never judged.
/// Keyed by entity id, not by name — two "Zhang Wei" with the same name are not the same question
pub async fn decided_before(
    pool: &PgPool,
    kb_id: Uuid,
    a: Uuid,
    b: Uuid,
) -> AppResult<Option<bool>> {
    let merged: Option<bool> = sqlx::query_scalar(
        "SELECT status = 'merged' FROM resolution_reviews
         WHERE kb_id = $1 AND decided_by IS NOT NULL AND status IN ('merged', 'kept')
           AND ((left_id = $2 AND right_id = $3) OR (left_id = $3 AND right_id = $2))
         ORDER BY decided_at DESC LIMIT 1",
    )
    .bind(kb_id)
    .bind(a)
    .bind(b)
    .fetch_optional(pool)
    .await?;
    Ok(merged)
}

/// A pair a person has already decided: follow the person's decision. Either the same entity pair was judged, or a pair of differently named names where the person's
/// decision on those names agrees ("OpenAI OpCo ≟ OpenAI" merged means merge). A pair of identical names does not count — that only shows the store once had a same-name clash,
/// not who these two are right now. Pairs with a retraction do not take this path
pub fn settled_by_people(
    left_name: &str,
    right_name: &str,
    p: &Precedents,
    prior: Option<bool>,
) -> Option<bool> {
    if !p.reverts.is_empty() {
        return None;
    }
    if let Some(m) = prior {
        return Some(m);
    }
    if left_name.to_lowercase() == right_name.to_lowercase() {
        return None;
    }
    let mut it = p.same_pair.iter();
    let first = it.next()?.merged();
    it.all(|x| x.merged() == first).then_some(first)
}

/// A pair with an open suggestion does not enter the queue: the agent has already asked, and is waiting for a person to answer. Those being judged (`adjudicating`)
/// do not enter either: they are clusters held by this task, and are not yet settled when the queue head is read
const OPEN_PROPOSAL: &str = "NOT EXISTS (SELECT 1 FROM agent_decisions d
    WHERE d.target_kind = 'review' AND d.target_id = rr.id AND d.status = 'proposed')
    AND rr.stage <> 'adjudicating'";

/// A store has only one governance task running at a time: a session-level advisory lock that follows this connection.
///
/// Every document gets a governance task queued after extraction, and `jobs::enqueue_unless_queued` only blocks queued ones, not running ones:
/// 64 workers picking them up together, ten tasks reading the same queue head at once and judging the same cluster ten times — in one 100-document
/// run, 1346 pairs were judged 8888 times, a third of the tokens spent on that. Use only **try**: if the lock is not obtained, someone is already governing this store, and that task will walk the queue to the end and queue another when there is a backlog
/// someone is already governing this store; that task will walk the queue to the end and queue another one if a backlog builds up
const BASE_TRY_LOCK: &str = "SELECT pg_try_advisory_lock(hashtextextended('governance:' || $1, 0))";
const BASE_UNLOCK: &str = "SELECT pg_advisory_unlock(hashtextextended('governance:' || $1, 0))";

/// The lock that was taken. Release it explicitly with [`BaseLock::release`]; if it is simply dropped, the lock is still held when the connection returns to the pool,
/// so `release` closes the connection when it cannot unlock, letting Postgres reclaim it
pub struct BaseLock {
    conn: sqlx::pool::PoolConnection<Postgres>,
    key: String,
}

impl BaseLock {
    pub async fn release(mut self) {
        let unlocked: Result<bool, _> = sqlx::query_scalar(BASE_UNLOCK)
            .bind(&self.key)
            .fetch_one(&mut *self.conn)
            .await;
        if !matches!(unlocked, Ok(true)) {
            let _ = self.conn.close().await;
        }
    }
}

/// Try to take this store's governance lock. `None` = another task is holding it
pub async fn try_lock_base(pool: &PgPool, kb_id: Uuid) -> AppResult<Option<BaseLock>> {
    let mut conn = pool.acquire().await?;
    let key = kb_id.to_string();
    let got: bool = sqlx::query_scalar(BASE_TRY_LOCK)
        .bind(&key)
        .fetch_one(&mut *conn)
        .await?;
    Ok(got.then_some(BaseLock { conn, key }))
}

/// Pairs of duplicates awaiting a person, first in first out
pub async fn queue(pool: &PgPool, kb_id: Uuid, limit: i64) -> AppResult<Vec<ReviewItem>> {
    let rows: Vec<ReviewRow> = sqlx::query_as(&format!(
        "SELECT rr.id, rr.left_id, rr.right_id, rr.score, rr.reason, rr.stage, rr.created_at
         FROM resolution_reviews rr
         WHERE rr.kb_id = $1 AND rr.status = 'pending' AND {OPEN_PROPOSAL}
         ORDER BY rr.created_at, rr.id LIMIT $2"
    ))
    .bind(kb_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    assemble_reviews(pool, kb_id, rows).await
}

/// The cluster at the head of the queue: other pairs awaiting a person that share its name or entity, also first in first out
pub async fn cluster_of(
    pool: &PgPool,
    kb_id: Uuid,
    head: &ReviewItem,
    limit: i64,
) -> AppResult<Vec<ReviewItem>> {
    let rows: Vec<ReviewRow> = sqlx::query_as(&format!(
        "SELECT rr.id, rr.left_id, rr.right_id, rr.score, rr.reason, rr.stage, rr.created_at
         FROM resolution_reviews rr
         JOIN entities a ON a.id = rr.left_id
         JOIN entities b ON b.id = rr.right_id
         WHERE rr.kb_id = $1 AND rr.status = 'pending' AND rr.id <> $2 AND {OPEN_PROPOSAL}
           AND (rr.left_id = ANY($3) OR rr.right_id = ANY($3)
                OR lower(a.canonical_name) = ANY($4) OR lower(b.canonical_name) = ANY($4))
         ORDER BY rr.created_at, rr.id LIMIT $5"
    ))
    .bind(kb_id)
    .bind(head.id)
    .bind(vec![head.left.id, head.right.id])
    .bind(vec![
        head.left.name.to_lowercase(),
        head.right.name.to_lowercase(),
    ])
    .bind(limit)
    .fetch_all(pool)
    .await?;
    assemble_reviews(pool, kb_id, rows).await
}

pub struct NewDecision<'a> {
    pub run_id: Uuid,
    pub target_id: Uuid,
    /// merge | keep | unsure
    pub action: &'a str,
    pub confidence: f32,
    pub reason: Option<&'a str>,
    pub precedents: serde_json::Value,
    /// proposed | applied
    pub status: &'a str,
    pub merge_id: Option<Uuid>,
    /// defer: questions left for a person (second slice)
    pub question: Option<&'a str>,
    /// What the loop looked at: [{tool, args, note}]
    pub trace: serde_json::Value,
    /// Model calls spent in the loop
    pub calls: i32,
}

pub async fn record(pool: &PgPool, kb_id: Uuid, d: NewDecision<'_>) -> AppResult<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO agent_decisions
            (id, kb_id, run_id, target_kind, target_id, action, confidence, reason,
             precedents, status, merge_id, question, trace, calls)
         VALUES ($1, $2, $3, 'review', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
    )
    .bind(id)
    .bind(kb_id)
    .bind(d.run_id)
    .bind(d.target_id)
    .bind(d.action)
    .bind(d.confidence)
    .bind(d.reason)
    .bind(d.precedents)
    .bind(d.status)
    .bind(d.merge_id)
    .bind(d.question)
    .bind(d.trace)
    .bind(d.calls)
    .execute(pool)
    .await?;
    Ok(id)
}

/// How many model calls this store spent in the loop today: the per-store, per-day budget is computed from it
pub async fn loop_calls_today(pool: &PgPool, kb_id: Uuid) -> AppResult<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(calls), 0)::bigint FROM agent_decisions
         WHERE kb_id = $1 AND created_at >= date_trunc('day', now())",
    )
    .bind(kb_id)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Tool: source text fragments mentioning this entity — which sentence its facts were extracted from, and which document it came from.
/// Uses the quote on the evidence if there is one; otherwise truncates the start of the chunk.
///
/// **Name facts go last, and quotes that are just the name itself are dropped** (0041). Name facts have confidence 1.0,
/// so ranking by confidence would always put them first in every chunk, while the quote "this chunk wrote its name" is often just the name —
/// handing the deciding agent a bare list of names is as good as giving no source text. Aliases with a full-sentence quote ("short name Haitan 1") are
/// kept as they are, since that is exactly what is needed to judge whether two are the same thing
pub async fn quotes_of(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    limit: i64,
) -> AppResult<Vec<(String, String)>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT filename, quote FROM (
           SELECT DISTINCT ON (fe.chunk_id) d.filename,
                  COALESCE(NULLIF(fe.quote, ''), left(c.text, 240)) AS quote
           FROM facts f
           -- 谓词可以为空（0010），LEFT JOIN 才不丢那些事实
           LEFT JOIN relation_types r ON r.id = f.predicate_id
           JOIN fact_evidence fe ON fe.fact_id = f.id
           JOIN chunks c ON c.id = fe.chunk_id
           JOIN documents d ON d.id = c.document_id
           WHERE f.kb_id = $1 AND f.invalidated_at IS NULL
             AND (f.subject_id = $2 OR f.object_id = $2)
             AND NOT (coalesce(r.builtin AND r.key = 'known_as', false)
                      AND lower(trim(coalesce(fe.quote, ''))) = lower(f.object_value->>'value'))
           ORDER BY fe.chunk_id, coalesce(r.builtin AND r.key = 'known_as', false), f.confidence DESC
         ) q
         LIMIT $3",
    )
    .bind(kb_id)
    .bind(entity_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Tool: the decisions a person has made in the ledger about a name — merges, splits and retractions for names containing the query
pub async fn ledger_search(
    pool: &PgPool,
    kb_id: Uuid,
    query: &str,
    limit: i64,
) -> AppResult<Vec<Precedent>> {
    let like = format!("%{}%", query.trim().to_lowercase());
    let rows: Vec<Precedent> = sqlx::query_as(&format!(
        "SELECT {COLS} FROM audit_events
         WHERE kb_id = $1 AND actor_id IS NOT NULL
           AND action IN ('review.merge', 'review.keep', 'merge.manual', 'merge.revert')
           AND ({L} LIKE $2 OR {R} LIKE $2)
         ORDER BY created_at DESC LIMIT $3"
    ))
    .bind(kb_id)
    .bind(&like)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Tool: other entities in the store whose names contain the query — how many use this name, what each is, and how many facts each has
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Namesake {
    pub name: String,
    pub type_label: Option<String>,
    pub facts: i64,
    pub merged: bool,
}

pub async fn namesakes(
    pool: &PgPool,
    kb_id: Uuid,
    query: &str,
    limit: i64,
) -> AppResult<Vec<Namesake>> {
    let like = format!("%{}%", query.trim().to_lowercase());
    let rows: Vec<Namesake> = sqlx::query_as(
        "SELECT e.canonical_name AS name, t.label AS type_label,
                (SELECT count(*) FROM facts f
                  WHERE f.kb_id = e.kb_id AND f.invalidated_at IS NULL
                    AND (f.subject_id = e.id OR f.object_id = e.id)) AS facts,
                (e.merged_into IS NOT NULL) AS merged
         FROM entities e
         LEFT JOIN entity_types t ON t.id = e.type_id
         WHERE e.kb_id = $1 AND lower(e.canonical_name) LIKE $2
         ORDER BY e.merged_into IS NOT NULL, facts DESC, e.canonical_name
         LIMIT $3",
    )
    .bind(kb_id)
    .bind(&like)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

const VIEW: &str = "SELECT d.id, d.run_id, d.target_kind, d.target_id, d.action, d.confidence,
        d.reason, d.precedents, d.status, d.merge_id, d.question, d.trace, d.calls,
        d.created_at, d.decided_at,
        u.display_name AS decided_by_name,
        a.canonical_name AS \"left\", b.canonical_name AS \"right\"
    FROM agent_decisions d
    LEFT JOIN users u ON u.id = d.decided_by
    LEFT JOIN resolution_reviews rr ON rr.id = d.target_id
    LEFT JOIN entities a ON a.id = rr.left_id
    LEFT JOIN entities b ON b.id = rr.right_id";

/// Agent queue: newest first
pub async fn list(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<AgentDecisionView>> {
    let rows = sqlx::query_as(&format!(
        "{VIEW} WHERE d.kb_id = $1 ORDER BY d.created_at DESC, d.id DESC LIMIT $2 OFFSET $3"
    ))
    .bind(kb_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get(pool: &PgPool, kb_id: Uuid, id: Uuid) -> AppResult<AgentDecisionView> {
    sqlx::query_as(&format!("{VIEW} WHERE d.kb_id = $1 AND d.id = $2"))
        .bind(kb_id)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound)
}

/// This store's govern task is running right now
pub async fn agent_running(pool: &PgPool, kb_id: Uuid) -> AppResult<bool> {
    let running = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = 'govern' AND status = 'running'
                          AND payload->>'kb_id' = $1)",
    )
    .bind(kb_id.to_string())
    .fetch_one(pool)
    .await?;
    Ok(running)
}

/// Pairs the agent has not yet looked at: those awaiting a person, without an open suggestion
pub async fn queue_len(pool: &PgPool, kb_id: Uuid) -> AppResult<i64> {
    let n = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM resolution_reviews rr
         WHERE rr.kb_id = $1 AND rr.status = 'pending' AND {OPEN_PROPOSAL}"
    ))
    .bind(kb_id)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// A cluster the agent is judging: marked adjudicating, the button on the UI is greyed out and the API refuses a person's change — while the model
/// call is in flight, if a person judges the pair, the landing step would collide
pub async fn lock(pool: &PgPool, kb_id: Uuid, ids: &[Uuid]) -> AppResult<()> {
    sqlx::query(
        "UPDATE resolution_reviews SET stage = 'adjudicating'
         WHERE kb_id = $1 AND id = ANY($2) AND status = 'pending'",
    )
    .bind(kb_id)
    .bind(ids)
    .execute(pool)
    .await?;
    Ok(())
}

/// Releases all locks when the task starts and ends: an error midway through a round, or the switch turned off, must not leave the pair locked
pub async fn release_locks(pool: &PgPool, kb_id: Uuid) -> AppResult<u64> {
    let n = sqlx::query(
        "UPDATE resolution_reviews SET stage = 'human'
         WHERE kb_id = $1 AND status = 'pending' AND stage = 'adjudicating'",
    )
    .bind(kb_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n)
}

/// Whether this pair is currently being judged by the agent: governance is on, the task is running, and this pair is marked adjudicating.
/// Missing any one of the three does not count as a lock — an adjudicating mark with no task running is just a leftover from the old arbiter, and a person can still judge it
pub async fn locked_by_agent(pool: &PgPool, kb_id: Uuid, review_id: Uuid) -> AppResult<bool> {
    let locked = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM resolution_reviews rr
            JOIN knowledge_bases k ON k.id = rr.kb_id
            WHERE rr.id = $2 AND rr.kb_id = $1 AND rr.status = 'pending'
              AND rr.stage = 'adjudicating' AND k.governance
              AND EXISTS (SELECT 1 FROM jobs j WHERE j.kind = 'govern' AND j.status = 'running'
                            AND j.payload->>'kb_id' = $1::text))",
    )
    .bind(kb_id)
    .bind(review_id)
    .fetch_one(pool)
    .await?;
    Ok(locked)
}

pub async fn count_proposed(pool: &PgPool, kb_id: Uuid) -> AppResult<i64> {
    let n = sqlx::query_scalar(
        "SELECT count(*) FROM agent_decisions WHERE kb_id = $1 AND status = 'proposed'",
    )
    .bind(kb_id)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// A person's answer: accepted / overridden answer a suggestion; reverted / overridden answer an automatic verdict.
/// An answer already given cannot be given again — that was another person's decision made just now
pub async fn settle(
    pool: &PgPool,
    kb_id: Uuid,
    id: Uuid,
    status: &str,
    user_id: Uuid,
) -> AppResult<()> {
    let n = sqlx::query(
        "UPDATE agent_decisions SET status = $3, decided_at = now(), decided_by = $4
         WHERE kb_id = $1 AND id = $2 AND status IN ('proposed', 'applied')",
    )
    .bind(kb_id)
    .bind(id)
    .bind(status)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::Conflict(
            "this decision has already been answered".into(),
        ));
    }
    Ok(())
}

/// A person judged a pair on the card: the open suggestion on this pair now has an answer — if the action matches the suggestion it is
/// accepted, if different it is overridden; with no open suggestion, nothing is done
pub async fn answer_open(
    pool: &PgPool,
    kb_id: Uuid,
    review_id: Uuid,
    action: &str,
    user_id: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE agent_decisions
            SET status = CASE WHEN action = $3 THEN 'accepted' ELSE 'overridden' END,
                decided_at = now(), decided_by = $4
          WHERE kb_id = $1 AND target_kind = 'review' AND target_id = $2 AND status = 'proposed'",
    )
    .bind(kb_id)
    .bind(review_id)
    .bind(action)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A person judged a pair: open suggestions on other pairs sharing its name or entity are now stale — they were written without this precedent.
/// Mark them superseded, return those pairs to the queue, and they will be reviewed next round with the new precedent
pub async fn supersede_siblings(pool: &PgPool, kb_id: Uuid, review_id: Uuid) -> AppResult<u64> {
    let n = sqlx::query(
        "UPDATE agent_decisions d SET status = 'superseded', decided_at = now()
         FROM resolution_reviews me
              JOIN entities ma ON ma.id = me.left_id
              JOIN entities mb ON mb.id = me.right_id,
              resolution_reviews rr
              JOIN entities a ON a.id = rr.left_id
              JOIN entities b ON b.id = rr.right_id
         WHERE me.id = $2 AND me.kb_id = $1
           AND d.kb_id = $1 AND d.status = 'proposed'
           AND d.target_kind = 'review' AND d.target_id = rr.id AND rr.id <> me.id
           AND (rr.left_id IN (me.left_id, me.right_id) OR rr.right_id IN (me.left_id, me.right_id)
                OR lower(a.canonical_name) IN (lower(ma.canonical_name), lower(mb.canonical_name))
                OR lower(b.canonical_name) IN (lower(ma.canonical_name), lower(mb.canonical_name)))",
    )
    .bind(kb_id)
    .bind(review_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n)
}

/// Circuit breaker (0025 decision 9): it trips when retractions reach a threshold within some number of days
pub const FUSE_WINDOW_DAYS: i64 = 7;
pub const FUSE_REVERTS: i64 = 2;

/// Since a given moment, how many times a person has retracted the agent's automatic merges
pub async fn reverts_since(pool: &PgPool, kb_id: Uuid, since: DateTime<Utc>) -> AppResult<i64> {
    let n = sqlx::query_scalar(
        "SELECT count(*) FROM agent_decisions
         WHERE kb_id = $1 AND status = 'reverted' AND decided_at >= $2",
    )
    .bind(kb_id)
    .bind(since)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Circuit breaker tripped: turns the switch off if it is on. Returns true if this time it really switched off — the alert fires only once
pub async fn trip(pool: &PgPool, kb_id: Uuid) -> AppResult<bool> {
    let n = sqlx::query(
        "UPDATE knowledge_bases SET governance = FALSE, updated_at = now()
         WHERE id = $1 AND governance",
    )
    .bind(kb_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// A person retracted a merge from the merge history: if the agent had decided it, that row is recorded as reverted.
/// Returns that row's id; if it was not an agent merge, None
pub async fn settle_by_merge(
    pool: &PgPool,
    kb_id: Uuid,
    merge_id: Uuid,
    user_id: Uuid,
) -> AppResult<Option<Uuid>> {
    let id = sqlx::query_scalar(
        "UPDATE agent_decisions SET status = 'reverted', decided_at = now(), decided_by = $3
         WHERE kb_id = $1 AND merge_id = $2 AND status = 'applied'
         RETURNING id",
    )
    .bind(kb_id)
    .bind(merge_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

/// Stores where the switch is on and the queue still has pairs the agent has not seen: used by the scheduled scan
pub async fn due(pool: &PgPool) -> AppResult<Vec<Uuid>> {
    let ids = sqlx::query_scalar(&format!(
        "SELECT kb.id FROM knowledge_bases kb
         WHERE kb.governance AND EXISTS (
             SELECT 1 FROM resolution_reviews rr
             WHERE rr.kb_id = kb.id AND rr.status = 'pending' AND {OPEN_PROPOSAL})"
    ))
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(action: &str) -> Precedent {
        Precedent {
            event_id: Uuid::now_v7(),
            action: action.into(),
            left: "a".into(),
            right: "b".into(),
            at: Utc::now(),
            why: None,
        }
    }

    fn stats(merged: i64, kept: i64, reverted: i64) -> Option<TypePairStats> {
        Some(TypePairStats {
            merged,
            kept,
            reverted,
        })
    }

    fn with_stats(merged: i64, kept: i64, reverted: i64) -> Precedents {
        Precedents {
            type_pair: stats(merged, kept, reverted),
            ..Default::default()
        }
    }

    #[test]
    fn unsure_and_low_confidence_are_proposed() {
        let none = Precedents::default();
        assert_eq!(
            gate(None, 0.99, false, NameShape::Unrelated, &none),
            Gate::Propose
        );
        assert_eq!(
            gate(Some(false), 0.6, false, NameShape::Unrelated, &none),
            Gate::Propose
        );
        assert_eq!(
            gate(Some(true), 0.84, false, NameShape::Unrelated, &none),
            Gate::Propose
        );
    }

    #[test]
    fn keeping_apart_needs_confidence_only() {
        let none = Precedents::default();
        assert_eq!(
            gate(Some(false), 0.9, false, NameShape::Unrelated, &none),
            Gate::Apply
        );
        // Pairs with conflicting types can also be split apart on their own: splitting does not change the graph
        assert_eq!(
            gate(Some(false), 0.9, true, NameShape::Unrelated, &none),
            Gate::Apply
        );
    }

    #[test]
    fn the_agent_judges_and_history_moves_the_bar() {
        // No history at all: the agent merges if its own confidence is enough, otherwise asks
        let none = Precedents::default();
        assert_eq!(
            gate(Some(true), 0.9, false, NameShape::Unrelated, &none),
            Gate::Apply
        );
        assert_eq!(
            gate(Some(true), 0.8, false, NameShape::Unrelated, &none),
            Gate::Propose
        );
        // History agrees: lower the threshold to SUPPORTED_CONF
        let pair = Precedents {
            same_pair: vec![p("review.merge")],
            ..Default::default()
        };
        assert_eq!(
            gate(Some(true), 0.8, false, NameShape::Unrelated, &pair),
            Gate::Apply
        );
        assert_eq!(
            gate(Some(true), 0.7, false, NameShape::Unrelated, &pair),
            Gate::Propose
        );
        let name = Precedents {
            same_name: vec![p("merge.manual"), p("review.merge")],
            ..Default::default()
        };
        assert_eq!(
            gate(Some(true), 0.8, false, NameShape::Unrelated, &name),
            Gate::Apply
        );
        // Decisions on the same name are a mix of merges and splits: that is not agreement, so fall back to the agent's own threshold
        let mixed = Precedents {
            same_name: vec![p("review.merge"), p("review.keep")],
            ..Default::default()
        };
        assert_eq!(
            gate(Some(true), 0.8, false, NameShape::Unrelated, &mixed),
            Gate::Propose
        );
        assert_eq!(
            gate(Some(true), 0.9, false, NameShape::Unrelated, &mixed),
            Gate::Apply
        );
        // The habit for this type pair
        assert_eq!(
            gate(
                Some(true),
                0.8,
                false,
                NameShape::Unrelated,
                &with_stats(6, 2, 0)
            ),
            Gate::Apply
        );
        assert_eq!(
            gate(
                Some(true),
                0.8,
                false,
                NameShape::Unrelated,
                &with_stats(3, 1, 0)
            ),
            Gate::Propose,
            "习惯要够 {TYPE_PAIR_MIN} 笔才算"
        );
        assert_eq!(
            gate(
                Some(true),
                0.8,
                false,
                NameShape::Unrelated,
                &with_stats(6, 2, 1)
            ),
            Gate::Propose,
            "撤回过的类型对不算合并的习惯"
        );
        // The split side is the same: names a person has always kept apart also have a lower split threshold
        assert_eq!(
            gate(Some(false), 0.8, false, NameShape::Unrelated, &none),
            Gate::Propose
        );
        let kept = Precedents {
            same_pair: vec![p("review.keep")],
            ..Default::default()
        };
        assert_eq!(
            gate(Some(false), 0.8, false, NameShape::Unrelated, &kept),
            Gate::Apply
        );
        assert_eq!(
            gate(
                Some(false),
                0.8,
                false,
                NameShape::Unrelated,
                &with_stats(1, 8, 0)
            ),
            Gate::Apply
        );
    }

    #[test]
    fn a_pair_people_decided_skips_the_agent() {
        let none = Precedents::default();
        assert_eq!(
            settled_by_people("Apple", "Apple Inc.", &none, Some(true)),
            Some(true)
        );
        assert_eq!(
            settled_by_people("Zhang Wei", "Zhang Wei", &none, Some(false)),
            Some(false)
        );
        assert_eq!(settled_by_people("Apple", "Apple Inc.", &none, None), None);
        let merged = Precedents {
            same_pair: vec![p("review.merge"), p("merge.manual")],
            ..Default::default()
        };
        assert_eq!(
            settled_by_people("Apple", "Apple Inc.", &merged, None),
            Some(true)
        );
        assert_eq!(
            settled_by_people("Zhang Wei", "Zhang Wei", &merged, None),
            None,
            "同名的一对：名字上的先例说不了眼前这两个是谁"
        );
        let mixed = Precedents {
            same_pair: vec![p("review.merge"), p("review.keep")],
            ..Default::default()
        };
        assert_eq!(settled_by_people("Apple", "Apple Inc.", &mixed, None), None);
        let reverted = Precedents {
            same_pair: vec![p("review.merge")],
            reverts: vec![p("merge.revert")],
            ..Default::default()
        };
        assert_eq!(
            settled_by_people("Apple", "Apple Inc.", &reverted, Some(true)),
            None
        );
    }

    #[test]
    fn a_name_has_a_shape() {
        use NameShape::*;
        assert_eq!(name_shape("Google", "google"), Identical);
        assert_eq!(
            name_shape("The New York Times", "New York Times"),
            Identical
        );
        assert_eq!(name_shape("Google DeepMind", "DeepMind"), Abbreviation);
        assert_eq!(name_shape("Adam D'Angelo", "D'Angelo"), Abbreviation);
        assert_eq!(name_shape("Apple Inc.", "Apple"), Suffix);
        assert_eq!(name_shape("Thrive Capital", "Thrive"), Suffix);
        assert_eq!(name_shape("Meta Platforms", "Meta"), Suffix);
        assert_eq!(name_shape("Claude 4 Opus", "Claude"), Version);
        assert_eq!(name_shape("AlphaFold2", "AlphaFold"), Version);
        assert_eq!(name_shape("GPT-4.5", "GPT-4"), Version);
        assert_eq!(name_shape("Lyria 3", "Lyria"), Version);
        assert_eq!(
            name_shape(
                "2024 International Mathematical Olympiad",
                "International Mathematical Olympiad"
            ),
            Version
        );
        assert_eq!(name_shape("DeepMind Health", "DeepMind"), Extension);
        // **A Chinese name must not blow it up.** Each pair below has to go down the path of "whole-word check failed,
        // try another position", and that path used to advance by bytes, landing in the middle of a multi-byte character
        // and panicking — the test asserts "there is an answer", not what the answer is
        for (a, b) in [
            ("繪圖處理器", "繪圖"),
            ("英伟达繪圖繪圖", "繪圖"),
            ("北京中关村科技园", "中关村"),
            ("東京都渋谷区", "渋谷"),
            ("Nvidia 繪圖處理器", "繪圖"),
        ] {
            let _ = name_shape(a, b);
            let _ = name_shape(b, a);
        }
        assert_eq!(
            name_shape("Gemini Robotics-ER", "Gemini Robotics"),
            Extension
        );
        assert_eq!(name_shape("OpenAI Ireland Ltd", "OpenAI"), Phrase);
        assert_eq!(
            name_shape("DeepMind Health data sharing", "DeepMind Health"),
            Phrase
        );
        assert_eq!(
            name_shape("reinforcement learning (RL)", "reinforcement learning"),
            Identical
        );
        assert_eq!(
            name_shape(
                "US Securities and Exchange Commission (SEC)",
                "Securities and Exchange Commission"
            ),
            Abbreviation
        );
        assert_eq!(
            name_shape(
                "United States Department of Defense (DoD)",
                "Department of Defense"
            ),
            Abbreviation
        );
        assert_eq!(
            name_shape("Amazon Web Services (AWS)", "Amazon Web Services"),
            Identical
        );
        assert_eq!(name_shape("Sam Altman's efforts", "Sam Altman"), Phrase);
        assert_eq!(
            name_shape("share sale led by Thrive Capital", "Thrive Capital"),
            Phrase
        );
        assert_eq!(
            name_shape("psychological abuse from Sam Altman", "Sam Altman"),
            Phrase
        );
        assert_eq!(
            name_shape("MuZero, AlphaStar, AlphaGeometry", "AlphaStar"),
            Phrase
        );
        assert_eq!(
            name_shape("MuZero, AlphaStar, AlphaGeometry", "MuZero"),
            Phrase
        );
        assert_eq!(name_shape("Time", "Financial Times"), Abbreviation);
        assert_eq!(
            name_shape(
                "C++ Standard Library sorting algorithms",
                "sorting algorithm"
            ),
            Phrase
        );
        assert_eq!(name_shape("Timeline", "Time"), Unrelated);
        assert_eq!(name_shape("Altimeter", "Altimeter Capital"), Suffix);
    }

    #[test]
    fn types_conflict_only_across_families() {
        assert!(!types_conflict(Some("Organization"), Some("Corporation")));
        assert!(!types_conflict(
            Some("SoftwareApplication"),
            Some("CreativeWork")
        ));
        assert!(!types_conflict(Some("Organization"), None));
        assert!(types_conflict(Some("Person"), Some("Organization")));
        assert!(!types_conflict(Some("City"), Some("Product")));
        assert!(types_conflict(Some("City"), Some("Corporation")));
        assert!(types_conflict(Some("ConferenceEvent"), Some("Person")));
    }

    #[test]
    fn a_version_or_a_phrase_never_merges_on_its_own() {
        let none = Precedents::default();
        assert_eq!(
            gate(Some(true), 0.99, false, NameShape::Version, &none),
            Gate::Propose
        );
        assert_eq!(
            gate(Some(true), 0.99, false, NameShape::Phrase, &none),
            Gate::Propose
        );
        assert_eq!(
            gate(Some(true), 0.99, false, NameShape::Extension, &none),
            Gate::Apply
        );
        assert_eq!(
            gate(Some(false), 0.9, false, NameShape::Version, &none),
            Gate::Apply
        );
    }

    #[test]
    fn hard_rules_come_first() {
        let pair = Precedents {
            same_pair: vec![p("review.merge")],
            ..Default::default()
        };
        assert_eq!(
            gate(Some(true), 0.99, true, NameShape::Unrelated, &pair),
            Gate::Propose,
            "类型冲突不合"
        );
        let reverted = Precedents {
            same_pair: vec![p("review.merge")],
            reverts: vec![p("merge.revert")],
            ..Default::default()
        };
        assert_eq!(
            gate(Some(true), 0.99, false, NameShape::Unrelated, &reverted),
            Gate::Propose
        );
        assert_eq!(
            gate(Some(false), 0.99, false, NameShape::Unrelated, &reverted),
            Gate::Propose
        );
        let kept = Precedents {
            same_pair: vec![p("review.keep")],
            ..Default::default()
        };
        assert_eq!(
            gate(Some(true), 0.99, false, NameShape::Unrelated, &kept),
            Gate::Propose,
            "人分开过这一对，模型说合，只建议"
        );
        let merged = Precedents {
            same_pair: vec![p("review.merge")],
            ..Default::default()
        };
        assert_eq!(
            gate(Some(false), 0.99, false, NameShape::Unrelated, &merged),
            Gate::Propose,
            "人合过这一对，模型说分，只建议"
        );
    }
}
