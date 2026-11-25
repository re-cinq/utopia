//! Ledger to RDF (0020).
//!
//! The reader is not another Utopia; it is someone holding a triple store who asks "what is the basis for this conclusion".
//! So what is exported is **validity intervals and provenance**, not just the edges that hold right now: a graph that reads clean and confident
//! but never mentions that half of it was retracted in March is worse than no export.
//!
//! Three things fix the shape of this file:
//!
//! 1. **Classes and relations imported from elsewhere keep their original IRIs** (`entity_types.iri` / `relation_types.iri`).
//!    A KB imported from schema.org still exports as `schema:Organization`, so the vocabulary the other side holds matches.
//! 2. **Intervals hang off reified statements, not off the triples.** RDF-star is more natural but most consumers
//!    cannot read it yet, and named graphs do not exist in Turtle at all: a file that cannot be opened is not an export.
//! 3. **If a standard term exists, do not invent one**: `prov:invalidatedAtTime` says exactly what happens on our recording axis,
//!    and a private name would only hide a concept everyone already knows.

use std::io::Write;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use oxrdf::vocab::{rdf, rdfs, xsd};
use oxrdf::{Literal, NamedNode, Term, TripleRef};
use utopia_store::export::{
    ExportAxiomViolation, ExportClass, ExportDerived, ExportDocument, ExportEntity, ExportFact,
    ExportFactConflict, ExportRelation,
};
use uuid::Uuid;

const OWL: &str = "http://www.w3.org/2002/07/owl#";
const PROV: &str = "http://www.w3.org/ns/prov#";
const SCHEMA: &str = "https://schema.org/";
/// Only the items with no standard term land here: confidence, the derived flag, and the model's original words.
///
/// **URN rather than http://**: the domain is not settled yet, and once a vocabulary IRI is published it should never change.
/// Pointing at an address we do not serve is worse than pointing at a URN that makes no promise about dereferencing.
const UTOPIA: &str = "urn:utopia:ns:";

fn nn(iri: impl Into<String>) -> NamedNode {
    // Every IRI built here comes from uuids / ontology keys / the configured base, and out-of-range characters are already
    // blocked in `Names::new`; if this still fails, it can only be a bug, and should not turn the whole export into a 500
    NamedNode::new(iri.into()).expect("exported IRI must be valid")
}

fn owl(term: &str) -> NamedNode {
    nn(format!("{OWL}{term}"))
}
fn prov(term: &str) -> NamedNode {
    nn(format!("{PROV}{term}"))
}
fn schema(term: &str) -> NamedNode {
    nn(format!("{SCHEMA}{term}"))
}
fn utopia(term: &str) -> NamedNode {
    nn(format!("{UTOPIA}{term}"))
}

/// How the IRIs in this export are built.
///
/// Default is a URN: `urn:utopia:kb:{kb}:entity:{uuid}`. **Stability comes before dereferenceability**:
/// exporting the same KB again a year later, the two files must match; and a self-hosted instance does not know
/// its own public address. Building from the request's Host header would make identity depend on which reverse proxy was used.
/// When the operator knows where it is published, `?base=https://…/` switches to http IRIs.
pub struct Names {
    prefix: String,
    sep: char,
}

impl Names {
    pub fn new(kb_id: Uuid, base: Option<&str>) -> Result<Self, String> {
        match base.map(str::trim).filter(|b| !b.is_empty()) {
            Some(base) => {
                if !(base.starts_with("http://") || base.starts_with("https://")) {
                    return Err("`base` must be an http(s) IRI".into());
                }
                if base.contains(['<', '>', '"', '{', '}', '|', '\\', '^', '`', ' ']) {
                    return Err("`base` contains characters that cannot appear in an IRI".into());
                }
                let trimmed = base.trim_end_matches('/');
                Ok(Self {
                    prefix: format!("{trimmed}/kb/{kb_id}/"),
                    sep: '/',
                })
            }
            None => Ok(Self {
                prefix: format!("urn:utopia:kb:{kb_id}:"),
                sep: ':',
            }),
        }
    }

    fn mint(&self, kind: &str, id: &str) -> NamedNode {
        nn(format!("{}{kind}{}{id}", self.prefix, self.sep))
    }

    pub fn entity(&self, id: Uuid) -> NamedNode {
        self.mint("entity", &id.to_string())
    }
    pub fn fact(&self, id: Uuid) -> NamedNode {
        self.mint("fact", &id.to_string())
    }
    pub fn derived(&self, id: Uuid) -> NamedNode {
        self.mint("derived", &id.to_string())
    }
    pub fn document(&self, id: Uuid) -> NamedNode {
        self.mint("document", &id.to_string())
    }
    pub fn rule(&self, id: Uuid) -> NamedNode {
        self.mint("rule", &id.to_string())
    }
    /// A factual dispute (0062): `…:conflict:{uuid}`
    pub fn fact_conflict(&self, id: Uuid) -> NamedNode {
        self.mint("conflict", &id.to_string())
    }
    /// An axiom violation finding (0062): `…:violation:{uuid}`
    pub fn axiom_violation(&self, id: Uuid) -> NamedNode {
        self.mint("violation", &id.to_string())
    }
    /// Classes and relations that the ontology grew itself use the **key** rather than the uuid: the key is the identifier this KB already uses internally
    /// (`UNIQUE (kb_id, key)`; extraction prompts and the API both use it), so the file reads well.
    /// Imported ones always use their original IRI
    pub fn class(&self, c: &ExportClass) -> NamedNode {
        match c.iri.as_deref() {
            Some(iri) => NamedNode::new(iri).unwrap_or_else(|_| self.mint("class", &c.key)),
            None => self.mint("class", &c.key),
        }
    }
    pub fn relation(&self, r: &ExportRelation) -> NamedNode {
        match r.iri.as_deref() {
            Some(iri) => NamedNode::new(iri).unwrap_or_else(|_| self.mint("relation", &r.key)),
            None => self.mint("relation", &r.key),
        }
    }
}

/// The place the serializer writes to. After each page is written, the bytes accumulated so far are taken and sent to the client:
/// hundreds of thousands of facts must not first be assembled into one whole String in memory
#[derive(Clone, Default)]
pub struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    pub fn take(&self) -> Vec<u8> {
        let mut guard = self.0.lock().expect("export buffer was poisoned");
        std::mem::take(&mut *guard)
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("export buffer was poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Both formats share the same set of triples; only the writing differs: `oxrdfio` puts the two serializers behind
/// one type, so there is no branch here.
pub struct Sink(oxrdfio::WriterQuadSerializer<SharedBuf>);

/// The prefix table. In Turtle it decides whether the file reads `schema:Organization` or a long run of angle brackets
const PREFIXES: [(&str, &str); 6] = [
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("owl", OWL),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("prov", PROV),
    ("schema", SCHEMA),
];

impl Sink {
    pub fn new(format: Format, buf: SharedBuf) -> Self {
        let mut s = oxrdfio::RdfSerializer::from_format(format.rdf_format());
        for (p, iri) in PREFIXES {
            s = s
                .with_prefix(p, iri)
                .expect("prefix table is a constant, it should never fail to parse");
        }
        s = s
            .with_prefix("utopia", UTOPIA)
            .expect("prefix table is a constant, it should never fail to parse");
        Sink(s.for_writer(buf))
    }

    fn triple(&mut self, t: TripleRef<'_>) -> std::io::Result<()> {
        self.0
            .serialize_quad(t.in_graph(oxrdf::GraphNameRef::DefaultGraph))
    }

    /// `s p o`, where o is a resource.
    fn r(&mut self, s: &NamedNode, p: &NamedNode, o: &NamedNode) -> std::io::Result<()> {
        self.triple(TripleRef::new(s.as_ref(), p.as_ref(), o.as_ref()))
    }

    /// `s p o`, where o is a literal value.
    fn l(&mut self, s: &NamedNode, p: &NamedNode, o: &Literal) -> std::io::Result<()> {
        self.triple(TripleRef::new(s.as_ref(), p.as_ref(), o.as_ref()))
    }

    pub fn finish(self) -> std::io::Result<()> {
        self.0.finish().map(|_| ())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Turtle,
    JsonLd,
}

impl Format {
    fn rdf_format(self) -> oxrdfio::RdfFormat {
        match self {
            Format::Turtle => oxrdfio::RdfFormat::Turtle,
            // Streaming profile: writes out group by group per subject, without building the whole document in memory.
            // Export is the only place in this repo where one output may be larger than the library itself
            Format::JsonLd => oxrdfio::RdfFormat::JsonLd {
                profile: oxrdfio::JsonLdProfile::Streaming.into(),
            },
        }
    }

    pub fn parse(raw: Option<&str>) -> Option<Self> {
        match raw.unwrap_or("turtle").trim().to_ascii_lowercase().as_str() {
            "turtle" | "ttl" => Some(Format::Turtle),
            "jsonld" | "json-ld" | "json" => Some(Format::JsonLd),
            _ => None,
        }
    }
    pub fn content_type(self) -> &'static str {
        match self {
            Format::Turtle => "text/turtle; charset=utf-8",
            Format::JsonLd => "application/ld+json",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            Format::Turtle => "ttl",
            Format::JsonLd => "jsonld",
        }
    }
}

fn dt(at: DateTime<Utc>) -> Literal {
    Literal::new_typed_literal(
        at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        xsd::DATE_TIME,
    )
}

/// World time is written as a literal at **the precision measured at the time**: if only the year is known, write `xsd:gYear`.
/// Writing everything as `xsd:date` would give the ledger a certainty it never had
fn world_time(at: DateTime<Utc>, precision: Option<&str>) -> Literal {
    let iso = at.format("%Y-%m-%d").to_string();
    match precision {
        Some("year") => Literal::new_typed_literal(iso[..4].to_string(), xsd::G_YEAR),
        Some("month") => Literal::new_typed_literal(iso[..7].to_string(), xsd::G_YEAR_MONTH),
        // Below the hour (0024): xsd:dateTime cannot express "accurate to the minute only", so emit_validity writes the precision separately as one more triple
        Some("hour" | "minute" | "second") => Literal::new_typed_literal(
            at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            xsd::DATE_TIME,
        ),
        _ => Literal::new_typed_literal(iso, xsd::DATE),
    }
}

/// Precision below the hour: XSD's types cannot express it, so a separate `utopia:*Precision` is written. Date granularity is not needed:
/// gYear / gYearMonth / date are already a precision
fn sub_day(precision: Option<&str>) -> Option<&str> {
    precision.filter(|p| matches!(*p, "hour" | "minute" | "second"))
}

fn text(s: impl Into<String>) -> Literal {
    Literal::new_simple_literal(s.into())
}

fn confidence(c: f32) -> Literal {
    Literal::new_typed_literal(format!("{c:.2}"), xsd::DECIMAL)
}

fn flag(b: bool) -> Literal {
    Literal::new_typed_literal(if b { "true" } else { "false" }, xsd::BOOLEAN)
}

/// Things looked up repeatedly during one export: the IRIs of classes and relations, and the ranges of properties.
pub struct Vocabulary {
    pub classes: Vec<(Uuid, NamedNode)>,
    pub relations: Vec<(Uuid, NamedNode, Option<String>, Option<String>)>,
}

impl Vocabulary {
    pub fn class(&self, id: Uuid) -> Option<&NamedNode> {
        self.classes.iter().find(|(i, _)| *i == id).map(|(_, n)| n)
    }
    pub fn relation(&self, id: Uuid) -> Option<&NamedNode> {
        self.relations
            .iter()
            .find(|(i, _, _, _)| *i == id)
            .map(|(_, n, _, _)| n)
    }
    /// (datatype, unit)
    fn literal_shape(&self, id: Uuid) -> (Option<&str>, Option<&str>) {
        self.relations
            .iter()
            .find(|(i, _, _, _)| *i == id)
            .map(|(_, _, d, u)| (d.as_deref(), u.as_deref()))
            .unwrap_or((None, None))
    }
}

pub fn vocabulary(
    names: &Names,
    classes: &[ExportClass],
    relations: &[ExportRelation],
) -> Vocabulary {
    Vocabulary {
        classes: classes.iter().map(|c| (c.id, names.class(c))).collect(),
        relations: relations
            .iter()
            .map(|r| (r.id, names.relation(r), r.datatype.clone(), r.unit.clone()))
            .collect(),
    }
}

pub fn emit_class(sink: &mut Sink, vocab: &Vocabulary, c: &ExportClass) -> std::io::Result<()> {
    let iri = match vocab.class(c.id) {
        Some(n) => n.clone(),
        None => return Ok(()),
    };
    sink.r(&iri, &nn(rdf::TYPE.as_str()), &owl("Class"))?;
    sink.l(&iri, &nn(rdfs::LABEL.as_str()), &text(c.label.clone()))?;
    if !c.description.is_empty() {
        sink.l(
            &iri,
            &nn(rdfs::COMMENT.as_str()),
            &text(c.description.clone()),
        )?;
    }
    for parent in &c.parents {
        if let Some(p) = vocab.class(*parent) {
            let p = p.clone();
            sink.r(&iri, &nn(rdfs::SUB_CLASS_OF.as_str()), &p)?;
        }
    }
    for other in &c.disjoint {
        if let Some(o) = vocab.class(*other) {
            let o = o.clone();
            sink.r(&iri, &owl("disjointWith"), &o)?;
        }
    }
    emit_display_hints(sink, &iri, &c.display_hints)
}

/// The display hints go back as the literals they came from (RC-008): one
/// `utopia:<key> "<value>"` per entry, so an export re-imports to the same hints.
fn emit_display_hints(
    sink: &mut Sink,
    iri: &NamedNode,
    hints: &serde_json::Value,
) -> std::io::Result<()> {
    for (key, value) in hints.as_object().into_iter().flatten() {
        if let Some(v) = value.as_str() {
            sink.l(iri, &utopia(key), &text(v))?;
        }
    }
    Ok(())
}

pub fn emit_relation(
    sink: &mut Sink,
    vocab: &Vocabulary,
    r: &ExportRelation,
) -> std::io::Result<()> {
    let iri = match vocab.relation(r.id) {
        Some(n) => n.clone(),
        None => return Ok(()),
    };
    let kind = if r.kind == "attribute" {
        owl("DatatypeProperty")
    } else {
        owl("ObjectProperty")
    };
    sink.r(&iri, &nn(rdf::TYPE.as_str()), &kind)?;
    sink.l(&iri, &nn(rdfs::LABEL.as_str()), &text(r.label.clone()))?;
    if !r.description.is_empty() {
        sink.l(
            &iri,
            &nn(rdfs::COMMENT.as_str()),
            &text(r.description.clone()),
        )?;
    }
    // Axioms are copied as they are, none dropped: the consistency check runs on them, and a reader must be able to recompute it themselves
    for (on, term) in [
        (r.functional, "FunctionalProperty"),
        (r.inverse_functional, "InverseFunctionalProperty"),
        (r.is_transitive, "TransitiveProperty"),
        (r.is_symmetric, "SymmetricProperty"),
        (r.is_asymmetric, "AsymmetricProperty"),
        (r.is_irreflexive, "IrreflexiveProperty"),
    ] {
        if on {
            sink.r(&iri, &nn(rdf::TYPE.as_str()), &owl(term))?;
        }
    }
    // Only declared edges are copied; no inverse declarations or transitive closure are added, and targets are looked up only in this KB's vocabulary.
    for (target, predicate) in [
        (r.inverse_of, owl("inverseOf")),
        (r.sub_property_of, nn(rdfs::SUB_PROPERTY_OF.as_str())),
    ] {
        if let Some(target) = target.and_then(|id| vocab.relation(id)) {
            sink.r(&iri, &predicate, target)?;
        }
    }
    // Temporal semantics are copied too (0031): both ends of an event predicate's fact are the same moment, and an eternal predicate's
    // fact has no date. A reader who ignores this would read the former as a one-day state and the latter as holding since an unknown time.
    // `state` is written too: import reads "not written" as "does not move", so if a relation changed back to state and is re-imported without it, it becomes event
    sink.l(&iri, &utopia("temporal"), &text(r.temporal.clone()))?;
    emit_display_hints(sink, &iri, &r.display_hints)?;
    for d in &r.domains {
        if let Some(c) = vocab.class(*d) {
            let c = c.clone();
            sink.r(&iri, &nn(rdfs::DOMAIN.as_str()), &c)?;
        }
    }
    for g in &r.ranges {
        if let Some(c) = vocab.class(*g) {
            let c = c.clone();
            sink.r(&iri, &nn(rdfs::RANGE.as_str()), &c)?;
        }
    }
    if let Some(unit) = &r.unit {
        sink.l(&iri, &utopia("unit"), &text(unit.clone()))?;
    }
    Ok(())
}

pub fn emit_entity(
    sink: &mut Sink,
    names: &Names,
    vocab: &Vocabulary,
    e: &ExportEntity,
) -> std::io::Result<()> {
    let iri = names.entity(e.id);
    sink.l(
        &iri,
        &nn(rdfs::LABEL.as_str()),
        &text(e.canonical_name.clone()),
    )?;
    // A type may be absent (0009): when it was not determined, write nothing, rather than padding in an owl:Thing
    if let Some(t) = e.type_id.and_then(|t| vocab.class(t)) {
        let t = t.clone();
        sink.r(&iri, &nn(rdf::TYPE.as_str()), &t)?;
    }
    Ok(())
}

pub fn emit_document(sink: &mut Sink, names: &Names, d: &ExportDocument) -> std::io::Result<()> {
    let iri = names.document(d.id);
    sink.r(&iri, &nn(rdf::TYPE.as_str()), &prov("Entity"))?;
    sink.l(&iri, &nn(rdfs::LABEL.as_str()), &text(d.filename.clone()))?;
    if let Some(key) = &d.external_key {
        sink.l(&iri, &utopia("externalKey"), &text(key.clone()))?;
    }
    if let Some(t) = d.doc_time {
        sink.l(&iri, &schema("datePublished"), &world_time(t, Some("day")))?;
    }
    sink.l(&iri, &prov("generatedAtTime"), &dt(d.created_at))?;
    // Deleted documents remain in the file: their facts still point at them as provenance, and erasing the provenance would erase the evidence chain
    if let Some(t) = d.deleted_at {
        sink.l(&iri, &prov("invalidatedAtTime"), &dt(t))?;
    }
    Ok(())
}

/// One fact: the reified statement (always emitted) + the current triples (emitted only while it is still held and still valid).
pub fn emit_fact(
    sink: &mut Sink,
    names: &Names,
    vocab: &Vocabulary,
    f: &ExportFact,
    now: DateTime<Utc>,
) -> std::io::Result<()> {
    let stmt = names.fact(f.id);
    let subject = names.entity(f.subject_id);
    let predicate = f.predicate_id.and_then(|p| vocab.relation(p)).cloned();
    let object: Option<Term> = match (f.object_id, &f.object_value) {
        (Some(o), _) => Some(names.entity(o).into()),
        (None, Some(v)) => {
            // An unbound statement still has an object; only its datatype is unknown.
            let datatype = f.predicate_id.and_then(|p| vocab.literal_shape(p).0);
            // #821: if it does not parse, do not write it, so that an empty literal like rdf:object="" does not mislead the audit
            // tool into saying "fact has no object". The old code's bug was `v.get("value").unwrap_or(v)`, which, for a `{"summary": ...}` shape,
            // serialized the whole object as literal text (#831).
            literal_value(v, datatype).map(Term::from)
        }
        _ => None,
    };

    sink.r(&stmt, &nn(rdf::TYPE.as_str()), &nn(rdf::STATEMENT.as_str()))?;
    sink.r(&stmt, &nn(rdf::SUBJECT.as_str()), &subject)?;
    match (&predicate, &f.surface_predicate) {
        (Some(p), _) => sink.r(&stmt, &nn(rdf::PREDICATE.as_str()), p)?,
        // The ontology did not catch this relation (0010): **do not mint a predicate**. The original words are kept here as a literal,
        // so a reader can see "the system heard this word, and the vocabulary has no such term"
        (None, Some(word)) => sink.l(&stmt, &utopia("proposedPredicate"), &text(word.clone()))?,
        (None, None) => {}
    }
    if let Some(o) = &object {
        sink.triple(TripleRef::new(stmt.as_ref(), rdf::OBJECT, o.as_ref()))?;
    }
    // A value given relative to one event only ("45 days after the trigger date", #681 §4): the literal is the original text, and this line says it is not a date
    if f.object_value.as_ref().is_some_and(is_relative) {
        sink.l(&stmt, &utopia("relativeValue"), &flag(true))?;
    }
    emit_validity(
        sink,
        &stmt,
        f.valid_from,
        f.valid_from_precision.as_deref(),
        f.valid_to,
        f.valid_to_precision.as_deref(),
    )?;
    sink.l(&stmt, &prov("generatedAtTime"), &dt(f.recorded_at))?;
    if let Some(t) = f.invalidated_at {
        sink.l(&stmt, &prov("invalidatedAtTime"), &dt(t))?;
    }
    sink.l(&stmt, &utopia("confidence"), &confidence(f.confidence))?;
    // Derived by a rule (0044 decision 3, fifth slice): not stated directly by a document, and an auditor must be able to see this layer
    if f.implied {
        sink.l(&stmt, &utopia("implied"), &flag(true))?;
    }
    if let Some(old) = f.supersedes {
        let old = names.fact(old);
        sink.r(&stmt, &utopia("supersedes"), &old)?;
    }
    for doc in &f.documents {
        let d = names.document(*doc);
        sink.r(&stmt, &prov("wasDerivedFrom"), &d)?;
    }
    for quote in &f.quotes {
        sink.l(&stmt, &utopia("quote"), &text(quote.clone()))?;
    }
    // Where the quotation came from (0040): an auditor can tell a quoted sentence from a description of an image without asking us (0020)
    for origin in &f.quote_origins {
        sink.l(&stmt, &utopia("evidenceOrigin"), &text(origin.clone()))?;
    }
    // Attributes on the edge (0037): one extra line on the statement node, the predicate is the attribute's IRI, and the literal follows its datatype
    for q in &f.qualifiers {
        let Some(p) = vocab.relation(q.qualifier_type_id) else {
            continue;
        };
        if let Some(v) = &q.value {
            let (datatype, _) = vocab.literal_shape(q.qualifier_type_id);
            // #821 + #831: if it does not parse, do not write the attribute on this edge, and do not stuff in an empty literal
            if let Some(lit) = literal_value(v, datatype) {
                sink.l(&stmt, p, &lit)?;
            }
        } else if let Some(e) = q.entity_id {
            sink.r(&stmt, p, &names.entity(e))?;
        }
    }

    // Current triples: **still held, and still true now**. An interval that has closed or been retracted does not write this one,
    // otherwise a consumer that ignores reification would read "Zhang San still runs that project".
    // "Still true now" is judged by the interval read out (0022): one with no start runs from the earliest evidence, and one that ended at an unknown
    // time runs up to the document that stated it. The rules live in world_axis; here only the two projected ends are looked at
    let held = f.invalidated_at.is_none();
    let holds_now = f.holds_from.is_some_and(|t| t <= now) && f.holds_to.is_none_or(|t| t > now);
    if held && holds_now {
        if let (Some(p), Some(o)) = (&predicate, &object) {
            sink.triple(TripleRef::new(subject.as_ref(), p.as_ref(), o.as_ref()))?;
        }
    }
    Ok(())
}

/// Derived fact (0002). **Current triples are not written**: it was inferred, not asserted by anyone;
/// a consumer that ignores reification should not take the engine's conclusion for words in a document
pub fn emit_derived(
    sink: &mut Sink,
    names: &Names,
    vocab: &Vocabulary,
    d: &ExportDerived,
) -> std::io::Result<()> {
    let stmt = names.derived(d.id);
    // Identity of the reasoning activity: axiom rules have their own id, business rules use theirs. Both need
    // a stable IRI, or an auditor reading a conclusion could not point to "what it is based on"
    let rule = match (d.rule_id, d.attribute_rule_id) {
        (Some(r), _) => names.rule(r),
        (None, Some(r)) => names.rule(r),
        // The CHECK in the DB guarantees both are not empty at once; if it ever got this far, skipping the whole entry is better
        (None, None) => return Ok(()),
    };
    sink.r(&stmt, &nn(rdf::TYPE.as_str()), &nn(rdf::STATEMENT.as_str()))?;
    sink.r(
        &stmt,
        &nn(rdf::SUBJECT.as_str()),
        &names.entity(d.subject_id),
    )?;
    if let Some(p) = vocab.relation(d.predicate_id).cloned() {
        sink.r(&stmt, &nn(rdf::PREDICATE.as_str()), &p)?;
    }
    // Two channels for the object, same as for asserted facts: entities go by IRI, literal conclusions go as literals (0021)
    match (d.object_id, &d.object_value) {
        (Some(o), _) => sink.r(&stmt, &nn(rdf::OBJECT.as_str()), &names.entity(o))?,
        (None, Some(v)) => {
            let (datatype, _) = vocab.literal_shape(d.predicate_id);
            // #821 + #831: if it does not parse, do not write this object
            if let Some(lit) = literal_value(v, datatype) {
                sink.l(&stmt, &nn(rdf::OBJECT.as_str()), &lit)?;
            }
        }
        (None, None) => {}
    }
    sink.l(&stmt, &utopia("derived"), &flag(true))?;
    emit_validity(
        sink,
        &stmt,
        d.valid_from,
        d.valid_from_precision.as_deref(),
        d.valid_to,
        d.valid_to_precision.as_deref(),
    )?;
    sink.l(&stmt, &prov("generatedAtTime"), &dt(d.derived_at))?;
    if let Some(t) = d.invalidated_at {
        sink.l(&stmt, &prov("invalidatedAtTime"), &dt(t))?;
    }
    sink.l(&stmt, &utopia("confidence"), &confidence(d.confidence))?;
    sink.r(&stmt, &prov("wasGeneratedBy"), &rule)?;
    sink.r(&rule, &nn(rdf::TYPE.as_str()), &prov("Activity"))?;
    // The rule's family and identity (0020, the 2026-09-25 revision, #902): a reader no longer guesses which table it came from by its label.
    // Axiom rules also write out their kind (a closed enumeration) and the predicate their declaration sits on: for inverse and sub_property
    // the predicate is not the conclusion's, and inferring it back from the exported owl:inverseOf / rdfs:subPropertyOf would be ambiguous.
    // Business rules' conditions and expressions are not exported: rules are updated in place, and this IRI cannot guarantee the definition the old conclusions were based on
    if d.rule_id.is_some() {
        sink.r(&rule, &nn(rdf::TYPE.as_str()), &utopia("AxiomRule"))?;
        sink.l(&rule, &utopia("axiomKind"), &text(d.rule.clone()))?;
        if let Some(p) = d.rule_predicate.and_then(|p| vocab.relation(p).cloned()) {
            sink.r(&rule, &utopia("declaredOn"), &p)?;
        }
    } else {
        sink.r(&rule, &nn(rdf::TYPE.as_str()), &utopia("BusinessRule"))?;
    }
    // The label uses the rule's own name (business rules); axioms fall back to their kind name, so the auditor reads
    // "Gas-bearing well" rather than "business"
    sink.l(
        &rule,
        &nn(rdfs::LABEL.as_str()),
        &text(d.rule_name.clone().unwrap_or_else(|| d.rule.clone())),
    )?;
    // Premises. An auditor following prov:used takes one step to reach the assertions, and another to reach the sentences
    for premise in &d.premises {
        let p = names.fact(*premise);
        sink.r(&stmt, &prov("used"), &p)?;
    }
    for premise in &d.premises_derived {
        let p = names.derived(*premise);
        sink.r(&stmt, &prov("used"), &p)?;
    }
    Ok(())
}

/// A factual dispute (0062): two assertions on the same slot that do not give way to each other.
///
/// `status` is given as-is: open means "still disputed"; resolved + `utopia:resolution` is a person's
/// verdict; `withdrawn` is not a verdict: when one side is withdrawn the question is gone, and `utopia:closedAt` records
/// the moment of withdrawal (0051), not the moment someone opened the review page
pub fn emit_fact_conflict(
    sink: &mut Sink,
    names: &Names,
    c: &ExportFactConflict,
) -> std::io::Result<()> {
    let iri = names.fact_conflict(c.id);
    sink.r(&iri, &nn(rdf::TYPE.as_str()), &utopia("FactConflict"))?;
    // The two seats are kept apart by direction: the pressed assertion and the newly arrived one are not symmetric (0062)
    for (pred, fact) in [
        (utopia("priorStatement"), c.old_fact_id),
        (utopia("incomingStatement"), c.new_fact_id),
    ] {
        sink.r(&iri, &pred, &names.fact(fact))?;
    }
    sink.l(&iri, &utopia("reason"), &text(c.reason.clone()))?;
    sink.l(&iri, &utopia("status"), &text(c.status.clone()))?;
    if let Some(r) = &c.resolution {
        sink.l(&iri, &utopia("resolution"), &text(r.clone()))?;
    }
    sink.l(&iri, &prov("generatedAtTime"), &dt(c.created_at))?;
    if let Some(t) = c.resolved_at {
        sink.l(&iri, &utopia("closedAt"), &dt(t))?;
    }
    Ok(())
}

/// An axiom violation finding (0062). This is **mutable current state**, not history: `open` means
/// reconciled against the current ontology, so the criterion is attached only to open rows; resolved rows do not pretend to know
/// which ontology version they were closed against
pub fn emit_axiom_violation(
    sink: &mut Sink,
    names: &Names,
    vocab: &Vocabulary,
    v: &ExportAxiomViolation,
) -> std::io::Result<()> {
    let iri = names.axiom_violation(v.id);
    sink.r(&iri, &nn(rdf::TYPE.as_str()), &utopia("AxiomViolation"))?;
    sink.l(&iri, &utopia("kind"), &text(v.kind.clone()))?;
    // For self_loop/signature the left and right are the same fact: under RDF set semantics a repeat is harmless, but in a Turtle
    // file it would be a duplicate triple
    let endpoints: &[Uuid] = if v.left_fact == v.right_fact {
        &[v.left_fact]
    } else {
        &[v.left_fact, v.right_fact]
    };
    for fact in endpoints {
        sink.r(&iri, &utopia("onStatement"), &names.fact(*fact))?;
    }
    // A cycle is an ordered chain of evidence (0054): flattening it into an unordered set of facts would make "A→B→C→A" impossible to read back.
    // rdf:List is an order-preserving structure that both Turtle and JSON-LD understand
    if !v.path.is_empty() {
        let items: Vec<Term> = v.path.iter().map(|f| names.fact(*f).into()).collect();
        let head = emit_list(sink, &items)?;
        sink.triple(TripleRef::new(
            iri.as_ref(),
            utopia("evidencePath").as_ref(),
            head.as_ref(),
        ))?;
    }
    sink.l(&iri, &utopia("status"), &text(v.status.clone()))?;
    if let Some(r) = &v.resolution {
        sink.l(&iri, &utopia("resolution"), &text(r.clone()))?;
    }
    sink.l(&iri, &utopia("detectedAt"), &dt(v.detected_at))?;
    if let Some(t) = v.decided_at {
        sink.l(&iri, &utopia("closedAt"), &dt(t))?;
    }
    // Criterion (0062 §4): open = reconciled against the current ontology, so it must point back to one relation and one
    // OWL/rdfs term. A signature's criterion is a pair, and both terms are emitted (the row does not record which side broke).
    // Resolved rows do not emit it: their criterion may long since have left the ontology
    if v.status == "open" {
        for term in v.criterion_terms().unwrap_or(&[]) {
            sink.r(&iri, &utopia("criterion"), &criterion_node(term))?;
        }
        if let Some(rel) = v
            .criterion_predicate
            .and_then(|p| vocab.relation(p))
            .cloned()
        {
            sink.r(&iri, &utopia("onRelation"), &rel)?;
        }
    }
    Ok(())
}

/// Term for `utopia:criterion`: `criterion_terms` produces only `owl:`/`rdfs:`, and other values cannot reach here
/// (the export side already refuses to export them)
fn criterion_node(term: &str) -> NamedNode {
    if let Some(t) = term.strip_prefix("owl:") {
        owl(t)
    } else {
        match term {
            "rdfs:domain" => rdfs::DOMAIN.into(),
            "rdfs:range" => rdfs::RANGE.into(),
            _ => unreachable!("criterion_terms only produces owl:/rdfs: terms"),
        }
    }
}

/// Writes an ordered list of resources as rdf:List, returning the head node (an empty list returns rdf:nil).
/// Built from the tail forward: each blank node knows its rest when it is emitted
fn emit_list(sink: &mut Sink, items: &[Term]) -> std::io::Result<Term> {
    let mut head: Term = nn(rdf::NIL.as_str()).into();
    for item in items.iter().rev() {
        let node = oxrdf::BlankNode::default();
        sink.triple(TripleRef::new(node.as_ref(), rdf::FIRST, item.as_ref()))?;
        sink.triple(TripleRef::new(node.as_ref(), rdf::REST, head.as_ref()))?;
        head = node.into();
    }
    Ok(head)
}

fn emit_validity(
    sink: &mut Sink,
    stmt: &NamedNode,
    from: Option<DateTime<Utc>>,
    from_precision: Option<&str>,
    to: Option<DateTime<Utc>>,
    to_precision: Option<&str>,
) -> std::io::Result<()> {
    if let Some(t) = from {
        sink.l(stmt, &schema("validFrom"), &world_time(t, from_precision))?;
        if let Some(p) = sub_day(from_precision) {
            sink.l(stmt, &utopia("validFromPrecision"), &text(p))?;
        }
    }
    match (to, to_precision) {
        (Some(t), p) => {
            sink.l(stmt, &schema("validThrough"), &world_time(t, p))?;
            if let Some(p) = sub_day(p) {
                sink.l(stmt, &utopia("validThroughPrecision"), &text(p))?;
            }
        }
        // "Ended, but not known which day": the ledger keeps a dedicated state for this, and the export must not flatten it
        // into "still holds" (that is the old problem of valid_to carrying two meanings in one column)
        (None, Some("unknown")) => sink.l(stmt, &utopia("endedUnknown"), &flag(true))?,
        (None, _) => {}
    }
    Ok(())
}

/// The value carries `"relative": true`: the original text gives it only relative to one event, and there is no calendar date
fn is_relative(v: &serde_json::Value) -> bool {
    v.get("relative").and_then(|r| r.as_bool()) == Some(true)
}

/// Extracts a literal from shapes such as `{"value": …, "unit": …}` or `{"summary": …}`.
///
/// **Resolves the text first and returns `None` when nothing resolves.** The
/// audit invariant #821 (`an_absent_object_is_not_an_empty_literal`) says that an
/// absent object should produce no `rdf:object` triple at all; the same principle
/// applies here when the value resolves to nothing — better to omit than to emit
/// a literal whose lexical form is `""`, since that turns into JSON serialisation
/// for an object value and is a parser-puzzle for downstream consumers.
///
/// A relative value is written as a plain string: `"45 days after the Trigger Date"^^xsd:date` is an invalid literal
fn literal_value(v: &serde_json::Value, datatype: Option<&str>) -> Option<Literal> {
    let (text, prose) = literal_text(v)?;
    if prose || is_relative(v) {
        return Some(Literal::new_simple_literal(text));
    }
    Some(match datatype {
        Some("number") => Literal::new_typed_literal(decimal_text(text), xsd::DECIMAL),
        Some("date") => {
            date_literal(&text).unwrap_or_else(|| Literal::new_typed_literal(text, xsd::DATE))
        }
        Some("bool") => Literal::new_typed_literal(text, xsd::BOOLEAN),
        _ => Literal::new_simple_literal(text),
    })
}

/// A JSON number may carry an exponent, but xsd:decimal may not. The text is expanded with the existing exact decimal type,
/// without going through f64 again and without rounding; forms with no exponent (including negative zero) are kept as they are
fn decimal_text(text: String) -> String {
    let Some((coefficient, exponent)) = text.split_once(['e', 'E']) else {
        return text;
    };
    // The normalized finite f64 exponent is only within this range. Other strings are not necessarily normalized,
    // so an unusually large exponent must not trigger unbounded allocation, and underscores and other forms that BigDecimal tolerates are not quietly cleaned up
    let supported_exponent = exponent
        .parse::<i32>()
        .is_ok_and(|exponent| (-324..=308).contains(&exponent));
    let decimal_coefficient = coefficient
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.'));
    if supported_exponent && decimal_coefficient {
        if let Ok(number) = text.parse::<sqlx::types::BigDecimal>() {
            return number.to_plain_string();
        }
    }
    text
}

/// A date kept on a property is the original text, not a world-time interval. parse_time truncates fractional seconds and reduces a
/// zone-less clock time to a day, so it is used to judge year/month/day only when there is no clock time; the clock and time zone are normalized from the original text.
/// Text that is not recognized is passed through by the caller unchanged; input validation may still accept non-date clock text,
/// and this change does not tighten its rules. The year is formatted by field and must not be sliced by four characters; chrono's
/// `+` added before a positive year must also be stripped, since XSD years do not allow a plus sign.
fn date_literal(value: &str) -> Option<Literal> {
    let Some((date, clock)) = value.split_once(['T', ' ']) else {
        let (date, precision) = utopia_extract::parse_time(value)?;
        let (format, datatype) = match precision {
            "year" => ("%Y", xsd::G_YEAR),
            "month" => ("%Y-%m", xsd::G_YEAR_MONTH),
            _ => ("%Y-%m-%d", xsd::DATE),
        };
        let lexical = date.format(format).to_string();
        return Some(Literal::new_typed_literal(
            lexical.trim_start_matches('+'),
            datatype,
        ));
    };
    let date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let calendar_date = date.format("%Y-%m-%d").to_string();
    let calendar_date = calendar_date.trim_start_matches('+');
    let (clock, zone) = if let Some(clock) = clock.strip_suffix(['Z', 'z']) {
        (clock, "Z".to_string())
    } else if let Some(index) = clock.find(['+', '-']) {
        let (clock, zone) = clock.split_at(index);
        (clock, xsd_timezone(zone)?)
    } else {
        // XSD allows a dateTime without a time zone; a UTC cannot be guessed for the original text.
        (clock, String::new())
    };
    let parts: Vec<&str> = clock.split(':').collect();
    let (hour, minute, second) = match parts.as_slice() {
        [hour] => (*hour, "00", "00"),
        [hour, minute] => (*hour, *minute, "00"),
        [hour, minute, second] => (*hour, *minute, *second),
        _ => return None,
    };
    let (second, fraction) = match second.split_once('.') {
        Some((second, fraction)) => {
            if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            (second, format!(".{fraction}"))
        }
        None => (second, String::new()),
    };
    if ![hour, minute, second]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let hour = hour.parse().ok()?;
    let minute = minute.parse().ok()?;
    let second = second.parse().ok()?;
    chrono::NaiveTime::from_hms_opt(hour, minute, second)?;
    // dateTime must have HH:MM:SS; zero-padding means the start of that hour/minute, which the type itself cannot express
    // ("accurate to the hour/minute only"). Fractional seconds are not handed to chrono, so original text beyond nanoseconds is not truncated.
    let lexical = format!("{calendar_date}T{hour:02}:{minute:02}:{second:02}{fraction}{zone}");
    Some(Literal::new_typed_literal(lexical, xsd::DATE_TIME))
}

/// parse_time also accepts ±HH and ±HHMM, while XSD time zones require ±HH:MM.
fn xsd_timezone(zone: &str) -> Option<String> {
    if !zone.is_ascii() {
        return None;
    }
    let normalized = match zone.len() {
        3 => format!("{zone}:00"),
        5 => format!("{}:{}", &zone[..3], &zone[3..]),
        6 if zone.as_bytes()[3] == b':' => zone.to_string(),
        _ => return None,
    };
    let digits = [&normalized[1..3], &normalized[4..6]];
    if !digits
        .iter()
        .all(|part| part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let hour: u32 = digits[0].parse().ok()?;
    let minute: u32 = digits[1].parse().ok()?;
    if hour > 14 || minute > 59 || (hour == 14 && minute != 0) {
        return None;
    }
    Some(normalized)
}

/// Extracts writable literal text from a fact's `object_value` shape, and whether this text is prose written by a person.
///
/// Shapes recognized (anything else is treated as a missing value):
/// 1. A string / number / boolean given directly
/// 2. `{"value": …}`: if `value` is a scalar, take it
/// 3. `{"summary": …}`: looked at only when `value` is absent or empty (`models.rs` records the two as alternatives,
///    and `api/tools.rs` also checks `value` first and then `summary`; no separate order is set here)
/// 4. `{"class": …}`: a rule's classification conclusion (`reasoning.rs` writes this shape, and five places read it back by key)
///
/// The old behavior was `v.get("value").unwrap_or(v)`: when the key was missing, the whole object was taken as the literal, so
/// `{"summary": …}` was serialized as literal text like `{"summary":"…"}` (#831). The new behavior parses it into
/// real text; if nothing parses (neither a scalar nor a recognized key) it returns `None`, and the caller does not write
/// the `rdf:object` triple. A second return value of `true` means the text came from `summary` / `class`:
/// that is prose for people or a class name, and should not get the property's declared `xsd:decimal` / `xsd:date`
fn literal_text(v: &serde_json::Value) -> Option<(String, bool)> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some((b.to_string(), false)),
        serde_json::Value::Number(n) => Some((n.to_string(), false)),
        serde_json::Value::String(s) => Some((s.clone(), false)),
        serde_json::Value::Array(_) => None,
        serde_json::Value::Object(map) => {
            if let Some((text, _)) = map.get("value").and_then(literal_text) {
                return Some((text, false));
            }
            for key in ["summary", "class"] {
                if let Some(text) = map.get(key).and_then(|s| s.as_str()) {
                    if !text.is_empty() {
                        return Some((text.to_string(), true));
                    }
                }
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::{NamedNodeRef, Quad};

    fn kb() -> Uuid {
        Uuid::parse_str("01a06dc4-f40a-7013-b09f-1b499e2e7441").unwrap()
    }

    fn id(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn class(n: u8, key: &str, iri: Option<&str>) -> ExportClass {
        ExportClass {
            id: id(n),
            key: key.into(),
            label: key.into(),
            description: String::new(),
            iri: iri.map(str::to_string),
            parents: vec![],
            disjoint: vec![],
            display_hints: serde_json::json!({}),
        }
    }

    fn relation(n: u8, key: &str, iri: Option<&str>, kind: &str) -> ExportRelation {
        ExportRelation {
            id: id(n),
            key: key.into(),
            label: key.into(),
            description: String::new(),
            iri: iri.map(str::to_string),
            kind: kind.into(),
            datatype: (kind == "attribute").then(|| "number".to_string()),
            unit: None,
            temporal: "state".into(),
            functional: true,
            inverse_functional: false,
            is_transitive: false,
            is_symmetric: false,
            is_asymmetric: false,
            is_irreflexive: false,
            inverse_of: None,
            sub_property_of: None,
            domains: vec![],
            ranges: vec![],
            display_hints: serde_json::json!({}),
        }
    }

    fn fact(n: u8) -> ExportFact {
        ExportFact {
            qualifiers: Vec::new(),
            id: id(n),
            subject_id: id(10),
            predicate_id: Some(id(2)),
            surface_predicate: None,
            object_id: Some(id(11)),
            object_value: None,
            valid_from: None,
            valid_from_precision: None,
            valid_to: None,
            valid_to_precision: None,
            // The interval read out (0022): with no start, it starts from the anchor; here it is made to be the same day as the recorded moment
            holds_from: Some(at("2026-01-01T00:00:00Z")),
            holds_to: None,
            recorded_at: at("2026-01-01T00:00:00Z"),
            invalidated_at: None,
            confidence: 0.9,
            implied: false,
            supersedes: None,
            documents: vec![],
            quotes: vec![],
            quote_origins: vec![],
            subject_kb: Some(kb()),
            object_kb: Some(kb()),
            predicate_kb: Some(kb()),
            supersedes_kb: None,
            foreign_document: false,
            foreign_chunk: false,
            subject_merged: false,
            object_merged: false,
        }
    }

    /// Export, then parse back. **It must parse back**: asserting that a string contains some segment
    /// does not prove the file's syntax is valid; whether literal text conforms to its XSD type must still be asserted separately
    fn export(format: Format, emit: impl FnOnce(&mut Sink, &Names, &Vocabulary)) -> Vec<Quad> {
        let names = Names::new(kb(), None).unwrap();
        let classes = vec![
            class(1, "person", Some("https://schema.org/Person")),
            class(3, "team", None),
        ];
        let relations = vec![
            relation(
                2,
                "works_for",
                Some("https://schema.org/worksFor"),
                "relation",
            ),
            relation(4, "headcount", None, "attribute"),
        ];
        let vocab = vocabulary(&names, &classes, &relations);
        let buf = SharedBuf::default();
        let mut sink = Sink::new(format, buf.clone());
        for c in &classes {
            emit_class(&mut sink, &vocab, c).unwrap();
        }
        for r in &relations {
            emit_relation(&mut sink, &vocab, r).unwrap();
        }
        emit(&mut sink, &names, &vocab);
        sink.finish().unwrap();
        let bytes = buf.take();
        oxrdfio::RdfParser::from_format(match format {
            Format::Turtle => oxrdfio::RdfFormat::Turtle,
            Format::JsonLd => oxrdfio::RdfFormat::JsonLd {
                profile: oxrdfio::JsonLdProfileSet::empty(),
            },
        })
        .for_slice(&bytes)
        .map(|q| q.expect("exported file must parse back"))
        .collect()
    }

    fn has(quads: &[Quad], s: &str, p: &str, o: &str) -> bool {
        quads.iter().any(|q| {
            q.subject.to_string() == s && q.predicate.to_string() == format!("<{p}>") && {
                let obj = q.object.to_string();
                obj == o || obj == format!("<{o}>")
            }
        })
    }

    fn objects(quads: &[Quad], s: &str, p: &str) -> Vec<String> {
        quads
            .iter()
            .filter(|q| q.subject.to_string() == s && q.predicate.to_string() == format!("<{p}>"))
            .map(|q| q.object.to_string())
            .collect()
    }

    const STMT: &str = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:fact:05050505-0505-0505-0505-050505050505>";
    const SUBJ: &str = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:entity:0a0a0a0a-0a0a-0a0a-0a0a-0a0a0a0a0a0a>";
    const OBJ: &str = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:entity:0b0b0b0b-0b0b-0b0b-0b0b-0b0b0b0b0b0b>";
    const WORKS_FOR: &str = "https://schema.org/worksFor";

    /// The same property value passes through every exit; RDF parsing successfully does not mean the XSD literal text conforms to its type.
    fn assert_attribute_literal_exports(
        value: serde_json::Value,
        datatype: &str,
        expected: Option<(&str, NamedNodeRef<'_>)>,
    ) {
        let mut attribute = fact(5);
        attribute.predicate_id = Some(id(4));
        attribute.object_id = None;
        attribute.object_value = Some(value.clone());
        attribute
            .qualifiers
            .push(utopia_core::models::FactQualifier {
                qualifier_type_id: id(4),
                value: Some(value.clone()),
                ..Default::default()
            });
        let derived = ExportDerived {
            id: id(7),
            subject_id: id(10),
            predicate_id: id(4),
            object_id: None,
            object_value: Some(value.clone()),
            rule_id: None,
            attribute_rule_id: Some(id(9)),
            valid_from: None,
            valid_from_precision: None,
            valid_to: None,
            valid_to_precision: None,
            derived_at: at("2026-02-01T00:00:00Z"),
            invalidated_at: None,
            confidence: 0.9,
            rule: "business".into(),
            rule_predicate: None,
            rule_name: Some("Attribute conclusion".into()),
            premises: vec![id(5)],
            premises_derived: Vec::new(),
            subject_kb: Some(kb()),
            object_kb: None,
            predicate_kb: Some(kb()),
            rule_kb: None,
            attribute_rule_kb: Some(kb()),
            foreign_fact_premise: false,
            foreign_derived_premise: false,
            subject_merged: false,
            object_merged: false,
        };
        let names = Names::new(kb(), None).unwrap();
        let mut property = relation(4, "value", None, "attribute");
        property.datatype = Some(datatype.into());
        let property_iri = names.relation(&property);
        let statement_iri = names.fact(attribute.id).to_string();
        let subject_iri = names.entity(attribute.subject_id).to_string();
        let derived_iri = names.derived(derived.id).to_string();
        for format in [Format::Turtle, Format::JsonLd] {
            let quads = export(format, |sink, names, _| {
                let vocab = vocabulary(names, &[], std::slice::from_ref(&property));
                emit_fact(sink, names, &vocab, &attribute, at("2026-06-01T00:00:00Z")).unwrap();
                emit_derived(sink, names, &vocab, &derived).unwrap();
            });
            for (outlet, subject, predicate) in [
                ("fact", &statement_iri, rdf::OBJECT.as_str()),
                ("current triple", &subject_iri, property_iri.as_str()),
                ("qualifier", &statement_iri, property_iri.as_str()),
                ("derived fact", &derived_iri, rdf::OBJECT.as_str()),
            ] {
                let objects: Vec<_> = quads
                    .iter()
                    .filter(|quad| {
                        quad.subject.to_string() == *subject && quad.predicate.as_str() == predicate
                    })
                    .map(|quad| &quad.object)
                    .collect();
                let context = format!("{outlet}, {format:?}, {datatype}, {value}");
                match expected {
                    Some((lexical, expected_datatype)) => {
                        assert_eq!(objects.len(), 1, "{context}");
                        let Term::Literal(literal) = objects[0] else {
                            panic!("expected a literal: {context}");
                        };
                        assert_eq!(literal.value(), lexical, "{context}");
                        assert_eq!(literal.datatype(), expected_datatype, "{context}");
                    }
                    None => assert!(objects.is_empty(), "unexpected object: {context}"),
                }
            }
        }
    }

    #[test]
    fn normalized_numbers_export_as_decimal_without_exponents() {
        for (input, expected) in [
            (serde_json::json!(0.0000001), "0.0000001"),
            (serde_json::json!("1e-7"), "0.0000001"),
            (serde_json::json!(1e20), "100000000000000000000"),
            (serde_json::json!(-1e-7), "-0.0000001"),
            (serde_json::json!(-1e20), "-100000000000000000000"),
            (serde_json::json!(0), "0.0"),
            (serde_json::json!(-0.0), "-0.0"),
            (serde_json::json!(12.375), "12.375"),
            (serde_json::json!(-12.375), "-12.375"),
            (serde_json::json!(1.25e-7), "0.000000125"),
        ] {
            let normalized = utopia_extract::normalize_attr_value("number", &input).unwrap();
            assert_attribute_literal_exports(
                normalized.clone(),
                "number",
                Some((expected, xsd::DECIMAL)),
            );
            assert_attribute_literal_exports(
                serde_json::json!({"value": normalized}),
                "number",
                Some((expected, xsd::DECIMAL)),
            );
        }
        // An integer already stored cannot go through f64 again: this value exceeds the range where a double can represent every integer.
        assert_attribute_literal_exports(
            serde_json::json!({"value": 9_007_199_254_740_993_u64}),
            "number",
            Some(("9007199254740993", xsd::DECIMAL)),
        );
        // Smallest subnormal and largest finite number: they must not become zero from fixed decimal places, nor overflow.
        for (input, expected) in [
            (f64::from_bits(1), format!("0.{}5", "0".repeat(323))),
            (f64::MAX, format!("17976931348623157{}", "0".repeat(292))),
        ] {
            let normalized =
                utopia_extract::normalize_attr_value("number", &serde_json::json!(input)).unwrap();
            assert_attribute_literal_exports(normalized, "number", Some((&expected, xsd::DECIMAL)));
        }
    }

    #[test]
    fn normalized_dates_export_their_precision_and_original_clock() {
        for (input, expected, datatype) in [
            ("2024", "2024", xsd::G_YEAR),
            ("2024-07", "2024-07", xsd::G_YEAR_MONTH),
            ("2024-07-15", "2024-07-15", xsd::DATE),
            (
                "2024-07-15T14:32:07Z",
                "2024-07-15T14:32:07Z",
                xsd::DATE_TIME,
            ),
            (
                "2024-07-15T14:32:07+08:00",
                "2024-07-15T14:32:07+08:00",
                xsd::DATE_TIME,
            ),
            (
                "2024-07-15T14:32:07.382-05:30",
                "2024-07-15T14:32:07.382-05:30",
                xsd::DATE_TIME,
            ),
            (
                "2024-07-15T14:32:07.123456789012Z",
                "2024-07-15T14:32:07.123456789012Z",
                xsd::DATE_TIME,
            ),
            ("2024-07-15T14Z", "2024-07-15T14:00:00Z", xsd::DATE_TIME),
            (
                "2024-07-15T14+08:00",
                "2024-07-15T14:00:00+08:00",
                xsd::DATE_TIME,
            ),
            ("2024-07-15T14:32Z", "2024-07-15T14:32:00Z", xsd::DATE_TIME),
            (
                "2024-07-15T14:32+08:00",
                "2024-07-15T14:32:00+08:00",
                xsd::DATE_TIME,
            ),
            (
                "2024-07-15T14:32+0800",
                "2024-07-15T14:32:00+08:00",
                xsd::DATE_TIME,
            ),
            (
                "2024-07-15T14+08",
                "2024-07-15T14:00:00+08:00",
                xsd::DATE_TIME,
            ),
            ("2024-07-15T14:32z", "2024-07-15T14:32:00Z", xsd::DATE_TIME),
            (
                "2024-07-15T14+14",
                "2024-07-15T14:00:00+14:00",
                xsd::DATE_TIME,
            ),
            (
                "2024-07-15T14-14",
                "2024-07-15T14:00:00-14:00",
                xsd::DATE_TIME,
            ),
            (
                "2024-07-15 14:32+08:00",
                "2024-07-15T14:32:00+08:00",
                xsd::DATE_TIME,
            ),
            ("2024-07-15T14", "2024-07-15T14:00:00", xsd::DATE_TIME),
            ("2024-07-15T14:32", "2024-07-15T14:32:00", xsd::DATE_TIME),
            (
                "2024-07-15T14:32:07.382",
                "2024-07-15T14:32:07.382",
                xsd::DATE_TIME,
            ),
        ] {
            let normalized =
                utopia_extract::normalize_attr_value("date", &serde_json::json!(input)).unwrap();
            assert_attribute_literal_exports(
                normalized.clone(),
                "date",
                Some((expected, datatype)),
            );
            assert_attribute_literal_exports(
                serde_json::json!({"value": normalized}),
                "date",
                Some((expected, datatype)),
            );
        }
    }

    #[test]
    fn attribute_calendar_years_are_not_cut_to_four_characters() {
        for (input, expected, datatype) in [
            ("-0001-07", "-0001-07", xsd::G_YEAR_MONTH),
            ("-0001-07-15", "-0001-07-15", xsd::DATE),
            ("+10000-07", "10000-07", xsd::G_YEAR_MONTH),
            ("+10000-07-15", "10000-07-15", xsd::DATE),
            (
                "+10000-07-15T14:32:07.382Z",
                "10000-07-15T14:32:07.382Z",
                xsd::DATE_TIME,
            ),
        ] {
            let normalized =
                utopia_extract::normalize_attr_value("date", &serde_json::json!(input)).unwrap();
            assert_attribute_literal_exports(normalized, "date", Some((expected, datatype)));
        }
    }

    #[test]
    fn typed_attributes_keep_value_precedence_prose_and_missing_objects() {
        for (value, datatype, expected) in [
            (
                serde_json::json!({"value": 12.5, "summary": "estimate"}),
                "number",
                Some(("12.5", xsd::DECIMAL)),
            ),
            (
                serde_json::json!({"value": "2024", "summary": "sometime in 2024"}),
                "date",
                Some(("2024", xsd::G_YEAR)),
            ),
            (
                serde_json::json!({"value": 0, "summary": ""}),
                "number",
                Some(("0", xsd::DECIMAL)),
            ),
            (
                serde_json::json!({"summary": "before the merge"}),
                "date",
                Some(("before the merge", xsd::STRING)),
            ),
            (
                serde_json::json!({"value": null, "summary": "not specified"}),
                "number",
                Some(("not specified", xsd::STRING)),
            ),
            (
                serde_json::json!({"class": "gas_well"}),
                "number",
                Some(("gas_well", xsd::STRING)),
            ),
            (
                serde_json::json!({"value": "45 days after the Trigger Date", "relative": true}),
                "date",
                Some(("45 days after the Trigger Date", xsd::STRING)),
            ),
            (serde_json::json!({"value": null}), "date", None),
            (serde_json::json!({"confidence": 0.9}), "number", None),
            (
                serde_json::json!({"summary": "", "class": ""}),
                "date",
                None,
            ),
            (serde_json::Value::Null, "number", None),
        ] {
            assert_attribute_literal_exports(value, datatype, expected);
        }
    }

    #[test]
    fn unbound_literal_objects_survive_both_formats() {
        for value in [
            serde_json::json!({"value": "待复检"}),
            serde_json::json!({"value": "quote: \" and slash: \\"}),
            serde_json::json!({"value": ""}),
            serde_json::json!({"value": 0}),
            serde_json::json!({"value": false}),
            serde_json::json!({"value": null, "summary": "not specified"}),
            serde_json::Value::Null,
        ] {
            let mut f = fact(5);
            f.predicate_id = None;
            f.surface_predicate = Some("状态".into());
            f.object_id = None;
            f.object_value = Some(value.clone());
            f.documents = vec![id(20)];
            f.quotes = vec!["设备 A 待复检".into()];
            f.supersedes = Some(id(6));
            for retracted in [false, true] {
                f.invalidated_at = retracted.then(|| at("2026-02-01T00:00:00Z"));
                let mut sets = Vec::new();
                for format in [Format::Turtle, Format::JsonLd] {
                    let quads = export(format, |sink, names, vocab| {
                        emit_fact(sink, names, vocab, &f, at("2026-06-01T00:00:00Z")).unwrap();
                    });
                    let expected: Vec<String> = literal_value(&value, None)
                        .map(|lit| lit.to_string())
                        .into_iter()
                        .collect();
                    assert_eq!(
                        objects(&quads, STMT, rdf::OBJECT.as_str()),
                        expected,
                        "unbound statement lost its literal object: {value}"
                    );
                    assert!(objects(&quads, STMT, rdf::PREDICATE.as_str()).is_empty());
                    assert!(!quads.iter().any(|q| q.subject.to_string() == SUBJ));
                    sets.push(quads.into_iter().collect::<std::collections::HashSet<_>>());
                }
                assert_eq!(sets[0], sets[1]);
            }
        }
    }

    #[test]
    fn bound_literal_datatypes_survive_both_formats() {
        for (datatype, value, expected) in [
            (
                "number",
                serde_json::json!(0),
                Literal::new_typed_literal("0", xsd::DECIMAL),
            ),
            (
                "text",
                serde_json::json!("待复检"),
                Literal::new_simple_literal("待复检"),
            ),
            (
                "bool",
                serde_json::json!(false),
                Literal::new_typed_literal("false", xsd::BOOLEAN),
            ),
        ] {
            let mut f = fact(5);
            f.predicate_id = Some(id(4));
            f.object_id = None;
            f.object_value = Some(serde_json::json!({"value": value}));
            for format in [Format::Turtle, Format::JsonLd] {
                let quads = export(format, |sink, names, _| {
                    let mut property = relation(4, "value", None, "attribute");
                    property.datatype = Some(datatype.into());
                    let vocab = vocabulary(names, &[], &[property]);
                    emit_fact(sink, names, &vocab, &f, at("2026-06-01T00:00:00Z")).unwrap();
                });
                assert_eq!(
                    objects(&quads, STMT, rdf::OBJECT.as_str()),
                    vec![expected.to_string()]
                );
                assert!(quads.iter().any(|q| q.subject.to_string() == SUBJ
                    && q.object == Term::Literal(expected.clone())));
            }
        }
    }

    #[test]
    fn an_absent_object_is_not_an_empty_literal() {
        let mut f = fact(5);
        f.predicate_id = None;
        f.object_id = None;
        f.object_value = None;
        for format in [Format::Turtle, Format::JsonLd] {
            let quads = export(format, |sink, names, vocab| {
                emit_fact(sink, names, vocab, &f, at("2026-06-01T00:00:00Z")).unwrap();
            });
            assert!(objects(&quads, STMT, rdf::OBJECT.as_str()).is_empty());
        }
    }

    /// #831: on export, if `object_value` is a structured object such as `{"summary": "..."}` or `{"value": "..."}`, the old code
    /// `v.get("value").unwrap_or(v)` would serialize the whole object as a JSON string literal.
    /// After the fix, `value` takes precedence, and `summary` is its fallback (consistent with `models.rs` and `api/tools.rs`);
    /// `summary` and `class` are prose or class names, stored as `xsd:string` rather than with the property's declared type;
    /// if parsing fails there is **no `rdf:object`**, the same principle as #821.
    ///
    /// Seven cases: value takes precedence over summary; only value; only summary (xsd:string);
    /// summary empty falls back to value; null value with no summary → not written; classification conclusion `{"class": …}`
    /// stored as xsd:string; unrecognized key → not written.
    #[test]
    fn an_object_value_with_summary_or_value_does_not_emit_a_json_literal() {
        let cases: &[(&str, serde_json::Value, &[&str])] = &[
            (
                "value wins over summary when both are present",
                serde_json::json!({ "value": 12.5, "summary": "about a dozen" }),
                &["\"12.5\"^^<http://www.w3.org/2001/XMLSchema#decimal>"],
            ),
            (
                "value alone resolves to its scalar string",
                serde_json::json!({ "value": "42" }),
                &["\"42\"^^<http://www.w3.org/2001/XMLSchema#decimal>"],
            ),
            (
                "summary alone is prose: a string, never the declared type",
                serde_json::json!({ "summary": "before the merge" }),
                &["\"before the merge\""],
            ),
            (
                "empty summary with a value resolves to the value",
                serde_json::json!({ "summary": "", "value": "0" }),
                &["\"0\"^^<http://www.w3.org/2001/XMLSchema#decimal>"],
            ),
            (
                "null value and no summary → no rdf:object",
                serde_json::json!({ "value": null }),
                &[],
            ),
            (
                "a typing conclusion keeps its class as a string",
                serde_json::json!({ "class": "gas_well" }),
                &["\"gas_well\""],
            ),
            (
                "object with no recognised key → no rdf:object",
                serde_json::json!({ "confidence": 0.9 }),
                &[],
            ),
        ];

        for (what, value, expected_objects) in cases {
            let mut f = fact(5);
            f.predicate_id = Some(id(4));
            f.object_id = None;
            f.object_value = Some(value.clone());
            for format in [Format::Turtle, Format::JsonLd] {
                let quads = export(format, |sink, names, vocab| {
                    emit_fact(sink, names, vocab, &f, at("2026-06-01T00:00:00Z")).unwrap();
                });
                let got: Vec<String> = objects(&quads, STMT, rdf::OBJECT.as_str());
                assert_eq!(
                    got, *expected_objects,
                    "{what} (format={format:?}): got {got:?} expected {expected_objects:?}"
                );
            }
        }
    }

    #[test]
    fn an_imported_class_keeps_its_own_iri() {
        let quads = export(Format::Turtle, |_, _, _| {});
        // A class imported from schema.org still exports as schema:Person
        assert!(has(
            &quads,
            "<https://schema.org/Person>",
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "http://www.w3.org/2002/07/owl#Class"
        ));
        // A class the ontology made itself is minted with its key, which is readable
        assert!(has(
            &quads,
            "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:class:team>",
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "http://www.w3.org/2002/07/owl#Class"
        ));
        // Axioms copied as they are: the consistency check runs on them, and a reader must be able to recompute it themselves
        assert!(has(
            &quads,
            &format!("<{WORKS_FOR}>"),
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "http://www.w3.org/2002/07/owl#FunctionalProperty"
        ));
    }

    /// A relation changed back to state and re-imported must also be state: import reads an unwritten `utopia:temporal` as "does not move"
    #[test]
    fn a_state_relation_exports_its_temporal() {
        let quads = export(Format::Turtle, |_, _, _| {});
        assert!(has(
            &quads,
            &format!("<{WORKS_FOR}>"),
            "urn:utopia:ns:temporal",
            "\"state\""
        ));
    }

    /// RC-008: what a file declared about how a type is shown comes back out of the export
    /// as the same literals, and the import reads them again
    #[test]
    fn display_hints_round_trip_through_the_export() {
        let names = Names::new(kb(), None).unwrap();
        let mut claim = class(1, "claim", Some("http://example.org/Claim"));
        claim.display_hints =
            serde_json::json!({"panel": "quote", "chain_first": "out:checked_by"});
        let mut outcome = relation(
            2,
            "outcome",
            Some("http://example.org/outcome"),
            "attribute",
        );
        outcome.display_hints =
            serde_json::json!({"role": "status", "tones": "passed=ok,failed=danger"});
        let vocab = vocabulary(
            &names,
            std::slice::from_ref(&claim),
            std::slice::from_ref(&outcome),
        );
        let buf = SharedBuf::default();
        let mut sink = Sink::new(Format::Turtle, buf.clone());
        emit_class(&mut sink, &vocab, &claim).unwrap();
        emit_relation(&mut sink, &vocab, &outcome).unwrap();
        sink.finish().unwrap();
        let bytes = buf.take();
        let read = utopia_ingest::ontology_rdf::project(
            &bytes,
            utopia_ingest::ontology_rdf::RdfFormat::Turtle,
        )
        .expect("exported file must parse back");
        let pairs = |items: &[(&str, &str)]| -> std::collections::BTreeMap<String, String> {
            items
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            read.classes[0].display_hints,
            pairs(&[("panel", "quote"), ("chain_first", "out:checked_by")])
        );
        assert_eq!(
            read.properties[0].display_hints,
            pairs(&[("role", "status"), ("tones", "passed=ok,failed=danger")])
        );
        assert!(read.unprojected.is_empty(), "{:?}", read.unprojected);
    }

    #[test]
    fn a_live_fact_is_both_a_statement_and_a_triple() {
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &fact(5), at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert!(has(
            &quads,
            STMT,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#Statement"
        ));
        assert!(has(
            &quads,
            STMT,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#subject",
            SUBJ
        ));
        assert!(
            has(&quads, SUBJ, WORKS_FOR, OBJ),
            "仍然成立的事实要有一条平铺三元组——不看具体化的消费者靠它拿到现状"
        );
        assert_eq!(
            objects(&quads, STMT, "urn:utopia:ns:confidence"),
            vec!["\"0.90\"^^<http://www.w3.org/2001/XMLSchema#decimal>"]
        );
    }

    #[test]
    fn a_closed_or_retracted_fact_is_a_statement_only() {
        // Interval closed: on the world axis it has already ended
        let mut closed = fact(5);
        closed.valid_from = Some(at("2023-01-01T00:00:00Z"));
        closed.valid_from_precision = Some("day".into());
        closed.valid_to = Some(at("2024-07-01T00:00:00Z"));
        closed.valid_to_precision = Some("day".into());
        // The interval read out follows the original text (0022): if both ends are given, they are those
        closed.holds_from = Some(at("2023-01-01T00:00:00Z"));
        closed.holds_to = Some(at("2024-07-01T00:00:00Z"));
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &closed, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert!(
            !has(&quads, SUBJ, WORKS_FOR, OBJ),
            "结束了的关系不该以现在时写出去——那正是导出会骗人的地方"
        );
        assert_eq!(
            objects(&quads, STMT, "https://schema.org/validThrough"),
            vec!["\"2024-07-01\"^^<http://www.w3.org/2001/XMLSchema#date>"]
        );

        // Withdrawn on the recording axis: on the world axis it may even still be "open", but we no longer consider it so
        let mut retracted = fact(5);
        retracted.invalidated_at = Some(at("2026-03-01T00:00:00Z"));
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &retracted, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert!(
            !has(&quads, SUBJ, WORKS_FOR, OBJ),
            "retracted ones do not produce flat triples"
        );
        assert_eq!(
            objects(&quads, STMT, "http://www.w3.org/ns/prov#invalidatedAtTime"),
            vec!["\"2026-03-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"]
        );
    }

    #[test]
    fn record_axis_subseconds_round_trip_without_changing_world_precision() {
        for timestamp in [
            "2026-09-20T00:00:00Z",
            "2026-09-20T00:00:00.100Z",
            "2026-09-20T00:00:00.100001Z",
            "2026-09-20T00:00:00.100002Z",
            "2026-09-20T00:00:00.123456789Z",
        ] {
            let original = at(timestamp);
            let literal = dt(original);
            assert_eq!(literal.datatype(), xsd::DATE_TIME);
            assert_eq!(literal.value().parse::<DateTime<Utc>>().unwrap(), original);
        }
        assert_eq!(
            dt(at("2026-09-20T00:00:00Z")).value(),
            "2026-09-20T00:00:00Z"
        );
        let instant = at("2026-09-20T12:34:56.123456Z");
        for (precision, lexical, datatype) in [
            ("year", "2026", xsd::G_YEAR),
            ("month", "2026-09", xsd::G_YEAR_MONTH),
            ("day", "2026-09-20", xsd::DATE),
            ("hour", "2026-09-20T12:34:56Z", xsd::DATE_TIME),
            ("minute", "2026-09-20T12:34:56Z", xsd::DATE_TIME),
            ("second", "2026-09-20T12:34:56Z", xsd::DATE_TIME),
        ] {
            assert_eq!(
                world_time(instant, Some(precision)),
                Literal::new_typed_literal(lexical, datatype)
            );
        }
    }

    #[test]
    fn a_year_stays_a_year() {
        let mut coarse = fact(5);
        coarse.valid_from = Some(at("2023-01-01T00:00:00Z"));
        coarse.valid_from_precision = Some("year".into());
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &coarse, at("2026-06-01T00:00:00Z")).unwrap();
        });
        // Only the year was measured, so write gYear. Writing everything as xsd:date would give the ledger a certainty it never had
        assert_eq!(
            objects(&quads, STMT, "https://schema.org/validFrom"),
            vec!["\"2023\"^^<http://www.w3.org/2001/XMLSchema#gYear>"]
        );
    }

    #[test]
    fn an_ended_but_undated_relation_says_so() {
        let mut ended = fact(5);
        ended.valid_to = None;
        ended.valid_to_precision = Some("unknown".into());
        // Ended, but not known which day: the end read out is the anchor (the date of the document that stated it), which is how the SQL projects it
        ended.holds_to = ended.holds_from;
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &ended, at("2026-06-01T00:00:00Z")).unwrap();
        });
        // "Ended, but not known which day" must not be flattened into "still holds"
        assert!(has(
            &quads,
            STMT,
            "urn:utopia:ns:endedUnknown",
            "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
        ));
        assert!(!has(&quads, SUBJ, WORKS_FOR, OBJ));
    }

    #[test]
    fn a_predicate_the_ontology_never_accepted_is_not_invented() {
        let mut bare = fact(5);
        bare.predicate_id = None;
        bare.surface_predicate = Some("acquired".into());
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &bare, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert!(
            objects(
                &quads,
                STMT,
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#predicate"
            )
            .is_empty(),
            "本体没有这条关系就不铸一个谓词出来（0010）"
        );
        assert_eq!(
            objects(&quads, STMT, "urn:utopia:ns:proposedPredicate"),
            vec!["\"acquired\""]
        );
    }

    #[test]
    fn an_attribute_carries_its_datatype() {
        let mut attr = fact(5);
        attr.predicate_id = Some(id(4));
        attr.object_id = None;
        attr.object_value = Some(serde_json::json!({ "value": 42, "unit": "people" }));
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &attr, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert_eq!(
            objects(
                &quads,
                STMT,
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#object"
            ),
            vec!["\"42\"^^<http://www.w3.org/2001/XMLSchema#decimal>"]
        );
    }

    /// A relative value on a date property (#681 §4) is exported as a plain string, and the statement carries a separate line saying it is relative;
    /// a literal written as xsd:date is invalid, and a strict parser would reject the whole file
    #[test]
    fn a_relative_deadline_is_a_string_that_says_it_is_relative() {
        let dated =
            literal_value(&serde_json::json!({ "value": "2020-06-23" }), Some("date")).unwrap();
        assert_eq!(dated.datatype(), xsd::DATE);
        let relative = literal_value(
            &serde_json::json!({ "value": "45 days after the Trigger Date", "relative": true }),
            Some("date"),
        )
        .unwrap();
        assert_eq!(relative.datatype(), xsd::STRING);
        assert_eq!(relative.value(), "45 days after the Trigger Date");

        let mut attr = fact(5);
        attr.predicate_id = Some(id(4));
        attr.object_id = None;
        attr.object_value = Some(
            serde_json::json!({ "value": "45 days after the Trigger Date", "relative": true }),
        );
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &attr, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert!(has(
            &quads,
            STMT,
            "urn:utopia:ns:relativeValue",
            "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
        ));
    }

    /// Business-rule conclusions must also appear in the export, and their object is a **literal**.
    ///
    /// This one guards against a silent loss: the fact-fetching side used to `JOIN rules`, but the business rule's
    /// `rule_id` is NULL, so the whole conclusion would be blocked out of the file by the inner join, while 0020 promises that
    /// "an auditor can read everything without relying on us". The activity's label must also be the rule's own name,
    /// since "business" means nothing to the reader
    #[test]
    fn a_rule_conclusion_reaches_the_export_as_a_literal() {
        let derived = ExportDerived {
            id: id(7),
            subject_id: id(10),
            predicate_id: id(2),
            object_id: None,
            object_value: Some(serde_json::json!({ "class": "gas_well" })),
            rule_id: None,
            attribute_rule_id: Some(id(9)),
            valid_from: None,
            valid_from_precision: None,
            valid_to: None,
            valid_to_precision: None,
            derived_at: at("2026-02-01T00:00:00Z"),
            invalidated_at: None,
            confidence: 0.9,
            rule: "business".into(),
            rule_predicate: None,
            rule_name: Some("Gas-bearing well".into()),
            premises: vec![id(5)],
            premises_derived: Vec::new(),
            subject_kb: Some(kb()),
            object_kb: None,
            predicate_kb: Some(kb()),
            rule_kb: None,
            attribute_rule_kb: Some(kb()),
            foreign_fact_premise: false,
            foreign_derived_premise: false,
            subject_merged: false,
            object_merged: false,
        };
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_derived(sink, names, vocab, &derived).unwrap();
        });
        let stmt = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:derived:07070707-0707-0707-0707-070707070707>";
        // The derived flag is still there: it is still inferred, not asserted by anyone
        assert!(has(
            &quads,
            stmt,
            "urn:utopia:ns:derived",
            "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
        ));
        // The object is a literal, not an entity IRI: the classification conclusion `{"class": …}` is resolved by key to a class name,
        // and written as xsd:string. The old code serialized the whole object as a literal like `{"class":"gas_well"}` (#831)
        let obj = objects(
            &quads,
            stmt,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#object",
        );
        assert_eq!(obj.len(), 1, "conclusion must have an object");
        assert_eq!(
            obj[0], "\"gas_well\"",
            "分类结论的宾语该是类名本身的字符串字面量，拿到的是 {}",
            obj[0]
        );
        // The rule resource states its own family (#902): a business rule, with no axiom kind to speak of
        let rule = Names::new(kb(), None).unwrap().rule(id(9)).to_string();
        assert!(has(
            &quads,
            &rule,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "<urn:utopia:ns:BusinessRule>"
        ));
        assert!(objects(&quads, &rule, "urn:utopia:ns:axiomKind").is_empty());
        assert!(objects(&quads, &rule, "urn:utopia:ns:declaredOn").is_empty());
        // Premises still attached: an auditor following prov:used reaches the two readings
        assert_eq!(
            objects(&quads, stmt, "http://www.w3.org/ns/prov#used").len(),
            1
        );
        // The activity's label is the rule's name
        let rule_iri =
            "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:rule:09090909-0909-0909-0909-090909090909>";
        assert_eq!(
            objects(
                &quads,
                rule_iri,
                "http://www.w3.org/2000/01/rdf-schema#label"
            ),
            vec!["\"Gas-bearing well\""],
            "推理活动要以规则名示人"
        );
    }

    #[test]
    fn declared_property_links_are_local_explicit_and_order_independent() {
        let root = relation(21, "root", Some("https://example.test/root"), "relation");
        let mut inverse = relation(22, "inverse", None, "relation");
        inverse.inverse_of = Some(root.id);
        let mut child = relation(23, "child", None, "relation");
        child.sub_property_of = Some(root.id);
        let mut leaf = relation(24, "leaf", None, "relation");
        leaf.sub_property_of = Some(child.id);
        let mut missing = relation(25, "unresolved", None, "relation");
        missing.inverse_of = Some(id(98));
        missing.sub_property_of = Some(id(99));
        let mut relations = vec![root, inverse, child, leaf, missing];
        let mut sets = Vec::new();
        for reverse in [false, true] {
            if reverse {
                relations.reverse();
            }
            for format in [Format::Turtle, Format::JsonLd] {
                let quads = export(format, |sink, names, _| {
                    let vocab = vocabulary(names, &[], &relations);
                    for r in &relations {
                        emit_relation(sink, &vocab, r).unwrap();
                    }
                });
                let names = Names::new(kb(), None).unwrap();
                let iri = |n| names.relation(relations.iter().find(|r| r.id == id(n)).unwrap());
                let expected: std::collections::HashSet<_> = [
                    (iri(22).into(), owl("inverseOf"), Term::from(iri(21))),
                    (
                        iri(23).into(),
                        nn(rdfs::SUB_PROPERTY_OF.as_str()),
                        Term::from(iri(21)),
                    ),
                    (
                        iri(24).into(),
                        nn(rdfs::SUB_PROPERTY_OF.as_str()),
                        Term::from(iri(23)),
                    ),
                ]
                .into_iter()
                .collect();
                let links: std::collections::HashSet<_> = quads
                    .iter()
                    .filter(|q| {
                        q.predicate == owl("inverseOf") || q.predicate == rdfs::SUB_PROPERTY_OF
                    })
                    .map(|q| (q.subject.clone(), q.predicate.clone(), q.object.clone()))
                    .collect();
                assert_eq!(
                    links, expected,
                    "only stored, local links should be emitted"
                );
                sets.push(quads.into_iter().collect::<std::collections::HashSet<_>>());
            }
        }
        for set in &sets[1..] {
            assert_eq!(&sets[0], set);
        }
    }

    #[test]
    fn a_derivation_says_it_is_one_and_names_its_premises() {
        let derived = ExportDerived {
            id: id(7),
            subject_id: id(10),
            predicate_id: id(2),
            object_id: Some(id(11)),
            object_value: None,
            rule_id: Some(id(8)),
            attribute_rule_id: None,
            valid_from: None,
            valid_from_precision: None,
            valid_to: None,
            valid_to_precision: None,
            derived_at: at("2026-02-01T00:00:00Z"),
            invalidated_at: None,
            confidence: 0.8,
            rule: "transitive".into(),
            rule_predicate: Some(id(2)),
            rule_name: None,
            premises: vec![id(5)],
            premises_derived: vec![id(6)],
            subject_kb: Some(kb()),
            object_kb: Some(kb()),
            predicate_kb: Some(kb()),
            rule_kb: Some(kb()),
            attribute_rule_kb: None,
            foreign_fact_premise: false,
            foreign_derived_premise: false,
            subject_merged: false,
            object_merged: false,
        };
        for format in [Format::Turtle, Format::JsonLd] {
            let quads = export(format, |sink, names, vocab| {
                emit_derived(sink, names, vocab, &derived).unwrap();
            });
            let stmt = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:derived:07070707-0707-0707-0707-070707070707>";
            assert!(has(
                &quads,
                stmt,
                "urn:utopia:ns:derived",
                "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
            ));
            assert_eq!(
                objects(&quads, stmt, "http://www.w3.org/ns/prov#used"),
                vec![
                    STMT.to_string(),
                    Names::new(kb(), None).unwrap().derived(id(6)).to_string()
                ]
            );
            assert!(
                !has(&quads, SUBJ, WORKS_FOR, OBJ),
                "推出来的边不写成平铺三元组：那会让人把引擎的结论当成文档里的话"
            );
            // The rule resource states its own family, kind and the predicate its declaration sits on (#902): the reader no longer
            // guesses from rdfs:label. Here the declared predicate is the conclusion's predicate (transitive), pointing at the same IRI
            let rule = Names::new(kb(), None).unwrap().rule(id(8)).to_string();
            assert!(has(
                &quads,
                &rule,
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                "<urn:utopia:ns:AxiomRule>"
            ));
            assert_eq!(
                objects(&quads, &rule, "urn:utopia:ns:axiomKind"),
                vec!["\"transitive\""]
            );
            assert_eq!(
                objects(&quads, &rule, "urn:utopia:ns:declaredOn"),
                objects(
                    &quads,
                    stmt,
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#predicate"
                )
            );
        }
    }

    #[test]
    fn evidence_points_back_at_the_document_and_the_sentence() {
        let mut cited = fact(5);
        cited.documents = vec![id(12)];
        cited.quotes = vec!["Lin Zhao joined Acme in 2023.".into()];
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &cited, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert_eq!(
            objects(&quads, STMT, "http://www.w3.org/ns/prov#wasDerivedFrom"),
            vec!["<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:document:0c0c0c0c-0c0c-0c0c-0c0c-0c0c0c0c0c0c>"]
        );
        assert_eq!(
            objects(&quads, STMT, "urn:utopia:ns:quote"),
            vec!["\"Lin Zhao joined Acme in 2023.\""]
        );
    }

    #[test]
    fn json_ld_carries_the_same_triples() {
        let emit = |sink: &mut Sink, names: &Names, vocab: &Vocabulary| {
            emit_fact(sink, names, vocab, &fact(5), at("2026-06-01T00:00:00Z")).unwrap();
        };
        let ttl = export(Format::Turtle, emit);
        let jsonld = export(Format::JsonLd, emit);
        assert_eq!(
            ttl.len(),
            jsonld.len(),
            "两种格式是同一张图的两种写法，三元组数目必须一样"
        );
        assert!(has(&jsonld, SUBJ, WORKS_FOR, OBJ));
    }

    #[test]
    fn a_published_deployment_can_mint_http_iris() {
        let names = Names::new(kb(), Some("https://acme.example/utopia/")).unwrap();
        assert_eq!(
            names.entity(id(10)).as_str(),
            "https://acme.example/utopia/kb/01a06dc4-f40a-7013-b09f-1b499e2e7441/entity/0a0a0a0a-0a0a-0a0a-0a0a-0a0a0a0a0a0a"
        );
        // A non-http base is refused: the result would be a file nobody can resolve
        assert!(Names::new(kb(), Some("javascript:alert(1)")).is_err());
        assert!(Names::new(kb(), Some("https://acme.example/a b")).is_err());
    }

    const CONFLICT: &str = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:conflict:07070707-0707-0707-0707-070707070707>";
    const VIOLATION: &str = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:violation:08080808-0808-0808-0808-080808080808>";

    fn conflict(status: &str, resolution: Option<&str>) -> ExportFactConflict {
        ExportFactConflict {
            id: id(7),
            old_fact_id: id(5),
            new_fact_id: id(6),
            reason: "simultaneous".into(),
            created_at: at("2026-03-10T00:00:00Z"),
            status: status.into(),
            resolution: resolution.map(str::to_string),
            resolved_at: (status != "open").then(|| at("2026-03-20T00:00:00Z")),
            old_fact_kb: Some(kb()),
            new_fact_kb: Some(kb()),
        }
    }

    fn violation(
        kind: &str,
        status: &str,
        resolution: Option<&str>,
        path: Vec<Uuid>,
    ) -> ExportAxiomViolation {
        ExportAxiomViolation {
            id: id(8),
            kind: kind.into(),
            left_fact: id(5),
            right_fact: id(6),
            path,
            status: status.into(),
            resolution: resolution.map(str::to_string),
            detected_at: at("2026-03-15T00:00:00Z"),
            decided_at: (status != "open").then(|| at("2026-03-20T00:00:00Z")),
            criterion_predicate: (status == "open").then_some(id(2)),
            criterion_axiom: (status == "open").then_some(kind.to_string()),
            left_kb: Some(kb()),
            right_kb: Some(kb()),
            criterion_predicate_kb: Some(kb()),
            foreign_path: false,
        }
    }

    #[test]
    fn a_contest_names_both_sides_and_its_disposition() {
        let open = conflict("open", None);
        let quads = export(Format::Turtle, |sink, names, _| {
            emit_fact_conflict(sink, names, &open).unwrap();
        });
        assert!(has(
            &quads,
            CONFLICT,
            rdf::TYPE.as_str(),
            "urn:utopia:ns:FactConflict"
        ));
        let names = Names::new(kb(), None).unwrap();
        // The two seats have direction: the old assertion is prior, the newly arrived one is incoming
        assert_eq!(
            objects(&quads, CONFLICT, "urn:utopia:ns:priorStatement"),
            vec![names.fact(id(5)).to_string()]
        );
        assert_eq!(
            objects(&quads, CONFLICT, "urn:utopia:ns:incomingStatement"),
            vec![names.fact(id(6)).to_string()]
        );
        assert_eq!(
            objects(&quads, CONFLICT, "urn:utopia:ns:reason"),
            vec!["\"simultaneous\""]
        );
        assert_eq!(
            objects(&quads, CONFLICT, "urn:utopia:ns:status"),
            vec!["\"open\""]
        );
        assert!(objects(&quads, CONFLICT, "urn:utopia:ns:resolution").is_empty());
        assert!(!objects(
            &quads,
            CONFLICT,
            "http://www.w3.org/ns/prov#generatedAtTime"
        )
        .is_empty());
        assert!(objects(&quads, CONFLICT, "urn:utopia:ns:closedAt").is_empty());

        // A person's verdict carries a verdict term; a withdrawn one is not a verdict: closedAt records the moment of withdrawal (0051),
        // and carries no resolution
        for (status, resolution) in [("resolved", Some("kept_both")), ("withdrawn", None)] {
            let c = conflict(status, resolution);
            let quads = export(Format::Turtle, |sink, names, _| {
                emit_fact_conflict(sink, names, &c).unwrap();
            });
            assert_eq!(
                objects(&quads, CONFLICT, "urn:utopia:ns:status"),
                vec![format!("\"{status}\"")]
            );
            assert_eq!(
                objects(&quads, CONFLICT, "urn:utopia:ns:resolution"),
                resolution
                    .map(|r| vec![format!("\"{r}\"")])
                    .unwrap_or_default()
            );
            assert!(!objects(&quads, CONFLICT, "urn:utopia:ns:closedAt").is_empty());
        }
    }

    #[test]
    fn an_open_violation_points_at_its_criterion() {
        let v = violation("asymmetry", "open", None, vec![]);
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_axiom_violation(sink, names, vocab, &v).unwrap();
        });
        assert!(has(
            &quads,
            VIOLATION,
            rdf::TYPE.as_str(),
            "urn:utopia:ns:AxiomViolation"
        ));
        assert_eq!(
            objects(&quads, VIOLATION, "urn:utopia:ns:kind"),
            vec!["\"asymmetry\""]
        );
        assert_eq!(
            objects(&quads, VIOLATION, "urn:utopia:ns:onStatement").len(),
            2,
            "违规必须指回两条事实"
        );
        // Criterion = the OWL term + the relation it lives on (0062)
        assert_eq!(
            objects(&quads, VIOLATION, "urn:utopia:ns:criterion"),
            vec!["<http://www.w3.org/2002/07/owl#AsymmetricProperty>"]
        );
        assert_eq!(
            objects(&quads, VIOLATION, "urn:utopia:ns:onRelation"),
            vec![format!("<{WORKS_FOR}>")]
        );
        assert!(!objects(&quads, VIOLATION, "urn:utopia:ns:detectedAt").is_empty());
        assert!(objects(&quads, VIOLATION, "urn:utopia:ns:closedAt").is_empty());
    }

    #[test]
    fn a_signature_violation_points_at_the_whole_signature() {
        // A signature's criterion is the pair of domain+range (0062): the row does not record which side broke,
        // so the contract does not pretend to know; both terms are emitted
        let v = violation("signature", "open", None, vec![]);
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_axiom_violation(sink, names, vocab, &v).unwrap();
        });
        let mut criterion = objects(&quads, VIOLATION, "urn:utopia:ns:criterion");
        criterion.sort();
        assert_eq!(
            criterion,
            vec![
                "<http://www.w3.org/2000/01/rdf-schema#domain>".to_string(),
                "<http://www.w3.org/2000/01/rdf-schema#range>".to_string()
            ]
        );
        assert_eq!(
            objects(&quads, VIOLATION, "urn:utopia:ns:onRelation"),
            vec![format!("<{WORKS_FOR}>")]
        );
    }

    #[test]
    fn a_closed_violation_does_not_claim_a_criterion() {
        // Resolved rows do not carry the criterion: the ontology that closed them may long since be gone (0062)
        let v = violation("signature", "resolved", Some("criterion_changed"), vec![]);
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_axiom_violation(sink, names, vocab, &v).unwrap();
        });
        assert!(objects(&quads, VIOLATION, "urn:utopia:ns:criterion").is_empty());
        assert!(objects(&quads, VIOLATION, "urn:utopia:ns:onRelation").is_empty());
        assert_eq!(
            objects(&quads, VIOLATION, "urn:utopia:ns:resolution"),
            vec!["\"criterion_changed\""]
        );
        assert!(!objects(&quads, VIOLATION, "urn:utopia:ns:closedAt").is_empty());
    }

    /// Walks an rdf:List and collects the members back in order
    fn list_items(quads: &[Quad], violation_iri: &str) -> Vec<String> {
        let nil: Term = nn(rdf::NIL.as_str()).into();
        let head = quads
            .iter()
            .find(|q| {
                q.subject.to_string() == violation_iri
                    && q.predicate.as_str() == "urn:utopia:ns:evidencePath"
            })
            .map(|q| q.object.clone())
            .expect("cycle must carry the path");
        let mut items = Vec::new();
        let mut node = head;
        while node != nil {
            let first = quads
                .iter()
                .find(|q| {
                    q.subject.to_string() == node.to_string()
                        && q.predicate.as_str() == rdf::FIRST.as_str()
                })
                .map(|q| q.object.clone())
                .expect("list node must have first");
            items.push(first.to_string());
            node = quads
                .iter()
                .find(|q| {
                    q.subject.to_string() == node.to_string()
                        && q.predicate.as_str() == rdf::REST.as_str()
                })
                .map(|q| q.object.clone())
                .expect("list node must have rest");
        }
        items
    }

    #[test]
    fn a_cycle_keeps_its_facts_in_order() {
        let path = vec![id(5), id(9), id(6)];
        let v = violation("cycle", "open", None, path.clone());
        let names = Names::new(kb(), None).unwrap();
        let expected: Vec<String> = path.iter().map(|f| names.fact(*f).to_string()).collect();
        for format in [Format::Turtle, Format::JsonLd] {
            let quads = export(format, |sink, names, vocab| {
                emit_axiom_violation(sink, names, vocab, &v).unwrap();
            });
            assert_eq!(
                list_items(&quads, VIOLATION),
                expected,
                "{format:?} 里环的顺序丢了"
            );
        }
    }

    #[test]
    fn contest_and_finding_survive_both_formats() {
        let emit = |sink: &mut Sink, names: &Names, vocab: &Vocabulary| {
            emit_fact_conflict(sink, names, &conflict("resolved", Some("rejected_new"))).unwrap();
            emit_axiom_violation(
                sink,
                names,
                vocab,
                &violation("asymmetry", "open", None, vec![id(5), id(9)]),
            )
            .unwrap();
        };
        let ttl = export(Format::Turtle, emit);
        let jsonld = export(Format::JsonLd, emit);
        assert_eq!(
            ttl.len(),
            jsonld.len(),
            "两种格式是同一张图的两种写法，三元组数目必须一样"
        );
        // Blank nodes are anonymous in each of the two formats, so sets cannot be compared: rather than comparing all non-anonymous ends,
        // the order of the cycle's evidence is checked by a_cycle_keeps_its_facts_in_order, once per format
        let named = |qs: &[Quad]| {
            qs.iter()
                .filter(|q| q.subject.is_named_node() && !matches!(q.object, Term::BlankNode(_)))
                .cloned()
                .collect::<std::collections::HashSet<_>>()
        };
        assert_eq!(named(&ttl), named(&jsonld));
    }
}
