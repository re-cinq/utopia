//! For pushed statements, remember the facts they wrote: position within the chunk + the statement's canonical key → fact (9012).

use std::collections::{HashMap, VecDeque};

use sqlx::PgPool;
use uuid::Uuid;

use utopia_core::AppResult;

/// Records the facts written by the `ord`-th statement in the chunk (canonical key `key`).
pub async fn remember(
    pool: &PgPool,
    chunk_id: Uuid,
    ord: i32,
    key: &str,
    fact_id: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO pushed_statement_writes (chunk_id, ord, key, fact_id)
         VALUES ($1, $2, $3, $4) ON CONFLICT (chunk_id, ord) DO NOTHING",
    )
    .bind(chunk_id)
    .bind(ord)
    .bind(key)
    .bind(fact_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Records the entity_id corresponding to the named entity with canonical key `key`.
pub async fn remember_entity(
    pool: &PgPool,
    chunk_id: Uuid,
    key: &str,
    entity_id: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO pushed_entity_handles (chunk_id, key, entity_id)
         VALUES ($1, $2, $3) ON CONFLICT (chunk_id, key) DO NOTHING",
    )
    .bind(chunk_id)
    .bind(key)
    .bind(entity_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The most recent chunk in the same document that was superseded, extracted, and had statements recorded: the previous version carried over
async fn previous_chunk(
    pool: &PgPool,
    document_id: Uuid,
    chunk_id: Uuid,
) -> AppResult<Option<Uuid>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT c.id FROM chunks c
         WHERE c.document_id = $1 AND c.id <> $2
           AND c.superseded_at IS NOT NULL AND c.extracted_at IS NOT NULL
           AND EXISTS (SELECT 1 FROM pushed_statement_writes w WHERE w.chunk_id = c.id)
         ORDER BY c.superseded_at DESC LIMIT 1",
    )
    .bind(document_id)
    .bind(chunk_id)
    .fetch_optional(pool)
    .await?)
}

/// Named entities recorded in the previous version: canonical key → entity_id still alive (not merged away); with no previous version this is an empty table
pub async fn carried_entities(
    pool: &PgPool,
    document_id: Uuid,
    chunk_id: Uuid,
) -> AppResult<HashMap<String, Uuid>> {
    let Some(previous) = previous_chunk(pool, document_id, chunk_id).await? else {
        return Ok(HashMap::new());
    };
    let remembered: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT h.key, h.entity_id FROM pushed_entity_handles h
         JOIN entities e ON e.id = h.entity_id
         WHERE h.chunk_id = $1 AND e.merged_into IS NULL",
    )
    .bind(previous)
    .fetch_all(pool)
    .await?;
    Ok(remembered.into_iter().collect())
}

/// Carries over facts from the previous version: the most recent superseded chunk in the same document that was extracted and had statements recorded is paired by canonical key (identical keys are paired
/// one by one in their original order), and statements that are paired and whose facts are still alive move their facts, evidence and role words onto the new chunk, and record for the new chunk
/// the same fact. Returns the original positions of the carried-over statements in the chunk and their facts; with no previous version this is an empty table
pub async fn carry_over(
    pool: &PgPool,
    document_id: Uuid,
    chunk_id: Uuid,
    keys: &[String],
) -> AppResult<HashMap<usize, Uuid>> {
    let Some(previous) = previous_chunk(pool, document_id, chunk_id).await? else {
        return Ok(HashMap::new());
    };
    let remembered: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT w.key, w.fact_id FROM pushed_statement_writes w
         JOIN facts f ON f.id = w.fact_id
         WHERE w.chunk_id = $1 AND f.invalidated_at IS NULL
         ORDER BY w.ord",
    )
    .bind(previous)
    .fetch_all(pool)
    .await?;
    let mut available: HashMap<&str, VecDeque<Uuid>> = HashMap::new();
    for (key, fact_id) in &remembered {
        available.entry(key).or_default().push_back(*fact_id);
    }
    let mut reused: Vec<(usize, &str, Uuid)> = Vec::new();
    for (raw_index, key) in keys.iter().enumerate() {
        if let Some(fact_id) = available
            .get_mut(key.as_str())
            .and_then(VecDeque::pop_front)
        {
            reused.push((raw_index, key, fact_id));
        }
    }
    if reused.is_empty() {
        return Ok(HashMap::new());
    }
    let fact_ids: Vec<Uuid> = reused.iter().map(|(_, _, fact_id)| *fact_id).collect();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, quote, proposed_predicate, document_id,
                                    doc_version, quote_start, quote_end)
         SELECT fe.fact_id, $1, fe.quote, fe.proposed_predicate, c.document_id, c.doc_version,
                fe.quote_start, fe.quote_end
         FROM fact_evidence fe JOIN chunks c ON c.id = $1
         WHERE fe.chunk_id = $2 AND fe.fact_id = ANY($3)
         ON CONFLICT (fact_id, chunk_id) DO NOTHING",
    )
    .bind(chunk_id)
    .bind(previous)
    .bind(&fact_ids)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO statement_qualifiers (fact_id, role, value, entity_id, chunk_id)
         SELECT fact_id, role, value, entity_id, $1
         FROM statement_qualifiers WHERE chunk_id = $2 AND fact_id = ANY($3)
         ON CONFLICT (fact_id, role, chunk_id) DO NOTHING",
    )
    .bind(chunk_id)
    .bind(previous)
    .bind(&fact_ids)
    .execute(&mut *tx)
    .await?;
    for (raw_index, key, fact_id) in &reused {
        sqlx::query(
            "INSERT INTO pushed_statement_writes (chunk_id, ord, key, fact_id)
             VALUES ($1, $2, $3, $4) ON CONFLICT (chunk_id, ord) DO NOTHING",
        )
        .bind(chunk_id)
        .bind(*raw_index as i32)
        .bind(*key)
        .bind(fact_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(reused
        .iter()
        .map(|(raw_index, _, fact_id)| (*raw_index, *fact_id))
        .collect())
}
