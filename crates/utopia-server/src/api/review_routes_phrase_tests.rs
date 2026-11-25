//! A person sets a short-phrase signature: the verdict and its recompute job are submitted together; the request answers 202 with a job id and does not make up a number (0051).
//!
//! Three things: the route answers 202, the job is queued, and the binding is written as the person decided; `GET /kbs/{id}/jobs/{job_id}` is readable in this database,
//! and in another database answers 404; a Viewer can read the status but cannot decide. Without `UTOPIA_DATABASE_URL` the test is skipped.
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use utopia_store::phrase_bindings;
use uuid::Uuid;

struct Fx {
    pool: sqlx::PgPool,
    app: axum::Router,
    org: Uuid,
    kb: Uuid,
    other_kb: Uuid,
    editor: String,
    viewer: String,
    binding: Uuid,
    _dir: tempfile::TempDir,
}

impl Fx {
    async fn new() -> anyhow::Result<Option<Self>> {
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(None);
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        utopia_store::db::migrate(&pool).await?;
        let (org, ws, kb, other_kb, editor, viewer, subject, object, property, statement) = (
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
        // Only locally generated UUIDs are interpolated into fixture SQL.
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','phrase-route-test');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','phrase-route-test');
             INSERT INTO users(id,org_id,email,display_name,password_hash) VALUES
                 ('{editor}','{org}','{editor}@phrase.test','editor','unused'),
                 ('{viewer}','{org}','{viewer}@phrase.test','viewer','unused');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES
                 ('{kb}','{ws}','phrases'), ('{other_kb}','{ws}','other');
             INSERT INTO kb_members(kb_id,user_id,role) VALUES
                 ('{kb}','{editor}','editor'), ('{kb}','{viewer}','viewer'),
                 ('{other_kb}','{editor}','editor');
             INSERT INTO entities(id,kb_id,canonical_name) VALUES
                 ('{subject}','{kb}','Acme'), ('{object}','{kb}','London');
             INSERT INTO relation_types(id,kb_id,key,label,temporal) VALUES
                 ('{property}','{kb}','based_in','based in','state');
             INSERT INTO facts(id,kb_id,subject_id,object_id,layer,phrase) VALUES
                 ('{statement}','{kb}','{subject}','{object}','open','based in');"
        ))
        .execute(&pool)
        .await?;
        // One entry in the queue: the agent judged it undecided, and a person decides
        let sig = phrase_bindings::signatures(&pool, kb).await?.remove(0);
        phrase_bindings::decide(
            &pool,
            kb,
            &sig,
            phrase_bindings::Decision {
                relation_type_id: None,
                direction: None,
                status: "undecided",
                votes: &json!({}),
                decided_by: "agent",
                basis: None,
                marks: None,
                marks_asked: false,
            },
        )
        .await?;
        let binding: Uuid = sqlx::query_scalar("SELECT id FROM phrase_bindings WHERE kb_id=$1")
            .bind(kb)
            .fetch_one(&pool)
            .await?;
        let dir = tempfile::tempdir()?;
        let cfg = utopia_core::config::AppConfig {
            data_dir: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let search = Arc::new(utopia_search::SearchIndex::open(
            &dir.path().join("search"),
        )?);
        let state = crate::state::AppState::new(pool.clone(), &cfg, search, "test-only".into());
        let editor_token = crate::auth::issue_token(&state, editor)?;
        let viewer_token = crate::auth::issue_token(&state, viewer)?;
        let app = super::super::router(state, &cfg);
        Ok(Some(Self {
            pool,
            app,
            org,
            kb,
            other_kb,
            editor: editor_token,
            viewer: viewer_token,
            binding,
            _dir: dir,
        }))
    }

    async fn call(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("Authorization", format!("Bearer {token}"));
        let request = match body {
            Some(b) => request
                .header("Content-Type", "application/json")
                .body(Body::from(b.to_string()))?,
            None => request.body(Body::empty())?,
        };
        let response = self.app.clone().oneshot(request).await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await?;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        };
        Ok((status, value))
    }

    async fn cleanup(self) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM jobs WHERE payload->>'kb_id'=$1")
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

#[tokio::test]
async fn a_phrase_decision_is_accepted_with_its_job() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        let path = format!(
            "/api/v1/kbs/{}/review/alignment/phrases/{}",
            f.kb, f.binding
        );
        let (status, body) = f
            .call(
                &f.editor,
                "POST",
                &path,
                Some(json!({ "property": "based_in", "direction": "forward" })),
            )
            .await?;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["ok"], json!(true));
        assert_eq!(body["status"], json!("accepted"));
        assert!(body.get("typed").is_none(), "no invented counts: {body}");
        let job_id = body["job_id"].as_i64().expect("job id");

        // Job queued, payload is this database; the binding is person-decided, bound
        let (kind, job_status, payload): (String, String, Value) =
            sqlx::query_as("SELECT kind, status, payload FROM jobs WHERE id=$1")
                .bind(job_id)
                .fetch_one(&f.pool)
                .await?;
        assert_eq!(kind, phrase_bindings::MATERIALIZE_KIND);
        assert_eq!(job_status, "queued");
        assert_eq!(payload["kb_id"], json!(f.kb));
        let b = phrase_bindings::bindings(&f.pool, f.kb).await?.remove(0);
        assert_eq!(
            (b.status.as_str(), b.decided_by.as_str()),
            ("bound", "person")
        );
        // This request did not recompute: the projection waits for the job
        assert_eq!(utopia_store::materialize::count(&f.pool, f.kb).await?, 0);

        // Status read: 200 in this database, 404 in another, Viewers can read too
        let (status, body) = f
            .call(
                &f.editor,
                "GET",
                &format!("/api/v1/kbs/{}/jobs/{job_id}", f.kb),
                None,
            )
            .await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["job"]["status"], json!("queued"));
        assert_eq!(
            body["job"]["kind"],
            json!(phrase_bindings::MATERIALIZE_KIND)
        );
        let (status, _) = f
            .call(
                &f.editor,
                "GET",
                &format!("/api/v1/kbs/{}/jobs/{job_id}", f.other_kb),
                None,
            )
            .await?;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = f
            .call(
                &f.viewer,
                "GET",
                &format!("/api/v1/kbs/{}/jobs/{job_id}", f.kb),
                None,
            )
            .await?;
        assert_eq!(status, StatusCode::OK);

        // A Viewer cannot decide
        let (status, _) = f
            .call(
                &f.viewer,
                "POST",
                &path,
                Some(json!({ "property": null, "direction": "forward" })),
            )
            .await?;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Do what the job should do, here once: read the current binding and compute one typed row
        let outcome = utopia_store::materialize::try_materialize(&f.pool, f.kb)
            .await?
            .expect("nobody holds the lock");
        assert_eq!(outcome.added, 1);
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// Under a state property, when a person signs the signature, the date of a statement carrying only one date is labelled (#966). If not stated, accept it and the signature
/// stays in the alignment queue for a person to fill in; the property and direction are the ones bound now. If stated, record it and dequeue. An unrecognized value, or a property that is
/// not a state property (or no property), answers 422 and writes nothing
#[tokio::test]
async fn a_phrase_decision_under_a_state_says_what_a_single_date_marks() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        let path = format!(
            "/api/v1/kbs/{}/review/alignment/phrases/{}",
            f.kb, f.binding
        );
        let queue = format!(
            "/api/v1/kbs/{}/review?queue=alignment&limit=10&offset=0",
            f.kb
        );
        let (status, body) = f
            .call(
                &f.editor,
                "POST",
                &path,
                Some(json!({ "property": "based_in", "direction": "forward" })),
            )
            .await?;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let (status, body) = f.call(&f.editor, "GET", &queue, None).await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["counts"]["alignment"], 1, "{body}");
        assert_eq!(body["items"][0]["bound_to"], "based_in", "{body}");
        assert_eq!(body["items"][0]["direction"], "forward", "{body}");

        let (status, body) = f
            .call(
                &f.editor,
                "POST",
                &path,
                Some(json!({ "property": "based_in", "direction": "forward", "marks": "start" })),
            )
            .await?;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let b = phrase_bindings::bindings(&f.pool, f.kb).await?.remove(0);
        assert_eq!(
            (b.decided_by.as_str(), b.marks.as_deref()),
            ("person", Some("start"))
        );
        let votes: Value = sqlx::query_scalar("SELECT votes FROM phrase_bindings WHERE id=$1")
            .bind(f.binding)
            .fetch_one(&f.pool)
            .await?;
        assert_eq!(votes["person"]["marks"], "start", "{votes}");
        let (_, body) = f.call(&f.editor, "GET", &queue, None).await?;
        assert_eq!(body["counts"]["alignment"], 0, "{body}");

        sqlx::query(
            "INSERT INTO relation_types(id,kb_id,key,label,temporal)
             VALUES ($1,$2,'acquired','acquired','event')",
        )
        .bind(Uuid::now_v7())
        .bind(f.kb)
        .execute(&f.pool)
        .await?;
        for (request, code) in [
            (
                json!({ "property": "based_in", "direction": "forward", "marks": "sometimes" }),
                "unknown_marks",
            ),
            (
                json!({ "property": "acquired", "direction": "forward", "marks": "start" }),
                "marks_needs_state",
            ),
            (
                json!({ "property": null, "marks": "start" }),
                "marks_needs_state",
            ),
        ] {
            let (status, body) = f.call(&f.editor, "POST", &path, Some(request)).await?;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(body["code"], code, "{body}");
        }
        let b = phrase_bindings::bindings(&f.pool, f.kb).await?.remove(0);
        assert_eq!(
            (b.relation_type_id.is_some(), b.marks.as_deref()),
            (true, Some("start")),
            "a refused request writes nothing"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn a_rule_decision_is_accepted_with_its_job() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        let property: Uuid =
            sqlx::query_scalar("SELECT id FROM relation_types WHERE kb_id=$1 AND key='based_in'")
                .bind(f.kb)
                .fetch_one(&f.pool)
                .await?;
        let votes = json!({});
        let rule = utopia_store::implication_rules::propose(
            &f.pool,
            f.kb,
            &utopia_store::implication_rules::Proposal {
                trigger: "phrase",
                phrase: "based in",
                subject_type_id: None,
                object_type_id: None,
                object_is_value: false,
                conclude_property_id: property,
                reading: Some("country_of_place"),
                status: "proposed",
                votes: &votes,
                basis: "b",
                statement_count: 1,
                examples: &[],
            },
        )
        .await?
        .expect("proposed");
        let path = format!("/api/v1/kbs/{}/review/alignment/rules/{}", f.kb, rule);
        // Viewer cannot batch-approve
        let (status, _) = f
            .call(&f.viewer, "POST", &path, Some(json!({ "approve": true })))
            .await?;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = f
            .call(&f.editor, "POST", &path, Some(json!({ "approve": true })))
            .await?;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let job_id = body["job_id"].as_i64().expect("job id");
        let kind: String = sqlx::query_scalar("SELECT kind FROM jobs WHERE id=$1")
            .bind(job_id)
            .fetch_one(&f.pool)
            .await?;
        assert_eq!(
            kind,
            utopia_store::implication_rules::READ_KIND,
            "a reading is needed first"
        );
        let r = utopia_store::implication_rules::get(&f.pool, f.kb, rule)
            .await?
            .unwrap();
        assert_eq!(
            (r.status.as_str(), r.decided_by.as_str()),
            ("approved", "person")
        );
        // An id from another database answers 404
        let (status, _) = f
            .call(
                &f.editor,
                "POST",
                &format!("/api/v1/kbs/{}/review/alignment/rules/{}", f.other_kb, rule),
                Some(json!({ "approve": false })),
            )
            .await?;
        assert_eq!(status, StatusCode::NOT_FOUND);
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// Errata queue (0044 decision 7): a person approves an entry the gate left; the action executes and answers 200; answering again gives 404; another database 404; Viewer 403
#[tokio::test]
async fn a_held_errata_action_is_decided_by_a_person() -> anyhow::Result<()> {
    let Some(f) = Fx::new().await? else {
        return Ok(());
    };
    let run = async {
        let (doc, chunk, typed_fact, run_id, action) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        let (subject, property): (Uuid, Uuid) = sqlx::query_as(
            "SELECT e.id, r.id FROM entities e, relation_types r
              WHERE e.kb_id=$1 AND e.canonical_name='Acme' AND r.kb_id=$1 AND r.key='based_in'",
        )
        .bind(f.kb)
        .fetch_one(&f.pool)
        .await?;
        let object: Uuid =
            sqlx::query_scalar("SELECT id FROM entities WHERE kb_id=$1 AND canonical_name='London'")
                .bind(f.kb)
                .fetch_one(&f.pool)
                .await?;
        sqlx::raw_sql(&format!(
            "INSERT INTO documents(id,kb_id,filename,sha256) VALUES ('{doc}','{}','acme.txt','x');
             INSERT INTO chunks(id,kb_id,document_id,seq,text) VALUES ('{chunk}','{}','{doc}',0,'Acme is based in London.');
             INSERT INTO facts(id,kb_id,subject_id,predicate_id,object_id,layer) VALUES
                 ('{typed_fact}','{}','{subject}','{property}','{object}','typed');
             INSERT INTO errata_runs(id,kb_id,document_id) VALUES ('{run_id}','{}','{doc}');
             INSERT INTO errata_actions(id,kb_id,run_id,document_id,fact_id,predicate_id,action,reason,quote,proposed,status,detail)
             VALUES ('{action}','{}','{run_id}','{doc}','{typed_fact}','{property}','retract','wrong','Acme is based in London',
                     '{{\"subject\":\"Acme\",\"property\":\"based_in\",\"object\":\"London\",\"subject_id\":\"{subject}\",\"predicate_id\":\"{property}\",\"object_id\":\"{object}\"}}',
                     'held','derived 1');",
            f.kb, f.kb, f.kb, f.kb, f.kb
        ))
        .execute(&f.pool)
        .await?;
        let path = format!("/api/v1/kbs/{}/review/errata/{}", f.kb, action);
        let (status, _) = f
            .call(&f.viewer, "POST", &path, Some(json!({ "approve": true })))
            .await?;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let other = format!("/api/v1/kbs/{}/review/errata/{}", f.other_kb, action);
        let (status, _) = f
            .call(&f.editor, "POST", &other, Some(json!({ "approve": true })))
            .await?;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // It is in the queue
        let (status, body) = f
            .call(&f.editor, "GET", &format!("/api/v1/kbs/{}/review?queue=errata&limit=10&offset=0", f.kb), None)
            .await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["counts"]["errata"], 1);
        assert_eq!(body["items"][0]["proposed"]["object"], "London");
        assert_eq!(body["items"][0]["detail"], "derived 1");
        let (status, body) = f
            .call(&f.editor, "POST", &path, Some(json!({ "approve": true })))
            .await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        let live: bool = sqlx::query_scalar("SELECT invalidated_at IS NULL FROM facts WHERE id=$1")
            .bind(typed_fact)
            .fetch_one(&f.pool)
            .await?;
        assert!(!live, "approving a held retraction retracts");
        let (status, _) = f
            .call(&f.editor, "POST", &path, Some(json!({ "approve": false })))
            .await?;
        assert_eq!(status, StatusCode::NOT_FOUND, "answered once");
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}
