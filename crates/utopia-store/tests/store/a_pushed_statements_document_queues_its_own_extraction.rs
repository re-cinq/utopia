//! A document of a `statements` source is extracted without the chat model, so its extraction is queued as `extract_statements`
//! (claimed ahead of model work) rather than `extract_document`; every other document keeps `extract_document`.

use sqlx::PgPool;
use utopia_store::{documents, jobs};
use uuid::Uuid;

const EXTRACTION_KINDS: &str = "ARRAY['extract_document', 'extract_statements']";

struct Seeded {
    kb: Uuid,
    source: Uuid,
    documents: Vec<Uuid>,
}

async fn seed(
    pool: &PgPool,
    source_kind: Option<&str>,
    document_count: usize,
) -> anyhow::Result<Seeded> {
    let [org, ws, kb, source] = std::array::from_fn(|_| Uuid::now_v7());
    let mut documents: Vec<Uuid> = (0..document_count).map(|_| Uuid::now_v7()).collect();
    documents.sort_by_key(Uuid::to_string);
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'pushed')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'pushed')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'pushed')")
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    if let Some(kind) = source_kind {
        sqlx::query("INSERT INTO sources (id, kb_id, kind, name) VALUES ($1, $2, $3, 'pushed')")
            .bind(source)
            .bind(kb)
            .bind(kind)
            .execute(pool)
            .await?;
    }
    for document in &documents {
        sqlx::query(
            "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status)
             VALUES ($1, $2, $3, 'a.md', $4, 'ready')",
        )
        .bind(document)
        .bind(kb)
        .bind(source_kind.map(|_| source))
        .bind(format!("sha-{document}"))
        .execute(pool)
        .await?;
    }
    Ok(Seeded {
        kb,
        source,
        documents,
    })
}

async fn queue_legacy(pool: &PgPool, kind: &str, document: Uuid) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO jobs (kind, payload) VALUES ($1, $2)")
        .bind(kind)
        .bind(document_payload(document))
        .execute(pool)
        .await?;
    Ok(())
}

fn document_payload(document: Uuid) -> serde_json::Value {
    serde_json::json!({ "document_id": document })
}

async fn queued(
    pool: &PgPool,
    seeded: &Seeded,
) -> anyhow::Result<Vec<(String, serde_json::Value)>> {
    Ok(sqlx::query_as(&format!(
        "SELECT kind, payload FROM jobs
          WHERE status = 'queued' AND kind = ANY({EXTRACTION_KINDS})
            AND payload->>'document_id' = ANY($1)
          ORDER BY payload->>'document_id', id"
    ))
    .bind(
        seeded
            .documents
            .iter()
            .map(Uuid::to_string)
            .collect::<Vec<_>>(),
    )
    .fetch_all(pool)
    .await?)
}

async fn tidy(pool: &PgPool, seeded: &Seeded) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM jobs WHERE payload->>'document_id' = ANY($1)")
        .bind(
            seeded
                .documents
                .iter()
                .map(Uuid::to_string)
                .collect::<Vec<_>>(),
        )
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(seeded.kb)
        .execute(pool)
        .await?;
    Ok(())
}

fn expected(kind: &str, seeded: &Seeded) -> Vec<(String, serde_json::Value)> {
    seeded
        .documents
        .iter()
        .map(|document| (kind.to_string(), document_payload(*document)))
        .collect()
}

#[tokio::test]
async fn enqueueing_a_statements_document_queues_extract_statements() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let seeded = seed(&pool, Some("statements"), 1).await?;

    jobs::enqueue_extraction(&pool, document_payload(seeded.documents[0]), 3).await?;
    let rows = queued(&pool, &seeded).await?;

    tidy(&pool, &seeded).await?;
    assert_eq!(rows, expected("extract_statements", &seeded));
    Ok(())
}

#[tokio::test]
async fn enqueueing_an_api_document_queues_extract_document() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let seeded = seed(&pool, Some("api"), 1).await?;

    jobs::enqueue_extraction(&pool, document_payload(seeded.documents[0]), 3).await?;
    let rows = queued(&pool, &seeded).await?;

    tidy(&pool, &seeded).await?;
    assert_eq!(rows, expected("extract_document", &seeded));
    Ok(())
}

#[tokio::test]
async fn enqueueing_a_document_with_no_source_queues_extract_document() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let seeded = seed(&pool, None, 1).await?;

    jobs::enqueue_extraction(&pool, document_payload(seeded.documents[0]), 3).await?;
    let rows = queued(&pool, &seeded).await?;

    tidy(&pool, &seeded).await?;
    assert_eq!(rows, expected("extract_document", &seeded));
    Ok(())
}

#[tokio::test]
async fn enqueueing_a_statements_document_twice_leaves_one_extract_statements() -> anyhow::Result<()>
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let seeded = seed(&pool, Some("statements"), 1).await?;
    let payload = document_payload(seeded.documents[0]);

    jobs::enqueue_extraction(&pool, payload.clone(), 3).await?;
    jobs::enqueue_extraction(&pool, payload, 3).await?;
    let rows = queued(&pool, &seeded).await?;

    tidy(&pool, &seeded).await?;
    assert_eq!(rows, expected("extract_statements", &seeded));
    Ok(())
}

#[tokio::test]
async fn enqueueing_a_statements_document_with_an_old_extract_document_queued_adds_nothing(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let seeded = seed(&pool, Some("statements"), 1).await?;
    queue_legacy(&pool, "extract_document", seeded.documents[0]).await?;

    jobs::enqueue_extraction(&pool, document_payload(seeded.documents[0]), 3).await?;
    let rows = queued(&pool, &seeded).await?;

    tidy(&pool, &seeded).await?;
    assert_eq!(rows, expected("extract_document", &seeded));
    Ok(())
}

#[tokio::test]
async fn a_manual_extract_of_a_statements_document_replaces_its_queued_jobs_with_one_extract_statements(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let seeded = seed(&pool, Some("statements"), 1).await?;
    queue_legacy(&pool, "extract_document", seeded.documents[0]).await?;
    queue_legacy(&pool, "extract_statements", seeded.documents[0]).await?;

    documents::queue_extraction_one(&pool, seeded.documents[0]).await?;
    let rows = queued(&pool, &seeded).await?;

    tidy(&pool, &seeded).await?;
    assert_eq!(rows, expected("extract_statements", &seeded));
    Ok(())
}

#[tokio::test]
async fn a_manual_extract_of_an_api_document_leaves_one_extract_document() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let seeded = seed(&pool, Some("api"), 1).await?;
    queue_legacy(&pool, "extract_document", seeded.documents[0]).await?;

    documents::queue_extraction_one(&pool, seeded.documents[0]).await?;
    let rows = queued(&pool, &seeded).await?;

    tidy(&pool, &seeded).await?;
    assert_eq!(rows, expected("extract_document", &seeded));
    Ok(())
}

#[tokio::test]
async fn re_extracting_a_statements_source_leaves_one_extract_statements_per_document(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let seeded = seed(&pool, Some("statements"), 2).await?;
    queue_legacy(&pool, "extract_document", seeded.documents[0]).await?;

    documents::queue_extraction(&pool, seeded.kb, Some(seeded.source)).await?;
    let rows = queued(&pool, &seeded).await?;

    tidy(&pool, &seeded).await?;
    assert_eq!(rows, expected("extract_statements", &seeded));
    Ok(())
}
