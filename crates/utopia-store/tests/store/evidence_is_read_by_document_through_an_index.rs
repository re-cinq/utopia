//! 证据按文档读要走索引（re:cinq 迁移 9000）：`resolve_time` 每篇文档都按 `document_id`
//! 读、改它的证据，勘误按文档挑候选，删文档时外键级联也按它找行。`fact_evidence` 的主键是
//! `(fact_id, chunk_id)`，没有这条索引时每一次都扫全表——所有库的证据。
use sqlx::PgPool;

#[tokio::test]
async fn fact_evidence_has_an_index_led_by_document_id() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let led_by_document: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM pg_index i
               JOIN pg_class t ON t.oid = i.indrelid
               JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = i.indkey[0]
              WHERE t.relname = 'fact_evidence' AND a.attname = 'document_id' AND i.indisvalid)",
    )
    .fetch_one(&pool)
    .await?;
    assert!(
        led_by_document,
        "an index on fact_evidence starts with document_id"
    );
    Ok(())
}
