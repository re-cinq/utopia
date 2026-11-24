//! 推送来的陈述记住它写下的事实：块内位置 + 陈述的规范键 → 事实（9012）。

use std::collections::{HashMap, VecDeque};

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

/// 同一文档里最近一块被取代、抽过、记过陈述的块：沿用的上一版
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

/// 上一版记下的具名实体：规范键 → 仍然活着（没被并走）的 entity_id；没有上一版就是空表
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

/// 沿用上一版的事实：同一文档里最近一块被取代、抽过、记过陈述的块，按规范键配对（相同的键按
/// 原有顺序一一配），配上且事实还活着的陈述把事实、证据和角色词搬到新块上，并给新块记下
/// 同一条事实。返回被沿用的陈述在块里的原始位置和它们的事实；没有上一版就是空表
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
