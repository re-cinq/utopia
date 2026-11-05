//! 一个来源关上自己不再陈述的事实（RC-004）。
//!
//! 推陈述的来源每次重推，旧版分块被取代；不再被说出的那条事实只剩停在旧分块上的证据，
//! 落进审核的「待确认」（[`crate::review::UNCONFIRMED_FACT`]）一直不走。这里成批关上它们：
//! 终点是它最后一条证据被取代的那一刻，走 [`crate::temporal::close_superseded`]——与
//! 审核里人点「闭合」同一条作废+改写，历史留着，不撤销也不否决。

use sqlx::PgPool;
use utopia_core::AppResult;
use uuid::Uuid;

/// 一批的结果。`remaining` 是这批之后还能关的条数，全量数（由来源的文档出发，走
/// `fact_evidence_document_idx`）；`skipped` 是起点晚于那一刻、关不上、留给人的条数
pub struct SourceClosing {
    pub closed: u64,
    pub skipped: i64,
    pub remaining: i64,
}

/// 范围 `$2`（一个来源，或一份文档）的待确认事实，库 `$1`：证据全出自这个来源的文档、全被取代；不含名字事实
/// （`known_as` 是 Utopia 自己写的）；带陈述来源的类型化行不算——它跟着陈述走，陈述关了
/// 物化把它按关上的陈述重建。`inverted`：起点（按它的精度截断）晚于终点，
/// 与 `close_superseded` 拒绝 `interval_inverted` 是同一个比较
fn candidates(scope: Scope) -> String {
    let key = scope.column();
    format!(
        "WITH ev AS (
           SELECT fe.fact_id, date_trunc('second', max(c.superseded_at), 'UTC') AS ended
             FROM documents d
             JOIN fact_evidence fe ON fe.document_id = d.id
             JOIN chunks c ON c.id = fe.chunk_id
            WHERE d.kb_id = $1 AND {key} = $2
            GROUP BY fe.fact_id
           HAVING bool_and(c.superseded_at IS NOT NULL))
         SELECT f.id, f.layer, ev.ended,
                COALESCE(date_trunc(CASE WHEN f.valid_from_precision IN
                                         ('year','month','day','hour','minute','second')
                                         THEN f.valid_from_precision ELSE 'microseconds' END,
                                    f.valid_from, 'UTC') > ev.ended, false) AS inverted
           FROM ev JOIN facts f ON f.id = ev.fact_id
          WHERE f.kb_id = $1 AND f.invalidated_at IS NULL AND {unconfirmed} AND {not_a_name}
            AND NOT EXISTS (SELECT 1 FROM fact_evidence fe
                              JOIN chunks c ON c.id = fe.chunk_id
                              JOIN documents d ON d.id = c.document_id
                             WHERE fe.fact_id = f.id AND {key} IS DISTINCT FROM $2)
            AND (f.layer = 'open'
                 OR NOT EXISTS (SELECT 1 FROM typed_fact_sources s WHERE s.fact_id = f.id))",
        unconfirmed = crate::review::UNCONFIRMED_FACT,
        not_a_name = crate::names::not_a_name("f"),
        key = key,
    )
}

/// 关上至多 `limit` 条，开放陈述在前。关了陈述就排一次类型化重算（与否决陈述同一个做法，
/// `graph::reject_fact`）：旧陈述作废，它物化出的行在重算里退下，再按关上的陈述重建
pub async fn close_of_source(
    pool: &PgPool,
    kb_id: Uuid,
    source_id: Uuid,
    limit: i64,
) -> AppResult<SourceClosing> {
    close_in(pool, kb_id, Scope::Source, source_id, limit).await
}

/// 推来的条目（一份文档）抽取完新版本之后，关上新版本不再陈述的事实（RC-006）：证据全停在
/// 这份文档被取代的分块上，别处没有证据。一批一批关到没有为止，轮数有界
pub async fn close_of_document(
    pool: &PgPool,
    kb_id: Uuid,
    document_id: Uuid,
) -> AppResult<SourceClosing> {
    let mut total = SourceClosing {
        closed: 0,
        skipped: 0,
        remaining: 1,
    };
    for _ in 0..MAX_DOCUMENT_ROUNDS {
        if total.remaining == 0 {
            break;
        }
        let batch = close_in(pool, kb_id, Scope::Document, document_id, DOCUMENT_BATCH).await?;
        total.closed += batch.closed;
        total.skipped = batch.skipped;
        total.remaining = batch.remaining;
    }
    Ok(total)
}

const DOCUMENT_BATCH: i64 = 1000;
const MAX_DOCUMENT_ROUNDS: usize = 50;

#[derive(Clone, Copy)]
enum Scope {
    Source,
    Document,
}

impl Scope {
    fn column(self) -> &'static str {
        match self {
            Scope::Source => "d.source_id",
            Scope::Document => "d.id",
        }
    }
}

async fn close_in(
    pool: &PgPool,
    kb_id: Uuid,
    scope: Scope,
    scope_id: Uuid,
    limit: i64,
) -> AppResult<SourceClosing> {
    let due: Vec<(Uuid, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(&format!(
        "SELECT id, layer, ended FROM ({}) c WHERE NOT inverted
          ORDER BY layer = 'typed', id LIMIT $3",
        candidates(scope)
    ))
    .persistent(false)
    .bind(kb_id)
    .bind(scope_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut closed = 0;
    let mut statements = false;
    for (fact_id, layer, ended) in &due {
        // None = 期间被别人作废或关了，不算这批的
        if crate::temporal::close_superseded(pool, *fact_id, *ended, "second")
            .await?
            .is_some()
        {
            closed += 1;
            statements |= layer == "open";
        }
    }
    if statements {
        crate::jobs::enqueue_unless_queued(
            pool,
            crate::phrase_bindings::MATERIALIZE_KIND,
            serde_json::json!({ "kb_id": kb_id }),
        )
        .await?;
    }
    let (remaining, skipped): (i64, i64) = sqlx::query_as(&format!(
        "SELECT count(*) FILTER (WHERE NOT inverted), count(*) FILTER (WHERE inverted)
           FROM ({}) c",
        candidates(scope)
    ))
    .persistent(false)
    .bind(kb_id)
    .bind(scope_id)
    .fetch_one(pool)
    .await?;
    Ok(SourceClosing {
        closed,
        skipped,
        remaining,
    })
}
