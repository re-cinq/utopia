//! A personal token with write permission can set a library up from nothing (#1243): create the library, create sources, read the push key,
//! import the ontology, and define category words. As before, a token only narrows: the person's role is the ceiling, and scope and `kb_ids` press lower still.
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
    ws: Uuid,
    /// Workspace admin: can create a library
    admin: Uuid,
    /// Workspace viewer: even with write ticked on the token, cannot create a library
    viewer: Uuid,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> anyhow::Result<Option<Self>> {
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(None);
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        utopia_store::db::migrate(&pool).await?;
        let (org, ws, admin, viewer) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','token-setup-test');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','token-setup-test');
             INSERT INTO users(id,org_id,email,display_name,password_hash) VALUES
                 ('{admin}','{org}','{admin}@token-setup.test','Admin','unused'),
                 ('{viewer}','{org}','{viewer}@token-setup.test','Viewer','unused');
             INSERT INTO memberships(user_id,workspace_id,role) VALUES
                 ('{admin}','{ws}','admin'), ('{viewer}','{ws}','viewer');"
        ))
        .execute(&pool)
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
        let app = super::router(state, &cfg);
        Ok(Some(Self {
            pool,
            app,
            org,
            ws,
            admin,
            viewer,
            _dir: dir,
        }))
    }

    async fn token(&self, user: Uuid, scope: &str, kb_ids: Option<&[Uuid]>) -> String {
        utopia_store::tokens::issue(&self.pool, user, "ci", scope, kb_ids, None)
            .await
            .expect("issuing a token")
            .1
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let request = Request::builder()
            .method(method)
            .uri(format!("/api/v1{path}"))
            .header("authorization", format!("Bearer {token}"));
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
            None => request.body(Body::empty())?,
        };
        self.respond(request).await
    }

    /// The ontology import accepts multipart. An empty form is rejected at the step of reading the upload (`no_files`),
    /// which is enough to tell "the token was blocked at the door" from "got through the door, but the request itself is wrong"
    async fn import_nothing(&self, kb: Uuid, token: &str) -> anyhow::Result<(StatusCode, Value)> {
        let request = Request::post(format!("/api/v1/kbs/{kb}/ontology/imports"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "multipart/form-data; boundary=x")
            .body(Body::from("--x--\r\n"))?;
        self.respond(request).await
    }

    /// Really imports a Turtle file: the field must carry a file name, since reading the upload only accepts ones that have a file name, as the web import panel does
    async fn import_turtle(
        &self,
        kb: Uuid,
        token: &str,
        ttl: &str,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let body = format!(
            "--x\r\nContent-Disposition: form-data; name=\"file\"; filename=\"o.ttl\"\r\n\r\n\
             {ttl}\r\n--x--\r\n"
        );
        let request = Request::post(format!("/api/v1/kbs/{kb}/ontology/imports"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "multipart/form-data; boundary=x")
            .body(Body::from(body))?;
        self.respond(request).await
    }

    async fn respond(&self, request: Request<Body>) -> anyhow::Result<(StatusCode, Value)> {
        let response = self.app.clone().oneshot(request).await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok((status, body))
    }

    async fn create_kb(&self, token: &str, name: &str) -> anyhow::Result<(StatusCode, Value)> {
        let ws = self.ws;
        self.send(
            "POST",
            &format!("/workspaces/{ws}/kbs"),
            token,
            Some(json!({ "name": name })),
        )
        .await
    }

    async fn create_source(&self, kb: Uuid, token: &str) -> anyhow::Result<(StatusCode, Value)> {
        self.create_source_of_kind(kb, token, "statements").await
    }

    async fn create_source_of_kind(
        &self,
        kb: Uuid,
        token: &str,
        kind: &str,
    ) -> anyhow::Result<(StatusCode, Value)> {
        self.send(
            "POST",
            &format!("/kbs/{kb}/sources"),
            token,
            Some(json!({ "kind": kind, "name": "ci" })),
        )
        .await
    }

    async fn push_key(
        &self,
        kb: Uuid,
        source: Uuid,
        token: &str,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let path = format!("/kbs/{kb}/sources/{source}/token");
        self.send("GET", &path, token, None).await
    }

    async fn decide_kind_word(&self, kb: Uuid, token: &str) -> anyhow::Result<(StatusCode, Value)> {
        let path = format!("/kbs/{kb}/review/alignment/kind-words/robot");
        self.send("POST", &path, token, Some(json!({}))).await
    }

    async fn list_kbs(&self, token: &str) -> anyhow::Result<(StatusCode, Value)> {
        let ws = self.ws;
        self.send("GET", &format!("/workspaces/{ws}/kbs"), token, None)
            .await
    }

    /// Write directly to the database to create a library owned by an admin: cases testing only the token boundary need not go through library creation first
    async fn seeded_kb(&self, name: &str) -> anyhow::Result<Uuid> {
        let kb = Uuid::now_v7();
        sqlx::query("INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ($1,$2,$3)")
            .bind(kb)
            .bind(self.ws)
            .bind(name)
            .execute(&self.pool)
            .await?;
        sqlx::query("INSERT INTO kb_members(kb_id,user_id,role) VALUES ($1,$2,'admin')")
            .bind(kb)
            .bind(self.admin)
            .execute(&self.pool)
            .await?;
        Ok(kb)
    }

    async fn cleanup(self) -> anyhow::Result<()> {
        // Tasks are attached to the library through the kb_id in their payload, with no foreign key, so deleting the organisation does not take them along
        sqlx::query(
            "DELETE FROM jobs WHERE payload->>'kb_id' IN
                 (SELECT id::text FROM knowledge_bases WHERE workspace_id = $1)",
        )
        .bind(self.ws)
        .execute(&self.pool)
        .await?;
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(self.org)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[tokio::test]
async fn a_live_write_token_of_a_workspace_admin_sets_up_a_base_and_a_dead_one_does_not(
) -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let result = async {
        let token = f.token(f.admin, "write", None).await;
        let token_id = utopia_store::tokens::authenticate(&f.pool, &token)
            .await?
            .token_id;

        let (status, kb) = f.create_kb(&token, "lore-mirror").await?;
        anyhow::ensure!(status == StatusCode::OK, "{status} {kb}");
        let kb: Uuid = serde_json::from_value(kb["id"].clone())?;

        let (status, created) = f.create_source(kb, &token).await?;
        anyhow::ensure!(status == StatusCode::OK, "{status} {created}");
        let source: Uuid = serde_json::from_value(created["source"]["id"].clone())?;

        let (status, key) = f.push_key(kb, source, &token).await?;
        anyhow::ensure!(status == StatusCode::OK, "{status} {key}");
        anyhow::ensure!(
            key["ingest_token"] == created["ingest_token"] && key["ingest_token"].is_string(),
            "{key} vs {created}"
        );

        // Got through the door, but the request itself is wrong: an empty upload has passed the token and role checks and stops at the step of reading the upload
        let (status, body) = f.import_nothing(kb, &token).await?;
        anyhow::ensure!(body["code"] == "no_files", "{status} {body}");
        let (status, body) = f.decide_kind_word(kb, &token).await?;
        anyhow::ensure!(status == StatusCode::OK, "{status} {body}");

        // Audit records the person who issued the token, same as for a session
        let actors: Vec<(String, Option<Uuid>)> = sqlx::query_as(
            "SELECT action, actor_id FROM audit_events
              WHERE kb_id = $1 AND action IN ('source.created', 'alignment.kind_word_decided')
              ORDER BY action",
        )
        .bind(kb)
        .fetch_all(&f.pool)
        .await?;
        anyhow::ensure!(
            actors
                == [
                    ("alignment.kind_word_decided".to_string(), Some(f.admin)),
                    ("source.created".to_string(), Some(f.admin)),
                ],
            "{actors:?}"
        );
        // Which token acted on someone's behalf (decision 5 relies on it): recorded in detail
        let detail: Value = sqlx::query_scalar(
            "SELECT detail FROM audit_events WHERE kb_id = $1 AND action = 'source.created'",
        )
        .bind(kb)
        .fetch_one(&f.pool)
        .await?;
        anyhow::ensure!(detail["token_id"] == json!(token_id), "{detail}");

        // The same person, the same path that just succeeded: expired and revoked tokens are still blocked at the door
        let (_, expired) = utopia_store::tokens::issue(
            &f.pool,
            f.admin,
            "expired",
            "write",
            None,
            Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
        )
        .await?;
        let (revoked_view, revoked) =
            utopia_store::tokens::issue(&f.pool, f.admin, "revoked", "write", None, None).await?;
        utopia_store::tokens::revoke(&f.pool, f.admin, revoked_view.id).await?;
        for token in [expired, revoked] {
            let (status, body) = f.create_source(kb, &token).await?;
            anyhow::ensure!(status == StatusCode::UNAUTHORIZED, "{status} {body}");
        }
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    result
}

#[tokio::test]
async fn a_write_token_creates_an_api_source_and_reads_its_push_key() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let result = async {
        let kb = f.seeded_kb("docs-mirror").await?;
        let token = f.token(f.admin, "write", None).await;

        let (status, created) = f.create_source_of_kind(kb, &token, "api").await?;
        anyhow::ensure!(status == StatusCode::OK, "{status} {created}");
        let source: Uuid = serde_json::from_value(created["source"]["id"].clone())?;

        let (status, key) = f.push_key(kb, source, &token).await?;
        anyhow::ensure!(
            status == StatusCode::OK
                && key["ingest_token"] == created["ingest_token"]
                && key["ingest_token"].is_string(),
            "{status} {key} vs {created}"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    result
}

#[tokio::test]
async fn a_write_token_creating_a_url_source_is_refused_with_token_source_kind(
) -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let result = async {
        let kb = f.seeded_kb("pull").await?;
        let token = f.token(f.admin, "write", None).await;

        let (status, body) = f.create_source_of_kind(kb, &token, "url").await?;
        anyhow::ensure!(
            status == StatusCode::FORBIDDEN && body["code"] == "token_source_kind",
            "{status} {body}"
        );
        let sources: i64 = sqlx::query_scalar("SELECT count(*) FROM sources WHERE kb_id = $1")
            .bind(kb)
            .fetch_one(&f.pool)
            .await?;
        anyhow::ensure!(sources == 0, "{sources} sources");
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    result
}

#[tokio::test]
async fn a_write_token_renames_re_extracts_sweeps_and_deletes_its_api_source() -> anyhow::Result<()>
{
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let result = async {
        let kb = f.seeded_kb("history-mirror").await?;
        let token = f.token(f.admin, "write", None).await;
        let (_, created) = f.create_source_of_kind(kb, &token, "api").await?;
        let path = format!(
            "/kbs/{kb}/sources/{}",
            created["source"]["id"].as_str().unwrap()
        );

        let renamed = f
            .send("PATCH", &path, &token, Some(json!({ "name": "commits" })))
            .await?;
        let re_extracted = f
            .send("POST", &format!("{path}/re-extract"), &token, None)
            .await?;
        let swept = f
            .send("POST", &format!("{path}/missing/cleanup"), &token, None)
            .await?;
        let deleted = f.send("DELETE", &path, &token, None).await?;
        anyhow::ensure!(
            renamed.1["source"]["name"] == "commits"
                && re_extracted.1["queued"] == 0
                && swept.1["deleted"] == 0
                && deleted.1["ok"] == true,
            "{renamed:?} {re_extracted:?} {swept:?} {deleted:?}"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    result
}

#[tokio::test]
async fn a_write_token_changing_a_url_source_is_refused_with_token_source_kind(
) -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let result = async {
        let kb = f.seeded_kb("pull").await?;
        let source = utopia_store::sources::create(
            &f.pool,
            kb,
            "url",
            "site",
            &json!({ "url": "https://example.com" }),
            None,
            None,
            None,
        )
        .await?;
        let token = f.token(f.admin, "write", None).await;

        let path = format!("/kbs/{kb}/sources/{}", source.id);
        let body = json!({ "config": { "url": "http://169.254.169.254" } });
        let (status, refused) = f.send("PATCH", &path, &token, Some(body)).await?;
        anyhow::ensure!(
            status == StatusCode::FORBIDDEN && refused["code"] == "token_source_kind",
            "{status} {refused}"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    result
}

#[tokio::test]
async fn a_read_token_lists_bases_and_is_refused_on_every_setup_route() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let result = async {
        let kb = f.seeded_kb("existing").await?;
        let source = Uuid::now_v7();
        sqlx::query("INSERT INTO sources(id,kb_id,kind,name) VALUES ($1,$2,'statements','ci')")
            .bind(source)
            .bind(kb)
            .execute(&f.pool)
            .await?;
        let token = f.token(f.admin, "read", None).await;

        let (status, listed) = f.list_kbs(&token).await?;
        anyhow::ensure!(status == StatusCode::OK, "{status} {listed}");
        anyhow::ensure!(listed[0]["name"] == "existing", "{listed}");

        for (route, (status, body)) in [
            ("create base", f.create_kb(&token, "nope").await?),
            ("create source", f.create_source(kb, &token).await?),
            ("read push key", f.push_key(kb, source, &token).await?),
            ("import ontology", f.import_nothing(kb, &token).await?),
            ("decide kind word", f.decide_kind_word(kb, &token).await?),
        ] {
            anyhow::ensure!(
                status == StatusCode::FORBIDDEN && body["code"] == "token_read_only",
                "{route}: {status} {body}"
            );
        }
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    result
}

#[tokio::test]
async fn a_write_token_reaches_no_further_than_its_owners_role_and_its_base_list(
) -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let result = async {
        // Workspace viewer: the token ticked write, but creating a library still requires workspace admin
        let viewers = f.token(f.viewer, "write", None).await;
        let (status, body) = f.create_kb(&viewers, "nope").await?;
        anyhow::ensure!(status == StatusCode::FORBIDDEN, "viewer: {status} {body}");

        let (a, b) = (f.seeded_kb("a").await?, f.seeded_kb("b").await?);
        let token = f.token(f.admin, "write", Some(&[a])).await;

        let (status, body) = f.create_source(a, &token).await?;
        anyhow::ensure!(status == StatusCode::OK, "base a: {status} {body}");

        let (status, body) = f.create_source(b, &token).await?;
        anyhow::ensure!(
            status == StatusCode::FORBIDDEN && body["code"] == "token_kb_out_of_scope",
            "base b: {status} {body}"
        );
        let (status, body) = f.create_kb(&token, "new").await?;
        anyhow::ensure!(
            status == StatusCode::FORBIDDEN && body["code"] == "token_kb_out_of_scope",
            "new base: {status} {body}"
        );

        let (status, listed) = f.list_kbs(&token).await?;
        let names: Vec<&str> = listed
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|kb| kb["name"].as_str())
            .collect();
        anyhow::ensure!(
            status == StatusCode::OK && names == ["a"],
            "{status} {listed}"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    result
}

/// Documents arrive first, ontology imported later (the create-library dialog says the ontology can be imported again later): with no classes, category words are judged "none",
/// and short-phrase signatures do not match attributes. For imported classes and attributes to pick them up, alignment has to be re-queued, as the editor's entry does: first queue
/// `embed_ontology`, then alignment; if not, the classes imported after the last document have no instances until the next document arrives
#[tokio::test]
async fn importing_an_ontology_queues_alignment_behind_the_index_refresh() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let result = async {
        let kb = f.seeded_kb("robots").await?;
        let token = f.token(f.admin, "write", None).await;
        let ttl = "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                   @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
                   @prefix ex: <http://example.org/> .\n\
                   ex:Robot a owl:Class ; rdfs:label \"Robot\" .\n";
        let (status, body) = f.import_turtle(kb, &token, ttl).await?;
        anyhow::ensure!(status == StatusCode::OK, "{status} {body}");

        let queued: Vec<String> = sqlx::query_scalar(
            "SELECT kind FROM jobs WHERE status = 'queued' AND payload->>'kb_id' = $1
              ORDER BY run_at, id",
        )
        .bind(kb.to_string())
        .fetch_all(&f.pool)
        .await?;
        anyhow::ensure!(
            queued == ["embed_ontology", "align_types"],
            "the alignment is queued behind the index refresh: {queued:?}"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    result
}

/// Import reads `utopia:temporal` (0031). The export side has always written this; the import side did not read it, so an ontology
/// that says "this relation is a moment" would be imported as `state` anyway, and #1242's "a non-state relation can bind without the model"
/// would never hold for imported ontologies. Newly created ones are built from the ontology; on re-import whatever the ontology states is applied,
/// and what it does not state is left alone: the same logic as domain/range. A real change bumps `updated_at`, as an edit on the page does
#[tokio::test]
async fn an_imported_property_keeps_the_temporal_it_declares() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let result = async {
        let kb = f.seeded_kb("temporal").await?;
        let token = f.token(f.admin, "write", None).await;
        const PREFIXES: &str = "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                                @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
                                @prefix utopia: <urn:utopia:ns:> .\n\
                                @prefix ex: <http://example.org/> .\n";
        let works_at = || {
            let pool = f.pool.clone();
            async move {
                sqlx::query_as::<_, (String, chrono::DateTime<chrono::Utc>)>(
                    "SELECT temporal, updated_at FROM relation_types
                      WHERE kb_id = $1 AND iri = 'http://example.org/worksAt'",
                )
                .bind(kb)
                .fetch_one(&pool)
                .await
            }
        };

        // The first version has no annotation: worksAt is built as state
        let first =
            format!("{PREFIXES}ex:worksAt a owl:ObjectProperty ; rdfs:label \"works at\" .\n");
        let (status, body) = f.import_turtle(kb, &token, &first).await?;
        anyhow::ensure!(status == StatusCode::OK, "{status} {body}");
        let (temporal, created) = works_at().await?;
        anyhow::ensure!(temporal == "state");

        // Second version: worksAt gains the annotation, and attended is newly created with the annotation
        let second = format!(
            "{PREFIXES}ex:worksAt a owl:ObjectProperty ; rdfs:label \"works at\" ; \
             utopia:temporal \"eternal\" .\n\
             ex:attended a owl:ObjectProperty ; rdfs:label \"attended\" ; \
             utopia:temporal \"event\" .\n"
        );
        let (status, body) = f.import_turtle(kb, &token, &second).await?;
        anyhow::ensure!(status == StatusCode::OK, "{status} {body}");
        let (temporal, changed) = works_at().await?;
        anyhow::ensure!(
            temporal == "eternal",
            "a re-import writes the temporal the file now declares"
        );
        anyhow::ensure!(
            changed > created,
            "a temporal changed by import moves updated_at"
        );
        let attended: String = sqlx::query_scalar(
            "SELECT temporal FROM relation_types
              WHERE kb_id = $1 AND iri = 'http://example.org/attended'",
        )
        .bind(kb)
        .fetch_one(&f.pool)
        .await?;
        anyhow::ensure!(
            attended == "event",
            "a fresh import creates the relation with the temporal it declares"
        );

        // Third version removes the annotation: unstated means unchanged, and what a person edited on the page is not wiped by re-import
        let (status, body) = f.import_turtle(kb, &token, &first).await?;
        anyhow::ensure!(status == StatusCode::OK, "{status} {body}");
        anyhow::ensure!(
            works_at().await? == ("eternal".to_string(), changed),
            "an unannotated re-import leaves temporal and updated_at alone"
        );

        // Fourth version states state again: the export side writes state, so re-importing an export brings the relation back to state
        let back = format!(
            "{PREFIXES}ex:worksAt a owl:ObjectProperty ; rdfs:label \"works at\" ; \
             utopia:temporal \"state\" .\n"
        );
        let (status, body) = f.import_turtle(kb, &token, &back).await?;
        anyhow::ensure!(status == StatusCode::OK, "{status} {body}");
        let (temporal, returned) = works_at().await?;
        anyhow::ensure!(
            temporal == "state" && returned > changed,
            "a re-import declaring state returns the relation to state"
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    result
}
