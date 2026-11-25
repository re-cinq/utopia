//! Fetching and storing for consistency checks. The judgment itself lives in `utopia-reason`; that layer does not touch the database.
//!
//! Three steps: fetch live facts as edges, fetch the axiom columns of `relation_types` as `Axioms`, and
//! write the violations that `check()` emits into `axiom_violations`.
//!
//! **Re-running is idempotent, and "recomputed" is the source of truth.** After each run, any `open`
//! row in this database that was not recomputed is deleted: they are derived state; if a fact is retracted or an axiom relaxed, that violation should not
//! still hang on the Review page. A row someone has already ruled on (`resolved`) is left alone: that is a human decision,
//! not something computed.
//!
//! The reverse of a rule that was learned the hard way (see `ontology_proposals`): there, re-running would reset rejected
//! proposals back to pending, wiping out a person's rejection every run. So here `ON CONFLICT`
//! does nothing: a row already in the database, open or resolved, stays as it is.

use serde_json::json;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use std::collections::{HashMap, HashSet};
use utopia_core::models::{AxiomViolation, DerivedFactView, OntologyDefect};
/// Literal rule kinds. &'static str rather than an enum: it goes straight into SQL and is also used directly as a key
type RuleKind = &'static str;
use utopia_core::AppResult;
use utopia_reason::derive::{Contradictions, Derivation, TimedEdge};
use utopia_reason::{check_all, Axioms, Edge, Kind, Violation};
use uuid::Uuid;

/// Output of one check, for the caller to write an audit and tell the user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    /// Number of edges taking part in the check
    pub edges: usize,
    /// Number of predicates that declare at least one axiom. **When zero, the conclusion is "no criterion" rather than "no contradiction"**
    pub predicates_with_axioms: usize,
    /// Total violations computed this time
    pub found: usize,
    /// Of those, the new ones (not previously in the database)
    pub inserted: usize,
    /// Stale open rows cleared
    pub cleared: usize,
    /// Number of derivations that collide with an assertion (0017), already included in `found`
    pub contradictions: usize,
    /// Number of contradictions that hit the per-predicate cap and did not enter the queue. **Non-zero means the root is in the rules**:
    /// when hundreds of derivations on one predicate all collide, looking at them one by one is pointless
    pub contradictions_capped: usize,
    /// Number of mutually colliding rule pairs: goes to `ontology_defects`, not this table
    pub rules_disagree: usize,
    /// Resolved rows reopened: a person had said it was retracted or closed, or that the ontology would be changed, yet the violation was computed again:
    /// the promise was not kept, and the queue does not stay silent on the person's behalf (#202)
    pub reopened: usize,
    /// Number of predicates whose cycle search did not finish (#642). When non-zero, cycles on those predicates are only partly reported, and last round's
    /// cycles are not cleared this round: an unfinished search cannot be taken to mean there are none
    pub cycles_capped: usize,
}

/// Cap on contradictions per predicate that enter the queue (0017 §1). Excess is only counted.
const MAX_CLASHES_PER_PREDICATE: usize = 50;

/// Fetch the predicate axioms for this database.
///
/// **Only fetch those that declare at least one flag**: a predicate with no flags declared would be put in the table but never checked (`says_nothing`
/// skips it), wasting memory. And this count itself is meaningful: it is the measure of "is there any criterion at all",
/// and the report uses it.
#[allow(clippy::type_complexity)]
async fn axioms(conn: &mut PgConnection, kb_id: Uuid) -> AppResult<HashMap<Uuid, Axioms>> {
    let rows: Vec<(
        Uuid,
        bool,
        bool,
        bool,
        bool,
        bool,
        bool,
        Option<Uuid>,
        Option<Uuid>,
    )> = sqlx::query_as(
        "SELECT id, is_transitive, is_symmetric, is_asymmetric, is_irreflexive,
                    functional, inverse_functional, inverse_of, sub_property_of
               FROM relation_types
              WHERE kb_id = $1
                AND (is_transitive OR is_symmetric OR is_asymmetric OR is_irreflexive
                     OR functional OR inverse_functional
                     OR inverse_of IS NOT NULL OR sub_property_of IS NOT NULL)",
    )
    .bind(kb_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut map: HashMap<Uuid, Axioms> = rows
        .into_iter()
        .map(
            |(
                id,
                transitive,
                symmetric,
                asymmetric,
                irreflexive,
                functional,
                inverse_functional,
                inverse_of,
                sub_property_of,
            )| {
                (
                    id,
                    Axioms {
                        transitive,
                        symmetric,
                        asymmetric,
                        irreflexive,
                        functional,
                        inverse_functional,
                        inverse_of,
                        sub_property_of,
                    },
                )
            },
        )
        .collect();

    // **Inverses are mutual, but the database stores only one direction.** If `p⁻¹ = q` is declared without filling in
    // `q⁻¹ = p`, then `A p B` derives `B q A`, but `B q A` cannot derive back to `A p B`:
    // "asking about work gives a different answer from asking about employment" is exactly what R1 is meant to eliminate, and fixing only half is no fix.
    //
    // Normalization lives here rather than in a database trigger: there is more than one way around triggers (RDF import,
    // direct table edits), whereas axiom loading happens in only one place, which nothing can bypass.
    let pairs: Vec<(Uuid, Uuid)> = map
        .iter()
        .filter_map(|(id, ax)| ax.inverse_of.map(|inv| (inv, *id)))
        .collect();
    for (target, source) in pairs {
        // If it has already declared its own inverse, leave it alone: **what a person wrote takes precedence over what was derived**,
        // and if the two sides point at different things, that is the ontology's own contradiction, left for R0 to report rather than silently changed here.
        map.entry(target)
            .or_default()
            .inverse_of
            .get_or_insert(source);
    }
    Ok(map)
}

/// Live facts whose subject is not in the predicate's declared domain, or whose object is not in its range (#190 / #196).
///
/// The signature check is at the **ledger layer**: alignment only offers structurally matching candidates (0053), but merging swaps the subject, and the ontology
/// can change domain after the fact, so a gate at write time cannot stop changes made after writing. So this scans the whole database, and any path that
/// wrote something backwards shows up on Review.
///
/// **Entities without a type are not counted**: they have no type to compare, and "unknown" is not "does not conform": per 0009,
/// unclassified is an honest state and should not be reported as a contradiction. Only predicates with declared domain / range are checked,
/// under the same discipline as the other four classes: no axiom, no criterion.
///
/// If `only` is given, only those facts are checked (right after a merge, the few that moved); None means all.
pub async fn signature_breaks(
    conn: &mut PgConnection,
    kb_id: Uuid,
    only: Option<&[Uuid]>,
) -> AppResult<Vec<Uuid>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "WITH RECURSIVE anc(type_id, anc_id) AS (
             SELECT id, id FROM entity_types WHERE kb_id = $1
             UNION
             SELECT a.type_id, p.parent_id
               FROM anc a JOIN entity_type_parents p ON p.child_id = a.anc_id
         )
         SELECT f.id
           FROM facts f
           JOIN relation_types r ON r.id = f.predicate_id
           JOIN entities s ON s.id = f.subject_id
           JOIN entities o ON o.id = f.object_id
          WHERE f.kb_id = $1 AND f.invalidated_at IS NULL
            AND ($2::uuid[] IS NULL OR f.id = ANY($2))
            AND (
              (s.type_id IS NOT NULL
               AND EXISTS (SELECT 1 FROM relation_type_domains d WHERE d.relation_type_id = r.id)
               AND NOT EXISTS (SELECT 1 FROM relation_type_domains d
                                 JOIN anc a ON a.anc_id = d.entity_type_id
                                WHERE d.relation_type_id = r.id AND a.type_id = s.type_id))
              OR
              (o.type_id IS NOT NULL
               AND EXISTS (SELECT 1 FROM relation_type_ranges g WHERE g.relation_type_id = r.id)
               AND NOT EXISTS (SELECT 1 FROM relation_type_ranges g
                                 JOIN anc a ON a.anc_id = g.entity_type_id
                                WHERE g.relation_type_id = r.id AND a.type_id = o.type_id))
            )
          ORDER BY f.recorded_at",
    )
    .bind(kb_id)
    .bind(only)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Writes signature violations into `axiom_violations` (kind = `signature`, left and right are the same fact).
/// Idempotent: the same fact reported twice is not stored twice. Returns the number of newly inserted rows.
pub async fn record_signature_breaks(
    pool: &PgPool,
    kb_id: Uuid,
    facts: &[Uuid],
) -> AppResult<usize> {
    let mut inserted = 0usize;
    for fact in facts {
        let id: Option<(Uuid,)> = sqlx::query_as(
            "INSERT INTO axiom_violations (id, kb_id, kind, left_fact, right_fact)
             VALUES ($1, $2, 'signature', $3, $3)
             ON CONFLICT (kb_id, kind, left_fact, right_fact) WHERE kind <> 'cycle' DO NOTHING
             RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(kb_id)
        .bind(fact)
        .fetch_optional(pool)
        .await?;
        if id.is_some() {
            inserted += 1;
        }
    }
    Ok(inserted)
}

/// All read-side outputs of one detection: assertion edges and axioms fetched in, rule candidates solved to a fixed point, five classes of checks
/// computed, and the sixth class (derivation colliding with an assertion) carrying details for humans to read. **Read-only, no writes**: `run` and the ontology
/// reconciliation in one transaction share the same fetching and filtering (0062).
struct Detection {
    /// The full set of violations computed this round (not yet minus the coexistence groups a person has accepted)
    violations: Vec<Violation>,
    /// The detail of `derived_contradiction` is keyed by (left, right): it points at rows that do not exist,
    /// and is only attached to the matching violation when stored
    details: HashMap<(Uuid, Uuid), serde_json::Value>,
    /// Predicates whose cycle search did not finish (#642): their open cycle rows are not judged this round
    cycles_capped: Vec<Uuid>,
    /// Mutually exclusive groups a person has accepted as coexisting: (kind, the set of facts in the group)
    accepted: Vec<(String, HashSet<Uuid>)>,
    derivation: Derivation,
    clashes: Contradictions,
    names: Names,
    edges: usize,
    predicates_with_axioms: usize,
    contradictions_capped: usize,
}

async fn detect(conn: &mut PgConnection, kb_id: Uuid) -> AppResult<Detection> {
    // Same solve as `materialize`: the candidates include relation edges derived by rules, and blocked ones too;
    // the queue reports exactly what was blocked there (0017, 0047 decision 3).
    let Resolved {
        edges: timed,
        spans,
        axioms,
        asserted_ids,
        checked,
        ..
    } = resolve(conn, kb_id).await?;
    let Checked {
        candidates: derivation,
        candidate_rule,
        clashes,
        ..
    } = checked;
    // Query with intervals: the three mutually exclusive classes count only when they hold together (#634). Previously the intervals were stripped before checking,
    // so every raise and every change of owner entered Review.
    let checked = check_all(&timed, &axioms);
    let cycles_capped = checked.cycles_capped;
    let mut violations = checked.violations;
    // The fifth class is not in the pure logic engine: it needs entity types and predicate domain / range, which are database things.
    // Once computed it follows the same storage and clear-stale rules as the other four classes.
    for fact in signature_breaks(conn, kb_id, None).await? {
        violations.push(Violation {
            kind: Kind::Signature,
            left: fact,
            right: fact,
            path: Vec::new(),
        });
    }

    // The sixth class (0017): a derivation that was inferred but cannot land. It is computed by the same function as `materialize`,
    // so what is reported here is exactly what was blocked there; if each computed its own, the queue would drift from the graph.
    let names = names_for(conn, &derivation, &clashes).await?;
    let mut details: HashMap<(Uuid, Uuid), serde_json::Value> = HashMap::new();
    let mut per_pred: HashMap<Uuid, usize> = HashMap::new();
    let mut contradictions_capped = 0usize;
    for c in &clashes.with_assertions {
        let d = &derivation.facts[c.derived];
        // Keys and foreign keys both need rows in `facts`. When a chain goes through a relation edge derived by a rule, the last premise is
        // its temporary id (different each round), so it falls back to the last assertion on the chain; a self-loop's `against` is that too
        let Some(&last) = d.premises.iter().rev().find(|p| asserted_ids.contains(p)) else {
            // When the entire premise chain is temporary ids derived by rules, there is no storable key or foreign key (queue rows
            // must trace back to `facts`), so this collision does not store a row: the blocked side of `check` still blocks it.
            continue;
        };
        let against = if asserted_ids.contains(&c.against) {
            c.against
        } else {
            last
        };
        let key = (against, last);
        if details.contains_key(&key) {
            continue;
        }
        let n = per_pred.entry(d.predicate).or_default();
        if *n >= MAX_CLASHES_PER_PREDICATE {
            contradictions_capped += 1;
            continue;
        }
        *n += 1;
        let span = utopia_reason::derive::validity(&d.premises, &spans);
        details.insert(
            key,
            json!({
                "axiom": c.axiom.as_str(),
                "rule": d.rule.as_str(),
                // The relation edge derived by a rule: which business rule it is (0047). Axiom derivations have none
                "attribute_rule_id": candidate_rule.get(&c.derived),
                "via": d.via,
                "via_label": names.predicate(d.via),
                "subject_id": d.subject,
                "subject": names.entity(d.subject),
                "predicate_id": d.predicate,
                "predicate": names.predicate(d.predicate),
                "object_id": d.object,
                "object": names.entity(d.object),
                "valid_from": span.and_then(|s| s.0).map(|t| stamp(t).to_rfc3339()),
                "valid_to": span.and_then(|s| s.1).map(|t| stamp(t).to_rfc3339()),
                "premises": d.premises,
            }),
        );
        violations.push(Violation {
            kind: Kind::DerivedContradiction,
            left: against,
            right: last,
            // The evidence chain keeps only assertion ids: when a chain goes through a relation edge derived by a rule, premises contain that round's
            // temporary ids (0047 decision 3), which belong to no table and never resolve after storage. The stored
            // path is the evidence chain shown to people and named in exports, so its members must trace back to rows in `facts`
            path: d
                .premises
                .iter()
                .copied()
                .filter(|p| asserted_ids.contains(p))
                .collect(),
        });
    }

    let accepted = accepted_groups(conn, kb_id).await?;
    Ok(Detection {
        edges: timed.len(),
        predicates_with_axioms: axioms.len(),
        contradictions_capped,
        cycles_capped,
        accepted,
        derivation,
        clashes,
        names,
        violations,
        details,
    })
}

/// A person accepted "these can coexist", and this round's group is entirely within those facts, e.g. one of them was later
/// retracted, so the group's ends and keys have changed, yet it is still governed by that acceptance and is not surfaced again.
/// If the group has a fact that was not present when accepted, it is not blocked: that is a new situation
fn suppressed(v: &Violation, accepted: &[(String, HashSet<Uuid>)]) -> bool {
    is_grouped(v.kind)
        && accepted
            .iter()
            .any(|(k, facts)| k == v.kind.as_str() && v.path.iter().all(|f| facts.contains(f)))
}

/// The storage side of one detection: violations upserted by row key (evidence refreshed to this round's), resolved rows that break their promise
/// reopen; **open rows not computed are not handled at this layer**: `run` deletes them, and the reconciliation in the ontology transaction
/// classifies them (0062). Returns the row ids that "still hold this round" and the (newly inserted, reopened) counts.
async fn settle_violations(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    det: &Detection,
) -> AppResult<(Vec<Uuid>, usize, usize)> {
    let mut fresh: Vec<Uuid> = Vec::with_capacity(det.violations.len());
    let mut inserted = 0usize;
    let mut reopened = 0usize;
    for v in &det.violations {
        let Violation {
            kind,
            left,
            right,
            path,
        } = v;
        if suppressed(v, &det.accepted) {
            continue;
        }
        let detail = det
            .details
            .get(&(*left, *right))
            .cloned()
            .unwrap_or_else(|| json!({}));
        // **Evidence goes with this round** (#619). Previously this was `DO NOTHING`, and no other place
        // wrote `path` / `detail`, and the reopen branch did not write them either. So a row, as long as it is not deleted, kept the
        // evidence it carried from the **first** time this key was recorded: the same derived contradiction later recomputed through another group of
        // premises kept the earliest group on the row. On reopen it was uglier: `detected_at`
        // became now() while path did not move, so one row ended up with a new timestamp paired with another round's evidence.
        //
        // **Cycles are not keyed by their first and last edges** (#641). Two cycles sharing the smallest edge and the closing edge, differing in the middle,
        // used to land on the same key, so the later one overwrote the earlier one's path: that was not "the same cycle with new evidence",
        // another cycle was swallowed. A cycle is its set of facts, keyed by its whole path (0054).
        //
        // Insert and update are merged into one statement: previously it was an insert then a SELECT, two statements asking about the same row.
        // `xmax = 0` holds only for rows truly newly inserted: `DO UPDATE` would also make RETURNING emit a row for updates, so without this split,
        // rows that were already there would be counted as newly inserted.
        //
        // Whether newly inserted or already there, both count as "still holds this round".
        // A row that was already there and is resolved must be looked at again: `fact_retracted` / `fact_closed` /
        // `axiom_relaxed` / `criterion_changed` are all promises that "the world will change": the fact is gone,
        // the interval closed, the criterion changed, so the violation should no longer be computed. If it is computed again, the promise was not kept,
        // and that row goes back to open for a person to look at again.
        // `accepted` is an intentional coexistence: no matter how many times it is recomputed, it stays silent (#202), so long as it is still those same facts as accepted.
        // A mutually exclusive group reaching here means the checks above did not stop it: one more under the same key, and that row also goes back to open
        // The two partial unique indexes each govern one class, and the conflict target has to spell out the index's WHERE verbatim for inference to work.
        let upsert = if *kind == Kind::Cycle {
            "INSERT INTO axiom_violations
                 (id, kb_id, kind, left_fact, right_fact, path, detail)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (kb_id, path) WHERE kind = 'cycle' DO UPDATE
                 SET path = EXCLUDED.path, detail = EXCLUDED.detail
             RETURNING id, status, resolution, xmax = 0"
        } else {
            "INSERT INTO axiom_violations
                 (id, kb_id, kind, left_fact, right_fact, path, detail)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (kb_id, kind, left_fact, right_fact) WHERE kind <> 'cycle' DO UPDATE
                 SET path = EXCLUDED.path, detail = EXCLUDED.detail
             RETURNING id, status, resolution, xmax = 0"
        };
        let (keep, status, resolution, is_new): (Uuid, String, Option<String>, bool) =
            sqlx::query_as(upsert)
                .bind(Uuid::now_v7())
                .bind(kb_id)
                .bind(kind.as_str())
                .bind(left)
                .bind(right)
                .bind(path)
                .bind(&detail)
                .fetch_one(&mut **tx)
                .await?;
        if is_new {
            inserted += 1;
        }
        let grouped = is_grouped(*kind);
        let broken = match resolution.as_deref() {
            Some("fact_retracted" | "fact_closed" | "axiom_relaxed" | "criterion_changed") => true,
            Some("accepted") => grouped,
            _ => false,
        };
        if status == "resolved" && broken {
            sqlx::query(
                "UPDATE axiom_violations
                    SET status = 'open', resolution = NULL, decided_by = NULL,
                        decided_at = NULL, detected_at = now()
                  WHERE id = $1",
            )
            .bind(keep)
            .execute(&mut **tx)
            .await?;
            reopened += 1;
        }
        fresh.push(keep);
    }

    // Predicates whose cycle search did not finish: last round's open cycles are not cleared this round (#642). Not found is not the same as not existing.
    // When the cap is hit, the batch reported is the one ordered by the data; if the data is unchanged it is the same batch; if the data changed, those old rows
    // are better kept for a person to look at than dropped because this round's search did not find them.
    if !det.cycles_capped.is_empty() {
        let kept: Vec<Uuid> = sqlx::query_scalar(
            "SELECT v.id FROM axiom_violations v
               JOIN facts f ON f.id = v.left_fact
              WHERE v.kb_id = $1 AND v.kind = 'cycle' AND v.status = 'open'
                AND f.predicate_id = ANY($2)",
        )
        .bind(kb_id)
        .bind(&det.cycles_capped)
        .fetch_all(&mut **tx)
        .await?;
        fresh.extend(kept);
    }
    Ok((fresh, inserted, reopened))
}

/// Contradictions between derived rules, colliding pairwise, go into `ontology_defects`: the root is those two declarations, not any particular fact.
/// A pair of predicates can collide in more than one way (functional and asymmetric each collide separately), but the unique key only goes to
/// the predicate pair, so it is merged into one row, and every collision mode is written into detail.
async fn settle_defects(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    det: &Detection,
) -> AppResult<()> {
    let mut by_pair: HashMap<(Uuid, Uuid), Vec<serde_json::Value>> = HashMap::new();
    let mut order: Vec<(Uuid, Uuid)> = Vec::new();
    for rc in &det.clashes.between_derivations {
        let triple = |i: usize| {
            let d = &det.derivation.facts[i];
            format!(
                "{} · {} · {}",
                det.names.entity(d.subject),
                det.names.predicate(d.predicate),
                det.names.entity(d.object)
            )
        };
        let examples: Vec<serde_json::Value> = rc
            .pairs
            .iter()
            .take(3)
            .map(|(i, j)| json!([triple(*i), triple(*j)]))
            .collect();
        let key = (rc.a.0, rc.b.0);
        if !by_pair.contains_key(&key) {
            order.push(key);
        }
        by_pair.entry(key).or_default().push(json!({
            "rule_a": rc.a.1.as_str(),
            "via_a": det.names.predicate(rc.a.0),
            "rule_b": rc.b.1.as_str(),
            "via_b": det.names.predicate(rc.b.0),
            "axiom": rc.axiom.as_str(),
            "count": rc.pairs.len(),
            "examples": examples,
        }));
    }
    let mut fresh_defects: Vec<Uuid> = Vec::with_capacity(order.len());
    for key in order {
        let rules = by_pair.remove(&key).unwrap_or_default();
        let count: usize = rules
            .iter()
            .map(|r| r["count"].as_u64().unwrap_or(0) as usize)
            .sum();
        // A row someone has already accepted stays resolved, only detail is refreshed: per 0017, once accepted it is no longer reported
        let (id,): (Uuid,) = sqlx::query_as(
            "INSERT INTO ontology_defects (id, kb_id, kind, subject, other, path, detail)
             VALUES ($1, $2, 'rules_disagree', $3, $4, '{}', $5)
             ON CONFLICT (kb_id, kind, subject, other) DO UPDATE SET detail = EXCLUDED.detail
             RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(kb_id)
        .bind(key.0)
        .bind(key.1)
        .bind(json!({ "count": count, "rules": rules }))
        .fetch_one(&mut **tx)
        .await?;
        fresh_defects.push(id);
    }
    sqlx::query(
        "DELETE FROM ontology_defects
          WHERE kb_id = $1 AND kind = 'rules_disagree' AND status = 'open'
            AND NOT (id = ANY($2))",
    )
    .bind(kb_id)
    .bind(&fresh_defects)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Run one check, writing the results to the database.
pub async fn run(pool: &PgPool, kb_id: Uuid) -> AppResult<Report> {
    // Detection and storage go into the same transaction: the read side sees the latest commit before each statement since the transaction started,
    // equivalent to taking one connection per read from the pool as before; the write side then has no window between "insert new" and "clear stale".
    let mut tx = pool.begin().await?;
    let det = detect(&mut tx, kb_id).await?;
    let mut report = Report {
        edges: det.edges,
        predicates_with_axioms: det.predicates_with_axioms,
        found: det.violations.len(),
        contradictions: det.details.len(),
        contradictions_capped: det.contradictions_capped,
        rules_disagree: det.clashes.between_derivations.len(),
        cycles_capped: det.cycles_capped.len(),
        ..Default::default()
    };

    let (fresh, inserted, reopened) = settle_violations(&mut tx, kb_id, &det).await?;
    report.inserted = inserted;
    report.reopened = reopened;

    // Open rows not computed this round are stale: a fact was retracted, or an axiom was relaxed.
    // Resolved rows are left alone: that is a human decision, not derived state.
    let cleared = sqlx::query(
        "DELETE FROM axiom_violations
          WHERE kb_id = $1 AND status = 'open' AND NOT (id = ANY($2))",
    )
    .bind(kb_id)
    .bind(&fresh)
    .execute(&mut *tx)
    .await?;
    report.cleared = cleared.rows_affected() as usize;

    settle_defects(&mut tx, kb_id, &det).await?;
    tx.commit().await?;
    Ok(report)
}

/// The identity of a violation in the database: same shape as the two unique indexes: cycles by their whole `path`, the rest by
/// `(kind, left_fact, right_fact)` (0054).
///
/// **Bucketing normalization**: for non-cycle kinds, `path` stores this round's evidence (premise sets / group members),
/// which can change on recomputation while the row is still the same violation; bucketing by it would mistake a row that
/// was truly detected under the old criterion for "long since stale", downgrading it from `criterion_changed` to a hard delete. So in the reconciliation key
/// the path of non-cycle rows is always normalized to empty.
pub type ViolationKey = (String, Uuid, Uuid, Vec<Uuid>);

fn partition_key(kind: &str, left: Uuid, right: Uuid, path: &[Uuid]) -> ViolationKey {
    (
        kind.to_string(),
        left,
        right,
        if kind == "cycle" {
            path.to_vec()
        } else {
            Vec::new()
        },
    )
}

/// The set of violation keys **that would be computed** under the current ontology (0062). Does not change violation state; but `detect` along
/// `resolve` also fills in rule rows compiled deterministically as a side effect (`INSERT ... ON CONFLICT DO
/// NOTHING`), so it is not literally read-only. The transaction that changes a criterion calls it before touching anything,
/// and what it gets is the baseline under the old ontology.
pub async fn detection_keys(
    conn: &mut PgConnection,
    kb_id: Uuid,
) -> AppResult<HashSet<ViolationKey>> {
    let det = detect(conn, kb_id).await?;
    Ok(det
        .violations
        .iter()
        .filter(|v| !suppressed(v, &det.accepted))
        .map(|v| partition_key(v.kind.as_str(), v.left, v.right, &v.path))
        .collect())
}

/// The criterion was just changed within the same transaction (0062): re-detect under the new ontology, store,
/// then give stale open rows an honest outcome:
///
/// - Those present in the old baseline: their criterion was cut off by this change, and they end as `criterion_changed`.
///   Not a human's ruling (`decided_by` is empty), but the axiom/signature changed; the next time it is computed, reopening follows
///   the same branch as `axiom_relaxed`
/// - Those not in the old baseline: they were already stale before the change (fact gone, and so on); delete them as before, under
///   the same stale-clearing rule as `run`
///
/// Invariant kept: a row with `status='open'` must have been reconciled against the **current** ontology.
/// Returns the number of violations detected this round (same measure as `report.found` in `run`)
pub async fn reconcile_ontology(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    was_detected: &HashSet<ViolationKey>,
) -> AppResult<usize> {
    let det = detect(tx, kb_id).await?;
    let (fresh, _, _) = settle_violations(tx, kb_id, &det).await?;
    let stale: Vec<(Uuid, String, Uuid, Uuid, Vec<Uuid>)> = sqlx::query_as(
        "SELECT id, kind, left_fact, right_fact, path FROM axiom_violations
          WHERE kb_id = $1 AND status = 'open' AND NOT (id = ANY($2))",
    )
    .bind(kb_id)
    .bind(&fresh)
    .fetch_all(&mut **tx)
    .await?;
    let mut disqualified: Vec<Uuid> = Vec::new();
    let mut evaporated: Vec<Uuid> = Vec::new();
    for (id, kind, left, right, path) in stale {
        if was_detected.contains(&partition_key(&kind, left, right, &path)) {
            disqualified.push(id);
        } else {
            evaporated.push(id);
        }
    }
    if !disqualified.is_empty() {
        sqlx::query(
            "UPDATE axiom_violations
                SET status = 'resolved', resolution = 'criterion_changed',
                    decided_by = NULL, decided_at = now()
              WHERE id = ANY($1)",
        )
        .bind(&disqualified)
        .execute(&mut **tx)
        .await?;
    }
    if !evaporated.is_empty() {
        sqlx::query("DELETE FROM axiom_violations WHERE id = ANY($1)")
            .bind(&evaporated)
            .execute(&mut **tx)
            .await?;
    }
    settle_defects(tx, kb_id, &det).await?;
    Ok(det.violations.len())
}

/// Mutually exclusive, three classes: a violation is a group of facts that hold together and conflict with each other (`path` is the whole group).
fn is_grouped(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Asymmetry | Kind::Functional | Kind::InverseFunctional
    )
}

/// Mutually exclusive groups a person accepted as coexisting: (kind, facts in the group).
///
/// Acceptance says "these can hold at the same time", so it compares by fact set, not by key: if one fact is retracted from the group,
/// the remaining first and last have changed and so have the keys, yet that acceptance still governs them (#624).
async fn accepted_groups(
    conn: &mut PgConnection,
    kb_id: Uuid,
) -> AppResult<Vec<(String, HashSet<Uuid>)>> {
    let rows: Vec<(String, Uuid, Uuid, Vec<Uuid>)> = sqlx::query_as(
        "SELECT kind, left_fact, right_fact, path FROM axiom_violations
          WHERE kb_id = $1 AND status = 'resolved' AND resolution = 'accepted'
            AND kind IN ('asymmetry', 'functional', 'inverse_functional')",
    )
    .bind(kb_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(kind, left, right, path)| {
            let mut facts: HashSet<Uuid> = path.into_iter().collect();
            // The first and last are already in `path`; pairs recorded before 0053 have an empty path, and these two columns
            facts.insert(left);
            facts.insert(right);
            (kind, facts)
        })
        .collect())
}

/// A contradiction must be written as text a person can read; derivations are not stored and have no text to look up, so names are filled in here.
struct Names {
    entities: HashMap<Uuid, String>,
    predicates: HashMap<Uuid, String>,
}

impl Names {
    fn entity(&self, id: Uuid) -> String {
        self.entities
            .get(&id)
            .cloned()
            .unwrap_or_else(|| "?".into())
    }
    fn predicate(&self, id: Uuid) -> String {
        self.predicates
            .get(&id)
            .cloned()
            .unwrap_or_else(|| "?".into())
    }
}

async fn names_for(
    conn: &mut PgConnection,
    derivation: &Derivation,
    clashes: &Contradictions,
) -> AppResult<Names> {
    let mut ents: HashSet<Uuid> = HashSet::new();
    let mut preds: HashSet<Uuid> = HashSet::new();
    let mut want = |i: usize| {
        let d = &derivation.facts[i];
        ents.insert(d.subject);
        ents.insert(d.object);
        preds.insert(d.predicate);
        preds.insert(d.via);
    };
    for c in &clashes.with_assertions {
        want(c.derived);
    }
    for rc in &clashes.between_derivations {
        for (i, j) in rc.pairs.iter().take(3) {
            want(*i);
            want(*j);
        }
    }
    for rc in &clashes.between_derivations {
        preds.insert(rc.a.0);
        preds.insert(rc.b.0);
    }
    let ents: Vec<Uuid> = ents.into_iter().collect();
    let preds: Vec<Uuid> = preds.into_iter().collect();
    let entities: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, canonical_name FROM entities WHERE id = ANY($1)")
            .bind(&ents)
            .fetch_all(&mut *conn)
            .await?;
    let predicates: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, label FROM relation_types WHERE id = ANY($1)")
            .bind(&preds)
            .fetch_all(&mut *conn)
            .await?;
    Ok(Names {
        entities: entities.into_iter().collect(),
        predicates: predicates.into_iter().collect(),
    })
}

/// What the Review page wants: unruled violations, together with the triple text of both facts.
///
/// Expanding to text is done in SQL rather than fetched again afterward: a page of dozens of rows, each with two triples,
/// fetched separately would mean a hundred-plus round trips. Predicates fall back to `fact_surface_predicate`: a fact with no
/// corresponding relation in the ontology is shown with its original wording (see `facts.predicate_id`).
pub async fn open_violations(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<AxiomViolation>> {
    // Let the path look up triples by fact ID, avoiding a scan of every materialized triple in the database for each path.
    Ok(sqlx::query_as(&format!(
        "WITH triple AS NOT MATERIALIZED (
             SELECT f.id,
                    s.canonical_name || ' · '
                      || COALESCE(r.label, fact_surface_predicate(f.id), '?') || ' · '
                      || COALESCE(o.canonical_name, f.object_value ->> 'summary',
                                  f.object_value #>> '{{}}', '?') AS text,
                    r.label AS predicate
               FROM facts f
               JOIN entities s ON s.id = f.subject_id
               LEFT JOIN relation_types r ON r.id = f.predicate_id
               LEFT JOIN entities o ON o.id = f.object_id
              WHERE f.kb_id = $1
         )
         SELECT v.id, v.kind, l.predicate,
                v.left_fact, l.text AS left_text,
                v.right_fact, rt.text AS right_text,
                coalesce(array_length(v.path, 1), 0) AS path_len,
                v.detected_at, v.detail,
                COALESCE((SELECT jsonb_agg(jsonb_build_object('id', x.id, 'text', pt.text)
                                           ORDER BY x.ord)
                            FROM unnest(v.path) WITH ORDINALITY AS x(id, ord)
                            JOIN triple pt ON pt.id = x.id), '[]'::jsonb) AS path,
                {left_holds_to} IS NULL AS left_open,
                lf.confidence AS left_confidence,
                -- 先按主键拿到两个实体，再各自问一句「同库里还有同名的吗」（走
                -- entities_kb_name_idx）。从前写成 e JOIN x 再 WHERE e.id IN (...)：
                -- 表刚成批写完、还没来得及 ANALYZE 时，规划器从 x 那一侧起步，每条违规
                -- 扫全部实体再逐个回查，一页 37 条要 1.6 秒，翻页越靠后越慢；有统计信息时
                -- 也要 40ms。改后两种情况都是十几毫秒，结果逐行相同
                EXISTS (
                    SELECT 1 FROM entities e
                    WHERE e.id IN (lf.subject_id, lf.object_id)
                      AND EXISTS (SELECT 1 FROM entities x
                                   WHERE x.kb_id = e.kb_id AND x.id <> e.id
                                     AND x.merged_into IS NULL
                                     AND lower(x.canonical_name) = lower(e.canonical_name))
                ) AS same_name_peers
           FROM axiom_violations v
           JOIN triple l  ON l.id  = v.left_fact
           JOIN triple rt ON rt.id = v.right_fact
           JOIN facts lf ON lf.id = v.left_fact
          WHERE v.kb_id = $1 AND v.status = 'open'
          -- id 做第二键（#646）：一轮检查插下的行 detected_at 全都相同，只按它排，
          -- 翻页时每一页的先后可以不同——有的行出现两次，有的一次也不出现
          ORDER BY v.detected_at DESC, v.id DESC
          LIMIT $2 OFFSET $3",
        // "The left side is still open" is judged from the end point read back (0022): one whose end date is unknown is not counted as open
        left_holds_to = crate::world_axis::facts_holds_to("lf"),
    ))
    .bind(kb_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r: ViolationRow| {
        let hint = if r.kind == "derived_contradiction" {
            hint_for(&r).map(String::from)
        } else {
            None
        };
        AxiomViolation {
            id: r.id,
            kind: r.kind,
            predicate: r.predicate,
            left_fact: r.left_fact,
            left_text: r.left_text,
            right_fact: r.right_fact,
            right_text: r.right_text,
            path_len: r.path_len,
            detected_at: r.detected_at,
            detail: r.detail,
            hint,
            path: serde_json::from_value(r.path).unwrap_or_default(),
        }
    })
    .collect())
}

#[derive(sqlx::FromRow)]
struct ViolationRow {
    id: Uuid,
    kind: String,
    predicate: Option<String>,
    left_fact: Uuid,
    left_text: String,
    right_fact: Uuid,
    right_text: String,
    path_len: i32,
    detected_at: chrono::DateTime<chrono::Utc>,
    detail: serde_json::Value,
    path: serde_json::Value,
    left_open: bool,
    left_confidence: f32,
    same_name_peers: bool,
}

/// Hints are ordered by the most common mistake (0017 §2): an old assertion with no end date, two entities with the same name, and extraction simply
/// unsure. Give only one at a time: three side by side is the same as giving none
fn hint_for(r: &ViolationRow) -> Option<&'static str> {
    if r.left_open && r.detail.get("valid_from").is_some_and(|v| !v.is_null()) {
        Some("stale")
    } else if r.same_name_peers {
        Some("duplicate")
    } else if r.left_confidence < 0.75 {
        Some("unsure")
    } else {
        None
    }
}

/// A person rules on one violation.
///
/// **Four ways out, not two.** Temporal conflict asks "which one is right", but here the definition may be wrong:
/// a user-imported ontology declared some attribute antisymmetric, while in his own corpus that relation is actually bidirectional.
/// `axiom_relaxed` records exactly that: it is the ontology that should change, not twenty facts.
///
/// Changing the status does not delete the row, the same rule as the ledger: the fact that a ruling was made must itself be recorded, and `run` relies on
/// `status = 'open'` to tell which are derived and can be recomputed away: a person's decision must survive re-runs.
/// Which fact to retract within one violation.
///
/// Single-fact kinds (self-loop, signature, derivation colliding with an assertion) have only one, no need to say; two-fact and cycle kinds need a person to name it,
/// and it can only name the facts the violation itself lists: retracting an unrelated fact is not a ruling, it is a mistake.
pub fn pick_retraction(
    left: Uuid,
    right: Uuid,
    path: &[Uuid],
    requested: Option<Uuid>,
) -> Option<Uuid> {
    if left == right {
        return match requested {
            None => Some(left),
            Some(r) if r == left => Some(left),
            Some(_) => None,
        };
    }
    let r = requested?;
    (r == left || r == right || path.contains(&r)).then_some(r)
}

/// "The data is wrong": **really retract that fact**, then mark the violation resolved (#202).
///
/// Before, only `axiom_violations` changed and the fact stayed alive on the graph; a re-run hitting the resolved row then did nothing,
/// so the violation neither disappeared nor stopped reappearing. What is retracted is the `reject_fact` path: `invalidated_at`,
/// evidence untouched, the ledger keeps the record. The retracted id is returned so the caller can write the audit.
pub async fn retract_from_violation(
    pool: &PgPool,
    kb_id: Uuid,
    violation_id: Uuid,
    requested: Option<Uuid>,
    actor: Uuid,
) -> AppResult<Uuid> {
    let row: Option<(Uuid, Uuid, Vec<Uuid>)> = sqlx::query_as(
        "SELECT left_fact, right_fact, path FROM axiom_violations
          WHERE id = $1 AND kb_id = $2 AND status = 'open'",
    )
    .bind(violation_id)
    .bind(kb_id)
    .fetch_optional(pool)
    .await?;
    let Some((left, right, path)) = row else {
        return Err(utopia_core::AppError::NotFound);
    };
    let Some(target) = pick_retraction(left, right, &path, requested) else {
        return Err(utopia_core::AppError::invalid(
            "fact_required",
            "这处违规涉及多条事实，要说撤哪一条，且只能是它列出的那几条",
        ));
    };
    crate::graph::reject_fact(pool, kb_id, target).await?;
    decide(pool, kb_id, violation_id, "fact_retracted", actor).await?;
    Ok(target)
}

pub async fn decide(
    pool: &PgPool,
    kb_id: Uuid,
    violation_id: Uuid,
    resolution: &str,
    actor: Uuid,
) -> AppResult<()> {
    let res = sqlx::query(
        "UPDATE axiom_violations
            SET status = 'resolved', resolution = $3, decided_by = $4, decided_at = now()
          WHERE id = $2 AND kb_id = $1 AND status = 'open'",
    )
    .bind(kb_id)
    .bind(violation_id)
    .bind(resolution)
    .bind(actor)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(utopia_core::AppError::NotFound);
    }
    Ok(())
}

// ===================== The other half of R0: the ontology itself =====================

/// Output of the ontology self-consistency check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OntologyReport {
    pub classes: usize,
    pub found: usize,
    pub inserted: usize,
    pub cleared: usize,
}

/// Measure the ontology itself: predicate axiom combinations, subClassOf cycles, unsatisfiable classes.
///
/// Same re-run rules as [`run`]: `open` is derived state and can be recomputed away, `resolved`
/// is a human ruling and is left alone.
pub async fn check_ontology(pool: &PgPool, kb_id: Uuid) -> AppResult<OntologyReport> {
    let ax = axioms(&mut *pool.acquire().await?, kb_id).await?;
    let parents: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT child_id, parent_id FROM entity_type_parents p
                          JOIN entity_types t ON t.id = p.child_id
                         WHERE t.kb_id = $1",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    let disjoint: Vec<(Uuid, Uuid)> =
        sqlx::query_as("SELECT a_id, b_id FROM entity_type_disjoint WHERE kb_id = $1")
            .bind(kb_id)
            .fetch_all(pool)
            .await?;
    let classes: i64 = sqlx::query_scalar("SELECT count(*) FROM entity_types WHERE kb_id = $1")
        .bind(kb_id)
        .fetch_one(pool)
        .await?;

    let defects = utopia_reason::ontology::check_ontology(&ax, &parents, &disjoint);
    let mut report = OntologyReport {
        classes: classes as usize,
        found: defects.len(),
        ..Default::default()
    };

    let mut tx = pool.begin().await?;
    let mut fresh: Vec<Uuid> = Vec::with_capacity(defects.len());
    for d in &defects {
        let existing: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM ontology_defects
              WHERE kb_id = $1 AND kind = $2 AND subject = $3
                AND other IS NOT DISTINCT FROM $4",
        )
        .bind(kb_id)
        .bind(d.kind.as_str())
        .bind(d.subject)
        .bind(d.other)
        .fetch_optional(&mut *tx)
        .await?;
        let id = match existing {
            Some((id,)) => id,
            None => {
                let id = Uuid::now_v7();
                sqlx::query(
                    "INSERT INTO ontology_defects (id, kb_id, kind, subject, other, path)
                     VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(id)
                .bind(kb_id)
                .bind(d.kind.as_str())
                .bind(d.subject)
                .bind(d.other)
                .bind(&d.path)
                .execute(&mut *tx)
                .await?;
                report.inserted += 1;
                id
            }
        };
        fresh.push(id);
    }
    let cleared = sqlx::query(
        "DELETE FROM ontology_defects
          WHERE kb_id = $1 AND status = 'open' AND kind <> 'rules_disagree'
            AND NOT (id = ANY($2))",
    )
    .bind(kb_id)
    .bind(&fresh)
    .execute(&mut *tx)
    .await?;
    report.cleared = cleared.rows_affected() as usize;
    tx.commit().await?;
    Ok(report)
}

// ===================== R1: materialized inference =====================

/// Output of one derivation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeriveReport {
    /// Number of compiled rules. **When zero, the conclusion is "no rules" rather than "nothing can be derived"**
    pub rules: usize,
    pub edges: usize,
    /// Total derivations computed this round
    pub derived: usize,
    /// Newly stored
    pub inserted: usize,
    /// Those whose premises are gone and were voided along with them
    pub invalidated: usize,
    /// Number of predicates that hit the per-predicate cap and were not fully derived
    pub capped: usize,
    /// **Number of derivations inferred but with no matching rule row. Normally this is always zero.**
    ///
    /// Non-zero means rule compilation and inference have fallen out of step. Previously this was a single `continue` here,
    /// so the `works_at` fact that `ceo_of ⊑ works_at` inferred was **inferred but never stored**,
    /// while the `employs` that downstream inferred from it did get stored: a derived premise vanished into thin air.
    /// Count it, and do not let it fail silently once more.
    pub unruled: usize,
    /// Inferred but collided with an assertion or another derivation, and blocked this round without landing (0017). **Every blocked one
    /// has a corresponding row on Review**: `run` and this use the same function to compute it
    pub blocked: usize,
    /// Number of business rules evaluated (0021)
    pub attribute_rules: usize,
    /// Number of business rule hits
    pub rule_hits: usize,
    /// Number of (rule, entity) pairs whose premise combinations were too many to fully expand. **Reported separately from "not satisfied"**
    pub rule_capped: usize,
    /// How many rounds the business rules ran (0030). One round's conclusions feed the next round's input, until a round produces
    /// no new conclusions. One link on the chain per round, so 1 means "no chain", 2 means `A → B`
    pub rule_rounds: usize,
    /// Still producing output after running all `MAX_DEPTH` rounds: the chain is longer than the cap, so the rest did not connect. **Reported separately
    /// from "not satisfied"**: "not derived" and "does not hold" look exactly the same in the results.
    pub rule_rounds_capped: bool,
    /// The conclusion did not change but the proof did, so the premise-chain rows were rewritten (0030). The same statement can have
    /// a second basis, and the reconciliation key has no premises: without rewriting, that row would keep hanging on last round's
    /// reason, and the chain might still hold a premise that was just voided
    pub reproved: usize,
    /// The conclusion did not change but the rule definition moved to a new version, so rows are kept and point at the new version (0060)
    pub redefined: usize,
}

/// The precision and confidence a derivation gets from its premises (0024).
///
/// **Extracted because of the chain** (0030): the middle layer of a chain needs it both to compute its own two ends and, as
/// the next layer's premise, to read it again through the same code. If the storage site and the fixed-point site each wrote their own copy,
/// their judgments about "which end was lifted by the anchor" would eventually diverge.
///
/// Precision follows **the premise that wins this end**: the start of the derivation is the latest start among the premises,
/// and its precision is that premise's precision. When several premises sit side by side, take the coarsest. If that end was lifted up
/// by a premise's **anchor** (0022), it has no precision to speak of.
fn premise_meta(
    premises: &[Uuid],
    from: Option<i64>,
    to: Option<i64>,
    spans: &HashMap<Uuid, (Option<i64>, Option<i64>)>,
    meta: &HashMap<Uuid, PremiseMeta>,
) -> PremiseMeta {
    let mut fp: Option<String> = None;
    let mut tp: Option<String> = None;
    let mut conf = 1.0f32;
    let mut from_anchored = false;
    let mut to_anchored = false;
    for p in premises {
        let Some((pf, pt, pc, fa, ta)) = meta.get(p) else {
            continue;
        };
        // Confidence takes the lowest among premises: a chain is only as trustworthy as its weakest link
        conf = conf.min(*pc);
        let Some((sf, st)) = spans.get(p) else {
            continue;
        };
        if from.is_some() && *sf == from {
            if *fa {
                from_anchored = true;
            } else {
                fp = coarsest(fp.as_deref(), pf.as_deref());
            }
        }
        if to.is_some() && *st == to {
            if *ta {
                to_anchored = true;
            } else {
                // 'unknown' is not a granularity, it is the mark for "ended, date unknown"; the end it lifts
                // is an anchor, which takes the path above
                tp = coarsest(
                    tp.as_deref(),
                    pt.as_deref().filter(|p| *p != crate::graph::ENDED_UNKNOWN),
                );
            }
        }
    }
    (fp, tp, conf, from_anchored, to_anchored)
}

/// The part of the rules that the main class reaches only through **derived classification**: the scope changes from a sieve into a condition (0030).
///
/// Expanding `scope AND ((A AND B) OR C)` gives `(scope AND A AND B) OR (scope AND C)`, so each group
/// adds one more, and disjunction stays one level deep (0029). Writing it as a condition rather than a sieve lets the classification
/// derived fact enter the premises: the conclusion's interval narrows with it, and when it is voided the conclusion retires too.
fn scoped_by_conclusion(
    rule: &utopia_reason::rules::BusinessRule,
    is_a: Uuid,
    classes: &[String],
) -> utopia_reason::rules::BusinessRule {
    use utopia_reason::rules::{Condition, Op, Operand};
    let groups: std::collections::BTreeSet<i32> = rule.conditions.iter().map(|c| c.group).collect();
    let mut conditions = rule.conditions.clone();
    for group in groups {
        conditions.push(Condition {
            group,
            side: utopia_reason::rules::Side::X,
            predicate: is_a,
            op: Op::In,
            operand: Operand::Set(classes.to_vec()),
        });
    }
    utopia_reason::rules::BusinessRule {
        id: rule.id,
        conclusion: rule.conclusion.clone(),
        join_predicate: rule.join_predicate,
        conditions,
    }
}

/// One fetch, three things: edges with intervals, each fact's interval, and precision and confidence.
/// Shared by `run` and `materialize`: the edges both see must be the same batch
/// of premises, with their precision and confidence, and whether each end **is an anchor** (0022): an end that comes from the evidence date rather than the document's
/// date; a derived row has no precision to speak of at that end
type PremiseMeta = (Option<String>, Option<String>, f32, bool, bool);

type TimedEdges = (
    Vec<TimedEdge>,
    HashMap<Uuid, (Option<i64>, Option<i64>)>,
    HashMap<Uuid, PremiseMeta>,
);

/// The interval a premise **reads out** (0022): if the text gives no start, it starts from the anchor; if it says "ended, date unknown", it runs up to
/// the anchor. A row unknown at both ends reads as an empty interval, which falls out naturally when intersecting: it cannot support any derivation.
/// Returns `(from, to, from_anchored, to_anchored)`.
fn read_span(
    temporal: crate::graph::Temporal,
    from: Option<chrono::DateTime<chrono::Utc>>,
    from_precision: Option<&str>,
    to: Option<chrono::DateTime<chrono::Utc>>,
    to_precision: Option<&str>,
    attested_from: Option<chrono::DateTime<chrono::Utc>>,
    attested_to: Option<chrono::DateTime<chrono::Utc>>,
) -> (Option<i64>, Option<i64>, bool, bool) {
    use crate::graph::Temporal;
    match temporal {
        // Constant at every moment; evidence dates do not gate it (0031)
        Temporal::Eternal => return (None, None, false, false),
        // An event holds within the bucket it names; an event with no date has an empty interval: `overlap` does not infer over an empty intersection,
        // so a chain through "acquired at an unknown time" infers nothing, consistent with the read side (0031).
        // Event rows written before 0031 have an empty end, and are read by their start bucket
        Temporal::Event => {
            return match from {
                // An event with no date has an empty interval. So does one without a witness (0064 decision 5): both ends fall
                // in an unreachable far distance, disjoint from any interval
                None => {
                    let a = attested_from.map_or(i64::MAX / 4, |a| a.timestamp());
                    (Some(a), Some(a), true, true)
                }
                Some(f) => {
                    let precision = to_precision
                        .filter(|p| *p != crate::graph::ENDED_UNKNOWN)
                        .or(from_precision);
                    let end = crate::graph::bucket_end(to.unwrap_or(f), precision);
                    (Some(f.timestamp()), Some(end.timestamp()), false, false)
                }
            };
        }
        Temporal::State => {}
    }
    let (f, from_anchored) = match from {
        Some(x) => (Some(x.timestamp()), false),
        // No start: start from the witness; with no witness either, the lower bound is open: it holds at any point in time (0064 decision 5)
        None => match attested_from {
            Some(a) => (Some(a.timestamp()), true),
            None => (None, false),
        },
    };
    // A row whose end is unknown must carry attested_to per the CHECK; if somehow absent, read it as open: better to infer a bit more
    // than to conjure an end point out of nothing
    let (t, to_anchored) = match (to, to_precision, attested_to) {
        (Some(x), _, _) => (Some(x.timestamp()), false),
        (None, Some(p), Some(a)) if p == crate::graph::ENDED_UNKNOWN => (Some(a.timestamp()), true),
        (None, _, _) => (None, false),
    };
    (f, t, from_anchored, to_anchored)
}

async fn timed_edges(conn: &mut PgConnection, kb_id: Uuid) -> AppResult<TimedEdges> {
    // The input is **assertions only**. Derivations live in another table, so no filter is even needed here: that is
    // what splitting into tables buys. Forgetting to exclude has the consequence of inferring nothing, not feeding one's own output back into itself
    let rows: Vec<EdgeRow> = sqlx::query_as(
        "SELECT id, predicate_id, subject_id, object_id,
                valid_from, valid_to, valid_from_precision, valid_to_precision, confidence,
                attested_from, attested_to
           FROM facts
          WHERE kb_id = $1
            AND invalidated_at IS NULL
            AND predicate_id IS NOT NULL
            AND object_id IS NOT NULL",
    )
    .bind(kb_id)
    .fetch_all(&mut *conn)
    .await?;
    // Temporal semantics of the predicate (0031): events read by their bucket, constants open at both ends. Fetch all at once, look up by predicate
    let temporal_of: HashMap<Uuid, crate::graph::Temporal> = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, temporal FROM relation_types WHERE kb_id = $1",
    )
    .bind(kb_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|(id, t)| (id, crate::graph::Temporal::parse(&t)))
    .collect();

    let mut edges = Vec::with_capacity(rows.len());
    let mut meta: HashMap<Uuid, PremiseMeta> = HashMap::new();
    let mut spans: HashMap<Uuid, (Option<i64>, Option<i64>)> = HashMap::new();
    for (id, pred, subj, obj, from, to, fp, tp, conf, attested_from, attested_to) in rows {
        // Infer from the read-back intervals (0022): premises with no start start from the earliest evidence, and ones where "ended, date unknown"
        // run up to the document that says it. Read as open, a chain through "former CEO" would infer
        // an edge that still holds today
        let temporal = temporal_of.get(&pred).copied().unwrap_or_default();
        let (f, t, fa, ta) = read_span(
            temporal,
            from,
            fp.as_deref(),
            to,
            tp.as_deref(),
            attested_from,
            attested_to,
        );
        edges.push(TimedEdge {
            edge: Edge {
                fact: id,
                predicate: pred,
                subject: subj,
                object: obj,
            },
            from: f,
            to: t,
        });
        spans.insert(id, (f, t));
        meta.insert(id, (fp, tp, conf, fa, ta));
    }
    Ok((edges, spans, meta))
}

/// Pairs of (derived triple, assertion) a person accepted as coexisting: these derivations still land next round as usual (0017 §2).
async fn accepted_clashes(
    conn: &mut PgConnection,
    kb_id: Uuid,
) -> AppResult<HashSet<(Uuid, Uuid, Uuid, Uuid)>> {
    let rows: Vec<(Uuid, serde_json::Value)> = sqlx::query_as(
        "SELECT left_fact, detail FROM axiom_violations
          WHERE kb_id = $1 AND kind = 'derived_contradiction' AND resolution = 'accepted'",
    )
    .bind(kb_id)
    .fetch_all(&mut *conn)
    .await?;
    let id = |v: &serde_json::Value, k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<Uuid>().ok())
    };
    Ok(rows
        .into_iter()
        .filter_map(|(against, d)| {
            Some((
                id(&d, "subject_id")?,
                id(&d, "predicate_id")?,
                id(&d, "object_id")?,
                against,
            ))
        })
        .collect())
}

/// Identity of a derived fact: subject + predicate + object + interval.
///
/// **Including the interval in the key** is deliberate: when the interval changes it is a different assertion; the old one is voided, the new one lands,
/// because the ledger does not allow in-place edits.
///
/// The two object slots correspond one-to-one with the two channels `derived_facts` widened to (0021 decision 1): an entity object
/// goes in `Option<Uuid>`, a literal conclusion goes in that normalized JSON string. Both slots take part in comparison; otherwise
/// two different conclusions on the same class would be taken as one.
/// The two slots of a premise on `fact_derivations` (0030): one for assertions, one for derivations,
/// exactly one of which has a value: the database's CHECK says just that
type PremiseCols = (Option<Uuid>, Option<Uuid>);

type DerivedKey = (
    Uuid,
    Uuid,
    Option<Uuid>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

type AssertedEdges = HashMap<(Uuid, Uuid, Uuid), Vec<(Option<i64>, Option<i64>)>>;

/// One derivation to be stored this round. The axiom-derived and rule-derived ones merge here:
/// **merging is required**: the reconciliation for stale rows scans the whole table, and two separately run diffs would judge
/// each other's rows stale and void them every round.
#[derive(Clone)]
struct Wanted {
    subject: Uuid,
    predicate: Uuid,
    object_id: Option<Uuid>,
    object_value: Option<serde_json::Value>,
    from: Option<i64>,
    to: Option<i64>,
    premises: Vec<Uuid>,
    /// An axiom rule (`rules.id`) or a business rule (`attribute_rules.id`), exactly one
    rule_id: Option<Uuid>,
    attribute_rule_id: Option<Uuid>,
    /// For a business rule: which version of its definition (0060)
    attribute_rule_version_id: Option<Uuid>,
}

/// Normalized text form of a JSON value, used only as a key.
///
/// `serde_json::Value` is not itself `Hash`, but the key must carry the literal object; serializing it to a
/// string is the simplest and most stable approach: `Map` in serde_json defaults to `BTreeMap`,
/// so the same content serializes byte-for-byte identically.
fn value_key(v: Option<&serde_json::Value>) -> Option<String> {
    v.map(|v| v.to_string())
}

/// Precision takes "the coarsest one".
///
/// The two ends of a derived interval each come from one premise; strictly, each should carry its own precision. Taking the coarsest is **deliberately conservative**:
/// a chain is only as certain as its least certain link, and labelling a conclusion drawn from a year-level premise as day
/// is exactly what the `facts.valid_from_precision` comment warns against: filling in
/// a definite value where one is not known.
fn coarsest(a: Option<&str>, b: Option<&str>) -> Option<String> {
    // Ladder order (0024): year is coarsest, second is finest; unrecognized is treated as finest, so as not to override recognized ones
    let rank = |p: &str| {
        crate::graph::WORLD_PRECISIONS
            .iter()
            .position(|w| *w == p)
            .unwrap_or(crate::graph::WORLD_PRECISIONS.len())
    };
    match (a, b) {
        (Some(x), Some(y)) => Some(if rank(x) <= rank(y) { x } else { y }.to_string()),
        (Some(x), None) | (None, Some(x)) => Some(x.to_string()),
        (None, None) => None,
    }
}

fn axiom_rule_keys(ax: &HashMap<Uuid, Axioms>) -> Vec<(Uuid, RuleKind)> {
    let mut want: Vec<(Uuid, RuleKind)> = Vec::new();
    for (&pred, a) in ax {
        if a.transitive {
            want.push((pred, "transitive"));
        }
        if a.symmetric {
            want.push((pred, "symmetric"));
        }
        // The last two kinds are rule sources added by migration 0016 (a_relation_can_name_its_inverse). **Rules attach to the side that has the declaration**:
        // both sides of a normalized inverse have declarations, so each direction gets one rule, matching the derivations
        // each one infers.
        if a.inverse_of.is_some() {
            want.push((pred, "inverse"));
        }
        if a.sub_property_of.is_some() {
            want.push((pred, "sub_property"));
        }
    }
    want.sort();
    want
}

/// Recompile rules from the ontology axioms, returning `(predicate, kind) → rule id`.
///
/// **Idempotent**: identity is `(kb, predicate, kind)`, so recompilation recognizes "still the same rule"; otherwise every run would make
/// `derived_facts.rule_id` point at a new id, and the history would break.
///
/// **Rules whose axioms were retracted are not deleted.** Derivation rows that have since been invalidated still point at them, and explaining
/// "which rule inferred this at the time" needs it to still exist; it no longer appears in the return value, so facts inferred from it are voided by the reconciliation below.
/// A database only has a few rules, so keeping them takes no real room.
async fn compile_rules(
    conn: &mut PgConnection,
    kb_id: Uuid,
    ax: &HashMap<Uuid, Axioms>,
) -> AppResult<HashMap<(Uuid, RuleKind), Uuid>> {
    let mut out = HashMap::new();
    for (pred, kind) in axiom_rule_keys(ax) {
        sqlx::query(
            "INSERT INTO rules (id, kb_id, predicate_id, kind) VALUES ($1, $2, $3, $4)
             ON CONFLICT (kb_id, predicate_id, kind) DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(kb_id)
        .bind(pred)
        .bind(kind)
        .execute(&mut *conn)
        .await?;
        let (id,): (Uuid,) = sqlx::query_as(
            "SELECT id FROM rules WHERE kb_id = $1 AND predicate_id = $2 AND kind = $3",
        )
        .bind(kb_id)
        .bind(pred)
        .bind(kind)
        .fetch_one(&mut *conn)
        .await?;
        out.insert((pred, kind), id);
    }
    Ok(out)
}

pub struct AxiomRule {
    pub kind: &'static str,
    pub predicate: String,
    pub target: String,
}

/// Reads the declarations the compiler adopted, including the filled-in inverse pairs.
/// Stored rules still keep retracted declarations, while new declarations may not be compiled yet; listing them cannot materialize or write rules.
pub async fn active_axiom_rules(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<AxiomRule>> {
    let mut conn = pool.acquire().await?;
    let ax = axioms(&mut conn, kb_id).await?;
    let names: HashMap<Uuid, String> =
        sqlx::query_as("SELECT id, key FROM relation_types WHERE kb_id=$1")
            .bind(kb_id)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();
    let mut rules = Vec::new();
    for (predicate, kind) in axiom_rule_keys(&ax) {
        let target = match kind {
            "inverse" => ax[&predicate].inverse_of,
            "sub_property" => ax[&predicate].sub_property_of,
            _ => Some(predicate),
        };
        if let (Some(predicate), Some(target)) =
            (names.get(&predicate), target.and_then(|id| names.get(&id)))
        {
            rules.push(AxiomRule {
                kind,
                predicate: predicate.clone(),
                target: target.clone(),
            });
        }
    }
    rules.sort_by(|a, b| a.predicate.cmp(&b.predicate).then(a.kind.cmp(b.kind)));
    Ok(rules)
}

/// Extra info fetched along with edges (precision and confidence, needed when storing).
type EdgeRow = (
    Uuid,
    Uuid,
    Uuid,
    Uuid,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
    Option<String>,
    f32,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
);

type LiveRow = (
    Uuid,
    Uuid,
    Uuid,
    Option<Uuid>,
    Option<serde_json::Value>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// One business rule with its conditions, already parsed into the evaluator's shape.
struct LoadedRule {
    rule: utopia_reason::rules::BusinessRule,
    /// The rule only looks at entities of this class **and its subclasses**
    subject_types: Vec<Uuid>,
    /// The names of the classes above (IRI, falling back to key when none). Derived classification records names, while the main class scope records
    /// ids: for a chain to connect, both sides need a copy (0030)
    subject_classes: Vec<String>,
    /// The predicate the conclusion lands on: classification lands on `is_a`, an attribute lands on its own
    conclude_predicate: Uuid,
    /// Which version of the rule definition this round read (0060): an inferred row records it, so the proof can say
    /// "how the rule read at the time". The migration gave every rule a version 1, so normally there always is one
    version: Option<Uuid>,
}

/// A batch of compiled rules, plus two things needed to connect chains (0030).
struct LoadedRules {
    rules: Vec<LoadedRule>,
    /// The built-in `is_a`. Derived classification lands on it, and reading it back down the chain also reads from it
    is_a: Option<Uuid>,
    /// Class name (IRI, falling back to key when none) → class id. For a derived classification to become "this entity belongs to this class",
    /// it must go through this step
    class_ids: HashMap<String, Uuid>,
}

/// One rule row needed for evaluation: id, main class, conclusion kind, the three slots of the conclusion, plus the conclusion class's IRI and key
/// (classification conclusions are recorded by IRI, falling back to key when none)
type RuleDefRow = (
    Uuid,
    Uuid,
    String,
    Option<Uuid>,
    Option<Uuid>,
    Option<serde_json::Value>,
    // The conclusion tree computed (0032)
    Option<serde_json::Value>,
    Option<String>,
    Option<String>,
    Option<Uuid>,
    // Current version's id (0060)
    Option<Uuid>,
);

/// Fetch business rules. A rule whose condition shape is invalid is **skipped whole rather than erroring out**: one badly written
/// rule should not stall the entire materialization, and the fact that it produces nothing is visible in the report's count.
async fn attribute_rules(conn: &mut PgConnection, kb_id: Uuid) -> AppResult<LoadedRules> {
    use utopia_reason::rules::{BusinessRule, Conclusion, Condition, Op};

    // **Sort by id**: when rules collide on the same conclusion, the proof left is the one that arrived first, and
    // "first arrived" should not be decided by HashMap order: two materializations of the same database must give the same proof
    // (0029 says the same thing between groups; 0030 amplified it through chains)
    let rows: Vec<RuleDefRow> = sqlx::query_as(
        "SELECT r.id, r.subject_type_id, r.conclusion,
                r.conclude_type_id, r.conclude_predicate_id, r.conclude_value,
                r.conclude_expr, ct.iri, ct.key, r.join_predicate_id,
                (SELECT v.id FROM attribute_rule_versions v
                  WHERE v.rule_id = r.id AND v.superseded_at IS NULL) AS version_id
           FROM attribute_rules r
           LEFT JOIN entity_types ct ON ct.id = r.conclude_type_id
          WHERE r.kb_id = $1 AND r.enabled
          ORDER BY r.id",
    )
    .bind(kb_id)
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(LoadedRules {
            rules: Vec::new(),
            is_a: None,
            class_ids: HashMap::new(),
        });
    }

    // Class name ↔ class id, one map in each direction: classification conclusions are recorded by name (renaming a label should not turn an
    // already-inferred conclusion into a different one, 0021 decision 2), whereas the main class scope and entity classes are ids
    let type_rows: Vec<(Uuid, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id, iri, key FROM entity_types WHERE kb_id = $1")
            .bind(kb_id)
            .fetch_all(&mut *conn)
            .await?;
    let mut class_ids: HashMap<String, Uuid> = HashMap::new();
    let mut class_name: HashMap<Uuid, String> = HashMap::new();
    for (id, iri, key) in type_rows {
        let Some(name) = iri.or(key) else { continue };
        class_ids.insert(name.clone(), id);
        class_name.insert(id, name);
    }

    // A classification conclusion must land on the built-in `is_a`. If the rule exists, it was already created
    // (the rule-creation step is responsible); if it cannot be found here, the database was edited by hand: skip rather than create one
    let is_a: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM relation_types WHERE kb_id = $1 AND key = 'is_a'")
            .bind(kb_id)
            .fetch_optional(&mut *conn)
            .await?;

    let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    // Group order first: when two groups infer the same interval, the proof left is the stable one (0029)
    let conds: Vec<(Uuid, i32, Uuid, String, Option<serde_json::Value>, String)> = sqlx::query_as(
        "SELECT rule_id, group_seq, predicate_id, op, operand, subject_side
           FROM attribute_rule_conditions
          WHERE rule_id = ANY($1)
          ORDER BY rule_id, group_seq, seq",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut by_rule: HashMap<Uuid, Vec<Condition>> = HashMap::new();
    let mut broken: HashSet<Uuid> = HashSet::new();
    for (rule_id, group, predicate, op, operand, side) in conds {
        let Some(op) = Op::parse(&op) else {
            broken.insert(rule_id);
            continue;
        };
        let Some(operand) = parse_operand(op, operand.as_ref()) else {
            broken.insert(rule_id);
            continue;
        };
        by_rule.entry(rule_id).or_default().push(Condition {
            group,
            side: utopia_reason::rules::Side::parse(&side).unwrap_or(utopia_reason::rules::Side::X),
            predicate,
            op,
            operand,
        });
    }

    let mut out = Vec::new();
    for (
        id,
        subject_type,
        conclusion,
        conclude_type,
        conclude_pred,
        conclude_value,
        conclude_expr,
        iri,
        key,
        join_predicate,
        version,
    ) in rows
    {
        if broken.contains(&id) {
            continue;
        }
        let Some(conditions) = by_rule.remove(&id) else {
            // A rule with no conditions infers nothing (the evaluator also judges it this way), so there is no need to go further
            continue;
        };
        let (conclusion, predicate) = match conclusion.as_str() {
            "typing" => {
                let (Some(_), Some((is_a,))) = (conclude_type, is_a) else {
                    continue;
                };
                // **Recorded by IRI; fall back to key only when there is no IRI**: renaming a label should not
                // turn an already-inferred conclusion into a different one (0021 decision 2)
                let Some(class) = iri.or(key) else { continue };
                (Conclusion::Typing { class }, is_a)
            }
            // Computed conclusion (0032): the predicate is unchanged, the value is computed by the formula at evaluation time from the chosen reading
            "computed" => {
                let (Some(p), Some(e)) = (conclude_pred, conclude_expr) else {
                    continue;
                };
                let Ok(expr) = utopia_reason::rules::Expr::from_json(&e) else {
                    continue;
                };
                (Conclusion::Computed { predicate: p, expr }, p)
            }
            "attribute" => {
                let (Some(p), Some(v)) = (conclude_pred, conclude_value) else {
                    continue;
                };
                (
                    Conclusion::Attribute {
                        predicate: p,
                        value: v,
                    },
                    p,
                )
            }
            // The edge conclusion uses the relation predicate directly; the
            // join edge itself arrives in the evaluator as another premise.
            "relation" => {
                let (Some(p), Some(_)) = (conclude_pred, join_predicate) else {
                    continue;
                };
                (Conclusion::Relation { predicate: p }, p)
            }
            _ => continue,
        };
        let subject_types = descendants_of(&mut *conn, kb_id, subject_type).await?;
        let subject_classes = subject_types
            .iter()
            .filter_map(|t| class_name.get(t).cloned())
            .collect();
        out.push(LoadedRule {
            rule: BusinessRule {
                id,
                conclusion,
                join_predicate,
                conditions,
            },
            subject_types,
            subject_classes,
            conclude_predicate: predicate,
            version,
        });
    }
    Ok(LoadedRules {
        rules: out,
        is_a: is_a.map(|(id,)| id),
        class_ids,
    })
}

/// Parse an operand by op. If the shape is wrong, return None and the caller skips the whole rule.
fn parse_operand(
    op: utopia_reason::rules::Op,
    raw: Option<&serde_json::Value>,
) -> Option<utopia_reason::rules::Operand> {
    use utopia_reason::rules::{Op, Operand};
    match op {
        Op::Present => Some(Operand::None),
        Op::Between => {
            let arr = raw?.as_array()?;
            let (lo, hi) = (arr.first()?.as_f64()?, arr.get(1)?.as_f64()?);
            Some(Operand::Range(lo.min(hi), lo.max(hi)))
        }
        // **In and NotIn share one branch.** Leaving out the latter does not mean "this condition was judged wrong" but
        // `parse_operand` returns None, and the whole rule is treated as badly written and skipped: a rule using "not in"
        // infers nothing from then on, while on screen it looks fine (0029 / #476)
        Op::In | Op::NotIn => {
            let arr = raw?.as_array()?;
            let set: Vec<String> = arr
                .iter()
                .map(|v| match v {
                    serde_json::Value::String(s) => s.trim().to_string(),
                    other => other.to_string(),
                })
                .collect();
            (!set.is_empty()).then_some(Operand::Set(set))
        }
        // The threshold can be computed (0032): an object that is a formula, otherwise still just a number.
        // The four operand shapes are all different (number, two-element array, string array, object), so
        // they can be told apart, with no extra column saying "which kind this is"
        _ => {
            let raw = raw?;
            if raw.is_object() {
                return utopia_reason::rules::Expr::from_json(raw)
                    .ok()
                    .map(Operand::Calc);
            }
            Some(Operand::Num(raw.as_f64().or_else(|| {
                raw.as_str().and_then(|s| s.trim().parse().ok())
            })?))
        }
    }
}

/// A class together with all its subclasses. A rule written on `Well` should also see `HorizontalWell`'s entities.
async fn descendants_of(conn: &mut PgConnection, kb_id: Uuid, root: Uuid) -> AppResult<Vec<Uuid>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "WITH RECURSIVE sub AS (
             SELECT id FROM entity_types WHERE id = $2 AND kb_id = $1
             UNION
             SELECT p.child_id FROM entity_type_parents p JOIN sub ON p.parent_id = sub.id
         )
         SELECT id FROM sub",
    )
    .bind(kb_id)
    .bind(root)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// One row returned from the attribute-fact query: the fact, subject, predicate, literal value, the interval ends and precision, confidence, and evidence date.
type AttrFactRow = (
    Uuid,
    Uuid,
    Uuid,
    serde_json::Value,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
    Option<String>,
    f32,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// Attribute facts: live facts on the literal-value channel, together with interval and precision.
///
/// A counterpart to `timed_edges`: that one takes edges with `object_id IS NOT NULL`,
/// this one takes literal values with `object_value IS NOT NULL`. Both look only at assertions.
async fn attribute_facts(
    conn: &mut PgConnection,
    kb_id: Uuid,
) -> AppResult<(
    Vec<utopia_reason::rules::AttrFact>,
    HashMap<Uuid, (Option<i64>, Option<i64>)>,
    HashMap<Uuid, PremiseMeta>,
)> {
    let rows: Vec<AttrFactRow> = sqlx::query_as(
        "SELECT f.id, f.subject_id, f.predicate_id, f.object_value,
                f.valid_from, f.valid_to, f.valid_from_precision, f.valid_to_precision,
                f.confidence, f.attested_from, f.attested_to
           FROM facts f
           JOIN entities e ON e.id = f.subject_id
          WHERE f.kb_id = $1
            AND f.invalidated_at IS NULL
            AND f.predicate_id IS NOT NULL
            AND f.object_value IS NOT NULL
            AND e.merged_into IS NULL",
    )
    .bind(kb_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut facts = Vec::with_capacity(rows.len());
    let mut spans = HashMap::new();
    let mut meta = HashMap::new();
    for (id, subject, predicate, value, from, to, fp, tp, conf, attested_from, attested_to) in rows
    {
        // The same reading as the axiom path (0022): if the reading has no date, count from its document.
        // Attributes are always states (the state is fixed when the attribute is created, and the UI does not allow changing it)
        let (f, t, fa, ta) = read_span(
            crate::graph::Temporal::State,
            from,
            fp.as_deref(),
            to,
            tp.as_deref(),
            attested_from,
            attested_to,
        );
        // A literal of an attribute fact is `{"value": …, "unit": …}`; the comparison is on the value inside.
        // If it cannot be read, hand the whole object to the evaluator: it always judges an unrecognized shape as not satisfied
        let inner = value.get("value").cloned().unwrap_or_else(|| value.clone());
        facts.push(utopia_reason::rules::AttrFact {
            id,
            subject,
            predicate,
            value: inner,
        });
        spans.insert(id, (f, t));
        meta.insert(id, (fp, tp, conf, fa, ta));
    }
    Ok((facts, spans, meta))
}

/// Infer once and land the derived facts in the ledger.
///
/// **The caller is responsible for checking the `materialize_inferences` switch.** This layer does not judge: it is also used by
/// the "preview what would be inferred" path, and a preview should not be bound by the switch.
/// The identity, in the fixed point, of a relation edge inferred by a business rule (0047 decision 3).
///
/// One that stands: enters the pool as a temporary id, as an ordinary edge with an interval, and goes into the contradiction check as a candidate.
/// One that is blocked (`refused`): leaves the pool and leaves `wanted`, but **stays in the candidates**: the review queue
/// reports exactly why it did not land; if each computed its own, the queue would drift from the graph (0017)
struct RelationCandidate {
    edge: utopia_reason::rules::RuleEdge,
    key: DerivedKey,
    premises: Vec<Uuid>,
    rule_id: Uuid,
    refused: bool,
}

/// Output of one check: axiom derivations on the pool, plus rule-inferred relation candidates, and what they collided with.
struct Checked {
    /// The first `axiom_count` are the axiom derivations on the pool, followed by the relation candidates (both the standing and the blocked are there,
    /// tagged `Rule::Business`)
    candidates: Derivation,
    axiom_count: usize,
    /// Candidate index → the business rule that inferred it. Axiom derivations have none
    candidate_rule: HashMap<usize, Uuid>,
    clashes: Contradictions,
    /// Indexes of candidates that do not land: those colliding with an assertion without an accepted coexistence, and those colliding with other derivations
    blocked: HashSet<usize>,
}

/// Runs the axiom inference over the pool of "assertion edges + standing relation edges", then measures the candidates against the axioms.
///
/// The assertion side gives only assertions: relation edges are derived, and their collisions with other derivations (including axiom derivations inferred through them)
/// go down the derivation-to-derivation path, so `against` is always a row in `facts`: the review queue's foreign key
/// points at that table
fn check(
    edges: &[TimedEdge],
    relation: &[RelationCandidate],
    axioms: &HashMap<Uuid, Axioms>,
    spans: &HashMap<Uuid, (Option<i64>, Option<i64>)>,
    accepted: &HashSet<(Uuid, Uuid, Uuid, Uuid)>,
) -> Checked {
    let mut timed: Vec<TimedEdge> = edges.to_vec();
    timed.extend(relation.iter().filter(|r| !r.refused).map(|r| {
        let (from, to) = spans.get(&r.edge.id).copied().unwrap_or_default();
        TimedEdge {
            edge: Edge {
                fact: r.edge.id,
                predicate: r.edge.predicate,
                subject: r.edge.subject,
                object: r.edge.object,
            },
            from,
            to,
        }
    }));
    let mut candidates = utopia_reason::derive::derive(&timed, axioms);
    let axiom_count = candidates.facts.len();
    let mut candidate_rule: HashMap<usize, Uuid> = HashMap::new();
    for r in relation {
        candidate_rule.insert(candidates.facts.len(), r.rule_id);
        candidates.facts.push(utopia_reason::derive::Derived {
            predicate: r.edge.predicate,
            via: r.edge.predicate,
            subject: r.edge.subject,
            object: r.edge.object,
            rule: utopia_reason::derive::Rule::Business,
            premises: r.premises.clone(),
        });
    }
    let clashes = utopia_reason::derive::contradictions(&candidates, edges, axioms, spans);
    // asserted > derived is a hard rule (0002): a derivation that collides with an assertion does not land. Except those a person has accepted as coexisting;
    // when two derivations collide, neither lands; whether to accept only affects whether it is reported (0017)
    let mut blocked: HashSet<usize> = HashSet::new();
    for c in &clashes.with_assertions {
        let d = &candidates.facts[c.derived];
        if !accepted.contains(&(d.subject, d.predicate, d.object, c.against)) {
            blocked.insert(c.derived);
        }
    }
    for rc in &clashes.between_derivations {
        for (i, j) in &rc.pairs {
            blocked.insert(*i);
            blocked.insert(*j);
        }
    }
    Checked {
        candidates,
        axiom_count,
        candidate_rule,
        clashes,
        blocked,
    }
}

/// Once a blocked relation edge is removed, the conclusions standing on it retire together: the rule conclusions that read it, and the ones standing on those
/// the conclusions on those conclusions, until none are new: a derivation retires when its premise is voided (0002), and holds
/// in the fixed point too. A relation candidate whose premise is gone is not blocked, it is no longer inferable: it leaves entirely, and no candidate is kept
fn retire(
    seed: HashSet<Uuid>,
    wanted: &mut HashMap<DerivedKey, Wanted>,
    provisional: &mut HashMap<DerivedKey, Uuid>,
    fact_pool: &mut Vec<utopia_reason::rules::AttrFact>,
    relation: &mut Vec<RelationCandidate>,
) {
    let mut gone = seed;
    loop {
        let dropped: Vec<DerivedKey> = wanted
            .iter()
            .filter(|(_, w)| w.premises.iter().any(|p| gone.contains(p)))
            .map(|(k, _)| k.clone())
            .collect();
        if dropped.is_empty() {
            break;
        }
        for key in dropped {
            wanted.remove(&key);
            if let Some(prov) = provisional.remove(&key) {
                gone.insert(prov);
            }
        }
    }
    fact_pool.retain(|f| !gone.contains(&f.id));
    relation.retain(|r| r.refused || !gone.contains(&r.edge.id));
}

/// Removes from the pool the relation candidates this check blocked. Returns whether any left: if so, check again,
/// since the pool has changed.
fn refuse_blocked(
    checked: &Checked,
    relation: &mut Vec<RelationCandidate>,
    wanted: &mut HashMap<DerivedKey, Wanted>,
    provisional: &mut HashMap<DerivedKey, Uuid>,
    fact_pool: &mut Vec<utopia_reason::rules::AttrFact>,
) -> bool {
    let mut retired: HashSet<Uuid> = HashSet::new();
    for &i in &checked.blocked {
        let Some(r) = i
            .checked_sub(checked.axiom_count)
            .and_then(|k| relation.get_mut(k))
        else {
            continue;
        };
        if r.refused {
            continue;
        }
        r.refused = true;
        retired.insert(r.edge.id);
        wanted.remove(&r.key);
        provisional.remove(&r.key);
    }
    if retired.is_empty() {
        return false;
    }
    retire(retired, wanted, provisional, fact_pool, relation);
    true
}

/// One solve shared by `run()` (review queue) and `materialize()` (storage).
///
/// Input is assertions only (0013); output is the axiom derivations on the last round's pool, the rule conclusions, and the blocked
/// candidates. Both sides take from the same source, so the queue matches the graph (0017). Rule-inferred relation edges enter the pool as ordinary edges;
/// axiom inference reruns each round (0047 decision 3); a blocked relation edge leaves the pool but stays in the
/// candidates, and conclusions standing on it retire with it
struct Resolved {
    /// Assertion edges
    edges: Vec<TimedEdge>,
    spans: HashMap<Uuid, (Option<i64>, Option<i64>)>,
    meta: HashMap<Uuid, PremiseMeta>,
    axioms: HashMap<Uuid, Axioms>,
    /// Axiom rule rows: (predicate the declaration is on, kind) → rule id
    rules: HashMap<(Uuid, RuleKind), Uuid>,
    /// Rows in `facts`: edges and attribute facts. The review queue's keys and foreign keys only accept these
    asserted_ids: HashSet<Uuid>,
    /// The last check run
    checked: Checked,
    /// Rule conclusions: attribute conclusions, and the relations that stand
    wanted: HashMap<DerivedKey, Wanted>,
    /// Temporary id of a rule conclusion → the row it finally lands on. Premises on the chain refer to the former,
    /// while `fact_derivations` stores the latter (0030). The key is the derivation key, the value is the temporary id
    provisional: HashMap<DerivedKey, Uuid>,
    loaded: LoadedRules,
    capped_by_rule: HashMap<Uuid, usize>,
    rounds: usize,
    rule_rounds_capped: bool,
}

async fn resolve(conn: &mut PgConnection, kb_id: Uuid) -> AppResult<Resolved> {
    let axioms = axioms(&mut *conn, kb_id).await?;
    let rules = compile_rules(&mut *conn, kb_id, &axioms).await?;
    let (edges, mut spans, mut meta) = timed_edges(&mut *conn, kb_id).await?;
    let accepted = accepted_clashes(&mut *conn, kb_id).await?;
    let loaded = attribute_rules(&mut *conn, kb_id).await?;
    let mut asserted_ids: HashSet<Uuid> = edges.iter().map(|e| e.edge.fact).collect();

    let mut wanted: HashMap<DerivedKey, Wanted> = HashMap::new();
    let mut provisional: HashMap<DerivedKey, Uuid> = HashMap::new();
    let mut relation: Vec<RelationCandidate> = Vec::new();
    let mut capped_by_rule: HashMap<Uuid, usize> = HashMap::new();
    let mut rounds = 0usize;
    let mut checked = check(&edges, &relation, &axioms, &spans, &accepted);

    // Second pass: business rules on attribute facts (0021). Conclusions **merge into the same `wanted`**:
    // the stale reconciliation on the storage side scans the whole `derived_facts` table, and two separately made diffs would judge
    // each other's landed rows stale every round
    if !loaded.rules.is_empty() {
        let (asserted, attr_spans, attr_meta) = attribute_facts(&mut *conn, kb_id).await?;
        // A join rule that only reads Y does not require X to have an attribute; the main class scope must be read from the entity independently.
        let entity_types: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT id, type_id FROM entities
              WHERE kb_id = $1 AND merged_into IS NULL AND type_id IS NOT NULL",
        )
        .bind(kb_id)
        .fetch_all(&mut *conn)
        .await?;
        let type_of: HashMap<Uuid, Uuid> = entity_types.into_iter().collect();
        asserted_ids.extend(asserted.iter().map(|f| f.id));
        // Precision and confidence of premises: the storage segment and the fixed-point segment share them, so the two metas are merged first;
        // so are intervals: recognizing which end of a derivation was lifted by a premise's anchor depends on them
        meta.extend(attr_meta);
        spans.extend(attr_spans);
        // A key already inferred by axioms on assertion edges: when a rule infers the same relation again, it does not create another row, the same rule as
        // "already inferred the same one last round" for attribute conclusions
        let axiom_keys: HashSet<DerivedKey> = checked
            .candidates
            .facts
            .iter()
            .filter_map(|d| {
                let (from, to) = utopia_reason::derive::validity(&d.premises, &spans)?;
                Some((d.subject, d.predicate, Some(d.object), None, from, to))
            })
            .collect();
        let edge_pool: Vec<utopia_reason::rules::RuleEdge> = edges
            .iter()
            .map(|e| utopia_reason::rules::RuleEdge {
                id: e.edge.fact,
                predicate: e.edge.predicate,
                subject: e.edge.subject,
                object: e.edge.object,
            })
            .collect();
        let asserted_edges: AssertedEdges = edges
            .iter()
            .map(|e| {
                (
                    (e.edge.subject, e.edge.predicate, e.edge.object),
                    (e.from, e.to),
                )
            })
            .fold(HashMap::new(), |mut acc, (key, span)| {
                acc.entry(key).or_default().push(span);
                acc
            });

        // ---- Fixed point: this round's conclusions feed the next round's input (0030)
        //
        // **Full re-run rather than semi-naive**: chains usually have only one or two links, and convergence relies on
        // the step "this round produced no new keys"; the real cost is two or three times a single pass. Semi-naive would need to maintain which predicates each rule reads,
        // and swapping in an index for a loop of two or three rounds is not worth it.
        //
        // The input side reads no `derived_facts` row at all: feedback all lives in this in-memory pool, so one materialization
        // is still a pure function of (facts, rules, axioms): the objection 0013 raised guards exactly this
        let mut fact_pool = asserted;
        // The classes each entity was **inferred** into. Asserted classes are in `type_of`; the two enter rules differently:
        // an asserted class has no interval and acts as a sieve, whereas an inferred class has an interval and must count as a condition
        let mut derived_types: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        // The previous round added relation edges, and the pool has not been checked yet
        let mut dirty = false;
        for _ in 0..utopia_reason::MAX_DEPTH {
            rounds += 1;
            // Relation edges inferred by rules pass the axiom and contradiction checks first (0047 decision 3): the ones that stand become
            // ordinary edges from this round on; blocked ones leave the pool. A changed pool means check again, until it settles
            let mut refused_now = false;
            while dirty {
                checked = check(&edges, &relation, &axioms, &spans, &accepted);
                dirty = refuse_blocked(
                    &checked,
                    &mut relation,
                    &mut wanted,
                    &mut provisional,
                    &mut fact_pool,
                );
                refused_now |= dirty;
            }
            let mut evaluation_edges = edge_pool.clone();
            evaluation_edges.extend(
                relation
                    .iter()
                    .filter(|r| !r.refused)
                    .map(|r| r.edge.clone()),
            );
            // Finish computing everything within a round before entering it into the pool: within one round, rules read the pool as it was at the end of the previous round,
            // so who goes first does not affect the result
            let mut fresh: Vec<(usize, utopia_reason::rules::RuleHit)> = Vec::new();
            capped_by_rule.clear();
            for (ri, lr) in loaded.rules.iter().enumerate() {
                let matches_asserted_scope = |subject: &Uuid| {
                    type_of
                        .get(subject)
                        .is_some_and(|t| lr.subject_types.contains(t))
                };
                // Main class is asserted: scope is a sieve, carries no interval, and does not enter premises
                let mut by_assertion: Vec<utopia_reason::rules::AttrFact> = Vec::new();
                // Reachable only through derived classification: the scope becomes a condition (the segment below)
                let mut by_conclusion: Vec<utopia_reason::rules::AttrFact> = Vec::new();
                for f in &fact_pool {
                    if matches_asserted_scope(&f.subject) {
                        by_assertion.push(f.clone());
                    } else if derived_types
                        .get(&f.subject)
                        .is_some_and(|ts| ts.iter().any(|t| lr.subject_types.contains(t)))
                    {
                        by_conclusion.push(f.clone());
                    }
                }
                // X is enumerated from the connecting edges; filtering attributes alone cannot stop a set of conditions that all read Y.
                let assertion_edges: Vec<_> = if let Some(join_predicate) = lr.rule.join_predicate {
                    evaluation_edges
                        .iter()
                        .filter(|e| {
                            e.predicate == join_predicate && matches_asserted_scope(&e.subject)
                        })
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
                let (hits, rr) = utopia_reason::rules::evaluate_with_pool(
                    std::slice::from_ref(&lr.rule),
                    &by_assertion,
                    &fact_pool,
                    &spans,
                    &assertion_edges,
                );
                let mut capped = rr.capped;
                fresh.extend(hits.into_iter().map(|h| (ri, h)));

                // The main class is the inferred version: **the scope is written as a condition**, rather than being applied again as a sieve.
                // That classification derived fact thus enters the premises, and the interval narrows with it (an entity
                // that was B during 2019–2022 should not satisfy a rule on B with a 2024 reading),
                // and when it is voided this conclusion retires with it: no separate code is needed for this one
                if !by_conclusion.is_empty() {
                    if let (Some(is_a), false) = (loaded.is_a, lr.subject_classes.is_empty()) {
                        let scoped = scoped_by_conclusion(&lr.rule, is_a, &lr.subject_classes);
                        let (h2, rr2) = utopia_reason::rules::evaluate_with_pool(
                            std::slice::from_ref(&scoped),
                            &by_conclusion,
                            &fact_pool,
                            &spans,
                            &evaluation_edges,
                        );
                        capped += rr2.capped;
                        fresh.extend(h2.into_iter().map(|h| (ri, h)));
                    }
                }
                *capped_by_rule.entry(lr.rule.id).or_default() += capped;
            }

            let before = provisional.len();
            for (ri, h) in fresh {
                let lr = &loaded.rules[ri];
                if matches!(
                    lr.rule.conclusion,
                    utopia_reason::rules::Conclusion::Relation { .. }
                ) {
                    let Some(object) = h.object else { continue };
                    let key = (
                        h.subject,
                        lr.conclude_predicate,
                        Some(object),
                        None,
                        h.from,
                        h.to,
                    );
                    // Already inferred the same one last round (both standing and blocked count; a blocked one
                    // is not tried a second time, otherwise it would enter and leave the pool every round, and the fixed point would never be reached)
                    if wanted.contains_key(&key)
                        || axiom_keys.contains(&key)
                        || relation.iter().any(|r| r.key == key)
                    {
                        continue;
                    }
                    // The same edge is already asserted on record and the intervals overlap: asserted > derived, no separate row
                    let overlaps_assertion = asserted_edges
                        .get(&(h.subject, lr.conclude_predicate, object))
                        .is_some_and(|spans| {
                            spans.iter().any(|&(from, to)| {
                                utopia_reason::derive::overlap((h.from, h.to), (from, to)).is_some()
                            })
                        });
                    if overlaps_assertion {
                        continue;
                    }
                    let prov = Uuid::now_v7();
                    let pm = premise_meta(&h.premises, h.from, h.to, &spans, &meta);
                    spans.insert(prov, (h.from, h.to));
                    meta.insert(prov, pm);
                    relation.push(RelationCandidate {
                        edge: utopia_reason::rules::RuleEdge {
                            id: prov,
                            predicate: lr.conclude_predicate,
                            subject: h.subject,
                            object,
                        },
                        key: key.clone(),
                        premises: h.premises.clone(),
                        rule_id: lr.rule.id,
                        refused: false,
                    });
                    provisional.insert(key.clone(), prov);
                    wanted.insert(
                        key,
                        Wanted {
                            subject: h.subject,
                            predicate: lr.conclude_predicate,
                            object_id: Some(object),
                            object_value: None,
                            from: h.from,
                            to: h.to,
                            premises: h.premises,
                            rule_id: None,
                            attribute_rule_id: Some(lr.rule.id),
                            attribute_rule_version_id: lr.version,
                        },
                    );
                    dirty = true;
                    continue;
                }
                let (value, inner) = match &lr.rule.conclusion {
                    utopia_reason::rules::Conclusion::Typing { class } => (
                        serde_json::json!({ "class": class }),
                        serde_json::Value::String(class.clone()),
                    ),
                    utopia_reason::rules::Conclusion::Attribute { value, .. } => {
                        (serde_json::json!({ "value": value }), value.clone())
                    }
                    // The evaluator has already rejected failed conversions.
                    // Preserve its scalar type when this result feeds another rule.
                    utopia_reason::rules::Conclusion::Computed { .. } => {
                        let Some(ref result) = h.value else { continue };
                        let v = result.to_json();
                        (serde_json::json!({ "value": v }), v)
                    }
                    // The relation arm above stores an entity edge and returns
                    // before this value-shaping match.
                    utopia_reason::rules::Conclusion::Relation { .. } => unreachable!(),
                };
                let key = (
                    h.subject,
                    lr.conclude_predicate,
                    None,
                    value_key(Some(&value)),
                    h.from,
                    h.to,
                );
                // Already inferred the same one last round: it does not enter the pool again, so it does not enter the frontier again.
                // Self-feeding loops stop exactly here: they infer the same key
                if wanted.contains_key(&key) {
                    continue;
                }
                // Temporary id: this one is now used as a fact, but which row it lands on is known only after reconciliation
                // (an unchanged conclusion keeps its original row). Swapped over before storage
                let prov = Uuid::now_v7();
                let pm = premise_meta(&h.premises, h.from, h.to, &spans, &meta);
                spans.insert(prov, (h.from, h.to));
                meta.insert(prov, pm);
                fact_pool.push(utopia_reason::rules::AttrFact {
                    id: prov,
                    subject: h.subject,
                    predicate: lr.conclude_predicate,
                    value: inner,
                });
                if let utopia_reason::rules::Conclusion::Typing { class } = &lr.rule.conclusion {
                    if let Some(&t) = loaded.class_ids.get(class) {
                        derived_types.entry(h.subject).or_default().push(t);
                    }
                }
                provisional.insert(key.clone(), prov);
                wanted.insert(
                    key,
                    Wanted {
                        subject: h.subject,
                        predicate: lr.conclude_predicate,
                        object_id: None,
                        object_value: Some(value),
                        from: h.from,
                        to: h.to,
                        premises: h.premises,
                        rule_id: None,
                        attribute_rule_id: Some(lr.rule.id),
                        attribute_rule_version_id: lr.version,
                    },
                );
            }
            // Nothing new was inferred this round and nothing exited: the fixed point is reached
            if provisional.len() == before && !refused_now {
                break;
            }
        }
        // The relation edges added by the last round have not yet been checked: check until it settles, just no longer running rules
        while dirty {
            checked = check(&edges, &relation, &axioms, &spans, &accepted);
            dirty = refuse_blocked(
                &checked,
                &mut relation,
                &mut wanted,
                &mut provisional,
                &mut fact_pool,
            );
        }
    }

    Ok(Resolved {
        edges,
        spans,
        meta,
        axioms,
        rules,
        asserted_ids,
        checked,
        wanted,
        provisional,
        loaded,
        capped_by_rule,
        rounds,
        rule_rounds_capped: rounds == utopia_reason::MAX_DEPTH,
    })
}

pub async fn materialize(pool: &PgPool, kb_id: Uuid) -> AppResult<DeriveReport> {
    let Resolved {
        edges,
        spans,
        meta,
        rules,
        checked,
        mut wanted,
        provisional,
        loaded,
        capped_by_rule,
        rounds,
        rule_rounds_capped,
        ..
    } = resolve(&mut *pool.acquire().await?, kb_id).await?;
    let Checked {
        candidates,
        axiom_count,
        blocked,
        ..
    } = checked;

    // Standing relation conclusions are already in `provisional`; the candidates outside them are the blocked ones
    let live_relations = wanted
        .values()
        .filter(|w| w.attribute_rule_id.is_some() && w.object_id.is_some())
        .count();
    let mut report = DeriveReport {
        rules: rules.len(),
        edges: edges.len(),
        // Number of derivations the engine inferred: the axiom derivations and relation candidates on the last round's pool (blocked ones were also inferred),
        // plus the number of **distinct** rule conclusions: in the fixed point the same conclusion is recomputed every round,
        // so accumulating by count would make it a function of the number of rounds
        derived: candidates.facts.len() + provisional.len() - live_relations,
        capped: candidates.capped.len(),
        blocked: blocked.len(),
        attribute_rules: loaded.rules.len(),
        rule_rounds: rounds,
        // Still producing output after running all the rounds: the chain is longer than MAX_DEPTH, so the rest did not connect. **Must be reported**:
        // "not reached" and "not satisfied" look exactly the same in the results (the combination cap is the same point)
        rule_rounds_capped,
        rule_hits: provisional.len(),
        ..Default::default()
    };

    // Axiom derivations merge into the same `wanted`. **Merging is required**: the reconciliation for stale rows scans the whole table,
    // and two separately made diffs would judge each other's rows stale and void them every round. Takes the last round's pool's
    // version: derivations standing on exited relation edges are no longer in it
    for (i, d) in candidates.facts.iter().take(axiom_count).enumerate() {
        if blocked.contains(&i) {
            continue;
        }
        let Some((from, to)) = utopia_reason::derive::validity(&d.premises, &spans) else {
            continue;
        };
        // **Look up by `via`, not `predicate`.** Rule rows are made for "the predicate that declared the
        // axiom"; in a rule across two predicates, the derived predicate is another one
        let Some(&rule_id) = rules.get(&(d.via, d.rule.as_str())) else {
            // Not finding a rule means **compilation and inference disagree**, not a normal situation. Count it,
            // do not let it disappear silently once more
            report.unruled += 1;
            continue;
        };
        // The same relation inferred by a rule arrived first: that row lands with the rule's proof, and the axiom version does not create another
        wanted
            .entry((d.subject, d.predicate, Some(d.object), None, from, to))
            .or_insert(Wanted {
                subject: d.subject,
                predicate: d.predicate,
                object_id: Some(d.object),
                object_value: None,
                from,
                to,
                premises: d.premises.clone(),
                rule_id: Some(rule_id),
                attribute_rule_id: None,
                attribute_rule_version_id: None,
            });
    }
    for lr in &loaded.rules {
        // Combination counts that cannot be fully expanded are written back to the rule: this number lives in the table permanently, not only flashing
        // in the hint at the moment the run finished. Take the last round's number: that round scanned the most complete pool
        let capped = capped_by_rule.get(&lr.rule.id).copied().unwrap_or(0);
        sqlx::query("UPDATE attribute_rules SET capped_at_last_run = $2 WHERE id = $1")
            .bind(lr.rule.id)
            .bind(capped as i32)
            .execute(pool)
            .await?;
        report.rule_capped += capped;
    }

    let mut tx = pool.begin().await?;
    let live: Vec<LiveRow> = sqlx::query_as(
        "SELECT id, subject_id, predicate_id, object_id, object_value, valid_from, valid_to
           FROM derived_facts
          WHERE kb_id = $1 AND invalidated_at IS NULL",
    )
    .bind(kb_id)
    .fetch_all(&mut *tx)
    .await?;

    let mut stale: Vec<Uuid> = Vec::new();
    // The row each still-holding conclusion finally lands on. **An unchanged conclusion keeps its original row**, so the premise on the chain
    // that points to it must point to this id, not one newly created in this round (0030)
    let mut settled: HashMap<DerivedKey, Uuid> = HashMap::new();
    // Those that still hold this round and whose rows are kept, together with **this round's** premises. The reconciliation key has no
    // premises, so "conclusion unchanged, reason changed" is invisible here: it is checked separately below
    let mut kept: Vec<(Uuid, Wanted)> = Vec::new();
    for (id, s, p, o, ov, from, to) in &live {
        let key = (
            *s,
            *p,
            *o,
            value_key(ov.as_ref()),
            from.map(|x| x.timestamp()),
            to.map(|x| x.timestamp()),
        );
        if let Some(d) = wanted.remove(&key) {
            settled.insert(key, *id);
            kept.push((*id, d));
        } else {
            stale.push(*id);
        }
    }

    // Premise gone, so the derivation is invalidated. **Set invalidated_at rather than delete**: exactly isomorphic to rejecting an assertion,
    // the record axis keeps "we once inferred this from it, and later the premise went away", which the entity history page
    // can display directly (0002 section 3)
    if !stale.is_empty() {
        sqlx::query("UPDATE derived_facts SET invalidated_at = now() WHERE id = ANY($1)")
            .bind(&stale)
            .execute(&mut *tx)
            .await?;
        report.invalidated = stale.len();
    }

    // The rest are new rows. **Assign all ids first, then insert**: the row a premise on the chain points to
    // may be another not yet inserted in this batch (0030)
    let fresh: Vec<(Uuid, Wanted)> = wanted
        .into_iter()
        .map(|(key, d)| {
            let id = Uuid::now_v7();
            settled.insert(key, id);
            (id, d)
        })
        .collect();
    // Temporary id → real row id. Premises on the chain use temporary ids in the fixed point, and storage must swap them over;
    // assertion premises are not in this table and pass through unchanged
    let resolve: HashMap<Uuid, Uuid> = provisional
        .iter()
        .filter_map(|(key, prov)| settled.get(key).map(|id| (*prov, *id)))
        .collect();

    for (id, d) in &fresh {
        // The constraint is "has precision means has a date" (0022 relaxed the reverse): when an intersection makes one end unbounded, that end's
        // precision is cleared; if that end comes from an evidence date rather than the text's date, it has no precision either: filling
        // in a definite value where one is unknown is the very disease the `facts.valid_from_precision` comment describes
        let (fp, tp, conf, from_anchored, to_anchored) =
            premise_meta(&d.premises, d.from, d.to, &spans, &meta);
        let fp = if from_anchored { None } else { d.from.and(fp) };
        let tp = if to_anchored { None } else { d.to.and(tp) };
        sqlx::query(
            "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_id,
                                        object_value, valid_from, valid_to,
                                        valid_from_precision, valid_to_precision,
                                        confidence, rule_id, attribute_rule_id,
                                        attribute_rule_version_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(id)
        .bind(kb_id)
        .bind(d.subject)
        .bind(d.predicate)
        .bind(d.object_id)
        .bind(&d.object_value)
        .bind(d.from.map(stamp))
        .bind(d.to.map(stamp))
        .bind(&fp)
        .bind(&tp)
        .bind(conf)
        .bind(d.rule_id)
        .bind(d.attribute_rule_id)
        .bind(d.attribute_rule_version_id)
        .execute(&mut *tx)
        .await?;
        report.inserted += 1;
    }

    // Conclusion unchanged, definition changed: this row now holds under the rule's new version (0060). The row is kept:
    // the reconciliation key has no version, and the conclusion itself has not changed; once the version is swapped, the proof can say what it now rests on
    for (id, d) in &kept {
        let Some(version) = d.attribute_rule_version_id else {
            continue;
        };
        let res = sqlx::query(
            "UPDATE derived_facts SET attribute_rule_version_id = $2
              WHERE id = $1 AND attribute_rule_version_id IS DISTINCT FROM $2",
        )
        .bind(id)
        .bind(version)
        .execute(&mut *tx)
        .await?;
        report.redefined += res.rows_affected() as usize;
    }

    // Conclusion unchanged, reason changed: **the same statement can have a second basis** (a different reading was used, or a different
    // set of conditions). The reconciliation key is subject-predicate-object plus interval, and the premises are not in it, so that row would keep
    // last round's proof. Chains make this more likely: a premise on it may have just been voided.
    // Look them all up in one go, then compare one by one; only those that really differ are rewritten
    let kept_ids: Vec<Uuid> = kept.iter().map(|(id, _)| *id).collect();
    let mut stored: HashMap<Uuid, Vec<PremiseCols>> = HashMap::new();
    if !kept_ids.is_empty() {
        let rows: Vec<(Uuid, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
            "SELECT derived_fact_id, premise_fact_id, premise_derived_id
               FROM fact_derivations WHERE derived_fact_id = ANY($1)
              ORDER BY derived_fact_id, seq",
        )
        .bind(&kept_ids)
        .fetch_all(&mut *tx)
        .await?;
        for (d, f, dv) in rows {
            stored.entry(d).or_default().push((f, dv));
        }
    }
    let premise_cols = |d: &Wanted| -> Vec<PremiseCols> {
        d.premises
            .iter()
            .map(|p| match resolve.get(p) {
                Some(x) => (None, Some(*x)),
                None => (Some(*p), None),
            })
            .collect()
    };
    let mut reproved: Vec<(Uuid, Vec<PremiseCols>)> = Vec::new();
    for (id, d) in &kept {
        let want = premise_cols(d);
        if stored.get(id).map(|s| s.as_slice()) == Some(want.as_slice()) {
            continue;
        }
        reproved.push((*id, want));
    }
    for (id, want) in &reproved {
        sqlx::query("DELETE FROM fact_derivations WHERE derived_fact_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for (seq, (f, d)) in want.iter().enumerate() {
            sqlx::query(
                "INSERT INTO fact_derivations (derived_fact_id, premise_fact_id,
                                               premise_derived_id, seq)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(f)
            .bind(d)
            .bind(seq as i32)
            .execute(&mut *tx)
            .await?;
        }
    }
    report.reproved = reproved.len();

    // Proofs are **written after all derivations are inserted**: a premise on the chain may point at another row in this batch,
    // and this batch has no order (`wanted` is a HashMap). If written in one pass, the row that gets its turn first
    // would point at a row that does not exist yet, and the foreign key would reject it on the spot
    for (id, d) in &fresh {
        for (seq, premise) in d.premises.iter().enumerate() {
            // A premise is either an assertion or another derivation: the two columns are one-of-two, `seq` is a sequence across both kinds,
            // so the proof reads as one path downward (0030)
            let derived_premise = resolve.get(premise).copied();
            sqlx::query(
                "INSERT INTO fact_derivations (derived_fact_id, premise_fact_id,
                                               premise_derived_id, seq)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(derived_premise.is_none().then_some(*premise))
            .bind(derived_premise)
            .bind(seq as i32)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(report)
}

fn stamp(secs: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default()
}

/// Ontology defects the Review page shows, together with their labels.
///
/// Labels are fetched in SQL rather than afterward: the `subject` column means one of two tables (a predicate or a class),
/// separate queries would first group by kind and then send two batches, whereas one LEFT JOIN across the two tables is enough:
/// an id can only hit one of them.
pub async fn open_defects(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<OntologyDefect>> {
    Ok(sqlx::query_as(
        "SELECT d.id, d.kind, d.detail,
                COALESCE(st.label, sr.label) AS subject_label,
                COALESCE(ot.label, orr.label) AS other_label,
                COALESCE(
                    (SELECT array_agg(t.label ORDER BY x.ord)
                       FROM unnest(d.path) WITH ORDINALITY AS x(id, ord)
                       JOIN entity_types t ON t.id = x.id),
                    ARRAY[]::text[]
                ) AS path_labels,
                d.detected_at
           FROM ontology_defects d
           LEFT JOIN entity_types   st ON st.id = d.subject
           LEFT JOIN relation_types sr ON sr.id = d.subject
           LEFT JOIN entity_types   ot ON ot.id = d.other
           LEFT JOIN relation_types orr ON orr.id = d.other
          WHERE d.kb_id = $1 AND d.status = 'open'
          ORDER BY d.detected_at DESC, d.id DESC
          LIMIT $2 OFFSET $3",
    )
    .bind(kb_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

/// A person rules on one ontology defect.
///
/// Two ways out, not three: an ontology defect has no "the data is wrong" option, since it never looked at data.
/// `fixed` means "I changed the ontology", `accepted` means "looked at it, no need to change".
pub async fn decide_defect(
    pool: &PgPool,
    kb_id: Uuid,
    defect_id: Uuid,
    resolution: &str,
    actor: Uuid,
) -> AppResult<()> {
    let res = sqlx::query(
        "UPDATE ontology_defects
            SET status = 'resolved', resolution = $3, decided_by = $4, decided_at = now()
          WHERE id = $2 AND kb_id = $1 AND status = 'open'",
    )
    .bind(kb_id)
    .bind(defect_id)
    .bind(resolution)
    .bind(actor)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(utopia_core::AppError::NotFound);
    }
    Ok(())
}

/// Libraries due for re-inference now.
///
/// Same shape as source sync: an interval plus a last-time. **Never-inferred counts as due**:
/// a library that just had the toggle turned on should not wait a full period for its first inference.
pub async fn due_for_inference(pool: &PgPool) -> AppResult<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM knowledge_bases
          WHERE materialize_inferences
            AND (last_inference_at IS NULL
                 OR last_inference_at < now()
                    - make_interval(mins => inference_interval_minutes))",
    )
    .fetch_all(pool)
    .await?)
}

/// One tick of the periodic scan: each due library queues one inference task; if one is already queued, it is not queued again.
///
/// Skipping only the queued ones is enough (#1304): the handler calls `mark_inference_ran` before inferring, so once the task starts running for this library it
/// is no longer due; the only still-due window is when the task is queued and `last_inference_at` has not moved yet, and that window is exactly what this blocks.
/// A library that cannot be queued does not block other libraries
pub async fn queue_due_inferences(pool: &PgPool) -> AppResult<()> {
    for kb_id in due_for_inference(pool).await? {
        if let Err(e) = crate::jobs::enqueue_unless_queued(
            pool,
            "materialize_inferences",
            json!({ "kb_id": kb_id }),
        )
        .await
        {
            tracing::warn!(kb_id = %kb_id, error = %e, "failed to enqueue reasoning task");
        }
    }
    Ok(())
}

/// Record the time this round of inference finished.
///
/// **Record it even if nothing changed**: this column answers "was it checked last time", not "did it change last time".
/// Without recording, unchanged libraries would be scanned and recomputed every minute.
pub async fn mark_inference_ran(pool: &PgPool, kb_id: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE knowledge_bases SET last_inference_at = now() WHERE id = $1")
        .bind(kb_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The proof of one derived fact, expanded down to the original sentences (0002 R2).
///
/// `fact_derivations` records only the direct premises; asking down layer by layer gives the full proof. A
/// premise is either an assertion (that step's leaf is its original sentence) or **another derivation** (0030),
/// in which case that step asks again what it rests on. **Retracted premises are still listed and marked**: a derivation
/// is invalidated along with its premises, but "what it relied on at the time" must still be readable, which is exactly why the record axis exists.
///
/// Returns None when the derivation is invalidated or does not exist: not an error, the UI collapses on that basis.
pub async fn proof(
    pool: &PgPool,
    kb_id: Uuid,
    derived_id: Uuid,
) -> AppResult<Option<utopia_core::models::Proof>> {
    let Some(derived) = derived_one(pool, kb_id, derived_id).await? else {
        return Ok(None);
    };
    let steps = premise_steps(pool, kb_id, derived_id, 0).await?;
    Ok(Some(utopia_core::models::Proof { derived, steps }))
}

/// One step of a derivation's direct premises; the derivation steps further expand one more layer (0030).
///
/// The depth cap is the same `MAX_DEPTH` as inference: the chain is at most this long, so the proof is at most this deep.
/// When it reaches the bottom, it stops at that step: its own triple is still listed, it just does not ask further.
fn premise_steps<'a>(
    pool: &'a PgPool,
    kb_id: Uuid,
    derived_id: Uuid,
    depth: usize,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = AppResult<Vec<utopia_core::models::ProofStep>>>
            + Send
            + 'a,
    >,
> {
    Box::pin(async move {
        let rows: Vec<(i32, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
            "SELECT seq, premise_fact_id, premise_derived_id
               FROM fact_derivations WHERE derived_fact_id = $1 ORDER BY seq",
        )
        .bind(derived_id)
        .fetch_all(pool)
        .await?;
        // Assertion steps are all fetched at once (each step also needs its own evidence fetch); derivation steps are asked one by one
        let asserted: Vec<Uuid> = rows.iter().filter_map(|(_, f, _)| *f).collect();
        let mut by_fact: HashMap<Uuid, utopia_core::models::ProofStep> =
            steps_for(pool, kb_id, &asserted)
                .await?
                .into_iter()
                .map(|s| (s.fact_id, s))
                .collect();

        let mut steps = Vec::with_capacity(rows.len());
        for (seq, fact, derived) in rows {
            if let Some(f) = fact {
                let Some(mut step) = by_fact.remove(&f) else {
                    continue;
                };
                step.seq = seq;
                steps.push(step);
            } else if let Some(d) = derived {
                let Some(mut step) = derived_step(pool, d).await? else {
                    continue;
                };
                step.seq = seq;
                if depth + 1 < utopia_reason::MAX_DEPTH {
                    step.premises = premise_steps(pool, kb_id, d, depth + 1).await?;
                }
                steps.push(step);
            }
        }
        Ok(steps)
    })
}

/// One step of a proof: a derived premise read out. Its triple and interval look like an assertion step, since the reader
/// cares about "is this sentence true", not which table it came from; the `derived` cell answers the latter,
/// and is also the basis for "can I click one level further".
async fn derived_step(
    pool: &PgPool,
    id: Uuid,
) -> AppResult<Option<utopia_core::models::ProofStep>> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        Uuid,
        Uuid,
        String,
        Option<Uuid>,
        Option<String>,
        Option<Uuid>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
        f32,
        bool,
    )> = sqlx::query_as(
        "SELECT d.id, d.subject_id, s.canonical_name,
                d.predicate_id, r.label, d.object_id,
                COALESCE(o.canonical_name, ct.label,
                         d.object_value ->> 'class',
                         d.object_value #>> '{value}'),
                d.valid_from, d.valid_to, d.confidence,
                d.invalidated_at IS NOT NULL
           FROM derived_facts d
           JOIN entities s ON s.id = d.subject_id
           LEFT JOIN relation_types r ON r.id = d.predicate_id
           LEFT JOIN entities o ON o.id = d.object_id
           LEFT JOIN attribute_rules ar ON ar.id = d.attribute_rule_id
           LEFT JOIN entity_types ct ON ct.id = ar.conclude_type_id
          WHERE d.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(
        |(
            fact_id,
            subject_id,
            subject,
            predicate_id,
            predicate,
            object_id,
            object,
            valid_from,
            valid_to,
            confidence,
            retracted,
        )| utopia_core::models::ProofStep {
            seq: 0,
            fact_id,
            derived: true,
            subject_id,
            subject,
            predicate_id,
            predicate,
            object_id,
            object,
            valid_from,
            valid_to,
            confidence,
            retracted,
            // A derivation has no original sentence: its "evidence" is the premise layer below it
            evidence: Vec::new(),
            premises: Vec::new(),
        },
    ))
}

/// A chain of premises expanded into proof steps: triple, interval, retracted or not, evidence.
///
/// Both derivations that landed (`fact_derivations`) and those that did not (`axiom_violations.path`) go through here:
/// premises are the same kind of thing, so there is no reason for the proof chain to have two shapes
async fn steps_for(
    pool: &PgPool,
    kb_id: Uuid,
    premises: &[Uuid],
) -> AppResult<Vec<utopia_core::models::ProofStep>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        Uuid,
        Uuid,
        String,
        Option<Uuid>,
        Option<String>,
        Option<Uuid>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
        f32,
        bool,
    )> = sqlx::query_as(
        "SELECT x.ord - 1, f.id, f.subject_id, s.canonical_name,
                f.predicate_id, r.label, f.object_id, o.canonical_name,
                f.valid_from, f.valid_to, f.confidence,
                f.invalidated_at IS NOT NULL
           FROM unnest($1::uuid[]) WITH ORDINALITY AS x(id, ord)
           JOIN facts f ON f.id = x.id
           JOIN entities s ON s.id = f.subject_id
           LEFT JOIN relation_types r ON r.id = f.predicate_id
           LEFT JOIN entities o ON o.id = f.object_id
          ORDER BY x.ord",
    )
    .bind(premises)
    .fetch_all(pool)
    .await?;
    let mut steps = Vec::with_capacity(rows.len());
    for (
        seq,
        fact_id,
        subject_id,
        subject,
        predicate_id,
        predicate,
        object_id,
        object,
        valid_from,
        valid_to,
        confidence,
        retracted,
    ) in rows
    {
        // A chain has at most MAX_DEPTH steps, so fetching evidence one by one is a countable number of round trips
        let evidence = crate::graph::fact_evidence(pool, kb_id, fact_id).await?;
        steps.push(utopia_core::models::ProofStep {
            seq: seq as i32,
            fact_id,
            derived: false,
            premises: Vec::new(),
            subject_id,
            subject,
            predicate_id,
            predicate,
            object_id,
            object,
            valid_from,
            valid_to,
            confidence,
            retracted,
            evidence,
        });
    }
    Ok(steps)
}

/// Among derivations that did not land, those relevant to this entity and **still open at the time** (0017 §3): the panel's
/// "not landed" subsection under the "inferred" tier.
///
/// The `blocked` tier on the panel uses the same `as_of` as the other keys (`#549` wired the `derived`
/// tier, `#307` connected the rest): a violation overturned in March should not leave a ghost edge on the panel when replaying. `v.status = 'open'`
/// does not tell apart "open now" from "open in March but closed by a person in April".
/// `violation_open_at` is the answer.
///
/// The write path does not go through here (`decide_violation` etc.).
pub async fn blocked_for_entity(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<utopia_core::models::BlockedDerivation>> {
    let violation_open = crate::record_axis::violation_open_at("v", as_of.map(|_| 3));
    Ok(sqlx::query_as(&format!(
        "SELECT v.id AS violation_id,
                (v.detail->>'subject_id')::uuid AS subject_id,
                COALESCE(v.detail->>'subject', '?') AS subject,
                (v.detail->>'object_id')::uuid AS object_id,
                COALESCE(v.detail->>'object', '?') AS object,
                COALESCE(v.detail->>'predicate', '?') AS predicate,
                COALESCE(v.detail->>'rule', '?') AS rule,
                COALESCE(v.detail->>'via_label', '?') AS via_label,
                (v.detail->>'valid_from')::timestamptz AS valid_from,
                (v.detail->>'valid_to')::timestamptz AS valid_to,
                v.left_fact AS against_fact,
                s.canonical_name || ' · '
                  || COALESCE(r.label, fact_surface_predicate(f.id), '?') || ' · '
                  || COALESCE(o.canonical_name, '?') AS against_text,
                v.path AS premises
           FROM axiom_violations v
           JOIN facts f ON f.id = v.left_fact
           JOIN entities s ON s.id = f.subject_id
           LEFT JOIN relation_types r ON r.id = f.predicate_id
           LEFT JOIN entities o ON o.id = f.object_id
          WHERE v.kb_id = $1 AND v.kind = 'derived_contradiction' AND {violation_open}
            AND (v.detail->>'subject_id' = $2::text OR v.detail->>'object_id' = $2::text)
          ORDER BY v.detected_at DESC",
    ))
    .bind(kb_id)
    .bind(entity_id)
    .bind(as_of)
    .fetch_all(pool)
    .await?)
}

/// The proof chain of a derivation that did not land: its premises are right in the violation's `path`. `None` if that violation is not found
pub async fn blocked_proof(
    pool: &PgPool,
    kb_id: Uuid,
    violation_id: Uuid,
) -> AppResult<Option<Vec<utopia_core::models::ProofStep>>> {
    let path: Option<(Vec<Uuid>,)> = sqlx::query_as(
        "SELECT path FROM axiom_violations
          WHERE id = $1 AND kb_id = $2 AND kind = 'derived_contradiction'",
    )
    .bind(violation_id)
    .bind(kb_id)
    .fetch_optional(pool)
    .await?;
    match path {
        None => Ok(None),
        Some((p,)) => Ok(Some(steps_for(pool, kb_id, &p).await?)),
    }
}

/// Fetch one derivation by id (invalidated ones too: the proof must be viewable historically).
/// The axiom name takes the predicate that declares it, not the conclusion predicate; a retracted axiom must still be able to explain historical facts.
async fn derived_one(
    pool: &PgPool,
    kb_id: Uuid,
    derived_id: Uuid,
) -> AppResult<Option<DerivedFactView>> {
    Ok(sqlx::query_as(
        "SELECT d.id, d.predicate_id, d.object_value, d.rule_id, d.attribute_rule_id,
                d.invalidated_at, d.valid_from_precision, d.valid_to_precision,
                d.subject_id, s.canonical_name AS subject,
                d.object_id,
                COALESCE(o.canonical_name, ct.label,
                         d.object_value ->> 'class',
                         d.object_value #>> '{value}',
                         d.object_value #>> '{}') AS object,
                r.label AS predicate,
                COALESCE(ru.kind, 'business') AS rule,
                COALESCE(ar.name, ru.kind || ' of ' || rp.key) AS rule_name,
                v.seq AS rule_version, v.definition AS rule_definition,
                d.valid_from, d.valid_to, d.confidence, d.derived_at,
                COALESCE(
                    (SELECT array_agg(
                                ps.canonical_name || ' · '
                                || COALESCE(pr.label, '?') || ' · '
                                || COALESCE(po.canonical_name,
                                            fd.object_value #>> '{value}',
                                            fd.object_value #>> '{class}',
                                            '?')
                                ORDER BY fd.seq)
                       FROM derivation_premises fd
                       JOIN entities ps    ON ps.id = fd.subject_id
                       LEFT JOIN relation_types pr ON pr.id = fd.predicate_id
                       LEFT JOIN entities po ON po.id = fd.object_id
                      WHERE fd.derived_fact_id = d.id),
                    ARRAY[]::text[]
                ) AS premises
           FROM derived_facts d
           JOIN entities s ON s.id = d.subject_id
           LEFT JOIN entities o ON o.id = d.object_id
           JOIN relation_types r ON r.id = d.predicate_id
           LEFT JOIN rules ru ON ru.id = d.rule_id
           LEFT JOIN relation_types rp ON rp.id = ru.predicate_id
                AND rp.kb_id = d.kb_id AND ru.kb_id = d.kb_id
           LEFT JOIN attribute_rules ar ON ar.id = d.attribute_rule_id
           LEFT JOIN attribute_rule_versions v ON v.id = d.attribute_rule_version_id
           LEFT JOIN entity_types ct ON ct.id = ar.conclude_type_id
          WHERE d.kb_id = $1 AND d.id = $2",
    )
    .bind(kb_id)
    .bind(derived_id)
    .fetch_optional(pool)
    .await?)
}

/// A derived fact, with the text needed for display and proof (the "inferred" tier of the entity panel).
///
/// **The proof is fetched along with it**: the reason this tier exists is "this edge was not said by anyone, it was inferred this way",
/// and without showing the premises it would look no different from an ordinary edge, exactly the contamination users worry about.
pub async fn derived_for_entity(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    at: Option<chrono::DateTime<chrono::Utc>>,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<DerivedFactView>> {
    // Record axis (0019 / #549): the assertion half always went through `held_at`; this half had been hardcoded to
    // `invalidated_at IS NULL`, so a panel replayed to March held conclusions inferred only in April:
    // none of the premises exist, yet the conclusion does. The predicate is composed only in record_axis; it is not written here.
    //
    // **Both the object and the rule sides are LEFT JOIN.** After the table was widened (0021), a derivation's object may
    // be a literal value rather than an entity, and the rule may be a business rule rather than an axiom: an inner join would
    // **silently** erase both kinds of conclusion from the panel, and they are exactly the kind most in need of explanation.
    //
    // The object's display text thus has three sources: the entity name, the class label in a classification conclusion, and the value of an attribute conclusion.
    Ok(sqlx::query_as(&format!(
        "SELECT d.id, d.predicate_id, d.object_value, d.rule_id, d.attribute_rule_id,
                d.invalidated_at, d.valid_from_precision, d.valid_to_precision,
                d.subject_id, s.canonical_name AS subject,
                d.object_id,
                COALESCE(o.canonical_name,
                         ct.label,
                         d.object_value ->> 'class',
                         d.object_value #>> '{{value}}',
                         d.object_value #>> '{{}}') AS object,
                r.label AS predicate,
                COALESCE(ru.kind, 'business') AS rule,
                COALESCE(ar.name, ru.kind || ' of ' || rp.key) AS rule_name,
                v.seq AS rule_version, v.definition AS rule_definition,
                d.valid_from, d.valid_to, d.confidence, d.derived_at,
                COALESCE(
                    (SELECT array_agg(
                                ps.canonical_name || ' · '
                                || COALESCE(pr.label, '?') || ' · '
                                || COALESCE(po.canonical_name,
                                            fd.object_value #>> '{{value}}',
                                            fd.object_value #>> '{{class}}',
                                            '?')
                                ORDER BY fd.seq)
                       FROM derivation_premises fd
                       JOIN entities ps    ON ps.id = fd.subject_id
                       LEFT JOIN relation_types pr ON pr.id = fd.predicate_id
                       LEFT JOIN entities po ON po.id = fd.object_id
                      WHERE fd.derived_fact_id = d.id),
                    ARRAY[]::text[]
                ) AS premises
           FROM derived_facts d
           JOIN entities s ON s.id = d.subject_id
           LEFT JOIN entities o ON o.id = d.object_id
           JOIN relation_types r ON r.id = d.predicate_id
           LEFT JOIN rules ru ON ru.id = d.rule_id
           LEFT JOIN relation_types rp ON rp.id = ru.predicate_id
                AND rp.kb_id = d.kb_id AND ru.kb_id = d.kb_id
           LEFT JOIN attribute_rules ar ON ar.id = d.attribute_rule_id
           LEFT JOIN attribute_rule_versions v ON v.id = d.attribute_rule_version_id
           LEFT JOIN entity_types ct ON ct.id = ar.conclude_type_id
          WHERE d.kb_id = $1 AND {derived_held}
            AND (d.subject_id = $2 OR d.object_id = $2)
            AND {derived_hold}
          ORDER BY d.derived_at DESC",
        derived_hold = crate::world_axis::derived_hold_at("d", 3),
        derived_held = crate::record_axis::derived_held_at("d", as_of.map(|_| 4)),
    ))
    .bind(kb_id)
    .bind(entity_id)
    .bind(at)
    .bind(as_of)
    .fetch_all(pool)
    .await?)
}
