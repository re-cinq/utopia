//! Shapes the ontology agent has reviewed (0061 cut 1.1): raised, already present, and not-raised shapes each get one line; the next round skips them accordingly.

use serde_json::Value;
use sqlx::PgPool;
use utopia_core::AppResult;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Review {
    /// phrase | kind_word
    pub kind: String,
    pub shape: Value,
    /// proposed | existing | declined
    pub outcome: String,
    pub target: Option<String>,
    /// Fingerprint of the vocabulary when it was checked; if it changed, existing and not-raised shapes are sent again.
    pub basis: String,
}

pub async fn list(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<Review>> {
    Ok(sqlx::query_as(
        "SELECT kind, shape, outcome, target, basis FROM ontology_agent_reviews WHERE kb_id = $1",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}

/// Records a shape reviewed in a round. Reviewing the same shape again refreshes it.
pub async fn record(pool: &PgPool, kb_id: Uuid, items: &[Review]) -> AppResult<()> {
    for it in items {
        sqlx::query(
            "INSERT INTO ontology_agent_reviews (kb_id, kind, shape, outcome, target, basis)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (kb_id, kind, shape) DO UPDATE
               SET outcome = EXCLUDED.outcome, target = EXCLUDED.target,
                   basis = EXCLUDED.basis, reviewed_at = now()",
        )
        .bind(kb_id)
        .bind(&it.kind)
        .bind(&it.shape)
        .bind(&it.outcome)
        .bind(&it.target)
        .bind(&it.basis)
        .execute(pool)
        .await?;
    }
    Ok(())
}
