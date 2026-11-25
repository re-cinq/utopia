//! Parsing and **projection** of OWL / RDFS files.
//!
//! Division of labour, per the three layers set in 0001: this module does only the second layer (projection); the first layer (the source kept verbatim in a blob)
//! is the caller's job. **Anything this reader cannot understand is not an error; it is "not yet projected"**: the source stays put,
//! and once a consumer is added later, the file is simply re-run.
//!
//! Parse only, no inference: the two small Oxigraph parsers yield triples, and we pick the usable ones from a list.
//! No horned-owl: reasoning is 0002's job, and it reads the source, not the projection.

use std::collections::{BTreeMap, BTreeSet};

/// One class from the projection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OwlClass {
    pub iri: String,
    /// Short label derived from the IRI (the token the model reads and writes); the caller is responsible for de-duplicating and adding suffixes
    pub key: String,
    pub label: String,
    /// `rdfs:comment`: a load-bearing field, passed verbatim into the extraction prompt
    pub description: String,
    /// All parent class IRIs from `rdfs:subClassOf` (multiple inheritance is the norm here)
    pub parents: Vec<String>,
    /// IRIs of the other side of `owl:disjointWith`. **Both directions are collected**, because the axiom is symmetric,
    /// while vocabularies usually write it only once
    pub disjoint_with: Vec<String>,
    /// How the type asks to be shown (RC-008): the `utopia:panel` / `utopia:chain_first`
    /// literals, keyed by the bare word. Empty when the file says nothing.
    pub display_hints: BTreeMap<String, String>,
}

/// One projected property (object property → relation, data property → attribute).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OwlProperty {
    pub iri: String,
    pub key: String,
    pub label: String,
    pub description: String,
    /// true = goes through the attribute channel (literal values), false = goes through the relation channel.
    /// Two sources: an explicit `owl:DatatypeProperty`, or a range that is all datatypes
    pub is_datatype: bool,
    pub functional: bool,
    pub inverse_functional: bool,
    /// OWL property axioms. The consistency check (0002 R0) decides on these: without them,
    /// there is no basis for judging whether `A part_of B` together with `B part_of A` is a contradiction or normal,
    pub transitive: bool,
    pub symmetric: bool,
    pub asymmetric: bool,
    pub irreflexive: bool,
    /// IRIs of the other side of `owl:inverseOf`. **Collected only in the direction written**; normalization (adding the inverse)
    /// happens when axioms are read (`reasoning::axioms`), which cannot be avoided there;
    /// adding it here would make "what the ontology itself wrote" indistinguishable from "what we inferred"
    pub inverse_of: Option<String>,
    /// Parent property IRIs from `rdfs:subPropertyOf`. Only the first one is kept when there are several:
    /// OWL allows multiple parents, but R1's rule moves up only one level at a time, and multiple parents need a different shape
    pub sub_property_of: Option<String>,
    /// Other side of `schema:supersededBy`: this property has been superseded by another. In schema.org,
    /// `employees` gives way to `employee`, and `founders` to `founder`; both are still in the vocabulary.
    /// If this bit is not read, both become relations, and at extraction each exact key matches its own, so one relation is permanently split in two (#560)
    pub superseded_by: Option<String>,
    pub domains: Vec<String>,
    pub ranges: Vec<String>,
    /// **Whether multiple ranges are a union or an intersection.** Several `rdfs:range` entries are an intersection
    /// ("must be both"), while several `schema:rangeIncludes` entries are a union
    /// ("either will do"). Pushing both into the same Vec without recording this bit means
    /// `author rangeIncludes Organization, Person` gets read as
    /// "must be both an organization and a person", then degrades to text: an edge is lost that way
    pub ranges_union: bool,
    /// Value of `utopia:temporal`: `state` / `event` / `eternal` (0031). The export side has always written this one;
    /// the import side did not read it before, so a source that says "this relation is a moment in time" was imported as `state`.
    /// Missing or any other value is `None`, stored as `state` when persisted; data properties do not need it when read
    pub temporal: Option<String>,
    /// How the property asks to be shown (RC-008): `utopia:role`, `utopia:tones`,
    /// `utopia:chain_next` literals, keyed by the bare word. Empty when the file says nothing.
    pub display_hints: BTreeMap<String, String>,
}

/// The result of one parse. `unprojected` is a **report**, not an error; see the module docs.
#[derive(Debug, Default)]
pub struct OwlProjection {
    pub classes: Vec<OwlClass>,
    pub properties: Vec<OwlProperty>,
    /// Predicates that appear but are not consumed today → count. Feeds the "not yet projected" column of the preview page
    pub unprojected: BTreeMap<String, usize>,
    /// Total triple count, so a person has a sense of "how big this file is"
    pub triples: usize,
    /// **IRIs that this file itself declares to be datatypes** → one of our four kinds.
    /// schema.org declares Text/Number/Date... as `a rdfs:Class, schema:DataType`;
    /// looking only at `rdfs:Class` would build them as entity classes (`text`, `boolean` become entity types)
    pub vocab_datatypes: VocabDatatypes,
}

/// Datatype IRIs the vocabulary declares itself → our four kinds (`text`/`number`/`date`/`bool`).
pub type VocabDatatypes = BTreeMap<String, &'static str>;

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDFS: &str = "http://www.w3.org/2000/01/rdf-schema#";
const OWL: &str = "http://www.w3.org/2002/07/owl#";
/// Our own annotation namespace, the same one the export side (`utopia-server::rdf`) uses
const UTOPIA_NS: &str = "urn:utopia:ns:";
/// The `utopia:*` literals that say how a type is shown (RC-008). Any other `utopia:*`
/// word stays reported as unprojected, so a typo in a file is visible in the preview.
pub const HINT_KEYS: [&str; 5] = ["panel", "chain_first", "role", "tones", "chain_next"];

/// schema.org's own domain/range scheme. **Not a standard vocabulary**, so it is hard-coded here,
/// but it must be recognized: schema.org and its derived vocabularies have no `rdfs:domain` at all,
/// and not recognizing these two predicates means ignoring the whole type system: 1600+ properties
/// would all become unconstrained relations.
/// Both schemes are accepted: old and new versions of the same vocabulary are published under https and http respectively.
const SCHEMA_NS: [&str; 2] = ["https://schema.org/", "http://schema.org/"];

/// Whether a predicate or class is one of the names under the schema.org namespace.
fn is_schema(iri: &str, local: &str) -> bool {
    SCHEMA_NS.iter().any(|ns| {
        iri.len() == ns.len() + local.len() && iri.starts_with(ns) && iri.ends_with(local)
    })
}

/// SHACL. Ontologies that put constraints on shapes (UCO is one, #923) likewise have almost no `rdfs:domain`:
/// a property shape `sh:property [ sh:path p; sh:class C ]` hangs off a node shape, and the node shape applies to
/// some class, and p is used on that class. The same reasoning as schema.org's `domainIncludes`: without recognizing it,
/// no data property has a domain, and the whole batch is skipped on import
const SH: &str = "http://www.w3.org/ns/shacl#";

/// The three entries on a property shape (path, class, datatype): triples whose subject is a blank node exist only to keep these
fn is_shape_detail(predicate: &str) -> bool {
    predicate
        .strip_prefix(SH)
        .is_some_and(|local| matches!(local, "path" | "class" | "datatype"))
}

/// Supported input formats. v1 does only these two, since most of what Protégé exports is them;
/// OWL/XML and Manchester syntax are deliberately cut (see "v1 cut" in 0001 P2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RdfFormat {
    Turtle,
    RdfXml,
}

impl RdfFormat {
    /// The extension is a strong signal; content overrules it only on a **clear contradiction**.
    ///
    /// It used to work the other way round (a `.rdf` that did not sniff as XML fell back to Turtle), and the FOAF official file
    /// starts with dozens of lines of `<!--` comments, with `<rdf:` outside the sniffing window, so it was fed to the
    /// Turtle parser and failed on the first line with "Invalid IRI code point".
    pub fn detect(filename: &str, bytes: &[u8]) -> Self {
        let lower = filename.to_ascii_lowercase();
        // Turtle's fingerprint is strong: only Turtle starts with @prefix / @base / PREFIX
        let looks_turtle = {
            let head = &bytes[..bytes.len().min(4096)];
            let s = String::from_utf8_lossy(head);
            s.lines()
                .map(str::trim_start)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .is_some_and(|l| {
                    l.starts_with("@prefix")
                        || l.starts_with("@base")
                        || l.starts_with("PREFIX")
                        || l.starts_with("BASE")
                })
        };
        if lower.ends_with(".ttl") || lower.ends_with(".turtle") || lower.ends_with(".n3") {
            return Self::Turtle;
        }
        if lower.ends_with(".rdf") || lower.ends_with(".owl") || lower.ends_with(".xml") {
            // .owl is common in both encodings, so content decides, but only "clearly looks like Turtle" overrules it
            return if looks_turtle {
                Self::Turtle
            } else {
                Self::RdfXml
            };
        }
        if looks_turtle {
            Self::Turtle
        } else {
            Self::RdfXml
        }
    }
}

/// Intermediate form of a triple: only the parts we can understand are kept.
struct Triple {
    subject: String,
    predicate: String,
    /// Some(...) when the object is an IRI
    object_iri: Option<String>,
    /// Some((value, language tag)) when the object is a literal
    object_lit: Option<(String, Option<String>)>,
    /// Some(`_:id`) when the object is a blank node. Only the SHACL pass reads this: the property shapes that `sh:property` points to
    /// are mostly blank nodes. The rest of the code looks only at `object_iri` and, as before, cannot see blank nodes
    object_node: Option<String>,
}

fn read_triples(bytes: &[u8], format: RdfFormat) -> anyhow::Result<Vec<Triple>> {
    use oxrdf::Term;
    let mut out = Vec::new();
    let mut push = |t: oxrdf::Triple| {
        let subject = match &t.subject {
            oxrdf::NamedOrBlankNode::NamedNode(n) => n.as_str().to_string(),
            // Property shapes are mostly blank nodes: their path, class and datatype are left for the SHACL pass
            oxrdf::NamedOrBlankNode::BlankNode(b) if is_shape_detail(t.predicate.as_str()) => {
                format!("_:{}", b.as_str())
            }
            // Blank nodes are anonymous class expressions (owl:Restriction and the like); projection does not touch them,
            // and they stay in the source for the reasoner
            oxrdf::NamedOrBlankNode::BlankNode(_) => return,
        };
        let (object_iri, object_lit) = match &t.object {
            Term::NamedNode(n) => (Some(n.as_str().to_string()), None),
            Term::Literal(l) => (
                None,
                Some((
                    l.value().to_string(),
                    l.language().map(|s| s.to_ascii_lowercase()),
                )),
            ),
            _ => (None, None),
        };
        let object_node = match &t.object {
            Term::BlankNode(b) => Some(format!("_:{}", b.as_str())),
            _ => None,
        };
        out.push(Triple {
            subject,
            predicate: t.predicate.as_str().to_string(),
            object_iri,
            object_lit,
            object_node,
        });
    };
    // **Relative IRIs need a base to resolve.** Reading from bytes gives no document URL, while the Turtle spec says
    // the base defaults to the document's own address. Give a placeholder: relative IRIs in ontology files are almost all
    // document-level metadata (PROV-O's `<#> a owl:Ontology`), and we do not consume owl:Ontology nodes,
    // so it is enough that they parse. Without it, the whole file fails at the first `<#>` with
    // "No scheme found in an absolute IRI", and everything is lost
    const BASE: &str = "urn:utopia:import";
    match format {
        RdfFormat::Turtle => {
            let p = oxttl::TurtleParser::new()
                .with_base_iri(BASE)
                .map_err(|e| anyhow::anyhow!("base IRI 无效：{e}"))?;
            for r in p.for_reader(bytes) {
                push(r?);
            }
        }
        RdfFormat::RdfXml => {
            let p = oxrdfxml::RdfXmlParser::new()
                .with_base_iri(BASE)
                .map_err(|e| anyhow::anyhow!("base IRI 无效：{e}"))?;
            for r in p.for_reader(bytes) {
                push(r?);
            }
        }
    }
    Ok(out)
}

/// Parse and project. Language tags prefer `@en`/`@zh`, then no tag, and finally any one.
pub fn project(bytes: &[u8], format: RdfFormat) -> anyhow::Result<OwlProjection> {
    let triples = read_triples(bytes, format)?;
    let mut proj = OwlProjection {
        // Blank-node triples left for property shapes are not counted: this number has always counted only subjects that are IRIs
        triples: triples
            .iter()
            .filter(|t| !t.subject.starts_with("_:"))
            .count(),
        ..Default::default()
    };

    // Classify first: which are classes, which are object properties, which are data properties, which carry the functional marker
    let mut classes: BTreeSet<String> = BTreeSet::new();
    let mut obj_props: BTreeSet<String> = BTreeSet::new();
    let mut data_props: BTreeSet<String> = BTreeSet::new();
    let mut functional: BTreeSet<String> = BTreeSet::new();
    let mut inverse_functional: BTreeSet<String> = BTreeSet::new();
    let mut transitive: BTreeSet<String> = BTreeSet::new();
    let mut symmetric: BTreeSet<String> = BTreeSet::new();
    // The two relations between properties (not type declarations, so collected separately)
    let mut inverse_of: BTreeMap<String, String> = BTreeMap::new();
    let mut sub_property_of: BTreeMap<String, String> = BTreeMap::new();
    let mut superseded_by: BTreeMap<String, String> = BTreeMap::new();
    let mut asymmetric: BTreeSet<String> = BTreeSet::new();
    let mut irreflexive: BTreeSet<String> = BTreeSet::new();
    let mut plain_props: BTreeSet<String> = BTreeSet::new();
    let mut datatype_roots: BTreeSet<String> = BTreeSet::new();
    // Nodes declared `sh:NodeShape`: those that are also classes apply to themselves via SHACL's implicit class target
    let mut node_shapes: BTreeSet<String> = BTreeSet::new();
    for t in &triples {
        if t.predicate != RDF_TYPE {
            continue;
        }
        let Some(o) = t.object_iri.as_deref() else {
            continue;
        };
        match o {
            x if x == format!("{OWL}Class") || x == format!("{RDFS}Class") => {
                classes.insert(t.subject.clone());
            }
            x if x == format!("{OWL}ObjectProperty") => {
                obj_props.insert(t.subject.clone());
            }
            x if x == format!("{OWL}DatatypeProperty") => {
                data_props.insert(t.subject.clone());
            }
            // rdf:Property does **not** say whether it is an object or a data property. Stored separately from explicit owl:ObjectProperty:
            // that one "says so" and must not be reclassified by range; this one "does not say", so
            // the channel is decided by range below, and falls back to relation if there is no range either
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#Property" => {
                plain_props.insert(t.subject.clone());
            }
            x if x == format!("{OWL}FunctionalProperty") => {
                functional.insert(t.subject.clone());
            }
            x if x == format!("{OWL}InverseFunctionalProperty") => {
                inverse_functional.insert(t.subject.clone());
            }
            // Property axioms: the R0 consistency check only becomes decidable with them.
            //
            // **The five bundled vocabularies cover only half of them** (TransitiveProperty: 14 entries,
            // SymmetricProperty: 1 entry, and Asymmetric and Irreflexive: none),
            // but those five are only a cold-start base. The end point of this line is importing **the enterprise's own ontologies**
            // (0001 opening: FIBO, industry standards, self-built Protégé ontologies), where
            // asymmetric and irreflexive declarations are common. Collect per OWL in full, not per the packs at hand.
            x if x == format!("{OWL}TransitiveProperty") => {
                transitive.insert(t.subject.clone());
            }
            x if x == format!("{OWL}SymmetricProperty") => {
                symmetric.insert(t.subject.clone());
            }
            x if x == format!("{OWL}AsymmetricProperty") => {
                asymmetric.insert(t.subject.clone());
            }
            x if x == format!("{OWL}IrreflexiveProperty") => {
                irreflexive.insert(t.subject.clone());
            }
            // Datatypes the vocabulary declares about itself. Only the explicitly declared roots are collected here;
            // subclasses (Integer ⊂ Number, URL ⊂ Text) are propagated down after the parent graph is collected.
            // **The marker classes are collected too**: schema:DataType's declaration is `a rdfs:Class`,
            // and if not collected, an entity type named data_type is left behind; this also lets
            // the `rdfs:subClassOf schema:DataType` form fall into the closure
            x if is_schema(x, "DataType") => {
                datatype_roots.insert(t.subject.clone());
                datatype_roots.insert(x.to_string());
            }
            x if x == format!("{SH}NodeShape") => {
                node_shapes.insert(t.subject.clone());
            }
            _ => {}
        }
    }

    // Then collect labels, comments, parents, domain/range
    let mut labels: BTreeMap<String, Vec<(String, Option<String>)>> = BTreeMap::new();
    let mut comments: BTreeMap<String, Vec<(String, Option<String>)>> = BTreeMap::new();
    let mut parents: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut disjoint: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut domains: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ranges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Properties used with rangeIncludes: their range is read as a union
    let mut union_ranged: BTreeSet<String> = BTreeSet::new();
    // The temporal semantics that `utopia:temporal` states; only three values are recognized
    let mut temporal: BTreeMap<String, String> = BTreeMap::new();
    let utopia_temporal = format!("{UTOPIA_NS}temporal");
    // subject → the display hints it declares (RC-008); the first value of a key wins
    let mut hints: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let known = |p: &str| {
        p == RDF_TYPE
            || p == format!("{RDFS}label")
            || p == format!("{RDFS}comment")
            || p == format!("{RDFS}subClassOf")
            || p == format!("{RDFS}subPropertyOf")
            || p == format!("{RDFS}domain")
            || p == format!("{RDFS}range")
            || is_schema(p, "domainIncludes")
            || is_schema(p, "rangeIncludes")
            || p == format!("{OWL}disjointWith")
            || p == format!("{OWL}inverseOf")
            || is_schema(p, "supersededBy")
            // The five things read by the shape pass (`shape_signatures`)
            || p.strip_prefix(SH).is_some_and(|local| {
                matches!(
                    local,
                    "targetClass" | "property" | "path" | "class" | "datatype"
                )
            })
    };
    for t in &triples {
        let p = t.predicate.as_str();
        // The export side writes exactly these three literal values (0031). Other values are not guessed: they fall through to the predicate report below, and persist as `state`
        let declared_temporal = t
            .object_lit
            .as_ref()
            .map(|(v, _)| v.as_str())
            .filter(|v| p == utopia_temporal && matches!(*v, "state" | "event" | "eternal"));
        let declared_hint = p
            .strip_prefix(UTOPIA_NS)
            .filter(|word| HINT_KEYS.contains(word))
            .and_then(|word| t.object_lit.as_ref().map(|(v, _)| (word, v.as_str())));
        if p == format!("{RDFS}label") {
            if let Some(l) = &t.object_lit {
                labels.entry(t.subject.clone()).or_default().push(l.clone());
            }
        } else if p == format!("{RDFS}comment") {
            if let Some(l) = &t.object_lit {
                comments
                    .entry(t.subject.clone())
                    .or_default()
                    .push(l.clone());
            }
        } else if p == format!("{OWL}disjointWith") {
            // **Both directions are recorded.** `owl:disjointWith` is symmetric, and vocabularies usually write it only once
            // (W3C Org's Role/Membership/Site/ChangeEvent are pairwise disjoint, but only six lines are written).
            // Storing only the written direction means that asking "are A and B disjoint?" depends on which end the caller happens to ask from
            if let Some(o) = &t.object_iri {
                disjoint
                    .entry(t.subject.clone())
                    .or_default()
                    .push(o.clone());
                disjoint
                    .entry(o.clone())
                    .or_default()
                    .push(t.subject.clone());
            }
        } else if p == format!("{OWL}inverseOf") {
            // **Collected only in the written direction.** In OWL, `p owl:inverseOf q` implies the reverse,
            // but vocabularies usually write it only once. Filling in the reverse is done when axioms are read
            // (`reasoning::axioms`, which cannot be bypassed there): doing it here would make
            // "what the ontology itself wrote" indistinguishable from "what we inferred", and R0 has an
            // `inverse_not_mutual` check whose whole purpose is to tell these apart
            if let Some(o) = &t.object_iri {
                inverse_of.entry(t.subject.clone()).or_insert(o.clone());
            }
        } else if is_schema(p, "supersededBy") {
            // The supersession relation appears only in the schema.org vocabulary; several are written, and only the first is kept
            if let Some(o) = &t.object_iri {
                superseded_by.entry(t.subject.clone()).or_insert(o.clone());
            }
        } else if p == format!("{RDFS}subPropertyOf") {
            // Before, this one appeared only in the `known()` allowlist: recognized, not warned about, and **then discarded**.
            // Importing an ontology with subPropertyOf meant that part of the information vanished on the spot, with no warning
            if let Some(o) = &t.object_iri {
                sub_property_of
                    .entry(t.subject.clone())
                    .or_insert(o.clone());
            }
        } else if p == format!("{RDFS}subClassOf") {
            if let Some(o) = &t.object_iri {
                parents
                    .entry(t.subject.clone())
                    .or_default()
                    .push(o.clone());
            }
        } else if p == format!("{RDFS}domain") {
            if let Some(o) = &t.object_iri {
                domains
                    .entry(t.subject.clone())
                    .or_default()
                    .push(o.clone());
            }
        } else if p == format!("{RDFS}range") {
            if let Some(o) = &t.object_iri {
                ranges.entry(t.subject.clone()).or_default().push(o.clone());
            }
        } else if is_schema(p, "domainIncludes") {
            // domainIncludes is a union ("can be used on these types"), and our domain
            // list is already union-semantic (signature `person|organization`), so it goes straight in
            if let Some(o) = &t.object_iri {
                domains
                    .entry(t.subject.clone())
                    .or_default()
                    .push(o.clone());
            }
        } else if is_schema(p, "rangeIncludes") {
            if let Some(o) = &t.object_iri {
                ranges.entry(t.subject.clone()).or_default().push(o.clone());
                union_ranged.insert(t.subject.clone());
            }
        } else if let Some(v) = declared_temporal {
            temporal.entry(t.subject.clone()).or_insert(v.to_string());
        } else if let Some((word, v)) = declared_hint {
            hints
                .entry(t.subject.clone())
                .or_default()
                .entry(word.to_string())
                .or_insert(v.to_string());
        } else if !known(p) {
            // Reported rather than dropped: the preview page must be able to say "what else in this file we do not consume"
            *proj.unprojected.entry(p.to_string()).or_insert(0) += 1;
        }
    }

    // Fills in the domain / range that the shape gives the property itself, which the property did not write (#923). **Fills blanks only**: an explicit
    // `rdfs:domain` / `rdfs:range` (and the two from schema.org) that are stated are not changed by the shape
    let (shape_domains, shape_ranges) = shape_signatures(&triples, &classes, &node_shapes);
    for (p, ds) in shape_domains {
        domains.entry(p).or_insert_with(|| ds.into_iter().collect());
    }
    for (p, rs) in shape_ranges {
        if let std::collections::btree_map::Entry::Vacant(e) = ranges.entry(p) {
            // Constrained to different types on several classes: that is "either will do", the same as rangeIncludes
            if rs.len() > 1 {
                union_ranged.insert(e.key().clone());
            }
            e.insert(rs.into_iter().collect());
        }
    }

    // Datatype closure: the roots are those explicitly `a schema:DataType`; subclasses propagate down
    // along subClassOf (Integer ⊂ Number, URL ⊂ Text).
    // **Which IRIs are datatypes is stated by the file itself**; what is hard-coded is only "which of our four kinds this datatype counts as":
    // that half is a naming convention, and cannot be inferred from RDF
    let mut datatype_classes = datatype_roots.clone();
    loop {
        let grown: Vec<String> = parents
            .iter()
            .filter(|(child, ps)| {
                !datatype_classes.contains(*child)
                    && ps.iter().any(|p| datatype_classes.contains(p))
            })
            .map(|(child, _)| child.clone())
            .collect();
        if grown.is_empty() {
            break;
        }
        datatype_classes.extend(grown);
    }
    for iri in &datatype_classes {
        if let Some(dt) = name_datatype(iri, &parents, &datatype_classes) {
            proj.vocab_datatypes.insert(iri.clone(), dt);
        }
    }

    let home = home_namespace(
        classes
            .iter()
            .chain(obj_props.iter())
            .chain(data_props.iter())
            .map(String::as_str),
    )
    .unwrap_or_default();
    for iri in &classes {
        // **A datatype is not an entity type.** schema:Text is declared as
        // `a rdfs:Class, schema:DataType`; looking only at the first half would build
        // entity types named `text`, `number`, `boolean`.
        if datatype_classes.contains(iri) {
            continue;
        }
        // **A bare foreign declaration is not a class of this vocabulary.** The schema.org dump has
        // `snomed:105590001 a rdfs:Class .`: it is only the object of some owl:equivalentClass,
        // with no label, no parent, and no property pointing to it, and its namespace is not schema.org's.
        // Building it would put eight digits at the very front of the class list (#286). The vocabulary says nothing about it at all,
        // so it is not built; it is recorded as "not projected" so the preview page can say where it went. A bare declaration in this namespace
        // is built: small vocabularies often write only `a owl:Class`, and that is their own class
        let bare = !labels.contains_key(iri)
            && !comments.contains_key(iri)
            && parents.get(iri).is_none_or(|p| p.is_empty())
            && !domains.values().any(|ds| ds.contains(iri))
            && !ranges.values().any(|rs| rs.contains(iri));
        if bare && namespace_of(iri) != home {
            *proj
                .unprojected
                .entry("bare class declared outside the vocabulary's own namespace".to_string())
                .or_insert(0) += 1;
            continue;
        }
        // First take the real label: the key of a numbered IRI must be derived from it (#324).
        // When the vocabulary gives no label, fall back to the local name, which is also the source of the key; behavior is unchanged
        let declared_label = pick_lang(labels.get(iri));
        proj.classes.push(OwlClass {
            key: key_from_iri_or_label(iri, declared_label.as_deref()),
            label: declared_label.unwrap_or_else(|| local_name(iri).to_string()),
            description: pick_lang(comments.get(iri)).unwrap_or_default(),
            parents: parents.get(iri).cloned().unwrap_or_default(),
            disjoint_with: {
                // De-duplicate: after collecting both directions, if the vocabulary wrote it on both sides, it would be duplicated
                let mut d = disjoint.get(iri).cloned().unwrap_or_default();
                d.sort();
                d.dedup();
                d
            },
            display_hints: hints.get(iri).cloned().unwrap_or_default(),
            iri: iri.clone(),
        });
    }
    // **Union, de-duplicated, not chained end to end**: vocabularies often declare the same property both as
    // rdf:Property and as owl:DatatypeProperty (FOAF's name, age, nick... all do this),
    // collecting each set once would make the chain emit it twice. Classifying by data_props is enough.
    let all_props: BTreeSet<&String> = obj_props
        .iter()
        .chain(data_props.iter())
        .chain(plain_props.iter())
        .collect();
    for iri in all_props {
        let rs = ranges.get(iri).cloned().unwrap_or_default();
        // Whether it goes through the attribute channel or the relation channel. The explicit declaration decides; if there is none, look at the range:
        // **it counts as an attribute only if every range is a datatype**. If any member of the union is a class, it goes through the relation channel:
        // `address` has range `PostalAddress|Text`, and treating it as an attribute would lose that edge
        // permanently, whereas a relation can always be given a new entity name. The rich side can fall back;
        // the poor side cannot go back
        let is_datatype = if data_props.contains(iri) {
            true
        } else if obj_props.contains(iri) {
            false
        } else {
            !rs.is_empty() && rs.iter().all(|r| datatype_classes.contains(r))
        };
        let declared_label = pick_lang(labels.get(iri));
        proj.properties.push(OwlProperty {
            key: key_from_iri_or_label(iri, declared_label.as_deref()),
            label: declared_label.unwrap_or_else(|| local_name(iri).replace('_', " ").to_string()),
            description: pick_lang(comments.get(iri)).unwrap_or_default(),
            is_datatype,
            functional: functional.contains(iri),
            inverse_functional: inverse_functional.contains(iri),
            transitive: transitive.contains(iri),
            symmetric: symmetric.contains(iri),
            asymmetric: asymmetric.contains(iri),
            inverse_of: inverse_of.get(iri).cloned(),
            sub_property_of: sub_property_of.get(iri).cloned(),
            superseded_by: superseded_by.get(iri).cloned(),
            irreflexive: irreflexive.contains(iri),
            domains: domains.get(iri).cloned().unwrap_or_default(),
            ranges: rs,
            ranges_union: union_ranged.contains(iri),
            temporal: temporal.get(iri).cloned(),
            display_hints: hints.get(iri).cloned().unwrap_or_default(),
            iri: iri.clone(),
        });
    }
    order_by_home_namespace(&mut proj);
    Ok(proj)
}

/// Property IRI → the domain or range the shape gives it (de-duplicated and ordered, so a re-import gives the same result)
type ShapeSignatures = BTreeMap<String, BTreeSet<String>>;

/// The domain and range that a SHACL shape states (#923).
///
/// `node shape sh:property property shape`: the property shape's `sh:path` says which property it is, and the class the node shape applies to
/// is its domain; `sh:class` / `sh:datatype` on the property shape is its range. When one
/// property constraint sits on several classes, the domain is the union of those classes, the same reading as `domainIncludes`.
///
/// The classes a node shape applies to: `sh:targetClass`, plus SHACL's implicit class target: a node that is both
/// a class and an `sh:NodeShape` applies to itself (UCO writes both). The forms not read each have a reason:
/// - `sh:targetSubjectsOf` / `sh:targetObjectsOf` state "nodes that use some property", not
///   a class;
/// - a path is taken only as a single IRI: inverse and sequence paths are blank nodes, and they do not state this property's own domain;
/// - the alternative types inside `sh:or`, and the shapes `sh:node` points to, are another layer of structure and stay in the source
fn shape_signatures(
    triples: &[Triple],
    classes: &BTreeSet<String>,
    node_shapes: &BTreeSet<String>,
) -> (ShapeSignatures, ShapeSignatures) {
    let mut targets: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for shape in node_shapes.intersection(classes) {
        targets
            .entry(shape.as_str())
            .or_default()
            .insert(shape.as_str());
    }
    // Node shape → its property shapes; property shape → path; property shape → class or datatype
    let mut members: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut paths: BTreeMap<&str, &str> = BTreeMap::new();
    let mut values: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for t in triples {
        let Some(local) = t.predicate.strip_prefix(SH) else {
            continue;
        };
        let object = t.object_iri.as_deref();
        match local {
            "targetClass" => {
                if let Some(class) = object {
                    targets.entry(t.subject.as_str()).or_default().insert(class);
                }
            }
            "property" => {
                if let Some(member) = object.or(t.object_node.as_deref()) {
                    members.entry(t.subject.as_str()).or_default().push(member);
                }
            }
            "path" => {
                if let Some(path) = object {
                    paths.entry(t.subject.as_str()).or_insert(path);
                }
            }
            "class" | "datatype" => {
                if let Some(value) = object {
                    values.entry(t.subject.as_str()).or_default().push(value);
                }
            }
            _ => {}
        }
    }
    let mut domains = ShapeSignatures::new();
    let mut ranges = ShapeSignatures::new();
    for (shape, shape_members) in &members {
        let Some(on) = targets.get(shape) else {
            continue;
        };
        for member in shape_members {
            let Some(property) = paths.get(member) else {
                continue;
            };
            domains
                .entry(property.to_string())
                .or_default()
                .extend(on.iter().map(|c| c.to_string()));
            if let Some(vs) = values.get(member) {
                ranges
                    .entry(property.to_string())
                    .or_default()
                    .extend(vs.iter().map(|v| v.to_string()));
            }
        }
    }
    (domains, ranges)
}

/// Puts **this file's own** vocabulary first.
///
/// On a key collision the caller decides first-come-first-served (the `claimed` in `owl_import::plan`), and "first" came
/// from the lexicographic order of IRIs: so `http://` sorted before `https://`, and the referenced
/// small vocabularies systematically beat the main one. The schema.org file merges 50 namespaces; the main vocabulary declares 94% of its terms,
/// but lost 114 of the 141 collisions:
/// `location` lost to OMG Commons, `country` to unece.org,
/// `organization` to purl.org. The terms lost were exactly the ones most in use.
///
/// The criterion is that **the namespace that declares the most terms is the file's owner**. The name schema.org is not required:
/// any vocabulary qualifies. On a tie, the lexicographically smaller one is taken, so the result is repeatable.
fn order_by_home_namespace(proj: &mut OwlProjection) {
    let Some(home) = home_namespace(
        proj.classes
            .iter()
            .map(|c| c.iri.as_str())
            .chain(proj.properties.iter().map(|p| p.iri.as_str())),
    ) else {
        return;
    };
    // Stable sort: only moves the main vocabulary to the front; everything else keeps its existing lexicographic order
    proj.classes.sort_by_key(|c| namespace_of(&c.iri) != home);
    proj.properties
        .sort_by_key(|p| namespace_of(&p.iri) != home);
}

/// The namespace declared the most among a batch of IRIs, i.e. "the owner of this file".
///
/// max_by_key takes the last maximum, and BTreeMap sorts keys ascending,
/// so on a tie it would get the lexicographically largest. What is wanted is the smallest, so compare by hand
fn home_namespace<'a>(iris: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for iri in iris {
        *counts.entry(namespace_of(iri)).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .fold(None::<(&str, usize)>, |best, (ns, n)| match best {
            Some((_, bn)) if bn >= n => best,
            _ => Some((ns, n)),
        })
        .map(|(ns, _)| ns.to_string())
}

/// The part of an IRI left after removing the local name (including the trailing `#` or `/`).
fn namespace_of(iri: &str) -> &str {
    match iri.rfind(['#', '/']) {
        Some(i) => &iri[..=i],
        None => iri,
    }
}

/// Datatype IRI → one of our four kinds. If the datatype itself can be named, use itself; otherwise follow `rdfs:subClassOf` upward
/// (Integer → Number, URL → Text).
///
/// If it cannot be named, return `None`. For example `schema:Time` has a time of day but no date,
/// and is treated the same as `xsd:time`: [`map_range`] takes [`RangeMapping::Degraded`],
/// built as text and **reported**.
fn name_datatype(
    iri: &str,
    parents: &BTreeMap<String, Vec<String>>,
    datatypes: &BTreeSet<String>,
) -> Option<&'static str> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = vec![iri];
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur) {
            continue;
        }
        if let Some(dt) = well_known_datatype(cur) {
            return Some(dt);
        }
        // Only walk up through datatypes: a datatype's parent may be an ordinary rdfs:Class
        //   (schema:DataType ⊂ rdfs:Class), and crossing that would walk into the whole class hierarchy
        for p in parents.get(cur).into_iter().flatten() {
            if datatypes.contains(p) {
                stack.push(p);
            }
        }
    }
    None
}

/// The half that is hard-coded: the **names** of datatypes, and which of our kinds each maps to.
///
/// This cannot be inferred from RDF: the file can say "Integer is a datatype", but cannot say "it is a number, not a date".
/// The table holds only the roots; subclasses are reached by walking up subClassOf.
///
/// Matching by local name is safe here; it does not conflict with [`datatype_of`]'s rule that matching must use the full IRI,
/// because an IRI that reaches this point is one **this file itself** declared to be a datatype,
/// so a file that says "Date is a datatype" cannot also use Date as an entity class.
fn well_known_datatype(iri: &str) -> Option<&'static str> {
    [
        ("Text", "text"),
        ("Number", "number"),
        ("Date", "date"),
        ("DateTime", "date"),
        ("Boolean", "bool"),
    ]
    .into_iter()
    .find(|(local, _)| is_schema(iri, local))
    .map(|(_, dt)| dt)
}

/// Picks one from multilingual labels: en / zh are preferred, then no language tag, and finally the first one.
fn pick_lang(vals: Option<&Vec<(String, Option<String>)>>) -> Option<String> {
    let vals = vals?;
    for want in ["en", "zh"] {
        if let Some((v, _)) = vals
            .iter()
            .find(|(_, l)| l.as_deref().is_some_and(|l| l.starts_with(want)))
        {
            return Some(v.clone());
        }
    }
    vals.iter()
        .find(|(_, l)| l.is_none())
        .or_else(|| vals.first())
        .map(|(v, _)| v.clone())
}

/// The local name of an IRI: the part after the last `#` or `/`.
pub fn local_name(iri: &str) -> &str {
    iri.rsplit(['#', '/']).next().unwrap_or(iri)
}

/// Whether the local name is **purely numeric**: forms like `BFO_0000144`, `CHEBI_15377`, `GO_0008150`.
///
/// OBO-family vocabularies (over two hundred in the OBO Foundry, covering life sciences, manufacturing, medicine) put the identifier
/// in the IRI and the meaning in `rdfs:label`. A key derived from the IRI, `bfo_0000144`, carries no
/// information, and the key is exactly the class list handed to the model: the model cannot recognize it, so the class never
/// gets any entity, and the import "succeeds" and nothing shows it is broken (#324).
///
/// The criterion is narrow: **a run of letters + one separator + at least three digits**. Names in schema.org that have digits
/// and yet are meaningful all fail it: `gtin12`, `sha256`, `percentile10` have no separator,
/// `emissionsCO2` has its digits not at the end, and `3DModel` is not this shape at all.
fn is_opaque_local_name(local: &str) -> bool {
    let Some(pos) = local.rfind(['_', '-']) else {
        return false;
    };
    let (prefix, digits) = (&local[..pos], &local[pos + 1..]);
    !prefix.is_empty()
        && prefix.chars().all(|c| c.is_ascii_alphabetic())
        && digits.len() >= 3
        && digits.chars().all(|c| c.is_ascii_digit())
}

/// Derived key: if the local name is purely numeric and the vocabulary gives a label, use the label; otherwise use the local name.
/// If the local name cannot yield a key (for example `ex:公司`, which has no ASCII letters at all), use the label as well.
///
/// **The IRI is always the identity.** Only the label shown to the model is replaced here, so re-import, alignment,
/// and the claims on `e_by_iri` are unaffected.
pub fn key_from_iri_or_label(iri: &str, label: Option<&str>) -> String {
    let from_iri = key_from_iri(iri);
    match label {
        Some(l)
            if (from_iri.is_empty() || is_opaque_local_name(local_name(iri)))
                && !l.trim().is_empty() =>
        {
            to_key(l)
        }
        _ => from_iri,
    }
}

/// Derives the key from the IRI. **The IRI is the identity; the key is a label for the model to read** (see 0001 P2):
/// only `[a-z0-9_]` is allowed, at most 40 characters, so the IRI itself cannot go in.
/// Camel case splits into underscores: `hasEmployee` → `has_employee`.
pub fn key_from_iri(iri: &str) -> String {
    to_key(local_name(iri))
}

/// Character rules for turning text into a key: camel case splits words, non-alphanumeric runs become one underscore, truncated at 40.
fn to_key(local: &str) -> String {
    let mut out = String::with_capacity(local.len() + 4);
    let mut prev_lower = false;
    for c in local.chars() {
        if c.is_ascii_uppercase() {
            if prev_lower && !out.is_empty() {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_lower = true;
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
            prev_lower = false;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out.chars().take(40).collect()
}

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

/// The result of mapping `rdfs:range` onto our four datatype kinds. **Three ways, not two.**
#[derive(Debug, Clone, PartialEq)]
pub enum RangeMapping {
    /// Can be mapped to `text` / `number` / `date` / `bool`.
    Datatype(&'static str),
    /// No range at all: the vocabulary made no declaration, and only knows it is a literal.
    /// `text` is an honest superset (it accepts any string and never rejects), so it is built, and listed in the preview.
    Absent,
    /// A range is given and the value is a **short, readable literal**, but our four kinds cannot express it
    /// (`time` has no year, `duration` is a length of time, not a point in time). It is built as `text` and **reported**:
    /// only the sort semantics are lost, the values remain; skipping it would mean this knowledge is never captured at all, which is worse.
    Degraded(String),
    /// A range is given, and **the extractor can never read this value from prose**: binary blocks,
    /// XML fragments, XML-internal identifiers. Skipping it protects no data (there would never be a value anyway);
    /// what it saves is the prompt: each property is one line of the extraction prompt, paid for in every text block.
    Unusable(String),
}

/// `rdfs:range` of a data property → datatype.
///
/// Matches by the **full IRI**, not the local name: a custom vocabulary may well have a class called `date`,
/// and matching by suffix would take it for `xsd:date`.
///
/// Multiple ranges always map to [`RangeMapping::Degraded`]: in RDFS that means an **intersection**
/// ("must be both"), which is almost always a modelling slip, but the spec says so, so it is not guessed.
pub fn map_range(ranges: &[String]) -> RangeMapping {
    resolve_range(ranges, false, &VocabDatatypes::new())
}

/// Resolves by the property's **own** range semantics.
///
/// Several `rdfs:range` entries are an intersection, while several `schema:rangeIncludes` entries are a union; both land in
/// the same `ranges`, and without checking the [`OwlProperty::ranges_union`] bit,
/// `author rangeIncludes Organization, Person` would be read as "must be both an organization and a person".
pub fn map_range_of(p: &OwlProperty, vocab: &VocabDatatypes) -> RangeMapping {
    resolve_range(&p.ranges, p.ranges_union, vocab)
}

fn resolve_range(ranges: &[String], union: bool, vocab: &VocabDatatypes) -> RangeMapping {
    // The vocabulary's own declared datatype is checked first (schema:Text → text); if that fails, fall through to the standard xsd/owl set
    let named = |iri: &String| vocab.get(iri).copied().or_else(|| datatype_of(iri));
    match ranges {
        [] => RangeMapping::Absent,
        [one] => match named(one) {
            Some(dt) => RangeMapping::Datatype(dt),
            None if unusable(one) => RangeMapping::Unusable(one.clone()),
            None => RangeMapping::Degraded(one.clone()),
        },
        many if union => {
            // Union: if all point to the same kind, that kind is used (Text ∪ URL are both text).
            // If they disagree, degrade to text and report: text is the honest upper bound of any union
            let dts: Vec<Option<&'static str>> = many.iter().map(named).collect();
            match dts[0] {
                Some(dt) if dts.iter().all(|d| *d == Some(dt)) => RangeMapping::Datatype(dt),
                _ if many.iter().all(|r| unusable(r)) => RangeMapping::Unusable(many.join(" ∪ ")),
                _ => RangeMapping::Degraded(many.join(" ∪ ")),
            }
        }
        // Multiple rdfs:range entries have intersection semantics; the type is not guessed, but the value is still a literal, so it is built as text
        many => RangeMapping::Degraded(many.join(" ∩ ")),
    }
}

/// Those the extractor could never read from prose: binary blocks and XML internal pipes.
///
/// The criterion is not "should this value be stored" (values exist in the graph anyway, via `facts.object_value`,
/// with evidence, temporality, and review in full). The criterion is **whether a value can ever appear**: "the store opens at 9:00 every day" contains
/// `09:00`, while a base64 floor plan never appears in prose; and even if a base64 blob did appear in a document,
/// extracting it as a fact would be wrong.
///
/// `hexBinary` is not on this list: its name says binary, but what is written is a string of hex digits, and prose carries values
/// like hashes, memory addresses and flag bits ("the file's SHA-256 is 9f86d0..."), so extracting them as facts is correct.
/// Twelve UCO properties are of this kind (review of #1001).
///
/// **Everything else degrades to text**: a value that can be extracted is better kept with a rough type than left with nowhere to go.
fn unusable(iri: &str) -> bool {
    if let Some(local) = iri.strip_prefix(XSD) {
        return matches!(
            local,
            "base64Binary"
                | "QName"
                | "NOTATION"
                | "ID"
                | "IDREF"
                | "IDREFS"
                | "ENTITY"
                | "ENTITIES"
        );
    }
    if let Some(local) = iri.strip_prefix(RDF_NS) {
        return local == "XMLLiteral";
    }
    false
}

fn datatype_of(iri: &str) -> Option<&'static str> {
    if let Some(local) = iri.strip_prefix(XSD) {
        return match local {
            // Bounded and unsigned variants are all collected into number: the difference is the value range, not the meaning
            "decimal" | "integer" | "int" | "long" | "short" | "byte" | "nonNegativeInteger"
            | "positiveInteger" | "nonPositiveInteger" | "negativeInteger" | "unsignedLong"
            | "unsignedInt" | "unsignedShort" | "unsignedByte" | "double" | "float" => {
                Some("number")
            }
            // Our date format is always YYYY[-MM[-DD]], with each level optional, so gYear / gYearMonth fit
            "date" | "dateTime" | "dateTimeStamp" | "gYear" | "gYearMonth" => Some("date"),
            "boolean" => Some("bool"),
            // hexBinary is written as hex digits: hashes, addresses, flag bits; storing it as text as-is loses nothing
            "string" | "normalizedString" | "token" | "language" | "Name" | "NCName"
            | "NMTOKEN" | "anyURI" | "hexBinary" => Some("text"),
            // time / gMonth / gDay / gMonthDay lack a year, and the duration family is a length of time, not a point in time:
            // they fall to None, and unusable() then routes them: they are readable literals, degraded to text;
            // base64 blocks and XML-internal identifiers are the ones truly not collected
            _ => None,
        };
    }
    if let Some(local) = iri.strip_prefix(RDF_NS) {
        // XMLLiteral is an XML fragment, not collected
        return matches!(local, "PlainLiteral" | "langString").then_some("text");
    }
    if let Some(local) = iri.strip_prefix(RDFS) {
        return (local == "Literal").then_some("text");
    }
    if let Some(local) = iri.strip_prefix(OWL) {
        return matches!(local, "real" | "rational").then_some("number");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_maps_every_numeric_variant() {
        for local in [
            "decimal",
            "integer",
            "int",
            "long",
            "short",
            "byte",
            "nonNegativeInteger",
            "positiveInteger",
            "nonPositiveInteger",
            "negativeInteger",
            "unsignedLong",
            "unsignedInt",
            "unsignedShort",
            "unsignedByte",
            "double",
            "float",
        ] {
            assert_eq!(
                map_range(&[format!("{XSD}{local}")]),
                RangeMapping::Datatype("number"),
                "{local}"
            );
        }
        // owl:real / owl:rational are also in OWL 2's datatype map
        assert_eq!(
            map_range(&[format!("{OWL}rational")]),
            RangeMapping::Datatype("number")
        );
    }

    /// Our date format is YYYY[-MM[-DD]], with each level optional, so only g-types missing the low-order parts fit
    #[test]
    fn partial_dates_fit_only_when_the_year_is_there() {
        for local in ["date", "dateTime", "dateTimeStamp", "gYear", "gYearMonth"] {
            assert_eq!(
                map_range(&[format!("{XSD}{local}")]),
                RangeMapping::Datatype("date"),
                "{local}"
            );
        }
        // Those missing a year cannot go into date, but they are readable literals, so they degrade to text instead of being dropped
        for local in ["gMonth", "gDay", "gMonthDay", "time"] {
            assert!(
                matches!(
                    map_range(&[format!("{XSD}{local}")]),
                    RangeMapping::Degraded(_)
                ),
                "{local} 该降级成 text，不该丢"
            );
        }
    }

    /// The dividing line is not "can it be mapped exactly", but **"should this value be in the graph"**.
    /// Anything that can be stored is kept: a rough type beats a piece of knowledge never being captured at all.
    #[test]
    fn a_value_we_can_store_is_kept_even_when_we_cannot_type_it() {
        // No range given: there is no declaration to lose, and text is the honest superset
        assert_eq!(map_range(&[]), RangeMapping::Absent);
        // Given but cannot be expressed: a duration is a readable literal, degraded to text and reported
        assert!(matches!(
            map_range(&[format!("{XSD}duration")]),
            RangeMapping::Degraded(_)
        ));
        // Values written in hex: hashes, addresses, flag bits. It is a value, stored as text as-is
        assert_eq!(
            map_range(&[format!("{XSD}hexBinary")]),
            RangeMapping::Datatype("text")
        );
        // The value should never have been in the graph: base64 blocks, XML fragments and XML-internal identifiers; these are the real skips
        for unusable in [
            format!("{XSD}base64Binary"),
            format!("{RDF_NS}XMLLiteral"),
            format!("{XSD}QName"),
        ] {
            assert!(
                matches!(
                    map_range(std::slice::from_ref(&unusable)),
                    RangeMapping::Unusable(_)
                ),
                "{unusable}"
            );
        }
    }

    /// Multiple ranges in RDFS are an **intersection** ("must be both"), not a union.
    /// It is almost always a modelling slip, but the spec says so: not guessed.
    #[test]
    fn several_ranges_are_an_intersection_we_refuse_to_guess() {
        let m = map_range(&[format!("{XSD}string"), format!("{XSD}integer")]);
        match m {
            // The intersection does not guess a type, but the value is still a literal, so it lands as text
            RangeMapping::Degraded(s) => assert!(s.contains('∩')),
            other => panic!("多条 range 不该被精确映射: {other:?}"),
        }
    }

    /// Matches by full IRI: a **class** named date in a custom vocabulary is not xsd:date
    #[test]
    fn a_class_that_happens_to_be_called_date_is_not_a_date() {
        assert!(matches!(
            map_range(&["http://acme.example/hr#date".into()]),
            RangeMapping::Degraded(_)
        ));
    }

    const TTL: &str = r#"
        @prefix owl: <http://www.w3.org/2002/07/owl#> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
        @prefix ex: <http://acme.example/hr#> .

        ex:Employee a owl:Class ;
            rdfs:label "Employee"@en ;
            rdfs:label "员工"@zh ;
            rdfs:comment "A person on the payroll."@en ;
            rdfs:subClassOf ex:Person .
        ex:Person a owl:Class ; rdfs:label "Person" .
        ex:hasManager a owl:ObjectProperty, owl:FunctionalProperty ;
            rdfs:label "has manager" ;
            rdfs:domain ex:Employee ;
            rdfs:range ex:Person .
        ex:salary a owl:DatatypeProperty ; rdfs:domain ex:Employee .
        ex:Employee owl:disjointWith ex:Contractor .
        ex:Employee owl:equivalentClass ex:Staff .
    "#;

    #[test]
    fn projects_classes_properties_and_reports_the_rest() {
        let p = project(TTL.as_bytes(), RdfFormat::Turtle).unwrap();
        let emp = p.classes.iter().find(|c| c.key == "employee").unwrap();
        // For multilingual labels, en is preferred
        assert_eq!(emp.label, "Employee");
        assert_eq!(emp.description, "A person on the payroll.");
        assert_eq!(emp.parents, vec!["http://acme.example/hr#Person"]);

        let mgr = p
            .properties
            .iter()
            .find(|x| x.key == "has_manager")
            .unwrap();
        assert!(mgr.functional && !mgr.is_datatype);
        assert_eq!(mgr.ranges, vec!["http://acme.example/hr#Person"]);

        let sal = p.properties.iter().find(|x| x.key == "salary").unwrap();
        assert!(sal.is_datatype);

        // Axioms that cannot be consumed go into the report: neither dropped nor an error.
        //
        // `disjointWith` used to be on this list; it is now consumed (it is the basis of the consistency check's
        // decision), so this guard uses an axiom that still cannot be consumed, to protect this property itself
        assert!(p
            .unprojected
            .contains_key("http://www.w3.org/2002/07/owl#equivalentClass"));
    }

    /// schema.org also declares as classes the objects of equivalentClass like `snomed:105590001 a rdfs:Class .`:
    /// no label, no parent, no property, and not in its own namespace. They are not built, but are accounted for;
    /// bare classes in its own namespace are still built
    #[test]
    fn a_bare_class_from_another_namespace_is_reported_not_projected() {
        let ttl = r#"
            @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
            @prefix owl: <http://www.w3.org/2002/07/owl#> .
            @prefix ex: <http://acme.example/hr#> .
            @prefix snomed: <http://purl.bioontology.org/ontology/SNOMEDCT/> .
            ex:Employee a owl:Class ; rdfs:label "Employee" ; owl:equivalentClass snomed:105590001 .
            ex:Person a owl:Class ; rdfs:label "Person" .
            ex:Contractor a owl:Class .
            snomed:105590001 a rdfs:Class .
        "#;
        let p = project(ttl.as_bytes(), RdfFormat::Turtle).unwrap();
        let keys: Vec<&str> = p.classes.iter().map(|c| c.key.as_str()).collect();
        assert!(keys.contains(&"employee") && keys.contains(&"person"));
        assert!(
            keys.contains(&"contractor"),
            "自家的裸声明是自己的类：{keys:?}"
        );
        assert!(
            !keys.iter().any(|k| k.chars().all(|c| c.is_ascii_digit())),
            "{keys:?}"
        );
        assert_eq!(
            p.unprojected
                .get("bare class declared outside the vocabulary's own namespace"),
            Some(&1)
        );
    }

    #[test]
    fn detects_rdfxml_that_opens_with_comments() {
        // The FOAF official file looks like this: dozens of lines of <!-- --> before <rdf:RDF> appears.
        // An earlier version, failing to sniff the XML marker, fell back to Turtle, and failed to parse on the first line
        let head = b"<!-- This is the FOAF vocabulary, expressed using RDFS and OWL. -->\n\
                     <!-- padding padding padding padding padding padding padding -->\n";
        assert_eq!(RdfFormat::detect("index.rdf", head), RdfFormat::RdfXml);
        // The other way round: Turtle inside a .owl is also common, so content decides
        assert_eq!(
            RdfFormat::detect("x.owl", b"@prefix owl: <http://x#> .\n"),
            RdfFormat::Turtle
        );
        // Turtle that starts with a comment is recognized too (# lines are skipped first)
        assert_eq!(
            RdfFormat::detect("x", b"# a note\n\n@base <http://x> .\n"),
            RdfFormat::Turtle
        );
    }

    #[test]
    fn key_derives_from_the_iri_local_name() {
        assert_eq!(key_from_iri("http://x/hr#hasEmployee"), "has_employee");
        assert_eq!(key_from_iri("http://x/ns/Person"), "person");
        assert_eq!(key_from_iri("http://x#HTTP_Server"), "http_server");
    }

    /// The key of numbered IRIs comes from the label instead (#324). These vocabularies put the identifier in the IRI
    /// and the meaning in the label; handing the model a key derived from the IRI is no help at all.
    #[test]
    fn an_opaque_iri_takes_its_key_from_the_label() {
        let k = |iri, label| key_from_iri_or_label(iri, Some(label));
        assert_eq!(
            k(
                "http://purl.obolibrary.org/obo/BFO_0000144",
                "process profile"
            ),
            "process_profile"
        );
        assert_eq!(
            k("http://purl.obolibrary.org/obo/CHEBI_15377", "water"),
            "water"
        );
        assert_eq!(
            k(
                "http://purl.obolibrary.org/obo/GO_0008150",
                "biological process"
            ),
            "biological_process"
        );
        // With no label there is no choice: fall back to the IRI. Better than nothing, and identity stays in the IRI
        assert_eq!(
            key_from_iri_or_label("http://purl.obolibrary.org/obo/BFO_0000144", None),
            "bfo_0000144"
        );
        // An empty label does not count either
        assert_eq!(
            k("http://purl.obolibrary.org/obo/BFO_0000144", "   "),
            "bfo_0000144"
        );
    }

    /// The criterion must be narrow. Names with digits in schema.org are all meaningful, and none may be rewritten:
    /// most classes have a label, so a wide criterion would rewrite the keys of the whole vocabulary.
    #[test]
    fn a_meaningful_name_keeps_its_key_even_with_digits() {
        let k = |iri, label| key_from_iri_or_label(iri, Some(label));
        // Digits right after letters, with no separator
        assert_eq!(k("https://schema.org/gtin12", "GTIN-12"), "gtin12");
        assert_eq!(k("https://schema.org/sha256", "sha256"), "sha256");
        assert_eq!(
            k("https://schema.org/percentile10", "percentile 10"),
            "percentile10"
        );
        // Digits not at the end
        assert_eq!(
            k("https://schema.org/emissionsCO2", "emissions CO2"),
            "emissions_co2"
        );
        // The character after the separator is not a digit
        assert_eq!(k("http://x#HTTP_Server", "HTTP server"), "http_server");
        // Fewer than three digits: may be a meaningful identifier (v2, Q1), so it is not treated as a number
        assert_eq!(k("http://x#part_12", "part twelve"), "part_12");
    }

    /// An IRI written in Chinese cannot yield a key: if there is an English label, use it; otherwise every class gets an empty key and the whole import is rejected
    #[test]
    fn a_chinese_iri_takes_its_key_from_the_label() {
        let ttl = r#"
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix ex: <http://example.cn/onto#> .

ex:公司 a owl:Class ; rdfs:label "Company"@en, "公司"@zh .
ex:人员 a owl:Class ; rdfs:label "Person"@en, "人员"@zh .
ex:注册资本 a owl:DatatypeProperty ; rdfs:label "registered capital"@en ;
    rdfs:domain ex:公司 ; rdfs:range xsd:decimal .
"#;
        let p = project(ttl.as_bytes(), RdfFormat::Turtle).unwrap();
        let mut keys: Vec<&str> = p.classes.iter().map(|c| c.key.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["company", "person"]);
        assert_eq!(p.properties[0].key, "registered_capital");
        // Those with no label still fall back to the local name
        assert_eq!(key_from_iri_or_label("http://x#Company", None), "company");
    }

    /// A minimal reproduction of the schema.org scheme: datatypes declare themselves, and properties use
    /// domainIncludes / rangeIncludes, all declared only as rdf:Property.
    const SCHEMA_ISH: &str = r#"
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix schema: <https://schema.org/> .

schema:DataType a rdfs:Class .
schema:Text a rdfs:Class, schema:DataType .
schema:Number a rdfs:Class, schema:DataType .
schema:Date a rdfs:Class, schema:DataType .
schema:Time a rdfs:Class, schema:DataType .
schema:URL a rdfs:Class ; rdfs:subClassOf schema:Text .
schema:Integer a rdfs:Class ; rdfs:subClassOf schema:Number .

schema:Organization a rdfs:Class ; rdfs:label "Organization" .
schema:Person a rdfs:Class ; rdfs:label "Person" .
schema:PostalAddress a rdfs:Class ; rdfs:label "PostalAddress" .

schema:foundingDate a rdf:Property ;
    schema:domainIncludes schema:Organization ;
    schema:rangeIncludes schema:Date .
schema:author a rdf:Property ;
    schema:domainIncludes schema:Organization ;
    schema:rangeIncludes schema:Organization, schema:Person .
schema:address a rdf:Property ;
    schema:domainIncludes schema:Organization ;
    schema:rangeIncludes schema:PostalAddress, schema:Text .
schema:homepage a rdf:Property ;
    schema:domainIncludes schema:Person ;
    schema:rangeIncludes schema:Text, schema:URL .
schema:opens a rdf:Property ;
    schema:domainIncludes schema:Organization ;
    schema:rangeIncludes schema:Time .
schema:knows a rdf:Property ;
    schema:domainIncludes schema:Person .
"#;

    fn schema_ish() -> OwlProjection {
        project(SCHEMA_ISH.as_bytes(), RdfFormat::Turtle).unwrap()
    }

    fn prop<'a>(p: &'a OwlProjection, key: &str) -> &'a OwlProperty {
        p.properties.iter().find(|x| x.key == key).unwrap()
    }

    #[test]
    fn schema_org_datatypes_are_not_entity_types() {
        let p = schema_ish();
        let keys: Vec<&str> = p.classes.iter().map(|c| c.key.as_str()).collect();
        // Text / Number / Date / Time are declared as `a rdfs:Class, schema:DataType`,
        // so looking only at the first half would build entity types named text and number
        for gone in [
            "text",
            "number",
            "date",
            "time",
            "url",
            "integer",
            "data_type",
        ] {
            assert!(!keys.contains(&gone), "{gone} 不该是实体类型：{keys:?}");
        }
        assert!(keys.contains(&"organization") && keys.contains(&"person"));
    }

    #[test]
    fn domain_includes_feeds_the_signature() {
        let p = schema_ish();
        // Without recognizing schema:domainIncludes this is empty, and the signature becomes (* → *)
        assert_eq!(
            prop(&p, "founding_date").domains,
            vec!["https://schema.org/Organization".to_string()]
        );
    }

    #[test]
    fn a_union_range_of_classes_stays_a_relation() {
        let p = schema_ish();
        // rangeIncludes Organization, Person: a union, both are classes
        assert!(!prop(&p, "author").is_datatype);
        // rangeIncludes PostalAddress, Text: the common schema.org form,
        // where Text means "a string, because I'm too lazy to build an entity". Treating it as an attribute would permanently lose that edge
        assert!(!prop(&p, "address").is_datatype);
    }

    #[test]
    fn a_union_range_of_datatypes_becomes_an_attribute() {
        let p = schema_ish();
        let fd = prop(&p, "founding_date");
        assert!(fd.is_datatype);
        assert_eq!(
            map_range_of(fd, &p.vocab_datatypes),
            RangeMapping::Datatype("date")
        );
        // Text ∪ URL: URL ⊂ Text, both resolve to text, so there is no need to degrade or report
        let hp = prop(&p, "homepage");
        assert!(hp.is_datatype);
        assert_eq!(
            map_range_of(hp, &p.vocab_datatypes),
            RangeMapping::Datatype("text")
        );
    }

    #[test]
    fn a_union_is_not_an_intersection() {
        let p = schema_ish();
        // This is the easiest step to get wrong: after two ranges are poured into the same Vec,
        // without checking ranges_union they would be taken with rdfs:range's intersection semantics
        assert!(prop(&p, "author").ranges_union);
        assert_eq!(prop(&p, "author").ranges.len(), 2);
        // Whereas two rdfs:range entries are still an intersection
        assert!(matches!(
            map_range(&[format!("{XSD}string"), format!("{XSD}integer")]),
            RangeMapping::Degraded(ref s) if s.contains('∩')
        ));
    }

    #[test]
    fn an_unnamed_datatype_degrades_and_is_reported() {
        let p = schema_ish();
        // schema:Time has a time of day but no date, the same treatment as xsd:time: it is a datatype
        //(so it goes through the attribute channel, not as an entity type), but it cannot be named, so it is built as text and **reported**
        let o = prop(&p, "opens");
        assert!(o.is_datatype);
        assert!(matches!(
            map_range_of(o, &p.vocab_datatypes),
            RangeMapping::Degraded(ref s) if s.ends_with("Time")
        ));
    }

    #[test]
    fn a_bare_rdf_property_without_range_is_still_a_relation() {
        let p = schema_ish();
        // No range means no basis for judgment, and the fallback is still relation: far more values are IRIs than literals
        let k = prop(&p, "knows");
        assert!(!k.is_datatype);
        assert!(k.ranges.is_empty());
    }

    #[test]
    fn schema_predicates_are_consumed_not_reported_as_unprojected() {
        let p = schema_ish();
        // Once recognized, it should not show up under "not yet projected", or the preview page would say
        // "2312 domainIncludes entries are still unconsumed", when in fact they were consumed
        for consumed in ["domainIncludes", "rangeIncludes"] {
            assert!(
                !p.unprojected.keys().any(|k| k.ends_with(consumed)),
                "{consumed} 应已消费：{:?}",
                p.unprojected
            );
        }
    }

    /// One file merges two vocabularies, with the main vocabulary's IRIs under https and the referenced ones under http:
    /// in lexicographic order http comes first, so the main vocabulary loses. This is the shape of the schema.org file.
    const TWO_VOCABS: &str = r#"
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix home: <https://home.example/> .
@prefix cited: <http://cited.example/> .

home:Location a rdfs:Class ; rdfs:label "Location" .
home:Country a rdfs:Class ; rdfs:label "Country" .
home:Person a rdfs:Class ; rdfs:label "Person" .
home:Organization a rdfs:Class ; rdfs:label "Organization" .
cited:Location a rdfs:Class ; rdfs:label "Location (cited)" .

home:worksAt a rdf:Property ; rdfs:label "worksAt" .
home:knows a rdf:Property ; rdfs:label "knows" .
cited:worksAt a rdf:Property ; rdfs:label "worksAt (cited)" .
"#;

    #[test]
    fn the_files_own_vocabulary_wins_a_key_collision() {
        let p = project(TWO_VOCABS.as_bytes(), RdfFormat::Turtle).unwrap();
        // A collision is decided first-come-first-served by the caller, so the order is the decision.
        // The main vocabulary declares the most, so it must come first, otherwise `http://` < `https://`
        // in this lexicographic order would let the referenced vocabularies take location, country, organization
        let first_location = p.classes.iter().find(|c| c.key == "location").unwrap();
        assert!(
            first_location.iri.starts_with("https://home.example/"),
            "输给了 {}",
            first_location.iri
        );
        let first_works = p.properties.iter().find(|x| x.key == "works_at").unwrap();
        assert!(first_works.iri.starts_with("https://home.example/"));
    }
}

#[cfg(test)]
mod axioms {
    use super::*;

    /// OWL property axioms and class disjointness must both be projected; they are the **decision basis** for the consistency check (0002 R0).
    /// Without them, there is no way to tell whether `A part_of B` and `B part_of A` holding together is a contradiction or
    /// normal: `alias_of` in both directions is right, while `produces` in both directions is almost certainly wrong,
    /// and only the ontology can tell the two apart.
    const AX: &str = r#"
        @prefix owl: <http://www.w3.org/2002/07/owl#> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
        @prefix ex: <http://acme.example/ax#> .

        ex:Person a owl:Class ; owl:disjointWith ex:Organization .
        ex:Organization a owl:Class .
        ex:Document a owl:Class .

        ex:partOf   a owl:ObjectProperty, owl:TransitiveProperty, owl:AsymmetricProperty .
        ex:aliasOf  a owl:ObjectProperty, owl:SymmetricProperty .
        ex:reportsTo a owl:ObjectProperty, owl:IrreflexiveProperty .
        ex:plain    a owl:ObjectProperty .
    "#;

    fn proj() -> OwlProjection {
        project(AX.as_bytes(), RdfFormat::Turtle).unwrap()
    }

    #[test]
    fn property_axioms_survive_the_projection() {
        let p = proj();
        let by = |k: &str| p.properties.iter().find(|x| x.key == k).unwrap().clone();

        let part_of = by("part_of");
        assert!(part_of.transitive, "TransitiveProperty 要落到属性上");
        assert!(part_of.asymmetric, "AsymmetricProperty 同上");
        assert!(!part_of.symmetric);

        assert!(
            by("alias_of").symmetric,
            "对称属性双向出现是正确的，不是矛盾"
        );
        assert!(by("reports_to").irreflexive, "非自反：自己汇报给自己是矛盾");

        // Anything not declared is false: **the default is not "unknown" but "no such axiom"**.
        // OWL is open-world, but the consistency check can only judge by what is written: if nothing is written, there is no basis,
        // and reporting no contradiction without a basis is safer than guessing an axiom into existence
        let plain = by("plain");
        assert!(!plain.transitive && !plain.symmetric);
        assert!(!plain.asymmetric && !plain.irreflexive);
    }

    /// **Both directions must be present.** Vocabularies usually write it once (W3C Org writes the pairwise disjointness of four classes as six lines),
    /// so storing only the written direction makes "are A and B disjoint?" depend on which end the caller happens to ask from.
    #[test]
    fn disjointness_is_recorded_from_both_ends() {
        let p = proj();
        let d = |k: &str| {
            p.classes
                .iter()
                .find(|c| c.key == k)
                .unwrap()
                .disjoint_with
                .clone()
        };
        assert_eq!(d("person"), vec!["http://acme.example/ax#Organization"]);
        assert_eq!(d("organization"), vec!["http://acme.example/ax#Person"]);
        assert!(d("document").is_empty(), "没声明互斥的类不该凭空多出来");
    }
}

/// Verify against the **real packs**, not just the fixtures.
///
/// Precedent is in `pack_alignment::against_real_packs`: that version caught four IRIs written wrongly from memory
/// using the real packs. A fixture can only prove "the Turtle I wrote parses under my own parser"; the real packs prove
/// "the forms the official files use are handled". The two are not the same thing.
#[cfg(test)]
mod against_real_packs {
    use super::*;
    use std::io::Read;

    fn load(name: &str) -> Vec<u8> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../utopia-server/packs/");
        let f = std::fs::File::open(format!("{path}{name}")).expect("包文件在");
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(f)
            .read_to_end(&mut out)
            .expect("解压");
        out
    }

    /// W3C Org makes Organization / Role / Membership / Site / ChangeEvent pairwise disjoint;
    /// the official file writes each pair only once. **Each of the five classes must see all four counterparts**: if only the written
    /// direction is stored, the ones written first would be missing counterparts, and which ones depends on line order in the file.
    ///
    /// (When writing this assertion I counted "four" by eye from grep; the real pack corrected it on the spot: `Organization`
    /// is also in the disjoint set. A fixture cannot prove that kind of thing.)
    #[test]
    fn w3c_org_declares_four_mutually_disjoint_classes() {
        let p = project(&load("w3c-org.ttl.gz"), RdfFormat::Turtle).unwrap();
        for key in ["organization", "role", "membership", "site", "change_event"] {
            let c = p
                .classes
                .iter()
                .find(|c| c.key == key)
                .unwrap_or_else(|| panic!("{key} 该被投影出来"));
            assert_eq!(
                c.disjoint_with.len(),
                4,
                "{key} 该与另外四个互斥,实得 {:?}",
                c.disjoint_with
            );
        }
    }

    /// IOF Core references BFO, and BFO's IRIs are numbered (`.../obo/BFO_0000144`).
    /// Handing the model `bfo_0000144`, derived from the IRI, is no help: the class would stay empty forever,
    /// while the import "succeeds" (#324). What is pinned here is **the one in the real pack**:
    /// its `rdfs:label` says "process profile".
    ///
    /// It also pins the opposite: none of schema.org's digit-bearing but meaningful keys may be rewritten.
    /// With a wide criterion, the affected thing is not one class but the whole vocabulary: most of its classes have a label.
    #[test]
    fn an_opaque_iri_in_a_real_pack_reads_from_its_label() {
        let p = project(&load("iof-core.rdf.gz"), RdfFormat::RdfXml).unwrap();
        let c = p
            .classes
            .iter()
            .find(|c| c.label == "process profile")
            .expect("IOF Core 里那个 BFO 类该被投影出来");
        assert_eq!(c.key, "process_profile", "编号式 IRI 该改用 label 派生 key");
        assert!(
            !p.classes.iter().any(|c| c.key.starts_with("bfo_0")),
            "不该再有编号形态的 key，实得 {:?}",
            p.classes
                .iter()
                .map(|c| &c.key)
                .filter(|k| k.starts_with("bfo_0"))
                .collect::<Vec<_>>()
        );

        let s = project(&load("schema-org.ttl.gz"), RdfFormat::Turtle).unwrap();
        for key in ["gtin12", "gtin13", "sha256", "emissions_co2"] {
            assert!(
                s.properties.iter().any(|x| x.key == key),
                "{key} 是有意义的名字，不该被当成编号改写"
            );
        }
    }

    /// IOF Core declares a batch of transitive properties (before/after/occursDuring...).
    /// This is the only place R0 cycle detection has a real basis
    #[test]
    fn iof_core_declares_transitive_properties() {
        let p = project(&load("iof-core.rdf.gz"), RdfFormat::RdfXml).unwrap();
        let n = p.properties.iter().filter(|x| x.transitive).count();
        assert!(n >= 8, "IOF 该有一批传递属性,实得 {n}");
    }

    /// FOAF's Person ⊥ Organization / Document: **the pair that collides most often**.
    /// The hand-written table in `classify_type_drift` (person vs organization judged Disjoint)
    /// is meant to express it, and the comment there also says that once axioms are persisted, this table should be read from the ontology.
    #[test]
    fn foaf_says_a_person_is_not_an_organization() {
        let p = project(&load("foaf.rdf.gz"), RdfFormat::RdfXml).unwrap();
        let person = p
            .classes
            .iter()
            .find(|c| c.key == "person")
            .expect("foaf:Person");
        assert!(
            person
                .disjoint_with
                .iter()
                .any(|d| d.ends_with("Organization")),
            "foaf:Person 该与 Organization 互斥,实得 {:?}",
            person.disjoint_with
        );
    }
}

#[cfg(test)]
mod property_axiom_tests {
    use super::*;

    /// `schema:supersededBy` must be read out: a property that gives way carries whom it gave way to
    #[test]
    fn a_superseded_property_names_its_successor() {
        let ttl = r#"
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix schema: <https://schema.org/> .
schema:Organization a rdfs:Class ; rdfs:label "Organization" .
schema:Person a rdfs:Class ; rdfs:label "Person" .
schema:employee a rdf:Property ; rdfs:label "employee" ;
    schema:domainIncludes schema:Organization ; schema:rangeIncludes schema:Person .
schema:employees a rdf:Property ; rdfs:label "employees" ;
    schema:domainIncludes schema:Organization ; schema:rangeIncludes schema:Person ;
    schema:supersededBy schema:employee .
"#;
        let p = project(ttl.as_bytes(), RdfFormat::Turtle).expect("解析");
        let by = |k: &str| {
            p.properties
                .iter()
                .find(|x| x.key == k)
                .unwrap_or_else(|| panic!("没有 {k}"))
        };
        assert_eq!(
            by("employees").superseded_by.as_deref(),
            Some("https://schema.org/employee")
        );
        assert!(by("employee").superseded_by.is_none());
        assert!(
            !p.unprojected.keys().any(|k| k.contains("supersededBy")),
            "认了就不该再算进「暂未投影」"
        );
    }

    /// `owl:inverseOf` and `rdfs:subPropertyOf` must be read out.
    ///
    /// Before, `subPropertyOf` appeared only in the `known()` allowlist: recognized, not warned about,
    /// **then discarded**; `inverseOf` was not even in the allowlist and was counted as "not yet projected".
    /// In both cases, importing an ontology made that information vanish on the spot.
    #[test]
    fn the_two_property_relations_survive_projection() {
        let ttl = include_str!("../tests/inverse_and_sub.ttl");
        let p = project(ttl.as_bytes(), RdfFormat::Turtle).expect("解析");
        let by = |k: &str| {
            p.properties
                .iter()
                .find(|x| x.key == k)
                .unwrap_or_else(|| panic!("没有 {k}"))
        };
        assert_eq!(
            by("employs").inverse_of.as_deref(),
            Some("http://example.org/worksAt"),
            "**逆要按写的方向收**——补反向是读取公理时的事"
        );
        assert_eq!(
            by("ceo_of").sub_property_of.as_deref(),
            Some("http://example.org/worksAt")
        );
        // If the reverse direction is not written it should be empty: what the ontology wrote and what we inferred must stay distinct,
        // and R0's inverse_not_mutual check relies on this distinction
        assert!(
            by("works_at").inverse_of.is_none(),
            "没写的方向不该在导入时被补上"
        );
        assert!(
            !p.unprojected.keys().any(|k| k.contains("inverseOf")),
            "**认了就不该再算进「暂未投影」**"
        );
    }

    /// `utopia:temporal` must be read out (0031). The export side has always written it; the import side did not read it before:
    /// an ontology that says "this relation is a moment in time" was imported as `state`, and the #1242 rule
    /// "a non-state relation can be bound without the model" never held for it.
    #[test]
    fn a_property_annotated_with_utopia_temporal_keeps_the_value_it_declared() {
        let ttl = r#"
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix utopia: <urn:utopia:ns:> .
@prefix ex: <http://example.org/> .

ex:Person a owl:Class .
ex:attended a owl:ObjectProperty ; rdfs:label "attended" ; utopia:temporal "event" .
ex:born_of a owl:ObjectProperty ; rdfs:label "born of" ; utopia:temporal "eternal" .
ex:works_at a owl:ObjectProperty ; rdfs:label "works at" .
ex:met a owl:ObjectProperty ; rdfs:label "met" ; utopia:temporal "sometimes" .
"#;
        let p = project(ttl.as_bytes(), RdfFormat::Turtle).expect("解析");
        let by = |k: &str| {
            p.properties
                .iter()
                .find(|x| x.key == k)
                .unwrap_or_else(|| panic!("没有 {k}"))
        };
        assert_eq!(by("attended").temporal.as_deref(), Some("event"));
        assert_eq!(by("born_of").temporal.as_deref(), Some("eternal"));
        assert_eq!(
            by("works_at").temporal,
            None,
            "没写就是没写，落库再按 state"
        );
        // Values outside the three are not guessed: left empty and reported by predicate; with only the `met` entry, the preview page can say why it did not come in
        assert_eq!(by("met").temporal, None);
        assert_eq!(p.unprojected.get("urn:utopia:ns:temporal"), Some(&1));
    }

    const HINTED: &str = r#"
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix utopia: <urn:utopia:ns:> .
@prefix ex: <http://example.org/> .

ex:Claim a owl:Class ; rdfs:label "claim" ; utopia:panel "quote" ; utopia:chain_first "out:checked_by" .
ex:Plain a owl:Class ; rdfs:label "plain" .
ex:outcome a owl:DatatypeProperty ; rdfs:label "outcome" ; rdfs:domain ex:Claim ;
    utopia:role "status" ; utopia:tones "passed=ok,failed=danger" .
ex:lines a owl:DatatypeProperty ; rdfs:label "lines" ; rdfs:domain ex:Claim ; utopia:role "range" .
ex:checked_by a owl:ObjectProperty ; rdfs:label "checked by" ; rdfs:domain ex:Claim ; rdfs:range ex:Plain ;
    utopia:chain_next "in:changes" ; utopia:colour "red" .
ex:twice a owl:ObjectProperty ; rdfs:label "twice" ; utopia:role "children" ; utopia:role "membership" .
"#;

    fn hinted() -> OwlProjection {
        project(HINTED.as_bytes(), RdfFormat::Turtle).expect("解析")
    }

    fn hints_of(p: &OwlProjection, key: &str) -> BTreeMap<String, String> {
        p.classes
            .iter()
            .find(|c| c.key == key)
            .map(|c| c.display_hints.clone())
            .or_else(|| {
                p.properties
                    .iter()
                    .find(|x| x.key == key)
                    .map(|x| x.display_hints.clone())
            })
            .unwrap_or_else(|| panic!("没有 {key}"))
    }

    fn pairs(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// A class says how its panel looks and where its chain starts (RC-008)
    #[test]
    fn a_class_annotated_with_utopia_panel_keeps_the_hint() {
        let p = hinted();
        assert_eq!(
            hints_of(&p, "claim"),
            pairs(&[("panel", "quote"), ("chain_first", "out:checked_by")])
        );
        assert!(
            hints_of(&p, "plain").is_empty(),
            "nothing declared, nothing kept"
        );
    }

    #[test]
    fn a_property_annotated_with_utopia_role_and_tones_keeps_both() {
        let p = hinted();
        assert_eq!(
            hints_of(&p, "outcome"),
            pairs(&[("role", "status"), ("tones", "passed=ok,failed=danger")])
        );
        assert_eq!(
            hints_of(&p, "checked_by"),
            pairs(&[("chain_next", "in:changes")])
        );
    }

    #[test]
    fn a_qualifier_attribute_annotated_with_utopia_role_range_keeps_the_hint() {
        let p = hinted();
        assert_eq!(hints_of(&p, "lines"), pairs(&[("role", "range")]));
    }

    /// `utopia:colour` is not a word this record knows: it is neither kept nor lost
    /// silently, the preview reports it under its own predicate
    #[test]
    fn an_unknown_utopia_annotation_is_reported_as_unprojected_and_not_stored() {
        let p = hinted();
        assert!(!hints_of(&p, "checked_by").contains_key("colour"));
        assert_eq!(p.unprojected.get("urn:utopia:ns:colour"), Some(&1));
        assert!(
            !p.unprojected
                .keys()
                .any(|k| k.ends_with(":role") || k.ends_with(":panel")),
            "a kept hint is not reported as unprojected"
        );
    }

    #[test]
    fn a_hint_given_twice_keeps_the_first_value() {
        let p = hinted();
        assert_eq!(hints_of(&p, "twice"), pairs(&[("role", "children")]));
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;

    /// SHACL shape domain / range (#923), one place per form:
    /// - `Account`: UCO's form, where the class is itself the shape, with an explicit `sh:targetClass` pointing at itself, and the property shapes
    ///   are blank nodes, one with `sh:class`, one with `sh:datatype`;
    /// - `Identity`: just a class plus `sh:NodeShape`, without `sh:targetClass` (implicit class target);
    /// - `DeviceShape`: a named node shape, referencing named property shapes;
    /// - `Tool`: `owner` is constrained again on a second class, with a different class as its value; `version` declares its own
    ///   domain and range; inverse paths and `sh:or` are not read; `undeclared` is not declared as a property in the file;
    /// - `Usage`: `sh:targetSubjectsOf` is not a class.
    const SHAPES: &str = r#"
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix ex: <https://ex.example/> .

ex:Account a owl:Class, sh:NodeShape ;
    sh:property [ sh:path ex:owner ; sh:class ex:Identity ; sh:maxCount 1 ] ,
                [ sh:path ex:accountType ; sh:datatype xsd:string ] ;
    sh:targetClass ex:Account .
ex:Identity a owl:Class, sh:NodeShape ;
    sh:property [ sh:path ex:givenName ; sh:datatype xsd:string ] .
ex:Device a owl:Class .
ex:DeviceShape a sh:NodeShape ;
    sh:targetClass ex:Device ;
    sh:property ex:serialShape .
ex:serialShape sh:path ex:serial ; sh:datatype xsd:string .
ex:Organization a owl:Class .
ex:Tool a owl:Class, sh:NodeShape ;
    sh:targetClass ex:Tool ;
    sh:property [ sh:path ex:owner ; sh:class ex:Organization ] ,
                [ sh:path ex:version ; sh:datatype xsd:string ] ,
                [ sh:path [ sh:inversePath ex:uses ] ; sh:class ex:Identity ] ,
                [ sh:path ex:vendor ; sh:or ( [ sh:class ex:Identity ] [ sh:class ex:Organization ] ) ] ,
                [ sh:path ex:undeclared ; sh:datatype xsd:string ] .
ex:Usage a sh:NodeShape ;
    sh:targetSubjectsOf ex:uses ;
    sh:property [ sh:path ex:uses ; sh:class ex:Tool ] .

ex:owner a owl:ObjectProperty .
ex:uses a owl:ObjectProperty .
ex:vendor a owl:ObjectProperty .
ex:accountType a owl:DatatypeProperty .
ex:givenName a owl:DatatypeProperty .
ex:serial a owl:DatatypeProperty .
ex:version a owl:DatatypeProperty ; rdfs:domain ex:Device ; rdfs:range xsd:integer .
"#;

    fn by_iri<'a>(p: &'a OwlProjection, local: &str) -> &'a OwlProperty {
        let iri = format!("https://ex.example/{local}");
        p.properties
            .iter()
            .find(|x| x.iri == iri)
            .unwrap_or_else(|| panic!("{iri}"))
    }

    fn ex(locals: &[&str]) -> Vec<String> {
        locals
            .iter()
            .map(|l| format!("https://ex.example/{l}"))
            .collect()
    }

    #[test]
    fn a_shape_gives_a_property_its_domain_and_range() {
        let p = project(SHAPES.as_bytes(), RdfFormat::Turtle).unwrap();
        // The class is itself the shape, with an explicit targetClass: one object property, one data property
        let t = by_iri(&p, "accountType");
        assert_eq!(
            (t.domains.clone(), t.ranges.clone()),
            (ex(&["Account"]), vec![format!("{XSD}string")])
        );
        assert!(!t.ranges_union);
        // Implicit class target: class plus sh:NodeShape, no targetClass written
        assert_eq!(by_iri(&p, "givenName").domains, ex(&["Identity"]));
        // A named node shape references named property shapes
        let s = by_iri(&p, "serial");
        assert_eq!(
            (s.domains.clone(), s.ranges.clone()),
            (ex(&["Device"]), vec![format!("{XSD}string")])
        );
        // Constrained on two classes: both are domains, and range is "either will do"
        let o = by_iri(&p, "owner");
        assert_eq!(o.domains, ex(&["Account", "Tool"]));
        assert_eq!(o.ranges, ex(&["Identity", "Organization"]));
        assert!(
            o.ranges_union,
            "different classes on different shapes are alternatives"
        );
    }

    #[test]
    fn a_declared_domain_and_range_win_over_a_shape() {
        let p = project(SHAPES.as_bytes(), RdfFormat::Turtle).unwrap();
        let v = by_iri(&p, "version");
        assert_eq!(v.domains, ex(&["Device"]), "not the shape's Tool");
        assert_eq!(
            v.ranges,
            vec![format!("{XSD}integer")],
            "not the shape's string"
        );
    }

    #[test]
    fn a_shape_reads_no_other_target_path_or_alternative() {
        let p = project(SHAPES.as_bytes(), RdfFormat::Turtle).unwrap();
        // An inverse path does not state uses' own domain; targetSubjectsOf is not a class
        let u = by_iri(&p, "uses");
        assert!(u.domains.is_empty() && u.ranges.is_empty(), "{u:?}");
        // The alternative types inside sh:or are not read; the path itself is read
        let v = by_iri(&p, "vendor");
        assert_eq!(v.domains, ex(&["Tool"]));
        assert!(v.ranges.is_empty(), "{v:?}");
        // A property the shape points at is built only if the file declares it
        assert!(!p.properties.iter().any(|x| x.iri.ends_with("undeclared")));
    }

    #[test]
    fn shape_predicates_are_consumed_not_reported_as_unprojected() {
        let p = project(SHAPES.as_bytes(), RdfFormat::Turtle).unwrap();
        for consumed in ["targetClass", "property", "path", "class", "datatype"] {
            assert!(
                !p.unprojected
                    .keys()
                    .any(|k| k == &format!("{SH}{consumed}")),
                "sh:{consumed} is read: {:?}",
                p.unprojected
            );
        }
        // Those not read are reported as before
        assert!(
            p.unprojected.contains_key(&format!("{SH}targetSubjectsOf")),
            "{:?}",
            p.unprojected
        );
        // The triple count still counts only subjects that are IRIs, as before: blank-node triples left for property shapes do not count
        let iri_subjects = oxttl::TurtleParser::new()
            .for_reader(SHAPES.as_bytes())
            .map(Result::unwrap)
            .filter(|t| matches!(t.subject, oxrdf::NamedOrBlankNode::NamedNode(_)))
            .count();
        assert_eq!(p.triples, iri_subjects);
    }
}
