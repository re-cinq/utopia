//! #513: ingest embedding batches run concurrently in flight; pairing for each chunk stays correct.
//!
//! Concurrency must not touch "which vector lands on which chunk": a misaligned write is not visible afterwards -- the text is still there, but the vector belongs to someone else.
//! So the tests here check pairing and completeness, not speed. The embedding endpoint is faked with wiremock; each text's vector
//! is computed from the text itself, so it can be verified on read-back; each request's arrival time and batch size are also recorded.
//!
//! 1. **Each chunk gets the vector of its own text.** 40 items, three batches, four-way concurrency; batches finish out of order, and read-back is checked one by one.
//!    Also: the remainder that is less than a full batch at the end is still sent; empty documents send no request at all.
//! 2. **A count mismatch abandons the whole batch**: one vector short, and none of that batch is written.
//! 3. **The ceiling holds**: twelve batches, 400ms delay per batch; never more than EMBED_JOBS in flight at once,
//!    and total time is clearly shorter than serial.
//! 4. **A document with a failed batch does not hang**: go through the full process_document; the second batch returns 500, and the document lands in failed
//!    with the reason, not left at embedding.
//! 5. **Everything is embedded before ready**: after process_document completes, no vector is empty.
//! 6. **Memory is embedded sentence by sentence** (#1187): memory_ingest embeds its own chunks; a sentence that cannot be embedded does not stop the others.
//! 7. **A NUL in the body does not destroy the whole document** (#611): Postgres TEXT does not accept 0x00; previously one byte put the whole document in
//!    failed. Now it goes through the full process_document to ready, and no chunk in the database carries a NUL.
//!
//! 8. **Scanned documents are read by a layout-recognition service** (0040 second cut): once the service is configured, the file is re-queued, submitted once, polled, and chunked per page.
//! 9. **Recordings are read by a transcription model that labels speakers** (third cut): speakers go into the body, timestamps go into anchors; when speakers cannot be told apart, it degrades.
//!
//! Without `UTOPIA_DATABASE_URL` the test is skipped rather than failed. It sets up and tears down its own database, never touching an existing one.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use utopia_core::models::Proposer;
use uuid::Uuid;
use wiremock::{
    matchers::method, matchers::path, Mock, MockServer, Request, Respond, ResponseTemplate,
};

/// Body -> vector: length, first character, byte sum modulo, constant 1. Four dimensions are enough to tell each one apart
fn vector_of(text: &str) -> Vec<f32> {
    vec![
        text.chars().count() as f32,
        text.chars().next().map(|c| c as u32 as f32).unwrap_or(0.0),
        (text.bytes().map(u32::from).sum::<u32>() % 97) as f32,
        1.0,
    ]
}

/// Fake embedding endpoint. `short_by` returns that many fewer vectors per batch; `fail_request` makes the Nth request return 500; `delay` how long each response is held.
/// Cloneable: one copy is mounted into wiremock, one is kept in the fixture to read the arrival records
#[derive(Clone)]
struct FakeEmbed {
    arrivals: Arc<Mutex<Vec<(Instant, usize)>>>,
    delay: Duration,
    short_by: usize,
    fail_request: Option<usize>,
}

impl FakeEmbed {
    fn new(delay: Duration) -> Self {
        Self {
            arrivals: Arc::new(Mutex::new(Vec::new())),
            delay,
            short_by: 0,
            fail_request: None,
        }
    }
    fn arrivals(&self) -> Vec<(Instant, usize)> {
        self.arrivals.lock().expect("arrivals lock").clone()
    }
}

impl Respond for FakeEmbed {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = request.body_json().expect("embedding request is JSON");
        let inputs = body["input"].as_array().cloned().unwrap_or_default();
        let ordinal = {
            let mut a = self.arrivals.lock().expect("arrivals lock");
            a.push((Instant::now(), inputs.len()));
            a.len()
        };
        if self.fail_request == Some(ordinal) {
            return ResponseTemplate::new(500).set_body_string("model is down");
        }
        let data: Vec<serde_json::Value> = inputs
            .iter()
            .take(inputs.len().saturating_sub(self.short_by))
            .map(|t| serde_json::json!({ "embedding": vector_of(t.as_str().unwrap_or("")) }))
            .collect();
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({ "data": data }))
            .set_delay(self.delay)
    }
}

struct Fx {
    pool: sqlx::PgPool,
    state: crate::state::AppState,
    server: MockServer,
    fake: FakeEmbed,
    org: Uuid,
    ws: Uuid,
    kb: Uuid,
    dir: std::path::PathBuf,
}

async fn fixture(fake: FakeEmbed) -> anyhow::Result<Option<Fx>> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(None);
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations(id,name) VALUES($1,'pipeline-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces(id,org_id,name) VALUES($1,$2,'pipeline-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases(id,workspace_id,name) VALUES($1,$2,'pipeline-test')")
        .bind(kb)
        .bind(ws)
        .execute(&pool)
        .await?;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(fake.clone())
        .mount(&server)
        .await;
    utopia_store::settings::upsert(
        &pool,
        ws,
        None,
        None,
        None,
        Some(&server.uri()),
        None,
        Some("fake-embed"),
        None,
    )
    .await?;
    let dir = std::env::temp_dir().join(format!("utopia-pipeline-{kb}"));
    let cfg = utopia_core::config::AppConfig {
        data_dir: dir.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let search = Arc::new(utopia_search::SearchIndex::open(&dir.join("search"))?);
    let state = crate::state::AppState::new(pool.clone(), &cfg, search, "test-only".into());
    Ok(Some(Fx {
        pool,
        state,
        server,
        fake,
        org,
        ws,
        kb,
        dir,
    }))
}

impl Fx {
    /// A document with only chunks and no vectors; the bodies all differ, so a wrong pairing would not match
    async fn document_with_chunks(&self, n: usize) -> anyhow::Result<Uuid> {
        let doc = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO documents(id,kb_id,filename,sha256,status) VALUES($1,$2,'m.md',$3,'ready')",
        )
        .bind(doc)
        .bind(self.kb)
        .bind(format!("sha-{doc}"))
        .execute(&self.pool)
        .await?;
        for seq in 0..n {
            sqlx::query("INSERT INTO chunks(id,kb_id,document_id,seq,text) VALUES($1,$2,$3,$4,$5)")
                .bind(Uuid::now_v7())
                .bind(self.kb)
                .bind(doc)
                .bind(seq as i32)
                .bind(format!("chunk {seq} of {doc}: {}", "x".repeat(seq % 7)))
                .execute(&self.pool)
                .await?;
        }
        Ok(doc)
    }

    /// A real document: the body goes to blob storage, the row is pending, waiting for process_document to parse it into chunks
    async fn document_to_process(&self, paragraphs: usize) -> anyhow::Result<Uuid> {
        let text: String = (0..paragraphs)
            .map(|i| format!("Paragraph {i}. {}\n\n", format!("word{i} ").repeat(140)))
            .collect();
        self.document_with_text(&text).await
    }

    /// Same as above, but the body is supplied by the caller
    async fn document_with_text(&self, text: &str) -> anyhow::Result<Uuid> {
        self.document_with_bytes("long.md", text.as_bytes()).await
    }

    /// Arbitrary bytes, arbitrary file names: images, recordings, and binaries all come in here
    async fn document_with_bytes(&self, filename: &str, bytes: &[u8]) -> anyhow::Result<Uuid> {
        use sha2::{Digest, Sha256};
        let sha: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        self.state.blob.put(&sha, bytes).await?;
        Ok(utopia_store::documents::create(
            &self.pool,
            self.kb,
            filename,
            "application/octet-stream",
            bytes.len() as i64,
            &sha,
            None,
            None,
            None,
        )
        .await?
        .id)
    }

    async fn alerts(&self, kind: &str) -> anyhow::Result<Vec<serde_json::Value>> {
        Ok(sqlx::query_scalar(
            "SELECT detail FROM alerts WHERE kb_id = $1 AND kind = $2 ORDER BY created_at",
        )
        .bind(self.kb)
        .bind(kind)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn embed(&self, doc: Uuid) -> anyhow::Result<usize> {
        let settings = utopia_store::settings::get(&self.pool, self.ws)
            .await?
            .expect("settings were written by the fixture");
        let client = crate::llm_util::embed_client(&settings).expect("embed model is configured");
        let input = utopia_store::documents::processing_input(&self.pool, doc).await?;
        super::embed_pending(&self.state, &settings, &client, &input).await
    }

    /// (body, vector) by seq; an empty vector means not embedded
    async fn stored(&self, doc: Uuid) -> anyhow::Result<Vec<(String, Option<Vec<f32>>)>> {
        Ok(sqlx::query_as(
            "SELECT text, embedding::real[] FROM chunks WHERE document_id = $1 ORDER BY seq",
        )
        .bind(doc)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn cleanup(self) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM organizations WHERE id=$1")
            .bind(self.org)
            .execute(&self.pool)
            .await?;
        let _ = std::fs::remove_dir_all(&self.dir);
        drop(self.server);
        Ok(())
    }
}

#[tokio::test]
async fn every_chunk_gets_the_vector_of_its_own_text() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::from_millis(30))).await? else {
        return Ok(());
    };
    let doc = f.document_with_chunks(40).await?;
    assert_eq!(f.embed(doc).await?, 40);
    for (text, vector) in f.stored(doc).await? {
        assert_eq!(
            vector.as_deref(),
            Some(vector_of(&text).as_slice()),
            "chunk {text:?} must carry its own vector"
        );
    }
    let mut sizes: Vec<usize> = f.fake.arrivals().into_iter().map(|(_, n)| n).collect();
    sizes.sort_unstable();
    assert_eq!(sizes, vec![8, 16, 16], "two full batches and the remainder");

    let empty = f.document_with_chunks(0).await?;
    assert_eq!(f.embed(empty).await?, 0);
    assert_eq!(
        f.fake.arrivals().len(),
        3,
        "an empty document makes no request"
    );
    f.cleanup().await
}

#[tokio::test]
async fn a_batch_that_answers_with_the_wrong_count_is_abandoned_whole() -> anyhow::Result<()> {
    let mut fake = FakeEmbed::new(Duration::ZERO);
    fake.short_by = 1;
    let Some(f) = fixture(fake).await? else {
        return Ok(());
    };
    let doc = f.document_with_chunks(10).await?;
    assert!(
        f.embed(doc).await.is_err(),
        "15 vectors for 16 texts is an error"
    );
    assert!(
        f.stored(doc).await?.iter().all(|(_, v)| v.is_none()),
        "nothing from a misaligned batch is written"
    );
    f.cleanup().await
}

#[tokio::test]
async fn the_embedding_gate_is_never_held_beyond_its_ceiling() -> anyhow::Result<()> {
    let delay = Duration::from_millis(400);
    let Some(f) = fixture(FakeEmbed::new(delay)).await? else {
        return Ok(());
    };
    let batches = 12;
    let doc = f.document_with_chunks(batches * super::EMBED_BATCH).await?;
    let started = Instant::now();
    assert_eq!(f.embed(doc).await?, batches * super::EMBED_BATCH);
    let elapsed = started.elapsed();

    // In-flight count = number of requests whose arrival falls in the same response-delay window (window slightly shorter than the delay, absorbing jitter)
    let arrivals = f.fake.arrivals();
    let window = delay - Duration::from_millis(20);
    let peak = arrivals
        .iter()
        .map(|(t, _)| {
            arrivals
                .iter()
                .filter(|(u, _)| *u <= *t && t.duration_since(*u) < window)
                .count()
        })
        .max()
        .unwrap_or(0);
    assert!(
        peak <= super::EMBED_JOBS,
        "peak in-flight batches {peak} exceeds the ceiling {}",
        super::EMBED_JOBS
    );
    assert!(peak >= 2, "batches actually overlap; peak was {peak}");
    // Total time is only a backstop for "the gate fully serialized the batches"; what really guards the ceiling and the overlap are the two checks above.
    // The delay must outweigh the fixed cost of database writes: 150ms ran to 1.51s on a two-core proot PostgreSQL,
    // where peaks and overlap were normal, yet it exceeded the 1.35s threshold. 400ms lets network wait dominate; still using three-quarters
    // of serial as the ceiling (3.6s vs 4.8s), so a real serial regression still fails.
    let serial = delay * batches as u32;
    assert!(
        elapsed < serial * 3 / 4,
        "twelve batches took {elapsed:?}; serial would be {serial:?}"
    );
    f.cleanup().await
}

#[tokio::test]
async fn a_large_statements_payload_is_searchable_without_embedding() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::ZERO)).await? else {
        return Ok(());
    };
    f.server.reset().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string("maximum input length is 8192 tokens"),
        )
        .expect(0)
        .mount(&f.server)
        .await;

    // Stay within the push route's 64 KiB cap, while exceeding the input limit of common embedding models.
    // The full contract must be able to enter deterministic extraction, not be blocked first by embedding (#1375).
    let payload = serde_json::json!({
        "external_id": "large-observation",
        "e": [["cup-7", "cup", true], ["kitchen table", "table", true]],
        "s": [[null, "cup-7", "is on", "kitchen table", null,
               {"note": "sensor ".repeat(9000)}, null, null]],
        "n": []
    });
    let doc = f
        .document_with_bytes("observation.json", payload.to_string().as_bytes())
        .await?;
    let source = Uuid::now_v7();
    sqlx::query("INSERT INTO sources(id,kb_id,kind,name) VALUES($1,$2,'statements','sensor')")
        .bind(source)
        .bind(f.kb)
        .execute(&f.pool)
        .await?;
    sqlx::query("UPDATE documents SET source_id = $2 WHERE id = $1")
        .bind(doc)
        .bind(source)
        .execute(&f.pool)
        .await?;

    super::process_document(&f.state, doc).await?;
    let row = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!(row.status, "ready");
    assert_eq!(row.graph_status, "queued");
    let stored = f.stored(doc).await?;
    assert_eq!(stored.len(), 1, "the contract stays in one chunk");
    assert!(stored[0].1.is_none());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored[0].0)?,
        payload
    );
    let chunks = utopia_store::documents::chunks_full(&f.pool, doc).await?;
    let hits = f
        .state
        .search
        .search(&f.kb.to_string(), "sensor", 1, None)?;
    assert_eq!(hits.len(), 1, "the payload remains searchable by keyword");
    assert_eq!(hits[0].chunk_id, chunks[0].id.to_string());
    f.cleanup().await
}

#[tokio::test]
async fn a_failed_batch_does_not_strand_the_document() -> anyhow::Result<()> {
    let mut fake = FakeEmbed::new(Duration::from_millis(20));
    fake.fail_request = Some(2);
    let Some(f) = fixture(fake).await? else {
        return Ok(());
    };
    let doc = f.document_to_process(20).await?;
    let original = utopia_store::documents::get(&f.pool, doc).await?;
    let error = super::process_document(&f.state, doc)
        .await
        .expect_err("the current revision really failed");
    assert!(utopia_core::is_deferred(&error).is_none());
    assert!(!utopia_core::is_terminal(&error));
    let error_message = error.to_string();
    assert!(error_message.contains("model is down"));
    let row = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!(row.status, "failed", "not left sitting in embedding");
    assert_eq!(row.error.as_deref(), Some(error_message.as_str()));
    assert_eq!(
        (row.text_len, row.chunk_count),
        (original.text_len, original.chunk_count)
    );
    f.cleanup().await
}

/// #1215: the first batch of embeddings must wait for the test to release it; a new-version request completes immediately, without guessing concurrency order from delays.
async fn pause_embedding(
    f: &Fx,
    doc: Uuid,
    fail_old: bool,
) -> anyhow::Result<(
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    tokio::task::JoinHandle<()>,
)> {
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let first_request = Arc::new(AtomicBool::new(true));
    let app = axum::Router::new().route(
        "/embeddings",
        axum::routing::post({
            let entered = entered.clone();
            let resume = resume.clone();
            move |axum::Json(body): axum::Json<serde_json::Value>| {
                let entered = entered.clone();
                let resume = resume.clone();
                let first_request = first_request.clone();
                async move {
                    let is_old = first_request.swap(false, Ordering::SeqCst);
                    if is_old {
                        entered.notify_one();
                        resume.notified().await;
                        if fail_old {
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({"error": "old embedding failed"})),
                            );
                        }
                    }
                    let data: Vec<_> = body["input"]
                        .as_array()
                        .expect("embedding input is an array")
                        .iter()
                        .map(|input| {
                            let mut vector = vector_of(input.as_str().expect("input is text"));
                            // A new version with the same SHA may reuse chunk IDs, and the old vectors must still be discarded.
                            if is_old {
                                vector[0] += 10_000.0;
                            }
                            serde_json::json!({"embedding": vector})
                        })
                        .collect();
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({"data": data})),
                    )
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    utopia_store::settings::upsert(
        &f.pool,
        f.ws,
        Some(&base),
        None,
        Some("fake-chat"),
        Some(&base),
        None,
        Some("fake-embed"),
        Some(4),
    )
    .await?;
    let state = f.state.clone();
    let processing = tokio::spawn(async move { super::process_document(&state, doc).await });
    tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
    Ok((resume, processing, server))
}

async fn replace_document_text(f: &Fx, doc: Uuid, text: &str) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};
    let sha: String = Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    f.state.blob.put(&sha, text.as_bytes()).await?;
    utopia_store::documents::replace_content_and_enqueue_processing(
        &f.pool,
        doc,
        "long.md",
        "text/markdown",
        text.len() as i64,
        &sha,
        None,
    )
    .await?;
    Ok(())
}

async fn processing_snapshot(f: &Fx, doc: Uuid) -> anyhow::Result<serde_json::Value> {
    Ok(sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'document', (SELECT to_jsonb(d) FROM documents d WHERE d.id = $1),
            'chunks', (SELECT coalesce(jsonb_agg(to_jsonb(c) ORDER BY c.seq, c.id), '[]')
                         FROM chunks c WHERE c.document_id = $1 AND c.superseded_at IS NULL),
            'jobs', (SELECT coalesce(jsonb_agg(to_jsonb(j) ORDER BY j.id), '[]')
                       FROM jobs j WHERE j.payload->>'document_id' = $1::text),
            'alerts', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.id), '[]')
                         FROM alerts a WHERE a.kb_id = $2))",
    )
    .bind(doc)
    .bind(f.kb)
    .fetch_one(&f.pool)
    .await?)
}

async fn late_embedding_preserves_newer_revision(
    fail_old: bool,
    restore_original: bool,
) -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::ZERO)).await? else {
        return Ok(());
    };
    let doc = f.document_to_process(4).await?;
    let original = utopia_store::documents::get(&f.pool, doc).await?;
    let versions = utopia_store::documents::versions(&f.pool, doc).await?;
    let original_version = versions.last().expect("the document has a version").version;
    let original_text = String::from_utf8(f.state.blob.get(&original.sha256).await?)?;
    let (resume, old, server) = pause_embedding(&f, doc, fail_old).await?;
    let old_chunks: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM chunks WHERE document_id = $1 AND superseded_at IS NULL ORDER BY seq",
    )
    .bind(doc)
    .fetch_all(&f.pool)
    .await?;
    assert!(old_chunks.len() > 1 && old_chunks.len() <= super::EMBED_BATCH);

    let new_text = "The NEW budget is 200.";
    replace_document_text(&f, doc, new_text).await?;
    if restore_original {
        // B has not been parsed yet; when A is restored it claims the same batch of chunk IDs the old task is embedding.
        replace_document_text(&f, doc, &original_text).await?;
    }
    super::process_document(&f.state, doc).await?;
    let newer = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!(newer.status, "ready");
    assert_eq!(newer.error, None);
    if restore_original {
        assert_eq!(newer.sha256, original.sha256);
        let versions = utopia_store::documents::versions(&f.pool, doc).await?;
        assert!(versions.last().expect("the document has a version").version > original_version);
        let current_chunks: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM chunks
              WHERE document_id = $1 AND superseded_at IS NULL ORDER BY seq",
        )
        .bind(doc)
        .fetch_all(&f.pool)
        .await?;
        assert_eq!(
            current_chunks, old_chunks,
            "restoring A reuses the chunks held by the old request"
        );
    } else {
        assert_ne!(newer.chunk_count, old_chunks.len() as i32);
    }

    // The new extraction has also completed, so the old task must not change it back to queued or queue extraction again.
    utopia_store::documents::set_graph_status(&f.pool, doc, "done").await?;
    sqlx::query("UPDATE jobs SET status = 'done' WHERE payload->>'document_id' = $1")
        .bind(doc.to_string())
        .execute(&f.pool)
        .await?;
    let completed = processing_snapshot(&f, doc).await?;
    resume.notify_one();
    let old_result = tokio::time::timeout(Duration::from_secs(10), old).await??;
    server.abort();
    let current = processing_snapshot(&f, doc).await?;
    sqlx::query("DELETE FROM jobs WHERE payload->>'document_id' = $1")
        .bind(doc.to_string())
        .execute(&f.pool)
        .await?;
    f.cleanup().await?;
    old_result?;
    assert_eq!(
        current, completed,
        "a late embedding must leave the newer revision intact"
    );
    Ok(())
}

#[tokio::test]
async fn a_late_successful_embedding_preserves_a_newer_revision() -> anyhow::Result<()> {
    late_embedding_preserves_newer_revision(false, false).await
}

#[tokio::test]
async fn a_late_failed_embedding_preserves_a_newer_revision() -> anyhow::Result<()> {
    late_embedding_preserves_newer_revision(true, false).await
}

#[tokio::test]
async fn a_late_embedding_cannot_accept_a_restored_sha_as_its_revision() -> anyhow::Result<()> {
    late_embedding_preserves_newer_revision(false, true).await
}

#[tokio::test]
async fn a_late_embedding_preserves_a_deleted_document() -> anyhow::Result<()> {
    for fail_old in [false, true] {
        let Some(f) = fixture(FakeEmbed::new(Duration::ZERO)).await? else {
            return Ok(());
        };
        let doc = f.document_to_process(4).await?;
        let (resume, old, server) = pause_embedding(&f, doc, fail_old).await?;
        utopia_store::documents::delete(&f.pool, f.kb, doc, None).await?;
        let deleted = processing_snapshot(&f, doc).await?;
        assert!(deleted["document"]["deleted_at"].is_string());
        assert_eq!(deleted["chunks"], serde_json::json!([]));
        resume.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(10), old).await??;
        server.abort();
        let current = processing_snapshot(&f, doc).await?;
        f.cleanup().await?;
        result?;
        assert_eq!(
            current, deleted,
            "deleted documents reject old success and failure"
        );
    }
    Ok(())
}

#[tokio::test]
async fn an_embedding_failure_does_not_hide_a_database_write_error() -> anyhow::Result<()> {
    let Some(mut f) = fixture(FakeEmbed::new(Duration::ZERO)).await? else {
        return Ok(());
    };
    let url = utopia_store::test_db::url().expect("the fixture has a configured database");
    let doc = f.document_to_process(4).await?;
    let (resume, processing, server) = pause_embedding(&f, doc, true).await?;
    f.pool.close().await;
    resume.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(10), processing).await??;
    server.abort();
    // Reconnecting is only to tear down the fixture; the old pool the handler holds is still closed.
    f.pool = sqlx::PgPool::connect(&url).await?;
    f.cleanup().await?;
    let error = result.expect_err("failure status could not be persisted");
    assert!(
        matches!(
            error.downcast_ref::<utopia_core::AppError>(),
            Some(utopia_core::AppError::Db(sqlx::Error::PoolClosed))
        ),
        "the database error must reach the caller: {error:#}"
    );
    Ok(())
}

/// #611: a single NUL previously failed the whole document -- only a few bytes were bad, but the whole document was lost
#[tokio::test]
async fn a_nul_byte_does_not_fail_the_whole_document() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::from_millis(5))).await? else {
        return Ok(());
    };
    // The shape a NUL takes in PDF text layers: in the middle of a word, between paragraphs, several in a row
    let text = "Revenue grew\0 twelve percent.\n\n\0\0Margins held at thirty\0-one.\n";
    let doc = f.document_with_text(text).await?;
    super::process_document(&f.state, doc).await?;

    let row = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!(row.status, "ready", "a NUL byte must not fail the document");
    let stored = f.stored(doc).await?;
    assert!(!stored.is_empty(), "the document was chunked");
    assert!(
        stored.iter().all(|(t, _)| !t.contains('\0')),
        "no stored chunk carries a NUL"
    );
    let joined: String = stored.iter().map(|(t, _)| t.as_str()).collect();
    assert!(
        joined.contains("Revenue grew twelve percent."),
        "the words around the NUL survive, joined as written"
    );
    f.cleanup().await
}

/// 0040: the text of images, scans, and recordings must be read by a model. When that kind of model is not configured, **degrade**: the file is kept, the document stops at
/// failed with a record of which kind is missing, and one database-level alert is raised; no retry, and no more garbage is decoded to chunk, embed, or extract
#[tokio::test]
async fn a_file_that_needs_a_reader_waits_and_says_so() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::from_millis(5))).await? else {
        return Ok(());
    };
    let png = [
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H',
    ];
    let mp3 = [b'I', b'D', b'3', 4, 0, 0, 0, 0, 0, 0, 0xFF, 0xFB];
    let image = f.document_with_bytes("contract-scan.png", &png).await?;
    let recording = f.document_with_bytes("board-meeting.mp3", &mp3).await?;

    for (doc, reader) in [(image, "ocr"), (recording, "transcribe")] {
        let err = super::process_document(&f.state, doc)
            .await
            .expect_err("nothing can be read without the model");
        assert!(
            utopia_core::is_terminal(&err),
            "retrying cannot configure a model"
        );
        let row = utopia_store::documents::get(&f.pool, doc).await?;
        assert_eq!(row.status, "failed");
        assert_eq!(row.reader_needed.as_deref(), Some(reader));
        assert!(row.error.is_some(), "the document says why");
        assert!(f.stored(doc).await?.is_empty(), "no garbage chunks");
    }
    let alerts = f.alerts("document.needs_reader").await?;
    assert_eq!(alerts.len(), 2);
    assert_eq!(alerts[0]["name"], "contract-scan.png");
    assert_eq!(alerts[0]["reader"], "ocr");
    assert_eq!(alerts[1]["reader"], "transcribe");
    f.cleanup().await
}

/// When the parser hits its safety ceiling, the document remains usable, but the message center must say plainly that only part was read.
#[tokio::test]
async fn a_truncated_csv_keeps_the_document_ready_and_reports_the_omitted_records(
) -> anyhow::Result<()> {
    let Some(mut f) = fixture(FakeEmbed::new(Duration::ZERO)).await? else {
        return Ok(());
    };
    // The assertion is about parser observability, not chunking. One large chunk keeps the
    // database-backed test focused and cheap.
    f.state.chunk_tokens = 1_000_000;
    let mut csv = String::from("id,name\n");
    for id in 1..=10_000 {
        writeln!(csv, "{id},customer-{id}").unwrap();
    }
    let doc = f
        .document_with_bytes("customers.csv", csv.as_bytes())
        .await?;

    super::process_document(&f.state, doc).await?;

    let row = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!(row.status, "ready", "the readable prefix remains available");
    let alerts = f.alerts("document.contents_truncated").await?;
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0]["name"], "customers.csv");
    assert_eq!(alerts[0]["warnings"][0]["kind"], "csv.records_truncated");
    assert_eq!(
        alerts[0]["warnings"][0]["detail"],
        serde_json::json!({
            "records_read": 10_000,
            "records_total": 10_001,
            "records_omitted": 1,
        })
    );
    f.cleanup().await
}

/// A fake `mineru-api`: submission returns `t-n`; before polling, `processing_polls` times it says still reading, then says done;
/// on `fail` it says reading failed. Records how many times it was submitted and what each carried
#[derive(Clone, Default)]
struct FakeMineru {
    submissions: Arc<Mutex<Vec<String>>>,
    polls: Arc<Mutex<usize>>,
    processing_polls: usize,
    fail: bool,
}

impl Respond for FakeMineru {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if request.method.as_str() == "POST" {
            let mut subs = self.submissions.lock().expect("lock");
            let auth = request
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            subs.push(format!(
                "{auth}\n{}",
                String::from_utf8_lossy(&request.body)
            ));
            return ResponseTemplate::new(202).set_body_json(
                serde_json::json!({ "task_id": format!("t-{}", subs.len()), "status": "pending" }),
            );
        }
        if request.url.path().ends_with("/result") {
            let list = serde_json::json!([
                { "type": "header", "text": "ACME", "page_idx": 0, "bbox": [0, 0, 1000, 20] },
                { "type": "text", "text": "Lease Agreement", "text_level": 1, "page_idx": 0, "bbox": [100, 40, 900, 80] },
                { "type": "text", "text": "Beta Robotics pays Alpha 1,000 per month.", "page_idx": 0, "bbox": [100, 100, 900, 140] },
                { "type": "text", "text": "Signed on 12 February 2025.", "page_idx": 1, "bbox": [100, 60, 700, 90] }
            ]);
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "completed", "backend": "vlm-auto-engine", "version": "2.5.4",
                "results": { "contract-scan": { "content_list": list.to_string() } }
            }));
        }
        let mut polls = self.polls.lock().expect("lock");
        *polls += 1;
        let status = if self.fail {
            serde_json::json!({ "status": "failed", "error": "CUDA out of memory" })
        } else if *polls <= self.processing_polls {
            serde_json::json!({ "status": "processing" })
        } else {
            serde_json::json!({ "status": "completed" })
        };
        ResponseTemplate::new(200).set_body_json(status)
    }
}

async fn with_mineru(f: &Fx, fake: &FakeMineru) -> anyhow::Result<()> {
    Mock::given(wiremock::matchers::path_regex("^/ocr/tasks.*"))
        .respond_with(fake.clone())
        .mount(&f.server)
        .await;
    utopia_store::settings::upsert_ocr(
        &f.pool,
        f.ws,
        Some("mineru"),
        Some(&format!("{}/ocr/", f.server.uri())),
        Some("ocr-secret"),
        Some("vlm-auto-engine"),
        None,
    )
    .await?;
    Ok(())
}

async fn reader_task(f: &Fx, doc: Uuid) -> anyhow::Result<Option<serde_json::Value>> {
    Ok(utopia_store::documents::reader_task(&f.pool, doc).await?)
}

/// 0040 second cut: a scan that stopped when no service was configured is re-queued once a service is configured; submit once, poll until read, chunk by page,
/// each chunk recording the service version that read it, the page, and the box. Waiting does not burn the retry budget, and does not resubmit
#[tokio::test]
async fn a_scan_waits_for_the_layout_service_and_keeps_its_page() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::from_millis(5))).await? else {
        return Ok(());
    };
    let png = [
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H',
    ];
    let doc = f.document_with_bytes("contract-scan.png", &png).await?;
    let err = super::process_document(&f.state, doc)
        .await
        .expect_err("no OCR yet");
    assert!(utopia_core::is_terminal(&err));

    let fake = FakeMineru {
        processing_polls: 1,
        ..Default::default()
    };
    with_mineru(&f, &fake).await?;
    let requeued =
        utopia_store::documents::requeue_waiting_for_reader(&f.pool, f.ws, "ocr").await?;
    assert_eq!(
        requeued,
        vec![(doc, f.kb)],
        "saving the service queues the scan again"
    );
    let row = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!((row.status.as_str(), row.reader_needed), ("pending", None));
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = 'process_document' AND status = 'queued'
            AND payload->>'document_id' = $1",
    )
    .bind(doc.to_string())
    .fetch_one(&f.pool)
    .await?;
    assert!(queued >= 1);

    // Submit: back on the queue to wait; the document is still parsing
    let err = super::process_document(&f.state, doc)
        .await
        .expect_err("submitted");
    assert!(utopia_core::is_deferred(&err).is_some(), "{err:#}");
    assert!(!utopia_core::is_terminal(&err));
    assert_eq!(
        utopia_store::documents::get(&f.pool, doc).await?.status,
        "parsing"
    );
    let task = reader_task(&f, doc).await?.expect("the task is remembered");
    assert_eq!(task["task_id"], "t-1");

    // Poll once: still reading
    let err = super::process_document(&f.state, doc)
        .await
        .expect_err("processing");
    assert!(utopia_core::is_deferred(&err).is_some());

    // Poll again: done reading
    super::process_document(&f.state, doc).await?;
    let row = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!(row.status, "ready", "{:?}", row.error);
    assert_eq!(
        reader_task(&f, doc).await?,
        None,
        "a finished read forgets its task"
    );
    let subs = fake.submissions.lock().expect("lock").clone();
    assert_eq!(subs.len(), 1, "waiting never submits twice");
    assert!(subs[0].starts_with("Bearer ocr-secret\n"));
    assert!(subs[0].contains("name=\"backend\""));
    assert!(subs[0].contains("vlm-auto-engine"));

    type Stored = (String, String, Option<String>, Option<serde_json::Value>);
    let chunks: Vec<Stored> = sqlx::query_as(
        "SELECT text, origin, origin_model, anchor FROM chunks
          WHERE document_id = $1 AND superseded_at IS NULL ORDER BY seq",
    )
    .bind(doc)
    .fetch_all(&f.pool)
    .await?;
    assert_eq!(chunks.len(), 2, "one chunk per page: {chunks:#?}");
    assert!(chunks.iter().all(|c| c.1 == "ocr"));
    assert_eq!(chunks[0].2.as_deref(), Some("mineru 2.5.4 vlm-auto-engine"));
    assert!(chunks[0].0.contains("1,000 per month"));
    assert!(!chunks[0].0.contains("ACME"), "the page header is not text");
    assert_eq!(
        chunks[0].3,
        Some(serde_json::json!({ "page": 1, "bbox": [100.0, 40.0, 900.0, 140.0] }))
    );
    assert_eq!(
        chunks[1].3.as_ref().map(|a| a["page"].clone()),
        Some(serde_json::json!(2))
    );
    assert!(
        f.stored(doc).await?.iter().all(|(_, v)| v.is_some()),
        "embedded like any text"
    );
    f.cleanup().await
}

/// The Ark path (0065): the whole image is sent to the vision model in one go; a rate-limited page backs off and retries within this round, not burning
/// the document's retry budget; with Ark, none of MinerU's task-number bookkeeping is recorded
#[tokio::test]
async fn a_scan_is_read_by_the_vision_model_in_one_pass() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::from_millis(5))).await? else {
        return Ok(());
    };
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = calls.clone();
    Mock::given(method("POST"))
        .and(path("/ark/chat/completions"))
        .respond_with(move |request: &Request| {
            let mut calls = seen.lock().expect("lock");
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            calls.push(format!(
                "{} {} {}",
                request.headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default(),
                body["model"],
                body["messages"][1]["content"][1]["image_url"]["url"].as_str().unwrap_or_default().chars().take(22).collect::<String>()
            ));
            if calls.len() == 1 {
                // First rate limit: back off and come back within this round; not a document failure
                return ResponseTemplate::new(429);
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "finish_reason": "stop", "message": { "role": "assistant",
                    "content": "{\"text\":\"Lease Agreement\\n\\nBeta Robotics pays Alpha 1,000 per month.\"}" } }]
            }))
        })
        .mount(&f.server)
        .await;
    utopia_store::settings::upsert_ocr(
        &f.pool,
        f.ws,
        Some("ark"),
        Some(&format!("{}/ark", f.server.uri())),
        Some("ark-secret"),
        None,
        Some("doubao-seed-2.1-pro"),
    )
    .await?;
    let png = [
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H',
    ];
    let doc = f.document_with_bytes("contract-scan.png", &png).await?;
    // Backoff is in milliseconds in test builds (`ark_ocr::BACKOFF`)
    super::process_document(&f.state, doc).await?;
    let row = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!(row.status, "ready", "{:?}", row.error);
    assert_eq!(
        reader_task(&f, doc).await?,
        None,
        "no remote task to remember"
    );
    let calls = calls.lock().expect("lock").clone();
    assert_eq!(calls.len(), 2, "one 429, then the page: {calls:?}");
    assert_eq!(
        calls[1],
        "Bearer ark-secret \"doubao-seed-2.1-pro\" data:image/png;base64,"
    );

    type Stored = (String, String, Option<String>, Option<serde_json::Value>);
    let chunks: Vec<Stored> = sqlx::query_as(
        "SELECT text, origin, origin_model, anchor FROM chunks
          WHERE document_id = $1 AND superseded_at IS NULL ORDER BY seq",
    )
    .bind(doc)
    .fetch_all(&f.pool)
    .await?;
    assert_eq!(chunks.len(), 1, "{chunks:#?}");
    assert_eq!(chunks[0].1, "ocr");
    assert_eq!(chunks[0].2.as_deref(), Some("ark doubao-seed-2.1-pro"));
    assert!(chunks[0].0.contains("1,000 per month"));
    assert_eq!(chunks[0].3, Some(serde_json::json!({ "page": 1 })));
    f.cleanup().await
}

/// The service says reading failed: the document lands in failed with the reason the service gave, the task number is cleared, and this failure takes the ordinary retry (the next resubmission)
#[tokio::test]
async fn a_failed_read_is_retried_from_a_fresh_submission() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::from_millis(5))).await? else {
        return Ok(());
    };
    let fake = FakeMineru {
        fail: true,
        ..Default::default()
    };
    with_mineru(&f, &fake).await?;
    let doc = f
        .document_with_bytes(
            "contract-scan.png",
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        )
        .await?;
    let err = super::process_document(&f.state, doc)
        .await
        .expect_err("submitted");
    assert!(utopia_core::is_deferred(&err).is_some());
    let err = super::process_document(&f.state, doc)
        .await
        .expect_err("failed");
    assert!(utopia_core::is_deferred(&err).is_none());
    assert!(
        !utopia_core::is_terminal(&err),
        "the next attempt may succeed"
    );
    let row = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!(row.status, "failed");
    assert!(row
        .error
        .as_deref()
        .is_some_and(|e| e.contains("CUDA out of memory")));
    assert_eq!(reader_task(&f, doc).await?, None);
    let err = super::process_document(&f.state, doc)
        .await
        .expect_err("submitted again");
    assert!(utopia_core::is_deferred(&err).is_some());
    assert_eq!(fake.submissions.lock().expect("lock").len(), 2);
    f.cleanup().await
}

/// Fake transcription endpoint: with `labels`, each sentence carries a speaker, otherwise only timestamps. Records the form received
#[derive(Clone, Default)]
struct FakeTranscriber {
    labels: bool,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Respond for FakeTranscriber {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let auth = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        self.requests.lock().expect("lock").push(format!(
            "{auth}\n{}",
            String::from_utf8_lossy(&request.body)
        ));
        let seg = |speaker: &str, start: f64, end: f64, text: &str| {
            if self.labels {
                serde_json::json!({ "type": "transcript.text.segment", "speaker": speaker, "start": start, "end": end, "text": text })
            } else {
                serde_json::json!({ "start": start, "end": end, "text": text })
            }
        };
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "text": "…",
            "segments": [
                seg("A", 0.0, 3.2, "Who delivers the Beta Robotics prototype?"),
                seg("B", 3.5, 7.25, "I will deliver it in Q3."),
            ]
        }))
    }
}

async fn with_transcriber(f: &Fx, fake: &FakeTranscriber, model: &str) -> anyhow::Result<()> {
    f.server.reset().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(f.fake.clone())
        .mount(&f.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/asr/audio/transcriptions"))
        .respond_with(fake.clone())
        .mount(&f.server)
        .await;
    utopia_store::settings::upsert_transcribe(
        &f.pool,
        f.ws,
        Some(&format!("{}/asr", f.server.uri())),
        Some("asr-secret"),
        Some(model),
    )
    .await?;
    Ok(())
}

const MP3: [u8; 12] = [b'I', b'D', b'3', 4, 0, 0, 0, 0, 0, 0, 0xFF, 0xFB];

async fn pause_transcription(
    f: &Fx,
    doc: Uuid,
) -> anyhow::Result<(
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    tokio::task::JoinHandle<()>,
)> {
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let app = axum::Router::new().route(
        "/audio/transcriptions",
        axum::routing::post({
            let entered = entered.clone();
            let resume = resume.clone();
            move || {
                let entered = entered.clone();
                let resume = resume.clone();
                async move {
                    entered.notify_one();
                    resume.notified().await;
                    axum::Json(serde_json::json!({"segments": [{
                        "speaker": "A", "start": 0.0, "end": 1.0,
                        "text": "The OLD budget is 100."
                    }]}))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    utopia_store::settings::upsert_transcribe(
        &f.pool,
        f.ws,
        Some(&base),
        None,
        Some("gpt-4o-transcribe-diarize"),
    )
    .await?;
    let state = f.state.clone();
    let processing = tokio::spawn(async move { super::process_document(&state, doc).await });
    tokio::time::timeout(Duration::from_secs(10), entered.notified()).await?;
    Ok((resume, processing, server))
}

#[tokio::test]
async fn a_late_reader_preserves_a_newer_processed_revision() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::from_millis(5))).await? else {
        return Ok(());
    };
    let doc = f.document_with_bytes("budget.mp3", &MP3).await?;
    let source = Uuid::now_v7();
    sqlx::query("INSERT INTO sources (id, kb_id, kind, name) VALUES ($1,$2,'api','audit')")
        .bind(source)
        .bind(f.kb)
        .execute(&f.pool)
        .await?;
    sqlx::query("UPDATE documents SET source_id=$2, external_key='budget' WHERE id=$1")
        .bind(doc)
        .bind(source)
        .execute(&f.pool)
        .await?;
    let (resume, old, server) = pause_transcription(&f, doc).await?;
    let text = "The NEW budget is 200.";
    use sha2::{Digest, Sha256};
    let sha: String = Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    f.state.blob.put(&sha, text.as_bytes()).await?;
    let mut tx = f.pool.begin().await?;
    let updated = utopia_store::documents::upsert_source_document_tx(
        &mut tx,
        f.kb,
        source,
        "budget",
        "budget.txt",
        "text/plain",
        text.len() as i64,
        &sha,
        None,
    )
    .await?;
    tx.commit().await?;
    assert_eq!(updated.id, doc);
    super::process_document(&f.state, doc).await?;
    // The new version's task has completed; if an old read overwrites its chunks, no later retry will repair it.
    let job: i64 = sqlx::query_scalar(
        "SELECT id FROM jobs WHERE kind = 'process_document' AND payload->>'document_id' = $1",
    )
    .bind(doc.to_string())
    .fetch_one(&f.pool)
    .await?;
    sqlx::query("UPDATE jobs SET status = 'done', updated_at = now() WHERE id = $1")
        .bind(job)
        .execute(&f.pool)
        .await?;
    let completed = utopia_store::documents::get(&f.pool, doc).await?;
    resume.notify_one();
    let old_result = old.await?;
    server.abort();
    let live: Vec<String> = sqlx::query_scalar(
        "SELECT text FROM chunks WHERE document_id=$1 AND superseded_at IS NULL ORDER BY seq",
    )
    .bind(doc)
    .fetch_all(&f.pool)
    .await?;
    let current = utopia_store::documents::get(&f.pool, doc).await?;
    f.cleanup().await?;
    old_result?;
    assert_eq!(current.sha256, sha);
    assert_eq!(current.status, "ready");
    assert_eq!(current.text_len, completed.text_len);
    assert_eq!(
        live,
        vec![text.to_string()],
        "a completed newer revision must not be replaced by an old read"
    );
    Ok(())
}

#[tokio::test]
async fn a_late_reader_does_not_repopulate_a_deleted_document() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::from_millis(5))).await? else {
        return Ok(());
    };
    let doc = f.document_with_bytes("budget.mp3", &MP3).await?;
    let (resume, processing, server) = pause_transcription(&f, doc).await?;
    utopia_store::documents::delete(&f.pool, f.kb, doc, None).await?;
    let deleted = utopia_store::documents::get(&f.pool, doc).await?;
    resume.notify_one();
    let result = processing.await?;
    server.abort();
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chunks WHERE document_id=$1 AND superseded_at IS NULL",
    )
    .bind(doc)
    .fetch_one(&f.pool)
    .await?;
    let current = utopia_store::documents::get(&f.pool, doc).await?;
    f.cleanup().await?;
    result?;
    assert!(current.deleted_at.is_some());
    assert_eq!(current.status, deleted.status);
    assert_eq!(live, 0);
    Ok(())
}

/// 0040 third cut: a recording is handed to a transcription model that labels speakers. Speakers go into the body, and each chunk records its start and end times and speaker
#[tokio::test]
async fn a_recording_is_read_with_who_said_what() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::from_millis(5))).await? else {
        return Ok(());
    };
    let fake = FakeTranscriber {
        labels: true,
        ..Default::default()
    };
    with_transcriber(&f, &fake, "gpt-4o-transcribe-diarize").await?;
    let doc = f.document_with_bytes("board-meeting.mp3", &MP3).await?;
    super::process_document(&f.state, doc).await?;
    let row = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!(row.status, "ready", "{:?}", row.error);

    let req = fake.requests.lock().expect("lock").clone();
    assert_eq!(req.len(), 1);
    assert!(req[0].starts_with("Bearer asr-secret\n"));
    assert!(req[0].contains("diarized_json"));
    assert!(req[0].contains("gpt-4o-transcribe-diarize"));

    type Stored = (String, String, Option<String>, Option<serde_json::Value>);
    let chunks: Vec<Stored> = sqlx::query_as(
        "SELECT text, origin, origin_model, anchor FROM chunks
          WHERE document_id = $1 AND superseded_at IS NULL ORDER BY seq",
    )
    .bind(doc)
    .fetch_all(&f.pool)
    .await?;
    assert_eq!(chunks.len(), 1, "{chunks:#?}");
    assert_eq!(chunks[0].1, "transcribed");
    assert_eq!(chunks[0].2.as_deref(), Some("gpt-4o-transcribe-diarize"));
    assert!(chunks[0].0.contains("Speaker B: I will deliver it in Q3."));
    assert_eq!(
        chunks[0].3,
        Some(serde_json::json!({ "start_ms": 0, "end_ms": 7250, "speaker": ["A", "B"] }))
    );
    f.cleanup().await
}

/// A transcription that cannot tell speakers apart: degrades the same as when nothing is configured (decision 5) -- not stored, an alert says why, no retry;
/// once a model that labels speakers is stored, the recording is re-queued and read into
#[tokio::test]
async fn a_transcript_that_cannot_say_who_spoke_waits_for_one_that_can() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::from_millis(5))).await? else {
        return Ok(());
    };
    let unlabelled = FakeTranscriber::default();
    with_transcriber(&f, &unlabelled, "whisper-1").await?;
    let doc = f.document_with_bytes("board-meeting.mp3", &MP3).await?;
    let err = super::process_document(&f.state, doc)
        .await
        .expect_err("no speakers");
    assert!(utopia_core::is_terminal(&err));
    let row = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!(row.status, "failed");
    assert_eq!(row.reader_needed.as_deref(), Some("transcribe"));
    assert!(row
        .error
        .as_deref()
        .is_some_and(|e| e.contains("who spoke")));
    assert!(
        f.stored(doc).await?.is_empty(),
        "nothing unattributed is kept"
    );
    let alerts = f.alerts("document.needs_reader").await?;
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0]["reader"], "transcribe");

    let labelled = FakeTranscriber {
        labels: true,
        ..Default::default()
    };
    with_transcriber(&f, &labelled, "gpt-4o-transcribe-diarize").await?;
    let requeued =
        utopia_store::documents::requeue_waiting_for_reader(&f.pool, f.ws, "transcribe").await?;
    assert_eq!(requeued, vec![(doc, f.kb)]);
    super::process_document(&f.state, doc).await?;
    assert_eq!(
        utopia_store::documents::get(&f.pool, doc).await?.status,
        "ready"
    );
    f.cleanup().await
}

/// A format that cannot be read (old .doc, archives): one failure, no retry, and not stored as garbage; it is not a missing model, so that alert is not raised
#[tokio::test]
async fn a_binary_file_fails_once_instead_of_becoming_text() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::from_millis(5))).await? else {
        return Ok(());
    };
    let doc = f
        .document_with_bytes(
            "minutes.doc",
            &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0, 0, 0, 0],
        )
        .await?;
    let err = super::process_document(&f.state, doc)
        .await
        .expect_err("not text");
    assert!(utopia_core::is_terminal(&err));
    let row = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!(row.status, "failed");
    assert_eq!(row.reader_needed, None);
    assert!(f.stored(doc).await?.is_empty());
    assert!(f.alerts("document.needs_reader").await?.is_empty());
    f.cleanup().await
}

#[test]
fn without_nul_borrows_when_there_is_nothing_to_strip() {
    use std::borrow::Cow;
    assert!(matches!(
        utopia_core::without_nul("plain text"),
        Cow::Borrowed(_)
    ));
    assert_eq!(utopia_core::without_nul("a\0b\0\0c"), "abc");
    assert_eq!(utopia_core::without_nul("\0"), "");
}

#[tokio::test]
async fn a_document_is_fully_embedded_before_it_is_ready() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::from_millis(20))).await? else {
        return Ok(());
    };
    let doc = f.document_to_process(20).await?;
    super::process_document(&f.state, doc).await?;
    let row = utopia_store::documents::get(&f.pool, doc).await?;
    assert_eq!(row.status, "ready");
    let stored = f.stored(doc).await?;
    assert_eq!(stored.len() as i32, row.chunk_count);
    assert!(
        stored.len() > super::EMBED_BATCH,
        "enough chunks for more than one batch"
    );
    assert!(
        stored.iter().all(|(_, v)| v.is_some()),
        "no chunk is left without a vector when the document is ready"
    );
    f.cleanup().await
}

#[tokio::test]
async fn a_memory_episode_gets_the_vector_of_its_own_text() -> anyhow::Result<()> {
    let Some(f) = fixture(FakeEmbed::new(Duration::ZERO)).await? else {
        return Ok(());
    };
    let doc = f.document_with_chunks(5).await?;
    super::memory_ingest(
        &f.state,
        doc,
        Proposer {
            user_id: None,
            token_id: None,
        },
    )
    .await?;
    for (text, vector) in f.stored(doc).await? {
        assert_eq!(vector.as_deref(), Some(vector_of(&text).as_slice()));
    }
    f.cleanup().await
}

/// #1187: previously every chunk without a vector was embedded together, and one error failed the whole task, so a memory that could not be embedded
/// blocked every new memory after it. Now that sentence is left without a vector, the others proceed, and the task finishes as usual
#[tokio::test]
async fn a_memory_that_cannot_be_embedded_does_not_hold_back_the_others() -> anyhow::Result<()> {
    let mut fake = FakeEmbed::new(Duration::ZERO);
    fake.fail_request = Some(2);
    let Some(f) = fixture(fake).await? else {
        return Ok(());
    };
    let doc = f.document_with_chunks(5).await?;
    super::memory_ingest(
        &f.state,
        doc,
        Proposer {
            user_id: None,
            token_id: None,
        },
    )
    .await?;
    let embedded: Vec<bool> = f
        .stored(doc)
        .await?
        .iter()
        .map(|(_, vector)| vector.is_some())
        .collect();
    assert_eq!(embedded, vec![true, false, true, true, true]);
    f.cleanup().await
}
