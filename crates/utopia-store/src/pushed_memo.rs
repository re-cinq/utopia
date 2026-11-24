//! 推送来的陈述记住它写下的事实：块内位置 + 陈述的规范键 → 事实（9012）。

use sqlx::PgPool;
use uuid::Uuid;

use utopia_core::AppResult;

/// 记下块内第 `ord` 条陈述（规范键 `key`）写出的事实。
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

/// 记下规范键 `key` 的具名实体对应的 entity_id。
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
