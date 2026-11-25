//! Lifecycle of a phrase judgment (0053, #807, #795): a candidate hits through inheritance, parent-edge add/remove makes a judgment stale,
//! no-candidate and over-limit outcomes are each persisted, a class change leaves old rows that no longer loop, and an edit made mid-request leaves visible staleness,
//! and a person's judgment is not overwritten. The model is a scripted HTTP endpoint; the database is a real PostgreSQL.
use super::*;
use anyhow::Context as _;
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use utopia_store::{materialize, phrase_bindings};

#[derive(Clone)]
struct Model {
    replies: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    hold: Arc<std::sync::atomic::AtomicBool>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    /// How many times the embedding endpoint was called: whether the shortlist should embed signatures (#1097) is judged by this count
    embeds: Arc<std::sync::atomic::AtomicUsize>,
    /// The embedding endpoint replies with balance exhausted (#1360)
    embeds_broke: Arc<std::sync::atomic::AtomicBool>,
}
/// Put this string in the script and the chat endpoint returns the OpenAI balance-exhausted 429
const OUT_OF_CREDIT: &str = "out_of_credit";
fn out_of_credit() -> axum::response::Response {
    let body = json!({"error": {"message": "You have no credits remaining.",
        "type": "insufficient_quota", "code": "insufficient_quota"}});
    (axum::http::StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response()
}
async fn reply(State(m): State<Model>, Json(body): Json<Value>) -> axum::response::Response {
    let n = {
        let mut seen = m.requests.lock().unwrap();
        seen.push(body);
        seen.len() - 1
    };
    if n == 0 && m.hold.load(std::sync::atomic::Ordering::SeqCst) {
        m.entered.notify_one();
        m.release.notified().await;
    }
    let text = {
        let mut replies = m.replies.lock().unwrap();
        if replies.is_empty() {
            panic!("unexpected model request #{n}");
        }
        replies.remove(0)
    };
    if text == OUT_OF_CREDIT {
        return out_of_credit();
    }
    let frame = json!({"choices":[{"delta":{"content":text.to_string()}}]});
    (
        [("content-type", "text/event-stream")],
        format!("data: {frame}\n\ndata: [DONE]\n\n"),
    )
        .into_response()
}

/// Scripted embedding endpoint: one four-dimensional vector per text, computed from the bytes. It only requires determinism and that the count matches --
/// what this tests is "whether a vector exists", not whether it is close
async fn embed(State(m): State<Model>, Json(body): Json<Value>) -> axum::response::Response {
    m.embeds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if m.embeds_broke.load(std::sync::atomic::Ordering::SeqCst) {
        return out_of_credit();
    }
    let data: Vec<Value> = body["input"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|t| {
            let bytes = t.as_str().unwrap_or("").as_bytes();
            let v: Vec<f32> = (0..4)
                .map(|i| {
                    bytes
                        .iter()
                        .skip(i)
                        .step_by(4)
                        .map(|b| *b as f32)
                        .sum::<f32>()
                        / 255.0
                })
                .collect();
            json!({ "embedding": v })
        })
        .collect();
    Json(json!({ "data": data })).into_response()
}

struct Fx {
    pool: sqlx::PgPool,
    state: AppState,
    org: Uuid,
    ws: Uuid,
    kb: Uuid,
    legal_entity: Uuid,
    organization: Uuid,
    place: Uuid,
    acme: Uuid,
    based_in: Uuid,
    model: Model,
    server: tokio::task::JoinHandle<()>,
    dir: tempfile::TempDir,
}

impl Fx {
    /// A database: legal_entity ⊃ organization, place; property based_in declared on legal_entity -> place;
    /// Acme (organization) -based in-> London (place): one open statement
    async fn new() -> anyhow::Result<Option<Self>> {
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(None);
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        utopia_store::db::migrate(&pool).await?;
        let (org, ws, kb, legal_entity, organization, place, acme, london, based_in, statement) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','phrase-lifecycle');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','phrase-lifecycle');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','phrase-lifecycle');
             INSERT INTO entity_types(id,kb_id,key,label,color,shape) VALUES
                 ('{legal_entity}','{kb}','legal_entity','Legal entity','#000','circle'),
                 ('{organization}','{kb}','organization','Organization','#000','circle'),
                 ('{place}','{kb}','place','Place','#000','circle');
             INSERT INTO entity_type_parents(child_id,parent_id,is_primary) VALUES
                 ('{organization}','{legal_entity}',true);
             INSERT INTO relation_types(id,kb_id,key,label,kind,temporal,description) VALUES
                 ('{based_in}','{kb}','based_in','based in','relation','state','where an entity is based');
             INSERT INTO relation_type_domains(relation_type_id,entity_type_id) VALUES ('{based_in}','{legal_entity}');
             INSERT INTO relation_type_ranges(relation_type_id,entity_type_id) VALUES ('{based_in}','{place}');
             INSERT INTO entities(id,kb_id,canonical_name,type_id) VALUES
                 ('{acme}','{kb}','Acme','{organization}'), ('{london}','{kb}','London','{place}');
             INSERT INTO facts(id,kb_id,subject_id,object_id,layer,phrase) VALUES
                 ('{statement}','{kb}','{acme}','{london}','open','based in');"
        ))
        .execute(&pool)
        .await?;
        let model = Model {
            replies: Arc::new(Mutex::new(Vec::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
            hold: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            embeds: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            embeds_broke: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let router = Router::new()
            .route("/chat/completions", post(reply))
            .route("/embeddings", post(embed))
            .with_state(model.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
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
        Ok(Some(Self {
            pool,
            state,
            org,
            ws,
            kb,
            legal_entity,
            organization,
            place,
            acme,
            based_in,
            model,
            server,
            dir,
        }))
    }
    fn script(&self, replies: Vec<Value>) {
        *self.model.replies.lock().unwrap() = replies;
    }
    /// Configure an embedding model for the workspace (same scripted endpoint). Not configured by default: most scenarios test structural candidates, and
    /// configuring embeddings would mix ontology vector backfill and the shortlist into them
    async fn enable_embeddings(&self) -> anyhow::Result<()> {
        let settings = utopia_store::settings::get(&self.pool, self.ws)
            .await?
            .ok_or_else(|| anyhow::anyhow!("fixture settings missing"))?;
        let endpoint = settings.chat_base_url.clone();
        utopia_store::settings::upsert(
            &self.pool,
            self.ws,
            endpoint.as_deref(),
            None,
            Some("scripted"),
            endpoint.as_deref(),
            None,
            Some("scripted-embed"),
            None,
        )
        .await?;
        Ok(())
    }
    /// An edit to this database: going through the API handler needs someone
    async fn editor(&self) -> anyhow::Result<utopia_core::models::User> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO users(id,org_id,email,password_hash,display_name) VALUES($1,$2,$3,'x','Editor')",
        )
        .bind(id)
        .bind(self.org)
        .bind(format!("editor-{id}@example.test"))
        .execute(&self.pool)
        .await?;
        sqlx::query("INSERT INTO kb_members(kb_id,user_id,role) VALUES($1,$2,'editor')")
            .bind(self.kb)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(utopia_core::models::User {
            id,
            org_id: self.org,
            email: format!("editor-{id}@example.test"),
            password_hash: "x".into(),
            display_name: "Editor".into(),
            is_admin: false,
            created_at: chrono::Utc::now(),
        })
    }
    /// The tasks queued for this database, in order
    async fn queued(&self) -> anyhow::Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT kind FROM jobs WHERE status='queued' AND payload->>'kb_id'=$1 ORDER BY run_at, id",
        )
        .bind(self.kb.to_string())
        .fetch_all(&self.pool)
        .await?)
    }
    /// Whether a relation/property has a vector, and which text segment was embedded
    async fn vector_of(&self, id: Uuid) -> anyhow::Result<(bool, Option<String>)> {
        Ok(sqlx::query_as(
            "SELECT embedding IS NOT NULL, embedded_text FROM relation_types WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?)
    }
    async fn run(&self) -> anyhow::Result<()> {
        align_phrases_reasking(&self.state, self.kb, 0).await
    }
    fn requests(&self) -> Vec<Value> {
        self.model.requests.lock().unwrap().clone()
    }
    fn embed_calls(&self) -> usize {
        self.model.embeds.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn prompt_of(&self, n: usize) -> String {
        self.requests()[n]["messages"][1]["content"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
    async fn binding(&self) -> anyhow::Result<phrase_bindings::Binding> {
        let mut all = phrase_bindings::bindings(&self.pool, self.kb).await?;
        anyhow::ensure!(!all.is_empty(), "no binding");
        Ok(all.remove(0))
    }
    /// The reason recorded when this signature was persisted (`votes.reason`): structural results do not ask a model, the reason is written on the vote
    async fn reason(&self) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT votes->>'reason' FROM phrase_bindings WHERE kb_id=$1 ORDER BY decided_at DESC LIMIT 1")
            .bind(self.kb)
            .fetch_one(&self.pool)
            .await?)
    }
    async fn typed(&self) -> anyhow::Result<i64> {
        Ok(materialize::count(&self.pool, self.kb).await?)
    }
    /// After this round finishes, whether alignment was queued again: the criterion for convergence
    async fn requeued(&self) -> anyhow::Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind='align_phrases' AND status='queued' AND payload->>'kb_id'=$1",
        )
        .bind(self.kb.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }
    async fn clear_jobs(&self) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM jobs WHERE payload->>'kb_id'=$1")
            .bind(self.kb.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn cleanup(self) -> anyhow::Result<()> {
        self.server.abort();
        self.clear_jobs().await?;
        sqlx::query("DELETE FROM organizations WHERE id=$1")
            .bind(self.org)
            .execute(&self.pool)
            .await?;
        drop(self.state);
        self.dir.close()?;
        Ok(())
    }
}

fn vote(key: Option<&str>, dir: Option<&str>) -> Value {
    marked(key, dir, key.map(|_| "none"))
}
/// One vote carries a fourth slot: based_in is a state, so a vote binding it must say which end the date of a statement carrying only one date is labelled (#966).
/// Statements in the fixture have no dates, so it normally answers none
fn marked(key: Option<&str>, dir: Option<&str>, marks: Option<&str>) -> Value {
    json!({"b":[[0, key, dir, marks]]})
}
/// After two votes alignment asks once more "what else does this shape entail" (0044 decision 3, fifth slice): the script answers "none"
fn nothing_implied() -> Value {
    json!({"i":[[0,null,null]]})
}
fn bound() -> Vec<Value> {
    vec![
        vote(Some("based_in"), Some("forward")),
        vote(Some("based_in"), Some("forward")),
        nothing_implied(),
    ]
}
fn none() -> Vec<Value> {
    vec![vote(None, None)]
}

#[tokio::test]
async fn agreeing_votes_cannot_bind_a_direction_outside_the_declared_ends() -> anyhow::Result<()> {
    for shape in ["forward-only", "reverse-only", "literal"] {
        let Some(f) = Fx::new().await? else {
            return Ok(());
        };
        let run = async {
            let invalid = match shape {
                "reverse-only" => {
                    sqlx::query("UPDATE facts SET subject_id=object_id,object_id=subject_id WHERE kb_id=$1 AND layer='open'")
                        .bind(f.kb).execute(&f.pool).await?;
                    "forward"
                }
                "literal" => {
                    sqlx::query("DELETE FROM relation_type_ranges WHERE relation_type_id=$1")
                        .bind(f.based_in).execute(&f.pool).await?;
                    sqlx::query("UPDATE relation_types SET kind='attribute' WHERE id=$1")
                        .bind(f.based_in).execute(&f.pool).await?;
                    sqlx::query("UPDATE facts SET object_id=NULL,object_value='{\"value\":\"London\"}' WHERE kb_id=$1 AND layer='open'")
                        .bind(f.kb).execute(&f.pool).await?;
                    "reverse"
                }
                _ => "reverse",
            };
            let answer = marked(Some("based_in"), Some(invalid), Some("end"));
            f.script(vec![answer.clone(), answer, nothing_implied()]);
            f.run().await?;
            let b = f.binding().await?;
            assert_eq!(b.status, "undecided", "{shape}: agreement is not structural validity");
            assert!(b.relation_type_id.is_none() && b.direction.is_none() && b.marks.is_none());
            let votes: Value = sqlx::query_scalar("SELECT votes FROM phrase_bindings WHERE kb_id=$1")
                .bind(f.kb).fetch_one(&f.pool).await?;
            assert_eq!(votes["reason"], "invalid_direction");
            assert_eq!(votes["first"]["direction"], invalid);
            assert_eq!(votes["second"]["direction"], invalid);
            assert_eq!(f.typed().await?, 0);
            assert_eq!(f.requests().len(), 2, "no implication question for an invalid binding");
            assert!(!f.requeued().await?);
            f.run().await?;
            assert_eq!(f.requests().len(), 2, "unchanged undecided stays in review");
            sqlx::query("UPDATE relation_types SET updated_at=clock_timestamp() WHERE id=$1")
                .bind(f.based_in).execute(&f.pool).await?;
            let allowed = if shape == "reverse-only" { "reverse" } else { "forward" };
            let answer = vote(Some("based_in"), Some(allowed));
            f.script(vec![answer.clone(), answer, nothing_implied()]);
            f.run().await?;
            let b = f.binding().await?;
            assert_eq!((b.status.as_str(), b.direction.as_deref()), ("bound", Some(allowed)));
            assert_eq!(f.typed().await?, 1, "a later valid decision still materializes");
            assert!(!f.requeued().await?);
            anyhow::Ok(())
        }.await;
        f.cleanup().await?;
        run?;
    }
    Ok(())
}

// Reproduce an old accepted reverse binding whose fingerprint is already current,
// including its materialized row. A version/fingerprint mismatch cannot rescue it.
async fn cached_invalid_direction(f: &Fx, decider: &str) -> anyhow::Result<String> {
    f.script(bound());
    f.run().await?;
    let basis = f.binding().await?.basis.unwrap();
    let sig = phrase_bindings::signatures(&f.pool, f.kb).await?.remove(0);
    phrase_bindings::decide(&f.pool, f.kb, &sig, Decision {
        relation_type_id: Some(f.based_in), direction: Some("reverse"), status: "bound",
        votes: &json!({"first":{"property":"based_in","direction":"reverse"},"second":{"property":"based_in","direction":"reverse"}}),
        decided_by: decider, basis: (decider == "agent").then_some(basis.as_str()),
        marks: Some("none"), marks_asked: true,
    }).await?;
    materialize::materialize(&f.pool, f.kb).await?;
    assert_eq!(f.typed().await?, 1);
    f.clear_jobs().await?;
    Ok(basis)
}

#[tokio::test]
async fn a_cached_invalid_direction_is_redecided_without_an_input_edit() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        let basis = cached_invalid_direction(&f, "agent").await?;
        f.script(bound());
        // This cache also predates the marks question: repairing its direction
        // must not be narrowed to a marks-only request by the unchanged basis.
        sqlx::query("UPDATE phrase_bindings SET marks=NULL,marks_asked_at=NULL WHERE kb_id=$1")
            .bind(f.kb).execute(&f.pool).await?;
        f.run().await?;
        let b = f.binding().await?;
        assert_eq!((b.status.as_str(), b.direction.as_deref()), ("bound", Some("forward")));
        assert_eq!(b.basis.as_deref(), Some(basis.as_str()));
        assert_eq!(f.requests().len(), 4, "two fresh votes despite the unchanged fingerprint");
        let correct: i64 = sqlx::query_scalar("SELECT count(*) FROM facts WHERE kb_id=$1 AND layer='typed' AND invalidated_at IS NULL AND subject_id=$2")
            .bind(f.kb).bind(f.acme).fetch_one(&f.pool).await?;
        assert_eq!(correct, 1, "the corrected projection follows the statement direction");
        assert!(!f.requeued().await?);
        anyhow::Ok(())
    }.await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn an_unreadable_repair_cannot_keep_an_invalid_cached_projection() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        let basis = cached_invalid_direction(&f, "agent").await?;
        f.script(vec![json!({"unreadable":true})]);
        f.run().await?;
        let b = f.binding().await?;
        assert_eq!(
            b.status, "undecided",
            "a failed model is not support for the old invalid direction"
        );
        assert_eq!(b.basis.as_deref(), Some(basis.as_str()));
        assert_eq!(f.typed().await?, 0);
        let votes: Value = sqlx::query_scalar("SELECT votes FROM phrase_bindings WHERE kb_id=$1")
            .bind(f.kb)
            .fetch_one(&f.pool)
            .await?;
        assert_eq!(votes["reason"], "invalid_direction");
        assert_eq!(votes["previous"]["first"]["direction"], "reverse");
        assert_eq!(f.requests().len(), 3);
        f.clear_jobs().await?;
        f.run().await?;
        assert_eq!(
            f.requests().len(),
            3,
            "the unresolved direction remains for a person"
        );
        assert!(!f.requeued().await?);
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn an_inapplicable_direction_chosen_by_a_person_is_preserved() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        cached_invalid_direction(&f, "person").await?;
        f.run().await?;
        let b = f.binding().await?;
        assert_eq!(
            (
                b.status.as_str(),
                b.direction.as_deref(),
                b.decided_by.as_str()
            ),
            ("bound", Some("reverse"), "person")
        );
        assert_eq!(
            f.requests().len(),
            2,
            "only the seed agent binding used the model"
        );
        assert_eq!(f.typed().await?, 1);
        assert!(!f.requeued().await?);
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// While category-word alignment is still queued, phrase alignment does not act: the classes at both ends are not settled yet, and a judgment now would be re-judged. It queues one for itself half a minute
/// later, and judges as usual once category-word alignment finishes
#[tokio::test]
async fn phrase_alignment_waits_while_a_kind_word_round_is_pending() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.script(bound());
        sqlx::query("INSERT INTO jobs (kind, payload) VALUES ('align_types', $1)")
            .bind(json!({ "kb_id": f.kb }))
            .execute(&f.pool)
            .await?;
        f.run().await?;
        assert_eq!(
            f.requests().len(),
            0,
            "nothing is asked while kind words are still being decided"
        );
        let deferred: Vec<(String, bool)> = sqlx::query_as(
            "SELECT status, run_at > now() FROM jobs
             WHERE kind='align_phrases' AND payload->>'kb_id'=$1",
        )
        .bind(f.kb.to_string())
        .fetch_all(&f.pool)
        .await?;
        assert_eq!(
            deferred,
            vec![("queued".to_string(), true)],
            "one phrase round is queued for later"
        );
        f.run().await?;
        assert_eq!(
            f.requests().len(),
            0,
            "still waiting: the kind-word job is still queued"
        );
        f.clear_jobs().await?;
        f.run().await?;
        assert_eq!(
            f.requests().len(),
            2,
            "the kind-word round gone, the two votes are asked"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn a_property_declared_on_an_ancestor_is_offered_with_its_basis_and_bound(
) -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.script(bound());
        f.run().await?;
        assert_eq!(
            f.requests().len(),
            2,
            "two votes; bound to the only property, so no rule question"
        );
        let prompt = f.prompt_of(0);
        assert!(prompt.contains("based_in"), "{prompt}");
        assert!(
            prompt.contains("fits by inheritance: organization is a subclass of legal_entity"),
            "the model is told why the candidate fits: {prompt}"
        );
        let b = f.binding().await?;
        assert_eq!(
            (b.status.as_str(), b.decided_by.as_str()),
            ("bound", "agent")
        );
        assert!(b.basis.is_some(), "an agent decision records its basis");
        assert_eq!(f.typed().await?, 1, "the projection follows");
        assert!(
            !f.requeued().await?,
            "unchanged inputs leave no queued work"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn removing_the_parent_edge_retires_the_projection_without_a_model_call() -> anyhow::Result<()>
{
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.script(bound());
        f.run().await?;
        assert_eq!(f.typed().await?, 1);
        sqlx::query("DELETE FROM entity_type_parents WHERE child_id=$1")
            .bind(f.organization)
            .execute(&f.pool)
            .await?;
        f.clear_jobs().await?;
        f.run().await?;
        assert_eq!(f.requests().len(), 2, "no candidate, nothing to ask");
        let b = f.binding().await?;
        assert_eq!(b.status, "none");
        assert_eq!(f.reason().await?.as_deref(), Some("no_candidates"));
        assert_eq!(f.typed().await?, 0, "the unsupported projection is retired");
        assert!(!f.requeued().await?);
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn adding_a_parent_edge_reopens_a_structural_none() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        sqlx::query("DELETE FROM entity_type_parents WHERE child_id=$1")
            .bind(f.organization)
            .execute(&f.pool)
            .await?;
        f.run().await?;
        assert_eq!(f.requests().len(), 0);
        assert_eq!(f.binding().await?.status, "none");
        assert!(!f.requeued().await?);
        sqlx::query(
            "INSERT INTO entity_type_parents(child_id,parent_id,is_primary) VALUES($1,$2,true)",
        )
        .bind(f.organization)
        .bind(f.legal_entity)
        .execute(&f.pool)
        .await?;
        f.script(bound());
        f.run().await?;
        assert_eq!(
            f.requests().len(),
            2,
            "the edge changed the basis, so it is asked again"
        );
        assert_eq!(f.binding().await?.status, "bound");
        assert_eq!(f.typed().await?, 1);
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn overflow_is_recorded_for_a_person_and_recovers_when_candidates_shrink(
) -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        // 61 relations that declare no domain/range: either end is accepted, plus based_in makes 62 > 60
        for i in 0..61 {
            sqlx::query("INSERT INTO relation_types(id,kb_id,key,label,kind,temporal) VALUES($1,$2,$3,$3,'relation','state')")
                .bind(Uuid::now_v7()).bind(f.kb).bind(format!("filler_{i}")).execute(&f.pool).await?;
        }
        f.run().await?;
        assert_eq!(f.requests().len(), 0, "too many to ask");
        let b = f.binding().await?;
        assert_eq!(b.status, "undecided");
        assert_eq!(f.reason().await?.as_deref(), Some("too_many_candidates"));
        assert!(!f.requeued().await?, "overflow must not queue a run it cannot execute");
        sqlx::query("DELETE FROM relation_types WHERE kb_id=$1 AND key LIKE 'filler_%'")
            .bind(f.kb)
            .execute(&f.pool)
            .await?;
        f.script(bound());
        f.run().await?;
        assert_eq!(f.requests().len(), 2);
        assert_eq!(f.binding().await?.status, "bound");
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// While the properties have no vectors yet, the shortlist does not embed signatures (#1097): embedding them would find no neighbors, and the model still sees all the structural
/// candidates. Previously a database that never ran `embed_ontology` sent every signature with many candidates off to embed each round, and none of it was used.
/// Once properties have vectors, the same signature is embedded as usual and the shortlist opens
#[tokio::test]
async fn signatures_are_not_embedded_while_no_property_has_a_vector() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.enable_embeddings().await?;
        // 12 relations declaring no domain/range, plus based_in, make 13 > SHORTLIST: this signature opens the shortlist
        for i in 0..12 {
            sqlx::query("INSERT INTO relation_types(id,kb_id,key,label,kind,temporal) VALUES($1,$2,$3,$3,'relation','state')")
                .bind(Uuid::now_v7()).bind(f.kb).bind(format!("filler_{i}")).execute(&f.pool).await?;
        }
        f.script(bound());
        f.run().await?;
        assert_eq!(f.embed_calls(), 0, "no property has a vector: nothing to embed for");
        assert_eq!(f.binding().await?.status, "bound");
        assert!(
            f.prompt_of(0).contains("filler_11"),
            "without a shortlist the model sees every fitting candidate"
        );

        sqlx::query("UPDATE relation_types SET embedding = '[1,0,0,0]' WHERE kb_id=$1")
            .bind(f.kb)
            .execute(&f.pool)
            .await?;
        f.script(bound());
        f.run().await?;
        assert_eq!(f.embed_calls(), 1, "with property vectors the signature is embedded");
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// Balance ran out (#1360): the first overdraft 429 from signature embedding or chat fails the whole round with a typed error, the task fails once,
/// and the alert fires. Previously it was recorded as "this batch waits until next time", the task still said done, a new one was queued at once, and the same 429 came back every few seconds,
/// until someone topped up
#[tokio::test]
async fn an_empty_balance_fails_the_run_instead_of_queueing_another() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.enable_embeddings().await?;
        // 13 > SHORTLIST candidates, and properties all have vectors: this signature gets embedded
        for i in 0..12 {
            sqlx::query("INSERT INTO relation_types(id,kb_id,key,label,kind,temporal) VALUES($1,$2,$3,$3,'relation','state')")
                .bind(Uuid::now_v7()).bind(f.kb).bind(format!("filler_{i}")).execute(&f.pool).await?;
        }
        sqlx::query("UPDATE relation_types SET embedding = '[1,0,0,0]' WHERE kb_id=$1")
            .bind(f.kb)
            .execute(&f.pool)
            .await?;

        f.model
            .embeds_broke
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let err = f.run().await.err().context("a refused embedding fails the run")?;
        anyhow::ensure!(crate::alerting::hopeless(&err), "not hopeless: {err:#}");
        assert!(f.requests().is_empty(), "nothing is asked after the refusal");
        assert!(!f.requeued().await?, "no follow-up alignment is queued");

        f.model
            .embeds_broke
            .store(false, std::sync::atomic::Ordering::SeqCst);
        f.script(vec![json!(OUT_OF_CREDIT)]);
        let err = f.run().await.err().context("a refused chat call fails the run")?;
        anyhow::ensure!(crate::alerting::hopeless(&err), "not hopeless: {err:#}");
        assert_eq!(f.requests().len(), 1);
        assert!(!f.requeued().await?, "no follow-up alignment is queued");
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// Store the signature's vector (#1097): if the database has not changed, the second round calls no embedding and asks the model nothing. If the embedding model changed,
/// old vectors and the new property vectors are not in the same space, so re-embed once and delete the old model's rows
#[tokio::test]
async fn a_second_run_over_an_unchanged_base_makes_no_embedding_call() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.enable_embeddings().await?;
        // 12 relations declaring no domain/range, plus based_in makes 13 > SHORTLIST: this signature opens the shortlist
        for i in 0..12 {
            sqlx::query("INSERT INTO relation_types(id,kb_id,key,label,kind,temporal) VALUES($1,$2,$3,$3,'relation','state')")
                .bind(Uuid::now_v7()).bind(f.kb).bind(format!("filler_{i}")).execute(&f.pool).await?;
        }
        sqlx::query("UPDATE relation_types SET embedding = '[1,0,0,0]' WHERE kb_id=$1")
            .bind(f.kb)
            .execute(&f.pool)
            .await?;
        let cached = || async {
            sqlx::query_scalar::<_, String>(
                "SELECT model FROM signature_vectors WHERE kb_id=$1 ORDER BY model",
            )
            .bind(f.kb)
            .fetch_all(&f.pool)
            .await
        };

        f.script(bound());
        f.run().await?;
        assert_eq!(f.embed_calls(), 1, "the first run embeds the signature");
        assert_eq!(f.binding().await?.status, "bound");
        let asked = f.requests().len();

        f.run().await?;
        assert_eq!(
            f.embed_calls(),
            1,
            "a second run over an unchanged base makes no embedding call"
        );
        assert_eq!(f.requests().len(), asked, "and asks the model nothing");
        assert_eq!(cached().await?, vec!["scripted-embed".to_string()]);

        sqlx::query("UPDATE llm_settings SET embed_model='scripted-embed-2' WHERE workspace_id=$1")
            .bind(f.ws)
            .execute(&f.pool)
            .await?;
        f.run().await?;
        assert_eq!(f.embed_calls(), 2, "a new embedding model embeds again");
        assert_eq!(
            cached().await?,
            vec!["scripted-embed-2".to_string()],
            "the old model's vectors are dropped"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn an_endpoint_class_change_moves_the_signature_and_the_old_row_stops_looping(
) -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        // Acme has no class yet: signature (based in, ?, place), and a property with a declared domain does not accept an empty end
        sqlx::query("UPDATE entities SET type_id=NULL WHERE id=$1")
            .bind(f.acme)
            .execute(&f.pool)
            .await?;
        f.run().await?;
        assert_eq!(f.requests().len(), 0);
        let old = f.binding().await?;
        assert_eq!((old.status.as_str(), old.subject_type_id), ("none", None));
        // The category word got bound: the signature becomes (based in, organization, place), and the old row becomes an orphan
        sqlx::query("UPDATE entities SET type_id=$2 WHERE id=$1")
            .bind(f.acme)
            .bind(f.organization)
            .execute(&f.pool)
            .await?;
        f.script(bound());
        f.run().await?;
        assert_eq!(f.requests().len(), 2);
        let all = phrase_bindings::bindings(&f.pool, f.kb).await?;
        assert_eq!(all.len(), 2, "the orphan stays as a cached decision");
        assert!(all
            .iter()
            .any(|b| b.subject_type_id == Some(f.organization) && b.status == "bound"));
        assert!(
            !f.requeued().await?,
            "an orphan must not queue work it cannot execute"
        );
        f.run().await?;
        assert_eq!(
            f.requests().len(),
            2,
            "a second run asks nothing and queues nothing"
        );
        assert!(!f.requeued().await?);
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn an_edit_during_the_model_request_leaves_the_decision_stale() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.model.hold.store(true, std::sync::atomic::Ordering::SeqCst);
        f.script(none());
        let state = f.state.clone();
        let kb = f.kb;
        let worker = tokio::spawn(async move { align_phrases_reasking(&state, kb, 0).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), f.model.entered.notified()).await?;
        // The model is still answering, and the definition changed: both votes read the old definition
        sqlx::query("UPDATE relation_types SET description='NEW definition', updated_at=clock_timestamp() WHERE id=$1")
            .bind(f.based_in)
            .execute(&f.pool)
            .await?;
        f.model.release.notify_one();
        worker.await??;
        let b = f.binding().await?;
        assert_eq!(b.status, "none");
        assert!(
            f.requeued().await?,
            "the run noticed its own basis is already stale and queued another"
        );
        f.clear_jobs().await?;
        f.script(bound());
        f.run().await?;
        assert_eq!(
            f.requests().len(),
            3,
            "one vote that says none, then two votes (a none is not asked for rules): the basis differs, not the clock"
        );
        assert_eq!(f.binding().await?.status, "bound");
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn a_person_decision_made_during_the_request_is_not_overwritten() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.model
            .hold
            .store(true, std::sync::atomic::Ordering::SeqCst);
        f.script(vec![
            vote(Some("based_in"), Some("reverse")),
            vote(Some("based_in"), Some("reverse")),
            nothing_implied(),
        ]);
        let state = f.state.clone();
        let kb = f.kb;
        let worker = tokio::spawn(async move { align_phrases_reasking(&state, kb, 0).await });
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            f.model.entered.notified(),
        )
        .await?;
        let sig = phrase_bindings::signatures(&f.pool, f.kb).await?.remove(0);
        phrase_bindings::decide(
            &f.pool,
            f.kb,
            &sig,
            phrase_bindings::Decision {
                relation_type_id: Some(f.based_in),
                direction: Some("forward"),
                status: "bound",
                votes: &json!({}),
                decided_by: "person",
                basis: None,
                marks: None,
                marks_asked: false,
            },
        )
        .await?;
        f.model.release.notify_one();
        worker.await??;
        let b = f.binding().await?;
        assert_eq!(
            (b.status.as_str(), b.decided_by.as_str()),
            ("bound", "person")
        );
        assert!(
            !f.requeued().await?,
            "a person's decision is never re-evaluated"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// The reply shape from the category-word bench, where "none of the 12 documents had a word bound", carried over to phrases: DeepSeek-V3.2
/// answered the whole thing as an object keyed by id, and the second vote wrote `b` as an object with value `{"key","direction"}`.
/// If the first vote says none, the conclusion is none: the second vote does not cast, and is not asked the rule either
#[tokio::test]
async fn a_first_vote_that_says_none_is_not_followed_by_a_second() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.script(vec![
            vote(None, None),
            vote(Some("based_in"), Some("forward")),
        ]);
        f.run().await?;
        assert_eq!(f.requests().len(), 1, "one vote");
        assert_eq!(f.binding().await?.status, "none");
        assert_eq!(f.reason().await?.as_deref(), Some("first_vote_none"));
        assert_eq!(f.typed().await?, 0);
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// Both votes must be readable before the signature can be bound
#[tokio::test]
async fn an_id_keyed_reply_still_binds_the_phrase() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.script(vec![
            json!({"0": ["based_in", "forward", "none"]}),
            json!({"b": {"0": {"key": "based_in", "direction": "forward", "marks": "none"}}}),
            nothing_implied(),
        ]);
        f.run().await?;
        assert_eq!(f.requests().len(), 2, "two votes, both read");
        let b = f.binding().await?;
        assert_eq!(
            (b.status.as_str(), b.decided_by.as_str()),
            ("bound", "agent")
        );
        assert_eq!(b.relation_type_id, Some(f.based_in));
        assert_eq!(b.direction.as_deref(), Some("forward"));
        assert_eq!(f.typed().await?, 1, "the projection follows");
        assert!(!f.requeued().await?, "nothing left to ask");
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// A reply that cannot be read is no longer "nothing answered" and then gone: this round draws no conclusion and writes no rows, and the task queues
/// another one itself (with reask); after MAX_REASK times it stops, waiting for the next document or an ontology change
#[tokio::test]
async fn an_unreadable_reply_is_asked_again_a_bounded_number_of_times() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.script(vec![
            json!({"answer": "based_in", "direction": "forward"}),
            json!({"answer": "based_in", "direction": "forward"}),
            json!({"answer": "based_in", "direction": "forward"}),
            json!({"answer": "based_in", "direction": "forward"}),
        ]);
        f.run().await?;
        assert_eq!(
            f.requests().len(),
            1,
            "the first vote is unreadable, the second is not cast"
        );
        assert!(
            phrase_bindings::bindings(&f.pool, f.kb).await?.is_empty(),
            "an unreadable reply writes no decision"
        );
        let reasks: Vec<(String, Option<i64>)> = sqlx::query_as(
            "SELECT status, (payload->>'reask')::bigint FROM jobs
             WHERE kind='align_phrases' AND payload->>'kb_id'=$1 ORDER BY id",
        )
        .bind(f.kb.to_string())
        .fetch_all(&f.pool)
        .await?;
        assert_eq!(
            reasks,
            vec![("queued".to_string(), Some(1))],
            "after the first round it queues itself once with reask=1"
        );
        f.clear_jobs().await?;
        // Already the last self-queued one: no more queueing
        align_phrases_reasking(&f.state, f.kb, MAX_REASK).await?;
        assert_eq!(f.requests().len(), 2, "the last allowed round still asks");
        let after: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind='align_phrases' AND payload->>'kb_id'=$1",
        )
        .bind(f.kb.to_string())
        .fetch_one(&f.pool)
        .await?;
        assert_eq!(after, 0, "past MAX_REASK it stops queueing itself");
        assert!(phrase_bindings::bindings(&f.pool, f.kb).await?.is_empty());
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// Properties created in the editor get vectors first, before phrase alignment runs: the handler queues `embed_ontology` in front of
/// `align_phrases`; alignment sees that backfill is still queued and waits; once the backfill finishes and vectors are in the table, alignment asks the model,
/// and the new property is among the candidates. A description change goes through again: the stored source text no longer matches, so the same task re-embeds it
#[tokio::test]
async fn a_property_created_in_the_editor_has_its_vector_before_phrases_are_aligned(
) -> anyhow::Result<()> {
    use crate::api::ontology_routes::{
        create_relation_type, update_relation_type, RelationTypeReq,
    };
    use crate::auth::AuthUser;
    use axum::extract::{Path, State};
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.enable_embeddings().await?;
        let editor = f.editor().await?;
        let req: RelationTypeReq = serde_json::from_value(json!({
            "key": "headquartered_in",
            "label": "headquartered in",
            "kind": "relation",
            "description": "where the main office of an entity is",
            "domains": [f.legal_entity],
            "ranges": [f.place],
        }))?;
        let created = create_relation_type(
            State(f.state.clone()),
            AuthUser(editor.clone()),
            Path(f.kb),
            axum::Json(req),
        )
        .await
        .map_err(|e| anyhow::anyhow!("create_relation_type: {:?}", e.0))?;
        let id: Uuid = serde_json::from_value(created.0["id"].clone())?;

        assert_eq!(
            f.queued().await?,
            vec!["embed_ontology".to_string(), "align_phrases".to_string()],
            "the index refresh is queued ahead of the alignment"
        );
        assert_eq!(
            f.vector_of(id).await?,
            (false, None),
            "the handler itself does not embed"
        );

        // Alignment got there first: the backfill is still queued, so it does not ask the model and queues itself behind
        f.script(bound());
        f.run().await?;
        assert_eq!(
            f.requests().len(),
            0,
            "nothing is asked while the index refresh is still queued"
        );
        assert_eq!(
            f.queued().await?,
            vec!["embed_ontology".to_string(), "align_phrases".to_string()],
            "the alignment is queued again behind the refresh"
        );

        // The worker reaches the backfill copy: run its body, mark it complete
        let embedded = crate::ontology_index::refresh(&f.state, f.kb).await?;
        assert_eq!(
            embedded, 8,
            "the three classes twice over (full text and label) plus based_in and the new property"
        );
        sqlx::query(
            "UPDATE jobs SET status='done' WHERE kind='embed_ontology' AND payload->>'kb_id'=$1",
        )
        .bind(f.kb.to_string())
        .execute(&f.pool)
        .await?;
        let (present, text) = f.vector_of(id).await?;
        assert!(
            present,
            "the property is in the vector table before align_phrases runs"
        );
        assert_eq!(
            text.as_deref(),
            Some("headquarteredIn\nwhere the main office of an entity is"),
            "label (camel-cased by the store) and description are what was embedded"
        );

        // Alignment now actually runs, and the new property is among the candidates
        f.run().await?;
        assert!(
            !f.requests().is_empty(),
            "the refresh done, the model is asked"
        );
        let prompt = f.prompt_of(0);
        assert!(
            prompt.contains("headquartered_in"),
            "the new property is offered: {prompt}"
        );

        // Description changed: the stored source text does not match, so the backfill task queues again and re-embeds only this one
        f.clear_jobs().await?;
        let req: RelationTypeReq = serde_json::from_value(json!({
            "label": "headquartered in",
            "kind": "relation",
            "description": "where the head office of an organization sits",
        }))?;
        let updated = update_relation_type(
            State(f.state.clone()),
            AuthUser(editor),
            Path((f.kb, id)),
            axum::Json(req),
        )
        .await
        .map_err(|e| anyhow::anyhow!("update_relation_type: {:?}", e.0))?;
        assert_eq!(updated.0["ok"], json!(true));
        assert_eq!(
            f.queued().await?,
            vec!["embed_ontology".to_string(), "align_phrases".to_string()],
            "a changed description queues the refresh ahead of the alignment again"
        );
        assert_eq!(
            crate::ontology_index::refresh(&f.state, f.kb).await?,
            1,
            "only the row whose text changed is re-embedded"
        );
        assert_eq!(
            f.vector_of(id).await?.1.as_deref(),
            Some("headquarteredIn\nwhere the head office of an organization sits"),
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// Under a state property, both votes must also say which end the date of a statement carrying only one date is labelled (#966). The candidate row states the property is
/// a state; if both votes agree, bind and marks are written along with it, and it is also in the votes' record
#[tokio::test]
async fn a_state_binding_carries_what_a_single_date_marks() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.script(vec![
            marked(Some("based_in"), Some("forward"), Some("start")),
            marked(Some("based_in"), Some("forward"), Some("start")),
        ]);
        f.run().await?;
        assert_eq!(f.requests().len(), 2);
        let prompt = f.prompt_of(0);
        assert!(
            prompt.contains("- based_in · based in · relation · state ·"),
            "the model is told the property is a state: {prompt}"
        );
        let b = f.binding().await?;
        assert_eq!(
            (b.status.as_str(), b.marks.as_deref()),
            ("bound", Some("start"))
        );
        let votes: Value = sqlx::query_scalar("SELECT votes FROM phrase_bindings WHERE kb_id=$1")
            .bind(f.kb)
            .fetch_one(&f.pool)
            .await?;
        assert_eq!(
            (&votes["first"]["marks"], &votes["second"]["marks"]),
            (&json!("start"), &json!("start"))
        );
        assert!(!f.requeued().await?);
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// This signature is in the alignment queue: bound to based_in, with the original direction, waiting only for a person to say which end the date is labelled
async fn waits_for_the_date(f: &Fx) -> anyhow::Result<bool> {
    let items = utopia_store::alignment_queue::list(&f.pool, f.kb, 10, 0).await?;
    let listed = matches!(
        items.as_slice(),
        [utopia_store::alignment_queue::AlignmentItem::Phrase {
            bound_to: Some(p),
            direction: Some(d),
            ..
        }] if p == "based_in" && d == "forward"
    );
    let waiting = utopia_store::alignment_queue::waiting(&f.pool, f.kb)
        .await?
        .0;
    let badge = utopia_store::review::counts(&f.pool, f.kb).await?.alignment;
    Ok(listed && waiting == 1 && badge == 1)
}

/// Both votes chose the same property and the same direction, but one says start and the other says end, or one did not say at all (answered only three slots):
/// the property is settled by both votes, so bind it; the date is not settled, so marks stays empty, the question is recorded as asked, and it goes to a person. It is not asked again, and does not
/// queue another round for itself (0053 revision 2026-09-27). Previously an unanswered slot counted as not answered, and a model answering only three slots could never bind
#[tokio::test]
async fn two_votes_that_do_not_agree_on_the_date_bind_and_leave_it_to_a_person(
) -> anyhow::Result<()> {
    let three_cells = json!({"b":[[0, "based_in", "forward"]]});
    for (label, first) in [
        (
            "end",
            marked(Some("based_in"), Some("forward"), Some("end")),
        ),
        ("three cells", three_cells),
    ] {
        let Some(f) = Fx::new().await? else {
            return Ok(());
        };
        let run = async {
            f.script(vec![
                first,
                marked(Some("based_in"), Some("forward"), Some("start")),
            ]);
            f.run().await?;
            let b = f.binding().await?;
            assert_eq!(
                (b.status.as_str(), b.relation_type_id, b.marks.as_deref()),
                ("bound", Some(f.based_in), None),
                "{label}"
            );
            assert!(b.marks_asked_at.is_some(), "{label}: asked");
            let votes: Value =
                sqlx::query_scalar("SELECT votes FROM phrase_bindings WHERE kb_id=$1")
                    .bind(f.kb)
                    .fetch_one(&f.pool)
                    .await?;
            assert_eq!(
                &votes["second"]["marks"],
                &json!("start"),
                "{label}: both readings are kept for the person"
            );
            assert_eq!(f.typed().await?, 1, "{label}: the statement is typed");
            assert!(waits_for_the_date(&f).await?, "{label}");
            assert!(!f.requeued().await?, "{label}: no re-ask");
            f.run().await?;
            assert_eq!(f.requests().len(), 2, "{label}: asked once");
            anyhow::Ok(())
        }
        .await;
        f.cleanup().await?;
        run?;
    }
    Ok(())
}

impl Fx {
    /// Restore the binding to how it was before this question existed: no marks, and not asked
    async fn as_before_the_question(&self) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE phrase_bindings SET marks = NULL, marks_asked_at = NULL WHERE kb_id=$1",
        )
        .bind(self.kb)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    /// Backfill the parts of the binding that the question must not change: property, direction, state, who judged it, when it was judged, fingerprint, and both votes
    async fn decision(&self) -> anyhow::Result<Value> {
        Ok(sqlx::query_scalar(
            "SELECT jsonb_build_array(relation_type_id, direction, status, decided_by,
                                      decided_at, basis, votes)
               FROM phrase_bindings WHERE kb_id=$1",
        )
        .bind(self.kb)
        .fetch_one(&self.pool)
        .await?)
    }
}

/// A binding judged before this question existed has no marks. Agent-judged: ask once even if the fingerprint is unchanged, asking only which end the date is labelled;
/// if both votes accept this binding and say the same value, write it down and leave the rest of the binding alone; once asked, do not ask again.
/// Person-judged: the agent does not ask; it waits in the alignment queue for a person to fill it in, and the property and direction are the ones bound now
#[tokio::test]
async fn a_binding_decided_before_the_question_is_asked_once_and_a_persons_waits_in_the_queue(
) -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.script(bound());
        f.run().await?;
        assert_eq!(f.binding().await?.marks.as_deref(), Some("none"));
        f.as_before_the_question().await?;
        let decided = f.decision().await?;
        f.script(vec![
            marked(Some("based_in"), Some("forward"), Some("start")),
            marked(Some("based_in"), Some("forward"), Some("start")),
        ]);
        f.run().await?;
        assert_eq!(
            f.requests().len(),
            4,
            "the fingerprint matches, yet the binding is asked for the value it lacks"
        );
        assert_eq!(f.decision().await?, decided, "only the value is written");
        let b = f.binding().await?;
        assert_eq!(b.marks.as_deref(), Some("start"));
        assert!(b.marks_asked_at.is_some());
        f.run().await?;
        assert_eq!(f.requests().len(), 4, "asked once: it has its value now");
        assert!(!f.requeued().await?);

        let sig = phrase_bindings::signatures(&f.pool, f.kb).await?.remove(0);
        phrase_bindings::decide(
            &f.pool,
            f.kb,
            &sig,
            phrase_bindings::Decision {
                relation_type_id: Some(f.based_in),
                direction: Some("forward"),
                status: "bound",
                votes: &json!({"person": {"property": "based_in", "direction": "forward"}}),
                decided_by: "person",
                basis: None,
                marks: None,
                marks_asked: false,
            },
        )
        .await?;
        f.run().await?;
        assert_eq!(f.requests().len(), 4, "a person's binding is not asked");
        assert!(waits_for_the_date(&f).await?);
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// The model never gives the fourth slot (#975 review): signatures bound before this column existed are asked only once. The binding stays as it is, its typed rows stay live,
/// and the signature waits in the queue for a person; a further round does not ask again, and does not queue the next one for itself
#[tokio::test]
async fn a_model_that_never_gives_the_value_is_asked_once() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.script(bound());
        f.run().await?;
        assert_eq!(f.typed().await?, 1);
        f.as_before_the_question().await?;
        let decided = f.decision().await?;
        let three_cells = json!({"b":[[0, "based_in", "forward"]]});
        f.script(vec![three_cells.clone(), three_cells]);
        f.run().await?;
        assert_eq!(f.requests().len(), 4, "one ask: two votes");
        assert_eq!(f.decision().await?, decided, "the binding is unchanged");
        assert_eq!(f.typed().await?, 1, "its typed row is still live");
        f.run().await?;
        f.run().await?;
        assert_eq!(f.requests().len(), 4, "not asked again");
        let b = f.binding().await?;
        assert_eq!(
            (b.marks.as_deref(), b.marks_asked_at.is_some()),
            (None, true)
        );
        assert!(waits_for_the_date(&f).await?);
        assert!(!f.requeued().await?, "no re-ask is queued");
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// A backfill question can only write which end the date is labelled; it cannot reopen the binding (#975 review): if the two votes did not choose the property, chose the other direction, or said a different value,
/// the binding stays as it is, its typed rows stay live, and the signature goes to a person.
/// Only when both votes accept this binding and say the same value is that value written.
#[tokio::test]
async fn asking_what_a_date_marks_never_reopens_the_binding() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        f.script(bound());
        f.run().await?;
        let answers = [
            ("no property", vote(None, None), vote(None, None), None),
            (
                "the other direction",
                marked(Some("based_in"), Some("reverse"), Some("start")),
                marked(Some("based_in"), Some("reverse"), Some("start")),
                None,
            ),
            (
                "two readings",
                marked(Some("based_in"), Some("forward"), Some("start")),
                marked(Some("based_in"), Some("forward"), Some("end")),
                None,
            ),
            (
                "one reading",
                marked(Some("based_in"), Some("forward"), Some("end")),
                marked(Some("based_in"), Some("forward"), Some("end")),
                Some("end"),
            ),
        ];
        for (label, first, second, value) in answers {
            f.as_before_the_question().await?;
            let decided = f.decision().await?;
            f.script(vec![first, second]);
            f.run().await?;
            assert_eq!(
                f.decision().await?,
                decided,
                "{label}: the binding is unchanged"
            );
            assert_eq!(f.typed().await?, 1, "{label}: its typed row is still live");
            let b = f.binding().await?;
            assert_eq!(
                (b.marks.as_deref(), b.marks_asked_at.is_some()),
                (value, true),
                "{label}"
            );
            assert_eq!(waits_for_the_date(&f).await?, value.is_none(), "{label}");
            assert!(!f.requeued().await?, "{label}");
        }
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn a_negative_case_follows_ordinary_rejudgement_and_keeps_human_binding() -> anyhow::Result<()>
{
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        use utopia_store::ontology_regressions as cases;
        let actor = f.editor().await?.id;
        let statement_id = sqlx::query_scalar("SELECT id FROM facts WHERE kb_id=$1 AND layer='open'")
            .bind(f.kb).fetch_one(&f.pool).await?;
        let id = cases::add(&f.pool,f.kb,cases::NewCase { statement_id, expected_property_id: None,
            expected_direction: None, created_by: actor, origin: "person" }).await?;
        f.script(none()); f.run().await?;
        assert_eq!(cases::get(&f.pool,f.kb,id).await?.unwrap().last_result.unwrap()["passed"],true);
        assert_eq!(f.requests().len(),1);
        f.clear_jobs().await?;
        sqlx::query("UPDATE relation_types SET description='New definition',updated_at=clock_timestamp() WHERE id=$1")
            .bind(f.based_in).execute(&f.pool).await?;
        f.script(bound()); f.run().await?;
        let result = cases::get(&f.pool,f.kb,id).await?.unwrap().last_result.unwrap();
        assert_eq!(result["status"],"bound"); assert_eq!(result["passed"],false);
        assert_eq!(f.requests().len(),3,"cases do not add model calls");
        let sig = phrase_bindings::signatures(&f.pool,f.kb).await?.remove(0);
        phrase_bindings::decide(&f.pool,f.kb,&sig,phrase_bindings::Decision {
            relation_type_id:Some(f.based_in),direction:Some("forward"),status:"bound",decided_by:"person",
            votes:&Value::Null,basis:None,marks:None,marks_asked:false,
        }).await?;
        let before = f.binding().await?;
        f.run().await?;
        let result = cases::get(&f.pool,f.kb,id).await?.unwrap().last_result.unwrap();
        assert_eq!(result["passed"],false); assert_eq!(result["conflict"],"human_binding");
        assert_eq!(f.requests().len(),3);
        let after = f.binding().await?;
        assert_eq!(before.decided_at,after.decided_at); assert_eq!(after.relation_type_id,Some(f.based_in));
        anyhow::Ok(())
    }.await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn a_base_with_no_properties_queues_propose_ontology_once_twenty_signatures_are_unbound(
) -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        sqlx::query("DELETE FROM relation_type_domains WHERE relation_type_id=$1")
            .bind(f.based_in)
            .execute(&f.pool)
            .await?;
        sqlx::query("DELETE FROM relation_type_ranges WHERE relation_type_id=$1")
            .bind(f.based_in)
            .execute(&f.pool)
            .await?;
        sqlx::query("DELETE FROM relation_types WHERE kb_id=$1")
            .bind(f.kb)
            .execute(&f.pool)
            .await?;
        for n in 0..crate::ontology_agent::TRIGGER_UNBOUND {
            let place = Uuid::now_v7();
            sqlx::query("INSERT INTO entities(id,kb_id,canonical_name,type_id) VALUES($1,$2,$3,$4)")
                .bind(place)
                .bind(f.kb)
                .bind(format!("Place {n}"))
                .bind(f.place)
                .execute(&f.pool)
                .await?;
            sqlx::query("INSERT INTO facts(id,kb_id,subject_id,object_id,layer,phrase) VALUES($1,$2,$3,$4,'open',$5)")
                .bind(Uuid::now_v7())
                .bind(f.kb)
                .bind(f.acme)
                .bind(place)
                .bind(format!("phrase {n}"))
                .execute(&f.pool)
                .await?;
        }
        f.run().await?;
        let queued = f.queued().await?;
        anyhow::ensure!(
            queued.contains(&"propose_ontology".to_string()),
            "propose_ontology not queued, queued: {queued:?}"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}
