//! RC-010: re-computing the witness after a deletion, or after undoing a deletion, does not happen inside the transaction that locks the source row.
//!
//! - When `documents::delete` returns, the witnesses have already moved onto the evidence that remains, and no `reattest_facts` task is left behind;
//! - A document whose upload hits a tombstone and is resurrected inside the caller's transaction queues a `reattest_facts`, and the witness returns only once that task has run.
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is not set. It builds and tears down its own data and never touches an existing DB.

use chrono::{DateTime, TimeZone, Utc};
use sqlx::PgPool;
use utopia_store::graph::{self, FactObject};
use uuid::Uuid;

const ORG: &str = "deletion-reattests-after-its-source-test";

fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
}

async fn attested(pool: &PgPool, fact: Uuid) -> anyhow::Result<Option<DateTime<Utc>>> {
    Ok(
        sqlx::query_scalar("SELECT attested_from FROM facts WHERE id = $1")
            .bind(fact)
            .fetch_one(pool)
            .await?,
    )
}

async fn queued_reattests(pool: &PgPool, kb: Uuid) -> anyhow::Result<Vec<serde_json::Value>> {
    Ok(sqlx::query_scalar(
        "SELECT payload FROM jobs
          WHERE kind = 'reattest_facts' AND status = 'queued' AND payload->>'kb_id' = $1",
    )
    .bind(kb.to_string())
    .fetch_all(pool)
    .await?)
}

/// A document carrying its own date, with one chunk of its body; `sha256` is the file name, so uploading the same content hits it
async fn document(pool: &PgPool, kb: Uuid, own: DateTime<Utc>) -> anyhow::Result<(Uuid, Uuid)> {
    let (doc, chunk) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, doc_time, doc_time_source)
         VALUES ($1, $2, $3, $3, $4, 'content')",
    )
    .bind(doc)
    .bind(kb)
    .bind(format!("doc-{doc}.md"))
    .bind(own)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, $4)",
    )
    .bind(chunk)
    .bind(kb)
    .bind(doc)
    .bind("码表所属市场为全球。")
    .execute(pool)
    .await?;
    Ok((doc, chunk))
}

#[tokio::test]
async fn a_deletion_reattests_before_it_returns_and_an_upload_restore_queues_it(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(ORG)
        .execute(&pool)
        .await?;
    let (org, ws, kb, watch, market) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','{ORG}');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','{ORG}');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','{ORG}');
         INSERT INTO entities(id,kb_id,canonical_name) VALUES
             ('{watch}','{kb}','码表'), ('{market}','{kb}','全球');"
    ))
    .execute(&pool)
    .await?;

    let run = async {
        let (early, early_chunk) = document(&pool, kb, day(2020, 1, 1)).await?;
        let (_late, late_chunk) = document(&pool, kb, day(2021, 1, 1)).await?;
        let (fact, _) = graph::insert_open_statement(
            &pool,
            kb,
            watch,
            "所属市场为",
            FactObject::Entity(market),
            None,
            0.9,
        )
        .await?;
        for chunk in [early_chunk, late_chunk] {
            graph::add_evidence(&pool, fact, chunk, Some("码表所属市场为全球"), None).await?;
        }
        utopia_store::documents::reattest_facts(&pool, kb, &[fact]).await?;
        let before = attested(&pool, fact).await?;

        utopia_store::documents::delete(&pool, kb, early, None).await?;
        let after_delete = (
            attested(&pool, fact).await?,
            queued_reattests(&pool, kb).await?.len(),
        );

        let filename = format!("doc-{early}.md");
        utopia_store::documents::create_from_upload(
            &pool,
            kb,
            &filename,
            "text/markdown",
            1,
            &filename,
            None,
            None,
        )
        .await?;
        let queued = queued_reattests(&pool, kb).await?;
        let waiting = attested(&pool, fact).await?;
        let fact_ids: Vec<Uuid> = serde_json::from_value(queued[0]["fact_ids"].clone())?;
        utopia_store::documents::reattest_facts(&pool, kb, &fact_ids).await?;
        let after_job = attested(&pool, fact).await?;

        anyhow::Ok((before, after_delete, queued.len(), waiting, after_job))
    }
    .await;

    sqlx::query("DELETE FROM jobs WHERE kind IN ('reattest_facts', 'process_document') AND (payload->>'kb_id' = $1 OR payload->>'document_id' IN (SELECT id::text FROM documents WHERE kb_id = $2))")
        .bind(kb.to_string())
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    pool.close().await;
    let (before, after_delete, queued, waiting, after_job) = run?;
    assert_eq!(
        (before, after_delete, queued, waiting, after_job),
        (
            Some(day(2020, 1, 1)),
            (Some(day(2021, 1, 1)), 0),
            1,
            Some(day(2021, 1, 1)),
            Some(day(2020, 1, 1)),
        )
    );
    Ok(())
}
