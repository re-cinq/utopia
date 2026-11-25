//! Ontology editor repository: type and relation CRUD (with usage counts and delete protection) + unmatched statistics.

use pgvector::Vector;
use sqlx::{PgConnection, PgPool};
use std::collections::HashSet;
use utopia_core::models::{
    EntityInstance, EntityTypeView, OntologyImportView, RelationAxioms, RelationTypeView,
    TypeCandidate,
};
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

pub async fn entity_type_views(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<EntityTypeView>> {
    Ok(sqlx::query_as(
        "SELECT t.id, t.key, t.label, t.color, t.shape, t.builtin, t.description,
                t.display_hints,
                ARRAY(SELECT p.parent_id FROM entity_type_parents p
                      WHERE p.child_id = t.id) AS parents,
                (SELECT p.parent_id FROM entity_type_parents p
                  WHERE p.child_id = t.id AND p.is_primary) AS primary_parent,
                ARRAY(SELECT d.b_id FROM entity_type_disjoint d
                      WHERE d.kb_id = t.kb_id AND d.a_id = t.id) AS disjoint,
                COALESCE(u.usage, 0) AS usage
         FROM entity_types t
         LEFT JOIN (
             SELECT type_id, count(*)::bigint AS usage
             FROM entities
             WHERE kb_id = $1 AND merged_into IS NULL AND type_id IS NOT NULL
             GROUP BY type_id
         ) u ON u.type_id = t.id
         WHERE t.kb_id = $1 ORDER BY lower(t.label)",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}

/// For "missing this relation", which end the entity sits on: Out = it is the subject, In = it is the object, Either = either end counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkEnd {
    Out,
    In,
    Either,
}

/// The forms in which a predicate can appear in the KB, from the side where the entity is the subject of the relation.
///
/// Typed rows write attributes; open statements write the document's phrases, and bindings (`phrase_bindings`) say which phrases
/// count as this attribute, and in which direction. Inverse and symmetric relations turn "subject" into "object": `A parentOf B` is
/// `B childOf A`, so an A missing parentOf must also have no "X childOf A".
#[derive(Debug, Clone)]
pub struct LinkPredicate {
    /// When the predicate is the key of an attribute in the ontology, that attribute
    pub relation_type_id: Option<Uuid>,
    along: Vec<Uuid>,
    against: Vec<Uuid>,
    phrases_along: Vec<String>,
    phrases_against: Vec<String>,
}

impl LinkPredicate {
    /// All phrases recognized as this predicate (normalized), both directions combined
    pub fn phrases(&self) -> Vec<String> {
        let mut all = [self.phrases_along.clone(), self.phrases_against.clone()].concat();
        all.sort();
        all.dedup();
        all
    }
}

/// Resolves a predicate (an attribute's key, or an open phrase) into its various spellings in the KB.
pub async fn link_predicate(
    pool: &PgPool,
    kb_id: Uuid,
    predicate: &str,
) -> AppResult<LinkPredicate> {
    let (relation_type_id, against, mut phrases_along, phrases_against): (
        Option<Uuid>,
        Vec<Uuid>,
        Vec<String>,
        Vec<String>,
    ) = sqlx::query_as(
        "WITH r AS (SELECT id, inverse_of, is_symmetric FROM relation_types
                     WHERE kb_id = $1 AND key = $2),
              inv AS (SELECT x.id FROM relation_types x, r
                       WHERE x.kb_id = $1 AND (x.id = r.inverse_of OR x.inverse_of = r.id)
                      UNION SELECT id FROM r WHERE is_symmetric),
              b AS (SELECT phrase, direction = 'forward' AS fwd,
                           relation_type_id IN (SELECT id FROM r) AS on_r,
                           relation_type_id IN (SELECT id FROM inv) AS on_inv
                      FROM phrase_bindings
                     WHERE kb_id = $1 AND status = 'bound')
         SELECT (SELECT id FROM r),
                ARRAY(SELECT id FROM inv),
                ARRAY(SELECT DISTINCT phrase FROM b
                       WHERE (on_r AND fwd) OR (on_inv AND NOT fwd)),
                ARRAY(SELECT DISTINCT phrase FROM b
                       WHERE (on_r AND NOT fwd) OR (on_inv AND fwd))",
    )
    .bind(kb_id)
    .bind(predicate)
    .fetch_one(pool)
    .await?;
    phrases_along.push(crate::phrase_bindings::normalize(predicate));
    Ok(LinkPredicate {
        relation_type_id,
        along: relation_type_id.into_iter().collect(),
        against,
        phrases_along,
        phrases_against,
    })
}

/// Lists only members missing a certain relation (#1269): no live fact (assertion, statement, or derivation) connects them through this predicate.
#[derive(Debug, Clone, Copy)]
pub struct Lacking<'a> {
    pub predicate: &'a LinkPredicate,
    pub end: LinkEnd,
}

impl Lacking<'_> {
    /// When the entity is the subject, which (attribute, phrase) pairs count; when it is the object, which ones do
    fn arms(&self) -> [(Vec<Uuid>, Vec<String>); 2] {
        let p = self.predicate;
        let along = (p.along.clone(), p.phrases_along.clone());
        let against = (p.against.clone(), p.phrases_against.clone());
        match self.end {
            LinkEnd::Out => [along, against],
            LinkEnd::In => [against, along],
            LinkEnd::Either => {
                let both = ([along.0, against.0].concat(), [along.1, against.1].concat());
                [both.clone(), both]
            }
        }
    }
}

/// One NOT EXISTS per end, each using the live-row index on (kb_id, subject_id) / (kb_id, object_id)
fn lacking_sql() -> String {
    let phrase = crate::phrase_bindings::phrase_sql("f.phrase");
    ["subject", "object"]
        .iter()
        .zip([("$3", "$4"), ("$5", "$6")])
        .map(|(end, (ids, phrases))| {
            format!(
                " AND NOT EXISTS (SELECT 1 FROM facts f
                    WHERE f.kb_id = e.kb_id AND f.{end}_id = e.id AND f.invalidated_at IS NULL
                      AND (f.predicate_id = ANY({ids})
                           OR (f.layer = 'open' AND {phrase} = ANY({phrases}))))
                  AND NOT EXISTS (SELECT 1 FROM derived_facts d
                    WHERE d.kb_id = e.kb_id AND d.{end}_id = e.id AND d.invalidated_at IS NULL
                      AND d.predicate_id = ANY({ids}))"
            )
        })
        .collect()
}

/// Entity instances under a class (in name order, paginated). Returns (rows, total).
/// Given `lacking`, only members missing that relation are kept; the page and the total are counted under the same condition.
pub async fn entity_instances(
    pool: &PgPool,
    kb_id: Uuid,
    type_id: Uuid,
    lacking: Option<Lacking<'_>>,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<EntityInstance>, i64)> {
    // When lacking is not given, $3..$6 are empty arrays and the condition does not reference them
    let [(subject_ids, subject_phrases), (object_ids, object_phrases)] =
        lacking.map(|l| l.arms()).unwrap_or_default();
    let condition = format!(
        "e.kb_id = $1 AND e.type_id = $2 AND e.merged_into IS NULL{}",
        lacking.map(|_| lacking_sql()).unwrap_or_default()
    );
    let rows: Vec<EntityInstance> = sqlx::query_as(&format!(
        "SELECT e.id, e.canonical_name AS name,
                (SELECT count(*) FROM facts f
                  WHERE f.kb_id = e.kb_id
                    AND (f.subject_id = e.id OR f.object_id = e.id)
                    AND f.invalidated_at IS NULL) AS fact_count
         FROM entities e
         WHERE {condition}
         ORDER BY lower(e.canonical_name), e.id
         LIMIT $7 OFFSET $8"
    ))
    .bind(kb_id)
    .bind(type_id)
    .bind(&subject_ids)
    .bind(&subject_phrases)
    .bind(&object_ids)
    .bind(&object_phrases)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) = sqlx::query_as(&format!(
        "SELECT count(*) FROM entities e WHERE {condition}"
    ))
    .bind(kb_id)
    .bind(type_id)
    .bind(&subject_ids)
    .bind(&subject_phrases)
    .bind(&object_ids)
    .bind(&object_phrases)
    .fetch_one(pool)
    .await?;
    Ok((rows, total))
}

fn validate_shape(shape: &str) -> AppResult<()> {
    if !matches!(shape, "circle" | "square") {
        return Err(AppError::Validation(
            "shape must be circle or square".into(),
        ));
    }
    Ok(())
}

pub async fn relation_type_views(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<RelationTypeView>> {
    Ok(sqlx::query_as(
        "SELECT r.id, r.key, r.label, r.temporal, r.functional, r.inverse_functional,
                r.is_transitive, r.is_symmetric, r.is_asymmetric, r.is_irreflexive,
                r.inverse_of, r.sub_property_of,
                r.builtin, r.description, r.display_hints,
                r.kind, r.datatype, r.unit,
                ARRAY(SELECT d.entity_type_id FROM relation_type_domains d
                      WHERE d.relation_type_id = r.id) AS domains,
                ARRAY(SELECT g.entity_type_id FROM relation_type_ranges g
                      WHERE g.relation_type_id = r.id) AS ranges,
                ARRAY(SELECT q.qualifier_type_id FROM relation_type_qualifiers q
                      WHERE q.relation_type_id = r.id) AS qualifiers,
                COALESCE(u.usage, 0) AS usage
         FROM relation_types r
         LEFT JOIN (
             SELECT predicate_id, count(*)::bigint AS usage
             FROM facts
             WHERE kb_id = $1 AND invalidated_at IS NULL AND predicate_id IS NOT NULL
             GROUP BY predicate_id
         ) u ON u.predicate_id = r.id
         WHERE r.kb_id = $1 ORDER BY lower(r.label)",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}

/// Display names of predicates are normalized to **lowerCamelCase** (see migration 0042).
///
/// Classes are UpperCamelCase and predicates are lowerCamelCase; this is the RDF/OWL and schema.org convention, and the casing
/// itself says whether the word is a class or a property. The KB held `acceptedAnswer`, `access to`, and
/// `ApplicableCertificate` all in different styles; in one column that reads as having no rules.
///
/// **Only separators and first letters are changed; the casing inside a word is not touched**: `productID`, `hasLEI`,
/// `accessibilityAPI` must stay as they are. Reconstructing from `key` cannot do this — `to_key`
/// does not insert underscores between runs of capitals, so rebuilding `product_id` only gives `productId`.
pub fn lower_camel(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut start_of_word = false;
    for c in label.trim().chars() {
        if c == ' ' || c == '_' || c == '-' {
            // Consecutive separators count as one; the word-start marker is kept
            start_of_word = !out.is_empty();
            continue;
        }
        if out.is_empty() {
            out.extend(c.to_lowercase());
        } else if start_of_word {
            out.extend(c.to_uppercase());
            start_of_word = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// `temporal` has only three values (0031): single create, single update, bulk create, and re-import update all go through this one statement
fn validate_temporal(temporal: &str) -> AppResult<()> {
    if !matches!(temporal, "state" | "event" | "eternal") {
        return Err(AppError::Validation(
            "temporal must be state / event / eternal".into(),
        ));
    }
    Ok(())
}

/// Display hints (RC-008) are a flat object of string values: one `utopia:<key> "<value>"`
/// literal per entry, exactly what the export writes back. The page, the import and the
/// bulk insert all pass through here, as `temporal` passes through `validate_temporal`.
pub fn validate_display_hints(hints: &serde_json::Value) -> AppResult<()> {
    let flat = hints
        .as_object()
        .is_some_and(|o| o.values().all(serde_json::Value::is_string));
    if !flat {
        return Err(AppError::Validation(
            "display_hints must be an object of string values".into(),
        ));
    }
    Ok(())
}

/// Replace a class's display hints. A page edit that says nothing about them leaves
/// them alone (the route only calls this on `Some`), the same rule as `set_disjoint_for`.
pub async fn set_type_display_hints(
    pool: &PgPool,
    kb_id: Uuid,
    id: Uuid,
    hints: &serde_json::Value,
) -> AppResult<()> {
    set_display_hints(pool, "entity_types", kb_id, id, hints).await
}

/// Replace a relation's or attribute's display hints; see [`set_type_display_hints`].
pub async fn set_relation_display_hints(
    pool: &PgPool,
    kb_id: Uuid,
    id: Uuid,
    hints: &serde_json::Value,
) -> AppResult<()> {
    set_display_hints(pool, "relation_types", kb_id, id, hints).await
}

async fn set_display_hints(
    pool: &PgPool,
    table: &str,
    kb_id: Uuid,
    id: Uuid,
    hints: &serde_json::Value,
) -> AppResult<()> {
    validate_display_hints(hints)?;
    let res = sqlx::query(&format!(
        "UPDATE {table} SET display_hints = $3 WHERE kb_id = $1 AND id = $2"
    ))
    .bind(kb_id)
    .bind(id)
    .bind(hints)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

fn validate_key(key: &str) -> AppResult<()> {
    let ok = !key.is_empty()
        && key.len() <= 40
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        return Err(AppError::invalid(
            "bad_key",
            "Key must be lowercase snake_case (a-z, 0-9, _), max 40 chars",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn create_entity_type<'a>(
    pool: impl sqlx::Acquire<'a, Database = sqlx::Postgres>,
    kb_id: Uuid,
    key: &str,
    label: &str,
    color: &str,
    shape: &str,
    parents: &[Uuid],
    description: &str,
) -> AppResult<Uuid> {
    validate_key(key)?;
    validate_shape(shape)?;
    let mut tx = pool.begin().await?;
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label, color, shape, description)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(kb_id)
    .bind(key)
    .bind(label)
    .bind(color)
    .bind(shape)
    .bind(description)
    .execute(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            AppError::Conflict(format!("Type key '{key}' already exists"))
        }
        _ => AppError::Db(e),
    })?;
    set_parents(&mut tx, kb_id, id, parents).await?;
    tx.commit().await?;
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
/// Change an entity class.
///
/// **`color: None` = keep the existing color**, not reset. It used to be `&str`, and when the caller did not give one it
/// hard-coded a default grey-blue, so any rename without a color wiped out the color the user had picked.
pub async fn update_entity_type(
    pool: &PgPool,
    kb_id: Uuid,
    id: Uuid,
    label: &str,
    color: Option<&str>,
    shape: &str,
    parents: &[Uuid],
    description: &str,
) -> AppResult<()> {
    validate_shape(shape)?;
    let mut tx = pool.begin().await?;
    // The parent chain is the signature criterion (0062): violations are computed from the ancestor chain. Only parents are compared — label, color and other
    // fields that do not change the criterion are written as usual, with no need to run detection
    let old_parents: Vec<Uuid> =
        sqlx::query_scalar("SELECT parent_id FROM entity_type_parents WHERE child_id = $1")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    let parents_changed =
        parents.len() != old_parents.len() || !parents.iter().all(|p| old_parents.contains(p));
    let was_detected = if parents_changed {
        Some(crate::reasoning::detection_keys(&mut tx, kb_id).await?)
    } else {
        None
    };
    let res = sqlx::query(
        "UPDATE entity_types SET label = $3, color = COALESCE($4, color), shape = $5,
                description = $6, updated_at = now()
         WHERE id = $2 AND kb_id = $1",
    )
    .bind(kb_id)
    .bind(id)
    .bind(label)
    .bind(color)
    .bind(shape)
    .bind(description)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    set_parents(&mut tx, kb_id, id, parents).await?;
    if let Some(was) = was_detected {
        crate::reasoning::reconcile_ontology(&mut tx, kb_id, &was).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Sets `parents` to all of this class's parents; the first one is the main parent (the left pane draws it under that branch).
///
/// **Check for cycles before writing.** In the single-parent era only self-loops needed blocking, since a chain cannot form a cycle naturally; in a DAG, A→B→A
/// is entirely possible, and `type_matches_domain` walks up the parent chain, so a cycle would be an infinite loop.
/// SQL cannot stop this — foreign keys block only self-loops, and longer ones need the application to check.
async fn set_parents(
    conn: &mut PgConnection,
    kb_id: Uuid,
    child: Uuid,
    parents: &[Uuid],
) -> AppResult<()> {
    if parents.contains(&child) {
        return Err(AppError::invalid(
            "self_parent",
            "A class cannot be its own parent",
        ));
    }
    if !parents.is_empty() {
        // If child appears among all the ancestors of a candidate parent, this edge would form a cycle
        let (cycles,): (i64,) = sqlx::query_as(
            "WITH RECURSIVE up(id) AS (
                 SELECT unnest($2::uuid[])
                 UNION
                 SELECT p.parent_id FROM entity_type_parents p JOIN up ON p.child_id = up.id
             )
             SELECT count(*) FROM up WHERE id = $1",
        )
        .bind(child)
        .bind(parents)
        .fetch_one(&mut *conn)
        .await?;
        if cycles > 0 {
            return Err(AppError::invalid(
                "parent_cycle",
                "That parent is already a subclass of this one",
            ));
        }
    }
    sqlx::query("DELETE FROM entity_type_parents WHERE child_id = $1")
        .bind(child)
        .execute(&mut *conn)
        .await?;
    for (i, p) in parents.iter().enumerate() {
        sqlx::query(
            "INSERT INTO entity_type_parents (child_id, parent_id, is_primary)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(child)
        .bind(p)
        // The first one is the main parent: the UI states "drawn under the first one", so no extra control is needed
        .bind(i == 0)
        .execute(&mut *conn)
        .await?;
    }
    let _ = kb_id;
    Ok(())
}

pub async fn delete_entity_type(pool: &PgPool, kb_id: Uuid, id: Uuid) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    let (usage,): (i64,) = sqlx::query_as("SELECT count(*) FROM entities WHERE type_id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if usage > 0 {
        return Err(AppError::Conflict(format!(
            "Cannot delete: {usage} entities use this type"
        )));
    }
    // Whether this class carries signature criteria — it is a parent of some class, or hangs off some relation's
    // domain / range — deleting it changes "which violations hold" (0062).
    // Before writing, record the set of detection keys under the old criteria, and reconcile before commit
    let affects_criteria: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM entity_type_parents WHERE parent_id = $1)
            OR EXISTS(SELECT 1 FROM relation_type_domains WHERE entity_type_id = $1)
            OR EXISTS(SELECT 1 FROM relation_type_ranges WHERE entity_type_id = $1)",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let was_detected = if affects_criteria {
        Some(crate::reasoning::detection_keys(&mut tx, kb_id).await?)
    } else {
        None
    };
    // An attribute whose only domain is this class goes with the class: an attribute must hang on a class,
    // leaving a domain-less attribute would leave a dead row that appears nowhere.
    // One that is also attached to other classes only loses this association (the foreign key CASCADE handles it).
    // Side effects (0070's FK CASCADE): the attribute is gone → facts using it as a predicate are gone →
    // violation rows pointing at those facts are also physically deleted. These rows do not go through `criterion_changed` —
    // their criterion did not change; the fact itself is gone, the same rule as `run` clearing out vanished facts
    sqlx::query(
        "DELETE FROM relation_types r
         WHERE r.kind = 'attribute' AND r.kb_id = $1
           AND EXISTS (SELECT 1 FROM relation_type_domains d
                       WHERE d.relation_type_id = r.id AND d.entity_type_id = $2)
           AND NOT EXISTS (SELECT 1 FROM relation_type_domains d
                           WHERE d.relation_type_id = r.id AND d.entity_type_id <> $2)",
    )
    .bind(kb_id)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let res = sqlx::query("DELETE FROM entity_types WHERE id = $2 AND kb_id = $1 AND NOT builtin")
        .bind(kb_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::Conflict(
            "Built-in types cannot be deleted".into(),
        ));
    }
    if let Some(was) = was_detected {
        crate::reasoning::reconcile_ontology(&mut tx, kb_id, &was).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Attribute field validation: an attribute must have its owning class and a legal datatype; for a relation, all three are forced to be empty.
fn validate_attribute_fields(
    kind: &str,
    domains: &[Uuid],
    datatype: Option<&str>,
) -> AppResult<()> {
    match kind {
        "relation" => Ok(()),
        "attribute" => {
            if domains.is_empty() {
                return Err(AppError::invalid(
                    "attr_needs_class",
                    "An attribute needs a class (domain)",
                ));
            }
            if !matches!(datatype, Some("text" | "number" | "date" | "bool")) {
                return Err(AppError::Validation(
                    "datatype must be text / number / date / bool".into(),
                ));
            }
            Ok(())
        }
        _ => Err(AppError::Validation(
            "kind must be relation / attribute".into(),
        )),
    }
}

/// Two axioms that point at other relations must pass this check before being saved.
///
/// **The most important one is same-KB.** The foreign key on the column is `REFERENCES relation_types(id)`,
/// which does not know about knowledge bases — at the database level, a relation in KB A can point at a relation in KB B. The RDF import path
/// naturally cannot get across (it looks up by IRI within this KB), while the API takes bare UUIDs: if not blocked here,
/// any UUID would let the reasoner read axioms across KBs. **Having the frontend list only this KB's options is not enough**;
/// that is interface politeness, not a boundary.
///
/// The other two: an attribute has no inverse (its object is a literal, so the reverse makes no sense),
/// and a sub-property cannot be itself (the DB has a CHECK, but hitting it gives a 500, so a plain message is needed here).
/// **Being its own inverse is allowed**, though: that amounts to symmetric, and R0 suggests switching to `symmetric` as clearer, but it is not an error.
async fn validate_property_links(
    conn: &mut PgConnection,
    kb_id: Uuid,
    self_id: Option<Uuid>,
    kind: &str,
    ax: RelationAxioms,
) -> AppResult<()> {
    let links = [ax.inverse_of, ax.sub_property_of];
    if links.iter().all(|l| l.is_none()) {
        return Ok(());
    }
    if kind == "attribute" {
        return Err(AppError::invalid(
            "attr_has_no_link",
            "An attribute cannot have an inverse or a super-property",
        ));
    }
    if self_id.is_some() && ax.sub_property_of == self_id {
        return Err(AppError::invalid(
            "sub_property_self",
            "A relation cannot be its own super-property",
        ));
    }
    for target in links.into_iter().flatten() {
        let ok: Option<(String,)> =
            sqlx::query_as("SELECT kind FROM relation_types WHERE id = $1 AND kb_id = $2")
                .bind(target)
                .bind(kb_id)
                .fetch_optional(&mut *conn)
                .await?;
        match ok {
            // Does not distinguish "does not exist" from "in another KB": being able to learn which UUID exists elsewhere
            // is itself information that should not be given out
            None => {
                return Err(AppError::invalid(
                    "unknown_relation",
                    "That relation is not in this knowledge base",
                ));
            }
            Some((k,)) if k != "relation" => {
                return Err(AppError::invalid(
                    "link_target_is_attr",
                    "An attribute cannot be an inverse or a super-property",
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn create_relation_type<'a>(
    pool: impl sqlx::Acquire<'a, Database = sqlx::Postgres>,
    kb_id: Uuid,
    key: &str,
    label: &str,
    temporal: &str,
    ax: RelationAxioms,
    description: &str,
    kind: &str,
    domains: &[Uuid],
    ranges: &[Uuid],
    datatype: Option<&str>,
    unit: Option<&str>,
) -> AppResult<Uuid> {
    validate_key(key)?;
    validate_temporal(temporal)?;
    validate_attribute_fields(kind, domains, datatype)?;
    let mut tx = pool.begin().await?;
    let label = &lower_camel(label);
    // A newly created row's id does not exist yet, so pointing it at itself makes no sense — hence self_id is passed as None
    validate_property_links(&mut tx, kb_id, None, kind, ax).await?;
    let is_attr = kind == "attribute";
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, temporal,
                                     functional, inverse_functional, description,
                                     kind, datatype, unit,
                                     is_transitive, is_symmetric,
                                     is_asymmetric, is_irreflexive,
                                     inverse_of, sub_property_of)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                 $16, $17)",
    )
    .bind(id)
    .bind(kb_id)
    .bind(key)
    .bind(label)
    .bind(temporal)
    .bind(ax.functional)
    .bind(ax.inverse_functional)
    .bind(description)
    .bind(kind)
    .bind(is_attr.then_some(datatype).flatten())
    .bind(is_attr.then_some(unit).flatten())
    .bind(ax.transitive)
    .bind(ax.symmetric)
    .bind(ax.asymmetric)
    .bind(ax.irreflexive)
    // Attributes do not carry these two (non-empty cases were already blocked above; this is a fallback)
    .bind(if is_attr { None } else { ax.inverse_of })
    .bind(if is_attr { None } else { ax.sub_property_of })
    .execute(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            AppError::Conflict(format!("Relation key '{key}' already exists"))
        }
        _ => AppError::Db(e),
    })?;
    // An attribute does not write a range: its value domain is a literal type, which lands on the datatype
    set_domains_ranges(&mut tx, id, domains, if is_attr { &[] } else { ranges }).await?;
    tx.commit().await?;
    Ok(id)
}

/// Overwrite-style write of domain / range. **Delete first, then insert**, so it works both for new creation and for re-import,
/// and leaves no residue from the previous round.
async fn set_domains_ranges(
    conn: &mut PgConnection,
    relation_type_id: Uuid,
    domains: &[Uuid],
    ranges: &[Uuid],
) -> AppResult<()> {
    for (table, ids) in [
        ("relation_type_domains", domains),
        ("relation_type_ranges", ranges),
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE relation_type_id = $1"))
            .bind(relation_type_id)
            .execute(&mut *conn)
            .await?;
        if ids.is_empty() {
            continue;
        }
        // unnest inserts everything in one go, saving a round trip per row
        sqlx::query(&format!(
            "INSERT INTO {table} (relation_type_id, entity_type_id)
             SELECT $1, x FROM unnest($2::uuid[]) AS x
             ON CONFLICT DO NOTHING"
        ))
        .bind(relation_type_id)
        .bind(ids)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The attributes a relation declaration's own edges can carry (0037). Overwrite-style write, the same as domain / range.
///
/// Both checks live here, since CHECK cannot reach other rows: same KB, and each one is `kind = 'attribute'`
/// — attributes on an edge are literal values, reusing exactly the datatype / unit / conversion of the attribute definition.
/// A relation cannot declare itself as its own attribute (the DB has a CHECK; here a plain message).
pub async fn set_relation_qualifiers(
    pool: &PgPool,
    kb_id: Uuid,
    relation_type_id: Uuid,
    qualifier_type_ids: &[Uuid],
) -> AppResult<()> {
    if qualifier_type_ids.contains(&relation_type_id) {
        return Err(AppError::invalid(
            "qualifier_is_self",
            "A relation cannot be its own qualifier",
        ));
    }
    if !qualifier_type_ids.is_empty() {
        let (ok,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM relation_types
             WHERE kb_id = $1 AND kind = 'attribute' AND id = ANY($2)",
        )
        .bind(kb_id)
        .bind(qualifier_type_ids)
        .fetch_one(pool)
        .await?;
        if ok as usize != qualifier_type_ids.len() {
            return Err(AppError::invalid(
                "qualifier_not_attribute",
                "Every qualifier must be an attribute of this base",
            ));
        }
    }
    sqlx::query("DELETE FROM relation_type_qualifiers WHERE relation_type_id = $1")
        .bind(relation_type_id)
        .execute(pool)
        .await?;
    if !qualifier_type_ids.is_empty() {
        sqlx::query(
            "INSERT INTO relation_type_qualifiers (relation_type_id, qualifier_type_id)
             SELECT $1, x FROM unnest($2::uuid[]) AS x
             ON CONFLICT DO NOTHING",
        )
        .bind(relation_type_id)
        .bind(qualifier_type_ids)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// **Appends** an attribute declaration on an edge to a relation (0037).
///
/// Unlike the overwrite style of `set_relation_qualifiers`: during extraction several documents run in parallel, each adding
/// declarations from the corpus, and an overwrite would wipe out what someone else just added (observed: `round` was added and then gone).
/// This only adds and never deletes; on a clash with an existing one it does nothing. Validation is the same as the overwrite style.
pub async fn add_relation_qualifier(
    pool: &PgPool,
    kb_id: Uuid,
    relation_type_id: Uuid,
    qualifier_type_id: Uuid,
) -> AppResult<()> {
    if relation_type_id == qualifier_type_id {
        return Err(AppError::invalid(
            "qualifier_is_self",
            "A relation cannot be its own qualifier",
        ));
    }
    let (ok,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM relation_types
         WHERE kb_id = $1 AND kind = 'attribute' AND id = $2",
    )
    .bind(kb_id)
    .bind(qualifier_type_id)
    .fetch_one(pool)
    .await?;
    if ok != 1 {
        return Err(AppError::invalid(
            "qualifier_not_attribute",
            "Every qualifier must be an attribute of this base",
        ));
    }
    sqlx::query(
        "INSERT INTO relation_type_qualifiers (relation_type_id, qualifier_type_id)
         VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(relation_type_id)
    .bind(qualifier_type_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn update_relation_type(
    pool: &PgPool,
    kb_id: Uuid,
    id: Uuid,
    label: &str,
    temporal: &str,
    ax: RelationAxioms,
    description: &str,
    datatype: Option<&str>,
    unit: Option<&str>,
    // Group semantics: both sides None = signature unchanged; either side Some = both sides are rewritten with the passed values
    // (the side not passed is cleared). A caller that does not manage signatures (e.g. changing only the axiom bits) must pass None
    // on both sides — `domains=Some(..)` with `ranges=None` would clear the range
    domains: Option<&[Uuid]>,
    ranges: Option<&[Uuid]>,
) -> AppResult<()> {
    // The whole change goes into one transaction: if the criterion changed, before writing, record the violations detected under the old ontology,
    // after writing re-detect against the new ontology and adopt the rows of the old criterion — `open` means "reconciled against the current ontology"
    //(0062), and no open violation that holds only under the old criterion may pass the commit point
    let mut tx = pool.begin().await?;
    // The name attribute is built in (0041): marking it functional would make every second name a conflict,
    // and even turn off the primary name; renaming it or changing its tense serves no legitimate purpose either
    let name_attribute: Option<(bool,)> = sqlx::query_as(
        "SELECT builtin AND key = 'known_as' FROM relation_types WHERE id = $1 AND kb_id = $2",
    )
    .bind(id)
    .bind(kb_id)
    .fetch_optional(&mut *tx)
    .await?;
    if name_attribute == Some((true,)) {
        return Err(AppError::invalid(
            "builtin_name_attribute",
            "The name attribute is built in and cannot be edited",
        ));
    }
    // Renames are normalized too, otherwise one edit in the UI could turn lowerCamelCase back into "access to"
    let label = &lower_camel(label);
    validate_temporal(temporal)?;
    if datatype.is_some() && !matches!(datatype, Some("text" | "number" | "date" | "bool")) {
        return Err(AppError::Validation(
            "datatype must be text / number / date / bool".into(),
        ));
    }
    // Fetch the old criterion first: the axiom bits, tense, the two relation chains, and domain/range together decide
    // "which violations hold". If the row is not there it is NotFound — the UPDATE would return nothing anyway
    let old: Option<OldCriteria> = sqlx::query_as(
        "SELECT kind, temporal, functional, inverse_functional, is_transitive,
                is_symmetric, is_asymmetric, is_irreflexive, inverse_of, sub_property_of
           FROM relation_types WHERE id = $1 AND kb_id = $2",
    )
    .bind(id)
    .bind(kb_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(old) = old else {
        return Err(AppError::NotFound);
    };
    // The two that point at other relations must first be checked against the database: whether the target is in this KB, and whether it is a relation.
    // Only checked when actually filled in — clearing (both None) has no target to validate
    if ax.inverse_of.is_some() || ax.sub_property_of.is_some() {
        validate_property_links(&mut tx, kb_id, Some(id), &old.kind, ax).await?;
    }
    // domain/range can be changed — they are the signature, not the identity; "this attribute also applies to contractors" is a legitimate
    // edit. kind is immutable (changing it would scramble the semantics of existing facts)
    let signature_touched = domains.is_some() || ranges.is_some();
    let mut signature_changed = false;
    let next_domains: &[Uuid] = domains.unwrap_or(&[]);
    let next_ranges: &[Uuid] = if old.kind == "attribute" {
        // An attribute has no range: its value domain is a literal type, which lands on the datatype
        &[]
    } else {
        ranges.unwrap_or(&[])
    };
    if signature_touched {
        if old.kind == "attribute" && next_domains.is_empty() {
            return Err(AppError::invalid(
                "attr_needs_class",
                "An attribute needs a class (domain)",
            ));
        }
        let old_domains: HashSet<Uuid> = sqlx::query_scalar(
            "SELECT entity_type_id FROM relation_type_domains WHERE relation_type_id = $1",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
        let old_ranges: HashSet<Uuid> = sqlx::query_scalar(
            "SELECT entity_type_id FROM relation_type_ranges WHERE relation_type_id = $1",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
        signature_changed = old_domains != next_domains.iter().copied().collect::<HashSet<_>>()
            || old_ranges != next_ranges.iter().copied().collect::<HashSet<_>>();
    }
    // label / description / datatype / unit are not in here: they do not change the criterion,
    // so changing them is not worth a run of detection
    let criteria_changed = old.temporal != temporal
        || old.functional != ax.functional
        || old.inverse_functional != ax.inverse_functional
        || old.is_transitive != ax.transitive
        || old.is_symmetric != ax.symmetric
        || old.is_asymmetric != ax.asymmetric
        || old.is_irreflexive != ax.irreflexive
        || old.inverse_of != ax.inverse_of
        || old.sub_property_of != ax.sub_property_of
        || signature_changed;
    // Detection must come **before** the write: at this moment the database still holds the old ontology, so what is recorded is exactly "the violations
    // that would be computed under the old criterion", to compare against before commit
    let was_detected = if criteria_changed {
        Some(crate::reasoning::detection_keys(&mut tx, kb_id).await?)
    } else {
        None
    };
    // The two chains are **overwritten together with the six axiom bits**: an omitted value = clear, not "leave unchanged".
    // Same rule as for `is_transitive` and the others — they are one set of declarations submitted together in the same form;
    // the semantics that really cause trouble are the half overwritten and half kept ones
    let res = sqlx::query(
        "UPDATE relation_types
            SET label = $3, temporal = $4,
                functional = $5, inverse_functional = $6, description = $7,
                datatype = CASE WHEN kind = 'attribute' AND $8 IS NOT NULL THEN $8 ELSE datatype END,
                unit = CASE WHEN kind = 'attribute' THEN $9 ELSE unit END,
                is_transitive = $10, is_symmetric = $11,
                is_asymmetric = $12, is_irreflexive = $13,
                inverse_of = $14, sub_property_of = $15, updated_at = now()
         WHERE id = $2 AND kb_id = $1",
    )
    .bind(kb_id)
    .bind(id)
    .bind(label)
    .bind(temporal)
    .bind(ax.functional)
    .bind(ax.inverse_functional)
    .bind(description)
    .bind(datatype)
    .bind(unit)
    .bind(ax.transitive)
    .bind(ax.symmetric)
    .bind(ax.asymmetric)
    .bind(ax.irreflexive)
    .bind(ax.inverse_of)
    .bind(ax.sub_property_of)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    if signature_touched {
        set_domains_ranges(&mut tx, id, next_domains, next_ranges).await?;
    }
    if let Some(was) = was_detected {
        crate::reasoning::reconcile_ontology(&mut tx, kb_id, &was).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The columns of a `relation_types` row relevant to "which violations hold" (the old criterion snapshot used by 0062 reconciliation).
/// label / description / datatype / unit are not included — they do not change the criterion
#[derive(sqlx::FromRow)]
struct OldCriteria {
    kind: String,
    temporal: String,
    functional: bool,
    inverse_functional: bool,
    is_transitive: bool,
    is_symmetric: bool,
    is_asymmetric: bool,
    is_irreflexive: bool,
    inverse_of: Option<Uuid>,
    sub_property_of: Option<Uuid>,
}

pub async fn delete_relation_type(pool: &PgPool, kb_id: Uuid, id: Uuid) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    let (usage,): (i64,) = sqlx::query_as("SELECT count(*) FROM facts WHERE predicate_id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if usage > 0 {
        return Err(AppError::Conflict(format!(
            "Cannot delete: {usage} facts use this relation"
        )));
    }
    // A relation's contribution to the criterion is not only the half "is anybody referencing it": its own flags and its outgoing
    // inverse_of / sub_property_of links feed the derivation chains of other predicates, and rules_disagree
    // defect rows also point at it by predicate id. Enumerating which columns count as criterion has been shown to leak — and deletion is rare enough
    // that it is not worth gating detection to save cost; always record the detection keys under the old criterion (0062)
    let was_detected = crate::reasoning::detection_keys(&mut tx, kb_id).await?;
    // 0070 builds these two foreign keys as DEFERRABLE SET NULL — the cascade action only lands at commit time,
    // but reconciliation detection needs to see the changed criterion, so the links are explicitly cut in the transaction first
    sqlx::query("UPDATE relation_types SET inverse_of = NULL WHERE inverse_of = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE relation_types SET sub_property_of = NULL WHERE sub_property_of = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let res =
        sqlx::query("DELETE FROM relation_types WHERE id = $2 AND kb_id = $1 AND NOT builtin")
            .bind(kb_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::Conflict(
            "Built-in relations cannot be deleted".into(),
        ));
    }
    crate::reasoning::reconcile_ontology(&mut tx, kb_id, &was_detected).await?;
    tx.commit().await?;
    Ok(())
}

/* ---- Unmatched statistics ---- */

/* ---- OWL import ---- */

/// Creates a class with an IRI. The IRI is the global identity; re-import matches by it (see 0001 P2).
pub async fn create_entity_type_with_iri(
    pool: &PgPool,
    kb_id: Uuid,
    key: &str,
    label: &str,
    description: &str,
    iri: &str,
) -> AppResult<Uuid> {
    validate_key(key)?;
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label, color, shape, description, iri)
         VALUES ($1, $2, $3, $4, $7, $8, $5, $6)",
    )
    .bind(id)
    .bind(kb_id)
    .bind(key)
    .bind(label)
    .bind(description)
    .bind(iri)
    // Pick the color by key rather than giving every class one grey-blue — importing a large ontology is only worth looking at if it has color
    .bind(crate::palette::color_for_key(key))
    // The shape records the provenance: this path carries an IRI, which means it was declared by the vocabulary
    .bind(crate::palette::shape_for(iri))
    .execute(pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            AppError::Conflict(format!("Type key '{key}' already exists"))
        }
        _ => AppError::Db(e),
    })?;
    Ok(id)
}

/// On re-import, update label and description by IRI. **The key does not change** — it may already have been extracted into entities
/// and referenced in prompts; changing it would break references in existing data. Upstream changing a label is normal,
/// while the IRI is the identity.
///
/// **`display_hints` are overwritten** (RC-008): the file is the truth about how a type
/// is shown, so a file that says nothing clears what an earlier one said.
pub async fn update_type_from_import(
    pool: &PgPool,
    kb_id: Uuid,
    iri: &str,
    label: &str,
    description: &str,
    display_hints: &serde_json::Value,
) -> AppResult<Option<Uuid>> {
    validate_display_hints(display_hints)?;
    let row: Option<(Uuid,)> = sqlx::query_as(
        "UPDATE entity_types
         SET label = $3,
             -- 空描述不覆盖已有的：上游可能没写 rdfs:comment，而本地可能
             -- 已经被人按自己的语料调过，那份调整比空值有价值
             description = CASE WHEN $4 = '' THEN description ELSE $4 END,
             display_hints = $5,
             updated_at = now()
         WHERE kb_id = $1 AND iri = $2 RETURNING id",
    )
    .bind(kb_id)
    .bind(iri)
    .bind(label)
    .bind(description)
    .bind(display_hints)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

/// Creates a relation from the import (`kind='relation'`), with an IRI.
///
/// `temporal` is fixed at `state`: OWL has no corresponding concept, and state (which has an interval) is the only one of the three
/// that loses no information — event would squash the interval into a point, and eternal would claim it never changes.
///
/// **`functional` / `inverse_functional` are written as the vocabulary says.** They drive the temporal engine to auto-close
/// old facts; a wrong guess produces conflicts in bulk (59 of them for `part_of`). The preview already lists the relations declared functional
/// separately for people to review, so here we do not presume to change them to false.
#[allow(clippy::too_many_arguments)]
pub async fn create_relation_with_iri(
    pool: &PgPool,
    kb_id: Uuid,
    key: &str,
    label: &str,
    description: &str,
    iri: &str,
    functional: bool,
    inverse_functional: bool,
    domains: &[Uuid],
    ranges: &[Uuid],
) -> AppResult<Option<Uuid>> {
    validate_key(key)?;
    let id = Uuid::now_v7();
    let row: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO relation_types
             (id, kb_id, key, label, temporal, functional, inverse_functional,
              description, kind, iri)
         VALUES ($1, $2, $3, $4, 'state', $5, $6, $7, 'relation', $8)
         ON CONFLICT (kb_id, key) DO NOTHING
         RETURNING id",
    )
    .bind(id)
    .bind(kb_id)
    .bind(key)
    .bind(label)
    .bind(functional)
    .bind(inverse_functional)
    .bind(description)
    .bind(iri)
    .fetch_optional(pool)
    .await?;
    let Some((new_id,)) = row else {
        return Ok(None);
    };
    set_domains_ranges(&mut *pool.acquire().await?, new_id, domains, ranges).await?;
    Ok(Some(new_id))
}

/// Updates an already-adopted relation by IRI on re-import.
///
/// **The key does not change** (it may already be referenced by facts), **an empty description does not overwrite** (what a person wrote beats upstream),
/// **domain/range are rewritten wholesale** — they are the upstream-declared structure, not wording someone tuned.
/// **`temporal` is written only when the ontology states it** (`Some`): an ontology that does not write `utopia:temporal` leaves it alone,
/// so what a person changed on the page is kept; what is written follows the ontology, for the same reason as domain/range.
/// A real change bumps `updated_at`, the same as an edit on the page: the 0053 phrase bindings read it,
/// since state vs non-state decides how phrases bind, and decisions made under the old value must be re-examinable.
/// **`display_hints` are always overwritten** (RC-008), unlike `temporal`: they are
/// display, not a judgement a person may have made, and the file is the truth about them.
#[allow(clippy::too_many_arguments)]
pub async fn update_relation_from_import(
    pool: &PgPool,
    kb_id: Uuid,
    iri: &str,
    label: &str,
    description: &str,
    domains: &[Uuid],
    ranges: &[Uuid],
    temporal: Option<&str>,
    display_hints: &serde_json::Value,
) -> AppResult<bool> {
    if let Some(t) = temporal {
        validate_temporal(t)?;
    }
    validate_display_hints(display_hints)?;
    let row: Option<(Uuid,)> = sqlx::query_as(
        "UPDATE relation_types
            SET label = $3,
                description = CASE WHEN $4 = '' THEN description ELSE $4 END,
                temporal = COALESCE($5::text, temporal),
                updated_at = CASE WHEN $5::text <> temporal THEN now() ELSE updated_at END,
                display_hints = $6
          WHERE kb_id = $1 AND iri = $2
          RETURNING id",
    )
    .bind(kb_id)
    .bind(iri)
    .bind(label)
    .bind(description)
    .bind(temporal)
    .bind(display_hints)
    .fetch_optional(pool)
    .await?;
    let Some((id,)) = row else {
        return Ok(false);
    };
    set_domains_ranges(&mut *pool.acquire().await?, id, domains, ranges).await?;
    Ok(true)
}

/// Records one import. The original text has already been stored in the blob, content-addressed; here we only do the bookkeeping.
#[allow(clippy::too_many_arguments)]
pub async fn record_import(
    pool: &PgPool,
    kb_id: Uuid,
    sha256: &str,
    filename: &str,
    format: &str,
    byte_size: i64,
    summary: &serde_json::Value,
    actor: Uuid,
) -> AppResult<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO ontology_imports
            (id, kb_id, sha256, filename, format, byte_size, summary, imported_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(kb_id)
    .bind(sha256)
    .bind(filename)
    .bind(format)
    .bind(byte_size)
    .bind(summary)
    .bind(actor)
    .execute(pool)
    .await?;
    Ok(id)
}

/// A KB's import history (with the importer's display name; NULL after the account is deleted).
pub async fn list_imports(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<OntologyImportView>> {
    Ok(sqlx::query_as(
        "SELECT i.id, i.filename, i.format, i.byte_size, i.summary, i.imported_at,
                u.display_name AS imported_by_name
         FROM ontology_imports i
         LEFT JOIN users u ON u.id = i.imported_by
         WHERE i.kb_id = $1 ORDER BY i.imported_at DESC LIMIT 50",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}

/// An ontology row waiting to be embedded. `text` is the passage to send for embedding; `kind` decides which table it is written back to.
#[derive(Debug, Clone)]
pub struct TypeToEmbed {
    pub id: Uuid,
    pub kind: TypeKind,
    pub text: String,
    /// Which column group it is written to. A class has two vectors: the whole passage (label + description) and one with only the label,
    /// serving respectively long profile queries and short phrase queries (see `entity_types.label_embedding`)
    pub field: EmbedField,
}

/// The two vectors of one row. **Short queries match Label, long profiles match Full** — queries come in two shapes,
/// and documents must too, otherwise short queries would be taken over by tautological classes (the `Map\nA map.` kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedField {
    Full,
    Label,
}

/// Which table an ontology row belongs to. Classes go in `entity_types`; relations and attributes share `relation_types`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Entity,
    Relation,
}

/// The passage used for embedding: label first, description after.
///
/// **The key is not included.** The key is the token the model reads and writes (`founding_date`); the description is where the meaning
/// of the type lies. Mixing the key in would let retrieval be misled by "two keys that look alike" — and
/// `position` (a list rank) and `position` (a job title) are exactly two different things that look identical.
fn embed_text(label: &str, description: &str) -> String {
    let d = description.trim();
    if d.is_empty() {
        label.trim().to_string()
    } else {
        format!("{}\n{}", label.trim(), d)
    }
}

/// Which ontology rows have stale vectors (never embedded, the description changed, or the embedding model was swapped).
///
/// The criterion is **comparing the source text embedded at the time and the model name**, not timestamps: if the description changed or the model was swapped,
/// a timestamp would show no difference. This also avoids needing a hook at every write point that edits a description — miss one and it quietly rots.
///
/// `only` limits backfilling to one half. Type resolution uses only classes, and waiting for the 1633 relations to finish embedding is six minutes of pure waiting;
/// while the background backfill task has no limit, and both fill the same batch of rows, so whichever runs first counts.
pub async fn types_needing_embedding(
    pool: &PgPool,
    kb_id: Uuid,
    model: &str,
    only: Option<TypeKind>,
) -> AppResult<Vec<TypeToEmbed>> {
    if only == Some(TypeKind::Relation) {
        // Type resolution uses only classes; waiting for 1633 relations to finish embedding is six minutes of wasted waiting
        return relations_needing_embedding(pool, kb_id, model).await;
    }
    // **Staleness is judged by the same function that generated the source text** (#672). Previously SQL reassembled the passage with `btrim`
    // before comparing, while `embed_text` uses Rust's `trim`: `btrim` strips only spaces, and `trim` also strips newlines and tabs. schema.org descriptions end with a newline and spaces, so the stored text
    // and the SQL-built one never matched, and every round judged those rows stale — the backfill task re-embedded the same batch over and over,
    // and the extraction gate (#526) waited on an index that never finished
    // until the deadline failed the document.
    // The same lesson as `mappings::needing_embedding`: pull the rows back and compare in Rust
    let rows: Vec<StoredEmbedding> = sqlx::query_as(
        "SELECT id, label, coalesce(description, '') AS description,
                embedding IS NOT NULL AS embedded, embedded_model, embedded_text,
                label_embedding IS NOT NULL AS label_embedded, label_embedded_model, label_embedded_text
           FROM entity_types
          WHERE kb_id = $1",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    let mut full = Vec::new();
    let mut labels = Vec::new();
    for r in rows {
        let text = embed_text(&r.label, &r.description);
        if is_stale(
            r.embedded,
            r.embedded_model.as_deref(),
            r.embedded_text.as_deref(),
            model,
            &text,
        ) {
            full.push(TypeToEmbed {
                id: r.id,
                kind: TypeKind::Entity,
                text,
                field: EmbedField::Full,
            });
        }
        // The embedding used only for labels (see `entity_types.label_embedding`). Short queries go through this index — queries come in two shapes,
        // and documents must too, otherwise short queries get taken over by tautological classes
        let short = r.label.trim().to_string();
        if is_stale(
            r.label_embedded,
            r.label_embedded_model.as_deref(),
            r.label_embedded_text.as_deref(),
            model,
            &short,
        ) {
            labels.push(TypeToEmbed {
                id: r.id,
                kind: TypeKind::Entity,
                text: short,
                field: EmbedField::Label,
            });
        }
    }
    let mut out = full;
    out.extend(labels);
    if only == Some(TypeKind::Entity) {
        return Ok(out);
    }
    out.extend(relations_needing_embedding(pool, kb_id, model).await?);
    Ok(out)
}

async fn relations_needing_embedding(
    pool: &PgPool,
    kb_id: Uuid,
    model: &str,
) -> AppResult<Vec<TypeToEmbed>> {
    // Criterion as above (#672): compare in Rust using `embed_text`, not by reassembling in SQL. Relations have only the full-passage
    // vector; the three label columns are left blank.
    //
    // Name attributes are not embedded (0041): with no vector they never come out of nearest-neighbour retrieval, so ontology proposals, attribute merging,
    // and the candidate lists during extraction never touch them — names only go through the `names` route in the extraction reply
    let rows: Vec<StoredEmbedding> = sqlx::query_as(
        "SELECT id, label, coalesce(description, '') AS description,
                embedding IS NOT NULL AS embedded, embedded_model, embedded_text,
                false AS label_embedded, NULL::text AS label_embedded_model,
                NULL::text AS label_embedded_text
           FROM relation_types
          WHERE kb_id = $1 AND NOT (builtin AND key = 'known_as')",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let text = embed_text(&r.label, &r.description);
            is_stale(
                r.embedded,
                r.embedded_model.as_deref(),
                r.embedded_text.as_deref(),
                model,
                &text,
            )
            .then_some(TypeToEmbed {
                id: r.id,
                kind: TypeKind::Relation,
                text,
                // Relations have no short-query path, only this one full passage
                field: EmbedField::Full,
            })
        })
        .collect())
}

/// The current state of the two vectors on a type row, for judging staleness
#[derive(sqlx::FromRow)]
struct StoredEmbedding {
    id: Uuid,
    label: String,
    description: String,
    embedded: bool,
    embedded_model: Option<String>,
    embedded_text: Option<String>,
    label_embedded: bool,
    label_embedded_model: Option<String>,
    label_embedded_text: Option<String>,
}

/// Not embedded yet, the model was swapped, or the source text embedded then differs from the one to be embedded now
fn is_stale(
    embedded: bool,
    stored_model: Option<&str>,
    stored_text: Option<&str>,
    model: &str,
    text: &str,
) -> bool {
    !embedded || stored_model != Some(model) || stored_text != Some(text)
}

/// Writes vectors back, together with "which text was embedded and which model was used". The three must be written in one go —
/// writing the vector without its source means the next round will think it is still stale, and from then on every round re-embeds it.
pub async fn set_type_embeddings(
    pool: &PgPool,
    model: &str,
    items: &[(TypeToEmbed, Vec<f32>)],
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    for (item, emb) in items {
        let table = match item.kind {
            TypeKind::Entity => "entity_types",
            TypeKind::Relation => "relation_types",
        };
        // The two vectors are written to their own columns (see `entity_types.label_embedding`). Column name prefixes differ; the rest is identical
        let (vec_col, text_col, model_col) = match item.field {
            EmbedField::Full => ("embedding", "embedded_text", "embedded_model"),
            EmbedField::Label => (
                "label_embedding",
                "label_embedded_text",
                "label_embedded_model",
            ),
        };
        sqlx::query(&format!(
            "UPDATE {table} SET {vec_col} = $2, {text_col} = $3, {model_col} = $4
             WHERE id = $1"
        ))
        .bind(item.id)
        .bind(Vector::from(emb.clone()))
        .bind(&item.text)
        .bind(model)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The k entity types nearest the given vector.
///
/// **Classes with neither a description nor a place in the class tree do not take part.** When importing a vocabulary, every external IRI
/// referenced by domainIncludes / rangeIncludes / equivalentClass gets built as a row
/// (OMG, UNECE, GS1…); they have no label text, no parent and no child — they are not vocabulary, they are dangling references.
///
/// They are unfortunately quite likely to win: `embed_text` degrades to embedding only the label when the description is empty, so this row embeds
/// just the word "Location". The shorter side's distance is systematically smaller (the same pattern has already bitten this file
/// and type resolution three times), so an empty shell beat `administrative_area` with its 83-character definition —
/// measured: a query for "Gongshu District, Hangzhou" was lost exactly this way.
///
/// And even if they were pushed through, they cannot be judged: the adjudicator sees `- location (location)`, with no definition to rely on.
/// In a KB that loads schema.org there are 50 such rows, and 43 of them have not a single inheritance edge.
///
/// **They are merely not made candidates; the rows are not deleted**: domain / range still point at them, and deleting would break references.
pub async fn nearest_entity_types(
    pool: &PgPool,
    kb_id: Uuid,
    embedding: &[f32],
    limit: i64,
    // true = the one with the full passage, rather than the label-only vector (see `entity_types.label_embedding`). Short phrasings take this path: **short against short**,
    // otherwise `district. place` would be beaten by one-line tautologies like `Map\nA map.`
    by_label: bool,
) -> AppResult<Vec<TypeCandidate>> {
    let col = if by_label {
        "label_embedding"
    } else {
        "embedding"
    };
    let rows: Vec<(Uuid, String, String, String, f64)> = sqlx::query_as(&format!(
        "SELECT id, key, label, coalesce(description, ''), ({col} <=> $2)::float8
         FROM entity_types t
         WHERE t.kb_id = $1 AND t.{col} IS NOT NULL
           AND (coalesce(btrim(t.description), '') <> ''
                OR EXISTS (SELECT 1 FROM entity_type_parents p
                           WHERE p.child_id = t.id OR p.parent_id = t.id))
         ORDER BY {col} <=> $2
         LIMIT $3"
    ))
    .bind(kb_id)
    .bind(Vector::from(embedding.to_vec()))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, key, label, description, distance)| TypeCandidate {
            id,
            key,
            label,
            description,
            kind: None,
            distance: distance as f32,
        })
        .collect())
}

/// The k relations/attributes nearest the given vector.
///
/// `only_kind` splits the lanes: facts with a literal object look for attributes, and those with an entity object look for relations.
/// Without splitting, an attribute like `founding_date` could be pushed onto a relation fact, and vice versa.
pub async fn nearest_relation_types(
    pool: &PgPool,
    kb_id: Uuid,
    embedding: &[f32],
    limit: i64,
    only_kind: Option<&str>,
) -> AppResult<Vec<TypeCandidate>> {
    let rows: Vec<(Uuid, String, String, String, String, f64)> = sqlx::query_as(
        "SELECT id, key, label, coalesce(description, ''), kind, (embedding <=> $2)::float8
         FROM relation_types
         WHERE kb_id = $1 AND embedding IS NOT NULL
           AND ($4::text IS NULL OR kind = $4)
         ORDER BY embedding <=> $2
         LIMIT $3",
    )
    .bind(kb_id)
    .bind(Vector::from(embedding.to_vec()))
    .bind(limit)
    .bind(only_kind)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, key, label, description, kind, distance)| TypeCandidate {
                id,
                key,
                label,
                description,
                kind: Some(kind),
                distance: distance as f32,
            },
        )
        .collect())
}

/// Claims an IRI onto an existing **local** class (one that had no IRI before).
///
/// **Writes only the IRI and shape; label, description and color are not touched.** Claiming is about fixing "this tree is broken",
/// not about "overwriting the user's wording with the vocabulary's wording": the seed classes' descriptions were tuned against extraction
/// and follow the KB's language, while schema.org's descriptions are English boilerplate. Overwriting them would be quietly swapping out
/// the most load-bearing sentence in the extraction prompt.
///
/// **The shape must change along with it, because the shape says where it came from** (square = declared by the vocabulary, circle = grown from the corpus).
/// A class claimed as "declared by the vocabulary" but still drawn as a circle makes the picture lie. This seam was measured:
/// importing schema.org into a KB that already has `person` / `organization`,
/// those classes got an IRI but stayed circles — an IRI with a circle shape is self-contradictory.
///
/// Color is not touched: color is **identity** (the same key is always the same color); claiming does not change who it is.
/// Shape is **provenance**, and claiming precisely changes that.
///
/// Written only when `iri IS NULL`, so repeated imports are idempotent, and it never takes a class that another
/// vocabulary has already claimed.
pub async fn adopt_iri_onto_key(
    pool: &PgPool,
    kb_id: Uuid,
    key: &str,
    iri: &str,
) -> AppResult<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "UPDATE entity_types SET iri = $3, shape = 'square', updated_at = now()
         WHERE kb_id = $1 AND key = $2 AND iri IS NULL
         RETURNING id",
    )
    .bind(kb_id)
    .bind(key)
    .bind(iri)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

pub async fn nearest_relation_type_ids(
    pool: &PgPool,
    kb_id: Uuid,
    embedding: &[f32],
    limit: i64,
    only_kind: Option<&str>,
) -> AppResult<Vec<Uuid>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM relation_types
         WHERE kb_id = $1 AND embedding IS NOT NULL
           AND ($4::text IS NULL OR kind = $4)
         ORDER BY embedding <=> $2
         LIMIT $3",
    )
    .bind(kb_id)
    .bind(Vector::from(embedding.to_vec()))
    .bind(limit)
    .bind(only_kind)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Whether this KB has any relations/attributes with vectors (`only_kind` as above). Its predicate is identical to [`nearest_relation_type_ids`]:
/// here it answers "none", while that one returns empty for any vector.
///
/// Why it exists: the short-phrase alignment shortlist embeds each signature first, then asks for the nearest attribute one by one; only the empty answer reveals
/// that attributes have no vectors yet. A KB that never ran `embed_ontology` sent 17,000 signatures for embedding every round,
/// none of which were ever used (#1097). Ask this question first; if it is unused, do not embed.
pub async fn has_relation_type_vectors(
    pool: &PgPool,
    kb_id: Uuid,
    only_kind: Option<&str>,
) -> AppResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM relation_types
                        WHERE kb_id = $1 AND embedding IS NOT NULL
                          AND ($2::text IS NULL OR kind = $2))",
    )
    .bind(kb_id)
    .bind(only_kind)
    .fetch_one(pool)
    .await?)
}

/// Inserts a batch of classes in one go, returns key → id.
///
/// **The reason it exists is fsync.** Executing each row individually via `execute(pool)` commits each one separately; importing schema.org
/// at that scale (968 classes plus about 1,500 attributes) is 5,000 fsyncs, measured at 45 seconds; the same number of rows
/// in one statement takes 536 milliseconds. The difference is not round trips, it is commits.
///
/// `ON CONFLICT DO NOTHING` rather than an error: the disposition for a key clash was already decided at the planning stage
/// ([`crate::ontology::create_entity_type_with_iri`] explains why it does not overwrite),
/// here we only execute the plan; a clash means the plan and the DB are out of sync, so skip it and let the caller discover from the returned map who is
/// missing.
pub async fn create_entity_types_bulk(
    pool: &PgPool,
    kb_id: Uuid,
    rows: &[BulkClass],
) -> AppResult<std::collections::HashMap<String, Uuid>> {
    if rows.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    for r in rows {
        validate_key(&r.key)?;
        validate_display_hints(&r.display_hints)?;
    }
    let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    let descs: Vec<&str> = rows.iter().map(|r| r.description.as_str()).collect();
    let iris: Vec<&str> = rows.iter().map(|r| r.iri.as_str()).collect();
    let hints: Vec<&serde_json::Value> = rows.iter().map(|r| &r.display_hints).collect();
    // Colors are computed on the Rust side by key and go in alongside UNNEST —
    // SQL cannot call a Rust function, and this batch is the path taken when importing a large ontology
    let colours: Vec<&str> = keys
        .iter()
        .map(|k| crate::palette::color_for_key(k))
        .collect();
    let shapes: Vec<&str> = iris.iter().map(|i| crate::palette::shape_for(i)).collect();
    let out: Vec<(Uuid, String)> = sqlx::query_as(
        "INSERT INTO entity_types
             (id, kb_id, key, label, color, shape, description, iri, display_hints)
         SELECT gen_random_uuid(), $1, k, l, c, s, d, i, h
         FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[],
                     $8::jsonb[])
              AS t(k, l, d, i, c, s, h)
         ON CONFLICT (kb_id, key) DO NOTHING
         RETURNING id, key",
    )
    .bind(kb_id)
    .bind(&keys)
    .bind(&labels)
    .bind(&descs)
    .bind(&iris)
    .bind(&colours)
    .bind(&shapes)
    .bind(&hints)
    .fetch_all(pool)
    .await?;
    Ok(out.into_iter().map(|(id, k)| (k, id)).collect())
}

/// A class to create in one bulk insert; see [`create_entity_types_bulk`].
pub struct BulkClass {
    pub key: String,
    pub label: String,
    pub description: String,
    pub iri: String,
    /// The `utopia:*` display hints the file declared (RC-008), `{}` for none
    pub display_hints: serde_json::Value,
}

/// One row when creating relations/attributes in bulk.
///
/// **`functional` / `inverse_functional` must be written as the vocabulary says; they cannot default to false.**
/// They are the basis on which the temporal engine auto-closes facts; a wrong guess makes conflicts in bulk — `part_of` once marked
/// functional accumulated 59 conflicts.
pub struct BulkRelation {
    pub key: String,
    pub label: String,
    pub description: String,
    pub iri: String,
    /// `relation` or `attribute`
    pub kind: &'static str,
    /// Only meaningful for attributes; relations pass `None`
    pub datatype: Option<String>,
    pub functional: bool,
    pub inverse_functional: bool,
    /// OWL property axioms, the criterion for consistency checks (0002 R0).
    /// Likewise must be written as the vocabulary says — `alias_of` being bidirectional is right, `produces` being bidirectional is wrong,
    /// and only the ontology can tell the two apart
    pub transitive: bool,
    pub symmetric: bool,
    pub asymmetric: bool,
    pub irreflexive: bool,
    /// `state` / `event` / `eternal` (0031). OWL itself has no such concept; object properties in standard vocabularies are all
    /// `state`; only what the ontology states with `utopia:temporal` is otherwise — the export side writes that one
    pub temporal: String,
    /// The `utopia:*` display hints the file declared (RC-008), `{}` for none
    pub display_hints: serde_json::Value,
}

/// Inserts a batch of relations or attributes in one go, returns key → id. Semantics as [`create_entity_types_bulk`].
///
/// `kind` decides whether it goes through the relation channel or the attribute channel; `datatype` is meaningful only for attributes, relations pass `None`.
/// `temporal` is written per row: an ontology without a `utopia:temporal` annotation is always `state`
///(consistent with the default in the single-row version), and written ones follow what they state.
pub async fn create_relation_types_bulk(
    pool: &PgPool,
    kb_id: Uuid,
    rows: &[BulkRelation],
) -> AppResult<std::collections::HashMap<String, Uuid>> {
    if rows.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    for r in rows {
        validate_key(&r.key)?;
        validate_temporal(&r.temporal)?;
        validate_display_hints(&r.display_hints)?;
    }
    let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    // The imported vocabulary may itself be inconsistent (in unece.org, `brandName` and
    // `ApplicableCertificate` coexist) — unify before saving; the original is kept in `iri`
    let camel: Vec<String> = rows.iter().map(|r| lower_camel(&r.label)).collect();
    let labels: Vec<&str> = camel.iter().map(String::as_str).collect();
    let descs: Vec<&str> = rows.iter().map(|r| r.description.as_str()).collect();
    let iris: Vec<&str> = rows.iter().map(|r| r.iri.as_str()).collect();
    let kinds: Vec<&str> = rows.iter().map(|r| r.kind).collect();
    let dts: Vec<Option<&str>> = rows.iter().map(|r| r.datatype.as_deref()).collect();
    let funcs: Vec<bool> = rows.iter().map(|r| r.functional).collect();
    let invs: Vec<bool> = rows.iter().map(|r| r.inverse_functional).collect();
    let trans: Vec<bool> = rows.iter().map(|r| r.transitive).collect();
    let syms: Vec<bool> = rows.iter().map(|r| r.symmetric).collect();
    let asyms: Vec<bool> = rows.iter().map(|r| r.asymmetric).collect();
    let irrefs: Vec<bool> = rows.iter().map(|r| r.irreflexive).collect();
    let temporals: Vec<&str> = rows.iter().map(|r| r.temporal.as_str()).collect();
    let hints: Vec<&serde_json::Value> = rows.iter().map(|r| &r.display_hints).collect();
    let out: Vec<(Uuid, String)> = sqlx::query_as(
        "INSERT INTO relation_types
             (id, kb_id, key, label, temporal, functional, inverse_functional,
              description, kind, datatype, iri,
              is_transitive, is_symmetric, is_asymmetric, is_irreflexive, display_hints)
         SELECT gen_random_uuid(), $1, k, l, tm, fu, iv, d, kind, dt, i,
                tr, sy, asym, irr, h
         FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[],
                     $8::bool[], $9::bool[], $10::bool[], $11::bool[], $12::bool[], $13::bool[],
                     $14::text[], $15::jsonb[])
              AS t(k, l, d, i, kind, dt, fu, iv, tr, sy, asym, irr, tm, h)
         ON CONFLICT (kb_id, key) DO NOTHING
         RETURNING id, key",
    )
    .bind(kb_id)
    .bind(&keys)
    .bind(&labels)
    .bind(&descs)
    .bind(&iris)
    .bind(&kinds)
    .bind(&dts)
    .bind(&funcs)
    .bind(&invs)
    .bind(&trans)
    .bind(&syms)
    .bind(&asyms)
    .bind(&irrefs)
    .bind(&temporals)
    .bind(&hints)
    .fetch_all(pool)
    .await?;
    Ok(out.into_iter().map(|(id, k)| (k, id)).collect())
}

/// Writes the domain / range of a batch of relations in one go.
///
/// The single-row [`set_domains_ranges`] already uses unnest to save round trips, but it runs 4 statements for **each relation**
/// (one DELETE and one INSERT for each of the two tables). 1,500 relations would be 6,000 separate commits
/// — and commits are the cost. Here the association rows of all relations are flattened into two statements.
///
/// **No DELETE:** the caller has just created the relations, so the association tables cannot hold any old rows.
/// Updating existing relations still goes through the single-row version.
pub async fn link_domains_ranges_bulk(
    pool: &PgPool,
    domains: &[(Uuid, Uuid)],
    ranges: &[(Uuid, Uuid)],
) -> AppResult<()> {
    for (table, pairs) in [
        ("relation_type_domains", domains),
        ("relation_type_ranges", ranges),
    ] {
        if pairs.is_empty() {
            continue;
        }
        let rels: Vec<Uuid> = pairs.iter().map(|p| p.0).collect();
        let types: Vec<Uuid> = pairs.iter().map(|p| p.1).collect();
        sqlx::query(&format!(
            "INSERT INTO {table} (relation_type_id, entity_type_id)
             SELECT r, t FROM UNNEST($1::uuid[], $2::uuid[]) AS x(r, t)
             ON CONFLICT DO NOTHING"
        ))
        .bind(&rels)
        .bind(&types)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Sets the parents of a batch of classes in one go. Semantics as [`set_parents`], but **does not check for cycles**.
///
/// The single-row version runs a recursive CTE to check for cycles every time, so a thousand classes would be a thousand recursive queries.
/// This is used only for classes newly created by an import: a cycle is a problem in the upstream vocabulary, and `set_parents`'s handling of a cycle
/// was already just to skip that one edge (`let _ =`), not to abort the import. After the import the ontology page
/// can still find it and let a person deal with it.
pub async fn set_parents_bulk(pool: &PgPool, pairs: &[(Uuid, Uuid)]) -> AppResult<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    let children: Vec<Uuid> = pairs.iter().map(|p| p.0).collect();
    let parents: Vec<Uuid> = pairs.iter().map(|p| p.1).collect();
    sqlx::query(
        "INSERT INTO entity_type_parents (child_id, parent_id)
         SELECT c, p FROM UNNEST($1::uuid[], $2::uuid[]) AS x(c, p)
         WHERE c <> p
         ON CONFLICT DO NOTHING",
    )
    .bind(&children)
    .bind(&parents)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persists class disjointness (see the axiom columns of `relation_types`). Semantics as [`set_parents_bulk`].
///
/// **Both directions are written.** The parsing side has already expanded the symmetry of `owl:disjointWith` into two rows,
/// so here we simply write them as-is — checking "are A and B disjoint" therefore does not care which end it is asked from.
///
/// `a <> b` blocks self-reference: being disjoint from oneself is a meaningless declaration, and it would make the consistency check
/// report every entity as contradictory. The table has the same CHECK; both guards are kept —
/// the constraint is the last line, and the filter here is so that one dirty row does not fail a whole batch insert.
pub async fn set_disjoint_bulk(
    pool: &PgPool,
    kb_id: Uuid,
    pairs: &[(Uuid, Uuid)],
) -> AppResult<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    let a: Vec<Uuid> = pairs.iter().map(|p| p.0).collect();
    let b: Vec<Uuid> = pairs.iter().map(|p| p.1).collect();
    sqlx::query(
        "INSERT INTO entity_type_disjoint (kb_id, a_id, b_id)
         SELECT $1, x, y FROM UNNEST($2::uuid[], $3::uuid[]) AS t(x, y)
         WHERE x <> y
         ON CONFLICT DO NOTHING",
    )
    .bind(kb_id)
    .bind(&a)
    .bind(&b)
    .execute(pool)
    .await?;
    Ok(())
}

/// Sets `others` to be **all** of this class's disjoint partners (any not in it are removed).
///
/// The split with [`set_disjoint_bulk`]: that one is "only add, never remove" for imports, while this one is "this is everything" for editing.
/// Editing must be able to cancel — otherwise unticking a box in the UI would have no effect,
/// and the user would think they had changed it.
///
/// Each direction writes one row, consistent with the import side: checking "are A and B disjoint" therefore does not care which end is asked from.
pub async fn set_disjoint_for(
    pool: &PgPool,
    kb_id: Uuid,
    class: Uuid,
    others: &[Uuid],
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    // First clear all disjointness edges this class participates in — both directions must be cleared, because it may appear on either side
    sqlx::query(
        "DELETE FROM entity_type_disjoint
          WHERE kb_id = $1 AND (a_id = $2 OR b_id = $2)",
    )
    .bind(kb_id)
    .bind(class)
    .execute(&mut *tx)
    .await?;
    for other in others {
        if *other == class {
            continue;
        }
        sqlx::query(
            "INSERT INTO entity_type_disjoint (kb_id, a_id, b_id)
             VALUES ($1, $2, $3), ($1, $3, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(kb_id)
        .bind(class)
        .bind(other)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Ontology proposals (see `ontology_proposals`)
// ---------------------------------------------------------------------------

/// A persisted proposal. `payload` is the item the interface returns as-is. Those the agent proposes (0061) carry three more things:
/// the question it serves, the shape it would bind, and who proposed it
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredProposal {
    pub section: String,
    pub key: String,
    pub payload: serde_json::Value,
    /// suggest | agent
    pub proposed_by: String,
    pub serves: Vec<Uuid>,
    pub signatures: serde_json::Value,
}

/// One proposal from the agent (0061 decision 2): the shape is the same as one item of `save_proposals`, plus the question it serves and the shape it would bind
pub struct AgentProposal {
    pub section: String,
    pub key: String,
    pub payload: serde_json::Value,
    pub serves: Vec<Uuid>,
    pub signatures: serde_json::Value,
}

/// Writes a round of agent proposals. Ones a person has already ruled on are left alone (same as `save_proposals`); proposing the same key again is a refresh,
/// and the question served and the shape are refreshed along with it
pub async fn save_agent_proposals(
    pool: &PgPool,
    kb_id: Uuid,
    items: &[AgentProposal],
) -> AppResult<()> {
    for it in items {
        sqlx::query(
            "INSERT INTO ontology_proposals
                 (id, kb_id, section, key, payload, proposed_by, serves, signatures)
             VALUES ($1, $2, $3, $4, $5, 'agent', $6, $7)
             ON CONFLICT (kb_id, section, key) DO UPDATE
               SET payload = EXCLUDED.payload, proposed_by = 'agent',
                   serves = EXCLUDED.serves, signatures = EXCLUDED.signatures,
                   created_at = now()
               WHERE ontology_proposals.status = 'open'",
        )
        .bind(Uuid::now_v7())
        .bind(kb_id)
        .bind(&it.section)
        .bind(&it.key)
        .bind(&it.payload)
        .bind(&it.serves)
        .bind(&it.signatures)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Shapes the agent has already proposed (regardless of how a human ruled): the next round does not propose them again — rejected ones especially are not proposed again
pub async fn signatures_already_proposed(
    pool: &PgPool,
    kb_id: Uuid,
) -> AppResult<Vec<serde_json::Value>> {
    Ok(sqlx::query_scalar(
        "SELECT signatures FROM ontology_proposals WHERE kb_id = $1 AND proposed_by = 'agent'",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}

/// A proposal was rejected, along with the reason (left blank if none was written).
///
/// **The status is changed rather than the row deleted**: the rejection happened and is recorded, on the same route as `fact_adoptions` and `entity_retypes`.
/// The record has a use right now: the agent's next round will not refresh it back into pending review.
pub async fn reject_proposal(
    pool: &PgPool,
    kb_id: Uuid,
    section: &str,
    key: &str,
    reason: Option<&str>,
    actor: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE ontology_proposals
            SET status = 'rejected', reason = $4, decided_by = $5, decided_at = now()
          WHERE kb_id = $1 AND section = $2 AND key = $3 AND status = 'open'",
    )
    .bind(kb_id)
    .bind(section)
    .bind(key)
    .bind(reason.map(str::trim).filter(|r| !r.is_empty()))
    .bind(actor)
    .execute(pool)
    .await?;
    Ok(())
}

/// An open proposal, fetched by key (read when the agent merges this round's map_to into the previous round's)
pub async fn open_proposal(
    pool: &PgPool,
    kb_id: Uuid,
    section: &str,
    key: &str,
) -> AppResult<Option<StoredProposal>> {
    Ok(sqlx::query_as(
        "SELECT section, key, payload, proposed_by, serves, signatures FROM ontology_proposals
         WHERE kb_id = $1 AND section = $2 AND key = $3 AND status = 'open'",
    )
    .bind(kb_id)
    .bind(section)
    .bind(key)
    .fetch_optional(pool)
    .await?)
}

/// Proposals still awaiting a human's look. Newest first — the older batch has already been looked at several times.
pub async fn open_proposals(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<StoredProposal>> {
    Ok(sqlx::query_as(
        "SELECT section, key, payload, proposed_by, serves, signatures FROM ontology_proposals
         WHERE kb_id = $1 AND status = 'open'
         ORDER BY created_at DESC, key",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}

/// All ancestors of these classes (excluding themselves). Walks up `subClassOf`; multiple inheritance and diamonds are handled.
///
/// Used to backfill the floor for candidates in chunk retrieval: vector retrieval favors leaf classes whose names literally appear in the text,
/// and general base classes rank very low (measured: `person` ranks 359th out of 976 classes), so the prompt would not contain them — entities have nowhere to land, and relation signatures degrade to `*`. Ancestors are
/// the generalization relations the ontology itself declares,
/// so backfilling with them is more reliable than keeping a list of "general classes".
pub async fn ancestors_of(pool: &PgPool, ids: &[Uuid]) -> AppResult<Vec<Uuid>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // In the recursive term, UNION de-duplicates, so diamond inheritance does not expand the same ancestor twice
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "WITH RECURSIVE up(id) AS (
             SELECT unnest($1::uuid[])
             UNION
             SELECT p.parent_id FROM entity_type_parents p JOIN up ON p.child_id = up.id
         )
         SELECT id FROM up WHERE id <> ALL($1::uuid[])",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Resolves `owl:inverseOf` / `rdfs:subPropertyOf` from IRIs to ids.
///
/// **Must be a second pass.** These two point at another relation type, and the id only exists after everything is inserted —
/// a one-pass approach can only handle files where the parent property happens to come first, and RDF triples have no order.
///
/// Pairs by IRI rather than by key: a key gets a suffix on a name clash (`part_of_2`),
/// whereas the IRI is the identity within this ontology.
pub async fn link_property_axioms_bulk(
    pool: &PgPool,
    kb_id: Uuid,
    inverse: &[(String, String)],
    sub_property: &[(String, String)],
) -> AppResult<(u64, u64)> {
    let run = |column: &'static str, pairs: &[(String, String)]| {
        let src: Vec<String> = pairs.iter().map(|(s, _)| s.clone()).collect();
        let dst: Vec<String> = pairs.iter().map(|(_, d)| d.clone()).collect();
        async move {
            if src.is_empty() {
                return AppResult::Ok(0);
            }
            // If the target IRI cannot be found in this KB, skip this one — **partial import is normal**
            // (it references attributes in an external vocabulary), and one unresolved link should not fail the whole import
            let sql = format!(
                "UPDATE relation_types r SET {column} = t.id
                   FROM UNNEST($2::text[], $3::text[]) AS p(src, dst)
                   JOIN relation_types t ON t.kb_id = $1 AND t.iri = p.dst
                  WHERE r.kb_id = $1 AND r.iri = p.src AND r.id <> t.id"
            );
            let n = sqlx::query(&sql)
                .bind(kb_id)
                .bind(&src)
                .bind(&dst)
                .execute(pool)
                .await?
                .rows_affected();
            AppResult::Ok(n)
        }
    };
    let inv = run("inverse_of", inverse).await?;
    let sub = run("sub_property_of", sub_property).await?;
    Ok((inv, sub))
}

#[cfg(test)]
mod display_hint_tests {
    use super::validate_display_hints;
    use serde_json::json;

    #[test]
    fn a_flat_object_of_strings_is_accepted_and_a_nested_value_is_refused() {
        assert!(validate_display_hints(&json!({})).is_ok());
        assert!(validate_display_hints(&json!({"panel": "card", "role": "status"})).is_ok());
        assert!(validate_display_hints(&json!({"tones": {"passed": "ok"}})).is_err());
        assert!(validate_display_hints(&json!({"panel": 1})).is_err());
        assert!(validate_display_hints(&json!(["card"])).is_err());
    }
}

#[cfg(test)]
mod name_shape_tests {
    use super::lower_camel;

    #[test]
    fn separators_become_camel_humps() {
        assert_eq!(lower_camel("access to"), "accessTo");
        assert_eq!(lower_camel("collaborated with"), "collaboratedWith");
        assert_eq!(lower_camel("works_for"), "worksFor");
        assert_eq!(lower_camel("date-applicability"), "dateApplicability");
        // Consecutive separators count as one
        assert_eq!(lower_camel("called   for"), "calledFor");
        assert_eq!(lower_camel("  start date  "), "startDate");
    }

    #[test]
    fn only_the_first_letter_is_lowered() {
        assert_eq!(
            lower_camel("ApplicableCertificate"),
            "applicableCertificate"
        );
        assert_eq!(lower_camel("EffectiveEndDateTime"), "effectiveEndDateTime");
    }

    /// **This is one of the reasons this function exists**: it cannot be reversed from `key`,
    /// since rebuilding `product_id` only gives `productId`
    #[test]
    fn acronyms_are_left_alone() {
        assert_eq!(lower_camel("productID"), "productID");
        assert_eq!(lower_camel("hasLEI"), "hasLEI");
        assert_eq!(lower_camel("accessibilityAPI"), "accessibilityAPI");
        assert_eq!(
            lower_camel("checkoutPageURLTemplate"),
            "checkoutPageURLTemplate"
        );
    }

    #[test]
    fn already_right_is_untouched() {
        for s in ["owns", "acceptedAnswer", "worksFor", "3DModel"] {
            assert_eq!(lower_camel(s), s, "{s} 不该被动");
        }
    }

    #[test]
    fn idempotent() {
        for s in ["access to", "ApplicableCertificate", "productID", "owns"] {
            let once = lower_camel(s);
            assert_eq!(lower_camel(&once), once, "{s} 归一两次结果要一样");
        }
    }
}

/// The second of two numbers (0061 decision 5): among the agent's proposals, the ones a person has ruled on — how many there are, and of those, how many were rejected and how many were adopted after revision,
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct AgentProposalReport {
    pub open: i64,
    pub adopted: i64,
    pub adopted_edited: i64,
    pub rejected: i64,
}

/// What the document says that the ontology still cannot hold: category words not bound to a class, and phrase shapes not bound to an attribute (those judged none
/// and those where the two votes disagreed). These are what the ontology agent reads (0061 decision 2); the workbench uses these two numbers to say "what is still missing",
/// no longer 0003's `ontology_misses` — open-graph extraction does not write to that table
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct Uncovered {
    pub kind_words: i64,
    pub phrases: i64,
}

pub async fn uncovered(pool: &PgPool, kb_id: Uuid) -> AppResult<Uncovered> {
    Ok(sqlx::query_as(
        "SELECT (SELECT count(*) FROM type_bindings
                  WHERE kb_id = $1 AND status <> 'bound') AS kind_words,
                (SELECT count(*) FROM phrase_bindings
                  WHERE kb_id = $1 AND status <> 'bound') AS phrases",
    )
    .bind(kb_id)
    .fetch_one(pool)
    .await?)
}

pub async fn agent_proposal_report(pool: &PgPool, kb_id: Uuid) -> AppResult<AgentProposalReport> {
    Ok(sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status = 'open') AS open,
                count(*) FILTER (WHERE status = 'adopted') AS adopted,
                count(*) FILTER (WHERE status = 'adopted' AND edited) AS adopted_edited,
                count(*) FILTER (WHERE status = 'rejected') AS rejected
           FROM ontology_proposals WHERE kb_id = $1 AND proposed_by = 'agent'",
    )
    .bind(kb_id)
    .fetch_one(pool)
    .await?)
}
