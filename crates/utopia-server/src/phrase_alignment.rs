//! Relation phrases bind to attributes by signature (the second slice of 0044 decision 3; for the ledger side see `utopia_store::phrase_bindings`,
//! for the contract see `utopia_extract::phrase_align`).
//!
//! Signature = phrase × the class of the subject × the class of the object (a literal object is recorded as "value"). The classes at both ends come from the category-word bindings
//! written onto entities as `type_id`, so this step runs after category-word alignment. The distinct signatures in a library are far fewer than the statements,
//! and each is judged only once: a binding happens only on **two agreeing votes** (the second vote's candidates are reversed, so "picking the first" cannot pass as agreement);
//! a disagreement is recorded as undecided for review (#725 alignment queue), and if no attribute matches it is recorded as none. The statement stays in the open
//! graph, nothing is lost, and the signature counts toward the workbench's suggestions. A binding is judged stale against the attribute's `updated_at` and the newest attribute in the library,
//! so when the ontology changes only the stale ones are judged again. This slice only records judgements; turning statements into typed facts by binding is the next slice.
//!
//! Where candidate attributes come from: first filter by declared domain/range: attributes where both ends of the signature's classes fall within the domain/range, or
//! where the attribute declares no domain/range (both directions count, since the binding can be reversed). **A signature with one end unbound to a class gets only
//! the attributes that declare nothing on that end**: when category words are not yet bound, attributes that declare a domain are not given to the model, since on the four NVDA documents
//! every row of the cash flow statement was bound to a generic value (the subject NVIDIA has no class, and value's domain is metrics); the judge
//! ruling it wrong was half of that. If the list still exceeds the cap after filtering, do not judge: guessing is worse than not judging.

use crate::extraction::chat_retrying_rate_limits_at;
use crate::llm_util;
use crate::state::AppState;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use utopia_core::models::RelationTypeView;
use utopia_extract::phrase_align::{
    build_phrase_messages, parse_phrase_response, Direction, Marks, PhraseItem, PropertyCandidate,
};
use utopia_store::phrase_bindings::{self, Decision, PhraseSignature};
use uuid::Uuid;

/// How many signatures to ask about at a time.
const BATCH: usize = 12;
/// After filtering by domain/range, at most this many candidate attributes; if there are more, do not judge.
const CANDIDATE_LIMIT: usize = 60;

/// One vote: which attribute this signature picked, in which direction, and, if the attribute is a state, which end a moment marks (#966).
/// None = no attribute matched
type Vote = Option<(String, Direction, Option<Marks>)>;

/// A binding the agent made to a state attribute but never asked which end a moment marks (the judgements before this column, #966): even if the fingerprint is unchanged, asked once,
/// asking only this cell, without re-judging the binding (0053 revision 2026-09-27). Those already asked are not asked again; a column that got no value goes to the alignment queue
/// to wait for a person. It enters only the todo at the start of a run, not the "did anything change" check at the end
fn awaits_marks(b: &phrase_bindings::Binding, props: &[RelationTypeView]) -> bool {
    b.status == "bound"
        && b.marks.is_none()
        && b.marks_asked_at.is_none()
        && b.relation_type_id
            .and_then(|id| props.iter().find(|p| p.id == id))
            .is_some_and(|p| p.temporal == "state")
}

/// A class together with all its ancestors. Candidates are matched on it: an attribute whose domain is declared as legal_entity,
/// with organization as its subclass, makes that attribute a candidate for organization signatures (#807 first item).
pub(crate) type Closure = HashMap<Uuid, Vec<Uuid>>;

pub(crate) fn closures<'a>(classes: impl IntoIterator<Item = (Uuid, &'a [Uuid])>) -> Closure {
    let classes: Vec<(Uuid, &[Uuid])> = classes.into_iter().collect();
    let parents: HashMap<Uuid, &[Uuid]> = classes.iter().copied().collect();
    classes
        .iter()
        .map(|(id, direct)| {
            let mut seen: Vec<Uuid> = vec![*id];
            let mut stack: Vec<Uuid> = direct.to_vec();
            // Multiple inheritance and diamonds: UNION semantics, so the same ancestor enters only once; cycles cannot occur (the editor does not allow them)
            while let Some(p) = stack.pop() {
                if seen.contains(&p) {
                    continue;
                }
                seen.push(p);
                if let Some(pp) = parents.get(&p) {
                    stack.extend_from_slice(pp);
                }
            }
            seen.sort();
            (*id, seen)
        })
        .collect()
}

/// How a candidate was matched: `via` is the basis of matching through inheritance (empty = matched directly, or nothing declared).
struct Fit {
    via: Vec<(Uuid, Uuid)>,
}

/// Whether one of the declared classes is this end's class or one of its ancestors. Nothing declared means unrestricted; when this end is not bound to a class, it is accepted only
/// if nothing is declared there. Returns the matched (declared class, this end's class) when it is not a direct match.
fn within(
    declared: &[Uuid],
    class: Option<Uuid>,
    closure: &Closure,
) -> Option<Option<(Uuid, Uuid)>> {
    if declared.is_empty() {
        return Some(None);
    }
    let c = class?;
    if declared.contains(&c) {
        return Some(None);
    }
    let up = closure.get(&c)?;
    declared
        .iter()
        .find(|d| up.contains(d))
        .map(|d| Some((*d, c)))
}

/// Structurally matches (the criterion the ontology agent uses when given the word list for each batch, the same as the alignment candidates)
pub(crate) fn structurally_fits(
    p: &RelationTypeView,
    sig: &PhraseSignature,
    closure: &Closure,
) -> bool {
    fits(p, sig, closure).is_some()
}

/// The classes at both ends of the signature fall within the attribute's declared domain/range, counting through inheritance too; both directions count.
fn fits(p: &RelationTypeView, sig: &PhraseSignature, closure: &Closure) -> Option<Fit> {
    fits_direction(p, sig, closure, Direction::Forward)
        .or_else(|| fits_direction(p, sig, closure, Direction::Reverse))
}

// Admitting a property in either orientation does not admit both orientations.
// Two agreeing model votes used to turn a forward-only candidate into a reverse binding.
fn fits_direction(
    p: &RelationTypeView,
    sig: &PhraseSignature,
    closure: &Closure,
    direction: Direction,
) -> Option<Fit> {
    let mut via = Vec::new();
    if sig.object_is_value {
        if p.kind != "attribute" || direction != Direction::Forward {
            return None;
        }
        via.extend(within(&p.domains, sig.subject_type_id, closure)?);
        return Some(Fit { via });
    }
    if p.kind == "attribute" {
        // Signatures whose object is an entity can also bind text attributes (0044 revision 2026-10-22): the value of legal representative and registered address
        // is a name, which extraction lists as a thing; only the forward direction is accepted, since the name is the value. Numbers, dates and true/false are not names.
        // Attributes with no declared domain are not counted (`known_as` and the like): nothing declared means unrestricted, so it would appear in every relation signature's
        // candidates, the lesson of the generic value attribute on NVDA swallowing every row of the cash flow statement
        if p.datatype.as_deref() != Some("text")
            || direction != Direction::Forward
            || p.domains.is_empty()
        {
            return None;
        }
        via.extend(within(&p.domains, sig.subject_type_id, closure)?);
        return Some(Fit { via });
    }
    if p.kind != "relation" {
        return None;
    }
    let (subject, object) = match direction {
        Direction::Forward => (sig.subject_type_id, sig.object_type_id),
        Direction::Reverse => (sig.object_type_id, sig.subject_type_id),
    };
    let a = within(&p.domains, subject, closure)?;
    let b = within(&p.ranges, object, closure)?;
    via.extend(a);
    via.extend(b);
    Some(Fit { via })
}

fn invalid_agent_direction(
    b: &phrase_bindings::Binding,
    sig: &PhraseSignature,
    props: &[RelationTypeView],
    closure: &Closure,
) -> bool {
    b.decided_by == "agent"
        && b.status == "bound"
        && b.relation_type_id
            .and_then(|id| props.iter().find(|p| p.id == id))
            .is_some_and(|p| match b.direction.as_deref() {
                Some("forward") => fits_direction(p, sig, closure, Direction::Forward).is_none(),
                Some("reverse") => fits_direction(p, sig, closure, Direction::Reverse).is_none(),
                _ => true,
            })
}

/// This relation is called this phrase: equal to it after normalising the key or label (the same normalisation as category words). Similar is not enough:
/// "validated by tests" is not validated_by; that is the model's problem
fn named_by(p: &RelationTypeView, phrase: &str) -> bool {
    phrase_bindings::normalize(&p.key) == phrase || phrase_bindings::normalize(&p.label) == phrase
}

/// A signature's phrase is exactly a declared relation (#1242): it is called this phrase, the subject's class falls within the declared domain,
/// and the object's class falls within the range (counting through inheritance), forward direction only. A domain or range that is not declared does not count: nothing declared means unrestricted,
/// not "exactly this one"; and a signature with no class at either end does not count either. State relations do not count: binding them would also need to say which end a statement carrying only one date marks
/// (`marks`, #966), which the ontology cannot answer, so it is left to the aligner. It binds and asks about this cell as it goes; cells it cannot ask about
/// go to the queue for a person. If more than one relation is called this phrase and both ends match, none is accepted: two relations with the same name should be picked between by a person or the model
/// (see the note above)
fn declared<'a>(
    sig: &PhraseSignature,
    props: &'a [RelationTypeView],
    closure: &Closure,
) -> Option<&'a RelationTypeView> {
    if sig.object_is_value {
        return None;
    }
    let mut fitting = props.iter().filter(|p| {
        p.kind == "relation"
            && p.temporal != "state"
            && !p.domains.is_empty()
            && !p.ranges.is_empty()
            && named_by(p, &sig.phrase)
            && fits_direction(p, sig, closure, Direction::Forward).is_some()
    });
    let only = fitting.next()?;
    fitting.next().is_none().then_some(only)
}

/// Judgements the aligner no longer makes: the person's, and those the ontology judged that are still the relation it declares. A row the ontology judged has no fingerprint,
/// so comparing fingerprints would always call it "stale": if it is not let through first, every round sends it to the model, and at the end, thinking it had changed, queues another copy
fn settled(
    b: &phrase_bindings::Binding,
    sig: &PhraseSignature,
    props: &[RelationTypeView],
    closure: &Closure,
) -> bool {
    b.decided_by == "person"
        || (b.decided_by == "ontology"
            && b.relation_type_id.is_some()
            && declared(sig, props, closure).map(|p| p.id) == b.relation_type_id)
}

/// Among these signatures, those the ontology can judge (`declared`), and that no one has judged yet nor judged against the relation currently declared (`settled`).
/// Both look only at the signature's keys (phrase, the classes at both ends, whether the object is a literal),
/// so there is no need to count all the library's signatures first
fn undecided(
    keys: HashSet<SignatureKey>,
    existing: &HashMap<SignatureKey, phrase_bindings::Binding>,
    props: &[RelationTypeView],
    closure: &Closure,
) -> HashSet<SignatureKey> {
    keys.into_iter()
        .filter(|key| {
            let sig = keyed(key);
            declared(&sig, props, closure).is_some()
                && !existing
                    .get(key)
                    .is_some_and(|b| settled(b, &sig, props, closure))
        })
        .collect()
}

/// After a pushed document is extracted (0054): among the signatures its statements fall on, those whose phrase is exactly a declared relation are bound by the ontology
/// directly and recorded as ontology-judged (#1242). The model is not asked, and this runs even without a dialogue model configured; other signatures remain for the aligner.
/// The judgement stays a row on the signature (0044 decision 3), visible and editable by people; a person's judgement is untouched (the `decide_on` rule),
/// and one already judged by the ontology against the same relation is not rewritten: the moment of judgement and the regression record should not be refreshed on every push.
/// Returns the number of judgements written
pub(crate) async fn bind_declared(
    pool: &sqlx::PgPool,
    kb_id: Uuid,
    document_id: Uuid,
) -> anyhow::Result<usize> {
    let props = utopia_store::ontology::relation_type_views(pool, kb_id).await?;
    let keys: HashSet<SignatureKey> =
        phrase_bindings::signature_keys_of_document(pool, kb_id, document_id)
            .await?
            .into_iter()
            .filter(|(phrase, ..)| props.iter().any(|p| named_by(p, phrase)))
            .collect();
    // Most pushes have no phrase that names a relation: then there is no need to count the library-level signatures
    if keys.is_empty() {
        return Ok(0);
    }
    let classes = utopia_store::graph::entity_types(pool, kb_id).await?;
    let closure = closures(classes.iter().map(|c| (c.id, c.parents.as_slice())));
    let existing: HashMap<_, _> = phrase_bindings::bindings(pool, kb_id)
        .await?
        .into_iter()
        .map(|b| (b.key(), b))
        .collect();
    // The signatures this push landed on cannot be judged by the ontology, or were already judged after the first document: even without counting the library's signatures we know there is nothing to judge (#1296)
    let keys = undecided(keys, &existing, &props, &closure);
    if keys.is_empty() {
        return Ok(0);
    }
    let mut written = 0;
    for s in phrase_bindings::signatures(pool, kb_id)
        .await?
        .iter()
        .filter(|s| keys.contains(&s.key()))
    {
        let Some(p) = declared(s, &props, &closure) else {
            continue;
        };
        if phrase_bindings::decide(
            pool,
            kb_id,
            s,
            Decision {
                relation_type_id: Some(p.id),
                direction: Some(Direction::Forward.as_str()),
                status: "bound",
                votes: &serde_json::json!({ "reason": "declared_relation" }),
                decided_by: "ontology",
                basis: None,
                marks: None,
                marks_asked: false,
            },
        )
        .await?
        {
            written += 1;
        }
    }
    Ok(written)
}

/// The signature's key: phrase + the classes at both ends + whether the object is a literal (same shape as `PhraseSignature::key`)
type SignatureKey = (String, Option<Uuid>, Option<Uuid>, bool);

/// A signature with only its key: `settled` looks only at these cells on the key, so it need not count the library's signatures for it
fn keyed(
    (phrase, subject_type_id, object_type_id, object_is_value): &SignatureKey,
) -> PhraseSignature {
    PhraseSignature {
        phrase: phrase.clone(),
        subject_type_id: *subject_type_id,
        subject_type_key: None,
        object_type_id: *object_type_id,
        object_type_key: None,
        object_is_value: *object_is_value,
        count: 0,
        examples: Vec::new(),
        quotes: Vec::new(),
        example_statement_ids: Vec::new(),
    }
}

/// This moment's candidates and fingerprint for each signature
type Considered<'a> = HashMap<SignatureKey, (Vec<&'a RelationTypeView>, String)>;

/// Each live signature's candidates at this moment (matched through inheritance) and fingerprint (0053). Computed once at the start to decide whom to judge,
/// and recomputed at the end with reloaded inputs to decide whether to queue again: the world may have changed between the two
fn consider<'a>(
    sigs: &[PhraseSignature],
    props: &'a [RelationTypeView],
    closure: &Closure,
    versions: &HashMap<Uuid, chrono::DateTime<chrono::Utc>>,
    shortlist: Option<&Shortlist>,
) -> Considered<'a> {
    let empty: Vec<Uuid> = Vec::new();
    sigs.iter()
        .map(|s| {
            let mut fitting: Vec<&RelationTypeView> = props
                .iter()
                .filter(|p| fits(p, s, closure).is_some())
                .collect();
            // When too many structurally match, keep only the nearest few (see `shortlist`), ranked by relevance: the first vote looks at the most similar first
            if let Some(keep) = shortlist.and_then(|m| m.get(&s.key())) {
                fitting.retain(|p| keep.contains(&p.id));
                fitting.sort_by_key(|p| keep.iter().position(|k| *k == p.id));
            }
            let cands: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> = fitting
                .iter()
                .filter_map(|p| versions.get(&p.id).map(|at| (p.id, *at)))
                .collect();
            let up = |c: Option<Uuid>| -> &[Uuid] {
                c.and_then(|c| closure.get(&c))
                    .map(Vec::as_slice)
                    .unwrap_or(&empty)
            };
            let basis = phrase_bindings::basis_of(
                up(s.subject_type_id),
                up(s.object_type_id),
                s.object_is_value,
                &cands,
            );
            (s.key(), (fitting, basis))
        })
        .collect()
}

/// Whether to cast the second vote. The second vote guards against "picking the first" passing as agreement; what needs two agreeing votes is the **binding**. If the first vote
/// says no signature matches, the second vote cannot bind it however it answers: answering "none" is "none", answering with one is "cannot decide".
/// Of the 864 signatures in one library's bench run, the 317 the first vote said matched none, none was bound in the end (bench README,
/// 2026-09-28), so this vote is saved. Those the first vote did not answer do not get a vote either: with one vote missing, no conclusion can be drawn anyway.
/// A re-ask for marks still casts two votes: it requires two votes saying the same value
fn second_vote_is_due(first: &Vote, first_answered: bool, marks_only: bool) -> bool {
    marks_only || (first_answered && first.is_some())
}

/// The object of a category-word rule is read from the category word's own characters ("british film" reads as British). Category words whose
/// head word ("state", "ship") has no readable characters: of 159 category words in one round, 127 are single words, and none of the 8 rules proposed could read an object (bench README, 2026-09-28).
/// These are not asked about.
///
/// Only writing split by spaces is recognised: a run of ASCII letters is one word. Hyphenated or numeric forms ("1968", "british-governed") and unspaced text
/// (such as a Chinese film title) cannot be seen to have modifiers, so they are still asked
fn has_modifier(kind_word: &str) -> bool {
    !kind_word.trim().chars().all(|c| c.is_ascii_alphabetic())
}

/// Splits signatures into batches, putting those with similar candidates together. Returns the index of each signature in each batch.
///
/// The attribute definition table is written once per batch and takes nearly half of one call's input: when cut every twelve in arrival order,
/// signatures in the same batch are unrelated (one covers birthplace, one covers director, one covers club), each with ten candidates that barely
/// overlap, adding up to half the ontology (about fifty of the ninety-six attributes in the measurement library). This changes only the ordering: each signature
/// sees every one of its candidates, and the judgement is unchanged.
///
/// Greedy: each batch starts from the first signature not yet placed, and each time adds the signature that keeps the union of the batch's candidates smallest; on a tie take
/// the earlier one. The result is determined only by the input, so the same signatures always form the same batches. Signatures with no candidates are not asked of the model,
/// placed last, not taking up room in a batch that has candidates
fn batch_by_candidates<K: Copy + Eq + std::hash::Hash>(
    sets: &[Vec<K>],
    size: usize,
) -> Vec<Vec<usize>> {
    let size = size.max(1);
    let mut left: Vec<usize> = (0..sets.len()).filter(|i| !sets[*i].is_empty()).collect();
    let empty: Vec<usize> = (0..sets.len()).filter(|i| sets[*i].is_empty()).collect();
    let mut out: Vec<Vec<usize>> = Vec::new();
    while !left.is_empty() {
        let first = left.remove(0);
        let mut batch = vec![first];
        let mut union: HashSet<K> = sets[first].iter().copied().collect();
        while batch.len() < size && !left.is_empty() {
            let (at, _) = left
                .iter()
                .enumerate()
                .map(|(at, i)| (at, sets[*i].iter().filter(|k| !union.contains(k)).count()))
                .min_by_key(|(at, added)| (*added, *at))
                .expect("left is not empty");
            let i = left.remove(at);
            union.extend(sets[i].iter().copied());
            batch.push(i);
        }
        out.push(batch);
    }
    out.extend(empty.chunks(size).map(<[usize]>::to_vec));
    out
}

/// A reply segment that fits in the log: whitespace folded into one space, at most this many characters.
const SNIPPET_CHARS: usize = 240;

fn snippet(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(SNIPPET_CHARS).collect();
    if flat.chars().count() > SNIPPET_CHARS {
        out.push('…');
    }
    out
}

/// If a signature structurally matches more attributes than this number, only this many by relevance are shown to the model. In the first real run each
/// signature dragged along dozens of candidates on average (six coarse classes cut nothing), 18,000 tokens per request, and alignment took
/// eighty percent of a round's usage (bench README, 2026-09-24); if none of the ten is right, most likely the ontology simply has none
const SHORTLIST: usize = 10;
/// Number of alignment batches in flight at once; the model gate (the workspace's concurrency cap) limits it again below
const PARALLEL_BATCHES: usize = 4;
/// How many signatures' text to embed at once
const SHORTLIST_EMBED_BATCH: usize = 32;

/// Signature → candidate ids shown to the model (by relevance). Signatures not in the table still see all structural candidates
type Shortlist = HashMap<SignatureKey, Vec<Uuid>>;

/// Give a short list to signatures with many candidates, ranked by relevance: the signature's text (the phrase plus one example sentence) is embedded and compared with the attribute vectors
/// (the set built by `embed_ontology`); the nearest [`SHORTLIST`] are kept. Attributes whose label words appear in the phrase
/// are kept regardless of distance ("based in" against "based in"). Without an embedding model, with attributes that have no vectors yet, or with few candidates to begin with,
/// signatures are not in the table: the model then still sees all structural candidates.
///
/// A signature's vector is stored in `signature_vectors` by (library, embedding model, hash of the text); each round only embeds texts not seen before
/// (#1097): a signature's text barely changes between rounds, whereas previously all were re-embedded every round, when only dozens of new ones needed judging
async fn shortlist(
    state: &AppState,
    settings: &utopia_core::models::LlmSettings,
    kb_id: Uuid,
    sigs: &[PhraseSignature],
    full: &Considered<'_>,
) -> anyhow::Result<Shortlist> {
    let mut out = Shortlist::new();
    let (Some(client), Some(model)) = (
        llm_util::embed_client(settings),
        settings.embed_model.as_deref(),
    ) else {
        return Ok(out);
    };
    let wide: Vec<&PhraseSignature> = sigs
        .iter()
        .filter(|s| full.get(&s.key()).is_some_and(|(f, _)| f.len() > SHORTLIST))
        .collect();
    let pool = &state.pool;
    let texts: Vec<String> = wide.iter().map(|s| signature_text(s)).collect();
    let hashes: Vec<Vec<u8>> = texts
        .iter()
        .map(|t| Sha256::digest(t.as_bytes()).to_vec())
        .collect();
    // Remove text no longer present this round, and vectors embedded by other models. Signatures with many candidates are kept, not only those embedded below:
    // when one batch's embedding fails, what was stored last round is not lost with it
    utopia_store::signature_vectors::prune(pool, kb_id, model, &hashes).await?;
    if wide.is_empty() {
        return Ok(out);
    }
    // First ask whether this class of attributes has vectors; signatures without are not embedded, since the nearest neighbours would be empty anyway and everything is seen. A library that has never run
    // `embed_ontology` used to send seventeen thousand signatures to embedding every round, with not one shortlisted (#1097).
    // Ask one question per class, at most two sentences
    let mut has_vectors = HashMap::new();
    for kind in ["relation", "attribute"] {
        if wide.iter().any(|s| property_kind(s) == kind) {
            let has =
                utopia_store::ontology::has_relation_type_vectors(pool, kb_id, Some(kind)).await?;
            has_vectors.insert(kind, has);
        }
    }
    let usable: Vec<usize> = (0..wide.len())
        .filter(|&i| has_vectors.get(property_kind(wide[i])) == Some(&true))
        .collect();
    let wanted: Vec<Vec<u8>> = usable.iter().map(|&i| hashes[i].clone()).collect();
    let mut vectors = utopia_store::signature_vectors::get(pool, kb_id, model, &wanted).await?;
    let reused = vectors.len();
    // Only embed what has never been stored; two signatures with the same text (same example, same quote) are embedded once
    let mut queued = HashSet::new();
    let missing: Vec<usize> = usable
        .iter()
        .copied()
        .filter(|&i| !vectors.contains_key(&hashes[i]) && queued.insert(&hashes[i]))
        .collect();
    let mut embedded = 0;
    for batch in missing.chunks(SHORTLIST_EMBED_BATCH) {
        let batch_texts: Vec<String> = batch.iter().map(|&i| texts[i].clone()).collect();
        let got = {
            let _permit = llm_util::acquire_embed(state, settings).await;
            match client.embed(&batch_texts).await {
                Ok(v) if v.len() == batch.len() => v,
                Ok(v) => {
                    tracing::warn!(%kb_id, sent = batch.len(), got = v.len(), "签名向量数量对不上，这一批不开短名单");
                    continue;
                }
                // Running out of credit is not this batch's concern: skip it; every later batch and the next alignment hit the same 429, and the task
                // still finishes done, no alert fires, and another is queued straight away (#1360). Carry it out with its type, so `alerting::hopeless` can see it
                Err(e) if utopia_llm::out_of_credit(&e).is_some() => return Err(e),
                Err(e) => {
                    tracing::warn!(%kb_id, error = %e, "签名向量没算出来，这一批不开短名单");
                    continue;
                }
            }
        };
        let items: Vec<(Vec<u8>, Vec<f32>)> =
            batch.iter().map(|&i| hashes[i].clone()).zip(got).collect();
        utopia_store::signature_vectors::put(pool, kb_id, model, &items).await?;
        embedded += items.len();
        vectors.extend(items);
    }
    for &i in &usable {
        let s = wide[i];
        // This batch could not be embedded: do not open the shortlist, the model sees all
        let Some(vector) = vectors.get(&hashes[i]) else {
            continue;
        };
        let (fitting, _) = &full[&s.key()];
        // Literal-valued signatures bind only attributes; signatures whose object is an entity could be either (a relation, or a text attribute), so do not filter by class
        let near = utopia_store::ontology::nearest_relation_type_ids(
            pool,
            kb_id,
            vector,
            (fitting.len() * 2) as i64,
            s.object_is_value.then_some("attribute"),
        )
        .await?;
        if near.is_empty() {
            // No vectors for the attributes yet: do not open the shortlist, the model sees all
            continue;
        }
        let fitting_ids: Vec<Uuid> = fitting.iter().map(|p| p.id).collect();
        let must_keep: Vec<Uuid> = fitting
            .iter()
            .filter(|p| label_in_phrase(&p.label, &s.phrase))
            .map(|p| p.id)
            .collect();
        out.insert(
            s.key(),
            pick_shortlist(&near, &fitting_ids, &must_keep, SHORTLIST),
        );
    }
    tracing::info!(
        %kb_id,
        wide = wide.len(),
        skipped = wide.len() - usable.len(),
        reused,
        embedded,
        shortlisted = out.len(),
        "候选短名单开好"
    );
    Ok(out)
}

/// The signature text to embed: the phrase plus the first example sentence and its quote. The cache keys on its hash, so changing this makes old vectors mismatch,
/// and every text is re-embedded once next round
fn signature_text(s: &PhraseSignature) -> String {
    match (s.examples.first(), s.quotes.first()) {
        (Some(e), Some(q)) => format!("{} · {e} · {q}", s.phrase),
        (Some(e), None) => format!("{} · {e}", s.phrase),
        _ => s.phrase.clone(),
    }
}

/// Which class of attribute a signature can bind: a literal-valued object binds attributes, otherwise it binds relations
fn property_kind(s: &PhraseSignature) -> &'static str {
    if s.object_is_value {
        "attribute"
    } else {
        "relation"
    }
}

/// Category word → attribute ids shown to the rule-proposer (by relevance). The word's text plus some example names is embedded, and the nearest
/// [`SHORTLIST`] are taken; without an embedding model, or with attributes that have no vectors, it is not in the table
async fn shortlist_kind_words(
    state: &AppState,
    settings: &utopia_core::models::LlmSettings,
    kb_id: Uuid,
    words: &[&utopia_store::type_bindings::KindWordSignature],
) -> anyhow::Result<HashMap<String, Vec<Uuid>>> {
    let mut out = HashMap::new();
    let Some(client) = llm_util::embed_client(settings) else {
        return Ok(out);
    };
    // Not one attribute has a vector: embedding would return nothing either (same reason as `shortlist`)
    if words.is_empty()
        || !utopia_store::ontology::has_relation_type_vectors(&state.pool, kb_id, None).await?
    {
        return Ok(out);
    }
    for batch in words.chunks(SHORTLIST_EMBED_BATCH) {
        let texts: Vec<String> = batch
            .iter()
            .map(|k| format!("{} · {}", k.kind_word, k.examples.join(", ")))
            .collect();
        let vectors = {
            let _permit = llm_util::acquire_embed(state, settings).await;
            match client.embed(&texts).await {
                Ok(v) if v.len() == batch.len() => v,
                // Same as `shortlist`: running out of credit stops the whole round
                Err(e) if utopia_llm::out_of_credit(&e).is_some() => return Err(e),
                Ok(_) | Err(_) => {
                    tracing::warn!(%kb_id, "类别词向量没算出来，这一批看全部候选");
                    continue;
                }
            }
        };
        for (k, vector) in batch.iter().zip(vectors) {
            let near = utopia_store::ontology::nearest_relation_type_ids(
                &state.pool,
                kb_id,
                &vector,
                SHORTLIST as i64,
                None,
            )
            .await?;
            if !near.is_empty() {
                out.insert(k.kind_word.clone(), near);
            }
        }
    }
    Ok(out)
}

/// A decent word in the label (four letters or more) appears in the phrase
fn label_in_phrase(label: &str, phrase: &str) -> bool {
    let phrase = phrase.to_lowercase();
    label
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w.len() >= 4 && phrase.contains(w))
}

/// Shortlist: first those whose labels match (however far), then topped up to the cap by vector distance; all of them structurally match
fn pick_shortlist(near: &[Uuid], fitting: &[Uuid], must_keep: &[Uuid], limit: usize) -> Vec<Uuid> {
    let mut out: Vec<Uuid> = must_keep
        .iter()
        .copied()
        .filter(|id| fitting.contains(id))
        .collect();
    for id in near {
        if out.len() >= limit {
            break;
        }
        if fitting.contains(id) && !out.contains(id) {
            out.push(*id);
        }
    }
    out
}

/// Those not finished in a round (call failed, reply unreadable, the model skipped some) are queued again a few times; past this number, wait
/// for the next document or an ontology change before asking again. Without a cap, a reply that is unreadable every time at temperature zero would make
/// the task send the same prompt again every few dozen seconds, with no end (same as category-word alignment)
pub(crate) const MAX_REASK: u32 = 3;

/// Run over a library once: new and stale signatures are each judged once.
/// `reask` is how many times this task has already queued itself (documents, ontology and category-word alignment queue 0)
pub async fn align_phrases_reasking(
    state: &AppState,
    kb_id: Uuid,
    reask: u32,
) -> anyhow::Result<()> {
    let pool = &state.pool;
    // Human interpretations are deliberately preserved by alignment. Report that preservation
    // explicitly even when no model is configured; it is not a fresh semantic judgment.
    utopia_store::ontology_regressions::record_human(pool, kb_id).await?;
    let kb = utopia_store::kbs::get(pool, kb_id).await?;
    let settings = utopia_store::settings::get(pool, kb.workspace_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot align phrases"))?;
    // Alignment is a judgement call: let the model think at the endpoint's default strength, not the minimal the workspace sets for extraction
    let client = llm_util::chat_client(&settings)
        .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot align phrases"))?;
    // Category-word alignment is still queued or running: the classes at both ends are not settled yet, so the fingerprints of the signatures judged now will change at once; judging now would be re-judging.
    // Wait for it to finish and come back: at most one queued, queued half a minute later. Phrase alignment queued from other entry points (the ontology page, the review page, after each document
    // is extracted) also goes through here, so this one guard is enough.
    //
    // Waiting also applies while ontology vectors are still being filled: when the editor creates an attribute it queues `embed_ontology` first and then this task; an attribute without a vector
    // cannot enter the shortlist, so the shape's fingerprint does not change, and judging now would still leave it out of the candidates (this is how `bordered_by` slipped through on its first real run).
    // The fill task takes seconds to minutes, so waiting is fine.
    let waiting_on = if utopia_store::jobs::pending_for_kb(pool, "align_types", kb_id).await? {
        Some("类别词对齐")
    } else if utopia_store::jobs::pending_for_kb(pool, "embed_ontology", kb_id).await? {
        Some("本体向量补齐")
    } else {
        None
    };
    if let Some(what) = waiting_on {
        tracing::info!(%kb_id, "短语对齐：{what}还没收尾，半分钟后再看");
        let payload = if reask == 0 {
            serde_json::json!({ "kb_id": kb_id })
        } else {
            serde_json::json!({ "kb_id": kb_id, "reask": reask })
        };
        utopia_store::jobs::enqueue_unless_queued_after(
            pool,
            "align_phrases",
            payload,
            std::time::Duration::from_secs(30),
        )
        .await?;
        return Ok(());
    }
    // Only one run per library at a time, for the same reason as category-word alignment (running in parallel makes the endpoint return 502)
    let mut guard = pool.acquire().await?;
    let locked: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext('align_phrases'), hashtext($1))")
            .bind(kb_id.to_string())
            .fetch_one(&mut *guard)
            .await?;
    if !locked {
        // A run in progress checks by itself at the end whether anything new has arrived (see the end of align_phrases_locked); here
        // it does not queue: queuing back would make it and the running one kick each other into a deadlock loop
        tracing::info!(%kb_id, "短语对齐已有一份在跑，这次跳过");
        return Ok(());
    }
    let result = align_phrases_locked(state, kb_id, reask, &settings, &client).await;
    let _ = sqlx::query("SELECT pg_advisory_unlock(hashtext('align_phrases'), hashtext($1))")
        .bind(kb_id.to_string())
        .execute(&mut *guard)
        .await;
    result
}

/// Alignment wrap-up: if enough shapes went unbound, call the ontology agent to look (0061 decision 2). The agent skips those it has already raised,
/// so here we only count and do not filter; a one-minute debounce means several documents in a row call it only once. Both with and without attributes pass through here:
/// signatures in an empty ontology all judge "none", which is exactly when the agent should come
async fn queue_ontology_agent_if_unbound(pool: &sqlx::PgPool, kb_id: Uuid) -> anyhow::Result<()> {
    let unbound = phrase_bindings::bindings(pool, kb_id)
        .await?
        .iter()
        .filter(|b| matches!(b.status.as_str(), "none" | "undecided"))
        .count();
    if unbound >= crate::ontology_agent::TRIGGER_UNBOUND {
        utopia_store::jobs::enqueue_unless_pending(
            pool,
            "propose_ontology",
            serde_json::json!({ "kb_id": kb_id }),
            std::time::Duration::from_secs(60),
        )
        .await?;
    }
    Ok(())
}

async fn align_phrases_locked(
    state: &AppState,
    kb_id: Uuid,
    reask: u32,
    settings: &utopia_core::models::LlmSettings,
    client: &utopia_llm::LlmClient,
) -> anyhow::Result<()> {
    let pool = &state.pool;
    let props = utopia_store::ontology::relation_type_views(pool, kb_id).await?;
    let classes = utopia_store::graph::entity_types(pool, kb_id).await?;
    let class_key: HashMap<Uuid, &str> = classes.iter().map(|c| (c.id, c.key.as_str())).collect();
    let by_key: HashMap<&str, &RelationTypeView> =
        props.iter().map(|p| (p.key.as_str(), p)).collect();
    let closure = closures(classes.iter().map(|c| (c.id, c.parents.as_slice())));
    let versions = phrase_bindings::property_versions(pool, kb_id).await?;
    let sigs = phrase_bindings::signatures(pool, kb_id).await?;
    let existing: HashMap<_, _> = phrase_bindings::bindings(pool, kb_id)
        .await?
        .into_iter()
        .map(|b| (b.key(), b))
        .collect();
    // The candidates and fingerprint of each live signature at this moment. Candidates are matched through inheritance; if there are many, they are cut to a short list by relevance; the fingerprint covers all
    // inputs to the judgement (0053), and the short list is included: if the list changes, ask again
    let full = consider(&sigs, &props, &closure, &versions, None);
    let short = shortlist(state, settings, kb_id, &sigs, &full).await?;
    let considered = consider(&sigs, &props, &closure, &versions, Some(&short));
    // Stale = the stored fingerprint differs from this moment's (those judged before this column existed are each judged again).
    // No longer by timestamp: timestamps cannot see parent-edge additions and removals, or edits during a request (#795). A person's judgement, and ones the ontology still
    // stands by (`settled`), are not judged again. A signature bound to a state attribute that was never asked which end a moment marks is also asked (`awaits_marks`, #966)
    let todo: Vec<&PhraseSignature> = sigs
        .iter()
        .filter(|s| match existing.get(&s.key()) {
            None => true,
            Some(b) => {
                !settled(b, s, &props, &closure)
                    && (b.basis.as_deref() != Some(considered[&s.key()].1.as_str())
                        || awaits_marks(b, &props)
                        || invalid_agent_direction(b, s, &props, &closure))
            }
        })
        .collect();
    let attempted: HashSet<_> = todo.iter().map(|s| s.key()).collect();
    // Among these, those whose fingerprint is unchanged and that came in only to ask about marks: this ask can write marks only, the binding stays as it is (0053 revision 2026-09-27).
    // Previously the whole binding was judged again; with two votes not picking an attribute, a signature that was bound got judged none, and its typed rows went with it
    let marks_only: HashMap<_, &phrase_bindings::Binding> = todo
        .iter()
        .filter_map(|s| {
            let b = existing.get(&s.key())?;
            (b.basis.as_deref() == Some(considered[&s.key()].1.as_str())
                && awaits_marks(b, &props)
                && !invalid_agent_direction(b, s, &props, &closure))
            .then_some((s.key(), b))
        })
        .collect();
    tracing::info!(%kb_id, signatures = sigs.len(), to_decide = todo.len(), properties = props.len(), "短语对齐开始");

    // No attribute to bind to: each is "none"; once an attribute appears the fingerprint changes, and they will be handed back
    if props.is_empty() {
        for s in &todo {
            phrase_bindings::decide(
                pool,
                kb_id,
                s,
                Decision {
                    relation_type_id: None,
                    direction: None,
                    status: "none",
                    votes: &serde_json::json!({ "reason": "no_properties" }),
                    decided_by: "agent",
                    basis: Some(&considered[&s.key()].1),
                    marks: None,
                    marks_asked: false,
                },
            )
            .await?;
        }
        // An empty ontology is exactly when the ontology agent is needed: still count the unbound signatures, and queue it once there are enough, so the ontology can grow
        queue_ontology_agent_if_unbound(pool, kb_id).await?;
        return Ok(());
    }

    let keys_of = |ids: &[Uuid]| -> Vec<&str> {
        ids.iter()
            .filter_map(|id| class_key.get(id).copied())
            .collect()
    };
    let (mut bound, mut none, mut undecided, mut skipped) = (0usize, 0usize, 0usize, 0usize);
    // Batches that failed to call or parse: skipped this round, and queued once more at the end
    let mut failed = 0usize;
    // Signatures asked of the model that it answered without covering: with one of two votes missing, no conclusion is drawn
    let mut unanswered = 0usize;
    // marks: where a re-ask wrote a value; where it was asked but two votes did not give the same value, the row goes to the alignment queue
    let (mut marked, mut unsettled) = (0usize, 0usize);
    // Batches run in parallel ([`PARALLEL_BATCHES`] in flight; the model gate limits it again): running two votes per batch in series makes the model think twice
    // each time, so running 22 batches one after another takes half an hour, and one stuck call among them can hold the whole round for 18 minutes (bench README,
    // 2026-09-24). Each batch records its own counts, which are added up on return
    {
        use futures_util::StreamExt;
        // Only move the references into each batch's future
        let (considered, full, closure, class_key, by_key, marks_only) = (
            &considered,
            &full,
            &closure,
            &class_key,
            &by_key,
            &marks_only,
        );
        // Build each batch's future first and then queue them: returning an async block directly from map leaves the borrowed lifetimes unable
        // to satisfy the Send that tokio::spawn requires
        // Signatures with similar candidates are put into the same batch (see `batch_by_candidates`): the attribute table is written once per batch, and the more
        // the signatures in a batch overlap in candidates, the shorter this table is
        let sets: Vec<Vec<Uuid>> = todo
            .iter()
            .map(|s| {
                let fitting = &considered[&s.key()].0;
                if fitting.len() > CANDIDATE_LIMIT {
                    Vec::new()
                } else {
                    fitting.iter().map(|p| p.id).collect()
                }
            })
            .collect();
        let batches: Vec<Vec<&PhraseSignature>> = batch_by_candidates(&sets, BATCH)
            .into_iter()
            .map(|group| group.into_iter().map(|i| todo[i]).collect())
            .collect();
        let futures: Vec<_> = batches
            .iter()
            .map(|batch| batch.as_slice())
            .map(|batch| async move {
                let (mut bound, mut none, mut undecided, mut skipped, mut failed, mut unanswered) =
                    (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
                let (mut marked, mut unsettled) = (0usize, 0usize);
        // Signatures with candidates over the cap are not asked of the model: recorded as undecided for a person, with the fingerprint recorded too, since as attributes shrink the fingerprint changes,
        // and they are asked then. Previously exceeding the cap was skipped silently, like having no candidates, so a signature stayed queued forever and could never execute (#807)
        let cands: Vec<Vec<&RelationTypeView>> = batch
            .iter()
            .map(|s| {
                let fitting = &considered[&s.key()].0;
                if fitting.len() > CANDIDATE_LIMIT {
                    Vec::new()
                } else {
                    fitting.clone()
                }
            })
            .collect();
        let mut votes: Vec<(Vote, Vote)> = vec![(None, None); batch.len()];
        let mut answered = vec![(false, false); batch.len()];
        // Number of rounds whose reply could be read: for a re-ask of marks, it counts as asked only when both rounds' replies have come back
        let mut replied = 0usize;
        for pass in 0..2 {
            let items: Vec<PhraseItem<'_>> = batch
                .iter()
                .enumerate()
                .filter(|(i, _)| !cands[*i].is_empty())
                .filter(|(i, s)| pass == 0 || second_vote_is_due(&votes[*i].0, answered[*i].0, marks_only.contains_key(&s.key())))
                .map(|(i, s)| {
                    let mut list: Vec<&RelationTypeView> = cands[i].clone();
                    if pass == 1 {
                        list.reverse();
                    }
                    PhraseItem {
                        id: i as i64,
                        phrase: &s.phrase,
                        subject_class: s.subject_type_key.as_deref(),
                        object_class: s.object_type_key.as_deref(),
                        object_is_value: s.object_is_value,
                        statement_count: s.count,
                        examples: &s.examples,
                        quotes: &s.quotes,
                        candidates: list
                            .iter()
                            .map(|p| PropertyCandidate {
                                key: &p.key,
                                label: &p.label,
                                description: &p.description,
                                kind: &p.kind,
                                temporal: &p.temporal,
                                domains: keys_of(&p.domains),
                                ranges: keys_of(&p.ranges),
                                via: fits(p, s, closure)
                                    .map(|f| {
                                        f.via
                                            .iter()
                                            .map(|(declared, class)| {
                                                format!(
                                                    "{} is a subclass of {}",
                                                    class_key.get(class).copied().unwrap_or("?"),
                                                    class_key.get(declared).copied().unwrap_or("?"),
                                                )
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            })
                            .collect(),
                        // A key that matches structurally but is not in the short list: if the model picks it from the batch's attribute table, it counts as a vote
                        also_allowed: full[&s.key()]
                            .0
                            .iter()
                            .filter(|p| !cands[i].iter().any(|c| c.id == p.id))
                            .map(|p| p.key.as_str())
                            .collect(),
                    }
                })
                .collect();
            if items.is_empty() {
                continue;
            }
            let messages = build_phrase_messages(&items);
            let reply =
                match chat_retrying_rate_limits_at(state, settings, client, &messages, Some(0.0))
                    .await
                {
                    Ok(r) => r,
                    // Out of credit: this batch's future returns an error; the `r?` below fails the whole round once and sets off the alert,
                    // and it is not recorded as failed, since recording it would only queue another copy that hits the same 429 (#1360)
                    Err(e) if utopia_llm::out_of_credit(&e).is_some() => return Err(e),
                    Err(e) => {
                        tracing::warn!(%kb_id, error = %e, "短语对齐调用失败，这一批留到下次");
                        failed += 1;
                        continue;
                    }
                };
            let (choices, malformed) = match parse_phrase_response(&reply.text, &items) {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(%kb_id, error = %e, "短语对齐回复解析失败，这一批留到下次");
                    failed += 1;
                    continue;
                }
            };
            skipped += malformed;
            if choices.is_empty() {
                // Decoded but not a single one read: the reply's shape is not one we recognise. This is the same as a parse failure,
                // counted as a failure and left for next time. Previously this said nothing, each signature was quietly skipped as "one vote missing",
                // and the log held only a string of "finished bound=0": the start of the reply has to go into the log, so next time we
                // know what it looks like (same as category-word alignment)
                tracing::warn!(
                    %kb_id,
                    pass,
                    items = items.len(),
                    item_ids = ?items.iter().map(|item| item.id).collect::<Vec<_>>(),
                    malformed,
                    finish_reason = ?reply.finish_reason,
                    repeating = reply.repeating,
                    chars = reply.text.chars().count(),
                    reply = %snippet(&reply.text),
                    "短语对齐回复读不出一条，这一批留到下次"
                );
                failed += 1;
                continue;
            }
            replied += 1;
            if malformed > 0 {
                // What a bad vote looks like has to be visible: on the first real run half the signatures were judged bad votes, and it took a day to learn the model was answering with the label
                tracing::info!(%kb_id, malformed, reply = %snippet(&reply.text), "短语对齐的回复里有坏票");
            }
            for c in choices {
                let Ok(i) = usize::try_from(c.id) else {
                    continue;
                };
                if let Some(slot) = votes.get_mut(i) {
                    let vote = c.property.map(|(key, direction)| (key, direction, c.marks));
                    if pass == 0 {
                        slot.0 = vote;
                        answered[i].0 = true;
                    } else {
                        slot.1 = vote;
                        answered[i].1 = true;
                    }
                }
            }
        }
        for (i, s) in batch.iter().enumerate() {
            let basis = considered[&s.key()].1.as_str();
            if let Some(asked) = marks_only.get(&s.key()) {
                // Re-ask marks (0053 revision 2026-09-27): both votes name this binding's attribute and direction and give the same
                // value, which is written down; any other answer (no attribute picked, a different one picked, nothing said, or said differently) leaves the binding as it is.
                // Both record that it was asked: it is not asked again, and a column with no value goes to the alignment queue to wait for a person. A reply that did not come back does not count as asked,
                // so it is asked again next time (same as signatures that got no answer); one that cannot be asked (candidates over the cap) goes straight to a person
                if !cands[i].is_empty() && replied < 2 {
                    unanswered += 1;
                    continue;
                }
                // This vote names this binding's attribute and gave a value: if so, the value it gave is the value
                let value_of = |v: &Vote| match v {
                    Some((k, d, m)) => m.filter(|_| {
                        by_key.get(k.as_str()).map(|p| p.id) == asked.relation_type_id
                            && asked.direction.as_deref() == Some(d.as_str())
                    }),
                    None => None,
                };
                let (a, b) = &votes[i];
                let value = value_of(a).filter(|m| value_of(b) == Some(*m));
                if phrase_bindings::record_marks(pool, kb_id, asked, value.map(Marks::as_str))
                    .await?
                {
                    if value.is_some() {
                        marked += 1;
                    } else {
                        unsettled += 1;
                    }
                }
                continue;
            }
            if cands[i].is_empty() {
                let fitting = considered[&s.key()].0.len();
                // The two kinds of "did not ask the model" are each written to the database, so the projection can retire them and the queue can settle:
                // no attribute matches → none (a bound signature loses its support, and typed rows are voided along with materialisation);
                // too many match → undecided, handed to a person, not re-sorted every round
                let (status, votes) = if fitting == 0 {
                    ("none", serde_json::json!({ "reason": "no_candidates" }))
                } else {
                    (
                        "undecided",
                        serde_json::json!({ "first": null, "second": null,
                                            "reason": "too_many_candidates", "candidates": fitting }),
                    )
                };
                if phrase_bindings::decide(
                    pool,
                    kb_id,
                    s,
                    Decision {
                        relation_type_id: None,
                        direction: None,
                        status,
                        votes: &votes,
                        decided_by: "agent",
                        basis: Some(basis),
                        marks: None,
                        marks_asked: false,
                    },
                )
                .await?
                {
                    if status == "none" {
                        none += 1;
                    } else {
                        undecided += 1;
                    }
                }
                skipped += 1;
                continue;
            }
            let (a, b) = &votes[i];
            let (ans_a, ans_b) = answered[i];
            // A signature the first vote said matched nothing does not get a second vote (see `second_vote_is_due`): the conclusion is none
            let asked_twice = second_vote_is_due(a, ans_a, false);
            if !ans_a || (asked_twice && !ans_b) {
                // One vote got no answer: no conclusion, ask again next time
                unanswered += 1;
                continue;
            }
            let show = |v: &Vote| {
                v.as_ref()
                    .map(|(k, d, m)| {
                        serde_json::json!({ "property": k, "direction": d.as_str(),
                                            "marks": m.map(Marks::as_str) })
                    })
                    .unwrap_or(serde_json::Value::Null)
            };
            let mut record = if asked_twice {
                serde_json::json!({ "first": show(a), "second": show(b) })
            } else {
                serde_json::json!({ "first": null, "reason": "first_vote_none" })
            };
            let valid = |v: &Vote| v.as_ref().is_none_or(|(k, d, _)| {
                by_key.get(k.as_str()).is_some_and(|p| fits_direction(p, s, closure, *d).is_some())
            });
            let invalid_direction = !valid(a) || !valid(b);
            if invalid_direction {
                // Keep both votes for review; an invalid direction is neither a
                // semantic none nor permission to silently flip the model's answer.
                record["reason"] = serde_json::json!("invalid_direction");
            }
            // Whether the attribute and direction the two votes picked (or both said none) agree
            let agree = match (a, b) {
                (Some((ka, da, _)), Some((kb, db, _))) => ka == kb && da == db,
                (None, None) => true,
                _ => false,
            };
            if !agree || invalid_direction {
                phrase_bindings::decide(
                    pool,
                    kb_id,
                    s,
                    Decision {
                        relation_type_id: None,
                        direction: None,
                        status: "undecided",
                        votes: &record,
                        decided_by: "agent",
                        basis: Some(basis),
                        marks: None,
                        marks_asked: false,
                    },
                )
                .await?;
                undecided += 1;
                continue;
            }
            // Where the attribute is a state, the two votes also say which end a moment marks (#966): only when they say the same value is it written down. If nothing was said, or
            // it differs, the binding stays, this cell is left empty, and it is recorded as asked: it is not asked again and goes to the alignment queue for a person (0053 revision
            // 2026-09-27). Previously not saying anything counted as not answered, so when the model only ever answered three cells, it was asked again every round and never bound.
            // Events and constants do not ask this cell
            let state = a
                .as_ref()
                .and_then(|(k, _, _)| by_key.get(k.as_str()))
                .is_some_and(|p| p.temporal == "state");
            let marks = match (a, b) {
                (Some((_, _, ma)), Some((_, _, mb))) if state && ma == mb => *ma,
                _ => None,
            };
            match a
                .as_ref()
                .and_then(|(k, d, _)| by_key.get(k.as_str()).map(|p| (p, *d)))
            {
                Some((p, d)) => {
                    if phrase_bindings::decide(
                        pool,
                        kb_id,
                        s,
                        Decision {
                            relation_type_id: Some(p.id),
                            direction: Some(d.as_str()),
                            status: "bound",
                            votes: &record,
                            decided_by: "agent",
                            basis: Some(basis),
                            marks: marks.map(Marks::as_str),
                            marks_asked: state,
                        },
                    )
                    .await?
                    {
                        bound += 1;
                        if state && marks.is_none() {
                            unsettled += 1;
                        }
                    }
                }
                None => {
                    if phrase_bindings::decide(
                        pool,
                        kb_id,
                        s,
                        Decision {
                            relation_type_id: None,
                            direction: None,
                            status: "none",
                            votes: &record,
                            decided_by: "agent",
                            basis: Some(basis),
                            marks: None,
                            marks_asked: false,
                        },
                    )
                    .await?
                    {
                        none += 1;
                    }
                }
            }
        }

                Ok::<_, anyhow::Error>((
                    (bound, none, undecided, skipped, failed, unanswered),
                    (marked, unsettled),
                ))
            })
            .collect();
        let mut results = futures_util::stream::iter(futures).buffer_unordered(PARALLEL_BATCHES);
        while let Some(r) = results.next().await {
            let ((b, n, u, sk, f, un), (m, us)) = r?;
            bound += b;
            none += n;
            undecided += u;
            skipped += sk;
            failed += f;
            unanswered += un;
            marked += m;
            unsettled += us;
        }
    }
    // Old invalid agent bindings have matching fingerprints, so explicitly retry
    // them above. If the model fails, structural invalidity still cannot support
    // their typed rows: keep the old votes in an undecided result for a person.
    // Read again so a person who decided during the request remains protected.
    let repair: Vec<_> = todo
        .iter()
        .copied()
        .filter(|s| {
            existing
                .get(&s.key())
                .is_some_and(|b| invalid_agent_direction(b, s, &props, &closure))
        })
        .collect();
    if !repair.is_empty() {
        let now: HashMap<_, _> = phrase_bindings::bindings(pool, kb_id)
            .await?
            .into_iter()
            .map(|b| (b.key(), b))
            .collect();
        for s in repair {
            let Some(b) = now.get(&s.key()) else {
                continue;
            };
            if !invalid_agent_direction(b, s, &props, &closure) {
                continue;
            }
            let previous: Option<serde_json::Value> = sqlx::query_scalar(
                "SELECT votes FROM phrase_bindings WHERE kb_id=$1 AND phrase=$2
             AND subject_type_id IS NOT DISTINCT FROM $3
             AND object_type_id IS NOT DISTINCT FROM $4 AND object_is_value=$5",
            )
            .bind(kb_id)
            .bind(&s.phrase)
            .bind(s.subject_type_id)
            .bind(s.object_type_id)
            .bind(s.object_is_value)
            .fetch_optional(pool)
            .await?;
            let Some(previous) = previous else {
                continue;
            };
            if phrase_bindings::decide(
                pool,
                kb_id,
                s,
                Decision {
                    relation_type_id: None,
                    direction: None,
                    status: "undecided",
                    votes: &serde_json::json!({"reason":"invalid_direction", "previous":previous,
                                      "previous_decided_at":b.decided_at}),
                    decided_by: "agent",
                    basis: Some(&considered[&s.key()].1),
                    marks: None,
                    marks_asked: false,
                },
            )
            .await?
            {
                undecided += 1;
            }
        }
    }
    tracing::info!(%kb_id, bound, none, undecided, skipped, failed, unanswered, marked, unsettled, "短语对齐完成");
    if unanswered > 0 {
        tracing::warn!(%kb_id, unanswered, "短语对齐有签名模型没答到，这些签名这轮没有结论");
    }
    // Rule proposal (0044 decision 3, fifth slice): for the signatures judged this round, and things with category words, ask the model "what else does this shape
    // entail?". Only those judged this round are asked: shapes whose fingerprint is unchanged were already asked last round, and the answer (a proposal or the agent's rejection)
    // is still in implication_rules; if the fingerprint changes it is in the todo, and it is asked again naturally
    {
        let decided_now: HashMap<_, _> = phrase_bindings::bindings(pool, kb_id)
            .await?
            .into_iter()
            .map(|b| (b.key(), b))
            .collect();
        let kind_words = utopia_store::type_bindings::signatures(pool, kb_id).await?;
        let existing_rules = utopia_store::implication_rules::list(pool, kb_id, None).await?;
        let asked_kind: HashSet<&str> = existing_rules
            .iter()
            .filter(|r| r.trigger == "kind_word")
            .map(|r| r.phrase.as_str())
            .collect();
        let mut asks: Vec<crate::implication::RuleAsk<'_>> = Vec::new();
        for s in &todo {
            // Only re-asked for marks and not re-judged: the fingerprint is unchanged, and its rules were asked last round
            if marks_only.contains_key(&s.key()) {
                continue;
            }
            let Some(b) = decided_now.get(&s.key()) else {
                continue;
            };
            // Only signatures that did bind are asked: shapes judged none produced, over a round of 1121 of them, under 1% of the rules proposed, yet took more than half
            // of the rule-proposing calls (bench README, 2026-09-24); the undecided ones wait for a person to settle them first
            if b.status != "bound" {
                continue;
            }
            let (fitting, basis) = &considered[&s.key()];
            let bound_to = b
                .relation_type_id
                .and_then(|id| props.iter().find(|p| p.id == id))
                .map(|p| p.key.as_str());
            asks.push(crate::implication::RuleAsk {
                phrase: Some(s),
                kind_word: None,
                bound_to,
                candidates: fitting
                    .iter()
                    .copied()
                    .filter(|p| Some(p.key.as_str()) != bound_to)
                    .collect(),
                basis,
            });
        }
        // Category words: each word is asked once; candidates are the relation attributes whose subject can fall within the class it is bound to (or that declare nothing)
        let kind_basis: Vec<String> = kind_words
            .iter()
            .map(|k| {
                phrase_bindings::basis_of(
                    &[],
                    &[],
                    false,
                    &versions
                        .iter()
                        .map(|(id, at)| (*id, *at))
                        .collect::<Vec<_>>(),
                ) + ":"
                    + &k.kind_word
            })
            .collect();
        // Category-word candidates also get a short list: after the word is embedded with some example names, take the nearest attributes; with no vectors, look at all
        let fresh: Vec<&utopia_store::type_bindings::KindWordSignature> = kind_words
            .iter()
            .filter(|k| !asked_kind.contains(k.kind_word.as_str()) && has_modifier(&k.kind_word))
            .collect();
        let kind_short = shortlist_kind_words(state, settings, kb_id, &fresh).await?;
        for (k, basis) in kind_words.iter().zip(kind_basis.iter()) {
            if asked_kind.contains(k.kind_word.as_str()) || !has_modifier(&k.kind_word) {
                continue;
            }
            let mut candidates: Vec<&RelationTypeView> = props
                .iter()
                .filter(|p| p.kind == "relation" || p.kind == "attribute")
                .collect();
            if let Some(keep) = kind_short.get(&k.kind_word) {
                candidates.retain(|p| keep.contains(&p.id));
                candidates.sort_by_key(|p| keep.iter().position(|id| *id == p.id));
            }
            asks.push(crate::implication::RuleAsk {
                phrase: None,
                kind_word: Some(k),
                bound_to: None,
                candidates,
                basis,
            });
        }
        if !asks.is_empty() {
            match crate::implication::propose_rules(
                state, kb_id, settings, client, &asks, &class_key, &by_key,
            )
            .await
            {
                Ok((proposed, rule_failed)) => {
                    tracing::info!(%kb_id, asked = asks.len(), proposed, failed = rule_failed, "提规则完成");
                    if proposed > 0 {
                        state.emit_review(kb_id);
                    }
                }
                Err(e) => tracing::warn!(%kb_id, error = %e, "提规则失败，下一轮再提"),
            }
        }
    }
    // Once the binding is settled, the view follows: signatures' statements that are bound become typed rows, and rows whose binding changed are voided (0067)
    let typed = utopia_store::materialize::materialize(pool, kb_id).await?;
    tracing::info!(%kb_id, added = typed.added, merged = typed.merged, retired = typed.retired, "类型化事实按绑定算完");
    if typed.added > 0 || typed.merged > 0 || typed.retired > 0 {
        state.emit_graph(kb_id);
    }
    // The world did not stop while this round ran: new documents bring new signatures, and changed attributes or moved parent edges make the bindings just judged
    // stale; triggers that did not get queued this round also land here. Signatures never tried, and bindings whose fingerprint changed after this round's judgement
    // (edits made mid-request, #795), are queued again (computed from scratch, since new signatures change the prompt).
    // Only **live** signatures are considered: when the endpoint's class changes, the rows of old signatures have no statements left to judge; they would be "stale" forever yet
    // never executable. Previously `stale` handed these orphans back every round, one job queued per orphan, three rounds three times (#807)
    let changed = {
        // **Reload**, rather than comparing with the snapshot from the start of the run: the snapshot is the fingerprint the judgement wrote, and comparing with it would always
        // be equal. If the definition was edited while the model was answering (#795), a parent edge was added, or a new document came in, only reading again sees it
        let props = utopia_store::ontology::relation_type_views(pool, kb_id).await?;
        let classes = utopia_store::graph::entity_types(pool, kb_id).await?;
        let closure = closures(classes.iter().map(|c| (c.id, c.parents.as_slice())));
        let versions = phrase_bindings::property_versions(pool, kb_id).await?;
        let sigs = phrase_bindings::signatures(pool, kb_id).await?;
        // The short list keeps the one computed at the start: if the vectors have not changed, the list has not; signatures that changed will be asked naturally later
        let now_considered = consider(&sigs, &props, &closure, &versions, Some(&short));
        let now: HashMap<_, _> = phrase_bindings::bindings(pool, kb_id)
            .await?
            .into_iter()
            .map(|b| (b.key(), b))
            .collect();
        sigs.iter().any(|s| match now.get(&s.key()) {
            None => !attempted.contains(&s.key()),
            Some(b) => {
                !settled(b, s, &props, &closure)
                    && b.basis.as_deref() != Some(now_considered[&s.key()].1.as_str())
            }
        })
    };
    // Those this round did not finish (call failed, reply unreadable, the model skipped some) queue themselves again, at most MAX_REASK times,
    // waiting a little longer each time. Previously only failed batches queued again; a reply that could not be read counted as "zero items, zero bad" and not as failure,
    // and a signature the model skipped could only wait for the next document to queue it. After the last document there is no next, so they never got a conclusion;
    // and a skip writes no row, so the review queue cannot see it either (same as category-word alignment)
    let unfinished = failed > 0 || unanswered > 0;
    if !changed && !unfinished {
        queue_ontology_agent_if_unbound(pool, kb_id).await?;
    }
    if changed {
        utopia_store::jobs::enqueue_unless_queued(
            pool,
            "align_phrases",
            serde_json::json!({ "kb_id": kb_id }),
        )
        .await?;
    } else if unfinished && reask < MAX_REASK {
        let delay = std::time::Duration::from_secs(20 * u64::from(reask + 1));
        tracing::info!(%kb_id, failed, unanswered, reask = reask + 1, delay_secs = delay.as_secs(), "短语对齐没判完，稍后再问");
        utopia_store::jobs::enqueue_unless_pending(
            pool,
            "align_phrases",
            serde_json::json!({ "kb_id": kb_id, "reask": reask + 1 }),
            delay,
        )
        .await?;
    } else if unfinished {
        tracing::warn!(%kb_id, failed, unanswered, reask, "短语对齐问了几轮仍没判完，等下一篇文档或本体改动再问");
    }
    Ok(())
}

#[cfg(test)]
#[path = "phrase_alignment_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
#[path = "phrase_alignment_declared_tests.rs"]
mod declared_tests;

#[cfg(test)]
mod tests {
    #[test]
    fn a_kind_word_that_is_only_a_head_word_has_nothing_to_read() {
        for bare in ["state", "ship", " settlement "] {
            assert!(!has_modifier(bare), "{bare}");
        }
        for readable in [
            "british film",
            "1968",
            "british-governed",
            "英国电影",
            "wine area",
        ] {
            assert!(has_modifier(readable), "{readable}");
        }
    }

    #[test]
    fn signatures_with_the_same_candidates_share_a_batch() {
        // Six signatures, three candidate sets, interleaved; two per batch
        let sets: Vec<Vec<u32>> = vec![
            vec![1, 2],
            vec![7, 8],
            vec![],
            vec![2, 1],
            vec![8, 9],
            vec![1, 2, 3],
        ];
        let batches = batch_by_candidates(&sets, 2);
        // Each signature appears exactly once
        let mut all: Vec<usize> = batches.iter().flatten().copied().collect();
        all.sort_unstable();
        assert_eq!(all, vec![0, 1, 2, 3, 4, 5]);
        // Those with the same candidates are placed together; those with no candidates go last
        assert_eq!(batches, vec![vec![0, 3], vec![1, 4], vec![5], vec![2]]);
        // The same input gives the same batches
        assert_eq!(batch_by_candidates(&sets, 2), batches);
        // The union of candidates in a batch is smaller than when cut in arrival order
        let union = |b: &Vec<usize>| {
            b.iter()
                .flat_map(|i| sets[*i].iter())
                .collect::<HashSet<_>>()
                .len()
        };
        let in_order: usize = [vec![0, 1], vec![2, 3], vec![4, 5]].iter().map(union).sum();
        let grouped: usize = batches.iter().map(union).sum();
        assert!(grouped < in_order, "{grouped} < {in_order}");
    }

    #[test]
    fn a_shortlist_keeps_label_matches_and_fills_by_distance_within_the_fitting_set() {
        let ids: Vec<Uuid> = (0..6).map(|_| Uuid::now_v7()).collect();
        // near by distance: ids[3] is nearest but not among the structural candidates; ids[5] matches the label, and is kept even though it sits last
        let near = vec![ids[3], ids[0], ids[1], ids[2], ids[4], ids[5]];
        let fitting = vec![ids[0], ids[1], ids[2], ids[4], ids[5]];
        let picked = pick_shortlist(&near, &fitting, &[ids[5]], 3);
        assert_eq!(picked, vec![ids[5], ids[0], ids[1]]);
        assert!(label_in_phrase(
            "headquarters location",
            "has its headquarters in"
        ));
        assert!(!label_in_phrase("country", "is based in"));
        assert!(
            !label_in_phrase("in", "is based in"),
            "short words do not count"
        );
    }

    use super::*;

    fn view(kind: &str, domains: Vec<Uuid>, ranges: Vec<Uuid>) -> RelationTypeView {
        RelationTypeView {
            id: Uuid::now_v7(),
            key: "p".into(),
            label: "p".into(),
            temporal: "state".into(),
            functional: false,
            inverse_functional: false,
            is_transitive: false,
            is_symmetric: false,
            is_asymmetric: false,
            is_irreflexive: false,
            inverse_of: None,
            sub_property_of: None,
            builtin: false,
            description: String::new(),
            kind: kind.into(),
            domains,
            ranges,
            datatype: None,
            unit: None,
            qualifiers: Vec::new(),
            display_hints: serde_json::json!({}),
            usage: 0,
        }
    }

    fn sig(subject: Option<Uuid>, object: Option<Uuid>, value: bool) -> PhraseSignature {
        PhraseSignature {
            phrase: "x".into(),
            subject_type_id: subject,
            subject_type_key: None,
            object_type_id: object,
            object_type_key: None,
            object_is_value: value,
            count: 1,
            examples: Vec::new(),
            quotes: Vec::new(),
            example_statement_ids: Vec::new(),
        }
    }

    /// Filter candidates by domain/range: a declared one must fall within it (both directions count); undeclared is unrestricted; an empty class at one end is unrestricted
    #[test]
    fn a_property_fits_a_signature_by_its_declared_ends_in_either_direction() {
        let (org, place, person) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        // With no parent edges the closure is empty: each class equals only itself, behaving the same as before
        let ok = |p: &RelationTypeView, s: &PhraseSignature| fits(p, s, &Closure::new()).is_some();
        let hq = view("relation", vec![org], vec![place]);
        assert!(ok(&hq, &sig(Some(org), Some(place), false)));
        assert!(ok(&hq, &sig(Some(place), Some(org), false)), "反向也算");
        assert!(!ok(&hq, &sig(Some(person), Some(place), false)));
        assert!(
            !ok(&hq, &sig(None, Some(place), false)),
            "没绑到类的一端不算落在声明的域里"
        );
        let any_to_place = view("relation", vec![], vec![place]);
        assert!(
            ok(&any_to_place, &sig(None, Some(place), false)),
            "没声明的一端接受没绑到类的"
        );
        assert!(!ok(&hq, &sig(Some(org), None, true)), "关系不接字面值");
        let open = view("relation", vec![], vec![]);
        assert!(
            ok(&open, &sig(Some(person), Some(person), false)),
            "没声明就不限"
        );
        let revenue = view("attribute", vec![org], vec![]);
        assert!(ok(&revenue, &sig(Some(org), None, true)));
        assert!(!ok(&revenue, &sig(Some(person), None, true)));
        assert!(
            !ok(&revenue, &sig(Some(org), Some(place), false)),
            "属性只接字面值"
        );
    }
    /// An attribute declared on an ancestor matches a subclass's signatures through inheritance (#807 first item); the basis must be explainable to the model
    #[test]
    fn a_property_declared_on_an_ancestor_fits_a_subclass_by_inheritance() {
        let (legal_entity, org, place) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let mut closure = Closure::new();
        closure.insert(org, {
            let mut v = vec![org, legal_entity];
            v.sort();
            v
        });
        closure.insert(legal_entity, vec![legal_entity]);
        closure.insert(place, vec![place]);
        let hq = view("relation", vec![legal_entity], vec![place]);
        let fit =
            fits(&hq, &sig(Some(org), Some(place), false), &closure).expect("fits via parent");
        assert_eq!(
            fit.via,
            vec![(legal_entity, org)],
            "the basis names the declared ancestor and the class"
        );
        let direct =
            fits(&hq, &sig(Some(legal_entity), Some(place), false), &closure).expect("direct");
        assert!(direct.via.is_empty(), "a direct hit needs no explanation");
        assert!(
            fits(&hq, &sig(Some(place), Some(org), false), &closure).is_some(),
            "reverse direction walks the hierarchy too"
        );
        assert!(
            fits(&hq, &sig(Some(org), Some(place), false), &Closure::new()).is_none(),
            "without the parent edge the property is not a candidate"
        );
    }

    #[test]
    fn a_selected_direction_respects_inheritance_unknown_types_and_literal_shape() {
        let (parent, child, place) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let closure = closures([(parent, &[][..]), (child, &[parent][..]), (place, &[][..])]);
        let relation = view("relation", vec![parent], vec![place]);
        let attribute = view("attribute", vec![parent], vec![]);
        let open = view("relation", vec![], vec![]);
        let any_to_place = view("relation", vec![], vec![place]);
        for (p, s, allowed) in [
            (
                &relation,
                sig(Some(child), Some(place), false),
                [true, false],
            ),
            (
                &relation,
                sig(Some(place), Some(child), false),
                [false, true],
            ),
            (&relation, sig(None, Some(place), false), [false, false]),
            (&any_to_place, sig(None, Some(place), false), [true, false]),
            (&any_to_place, sig(Some(place), None, false), [false, true]),
            (&open, sig(None, None, false), [true, true]),
            (&attribute, sig(Some(child), None, true), [true, false]),
            (
                &attribute,
                sig(Some(child), Some(place), false),
                [false, false],
            ),
            (&relation, sig(Some(child), None, true), [false, false]),
        ] {
            for (direction, expected) in [Direction::Forward, Direction::Reverse]
                .into_iter()
                .zip(allowed)
            {
                assert_eq!(
                    fits_direction(p, &s, &closure, direction).is_some(),
                    expected
                );
            }
        }
    }

    /// Closure: multiple inheritance and diamonds, each ancestor appears once, and it includes itself
    #[test]
    fn closures_walk_the_hierarchy_once_per_ancestor() {
        let (thing, agent, legal, org) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        // org ⊂ agent ⊂ thing and org ⊂ legal ⊂ thing: a diamond
        let (a, l, o) = ([thing], [thing], [agent, legal]);
        let c = closures([
            (thing, &[][..]),
            (agent, &a[..]),
            (legal, &l[..]),
            (org, &o[..]),
        ]);
        let mut expect = vec![org, agent, legal, thing];
        expect.sort();
        assert_eq!(c[&org], expect);
        assert_eq!(c[&thing], vec![thing]);
    }
}
