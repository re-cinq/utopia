//! 推送来的名字是客户端给的身份（#1293、#1296）：`statements` 来源的文档里一个有名字的实体，
//! 只按名字字面归并或新建。它不拿名字向量去找相近的名字，也不按名字互相包含去找简称、全称，
//! 两样都不给裁决器排对。机器起的名字共用一个前缀（`owner/repo:…`），向量上个个相近、
//! 字面上层层包含，排出来的对永远不是一个；而每个名字一次全库扫描，是推送时数据库的主要开销。
//!
//! 真库、脚本化的嵌入端点：所有名字的向量都一样，所以只要走了名字向量通道就一定排出一对。
//! 没有 `UTOPIA_DATABASE_URL` 时跳过而不是失败。自建自拆，绝不碰已有的库。
use super::*;
use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::Arc;

/// 一个已有实体 `known`（带名字向量）的库里，推一篇文档，里面只有一个名字 `pushed`。
/// 答：推送的名字成没成实体，以及这个库里排下的所有审核对的理由
async fn push_next_to(known: &str, pushed: &str) -> anyhow::Result<Option<(bool, Vec<String>)>> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(None);
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let (org, ws, kb, source, doc, chunk, known_id) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let payload: Value = json!({
        "external_id": "commit:pushed",
        "e": [[pushed, "SourceFile", true]],
        "s": [[null, pushed, "path", null, "src/x.ts", null, null, null]],
        "n": []
    });
    let text = serde_json::to_string_pretty(&payload)?;
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','pushed-names');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','pushed-names');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','pushed-names');
         INSERT INTO sources(id,kb_id,kind,name) VALUES ('{source}','{kb}','statements','lore');
         INSERT INTO documents(id,kb_id,filename,sha256,source_id) VALUES
             ('{doc}','{kb}','commit.json','x{doc}','{source}');
         INSERT INTO chunks(id,kb_id,document_id,seq,text) VALUES
             ('{chunk}','{kb}','{doc}',0,$${text}$$);"
    ))
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO entities(id,kb_id,canonical_name) VALUES ($1,$2,$3)")
        .bind(known_id)
        .bind(kb)
        .bind(known)
        .execute(&pool)
        .await?;
    let fact = utopia_store::names::record(&pool, kb, known_id, known, None, None)
        .await?
        .expect("a named entity gets a name fact");
    utopia_store::name_vectors::set(&pool, kb, &[(fact, known_id, vec![1.0, 0.0, 0.0, 0.0])])
        .await?;

    // 每个名字都是同一条向量：走了名字向量通道，`known` 就一定是 `pushed` 的近邻
    let router = Router::new().route(
        "/embeddings",
        post(|Json(body): Json<Value>| async move {
            let data: Vec<Value> = body["input"]
                .as_array()
                .expect("embedding input is an array")
                .iter()
                .map(|_| json!({"embedding": [1.0, 0.0, 0.0, 0.0]}))
                .collect();
            Json(json!({"data": data}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    utopia_store::settings::upsert(
        &pool,
        ws,
        None,
        None,
        None,
        Some(&base),
        None,
        Some("fake-embed"),
        Some(4),
    )
    .await?;

    let dir = tempfile::tempdir()?;
    let cfg = utopia_core::config::AppConfig {
        data_dir: dir.path().to_string_lossy().into_owned(),
        ..Default::default()
    };
    let search = Arc::new(utopia_search::SearchIndex::open(
        &dir.path().join("search"),
    )?);
    let state = AppState::new(pool.clone(), &cfg, search, "test-only".into());
    crate::extraction::extract_document(&state, doc, utopia_core::models::Proposer::default())
        .await?;

    let reasons: Vec<String> = sqlx::query_scalar(
        "SELECT reason FROM resolution_reviews WHERE kb_id = $1 ORDER BY reason",
    )
    .bind(kb)
    .fetch_all(&pool)
    .await?;
    let created: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM entities WHERE kb_id = $1 AND canonical_name = $2)",
    )
    .bind(kb)
    .bind(pushed)
    .fetch_one(&pool)
    .await?;

    server.abort();
    sqlx::query("DELETE FROM jobs WHERE payload->>'kb_id' = $1")
        .bind(kb.to_string())
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    Ok(Some((created, reasons)))
}

#[tokio::test]
async fn a_pushed_name_is_never_proposed_by_name_vector() -> anyhow::Result<()> {
    let Some((created, reasons)) =
        push_next_to("re-cinq/lore:src/a.ts", "re-cinq/lore:src/b.ts").await?
    else {
        return Ok(());
    };
    assert!(created, "the pushed name is still resolved into an entity");
    assert!(
        !reasons.iter().any(|r| r.starts_with("name_vector|")),
        "a pushed name proposes no name_vector pair: {reasons:?}"
    );
    Ok(())
}

/// 名字互相包含（`a.ts` 在 `a.ts#parse` 里）的既有实体，推送的名字也不排对（#1293 第二条）：
/// 那条召回是 `LIKE '%…%'`，用不上索引，每个名字扫全库的实体，实测中位 12 秒一次
#[tokio::test]
async fn a_pushed_name_is_never_proposed_by_containment() -> anyhow::Result<()> {
    let Some((created, reasons)) =
        push_next_to("re-cinq/lore:src/a.ts", "re-cinq/lore:src/a.ts#parse").await?
    else {
        return Ok(());
    };
    assert_eq!((created, reasons), (true, Vec::<String>::new()));
    Ok(())
}
