//! Relative time words count from the date of their own section (0064 decisions 1 and 4).
//!
//! One document holds two weekly reports, each section has its own reporting date, and each section contains a sentence "last week...". Extraction for each chunk
//! reports the date its own passage mentions (`t`); the server works out from the heading in that chunk's text the range this date governs; at parse time
//! each time phrase finds its starting point by the section it sits in. So two "last week" sentences come out a week apart, rather than both counting from the first
//! weekly report's date. The model's granularity `week` as reported falls in the day bucket. No separate question about the opening is asked afterwards.
//!
//! Nothing is passed between chunks: neither chunk's prompt contains the other's date (#588; this still holds when chunks are extracted in parallel).
//!
//! Scripted model endpoint plus a real database, the same approach as `extraction_open_known_flow_tests.rs`.
//! Skipped rather than failing without `UTOPIA_DATABASE_URL`. It creates and tears down its own database and never touches an existing one.
use super::*;
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Model {
    replies: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn reply(State(m): State<Model>, Json(body): Json<Value>) -> impl IntoResponse {
    m.requests.lock().unwrap().push(body);
    let text = {
        let mut replies = m.replies.lock().unwrap();
        if replies.is_empty() {
            panic!("unexpected model request");
        }
        replies.remove(0)
    };
    let frame = json!({"choices":[{"delta":{"content":text}}]});
    let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
    (
        [("content-type", "text/event-stream")],
        format!("data: {frame}\n\ndata: {done}\n\ndata: [DONE]\n\n"),
    )
}

const WEEK_35: &str =
    "# 周报汇编\n\n## 第35周周报\n\n提报日期：2026年8月28日\n\n上周，码表一代在韩国市场开售。";
const WEEK_36: &str =
    "# 周报汇编\n\n## 第36周周报\n\n提报日期：2026年9月4日\n\n上周，码表一代在泰国市场开售。";

#[tokio::test]
async fn last_week_is_counted_from_the_date_of_its_own_section() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let ids: Vec<Uuid> = (0..6).map(|_| Uuid::now_v7()).collect();
    let (org, ws, kb, doc, chunk0, chunk1) = (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5]);
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','time-context');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','time-context');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','time-context');
         INSERT INTO documents(id,kb_id,filename,sha256) VALUES ('{doc}','{kb}','周报汇编.md','x');"
    ))
    .execute(&pool)
    .await?;
    for (id, seq, text) in [(chunk0, 0, WEEK_35), (chunk1, 1, WEEK_36)] {
        sqlx::query("INSERT INTO chunks(id,kb_id,document_id,seq,text) VALUES ($1,$2,$3,$4,$5)")
            .bind(id)
            .bind(kb)
            .bind(doc)
            .bind(seq)
            .bind(text)
            .execute(&pool)
            .await?;
    }
    let anchored = json!({
        "kind": "anchored", "anchor": {"kind": "document"},
        "offset": {"count": 1, "unit": "week", "direction": "before"}
    });
    let model = Model {
        replies: Arc::new(Mutex::new(vec![
            json!({
                "e": [["码表一代", "产品", 1], ["韩国市场", "市场", 1]],
                "s": [["上周，码表一代在韩国市场开售。", "码表一代", "开售于", "韩国市场", null, null, "上周", null]],
                "n": [],
                "t": [["now", "提报日期", "2026年8月28日", {"y": 2026, "m": 8, "d": 28}, null]]
            })
            .to_string(),
            json!({
                "e": [["泰国市场", "市场", 1]],
                "s": [["上周，码表一代在泰国市场开售。", "码表一代", "开售于", "泰国市场", null, null, "上周", null]],
                "n": [],
                "t": [
                    ["now", "提报日期", "2026年9月4日", {"y": 2026, "m": 9, "d": 4}, null],
                    // Dates whose words are not in this chunk do not count
                    ["date", "数据截止", "2026年8月31日", {"y": 2026, "m": 8, "d": 31}, null]
                ]
            })
            .to_string(),
            // Parsing: the two "last week" wordings and sentences differ, so each is one entry; granularity is as reported: week
            json!({ "m": [[0, "point", anchored, "week"], [1, "point", anchored, "week"]] })
                .to_string(),
        ])),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let router = Router::new()
        .route("/chat/completions", post(reply))
        .with_state(model.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    utopia_store::settings::upsert(
        &pool,
        ws,
        Some(&endpoint),
        None,
        Some("scripted"),
        None,
        None,
        None,
        None,
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

    let run = async {
        crate::extraction::extract_document(&state, doc, utopia_core::models::Proposer::default())
            .await?;
        // Two starting points are stored on the document, each with its heading path; the one whose words are not in the chunk did not get in
        let context: DocumentDating = serde_json::from_value(
            sqlx::query_scalar::<_, Value>("SELECT time_context FROM documents WHERE id=$1")
                .bind(doc)
                .fetch_one(&pool)
                .await?,
        )?;
        let entries: Vec<(String, String, Vec<String>)> = context
            .entries
            .iter()
            .map(|e| (e.kind.clone(), e.words.clone(), e.scope.clone()))
            .collect();
        assert_eq!(
            entries,
            vec![
                (
                    "now".to_string(),
                    "2026年8月28日".to_string(),
                    vec!["周报汇编".to_string(), "第35周周报".to_string()]
                ),
                (
                    "now".to_string(),
                    "2026年9月4日".to_string(),
                    vec!["周报汇编".to_string(), "第36周周报".to_string()]
                ),
            ]
        );
        // Nothing is passed between chunks: the second chunk's prompt has nothing to rely on beyond the first chunk's date:
        // its own text already has its own heading and date
        let second = model.requests.lock().unwrap()[1]["messages"][1]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(second.contains("第36周周报") && second.contains("2026年9月4日"));

        resolve_document(&state, doc).await?;
        assert_eq!(
            model.requests.lock().unwrap().len(),
            3,
            "two extraction calls and one interpretation call: the opening is not asked for a date"
        );
        let dated: Vec<(String, Option<DateTime<Utc>>, Option<String>)> = sqlx::query_as(
            "SELECT o.canonical_name, f.valid_from, f.valid_from_grade
               FROM facts f JOIN entities o ON o.id = f.object_id
              WHERE f.kb_id = $1 AND f.layer = 'open' ORDER BY o.canonical_name",
        )
        .bind(kb)
        .fetch_all(&pool)
        .await?;
        let day = |y, m, d| Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap();
        // Each statement is attested by its own section's reporting date, including that date's name; not the moment the document is processed
        let attested: Vec<(String, Option<DateTime<Utc>>, Option<String>)> = sqlx::query_as(
            "SELECT o.canonical_name, f.attested_from, f.attested_by
               FROM facts f JOIN entities o ON o.id = f.object_id
              WHERE f.kb_id = $1 AND f.layer = 'open' ORDER BY o.canonical_name",
        )
        .bind(kb)
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            attested,
            vec![
                (
                    "泰国市场".to_string(),
                    Some(day(2026, 9, 4)),
                    Some("提报日期 2026年9月4日".to_string())
                ),
                (
                    "韩国市场".to_string(),
                    Some(day(2026, 8, 28)),
                    Some("提报日期 2026年8月28日".to_string())
                ),
            ]
        );
        assert_eq!(
            dated,
            vec![
                // Week 36's "last week" counts from September 4
                (
                    "泰国市场".to_string(),
                    Some(day(2026, 8, 28)),
                    Some("B".to_string())
                ),
                // Week 35's "last week" counts from August 28
                (
                    "韩国市场".to_string(),
                    Some(day(2026, 8, 21)),
                    Some("B".to_string())
                ),
            ]
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    server.abort();
    sqlx::query("DELETE FROM jobs WHERE payload->>'document_id'=$1 OR payload->>'kb_id'=$2")
        .bind(doc.to_string())
        .bind(kb.to_string())
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}

/// The document does not say which day it is: extraction reported "none", so it is none. No separate question about the opening; relative time words
/// cannot be anchored, so they wait (level C), and the statement has no attestation.
#[tokio::test]
async fn a_document_that_states_no_date_is_not_asked_for_one() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let ids: Vec<Uuid> = (0..5).map(|_| Uuid::now_v7()).collect();
    let (org, ws, kb, doc, chunk) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','time-context-undated');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','time-context-undated');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','time-context-undated');
         INSERT INTO documents(id,kb_id,filename,sha256) VALUES ('{doc}','{kb}','背景.md','x');
         INSERT INTO chunks(id,kb_id,document_id,seq,text) VALUES
             ('{chunk}','{kb}','{doc}',0,'# 项目背景\n\n去年，码表一代进入北美市场。');"
    ))
    .execute(&pool)
    .await?;
    let model = Model {
        replies: Arc::new(Mutex::new(vec![
            json!({
                "e": [["码表一代", "产品", 1], ["北美市场", "市场", 1]],
                "s": [["去年，码表一代进入北美市场。", "码表一代", "进入", "北美市场", null, null, "去年", null]],
                "n": [],
                "t": []
            })
            .to_string(),
            json!({ "m": [[0, "point", {
                "kind": "anchored", "anchor": {"kind": "document"},
                "offset": {"count": 1, "unit": "year", "direction": "before"}
            }, "year"]] })
            .to_string(),
        ])),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let router = Router::new()
        .route("/chat/completions", post(reply))
        .with_state(model.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    utopia_store::settings::upsert(
        &pool,
        ws,
        Some(&endpoint),
        None,
        Some("scripted"),
        None,
        None,
        None,
        None,
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

    let run = async {
        crate::extraction::extract_document(&state, doc, utopia_core::models::Proposer::default())
            .await?;
        resolve_document(&state, doc).await?;
        let requests = model.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            2,
            "one extraction call and one interpretation call; the opening is not read for a date"
        );
        for r in &requests {
            let system = r["messages"][0]["content"].as_str().unwrap_or("");
            assert!(
                !system.starts_with("You read the opening of a document"),
                "the dating prompt is not sent"
            );
        }
        let row: (Option<DateTime<Utc>>, Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT valid_from, valid_from_grade, attested_from FROM facts
              WHERE kb_id = $1 AND layer = 'open'",
        )
        .bind(kb)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            row,
            (None, Some("C".to_string()), None),
            "no anchor: the time word waits, and the statement has no attestation"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    server.abort();
    sqlx::query("DELETE FROM jobs WHERE payload->>'document_id'=$1 OR payload->>'kb_id'=$2")
        .bind(doc.to_string())
        .bind(kb.to_string())
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
