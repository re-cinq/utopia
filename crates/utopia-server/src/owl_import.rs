//! OWL import: faithful original text -> projection -> preview -> persist.
//!
//! **Preview and persist go through the same plan.** Two independent code paths sooner or later diverge, and the consequence of divergence is that
//! what happens after the user clicks confirm is not what he just reviewed -- which is worse than having no preview at all.
//!
//! Matching is by **IRI**, not by key: when upstream changes `rdfs:label` once, the derived key changes,
//! and matching by key would create the same class as a new one and leave all its entities orphaned (see 0001 P2).

use std::collections::{BTreeMap, HashMap, HashSet};

use utopia_core::AppResult;
use utopia_ingest::ontology_rdf::{self, OwlProjection, RdfFormat};
use uuid::Uuid;

use crate::state::AppState;

/// What happens to a class or property in this import.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// No such IRI in the ontology -> create
    Create,
    /// IRI already present -> update label and description (key stays: it may already be referenced)
    Update,
    /// Key taken by another IRI -> skip and report; neither rename silently nor overwrite
    KeyTaken,
    /// Key taken by another IRI, but the alignment table says the two are **the same thing** -> skip, not counted as a conflict.
    /// Kept apart from KeyTaken because it needs no human decision: not creating a duplicate class is exactly the desired result
    Aligned,
    /// The vocabulary itself says this property is superseded by another (`schema:supersededBy`), and that superseding one
    /// is in this file or in the database -> do not create. Creating it would make two keys point at the same relation: an exact key
    /// match in extraction would permanently split `employees` from `employee`, and morphological normalization would never get a chance (#560).
    /// Once not created, the model saying `employees` folds to `employee`
    Superseded,
}

/// Whether a property will be created in this import, and why.
///
/// **Preview must be able to say why.** The earlier version only reported "resolves to 54 properties", so readers could not tell
/// whether that meant "all will be created" or "none will be" -- and in fact it was the latter.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome", content = "detail")]
pub enum AttrNote {
    /// Will be created, with this datatype
    Datatype(&'static str),
    /// Will be created as text: no range was written and the vocabulary made no declaration; text is an honest superset
    NoRange,
    /// Will be created as text: the range is a readable literal but we cannot express its type (`xsd:time` etc.).
    /// The original IRI is reported so a person can change it to a better type; but the value still lands, and knowledge is not dropped because the type is rough
    DegradedToText(String),
    /// Not created: extraction could never read this kind of value out of prose (binary, XML fragments, XML-internal identifiers).
    /// Even if created it would never be filled, and would only add a line of dead noise to every text block's prompt
    UnusableRange(String),
    /// Not created: no domain written. A property must hang off a class; this is a hard constraint of the store layer
    NoDomain,

    /// Not created: the domain points at a class **that is in this file but was skipped** (most likely a key collision).
    /// Reported separately from UnknownDomain because the remedy differs: this one is resolved by renaming the existing class
    DomainSkipped(String),
    /// Not created: the domain points at a class not in this file at all (an external vocabulary)
    UnknownDomain(String),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PlannedItem {
    pub iri: String,
    pub key: String,
    pub label: String,
    pub has_description: bool,
    pub disposition: Disposition,
    /// Relation-only: the import declares it functional -- preview must list these separately
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub functional: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_with: Option<String>,
    /// Property-only: which datatype it would be created with, or why it cannot be created
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attr: Option<AttrNote>,
}

/// The full plan for one import. Preview returns it, and persist executes it too.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportPlan {
    pub format: String,
    pub triples: usize,
    pub classes: Vec<PlannedItem>,
    pub relations: Vec<PlannedItem>,
    pub attributes: Vec<PlannedItem>,
    /// Axioms that appeared but are not consumed today -> counts. **Not "skipped", but "not projected yet"**
    pub unprojected: Vec<(String, usize)>,
    /// Number of classes without `rdfs:comment`. Their extraction quality is noticeably lower --
    /// the description goes verbatim into the prompt, and it is the only basis the model has for judging "what counts as this class"
    pub classes_without_description: usize,
    /// Number of relations declared functional. **This is the enterprise edition of the part_of pitfall**: the ontology declares uniqueness,
    /// the data does not obey it, and right after import you get a queue of false conflicts
    pub functional_relations: usize,
}

/// Where a property goes. **Domain is judged first**: a property without a domain simply cannot be created,
/// and what its range is no longer matters at that point.
fn attr_note(
    p: &ontology_rdf::OwlProperty,
    vocab: &ontology_rdf::VocabDatatypes,
    resolvable: &HashSet<&str>,
    in_file: &HashSet<&str>,
) -> AttrNote {
    if p.domains.is_empty() {
        return AttrNote::NoDomain;
    }
    // **It fails only if none resolve at all**: when some of several domains are skipped,
    // the property is still created, just attached to fewer classes -- better than dropping the whole thing, and the skipped classes
    // were each already reported in the plan as key collisions
    if !p.domains.iter().any(|d| resolvable.contains(d.as_str())) {
        let first = &p.domains[0];
        return if in_file.contains(first.as_str()) {
            AttrNote::DomainSkipped(first.clone())
        } else {
            AttrNote::UnknownDomain(first.clone())
        };
    }
    match ontology_rdf::map_range_of(p, vocab) {
        ontology_rdf::RangeMapping::Datatype(dt) => AttrNote::Datatype(dt),
        ontology_rdf::RangeMapping::Absent => AttrNote::NoRange,
        ontology_rdf::RangeMapping::Degraded(iri) => AttrNote::DegradedToText(iri),
        ontology_rdf::RangeMapping::Unusable(iri) => AttrNote::UnusableRange(iri),
    }
}

/// Whether a property will be created, and with which datatype. Persist and statistics share it,
/// so that "preview says it will be created" and "it was created" are not judged twice in two places and drift apart.
fn attr_datatype(note: &AttrNote) -> Option<&'static str> {
    match note {
        AttrNote::Datatype(dt) => Some(dt),
        // No range was written, or it was written but the type cannot be expressed: the value is still a literal,
        // and text is an honest superset that blocks nothing
        AttrNote::NoRange | AttrNote::DegradedToText(_) => Some("text"),
        _ => None,
    }
}

impl ImportPlan {
    /// Properties that cannot be created, counted grouped by reason. **Reporting only the total in preview is not enough** --
    /// "54 properties" does not tell whether they will all be created or none, and in fact it depends on the reason.
    pub fn attr_skips(&self) -> BTreeMap<&str, usize> {
        let mut out: BTreeMap<&str, usize> = BTreeMap::new();
        for a in &self.attributes {
            if a.disposition == Disposition::KeyTaken {
                *out.entry("key_taken").or_default() += 1;
                continue;
            }
            if a.disposition == Disposition::Superseded {
                *out.entry("superseded").or_default() += 1;
                continue;
            }
            let reason = match a.attr.as_ref() {
                Some(AttrNote::Datatype(_))
                | Some(AttrNote::NoRange)
                | Some(AttrNote::DegradedToText(_)) => continue,
                Some(AttrNote::UnusableRange(_)) => "unusable_range",
                Some(AttrNote::NoDomain) => "no_domain",
                Some(AttrNote::DomainSkipped(_)) => "domain_skipped",
                Some(AttrNote::UnknownDomain(_)) => "unknown_domain",
                None => continue,
            };
            *out.entry(reason).or_default() += 1;
        }
        out
    }
}

/// The parser's hint map as the flat object the store keeps (RC-008)
fn display_hints_json(hints: &std::collections::BTreeMap<String, String>) -> serde_json::Value {
    serde_json::Value::Object(
        hints
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect(),
    )
}

/// Parse the file and compute the plan against the existing ontology. Writes nothing.
pub async fn plan(
    state: &AppState,
    kb_id: Uuid,
    filename: &str,
    bytes: &[u8],
) -> AppResult<(ImportPlan, OwlProjection, RdfFormat)> {
    let format = RdfFormat::detect(filename, bytes);
    let proj = ontology_rdf::project(bytes, format).map_err(|e| {
        utopia_core::AppError::invalid_detail(
            "bad_ontology_file",
            "Could not parse this ontology file",
            e.to_string(),
        )
    })?;

    // Existing ontology: build one index by IRI and one by key, and judge the two kinds of conflict separately
    let etypes = utopia_store::graph::entity_types(&state.pool, kb_id).await?;
    let rtypes = utopia_store::graph::relation_types(&state.pool, kb_id).await?;
    let e_by_iri: HashMap<&str, &_> = etypes
        .iter()
        .filter_map(|t| t.iri.as_deref().map(|i| (i, t)))
        .collect();
    let e_by_key: HashMap<&str, &_> = etypes.iter().map(|t| (t.key.as_str(), t)).collect();
    let r_by_iri: HashMap<&str, &_> = rtypes
        .iter()
        .filter_map(|t| t.iri.as_deref().map(|i| (i, t)))
        .collect();
    let r_by_key: HashMap<&str, &_> = rtypes.iter().map(|t| (t.key.as_str(), t)).collect();

    // Class IRIs that can resolve to an id after this run: those created or updated in the file, plus those already in the database with the same IRI.
    // Classes skipped for a key collision are **not among them** -- they will not be created, so properties hanging off them have nowhere to hang
    // Within this import, keys also collide: different IRIs derive the same short label
    //(FOAF's familyName and family_name both became family_name).
    // Checking only against the database is not enough -- then the second one would show "will create" in preview,
    // and at persist time would be silently dropped by ON CONFLICT, so preview would be telling a lie
    //
    // **Two namespaces, not one**: classes go in entity_types, relations and properties in relation_types,
    // each with its own (kb_id, key) unique constraint. Merging them into one table would invent a constraint out of nothing --
    // and the cost is concrete: schema.org's location / address are properties, but they were first taken by
    // the **classes** Location / Address from OMG Commons (classes are processed first),
    // so when extraction names location, the database simply has none
    let mut claimed_class: HashMap<&str, &str> = HashMap::new();
    let mut claimed_prop: HashMap<&str, &str> = HashMap::new();

    let mut classes = Vec::new();
    for c in &proj.classes {
        // When the alignment table judges "same name, different meaning", use the key it declares in place of the derived one
        let mut renamed_key: Option<&'static str> = None;
        let (disposition, conflict_with) = if let Some(prev) = claimed_class.get(c.key.as_str()) {
            (Disposition::KeyTaken, Some((*prev).to_string()))
        } else if e_by_iri.contains_key(c.iri.as_str()) {
            (Disposition::Update, None)
        } else if let Some(existing) = e_by_key.get(c.key.as_str()) {
            match existing.iri.as_deref() {
                // **The placeholder has no IRI: claim it.**
                //
                // No IRI means this class was given a local name (a seed ontology, or hand-built),
                // not a homonym from another vocabulary. The import says "this IRI is that class",
                // and if this step were not done the whole tree would be broken: schema.org's Organization
                // collides with the built-in organization, is skipped, so Corporation's parent points at
                // something never created -- none of the built-in base classes gets a subclass attached,
                // and type refinement is out of reach.
                None => (Disposition::Update, None),
                // There is already a **different** IRI: two vocabularies compete for the same short label.
                // First check the prepackaged alignment table -- that is a **declared** disposition, so reimporting gives the same result,
                // and so the reason below for "no automatic suffix" does not apply to it
                Some(other) => match crate::pack_alignment::lookup(&c.iri, other) {
                    // Same meaning: the existing one is it, and not creating a duplicate class is exactly what is wanted
                    Some(crate::pack_alignment::Alignment::SameAs) => {
                        (Disposition::Aligned, existing.iri.clone())
                    }
                    // Same name, different meaning: create it under a declared key instead
                    Some(crate::pack_alignment::Alignment::Rename(k)) => {
                        renamed_key = Some(k);
                        (Disposition::Create, None)
                    }
                    // Not in the table: this is a real conflict. No automatic suffix -- that would stop a reimport
                    // from recognizing which one it created last time
                    None => (Disposition::KeyTaken, existing.iri.clone()),
                },
            }
        } else {
            (Disposition::Create, None)
        };
        // The renamed and original name are both borrows (the former 'static, the latter borrowed from proj), unified into one reference used then landed
        let key: &str = renamed_key.unwrap_or(c.key.as_str());
        if !matches!(disposition, Disposition::KeyTaken | Disposition::Aligned) {
            claimed_class.insert(key, c.iri.as_str());
        }
        classes.push(PlannedItem {
            iri: c.iri.clone(),
            key: key.to_string(),
            label: c.label.clone(),
            has_description: !c.description.trim().is_empty(),
            disposition,
            functional: false,
            conflict_with,
            attr: None,
        });
    }

    // Classes that appeared in the file (including skipped ones) -- used to distinguish "skipped" from "not in this file at all"
    let in_file: HashSet<&str> = proj.classes.iter().map(|c| c.iri.as_str()).collect();
    let resolvable: HashSet<&str> = classes
        .iter()
        .filter(|c| c.disposition != Disposition::KeyTaken)
        .map(|c| c.iri.as_str())
        .chain(e_by_iri.keys().copied())
        .collect();

    let mut relations = Vec::new();
    let mut attributes = Vec::new();
    // A yielding property is skipped only when the superseding one **can** land: the superseder is in this file, or already in the database.
    // When the superseder is absent, create it anyway -- otherwise a small vocabulary that carried only the yielding side would create nothing at all
    let prop_iris: HashSet<&str> = proj.properties.iter().map(|p| p.iri.as_str()).collect();
    for p in &proj.properties {
        let successor = p
            .superseded_by
            .as_deref()
            .filter(|s| prop_iris.contains(s) || r_by_iri.contains_key(s));
        let (disposition, conflict_with) = if let Some(s) = successor {
            (Disposition::Superseded, Some(s.to_string()))
        } else if let Some(prev) = claimed_prop.get(p.key.as_str()) {
            (Disposition::KeyTaken, Some((*prev).to_string()))
        } else if r_by_iri.contains_key(p.iri.as_str()) {
            (Disposition::Update, None)
        } else if let Some(existing) = r_by_key.get(p.key.as_str()) {
            (Disposition::KeyTaken, existing.iri.clone())
        } else {
            (Disposition::Create, None)
        };
        if !matches!(disposition, Disposition::KeyTaken | Disposition::Superseded) {
            claimed_prop.insert(p.key.as_str(), p.iri.as_str());
        }
        let mut item = PlannedItem {
            iri: p.iri.clone(),
            key: p.key.clone(),
            label: p.label.clone(),
            has_description: !p.description.trim().is_empty(),
            disposition,
            functional: p.functional,
            conflict_with,
            attr: None,
        };
        if p.is_datatype {
            item.attr = Some(attr_note(p, &proj.vocab_datatypes, &resolvable, &in_file));
            attributes.push(item);
        } else {
            relations.push(item);
        }
    }

    let mut unprojected: Vec<(String, usize)> = proj
        .unprojected
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    unprojected.sort_by_key(|(_, n)| std::cmp::Reverse(*n));

    let plan = ImportPlan {
        format: format!("{format:?}").to_lowercase(),
        triples: proj.triples,
        classes_without_description: classes.iter().filter(|c| !c.has_description).count(),
        functional_relations: relations.iter().filter(|r| r.functional).count(),
        classes,
        relations,
        attributes,
        unprojected,
    };
    Ok((plan, proj, format))
}

/// After installing a group of packages, go over them once more: wire up the relations whose domain / range point to **classes from other packages**.
///
/// A single import resolves classes in this file and classes already in the database (see resolve in [`apply`]), but packages are installed one by one:
/// when installing W3C Org, FOAF was not there yet, so `headOf`'s `rdfs:domain foaf:Agent` landed on nothing;
/// once FOAF is installed, nobody goes back to fill it in. A predicate without a domain cannot be given a direction,
/// so the reverse `Project Aurora head_of Li Ting` goes into the graph as is (#222).
///
/// Only fills in, never deletes; the join table uses ON CONFLICT DO NOTHING, so re-running is harmless. Properties are not here: their fate
/// was settled at plan time (one without a domain cannot be created at all), and filling in a domain afterwards cannot change
/// whether it exists
pub async fn relink_domains_ranges(
    state: &AppState,
    kb_id: Uuid,
    filename: &str,
    bytes: &[u8],
) -> AppResult<(usize, usize)> {
    let format = RdfFormat::detect(filename, bytes);
    let proj = ontology_rdf::project(bytes, format).map_err(|e| {
        utopia_core::AppError::invalid_detail(
            "bad_ontology_file",
            "Could not parse this ontology file",
            e.to_string(),
        )
    })?;
    let classes: HashMap<String, Uuid> = utopia_store::graph::entity_types(&state.pool, kb_id)
        .await?
        .into_iter()
        .filter_map(|t| t.iri.clone().map(|i| (i, t.id)))
        .collect();
    let relations: HashMap<String, Uuid> = utopia_store::graph::relation_types(&state.pool, kb_id)
        .await?
        .into_iter()
        .filter_map(|r| r.iri.clone().map(|i| (i, r.id)))
        .collect();
    let mut link_d: Vec<(Uuid, Uuid)> = Vec::new();
    let mut link_r: Vec<(Uuid, Uuid)> = Vec::new();
    for p in &proj.properties {
        if p.is_datatype {
            continue;
        }
        let Some(&rid) = relations.get(&p.iri) else {
            continue;
        };
        link_d.extend(
            p.domains
                .iter()
                .filter_map(|d| classes.get(d).map(|&t| (rid, t))),
        );
        link_r.extend(
            p.ranges
                .iter()
                .filter_map(|r| classes.get(r).map(|&t| (rid, t))),
        );
    }
    utopia_store::ontology::link_domains_ranges_bulk(&state.pool, &link_d, &link_r).await?;
    Ok((link_d.len(), link_r.len()))
}

/// Execute the plan. Properties persist after classes -- they must hang off a domain, and the domain must wait
/// until classes are created and their IRIs resolve to ids (which is the `id_of` below).
///
/// `via_token`: the token that imported on behalf of `actor`, recorded only in the audit row (0014 decision 6); None for a session
pub async fn apply(
    state: &AppState,
    kb_id: Uuid,
    actor: Uuid,
    via_token: Option<Uuid>,
    filename: &str,
    bytes: &[u8],
) -> AppResult<(Uuid, ImportPlan)> {
    let (plan, proj, format) = plan(state, kb_id, filename, bytes).await?;

    // Layer one: the original text is stored as a blob addressed by content. **Store the original before changing the ontology** -- projection failures can be retried,
    // but once the original is lost you can never go back
    let sha = sha256_hex(bytes);
    state
        .blob
        .put(&sha, bytes)
        .await
        .map_err(utopia_core::AppError::Other)?;

    let by_iri: HashMap<&str, &_> = proj.classes.iter().map(|c| (c.iri.as_str(), c)).collect();
    // Classes already in the database: IRI -> id. Aligned uses this to connect the "same-meaning one" to its parent reference
    let existing_by_iri: HashMap<String, Uuid> =
        utopia_store::graph::entity_types(&state.pool, kb_id)
            .await?
            .into_iter()
            .filter_map(|t| t.iri.clone().map(|i| (i, t.id)))
            .collect();
    let mut created_classes = 0usize;
    let mut updated_classes = 0usize;
    // IRI -> id in the ontology, needed by parent-class resolution
    let mut id_of: HashMap<String, Uuid> = HashMap::new();

    // **Newly created classes are inserted in one batch, not one by one.**
    //
    // Executing one by one with `execute(pool)` commits each separately; at the scale of schema.org (968 classes and about
    // 1500 properties) that is five thousand fsyncs -- measured at 45 seconds for the import. The same rows in one
    // UNNEST statement take 536 milliseconds. Updates and Aligned stay one by one below: they are single digits on an empty database, and
    // each reads the current state once, so they cannot be batched.
    let new_classes: Vec<utopia_store::ontology::BulkClass> = plan
        .classes
        .iter()
        .filter(|i| i.disposition == Disposition::Create)
        .filter_map(|i| by_iri.get(i.iri.as_str()).map(|c| (i, *c)))
        // The key uses the item's: when the alignment table judges same name / different meaning, it is the one that was changed
        .map(|(i, c)| utopia_store::ontology::BulkClass {
            key: i.key.clone(),
            label: c.label.clone(),
            description: c.description.clone(),
            iri: c.iri.clone(),
            display_hints: display_hints_json(&c.display_hints),
        })
        .collect();
    let created_ids =
        utopia_store::ontology::create_entity_types_bulk(&state.pool, kb_id, &new_classes).await?;
    for c in &new_classes {
        if let Some(id) = created_ids.get(&c.key) {
            id_of.insert(c.iri.clone(), *id);
            created_classes += 1;
        }
    }

    for item in &plan.classes {
        let Some(c) = by_iri.get(item.iri.as_str()) else {
            continue;
        };
        match item.disposition {
            // Above, already batch-created
            Disposition::Create => {}
            Disposition::Update => {
                // Two kinds of Update: this IRI was imported last time (found by IRI),
                // or it is claiming a same-named local class (found by key, and that row has no IRI yet)
                let updated = utopia_store::ontology::update_type_from_import(
                    &state.pool,
                    kb_id,
                    &c.iri,
                    &c.label,
                    &c.description,
                    &display_hints_json(&c.display_hints),
                )
                .await?;
                let id = match updated {
                    Some(id) => Some(id),
                    None => {
                        utopia_store::ontology::adopt_iri_onto_key(
                            &state.pool,
                            kb_id,
                            &c.key,
                            &c.iri,
                        )
                        .await?
                    }
                };
                if let Some(id) = id {
                    id_of.insert(c.iri.clone(), id);
                    updated_classes += 1;
                }
            }
            // Key taken: reported already, do not touch
            // Same meaning: do not create. But point the IRI at the existing class's id, otherwise a subclass whose parent is it
            // would fail to resolve, and the whole tree would break here
            Disposition::Aligned => {
                if let Some(target) = item.conflict_with.as_deref() {
                    if let Some(id) = existing_by_iri.get(target) {
                        id_of.insert(c.iri.clone(), *id);
                    }
                }
            }
            // Classes have no notion of yielding (schema:supersededBy only marks properties); exhaustive matching lets the compiler watch this for us
            Disposition::KeyTaken | Disposition::Superseded => {}
        }
    }

    // Second parent-resolution pass: the parent may not have been created yet on the first pass
    // (child, parent) edges are collected first, then inserted in one go
    let mut parent_edges: Vec<(Uuid, Uuid)> = Vec::new();
    for c in &proj.classes {
        let Some(&child) = id_of.get(&c.iri) else {
            continue;
        };
        // **All parents**, no longer just the first. FOAF's Person is both an Agent and a
        // SpatialThing; dropping the latter branch would make properties whose domain sits on that branch fail their check.
        // Parents pointing at classes never created simply fall out -- losing one branch beats losing the whole thing
        let parents: Vec<Uuid> = c
            .parents
            .iter()
            .filter_map(|iri| id_of.get(iri).copied())
            .collect();
        parent_edges.extend(parents.into_iter().map(|p| (child, p)));
    }
    // Inserted in one go. The single version runs a recursive CTE to check for cycles every time; schema.org has 975 parent edges,
    // which is 975 recursive queries plus 975 commits.
    //
    // **The batch version does not check for cycles.** The single version's handling of a cycle was only to skip that one (`let _ =`),
    // without aborting the import; and after import the ontology page can still discover it and let a person deal with it. Spending nine hundred-odd recursive queries
    // to discover it a bit earlier is not worth it
    utopia_store::ontology::set_parents_bulk(&state.pool, &parent_edges).await?;

    // Mutual exclusion of classes is batched the same way (see the axiom columns of `relation_types`). Those pointing at classes never created simply fall out --
    // both ends of a mutual-exclusion declaration must be in this database before it can be used to judge contradictions
    let mut disjoint_edges: Vec<(Uuid, Uuid)> = Vec::new();
    for c in &proj.classes {
        let Some(&a) = id_of.get(&c.iri) else {
            continue;
        };
        for other in &c.disjoint_with {
            if let Some(&b) = id_of.get(other) {
                disjoint_edges.push((a, b));
            }
        }
    }
    utopia_store::ontology::set_disjoint_bulk(&state.pool, kb_id, &disjoint_edges).await?;

    let by_prop_iri: HashMap<&str, &_> = proj
        .properties
        .iter()
        .map(|p| (p.iri.as_str(), p))
        .collect();
    // Relations. Before this, apply skipped them entirely -- yet preview wrote "N new" on the relation line,
    // promising something that would not happen. This is the same kind of defect as the key collision fixed today.
    //
    // **functional / inverse_functional are written as the vocabulary declares them**: they are what the temporal engine
    // uses to close facts automatically, and a wrong guess would produce a batch of false conflicts (the part_of incident had 59). So we do not guess --
    // what the vocabulary says is what it is, and preview has already listed them separately for a person to review.
    let mut created_rels = 0usize;
    let mut updated_rels = 0usize;
    let mut new_rels: Vec<utopia_store::ontology::BulkRelation> = Vec::new();
    // (key, domains, ranges): the relation id is not known until the batch insert finishes, so it is recorded by key for now
    let mut pending_links: Vec<(String, Vec<Uuid>, Vec<Uuid>)> = Vec::new();
    for item in &plan.relations {
        if matches!(
            item.disposition,
            Disposition::KeyTaken | Disposition::Superseded
        ) {
            continue;
        }
        let Some(p) = by_prop_iri.get(item.iri.as_str()) else {
            continue;
        };
        // When a domain/range points to a class never created, drop only that one, not the whole relation:
        // a relation, unlike a property, does not have to hang off a class; no domain just means "unrestricted subject type".
        //
        // **Classes already in the database count too.** Previously only classes in this file were recognized, so W3C Org's
        // `headOf rdfs:domain foaf:Agent` lost its domain even in a database where FOAF was already installed,
        // and a predicate without a domain cannot be given a direction at all (#222)
        let resolve = |iris: &[String]| -> Vec<Uuid> {
            iris.iter()
                .filter_map(|i| {
                    id_of
                        .get(i)
                        .copied()
                        .or_else(|| existing_by_iri.get(i).copied())
                })
                .collect()
        };
        let domains = resolve(&p.domains);
        let ranges = resolve(&p.ranges);

        if item.disposition == Disposition::Update {
            if utopia_store::ontology::update_relation_from_import(
                &state.pool,
                kb_id,
                &p.iri,
                &p.label,
                &p.description,
                &domains,
                &ranges,
                p.temporal.as_deref(),
                &display_hints_json(&p.display_hints),
            )
            .await?
            {
                updated_rels += 1;
            }
            continue;
        }
        // Newly created ones are collected and inserted in one go -- the reason is the same as for classes above
        new_rels.push(utopia_store::ontology::BulkRelation {
            key: p.key.clone(),
            label: p.label.clone(),
            description: p.description.clone(),
            iri: p.iri.clone(),
            kind: "relation",
            datatype: None,
            functional: p.functional,
            inverse_functional: p.inverse_functional,
            transitive: p.transitive,
            symmetric: p.symmetric,
            asymmetric: p.asymmetric,
            irreflexive: p.irreflexive,
            // What the ontology states with `utopia:temporal` is taken as stated (0031); what it does not state is state
            temporal: p.temporal.as_deref().unwrap_or("state").to_string(),
            display_hints: display_hints_json(&p.display_hints),
        });
        pending_links.push((p.key.clone(), domains, ranges));
    }
    let rel_ids =
        utopia_store::ontology::create_relation_types_bulk(&state.pool, kb_id, &new_rels).await?;
    created_rels += rel_ids.len();

    // Inverse and parent property point at **another relation type**, whose id is only known after all are inserted -- so this is a second pass.
    // A single pass could only handle files where "the parent property happens to come first", and RDF triples have no order.
    //
    // Both newly created and already existing ones are collected: on re-import of an ontology, these two declarations may have been added just now
    let mut inv_pairs: Vec<(String, String)> = Vec::new();
    let mut sub_pairs: Vec<(String, String)> = Vec::new();
    for item in &plan.relations {
        let Some(p) = by_prop_iri.get(item.iri.as_str()) else {
            continue;
        };
        if let Some(o) = &p.inverse_of {
            inv_pairs.push((p.iri.clone(), o.clone()));
        }
        if let Some(o) = &p.sub_property_of {
            sub_pairs.push((p.iri.clone(), o.clone()));
        }
    }
    let (linked_inv, linked_sub) = utopia_store::ontology::link_property_axioms_bulk(
        &state.pool,
        kb_id,
        &inv_pairs,
        &sub_pairs,
    )
    .await?;
    // Join rows are flattened too: the single version runs 4 statements per relation (one DELETE and one INSERT per table),
    // so 1,500 relations would be 6,000 commits
    let mut link_d: Vec<(Uuid, Uuid)> = Vec::new();
    let mut link_r: Vec<(Uuid, Uuid)> = Vec::new();
    for (key, domains, ranges) in &pending_links {
        let Some(rid) = rel_ids.get(key) else {
            continue;
        };
        link_d.extend(domains.iter().map(|d| (*rid, *d)));
        link_r.extend(ranges.iter().map(|r| (*rid, *r)));
    }
    utopia_store::ontology::link_domains_ranges_bulk(&state.pool, &link_d, &link_r).await?;

    // Properties: they come only after classes are created and id_of is filled in. The plan has already worked out each property's fate,
    // **here we only execute, and do not judge again** -- judging in two places would diverge, and divergence means
    // what preview says is not what actually gets done
    let mut created_attrs = 0usize;
    let mut updated_attrs = 0usize;
    let mut new_attrs: Vec<utopia_store::ontology::BulkRelation> = Vec::new();
    let mut pending_attr_domains: Vec<(String, Vec<Uuid>)> = Vec::new();
    for item in &plan.attributes {
        if matches!(
            item.disposition,
            Disposition::KeyTaken | Disposition::Superseded
        ) {
            continue;
        }
        let (Some(note), Some(p)) = (item.attr.as_ref(), by_prop_iri.get(item.iri.as_str())) else {
            continue;
        };
        let Some(dt) = attr_datatype(note) else {
            continue;
        };
        // Those judged resolvable at plan time but lacking an id at persist time (class skipped or update failed) fall out naturally;
        // if all fall out, create nothing, rather than making one that hangs off nothing
        let domain_ids: Vec<Uuid> = p
            .domains
            .iter()
            .filter_map(|iri| id_of.get(iri).copied())
            .collect();
        if domain_ids.is_empty() {
            continue;
        }
        // An attribute this file already brought in is refreshed like a relation is
        // (RC-008 needs its hints to follow the file); the bulk insert below would
        // only skip it on the key conflict
        if item.disposition == Disposition::Update {
            if utopia_store::ontology::update_relation_from_import(
                &state.pool,
                kb_id,
                &p.iri,
                &p.label,
                &p.description,
                &domain_ids,
                &[],
                None,
                &display_hints_json(&p.display_hints),
            )
            .await?
            {
                updated_attrs += 1;
            }
            continue;
        }
        new_attrs.push(utopia_store::ontology::BulkRelation {
            key: p.key.clone(),
            label: p.label.clone(),
            description: p.description.clone(),
            iri: p.iri.clone(),
            kind: "attribute",
            datatype: Some(dt.to_string()),
            // Properties do not take part in temporal closure, nor in consistency checks: those judgments are all about
            // **edges between entities**, while a property's object is a literal value
            functional: false,
            inverse_functional: false,
            transitive: false,
            symmetric: false,
            asymmetric: false,
            irreflexive: false,
            // Properties are created as state, and the page offers no other choice (one of the "undecided" items in 0031)
            temporal: "state".to_string(),
            display_hints: display_hints_json(&p.display_hints),
        });
        pending_attr_domains.push((p.key.clone(), domain_ids));
    }
    let attr_ids =
        utopia_store::ontology::create_relation_types_bulk(&state.pool, kb_id, &new_attrs).await?;
    created_attrs += attr_ids.len();
    let mut attr_links: Vec<(Uuid, Uuid)> = Vec::new();
    for (key, domains) in &pending_attr_domains {
        let Some(aid) = attr_ids.get(key) else {
            continue;
        };
        attr_links.extend(domains.iter().map(|d| (*aid, *d)));
    }
    utopia_store::ontology::link_domains_ranges_bulk(&state.pool, &attr_links, &[]).await?;

    let summary = serde_json::json!({
        "classes_created": created_classes,
        "classes_updated": updated_classes,
        "classes_key_taken": plan.classes.iter().filter(|c| c.disposition == Disposition::KeyTaken).count(),
        "relations_seen": plan.relations.len(),
        // Count of inverse / parent properties connected. **Report it** -- when the target IRI is not in this database it is silently skipped
        // (referencing an external vocabulary is normal), and without a count nobody knows how many links were missed
        "inverse_linked": linked_inv,
        "sub_property_linked": linked_sub,
        "relations_created": created_rels,
        // Those the vocabulary itself yields on: not created, and not counted as conflicts either
        "relations_superseded": plan.relations.iter().filter(|r| r.disposition == Disposition::Superseded).count(),
        "relations_updated": updated_rels,
        "attributes_seen": plan.attributes.len(),
        "attributes_created": created_attrs,
        "attributes_updated": updated_attrs,
        "attributes_skipped": plan.attr_skips(),
        "classes_without_description": plan.classes_without_description,
        "functional_relations": plan.functional_relations,
        "unprojected": plan.unprojected.iter().take(30).collect::<Vec<_>>(),
        "triples": plan.triples,
    });
    let import_id = utopia_store::ontology::record_import(
        &state.pool,
        kb_id,
        &sha,
        filename,
        &format!("{format:?}").to_lowercase(),
        bytes.len() as i64,
        &summary,
        actor,
    )
    .await?;

    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        actor,
        "ontology.imported",
        "kb",
        Some(kb_id),
        utopia_store::audit::via_token(summary, via_token),
    )
    .await;
    // The ontology just changed, so the vectors are stale. **Queue a task rather than run in place**: this pass embeds thousands of rows;
    // inside the import request it would make the user wait six to eight more minutes after clicking confirm. If it cannot be queued, that is fine --
    // the index heals itself, and the next person who uses retrieval backfills it
    let _ = utopia_store::jobs::enqueue(
        &state.pool,
        "embed_ontology",
        serde_json::json!({ "kb_id": kb_id }),
    )
    .await;

    Ok((import_id, plan))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
