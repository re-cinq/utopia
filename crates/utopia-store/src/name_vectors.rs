//! Name vectors: the second recall channel (0041 decision 3, slice 2).
//!
//! Channel 1 is literal equality: a mention's name equals some name fact's after `normalize_name`. It cannot find
//! abbreviations ("Haitan 1" ↔ "Ocean Probe No. 1"), nor the same name written in another script (#709), and those two
//! are precisely the main reason a "multi-name" gets wrongly split into two entities. Here each name fact stores a vector of the name string itself,
//! and at query time the nearest few within the same KB are taken; **it only proposes, it does not decide** — whether the nearest name is the same thing
//! is judged by the resolution side under the existing rules (profiles, the arbiter, and later slice 3's evidence).
//!
//! Vectors follow the embedding model and have no fixed dimension (same as `chunks.embedding`). Queries follow the two rules from 0035:
//! cast both sides of `<=>` to the literal dimension, and put `vector_dims(...) = N` in the predicate; once the HNSW index is built it is used,
//! and without it the exact path is taken, with the same results.

use pgvector::Vector;
use sqlx::PgPool;
use utopia_core::AppResult;
use uuid::Uuid;

use crate::vector_index::{self, Target};

/// At most how many to propose at once. Only a few of the truly relevant nearest neighbours of a name vector ever exceed the first few; more just gives the arbiter
/// noise. The value is to be settled by `identity.mjs`; for now take one that will not flood the queue
pub const TOP_K: usize = 8;

/// Cosine floor. A name string's vector is "tighter" than a whole passage's vector — two unrelated names can still have
/// a cosine around 0.4 — so this threshold sits above the profile's `SIM_ATTACH`. Likewise a provisional value to be measured
pub const SIM_FLOOR: f32 = 0.60;

/// Name fact `$fact` has identity-source evidence (0071 decisions 4, 5): such a name stores no vector.
/// `$kb` is the KB the fact lives in: the evidence document and the fact must be in the same KB (the (kb_id, document_id) foreign key of 0070),
/// so search only within this KB; without it the anti-join spreads over every KB's identity evidence (the internal cluster: 64 names took 143 seconds, 9.3 million buffers)
macro_rules! has_identity_evidence {
    ($fact:literal, $kb:literal) => {
        concat!(
            "EXISTS (SELECT 1 FROM fact_evidence fe
                       JOIN documents d ON d.id = fe.document_id
                       JOIN sources s ON s.id = d.source_id
                      WHERE fe.fact_id = ",
            $fact,
            " AND d.kb_id = ",
            $kb,
            " AND s.kind = 'statements' AND s.config->>'names' = 'identity')"
        )
    };
}

/// Take the `known_as` id via a subquery, do not change it back to a JOIN: the subquery computes a value first, so `predicate_id = it` becomes the index
/// condition of `facts_live_predicate_idx` (0115); the scan touches only this KB's name facts, and still walks by id, and
/// `ORDER BY ... LIMIT` does no sort. Written as a JOIN, the planner walks the whole KB in id order along `facts_kb_id_key`,
/// and the predicate can only filter row by row (#1296: on a KB with 200,000 facts it read 200,000 rows; this reads 20,000)
///
/// A name with identity-source evidence is not backfilled with a vector, even if prose mentions it (0071 decisions 4, 5)
const PENDING_SQL: &str = concat!(
    "SELECT f.id, f.subject_id, f.object_value->>'value'
       FROM facts f
       LEFT JOIN name_vectors v ON v.fact_id = f.id
      WHERE f.kb_id = $1
        AND f.predicate_id = (SELECT nr.id FROM relation_types nr
                               WHERE nr.kb_id = $1 AND nr.builtin AND nr.key = $2)
        AND f.invalidated_at IS NULL AND f.object_value IS NOT NULL
        AND v.fact_id IS NULL
        AND NOT ",
    has_identity_evidence!("f.id", "f.kb_id"),
    "
      ORDER BY f.id
      LIMIT $3"
);

/// Name facts with no vector yet: the current ones, on `known_as`, with no entry in `name_vectors`, and
/// without identity-source evidence.
/// Taken in write order (`facts.id` is a uuid v7, so ordering by it is ordering by write time; this table has no created_at —
/// found by running end to end: ordering by a column that does not exist made backfill fail silently on every document), one batch at a time
pub async fn pending(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
) -> AppResult<Vec<(Uuid, Uuid, String)>> {
    let rows: Vec<(Uuid, Uuid, String)> = sqlx::query_as(PENDING_SQL)
        .bind(kb_id)
        .bind(crate::names::KNOWN_AS)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

const DROP_FOR_SOURCE_SQL: &str = "DELETE FROM name_vectors v
          USING fact_evidence fe JOIN documents d ON d.id = fe.document_id
          WHERE fe.fact_id = v.fact_id AND d.source_id = $1";

const DROP_FOR_DOCUMENT_SQL: &str = "DELETE FROM name_vectors v USING fact_evidence fe
          WHERE fe.fact_id = v.fact_id AND fe.document_id = $1";

/// The source has just enabled identity names (0071 decision 6): delete name vectors that have document evidence for it, in the same transaction as the config change
pub async fn drop_for_source(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    source_id: Uuid,
) -> AppResult<()> {
    sqlx::query(DROP_FOR_SOURCE_SQL)
        .bind(source_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Identity-source documents have finished extraction (0071 decision 5): a name vector that prose wrote first is deleted once this document provides identity evidence
pub async fn drop_for_document(pool: &PgPool, document_id: Uuid) -> AppResult<()> {
    sqlx::query(DROP_FOR_DOCUMENT_SQL)
        .bind(document_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// A merge copied duplicate names' evidence onto the surviving name (#1426): where that gave it identity evidence, delete the vector
pub async fn drop_with_identity_evidence(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    facts: &[Uuid],
) -> AppResult<()> {
    if facts.is_empty() {
        return Ok(());
    }
    sqlx::query(concat!(
        "DELETE FROM name_vectors v WHERE v.fact_id = ANY($1) AND ",
        has_identity_evidence!("v.fact_id", "v.kb_id")
    ))
    .bind(facts)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Writes a batch of name vectors: `(fact_id, entity_id, embedding)`. Rewriting the same fact overwrites (after a model change it is
/// recomputed). The first time a vector of this dimension is written, the index should kick in (0035).
/// Re-check the identity evidence at write time: if backfill was midway when the source enabled identity names, or identity documents finished extraction and a deletion round ran,
/// this must not write it back
pub async fn set(pool: &PgPool, kb_id: Uuid, items: &[(Uuid, Uuid, Vec<f32>)]) -> AppResult<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for (fact_id, entity_id, emb) in items {
        sqlx::query(concat!(
            "INSERT INTO name_vectors (fact_id, kb_id, entity_id, embedding)
                 SELECT $1::uuid, $2::uuid, $3::uuid, $4::vector WHERE NOT ",
            has_identity_evidence!("$1", "$2"),
            " ON CONFLICT (fact_id) DO UPDATE SET embedding = EXCLUDED.embedding"
        ))
        .bind(fact_id)
        .bind(kb_id)
        .bind(entity_id)
        .bind(Vector::from(emb.clone()))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    if let Some((_, _, first)) = items.first() {
        vector_index::request(pool, Target::NameVectors, first.len()).await?;
    }
    Ok(())
}

/// One recalled name: which entity it hangs on, what class that entity is, the name itself, and its cosine to the query
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Near {
    pub entity_id: Uuid,
    pub fact_id: Uuid,
    pub name: String,
    pub type_id: Option<Uuid>,
    pub type_label: Option<String>,
    pub similarity: f32,
}

/// The `k` names in the same KB nearest to the query vector, in descending similarity. Only considers current name facts and entities not merged away.
/// It does not filter on `description`: a described thing has no name fact (0044), and the JOIN to facts already
/// excludes it; should a named thing later also carry a description, its name should still be recalled (#877 review).
///
/// The inner layer takes the `k * 4` nearest in a shape the index can serve, and the outer layer then filters on the state of the fact and entity — filtering
/// inside would stop the index being used (0035); taking four times as many leaves headroom for the filter, and name-fact invalidation and entity merges
/// are both uncommon, so usually not a single one gets filtered out
pub async fn nearest(pool: &PgPool, kb_id: Uuid, query: &[f32], k: usize) -> AppResult<Vec<Near>> {
    let dims = query.len();
    if dims == 0 || k == 0 {
        return Ok(Vec::new());
    }
    let distance = vector_index::distance("v.embedding", 2, dims);
    let order = vector_index::index_order("v.embedding", 2, dims);
    let same_dims = vector_index::same_dims("v.embedding", dims);
    let sql = format!(
        // The outer ordering follows the `vector_index::RESORT` rule (`distance + 0, id`), and column names are qualified to the CTE:
        // the outer query also joins facts / entities / entity_types, so a bare `id` would be ambiguous
        "WITH near AS MATERIALIZED (
             SELECT v.fact_id AS id, v.entity_id, ({distance}) AS distance
               FROM name_vectors v
              WHERE v.kb_id = $1 AND {same_dims}
              ORDER BY {order}
              LIMIT $3
         )
         SELECT n.entity_id, n.id AS fact_id,
                f.object_value->>'value' AS name,
                e.type_id,
                et.label AS type_label,
                (1 - n.distance)::real AS similarity
           FROM near n
           JOIN facts f ON f.id = n.id AND f.invalidated_at IS NULL
           JOIN entities e ON e.id = n.entity_id AND e.merged_into IS NULL
           LEFT JOIN entity_types et ON et.id = e.type_id
          ORDER BY n.distance + 0, n.id
          LIMIT $4",
    );
    // Rule 3: with the index in place, a small KB on a shared table will not fill up without iterative_scan turned on
    let mut tx = pool.begin().await?;
    vector_index::relaxed_order(pool, &mut tx).await?;
    let rows: Vec<Near> = sqlx::query_as(&sql)
        .bind(kb_id)
        .bind(Vector::from(query.to_vec()))
        .bind((k * 4) as i64)
        .bind(k as i64)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}

#[cfg(test)]
#[path = "name_vectors_plan_tests.rs"]
mod plan_tests;
