//! Type resolution: correct each entity's type -- **go finer, and pull back the ones that went wrong**.
//!
//! At first only the first half existed: extraction gave a coarse class, and resolution refined it toward its descendants. Once extraction began retrieving candidates per chunk,
//! it picked finer classes itself, and so it also picked wrong ones (measured: `Shaoxing → address`,
//! `chronic-disease management mini-program → entry_point`). The right answer is a **sibling** of the wrong class, not its descendant,
//! so the rule "candidates must be finer" blocked it at the door -- a rule I wrote into the prompt myself.
//!
//! Both directions are accepted now. A correction is naturally judged as crossing an axis, so it always goes to a human: overturning extraction's judgment is riskier than refining it,
//! and should not happen automatically.
//!
//! After 0009 there is a third input, and the most common one: **there is no class at all yet**. Once the catch-all class is deleted,
//! entities the ontology cannot hold are no longer stuffed into `concept`; they get `type_id IS NULL`. This tier has no
//! "extraction's judgment" to overturn; assigning it a class is filling in, not reclassifying, so it is **not judged as crossing an axis**,
//! and a high-confidence result is written straight to the database -- otherwise deleting the sentinel would cost a human look at every entity.
//!
//! **Why this can be done afterwards, but predicates cannot** (the asymmetry from 0001): a type is an annotation attached to a node, and can be affixed once enough evidence has accumulated;
//! a predicate is the fact itself, and `(NVIDIA, ?, Mellanox)` is not a fact at all.
//! So predicates go "record the original word first, map later", while types go "coarse first, then fine".
//!
//! What resolution has to work with differs entirely from what extraction had on the spot:
//!
//! 1. **`proposed_type`** -- the type name the model itself reported during extraction, kept only if the vocabulary did not already have it.
//!    The strongest signal, because the task changes from "understand what this is" to "which class in the ontology means this".
//! 2. **Entity profile** -- all the predicates it takes part in, accumulated across documents.
//! 3. **Evidence citations** -- the same sentences, but grouped together, and not competing for attention with dozens of other entities.
//!
//! Candidates come from **two routes**, taken as a union without merging scores: one route searches class descriptions with the profile, the other searches with the context vector
//! for entities whose classes are settled, and votes with their classes. The second route avoids the first route's weak spot (a Chinese profile against
//! English boilerplate descriptions), and it gets more accurate the larger the database. One route's distance lives in class space and the other's in entity space,
//! so merging them into one ranking is self-deception -- and measured distances within one route cannot even be compared across entities.
//!
//! Descendants of the coarse class are **ranked first**, but they are not the only choice -- the two ontologies' classification axes often do not coincide
//! (schema.org files software under CreativeWork, while extraction's coarse class was product).
//! The step that says no is adjudication: it sees both the descriptions and the coarse class.

use crate::{llm_util, ontology_index, AppState};
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// How many entities one round looks at. Enough for one human review pass, and enough to see whether retrieval is accurate.
const BATCH: i64 = 60;
/// How many candidates to retrieve per entity.
///
/// **Retrieval's job is to put candidates on the table; saying no is not its job.** So it is better to put two more out -- adjudication sees descriptions
/// and the current class, and can say "none of these"; but what retrieval misses, however good adjudication is, it cannot reach. The failures measured in 0001 P3a
/// were entirely on the retrieval side (`administrative_area` and `periodical` were never put on the table).
///
/// Raised from 8 to 10: the two routes alternate, five seats each. The cost is two more class definitions in the prompt, and a miss has no fallback.
const CANDIDATES: i64 = 10;
/// Look at a few neighbors of settled classes. Fewer cannot make up a vote count; more and the tail is all noise.
const NEIGHBOURS: i64 = 10;

/// Resolution suggestion for one entity (used by preview; writes nothing to the database).
#[derive(Debug, serde::Serialize)]
pub struct TypeSuggestion {
    pub entity_id: Uuid,
    pub name: String,
    /// The class it currently carries, which **may be absent** (0009)
    pub coarse: Option<String>,
    /// The current class's description. Adjudication has to judge whether "this class is right now"; looking at the key alone is not enough
    pub coarse_description: Option<String>,
    /// The coarse class's id. Adjudication pairs it with the target class and checks whether a person has approved this pair
    #[serde(skip)]
    pub coarse_id: Option<Uuid>,
    pub proposed_type: Option<String>,
    pub specific_type: Option<String>,
    pub fact_count: i64,
    /// The text sent for retrieval. **Returned to the caller** -- when retrieval finds nothing,
    /// the first thing to check is "what did we search with"
    pub profile: String,
    /// First-route candidates: profile -> class description
    pub candidates: Vec<utopia_core::models::TypeCandidate>,
    /// Second-route candidates: settled-class entities with similar context, voting by class
    pub neighbours: Vec<NeighbourVote>,
    /// All descendants of the coarse class. Adjudication tiers on this: a chosen class inside it = one step down,
    /// not inside = a different classification axis. **Not serialized** -- it is for tiering, not for people to read
    #[serde(skip)]
    pub descendants: std::collections::HashSet<Uuid>,
}

/// A class that neighbors voted for.
///
/// **Not merged with `candidates` into one ranking**: the two routes' distances, one in class space and one in entity space,
/// are not comparable to begin with -- and measured distances within one route cannot even be compared across entities. The union goes to adjudication,
/// each labelled with its source. A side benefit: a person reviewing the evidence can read this route's:
///"like Milvus, which is tagged software_application" is far more useful than "cosine 0.49".
#[derive(Debug, serde::Serialize)]
pub struct NeighbourVote {
    pub key: String,
    /// How many neighbors are this class
    pub votes: usize,
    /// How close the nearest neighbor is
    pub best_distance: f64,
    /// The names of the voting entities, as evidence for people
    pub examples: Vec<String>,
    /// Whether these neighbors all come from **the same batch of documents**.
    /// If so, this vote is discounted: an entity that appears only once has a context vector that is just that chunk's vector,
    /// and entities from the same document naturally become neighbors of each other, which is not type evidence
    pub same_document_only: bool,
}

/// Compute only, write nothing: the profile and candidates of each entity awaiting resolution.
///
/// Making this a separate step is deliberate -- before investing effort in building adjudication, first answer "can retrieval find it at all".
/// If it cannot be found, adjudication is useless however good it is.
pub async fn preview(state: &AppState, kb_id: Uuid) -> AppResult<Vec<TypeSuggestion>> {
    preview_with(state, kb_id, false).await
}

/// `unattended` = automatic run: looks only at entities the engine has not looked at (see `entities_for_type_resolution`)
async fn preview_with(
    state: &AppState,
    kb_id: Uuid,
    unattended: bool,
) -> AppResult<Vec<TypeSuggestion>> {
    // Backfill only the class half: this step does not use relations, and a large ontology has over a thousand of them
    let _ = ontology_index::refresh_scoped(
        state,
        kb_id,
        Some(utopia_store::ontology::TypeKind::Entity),
    )
    .await;
    let subjects = utopia_store::resolution::entities_for_type_resolution(
        &state.pool,
        kb_id,
        BATCH,
        unattended,
    )
    .await?;
    if subjects.is_empty() {
        return Ok(Vec::new());
    }
    // **Two queries per entity, not one.**
    //
    // The model's own wording in the profile comes first, but the rest of the paragraph is context, and it all goes into the vector alike. Measured:
    // the profile of "Gongshu District, Hangzhou" is `district. Gongshu District, Hangzhou. located_in by Renhe Tang chain pharmacy.
    // Renhe Tang chain pharmacy opened its 40th store in Gongshu District, Hangzhou` -- the whole paragraph is about the pharmacy,
    // so the candidates returned pharmacy and store, and administrative_area never came up once.
    // The name was diluted into the paragraph.
    //
    // So the name is sent on its own: a short query against a short label is exactly the shape this index is good at;
    // the profile query is still sent, for entities without a specific_type and those that can only be judged with context.
    // The two results are unioned -- one more embedding, in exchange for eliminating a whole class of misses.
    let profiles: Vec<String> = subjects.iter().map(profile_of).collect();
    let names: Vec<Option<String>> = subjects.iter().map(name_query_of).collect();
    // **Each route queries its own index** (see `entity_types.label_embedding`). Previously both routes shared one whole-text index, so the short-wording route
    // was taken over by classes with tautological definitions: `district. place` returned Map / Park / Country /
    // Museum; the four winners' embedded source text was all a one-line `X\nAn X.` pattern, while AdministrativeArea, with its hundred-word definition,
    // could not make the top eight. Short against short, long against long: that is what is comparable
    let name_queries: Vec<String> = names.iter().flatten().cloned().collect();
    let (profile_hits, name_hits) = tokio::join!(
        ontology_index::nearest_for_each(
            state,
            kb_id,
            &profiles,
            // Take more and then filter by ancestry: the filter runs after retrieval, so leave headroom for what gets filtered out
            CANDIDATES * 4,
            ontology_index::Target::Class,
        ),
        ontology_index::nearest_for_each(
            state,
            kb_id,
            &name_queries,
            CANDIDATES * 4,
            ontology_index::Target::ClassLabel,
        )
    );
    let profile_hits = profile_hits.unwrap_or_default();
    let name_hits = name_hits.unwrap_or_default();
    // The index each entity occupies in the two result sets: (profile, name). The name route only fires for entities that can be given
    // a wording, so its indices must be counted separately
    let mut slots: Vec<(usize, Option<usize>)> = Vec::with_capacity(subjects.len());
    let mut ni = 0usize;
    for (pi, n) in names.iter().enumerate() {
        let slot = n.as_ref().map(|_| {
            let k = ni;
            ni += 1;
            k
        });
        slots.push((pi, slot));
    }

    // Fetch the neighbors together first (#514): the sixty queries are independent of one another; fetch them with bounded concurrency, and reason in original order below.
    // Descendant sets are batched per coarse class: the recursive CTE for one coarse class is sent once per batch
    let ids: Vec<Uuid> = subjects.iter().map(|s| s.id).collect();
    let mut neighbour_lists =
        utopia_store::resolution::nearest_typed_for_each(&state.pool, kb_id, &ids, NEIGHBOURS)
            .await?;
    let mut descendants_memo = utopia_store::resolution::DescendantsMemo::default();

    let mut out = Vec::with_capacity(subjects.len());
    for (i, s) in subjects.iter().enumerate() {
        // Descendants of the coarse class are **ranked first, but are not the only choice**.
        //
        // At first this was a hard gate (only descendants allowed); measured, it blocked 4 of 17 entities' correct answers:
        // schema.org files both SoftwareApplication and Periodical under CreativeWork,
        // while extraction's coarse classes were product / organization -- the two classification axes simply do not coincide,
        // so a hard block just locks the correct answers permanently outside the door. The same lesson as the part_of incident:
        // **a signature is a guide, not a gate**; systematically losing data costs far more than an occasional wrong judgment.
        //
        // The step that says no is adjudication: it sees the descriptions and the coarse class, and can say "this is not it".
        // Entities with no class yet have no "descendants of the coarse class" axis to use (0009), so the whole class table is candidates,
        // and the ordering simply follows retrieval order
        let descendants = descendants_memo
            .get(&state.pool, kb_id, s.coarse_id)
            .await?;
        // **Take the two routes alternately; do not merge by distance.**
        //
        // Distances are not comparable between the two routes: a short query ("pharmaceutical group") yields distances systematically smaller than
        // a whole-paragraph profile, so sorting by distance would let the name route monopolize the top places. Measured after doing this,
        // Renhe Pharmaceutical Group, the National Medical Products Administration and the Chinese Medical Association all had their correct candidates pushed out --
        // in the previous round they had passed automatically. It is the same rule as the "take the union, do not merge scores" of the A/B routes,
        // which I violated here.
        //
        // After alternating, each route holds half the seats, and the size of one route's distance number no longer affects whether it is seen.
        let (pi, ni) = slots[i];
        let mut lists: Vec<Vec<_>> = vec![profile_hits.get(pi).cloned().unwrap_or_default()];
        if let Some(k) = ni {
            lists.push(name_hits.get(k).cloned().unwrap_or_default());
        }
        let mut seen_ids: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
        let mut ranked: Vec<utopia_core::models::TypeCandidate> = Vec::new();
        let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
        for rank in 0..longest {
            for list in &lists {
                let Some(c) = list.get(rank) else { continue };
                if Some(c.id) == s.coarse_id || !seen_ids.insert(c.id) {
                    continue;
                }
                ranked.push(c.clone());
            }
        }
        // **No longer moves descendants to the front unconditionally.**
        //
        // Previously this was `ranked.sort_by_key(|c| !descendants.contains(&c.id))`: the key is
        // a boolean, so the candidates were split into a "descendant" pile and a "non-descendant" pile, the first entirely overriding the second, **distance
        // does not count at all**. Measured: "deep blue vector database" (current class product, specific = vector database
        // software): the correct answer `software_application` ranked **first** in retrieval, yet was pushed out of the candidates
        // by product_collection (0.437), some_products (0.440) and five other unrelated product
        // descendants -- they got in only because of their origin, not because they resembled the entity.
        //
        // This preference was expressed three times: this sort line, the narrowing/correcting distinction in the prompt,
        // and the `crosses_axis` tiering. Removing the one that argues from the least evidence leaves the other two as they were -- an axis-changing
        // change still does not get written automatically and still goes to a human; the risk surface has not changed.
        //
        // The remaining order is the alternating retrieval order of the two routes: retrieval decides what is put on the table, adjudication decides what it is,
        // and `crosses_axis` decides whether a human looks. One layer, one job.
        let candidates: Vec<_> = ranked.into_iter().take(CANDIDATES as usize).collect();
        // Second route: settled-class entities with similar context, voting by class (fetched together above; taken here in order)
        let raw = std::mem::take(&mut neighbour_lists[i]);
        let mut votes: std::collections::BTreeMap<String, (usize, f64, Vec<String>, bool)> =
            std::collections::BTreeMap::new();
        for (name, _tid, key, distance, same_doc) in raw {
            let slot = votes
                .entry(key)
                .or_insert_with(|| (0, distance, Vec::new(), true));
            slot.0 += 1;
            slot.1 = slot.1.min(distance);
            if slot.2.len() < 3 {
                slot.2.push(name);
            }
            slot.3 &= same_doc;
        }
        let mut neighbours: Vec<NeighbourVote> = votes
            .into_iter()
            .filter(|(key, _)| Some(key) != s.coarse_key.as_ref())
            .map(
                |(key, (votes, best_distance, examples, same_document_only))| NeighbourVote {
                    key,
                    votes,
                    best_distance,
                    examples,
                    same_document_only,
                },
            )
            .collect();
        neighbours.sort_by(|a, b| {
            b.votes
                .cmp(&a.votes)
                .then(a.best_distance.total_cmp(&b.best_distance))
        });

        out.push(TypeSuggestion {
            entity_id: s.id,
            name: s.canonical_name.clone(),
            coarse: s.coarse_key.clone(),
            coarse_description: s.coarse_description.clone(),
            coarse_id: s.coarse_id,
            proposed_type: s.proposed_type.clone(),
            specific_type: s.specific_type.clone(),
            fact_count: s.fact_count,
            profile: profiles[i].clone(),
            candidates,
            neighbours,
            descendants,
        });
    }
    Ok(out)
}

/// Entity profile: the text sent for vector retrieval.
///
/// **The model's own reported type name goes first.** It is the strongest signal, and retrieval matches a class's
/// `label + description` -- a class name against a class definition is far closer than a string of predicates against one.
///
/// Names and aliases come next; predicates and citations follow as context.
///
/// This paragraph is the **context query**; it is sent once alongside the short query from [`name_query_of`], and the results are unioned.
fn profile_of(s: &utopia_store::resolution::TypeCandidateSubject) -> String {
    let mut parts: Vec<String> = Vec::new();
    // The model's own wording goes first; write both if both exist. They are **names**, and the retrieval target
    // (the class label) is also a name -- name against name is exactly the shape this index is good at
    for named in [s.specific_type.as_deref(), s.proposed_type.as_deref()] {
        if let Some(p) = named.map(str::trim).filter(|x| !x.is_empty()) {
            parts.push(p.to_string());
        }
    }
    parts.push(s.canonical_name.clone());
    if !s.other_names.is_empty() {
        parts.push(s.other_names.join(", "));
    }
    if !s.roles.is_empty() {
        parts.push(s.roles.join(" "));
    }
    // Citations follow as context, and only two sentences: they are indeed sentences about this entity, but they also carry
    // a pile of things unrelated to type (times, numbers, other entities); too many would shift the profile's centre of gravity
    // from "what this is" to "what this text is about". The query side already guarantees only subject-position
    for q in s.quotes.iter().take(2) {
        parts.push(q.chars().take(120).collect());
    }
    parts.join(". ")
}

/// One adjudication result.
#[derive(Debug, serde::Deserialize)]
struct Verdict {
    /// The number of that entry in the prompt -- **the key that matches results back to preview**.
    ///
    /// It used to rely on `name`. After 0009, unclassified entities with the same name can coexist (NULL ≠ NULL, see that document's
    /// unique-index section); measured in one database: 4 "Zhang Wei"s. Matching back by name would collapse them into
    /// one, the same entity_id pushed into picks four times, and hitting the (batch_id, entity_id) primary
    /// key on persist; and the other three would never be classified -- they are simply invisible to this route
    #[serde(default)]
    id: Option<usize>,
    /// Entity name. The fallback when the model omits the id, and sufficient when names are not duplicated
    name: String,
    /// The chosen class key; empty when it cannot be judged
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct VerdictReply {
    #[serde(default)]
    verdicts: Vec<Verdict>,
}

/// Anything below this line goes to a human, wherever it falls.
///
/// **It is not the main criterion for the gray zone**: measured, the model's self-reported confidence is bimodal -- 15 entries all ≥0.85,
/// 4 nulls, and none in between. A self-reported confidence is a style of writing, not a probability: the model picks a number
/// that matches its own tone. Used as a gate, the gate blocks nothing.
/// The real tiering is the "crosses a classification axis or not" criterion below; this one only catches the occasional low score.
const AUTO_THRESHOLD: f32 = 0.85;

/// The result of one resolution, which tells the caller clearly where each of the three tiers went.
#[derive(Debug, serde::Serialize)]
pub struct ResolutionOutcome {
    pub batch: Option<Uuid>,
    /// Changed automatically
    pub retyped: u32,
    /// Crossed a classification axis, or confidence was insufficient; left for a human
    pub for_review: Vec<ReviewItem>,
    /// Adjudication said "none of these", **together with the reason it gave**.
    ///
    /// Reporting only a count is not enough: the whole design of this step rests on "choosing none of these is a respectable answer",
    /// and that is the biggest tier -- without recording reasons, the biggest tier is opaque.
    /// It is the same rule as the "must be able to say why" in the ontology import preview.
    pub left_alone: Vec<DeclineNote>,
}

#[derive(Debug, serde::Serialize)]
pub struct DeclineNote {
    pub name: String,
    pub coarse: Option<String>,
    pub specific_type: Option<String>,
    /// The reason the model gave; empty when it did not mention this entity at all
    pub reason: Option<String>,
    /// The first candidate retrieval gave. When the reason does not make sense, this shows whether retrieval failed to find it
    /// or adjudication passed over it
    pub top_candidate: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct ReviewItem {
    pub entity_id: Uuid,
    pub name: String,
    pub coarse: Option<String>,
    /// The two ends of the pair. Approving this one approves **this pair of classes**, not this one entity.
    /// The starting end may be absent (0009) -- then there is no "pair of classes" to approve, and entities can only be changed one by one
    pub from_type_id: Option<Uuid>,
    pub to_type_id: Uuid,
    pub choice: String,
    pub confidence: f32,
    pub reason: Option<String>,
    /// The chosen class is **not** in the coarse class's subtree -- it changes the classification axis, not one step down
    pub crosses_axis: bool,
}

/// Run one round of type resolution and persist it.
///
/// **Persisting carries no actor, even though a person clicked the run.** The actor parameter of `retype_entities`
/// now has two roles: the ledger records "who changed it", and it also decides `type_source` --
/// with an actor it is `human`, and `human` means **the engine never touches this entity again**.
///
/// These two questions are not the same thing:
///
/// | | Who initiates | Who decides what this entity is |
/// |---|---|---|
/// | Manually change an entity's type | Person | Person |
/// | Approve a class pair + name entities | Person | Person |
/// | **Run a round of resolution** | Person clicked run | **The engine** |
///
/// If the third row passes a user, it is as good as declaring "this class was decided by a person", so **an entity that has once been through resolution
/// is never resolved again**. Measured: in a database with no manual PATCH records at all (`entity.retyped`
/// audit: 0 rows), after a resolution run every entity became `type_source = human`,
/// and the next preview returned an empty list.
///
/// Who clicked the run is recorded in the `ontology.types_resolved` audit, with the batch id, and can be looked up.
pub async fn resolve(state: &AppState, kb_id: Uuid) -> AppResult<ResolutionOutcome> {
    resolve_with(state, kb_id, false).await
}

/// An automatic round after extraction ends (0016 C2).
///
/// It is the same judgment as the round a person clicks -- refinements within the original class's subtree are changed automatically, cross-axis ones are left to a human -- the only differences are
/// two: it looks only at entities the engine has not seen, and those it has seen get `type_resolved_at`, so the next round does not revisit them.
/// The tier awaiting a human is **not persisted** here: they stay among the candidates, and a human will see them again on a run from the Ontology page
/// (that path does not look at the marker). A database whose switch is off does nothing at all.
///
/// One task runs at most `MAX_ROUNDS` rounds, each one model call over sixty entities; more wait until the next document
/// finishes extraction and is queued then -- tasks are enqueued deduplicated per database, so they do not pile up
pub async fn resolve_types_job(state: &AppState, kb_id: Uuid) -> anyhow::Result<()> {
    const MAX_ROUNDS: usize = 10;
    let kb = utopia_store::kbs::get(&state.pool, kb_id).await?;
    if !kb.auto_type_resolution {
        return Ok(());
    }
    let (mut retyped, mut for_review, mut left_alone, mut rounds) = (0u32, 0usize, 0usize, 0usize);
    for _ in 0..MAX_ROUNDS {
        let outcome = resolve_with(state, kb_id, true).await?;
        let seen = outcome.retyped as usize + outcome.for_review.len() + outcome.left_alone.len();
        if seen == 0 {
            break;
        }
        rounds += 1;
        retyped += outcome.retyped;
        for_review += outcome.for_review.len();
        left_alone += outcome.left_alone.len();
    }
    if rounds > 0 {
        let _ = utopia_store::audit::record_opt(
            &state.pool,
            Some(kb_id),
            None,
            "ontology.types_resolved",
            "knowledge_base",
            Some(kb_id),
            serde_json::json!({
                "via": "job", "rounds": rounds, "retyped": retyped,
                "for_review": for_review, "left_alone": left_alone,
            }),
        )
        .await;
        tracing::info!(%kb_id, rounds, retyped, for_review, left_alone, "类型消解自动跑完成");
    }
    Ok(())
}

async fn resolve_with(
    state: &AppState,
    kb_id: Uuid,
    unattended: bool,
) -> AppResult<ResolutionOutcome> {
    let items = preview_with(state, kb_id, unattended).await?;
    // Automatic run: this batch counts as looked at whichever tier it lands in (including those for which retrieval gave no candidates --
    // they still have none next round), otherwise the same batch of entities would come back every round
    if unattended && !items.is_empty() {
        let ids: Vec<Uuid> = items.iter().map(|i| i.entity_id).collect();
        utopia_store::resolution::mark_type_judged(&state.pool, kb_id, &ids).await?;
    }
    let items: Vec<_> = items
        .into_iter()
        .filter(|i| !i.candidates.is_empty() || !i.neighbours.is_empty())
        .collect();
    if items.is_empty() {
        return Ok(ResolutionOutcome {
            batch: None,
            retyped: 0,
            for_review: Vec::new(),
            left_alone: Vec::new(),
        });
    }

    let kb = utopia_store::kbs::get(&state.pool, kb_id).await?;
    let settings = utopia_store::settings::get(&state.pool, kb.workspace_id)
        .await?
        .ok_or_else(|| AppError::invalid("no_chat_model", "Chat model not configured"))?;
    let client = llm_util::chat_client(&settings)
        .ok_or_else(|| AppError::invalid("no_chat_model", "Chat model not configured"))?;

    let reply = client
        .chat(&[utopia_llm::ChatMessage {
            role: "user".into(),
            content: adjudication_prompt(&items),
        }])
        .await
        .map_err(AppError::Other)?;
    let block = utopia_extract::json_block(&reply).map_err(AppError::Other)?;
    let parsed: VerdictReply =
        serde_json::from_str(&block).map_err(|e| AppError::Other(e.into()))?;

    // Name -> index **list**, not a single entry. Unclassified entities with the same name can coexist (0009),
    // collapsing them into one would leave some of them never getting a verdict. Verdicts are matched back by id first,
    // and the name is only the fallback when the model omits the id
    let mut by_name: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
    for (i, it) in items.iter().enumerate() {
        by_name.entry(it.name.as_str()).or_default().push(i);
    }
    // A pair a person has approved: the same pair no longer goes to a human. Crossing an axis is a matter between classes,
    // and the entity just happened to collide with it -- the second city should not be asked again
    let approved = utopia_store::resolution::approved_refinements(&state.pool, kb_id).await?;
    let mut picks: Vec<(Uuid, Uuid)> = Vec::new();
    let mut for_review: Vec<ReviewItem> = Vec::new();
    let mut left_alone: Vec<DeclineNote> = Vec::new();
    let mut decided: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let decline = |item: &TypeSuggestion, reason: Option<String>| DeclineNote {
        name: item.name.clone(),
        coarse: item.coarse.clone(),
        specific_type: item.specific_type.clone(),
        reason,
        top_candidate: item.candidates.first().map(|c| c.key.clone()),
    };
    for v in &parsed.verdicts {
        // The id takes precedence; if omitted, fall back to the name, taking the first entry under that name that has **not yet been adjudicated**.
        // An out-of-range id counts as not given -- the model occasionally makes one up
        let idx = v.id.filter(|i| *i < items.len()).or_else(|| {
            by_name
                .get(v.name.as_str())
                .and_then(|ids| ids.iter().find(|i| !decided.contains(i)).copied())
        });
        let Some(idx) = idx else { continue };
        // For the same entry, only the first verdict counts. When the model answers twice, the second would push the same entity_id
        // into picks once more, and hit the primary key on persist
        if !decided.insert(idx) {
            continue;
        }
        let item = &items[idx];
        // **The string "null" is also null.** The model sometimes gives JSON null and sometimes these four letters,
        // and looking that up as a key among the candidates always fails, so this entry gets recorded as "picked a key outside the candidates" --
        // a fabricated rejection reason overwrites the one the model actually gave. The rejection reason is the most important output of this step,
        // and having it polluted by our own parsing is worse than having none
        let Some(choice) = v.choice.as_deref().map(str::trim).filter(|c| {
            !c.is_empty() && !c.eq_ignore_ascii_case("null") && !c.eq_ignore_ascii_case("none")
        }) else {
            left_alone.push(decline(item, v.reason.clone()));
            continue;
        };
        // Only keys in the candidate list are accepted. An answer outside the list is not a "better judgment",
        // it is the model writing from memory a name from schema.org -- which the ontology may not have
        let Some(target) = item.candidates.iter().find(|c| c.key == choice) else {
            left_alone.push(decline(
                item,
                Some(format!("chose {choice}, which is not among the candidates")),
            ));
            continue;
        };
        let confidence = v.confidence.unwrap_or(0.0);
        // **Tiering looks at whether an axis was crossed, not at the number the model self-reported.**
        //
        // Inside the coarse class's subtree = one step down, extraction's judgment is not overturned, changed automatically.
        // Not inside = a different classification axis (something judged product ended up under CreativeWork),
        // that is reclassification, not refinement, and it deserves a human look. The only clear error measured in that round
        // ("China Data Intelligence" -> publication_issue) was exactly of this kind.
        //
        // **Corrections also go this way, and naturally so**: after extraction picked a wrong finer class (Shaoxing -> address),
        // the right answer is its sibling, not its descendant, so it is always judged as crossing an axis and always goes to a human.
        // That is exactly what is wanted -- overturning extraction's judgment is riskier than refining it, and should not happen automatically.
        //
        // **Entities with no class yet do not count as crossing an axis** (0009): there is no "extraction judgment" on them to be
        // overturned; assigning a class for the first time is filling in, not reclassifying. If it were judged as crossing an axis here, it would amount to
        // every entity needing a human look once the catch-all class is deleted, and the whole automation would be useless
        let crosses_axis = match item.coarse_id {
            Some(from) => {
                !item.descendants.contains(&target.id) && !approved.contains(&(from, target.id))
            }
            None => false,
        };
        if confidence >= AUTO_THRESHOLD && !crosses_axis {
            picks.push((item.entity_id, target.id));
        } else {
            for_review.push(ReviewItem {
                entity_id: item.entity_id,
                name: item.name.clone(),
                coarse: item.coarse.clone(),
                from_type_id: item.coarse_id,
                to_type_id: target.id,
                choice: choice.to_string(),
                confidence,
                reason: v.reason.clone(),
                crosses_axis,
            });
        }
    }
    // Entities adjudication never mentioned also count as "untouched"; otherwise the three tiers would not add up to the total
    for (i, item) in items.iter().enumerate() {
        if !decided.contains(&i) {
            left_alone.push(decline(item, None));
        }
    }

    let (batch, retyped) = if picks.is_empty() {
        (None, 0)
    } else {
        let (b, n) =
            utopia_store::resolution::retype_entities(&state.pool, kb_id, &picks, None).await?;
        (Some(b), n)
    };
    Ok(ResolutionOutcome {
        batch,
        retyped,
        for_review,
        left_alone,
    })
}

/// Adjudication prompt.
///
/// **"None of these" must be a respectable answer.** This is exactly the opposite of the `related_to` escape hatch:
/// there, the catch-all option destroys information, so it was removed; here keeping the coarse class loses nothing -- the entity is still on the graph,
/// its facts are still attached, it just did not become more specific. Forcing the model to pick one of the candidates gets a batch of
/// confident errors, and they do not enter the timeline and are hard to notice.
fn adjudication_prompt(items: &[TypeSuggestion]) -> String {
    let mut blocks = Vec::new();
    // **The number is the key, the name is not** (0009). Unclassified entities with the same name can coexist; in one database
    // measured 4 "Zhang Wei"s -- matching back by name would collapse them into one entry
    for (i, it) in items.iter().enumerate() {
        // Give the current class together with its description: to judge "whether this class is right now", the key alone is not enough --
        // keys from imported ontologies often cannot explain themselves (what is `entry_point`?)
        //
        // An entity with no class says so plainly (0009). This line used to always hold concept, and the model read
        // "it is already a concept" -- a wrong prior; now it reads "not yet decided", which is the truth
        let current = match (&it.coarse, it.coarse_description.as_deref().map(str::trim)) {
            (Some(k), Some(d)) if !d.is_empty() => format!("{k} ({d})"),
            (Some(k), _) => k.clone(),
            (None, _) => "not yet typed".into(),
        };
        let mut lines = vec![format!(
            "### [{}] {}\ncurrently: {}\nthe extractor called it: {}\nseen as: {}",
            i,
            it.name,
            current,
            it.specific_type.as_deref().unwrap_or("-"),
            it.profile.chars().take(200).collect::<String>()
        )];
        lines.push("candidates:".into());
        for c in &it.candidates {
            let d = c.description.trim();
            lines.push(if d.is_empty() {
                format!("- {} ({})", c.key, c.label)
            } else {
                format!("- {}: {d}", c.key)
            });
        }
        if !it.neighbours.is_empty() {
            let n: Vec<String> = it
                .neighbours
                .iter()
                .take(3)
                .map(|n| format!("{} (like {})", n.key, n.examples.join(", ")))
                .collect();
            lines.push(format!("similar entities are typed: {}", n.join("; ")));
        }
        blocks.push(lines.join("\n"));
    }
    format!(
        "You are fixing entity types in a knowledge graph. Each entity below has a type it was \
         given during extraction, and a list of candidate types retrieved from the ontology.\n\
         \n\
         For each entity choose ONE candidate key, or null. There are two reasons to choose \
         a candidate, and they are different:\n\
         \n\
         **Narrowing** — the current type is right but broad, and a candidate says the same \
         thing more precisely (organization → hospital).\n\
         \n\
         **Correcting** — the current type is simply wrong, and a candidate is right. This \
         happens because extraction picks from a retrieved shortlist and can pick badly: a city \
         typed as an address, an app typed as an entry point. A correcting candidate is \
         usually a sibling of the current type rather than a narrower version of it, so do not \
         withhold it on the grounds that it is not more specific. Say plainly in the reason \
         that the current type is wrong; a person will see this one before it is applied.\n\
         \n\
         Choose null whenever any of these hold, and expect null to be a common answer:\n\
         - no candidate actually means the thing (the list is retrieved by similarity, so it \
           usually contains near-misses and sometimes contains nothing right at all);\n\
         - the current type is already right and no candidate is more precise;\n\
         - the entity is not a thing of that kind at all — a quantity, a capability, a phrase.\n\
         Keeping the current type loses nothing: the entity and its facts stay exactly as they \
         are. Picking a wrong type is worse than picking none, because it reads as a decided \
         fact.\n\
         \n\
         confidence is your own 0~1: use above 0.85 only when the candidate's definition \
         plainly describes this entity, not when it is merely the closest of a weak list.\n\
         \n\
         reason is required on every verdict, including the nulls — especially the nulls. \
         When you choose null, say which candidate came closest and what it got wrong \
         (\"nearest was publication_issue, but that is one issue of a journal, not the \
         journal\"). A refusal without a reason cannot be acted on: nobody can tell whether \
         the ontology is missing the class, or the search failed to surface it, or you read \
         the entity differently.\n\
         \n\
         {}\n\
         \n\
         id is the number in the heading, and it is what identifies the verdict — not the \
         name. Two entries can carry the same name and still be different things (two people \
         called Zhang Wei, each with their own facts); judge each one on its own block and \
         give one verdict per id you answer. Never merge two ids into one verdict.\n\
         \n\
         Output exactly one JSON object:\n\
         {{\"verdicts\":[{{\"id\":0,\"name\":\"entity name exactly as given\",\"choice\":\"candidate key or null\",\"confidence\":0.0,\"reason\":\"one short clause\"}}]}}",
        blocks.join("\n\n")
    )
}

/// The name-only query: the model's own wording, and nothing else at all.
///
/// **The reason for sending them separately is dilution.** In the context query these words come first, but the whole paragraph after them goes into the vector alike,
/// and the context is often about someone else: the citation for "Gongshu District, Hangzhou" is about a pharmacy, so the candidates came back as
/// pharmacy and store. A short query against a short label does not have this problem -- the retrieval target (the class's label)
/// is a name to begin with.
///
/// If neither wording exists, return `None`: the name-only query would be identical to the start of the context query,
/// and sending it again would waste an embedding.
fn name_query_of(s: &utopia_store::resolution::TypeCandidateSubject) -> Option<String> {
    let parts: Vec<&str> = [s.specific_type.as_deref(), s.proposed_type.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(". "))
}
