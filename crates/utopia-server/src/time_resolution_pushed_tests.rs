//! A pushed statement writes `when` / `ended` as a full date or a timestamp with a zone, and `resolve_time` reads it itself (#1089,
//! the landing point of the open question in 0054): no model is asked, and the task finishes normally even without a chat model.
//!
//! Same approach as `time_context_tests.rs`: scripted model endpoint, real database,
//! skipped rather than failing without `UTOPIA_DATABASE_URL`. It creates and tears down its own database and never touches an existing one.
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

/// Open rows of the statement: (object name, valid_from, from precision, valid_to, to precision, start level)
type FactRow = (
    String,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<String>,
);

/// One time mention: (word, slot, level, shape)
type MentionRow = (String, String, Option<String>, Option<String>);

/// One `statements` source, one pushed document (the chunk is the payload), and optionally one scripted chat endpoint.
/// `replies` None means no model is configured; that is exactly what this path's tests check
struct Rig {
    pool: sqlx::PgPool,
    state: AppState,
    org: Uuid,
    kb: Uuid,
    doc: Uuid,
    model: Option<Model>,
    server: Option<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Rig {
    async fn new(payload: Value, replies: Option<Vec<String>>) -> anyhow::Result<Option<Self>> {
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(None);
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        utopia_store::db::migrate(&pool).await?;
        let (org, ws, kb, source, doc, chunk) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        // The stored document is the one `push_statements` rebuilds itself: identity, date (if any),
        // then the contract's three arrays, in fixed key order
        let text = serde_json::to_string_pretty(&payload)?;
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','pushed-time');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','pushed-time');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','pushed-time');
             INSERT INTO sources(id,kb_id,kind,name) VALUES ('{source}','{kb}','statements','robot-1');
             INSERT INTO documents(id,kb_id,filename,sha256,source_id) VALUES
                 ('{doc}','{kb}','obs-1.json','x{doc}','{source}');
             INSERT INTO chunks(id,kb_id,document_id,seq,text) VALUES
                 ('{chunk}','{kb}','{doc}',0,$${text}$$);"
        ))
        .execute(&pool)
        .await?;
        let (model, server) = match replies {
            Some(replies) => {
                let model = Model {
                    replies: Arc::new(Mutex::new(replies)),
                    requests: Arc::new(Mutex::new(Vec::new())),
                };
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let endpoint = format!("http://{}", listener.local_addr()?);
                let router = Router::new()
                    .route("/chat/completions", post(reply))
                    .with_state(model.clone());
                let server =
                    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
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
                (Some(model), Some(server))
            }
            None => (None, None),
        };
        let dir = tempfile::tempdir()?;
        let cfg = utopia_core::config::AppConfig {
            data_dir: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let search = Arc::new(utopia_search::SearchIndex::open(
            &dir.path().join("search"),
        )?);
        let state = AppState::new(pool.clone(), &cfg, search, "test-only".into());
        // Extraction: the push path resolves against the contract and does not ask the model (0054); this step writes the statements and time words into the store
        crate::extraction::extract_document(&state, doc, utopia_core::models::Proposer::default())
            .await?;
        Ok(Some(Self {
            pool,
            state,
            org,
            kb,
            doc,
            model,
            server,
            _dir: dir,
        }))
    }

    /// Open rows of the statement, sorted by object name
    async fn fact_rows(&self) -> anyhow::Result<Vec<FactRow>> {
        Ok(sqlx::query_as(
            "SELECT o.canonical_name, f.valid_from, f.valid_from_precision,
                    f.valid_to, f.valid_to_precision, f.valid_from_grade
               FROM facts f JOIN entities o ON o.id = f.object_id
              WHERE f.kb_id = $1 AND f.layer = 'open' AND f.invalidated_at IS NULL
              ORDER BY o.canonical_name",
        )
        .bind(self.kb)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Time mention rows: (word, slot, level, reading shape)
    async fn mention_rows(&self) -> anyhow::Result<Vec<MentionRow>> {
        Ok(sqlx::query_as(
            "SELECT text, role, grade, shape FROM time_mentions
              WHERE kb_id = $1 ORDER BY role, text",
        )
        .bind(self.kb)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn requests(&self) -> Vec<Value> {
        match &self.model {
            Some(m) => m.requests.lock().unwrap().clone(),
            None => Vec::new(),
        }
    }

    async fn cleanup(self) -> anyhow::Result<()> {
        if let Some(server) = self.server {
            server.abort();
        }
        sqlx::query("DELETE FROM jobs WHERE payload->>'document_id'=$1 OR payload->>'kb_id'=$2")
            .bind(self.doc.to_string())
            .bind(self.kb.to_string())
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM organizations WHERE id=$1")
            .bind(self.org)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn payload(when: &str, ended: Option<&str>) -> Value {
    json!({
        "external_id": "obs-1",
        "doc_time": "2026-10-01T00:00:00Z",
        "e": [["cup-7", "cup", true], ["kitchen table", "table", true]],
        "s": [[null, "cup-7", "is on", "kitchen table", null, null, when, ended]],
        "n": []
    })
}

fn payload_two(when_a: &str, when_b: &str) -> Value {
    json!({
        "external_id": "obs-1",
        "doc_time": "2026-10-01T00:00:00Z",
        "e": [["cup-7", "cup", true], ["kitchen table", "table", true], ["counter", "table", true]],
        "s": [
            [null, "cup-7", "is on", "kitchen table", null, null, when_a, null],
            [null, "cup-7", "is on", "counter", null, null, when_b, null]
        ],
        "n": []
    })
}

fn day(y: i32, m: u32, d: u32) -> Option<DateTime<Utc>> {
    Some(Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap())
}

fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<DateTime<Utc>> {
    Some(Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap())
}

/// No chat model: the code reads the date the payload states as level A, at the precision the word gives, and the task finishes normally;
/// the word itself (`"2026-09-23"`) is kept as the mention and evidence
#[tokio::test]
async fn a_pushed_date_is_resolved_in_code_without_a_model() -> anyhow::Result<()> {
    let Some(rig) = Rig::new(payload("2026-09-23", None), None).await? else {
        return Ok(());
    };
    resolve_document(&rig.state, rig.doc).await?;
    assert_eq!(
        rig.fact_rows().await?,
        vec![(
            "kitchen table".to_string(),
            day(2026, 9, 23),
            Some("day".to_string()),
            day(2026, 9, 23),
            Some("day".to_string()),
            Some("A".to_string()),
        )],
        "a bare date is a single date at day precision, grade A"
    );
    assert_eq!(
        rig.mention_rows().await?,
        vec![(
            "2026-09-23".to_string(),
            "when".to_string(),
            Some("A".to_string()),
            Some("point".to_string())
        )],
        "the literal string stays as the mention"
    );
    rig.cleanup().await
}

/// Timestamps with `Z` and with an offset: precision to the second, and the value is the same instant converted to UTC. The offset is not just a label;
/// it actually goes into the value
#[tokio::test]
async fn a_pushed_timestamp_resolves_at_second_precision_and_its_zone_counts() -> anyhow::Result<()>
{
    let Some(rig) = Rig::new(payload("2026-09-23T08:14:03Z", None), None).await? else {
        return Ok(());
    };
    resolve_document(&rig.state, rig.doc).await?;
    assert_eq!(
        rig.fact_rows().await?,
        vec![(
            "kitchen table".to_string(),
            at(2026, 9, 23, 8, 14, 3),
            Some("second".to_string()),
            at(2026, 9, 23, 8, 14, 3),
            Some("second".to_string()),
            Some("A".to_string()),
        )]
    );
    rig.cleanup().await?;

    let Some(rig) = Rig::new(payload("2026-09-23T08:14:03+08:00", None), None).await? else {
        return Ok(());
    };
    resolve_document(&rig.state, rig.doc).await?;
    assert_eq!(
        rig.fact_rows().await?,
        vec![(
            "kitchen table".to_string(),
            // 08:14:03 at +08:00 is 00:14:03 UTC
            at(2026, 9, 23, 0, 14, 3),
            Some("second".to_string()),
            at(2026, 9, 23, 0, 14, 3),
            Some("second".to_string()),
            Some("A".to_string()),
        )]
    );
    rig.cleanup().await
}

/// The precision goes only as far as the word does: `T08` is hour, `T08:14` is minute; do not fill in
/// seconds it never wrote (0024)
#[tokio::test]
async fn the_precision_is_the_one_the_payload_writes() -> anyhow::Result<()> {
    for (when, expected, precision) in [
        ("2026-09-23", (0u32, 0u32, 0u32), "day"),
        ("2026-09-23T08Z", (8, 0, 0), "hour"),
        ("2026-09-23T08:14Z", (8, 14, 0), "minute"),
        ("2026-09-23T08:14:03Z", (8, 14, 3), "second"),
    ] {
        let Some(rig) = Rig::new(payload(when, None), None).await? else {
            return Ok(());
        };
        resolve_document(&rig.state, rig.doc).await?;
        let (h, m, s) = expected;
        assert_eq!(
            rig.fact_rows().await?,
            vec![(
                "kitchen table".to_string(),
                at(2026, 9, 23, h, m, s),
                Some(precision.to_string()),
                at(2026, 9, 23, h, m, s),
                Some(precision.to_string()),
                Some("A".to_string()),
            )],
            "{when} resolves at {precision}"
        );
        rig.cleanup().await?;
    }
    Ok(())
}

/// `when` plus `ended` is an interval: start is `when`, end is `ended`, each with its own precision
#[tokio::test]
async fn when_plus_ended_is_an_interval() -> anyhow::Result<()> {
    let Some(rig) = Rig::new(payload("2026-09-20", Some("2026-09-23T08:14:03Z")), None).await?
    else {
        return Ok(());
    };
    resolve_document(&rig.state, rig.doc).await?;
    assert_eq!(
        rig.fact_rows().await?,
        vec![(
            "kitchen table".to_string(),
            day(2026, 9, 20),
            Some("day".to_string()),
            at(2026, 9, 23, 8, 14, 3),
            Some("second".to_string()),
            Some("A".to_string()),
        )]
    );
    rig.cleanup().await
}

/// Bare years and year-months are not handled in this step: without a model they stay C, and the task still finishes normally;
/// natural-language time words also stay C
#[tokio::test]
async fn a_bare_year_or_year_month_or_words_are_left_for_a_model() -> anyhow::Result<()> {
    for when in ["2026", "2026-09", "last Tuesday"] {
        let Some(rig) = Rig::new(payload(when, None), None).await? else {
            return Ok(());
        };
        resolve_document(&rig.state, rig.doc).await?;
        assert_eq!(
            rig.mention_rows().await?,
            vec![(
                when.to_string(),
                "when".to_string(),
                Some("C".to_string()),
                None
            )],
            "{when:?} is not code-resolved"
        );
        assert_eq!(
            rig.fact_rows().await?,
            vec![(
                "kitchen table".to_string(),
                None,
                None,
                None,
                None,
                Some("C".to_string()),
            )],
            "{when:?}: the statement keeps its time words, unresolved, and the job ends"
        );
        rig.cleanup().await?;
    }
    Ok(())
}

/// One payload, two kinds of word: those that can be read resolve to A, and those that cannot stay C. Both outcomes can coexist in one document,
/// and the task still finishes normally
#[tokio::test]
async fn a_mixed_push_resolves_what_code_can_and_leaves_the_rest() -> anyhow::Result<()> {
    let Some(rig) = Rig::new(payload_two("2026-09-23", "last Tuesday"), None).await? else {
        return Ok(());
    };
    resolve_document(&rig.state, rig.doc).await?;
    assert_eq!(
        rig.mention_rows().await?,
        vec![
            (
                "2026-09-23".to_string(),
                "when".to_string(),
                Some("A".to_string()),
                Some("point".to_string())
            ),
            (
                "last Tuesday".to_string(),
                "when".to_string(),
                Some("C".to_string()),
                None
            ),
        ]
    );
    rig.cleanup().await
}

/// A document word that looks like RFC 3339 does not take the push path either: the criterion is the source kind, not the shape of the word.
/// With a model configured, this mention is still sent to ask: the source is not `statements`, and the code does not read it on its behalf
#[tokio::test]
async fn a_document_mention_that_looks_machine_readable_stays_on_the_model_path(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let (org, ws, kb, doc, chunk) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let text = "The cup was on the kitchen table at 2026-09-23T08:14:03Z.";
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','document-path');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','document-path');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','document-path');
         INSERT INTO documents(id,kb_id,filename,sha256) VALUES ('{doc}','{kb}','note.md','x{doc}');
         INSERT INTO chunks(id,kb_id,document_id,seq,text) VALUES
             ('{chunk}','{kb}','{doc}',0,$${text}$$);"
    ))
    .execute(&pool)
    .await?;
    let absolute = json!({
        "kind": "absolute",
        // In the contract, date parts use compact keys (y/m/d/h/min/s)
        "from": {"y": 2026, "m": 9, "d": 23, "h": 8, "min": 14, "s": 3}
    });
    let model = Model {
        replies: Arc::new(Mutex::new(vec![
            json!({
                "e": [["the cup", "cup", 1], ["the kitchen table", "table", 1]],
                "s": [["The cup was on the kitchen table at 2026-09-23T08:14:03Z.",
                       "the cup", "was on", "the kitchen table", null, null,
                       "2026-09-23T08:14:03Z", null]],
                "n": [],
                "t": []
            })
            .to_string(),
            // Explanation: the model answers as usual; this still goes through the model path, the code does not read it on its behalf
            json!({ "m": [[0, "point", absolute, "second"]] }).to_string(),
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
    crate::extraction::extract_document(&state, doc, utopia_core::models::Proposer::default())
        .await?;
    resolve_document(&state, doc).await?;
    // One extraction plus one explanation: a word that looks like a date is still asked of the model; the code does not read it for the model
    assert_eq!(
        model.requests.lock().unwrap().len(),
        2,
        "the mention went to the model: source identity, not string shape"
    );
    // Statement row: the one whose object is "the kitchen table" (name facts and other rows do not count)
    let row: (Option<DateTime<Utc>>, Option<String>) = sqlx::query_as(
        "SELECT f.valid_from, f.valid_from_grade FROM facts f
           JOIN entities o ON o.id = f.object_id
          WHERE f.kb_id = $1 AND f.layer = 'open' AND o.canonical_name = 'the kitchen table'",
    )
    .bind(kb)
    .fetch_one(&pool)
    .await?;
    assert_eq!(row, (at(2026, 9, 23, 8, 14, 3), Some("A".to_string())));
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
    Ok(())
}

/// Even with a model configured, mentions the code can read are not sent to the model (the reply's `anchored` entry is the model's
/// answer for "last week": it anchors on another mention, and that one is the one the code reads). Running
/// `resolve_time` again gives the same answer: timestamps are still not asked, and "last week" asked again gets the same position;
/// what the code reads has no model to override it
#[tokio::test]
async fn a_code_resolved_mention_is_never_asked_and_survives_reruns() -> anyhow::Result<()> {
    // Two time words: the timestamp the code can read is entry 0 (distinct); "last week" is entry 1, the only one that asks the model.
    // The model anchors it on entry 0: the code-read explanation and the model's explanation sit in the same anchor table
    let last_week = json!({ "m": [[1, "point", {
        "kind": "anchored", "anchor": {"kind": "mention", "id": 0},
        "offset": {"count": 1, "unit": "week", "direction": "before"}
    }, "week"]] })
    .to_string();
    let Some(rig) = Rig::new(
        payload_two("2026-09-23T08:14:03Z", "上周"),
        // Rerunning would ask the model about "last week" again: give it the same answer
        Some(vec![last_week.clone(), last_week]),
    )
    .await?
    else {
        return Ok(());
    };
    resolve_document(&rig.state, rig.doc).await?;
    // Extraction does not ask the model (push path), so there is only one explanation call here: only "last week" is asked.
    // The prompt's mentions section does not contain that timestamp; it appears as the payload body (sentence) in
    // the context of "last week", which is the document's own words. The point is that it was not asked as a mention
    let requests = rig.requests().await;
    assert_eq!(requests.len(), 1, "only the interpretation call");
    let asked = requests[0].to_string();
    // Only "last week" is asked in the mentions section (- id appears once); the timestamp is not in it:
    // it appears as the payload body in the context of "last week", which is the document's own words
    assert!(asked.contains("上周"), "asked: {asked}");
    assert_eq!(asked.matches("- id ").count(), 1, "asked: {asked}");

    let first = rig.fact_rows().await?;
    assert_eq!(
        first,
        vec![
            // "last week" anchored on 9-23T08:14:03Z: the week before, 2026-09-16, day precision, level B
            (
                "counter".to_string(),
                day(2026, 9, 16),
                Some("day".to_string()),
                day(2026, 9, 16),
                Some("day".to_string()),
                Some("B".to_string()),
            ),
            (
                "kitchen table".to_string(),
                at(2026, 9, 23, 8, 14, 3),
                Some("second".to_string()),
                at(2026, 9, 23, 8, 14, 3),
                Some("second".to_string()),
                Some("A".to_string()),
            ),
        ],
        "code-resolved, precision as written"
    );
    // Rerun: same answer; the scripted endpoint has a second reply waiting for "last week"
    resolve_document(&rig.state, rig.doc).await?;
    let requests = rig.requests().await;
    assert_eq!(
        requests.len(),
        2,
        "the rerun asked the model for 上周 again"
    );
    assert_eq!(
        requests[1].to_string().matches("- id ").count(),
        1,
        "the rerun also asked only about 上周"
    );
    assert_eq!(
        rig.fact_rows().await?,
        first,
        "a rerun gives the same answer"
    );
    rig.cleanup().await
}
