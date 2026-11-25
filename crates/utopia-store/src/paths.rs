//! Paths between two points (#558): `paths_between(a, b)`: how A and B are connected.
//!
//! "The OpenAI and Anthropic relationship" is a graph question, and the graph tools used to only find an entity by name and
//! dump all facts about one entity. To answer it, the model had to dump OpenAI's 555 facts and Anthropic's
//! all into context and wire them up itself; in sixteen real runs, three connected, and three times it searched six or seven rounds before declaring "nothing in the database".
//! The database has it, two hops: OpenAI -employee-> Dario Amodei -founder-> Anthropic.
//!
//! **Returns chains, not node sets.** One-hop neighborhood expansion already exists (`graph::neighborhood`, for the canvas),
//! and using it to answer relationship questions would mean dumping neighbors instead of facts. Each result here is an edge sequence from a to b,
//! shortest first, and the more obscure the nodes along the way, the earlier it ranks.
//!
//! **Both time axes must hold.** With `at`, **every edge** on the path must hold at that moment: in 2019 there was
//! no Anthropic between OpenAI and Anthropic yet; with `as_of`, follow the records as they stood then, including
//! who owned entities not yet merged at the time (`record_axis::owner_at`). This is half of what a plain graph path query lacks,
//! and it is the half this database should expose.
//!
//! **Search meets in the middle.** A path of up to three hops a-x-y-b can be assembled from a's two-hop adjacency and b's one-hop
//! neighbors, without spreading out a's third layer: the third layer is tens of thousands of edges for a node like OpenAI.
//! Hubs (intermediate nodes with degree above `hub_degree`) are not expanded: paths between two points that go through Sam Altman are
//! everywhere and prove nothing; the endpoints themselves are exempt, and the search walks out from whichever one is the start.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use utopia_core::AppResult;
use uuid::Uuid;

use crate::{record_axis, world_axis};

/// Search bounds. Tests shrink `hub_degree` to verify hubs are not crossed; the product uses the defaults.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Maximum hops (1..=3; longer paths are everywhere between any two points and prove nothing about the relationship)
    pub max_hops: usize,
    /// Maximum number to return
    pub max_paths: usize,
    /// Degree cap for intermediate nodes: above it, do not expand (endpoints exempt)
    pub hub_degree: i64,
    /// Maximum edges to read for the second-layer expansion: take intermediate nodes by ascending degree, stop when the budget is read
    pub max_edges: i64,
    /// Maximum candidates to collect before sorting: multiple edges between the same pair of nodes would blow up the combinatorics
    pub max_candidates: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_hops: 3,
            max_paths: 10,
            hub_degree: 150,
            max_edges: 20_000,
            max_candidates: 400,
        }
    }
}

/// An edge on the path, with the names of its two ends and its own interval; the caller judges the direction from the traversal
/// (`subject_id` equal to the previous node means it is walked forward)
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PathEdge {
    pub fact_id: Uuid,
    pub subject_id: Uuid,
    pub subject_name: String,
    pub object_id: Uuid,
    pub object_name: String,
    pub predicate: Option<String>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_from_precision: Option<String>,
    pub valid_to: Option<DateTime<Utc>>,
    pub valid_to_precision: Option<String>,
    pub holds_from: Option<DateTime<Utc>>,
    pub holds_to: Option<DateTime<Utc>>,
    pub confidence: f32,
    /// The end is derived from the timeline; the interval was edited by a person, and which end was edited (#970, same as `EntityFact`)
    pub end_derived: bool,
    pub time_corrected: bool,
    pub corrected_ends: Option<String>,
    /// The row the timeline connected when it closed this, the end it took over, and the correction note (#970 step 2, same as `EntityFact`)
    pub closed_by_id: Option<Uuid>,
    pub closed_by: Option<String>,
    pub closed_by_value: Option<serde_json::Value>,
    pub correction_note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Path {
    /// Node sequence from a to b, including both ends
    pub nodes: Vec<Uuid>,
    /// Edge between `nodes[i]` and `nodes[i+1]`
    pub edges: Vec<PathEdge>,
    /// Sum of ln(1+d) over intermediate nodes' degrees; smaller means more specific; endpoints not counted
    pub specificity: f64,
}

impl Path {
    pub fn hops(&self) -> usize {
        self.edges.len()
    }
}

/// The two entities at one edge (already converted to their ownership at that time via `as_of`)
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
struct Touch {
    id: Uuid,
    subject_id: Uuid,
    object_id: Uuid,
}

/// Candidate: fact id sequence and node sequence, collected before sorting
struct Candidate {
    nodes: Vec<Uuid>,
    facts: Vec<Uuid>,
}

pub async fn paths_between(
    pool: &PgPool,
    kb_id: Uuid,
    a: Uuid,
    b: Uuid,
    at: Option<DateTime<Utc>>,
    as_of: Option<DateTime<Utc>>,
    limits: Limits,
) -> AppResult<Vec<Path>> {
    let max_hops = limits.max_hops.clamp(1, 3);
    if a == b {
        return Ok(Vec::new());
    }

    // a's one hop: neighbor -> the edge connecting to it
    let mut a_edges: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for t in touching(pool, kb_id, &[a], at, as_of).await? {
        let other = if t.subject_id == a {
            t.object_id
        } else {
            t.subject_id
        };
        a_edges.entry(other).or_default().push(t.id);
    }

    // b's one hop (only needed for paths of two or more hops)
    let mut b_edges: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    if max_hops >= 2 {
        for t in touching(pool, kb_id, &[b], at, as_of).await? {
            let other = if t.subject_id == b {
                t.object_id
            } else {
                t.subject_id
            };
            b_edges.entry(other).or_default().push(t.id);
        }
    }

    // a's second layer: expand only non-hub one-hop neighbors, reading in ascending degree until `max_edges`
    let mut adj2: HashMap<Uuid, Vec<(Uuid, Uuid)>> = HashMap::new();
    if max_hops >= 3 {
        let mids: Vec<Uuid> = a_edges
            .keys()
            .copied()
            .filter(|x| *x != a && *x != b)
            .collect();
        let degree = degrees(pool, kb_id, &mids, at, as_of).await?;
        let mut by_degree: Vec<(Uuid, i64)> = mids
            .iter()
            .map(|x| (*x, degree.get(x).copied().unwrap_or(0)))
            .filter(|(_, d)| *d <= limits.hub_degree)
            .collect();
        by_degree.sort_by_key(|(_, d)| *d);
        let mut budget = limits.max_edges;
        let mut expand = Vec::new();
        for (x, d) in by_degree {
            if budget < d {
                break;
            }
            budget -= d;
            expand.push(x);
        }
        if !expand.is_empty() {
            let expand_set: HashSet<Uuid> = expand.iter().copied().collect();
            for t in touching(pool, kb_id, &expand, at, as_of).await? {
                for (x, y) in [(t.subject_id, t.object_id), (t.object_id, t.subject_id)] {
                    if expand_set.contains(&x) {
                        adj2.entry(x).or_default().push((y, t.id));
                    }
                }
            }
        }
    }

    // Assemble candidates: direct one-hop, two-hop intersection, three-hop reaching b's neighbor via the second layer
    let mut candidates: Vec<Candidate> = Vec::new();
    let cap = limits.max_candidates.max(1);
    if let Some(direct) = a_edges.get(&b) {
        for f in direct {
            candidates.push(Candidate {
                nodes: vec![a, b],
                facts: vec![*f],
            });
        }
    }
    if max_hops >= 2 {
        let mut mids: Vec<&Uuid> = a_edges
            .keys()
            .filter(|x| **x != a && **x != b && b_edges.contains_key(*x))
            .collect();
        mids.sort();
        'two: for x in mids {
            for fa in &a_edges[x] {
                for fb in &b_edges[x] {
                    if candidates.len() >= cap {
                        break 'two;
                    }
                    candidates.push(Candidate {
                        nodes: vec![a, *x, b],
                        facts: vec![*fa, *fb],
                    });
                }
            }
        }
    }
    if max_hops >= 3 {
        let mut xs: Vec<&Uuid> = adj2.keys().collect();
        xs.sort();
        'three: for x in xs {
            for (y, fxy) in &adj2[x] {
                if *y == a || *y == b || y == x {
                    continue;
                }
                let Some(fbs) = b_edges.get(y) else {
                    continue;
                };
                for fa in &a_edges[x] {
                    for fb in fbs {
                        if candidates.len() >= cap {
                            break 'three;
                        }
                        candidates.push(Candidate {
                            nodes: vec![a, *x, *y, b],
                            facts: vec![*fa, *fxy, *fb],
                        });
                    }
                }
            }
        }
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    // Sort: shorter first; for equal length, by how obscure the intermediate nodes are; then by confidence
    let mut mids: Vec<Uuid> = candidates
        .iter()
        .flat_map(|c| c.nodes[1..c.nodes.len() - 1].iter().copied())
        .collect();
    mids.sort();
    mids.dedup();
    let degree = degrees(pool, kb_id, &mids, at, as_of).await?;
    let specificity = |c: &Candidate| -> f64 {
        c.nodes[1..c.nodes.len() - 1]
            .iter()
            .map(|n| (1.0 + degree.get(n).copied().unwrap_or(0) as f64).ln())
            .sum()
    };
    let mut all_facts: Vec<Uuid> = candidates.iter().flat_map(|c| c.facts.clone()).collect();
    all_facts.sort();
    all_facts.dedup();
    let detail = details(pool, kb_id, &all_facts, as_of).await?;
    let confidence = |c: &Candidate| -> f64 {
        let n = c.facts.len().max(1) as f64;
        c.facts
            .iter()
            .map(|f| detail.get(f).map(|e| e.confidence as f64).unwrap_or(0.0))
            .sum::<f64>()
            / n
    };
    let mut scored: Vec<(usize, f64, f64, Candidate)> = candidates
        .into_iter()
        .map(|c| (c.facts.len(), specificity(&c), confidence(&c), c))
        .collect();
    scored.sort_by(|x, y| {
        x.0.cmp(&y.0)
            .then(x.1.partial_cmp(&y.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(y.2.partial_cmp(&x.2).unwrap_or(std::cmp::Ordering::Equal))
    });

    // Only one result per identical node chain and identical directed predicates: repeated observations in the same direction fold, but A -> B and
    // A <- B are not the same relation, so a direction must not be dropped just because the two ends match during traversal.
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (_, spec, _, c) in scored {
        if out.len() >= limits.max_paths {
            break;
        }
        let edges: Option<Vec<PathEdge>> = c.facts.iter().map(|f| detail.get(f).cloned()).collect();
        // An edge whose details cannot be looked up (one end invisible at the as_of moment) drops the whole path: a chain broken at one link is not a chain
        let Some(edges) = edges else {
            continue;
        };
        let directed_predicates: Vec<_> = edges
            .iter()
            .zip(&c.nodes)
            .map(|(e, node)| {
                (
                    e.predicate.clone().unwrap_or_default(),
                    e.subject_id == *node,
                )
            })
            .collect();
        if !seen.insert((c.nodes.clone(), directed_predicates)) {
            continue;
        }
        out.push(Path {
            nodes: c.nodes,
            edges,
            specificity: spec,
        });
    }
    Ok(out)
}

/// Edges touching this batch of entities (both directions), filtered by both time axes; self-loops excluded
async fn touching(
    pool: &PgPool,
    kb_id: Uuid,
    ids: &[Uuid],
    at: Option<DateTime<Utc>>,
    as_of: Option<DateTime<Utc>>,
) -> AppResult<Vec<Touch>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let subject = record_axis::owner_at("f", "subject_id", as_of.map(|_| 3), false);
    let object = record_axis::owner_at("f", "object_id", as_of.map(|_| 3), true);
    let rows: Vec<Touch> = sqlx::query_as(&format!(
        "SELECT f.id, {subject} AS subject_id, {object} AS object_id
           FROM facts f
          WHERE f.kb_id = $1 AND f.object_id IS NOT NULL
            AND ({subject} = ANY($2) OR {object} = ANY($2))
            AND {subject} <> {object}
            AND {held} AND {hold}",
        held = record_axis::facts_held_at("f", as_of.map(|_| 3)),
        hold = world_axis::facts_hold_at("f", 4),
    ))
    .bind(kb_id)
    .bind(ids)
    .bind(as_of)
    .bind(at)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Each entity's degree in this batch, under the same time-axis filter
async fn degrees(
    pool: &PgPool,
    kb_id: Uuid,
    ids: &[Uuid],
    at: Option<DateTime<Utc>>,
    as_of: Option<DateTime<Utc>>,
) -> AppResult<HashMap<Uuid, i64>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let subject = record_axis::owner_at("f", "subject_id", as_of.map(|_| 3), false);
    let object = record_axis::owner_at("f", "object_id", as_of.map(|_| 3), true);
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(&format!(
        "SELECT n.id, count(f.id)
           FROM unnest($2::uuid[]) AS n(id)
           JOIN facts f ON f.kb_id = $1 AND f.object_id IS NOT NULL
                       AND ({subject} = n.id OR {object} = n.id)
                       AND {held} AND {hold}
          GROUP BY n.id",
        held = record_axis::facts_held_at("f", as_of.map(|_| 3)),
        hold = world_axis::facts_hold_at("f", 4),
    ))
    .bind(kb_id)
    .bind(ids)
    .bind(as_of)
    .bind(at)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Both-end names, predicates, and intervals of this batch of facts
async fn details(
    pool: &PgPool,
    kb_id: Uuid,
    fact_ids: &[Uuid],
    as_of: Option<DateTime<Utc>>,
) -> AppResult<HashMap<Uuid, PathEdge>> {
    if fact_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let subject = record_axis::owner_at("f", "subject_id", as_of.map(|_| 3), false);
    let object = record_axis::owner_at("f", "object_id", as_of.map(|_| 3), true);
    let rows: Vec<PathEdge> = sqlx::query_as(&format!(
        "WITH {targets}
         SELECT f.id AS fact_id,
                {subject} AS subject_id, s.canonical_name AS subject_name,
                {object} AS object_id, o.canonical_name AS object_name,
                COALESCE(r.label, fact_surface_predicate(f.id)) AS predicate,
                f.valid_from, f.valid_from_precision, f.valid_to, f.valid_to_precision,
                {holds_from} AS holds_from, {holds_to} AS holds_to, f.confidence,
                f.end_derived, {corrected_ends} AS corrected_ends,
                {corrected_ends} IS NOT NULL AS time_corrected,
                closer.closed_by_id, closer.closed_by, closer.closed_by_value,
                CASE WHEN {corrected_ends} IS NOT NULL THEN {correction_note} END
                    AS correction_note
           FROM facts f
           LEFT JOIN relation_types r ON r.id = f.predicate_id
           JOIN entities s ON s.id = {subject}
           JOIN entities o ON o.id = {object}
           {closer}
          WHERE f.kb_id = $1 AND f.id = ANY($2)",
        holds_from = world_axis::facts_holds_from("f"),
        holds_to = world_axis::facts_holds_to("f"),
        targets = crate::graph::TIME_CORRECTED_TARGETS,
        corrected_ends = crate::graph::corrected_ends_sql("f"),
        correction_note = crate::graph::correction_note_sql("f"),
        closer = crate::graph::closed_by_join(&subject, &object, as_of.map(|_| 3)),
    ))
    .bind(kb_id)
    .bind(fact_ids)
    .bind(as_of)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|e| (e.fact_id, e)).collect())
}
