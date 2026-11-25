//! One round of the ontology agent (0061): unmatched shapes and accepted questions go to a scripted model, proposals land in
//! `ontology_proposals` and record the questions and shapes served; adoption builds the property and queues alignment; a shape already proposed is not
//! sent again next round. Skipped when there is no `UTOPIA_DATABASE_URL`.
use super::*;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use utopia_store::phrase_bindings::Decision;

#[derive(Clone)]
struct Model {
    replies: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    embeds: Arc<AtomicUsize>,
}
/// The embedding endpoint only counts calls: adoption should not call it in the request
async fn embed(State(m): State<Model>) -> Json<Value> {
    m.embeds.fetch_add(1, Ordering::SeqCst);
    Json(json!({ "data": [] }))
}
async fn reply(State(m): State<Model>, Json(body): Json<Value>) -> impl IntoResponse {
    m.requests.lock().unwrap().push(body);
    let text = {
        let mut replies = m.replies.lock().unwrap();
        if replies.is_empty() {
            panic!("unexpected model request");
        }
        replies.remove(0).to_string()
    };
    let frame = json!({"choices":[{"delta":{"content":text}}]});
    (
        [("content-type", "text/event-stream")],
        format!("data: {frame}\n\ndata: [DONE]\n\n"),
    )
}

struct Fx {
    pool: sqlx::PgPool,
    state: AppState,
    org: Uuid,
    kb: Uuid,
    organization: Uuid,
    place: Uuid,
    located_in: Uuid,
    /// The person adopting (decided_by is a foreign key)
    user: Uuid,
    model: Model,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Fx {
    /// One KB: two classes, organization and place, and one property located_in (organization → place);
    /// two open statements, Acme —founded in→ London and Acme —located in→ London (shapes are ordered by phrase:
    /// s0 = founded in, s1 = located in)
    async fn new(replies: Vec<Value>) -> anyhow::Result<Option<Self>> {
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(None);
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        utopia_store::db::migrate(&pool).await?;
        let (org, ws, kb, organization, place, acme, london, statement, user, located_in, st2) = (
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
            Uuid::now_v7(),
        );
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','ontology-agent');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','ontology-agent');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','ontology-agent');
             INSERT INTO users(id,org_id,email,password_hash,display_name) VALUES ('{user}','{org}','{user}@agent.test','unused','Adopter');
             INSERT INTO entity_types(id,kb_id,key,label,color,shape,description) VALUES
                 ('{organization}','{kb}','organization','Organization','#000','circle','a company or institution'),
                 ('{place}','{kb}','place','Place','#000','circle','a geographic location');
             INSERT INTO entities(id,kb_id,canonical_name,type_id) VALUES
                 ('{acme}','{kb}','Acme','{organization}'), ('{london}','{kb}','London','{place}');
             INSERT INTO relation_types(id,kb_id,key,label,kind,temporal,description) VALUES
                 ('{located_in}','{kb}','located_in','located in','relation','state','where an organization sits');
             INSERT INTO relation_type_domains(relation_type_id,entity_type_id) VALUES ('{located_in}','{organization}');
             INSERT INTO relation_type_ranges(relation_type_id,entity_type_id) VALUES ('{located_in}','{place}');
             INSERT INTO facts(id,kb_id,subject_id,object_id,layer,phrase) VALUES
                 ('{statement}','{kb}','{acme}','{london}','open','founded in'),
                 ('{st2}','{kb}','{acme}','{london}','open','located in');"
        ))
        .execute(&pool)
        .await?;
        let model = Model {
            replies: Arc::new(Mutex::new(replies)),
            requests: Arc::new(Mutex::new(Vec::new())),
            embeds: Arc::new(AtomicUsize::new(0)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let router = Router::new()
            .route("/chat/completions", post(reply))
            .route("/embeddings", post(embed))
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
        Ok(Some(Self {
            pool,
            state,
            org,
            kb,
            organization,
            place,
            located_in,
            user,
            model,
            server,
            _dir: dir,
        }))
    }

    /// Alignment has judged these two shapes: there is no property to bind to
    async fn decide_none(&self) -> anyhow::Result<()> {
        let sigs = phrase_bindings::signatures(&self.pool, self.kb).await?;
        assert_eq!(sigs.len(), 2, "two open statements, two signatures");
        for sig in &sigs {
            phrase_bindings::decide(
                &self.pool,
                self.kb,
                sig,
                Decision {
                    relation_type_id: None,
                    direction: None,
                    status: "none",
                    votes: &json!({ "reason": "no_candidates" }),
                    decided_by: "agent",
                    basis: Some("test"),
                    marks: None,
                    marks_asked: false,
                },
            )
            .await?;
        }
        Ok(())
    }

    /// The KB has a question a person wrote: the ontology agent does not ask questions first (that step is for KBs without questions)
    async fn seed_question(&self) -> anyhow::Result<Uuid> {
        Ok(competency_questions::create(
            &self.pool,
            self.kb,
            competency_questions::NewQuestion {
                question: "Where was each organization founded?",
                expected_answer: None,
                needs: None,
                origin: "person",
                status: "accepted",
                created_by: None,
            },
        )
        .await?)
    }

    async fn requests(&self) -> usize {
        self.model.requests.lock().unwrap().len()
    }

    /// What a shape looks like written into a proposal, with the statement id of its first example sentence
    async fn shape(&self, phrase: &str) -> anyhow::Result<Value> {
        let sigs = phrase_bindings::signatures(&self.pool, self.kb).await?;
        let sig = sigs
            .iter()
            .find(|s| s.phrase == phrase)
            .ok_or_else(|| anyhow::anyhow!("no shape `{phrase}`"))?;
        Ok(shape_of(sig))
    }

    /// One the agent proposed, stored directly
    async fn seed(
        &self,
        section: &str,
        key: &str,
        payload: Value,
        signatures: Value,
    ) -> anyhow::Result<()> {
        utopia_store::ontology::save_agent_proposals(
            &self.pool,
            self.kb,
            &[utopia_store::ontology::AgentProposal {
                section: section.into(),
                key: key.into(),
                payload,
                serves: Vec::new(),
                signatures,
            }],
        )
        .await?;
        Ok(())
    }

    /// Call the endpoint as this KB's editor
    async fn post(&self, path: &str, body: Value) -> anyhow::Result<(StatusCode, Value)> {
        sqlx::query(
            "INSERT INTO kb_members(kb_id,user_id,role) VALUES($1,$2,'editor') ON CONFLICT DO NOTHING",
        )
        .bind(self.kb)
        .bind(self.user)
        .execute(&self.pool)
        .await?;
        let token = crate::auth::issue_token(&self.state, self.user)?;
        let request = Request::post(format!("/api/v1/kbs/{}{path}", self.kb))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))?;
        let response = crate::api::router(self.state.clone(), &Default::default())
            .oneshot(request)
            .await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await?;
        Ok((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ))
    }

    /// Jobs queued for this KB, ordered by due time
    async fn queued(&self) -> anyhow::Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT kind FROM jobs WHERE status='queued' AND payload->>'kb_id'=$1 ORDER BY run_at, id",
        )
        .bind(self.kb.to_string())
        .fetch_all(&self.pool)
        .await?)
    }

    /// What one key left behind: whether the element with that key exists, the proposal's status, this KB's job count, and the regression case count
    async fn left_by(&self, key: &str) -> anyhow::Result<(bool, String, i64, i64)> {
        Ok(sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM relation_types WHERE kb_id=$1 AND key=$2)
                    OR EXISTS(SELECT 1 FROM entity_types WHERE kb_id=$1 AND key=$2),
                    (SELECT status FROM ontology_proposals WHERE kb_id=$1 AND key=$2),
                    (SELECT count(*) FROM jobs WHERE payload->>'kb_id'=$1::text),
                    (SELECT count(*) FROM ontology_regression_cases WHERE kb_id=$1)",
        )
        .bind(self.kb)
        .bind(key)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn cleanup(self) -> anyhow::Result<()> {
        self.server.abort();
        sqlx::query("DELETE FROM jobs WHERE payload->>'kb_id'=$1")
            .bind(self.kb.to_string())
            .execute(&self.pool)
            .await?;
        // decided_by points at users and does not cascade: clear the proposals first, then delete the organization
        sqlx::query("DELETE FROM ontology_agent_reviews WHERE kb_id=$1")
            .bind(self.kb)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM ontology_proposals WHERE kb_id=$1")
            .bind(self.kb)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM organizations WHERE id=$1")
            .bind(self.org)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// Relation produced from the founded in shape: organization → place
fn founded_in() -> Value {
    json!({ "key": "founded_in", "label": "founded in", "description": "where an organization was founded",
            "temporal": "state", "domains": ["organization"], "ranges": ["place"], "proposed_by": "agent" })
}

/// A new class proposed by the agent: city, parent class place
fn city() -> Value {
    json!({ "key": "city", "label": "City", "description": "a city", "parents": ["place"], "proposed_by": "agent" })
}

fn user_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn an_unbound_shape_becomes_a_proposal_that_serves_a_question_and_adoption_builds_the_property(
) -> anyhow::Result<()> {
    let Some(f) = Fx::new(vec![json!({
        "p": [{
            "kind": "property", "key": "founded_in", "label": "founded in",
            "definition": "the place where an organization was founded",
            "value": false, "datatype": null,
            "domains": ["organization"], "ranges": ["place"],
            "s": [0], "k": [], "q": [0]
        }],
        "existing": []
    })])
    .await?
    else {
        return Ok(());
    };
    f.decide_none().await?;
    let qid = f.seed_question().await?;

    propose(&f.state, f.kb).await?;

    // The model saw the shapes, the vocabulary and the question
    let requests = f.model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "one batch, one call");
    let text = user_text(&requests[0]);
    assert!(
        text.contains("founded in"),
        "the open shape is offered: {text}"
    );
    assert!(
        text.contains("organization"),
        "the glossary is offered: {text}"
    );
    assert!(
        text.contains("located_in"),
        "existing properties are in the glossary: {text}"
    );
    assert!(
        text.contains("Where was each organization founded?"),
        "the accepted question is offered: {text}"
    );

    // The proposal is stored, carrying who proposed it, the question served, and the shape it would bind
    let open = utopia_store::ontology::open_proposals(&f.pool, f.kb).await?;
    assert_eq!(open.len(), 1, "one proposal: {open:?}");
    let p = &open[0];
    assert_eq!(p.section, "relation_types");
    assert_eq!(p.key, "founded_in");
    assert_eq!(p.proposed_by, "agent");
    assert_eq!(p.serves, vec![qid]);
    assert_eq!(
        p.signatures["phrases"][0]["phrase"], "founded in",
        "the shape it binds is recorded: {}",
        p.signatures
    );
    assert_eq!(p.signatures["phrases"][0]["subject"], "organization");
    assert_eq!(p.payload["forms"][0], "founded in");
    assert_eq!(p.payload["domains"][0], "organization");
    assert_eq!(p.payload["ranges"][0], "place");
    let example_id: Uuid =
        serde_json::from_value(p.signatures["phrases"][0]["example_statement_ids"][0].clone())?;
    let phrase: String = sqlx::query_scalar("SELECT phrase FROM facts WHERE id=$1 AND kb_id=$2")
        .bind(example_id)
        .bind(f.kb)
        .fetch_one(&f.pool)
        .await?;
    assert_eq!(
        phrase, "founded in",
        "the proposed example has a stable source identity"
    );
    assert_eq!(
        p.payload["description"],
        "the place where an organization was founded"
    );

    // Shapes already proposed, and shapes seen but not proposed (located in), are not sent next round: no model call
    let outcomes: Vec<(String, String)> = sqlx::query_as(
        "SELECT shape->>'phrase', outcome FROM ontology_agent_reviews WHERE kb_id=$1 ORDER BY 1",
    )
    .bind(f.kb)
    .fetch_all(&f.pool)
    .await?;
    assert_eq!(
        outcomes,
        vec![
            ("founded in".to_string(), "proposed".to_string()),
            ("located in".to_string(), "declined".to_string())
        ],
        "every offered shape is recorded with its outcome"
    );
    propose(&f.state, f.kb).await?;
    assert_eq!(
        f.requests().await,
        1,
        "already-proposed and declined shapes are not offered again"
    );

    // Adopt: the property is built, the signature's domain and range follow the proposal, the proposal is marked adopted, and phrase alignment is queued
    let id = adopt(
        &f.state,
        f.kb,
        "relation_types",
        "founded_in",
        AdoptEdits {
            label: Some("Founded in".into()),
            ..Default::default()
        },
        f.user,
    )
    .await?
    .id;
    let props = utopia_store::ontology::relation_type_views(&f.pool, f.kb).await?;
    let built = props
        .iter()
        .find(|p| p.id == id)
        .expect("the property exists");
    assert_eq!(built.key, "founded_in");
    // The property label follows the library's convention, written as lowerCamelCase: the person's edits went in, only the spelling changed
    assert_eq!(built.label, "foundedIn", "the person's edit wins");
    assert_eq!(built.kind, "relation");
    assert_eq!(built.domains, vec![f.organization]);
    assert_eq!(built.ranges, vec![f.place]);
    assert_eq!(
        built.description,
        "the place where an organization was founded"
    );
    assert!(
        utopia_store::ontology::open_proposals(&f.pool, f.kb)
            .await?
            .is_empty(),
        "the proposal is no longer open"
    );
    let status: String = sqlx::query_scalar(
        "SELECT status FROM ontology_proposals WHERE kb_id=$1 AND section='relation_types' AND key='founded_in'",
    )
    .bind(f.kb)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(status, "adopted");
    let cases = utopia_store::ontology_regressions::list(&f.pool, f.kb).await?;
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].statement_id, example_id);
    assert_eq!(cases[0].expected_property_id, Some(id));
    assert_eq!(cases[0].expected_direction.as_deref(), Some("forward"));
    assert_eq!(cases[0].created_by, Some(f.user));
    assert_eq!(cases[0].origin, "adoption");
    assert!(
        utopia_store::jobs::pending_for_kb(&f.pool, "align_phrases", f.kb).await?,
        "adoption re-decides the shapes through alignment"
    );

    // Something already adopted cannot be adopted again
    let again = adopt(
        &f.state,
        f.kb,
        "relation_types",
        "founded_in",
        AdoptEdits::default(),
        f.user,
    )
    .await;
    assert!(
        matches!(again, Err(AppError::NotFound)),
        "a decided proposal is not open: {again:?}"
    );

    // Older proposals lack source IDs: adoption succeeds without inventing cases.
    let mut legacy_shape = p.signatures["phrases"][0].clone();
    legacy_shape
        .as_object_mut()
        .unwrap()
        .remove("example_statement_ids");
    utopia_store::ontology::save_agent_proposals(
        &f.pool,
        f.kb,
        &[utopia_store::ontology::AgentProposal {
            section: "map_to".into(),
            key: "located_in".into(),
            payload: json!({"kind":"relation"}),
            serves: vec![],
            signatures: json!({"phrases":[legacy_shape]}),
        }],
    )
    .await?;
    adopt(
        &f.state,
        f.kb,
        "map_to",
        "located_in",
        AdoptEdits::default(),
        f.user,
    )
    .await?;
    assert!(
        utopia_store::ontology::open_proposal(&f.pool, f.kb, "map_to", "located_in")
            .await?
            .is_none()
    );
    assert_eq!(
        utopia_store::ontology_regressions::list(&f.pool, f.kb)
            .await?
            .len(),
        1,
        "a proposal without example IDs adds no regression case"
    );
    f.cleanup().await
}

#[tokio::test]
async fn a_proposal_for_an_existing_key_or_binding_nothing_is_dropped() -> anyhow::Result<()> {
    let Some(f) = Fx::new(vec![json!({
        "p": [
            // Existing classes: not proposed
            {"kind": "class", "key": "place", "label": "Place", "definition": "x", "parents": [], "s": [], "k": [], "q": []},
            // Bind nothing: bad item
            {"kind": "property", "key": "orphan", "label": "orphan", "definition": "x", "value": false,
             "domains": [], "ranges": [], "s": [], "k": [], "q": []},
            // Literal-value property: goes into attribute_types, datatype defaults to text
            {"kind": "property", "key": "founding_note", "label": "founding note", "definition": "x", "value": true,
             "datatype": "weird", "domains": ["organization"], "ranges": [], "s": [0], "k": [], "q": []}
        ],
        "existing": []
    })])
    .await?
    else {
        return Ok(());
    };
    f.decide_none().await?;
    f.seed_question().await?;
    propose(&f.state, f.kb).await?;
    let open = utopia_store::ontology::open_proposals(&f.pool, f.kb).await?;
    assert_eq!(open.len(), 1, "only the attribute survives: {open:?}");
    assert_eq!(open[0].section, "attribute_types");
    assert_eq!(open[0].key, "founding_note");
    assert_eq!(open[0].payload["datatype"], "text");
    assert!(open[0].serves.is_empty());
    f.cleanup().await
}

#[tokio::test]
async fn a_declined_shape_returns_when_its_statements_double_and_an_existing_answer_becomes_a_binding(
) -> anyhow::Result<()> {
    let Some(f) = Fx::new(vec![
        // Round one: founded in was not proposed; located in is already in the ontology (located_in, forward)
        json!({ "p": [], "existing": [{ "key": "located_in", "dir": "forward", "s": [1], "k": [] }] }),
        // The round after the statements doubled: only founded in is sent (it is s0 in this batch), and the answer reads located_in in reverse
        json!({ "p": [], "existing": [{ "key": "located_in", "dir": "reverse", "s": [0], "k": [] }] }),
    ])
    .await?
    else {
        return Ok(());
    };
    f.decide_none().await?;
    f.seed_question().await?;

    propose(&f.state, f.kb).await?;
    assert_eq!(f.requests().await, 1);
    let open = utopia_store::ontology::open_proposals(&f.pool, f.kb).await?;
    assert_eq!(
        open.len(),
        1,
        "the existing answer is a map_to proposal: {open:?}"
    );
    let m = &open[0];
    assert_eq!(m.section, "map_to");
    assert_eq!(m.key, "located_in");
    assert_eq!(m.proposed_by, "agent");
    assert_eq!(m.payload["kind"], "relation");
    assert_eq!(
        m.payload["label"], "located in",
        "the target's label travels with it"
    );
    assert_eq!(m.payload["forms"][0], "located in");
    assert_eq!(m.signatures["phrases"][0]["phrase"], "located in");
    assert_eq!(m.signatures["phrases"][0]["direction"], "forward");

    // Round two: one proposed (map_to), one seen but not proposed, vocabulary unchanged: do not ask
    propose(&f.state, f.kb).await?;
    assert_eq!(f.requests().await, 1, "nothing new to look at");

    // The vocabulary changed (one more class): the seen-but-not-proposed ones are not asked again; alignment itself will judge the new elements, asking the agent only burns tokens
    utopia_store::ontology::create_entity_type(
        &f.pool,
        f.kb,
        "city",
        "City",
        "#000",
        "circle",
        &[f.place],
        "a city",
    )
    .await?;
    propose(&f.state, f.kb).await?;
    assert_eq!(
        f.requests().await,
        1,
        "a changed glossary alone does not re-offer declined shapes"
    );

    // The statements for founded in doubled (1 → 2): send it again; located in, already proposed under map_to, is not sent
    sqlx::query(
        "INSERT INTO facts(id,kb_id,subject_id,object_id,layer,phrase)
         SELECT $1, kb_id, subject_id, object_id, 'open', 'founded in' FROM facts
          WHERE kb_id=$2 AND phrase='founded in' LIMIT 1",
    )
    .bind(Uuid::now_v7())
    .bind(f.kb)
    .execute(&f.pool)
    .await?;
    propose(&f.state, f.kb).await?;
    assert_eq!(
        f.requests().await,
        2,
        "a declined shape with twice the statements is offered again"
    );
    let text = user_text(&f.model.requests.lock().unwrap()[1]);
    assert!(
        text.contains("founded in"),
        "the declined shape is offered again: {text}"
    );
    assert!(
        !text.contains("s1"),
        "the shape already proposed as map_to is not offered: {text}"
    );

    // This round's answers are merged into the same map_to: two shapes, each with its own direction
    let open = utopia_store::ontology::open_proposals(&f.pool, f.kb).await?;
    assert_eq!(open.len(), 1, "still one map_to row: {open:?}");
    let shapes = open[0].signatures["phrases"].as_array().unwrap().clone();
    assert_eq!(
        shapes.len(),
        2,
        "shapes from both rounds are kept: {shapes:?}"
    );
    assert_eq!(shapes[0]["phrase"], "located in");
    assert_eq!(shapes[0]["direction"], "forward");
    assert_eq!(shapes[1]["phrase"], "founded in");
    assert_eq!(shapes[1]["direction"], "reverse");
    assert_eq!(
        open[0].payload["forms"],
        json!(["located in", "founded in"])
    );

    // Adopt map_to: both shapes became the person's bindings, bound to located_in, each in its own direction
    let id = adopt(
        &f.state,
        f.kb,
        "map_to",
        "located_in",
        AdoptEdits::default(),
        f.user,
    )
    .await?
    .id;
    assert_eq!(id, f.located_in);
    let bindings = phrase_bindings::bindings(&f.pool, f.kb).await?;
    for (phrase, dir) in [("located in", "forward"), ("founded in", "reverse")] {
        let b = bindings
            .iter()
            .find(|b| b.phrase == phrase)
            .expect("the binding exists");
        assert_eq!(b.status, "bound", "{phrase}");
        assert_eq!(b.decided_by, "person", "{phrase}");
        assert_eq!(b.relation_type_id, Some(f.located_in), "{phrase}");
        assert_eq!(b.direction.as_deref(), Some(dir), "{phrase}");
    }
    assert!(
        utopia_store::ontology::open_proposals(&f.pool, f.kb)
            .await?
            .is_empty(),
        "the map_to proposal is adopted"
    );
    // Another round: both are bound now, so do not ask
    propose(&f.state, f.kb).await?;
    assert_eq!(f.requests().await, 2);
    f.cleanup().await
}

#[tokio::test]
async fn a_base_with_no_questions_gets_them_proposed_and_the_report_counts_them(
) -> anyhow::Result<()> {
    let Some(f) = Fx::new(vec![
        // The ontology agent finds the KB has no questions at all: ask questions first (this round's first call)
        json!({ "q": [
            { "question": "Where is each organization located?", "classes": ["organization", "place"], "properties": ["located_in", "made_up"] },
            { "question": "Why?", "classes": [], "properties": [] }
        ] }),
        // Then propose the ontology by phrase, as before
        json!({ "p": [], "existing": [] }),
        // Asking to raise questions again from the UI: the same sentence is not repeated, new ones come in
        json!({ "q": [
            { "question": "where is each ORGANIZATION located?", "classes": [], "properties": [] },
            { "question": "Which organizations were founded in each place?", "classes": ["organization"], "properties": [] }
        ] }),
    ])
    .await?
    else {
        return Ok(());
    };
    f.decide_none().await?;

    propose(&f.state, f.kb).await?;
    assert_eq!(
        f.requests().await,
        2,
        "questions first, then the ontology round"
    );
    let text = user_text(&f.model.requests.lock().unwrap()[0]);
    assert!(text.contains("Most frequent statement shapes"), "{text}");
    assert!(
        text.contains("\"founded in\""),
        "the shapes are offered: {text}"
    );
    assert!(
        text.contains("Acme (organization)"),
        "the connected entities are offered: {text}"
    );
    let qs = competency_questions::list(&f.pool, f.kb).await?;
    assert_eq!(qs.len(), 1, "the short one is dropped: {qs:?}");
    assert_eq!(qs[0].origin, "agent");
    assert_eq!(qs[0].status, "proposed");
    assert_eq!(
        qs[0].needs,
        Some(json!({ "classes": ["organization", "place"], "properties": ["located_in"] })),
        "keys the ontology lacks are dropped from needs"
    );
    // Raised but not yet accepted: this ontology round has no questions to use
    let text = user_text(&f.model.requests.lock().unwrap()[1]);
    assert!(text.contains("(none written yet"), "{text}");

    let written = propose_questions(&f.state, f.kb).await?;
    assert_eq!(written, 1, "the duplicate is skipped, the new one written");
    assert_eq!(f.requests().await, 3);
    let text = user_text(&f.model.requests.lock().unwrap()[2]);
    assert!(text.contains("do not repeat them"), "{text}");
    assert!(
        text.contains("Where is each organization located?"),
        "{text}"
    );

    // One accepted, one asked: the reported counts can be read
    let qs = competency_questions::list(&f.pool, f.kb).await?;
    competency_questions::set_status(&f.pool, f.kb, qs[0].id, "accepted").await?;
    competency_questions::set_status(&f.pool, f.kb, qs[1].id, "accepted").await?;
    assert!(
        competency_questions::record_result(
            &f.pool,
            f.kb,
            qs[0].id,
            &json!({ "answered": true, "judged_by": "shape" })
        )
        .await?
    );
    let r = competency_questions::report(&f.pool, f.kb).await?;
    assert_eq!(
        (r.accepted, r.proposed, r.checked, r.answered),
        (2, 0, 1, 1)
    );
    let p = utopia_store::ontology::agent_proposal_report(&f.pool, f.kb).await?;
    assert_eq!(
        (p.open, p.adopted, p.adopted_edited, p.rejected),
        (0, 0, 0, 0)
    );
    f.cleanup().await
}

/// Adoption that fails midway leaves nothing behind. A literal-value property's example object is an entity, and the regression case cannot be written: before, the property was committed first,
/// alignment was queued, the proposal stayed open while its key was already taken, and this proposal could never be adopted again
#[tokio::test]
async fn a_failed_adoption_leaves_nothing_behind() -> anyhow::Result<()> {
    let Some(f) = Fx::new(Vec::new()).await? else {
        return Ok(());
    };
    let founding_note = json!({ "key": "founding_note", "label": "founding note", "description": "x",
                                "datatype": "text", "domains": ["organization"], "proposed_by": "agent" });
    let shapes = json!({ "phrases": [f.shape("founded in").await?] });
    f.seed("attribute_types", "founding_note", founding_note, shapes)
        .await?;
    let adopt = json!({ "section": "attribute_types", "key": "founding_note" });
    let (status, body) = f.post("/ontology/proposals/adopt", adopt).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "bad_property_shape");
    assert_eq!(
        f.left_by("founding_note").await?,
        (false, "open".to_string(), 0, 0),
        "no attribute, no job, no case; the proposal stays open for a person to reject"
    );
    f.cleanup().await
}

/// The endpoint that only changes status no longer accepts "adopt": before, it marked the proposal adopted, but the element was never built
#[tokio::test]
async fn the_status_only_endpoint_does_not_adopt() -> anyhow::Result<()> {
    let Some(f) = Fx::new(Vec::new()).await? else {
        return Ok(());
    };
    f.seed("relation_types", "founded_in", founded_in(), json!({}))
        .await?;
    let adopt = json!({ "section": "relation_types", "key": "founded_in", "status": "adopted" });
    let (status, body) = f.post("/ontology/proposals", adopt).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "adoption_endpoint");
    assert_eq!(
        f.left_by("founded_in").await?,
        (false, "open".to_string(), 0, 0)
    );
    f.cleanup().await
}

/// A proposal left by the retired Suggest cannot be adopted: the workbench no longer lists it (`stored_proposals`), but before, the adopt endpoint called directly
/// still built the element
#[tokio::test]
async fn a_proposal_the_retired_suggest_left_is_not_adopted() -> anyhow::Result<()> {
    let Some(f) = Fx::new(Vec::new()).await? else {
        return Ok(());
    };
    f.seed("relation_types", "founded_in", founded_in(), json!({}))
        .await?;
    sqlx::query("UPDATE ontology_proposals SET proposed_by='suggest' WHERE kb_id=$1")
        .bind(f.kb)
        .execute(&f.pool)
        .await?;
    let adopt = json!({ "section": "relation_types", "key": "founded_in" });
    let (status, body) = f.post("/ontology/proposals/adopt", adopt).await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    f.cleanup().await
}

/// Adoption, like the editor, queues a vector backfill first and then alignment (new properties queue phrase alignment, new classes queue category-word alignment), committed together with the elements,
/// with no embedding call in the request. Alignment is due 5 seconds late: if both were due at once, both would be picked up together, and alignment would see the backfill still running,
/// so it moves itself half a minute later
#[tokio::test]
async fn adoption_queues_vectors_before_alignment() -> anyhow::Result<()> {
    let Some(f) = Fx::new(Vec::new()).await? else {
        return Ok(());
    };
    // With an embedding model configured: if embedding ran in the request, the embedding endpoint's count would be affected
    sqlx::query(
        "UPDATE llm_settings SET embed_base_url = chat_base_url, embed_model = 'scripted-embed'
          WHERE workspace_id = (SELECT workspace_id FROM knowledge_bases WHERE id = $1)",
    )
    .bind(f.kb)
    .execute(&f.pool)
    .await?;
    f.seed("relation_types", "founded_in", founded_in(), json!({}))
        .await?;
    f.seed("entity_types", "city", city(), json!({})).await?;
    for (section, key, align) in [
        ("relation_types", "founded_in", "align_phrases"),
        ("entity_types", "city", "align_types"),
    ] {
        let adopt = json!({ "section": section, "key": key });
        let (status, body) = f.post("/ontology/proposals/adopt", adopt).await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            (f.queued().await?, f.model.embeds.load(Ordering::SeqCst)),
            (vec!["embed_ontology".to_string(), align.to_string()], 0),
            "{key}: the vectors are queued ahead of alignment, and the request embeds nothing"
        );
        let gap: f64 = sqlx::query_scalar(
            "SELECT extract(epoch FROM max(run_at) - min(run_at))::float8 FROM jobs WHERE payload->>'kb_id'=$1",
        )
        .bind(f.kb.to_string())
        .fetch_one(&f.pool)
        .await?;
        assert!(
            gap >= 5.0,
            "{key}: alignment is due {gap} s after the vectors"
        );
        sqlx::query("DELETE FROM jobs WHERE payload->>'kb_id'=$1")
            .bind(f.kb.to_string())
            .execute(&f.pool)
            .await?;
    }
    f.cleanup().await
}

/// Reusing a property writes one person binding per shape, and one projection is queued for the whole adoption, not one per shape
#[tokio::test]
async fn a_reuse_queues_one_projection_for_its_shapes() -> anyhow::Result<()> {
    let Some(f) = Fx::new(Vec::new()).await? else {
        return Ok(());
    };
    let shapes = json!({ "phrases": [f.shape("located in").await?, f.shape("founded in").await?] });
    let reuse = json!({ "key": "located_in", "label": "located in", "kind": "relation", "proposed_by": "agent" });
    f.seed("map_to", "located_in", reuse, shapes).await?;
    let adopt = json!({ "section": "map_to", "key": "located_in" });
    let (status, body) = f.post("/ontology/proposals/adopt", adopt).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let bound: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM phrase_bindings WHERE kb_id=$1 AND decided_by='person' AND relation_type_id=$2",
    )
    .bind(f.kb)
    .bind(f.located_in)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        (bound, f.queued().await?),
        (2, vec![phrase_bindings::MATERIALIZE_KIND.to_string()]),
        "both shapes are the person's, and one projection follows them"
    );
    f.cleanup().await
}

/// Reusing a class writes the category word as a person's decision; the whole adoption queues a separate phrase alignment, and one already queued is still queued: that one may be picked up
/// before this commit, and would not see the class that changed
#[tokio::test]
async fn a_class_reuse_queues_its_own_phrase_alignment() -> anyhow::Result<()> {
    let Some(f) = Fx::new(Vec::new()).await? else {
        return Ok(());
    };
    utopia_store::jobs::enqueue(&f.pool, "align_phrases", json!({ "kb_id": f.kb })).await?;
    let reuse = json!({ "key": "organization", "kind": "class", "proposed_by": "agent" });
    let words = json!({ "kind_words": ["company"] });
    f.seed("map_to", "organization", reuse, words).await?;
    let adopt = json!({ "section": "map_to", "key": "organization" });
    let (status, body) = f.post("/ontology/proposals/adopt", adopt).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let bound: Option<Uuid> = sqlx::query_scalar(
        "SELECT type_id FROM type_bindings WHERE kb_id=$1 AND kind_word='company' AND decided_by='person'",
    )
    .bind(f.kb)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        (bound, f.queued().await?),
        (Some(f.organization), vec!["align_phrases".to_string(); 2]),
        "company is the person's, and the adoption queues its own phrase alignment"
    );
    f.cleanup().await
}

/// Reuse replaces the decision the agent read, while what a person decided after reading is kept. founded in and firm are "none" that a person stated before the agent read, which is still replaced
/// (firm was read in round one, and company merges into the same row next round; the merge does not lose the moment it was read);
/// located in and company were bound elsewhere by a person after reading: they are kept and listed in the receipt, and located in is committed when a colleague's adoption waiting on that row
/// is committed; headquartered in and wharf were bound by a person after reading exactly as the proposal said: already written, so not rewritten and not listed, while the shape still
/// has its regression case; the wharf-only entry reuses nothing and queues no separate phrase alignment. The venue entry cannot write anything at all, so the whole entry does not count
/// as adopted; old proposals that did not record the moment they were read have all their person decisions left unchanged
#[tokio::test]
async fn a_reuse_keeps_what_a_person_decided_after_the_agent_read_it() -> anyhow::Result<()> {
    let Some(f) = Fx::new(vec![
        // Shapes are ordered by phrase: s0 founded in, s1 headquartered in, s2 located in;
        // category words k0 firm, k1 venue, k2 wharf
        json!({ "p": [], "existing": [{ "key": "located_in", "s": [0, 1, 2] },
            { "key": "organization", "k": [0] }, { "key": "place", "k": [1] },
            { "key": "site", "k": [2] }] }),
        json!({ "p": [], "existing": [{ "key": "organization", "k": [0] }] }),
    ])
    .await?
    else {
        return Ok(());
    };
    f.decide_none().await?;
    f.seed_question().await?;
    let based_in = Uuid::now_v7();
    let site = Uuid::now_v7();
    // Category word placed on an entity that does not appear in the statements: classed, and the shape's ends do not change with it
    sqlx::raw_sql(&format!(
        "INSERT INTO relation_types(id,kb_id,key,label,kind,temporal,description)
             VALUES ('{based_in}','{kb}','based_in','based in','relation','state','x');
         INSERT INTO entity_types(id,kb_id,key,label,color,shape,description)
             VALUES ('{site}','{kb}','site','Site','#000','circle','x');
         INSERT INTO facts(id,kb_id,subject_id,object_id,layer,phrase)
             SELECT '{fact}', kb_id, subject_id, object_id, 'open', 'headquartered in'
               FROM facts WHERE kb_id='{kb}' AND phrase='located in';
         INSERT INTO entities(id,kb_id,canonical_name,specific_type) VALUES
             ('{}','{kb}','Initech','firm'), ('{}','{kb}','Hall','venue'),
             ('{}','{kb}','Globex','company'), ('{}','{kb}','Pier','wharf');",
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        kb = f.kb,
        fact = Uuid::now_v7(),
    ))
    .execute(&f.pool)
    .await?;
    let votes = json!({ "person": "test" });
    let word = |w: &'static str, class: Option<Uuid>, by: &'static str| {
        let status = if class.is_some() { "bound" } else { "none" };
        type_bindings::decide(&f.pool, f.kb, w, &[], class, status, &votes, by)
    };
    let sigs = phrase_bindings::signatures(&f.pool, f.kb).await?;
    let sig = |phrase: &str| sigs.iter().find(|s| s.phrase == phrase).unwrap();
    let person = |property: Option<Uuid>| Decision {
        relation_type_id: property,
        direction: property.map(|_| "forward"),
        status: if property.is_some() { "bound" } else { "none" },
        votes: &votes,
        decided_by: "person",
        basis: None,
        marks: None,
        marks_asked: false,
    };
    let shape = |phrase: &str, property: Option<Uuid>| {
        phrase_bindings::decide(&f.pool, f.kb, sig(phrase), person(property))
    };
    shape("founded in", None).await?;
    shape("headquartered in", None).await?;
    word("firm", None, "person").await?;
    word("venue", None, "agent").await?;
    word("wharf", None, "agent").await?;
    // company has not been judged yet, so it is not sent this round; it is sent next round once judged "none"
    propose(&f.state, f.kb).await?;
    word("company", None, "agent").await?;
    propose(&f.state, f.kb).await?;
    assert_eq!(f.requests().await, 2);

    // A person decided this after the agent read it
    shape("headquartered in", Some(f.located_in)).await?;
    word("company", Some(f.place), "person").await?;
    word("venue", Some(f.organization), "person").await?;
    word("wharf", Some(site), "person").await?;
    // located in was decided by a colleague, and is committed only after located_in's adoption is queued behind that row. That transaction started
    // before the decision the agent read: decided_at is the transaction start time, which is earlier than the read time, yet it was still decided after reading
    let mut deciding = f.pool.begin().await?;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *deciding)
        .await?;
    let colleague = person(Some(based_in));
    phrase_bindings::decide_on(&mut deciding, f.kb, sig("located in"), colleague).await?;
    sqlx::query(
        "UPDATE phrase_bindings SET decided_at = decided_at - interval '1 day'
          WHERE kb_id = $1 AND phrase = 'located in'",
    )
    .bind(f.kb)
    .execute(&mut *deciding)
    .await?;
    let pool = f.pool.clone();
    let commit = async move {
        for _ in 0..500 {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))",
            )
            .bind(pid)
            .fetch_one(&pool)
            .await?;
            if waiting {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        deciding.commit().await?;
        anyhow::Ok(())
    };

    let adopt = |key: &str| json!({ "section": "map_to", "key": key });
    let ((status, body), ()) = tokio::try_join!(
        f.post("/ontology/proposals/adopt", adopt("located_in")),
        commit
    )?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["skipped"],
        json!([{ "phrase": "located in", "subject": "organization", "object": "place", "value": false,
                 "person": { "status": "bound", "property": "based_in", "direction": "forward" } }])
    );
    let (status, body) = f
        .post("/ontology/proposals/adopt", adopt("organization"))
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["skipped"],
        json!([{ "kind_word": "company", "person": { "status": "bound", "class": "place" } }])
    );
    let (status, body) = f.post("/ontology/proposals/adopt", adopt("site")).await?;
    assert_eq!(
        (status, &body["skipped"]),
        (StatusCode::OK, &json!([])),
        "{body}"
    );
    let (status, body) = f.post("/ontology/proposals/adopt", adopt("place")).await?;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::CONFLICT, &json!("proposal_stale")),
        "{body}"
    );
    // A reuse saved in this cell before the read moment was recorded: founded in is now a person's decision, not changed, and the whole entry does not count as adopted
    let reuse = json!({ "key": "based_in", "kind": "relation", "proposed_by": "agent" });
    let shapes = json!({ "phrases": [f.shape("founded in").await?] });
    f.seed("map_to", "based_in", reuse, shapes).await?;
    let (status, body) = f
        .post("/ontology/proposals/adopt", adopt("based_in"))
        .await?;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::CONFLICT, &json!("proposal_stale")),
        "{body}"
    );

    type Row = (String, Option<String>, Option<String>);
    let row = |s: &str, key: &str, via: Option<&str>| -> Row {
        (s.into(), Some(key.into()), via.map(Into::into))
    };
    let shapes: Vec<Row> = sqlx::query_as(
        "SELECT b.phrase, r.key, b.votes #>> '{person,via}' FROM phrase_bindings b
           LEFT JOIN relation_types r ON r.id = b.relation_type_id WHERE b.kb_id = $1 ORDER BY 1",
    )
    .bind(f.kb)
    .fetch_all(&f.pool)
    .await?;
    let words: Vec<Row> = sqlx::query_as(
        "SELECT b.kind_word, t.key, b.votes->>'via' FROM type_bindings b
           LEFT JOIN entity_types t ON t.id = b.type_id WHERE b.kb_id = $1 ORDER BY 1",
    )
    .bind(f.kb)
    .fetch_all(&f.pool)
    .await?;
    let via = Some("ontology_agent");
    assert_eq!(
        (shapes, words),
        (
            vec![
                row("founded in", "located_in", via),
                row("headquartered in", "located_in", None),
                row("located in", "based_in", None),
            ],
            vec![
                row("company", "place", None),
                row("firm", "organization", via),
                row("venue", "organization", None),
                row("wharf", "site", None),
            ]
        ),
        "what the agent read is replaced; what a person decided after it stays theirs"
    );
    let cases: Vec<String> = sqlx::query_scalar(
        "SELECT f.phrase FROM ontology_regression_cases c JOIN facts f ON f.id = c.statement_id
          WHERE c.kb_id = $1 ORDER BY 1",
    )
    .bind(f.kb)
    .fetch_all(&f.pool)
    .await?;
    assert_eq!(
        cases,
        ["founded in", "headquartered in"],
        "none for the kept shape"
    );
    // The venue entry is still open. The last three did not queue extra jobs or write extra cases: the two jobs and two cases came from the first two adoptions
    assert_eq!(f.left_by("place").await?, (true, "open".to_string(), 2, 2));
    f.cleanup().await
}

/// Mid-adoption, the class the proposal needs was deleted: after the deletion it answers unknown_class and writes nothing, rather than hitting the foreign key only when the parent class is written
#[tokio::test]
async fn a_class_deleted_during_adoption_answers_unknown_class() -> anyhow::Result<()> {
    let Some(f) = Fx::new(Vec::new()).await? else {
        return Ok(());
    };
    // Only a class with no entities can be deleted
    sqlx::query(
        "INSERT INTO entity_types(id,kb_id,key,label,color,shape,description)
         VALUES ($1,$2,'region','Region','#000','circle','x')",
    )
    .bind(Uuid::now_v7())
    .bind(f.kb)
    .execute(&f.pool)
    .await?;
    let mut town = city();
    town["parents"] = json!(["region"]);
    f.seed("entity_types", "city", town, json!({})).await?;
    let mut deleting = f.pool.begin().await?;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *deleting)
        .await?;
    sqlx::query("DELETE FROM entity_types WHERE kb_id=$1 AND key='region'")
        .bind(f.kb)
        .execute(&mut *deleting)
        .await?;
    let pool = f.pool.clone();
    let delete = async move {
        // Commit only after adoption is queued behind this deletion
        for _ in 0..500 {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))",
            )
            .bind(pid)
            .fetch_one(&pool)
            .await?;
            if waiting {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        deleting.commit().await?;
        anyhow::Ok(())
    };
    let adopt = json!({ "section": "entity_types", "key": "city" });
    let ((status, body), ()) =
        tokio::try_join!(f.post("/ontology/proposals/adopt", adopt), delete)?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "unknown_class");
    assert_eq!(f.left_by("city").await?, (false, "open".to_string(), 0, 0));
    f.cleanup().await
}

#[tokio::test]
async fn adoption_skips_negative_examples_without_vetoing_creation_or_reuse() -> anyhow::Result<()>
{
    for (section, key) in [("relation_types", "founded_in"), ("map_to", "located_in")] {
        for ordinary_example in [false, true] {
            let Some(f) = Fx::new(Vec::new()).await? else {
                return Ok(());
            };
            let result = async {
                f.decide_none().await?;
                let negative = f.shape("founded in").await?;
                let statement_id =
                    serde_json::from_value(negative["example_statement_ids"][0].clone())?;
                let case = utopia_store::ontology_regressions::add(
                    &f.pool,
                    f.kb,
                    utopia_store::ontology_regressions::NewCase {
                        statement_id,
                        expected_property_id: None,
                        expected_direction: None,
                        created_by: f.user,
                        origin: "person",
                    },
                )
                .await?;
                let before = utopia_store::ontology_regressions::get(&f.pool, f.kb, case)
                    .await?
                    .unwrap();
                let mut shapes = vec![negative];
                if ordinary_example {
                    shapes.push(f.shape("located in").await?);
                }
                f.seed(
                    section,
                    key,
                    if section == "map_to" {
                        json!({"kind":"relation"})
                    } else {
                        founded_in()
                    },
                    json!({"phrases":shapes}),
                )
                .await?;
                let (status, body) = f
                    .post(
                        "/ontology/proposals/adopt",
                        json!({"section":section,"key":key}),
                    )
                    .await?;
                assert_eq!(status, StatusCode::OK, "{body}");
                assert_eq!(
                    f.left_by(key).await?,
                    (
                        true,
                        "adopted".into(),
                        if section == "map_to" { 1 } else { 2 },
                        1 + i64::from(ordinary_example)
                    )
                );
                let positive: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM ontology_regression_cases
                    WHERE kb_id=$1 AND statement_id=$2 AND expected_property_id IS NOT NULL",
                )
                .bind(f.kb)
                .bind(statement_id)
                .fetch_one(&f.pool)
                .await?;
                assert_eq!(
                    positive, 0,
                    "negative examples do not gain an automatic positive case"
                );
                let property: Uuid = serde_json::from_value(body["id"].clone())?;
                // Creation queues alignment; exercise that later committed decision without a model.
                // Reuse has already written the person's binding in the adoption transaction.
                if section != "map_to" {
                    let sig = phrase_bindings::signatures(&f.pool, f.kb)
                        .await?
                        .into_iter()
                        .find(|s| s.phrase == "founded in")
                        .unwrap();
                    phrase_bindings::decide(
                        &f.pool,
                        f.kb,
                        &sig,
                        Decision {
                            relation_type_id: Some(property),
                            direction: Some("forward"),
                            status: "bound",
                            decided_by: "agent",
                            votes: &Value::Null,
                            basis: None,
                            marks: None,
                            marks_asked: false,
                        },
                    )
                    .await?;
                }
                let after = utopia_store::ontology_regressions::get(&f.pool, f.kb, case)
                    .await?
                    .unwrap();
                assert_eq!(
                    (after.created_by, after.created_at, after.origin),
                    (before.created_by, before.created_at, before.origin)
                );
                let report = after.last_result.unwrap();
                assert_eq!(report["passed"], false);
                assert_eq!(report["actual_property_id"], json!(property));
                assert_eq!(
                    report["conflict"],
                    if section == "map_to" {
                        json!("human_binding")
                    } else {
                        Value::Null
                    }
                );
                assert!(f.model.requests.lock().unwrap().is_empty());
                assert_eq!(f.model.embeds.load(Ordering::SeqCst), 0);
                anyhow::Ok(())
            }
            .await;
            f.cleanup().await?;
            result?;
        }
    }
    Ok(())
}
