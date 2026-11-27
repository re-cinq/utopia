//! A name is a fact about an entity (0041 decision 1).
//!
//! Each name of an entity — its primary name, short name, former name, and spelling in another script — is a value fact on the built-in attribute
//! `known_as`: `object_value = {"value": name}`, with `object_id` empty.
//! It has a source (evidence quote), two clocks, can have a validity period (renaming), can be retracted, and when merging it moves along
//! with other facts, and moves back when the merge is retracted. **It is a value, not a node**: the canvas only draws edges that carry
//! an `object_id`.
//!
//! When reading, two things must be kept apart; this module provides two SQL fragments:
//! - Recall: `has_name_in` — whether this entity has any of these names (including former names: on the world axis a name that has ended
//!   is still recognized in old documents, so only the record axis `invalidated_at` is checked);
//! - Listing facts: `not_a_name` — in the entity panel, review cards, degree counts, and rules, a name does not count
//!   as a "fact": it cannot pass as evidence, and it cannot inflate the count.

use crate::graph::{add_evidence, insert_value_fact, Validity};
use crate::resolution::{fold_markdown_markers, normalize_name};
use sqlx::PgPool;
use std::collections::HashMap;
use utopia_core::models::NameView;
use utopia_core::AppResult;
use uuid::Uuid;

/// Key of the built-in attribute. Same approach as `is_a`: `builtin = TRUE`, not seeded at store creation, created the first time it is needed
pub const KNOWN_AS: &str = "known_as";

/// Gets (or creates) this store's `known_as` attribute.
pub async fn ensure_known_as(pool: &PgPool, kb_id: Uuid) -> AppResult<Uuid> {
    if let Some((id,)) =
        sqlx::query_as::<_, (Uuid,)>("SELECT id FROM relation_types WHERE kb_id = $1 AND key = $2")
            .bind(kb_id)
            .bind(KNOWN_AS)
            .fetch_optional(pool)
            .await?
    {
        return Ok(id);
    }
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind, datatype, temporal, builtin, description)
         VALUES ($1, $2, $3, 'known as', 'attribute', 'text', 'state', TRUE, $4)
         ON CONFLICT (kb_id, key) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(kb_id)
    .bind(KNOWN_AS)
    .bind(
        "A name a text uses for this entity. Each name is a fact with its source and both clocks; \
         a name is a value and never becomes a node.",
    )
    .execute(pool)
    .await?;
    // An ON CONFLICT hit means it was created concurrently; fetch that row
    let (id,): (Uuid,) =
        sqlx::query_as("SELECT id FROM relation_types WHERE kb_id = $1 AND key = $2")
            .bind(kb_id)
            .bind(KNOWN_AS)
            .fetch_one(pool)
            .await?;
    Ok(id)
}

/// Source of a name: which chunk, and the quote. Names with no source (written by a person in the UI, or backfilled by migration) pass None
#[derive(Debug, Clone, Copy)]
pub struct NameSource<'a> {
    pub chunk_id: Uuid,
    pub quote: &'a str,
}

/// Records that "this passage calls this entity by this name". Returns the id of the name fact; returns None if the name is empty.
///
/// The same entity with the same name has only one row (`insert_value_fact` deduplicates by subject, predicate, and value),
/// mentioning it again only adds one more piece of evidence and moves the evidence date earlier.
pub async fn record(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    name: &str,
    source: Option<NameSource<'_>>,
    attested_at: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Option<Uuid>> {
    let name = normalize_name(name);
    let name = fold_markdown_markers(&name);
    if name.is_empty() {
        return Ok(None);
    }
    let attr = ensure_known_as(pool, kb_id).await?;
    let (fact_id, _) = insert_value_fact(
        pool,
        kb_id,
        entity_id,
        Some(attr),
        &serde_json::json!({ "value": name }),
        Validity {
            attested_at,
            ..Default::default()
        },
        1.0,
    )
    .await?;
    if let Some(s) = source {
        add_evidence(pool, fact_id, s.chunk_id, Some(s.quote), Some(KNOWN_AS)).await?;
    }
    Ok(Some(fact_id))
}

/// For recall: entity `{entity}` has a current name fact, with the store id in parameter `${kb}`,
/// and the lowercase value in parameter `${names}` (text[]).
///
/// Record axis only: names that ended on the world axis (former names) are still recalled — documents from before a rename still use it.
///
/// **Written as an uncorrelated subquery, filtered by store, and carrying the index's predicate** (`facts_value_text_idx` is
/// `(kb_id, lower(value)) WHERE invalidated_at IS NULL AND object_value IS NOT NULL`).
/// A correlated subquery `nf.subject_id = e.id` does not carry the store, so the planner cannot use this index, and every recall would have to
/// scan the facts of every store in the whole table
pub fn has_name_in(entity: &str, kb: usize, names: usize) -> String {
    format!("{entity}.id IN ({})", named_ids(kb, names))
}

/// The subquery for [`has_name_in`] itself: the ids of entities that have one of these names
///
/// **The value lookup runs first inside an `OFFSET 0` fence.** The planner cannot see the expression statistics of the partial index
/// `facts_value_text_idx`, so it estimates any value at 0.5% of the store's rows, and instead picks the
/// `known_as` predicate index, reading every name fact in this store and then filtering by value (in production, 71,665 buffers, versus 23 for looking up by value first).
/// The fence stops the planner from merging the value filter into the following predicate join.
pub fn named_ids(kb: usize, names: usize) -> String {
    format!(
        "SELECT v.subject_id FROM (
                    SELECT nf.subject_id, nf.predicate_id FROM facts nf
                     WHERE nf.kb_id = ${kb} AND nf.invalidated_at IS NULL
                       AND nf.object_value IS NOT NULL
                       AND lower(nf.object_value->>'value') = ANY(${names})
                    OFFSET 0) v
                   JOIN relation_types nr ON nr.id = v.predicate_id
                  WHERE nr.kb_id = ${kb} AND nr.builtin AND nr.key = '{KNOWN_AS}'"
    )
}

/// For recall: the canonical name of entity `{entity}` (if not merged) or a current name fact is one of the lowercase
/// names in parameter `${names}`.
///
/// Canonical names and aliases are written as one id set, each going through its own index (`entities_kb_name_idx`,
/// `facts_value_text_idx`). Written as `lower(canonical_name) = ANY(..) OR {has_name_in}`, one side is a subquery, so the planner can only scan
/// every entity in the store.
pub fn has_name_or_alias_in(entity: &str, kb: usize, names: usize) -> String {
    format!(
        "{entity}.id IN (SELECT x.id FROM entities x
                       WHERE x.kb_id = ${kb} AND lower(x.canonical_name) = ANY(${names})
                         AND x.merged_into IS NULL
                      UNION ALL {})",
        named_ids(kb, names)
    )
}

/// For substring lookup: the ids of entities with a current name fact matching the pattern in `${pattern}`, the store id in `${kb}`.
///
/// `lower() LIKE lower()` rather than `ILIKE`: the trigram index on the lowercased value only matches that form.
pub fn names_like_ids(kb: usize, pattern: usize) -> String {
    format!(
        "SELECT nf.subject_id FROM facts nf
                    JOIN relation_types nr ON nr.id = nf.predicate_id
                   WHERE nf.kb_id = ${kb} AND nr.kb_id = ${kb} AND nr.builtin AND nr.key = '{KNOWN_AS}'
                     AND nf.invalidated_at IS NULL AND nf.object_value IS NOT NULL
                     AND lower(nf.object_value->>'value') LIKE lower(${pattern})"
    )
}

/// For listing facts: fact `{fact}` is not a name fact.
pub fn not_a_name(fact: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM relation_types nr
                      WHERE nr.id = {fact}.predicate_id AND nr.builtin AND nr.key = '{KNOWN_AS}')"
    )
}

/// An entity's name column: the primary name first, the rest in the order first recorded. `as_of` replays to the names the store had recorded at that time.
pub async fn for_entity(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<NameView>> {
    Ok(sqlx::query_as(&format!(
        "SELECT f.id AS fact_id, f.object_value->>'value' AS name,
                lower(f.object_value->>'value') = lower(e.canonical_name) AS canonical,
                f.recorded_at, f.valid_from, f.valid_from_precision, f.valid_to, f.valid_to_precision,
                ARRAY(SELECT DISTINCT fe.document_id FROM fact_evidence fe
                       WHERE fe.fact_id = f.id AND fe.document_id IS NOT NULL
                       ORDER BY fe.document_id) AS document_ids,
                (SELECT count(*) FROM fact_evidence fe WHERE fe.fact_id = f.id) AS evidence_count
           FROM facts f
           JOIN relation_types r ON r.id = f.predicate_id
           JOIN entities e ON e.id = $2
          WHERE f.kb_id = $1 AND r.builtin AND r.key = '{KNOWN_AS}'
            AND {owner} = $2 AND {held}
          ORDER BY canonical DESC, f.recorded_at",
        owner = crate::record_axis::owner_at("f", "subject_id", as_of.map(|_| 3), false),
        held = crate::record_axis::facts_held_at("f", as_of.map(|_| 3)),
    ))
    .bind(kb_id)
    .bind(entity_id)
    .bind(as_of)
    .fetch_all(pool)
    .await?)
}

/// An alias just read from the source text that another entity already uses: send this pair to adjudication (step one of 0041 decision 5).
///
/// **Only queues, does not merge.** Two entities with the same name may be one thing split in two (a "Haitan 1" that came first,
/// and a document that came later saying "short name Haitan 1"), or they may really be two things. The judgment belongs to the arbiter and the governance gate.
/// Without queuing, the document that arrives later records the name on a new entity, so two entities are both called "Haitan 1" with nothing
/// pairing them — the insertion order decides the outcome once again.
///
/// Paired only when a type side is empty or both sides match: same names with declared different types are the business of the same-name path.
///
/// **Pairs already judged "not the same" are not queued again.** Each time the same short name appears in a chunk, it reaches here once;
/// if a pair a person (or the arbiter) kept apart were re-queued every time, the split decision would be as good as not remembered.
/// Returns the number of pairs queued
pub async fn pair_shared_name(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    name: &str,
) -> AppResult<usize> {
    let name = normalize_name(name);
    let others: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT DISTINCT e.id
           FROM entities e
           JOIN entities me ON me.id = $2
           JOIN facts nf ON nf.subject_id = e.id AND nf.kb_id = $1
                        AND nf.invalidated_at IS NULL AND nf.object_value IS NOT NULL
           JOIN relation_types nr ON nr.id = nf.predicate_id AND nr.builtin AND nr.key = $4
          WHERE e.kb_id = $1 AND e.merged_into IS NULL AND e.id <> $2
            AND lower(nf.object_value->>'value') = lower($3)
            AND (e.type_id IS NULL OR me.type_id IS NULL OR e.type_id = me.type_id)
            AND NOT EXISTS (
                SELECT 1 FROM resolution_reviews rr
                 WHERE rr.kb_id = $1 AND rr.status = 'kept'
                   AND least(rr.left_id, rr.right_id) = least(e.id, $2)
                   AND greatest(rr.left_id, rr.right_id) = greatest(e.id, $2))
          LIMIT 4",
    )
    .bind(kb_id)
    .bind(entity_id)
    .bind(&name)
    .bind(KNOWN_AS)
    .fetch_all(pool)
    .await?;
    for (other,) in &others {
        crate::resolution::create_review(
            pool,
            kb_id,
            entity_id,
            *other,
            0.0,
            &format!("shared_name|{name}"),
            crate::resolution::ReviewStage::Adjudicating,
        )
        .await?;
    }
    Ok(others.len())
}

/// Both names carry numbers, and the numbers differ: recall does not pair them.
///
/// Names that are close and differ only by one number are almost never the same thing: `migration 394` and `migration 395`,
/// `#440` and `#441`, `海洋探测器1号` and `2号`. Name vectors cannot tell them apart — two names differing by one number
/// have cosine above 0.9 under any embedding model — and containment cannot either (`migration 39` is a substring of
/// `migration 395`). On a documentation store of engineering documents, this kind of candidate judged 1,663 pairs, merged only 3, and took
/// forty percent of the queue (#1104).
///
/// **Blocks only when both sides have numbers.** A candidate with no number on one side is still proposed as usual: `沧海` and `沧海分布式推理平台 2.0`
/// are a short name paired with its full name, exactly the containment case that should be caught.
///
/// Compares the sorted digit strings; leading zeros do not count (`0106` is `106`), and full-width digits count as half-width. Chinese numerals
/// are not recognized: the "一" in `一号` and the "一" in `一个` cannot be told apart here, so it is better to propose as usual.
pub fn numbers_differ(a: &str, b: &str) -> bool {
    let (a, b) = (digit_runs(a), digit_runs(b));
    !a.is_empty() && !b.is_empty() && a != b
}

fn digit_runs(name: &str) -> Vec<String> {
    let mut runs: Vec<String> = Vec::new();
    let mut open = false;
    for c in name.chars() {
        let digit = match c {
            '0'..='9' => Some(c),
            '\u{FF10}'..='\u{FF19}' => char::from_u32(c as u32 - 0xFEE0),
            _ => None,
        };
        match digit {
            Some(d) if open => runs.last_mut().expect("an open run").push(d),
            Some(d) => {
                runs.push(d.to_string());
                open = true;
            }
            None => open = false,
        }
    }
    for run in &mut runs {
        let trimmed = run.trim_start_matches('0');
        *run = if trimmed.is_empty() {
            "0".into()
        } else {
            trimmed.into()
        };
    }
    runs.sort();
    runs
}

/// Latin word characters. Ranges are listed one by one: under a UTF-8 locale, Postgres's `\w` and `[[:alnum:]]` count Han characters as letters too.
const LATIN_WORD: &[(char, char)] = &[
    ('0', '9'),
    ('A', 'Z'),
    ('_', '_'),
    ('a', 'z'),
    ('\u{C0}', '\u{D6}'),
    // Combining mark, used for the decomposed spelling of `é`
    ('\u{300}', '\u{36F}'),
    ('\u{D8}', '\u{F6}'),
    ('\u{F8}', '\u{24F}'),
    ('\u{1E00}', '\u{1EFF}'),
];

fn is_latin_word(c: char) -> bool {
    LATIN_WORD.iter().any(|&(lo, hi)| (lo..=hi).contains(&c))
}

/// When `shorter` is inside `longer`, and it starts or ends with a Latin character, that side must not touch a Latin word character (#1104).
/// `token` is not inside `service_token`, `holmes` is inside `sherlock holmes`; no rule for Han character edges.
pub fn contains_at_word_edges(longer: &str, shorter: &str) -> bool {
    // Consistent with SQL: an empty name is inside any name
    let (Some(first), Some(last)) = (shorter.chars().next(), shorter.chars().last()) else {
        return true;
    };
    // Not `match_indices`: one hit may overlap a previous hit that failed the boundary check
    longer.char_indices().any(|(at, _)| {
        if !longer[at..].starts_with(shorter) {
            return false;
        }
        let before = longer[..at].chars().next_back();
        let after = longer[at + shorter.len()..].chars().next();
        let joins_before = is_latin_word(first) && before.is_some_and(is_latin_word);
        let joins_after = is_latin_word(last) && after.is_some_and(is_latin_word);
        !(joins_before || joins_after)
    })
}

fn word_class() -> String {
    LATIN_WORD
        .iter()
        .map(|&(lo, hi)| {
            if lo == hi {
                lo.to_string()
            } else {
                format!("{lo}-{hi}")
            }
        })
        .collect()
}

/// The same rule, written as a Postgres regex for `name`.
pub fn word_edges_pattern(name: &str) -> String {
    let (Some(first), Some(last)) = (name.chars().next(), name.chars().last()) else {
        return String::new();
    };
    let class = word_class();
    let mut p = String::new();
    if is_latin_word(first) {
        p.push_str(&format!("(^|[^{class}])"));
    }
    for c in name.chars() {
        if r"\.^$|?*+()[]{}".contains(c) {
            p.push('\\');
        }
        p.push(c);
    }
    if is_latin_word(last) {
        p.push_str(&format!("($|[^{class}])"));
    }
    p
}

/// The same rule, written as SQL, for names in a column. `expr` must be trusted SQL.
pub(crate) fn word_edges_sql(expr: &str) -> String {
    let class = word_class();
    format!(
        r"(CASE WHEN {expr} ~ '^[{class}]' THEN '(^|[^{class}])' ELSE '' END
          || regexp_replace({expr}, '([\\.^$|?*+()[\]{{}}])', '\\\1', 'g')
          || CASE WHEN {expr} ~ '[{class}]$' THEN '($|[^{class}])' ELSE '' END)"
    )
}

/// Current names other than the primary name, in the order first recorded. For the "also known as" line given to the arbiter and review cards.
pub async fn other_names(pool: &PgPool, entity_id: Uuid) -> AppResult<Vec<String>> {
    Ok(other_names_many(pool, &[entity_id])
        .await?
        .remove(&entity_id)
        .unwrap_or_default())
}

/// Keep deduplication and first-seen ordering within each entity. The KB join
/// lets the existing (kb_id, subject_id) indexes serve even a single-ID lookup.
pub(crate) async fn other_names_many(
    pool: &PgPool,
    entity_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<String>>> {
    if entity_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT entity_id, v FROM (
           SELECT DISTINCT ON (f.subject_id, lower(f.object_value->>'value'))
                  f.subject_id AS entity_id, f.object_value->>'value' AS v, f.recorded_at
             FROM entities e
             JOIN facts f ON f.subject_id = e.id AND f.kb_id = e.kb_id
             JOIN relation_types r ON r.id = f.predicate_id
            WHERE e.id = ANY($1) AND r.builtin AND r.key = $2
              AND f.invalidated_at IS NULL
              AND lower(f.object_value->>'value') <> lower(e.canonical_name)
            ORDER BY f.subject_id, lower(f.object_value->>'value'), f.recorded_at
         ) n ORDER BY n.entity_id, n.recorded_at",
    )
    .bind(entity_ids)
    .bind(KNOWN_AS)
    .fetch_all(pool)
    .await?;
    let mut names: HashMap<Uuid, Vec<String>> = HashMap::new();
    for (id, name) in rows {
        names.entry(id).or_default().push(name);
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::{contains_at_word_edges, numbers_differ, word_edges_pattern, word_edges_sql};

    /// (longer, shorter, whether it contains), shared by Rust and both SQL forms
    const WORD_EDGES: &[(&str, &str, bool)] = &[
        ("service_token", "token", false),
        ("install_scheduled_task.ps1", "task", false),
        ("areascorecard", "area", false),
        ("pumpkin soup", "pump", false),
        ("sherlock holmes", "holmes", true),
        ("mr. holmes", "holmes", true),
        ("install_scheduled_task.ps1", "install_scheduled_task", true),
        ("api/main.py", "main.py", true),
        ("café royal", "café", true),
        ("cafés", "café", false),
        ("zürich ag", "rich", false),
        ("issue#440", "#440", true),
        ("a.b+c (x) d", "b+c (x)", true),
        ("axb", "a.b", false),
        ("星云科技上海研究院", "上海研究院", true),
        ("nebula上海研究院", "上海研究院", true),
        ("openai中国", "openai", true),
        ("tokens token", "token", true),
        ("cafe\u{301}s", "cafe\u{301}", false),
        ("anything", "", true),
        ("x1.1.1", "1.1", true),
        (r"x [a]{2}\b|c? y", r"[a]{2}\b|c?", true),
    ];

    #[test]
    fn the_rust_rule_contains_a_latin_name_only_at_word_edges() {
        for &(longer, shorter, want) in WORD_EDGES {
            assert_eq!(
                contains_at_word_edges(longer, shorter),
                want,
                "{shorter} in {longer}"
            );
        }
    }

    #[tokio::test]
    async fn the_sql_rule_agrees_with_the_rust_rule() -> anyhow::Result<()> {
        let Some(url) = crate::test_db::url() else {
            return Ok(());
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        let sql = format!("SELECT $1 ~ $2, $1 ~ {}", word_edges_sql("$3"));
        for &(longer, shorter, want) in WORD_EDGES {
            let (by_pattern, by_sql): (bool, bool) = sqlx::query_as(&sql)
                .bind(longer)
                .bind(word_edges_pattern(shorter))
                .bind(shorter)
                .fetch_one(&pool)
                .await?;
            assert_eq!((by_pattern, by_sql), (want, want), "{shorter} in {longer}");
        }
        Ok(())
    }

    #[test]
    fn names_with_different_numbers_are_not_a_pair() {
        for (a, b) in [
            ("migration 394", "migration 395"),
            ("#440", "#441"),
            ("海洋探测器1号", "海洋探测器2号"),
            ("api/main.py:84-142", "api/main.py:145-169"),
            ("migration 39", "migration 395"),
            ("v1.2", "v1.3"),
        ] {
            assert!(numbers_differ(a, b), "{a} / {b}");
        }
    }

    #[test]
    fn the_same_numbers_or_none_on_one_side_still_pair() {
        for (a, b) in [
            ("海洋探测器1号", "海探1"),
            ("沧海", "沧海分布式推理平台 2.0"),
            ("migration 0106", "migration 106"),
            ("Ｑ３ ２０２４", "2024 Q3"),
            ("sección 01010101", "Sección 01010101 (Distrito Nacional)"),
            ("Sherlock Holmes", "Mr. Holmes"),
        ] {
            assert!(!numbers_differ(a, b), "{a} / {b}");
        }
    }
}
