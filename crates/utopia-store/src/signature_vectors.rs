//! Vectors for phrase signatures (#1097, 0110).
//!
//! Phrase alignment gives a short list to signatures with many candidates, so their text (the phrase, one example sentence, a quote) must be embedded.
//! The text of a signature barely changes between rounds, but previously every round re-embedded everything: with seventeen thousand signatures that have many candidates in the store, each round did
//! seventeen thousand embeddings, when the judgment might only concern a few dozen newcomers. Here vectors are stored by (store, embedding model, hash of the text)
//! once computed, and a round embeds only the text it has not seen.
//!
//! Read by key only, with no nearest-neighbour query, so no HNSW is built and nothing is registered in `vector_index`; `embedding` has no fixed dimension,
//! following the chosen embedding model (the same rule as `chunks.embedding`). This is a cache, not a ledger: if it is deleted, the next round
//! re-embeds, and the result is the same.

use pgvector::Vector;
use sqlx::PgPool;
use std::collections::HashMap;
use utopia_core::AppResult;
use uuid::Uuid;

/// Among these hashes, those that already have a stored vector: hash → vector
pub async fn get(
    pool: &PgPool,
    kb_id: Uuid,
    model: &str,
    hashes: &[Vec<u8>],
) -> AppResult<HashMap<Vec<u8>, Vec<f32>>> {
    if hashes.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(Vec<u8>, Vector)> = sqlx::query_as(
        "SELECT text_hash, embedding FROM signature_vectors
          WHERE kb_id = $1 AND model = $2 AND text_hash = ANY($3)",
    )
    .bind(kb_id)
    .bind(model)
    .bind(hashes)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(hash, v)| (hash, v.to_vec()))
        .collect())
}

/// Stores a batch of newly embedded vectors. Committed in one transaction: committing each one separately is one fsync per row. A key collision (the same text
/// written elsewhere between two rounds) is not overwritten: same model and same text give the same vector
pub async fn put(
    pool: &PgPool,
    kb_id: Uuid,
    model: &str,
    items: &[(Vec<u8>, Vec<f32>)],
) -> AppResult<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for (hash, emb) in items {
        sqlx::query(
            "INSERT INTO signature_vectors (kb_id, model, text_hash, embedding)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
        )
        .bind(kb_id)
        .bind(model)
        .bind(hash)
        .bind(Vector::from(emb.clone()))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Deletes vectors in this store that are no longer used: those whose hash is not in `keep`, and those embedded by another model (after a model change,
/// the old vectors are in a different space from the new attribute vectors, so keeping them only takes up room). `keep` must be the hashes of **all** signatures in this round,
/// not just those embedded this round — otherwise, if one batch's embedding fails, the batch stored in the previous round would be deleted as well. Returns how many rows were deleted
pub async fn prune(pool: &PgPool, kb_id: Uuid, model: &str, keep: &[Vec<u8>]) -> AppResult<u64> {
    Ok(sqlx::query(
        "DELETE FROM signature_vectors
          WHERE kb_id = $1 AND (model <> $2 OR NOT (text_hash = ANY($3)))",
    )
    .bind(kb_id)
    .bind(model)
    .bind(keep)
    .execute(pool)
    .await?
    .rows_affected())
}
