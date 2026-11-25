//! After tombstone cleanup, derivations must catch up (#875). `DELETE /documents/{id}` re-pushes derivations per the library's switch after deleting one document
//! (`settle_derivations`, shared by deletion, revocation and sync revival); `POST .../missing/cleanup` deletes a batch of
//! tombstone documents at once. Previously, deleting them did not re-push: conclusions resting on those documents stayed in place until the next scheduled derivation run.
//! The same fixture walks both paths; single-document deletion is the control.
//!
//! A test that connects to the database; skipped without `UTOPIA_DATABASE_URL` (same as documents_routes_tests).

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use utopia_core::models::RelationAxioms;
use utopia_store::business_rules::ConditionInput;
use utopia_store::graph::Validity;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    app: axum::Router,
    org: Uuid,
    kb: Uuid,
    source: Uuid,
    token: String,
    cup: Uuid,
    location: Uuid,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> anyhow::Result<Option<Self>> {
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(None);
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        utopia_store::db::migrate(&pool).await?;
        let (org, ws, kb, user, source) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        // Only locally generated UUIDs are interpolated into fixture SQL.
        // The base keeps the column default: materialized inference is on (0050).
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','cleanup-settle-test');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','cleanup-settle-test');
             INSERT INTO users(id,org_id,email,display_name,password_hash)
                 VALUES ('{user}','{org}','{user}@cleanup.test','cleanup-test','unused');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','observations');
             INSERT INTO kb_members(kb_id,user_id,role) VALUES ('{kb}','{user}','editor');
             INSERT INTO sources(id,kb_id,kind,name) VALUES ('{source}','{kb}','api','robot-1');"
        ))
        .execute(&pool)
        .await?;
        let cup = utopia_store::ontology::create_entity_type(
            &pool,
            kb,
            "cup",
            "Cup",
            "#7fd0ff",
            "circle",
            &[],
            "",
        )
        .await?;
        let ready = utopia_store::ontology::create_entity_type(
            &pool,
            kb,
            "step_ready",
            "Step precondition holds",
            "#7fd0ff",
            "circle",
            &[],
            "",
        )
        .await?;
        let location = utopia_store::ontology::create_relation_type(
            &pool,
            kb,
            "location",
            "location",
            "state",
            RelationAxioms {
                functional: true,
                ..Default::default()
            },
            "",
            "attribute",
            &[cup],
            &[],
            Some("text"),
            None,
        )
        .await?;
        utopia_store::business_rules::create(
            &pool,
            kb,
            "pick cup from desk",
            "",
            cup,
            "typing",
            Some(ready),
            None,
            None,
            None,
            None,
            &[ConditionInput {
                group: 0,
                predicate_id: location,
                op: "in".into(),
                operand: Some(json!(["desk"])),
                side: "x".into(),
            }],
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
        let state = crate::state::AppState::new(pool.clone(), &cfg, search, "test-only".into());
        let token = crate::auth::issue_token(&state, user)?;
        let app = super::super::router(state.clone(), &cfg);
        Ok(Some(Self {
            pool,
            app,
            org,
            kb,
            source,
            token,
            cup,
            location,
            _dir: dir,
        }))
    }

    /// A thing, one document attached under a source, and one reading whose only evidence is that document, "on the table"
    async fn reading(&self, name: &str) -> anyhow::Result<(Uuid, Uuid, Uuid)> {
        let (doc, chunk, thing) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        sqlx::query(
            "INSERT INTO documents (id, kb_id, source_id, filename, sha256, external_key, status)
             VALUES ($1, $2, $3, $4, $5, $6, 'ready')",
        )
        .bind(doc)
        .bind(self.kb)
        .bind(self.source)
        .bind(format!("{name}.json"))
        .bind(format!("sha-{doc}"))
        .bind(format!("api:{name}"))
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, $4)",
        )
        .bind(chunk)
        .bind(self.kb)
        .bind(doc)
        .bind(format!("{name} is on desk"))
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(thing)
        .bind(self.kb)
        .bind(self.cup)
        .bind(name)
        .execute(&self.pool)
        .await?;
        let (fact, _) = utopia_store::graph::insert_value_fact(
            &self.pool,
            self.kb,
            thing,
            Some(self.location),
            &json!({ "value": "desk" }),
            Validity::default().attested(Some("2026-09-23T08:00:00Z".parse()?)),
            1.0,
        )
        .await?;
        utopia_store::graph::add_evidence(&self.pool, fact, chunk, None, Some("is on")).await?;
        Ok((doc, thing, fact))
    }

    async fn call(&self, method: &str, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("Authorization", format!("Bearer {}", self.token))
                    .body(Body::empty())?,
            )
            .await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await?;
        Ok((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ))
    }

    /// Does this thing still carry a rule-derived conclusion right now?
    async fn concluded(&self, thing: Uuid) -> anyhow::Result<bool> {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM derived_facts WHERE subject_id = $1 AND invalidated_at IS NULL",
        )
        .bind(thing)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    async fn conclusion_ids(&self, thing: Uuid) -> anyhow::Result<Vec<Uuid>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM derived_facts WHERE subject_id = $1 AND invalidated_at IS NULL ORDER BY id",
        )
        .bind(thing)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn cleanup(self) -> anyhow::Result<()> {
        // Library first: the deletion records (document_deletions) cascade with the library, and they remember who deleted, so that person must be kept until last
        sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
            .bind(self.kb)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(self.org)
            .execute(&self.pool)
            .await?;
        self.pool.close().await;
        Ok(())
    }
}

#[tokio::test]
async fn a_cleanup_retires_what_its_documents_concluded() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let run = async {
        let (deleted, by_delete, _) = f.reading("cup-1").await?;
        let (_, by_cleanup, reading) = f.reading("cup-2").await?;
        let (_, unrelated, _) = f.reading("cup-3").await?;
        utopia_store::reasoning::materialize(&f.pool, f.kb).await?;
        assert!(f.concluded(by_delete).await? && f.concluded(by_cleanup).await?);
        let cleanup_before = f.conclusion_ids(by_cleanup).await?;
        let unrelated_before = f.conclusion_ids(unrelated).await?;
        assert_eq!(unrelated_before.len(), 1);

        // Control: delete one document, re-push per the switch right after deleting
        let (status, body) = f
            .call("DELETE", &format!("/api/v1/documents/{deleted}"))
            .await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !f.concluded(by_delete).await?,
            "a single delete settles the derivations"
        );
        assert_eq!(f.conclusion_ids(by_cleanup).await?, cleanup_before);
        assert_eq!(f.conclusion_ids(unrelated).await?, unrelated_before);

        // Tombstone + cleanup: the same thing, done as a batch
        utopia_store::documents::mark_missing_keys(&f.pool, f.source, &["api:cup-2".into()])
            .await?;
        let (status, body) = f
            .call(
                "POST",
                &format!("/api/v1/kbs/{}/sources/{}/missing/cleanup", f.kb, f.source),
            )
            .await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted"], json!(1));
        let (gone,): (bool,) =
            sqlx::query_as("SELECT invalidated_at IS NOT NULL FROM facts WHERE id = $1")
                .bind(reading)
                .fetch_one(&f.pool)
                .await?;
        assert!(gone, "the reading's only evidence was deleted");
        assert!(
            !f.concluded(by_cleanup).await?,
            "the conclusion that reading supported must be retired as after a single delete"
        );
        assert_eq!(
            f.conclusion_ids(unrelated).await?,
            unrelated_before,
            "cleanup must preserve the pre-existing unrelated conclusion row"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}
