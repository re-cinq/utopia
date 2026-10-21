//! 陈旧的文档（RC-003），打在真库上。
//!
//! 要钉住的行为：只有打开了衰减的源的文档才可能陈旧；久没被检索（没被检索过就看创建）
//! 超过源里的 `stale_after_days` 才算；检索一次就不陈旧了。
//!
//! 没有 `UTOPIA_DATABASE_URL` 时跳过而不是失败。自建自拆，绝不碰已有的库。

use sqlx::PgPool;
use utopia_store::decay::{record_retrievals, stale_documents};
use uuid::Uuid;

#[tokio::test]
async fn a_stale_document_is_found_among_a_decaying_sources() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (decaying, plain) = (Uuid::now_v7(), Uuid::now_v7());
    let (old, recent, read, unwatched) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'stale-document-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'stale-document-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'stale-document-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    for (id, decay) in [
        (decaying, Some(r#"{"stale_after_days": 20}"#)),
        (plain, None),
    ] {
        sqlx::query("INSERT INTO sources (id, kb_id, kind, name, decay) VALUES ($1, $2, 'folder', 'f', $3::jsonb)")
            .bind(id)
            .bind(kb)
            .bind(decay)
            .execute(&pool)
            .await?;
    }
    for (id, source, days_old) in [
        (old, decaying, 25),
        (recent, decaying, 10),
        (read, decaying, 25),
        (unwatched, plain, 400),
    ] {
        sqlx::query(
            "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status, created_at)
             VALUES ($1, $2, $3, $1::text, $1::text, 'ready', now() - make_interval(days => $4))",
        )
        .bind(id)
        .bind(kb)
        .bind(source)
        .bind(days_old)
        .execute(&pool)
        .await?;
    }

    let run = async {
        let all = [old, recent, read, unwatched];
        let stale = stale_documents(&pool, &all).await?;
        assert_eq!(
            stale,
            [old, read].into_iter().collect(),
            "past 20 days, in a decaying source"
        );

        record_retrievals(&pool, &[read]).await?;
        let stale = stale_documents(&pool, &all).await?;
        assert_eq!(
            stale,
            [old].into_iter().collect(),
            "a retrieval makes it fresh"
        );
        assert!(stale_documents(&pool, &[]).await?.is_empty());
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
