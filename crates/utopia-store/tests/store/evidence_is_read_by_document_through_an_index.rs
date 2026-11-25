//! Reading evidence by document must go through an index (re:cinq migration 9000): `resolve_time` picks candidates per document by `document_id`,
//! reads and changes its evidence, reconciling picks candidates by document, and when a document is deleted the foreign-key cascade also finds rows through it. The primary key of `fact_evidence` is
//! `(fact_id, chunk_id)`, and without this index every lookup would scan the whole table — the evidence of every store.
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
