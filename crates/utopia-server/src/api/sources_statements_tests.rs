//! Source that pushes statements (0054): the request body is the open extraction contract; the door refuses keys outside the contract, and a payload that passes
//! is kept as a single block; extraction parses it per the contract and **does not ask a model** -- the fixture deliberately has no chat model configured, to prove this path does not need one.
//! The database-backed part is skipped without `UTOPIA_DATABASE_URL` (same as documents_routes_tests).

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use utopia_core::models::Proposer;
use utopia_store::documents;
use uuid::Uuid;

/// The door's validation does not touch the database, so it runs in every environment
#[test]
fn the_door_refuses_what_the_contract_has_no_slot_for() {
    let ok = json!({
        "external_id": "obs-1",
        "e": [["cup-7", "cup", true], ["kitchen table", "table", true]],
        "s": [[null, "cup-7", "is on", "kitchen table", null, {}, "08:14:03", null]],
        "n": []
    });
    let (_, content) = super::validate_statements_payload(ok.to_string().as_bytes())
        .expect("a well-formed contract passes");
    let content = content.expect("a non-tombstone yields the document text");
    // Stores the copy we re-serialized: with this observation's identity, and it reads back through the same parser extraction uses
    // (it reads only the three arrays; it ignores the identity and date keys)
    let stored: Value = serde_json::from_str(&content).unwrap();
    assert_eq!(stored["external_id"], "obs-1");
    assert!(
        stored.get("doc_time").is_none(),
        "no date was given, none is stored"
    );
    assert_eq!(
        stored.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["e", "external_id", "n", "s"],
        "identity plus the three arrays, nothing else"
    );
    let parsed = utopia_extract::open::parse_open_response(&content).unwrap();
    assert_eq!(parsed.statements.len(), 1);
    assert_eq!(parsed.statements[0].phrase, "is on");

    let refuse = |body: Value, needle: &str| {
        let err = super::validate_statements_payload(body.to_string().as_bytes())
            .err()
            .unwrap_or_else(|| panic!("{body} must be refused"));
        assert!(err.contains(needle), "{err:?} should mention {needle:?}");
    };
    // A cell with no property in the contract: a `predicate` key is blocked at the door rather than silently ignored
    let mut typed = ok.clone();
    typed["predicate"] = json!("located_in");
    refuse(typed, "unknown key");
    // The citation cell must be empty: the entry is itself the evidence
    let mut quoted = ok.clone();
    quoted["s"][0][0] = json!("cup-7 is on the kitchen table");
    refuse(quoted, "quote");
    // One cell short of eight is not truncation, it is a shape error
    let mut short = ok.clone();
    short["s"][0] = json!([null, "cup-7", "is on", "kitchen table"]);
    refuse(short, "eight");
    // No identity, no update semantics
    let mut anon = ok.clone();
    anon["external_id"] = json!("  ");
    refuse(anon, "external_id");
    // Empty statement array: nothing can be pushed into the graph; say so directly
    let mut empty = ok.clone();
    empty["s"] = json!([]);
    refuse(empty, "at least one");
    // The subject is not in `e`: extraction would silently drop it as UNKNOWN_REF, so the door must refuse
    let mut stray = ok.clone();
    stray["s"][0][1] = json!("cup-8");
    refuse(stray, "not a thing listed in e");
    // Names that differ only in whitespace and case count as the same name (same folding rule as extraction's `name_key`)
    let mut folded = ok.clone();
    folded["s"][0][1] = json!("  Cup-7 ");
    super::validate_statements_payload(folded.to_string().as_bytes())
        .expect("whitespace and case do not make a different thing");
    // The same for the thing an alias belongs to; the alias's citation cell must likewise be empty
    let mut alias = ok.clone();
    alias["n"] = json!([["mug-7", "the cup", null]]);
    refuse(alias, "n[0][0]");
    let mut alias_quoted = ok.clone();
    alias_quoted["n"] = json!([["cup-7", "the cup", "the cup sat there"]]);
    refuse(alias_quoted, "n[0][2]");
    // Caps on count and bytes: a first-cut limit, exceeding it says so rather than truncating
    let mut many = ok.clone();
    many["s"] = json!(vec![ok["s"][0].clone(); 201]);
    refuse(many, "limit is 200");
    let mut fat = ok.clone();
    fat["s"][0][5] = json!({ "note": "x".repeat(64 * 1024) });
    refuse(fat, "limit is 65536");
}

pub(super) struct Fixture {
    pub(super) pool: sqlx::PgPool,
    pub(super) state: crate::state::AppState,
    app: axum::Router,
    org: Uuid,
    pub(super) kb: Uuid,
    user: Uuid,
    pub(super) source: Uuid,
    api_source: Uuid,
    pub(super) token: String,
    api_token: String,
    /// The editor's session token: viewing and rotating the key use session auth, not the push key
    session: String,
    _dir: tempfile::TempDir,
}

impl Fixture {
    pub(super) async fn new() -> anyhow::Result<Option<Self>> {
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(None);
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        utopia_store::db::migrate(&pool).await?;
        let (org, ws, kb, user, source, api_source) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        // Only locally generated UUIDs are interpolated into fixture SQL.
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','statements-push-test');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','statements-push-test');
             INSERT INTO users(id,org_id,email,display_name,password_hash)
                 VALUES ('{user}','{org}','{user}@statements.test','statements-test','unused');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','observations');
             INSERT INTO kb_members(kb_id,user_id,role) VALUES ('{kb}','{user}','editor');
             INSERT INTO sources(id,kb_id,kind,name) VALUES
                 ('{source}','{kb}','statements','robot-1'),
                 ('{api_source}','{kb}','api','api-source');"
        ))
        .execute(&pool)
        .await?;
        // Deliberately does NOT configure a chat model: this path does not need one
        let token = super::new_ingest_token();
        utopia_store::sources::set_ingest_token(&pool, source, &token).await?;
        let api_token = super::new_ingest_token();
        utopia_store::sources::set_ingest_token(&pool, api_source, &api_token).await?;
        let dir = tempfile::tempdir()?;
        let cfg = utopia_core::config::AppConfig {
            data_dir: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let search = Arc::new(utopia_search::SearchIndex::open(
            &dir.path().join("search"),
        )?);
        let state = crate::state::AppState::new(pool.clone(), &cfg, search, "test-only".into());
        let session = crate::auth::issue_token(&state, user)?;
        let app = super::super::router(state.clone(), &cfg);
        Ok(Some(Self {
            pool,
            state,
            app,
            org,
            kb,
            user,
            source,
            api_source,
            token,
            api_token,
            session,
            _dir: dir,
        }))
    }

    pub(super) async fn push(
        &self,
        source: Uuid,
        token: &str,
        body: &Value,
    ) -> anyhow::Result<(StatusCode, Value)> {
        self.push_raw(source, Some(token), body.to_string().into_bytes())
            .await
    }

    /// Sends no Authorization header (`None`) or pushes raw bytes: the door's 401 and byte cap must be seen from the HTTP side
    async fn push_raw(
        &self,
        source: Uuid,
        token: Option<&str>,
        body: Vec<u8>,
    ) -> anyhow::Result<(StatusCode, Value)> {
        self.send(&format!("/api/v1/sources/{source}/statements"), token, body)
            .await
    }

    /// Pushes to an api source (`/ingest`), using its own key
    async fn push_api(&self, body: &Value) -> anyhow::Result<(StatusCode, Value)> {
        let path = format!("/api/v1/sources/{}/ingest", self.api_source);
        self.send(&path, Some(&self.api_token), body.to_string().into_bytes())
            .await
    }

    async fn send(
        &self,
        path: &str,
        token: Option<&str>,
        body: Vec<u8>,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let mut request = Request::post(path).header("Content-Type", "application/json");
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body))?)
            .await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok((status, value))
    }

    /// Calls a source endpoint as the editor's session (view / rotate key)
    async fn as_editor(&self, method: &str, path: &str) -> anyhow::Result<(StatusCode, Value)> {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/api/v1/kbs/{}/sources/{path}", self.kb))
                    .header("Authorization", format!("Bearer {}", self.session))
                    .body(Body::empty())?,
            )
            .await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok((status, value))
    }

    /// One read token from the editor; when `kb_ids` is `None` it is not limited by database
    async fn read_token(&self, kb_ids: Option<&[Uuid]>) -> anyhow::Result<String> {
        let (_, token) =
            utopia_store::tokens::issue(&self.pool, self.user, "ci", "read", kb_ids, None).await?;
        Ok(token)
    }

    /// Reads back a document from a statements source by external_id
    async fn read_back(
        &self,
        token: &str,
        external_id: &str,
    ) -> anyhow::Result<(StatusCode, Value)> {
        read_back(self.app.clone(), self.kb, self.source, token, external_id).await
    }

    pub(super) async fn cleanup(self) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(self.org)
            .execute(&self.pool)
            .await?;
        self.pool.close().await;
        Ok(())
    }
}

/// Does not borrow `Fixture`, so the test can spawn another task to read it
async fn read_back(
    app: axum::Router,
    kb: Uuid,
    source: Uuid,
    token: &str,
    external_id: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let path = format!(
        "/api/v1/kbs/{kb}/sources/{source}/documents/by-external-id?external_id={external_id}"
    );
    let response = app
        .oneshot(
            Request::get(path)
                .header("Authorization", format!("Bearer {token}"))
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

fn observation(when: &str, place: &str) -> Value {
    json!({
        "external_id": "obs-000412",
        "doc_time": "2026-09-23T08:14:03Z",
        "e": [["cup-7", "cup", true], [place, "table", true]],
        "s": [[null, "cup-7", "is on", place, null, {}, when, null]],
        "n": []
    })
}

/// Pushes one statement through the full processing and extraction; it is an open statement: it has a phrase, subject and object entities, and evidence rows
/// (block = payload, offsets are empty), while the workspace has no chat model at all
#[tokio::test]
async fn a_pushed_statement_reaches_the_open_graph_without_a_model() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let (status, body) = f
        .push(
            f.source,
            &f.token,
            &observation("08:14:03", "kitchen table"),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action"], "created");

    let doc = documents::find_by_external_key(&f.pool, f.source, "statements:obs-000412")
        .await?
        .expect("the push created a document under its identity");
    assert_eq!(doc.mime, "application/json");

    crate::pipeline::process_document(&f.state, doc.id).await?;
    let chunks: Vec<(String,)> =
        sqlx::query_as("SELECT text FROM chunks WHERE document_id = $1 AND superseded_at IS NULL")
            .bind(doc.id)
            .fetch_all(&f.pool)
            .await?;
    assert_eq!(
        chunks.len(),
        1,
        "the payload is one chunk, not a budgeted split"
    );
    utopia_extract::open::parse_open_response(&chunks[0].0)
        .expect("the chunk is the contract verbatim");

    crate::extraction::extract_document(
        &f.state,
        doc.id,
        Proposer {
            user_id: None,
            token_id: None,
        },
    )
    .await?;
    let (status,): (String,) = sqlx::query_as("SELECT graph_status FROM documents WHERE id = $1")
        .bind(doc.id)
        .fetch_one(&f.pool)
        .await?;
    assert_ne!(status, "failed", "extraction must not need a chat model");

    let facts: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, phrase FROM facts
          WHERE kb_id = $1 AND layer = 'open' AND invalidated_at IS NULL",
    )
    .bind(f.kb)
    .fetch_all(&f.pool)
    .await?;
    assert_eq!(facts.len(), 1, "{facts:?}");
    assert_eq!(facts[0].1, "is on");
    let (chunk_matches, quote_null, offsets_null): (bool, bool, bool) = sqlx::query_as(
        "SELECT chunk_id = (SELECT id FROM chunks WHERE document_id = $2 AND superseded_at IS NULL),
                quote IS NULL, quote_start IS NULL AND quote_end IS NULL
           FROM fact_evidence WHERE fact_id = $1",
    )
    .bind(facts[0].0)
    .bind(doc.id)
    .fetch_one(&f.pool)
    .await?;
    assert!(chunk_matches, "the evidence is the payload's own chunk");
    assert!(
        quote_null && offsets_null,
        "the item is its own evidence: no quote, no offsets"
    );
    let (entities,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM entities
          WHERE kb_id = $1 AND canonical_name IN ('cup-7', 'kitchen table')",
    )
    .bind(f.kb)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        entities, 2,
        "both things are entities with the pushed names"
    );
    // Time words become mentions (0054 decision 3): when there is no citation, look in the payload itself rather than exiting via the citation path
    let mentions: Vec<(String, String)> =
        sqlx::query_as("SELECT text, role FROM time_mentions WHERE fact_id = $1")
            .bind(facts[0].0)
            .fetch_all(&f.pool)
            .await?;
    assert_eq!(
        mentions,
        vec![("08:14:03".to_string(), "when".to_string())],
        "the pushed `when` is a time mention on the fact"
    );
    f.cleanup().await
}

/// Pushing new content again under the same identity is an update: replace in place and record a version, the same semantics as an `api` push
#[tokio::test]
async fn a_second_push_under_the_same_identity_updates_in_place() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let (status, body) = f
        .push(
            f.source,
            &f.token,
            &observation("08:14:03", "kitchen table"),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = f
        .push(
            f.source,
            &f.token,
            &observation("08:14:03", "kitchen table"),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action"], "unchanged", "same content is a no-op");
    let (status, body) = f
        .push(f.source, &f.token, &observation("08:20:00", "counter"))
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action"], "updated");
    let doc = documents::find_by_external_key(&f.pool, f.source, "statements:obs-000412")
        .await?
        .expect("still one document under the identity");
    let (versions,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM document_versions WHERE document_id = $1")
            .bind(doc.id)
            .fetch_one(&f.pool)
            .await?;
    assert_eq!(versions, 2, "the update recorded a version");
    f.cleanup().await
}

/// The 0070 example: pushing again changes `lines` from 40-52 to 58-70, and reading back shows only the new range
#[tokio::test]
async fn a_re_push_that_moves_lines_to_58_70_reads_back_only_58_70() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    for lines in ["40-52", "58-70"] {
        let mut body = observation("08:14:03", "kitchen table");
        body["s"][0][5] = json!({ "lines": lines });
        let (status, body) = f.push(f.source, &f.token, &body).await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        let doc = documents::find_by_external_key(&f.pool, f.source, "statements:obs-000412")
            .await?
            .expect("the push has a document");
        crate::pipeline::process_document(&f.state, doc.id).await?;
        let proposer = Proposer {
            user_id: None,
            token_id: None,
        };
        crate::extraction::extract_document(&f.state, doc.id, proposer).await?;
    }
    let (cup,): (Uuid,) =
        sqlx::query_as("SELECT id FROM entities WHERE kb_id = $1 AND canonical_name = 'cup-7'")
            .bind(f.kb)
            .fetch_one(&f.pool)
            .await?;
    let (_, facts) = utopia_store::graph::entity_detail(&f.pool, f.kb, cup, None, None).await?;
    let said: Vec<_> = facts
        .iter()
        .flat_map(|x| &x.statement_qualifiers)
        .map(|q| (q.role.as_str(), q.value.clone()))
        .collect();
    assert_eq!(said, vec![("lines", Some(json!("58-70")))]);
    f.cleanup().await
}

/// A door refusal reaches HTTP as 422 (`AppError::Validation`, the same code as when an `api` push is refused), and this push
/// stays in the run history; the `api` source does not recognize this route (404), and a wrong key is 401
#[tokio::test]
async fn the_route_answers_422_404_and_401_at_the_door() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let mut typed = observation("08:14:03", "kitchen table");
    typed["class"] = json!("Cup");
    let (status, body) = f.push(f.source, &f.token, &typed).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _) = f
        .push(
            f.api_source,
            &f.api_token,
            &observation("08:14:03", "kitchen table"),
        )
        .await?;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an api source has no statements route"
    );
    let (status, _) = f
        .push(
            f.source,
            "utp_not-the-key",
            &observation("08:14:03", "kitchen table"),
        )
        .await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = f
        .push_raw(
            f.source,
            None,
            observation("08:14:03", "kitchen table")
                .to_string()
                .into_bytes(),
        )
        .await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no header is no key");
    // The 64 KiB cap is still 422 with an explanation from the HTTP side, not a 413 from the routing layer
    let mut fat = observation("08:14:03", "kitchen table");
    fat["s"][0][5] = json!({ "note": "x".repeat(64 * 1024) });
    let (status, body) = f.push(f.source, &f.token, &fat).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body.to_string().contains("limit is 65536"),
        "the refusal names the limit: {body}"
    );
    f.cleanup().await
}

/// Tombstone and resurrection, same semantics as the `api` push: `deleted: true` marks that identity "Not in source" without deleting;
/// pushing content for the same identity again clears the mark. Tombstoning an identity never seen is a no-op, and still answers marked_missing
#[tokio::test]
async fn a_tombstone_marks_the_item_missing_and_a_new_push_revives_it() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let (status, body) = f
        .push(
            f.source,
            &f.token,
            &observation("08:14:03", "kitchen table"),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = f
        .push(
            f.source,
            &f.token,
            &json!({ "external_id": "obs-000412", "deleted": true }),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action"], "marked_missing");
    let doc = documents::find_by_external_key(&f.pool, f.source, "statements:obs-000412")
        .await?
        .expect("a tombstone marks, it does not delete");
    let missing: (bool, bool) = sqlx::query_as(
        "SELECT missing_since IS NOT NULL, deleted_at IS NULL FROM documents WHERE id = $1",
    )
    .bind(doc.id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(missing, (true, true), "marked missing, still present");
    // Resurrection: same identity, same content -- the document did not change (unchanged), but the mark was cleared
    let (status, body) = f
        .push(
            f.source,
            &f.token,
            &observation("08:14:03", "kitchen table"),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action"], "unchanged");
    let (revived,): (bool,) =
        sqlx::query_as("SELECT missing_since IS NULL FROM documents WHERE id = $1")
            .bind(doc.id)
            .fetch_one(&f.pool)
            .await?;
    assert!(revived, "a new push under the identity clears the marker");
    // An identity never seen: it reaches no document, and that is not an error either
    let (status, body) = f
        .push(
            f.source,
            &f.token,
            &json!({ "external_id": "never-pushed", "deleted": true }),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action"], "marked_missing");
    assert!(
        documents::find_by_external_key(&f.pool, f.source, "statements:never-pushed")
            .await?
            .is_none(),
        "a tombstone never creates a document"
    );
    f.cleanup().await
}

/// An api source's tombstone identifies by external_id, no longer needing a file name; a normal push without a file name is still refused as before
#[tokio::test]
async fn an_api_tombstone_needs_only_its_external_id() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let (status, body) = f
        .push_api(&json!({ "filename": "note-7.md", "content": "Kitchen inventory.", "external_id": "note-7" }))
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = f
        .push_api(&json!({ "external_id": "note-7", "deleted": true }))
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action"], "marked_missing");
    // The reply is marked_missing for an id never pushed too, so check that the document itself is really marked
    let doc = documents::find_by_external_key(&f.pool, f.api_source, "api:note-7")
        .await?
        .expect("the tombstone marks the document, it does not delete it");
    let (missing,): (bool,) =
        sqlx::query_as("SELECT missing_since IS NOT NULL FROM documents WHERE id = $1")
            .bind(doc.id)
            .fetch_one(&f.pool)
            .await?;
    assert!(missing, "api:note-7 is marked missing");
    let (status, body) = f
        .push_api(&json!({ "content": "Kitchen inventory.", "external_id": "note-8" }))
        .await?;
    assert_ne!(
        status,
        StatusCode::OK,
        "a push with content still needs its filename: {body}"
    );
    f.cleanup().await
}

/// Viewing and rotating the key work for both `statements` and `api` sources, the same as at creation: the key given once at creation
/// is still retrievable afterwards, and after a rotation the old key stops working immediately and the new one can push. Caught when running the service end to end: these two endpoints
/// originally accepted only `api`
#[tokio::test]
async fn the_push_token_can_be_viewed_and_rotated_like_an_api_source() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let (status, body) = f.as_editor("GET", &format!("{}/token", f.source)).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["ingest_token"], f.token,
        "the token given at creation is viewable"
    );
    let (status, body) = f
        .as_editor("POST", &format!("{}/rotate-token", f.source))
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rotated = body["ingest_token"]
        .as_str()
        .expect("a new token")
        .to_string();
    assert_ne!(rotated, f.token);
    let (status, _) = f
        .push(
            f.source,
            &f.token,
            &observation("08:14:03", "kitchen table"),
        )
        .await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the old token is dead");
    let (status, body) = f
        .push(
            f.source,
            &rotated,
            &observation("08:14:03", "kitchen table"),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    f.cleanup().await
}

/// The same payload under a new identity is another observation (#900): two sightings of a cup on the table are two documents, each carrying its own
/// date. The identity is in the body, so the two bodies differ; the database's "one document per content" uniqueness and the file-type
/// source's "same content at a new path = rename" detection do not touch it
#[tokio::test]
async fn the_same_payload_under_a_new_identity_is_a_second_observation() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let mut first = observation("08:14:03", "kitchen table");
    first["external_id"] = json!("obs-1");
    first["doc_time"] = json!("2026-09-23T08:14:03Z");
    let (status, body) = f.push(f.source, &f.token, &first).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action"], "created");
    let mut second = first.clone();
    second["external_id"] = json!("obs-2");
    second["doc_time"] = json!("2026-09-23T08:20:00Z");
    let (status, body) = f.push(f.source, &f.token, &second).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action"], "created", "not a move");
    for (key, when) in [
        ("statements:obs-1", "2026-09-23T08:14:03Z"),
        ("statements:obs-2", "2026-09-23T08:20:00Z"),
    ] {
        let doc = documents::find_by_external_key(&f.pool, f.source, key)
            .await?
            .unwrap_or_else(|| panic!("{key} is its own document"));
        assert_eq!(
            doc.doc_time
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            Some(when.to_string()),
            "each observation keeps its own date"
        );
    }
    f.cleanup().await
}

/// If an api push carries an `external_id`, the identity is that ID: another ID pushing the same bytes does not rename the first document
/// over (previously "same content at a new path = move" would make the first ID vanish), but honestly answers which document it duplicates,
/// and the document already under the two IDs stays as it is
#[tokio::test]
async fn a_new_id_with_the_same_content_does_not_take_the_first_ids_document() -> anyhow::Result<()>
{
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let record = |id: &str| json!({ "filename": "record.txt", "content": "colour = blue", "external_id": id });
    let (status, body) = f.push_api(&record("a")).await?;
    assert_eq!(
        (status, body["action"].as_str()),
        (StatusCode::OK, Some("created")),
        "{body}"
    );
    let first = documents::find_by_external_key(&f.pool, f.api_source, "api:a")
        .await?
        .expect("a is stored");

    let (status, body) = f.push_api(&record("b")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "action": "duplicate", "duplicate_of": first.id })
    );
    let kept = documents::find_by_external_key(&f.pool, f.api_source, "api:a").await?;
    assert_eq!(kept.map(|d| d.id), Some(first.id), "a keeps its document");
    f.cleanup().await
}

/// The same bytes already exist elsewhere in the database (here an ordinary upload): previously it answered `unchanged` and created nothing,
/// so the caller believed its ID had been persisted
#[tokio::test]
async fn content_already_in_the_base_is_reported_as_a_duplicate() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    crate::ingest_sources::ingest_upload(
        &f.state,
        f.kb,
        "upload.txt",
        "text/plain",
        b"colour = green",
        None,
    )
    .await?;
    let upload: Uuid =
        sqlx::query_scalar("SELECT id FROM documents WHERE kb_id = $1 AND source_id IS NULL")
            .bind(f.kb)
            .fetch_one(&f.pool)
            .await?;

    let push = json!({ "filename": "record.txt", "content": "colour = green", "external_id": "c" });
    let (status, body) = f.push_api(&push).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "action": "duplicate", "duplicate_of": upload })
    );
    f.cleanup().await
}

/// An existing ID changed to the content of another document: previously it hit `documents_kb_sha_idx`, and the push failed with a SQL error,
/// marking the source as failed to sync; now it answers which document it duplicates, and this ID's document keeps its original content
#[tokio::test]
async fn an_update_to_another_documents_content_is_a_duplicate_not_a_failure() -> anyhow::Result<()>
{
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let push = |id: &str, content: &str| json!({ "filename": "record.txt", "content": content, "external_id": id });
    f.push_api(&push("a", "colour = blue")).await?;
    f.push_api(&push("b", "colour = red")).await?;
    let a = documents::find_by_external_key(&f.pool, f.api_source, "api:a")
        .await?
        .expect("a is stored");

    let (status, body) = f.push_api(&push("b", "colour = blue")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "action": "duplicate", "duplicate_of": a.id }));
    let b = documents::find_by_external_key(&f.pool, f.api_source, "api:b")
        .await?
        .expect("b is still stored");
    assert_ne!(b.sha256, a.sha256, "b keeps its own content");
    f.cleanup().await
}

/// A caller that uses the ID as a path renames like this: tombstone the old ID first, then push the new ID with the same bytes. The new ID takes over the document
/// that the tombstone marked (the same document, history intact), and the old ID no longer leaves an orphan copy marked "not in source"
#[tokio::test]
async fn a_new_id_with_a_tombstoned_ids_content_takes_over_its_document() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let record = |id: &str| json!({ "filename": "record.txt", "content": "colour = blue", "external_id": id });
    f.push_api(&record("a")).await?;
    let first = documents::find_by_external_key(&f.pool, f.api_source, "api:a")
        .await?
        .expect("a is stored");
    f.push_api(&json!({ "filename": "record.txt", "external_id": "a", "deleted": true }))
        .await?;

    let (status, body) = f.push_api(&record("b")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "action": "moved" }));
    let b = documents::find_by_external_key(&f.pool, f.api_source, "api:b")
        .await?
        .expect("b is stored");
    assert_eq!((b.id, b.missing_since), (first.id, None));
    let a = documents::find_by_external_key(&f.pool, f.api_source, "api:a").await?;
    assert!(a.is_none(), "a is gone");
    f.cleanup().await
}

/// An ID in the trash pushed with another document's content: answers duplicate first, and the one in the trash is not restored
#[tokio::test]
async fn a_trashed_id_pushed_with_another_documents_content_stays_in_the_trash(
) -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let push = |id: &str, content: &str| json!({ "filename": "record.txt", "content": content, "external_id": id });
    f.push_api(&push("a", "colour = blue")).await?;
    f.push_api(&push("b", "colour = red")).await?;
    let a = documents::find_by_external_key(&f.pool, f.api_source, "api:a")
        .await?
        .expect("a is stored");
    let b = documents::find_by_external_key(&f.pool, f.api_source, "api:b")
        .await?
        .expect("b is stored");
    documents::delete(&f.pool, f.kb, a.id, None).await?;

    let (status, body) = f.push_api(&push("a", "colour = red")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "action": "duplicate", "duplicate_of": b.id }));
    let a = documents::find_by_external_key(&f.pool, f.api_source, "api:a")
        .await?
        .expect("a is still stored");
    assert!(a.deleted_at.is_some(), "a stays in the trash");
    f.cleanup().await
}

/// An identity pushed up can be read back by external_id (#1276): a read token gets the current body and both versions;
/// a token for another database gets 404, the same as an unreadable document
#[tokio::test]
async fn a_read_token_reads_an_updated_identity_back_and_one_for_another_base_gets_404(
) -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    for place in ["kitchen table", "counter"] {
        let (status, body) = f
            .push(f.source, &f.token, &observation("08:14:03", place))
            .await?;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let token = f.read_token(Some(&[f.kb])).await?;
    let (status, body) = f.read_back(&token, "obs-000412").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let content: Value = serde_json::from_str(body["content"].as_str().unwrap_or_default())?;
    assert_eq!(content["e"][1][0], "counter", "the current content");
    let versions: Vec<&Value> = body["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| &v["version"])
        .collect();
    assert_eq!(
        (&body["external_id"], &body["version"], versions),
        (&json!("obs-000412"), &json!(2), vec![&json!(1), &json!(2)])
    );
    let elsewhere = f.read_token(Some(&[Uuid::now_v7()])).await?;
    let (status, body) = f.read_back(&elsewhere, "obs-000412").await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    f.cleanup().await
}

/// An identity marked by tombstone and a document in the trash are both unreadable, and answer 404 just like an identity never pushed
#[tokio::test]
async fn a_tombstoned_identity_and_a_trashed_document_read_back_as_404() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let mut trashed = observation("08:14:03", "counter");
    trashed["external_id"] = json!("obs-000413");
    for body in [
        observation("08:14:03", "kitchen table"),
        json!({ "external_id": "obs-000412", "deleted": true }),
        trashed,
    ] {
        let (status, body) = f.push(f.source, &f.token, &body).await?;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let doc = documents::find_by_external_key(&f.pool, f.source, "statements:obs-000413")
        .await?
        .expect("obs-000413 is stored");
    documents::delete(&f.pool, f.kb, doc.id, None).await?;

    let token = f.read_token(None).await?;
    for external_id in ["obs-000412", "obs-000413"] {
        let (status, body) = f.read_back(&token, external_id).await?;
        assert_eq!(status, StatusCode::NOT_FOUND, "{external_id}: {body}");
    }
    f.cleanup().await
}

/// After finding the document but before reading the original, it gets tombstoned: the check lands on the locked row, so it answers 404,
/// rather than trusting the row read before the lock and handing over the body anyway
#[tokio::test]
async fn an_identity_tombstoned_while_the_read_waits_for_its_row_reads_back_as_404(
) -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let (status, body) = f
        .push(f.source, &f.token, &observation("08:14:03", "counter"))
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc = documents::find_by_external_key(&f.pool, f.source, "statements:obs-000412")
        .await?
        .expect("obs-000412 is stored");
    let token = f.read_token(None).await?;

    // An uncommitted tombstone holds the row lock: the read route's lookup still sees the live row, and then blocks on the lock
    let mut tombstone = f.pool.begin().await?;
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tombstone)
        .await?;
    sqlx::query("UPDATE documents SET missing_since = now() WHERE id = $1")
        .bind(doc.id)
        .execute(&mut *tombstone)
        .await?;
    let (app, kb, source) = (f.app.clone(), f.kb, f.source);
    let read = tokio::spawn(async move { read_back(app, kb, source, &token, "obs-000412").await });
    let mut waiting = false;
    for _ in 0..500 {
        waiting = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))",
        )
        .bind(holder)
        .fetch_one(&f.pool)
        .await?;
        if waiting {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(waiting, "the read waits on the tombstone's row lock");
    tombstone.commit().await?;

    let (status, body) = read.await??;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    f.cleanup().await
}
