//! Entity resolution v2: same name does not mean same person (the flowchart and the fixes from three measured runs are in `docs/pipeline.md`, section 2).
//!
//! Funnel: name candidate recall (free, including generic-suffix stem cross-matching) → profile vector similarity tiers (milliseconds, reusing the chunk embeddings from the ingestion stage)
//! → grey zone: create a new entity plus a suspected-duplicate review item (prefer splitting to merging); LLM batch adjudication runs in a separate background task,
//! with final human review as the fallback. The LLM is never on the critical path of extraction writes.
//!
//! Type drift: when a same name has no candidate under the same type, look up other types too (the same team being extracted as organization /
//! project / concept is the norm), routing by how strongly the type pair excludes each other: a concept fallback type goes through profile tiers as a recall candidate;
//! easily confused concrete types still create an entity but enqueue a review pair; hard exclusions (person vs organization) are kept fully apart.

use chrono::{DateTime, Utc};
use pgvector::Vector;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use utopia_core::models::{MergeLogView, ReviewBatchOutcome, ReviewItem, ReviewSide};
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// Context similarity threshold (an empirical cosine value for bge-m3-style models; tunable later).
/// ≥ ATTACH merges into the existing entity; < NEW is judged a different entity; in the grey zone in between, prefer splitting to merging, plus a review item.
pub const SIM_ATTACH: f32 = 0.55;
pub const SIM_NEW: f32 = 0.35;

/// Boundary for a same-name tie: if two **same-name** candidates' profile scores differ by no more than this, they count as "cannot be told apart".
/// When profiles cannot tell who is who, attaching to the higher scorer is just a coin toss over candidate order (#270) --
/// rather than toss a coin, merge neither and send both to human review. The value is deliberately tight: genuinely distinguishable same-named people (different
/// chunks, different accumulated facts) differ by far more than this, and only nearly identical centroids (e.g., seeded from the same chunk) trigger it.
pub const SIM_TIE_MARGIN: f32 = 0.02;

/// Minimum length of a corroborating object. Shorter ones ("IT", "A group") can appear in any document,
/// so a hit carries no information. **Better to miss a corroborating clue**: missing it is today's behavior, while getting it wrong would merge the wrong person.
const MIN_CORROBORATION_CHARS: usize = 2;

/// How many disambiguating objects to use per candidate as corroboration. More than one, because the document may mention not the top-ranked
/// one (his department in this sentence, his title in the next), but not all of them -- the more facts a candidate has,
/// the greater the chance of hitting a generic object.
const CORROBORATION_OBJECTS: i64 = 3;

/// Name normalization: full-width ASCII → half-width, full-width spaces → half-width, whitespace collapsed.
/// Returns the display form (case preserved); matching always applies SQL lower() again.
pub fn normalize_name(raw: &str) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| match c {
            '\u{3000}' => ' ',
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            _ => c,
        })
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Fold unambiguous Markdown formatting in names; evidence keeps the original text.
/// Input should already have passed through [`normalize_name`]. Only whole-name
/// `**...**` / `__...__` emphasis is removed; empty, nested, mismatched or
/// whitespace-padded markup is kept.
/// `__identifier__` stays literal (Unicode letters/digits and underscores count).
/// Single backticks are removed only around a slash path or a dotted filename
/// containing a letter: only letters, digits, `/`, `.`, `_` and `-` are allowed,
/// with at least one alphanumeric character and no trailing dot. This deliberately
/// leaves code, bare identifiers, numeric versions and paths with spaces alone.
pub fn fold_markdown_markers(name: &str) -> &str {
    for marker in ["**", "__", "`"] {
        let Some(inner) = name
            .strip_prefix(marker)
            .and_then(|s| s.strip_suffix(marker))
        else {
            continue;
        };
        if inner.is_empty()
            || inner.trim() != inner
            || inner.contains(['*', '`'])
            || (marker != "`"
                && (inner.starts_with('_') || inner.ends_with('_') || inner.contains("__")))
        {
            return name;
        }
        let fold = match marker {
            "__" => !inner.chars().all(|c| c.is_alphanumeric() || c == '_'),
            "`" => {
                (inner.contains('/')
                    || (inner.contains('.') && inner.chars().any(char::is_alphabetic)))
                    && inner.chars().any(char::is_alphanumeric)
                    && !inner.ends_with('.')
                    && inner
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
            }
            _ => true,
        };
        return if fold { inner } else { name };
    }
    name
}

/// Shared identity key for recall and document-level name claims.
pub fn name_key(raw: &str) -> String {
    fold_markdown_markers(&normalize_name(raw)).to_lowercase()
}

/// Generic suffix word list: Chinese ones are attached directly after the stem; English ones are treated as separate words, and both word orders are accepted
/// ("Phoenix Project" / "Project Phoenix"). Affects recall only; judgment still goes through profile similarity.
const GENERIC_SUFFIXES_CJK: &[&str] = &["项目", "公司", "集团", "部门", "团队"];
const GENERIC_WORDS_EN: &[&str] = &["project", "corp", "inc", "team"];

/// Stem: the lowercase form after stripping one generic suffix. Returns None when nothing matched, when stripping leaves nothing, or when the stem is
/// itself a generic word ("project team"). Input should already have passed normalize_name.
pub fn name_stem(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let strip_punct = |w: &str| w.trim_end_matches(['.', ',']).to_string();
    let generic = |s: &str| {
        GENERIC_SUFFIXES_CJK.contains(&s) || GENERIC_WORDS_EN.contains(&strip_punct(s).as_str())
    };
    for suf in GENERIC_SUFFIXES_CJK {
        if let Some(stem) = lower.strip_suffix(suf) {
            let stem = stem.trim_end();
            if !stem.is_empty() && !generic(stem) {
                return Some(stem.to_string());
            }
        }
    }
    let words: Vec<&str> = lower.split(' ').collect();
    if words.len() >= 2 {
        if generic(words[words.len() - 1]) {
            let stem = words[..words.len() - 1].join(" ");
            // "Acme, Inc.": the comma before the suffix is punctuation style, not part of the name
            let stem = stem.trim_end_matches(',');
            if !stem.is_empty() && !generic(stem) {
                return Some(stem.to_string());
            }
        }
        if generic(words[0]) {
            let stem = words[1..].join(" ");
            if !generic(&stem) {
                return Some(stem);
            }
        }
    }
    None
}

/// Input should already have passed [`normalize_name`].
/// The set of recall keys for a mention (all lowercase): the bare name + stem + augmentation by the stem's generic suffixes.
/// The augmentation covers the reverse direction (the library has "Stardust Project" and the mention only says "Stardust"); when the stem contains CJK it appends the Chinese
/// suffix, otherwise the English word (both orders). At most 11 keys, looked up as multiple points on the (kb, type, lower(name)) index.
pub fn recall_keys(normalized: &str) -> Vec<String> {
    let name = fold_markdown_markers(normalized);
    let lower = name.to_lowercase();
    let base = name_stem(name).unwrap_or_else(|| lower.clone());
    let mut keys = vec![lower];
    fn add(keys: &mut Vec<String>, k: String) {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    // Keep exact recall of names already stored with formatting as well.
    add(&mut keys, normalized.to_lowercase());
    add(&mut keys, base.clone());
    if base.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)) {
        for suf in GENERIC_SUFFIXES_CJK {
            add(&mut keys, format!("{base}{suf}"));
        }
    } else {
        for w in GENERIC_WORDS_EN {
            add(&mut keys, format!("{base} {w}"));
            add(&mut keys, format!("{w} {base}"));
        }
    }
    keys
}

// ---------------------------------------------------------------------------
// Type drift: the same-named entity was extracted as different types ("Orion platform team" ↔ organization / project)
// ---------------------------------------------------------------------------

/// Easily confused concrete types: extraction often wavers among these (is a team an organization or a project? is a platform a project or a product?).
/// A same name across this group of types → create an entity (prefer splitting to merging), but the enqueued review pair goes to the LLM or a person for adjudication.
///
/// **The ontology decides; this table is only the fallback when nothing is declared.** To judge whether two classes can refer to the same thing, first look at
/// `owl:disjointWith` (`entity_type_disjoint`, including inheritance: Person ⟂ Organization implies
/// Corporation ⟂ Person) -- anything declared mutually exclusive is kept apart, even if they sit in one family on the class hierarchy;
/// for undeclared pairs, look at the class hierarchy (same branch counts as confusable, #226); only last are these three hard keys used.
/// In a library with no package installed and nothing declared, these three keys do not exist, so every cross-type same name is judged `Disjoint` --
/// it errs toward strictness, never toward wrongly merging (0016 B3).
pub const CONFUSABLE_TYPE_KEYS: &[&str] = &["organization", "project", "product"];

/// The maximum number of drift review pairs enqueued per resolution (to keep a large same-name group from flooding the review queue).
const MAX_DRIFT_REVIEWS: usize = 4;

/// Disposition classification for a same name across types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeDrift {
    /// One side is the fallback type: it is a recall candidate and goes through profile-similarity tiers (may ATTACH)
    Recall,
    /// Two easily confused concrete types: create + review pair
    Review,
    /// Hard exclusion (person vs organization etc., including unknown custom types): kept fully apart
    Disjoint,
}

fn classify_type_drift(a: Option<&str>, b: Option<&str>) -> TypeDrift {
    // **The same type can of course be the same thing.**
    //
    // This tier did not exist originally, because this function was born only to serve "type drift" -- a same name extracted as two
    // types -- where identical types on both sides never happens at all. Later containment_reviews borrowed it as a compatibility
    // criterion, and there **identical types on both sides is the most common case**, so person against person fell into
    // the last line, Disjoint, and was read as "can never be the same thing".
    //
    // The cost is that the most obvious coreference pairs in the text could not enter the queue: in the first six Holmes stories,
    // Sherlock Holmes and Holmes were two entities, and only 14 of 488 entities were merged.
    // All 12 original unit tests covered cross-type, and not one covered the same type.
    if a == b {
        return TypeDrift::Recall;
    }
    // **One side has not been judged yet** → it is a recall candidate and goes through profile-similarity tiers.
    // It used to compare against `== FALLBACK_TYPE_KEY`, a key that no longer exists:
    // "not judged yet" is now expressed as `None` (0009). When both sides are None,
    // it is already caught by the `a == b` line above.
    let (Some(a), Some(b)) = (a, b) else {
        return TypeDrift::Recall;
    };
    if CONFUSABLE_TYPE_KEYS.contains(&a) && CONFUSABLE_TYPE_KEYS.contains(&b) {
        return TypeDrift::Review;
    }
    TypeDrift::Disjoint
}

fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

#[derive(Debug, sqlx::FromRow)]
struct Candidate {
    id: Uuid,
    canonical_name: String,
    type_id: Option<Uuid>,
    exact_name: bool,
    profile_embedding: Option<Vector>,
    profile_n: i32,
}

/// Each entity's own degree (not counting name facts), in the same order as `ids`; zero for entities with no facts.
async fn degrees(pool: &PgPool, kb_id: Uuid, ids: &[Uuid]) -> AppResult<Vec<i64>> {
    let rows: HashMap<Uuid, i64> = sqlx::query_as(&format!(
        "SELECT n.id, count(*)
           FROM unnest($2::uuid[]) AS n(id)
           JOIN facts f ON f.kb_id = $1 AND (f.subject_id = n.id OR f.object_id = n.id)
                       AND f.invalidated_at IS NULL AND {not_name}
          GROUP BY n.id",
        not_name = crate::names::not_a_name("f"),
    ))
    .bind(kb_id)
    .bind(ids)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    Ok(ids
        .iter()
        .map(|id| rows.get(id).copied().unwrap_or(0))
        .collect())
}

/// Resolution result: which entity the mention landed on, plus the review pairs to enqueue
/// (same-name grey zone / type drift), which the caller writes into the review queue, triggering the adjudication task.
#[derive(Debug)]
pub struct Resolution {
    pub entity_id: Uuid,
    pub created: bool,
    pub reviews: Vec<ReviewRequest>,
}

/// A review pair awaiting enqueue: `Resolution::entity_id` vs `other_id`.
#[derive(Debug)]
pub struct ReviewRequest {
    pub other_id: Uuid,
    pub score: f32,
    pub reason: String,
    /// Who adjudicates: profile grey zones go to [`ReviewStage::Adjudicating`] (the batch adjudicator),
    /// and same-name ties that only a person can tell apart go to [`ReviewStage::Human`].
    pub stage: ReviewStage,
}

/// Who consumes the review item: an ordinary profile grey zone goes to the batch adjudicator first; a same-name entity that the extractor explicitly
/// split out in the same reply can only be judged by a person, so nearly identical profiles must not trigger automatic merging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStage {
    Adjudicating,
    Human,
}

impl ReviewStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Adjudicating => "adjudicating",
            Self::Human => "human",
        }
    }
}

/// Resolve one mention. Recall goes through two channels (0041 decision 3): literal name equality (channel 1,
/// the SQL in `resolve_by_name`) and the nearest neighbours of the name vector (channel 2, used only when `name_vector` is given).
///
/// **Channel 2 only proposes, it does not decide.** The entities it recalls do not take part in merging at this step -- deciding should be done by evidence,
/// which is step 3 of 0041 and not yet built -- it only queues a pair for the adjudicator (`name_vector|<cosine>`), letting it compare two
/// profiles and precedents to judge whether they are one. So this step adds "ask one more question", not "merge one more time": a wrong merge
/// is silent and must be undone by a person later, while asking more only costs a bit more.
///
/// The same literal name does not go through channel 2: that is channel 1's territory, which has already judged it by profile, and queuing it again would make
/// the adjudicator reconsider a decision just made. Pairs whose broad categories do not match are not proposed either (Sea Probe 1 is a device, it cannot be a person).
#[allow(clippy::too_many_arguments)]
pub async fn resolve_mention(
    pool: &PgPool,
    kb_id: Uuid,
    type_id: Option<Uuid>,
    raw_name: &str,
    context: Option<&[f32]>,
    // The name vector of the mention itself (not of a chunk). None = no embedding model is configured, or it was not computed this time
    name_vector: Option<&[f32]>,
    text: Option<&str>,
    exclude: &[Uuid],
) -> AppResult<Resolution> {
    let mut r = resolve_by_name(pool, kb_id, type_id, raw_name, context, text, exclude).await?;
    if let Some(query) = name_vector {
        propose_by_name_vector(pool, kb_id, type_id, raw_name, query, exclude, &mut r).await?;
    }
    // Pairs already judged "not the same" are not queued again. A newly created entity has no past, so there is nothing to queue and nothing to ask
    if !r.created && !r.reviews.is_empty() {
        let kept = kept_apart_from(pool, kb_id, r.entity_id).await?;
        r.reviews.retain(|v| !kept.contains(&v.other_id));
    }
    Ok(r)
}

/// Those entities that were judged "not the same" as this entity.
///
/// **Every mention goes through recall**: the name-vector channel does not distinguish creating from merging, and the same-name confusable-type path also
/// keeps queuing after a merge. But the unique index on the review table covers only pending rows -- a judged pair, when mentioned again next time,
/// becomes a new pending row. On a library with 60,000 entities, every time `migration 395` appears in a new document, it re-queues against its eight
/// neighbours such as `migration 394`, no matter how many times the adjudicator (or a person) has separated them
/// (#1104). The decision to separate must be remembered; [`crate::names::pair_shared_name`] has always done this.
///
/// A person can still merge a pair that was separated, on the review page: what is blocked here is automatic re-proposal, not the merge itself.
async fn kept_apart_from(pool: &PgPool, kb_id: Uuid, entity_id: Uuid) -> AppResult<HashSet<Uuid>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT CASE WHEN left_id = $2 THEN right_id ELSE left_id END
           FROM resolution_reviews
          WHERE kb_id = $1 AND status = 'kept' AND (left_id = $2 OR right_id = $2)",
    )
    .bind(kb_id)
    .bind(entity_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// The holder query for [`resolve_identity_name`]: `$1` library, `$2` declared class, `$3` the lowercase forms.
/// It is a separate function so that the plan test can EXPLAIN the same text.
pub(crate) fn identity_holders_sql() -> String {
    format!(
        "SELECT e.id, e.canonical_name, e.type_id, e.profile_embedding, e.profile_n,
                TRUE AS exact_name
           FROM entities e
          WHERE e.kb_id = $1 AND e.merged_into IS NULL AND e.description IS NULL
            AND (e.type_id IS NULL OR e.type_id = $2)
            AND {named}",
        named = crate::names::has_name_or_alias_in("e", 1, 3),
    )
}

/// 0071 decisions 2 and 3: when exactly one live same-name holder exists (class being the declared class or no class), merge straight in without looking at profiles;
/// if there is none, create a new one, even if this name already exists under another class (type-drift review still keeps it); two or more take the original profile path.
/// Holders are matched by bare name or by name fact (a merge moves the loser's name to the survivor as known_as).
pub async fn resolve_identity_name(
    pool: &PgPool,
    kb_id: Uuid,
    type_id: Option<Uuid>,
    raw_name: &str,
    context: Option<&[f32]>,
    text: Option<&str>,
    exclude: &[Uuid],
) -> AppResult<Resolution> {
    let normalized = normalize_name(raw_name);
    let name = fold_markdown_markers(&normalized);
    let spellings = vec![normalized.to_lowercase(), name.to_lowercase()];
    let holders: Vec<Candidate> = sqlx::query_as(&identity_holders_sql())
        .bind(kb_id)
        .bind(type_id)
        .bind(&spellings)
        .fetch_all(pool)
        .await?
        .into_iter()
        .filter(|holder: &Candidate| !exclude.contains(&holder.id))
        .collect();
    match holders.as_slice() {
        [holder] => {
            match context {
                Some(ctx) => update_profile(pool, holder.id, holder.profile_n, ctx).await?,
                None => touch_entity(pool, holder.id).await?,
            }
            if holder.type_id.is_none() {
                adopt_type(pool, kb_id, holder.id, &holder.canonical_name, type_id).await?;
            }
            Ok(Resolution {
                entity_id: holder.id,
                created: false,
                reviews: Vec::new(),
            })
        }
        [] => {
            let keys = recall_keys(&normalized);
            resolve_type_drift(pool, kb_id, type_id, name, &keys, context, exclude, false).await
        }
        _ => resolve_by_name(pool, kb_id, type_id, raw_name, context, text, exclude).await,
    }
}

/// Channel 2: the nearest neighbours of the mention's name vector, each queued as a pair (rules as in [`resolve_mention`])
async fn propose_by_name_vector(
    pool: &PgPool,
    kb_id: Uuid,
    type_id: Option<Uuid>,
    raw_name: &str,
    query: &[f32],
    exclude: &[Uuid],
    r: &mut Resolution,
) -> AppResult<()> {
    let mention_name = name_key(raw_name);
    let mention_family = match type_id {
        Some(t) => type_label(pool, t)
            .await?
            .as_deref()
            .and_then(crate::governance::type_family),
        None => None,
    };
    let disjoint = declared_disjoint_from(pool, kb_id, type_id).await?;
    let mut seen: HashSet<Uuid> = r.reviews.iter().map(|v| v.other_id).collect();
    seen.insert(r.entity_id);
    seen.extend(exclude.iter().copied());
    for near in crate::name_vectors::nearest(pool, kb_id, query, crate::name_vectors::TOP_K).await?
    {
        if near.similarity < crate::name_vectors::SIM_FLOOR {
            break; // break; // descending order: the rest are farther
        }
        if near.name.to_lowercase() == mention_name || !seen.insert(near.entity_id) {
            continue;
        }
        // Name vectors that differ by only one digit almost coincide, yet are almost never the same (see `numbers_differ`)
        if crate::names::numbers_differ(raw_name, &near.name) {
            continue;
        }
        let near_family = near
            .type_label
            .as_deref()
            .and_then(crate::governance::type_family);
        if let (Some(a), Some(b)) = (mention_family, near_family) {
            if a != b {
                continue;
            }
        }
        // Classes that cannot be recognized by broad category (Spec, Test...) are separated by ontology declarations, in the same table as containment and type drift
        if near.type_id.is_some_and(|t| disjoint.contains(&t)) {
            continue;
        }
        r.reviews.push(ReviewRequest {
            other_id: near.entity_id,
            score: near.similarity,
            reason: format!(
                "{}{:.2}",
                utopia_core::review_reasons::NAME_VECTOR,
                near.similarity
            ),
            stage: ReviewStage::Adjudicating,
        });
    }
    Ok(())
}

async fn type_label(pool: &PgPool, type_id: Uuid) -> AppResult<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT label FROM entity_types WHERE id = $1")
            .bind(type_id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Channel 1: candidates with a literally equal name, merged or created by profile tiers (all of the original `resolve_mention`).
/// `context` is the vector of the chunk the mention sits in (None when there is no embedding model, degrading to v1 behavior:
/// a same name merges into the candidate with the most facts).
#[allow(clippy::too_many_arguments)]
async fn resolve_by_name(
    pool: &PgPool,
    kb_id: Uuid,
    // None = the type the extractor gave is not in the ontology, or the library has no class at all (0009)
    type_id: Option<Uuid>,
    raw_name: &str,
    context: Option<&[f32]>,
    // Grounded quotes owned by this mention's subject, supplied by the extractor.
    // A whole chunk can mention another person's department and is not identity evidence.
    text: Option<&str>,
    // Candidates already claimed by a different response-local handle. They remain stored
    // and recallable on later calls, but this mention cannot attach to them.
    exclude: &[Uuid],
) -> AppResult<Resolution> {
    let normalized = normalize_name(raw_name);
    // Recall keys = the bare name + the generic-suffix stem and its augmentations ("Stardust" ↔ "Stardust Project" are candidates for each other).
    // This only widens recall; whether to merge is still decided by the profile-similarity tiers below.
    let keys = recall_keys(&normalized);
    // Store the same spelling whichever form is mentioned first.
    let name = fold_markdown_markers(&normalized);
    // Name recall reads name facts (0041 decision 1): the bare name, short names and former names are each one row in `known_as`.
    // Degree does not count name facts -- a name is not "a thing about it", and counting it in would give every entity a phantom extra
    // Keep degree counting lazy, including when corroboration widens exact-name recall.
    let mut candidates: Vec<Candidate> = sqlx::query_as(&format!(
        "SELECT e.id, e.canonical_name, e.type_id, e.profile_embedding, e.profile_n,
                (lower(e.canonical_name) = ANY($4) OR {exact_named}) AS exact_name
         FROM entities e
         -- IS NOT DISTINCT FROM 而不是 =（0009 的那个陷阱）：开放图谱里的实体都没有类
         -- （类由对齐来定），`type_id = NULL` 永远不成立，同名的它就永远撞不上——
         -- 实测一个库里 Securities and Exchange Commission 与它的全大写写法成了两个实体
         WHERE e.kb_id = $1 AND e.merged_into IS NULL
           AND (e.type_id IS NOT DISTINCT FROM $2
                OR ($5 AND ($2::uuid IS NULL OR e.type_id IS NULL)
                    AND (lower(e.canonical_name) = ANY($4) OR {exact_named})))
           -- 被描述的东西没有名字（0044）：它的 canonical_name 只是显示用的描述，
           -- 不是召回的桥——两篇文档里描述得一样的两个东西不能因此接到一起
           AND e.description IS NULL
           AND {named}",
        named = crate::names::has_name_or_alias_in("e", 1, 3),
        exact_named = crate::names::has_name_in("e", 1, 4),
    ))
    .bind(kb_id)
    .bind(type_id)
    .bind(&keys)
    // Match both the folded spelling and already-stored formatting, as recall does.
    .bind(vec![name.to_lowercase(), normalized.to_lowercase()])
    .bind(text.is_some())
    .fetch_all(pool)
    .await?
    .into_iter()
    .filter(|candidate: &Candidate| !exclude.contains(&candidate.id))
    .collect();

    // A clear mention-local clue outranks the chunk's profile, even outside the tie
    // margin. Include exact names whose class is not yet known on either side, but
    // do not let that wider recall change profile fallback or classify the winner.
    let tier_evidence = if let Some(text) = text {
        let exact: Vec<_> = candidates.iter().filter(|c| c.exact_name).collect();
        if exact.len() > 1 {
            if let Some(c) = corroborating_candidate(pool, kb_id, &exact, text).await? {
                if let Some(ctx) = context {
                    update_profile(pool, c.id, c.profile_n, ctx).await?;
                } else {
                    touch_entity(pool, c.id).await?;
                }
                return Ok(Resolution {
                    entity_id: c.id,
                    created: false,
                    reviews: Vec::new(),
                });
            }
            // Shared/conflicting clues must not become unique by retrying a smaller
            // subset (the best two profiles, or only candidates above SIM_NEW).
            None
        } else {
            Some(text)
        }
    } else {
        None
    };
    candidates.retain(|c| c.type_id == type_id);

    if candidates.is_empty() {
        // Same type with no candidate ≠ new name: type labels drift (the same team extracted as organization / project /
        // concept), so first look up same-named entities under other types, and route by how strongly the type pair excludes each other.
        return resolve_type_drift(pool, kb_id, type_id, name, &keys, context, exclude, true).await;
    }

    let Some(ctx) = context else {
        // No vector to compare: v1 compatibility -- merge into the same-name candidate with the most facts
        let best = match candidates.as_slice() {
            [only] => only,
            _ => {
                let ids: Vec<Uuid> = candidates.iter().map(|c| c.id).collect();
                let degree = degrees(pool, kb_id, &ids).await?;
                candidates
                    .iter()
                    .zip(degree)
                    .max_by_key(|(_, d)| *d)
                    .map(|(c, _)| c)
                    .expect("non-empty")
            }
        };
        touch_entity(pool, best.id).await?;
        return Ok(Resolution {
            entity_id: best.id,
            created: false,
            reviews: Vec::new(),
        });
    };

    // Everything this call **must never merge into**: what the caller names as excluded (`exclude`), plus the same-name candidates this round
    // has already seen and would judge "not the same". Whichever branch below decides to create new, this list must be handed to `create_entity` --
    // the lookup inside the lock fetches by name, and without the list it would fetch them back
    let weighed: Vec<Uuid> = exclude
        .iter()
        .copied()
        .chain(candidates.iter().map(|c| c.id))
        .collect();

    // Candidates with profiles compute similarity; candidates without a profile (historical data, or created while no embedding model existed) are classified separately
    let mut scored: Vec<(&Candidate, f32)> = Vec::new();
    let mut unprofiled: Vec<&Candidate> = Vec::new();
    for c in &candidates {
        match c
            .profile_embedding
            .as_ref()
            .and_then(|p| cosine(p.as_slice(), ctx))
        {
            Some(sim) => scored.push((c, sim)),
            None => unprofiled.push(c),
        }
    }
    // Highest-scoring candidate: on a tie, keep the one met first (consistent with the old strict `sim > s`)
    let best_scored = scored
        .iter()
        .copied()
        .reduce(|acc, cur| if cur.1 > acc.1 { cur } else { acc });

    if let Some((best, sim)) = best_scored {
        if sim >= SIM_ATTACH {
            // Same-name tie grey zone: another **same-name** candidate has a score close to the top (within SIM_TIE_MARGIN), so
            // profiles cannot tell who is who -- attaching to the higher scorer is just a coin toss over candidate order (#270).
            // Prefer splitting to merging: do not attach; create a new entity, and enter one **manual** review pair for each of the two tied candidates.
            // Blocked regardless of how high the scores are: a tie at high scores is exactly where silent wrong merging happens, and "similar enough" must not let it through.
            if let Some((runner, r_sim)) = scored
                .iter()
                .copied()
                .filter(|(c, s)| {
                    // Same-name comparison must ignore case, as recall does: recall uses SQL `lower()`, and
                    // "Zhang Wei" and "zhang wei" are already a same-name candidate pair, so this must not be missed.
                    c.id != best.id
                        && c.canonical_name.to_lowercase() == best.canonical_name.to_lowercase()
                        && sim - *s <= SIM_TIE_MARGIN
                })
                .reduce(|acc, cur| if cur.1 > acc.1 { cur } else { acc })
            {
                // Before flipping a coin on profiles, ask the facts (#331): does this sentence mention one of the candidates'
                // department or title? If so, it is not a coin toss, it is a clue
                if let Some(t) = tier_evidence {
                    if let Some(c) =
                        corroborating_candidate(pool, kb_id, &[best, runner], t).await?
                    {
                        update_profile(pool, c.id, c.profile_n, ctx).await?;
                        return Ok(Resolution {
                            entity_id: c.id,
                            created: false,
                            reviews: Vec::new(),
                        });
                    }
                }
                let (id, created) =
                    create_entity(pool, kb_id, type_id, name, context, &weighed).await?;
                if !created {
                    // Another document being processed in parallel just created it: use that one, and the review pair is queued by it too
                    return Ok(Resolution {
                        entity_id: id,
                        created: false,
                        reviews: Vec::new(),
                    });
                }
                refresh_disambiguators(pool, kb_id, name).await?;
                let reviews = [(best, sim), (runner, r_sim)]
                    .into_iter()
                    .map(|(c, s)| ReviewRequest {
                        other_id: c.id,
                        score: s,
                        reason: format!("namesake_tie|{s:.2}"),
                        stage: ReviewStage::Human,
                    })
                    .collect();
                return Ok(Resolution {
                    entity_id: id,
                    created: true,
                    reviews,
                });
            }
            update_profile(pool, best.id, best.profile_n, ctx).await?;
            return Ok(Resolution {
                entity_id: best.id,
                created: false,
                reviews: Vec::new(),
            });
        }
    }
    let unprofiled = match unprofiled.as_slice() {
        [] => None,
        [only] => Some(*only),
        many => {
            let ids: Vec<Uuid> = many.iter().map(|c| c.id).collect();
            let degree = degrees(pool, kb_id, &ids).await?;
            many.iter()
                .copied()
                .zip(degree)
                .reduce(|acc, cur| if cur.1 > acc.1 { cur } else { acc })
                .map(|(c, _)| c)
        }
    };
    if let Some(c) = unprofiled {
        // Candidates without a profile cannot be told apart: v1-compatible merge, and initialize the profile with this context
        update_profile(pool, c.id, c.profile_n, ctx).await?;
        return Ok(Resolution {
            entity_id: c.id,
            created: false,
            reviews: Vec::new(),
        });
    }

    // Reached here: all candidates have profiles and the top score is < ATTACH → create a new entity (same name, different person).
    //
    // But first ask the facts once (#331). The grey zone means "the vectors say it is unclear", not "it is definitely a new person" --
    // if this sentence happens to name one candidate's department or title, that is more reliable than the cosine value.
    // **Only the grey zone counts** (≥ SIM_NEW): candidates scoring below it are not even worth queuing as a review pair,
    // and using one corroborating clue to pull one in as the same person would bypass the threshold rather than supplement it.
    if let Some(t) = tier_evidence {
        let in_grey: Vec<&Candidate> = scored
            .iter()
            .filter(|(_, s)| *s >= SIM_NEW)
            .map(|(c, _)| *c)
            .collect();
        if let Some(c) = corroborating_candidate(pool, kb_id, &in_grey, t).await? {
            update_profile(pool, c.id, c.profile_n, ctx).await?;
            return Ok(Resolution {
                entity_id: c.id,
                created: false,
                reviews: Vec::new(),
            });
        }
    }
    let (id, created) = create_entity(pool, kb_id, type_id, name, context, &weighed).await?;
    if !created {
        // Another document being processed in parallel just created it: use that one, and the review pair is queued by it too
        return Ok(Resolution {
            entity_id: id,
            created: false,
            reviews: Vec::new(),
        });
    }
    refresh_disambiguators(pool, kb_id, name).await?;
    let mut reviews = best_scored
        .filter(|(_, sim)| *sim >= SIM_NEW)
        .map(|(c, sim)| {
            vec![ReviewRequest {
                other_id: c.id,
                score: sim,
                reason: format!("ambiguous_name|{sim:.2}"),
                stage: ReviewStage::Adjudicating,
            }]
        })
        .unwrap_or_default();
    // Existing entities whose names contain each other: equality recall cannot see them (prefix enumeration is incomplete),
    // so a short name would silently become a second entity. Queue only, do not merge
    reviews.extend(containment_reviews(pool, kb_id, type_id, name, id, context).await?);
    Ok(Resolution {
        entity_id: id,
        created: true,
        reviews,
    })
}

/// **Fact corroboration**: does the mention's original text contain a candidate's disambiguating object (#331)?
///
/// When profile vectors cannot tell who is who, the clue that separates them is often in the facts: one Zhang Wei `works_for`
/// the platform engineering department, another `works_for` the finance department, and this sentence says "finance". On the review card a person sees
/// exactly these facts (`top_facts`); this step hands back to code what the person does.
///
/// **Evidence only runs one way.** A match is weak evidence for "the same person"; a mismatch is **nothing** --
/// a person who works at Acme and part-time at Zenith has two `works_for` values, and is still one person. So
/// this function only answers "which candidate the original text mentions", and never answers "which candidate is ruled out".
///
/// **Counts only when exactly one candidate hits.** If both hit, the clue cannot tell them apart either,
/// so rather than picking one, keep things as they are.
///
/// The object is picked the same way as in `refresh_disambiguators` (since #299 it is chosen from the ontology's declared range,
/// not from a word list), and **excludes the ones a same-name peer also points at**: when two Zhang Weis work at the same company,
/// the company name is no help at all for telling them apart, yet it is the easiest thing to hit in a document.
async fn corroborating_candidate<'a>(
    pool: &PgPool,
    kb_id: Uuid,
    candidates: &[&'a Candidate],
    text: &str,
) -> AppResult<Option<&'a Candidate>> {
    // A single candidate also counts: "no other candidate hit" holds trivially when there is only one candidate
    if candidates.is_empty() || text.trim().is_empty() {
        return Ok(None);
    }
    let ids: Vec<Uuid> = candidates.iter().map(|c| c.id).collect();
    // Read all candidates in one query: for each candidate, take a few object names that its peers do not point at.
    //
    // **LEFT JOIN relation_types** (0044): an open statement has no predicate, only the phrase copied as-is,
    // and the object it points at is just as much this candidate's profile -- "acquired Beta" does not stop counting as corroboration
    // just because the ontology failed to catch "acquired". In the ordering, the three boolean bits of the relation are NULL for open rows; COALESCE
    // turns them into false so they sort after the typed rows: the order among typed rows stays exactly the same
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT s.subject_id, s.name FROM (
           SELECT f.subject_id, o.canonical_name AS name,
                  row_number() OVER (
                    PARTITION BY f.subject_id
                    ORDER BY EXISTS (SELECT 1 FROM relation_type_ranges rr
                                      WHERE rr.relation_type_id = r.id) DESC,
                             COALESCE(r.temporal = 'state', FALSE) DESC,
                             COALESCE(r.functional, FALSE) DESC,
                             f.confidence DESC, f.recorded_at DESC
                  ) AS rn
           FROM facts f
           LEFT JOIN relation_types r ON r.id = f.predicate_id
           JOIN entities o ON o.id = f.object_id
           WHERE f.kb_id = $1 AND f.subject_id = ANY($2)
             AND f.invalidated_at IS NULL AND f.object_id IS NOT NULL
             -- 同名同伴也指着的宾语没有分辨力，排除
             AND NOT EXISTS (SELECT 1 FROM facts g
                              WHERE g.kb_id = $1 AND g.subject_id = ANY($2)
                                AND g.subject_id <> f.subject_id
                                AND g.object_id = f.object_id
                                AND g.invalidated_at IS NULL)
         ) s WHERE s.rn <= $3",
    )
    .bind(kb_id)
    .bind(&ids)
    .bind(CORROBORATION_OBJECTS)
    .fetch_all(pool)
    .await?;

    let haystack = text.to_lowercase();
    let mut hit: Option<&'a Candidate> = None;
    for (subject_id, object_name) in &rows {
        let needle = object_name.trim().to_lowercase();
        if needle.chars().count() < MIN_CORROBORATION_CHARS || !haystack.contains(&needle) {
            continue;
        }
        let Some(c) = candidates.iter().find(|c| c.id == *subject_id) else {
            continue;
        };
        match hit {
            // The same candidate's second object also hits: still the same candidate, not a disagreement
            Some(prev) if prev.id == c.id => {}
            // A second candidate was hit: this clue cannot tell them apart
            Some(_) => return Ok(None),
            None => hit = Some(c),
        }
    }
    Ok(hit)
}

/// The shortest side of a containment candidate: the **shorter** of the two names must be at least this long to count.
/// Below it are mostly generic names like "Research Institute" or "Center", which pair with no information and only flood the queue.
const MIN_CONTAIN_CHARS: i32 = 4;

/// The maximum number of containment review pairs produced at once. Same reasoning as `MAX_DRIFT_REVIEWS`:
/// a generic name may be contained in dozens of entities, and including them all would flood the queue.
const MAX_CONTAIN_REVIEWS: usize = 4;

/// Fetch a few extra rows on the SQL side: the hard-exclusive types are filtered out only on the Rust side,
/// and if only 4 rows were fetched, all 4 might be exclusive types, with the real pair cut off by the LIMIT.
const CONTAIN_SCAN_LIMIT: i64 = 16;

/// The name is used as a literal substring in LIKE: without escaping, a `_` in the name would match any single character (#1293)
fn like_escaped(name: &str) -> String {
    name.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// After creating an entity, find existing entities whose **names contain each other**, as review candidates.
///
/// Chinese business text turns a full name into a short name within a single document (e.g. "Xingyun Technology Shanghai Research Institute" → "Shanghai Research Institute"),
/// and [`recall_keys`] is an equality lookup: it catches the reverse direction only by enumerating generic suffixes, but the prefix can be any organization name,
/// so **enumeration cannot be complete**. As a result all three kinds slip through and silently become a second entity:
///
/// ```text
/// Shanghai Research Institute       ⊂ Xingyun Technology Shanghai Research Institute   suffix bounded
/// Qiming X7 Accelerator Card        ~ Qiming X7 Inference Accelerator Card            a word inserted in the middle (LIKE cannot cover it, see below)
/// Canghai                           ⊂ Canghai Distributed Inference Platform 2.0       prefix extended
/// ```
///
/// **Only proposes candidates, never merges automatically.** The same-name candidate path merges straight in when similarity ≥ [`SIM_ATTACH`],
/// but containment must never take that path: `Huarui Group Technology Center` and `Xingyun Technology Technology Center` both contain "Technology Center",
/// and contextual similarity within the same document easily clears the bar, yet they are two departments. Prefer splitting to merging.
///
/// **Other names take part too**: a merge moves the name facts onto the survivor, so looking up only `canonical_name` would
/// cost one recall bridge for every successful merge -- once `Holmes` merges into `Sherlock Holmes`,
/// a later `Mr. Holmes` can no longer connect (neither contains the other with `Sherlock Holmes`).
/// The more merges succeed, the more gets missed: a hole that makes itself worse.
///
/// **Known not covered**: the case where two names neither contain each other nor share a common alias to bridge them
///(`Qiming X7 Accelerator Card` and `Qiming X7 Inference Accelerator Card`). That needs trigram similarity, and
/// `CREATE EXTENSION pg_trgm` requires superuser rights -- this repo connects as a restricted role (see `migrations/0010_least_privilege_role.sql`),
/// so installing the extension would fail on deployment. Left for later, when needed.
///
/// **Performance**: starts from this library's `known_as` facts (through `facts_live_predicate_idx`), joins back to entities by id,
/// read in one pass, with no per-entity correlated subquery. LIKE itself cannot use an index, so it is still one scan per name;
/// it runs only **when an entity is created**, not on every mention. On a large library, if that is not enough, the right fix is a "suffix key" table
/// with equality lookup, not a fuzzy index.
/// One row of the containment scan: (id, bare name, type key, type id, profile). The type id is used to compare the ontology's declared disjointness
type ContainRow = (Uuid, String, Option<String>, Option<Uuid>, Option<Vector>);

async fn containment_reviews(
    pool: &PgPool,
    kb_id: Uuid,
    // None = the type the extractor gave is not in the ontology, or the library has no class at all (0009)
    type_id: Option<Uuid>,
    name: &str,
    new_id: Uuid,
    ctx: Option<&[f32]>,
) -> AppResult<Vec<ReviewRequest>> {
    let lower = name.to_lowercase();
    if lower.chars().count() < MIN_CONTAIN_CHARS as usize {
        return Ok(Vec::new());
    }
    // The entity may not have been typed yet (0009); in that case there is no key to look up
    let mention_key: Option<String> = match type_id {
        Some(t) => sqlx::query_as::<_, (String,)>("SELECT key FROM entity_types WHERE id = $1")
            .bind(t)
            .fetch_optional(pool)
            .await?
            .map(|(k,)| k),
        None => None,
    };
    // **No filtering by type**: short names often fall into the concept fallback, while the full name is a concrete type
    //(measured: Shanghai Research Institute → concept vs Xingyun Technology Shanghai Research Institute → organization,
    //  Qiming X7 Accelerator Card → concept vs Qiming X7 Inference Accelerator Card → product),
    //  so looking up by type equality would find neither pair. Compatibility is left to classify_type_drift below.
    //  Fetch more rows, because the hard-exclusive ones are filtered out on the Rust side
    // The classes the ontology declares as exclusive with this class (including inheritance), fetched in one go and compared row by row by id
    let disjoint = declared_disjoint_from(pool, kb_id, type_id).await?;
    // The third column is nullable: unclassified entities also take part in the containment scan (0009)
    let rows: Vec<ContainRow> = sqlx::query_as(&format!(
        "SELECT e.id, e.canonical_name, t.key, e.type_id, e.profile_embedding
         FROM entities e LEFT JOIN entity_types t ON t.id = e.type_id
         WHERE e.id IN (
             -- **从名字事实出发，不逐个实体去探。** 本名和别名都是 `known_as` 事实，一条路覆盖两种。
             -- 合并搬走名字事实后，`Holmes` 并入 `Sherlock Holmes` 的桥照样在存活者身上。
             -- 谓词用标量子查询：这样它是 `facts_live_predicate_idx` 的索引条件（同 `name_vectors::PENDING_SQL`），
             -- 写成 JOIN 规划器会沿别的索引读整个库。
             -- 按描述建的实体没有名字事实（0044），不在这里出现：描述不是召回的桥。
             SELECT nf.subject_id FROM facts nf
              WHERE nf.kb_id = $1
                AND nf.predicate_id = (SELECT nr.id FROM relation_types nr
                                        WHERE nr.kb_id = $1 AND nr.builtin AND nr.key = 'known_as')
                AND nf.invalidated_at IS NULL
                AND char_length(nf.object_value->>'value') >= $4
                AND ((lower(nf.object_value->>'value') LIKE '%' || $6 || '%'
                      AND lower(nf.object_value->>'value') ~ $7)
                     OR ($3 LIKE '%' || replace(replace(replace(lower(nf.object_value->>'value'), '\\', '\\\\'), '%', '\\%'), '_', '\\_') || '%'
                      AND $3 ~ {name_edges})))
           AND e.kb_id = $1 AND e.merged_into IS NULL
           AND e.id <> $2
           AND lower(e.canonical_name) <> $3
         ORDER BY char_length(e.canonical_name)
         LIMIT $5",
        name_edges = crate::names::word_edges_sql("lower(nf.object_value->>'value')"),
    ))
    .bind(kb_id)
    .bind(new_id)
    .bind(&lower)
    .bind(MIN_CONTAIN_CHARS)
    .bind(CONTAIN_SCAN_LIMIT)
    .bind(like_escaped(&lower))
    // LIKE alone is not enough: `token` would match `service_token`, so a Latin name must sit on word boundaries to count as contained (#1104)
    .bind(crate::names::word_edges_pattern(&lower))
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        // Which type pairs might refer to the same thing: the existing rules already settled this, so do not invent another set:
        // a pair the ontology declares mutually exclusive never merges, and person vs organization never merges,
        // the concept fallback could be anything and may be the same
        .filter(|(_, other_name, type_key, other_type, _)| {
            !other_type.is_some_and(|t| disjoint.contains(&t))
                && classify_type_drift(mention_key.as_deref(), type_key.as_deref())
                    != TypeDrift::Disjoint
                // `migration 39` is a substring of `migration 395`, but not its short name (see `numbers_differ`)
                // Filter before take: a sibling carrying a different number should not use up a slot meant for the real candidate
                && !crate::names::numbers_differ(name, other_name)
        })
        .take(MAX_CONTAIN_REVIEWS)
        .map(|(id, other_name, _, _, emb)| {
            // The score is only a reference for sorting the queue, and does **not take part in the decision whether to merge** --
            // that decision was never on this path
            let score = ctx
                .and_then(|x| emb.as_ref().and_then(|p| cosine(p.as_slice(), x)))
                .unwrap_or(0.0);
            ReviewRequest {
                other_id: id,
                score,
                reason: format!("contains|{other_name}"),
                stage: ReviewStage::Adjudicating,
            }
        })
        .collect())
}

#[derive(Debug, sqlx::FromRow)]
struct CrossCandidate {
    id: Uuid,
    canonical_name: String,
    // None = this candidate has not yet been typed (0009)
    type_key: Option<String>,
    // Same as above; `types_are_kin` needs to walk the class hierarchy by id
    type_id: Option<Uuid>,
    profile_embedding: Option<Vector>,
    profile_n: i32,
}

/// `reason` stores a code; the wording belongs to the interface (docs/decisions/0004) -- this column should not be half code
/// and half English sentence, otherwise on a Chinese interface half of it can be translated and half cannot.
fn drift_reason(mention_key: Option<&str>, other_key: Option<&str>, sim: Option<f32>) -> String {
    // The untyped side is written as `(untyped)`. This column stores a code for the interface to translate;
    // leaving it empty would turn `a vs b` into `a vs `, which reads as truncated rather than as "none"
    let a = mention_key.unwrap_or("(untyped)");
    let b = other_key.unwrap_or("(untyped)");
    match sim {
        Some(s) => format!("type_drift|{a} vs {b} {s:.2}"),
        None => format!("type_drift|{a} vs {b}"),
    }
}

/// All classes the ontology declares as exclusive with this class (0016 B3).
///
/// **Exclusion is inherited**: one declaration that Person ⟂ Organization makes every subclass of Person exclusive with
/// every subclass of Organization. So first collect this class's ancestors by walking up the parent chain, take the exclusion partners they declare,
/// then expand down the child chain. The table stores one row per direction, so asking in one direction is enough.
///
/// With no type judged (`None`) there is no class to ask about, so return the empty set: that side already sits at the recall-candidate tier
async fn declared_disjoint_from(
    pool: &PgPool,
    kb_id: Uuid,
    type_id: Option<Uuid>,
) -> AppResult<HashSet<Uuid>> {
    let Some(type_id) = type_id else {
        return Ok(HashSet::new());
    };
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "WITH RECURSIVE up(id) AS (
             SELECT $2::uuid
             UNION
             SELECT p.parent_id FROM entity_type_parents p JOIN up ON p.child_id = up.id
         ), hit(id) AS (
             SELECT d.b_id FROM entity_type_disjoint d JOIN up ON d.a_id = up.id
              WHERE d.kb_id = $1
         ), down(id) AS (
             SELECT id FROM hit
             UNION
             SELECT p.child_id FROM entity_type_parents p JOIN down ON p.parent_id = down.id
         )
         SELECT id FROM down",
    )
    .bind(kb_id)
    .bind(type_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Whether two classes belong to one family: one is an ancestor of the other, or both share an ancestor that is **not the root**.
///
/// The hard table of three keys in `CONFUSABLE_TYPE_KEYS` is for libraries without a package installed; once schema.org is installed,
/// the same company can be extracted as Organization / Corporation / OnlineBusiness -- all three under Organization -- yet none of the pairs pass the hard table,
/// so three same-named entities coexist, and
/// the review queue holds none of them, while the panel points to Review saying "merge there" (#226).
///
/// The root is not a shared ancestor: in schema.org everything is Thing, and counting it would put Person and Organization in one family.
/// A vocabulary without a root (where W3C Org's Organization is itself the top) relies on
/// the ancestor/descendant half to catch it.
async fn types_are_kin(pool: &PgPool, a: Uuid, b: Uuid) -> AppResult<bool> {
    let (kin,): (bool,) = sqlx::query_as(
        "WITH RECURSIVE up_a(id) AS (
             SELECT $1::uuid
             UNION
             SELECT p.parent_id FROM entity_type_parents p JOIN up_a ON p.child_id = up_a.id
         ), up_b(id) AS (
             SELECT $2::uuid
             UNION
             SELECT p.parent_id FROM entity_type_parents p JOIN up_b ON p.child_id = up_b.id
         )
         SELECT EXISTS (SELECT 1 FROM up_a WHERE id = $2)
             OR EXISTS (SELECT 1 FROM up_b WHERE id = $1)
             OR EXISTS (SELECT 1 FROM up_a JOIN up_b USING (id)
                         WHERE EXISTS (SELECT 1 FROM entity_type_parents p
                                        WHERE p.child_id = up_a.id))",
    )
    .bind(a)
    .bind(b)
    .fetch_one(pool)
    .await?;
    Ok(kin)
}

fn confusable_reviews(
    // None = this side has not yet been typed (0009)
    mention_key: Option<&str>,
    cands: &[&CrossCandidate],
    ctx: Option<&[f32]>,
) -> Vec<ReviewRequest> {
    cands
        .iter()
        .map(|c| {
            let sim = ctx.and_then(|x| {
                c.profile_embedding
                    .as_ref()
                    .and_then(|p| cosine(p.as_slice(), x))
            });
            ReviewRequest {
                other_id: c.id,
                score: sim.unwrap_or(0.0),
                reason: drift_reason(mention_key, c.type_key.as_deref(), sim),
                stage: ReviewStage::Adjudicating,
            }
        })
        .collect()
}

/// The holder had no type yet and one was determined this time → promote.
/// This is not a merge (there is no second entity), so it is not recorded in entity_merges; the ontology page can edit it back by hand.
///
/// **A "no type" a person has stated is not "not yet determined".** Since 0009 both are NULL,
/// so type_id alone cannot tell them apart -- an entity a person has looked at and judged to have no suitable class in the ontology
/// would get a type attached on the next extraction
async fn adopt_type(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    canonical_name: &str,
    type_id: Option<Uuid>,
) -> AppResult<()> {
    let Some(type_id) = type_id else {
        return Ok(());
    };
    let adopted = sqlx::query(
        "UPDATE entities
         SET type_id = $2, type_source = 'extracted', updated_at = now()
         WHERE id = $1 AND type_id IS NULL AND type_source <> 'human'",
    )
    .bind(entity_id)
    .bind(type_id)
    .execute(pool)
    .await?;
    if adopted.rows_affected() > 0 {
        // The disambiguation suffix of the type-label fallback may be out of date
        refresh_disambiguators(pool, kb_id, canonical_name).await?;
    }
    Ok(())
}

/// Cross-type disposition when same-type recall is empty (type drift).
/// A concept fallback candidate runs the existing profile tiers: high similarity ATTACHes directly (a fix for drift recall; the concept-side type is promoted to a concrete type),
/// a grey zone or no basis to judge → prefer splitting to merging: create new + review pair; easily confused concrete types
/// always create new + review pair (a same name with a wavering type is itself a signal, so no similarity threshold is set); hard exclusions are ignored.
#[allow(clippy::too_many_arguments)]
async fn resolve_type_drift(
    pool: &PgPool,
    kb_id: Uuid,
    // None = the type the extractor gave is not in the ontology, or the library has no class at all (0009)
    type_id: Option<Uuid>,
    name: &str,
    keys: &[String],
    context: Option<&[f32]>,
    exclude: &[Uuid],
    // false = do not merge into the concept fallback: the identity name is created under the class it declares itself, and must not be swallowed by the fallback
    may_attach: bool,
) -> AppResult<Resolution> {
    // This side may also not have been typed yet (0009); in that case there is no key to look up
    let mention_key: Option<String> = match type_id {
        Some(t) => sqlx::query_as::<_, (String,)>("SELECT key FROM entity_types WHERE id = $1")
            .bind(t)
            .fetch_optional(pool)
            .await?
            .map(|(k,)| k),
        None => None,
    };
    let cross: Vec<CrossCandidate> = sqlx::query_as(&format!(
        "SELECT e.id, e.canonical_name, t.key AS type_key, e.type_id,
                e.profile_embedding, e.profile_n
         FROM entities e LEFT JOIN entity_types t ON t.id = e.type_id
         -- IS DISTINCT FROM 而不是 <>：后者遇 NULL 返回 NULL，被 WHERE 当假，
         -- 未分类实体会被整个漏掉（0009）
         WHERE e.kb_id = $1 AND e.type_id IS DISTINCT FROM $2 AND e.merged_into IS NULL
           -- 被描述的东西不参与名字召回（0044），同上
           AND e.description IS NULL
           AND {named}",
        named = crate::names::has_name_or_alias_in("e", 1, 3),
    ))
    .bind(kb_id)
    .bind(type_id)
    .bind(keys)
    .fetch_all(pool)
    .await?
    .into_iter()
    .filter(|candidate: &CrossCandidate| !exclude.contains(&candidate.id))
    .collect();

    // The classes the ontology declares as exclusive, fetched in one go (including inheritance). The declaration takes precedence over all the heuristics below
    let disjoint = declared_disjoint_from(pool, kb_id, type_id).await?;
    let mut recall_cands: Vec<&CrossCandidate> = Vec::new();
    let mut review_cands: Vec<&CrossCandidate> = Vec::new();
    for c in &cross {
        let mut drift = classify_type_drift(mention_key.as_deref(), c.type_key.as_deref());
        if c.type_id.is_some_and(|t| disjoint.contains(&t)) {
            // The ontology says these two classes are exclusive: even if the hard table calls them confusable and the class hierarchy says they are one family, keep them apart.
            // A declaration is a judgment a person wrote; heuristics are only guesses when nothing is declared
            drift = TypeDrift::Disjoint;
        } else if drift == TypeDrift::Disjoint {
            // Where the hard table cannot decide, look at the class hierarchy: same name under the same branch counts as confusable, and enters the review queue
            if let (Some(a), Some(b)) = (type_id, c.type_id) {
                if types_are_kin(pool, a, b).await? {
                    drift = TypeDrift::Review;
                }
            }
        }
        match drift {
            TypeDrift::Recall => recall_cands.push(c),
            TypeDrift::Review => review_cands.push(c),
            TypeDrift::Disjoint => {}
        }
    }

    if let Some(ctx) = context.filter(|_| may_attach) {
        let best = recall_cands
            .iter()
            .filter_map(|c| {
                c.profile_embedding
                    .as_ref()
                    .and_then(|p| cosine(p.as_slice(), ctx))
                    .map(|sim| (*c, sim))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((best, sim)) = best {
            if sim >= SIM_ATTACH {
                update_profile(pool, best.id, best.profile_n, ctx).await?;
                if best.type_key.is_none() {
                    adopt_type(pool, kb_id, best.id, &best.canonical_name, type_id).await?;
                }
                // The mention has settled onto the recalled entity; the suspicion about the same-named confusable-type entity still stands → enqueue as usual
                let mut reviews =
                    confusable_reviews(mention_key.as_deref(), &review_cands, Some(ctx));
                reviews.truncate(MAX_DRIFT_REVIEWS);
                return Ok(Resolution {
                    entity_id: best.id,
                    created: false,
                    reviews,
                });
            }
        }
    }

    // Same as above: the ones named as excluded, plus those weighed in the cross-type same name
    let weighed: Vec<Uuid> = exclude
        .iter()
        .copied()
        .chain(cross.iter().map(|c| c.id))
        .collect();
    let (id, created) = create_entity(pool, kb_id, type_id, name, context, &weighed).await?;
    if !created {
        // Another document being processed in parallel just created it: use that one, and the review pair is queued by it too
        return Ok(Resolution {
            entity_id: id,
            created: false,
            reviews: Vec::new(),
        });
    }
    if !cross.is_empty() {
        // Cross-type same names coexist: disambiguation suffixes are grouped by name (not by type), so they need refreshing
        refresh_disambiguators(pool, kb_id, name).await?;
    }
    let mut reviews = confusable_reviews(mention_key.as_deref(), &review_cands, context);
    for c in &recall_cands {
        let sim = context.and_then(|ctx| {
            c.profile_embedding
                .as_ref()
                .and_then(|p| cosine(p.as_slice(), ctx))
        });
        match sim {
            // The profile clearly does not look alike: keep them fully apart, and do not bother the review queue
            Some(s) if s < SIM_NEW => {}
            // Grey zone or no basis to judge (no embedding / candidate has no profile): prefer splitting to merging, plus a review pair
            _ => reviews.push(ReviewRequest {
                other_id: c.id,
                score: sim.unwrap_or(0.0),
                reason: drift_reason(mention_key.as_deref(), c.type_key.as_deref(), sim),
                stage: ReviewStage::Adjudicating,
            }),
        }
    }
    reviews.truncate(MAX_DRIFT_REVIEWS);
    // The drift path also creates an entity in this case; containment is checked as usual
    reviews.extend(containment_reviews(pool, kb_id, type_id, name, id, context).await?);
    Ok(Resolution {
        entity_id: id,
        created: true,
        reviews,
    })
}

/// Create one entity. Returns `(id, whether it was actually created)`.
///
/// **Creating a same-name entity of the same class is serialized.** The lookup above is not in the transaction: two documents extracted in parallel each look up the name
/// and find nothing, and each creates one -- measured: "Lantu Data" got two within the same second, and every later mention then hit both candidates,
/// each creating another and queuing another review pair, so one corpus run split into four.
/// Here an advisory lock is taken per (library, name) at transaction level, and the lookup is repeated under the lock: if someone else just created it, use theirs.
/// Same-name entities of different types are not covered by this -- that is a disambiguation matter, not a race.
async fn create_entity(
    pool: &PgPool,
    kb_id: Uuid,
    // None = the type the extractor gave is not in the ontology, or the library has no class at all (0009)
    type_id: Option<Uuid>,
    name: &str,
    context: Option<&[f32]>,
    // The same-name entities that the caller has just weighed and decided NOT to merge into.
    //
    // Without this, the lookup under the lock cannot tell two things apart: one is "a parallel document created the same-named one a millisecond ago" -- use that one;
    // the other is "this name already had someone, and the caller looked at them and decided to create another" -- then the lookup would only fetch back the candidate it just rejected,
    // which would let the lock assign the mention to one of them on the caller's behalf.
    // On the same-name tie path this is exactly what #270 forbids:
    // if they cannot be told apart, do not force a split; assign to neither, and send both to review.
    weighed: &[Uuid],
) -> AppResult<(Uuid, bool)> {
    // The name attribute is fetched outside the lock: it does its own insert, and putting it inside the lock would queue every entity creator behind the same lock
    let known_as = crate::names::ensure_known_as(pool, kb_id).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
        .bind(kb_id.to_string())
        .bind(name)
        .execute(&mut *tx)
        .await?;
    let existing: Option<(Uuid,)> = sqlx::query_as(
        // A described thing does not count as "the same-named one" (0044): its canonical_name is only a display description
        "SELECT id FROM entities
         WHERE kb_id = $1 AND canonical_name = $2 AND type_id IS NOT DISTINCT FROM $3
           AND merged_into IS NULL AND description IS NULL AND id <> ALL($4)
         ORDER BY id LIMIT 1",
    )
    .bind(kb_id)
    .bind(name)
    .bind(type_id)
    .bind(weighed)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((id,)) = existing {
        tx.commit().await?;
        return Ok((id, false));
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entities (id, kb_id, type_id, canonical_name, profile_embedding, profile_n)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(kb_id)
    .bind(type_id)
    .bind(name)
    .bind(context.map(|c| Vector::from(c.to_vec())))
    .bind(i32::from(context.is_some()))
    .execute(&mut *tx)
    .await?;
    // Same transaction as the entity: recall finds it by name fact, so there must be no instant where it exists but its name cannot be looked up
    // The provenance (which chunk, which sentence) is added as evidence for this fact by extraction afterwards
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value)
         VALUES ($1, $2, $3, $4, jsonb_build_object('value', $5::text))",
    )
    .bind(Uuid::now_v7())
    .bind(kb_id)
    .bind(id)
    .bind(known_as)
    .bind(name)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((id, true))
}

/// A described, unnamed thing (0044 first cut, #729): "the buyer's parent company",
/// "that Shanghai subsidiary". It is an entity -- statements must point at it -- but it has no name.
///
/// `canonical_name` is set to the description itself, **for display only**; `description` is its real identity.
/// **No `known_as`**: a name fact is the bridge for recall (0041), and a description used as a bridge would join two things that
/// happen to be described the same way in two different documents. With no name there is no bridge, and a later mention cannot find it --
/// which is right: what a description refers to is settled by alignment (the second cut), not by literal sameness.
///
/// `kind_word` is the document's own category word ("company", "subsidiary"), placed on `specific_type`;
/// `type_id` is left empty and `type_source` takes the default `extracted`.
///
/// Plain insert: no lock, no candidate lookup, no dedup -- the caller merges a description repeated within one document into one.
/// Two documents each describe it once: that makes two entities, and alignment later decides whether they are one
pub async fn create_described(
    pool: &PgPool,
    kb_id: Uuid,
    description: &str,
    kind_word: Option<&str>,
) -> AppResult<Uuid> {
    let description = description.trim();
    if description.is_empty() {
        return Err(AppError::invalid(
            "description_missing",
            "a described thing needs the document's description",
        ));
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entities (id, kb_id, type_id, canonical_name, description, specific_type)
         VALUES ($1, $2, NULL, $3, $3, left($4, 80))",
    )
    .bind(id)
    .bind(kb_id)
    .bind(description)
    .bind(kind_word.map(str::trim).filter(|k| !k.is_empty()))
    .execute(pool)
    .await?;
    Ok(id)
}

async fn touch_entity(pool: &PgPool, id: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE entities SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Incremental profile centroid: profile ← (profile·n + ctx) / (n+1).
/// On a dimension mismatch (the embedding model has been changed), reset the profile with the new vector.
async fn update_profile(pool: &PgPool, id: Uuid, n: i32, ctx: &[f32]) -> AppResult<()> {
    let existing: Option<(Option<Vector>,)> =
        sqlx::query_as("SELECT profile_embedding FROM entities WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let old = existing.and_then(|(v,)| v);
    let (new_vec, new_n) = match old {
        Some(p) if p.as_slice().len() == ctx.len() && n > 0 => {
            let nf = n as f32;
            let merged: Vec<f32> = p
                .as_slice()
                .iter()
                .zip(ctx)
                .map(|(a, b)| (a * nf + b) / (nf + 1.0))
                .collect();
            (merged, n + 1)
        }
        _ => (ctx.to_vec(), 1),
    };
    let dims = new_vec.len();
    sqlx::query(
        "UPDATE entities SET profile_embedding = $2, profile_n = $3, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(Vector::from(new_vec))
    .bind(new_n)
    .execute(pool)
    .await?;
    // The profile table needs an index too (0035 / #514): type resolution scans it entity by entity by subject. With it present, this line is a single lookup
    crate::vector_index::request(pool, crate::vector_index::Target::EntityProfiles, dims).await?;
    Ok(())
}

/// Disambiguating display for a same-name group: when a group has at least 2 live entities, each takes the object name of the fact that best separates it
/// from its namesakes, otherwise it falls back to the type label; a group with a unique member is cleared.
///
/// **Names that ignore predicates** (#299). This used to hardcode four keys: works_at / part_of / located_in / leads,
/// but libraries built from the schema.org package use works_for / member_of / affiliation,
/// so two John Smiths that were clearly separable (one at Acme Robotics, one at
/// St Mary's Hospital) both fell back to the type label and were displayed side by side as two "John Smith · Person".
/// A hand-written word list cannot keep up with someone else's vocabulary; this is the same flaw as the #193 case where "a closed verb list leaks as soon as the corpus changes".
/// -- the same flaw, in both cases.
///
/// Replaced with sorting by **whether this fact can separate them**, with every reason coming from the ontology's own declarations:
///
/// 1. **The object is unique within the same-name group** -- that is the whole point of the suffix. When both people are at Acme it separates neither,
///    and falls through to the criteria below, picked by information content, rather than inventing a fake distinction
/// 2. **The predicate declares its range** (`relation_type_ranges`) -- someone deliberately said what this relation points at,
///    which makes it more likely to be identity-bearing than ones extraction happened to grow
/// 3. **State, not event** (`temporal`): "works somewhere" is identity, "held a meeting one day" is not
/// 4. **Single-valued relation** (`functional`): a relation where a subject can have only one value is itself an identity anchor
///
/// Facts without a predicate (`predicate_id IS NULL`, 0010) do not take part: the original words stay in the evidence,
/// but they are not a claim the ontology acknowledges, and should not be written into a person's identity suffix.
/// Whether any entity in the library has this name, **regardless of type**; merged-away ones do not count.
///
/// Extraction uses it to decide whether an undeclared subject or object is something already known (#559): the model occasionally misses an entity
/// yet uses it in a fact, and then the library usually already has it; if the library does not have it either, it is not a miss -- it is a
/// description ("lawsuit against OpenAI") that should not become a node. When there are several with the name, take the one with the most facts --
/// this only answers "does one exist"; deciding who is who is left to resolution.
pub async fn existing_by_name(
    pool: &PgPool,
    kb_id: Uuid,
    raw_name: &str,
) -> AppResult<Option<Uuid>> {
    let name = normalize_name(raw_name);
    let keys = recall_keys(&name);
    let id: Option<Uuid> = sqlx::query_scalar(&format!(
        "SELECT e.id FROM entities e
          WHERE e.kb_id = $1 AND e.merged_into IS NULL
            -- 被描述的东西不参与名字召回（0044）：描述不是桥
            AND e.description IS NULL
            AND (lower(e.canonical_name) = ANY($2) OR {named})
          ORDER BY (SELECT count(*) FROM facts f
                     WHERE f.kb_id = e.kb_id AND (f.subject_id = e.id OR f.object_id = e.id) AND f.invalidated_at IS NULL AND {not_name}) DESC,
                   e.created_at
          LIMIT 1",
        named = crate::names::has_name_or_alias_in("e", 1, 2),
        not_name = crate::names::not_a_name("f"),
    ))
    .bind(kb_id)
    .bind(&keys)
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

pub async fn refresh_disambiguators(pool: &PgPool, kb_id: Uuid, name: &str) -> AppResult<()> {
    let group: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM entities
         WHERE kb_id = $1 AND merged_into IS NULL AND lower(canonical_name) = lower($2)",
    )
    .bind(kb_id)
    .bind(name)
    .fetch_all(pool)
    .await?;

    if group.len() < 2 {
        for (id,) in &group {
            sqlx::query("UPDATE entities SET disambiguator = NULL WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await?;
        }
        return Ok(());
    }

    let peers: Vec<Uuid> = group.iter().map(|(id,)| *id).collect();
    for (id,) in &group {
        // LEFT JOIN (0044): an object an open statement points at can also serve as a disambiguating suffix -- "Zhang San · Xingyun Technology"
        // can still be written even when "works at" is not in the ontology. The relation's boolean bits are COALESCEd to false for open rows,
        // so the order among typed rows is unchanged
        let label: Option<(String,)> = sqlx::query_as(
            "SELECT o.canonical_name FROM facts f
             LEFT JOIN relation_types r ON r.id = f.predicate_id
             JOIN entities o ON o.id = f.object_id
             WHERE f.kb_id = $1 AND f.subject_id = $2
               AND f.invalidated_at IS NULL AND f.object_id IS NOT NULL
             ORDER BY
               -- 同名的另一个也指着它，这条就分不开谁是谁
               (NOT EXISTS (SELECT 1 FROM facts g
                             WHERE g.kb_id = $1 AND g.subject_id = ANY($3)
                               AND g.subject_id <> $2 AND g.object_id = f.object_id
                               AND g.invalidated_at IS NULL)) DESC,
               EXISTS (SELECT 1 FROM relation_type_ranges rr
                        WHERE rr.relation_type_id = r.id) DESC,
               COALESCE(r.temporal = 'state', FALSE) DESC,
               COALESCE(r.functional, FALSE) DESC,
               f.confidence DESC, f.recorded_at DESC
             LIMIT 1",
        )
        .bind(kb_id)
        .bind(id)
        .bind(&peers)
        .fetch_optional(pool)
        .await?;
        // If no related fact is found, fall back to the type label; **the type may also be missing** (0009),
        // in which case there is no suffix to write -- leave NULL, and the interface shows it as a same-name tie; do not invent one
        let disambiguator: Option<String> = match label {
            Some((l,)) => Some(l),
            None => sqlx::query_as::<_, (String,)>(
                "SELECT t.label FROM entities e JOIN entity_types t ON t.id = e.type_id
                     WHERE e.id = $1",
            )
            .bind(id)
            .fetch_optional(pool)
            .await?
            .map(|(l,)| l),
        };
        sqlx::query("UPDATE entities SET disambiguator = $2 WHERE id = $1")
            .bind(id)
            .bind(disambiguator)
            .execute(pool)
            .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Review queue
// ---------------------------------------------------------------------------

/// Enqueue a suspected duplicate pair from the grey zone (idempotent for the same pending pair).
pub async fn create_review(
    pool: &PgPool,
    kb_id: Uuid,
    left_id: Uuid,
    right_id: Uuid,
    score: f32,
    reason: &str,
    stage: ReviewStage,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO resolution_reviews AS existing
             (id, kb_id, left_id, right_id, score, reason, stage)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (kb_id, least(left_id, right_id), greatest(left_id, right_id))
             WHERE status = 'pending'
         DO UPDATE SET stage = 'human', reason = EXCLUDED.reason
           WHERE EXCLUDED.stage = 'human' AND existing.stage = 'adjudicating'",
    )
    .bind(Uuid::now_v7())
    .bind(kb_id)
    .bind(left_id)
    .bind(right_id)
    .bind(score)
    .bind(reason)
    .bind(stage.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct ReviewRow {
    id: Uuid,
    left_id: Uuid,
    right_id: Uuid,
    score: f32,
    reason: Option<String>,
    stage: String,
    created_at: DateTime<Utc>,
}

async fn review_sides(
    pool: &PgPool,
    kb_id: Uuid,
    entity_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, ReviewSide>> {
    #[derive(sqlx::FromRow)]
    struct SideRow {
        id: Uuid,
        name: String,
        type_label: Option<String>,
        color: String,
        disambiguator: Option<String>,
        degree: i64,
    }
    let rows: Vec<SideRow> = sqlx::query_as(&format!(
        "SELECT e.id, e.canonical_name AS name, t.label AS type_label,
                coalesce(t.color, '#94a3b8') AS color, e.disambiguator,
                (SELECT count(*) FROM facts f
                 WHERE f.kb_id = e.kb_id AND (f.subject_id = e.id OR f.object_id = e.id)
                   AND f.invalidated_at IS NULL AND {not_name}) AS degree
         -- LEFT JOIN：没判出类型的实体照样要能进审核（0009）。
         -- 内连接会让它整条审核项取不出来，而漂移审核恰恰最常发生在它们身上
         FROM entities e LEFT JOIN entity_types t ON t.id = e.type_id
         WHERE e.kb_id = $1 AND e.id = ANY($2)",
        not_name = crate::names::not_a_name("f"),
    ))
    .bind(kb_id)
    .bind(entity_ids)
    .fetch_all(pool)
    .await?;
    let mut lines = entity_fact_lines_many(pool, kb_id, entity_ids, 4).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.id,
                ReviewSide {
                    id: row.id,
                    name: row.name,
                    type_label: row.type_label,
                    color: row.color,
                    disambiguator: row.disambiguator,
                    degree: row.degree,
                    top_facts: lines.remove(&row.id).unwrap_or_default(),
                },
            )
        })
        .collect())
}

/// A summary line of an entity's facts: "works at → Xingyun Technology (2023-01 → now)", shared by the adjudication prompt and the review UI.
pub async fn entity_fact_lines(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    limit: i64,
) -> AppResult<Vec<String>> {
    Ok(entity_fact_lines_many(pool, kb_id, &[entity_id], limit)
        .await?
        .remove(&entity_id)
        .unwrap_or_default())
}

/// The lateral limit applies to each entity, not to the whole page. Keep this
/// path shared with the single-entity reader used by adjudication.
async fn entity_fact_lines_many(
    pool: &PgPool,
    kb_id: Uuid,
    entity_ids: &[Uuid],
    limit: i64,
) -> AppResult<HashMap<Uuid, Vec<String>>> {
    if entity_ids.is_empty() {
        return Ok(HashMap::new());
    }
    #[derive(sqlx::FromRow)]
    struct Line {
        entity_id: Uuid,
        direction: String,
        predicate_label: String,
        other_name: Option<String>,
        valid_from: Option<DateTime<Utc>>,
        valid_to: Option<DateTime<Utc>>,
    }
    // A name does not count as one of the facts on a review card (0041): two same-named entities each have a "known as Zhang Wei",
    // and laying it out looks like shared evidence, when in fact it separates nothing.
    // An open statement (0044) is read by the phrase it copies as-is: `phrase` comes after the relation label and before the evidence majority;
    // the phrase of typed rows is NULL, and their display is unchanged
    let rows: Vec<Line> = sqlx::query_as(&format!(
        "SELECT requested.id AS entity_id, line.*
         FROM unnest($2::uuid[]) AS requested(id)
         CROSS JOIN LATERAL (
         SELECT CASE WHEN f.subject_id = requested.id THEN 'out' ELSE 'in' END AS direction,
                COALESCE(r.label, f.phrase, fact_surface_predicate(f.id)) AS predicate_label,
                -- 字面值的事实没有对端实体：值本身就是这一行要说的（职位、工号），不能读成「?」
                COALESCE(o.canonical_name, left(f.object_value->>'value', 120)) AS other_name,
                f.valid_from, f.valid_to, f.confidence, f.recorded_at
         FROM facts f
         LEFT JOIN relation_types r ON r.id = f.predicate_id
         LEFT JOIN entities o
           ON o.id = CASE WHEN f.subject_id = requested.id THEN f.object_id ELSE f.subject_id END
         WHERE f.kb_id = $1 AND f.invalidated_at IS NULL
           AND (f.subject_id = requested.id OR f.object_id = requested.id)
           AND COALESCE(r.label, f.phrase, fact_surface_predicate(f.id)) IS NOT NULL
           AND {not_name}
         ORDER BY f.confidence DESC, f.recorded_at DESC
         LIMIT $3
         ) line
         ORDER BY requested.id, line.confidence DESC, line.recorded_at DESC",
        not_name = crate::names::not_a_name("f"),
    ))
    .bind(kb_id)
    .bind(entity_ids)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    // Names other than the main name get their own leading line (0041): "Ocean Probe No. 1" against "Sea Probe 1" -- the adjudicator must be able to see
    // that the former is also called Sea Probe 1, otherwise each side's facts speak separately and it can only judge "not the same". The main name is not listed --
    // two Zhang Weis each have a "known as Zhang Wei", which looks like shared evidence but separates nothing
    let names = crate::names::other_names_many(pool, entity_ids).await?;
    let mut lines: HashMap<Uuid, Vec<String>> = names
        .into_iter()
        .map(|(id, names)| (id, vec![format!("also known as: {}", names.join(", "))]))
        .collect();
    for l in rows {
        let other = l.other_name.unwrap_or_else(|| "?".into());
        let core = if l.direction == "out" {
            format!("{} → {}", l.predicate_label, other)
        } else {
            format!("{} ← {}", l.predicate_label, other)
        };
        let text = match (l.valid_from, l.valid_to) {
            (Some(f), Some(t)) => {
                format!("{core} ({} → {})", f.format("%Y-%m"), t.format("%Y-%m"))
            }
            (Some(f), None) => format!("{core} ({} → now)", f.format("%Y-%m")),
            _ => core,
        };
        lines.entry(l.entity_id).or_default().push(text);
    }
    Ok(lines)
}

pub(crate) async fn assemble_reviews(
    pool: &PgPool,
    kb_id: Uuid,
    rows: Vec<ReviewRow>,
) -> AppResult<Vec<ReviewItem>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    // An entity can occur on either side of several pairs. Read it once for
    // this page, then reuse its summary without changing the review order.
    let mut entity_ids: Vec<Uuid> = rows.iter().flat_map(|r| [r.left_id, r.right_id]).collect();
    entity_ids.sort_unstable();
    entity_ids.dedup();
    let sides = review_sides(pool, kb_id, &entity_ids).await?;
    // Suggestions open on this page (0025): fetched in one pass, attached to the review rows
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let proposals: HashMap<Uuid, utopia_core::models::ReviewProposal> =
        sqlx::query_as::<_, (Uuid, Uuid, String, f32, Option<String>)>(
            "SELECT target_id, id, action, confidence, reason FROM agent_decisions
         WHERE target_kind = 'review' AND target_id = ANY($1) AND status = 'proposed'",
        )
        .bind(&ids)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(target, id, action, confidence, reason)| {
            (
                target,
                utopia_core::models::ReviewProposal {
                    id,
                    action,
                    confidence,
                    reason,
                },
            )
        })
        .collect();
    let mut items = Vec::with_capacity(rows.len());
    for r in rows {
        let proposal = proposals.get(&r.id).cloned();
        items.push(ReviewItem {
            id: r.id,
            score: r.score,
            reason: r.reason,
            stage: r.stage,
            created_at: r.created_at,
            left: sides.get(&r.left_id).cloned().ok_or(AppError::NotFound)?,
            right: sides.get(&r.right_id).cloned().ok_or(AppError::NotFound)?,
            proposal,
        });
    }
    Ok(items)
}

/// Duplicate items are split into three tiers by the relation of the two sides' types (#428). **A side with no type fits no tier**: what is not known
/// can neither be treated as the same nor as different. `clause()` is a SQL fragment with fixed aliases a / b
/// (the two entities, left and right), shared between the list and `review::counts`, so the two places cannot diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeFilter {
    Any,
    /// Both sides typed and equal -- same name, same class: the tier people most want to merge in bulk
    Same,
    /// Both sides typed and unequal -- same name, different meanings: merging would be wrong
    Conflict,
}

impl TypeFilter {
    /// The spelling in the query string: default `any`; an unrecognized value is reported as a contract error
    pub fn parse(s: Option<&str>) -> AppResult<Self> {
        match s.unwrap_or("any") {
            "any" => Ok(Self::Any),
            "same" => Ok(Self::Same),
            "conflict" => Ok(Self::Conflict),
            other => Err(AppError::invalid(
                "unknown_type_filter",
                format!("types must be any, same or conflict, not {other}"),
            )),
        }
    }

    pub fn clause(self) -> &'static str {
        match self {
            Self::Any => "TRUE",
            Self::Same => "a.type_id IS NOT NULL AND a.type_id = b.type_id",
            Self::Conflict => {
                "a.type_id IS NOT NULL AND b.type_id IS NOT NULL AND a.type_id <> b.type_id"
            }
        }
    }
}

/// All pending review items (both those under LLM adjudication and those waiting for a person are shown; a person can decide at any time, jumping ahead).
pub async fn list_reviews(
    pool: &PgPool,
    kb_id: Uuid,
    types: TypeFilter,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<ReviewItem>> {
    let sql = format!(
        "SELECT rr.id, rr.left_id, rr.right_id, rr.score, rr.reason, rr.stage, rr.created_at
         FROM resolution_reviews rr
         JOIN entities a ON a.id = rr.left_id
         JOIN entities b ON b.id = rr.right_id
         WHERE rr.kb_id = $1 AND rr.status = 'pending' AND {}
         ORDER BY rr.created_at DESC, rr.id DESC LIMIT $2 OFFSET $3",
        types.clause()
    );
    let rows: Vec<ReviewRow> = sqlx::query_as(&sql)
        .bind(kb_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    assemble_reviews(pool, kb_id, rows).await
}

/// Bulk adjudication = item-by-item human adjudication (#428): each item goes through `decide_review`, and the merge direction, status,
/// and decided_by are exactly the same as for a single item. One failure (not found, already adjudicated, or an error halfway through) does not drag down
/// the rest: results come back item by item; an action does not reject the whole batch because of it -- that is not a matter for any one item.
pub async fn decide_reviews(
    pool: &PgPool,
    kb_id: Uuid,
    ids: &[Uuid],
    action: &str,
    user_id: Uuid,
    rationale: Option<&str>,
) -> AppResult<Vec<ReviewBatchOutcome>> {
    if action != "merge" && action != "keep" {
        return Err(AppError::Validation("action must be merge or keep".into()));
    }
    let mut out = Vec::with_capacity(ids.len());
    for &id in ids {
        let error = decide_review(pool, kb_id, id, action, user_id, rationale)
            .await
            .err()
            .map(|e| e.to_string());
        out.push(ReviewBatchOutcome { id, error });
    }
    Ok(out)
}

/// Review items awaiting LLM adjudication (consumed by the background adjudication task).
pub async fn pending_adjudications(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
) -> AppResult<Vec<ReviewItem>> {
    let rows: Vec<ReviewRow> = sqlx::query_as(
        "SELECT id, left_id, right_id, score, reason, stage, created_at
         FROM resolution_reviews
         WHERE kb_id = $1 AND status = 'pending' AND stage = 'adjudicating'
         ORDER BY created_at LIMIT $2",
    )
    .bind(kb_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    assemble_reviews(pool, kb_id, rows).await
}

/// The LLM is unsure, or no model is configured → hand to a person.
/// `reason` stores a **code**, optionally followed by `|detail` -- not a sentence for people to read.
///
/// The interface language lives on the client (see docs/decisions/0004), and the server has no locale to word anything with;
/// English prose written into this column would stay forever on a Chinese interface. Wording belongs to i18n; here we keep only the stable code.
pub async fn escalate_review(pool: &PgPool, review_id: Uuid, reason: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE resolution_reviews SET stage = 'human', reason = $2
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(review_id)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// Automatic decision (LLM high confidence): merged / kept. The merge action itself is executed by the caller first.
/// Returns how many rows were tied: 0 = this pair is no longer pending (a person decided it, or another path got there first), so the caller should not record an identical verdict
pub async fn close_review_auto(
    pool: &PgPool,
    review_id: Uuid,
    status: &str,
    reason: &str,
) -> AppResult<u64> {
    let res = sqlx::query(
        "UPDATE resolution_reviews SET status = $2, reason = $3, decided_at = now()
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(review_id)
    .bind(status)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// A person's decision. Merge direction: the side with higher degree (more facts) becomes the surviving target; on a tie, the one created earlier.
/// `rationale`: the sentence a person writes when deciding (0026). **Leaving it empty is allowed** -- the question is "what makes you
/// decide this way", not a mandatory form; but once written, it stays with this row and the ledger, so the next adjudicator and the agent
/// that reads precedents get more than just the outcome.
pub async fn decide_review(
    pool: &PgPool,
    kb_id: Uuid,
    review_id: Uuid,
    action: &str,
    user_id: Uuid,
    rationale: Option<&str>,
) -> AppResult<()> {
    let rationale = rationale.map(str::trim).filter(|s| !s.is_empty());
    let row: Option<ReviewRow> = sqlx::query_as(
        "SELECT id, left_id, right_id, score, reason, stage, created_at
         FROM resolution_reviews WHERE id = $1 AND kb_id = $2 AND status = 'pending'",
    )
    .bind(review_id)
    .bind(kb_id)
    .fetch_optional(pool)
    .await?;
    let row = row.ok_or(AppError::NotFound)?;
    // A pair an agent is adjudicating (0025): wait for its suggestion, or turn off the switch
    if crate::governance::locked_by_agent(pool, kb_id, review_id).await? {
        return Err(AppError::Conflict(
            "The agent is deciding this pair right now; wait for its proposal or turn governance off."
                .into(),
        ));
    }

    match action {
        "merge" => {
            // Same-name chain (A-B merged, B-C still waiting): B has merged into A, so this pair is really A-C.
            // Follow merged_into to the living one and merge with that; if both sides arrive at the same one, all that is left is closing the review row
            let (l, r) = (
                survivor(pool, kb_id, row.left_id).await?,
                survivor(pool, kb_id, row.right_id).await?,
            );
            if l != r {
                let (target, source) = merge_direction(pool, l, r).await?;
                // The merge log's reason also records this sentence: the column in Review › Merges reads it
                merge_entities(
                    pool,
                    kb_id,
                    source,
                    target,
                    Some(user_id),
                    rationale.unwrap_or("review decision"),
                )
                .await?;
            }
            sqlx::query(
                "UPDATE resolution_reviews
                 SET status = 'merged', decided_at = now(), decided_by = $2, rationale = $3
                 WHERE id = $1",
            )
            .bind(review_id)
            .bind(user_id)
            .bind(rationale)
            .execute(pool)
            .await?;
        }
        "keep" => {
            sqlx::query(
                "UPDATE resolution_reviews
                 SET status = 'kept', decided_at = now(), decided_by = $2, rationale = $3
                 WHERE id = $1",
            )
            .bind(review_id)
            .bind(user_id)
            .bind(rationale)
            .execute(pool)
            .await?;
        }
        _ => return Err(AppError::Validation("action must be merge or keep".into())),
    }
    Ok(())
}

/// Follows `merged_into` to the entity that is still alive. After a cluster of chained merges, one side of a pair may already have
/// merged into someone else; the merge should combine the living one, not the row that is already empty
pub async fn survivor(pool: &PgPool, kb_id: Uuid, mut id: Uuid) -> AppResult<Uuid> {
    for _ in 0..16 {
        let next: Option<Option<Uuid>> =
            sqlx::query_scalar("SELECT merged_into FROM entities WHERE id = $1 AND kb_id = $2")
                .bind(id)
                .bind(kb_id)
                .fetch_optional(pool)
                .await?;
        match next {
            Some(Some(n)) => id = n,
            Some(None) => return Ok(id),
            None => return Err(AppError::NotFound),
        }
    }
    Err(AppError::Conflict("merge chain too long".into()))
}

/// Merge direction: returns (target alive, source merged away).
pub async fn merge_direction(pool: &PgPool, a: Uuid, b: Uuid) -> AppResult<(Uuid, Uuid)> {
    // The fact index starts with kb_id; first use a scalar subquery to get the library, so that both sides of the OR can use the composite index.
    // On PG16 a direct JOIN may scan the whole library by kb_id alone and then filter subject/object.
    let (deg_a,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM facts
         WHERE kb_id = (SELECT kb_id FROM entities WHERE id = $1)
           AND (subject_id = $1 OR object_id = $1) AND invalidated_at IS NULL",
    )
    .bind(a)
    .fetch_one(pool)
    .await?;
    let (deg_b,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM facts
         WHERE kb_id = (SELECT kb_id FROM entities WHERE id = $1)
           AND (subject_id = $1 OR object_id = $1) AND invalidated_at IS NULL",
    )
    .bind(b)
    .fetch_one(pool)
    .await?;
    // uuidv7 is time-ordered: on a degree tie, the one created earlier survives
    Ok(if deg_a > deg_b || (deg_a == deg_b && a < b) {
        (a, b)
    } else {
        (b, a)
    })
}

// ---------------------------------------------------------------------------
// Merge / rollback
// ---------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
struct EntityFull {
    // None = not yet judged (0009)
    type_id: Option<Uuid>,
    canonical_name: String,
    profile_embedding: Option<Vector>,
    profile_n: i32,
    merged_into: Option<Uuid>,
}

async fn entity_full(pool: &PgPool, kb_id: Uuid, id: Uuid) -> AppResult<EntityFull> {
    sqlx::query_as(
        "SELECT type_id, canonical_name, profile_embedding, profile_n, merged_into
         FROM entities WHERE kb_id = $1 AND id = $2",
    )
    .bind(kb_id)
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(AppError::NotFound)
}

/// Merge source → target: facts move to target, facts that point at each other and duplicate facts after the merge are voided,
/// attribute values are merged weighted by profile, and source is marked merged_into. The whole thing is logged so it can be rolled back.
///
/// **Names need no special case** (0041): the source's name is a `known_as` fact on it, moved to target together with the other facts,
/// and recorded in `moved_subject_facts`; the same-name one is voided as a duplicate fact. When a merge is retracted, it is moved back together.
pub async fn merge_entities(
    pool: &PgPool,
    kb_id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
    merged_by: Option<Uuid>,
    reason: &str,
) -> AppResult<Uuid> {
    if source_id == target_id {
        return Err(AppError::invalid(
            "self_merge",
            "Cannot merge an entity into itself",
        ));
    }
    let source = entity_full(pool, kb_id, source_id).await?;
    let target = entity_full(pool, kb_id, target_id).await?;
    if source.merged_into.is_some() || target.merged_into.is_some() {
        return Err(AppError::Conflict("Entity already merged".into()));
    }

    let mut tx = pool.begin().await?;

    // Timelines the move would involve are locked first, in a fixed order, before any row is changed (retracting a merge uses the same order, see the temporal module header):
    // without locking, a merge and a retraction, or a merge and a database reconciliation, would each hold half the row locks and wait on each other
    let moving: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM facts WHERE kb_id = $1 AND (subject_id = $2 OR object_id = $2)",
    )
    .bind(kb_id)
    .bind(source_id)
    .fetch_all(&mut *tx)
    .await?;
    let timelines =
        crate::temporal::timelines_of(&mut *tx, kb_id, &moving, Some((&[source_id], target_id)))
            .await?;
    crate::temporal::lock_timelines(&mut tx, kb_id, &timelines).await?;

    // Facts that point at each other (self-loops after merging) → void
    let cross: Vec<(Uuid,)> = sqlx::query_as(
        "UPDATE facts SET invalidated_at = now()
         WHERE kb_id = $1 AND invalidated_at IS NULL
           AND ((subject_id = $2 AND object_id = $3) OR (subject_id = $3 AND object_id = $2))
         RETURNING id",
    )
    .bind(kb_id)
    .bind(source_id)
    .bind(target_id)
    .fetch_all(&mut *tx)
    .await?;
    let mut invalidated: Vec<Uuid> = cross.into_iter().map(|(id,)| id).collect();

    let moved_subject: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
        "UPDATE facts SET subject_id = $2 WHERE kb_id = $3 AND subject_id = $1 RETURNING id",
    )
    .bind(source_id)
    .bind(target_id)
    .bind(kb_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|(id,)| id)
    .collect();
    let moved_object: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
        "UPDATE facts SET object_id = $2 WHERE kb_id = $3 AND object_id = $1 RETURNING id",
    )
    .bind(source_id)
    .bind(target_id)
    .bind(kb_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|(id,)| id)
    .collect();

    // After merge, live facts with the same SPO + valid_from are duplicates: keep the one with the earliest recorded_at, void the rest.
    // Rows without a predicate do not take part: different phrases of open statements all carry NULL, and must not be treated as the same sentence because of that.
    //
    // **Both sides of the object must be grouped.** Literal-value facts all have object_id NULL, so grouping by it alone would treat
    // **all values** under the same subject and predicate as one assertion: after a merge, apart from (company, founded, 2015) vs (company, registered capital, ...),
    // several values of one predicate survive only as the earliest recorded one
    // the rest silently disappear, with no supersedes to look up.
    let dups: Vec<(Vec<Uuid>,)> = sqlx::query_as(
        "SELECT array_agg(id ORDER BY recorded_at, id) FROM facts
         WHERE kb_id = $1 AND invalidated_at IS NULL AND predicate_id IS NOT NULL
           AND (subject_id = $2 OR object_id = $2)
         GROUP BY subject_id, predicate_id, object_id, object_value, valid_from
         HAVING count(*) > 1",
    )
    .bind(kb_id)
    .bind(target_id)
    .fetch_all(&mut *tx)
    .await?;
    let mut dup_ids = Vec::new();
    let (mut copied_to, mut copied_chunks) = (Vec::new(), Vec::new());
    for (ids,) in dups {
        // First attach the sources to the surviving row, otherwise the next materialization could void it; then rebuild from the original text, dropping a person's correction.
        // The old row's link is kept for now: retracting the merge still needs it; materialization cleans up invalid links under its existing rules.
        sqlx::query(
            "INSERT INTO typed_fact_sources (fact_id, statement_id)
             SELECT $1, statement_id FROM typed_fact_sources WHERE fact_id = ANY($2)
             ON CONFLICT DO NOTHING",
        )
        .bind(ids[0])
        .bind(&ids[1..])
        .execute(&mut *tx)
        .await?;
        // Evidence is copied too (#1426): if the identity source's name evidence stayed only on the voided row, the surviving side would no longer be
        // an identity entity, and exact-name attachment and the pre-merge manual check (0071 decisions 2 and 7) would not recognize it.
        // Rows already on the surviving side for the same chunk are left as they are; copied ones are recorded in the ledger, so retraction deletes only these.
        let copied: Vec<Uuid> = sqlx::query_scalar(
            "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, doc_version,
                    proposed_predicate, quote_start, quote_end, attested_at, attested_by)
             SELECT DISTINCT ON (chunk_id) $1, chunk_id, quote, document_id, doc_version,
                    proposed_predicate, quote_start, quote_end, attested_at, attested_by
               FROM fact_evidence WHERE fact_id = ANY($2)
              ORDER BY chunk_id, array_position($2, fact_id)
             ON CONFLICT DO NOTHING
             RETURNING chunk_id",
        )
        .bind(ids[0])
        .bind(&ids[1..])
        .fetch_all(&mut *tx)
        .await?;
        copied_to.extend(std::iter::repeat_n(ids[0], copied.len()));
        copied_chunks.extend(copied);
        dup_ids.extend_from_slice(&ids[1..]);
    }
    if !dup_ids.is_empty() {
        sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = ANY($1)")
            .bind(&dup_ids)
            .execute(&mut *tx)
            .await?;
        invalidated.extend(dup_ids);
    }
    // If what was copied is identity evidence, the surviving name should no longer keep its vector (0071 decision 5)
    crate::name_vectors::drop_with_identity_evidence(&mut tx, &copied_to).await?;

    // Attribute values merged weighted by profile
    let (profile, profile_n) = match (&target.profile_embedding, &source.profile_embedding) {
        (Some(t), Some(s)) if t.as_slice().len() == s.as_slice().len() => {
            let (nt, ns) = (
                target.profile_n.max(1) as f32,
                source.profile_n.max(1) as f32,
            );
            let merged: Vec<f32> = t
                .as_slice()
                .iter()
                .zip(s.as_slice())
                .map(|(a, b)| (a * nt + b * ns) / (nt + ns))
                .collect();
            (
                Some(Vector::from(merged)),
                target.profile_n + source.profile_n,
            )
        }
        (Some(t), _) => (Some(t.clone()), target.profile_n),
        (None, Some(s)) => (Some(s.clone()), source.profile_n),
        (None, None) => (None, 0),
    };

    // Type reconciliation: **the side with no type yields**. If the surviving side has not been typed but the merged-away side has, carry that type over;
    // otherwise keep the surviving side's.
    //
    // Previously this had to look up the concept row's id in the database and then compare against both sides. "Not yet judged"
    // is now `None` (0009), so one `or` says it all, and that query is saved too
    let new_type_id = target.type_id.or(source.type_id);

    sqlx::query(
        "UPDATE entities SET profile_embedding = $2, profile_n = $3,
                type_id = $4, updated_at = now() WHERE id = $1",
    )
    .bind(target_id)
    .bind(&profile)
    .bind(profile_n)
    .bind(new_type_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE entities SET merged_into = $2, updated_at = now() WHERE id = $1")
        .bind(source_id)
        .bind(target_id)
        .execute(&mut *tx)
        .await?;

    // Other pending review items involving source: **repoint them at the merge target, do not close them**.
    //
    // This used to close them all, with the reason that "if the doubt still holds, a later mention will raise it again".
    // **That sentence was wrong**: containment recall runs only **when an entity is created**, and these entities already exist,
    // so they will not be created again, and there is no "later mention" to raise it again. Closing them meant closing them for good.
    //
    // Measured: `Mr. Holmes` was queued paired with `Holmes` (0.70), then `Holmes` merged into
    // `Sherlock Holmes`, and this pair was closed as superseded by merge -- while
    // the question `Mr. Holmes` vs `Sherlock Holmes`, which still holds, was never asked again.
    //
    // Close first the two kinds that are truly out of date: those that become self-loops after redirection, and those whose target pair is already in the queue.
    sqlx::query(
        "UPDATE resolution_reviews AS r
         SET status = 'kept', reason = 'superseded by merge', decided_at = now()
         WHERE r.kb_id = $1 AND r.status = 'pending'
           AND (r.left_id = $2 OR r.right_id = $2)
           AND NOT (least(r.left_id, r.right_id) = least($2, $3)
                    AND greatest(r.left_id, r.right_id) = greatest($2, $3))
           AND (
             (CASE WHEN r.left_id = $2 THEN r.right_id ELSE r.left_id END) = $3
             OR EXISTS (
               SELECT 1 FROM resolution_reviews d
               WHERE d.kb_id = $1 AND d.status = 'pending' AND d.id <> r.id
                 AND least(d.left_id, d.right_id)
                     = least($3, CASE WHEN r.left_id = $2 THEN r.right_id ELSE r.left_id END)
                 AND greatest(d.left_id, d.right_id)
                     = greatest($3, CASE WHEN r.left_id = $2 THEN r.right_id ELSE r.left_id END))
           )",
    )
    .bind(kb_id)
    .bind(source_id)
    .bind(target_id)
    .execute(&mut *tx)
    .await?;
    // The rest are repointed at the target, and the question stays on the queue awaiting adjudication.
    // The (source, target) pair itself is excepted -- it is the object of this merge's adjudication, marked merged by the caller,
    // and touching it here would make it wrongly appear in history as "kept apart"
    sqlx::query(
        "UPDATE resolution_reviews
         SET left_id = CASE WHEN left_id = $2 THEN $3 ELSE left_id END,
             right_id = CASE WHEN right_id = $2 THEN $3 ELSE right_id END,
             reason = reason || '|redirected'
         WHERE kb_id = $1 AND status = 'pending' AND (left_id = $2 OR right_id = $2)
           AND NOT (least(left_id, right_id) = least($2, $3)
                    AND greatest(left_id, right_id) = greatest($2, $3))",
    )
    .bind(kb_id)
    .bind(source_id)
    .bind(target_id)
    .execute(&mut *tx)
    .await?;

    let merge_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entity_merges (id, kb_id, source_id, target_id,
                moved_subject_facts, moved_object_facts, invalidated_facts,
                target_profile_before, target_profile_n_before, target_type_before,
                merged_by, reason, copied_evidence_facts, copied_evidence_chunks)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(merge_id)
    .bind(kb_id)
    .bind(source_id)
    .bind(target_id)
    .bind(&moved_subject)
    .bind(&moved_object)
    .bind(&invalidated)
    .bind(&target.profile_embedding)
    .bind(target.profile_n)
    .bind(target.type_id)
    .bind(merged_by)
    .bind(reason)
    .bind(&copied_to)
    .bind(&copied_chunks)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    // Temporal reconciliation after the move: facts whose subject or object changed are equivalent to a new observation written to the database -- once two objects fold into one,
    // the uniqueness invariant is checked for the first time, where an old open interval collides with its successor (e.g. "Stardust" merged into "Stardust Project",
    // the old lead's leads should close at the new appointee's start).
    // The corrected row ids are recorded in the merge ledger for audit. Rollback does not depend on it: rollback recomputes the timeline from the rows left after the move,
    // and the endpoints the engine draws follow along naturally (0057).
    // Note: this step is outside the transaction, and failure is self-healing -- the whole timeline is recomputed the next time a fact is written to it.
    let moved_all: Vec<Uuid> = moved_subject
        .iter()
        .chain(moved_object.iter())
        .copied()
        .collect();

    // **Merging is a third path that rewrites facts** (#196): swapping out a subject or object can turn a legal fact into a signature violation -- a direction corrected at extraction time
    // can be flipped back by one merge. Here nothing is corrected or changed; it only reports:
    // check domain / range on the moved facts, and those that violate go into `axiom_violations`
    //(kind = signature), in the same queue as other axiom violations, with the same three ways out.
    // Placed after the transaction: the target entity's type was just reconciled inside the transaction, and is visible only once it has committed
    match crate::reasoning::signature_breaks(&mut *pool.acquire().await?, kb_id, Some(&moved_all))
        .await
    {
        Ok(broken) if !broken.is_empty() => {
            if let Err(e) = crate::reasoning::record_signature_breaks(pool, kb_id, &broken).await {
                tracing::warn!(%kb_id, error = %e, "合并后的签名违规没能入库");
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(%kb_id, error = %e, "合并后的签名检查失败"),
    }
    let report = crate::temporal::reconcile_moved_facts(pool, kb_id, &moved_all).await?;
    if !report.corrected.is_empty() {
        sqlx::query("UPDATE entity_merges SET temporal_corrections = $2 WHERE id = $1")
            .bind(merge_id)
            .bind(&report.corrected)
            .execute(pool)
            .await?;
    }

    refresh_disambiguators(pool, kb_id, &source.canonical_name).await?;
    if !source
        .canonical_name
        .eq_ignore_ascii_case(&target.canonical_name)
    {
        refresh_disambiguators(pool, kb_id, &target.canonical_name).await?;
    }
    Ok(merge_id)
}

#[derive(Debug, sqlx::FromRow)]
struct MergeRow {
    source_id: Uuid,
    target_id: Uuid,
    moved_subject_facts: Vec<Uuid>,
    moved_object_facts: Vec<Uuid>,
    invalidated_facts: Vec<Uuid>,
    copied_evidence_facts: Vec<Uuid>,
    copied_evidence_chunks: Vec<Uuid>,
    target_profile_before: Option<Vector>,
    target_profile_n_before: i32,
    target_type_before: Option<Uuid>,
    reverted_at: Option<DateTime<Utc>>,
}

/// Facts that were moved, together with every row rewritten from them (followed all the way down via supersedes, voided ones included)
async fn with_rewrites(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kb_id: Uuid,
    roots: &[Uuid],
) -> AppResult<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "WITH RECURSIVE chain(id) AS (
             SELECT unnest($1::uuid[])
             UNION
             SELECT f.id FROM facts f JOIN chain c ON f.supersedes = c.id AND f.kb_id = $2
         )
         SELECT id FROM chain",
    )
    .bind(roots)
    .bind(kb_id)
    .fetch_all(&mut **tx)
    .await?)
}

/// Exact rollback of one merge: facts moved back to their original place, voids undone, the target's profile and type restored from snapshot, source revived.
///
/// **What moves back is the facts that were moved, together with every row rewritten from them.** After the merge the engine may have closed one, a person may have edited
/// its interval, or rejected it -- those rows belong to it and go back to the source entity, each keeping its state: a person's edited interval is still there,
/// and a rejected one is still rejected. Closures the engine made at merge time are not undone separately: both sides' timelines are recomputed from the remaining rows after the move;
/// the boundary goes with the rows that moved, a row that was closed there naturally reopens, and a stated end does not change (0057).
///
/// There are two ways to move, so that replaying the merge window on the record axis still answers correctly (`fact_owner_at` in 0027 trusts only the ledger):
/// rows on the ledger are changed back to their subject/object in place; rows rewritten only after the merge are not on the ledger: the live ones are voided, and a
/// new row is started after them on the source entity; the voided ones stay on the target entity -- they really were attached there during that window
///
/// Before moving, take the locks on every timeline involved, on both sides, in a fixed order (see the temporal module header): reconciliation takes the lock
/// before locking rows, and if this took row locks first, the two sides would wait on each other forever
pub async fn revert_merge(pool: &PgPool, kb_id: Uuid, merge_id: Uuid) -> AppResult<()> {
    let m: MergeRow = sqlx::query_as(
        "SELECT source_id, target_id, moved_subject_facts, moved_object_facts,
                invalidated_facts, copied_evidence_facts, copied_evidence_chunks,
                target_profile_before, target_profile_n_before, target_type_before, reverted_at
         FROM entity_merges WHERE id = $1 AND kb_id = $2",
    )
    .bind(merge_id)
    .bind(kb_id)
    .fetch_optional(pool)
    .await?
    .ok_or(AppError::NotFound)?;
    if m.reverted_at.is_some() {
        return Err(AppError::Conflict("Merge already reverted".into()));
    }

    let mut tx = pool.begin().await?;
    // Whom the fact is attached to right now: the target entity, or an entity the target was later merged into (S merged into T, then T merged into C;
    // when S→T is retracted, S's facts are on C). If only the target were checked, then in a chain of merges, retracting the first link would revive
    // the source entity while its facts stay at the end of the chain (#679 fourth-round review)
    let holders: Vec<Uuid> = sqlx::query_scalar(
        "WITH RECURSIVE chain(id) AS (
             SELECT $1::uuid
             UNION SELECT e.merged_into FROM entities e JOIN chain ON e.id = chain.id
              WHERE e.merged_into IS NOT NULL)
         SELECT id FROM chain",
    )
    .bind(m.target_id)
    .fetch_all(&mut *tx)
    .await?;
    let touched: Vec<Uuid> = with_rewrites(&mut tx, kb_id, &m.moved_subject_facts)
        .await?
        .into_iter()
        .chain(with_rewrites(&mut tx, kb_id, &m.moved_object_facts).await?)
        .chain(m.invalidated_facts.iter().copied())
        .collect();
    let timelines =
        crate::temporal::timelines_of(&mut *tx, kb_id, &touched, Some((&holders, m.source_id)))
            .await?;
    crate::temporal::lock_timelines(&mut tx, kb_id, &timelines).await?;
    // Walk it again after taking the lock: while waiting for the lock, the engine may just have written a new row rewritten from them
    let subject_rows = with_rewrites(&mut tx, kb_id, &m.moved_subject_facts).await?;
    let object_rows = with_rewrites(&mut tx, kb_id, &m.moved_object_facts).await?;

    sqlx::query("UPDATE facts SET subject_id = $1 WHERE id = ANY($2) AND subject_id = ANY($3)")
        .bind(m.source_id)
        .bind(&m.moved_subject_facts)
        .bind(&holders)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE facts SET object_id = $1 WHERE id = ANY($2) AND object_id = ANY($3)")
        .bind(m.source_id)
        .bind(&m.moved_object_facts)
        .bind(&holders)
        .execute(&mut *tx)
        .await?;
    for (rows, ledger, on_object) in [
        (&subject_rows, &m.moved_subject_facts, false),
        (&object_rows, &m.moved_object_facts, true),
    ] {
        let holder = if on_object { "object_id" } else { "subject_id" };
        let live: Vec<Uuid> = sqlx::query_scalar(&format!(
            "SELECT id FROM facts
              WHERE id = ANY($1) AND id <> ALL($2) AND invalidated_at IS NULL AND {holder} = ANY($3)"
        ))
        .bind(rows)
        .bind(ledger)
        .bind(&holders)
        .fetch_all(&mut *tx)
        .await?;
        for id in live {
            let (subject, object) = if on_object {
                (None, Some(m.source_id))
            } else {
                (Some(m.source_id), None)
            };
            crate::temporal::rehome_tx(&mut tx, id, subject, object).await?;
        }
    }
    // Evidence copied onto the surviving row during the merge is deleted: the duplicate that comes back still has its evidence on it (#1426).
    // The surviving row may have been rewritten since (a person edited the interval, the timeline closed it, or the move-back above), and the evidence was copied along with the row to new rows,
    // so delete all the way down along supersedes, the same chain as `with_rewrites`.
    // If the surviving name no longer has identity evidence as a result, it waits again to be backfilled with a vector, just as when the source turned off its identity name (0071 decision 6)
    sqlx::query(
        "WITH RECURSIVE chain(fact_id, chunk_id) AS (
             SELECT * FROM unnest($1::uuid[], $2::uuid[])
             UNION
             SELECT f.id, c.chunk_id FROM facts f JOIN chain c ON f.supersedes = c.fact_id AND f.kb_id = $3
         )
         DELETE FROM fact_evidence fe USING chain c
          WHERE fe.fact_id = c.fact_id AND fe.chunk_id = c.chunk_id",
    )
    .bind(&m.copied_evidence_facts)
    .bind(&m.copied_evidence_chunks)
    .bind(kb_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE facts SET invalidated_at = NULL WHERE id = ANY($1)")
        .bind(&m.invalidated_facts)
        .execute(&mut *tx)
        .await?;
    crate::temporal::tidy_timelines_tx(&mut tx, kb_id, &timelines).await?;

    let source = entity_full(pool, kb_id, m.source_id).await?;
    // The source's name is its name fact, already moved back with moved_subject_facts
    sqlx::query(
        "UPDATE entities SET
            profile_embedding = $2, profile_n = $3,
            type_id = coalesce($4, type_id), updated_at = now()
         WHERE id = $1",
    )
    .bind(m.target_id)
    .bind(&m.target_profile_before)
    .bind(m.target_profile_n_before)
    .bind(m.target_type_before)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE entities SET merged_into = NULL, updated_at = now() WHERE id = $1")
        .bind(m.source_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE entity_merges SET reverted_at = now() WHERE id = $1")
        .bind(merge_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    refresh_disambiguators(pool, kb_id, &source.canonical_name).await?;
    Ok(())
}

/// Merge log (the history area of the Review page).
pub async fn list_merges(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<MergeLogView>> {
    let rows: Vec<MergeLogView> = sqlx::query_as(
        "SELECT m.id, s.canonical_name AS source_name, t.canonical_name AS target_name,
                u.display_name AS merged_by_name, m.reason, m.created_at, m.reverted_at
         FROM entity_merges m
         JOIN entities s ON s.id = m.source_id
         JOIN entities t ON t.id = m.target_id
         LEFT JOIN users u ON u.id = m.merged_by
         WHERE m.kb_id = $1
         ORDER BY m.created_at DESC, m.id DESC LIMIT $2 OFFSET $3",
    )
    .bind(kb_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// LLM adjudication cache
// ---------------------------------------------------------------------------

pub async fn get_verdict(
    pool: &PgPool,
    kb_id: Uuid,
    pair_key: &str,
) -> AppResult<Option<(Option<bool>, f32)>> {
    let row: Option<(Option<bool>, f32)> = sqlx::query_as(
        "SELECT same, confidence FROM resolution_verdicts WHERE kb_id = $1 AND pair_key = $2",
    )
    .bind(kb_id)
    .bind(pair_key)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn put_verdict(
    pool: &PgPool,
    kb_id: Uuid,
    pair_key: &str,
    same: Option<bool>,
    confidence: f32,
    model: &str,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO resolution_verdicts (kb_id, pair_key, same, confidence, model)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (kb_id, pair_key)
         DO UPDATE SET same = $3, confidence = $4, model = $5, created_at = now()",
    )
    .bind(kb_id)
    .bind(pair_key)
    .bind(same)
    .bind(confidence)
    .bind(model)
    .execute(pool)
    .await?;
    Ok(())
}

/// Undo an entity's type change: put the entities back to their original type.
///
/// The type itself is not deleted -- same reason as for predicates: entities have pointed at it, and "it existed" is history.
/// `proposed_type` is restored too, otherwise after the undo those entities could never claim it back.
pub async fn unadopt_types(pool: &PgPool, kb_id: Uuid, batch_id: Uuid) -> AppResult<u32> {
    // from_type_id may be NULL: after 0009 the most common type change is exactly "from no class to a class",
    // and undoing it means pushing it back to no class. Decoding it as a Uuid would crash here on the spot
    let rows: Vec<(Uuid, Option<Uuid>, String)> = sqlx::query_as(
        "SELECT r.entity_id, r.from_type_id, t.key
         FROM entity_retypes r JOIN entity_types t ON t.id = r.to_type_id
         WHERE r.batch_id = $1 AND r.kb_id = $2 AND r.reverted_at IS NULL",
    )
    .bind(batch_id)
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Err(AppError::NotFound);
    }
    let mut names: Vec<String> = Vec::new();
    let mut tx = pool.begin().await?;
    let mut reverted = 0u32;
    for (entity_id, from_type, adopted_key) in &rows {
        let row: Option<(String,)> = sqlx::query_as(
            "UPDATE entities SET type_id = $2, proposed_type = $3, updated_at = now()
             WHERE id = $1 RETURNING canonical_name",
        )
        .bind(entity_id)
        .bind(from_type)
        .bind(adopted_key)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((name,)) = row {
            names.push(name);
        }
        reverted += 1;
    }
    sqlx::query(
        "UPDATE entity_retypes SET reverted_at = now()
         WHERE batch_id = $1 AND kb_id = $2 AND reverted_at IS NULL",
    )
    .bind(batch_id)
    .bind(kb_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    for n in &names {
        refresh_disambiguators(pool, kb_id, n).await?;
    }
    Ok(reverted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_escaped_turns_backslash_percent_and_underscore_into_literals() {
        assert_eq!(like_escaped(r"a\b%c_d"), r"a\\b\%c\_d");
    }

    #[test]
    fn normalize_fullwidth_and_whitespace() {
        assert_eq!(normalize_name("　ＡＣＭＥ  Corp　"), "ACME Corp");
        assert_eq!(normalize_name("张三"), "张三");
        assert_eq!(normalize_name("张  三"), "张 三");
    }

    #[test]
    fn markdown_folding_is_conservative() {
        for (raw, expected) in [
            ("**Pump 1**", "Pump 1"),
            ("__Pump 1__", "Pump 1"),
            ("__init__", "__init__"),
            ("`docs/setup.md`", "docs/setup.md"),
            ("`setup.md`", "setup.md"),
            ("`acme`", "`acme`"),
            ("`main()`", "`main()`"),
            ("`1.2`", "`1.2`"),
            ("**Pump 1*", "**Pump 1*"),
            ("***Pump 1***", "***Pump 1***"),
            ("** Pump 1 **", "** Pump 1 **"),
            ("****", "****"),
        ] {
            assert_eq!(fold_markdown_markers(raw), expected, "{raw:?}");
        }
    }

    #[test]
    fn recall_keys_fold_markdown_before_stemming_and_keep_exact_spelling() {
        let normalized = normalize_name("　＊＊Ｐｈｏｅｎｉｘ　 Project＊＊ ");
        let keys = recall_keys(&normalized);
        assert_eq!(keys[0], name_key(&normalized));
        for key in recall_keys("Phoenix Project") {
            assert!(keys.contains(&key), "must recall {key:?}");
        }
        assert!(keys.contains(&"**phoenix project**".to_string()));
        assert!(keys.len() <= 11);
    }

    #[test]
    fn stem_generic_suffixes() {
        assert_eq!(name_stem("星尘项目").as_deref(), Some("星尘"));
        assert_eq!(name_stem("星辰科技公司").as_deref(), Some("星辰科技"));
        assert_eq!(name_stem("Phoenix Project").as_deref(), Some("phoenix"));
        assert_eq!(name_stem("Project Phoenix").as_deref(), Some("phoenix"));
        assert_eq!(name_stem("Acme Inc.").as_deref(), Some("acme"));
        assert_eq!(name_stem("星尘"), None);
        assert_eq!(name_stem("项目"), None); // empty after stripping is not a stem
        assert_eq!(name_stem("项目团队"), None); // the stem itself is a generic word
        assert_eq!(name_stem("Team Project"), None);
    }

    #[test]
    fn recall_keys_bidirectional() {
        // A mention without a suffix can recall an entity that has one (augmentation); the reverse relies on the stem
        let k = recall_keys("星尘");
        assert!(k.contains(&"星尘".to_string()));
        assert!(k.contains(&"星尘项目".to_string()));
        let k = recall_keys("星尘项目");
        assert!(k.contains(&"星尘项目".to_string()));
        assert!(k.contains(&"星尘".to_string()));
        let k = recall_keys("Phoenix");
        assert!(k.contains(&"phoenix project".to_string()));
        assert!(k.contains(&"project phoenix".to_string()));
        let k = recall_keys("Project Phoenix");
        assert!(k.contains(&"phoenix".to_string()));
        assert!(recall_keys("张三").len() <= 10);
    }

    /// English company names often have a comma before the suffix: "Acme, Inc." and "Acme" and "Acme Inc" are the same recall
    #[test]
    fn a_comma_before_the_generic_suffix_is_not_part_of_the_stem() {
        assert_eq!(name_stem("Acme, Inc.").as_deref(), Some("acme"));
        assert_eq!(name_stem("Orion Labs, Inc").as_deref(), Some("orion labs"));
        let k = recall_keys("Acme, Inc.");
        assert!(k.contains(&"acme".to_string()), "{k:?}");
        assert!(k.contains(&"acme inc".to_string()), "{k:?}");
        assert_eq!(name_stem(", Inc."), None);
    }

    #[test]
    fn type_drift_classes() {
        // A side not yet typed → recall candidate. Before 0009 this tier compared the `concept`
        // key; now it compares whether a class exists at all
        assert_eq!(
            classify_type_drift(None, Some("organization")),
            TypeDrift::Recall
        );
        assert_eq!(
            classify_type_drift(Some("project"), None),
            TypeDrift::Recall
        );
        // Both sides not yet typed: they also go to recall, and profile similarity decides
        assert_eq!(classify_type_drift(None, None), TypeDrift::Recall);
        // Pairs of easily confused concrete types → review pair
        assert_eq!(
            classify_type_drift(Some("organization"), Some("project")),
            TypeDrift::Review
        );
        assert_eq!(
            classify_type_drift(Some("project"), Some("product")),
            TypeDrift::Review
        );
        assert_eq!(
            classify_type_drift(Some("product"), Some("organization")),
            TypeDrift::Review
        );
        // Hard exclusions and unknown custom types → kept fully apart
        assert_eq!(
            classify_type_drift(Some("person"), Some("organization")),
            TypeDrift::Disjoint
        );
        assert_eq!(
            classify_type_drift(Some("person"), Some("project")),
            TypeDrift::Disjoint
        );
        assert_eq!(
            classify_type_drift(Some("event"), Some("location")),
            TypeDrift::Disjoint
        );
        assert_eq!(
            classify_type_drift(Some("team"), Some("organization")),
            TypeDrift::Disjoint
        );
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]).unwrap() - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).unwrap().abs() < 1e-6);
        assert!(cosine(&[1.0], &[1.0, 2.0]).is_none());
        assert!(cosine(&[0.0, 0.0], &[1.0, 1.0]).is_none());
    }
}

/// An entity awaiting type refinement, together with all the material used to judge what it is.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TypeCandidateSubject {
    pub id: Uuid,
    pub canonical_name: String,
    /// Names other than the main name (name facts, 0041). For the model: "Sea Probe 1" alone cannot be judged to be what it is,
    /// but alongside "Ocean Probe No. 1" it can be.
    pub other_names: Vec<String>,
    /// The class it currently hangs under, which **may be none** (0009: not judged yet means NULL). The "coarse" in the name
    /// is history -- extraction now may also give a fine class directly, and may give the wrong one (measured:
    /// `Shaoxing → address`), so this may also be the one to be **corrected**.
    pub coarse_key: Option<String>,
    pub coarse_id: Option<Uuid>,
    /// The description of the current class. Adjudication needs to judge "is this class right now" -- the key alone is not enough,
    /// because imported-ontology keys often cannot explain themselves (what is `entry_point`?)
    pub coarse_description: Option<String>,
    /// The type name the model itself reported during extraction; it is kept here only when it is **not in the vocabulary**
    pub proposed_type: Option<String>,
    /// The model's own description of it; every entity has one.
    ///
    /// This is the strongest signal, because it turns the task from "understand what this is" into "which class in the ontology is called this" --
    /// short name to short label. The measured failure was on the other side: matching a Chinese profile against
    /// schema.org's "A software application." -- the two shapes simply do not correspond.
    pub specific_type: Option<String>,
    /// The predicates it takes part in, together with the other party's name (`produces Deep Blue`, `leads by Zhang Wei`).
    /// **Accumulated across documents**, which is exactly what extraction did not have on the spot.
    pub roles: Vec<String>,
    /// Evidence quotes, a few sentences at most. Extraction sees the same sentences, but that one pass must simultaneously do entity recognition,
    /// relation judgment, temporal resolution and JSON formatting, and compete for attention with dozens of other entities.
    /// Evidence quotes, **only those where it is the subject**. A quote in object position describes the subject:
    /// "Shanghai Pudong New Area" is the object of located_in, but the quote is "Xingyun Technology (Shanghai) Co., Ltd. is a joint-stock company registered in Shanghai Pudong New Area", with the weight of the sentence
    /// on "joint-stock company" -- measured: the candidates that version of retrieval returned were all corporation-type
    /// (the kind of entity it was supposed to refine away from).
    pub quotes: Vec<String>,
    pub fact_count: i64,
}

/// Entities worth sending for type refinement: **those with no class yet**, or those where the model reported a type outside the vocabulary.
///
/// Entities already typed very specifically are left alone -- re-judging them carries only a risk of going down, with no room to go up.
///
/// `unattended` = runs automatically (0016 C2): **skips those the engine has already looked at** (`type_resolved_at`). Candidates are sorted by
/// fact count, sixty per round; without skipping, every round would be the same batch and the ones behind would never come up. The path
/// a person clicks passes false -- a person wants the current state reviewed again.
pub async fn entities_for_type_resolution(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
    unattended: bool,
) -> AppResult<Vec<TypeCandidateSubject>> {
    Ok(sqlx::query_as(
        "SELECT e.id, e.canonical_name,
                ARRAY(SELECT DISTINCT nf.object_value->>'value' FROM facts nf
                        JOIN relation_types nr ON nr.id = nf.predicate_id
                       WHERE nf.subject_id = e.id AND nr.builtin AND nr.key = 'known_as'
                         AND nf.invalidated_at IS NULL
                         AND lower(nf.object_value->>'value') <> lower(e.canonical_name)) AS other_names,
                t.key AS coarse_key, t.id AS coarse_id,
                t.description AS coarse_description,
                e.proposed_type, e.specific_type,
                -- 对方写**名字**，不写它的类型 key。
                -- 写 key 是自毁：查询里出现 organization / product / event 这些词，
                -- 而检索的目标正是这些类自己，于是候选就是画像里那几个词。
                -- 实测「张伟」的画像是 leads→organization … works_at→organization，
                -- 回来的候选是 corporation、organization、business_entity_type——
                -- 检索到的是画像自己，不是这个人是什么
                ARRAY(
                  SELECT DISTINCT COALESCE(rt.key, fact_surface_predicate(f.id))
                                  || CASE WHEN f.subject_id = e.id THEN ' ' ELSE ' by ' END
                                  || coalesce(oe.canonical_name, 'a value')
                  FROM facts f
                  LEFT JOIN relation_types rt ON rt.id = f.predicate_id
                  LEFT JOIN entities oe ON oe.id = CASE WHEN f.subject_id = e.id
                                                       THEN f.object_id ELSE f.subject_id END
                  WHERE f.kb_id = $1 AND f.invalidated_at IS NULL
                    AND (f.subject_id = e.id OR f.object_id = e.id)
                    AND COALESCE(rt.key, fact_surface_predicate(f.id)) IS NOT NULL
                    -- 名字不是它扮演的角色，每个实体都有，判不出类型（0041）
                    AND NOT coalesce(rt.builtin AND rt.key = 'known_as', false)
                  LIMIT 12
                ) AS roles,
                ARRAY(
                  SELECT DISTINCT ev.quote FROM fact_evidence ev
                  JOIN facts f2 ON f2.id = ev.fact_id
                  LEFT JOIN relation_types rt2 ON rt2.id = f2.predicate_id
                  WHERE f2.kb_id = $1 AND f2.invalidated_at IS NULL
                    AND f2.subject_id = e.id
                    AND ev.quote IS NOT NULL
                    AND NOT coalesce(rt2.builtin AND rt2.key = 'known_as', false)
                  LIMIT 3
                ) AS quotes,
                (SELECT count(*) FROM facts f3
                 LEFT JOIN relation_types rt3 ON rt3.id = f3.predicate_id
                 WHERE f3.kb_id = $1 AND f3.invalidated_at IS NULL
                   AND (f3.subject_id = e.id OR f3.object_id = e.id)
                   AND NOT coalesce(rt3.builtin AND rt3.key = 'known_as', false)) AS fact_count
         FROM entities e
         LEFT JOIN entity_types t ON t.id = e.type_id
         WHERE e.kb_id = $1 AND e.merged_into IS NULL
           -- **人拍过板的不再重判。**
           --
           -- 少了这一行，下面第三种条件会把它们全都捞回来：一个人工定成
           -- organization 的实体，只要 organization 有子类就够格，于是引擎
           -- 每一轮都去重新裁决一遍人已经决定过的事，而账本事后才看得出是谁改的。
           --
           -- 「不落库」不等于「不许说话」：引擎若认为人定错了，该走 Review 队列，
           -- 而不是直接改掉。憋着不说和直接改掉都是失真，只是方向相反
           AND e.type_source <> 'human'
           -- 值得看的三种：**还没有类的**、模型报过词表外类型的、
           -- **以及现类本身还有子类的**。第三种是主力：抽取只认基类之后，
           -- organization 下面挂着一大批更具体的类，那才是导进来的本体
           -- 唯一的用武之地。只看前两种就把它整个漏掉了
           AND (e.type_id IS NULL OR e.proposed_type IS NOT NULL OR e.specific_type IS NOT NULL
                OR EXISTS (SELECT 1 FROM entity_type_parents p WHERE p.parent_id = t.id))
           AND (NOT $3::bool OR e.type_resolved_at IS NULL)
         ORDER BY fact_count DESC, e.created_at
         LIMIT $2",
    )
    .bind(kb_id)
    .bind(limit)
    .bind(unattended)
    .fetch_all(pool)
    .await?)
}

/// The engine has looked at it: all three outcomes (changed / left for a person / left alone) count as looked at, and automatic runs never come back to it.
/// Write paths such as a person manually changing a class or merging do not clear this mark -- they were not among the automatic run's candidates anyway.
pub async fn mark_type_judged(pool: &PgPool, kb_id: Uuid, ids: &[Uuid]) -> AppResult<u64> {
    Ok(sqlx::query(
        "UPDATE entities SET type_resolved_at = now() WHERE kb_id = $1 AND id = ANY($2)",
    )
    .bind(kb_id)
    .bind(ids)
    .execute(pool)
    .await?
    .rows_affected())
}

/// All descendants of a class (including itself). Refinement can only move down from a coarse class to its descendants.
///
/// **Recursive, not one level**: the ontology is a DAG, `software_application ⊂ creative_work ⊂ thing`,
/// and checking just one level would shut out most of the correct answers.
pub async fn descendants_of(pool: &PgPool, kb_id: Uuid, root: Uuid) -> AppResult<Vec<Uuid>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "WITH RECURSIVE d(id) AS (
             SELECT $2::uuid
             UNION
             SELECT p.child_id FROM entity_type_parents p JOIN d ON d.id = p.parent_id
         )
         SELECT d.id FROM d JOIN entity_types t ON t.id = d.id WHERE t.kb_id = $1",
    )
    .bind(kb_id)
    .bind(root)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Record the model's own description of this entity.
///
/// **A later write does not overwrite an earlier one** (written only `IS NULL`), the same as `set_proposed_type`:
/// the same entity is mentioned by several chunks; the first description usually comes from the most complete sentence,
/// and later chunks are often only an aside.
pub async fn set_specific_type(pool: &PgPool, entity_id: Uuid, value: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE entities SET specific_type = left($2, 80)
         WHERE id = $1 AND specific_type IS NULL",
    )
    .bind(entity_id)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// The **already-typed** entities most similar to this one, together with their classes.
///
/// The second source of candidates for type resolution. The first, searching for classes by a profile's description, has a weakness that measurement made clear:
/// Chinese profiles against schema.org's "A software application." -- the two shapes simply do not correspond.
/// This one goes around that obstacle: **name to name, same language**, and it gets more accurate as the library grows:
/// "Deep Blue vector database" resembles "Milvus", and Milvus is already marked software_application.
///
/// It compares the two sides' `profile_embedding` (a moving average of the contexts in which they appear): **the same kind of vector against the same kind of
/// vector**, not a text profile against a context vector. The cost is zero -- entity resolution already maintains this vector.
///
/// Known weakness: an entity appearing in only one document has a context vector that is that document's vector, so entities from the same document
/// become each other's nearest neighbours. Callers must look at `same_document` and must not treat it as type evidence.
/// Memo of [`descendants_of`] within one batch (#514).
///
/// Coarse classes come from the extraction's small vocabulary (person, organization, product plus a few more), and among sixty subjects
/// the same `coarse_id` recurs, so the same recursive CTE keeps firing. The ontology does not change within the batch, and the same input
/// gives the same result, so remembering it does not change the answer. **Subjects with no coarse class do not go in this table**: they have no "descendants" axis
/// (0009), and the whole class table is candidates; using an `Option` as the key would mix them up with some real class
#[derive(Default)]
pub struct DescendantsMemo {
    sets: std::collections::HashMap<Uuid, HashSet<Uuid>>,
}

impl DescendantsMemo {
    pub async fn get(
        &mut self,
        pool: &PgPool,
        kb_id: Uuid,
        root: Option<Uuid>,
    ) -> AppResult<HashSet<Uuid>> {
        let Some(root) = root else {
            return Ok(HashSet::new());
        };
        if let Some(set) = self.sets.get(&root) {
            return Ok(set.clone());
        }
        let set: HashSet<Uuid> = descendants_of(pool, kb_id, root)
            .await?
            .into_iter()
            .collect();
        self.sets.insert(root, set.clone());
        Ok(set)
    }

    /// Remember how many roots
    pub fn len(&self) -> usize {
        self.sets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }
}

/// The neighbours of one subject: already-typed entities with similar context, together with "is it the same document".
///
/// The subject's own vector is fetched first and then queried: the dimension must be written into the SQL (`vector_index` rule 1), which a subquery in the SQL
/// cannot provide. A subject without a vector has no neighbours, so return empty.
pub async fn nearest_typed_entities(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    limit: i64,
) -> AppResult<Vec<(String, Uuid, String, f64, bool)>> {
    let me: Option<(Option<Vector>,)> =
        sqlx::query_as("SELECT profile_embedding FROM entities WHERE id = $2 AND kb_id = $1")
            .bind(kb_id)
            .bind(entity_id)
            .fetch_optional(pool)
            .await?;
    let Some(query_vec) = me.and_then(|(v,)| v) else {
        return Ok(Vec::new());
    };
    let dims = query_vec.as_slice().len();
    let mut tx = pool.begin().await?;
    crate::vector_index::relaxed_order(pool, &mut tx).await?;
    let rows = sqlx::query_as(&format!(
        "WITH my_docs AS (
             SELECT DISTINCT ev.document_id FROM fact_evidence ev
             JOIN facts f ON f.id = ev.fact_id
             WHERE f.kb_id = $1 AND (f.subject_id = $2 OR f.object_id = $2)
         ),
         nearest AS MATERIALIZED (
             SELECT e.id, e.canonical_name, t.id AS type_id, t.key,
                    ({distance})::float8 AS distance,
                    EXISTS (SELECT 1 FROM fact_evidence ev2
                            JOIN facts f2 ON f2.id = ev2.fact_id
                            WHERE f2.kb_id = $1 AND (f2.subject_id = e.id OR f2.object_id = e.id)
                              AND ev2.document_id IN (SELECT document_id FROM my_docs))
                    AS same_document
             -- 内连接就是那道门：没判出类型的实体（type_id IS NULL）不是答案，
             -- 拿它当邻居的证据只会把「没判出来」传染开
             FROM entities e
             JOIN entity_types t ON t.id = e.type_id
             WHERE e.kb_id = $1 AND e.merged_into IS NULL AND e.id <> $2
               AND e.profile_embedding IS NOT NULL AND {same_dims}
             ORDER BY {order}
             LIMIT $4
         )
         -- 次序在外层再排一遍，并列由实体 id 定（`vector_index::RESORT`，#652）
         SELECT canonical_name, type_id, key, distance, same_document
         FROM nearest ORDER BY {resort}",
        distance = crate::vector_index::distance("e.profile_embedding", 3, dims),
        order = crate::vector_index::index_order("e.profile_embedding", 3, dims),
        resort = crate::vector_index::RESORT,
        same_dims = crate::vector_index::same_dims("e.profile_embedding", dims),
    ))
    .bind(kb_id)
    .bind(entity_id)
    .bind(&query_vec)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Several neighbour queries run at once. The pool is 32, sized for "short queries running at the same time" (`db.rs`); sixty
/// full scans at once is what emptying that pool looks like -- slow requests and timeouts, with no message saying the pool was small.
/// Eight leaves enough headroom for requests, and once HNSW is in place each query takes only a few milliseconds, so a higher number would not help.
pub const NEIGHBOUR_SCANS: usize = 8;

/// Each subject's neighbours in a batch, **returned in the order sent in**: downstream adjudication reads them in this order.
///
/// Sixty queries are independent of one another; they ran serially only because the loop was serial (#514). Here they are fetched with bounded concurrency,
/// `buffered` keeps the order, and inference still runs in the original order.
pub async fn nearest_typed_for_each(
    pool: &PgPool,
    kb_id: Uuid,
    ids: &[Uuid],
    limit: i64,
) -> AppResult<Vec<Vec<(String, Uuid, String, f64, bool)>>> {
    nearest_typed_for_each_with(pool, kb_id, ids, limit, NEIGHBOUR_SCANS).await
}

/// Same as above; the concurrency ceiling is given by the caller (tests use it to prove the ceiling does not change the answer)
pub async fn nearest_typed_for_each_with(
    pool: &PgPool,
    kb_id: Uuid,
    ids: &[Uuid],
    limit: i64,
    at_once: usize,
) -> AppResult<Vec<Vec<(String, Uuid, String, f64, bool)>>> {
    use futures_util::{stream, StreamExt, TryStreamExt};
    stream::iter(ids.iter().copied())
        .map(|id| nearest_typed_entities(pool, kb_id, id, limit))
        .buffered(at_once.max(1))
        .try_collect()
        .await
}

/// Reclassify entities one by one, writing all of it into one ledger. Returns (batch id, number of changes).
///
/// Named by the caller: the type-resolution adjudication decided that "this entity is that class", not that "everyone called this name is that class".
///
/// The ledger format is identical, so [`unadopt_types`] can undo it as is.
pub async fn retype_entities(
    pool: &PgPool,
    kb_id: Uuid,
    picks: &[(Uuid, Uuid)],
    // None = automatic engine adjudication. The same convention as entity_merges.merged_by,
    // so the entity history can tell "changed by someone" from "automatically changed with high confidence"
    actor: Option<Uuid>,
) -> AppResult<(Uuid, u32)> {
    let batch_id = Uuid::now_v7();
    let mut moved = 0u32;
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    // An entity is changed only once per batch. The ledger's primary key is (batch_id, entity_id), and the same id
    // arriving twice would collide -- and the caller got a 500, so nothing in the whole batch was written. Blocking it here is more reliable than
    // chasing every path that produces picks over there: a second duplicate was meaningless anyway
    let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    for (entity_id, type_id) in picks {
        if !seen.insert(*entity_id) {
            continue;
        }
        let mut tx = pool.begin().await?;
        // Rows already on the target class are not changes and do not go in the ledger -- undo should not push them back.
        //
        // **The old type must be read from the CTE**: `UPDATE … RETURNING` gives the new value, while the ledger needs to record
        // the value from before the change. RETURNING type_id directly gives back the one just written, so undoing would
        // amount to "putting the entity back" where it already is -- the ledger looks full, but nothing can actually be undone.
        // The condition is also put into the CTE: the row still exists, is not merged, and really is about to change
        //
        // **`IS DISTINCT FROM` rather than `<>`**: after 0009 the starting point is often NULL, and
        // `NULL <> uuid` is NULL, not true -- the CTE would come back empty, the UPDATE would change no row,
        // so "assigning a class to an entity that has none", the main scenario, would become a no-op entirely
        let row: Option<(Option<Uuid>, String)> = sqlx::query_as(
            "WITH before AS (
                 SELECT id, type_id, canonical_name FROM entities
                 WHERE id = $1 AND kb_id = $3 AND merged_into IS NULL
                   AND type_id IS DISTINCT FROM $2
             )
             UPDATE entities e SET type_id = $2, updated_at = now(),
                    type_source = CASE WHEN $4::uuid IS NULL THEN 'inferred' ELSE 'human' END
             FROM before
             WHERE e.id = before.id
             RETURNING before.type_id, before.canonical_name",
        )
        .bind(entity_id)
        .bind(type_id)
        .bind(kb_id)
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((from_type, name)) = row else {
            tx.rollback().await?;
            continue;
        };
        sqlx::query(
            "INSERT INTO entity_retypes
                (batch_id, kb_id, entity_id, from_type_id, to_type_id, actor_id)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(batch_id)
        .bind(kb_id)
        .bind(entity_id)
        .bind(from_type)
        .bind(type_id)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        names.insert(name);
        moved += 1;
    }
    // The disambiguation suffix's fallback value is the type label, so changing the class means recomputing it
    for n in &names {
        refresh_disambiguators(pool, kb_id, n).await?;
    }
    Ok((batch_id, moved))
}

/// A "coarse class → fine class" pairing a person has approved.
///
/// The path that waits for a person is triggered by "does it cross a classification axis", and that check often tests whether **the seed class
/// and the imported vocabulary are connected**, not whether there is a risk: schema.org's Place uses its own key, and the built-in location has zero subclasses,
/// so every city would be asked about. A pairing is a relation between classes, and an entity just happens to hit it --
/// once approved, it should count every time.
pub async fn approved_refinements(
    pool: &PgPool,
    kb_id: Uuid,
) -> AppResult<std::collections::HashSet<(Uuid, Uuid)>> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT from_type_id, to_type_id FROM type_refinement_pairs WHERE kb_id = $1",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Record an approved pairing. Re-approving it is idempotent.
pub async fn approve_refinement(
    pool: &PgPool,
    kb_id: Uuid,
    from_type_id: Uuid,
    to_type_id: Uuid,
    by: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO type_refinement_pairs (kb_id, from_type_id, to_type_id, approved_by)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (kb_id, from_type_id, to_type_id) DO NOTHING",
    )
    .bind(kb_id)
    .bind(from_type_id)
    .bind(to_type_id)
    .bind(by)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod same_type_tests {
    use super::{classify_type_drift, TypeDrift};

    /// **Same type is the most common case, and it used to fall into Disjoint.**
    ///
    /// This function was born to serve "type drift" (the same name extracted as two types), where identical types on both sides never happens;
    /// later `containment_reviews` borrowed it as a compatibility criterion, where identical types on both sides are the norm.
    /// The result: neither `Sherlock Holmes` against `Holmes` pair could enter the review queue.
    #[test]
    fn the_same_type_is_a_recall_candidate() {
        for k in ["person", "location", "organization", "product", "event"] {
            assert_eq!(
                classify_type_drift(Some(k), Some(k)),
                TypeDrift::Recall,
                "{k} 对 {k} 必须可召回"
            );
        }
        // Custom types are the same: here we judge "whether the two sides are the same kind of thing", not "whether it is on the allowlist"
        assert_eq!(
            classify_type_drift(Some("drug"), Some("drug")),
            TypeDrift::Recall
        );
        // The old conclusions across types do not change at all
        assert_eq!(
            classify_type_drift(Some("person"), Some("organization")),
            TypeDrift::Disjoint
        );
    }
}

#[cfg(test)]
#[path = "resolution_plan_tests.rs"]
mod plan_tests;
