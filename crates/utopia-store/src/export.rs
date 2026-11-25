//! Read surface used for export (0020).
//!
//! Written separately from the views in the UI, because the questions differ: the UI asks "what does it look like now",
//! while export must have **everything** — closed intervals, retracted rows, correction chains; not one may be missing, or the export becomes
//! a clean, confident graph, which is exactly how the things an audit needs to see get erased.
//!
//! Everything except the ontology is **paginated by id**: a KB can have hundreds of thousands of facts, and reading them all into memory before serializing
//! would blow up on exactly the deployments that need it most. The id is a uuid v7, so sorting by it sorts by write order.
//! The first page has no lower bound (`$2 IS NULL OR id > $2`): a uuid has no smaller sentinel to pad with —
//! `id > NIL` would forever keep out a legitimate row whose primary key is exactly NIL, while the
//! (document_id, version) locator pointing at it would still resolve, leaving an edge with no object.
//!
//! **One shared snapshot**: all the data reads take the caller's transaction (`&mut Transaction`), and the route side sets it
//! to read-only REPEATABLE READ — the health check, the vocabulary, and every page read the database as of the same moment.
//! If each page took its own connection from the pool, a rule committed after the vocabulary was sent would leave, in a derived page,
//! a `wasGeneratedBy` pointing at it — a reference to an ontology that is not in the graph.
//!
//! **Per-page validation uses the rows that were left behind**: each page query selects the referenced rows'
//! kb_id **atomically together with** the row itself, and validation runs in memory. It cannot "fetch a page, then ask the database
//! once more" — the second ask sees a state at another moment, and the rows left behind are no longer that state.
//!
//! The health check covers **every structural reference edge protected by 0070/0091** — not only those this export actually resolves.
//! Edges the export has not yet read (the rules table itself, statement attributes, temporal mentions, etc.) are also checked
//! first in the same snapshot: the catalog guard (migration_0070_runs_under_any_search_path.rs) verifies that every scan branch of
//! export_provenance_integrity.sql maps each scan branch to one protected
//! catalog edge: if a new protected reference is added later and not registered, the health check goes red straight away.

use chrono::{DateTime, Utc};
use sqlx::{Postgres, Transaction};
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// How many rows to fetch at once. Big enough to keep round trips down, small enough that one page cannot exhaust memory.
pub const PAGE: i64 = 500;

/// The only SQL for the provenance check: it runs at runtime, and the catalog guard reads this same one —
/// the edge set is registered in exactly one place, with no second list to keep aligned by hand
const PREFLIGHT_SQL: &str = include_str!("export_provenance_integrity.sql");

/// A class of reference whose provenance chain crosses out of its KB. Same-KB foreign keys check only the id, not the KB: a row in KB A
/// may reference an object in KB B, and the schema blocks nothing. The 0070 trigger blocks new rows; what this catches is
/// **bad rows already stored** and rows written past the trigger.
///
/// The disposition is always **reject the whole export**: minting another KB's object id under this KB's IRI
/// (`urn:utopia:kb:A:document:{B's document}`) is forging an identity — the file looks
/// complete but is actually dangling. A reference resolved by the vocabulary (predicate, attribute type, entity type) that crosses KBs
/// would silently vanish — a bad row is not let through either. Dropping one row or swapping the IRI are not among the options.
#[derive(Debug, sqlx::FromRow)]
pub struct CrossKbViolation {
    /// which edge: evidence.chunk | derivation.premise_fact | …
    pub edge: String,
    pub rows: i64,
}

fn cross_kb_error(violations: &[CrossKbViolation]) -> AppError {
    let detail = violations
        .iter()
        .map(|v| format!("{}: {} row(s)", v.edge, v.rows))
        .collect::<Vec<_>>()
        .join("; ");
    AppError::invalid_detail(
        "cross_kb_provenance",
        "export refused: KB-scoped provenance points outside the knowledge base",
        detail,
    )
}

/// A reference points at something **in this KB but not in the export set** (a merged-away entity is the only one that can be
/// absent — `entities_page` filters out rows with a non-empty `merged_into`). A same-KB but absent reference
/// cannot be given a different IRI, nor silently omitted: the whole export is rejected, the same disposition as a cross-KB reference.
fn unexported_error(violations: &[CrossKbViolation]) -> AppError {
    let detail = violations
        .iter()
        .map(|v| format!("{}: {} row(s)", v.edge, v.rows))
        .collect::<Vec<_>>()
        .join("; ");
    AppError::invalid_detail(
        "unexported_target",
        "export refused: a reference points at a row that is not in this KB's exported set",
        detail,
    )
}

fn tally(violations: &mut Vec<CrossKbViolation>, edge: &str, rows: i64) {
    if rows > 0 {
        violations.push(CrossKbViolation {
            edge: edge.into(),
            rows,
        });
    }
}

/// Judging a reference column: **whoever was left behind is who gets checked**. `ref_kb` is selected atomically together with
/// the row by the page query — another KB or dangling (NULL) does not count as this KB; in every case it is a violation
fn foreign(ref_kb: Option<Uuid>, kb_id: Uuid) -> bool {
    ref_kb != Some(kb_id)
}

/// Provenance check before export. Counts out-of-bounds references class by class; a single row means the whole export is rejected.
/// There are two classes of out-of-bounds, each with its own error: **other KB / dangling** (`cross_kb`) and **same KB but not in the export set**
/// (`unexported` — merged-away entities). It reports only which edge is broken and how many rows — which rows are broken
/// is the database's business and does not go into the error shown to the exporter.
///
/// Edges scanned = **all structural reference edges protected by 0070/0091** (export_provenance_integrity.sql,
/// each branch carries an `@edge`/`@filter` marker for the catalog guard to verify) — including edges export has not yet
/// serialized: if any protected same-KB reference on the ledger is broken, this export cannot be trusted,
/// so it is better to reject the whole thing. **Must run inside the export's own transaction** (REPEATABLE READ):
/// the check and every page query must see the same snapshot; switching connections after the check would leave a gap between two moments,
/// in which a just-committed bad row could slip through
pub async fn provenance_integrity(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
) -> AppResult<()> {
    #[derive(sqlx::FromRow)]
    struct ScanViolation {
        edge: String,
        kind: String,
        rows: i64,
    }
    let violations: Vec<ScanViolation> = sqlx::query_as(PREFLIGHT_SQL)
        .bind(kb_id)
        .fetch_all(&mut **tx)
        .await?;
    let cross_kb: Vec<CrossKbViolation> = violations
        .iter()
        .filter(|v| v.kind == "cross_kb")
        .map(|v| CrossKbViolation {
            edge: v.edge.clone(),
            rows: v.rows,
        })
        .collect();
    let unexported: Vec<CrossKbViolation> = violations
        .iter()
        .filter(|v| v.kind == "unexported")
        .map(|v| CrossKbViolation {
            edge: v.edge.clone(),
            rows: v.rows,
        })
        .collect();
    if !cross_kb.is_empty() {
        return Err(cross_kb_error(&cross_kb));
    }
    if !unexported.is_empty() {
        return Err(unexported_error(&unexported));
    }
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportClass {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub description: String,
    /// An imported class keeps its original IRI — a schema.org Organization still exports as
    /// `schema:Organization`, matching the vocabulary the reader has.
    pub iri: Option<String>,
    pub parents: Vec<Uuid>,
    pub disjoint: Vec<Uuid>,
    /// The `utopia:*` display hints (RC-008), written back one literal per entry
    pub display_hints: serde_json::Value,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportRelation {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub description: String,
    pub iri: Option<String>,
    /// relation | attribute (the latter's value domain is literal values)
    pub kind: String,
    pub datatype: Option<String>,
    pub unit: Option<String>,
    pub temporal: String,
    pub functional: bool,
    pub inverse_functional: bool,
    pub is_transitive: bool,
    pub is_symmetric: bool,
    pub is_asymmetric: bool,
    pub is_irreflexive: bool,
    /// Self-reference within the same table: exports owl:inverseOf / rdfs:subPropertyOf. Whether existing rows are legal is governed
    /// by the deferred constraints in the migration; here we only carry the set along (cross-KB / dangling → reject)
    pub inverse_of: Option<Uuid>,
    pub sub_property_of: Option<Uuid>,
    pub domains: Vec<Uuid>,
    pub ranges: Vec<Uuid>,
    /// The `utopia:*` display hints (RC-008), written back one literal per entry
    pub display_hints: serde_json::Value,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportEntity {
    pub id: Uuid,
    pub canonical_name: String,
    pub type_id: Option<Uuid>,
    /// The KB of the class that type_id points at (selected by a LEFT JOIN). Cross-KB / dangling → reject:
    /// serialization looks the class up in this KB's vocabulary by id, and if it is not found the type would be silently dropped
    pub type_kb: Option<Uuid>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportFact {
    pub id: Uuid,
    pub subject_id: Uuid,
    pub predicate_id: Option<Uuid>,
    /// The model's own words when the ontology did not catch this relation (0010). Exported so the reader can see
    /// "the system heard this word at the time, and the vocabulary has nothing for it"
    pub surface_predicate: Option<String>,
    pub object_id: Option<Uuid>,
    pub object_value: Option<serde_json::Value>,
    /// Attributes on the edge (0037), filled in by fact id after loading
    #[sqlx(skip)]
    pub qualifiers: Vec<utopia_core::models::FactQualifier>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_from_precision: Option<String>,
    pub valid_to: Option<DateTime<Utc>>,
    pub valid_to_precision: Option<String>,
    /// The read-out interval (0022): the "still holds now" triple is judged by it, rather than interpreting NULL itself
    pub holds_from: Option<DateTime<Utc>>,
    pub holds_to: Option<DateTime<Utc>>,
    pub recorded_at: DateTime<Utc>,
    pub invalidated_at: Option<DateTime<Utc>>,
    pub confidence: f32,
    /// Implied rows computed by a rule (piece 5 of decision 3 of 0044): a reader must be able to tell that it is not directly stated by a statement
    pub implied: bool,
    pub supersedes: Option<Uuid>,
    pub documents: Vec<Uuid>,
    pub quotes: Vec<String>,
    /// Source of the evidence text (0040), deduplicated: whether a statement's quotation came from a scan, a transcription, or an image description
    pub quote_origins: Vec<String>,
    /// The fields below are the KB of the referenced rows, selected atomically together with the row itself.
    /// A reference to another KB or a dangling (NULL) one must not be minted under this KB's IRI, nor silently skipped
    pub subject_kb: Option<Uuid>,
    pub object_kb: Option<Uuid>,
    pub predicate_kb: Option<Uuid>,
    pub supersedes_kb: Option<Uuid>,
    /// Whether documents[] contains a document pointer to another KB or a dangling one
    pub foreign_document: bool,
    /// Whether the passage joined for the quote contains one from another KB or dangling (quote_origins' sources would silently vanish)
    pub foreign_chunk: bool,
    /// Subject/object point at a **merged** entity: same KB but not in the export set (rows with merged_into IS NOT NULL
    /// are not exported by entities_page). Selected atomically together with the row itself
    pub subject_merged: bool,
    pub object_merged: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportDerived {
    pub id: Uuid,
    pub subject_id: Uuid,
    pub predicate_id: Uuid,
    /// A literal-valued conclusion (business rule's classification and attributes) has no entity object (0021)
    pub object_id: Option<Uuid>,
    pub object_value: Option<serde_json::Value>,
    /// Axiom rule. For a business-rule-inferred one it is None — its identity is on attribute_rule_id
    pub rule_id: Option<Uuid>,
    pub attribute_rule_id: Option<Uuid>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_from_precision: Option<String>,
    pub valid_to: Option<DateTime<Utc>>,
    pub valid_to_precision: Option<String>,
    pub derived_at: DateTime<Utc>,
    pub invalidated_at: Option<DateTime<Utc>>,
    pub confidence: f32,
    /// transitive | symmetric | inverse | sub_property, or business
    pub rule: String,
    /// The predicate on which the axiom rule is declared (`rules.predicate_id`). For inverse and sub_property it is
    /// not the conclusion's predicate, so the export must state it (0020 revision of 2026-09-25, #902). None for business rules
    pub rule_predicate: Option<Uuid>,
    /// The name of the business rule, carried into RDF as the label of this inference activity
    pub rule_name: Option<String>,
    /// Premise facts. An audit follows this down to the sentence
    pub premises: Vec<Uuid>,
    /// Those premises that are **another derivation** (0030). Kept separate from the column above, because the reader
    /// needs to know which table to continue down; if folded into one column, a chain would break in the export
    pub premises_derived: Vec<Uuid>,
    /// The KB of the referenced row, selected atomically together with the row itself
    pub subject_kb: Option<Uuid>,
    pub object_kb: Option<Uuid>,
    pub predicate_kb: Option<Uuid>,
    /// The KB of the rule row that rule_id / attribute_rule_id points at — it is minted as wasGeneratedBy
    /// the Activity IRI
    pub rule_kb: Option<Uuid>,
    pub attribute_rule_kb: Option<Uuid>,
    /// Whether premises[] / premises_derived[] contains a premise from another KB or dangling (the two kinds report edges separately)
    pub foreign_fact_premise: bool,
    pub foreign_derived_premise: bool,
    /// Subject/object points at a merged entity: same KB but not in the export set
    pub subject_merged: bool,
    pub object_merged: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportDocument {
    pub id: Uuid,
    pub filename: String,
    pub external_key: Option<String>,
    pub doc_time: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// A deleted document keeps a tombstone (#268). In the export it is still present, but already ended on the recording axis
    pub deleted_at: Option<DateTime<Utc>>,
}

pub async fn classes(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
) -> AppResult<Vec<ExportClass>> {
    let classes: Vec<ExportClass> = sqlx::query_as(
        "SELECT t.id, t.key, t.label, t.description, t.iri, t.display_hints,
                COALESCE(ARRAY(SELECT p.parent_id FROM entity_type_parents p
                                WHERE p.child_id = t.id ORDER BY p.parent_id), '{}') AS parents,
                COALESCE(ARRAY(SELECT CASE WHEN d.a_id = t.id THEN d.b_id ELSE d.a_id END
                                 FROM entity_type_disjoint d
                                WHERE d.kb_id = $1 AND (d.a_id = t.id OR d.b_id = t.id)
                                ORDER BY 1), '{}') AS disjoint
           FROM entity_types t WHERE t.kb_id = $1 ORDER BY t.key",
    )
    .bind(kb_id)
    .fetch_all(&mut **tx)
    .await?;
    // Parent classes and disjoint classes are looked up by id later in this KB's vocabulary — if not found, they would be silently dropped.
    // Those that can be resolved are resolved in place: the full vocabulary set is in hand, so a reference not in the set is cross-KB / dangling
    let own: std::collections::HashSet<Uuid> = classes.iter().map(|c| c.id).collect();
    let mut violations = Vec::new();
    for c in &classes {
        let bad_parents = c.parents.iter().filter(|p| !own.contains(p)).count() as i64;
        let bad_disjoint = c.disjoint.iter().filter(|p| !own.contains(p)).count() as i64;
        tally(&mut violations, "class.parent", bad_parents);
        tally(&mut violations, "class.disjoint", bad_disjoint);
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    Ok(classes)
}

pub async fn relations(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
) -> AppResult<Vec<ExportRelation>> {
    let relations: Vec<ExportRelation> = sqlx::query_as(
        "SELECT r.id, r.key, r.label, r.description, r.iri, r.kind, r.datatype, r.unit,
                r.temporal, r.functional, r.inverse_functional,
                r.is_transitive, r.is_symmetric, r.is_asymmetric, r.is_irreflexive,
                r.inverse_of, r.sub_property_of, r.display_hints,
                COALESCE(ARRAY(SELECT d.entity_type_id FROM relation_type_domains d
                                WHERE d.relation_type_id = r.id ORDER BY 1), '{}') AS domains,
                COALESCE(ARRAY(SELECT g.entity_type_id FROM relation_type_ranges g
                                WHERE g.relation_type_id = r.id ORDER BY 1), '{}') AS ranges
           FROM relation_types r WHERE r.kb_id = $1 ORDER BY r.key",
    )
    .bind(kb_id)
    .fetch_all(&mut **tx)
    .await?;
    // The class ids of domain/range and the relation ids of inverse/sub_property must both be looked up in this KB's vocabulary —
    // if not found, the axiom is silently dropped. **Both complete sets are in hand**, so a reference not in the set is
    // cross-KB / dangling
    let own_types: Vec<Uuid> = sqlx::query_scalar(
        "SELECT COALESCE(ARRAY(SELECT id FROM entity_types WHERE kb_id = $1), '{}')",
    )
    .bind(kb_id)
    .fetch_one(&mut **tx)
    .await?;
    let own: std::collections::HashSet<Uuid> = own_types.into_iter().collect();
    let own_rel: std::collections::HashSet<Uuid> = relations.iter().map(|r| r.id).collect();
    let mut violations = Vec::new();
    for r in &relations {
        let bad_domains = r.domains.iter().filter(|t| !own.contains(t)).count() as i64;
        let bad_ranges = r.ranges.iter().filter(|t| !own.contains(t)).count() as i64;
        tally(&mut violations, "relation.domain", bad_domains);
        tally(&mut violations, "relation.range", bad_ranges);
        if let Some(t) = r.inverse_of {
            tally(
                &mut violations,
                "relation.inverse",
                (!own_rel.contains(&t)) as i64,
            );
        }
        if let Some(t) = r.sub_property_of {
            tally(
                &mut violations,
                "relation.sub_property",
                (!own_rel.contains(&t)) as i64,
            );
        }
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    Ok(relations)
}

/// Merged-away entities are not exported: they are no longer a thing; their facts moved long ago to the one that remains.
pub async fn entities_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<Uuid>,
) -> AppResult<Vec<ExportEntity>> {
    let page: Vec<ExportEntity> = sqlx::query_as(
        "SELECT e.id, e.canonical_name, e.type_id, t.kb_id AS type_kb
           FROM entities e LEFT JOIN entity_types t ON t.id = e.type_id
          WHERE e.kb_id = $1 AND e.merged_into IS NULL
            AND ($2 IS NULL OR e.id > $2)
          ORDER BY e.id LIMIT $3",
    )
    .bind(kb_id)
    .bind(after)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    for e in &page {
        if e.type_id.is_some() && foreign(e.type_kb, kb_id) {
            tally(&mut violations, "entity.type", 1);
        }
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    Ok(page)
}

/// **Does not filter `invalidated_at`.** Retracted rows, rows overwritten by a correction, and rows whose interval has long since closed are all in there
/// — each carries its moments on both axes, and the reader judges for themselves whether it held at the time (0019, 0020).
pub async fn facts_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<Uuid>,
) -> AppResult<Vec<ExportFact>> {
    let mut facts: Vec<ExportFact> = sqlx::query_as(&format!(
        "SELECT f.id, f.subject_id, f.predicate_id,
                fact_surface_predicate(f.id) AS surface_predicate,
                f.object_id, f.object_value,
                f.valid_from, f.valid_from_precision, f.valid_to, f.valid_to_precision,
                {holds_from} AS holds_from, {holds_to} AS holds_to,
                f.recorded_at, f.invalidated_at, f.confidence, f.implied, f.supersedes,
                COALESCE(ARRAY(SELECT DISTINCT e.document_id FROM fact_evidence e
                                WHERE e.fact_id = f.id AND e.document_id IS NOT NULL), '{{}}')
                  AS documents,
                COALESCE(ARRAY(SELECT e.quote FROM fact_evidence e
                                WHERE e.fact_id = f.id AND e.quote IS NOT NULL
                                ORDER BY e.chunk_id), '{{}}') AS quotes,
                COALESCE(ARRAY(SELECT DISTINCT c.origin FROM fact_evidence e
                                JOIN chunks c ON c.id = e.chunk_id
                                WHERE e.fact_id = f.id ORDER BY c.origin), '{{}}')
                  AS quote_origins,
                s.kb_id AS subject_kb, o.kb_id AS object_kb,
                p.kb_id AS predicate_kb, sp.kb_id AS supersedes_kb,
                EXISTS(SELECT 1 FROM fact_evidence e
                        LEFT JOIN documents ed ON ed.id = e.document_id
                        WHERE e.fact_id = f.id AND e.document_id IS NOT NULL
                          AND ed.kb_id IS DISTINCT FROM f.kb_id) AS foreign_document,
                EXISTS(SELECT 1 FROM fact_evidence e
                        LEFT JOIN chunks ec ON ec.id = e.chunk_id
                        WHERE e.fact_id = f.id
                          AND ec.kb_id IS DISTINCT FROM f.kb_id) AS foreign_chunk,
                (s.merged_into IS NOT NULL) AS subject_merged,
                (o.merged_into IS NOT NULL) AS object_merged
           FROM facts f
           LEFT JOIN entities s ON s.id = f.subject_id
           LEFT JOIN entities o ON o.id = f.object_id
           LEFT JOIN relation_types p ON p.id = f.predicate_id
           LEFT JOIN facts sp ON sp.id = f.supersedes
          WHERE f.kb_id = $1 AND ($2 IS NULL OR f.id > $2)
          ORDER BY f.id LIMIT $3",
        holds_from = crate::world_axis::facts_holds_from("f"),
        holds_to = crate::world_axis::facts_holds_to("f"),
    ))
    .bind(kb_id)
    .bind(after)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?;
    // Go through the rows left behind one by one: a reference minted under this KB's IRI must not cross KBs, and a reference entering the vocabulary must not look up empty.
    // The check uses the ref_kb selected atomically with the row — not another moment asked of the database
    let mut violations = Vec::new();
    let mut unexported = Vec::new();
    for f in &facts {
        tally(
            &mut violations,
            "fact.subject",
            foreign(f.subject_kb, kb_id) as i64,
        );
        tally(
            &mut unexported,
            "fact.subject(merged)",
            f.subject_merged as i64,
        );
        if f.object_id.is_some() {
            tally(
                &mut violations,
                "fact.object",
                foreign(f.object_kb, kb_id) as i64,
            );
            tally(
                &mut unexported,
                "fact.object(merged)",
                f.object_merged as i64,
            );
        }
        if f.predicate_id.is_some() {
            tally(
                &mut violations,
                "fact.predicate",
                foreign(f.predicate_kb, kb_id) as i64,
            );
        }
        if f.supersedes.is_some() {
            tally(
                &mut violations,
                "fact.supersedes",
                foreign(f.supersedes_kb, kb_id) as i64,
            );
        }
        tally(
            &mut violations,
            "evidence.document",
            f.foreign_document as i64,
        );
        tally(&mut violations, "evidence.chunk", f.foreign_chunk as i64);
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    if !unexported.is_empty() {
        return Err(unexported_error(&unexported));
    }
    // The attributes on the edge live in another table (0037); they are fetched back by fact id in one batch and filled in — selecting the attribute type's KB together with the
    // entity value's KB: a cross-KB type would be silently skipped by the vocabulary, and a cross-KB entity would be minted into this KB's
    // IRI; both kinds of row must be stopped before serialization
    {
        let ids: Vec<Uuid> = facts.iter().map(|f| f.id).collect();
        let mut by_fact = qualifiers_for_export(tx, &ids, kb_id).await?;
        for f in facts.iter_mut() {
            if let Some(q) = by_fact.remove(&f.id) {
                f.qualifiers = q;
            }
        }
    }
    Ok(facts)
}

#[derive(sqlx::FromRow)]
struct ExportQualifierRow {
    fact_id: Uuid,
    qualifier_type_id: Uuid,
    key: Option<String>,
    label: Option<String>,
    value: Option<serde_json::Value>,
    entity_id: Option<Uuid>,
    entity_name: Option<String>,
    type_kb: Option<Uuid>,
    entity_kb: Option<Uuid>,
    entity_merged: bool,
}

/// Attribute fetch for export: the same shape as graph::fact_qualifiers_for, but selecting two extra KB columns.
/// That one uses an INNER JOIN — when the referenced type is absent, the whole attribute row silently disappears; export cannot swallow it that way.
/// An attribute type from another KB would be looked up empty in the vocabulary and silently skipped; an entity value from another KB would be minted into this KB's
/// entity IRI — both are rejected as bad rows
async fn qualifiers_for_export(
    tx: &mut Transaction<'_, Postgres>,
    fact_ids: &[Uuid],
    kb_id: Uuid,
) -> AppResult<std::collections::HashMap<Uuid, Vec<utopia_core::models::FactQualifier>>> {
    let mut out: std::collections::HashMap<Uuid, Vec<utopia_core::models::FactQualifier>> =
        std::collections::HashMap::new();
    if fact_ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<ExportQualifierRow> = sqlx::query_as(
        "SELECT q.fact_id, q.qualifier_type_id, r.key, r.label, q.value, q.entity_id,
                e.canonical_name AS entity_name,
                r.kb_id AS type_kb, e.kb_id AS entity_kb,
                (e.merged_into IS NOT NULL) AS entity_merged
             FROM fact_qualifiers q
             LEFT JOIN relation_types r ON r.id = q.qualifier_type_id
             LEFT JOIN entities e ON e.id = q.entity_id
             WHERE q.fact_id = ANY($1)
             ORDER BY q.fact_id, r.key",
    )
    .bind(fact_ids)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    let mut unexported = Vec::new();
    for r in &rows {
        tally(
            &mut violations,
            "qualifier.type",
            foreign(r.type_kb, kb_id) as i64,
        );
        if r.entity_id.is_some() {
            tally(
                &mut violations,
                "qualifier.entity",
                foreign(r.entity_kb, kb_id) as i64,
            );
            tally(
                &mut unexported,
                "qualifier.entity(merged)",
                r.entity_merged as i64,
            );
        }
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    if !unexported.is_empty() {
        return Err(unexported_error(&unexported));
    }
    for r in rows {
        out.entry(r.fact_id)
            .or_default()
            .push(utopia_core::models::FactQualifier {
                qualifier_type_id: r.qualifier_type_id,
                // Having passed validation, r must exist: the key/label cannot fail to be fetched
                key: r.key.unwrap_or_default(),
                label: r.label.unwrap_or_default(),
                value: r.value,
                entity_id: r.entity_id,
                entity_name: r.entity_name,
            });
    }
    Ok(out)
}

pub async fn derived_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<Uuid>,
) -> AppResult<Vec<ExportDerived>> {
    let page: Vec<ExportDerived> = sqlx::query_as(
        // **Two LEFT JOINs.** After the table was widened (0021), a derivation may have no entity object,
        // and may come from a business rule rather than an axiom — an inner join would block whole conclusions of this kind from the export,
        // yet what 0020 promises is exactly that "an auditor can read everything without relying on us"
        "SELECT d.id, d.subject_id, d.predicate_id, d.object_id, d.object_value,
                d.rule_id, d.attribute_rule_id,
                d.valid_from, d.valid_from_precision, d.valid_to, d.valid_to_precision,
                d.derived_at, d.invalidated_at, d.confidence,
                COALESCE(ru.kind, 'business') AS rule, ru.predicate_id AS rule_predicate,
                ar.name AS rule_name,
                COALESCE(ARRAY(SELECT fd.premise_fact_id FROM fact_derivations fd
                                WHERE fd.derived_fact_id = d.id
                                  AND fd.premise_fact_id IS NOT NULL
                                ORDER BY fd.seq), '{}') AS premises,
                COALESCE(ARRAY(SELECT fd.premise_derived_id FROM fact_derivations fd
                                WHERE fd.derived_fact_id = d.id
                                  AND fd.premise_derived_id IS NOT NULL
                                ORDER BY fd.seq), '{}') AS premises_derived,
                s.kb_id AS subject_kb, o.kb_id AS object_kb, p.kb_id AS predicate_kb,
                ru.kb_id AS rule_kb, ar.kb_id AS attribute_rule_kb,
                EXISTS(SELECT 1 FROM fact_derivations fd
                        LEFT JOIN facts pf ON pf.id = fd.premise_fact_id
                        WHERE fd.derived_fact_id = d.id AND fd.premise_fact_id IS NOT NULL
                          AND pf.kb_id IS DISTINCT FROM d.kb_id) AS foreign_fact_premise,
                EXISTS(SELECT 1 FROM fact_derivations fd
                        LEFT JOIN derived_facts pd ON pd.id = fd.premise_derived_id
                        WHERE fd.derived_fact_id = d.id AND fd.premise_derived_id IS NOT NULL
                          AND pd.kb_id IS DISTINCT FROM d.kb_id) AS foreign_derived_premise,
                (s.merged_into IS NOT NULL) AS subject_merged,
                (o.merged_into IS NOT NULL) AS object_merged
           FROM derived_facts d
           LEFT JOIN rules ru ON ru.id = d.rule_id
           LEFT JOIN attribute_rules ar ON ar.id = d.attribute_rule_id
           LEFT JOIN entities s ON s.id = d.subject_id
           LEFT JOIN entities o ON o.id = d.object_id
           LEFT JOIN relation_types p ON p.id = d.predicate_id
          WHERE d.kb_id = $1 AND ($2 IS NULL OR d.id > $2)
          ORDER BY d.id LIMIT $3",
    )
    .bind(kb_id)
    .bind(after)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    let mut unexported = Vec::new();
    for d in &page {
        tally(
            &mut violations,
            "derived.subject",
            foreign(d.subject_kb, kb_id) as i64,
        );
        tally(
            &mut unexported,
            "derived.subject(merged)",
            d.subject_merged as i64,
        );
        if d.object_id.is_some() {
            tally(
                &mut violations,
                "derived.object",
                foreign(d.object_kb, kb_id) as i64,
            );
            tally(
                &mut unexported,
                "derived.object(merged)",
                d.object_merged as i64,
            );
        }
        // The derived predicate is non-empty (guaranteed by CHECK); a NULL ref_kb is likewise out-of-bounds / dangling
        tally(
            &mut violations,
            "derived.predicate",
            foreign(d.predicate_kb, kb_id) as i64,
        );
        if d.rule_id.is_some() {
            tally(
                &mut violations,
                "derived.rule",
                foreign(d.rule_kb, kb_id) as i64,
            );
        }
        if d.attribute_rule_id.is_some() {
            tally(
                &mut violations,
                "derived.attribute_rule",
                foreign(d.attribute_rule_kb, kb_id) as i64,
            );
        }
        tally(
            &mut violations,
            "derivation.premise_fact",
            d.foreign_fact_premise as i64,
        );
        tally(
            &mut violations,
            "derivation.premise_derived",
            d.foreign_derived_premise as i64,
        );
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    if !unexported.is_empty() {
        return Err(unexported_error(&unexported));
    }
    Ok(page)
}

pub async fn documents_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<Uuid>,
) -> AppResult<Vec<ExportDocument>> {
    Ok(sqlx::query_as(
        "SELECT id, filename, external_key, doc_time, created_at, deleted_at
           FROM documents
          WHERE kb_id = $1 AND ($2 IS NULL OR id > $2)
          ORDER BY id LIMIT $3",
    )
    .bind(kb_id)
    .bind(after)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?)
}

/* ---- Epistemic state (0062) ----
 *
 * Two tables, two different things, not merged into one class:
 *   fact_conflicts   two assertions in the same slot do not yield to each other — the data is disputed; the criterion itself is fine
 *   axiom_violations current data hits the current criterion — at least one side, data or ontology, must change
 *
 * Read-only: it does not go through a read path with lazy cleanup like `temporal::list_conflicts` (0051 already removed that),
 * and does not trigger detection. The export records the state stored in the database. */

/// Two facts contest the same slot. A `resolved` row is also here — it records a dispute that once existed, not a
/// to-do; a `withdrawn` one is retired by a trigger after its fact was voided (0051), and `resolved_at` records the
/// moment of voiding, not the moment someone opened the page
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportFactConflict {
    pub id: Uuid,
    pub old_fact_id: Uuid,
    pub new_fact_id: Uuid,
    /// How the dispute was recorded: no_time | simultaneous | low_confidence | described_evidence
    /// (this column has no CHECK, so old values in the database may also come through; the contract vocabulary is authoritative per contest_integrity)
    pub reason: String,
    pub created_at: DateTime<Utc>,
    /// open | resolved | withdrawn
    pub status: String,
    /// closed | kept_both | rejected_new; only present when resolved
    pub resolution: Option<String>,
    /// resolved: the moment of the ruling; withdrawn: the moment of the side voided first (0051)
    pub resolved_at: Option<DateTime<Utc>>,
    /// The KB of both facts, selected atomically with the row: cross-KB / dangling → reject
    pub old_fact_kb: Option<Uuid>,
    pub new_fact_kb: Option<Uuid>,
}

pub async fn fact_conflicts_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<Uuid>,
) -> AppResult<Vec<ExportFactConflict>> {
    let page: Vec<ExportFactConflict> = sqlx::query_as(
        "SELECT c.id, c.old_fact_id, c.new_fact_id, c.reason, c.created_at,
                c.status, c.resolution, c.resolved_at,
                of_.kb_id AS old_fact_kb, nf.kb_id AS new_fact_kb
           FROM fact_conflicts c
           LEFT JOIN facts of_ ON of_.id = c.old_fact_id
           LEFT JOIN facts nf ON nf.id = c.new_fact_id
          WHERE c.kb_id = $1 AND ($2 IS NULL OR c.id > $2)
          ORDER BY c.id LIMIT $3",
    )
    .bind(kb_id)
    .bind(after)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    for c in &page {
        tally(
            &mut violations,
            "conflict.old_fact",
            foreign(c.old_fact_kb, kb_id) as i64,
        );
        tally(
            &mut violations,
            "conflict.new_fact",
            foreign(c.new_fact_kb, kb_id) as i64,
        );
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    Ok(page)
}

/// A finding that "current data does not agree with the current criterion". `open` means it has been reconciled against the **current**
/// ontology (0062) — the transaction that changed the criterion adopted it or re-detected it.
///
/// The row is **mutable current state**, not an append-only history: if the same violation is computed again after being resolved, it reopens;
/// `detected_at` records the moment of the most recent detection or reopening; earlier loops
/// are not kept in the row, so the export does not claim them.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportAxiomViolation {
    pub id: Uuid,
    pub kind: String,
    pub left_fact: Uuid,
    pub right_fact: Uuid,
    /// The full chain of facts along the cycle, in order (other kinds store detection evidence; self-loops and signatures are empty)
    pub path: Vec<Uuid>,
    pub status: String,
    /// fact_retracted | fact_closed | axiom_relaxed | accepted | criterion_changed
    pub resolution: Option<String>,
    /// The moment of the most recent detection or reopening (not the first discovery)
    pub detected_at: DateTime<Utc>,
    /// The moment it stopped being open (adopted, or ruled on by a human)
    pub decided_at: Option<DateTime<Utc>>,
    /// The two columns below are **resolved only on open rows** (0062): open = reconciled against the current ontology,
    /// so the criterion must point back to something; a resolved row does not link to the criterion — the ontology that closed it may long be gone
    pub criterion_predicate: Option<Uuid>,
    /// The criterion's word: the kind itself, or detail.axiom recorded by derived_contradiction
    /// (self_loop | asymmetry | cycle | functional | inverse_functional | signature)
    pub criterion_axiom: Option<String>,
    /// The KB of the referenced row, selected atomically together with it
    pub left_kb: Option<Uuid>,
    pub right_kb: Option<Uuid>,
    pub criterion_predicate_kb: Option<Uuid>,
    /// Whether a fact in the path is from another KB or dangling (the evidence chain of a cycle is minted as IRIs in order)
    pub foreign_path: bool,
}

/// detail.predicate_id is stored as JSON text; check its form before casting to uuid —
/// a malformed value should fail to parse (→ reject) rather than silently point at the wrong relation
const UUID_TEXT_RE: &str =
    "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$";

/// The predicate resolution for an open row's criterion: derived_contradiction's criterion sits on the **derived predicate** recorded in detail
/// (the axiom is declared there); for other kinds it is the predicate of left_fact.
/// Pagination and the pre-check share this one expression — writing it in two places would drift sooner or later
fn criterion_predicate_sql() -> String {
    format!(
        "CASE WHEN v.kind = 'derived_contradiction'
              THEN CASE WHEN v.detail->>'predicate_id' ~ '{UUID_TEXT_RE}'
                        THEN (v.detail->>'predicate_id')::uuid END
              ELSE lf.predicate_id END"
    )
}

/// The criterion-word resolution for an open row: derived_contradiction looks at detail.axiom, the rest look at kind
const CRITERION_AXIOM_SQL: &str =
    "CASE WHEN v.kind = 'derived_contradiction' THEN v.detail->>'axiom' ELSE v.kind END";

/// Stored values of kind / detail.axiom → the word the criterion should use (0062 §4).
/// A signature's criterion is a pair of signatures (the row does not record which side broke, and the contract does not pretend to know).
/// None = an unknown stored value — an open row hitting it must be rejected; the criterion must not be silently dropped
fn criterion_terms(axiom: &str) -> Option<&'static [&'static str]> {
    Some(match axiom {
        "self_loop" => &["owl:IrreflexiveProperty"],
        "asymmetry" => &["owl:AsymmetricProperty"],
        "cycle" => &["owl:TransitiveProperty"],
        "functional" => &["owl:FunctionalProperty"],
        "inverse_functional" => &["owl:InverseFunctionalProperty"],
        "signature" => &["rdfs:domain", "rdfs:range"],
        _ => return None,
    })
}

impl ExportAxiomViolation {
    /// The RDF term of the criterion (the object of `utopia:criterion`; there may be more than one).
    /// An open row must always obtain it — failing to obtain it means the stored value is something the contract does not recognize; reject rather than make it up
    pub fn criterion_terms(&self) -> Option<&'static [&'static str]> {
        self.criterion_axiom.as_deref().and_then(criterion_terms)
    }
}

pub async fn axiom_violations_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<Uuid>,
) -> AppResult<Vec<ExportAxiomViolation>> {
    let page: Vec<ExportAxiomViolation> = sqlx::query_as(&format!(
        "SELECT v.id, v.kind, v.left_fact, v.right_fact, v.path, v.status, v.resolution,
                v.detected_at, v.decided_at,
                CASE WHEN v.status = 'open' THEN {criterion_predicate}
                END AS criterion_predicate,
                CASE WHEN v.status = 'open' THEN {criterion_axiom}
                END AS criterion_axiom,
                lf.kb_id AS left_kb, rf.kb_id AS right_kb, cp.kb_id AS criterion_predicate_kb,
                EXISTS(SELECT 1 FROM unnest(v.path) AS member(fact_id)
                        WHERE NOT EXISTS(SELECT 1 FROM facts pf
                                          WHERE pf.id = member.fact_id
                                            AND pf.kb_id = v.kb_id)) AS foreign_path
           FROM axiom_violations v
           LEFT JOIN facts lf ON lf.id = v.left_fact
           LEFT JOIN facts rf ON rf.id = v.right_fact
           LEFT JOIN relation_types cp ON cp.id =
                CASE WHEN v.status = 'open' THEN {criterion_predicate} END
          WHERE v.kb_id = $1 AND ($2 IS NULL OR v.id > $2)
          ORDER BY v.id LIMIT $3",
        criterion_predicate = criterion_predicate_sql(),
        criterion_axiom = CRITERION_AXIOM_SQL,
    ))
    .bind(kb_id)
    .bind(after)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    let mut unresolvable = Vec::new();
    for v in &page {
        tally(
            &mut violations,
            "violation.left_fact",
            foreign(v.left_kb, kb_id) as i64,
        );
        tally(
            &mut violations,
            "violation.right_fact",
            foreign(v.right_kb, kb_id) as i64,
        );
        tally(&mut violations, "violation.path", v.foreign_path as i64);
        if v.status == "open" {
            // The promise of open is "reconciled against the current ontology": if the criterion cannot be pointed back to, this export is
            // vouching for a broken invariant — rejecting it follows the same discipline as cross-KB
            if v.criterion_predicate.is_none() || v.criterion_terms().is_none() {
                unresolvable.push(CrossKbViolation {
                    edge: "violation.criterion".into(),
                    rows: 1,
                });
            } else {
                tally(
                    &mut violations,
                    "violation.criterion",
                    foreign(v.criterion_predicate_kb, kb_id) as i64,
                );
            }
        }
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    if !unresolvable.is_empty() {
        let detail = unresolvable
            .iter()
            .map(|v| format!("{}: {} row(s)", v.edge, v.rows))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(AppError::invalid_detail(
            "unresolvable_criterion",
            "export refused: an open axiom violation has no governing criterion to link",
            detail,
        ));
    }
    Ok(page)
}

/// Pre-stream check for epistemic state (0062): the paginated queries also carry the same checks, but by then the file header
/// has already been sent and can only be truncated — so count first here, and reject the whole thing if there is a single bad row.
///
/// Three classes of problem:
/// - **Cross-KB / dangling**: a conflict's two facts, and a violation's left/right/path members, must
///   be facts rows of this KB (dangling = minting a non-existent IRI, same sin as cross-KB)
/// - **Values outside the contract**: status/resolution/kind/reason outside the vocabulary, or
///   status and resolution inconsistent (e.g. open carrying a resolution) — dirty rows
///   `fact_conflicts.reason` has no CHECK guarding it, and the other columns have CHECKs that only apply to new writes,
///   so stored dirty rows are rejected as they are, and not echoed out
/// - **Open row criterion not parseable**: open = reconciled against the current ontology, so the criterion must point back to
///   a relation in this KB and a word the contract recognizes (a malformed derived_contradiction detail.predicate_id also counts —
///   silently falling back to the left_fact predicate would be pointing blindly)
pub async fn contest_integrity(tx: &mut Transaction<'_, Postgres>, kb_id: Uuid) -> AppResult<()> {
    #[derive(sqlx::FromRow)]
    struct ContestViolation {
        edge: String,
        kind: String,
        rows: i64,
    }
    let violations: Vec<ContestViolation> = sqlx::query_as(&format!(
        "SELECT edge, kind, count(*) AS rows FROM (
             SELECT 'conflict.old_fact' AS edge, 'cross_kb' AS kind
               FROM fact_conflicts c LEFT JOIN facts f ON f.id = c.old_fact_id
              WHERE c.kb_id = $1 AND f.kb_id IS DISTINCT FROM c.kb_id
             UNION ALL
             SELECT 'conflict.new_fact', 'cross_kb'
               FROM fact_conflicts c LEFT JOIN facts f ON f.id = c.new_fact_id
              WHERE c.kb_id = $1 AND f.kb_id IS DISTINCT FROM c.kb_id
             UNION ALL
             SELECT 'conflict.vocabulary', 'contract'
               FROM fact_conflicts c
              WHERE c.kb_id = $1 AND (
                    COALESCE(c.status, '') NOT IN ('open', 'resolved', 'withdrawn')
                    OR COALESCE(c.reason, '') NOT IN
                       ('no_time', 'simultaneous', 'low_confidence', 'described_evidence')
                    OR c.resolution IS NOT NULL
                       AND c.resolution NOT IN ('closed', 'kept_both', 'rejected_new')
                    -- 0051：resolved=人裁了（resolution 必有值），其余状态不该有
                    OR c.status = 'resolved' AND c.resolution IS NULL
                    OR c.status IN ('open', 'withdrawn') AND c.resolution IS NOT NULL)
             UNION ALL
             SELECT 'violation.left_fact', 'cross_kb'
               FROM axiom_violations v LEFT JOIN facts f ON f.id = v.left_fact
              WHERE v.kb_id = $1 AND f.kb_id IS DISTINCT FROM v.kb_id
             UNION ALL
             SELECT 'violation.right_fact', 'cross_kb'
               FROM axiom_violations v LEFT JOIN facts f ON f.id = v.right_fact
              WHERE v.kb_id = $1 AND f.kb_id IS DISTINCT FROM v.kb_id
             UNION ALL
             SELECT 'violation.path', 'cross_kb'
               FROM axiom_violations v
              WHERE v.kb_id = $1 AND EXISTS(
                    SELECT 1 FROM unnest(v.path) AS member(fact_id)
                     WHERE NOT EXISTS(SELECT 1 FROM facts pf
                                       WHERE pf.id = member.fact_id
                                         AND pf.kb_id = v.kb_id))
             UNION ALL
             SELECT 'violation.vocabulary', 'contract'
               FROM axiom_violations v
              WHERE v.kb_id = $1 AND (
                    COALESCE(v.kind, '') NOT IN
                       ('self_loop', 'asymmetry', 'cycle', 'functional',
                        'inverse_functional', 'signature', 'derived_contradiction')
                    OR COALESCE(v.status, '') NOT IN ('open', 'resolved')
                    OR v.resolution IS NOT NULL AND v.resolution NOT IN
                       ('fact_retracted', 'fact_closed', 'axiom_relaxed', 'accepted',
                        'criterion_changed')
                    -- 写路从来都是成对落（决定+时间戳）；open 带着 resolution
                    -- 或 resolved 丢了 resolution 都是自相矛盾的脏行
                    OR v.status = 'resolved' AND v.resolution IS NULL
                    OR v.status = 'open' AND v.resolution IS NOT NULL)
             UNION ALL
             SELECT 'violation.criterion', 'unresolvable'
               FROM axiom_violations v
               LEFT JOIN facts lf ON lf.id = v.left_fact
              WHERE v.kb_id = $1 AND v.status = 'open' AND (
                    COALESCE({criterion_axiom}, '') NOT IN
                       ('self_loop', 'asymmetry', 'cycle', 'functional',
                        'inverse_functional', 'signature')
                    OR NOT EXISTS(
                        SELECT 1 FROM relation_types rt
                         WHERE rt.kb_id = v.kb_id
                           AND rt.id = {criterion_predicate}))
        ) t GROUP BY edge, kind",
        criterion_axiom = CRITERION_AXIOM_SQL,
        criterion_predicate = criterion_predicate_sql(),
    ))
    .bind(kb_id)
    .fetch_all(&mut **tx)
    .await?;
    let cross_kb: Vec<CrossKbViolation> = violations
        .iter()
        .filter(|v| v.kind == "cross_kb")
        .map(|v| CrossKbViolation {
            edge: v.edge.clone(),
            rows: v.rows,
        })
        .collect();
    if !cross_kb.is_empty() {
        return Err(cross_kb_error(&cross_kb));
    }
    let rest: Vec<&ContestViolation> = violations.iter().filter(|v| v.kind != "cross_kb").collect();
    if !rest.is_empty() {
        let detail = rest
            .iter()
            .map(|v| format!("{}: {} row(s)", v.edge, v.rows))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(AppError::invalid_detail(
            "contested_state_contract",
            "export refused: contested-state rows fall outside the contract vocabulary or an open violation has no resolvable criterion",
            detail,
        ));
    }
    Ok(())
}
