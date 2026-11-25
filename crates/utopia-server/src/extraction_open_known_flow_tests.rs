//! Entities recognized by earlier chunks are recognized by later ones (#588 cut 1).
//!
//! Extraction works chunk by chunk: each chunk's prompt lists the entities that earlier chunks of this document already recognized (`known`), so the model
//! does not need to list an entity again when a later chunk mentions it; the write matches it back to the same row. Concurrent chunk model calls (cut 2) must keep
//! exactly this, so pin it down first: chunk one lists `Acme`, chunk two's reply does **not** list it and only mentions it in the statements,
//! and two statements hang on the same entity, with `- Acme (organization)` in chunk two's prompt.
//! If chunk two listed `Acme` again itself, looking it up by name in the store would still find it, and then the test would not be measuring `known`.
//!
//! With `extract_known_in_prompt` turned off (the recall bench uses this to measure the effect of concurrency), the prompt has no such line,
//! and the write still matches it back.
//!
//! Scripted model endpoint plus a real database, the same approach as `phrase_alignment_tests.rs`.
//! Skipped rather than failing without `UTOPIA_DATABASE_URL`. It creates and tears down its own database and never touches an existing one.
use super::*;
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

#[derive(Default)]
struct Pause {
    arrived: Notify,
    resume: Notify,
}

#[derive(Clone)]
struct Model {
    replies: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    pause_second: Option<Arc<Pause>>,
}

/// The open-extraction reply: the script's string is the `delta.content` of the chat stream.
/// Same tier as `errata_tests.rs::reply` (text/event-stream, closed with `usage`),
/// but `chat_retrying_rate_limits_at` reads only the `content` part when parsing the stream.
async fn reply(State(m): State<Model>, Json(body): Json<Value>) -> impl IntoResponse {
    let request = {
        let mut requests = m.requests.lock().unwrap();
        requests.push(body);
        requests.len()
    };
    if let Some(pause) = m.pause_second.filter(|_| request == 2) {
        pause.arrived.notify_one();
        pause.resume.notified().await;
    }
    let text = {
        let mut replies = m.replies.lock().unwrap();
        if replies.is_empty() {
            panic!("unexpected model request");
        }
        replies.remove(0)
    };
    let frame = json!({"choices":[{"delta":{"content":text}}]});
    let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
    let usage = json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":34}});
    (
        [("content-type", "text/event-stream")],
        format!("data: {frame}\n\ndata: {done}\n\ndata: {usage}\n\ndata: [DONE]\n\n"),
    )
}

struct Fx {
    pool: sqlx::PgPool,
    state: AppState,
    org: Uuid,
    kb: Uuid,
    doc: Uuid,
    model: Model,
    _server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Fx {
    async fn new(
        replies: Vec<String>,
        known_in_prompt: bool,
        pause_second: Option<Arc<Pause>>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(None);
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        utopia_store::db::migrate(&pool).await?;
        let ids: Vec<Uuid> = (0..6).map(|_| Uuid::now_v7()).collect();
        let (org, ws, kb, doc, chunk0, chunk1) = (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5]);
        // Only locally generated UUIDs are inserted
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','open-known-flow');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','open-known-flow');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','open-known-flow');
             INSERT INTO documents(id,kb_id,filename,sha256,status) VALUES ('{doc}','{kb}','acme.txt','x','ready');
             INSERT INTO chunks(id,kb_id,document_id,seq,text) VALUES
                 ('{chunk0}','{kb}','{doc}',0,'Acme is based in London.'),
                 ('{chunk1}','{kb}','{doc}',1,'Acme runs the harbour facility.');"
        ))
        .execute(&pool)
        .await?;
        let model = Model {
            replies: Arc::new(Mutex::new(replies)),
            requests: Arc::new(Mutex::new(Vec::new())),
            pause_second,
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
            extract_known_in_prompt: known_in_prompt,
            ..Default::default()
        };
        let search = Arc::new(utopia_search::SearchIndex::open(
            &dir.path().join("search"),
        )?);
        let state = AppState::new(pool.clone(), &cfg, search, "test-only".into());
        Ok(Some(Self {
            pool,
            state,
            org,
            kb,
            doc,
            model,
            _server: server,
            _dir: dir,
        }))
    }

    async fn cleanup(&self) -> anyhow::Result<()> {
        self._server.abort();
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

/// Replies for the two chunks: chunk one lists `Acme`; chunk two does not list it and mentions it only in statements.
/// The 8 cells of a compact array are `[quote, subject, phrase, object, value, qualifiers, when, ended]`
fn replies() -> Vec<String> {
    vec![
        json!({
            "e": [["Acme", "organization", 1]],
            "s": [["Acme is based in London.", "Acme", "based in", null, "London", null, null, null]],
            "n": []
        })
        .to_string(),
        json!({
            "e": [],
            "s": [["Acme runs the harbour facility.", "Acme", "runs", null, "harbour", null, null, null]],
            "n": []
        })
        .to_string(),
    ]
}

/// After extraction: there is only one `Acme`, and both statements hang on it. Returns chunk two's prompt
async fn extract_and_read_the_second_prompt(f: &Fx) -> anyhow::Result<String> {
    crate::extraction::extract_document(&f.state, f.doc, utopia_core::models::Proposer::default())
        .await?;
    let acme: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM entities WHERE kb_id=$1 AND canonical_name='Acme'")
            .bind(f.kb)
            .fetch_all(&f.pool)
            .await?;
    anyhow::ensure!(
        acme.len() == 1,
        "one Acme across both chunks, got {}",
        acme.len()
    );
    let subjects: Vec<Uuid> =
        sqlx::query_scalar("SELECT subject_id FROM facts WHERE kb_id=$1 AND layer='open'")
            .bind(f.kb)
            .fetch_all(&f.pool)
            .await?;
    anyhow::ensure!(
        subjects == vec![acme[0], acme[0]],
        "both statements hang on the one Acme: {subjects:?}"
    );
    let requests = f.model.requests.lock().unwrap();
    anyhow::ensure!(
        requests.len() == 2,
        "one request a chunk, got {}",
        requests.len()
    );
    Ok(requests[1]["messages"][1]["content"]
        .as_str()
        .unwrap_or("")
        .to_string())
}

#[tokio::test]
async fn a_thing_listed_by_an_earlier_chunk_reaches_a_later_one_via_known() -> anyhow::Result<()> {
    let Some(f) = Fx::new(replies(), true, None).await? else {
        return Ok(());
    };
    let run = extract_and_read_the_second_prompt(&f).await;
    f.cleanup().await?;
    let prompt = run?;
    assert!(
        prompt.contains("- Acme (organization)"),
        "the second chunk is told what the first one listed: {prompt}"
    );
    Ok(())
}

/// Knob turned off: chunk two's prompt no longer lists the previously recognized entities, yet the write still matches the same `Acme`
#[tokio::test]
async fn with_known_left_out_of_the_prompt_a_later_chunk_still_lands_on_the_same_entity(
) -> anyhow::Result<()> {
    let Some(f) = Fx::new(replies(), false, None).await? else {
        return Ok(());
    };
    let run = extract_and_read_the_second_prompt(&f).await;
    f.cleanup().await?;
    let prompt = run?;
    assert!(
        !prompt.contains("- Acme (organization)"),
        "nothing listed as known: {prompt}"
    );
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Interruption {
    RebuildInFlight,
    ReextractInFlight,
    RebuildDuringApply,
    PurgeDuringApply,
}

// The trigger DDL on facts locks the whole table, so the two barrier cases must run serially until cleanup finishes.
static FACTS_TRIGGER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Chunk one leaves behind a cached Acme id; a rebuild deletes it, while a single-document re-extract keeps the entity and resets the chunking.
/// The barrier for the write phase sits on the actual INSERT, to ensure the rebuild happens after the epoch check.
async fn interrupt_the_second_chunk(interruption: Interruption) -> anyhow::Result<()> {
    let during_apply = matches!(
        interruption,
        Interruption::RebuildDuringApply | Interruption::PurgeDuringApply
    );
    let _trigger_guard = if during_apply {
        Some(FACTS_TRIGGER_LOCK.lock().await)
    } else {
        None
    };
    let pause = Arc::new(Pause::default());
    let Some(f) = Fx::new(replies(), true, Some(pause.clone())).await? else {
        return Ok(());
    };
    let rebuild = interruption != Interruption::ReextractInFlight;
    let barrier = format!("pause_extraction_{}", f.doc.simple());
    let mut gate = None;
    let mut gate_pid = 0i32;
    if during_apply {
        let mut tx = f.pool.begin().await?;
        gate_pid = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query("SELECT pg_advisory_xact_lock(1361, $1)")
            .bind(gate_pid)
            .execute(&mut *tx)
            .await?;
        gate = Some(tx);
    }
    let run = tokio::time::timeout(Duration::from_secs(30), async {
        let extraction = crate::extraction::extract_document(&f.state, f.doc, Proposer::default());
        tokio::pin!(extraction);
        tokio::select! {
            result = &mut extraction => {
                result?;
                anyhow::bail!("extraction finished before the second reply was released");
            }
            _ = pause.arrived.notified() => {}
        }
        if during_apply {
            // Identifiers and conditions all come from this fixture's UUIDs; other knowledge bases do not wait on this lock.
            sqlx::raw_sql(&format!(
                "CREATE FUNCTION {barrier}() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN
                   IF NEW.kb_id = '{}'::uuid THEN
                     PERFORM pg_advisory_xact_lock(1361, {gate_pid});
                   END IF;
                   RETURN NEW;
                 END $$;
                 CREATE TRIGGER {barrier} BEFORE INSERT ON facts
                 FOR EACH ROW EXECUTE FUNCTION {barrier}();",
                f.kb
            ))
            .execute(&f.pool)
            .await?;
            pause.resume.notify_one();
            loop {
                let blocked: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))",
                )
                .bind(gate_pid)
                .fetch_one(&f.pool)
                .await?;
                if blocked {
                    break;
                }
                tokio::select! {
                    result = &mut extraction => {
                        result?;
                        anyhow::bail!("extraction finished before the INSERT barrier");
                    }
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
        }
        if rebuild {
            utopia_store::graph::purge_graph(&f.pool, f.kb).await?;
            if interruption != Interruption::PurgeDuringApply {
                let queued = utopia_store::documents::queue_extraction(&f.pool, f.kb, None).await?;
                anyhow::ensure!(queued == vec![f.doc], "rebuild must queue the document");
            }
        } else {
            utopia_store::documents::queue_extraction_one(&f.pool, f.doc).await?;
        }
        if let Some(tx) = gate.take() {
            tx.rollback().await?;
        }
        pause.resume.notify_one();
        let result = extraction.await;
        if interruption == Interruption::PurgeDuringApply {
            // The same foreign key failure, without a new epoch, is still the current task's error and must not be swallowed wholesale.
            let error = result.err().ok_or_else(|| anyhow::anyhow!("current job lost its error"))?;
            anyhow::ensure!(format!("{error:#}").contains("facts_subject_same_kb"), "{error:#}");
            return Ok(());
        }
        result?;

        let doc = utopia_store::documents::get(&f.pool, f.doc).await?;
        anyhow::ensure!(doc.graph_status == "queued", "the new job keeps its status");
        anyhow::ensure!(doc.graph_error.is_none(), "superseded work is not a failure");
        let extracted: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM chunks WHERE document_id=$1 AND extracted_at IS NOT NULL",
        )
        .bind(f.doc)
        .fetch_one(&f.pool)
        .await?;
        anyhow::ensure!(extracted == 0, "the old reply must not mark a chunk extracted");
        let statements: i64 =
            sqlx::query_scalar("SELECT count(*) FROM facts WHERE kb_id=$1 AND layer='open'")
                .bind(f.kb)
                .fetch_one(&f.pool)
                .await?;
        anyhow::ensure!(
            statements == if rebuild { 0 } else { 1 },
            "only statements from before re-extract may remain, got {statements}"
        );
        let jobs: Vec<(String, String)> = sqlx::query_as(
            "SELECT kind, status FROM jobs WHERE payload->>'document_id'=$1 OR payload->>'kb_id'=$2",
        )
        .bind(f.doc.to_string())
        .bind(f.kb.to_string())
        .fetch_all(&f.pool)
        .await?;
        anyhow::ensure!(
            jobs == vec![("extract_document".into(), "queued".into())],
            "only the replacement job remains, with no follow-up jobs: {jobs:?}"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await;
    pause.resume.notify_one();
    if let Some(tx) = gate.take() {
        tx.rollback().await?;
    }
    if during_apply {
        sqlx::raw_sql(&format!(
            "DROP TRIGGER IF EXISTS {barrier} ON facts; DROP FUNCTION IF EXISTS {barrier}();"
        ))
        .execute(&f.pool)
        .await?;
    }
    f.cleanup().await?;
    run?
}

#[tokio::test]
async fn a_rebuild_discards_an_extraction_reply_that_is_already_in_flight() -> anyhow::Result<()> {
    interrupt_the_second_chunk(Interruption::RebuildInFlight).await
}

#[tokio::test]
async fn reextract_discards_an_extraction_reply_that_is_already_in_flight() -> anyhow::Result<()> {
    interrupt_the_second_chunk(Interruption::ReextractInFlight).await
}

#[tokio::test]
async fn a_rebuild_after_the_epoch_check_does_not_fail_the_superseded_job() -> anyhow::Result<()> {
    interrupt_the_second_chunk(Interruption::RebuildDuringApply).await
}

#[tokio::test]
async fn an_apply_error_without_a_replacement_epoch_is_still_returned() -> anyhow::Result<()> {
    interrupt_the_second_chunk(Interruption::PurgeDuringApply).await
}
