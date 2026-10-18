//! 检索交得出它的分数、翻得了页、收个人令牌（#1271）。
//!
//! 夹具：一个库、一个 statements 来源推来的一篇文档、五个都含 apple 的分块，
//! 只进了全文索引；工作区没配嵌入模型，所以只有 BM25 那一路。
use crate::state::AppState;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    app: axum::Router,
    org: Uuid,
    kb: Uuid,
    source: Uuid,
    doc: Uuid,
    /// BM25 排第一的那块
    best: Uuid,
    token: String,
    search: Arc<utopia_search::SearchIndex>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> anyhow::Result<Option<Self>> {
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(None);
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        utopia_store::db::migrate(&pool).await?;
        let (org, ws, kb, user, source, doc) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','search-test');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','search-test');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','search-test');
             INSERT INTO users(id,org_id,email,display_name,password_hash) VALUES
                 ('{user}','{org}','{user}@search.test','Reader','unused');
             INSERT INTO memberships(user_id,workspace_id,role) VALUES ('{user}','{ws}','viewer');
             INSERT INTO sources(id,kb_id,kind,name) VALUES ('{source}','{kb}','statements','ci');
             INSERT INTO documents(id,kb_id,source_id,external_key,filename,sha256)
                 VALUES ('{doc}','{kb}','{source}','statements:obs-7','obs-7.md',repeat('0',64));"
        ))
        .execute(&pool)
        .await?;
        let texts = [
            "apple apple apple",
            "apple orchard in the north valley",
            "an apple pie recipe from the old family book",
            "the apple harvest report for the second quarter of the year",
            "one apple fell from the tree behind the long stone wall by the river",
        ];
        let chunks: Vec<(String, String)> = texts
            .iter()
            .map(|text| (Uuid::now_v7().to_string(), text.to_string()))
            .collect();
        for (seq, (id, text)) in chunks.iter().enumerate() {
            sqlx::query("INSERT INTO chunks(id,kb_id,document_id,seq,text) VALUES($1,$2,$3,$4,$5)")
                .bind(id.parse::<Uuid>()?)
                .bind(kb)
                .bind(doc)
                .bind(seq as i32)
                .bind(text)
                .execute(&pool)
                .await?;
        }
        let dir = tempfile::tempdir()?;
        let cfg = utopia_core::config::AppConfig {
            data_dir: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let search = Arc::new(utopia_search::SearchIndex::open(
            &dir.path().join("search"),
        )?);
        search.reindex_document(&kb.to_string(), &doc.to_string(), &chunks)?;
        let token = utopia_store::tokens::issue(&pool, user, "ci", "read", None, None)
            .await?
            .1;
        let state = AppState::new(pool.clone(), &cfg, search.clone(), "test-only".into());
        Ok(Some(Self {
            app: super::router(state, &cfg),
            pool,
            org,
            kb,
            source,
            doc,
            best: chunks[0].0.parse()?,
            token,
            search,
            _dir: dir,
        }))
    }

    /// 本库 statements 来源里再推一篇一块的文档，进全文索引；交回那一块的 id
    async fn push(&self, external_id: &str, text: &str) -> anyhow::Result<Uuid> {
        let (doc, chunk) = (Uuid::now_v7(), Uuid::now_v7());
        sqlx::query(
            "INSERT INTO documents(id,kb_id,source_id,external_key,filename,sha256)
             VALUES ($1,$2,$3,$4,$5,$1::text)",
        )
        .bind(doc)
        .bind(self.kb)
        .bind(self.source)
        .bind(format!("statements:{external_id}"))
        .bind(external_id)
        .execute(&self.pool)
        .await?;
        sqlx::query("INSERT INTO chunks(id,kb_id,document_id,seq,text) VALUES($1,$2,$3,0,$4)")
            .bind(chunk)
            .bind(self.kb)
            .bind(doc)
            .bind(text)
            .execute(&self.pool)
            .await?;
        let chunks = [(chunk.to_string(), text.to_string())];
        self.search
            .reindex_document(&self.kb.to_string(), &doc.to_string(), &chunks)?;
        Ok(chunk)
    }

    async fn search(&self, body: Value) -> anyhow::Result<(StatusCode, Value)> {
        let request = Request::post(format!("/api/v1/kbs/{}/search", self.kb))
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))?;
        let response = self.app.clone().oneshot(request).await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
        Ok((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ))
    }

    async fn cleanup(self) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM organizations WHERE id=$1")
            .bind(self.org)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[tokio::test]
async fn a_read_token_gets_each_hit_with_its_score_and_rank() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let (status, body) = f.search(json!({ "q": "apple", "top_k": 1 })).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({
            "results": [{
                "id": f.best,
                "document_id": f.doc,
                "seq": 0,
                "text": "apple apple apple",
                "filename": "obs-7.md",
                "source_id": f.source,
                "external_id": "obs-7",
                "score": 1.0 / 61.0,
                "bm25_rank": 1,
                "vector_rank": null,
            }],
            "offset": 0,
            "top_k": 1,
            "has_more": true,
            "vector_used": false,
        })
    );
    f.cleanup().await
}

#[tokio::test]
async fn pages_of_two_cover_five_hits_without_overlap() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let mut seen = Vec::new();
    let mut more = Vec::new();
    for offset in [0, 2, 4] {
        let (status, body) = f
            .search(json!({ "q": "apple", "top_k": 2, "offset": offset }))
            .await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        for hit in body["results"].as_array().into_iter().flatten() {
            seen.push((hit["bm25_rank"].clone(), hit["id"].clone()));
        }
        more.push(body["has_more"].clone());
    }
    let ranks: Vec<Value> = seen.iter().map(|(rank, _)| rank.clone()).collect();
    assert_eq!(ranks, (1..=5).map(Value::from).collect::<Vec<_>>());
    let mut ids: Vec<String> = seen.iter().map(|(_, id)| id.to_string()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 5, "no hit appears on two pages");
    assert_eq!(more, vec![json!(true), json!(true), json!(false)]);
    f.cleanup().await
}

#[tokio::test]
async fn a_page_past_the_hundredth_hit_is_refused() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let (status, body) = f
        .search(json!({ "q": "apple", "top_k": 50, "offset": 51 }))
        .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "page_too_deep");
    f.cleanup().await
}

fn ids(body: &Value) -> Vec<Value> {
    body["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hit| hit["id"].clone())
        .collect()
}

/// 三十篇代码条目都比那条规格陈述更像 apple；每路只召回 24 条，召回完再筛就什么也
/// 剩不下。过滤在召回之内，那条陈述才回得来
#[tokio::test]
async fn a_prefix_finds_the_statement_that_thirty_better_matches_crowd_out() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    for n in 0..30 {
        f.push(&format!("code:src/a_{n}.rs"), "apple apple apple apple")
            .await?;
    }
    let spec = f
        .push(
            "doc:specs/x_y.md",
            "one apple among many quiet words about the spring planting schedule",
        )
        .await?;

    let (status, body) = f.search(json!({ "q": "apple", "top_k": 10 })).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!ids(&body).contains(&json!(spec)), "{body}");

    let (status, body) = f
        .search(json!({
            "q": "apple",
            "top_k": 10,
            "source_id": f.source,
            "external_id_prefixes": ["doc:"],
        }))
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(ids(&body), vec![json!(spec)]);

    // `_` 与 `%` 是路径里的字，不是通配符：没有哪篇以 `doc:specs/x%` 开头
    let (status, body) = f
        .search(json!({
            "q": "apple",
            "source_id": f.source,
            "external_id_prefixes": ["doc:specs/x%"],
        }))
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(ids(&body), Vec::<Value>::new());
    f.cleanup().await
}

#[tokio::test]
async fn prefixes_without_a_source_are_refused() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let (status, body) = f
        .search(json!({ "q": "apple", "external_id_prefixes": ["doc:"] }))
        .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "prefixes_need_source");
    f.cleanup().await
}

#[tokio::test]
async fn a_source_of_another_base_is_not_found() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let (other_kb, other_source) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::raw_sql(&format!(
        "INSERT INTO knowledge_bases(id,workspace_id,name)
             SELECT '{other_kb}', workspace_id, 'other' FROM knowledge_bases WHERE id = '{kb}';
         INSERT INTO sources(id,kb_id,kind,name) VALUES ('{other_source}','{other_kb}','statements','ci');",
        kb = f.kb,
    ))
    .execute(&f.pool)
    .await?;
    let (status, body) = f
        .search(json!({ "q": "apple", "source_id": other_source }))
        .await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    f.cleanup().await
}
