//! 源的衰减开关与文档的检索记录（RC-003），打在真库上。
//!
//! 要钉住的行为：
//!
//! - **关着是默认**：新建的源 `decay` 为 NULL，没人要求就什么都不多写
//! - **开关是个对象**：`decay` 只收 JSON 对象，别的形状数据库拒绝
//! - **半衰期有范围**：`document_salience.half_life_days` 在 1 到 365 之外数据库拒绝
//! - **跟着文档走**：文档删掉，它的检索记录一起没
//!
//! 没有 `UTOPIA_DATABASE_URL` 时跳过而不是失败。自建自拆，绝不碰已有的库。

use sqlx::PgPool;
use uuid::Uuid;

#[tokio::test]
async fn a_source_decays_only_when_asked() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let (org, ws, kb, source, document) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'source-decay-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'source-decay-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'source-decay-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO sources (id, kb_id, kind, name) VALUES ($1, $2, 'folder', 'f')")
        .bind(source)
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status)
         VALUES ($1, $2, $3, 'a.md', 'x', 'ready')",
    )
    .bind(document)
    .bind(kb)
    .bind(source)
    .execute(&pool)
    .await?;

    let run = async {
        let decay: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT decay FROM sources WHERE id = $1")
                .bind(source)
                .fetch_one(&pool)
                .await?;
        assert_eq!(decay, None, "a new source does not decay");

        let not_an_object = sqlx::query("UPDATE sources SET decay = '[]'::jsonb WHERE id = $1")
            .bind(source)
            .execute(&pool)
            .await;
        assert!(not_an_object.is_err(), "decay is an object or nothing");
        sqlx::query("UPDATE sources SET decay = '{}'::jsonb WHERE id = $1")
            .bind(source)
            .execute(&pool)
            .await?;

        for half_life in [0.5_f32, 366.0] {
            let out_of_range = sqlx::query(
                "INSERT INTO document_salience (document_id, half_life_days) VALUES ($1, $2)",
            )
            .bind(document)
            .bind(half_life)
            .execute(&pool)
            .await;
            assert!(out_of_range.is_err(), "{half_life} days is out of range");
        }
        sqlx::query("INSERT INTO document_salience (document_id, half_life_days) VALUES ($1, 62)")
            .bind(document)
            .execute(&pool)
            .await?;

        sqlx::query("DELETE FROM documents WHERE id = $1")
            .bind(document)
            .execute(&pool)
            .await?;
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM document_salience WHERE document_id = $1")
                .bind(document)
                .fetch_one(&pool)
                .await?;
        assert_eq!(remaining, 0, "a deleted document leaves no record");
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
