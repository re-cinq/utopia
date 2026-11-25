//! Alignment queue (the "Alignment" tier of #725): signatures and category words that the aligner's two votes disagree on, or cannot settle, wait here for a person to decide.
//!
//! Both kinds of entry share this tier: a phrase signature (phrase × the classes at both ends, a few example sentences, what each vote chose) needs a person to decide
//! the attribute and direction, or "none"; a category word (the word, its spellings, example names, both votes) needs a person to decide its class or "none". Once decided, the answer is written back
//! to `phrase_bindings` / `type_bindings` with `decided_by = 'person'`, and the agent does not change it afterwards; once a phrase is decided,
//! the typed graph is recomputed with the new binding; once a category word is decided, the entities under it change class and phrase signatures follow.
//!
//! Cards are assembled from the ledger: this only reads the two binding tables and the category-word signatures, and does not call a model.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utopia_core::AppResult;
use uuid::Uuid;

/// One queue entry: a phrase signature or a category word.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AlignmentItem {
    Phrase {
        id: Uuid,
        phrase: String,
        subject_class: Option<String>,
        object_class: Option<String>,
        object_is_value: bool,
        statement_count: i32,
        /// Example sentence recorded at judgment time ("subject —phrase→ object")
        examples: Vec<String>,
        /// What each vote chose: `{"first": {...} | null, "second": {...} | null}`
        votes: Option<serde_json::Value>,
        decided_at: DateTime<Utc>,
        /// Signatures that are bound but never marked which end holds at a moment (#966): bound by a person, or the aligner asked and the two votes did not agree on the same
        /// value. The attribute and direction bound now are pre-selected by the UI, waiting only for a person to fill in the marks. For undecided entries both
        /// are None
        bound_to: Option<String>,
        direction: Option<String>,
    },
    KindWord {
        kind_word: String,
        words: Vec<String>,
        examples: Vec<String>,
        phrases: Vec<String>,
        entity_count: i64,
        votes: Option<serde_json::Value>,
        decided_at: DateTime<Utc>,
    },
    /// An implication rule proposed by the aligner (0044 decision 3, slice 5): which attribute this shape implies, and how its object is read
    Rule {
        id: Uuid,
        trigger: String,
        phrase: String,
        subject_class: Option<String>,
        object_class: Option<String>,
        object_is_value: bool,
        property: String,
        property_label: String,
        reading: Option<String>,
        statement_count: i32,
        examples: Vec<String>,
        votes: Option<serde_json::Value>,
        decided_at: DateTime<Utc>,
    },
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: Uuid,
    trigger: String,
    phrase: String,
    subject_class: Option<String>,
    object_class: Option<String>,
    object_is_value: bool,
    property: String,
    property_label: String,
    reading: Option<String>,
    statement_count: i32,
    examples: Vec<String>,
    votes: Option<serde_json::Value>,
    decided_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct PhraseRow {
    id: Uuid,
    phrase: String,
    subject_class: Option<String>,
    object_class: Option<String>,
    object_is_value: bool,
    statement_count: i32,
    examples: Vec<String>,
    votes: Option<serde_json::Value>,
    decided_at: DateTime<Utc>,
    bound_to: Option<String>,
    direction: Option<String>,
}

/// Phrase signatures awaiting a person's decision: those where the two votes disagree, plus those bound to a state attribute that never marked which end holds at a moment (#966), including those bound by a person
/// (the agent does not change a person's judgment), and those the aligner asked about where the two votes did not agree on the same value (asked once, 0053 revision). Not listed
/// here, statements under them that only state a moment never count. Entries the agent bound earlier that have not been asked about yet are not listed: the aligner
/// asks them first in the next round
pub(crate) const WAITING_PHRASE: &str = "(b.status = 'undecided'
       OR (b.status = 'bound' AND b.marks IS NULL
           AND (b.decided_by = 'person' OR b.marks_asked_at IS NOT NULL)
           AND EXISTS (SELECT 1 FROM relation_types rt
                        WHERE rt.id = b.relation_type_id AND rt.temporal = 'state')))";

#[derive(sqlx::FromRow)]
struct KindWordRow {
    kind_word: String,
    words: Vec<String>,
    votes: Option<serde_json::Value>,
    decided_at: DateTime<Utc>,
}

/// Entries awaiting a person's decision, oldest first, both kinds mixed.
pub async fn list(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<AlignmentItem>> {
    let phrases: Vec<PhraseRow> = sqlx::query_as(&format!(
        "SELECT b.id, b.phrase, st.key AS subject_class, ot.key AS object_class,
                b.object_is_value, b.statement_count, b.examples, b.votes, b.decided_at,
                CASE WHEN b.status = 'bound' THEN p.key END AS bound_to,
                CASE WHEN b.status = 'bound' THEN b.direction END AS direction
           FROM phrase_bindings b
      LEFT JOIN entity_types st ON st.id = b.subject_type_id
      LEFT JOIN entity_types ot ON ot.id = b.object_type_id
      LEFT JOIN relation_types p ON p.id = b.relation_type_id
          WHERE b.kb_id = $1 AND {WAITING_PHRASE}"
    ))
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    let rules: Vec<RuleRow> = sqlx::query_as(
        "SELECT r.id, r.trigger, r.phrase, st.key AS subject_class, ot.key AS object_class,
                r.object_is_value, p.key AS property, p.label AS property_label, r.reading,
                r.statement_count, r.examples, r.votes, r.decided_at
           FROM implication_rules r
           JOIN relation_types p ON p.id = r.conclude_property_id
      LEFT JOIN entity_types st ON st.id = r.subject_type_id
      LEFT JOIN entity_types ot ON ot.id = r.object_type_id
          WHERE r.kb_id = $1 AND r.status = 'proposed'",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    let words: Vec<KindWordRow> = sqlx::query_as(
        "SELECT kind_word, words, votes, decided_at
           FROM type_bindings WHERE kb_id = $1 AND status = 'undecided'",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    // Example names and phrases for a category word come from the signatures (counted from live entities), matched by word
    let sigs = if words.is_empty() {
        Vec::new()
    } else {
        crate::type_bindings::signatures(pool, kb_id).await?
    };
    let mut items: Vec<(DateTime<Utc>, AlignmentItem)> = phrases
        .into_iter()
        .map(|r| {
            (
                r.decided_at,
                AlignmentItem::Phrase {
                    id: r.id,
                    phrase: r.phrase,
                    subject_class: r.subject_class,
                    object_class: r.object_class,
                    object_is_value: r.object_is_value,
                    statement_count: r.statement_count,
                    examples: r.examples,
                    votes: r.votes,
                    decided_at: r.decided_at,
                    bound_to: r.bound_to,
                    direction: r.direction,
                },
            )
        })
        .collect();
    for w in words {
        let sig = sigs.iter().find(|s| s.kind_word == w.kind_word);
        items.push((
            w.decided_at,
            AlignmentItem::KindWord {
                kind_word: w.kind_word,
                words: w.words,
                examples: sig.map(|s| s.examples.clone()).unwrap_or_default(),
                phrases: sig.map(|s| s.phrases.clone()).unwrap_or_default(),
                entity_count: sig.map(|s| s.count).unwrap_or(0),
                votes: w.votes,
                decided_at: w.decided_at,
            },
        ));
    }
    for r in rules {
        items.push((
            r.decided_at,
            AlignmentItem::Rule {
                id: r.id,
                trigger: r.trigger,
                phrase: r.phrase,
                subject_class: r.subject_class,
                object_class: r.object_class,
                object_is_value: r.object_is_value,
                property: r.property,
                property_label: r.property_label,
                reading: r.reading,
                statement_count: r.statement_count,
                examples: r.examples,
                votes: r.votes,
                decided_at: r.decided_at,
            },
        ));
    }
    items.sort_by_key(|(at, _)| *at);
    Ok(items
        .into_iter()
        .map(|(_, item)| item)
        .skip(usize::try_from(offset).unwrap_or(0))
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .collect())
}

/// Number of entries awaiting a person's decision, and since when the oldest one has been waiting.
pub async fn waiting(pool: &PgPool, kb_id: Uuid) -> AppResult<(i64, Option<DateTime<Utc>>)> {
    Ok(sqlx::query_as(&format!(
        "SELECT count(*), min(decided_at) FROM (
            SELECT b.decided_at FROM phrase_bindings b WHERE b.kb_id = $1 AND {WAITING_PHRASE}
            UNION ALL
            SELECT decided_at FROM type_bindings WHERE kb_id = $1 AND status = 'undecided'
            UNION ALL
            SELECT decided_at FROM implication_rules WHERE kb_id = $1 AND status = 'proposed'
         ) q"
    ))
    .bind(kb_id)
    .fetch_one(pool)
    .await?)
}
