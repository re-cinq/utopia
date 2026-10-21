//! 每日扫描把超出容量的源淘汰回去（RC-003），打在真库上。
//!
//! 要钉住的行为：分最低的文档被删掉（可恢复）、从全文索引里拿掉，留下的原样，
//! 并记一笔无人操作的审计；没超出的源什么都不动。
//!
//! 没有 `UTOPIA_DATABASE_URL` 时跳过而不是失败。自建自拆，绝不碰已有的库。

use std::sync::Arc;
use uuid::Uuid;

#[tokio::test]
async fn a_sweep_removes_the_lowest_scoring_document_from_the_base_and_the_index(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let (org, ws, kb, source) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (weak, strong) = (Uuid::now_v7(), Uuid::now_v7());
    let (weak_chunk, strong_chunk) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations(id,name) VALUES($1,'decay-sweep-server-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces(id,org_id,name) VALUES($1,$2,'decay-sweep-server-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases(id,workspace_id,name) VALUES($1,$2,'decay-sweep-server-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO sources(id,kb_id,kind,name,decay)
         VALUES($1,$2,'folder','f','{\"max_items\":1,\"min_age_days\":0}'::jsonb)",
    )
    .bind(source)
    .bind(kb)
    .execute(&pool)
    .await?;
    for (doc, chunk, name, text) in [
        (weak, weak_chunk, "notes/a", "apple orchard harvest report"),
        (
            strong,
            strong_chunk,
            "decision/b",
            "bridge concrete inspection",
        ),
    ] {
        sqlx::query(
            "INSERT INTO documents(id,kb_id,source_id,filename,sha256,created_at)
             VALUES($1,$2,$3,$4,$1::text, now() - interval '100 days')",
        )
        .bind(doc)
        .bind(kb)
        .bind(source)
        .bind(name)
        .execute(&pool)
        .await?;
        sqlx::query("INSERT INTO chunks(id,kb_id,document_id,seq,text) VALUES($1,$2,$3,0,$4)")
            .bind(chunk)
            .bind(kb)
            .bind(doc)
            .bind(text)
            .execute(&pool)
            .await?;
    }

    let dir = std::env::temp_dir().join(format!("utopia-decay-sweep-{kb}"));
    let cfg = utopia_core::config::AppConfig {
        data_dir: dir.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let search = Arc::new(utopia_search::SearchIndex::open(&dir.join("search"))?);
    for (doc, chunk, text) in [
        (weak, weak_chunk, "apple orchard harvest report"),
        (strong, strong_chunk, "bridge concrete inspection"),
    ] {
        search.reindex_document(
            &kb.to_string(),
            &doc.to_string(),
            &[(chunk.to_string(), text.into())],
        )?;
    }
    let state = crate::state::AppState::new(pool.clone(), &cfg, search.clone(), "test-only".into());

    let run = async {
        super::sweep_source(&state, source).await?;

        let deleted: Vec<(Uuid, bool)> = sqlx::query_as(
            "SELECT id, deleted_at IS NOT NULL FROM documents WHERE source_id = $1 ORDER BY filename",
        )
        .bind(source)
        .fetch_all(&pool)
        .await?;
        assert_eq!(deleted, vec![(strong, false), (weak, true)]);
        assert!(search.search(&kb.to_string(), "apple", 5, None)?.is_empty());
        assert_eq!(search.search(&kb.to_string(), "bridge", 5, None)?.len(), 1);
        let audited: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE target_id = $1 AND action = 'decay.evict'
               AND actor_id IS NULL",
        )
        .bind(source)
        .fetch_one(&pool)
        .await?;
        assert_eq!(audited, 1);

        super::sweep_source(&state, source).await?;
        let still_live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM documents WHERE source_id = $1 AND deleted_at IS NULL",
        )
        .bind(source)
        .fetch_one(&pool)
        .await?;
        assert_eq!(still_live, 1, "a source at its cap loses nothing more");
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    let _ = std::fs::remove_dir_all(&dir);
    run
}
