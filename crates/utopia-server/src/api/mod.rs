mod admin_routes;
mod agent;
mod alerts_routes;
mod auth_routes;
mod build_version;
mod chat;
mod chat_context;
mod datasource_routes;
pub(crate) mod documents_routes;
mod events_routes;
mod export_routes;
mod google_drive_routes;
mod graph_routes;
mod jobs_routes;
mod kbs;
mod mapping_routes;
mod mcp;
mod members_routes;
pub mod oidc_routes;
mod ontology_regression_routes;
pub(crate) mod ontology_routes;
mod question_routes;
mod review_routes;
mod rig_model;
mod rule_expression_input;
pub(crate) mod rule_routes;
mod search_routes;
mod settings_routes;
mod sources_routes;
#[cfg(test)]
mod static_files_tests;
mod token_routes;
mod tools;
mod tools_graph;
mod workspaces;

use axum::extract::DefaultBodyLimit;
use axum::http::{header, HeaderValue, Method};
use axum::routing::{any, get, patch, post, put};
use axum::{Json, Router};
use serde_json::json;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use utopia_core::config::AppConfig;
use utopia_core::error::AppError;

use crate::error::ApiErr;

use crate::state::AppState;

const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;

/// Attaches the build artifacts. **A separate block, with no state**, so it can be unit-tested without the database:
/// The contract of this block (which route 404s, which route returns the home page, and each one's cache directives) has already broken twice:
/// once a blank white page on upgrade (#616), once a 404 that was cached for a year.
fn with_static_files(mut app: Router, web_dist: &str) -> Router {
    /* SPA hosting. **Two routes, because they fail in different ways**:

    `/assets` `/assets` holds the build artifacts, with content hashes in their file names. The ServeDir here has **no fallback**,
    so a missing file is a 404. It used to share one service with the pages, which had a history fallback, so after an upgrade
    requests for old hashes got a `200 text/html` home page -- the browser treats it as a module script
    and, going by the MIME type alone, refuses to run it, so the UI goes blank (#616).

    Cache directives follow the original meaning of each file name: a hashed artifact whose content changes gets a new name, so it can be
    `immutable` `immutable` for a year; `index.html` must always be revalidated with the origin, otherwise it would point at a set of
    hashes that no longer exist. Without this rule, an upgrade has to wait for the browser's heuristic cache to expire on its own. */
    let index = std::path::Path::new(web_dist).join("index.html");
    if index.exists() {
        /* Each route applies its own headers. **The layer must be attached to the Router of this block, not to the whole
        app** -- attaching it to the app would also put `immutable` on API responses. */
        let assets = Router::new()
            .fallback_service(ServeDir::new(std::path::Path::new(web_dist).join("assets")))
            /* **Only the response that actually delivers something gets `immutable`.** This header used to be applied unconditionally, so
            the 404 for `/assets/<missing file>` also carried "store for a year" -- the browser (and any intermediate cache) would remember "this file does not exist" for a year.
            Asset file names carry hashes, so after a deploy the requests are for the new names; a 404 cached a moment earlier would keep the new version's js
            from loading on that machine for a whole year. This is the
            same chain #616 guards against, just from the other end.
            A response that delivers nothing gets `no-cache` instead, so the next request goes to the origin as usual. */
            .layer(SetResponseHeaderLayer::overriding(
                header::CACHE_CONTROL,
                |res: &axum::response::Response| {
                    Some(if res.status().is_success() {
                        HeaderValue::from_static("public, max-age=31536000, immutable")
                    } else {
                        HeaderValue::from_static("no-cache")
                    })
                },
            ));
        let spa = Router::new()
            .fallback_service(ServeDir::new(web_dist).fallback(ServeFile::new(index)))
            .layer(SetResponseHeaderLayer::overriding(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache"),
            ));
        app = app.nest("/assets", assets).fallback_service(spa);
        tracing::info!("已托管前端产物: {}", web_dist);
    }
    app
}

pub fn router(state: AppState, cfg: &AppConfig) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/auth/register", post(auth_routes::register))
        .route("/auth/login", post(auth_routes::login))
        .route("/auth/logout", post(auth_routes::logout))
        // Alert center: cross-database, not mounted under /kbs/{id}
        .route("/alerts", get(alerts_routes::list))
        .route("/alerts/unread", get(alerts_routes::unread))
        .route("/alerts/read-all", post(alerts_routes::mark_all_read))
        .route("/alerts/read-group", post(alerts_routes::mark_group_read))
        .route("/alerts/events", get(alerts_routes::stream))
        .route(
            "/auth/me",
            get(auth_routes::me).patch(auth_routes::update_me),
        )
        .route("/auth/password", post(auth_routes::change_password))
        // Single sign-on: narrow-scope OIDC authorization-code flow (0056). When not configured, status reports false,
        // and the UI uses that to decide whether to show the login page button
        .route("/auth/oidc/status", get(oidc_routes::status))
        .route("/auth/oidc/start", get(oidc_routes::start))
        .route("/auth/oidc/callback", get(oidc_routes::callback))
        // Another trusted app exchanges the ID token its user just obtained for a session (0066)
        .route("/auth/oidc/exchange", post(oidc_routes::exchange))
        // Google Drive sources (0074): only present if the deployment configured an OAuth client; the callback lands in the connect popup
        .route("/google-drive/status", get(google_drive_routes::status))
        .route("/google-drive/callback", get(google_drive_routes::callback))
        // My own bindings: view and unlink. Binding goes through `/auth/oidc/start?link=1`, and the person completes it themselves
        .route(
            "/auth/oidc/me",
            get(oidc_routes::me).delete(oidc_routes::unlink_me),
        )
        .route(
            "/workspaces",
            get(workspaces::list).post(workspaces::create),
        )
        .route(
            "/workspaces/{id}",
            get(workspaces::get_one)
                .patch(workspaces::rename)
                .delete(workspaces::delete),
        )
        .route("/workspaces/{id}/kbs", get(kbs::list).post(kbs::create))
        // Static list used by the create-database UI, unrelated to any particular workspace
        .route("/ontology-packs", get(kbs::list_packs))
        .route("/scenario-packs", get(kbs::list_scenario_packs))
        .route("/workspaces/{id}/my-kbs", get(kbs::my_kbs))
        .route(
            "/workspaces/{id}/settings",
            get(settings_routes::get).put(settings_routes::put),
        )
        .route(
            "/workspaces/{id}/settings/ocr",
            put(settings_routes::put_ocr),
        )
        .route(
            "/workspaces/{id}/settings/transcribe",
            put(settings_routes::put_transcribe),
        )
        .route(
            "/workspaces/{id}/settings/test",
            post(settings_routes::test),
        )
        .route("/workspaces/{id}/members", get(members_routes::list))
        .route(
            "/workspaces/{id}/members/{user_id}",
            axum::routing::put(members_routes::set_role).delete(members_routes::remove),
        )
        .route("/users", get(members_routes::org_users))
        // Deactivated accounts: without this entry, restoring is out of reach (the person disappears from every list)
        .route("/users/deactivated", get(members_routes::deactivated_users))
        .route(
            "/kbs/{id}",
            patch(kbs::update).get(kbs::get_one).delete(kbs::delete),
        )
        // One step decision shared by the empty states of four pages (#313)
        .route("/kbs/{id}/readiness", get(kbs::readiness))
        .route("/kbs/{id}/members", get(kbs::members))
        .route("/kbs/{id}/audit", get(kbs::audit_log))
        // Full-database export as RDF (0020). Viewers can export: what they can see they could already copy out one by one,
        // so blocking it here only makes honest people work a bit harder
        .route("/kbs/{id}/export", get(export_routes::export))
        // Failed tasks back to the queue (#216): Editor within the database, admin globally
        .route("/kbs/{id}/jobs/failed", get(jobs_routes::failed_in_kb))
        .route("/kbs/{id}/jobs/requeue", post(jobs_routes::requeue_in_kb))
        .route("/jobs/requeue", post(jobs_routes::requeue_all))
        // A task's status (0051): after a person has signed the phrase and gotten a job id, they come here to ask whether it has finished
        .route("/kbs/{id}/jobs/{job_id}", get(jobs_routes::job_in_kb))
        .route(
            "/kbs/{id}/members/{user_id}",
            axum::routing::put(kbs::set_member).delete(kbs::remove_member),
        )
        .route(
            "/admin/deployment",
            get(admin_routes::get_deployment).put(admin_routes::put_deployment),
        )
        .route("/admin/users", post(admin_routes::create_user))
        // Deactivate / reactivate an account (see `users.deactivated_at`). DELETE means "this person no longer has access",
        // not "this row is gone" -- attribution is still queryable as before
        .route(
            "/admin/users/{id}",
            axum::routing::delete(admin_routes::deactivate_user)
                .post(admin_routes::reactivate_user)
                .put(admin_routes::set_system_admin),
        )
        // Admins can see who bound which subject and can unlink; **cannot bind on someone's behalf** (0056)
        .route("/admin/oidc/identities", get(oidc_routes::identities))
        .route(
            "/admin/oidc/identities/{user_id}",
            axum::routing::delete(oidc_routes::unbind),
        )
        .route(
            "/admin/data-sources",
            get(datasource_routes::list).post(datasource_routes::create),
        )
        .route(
            "/admin/data-sources/{id}",
            axum::routing::delete(datasource_routes::delete),
        )
        // The static segment sits before `{id}`: try a connection before saving, nothing is persisted
        .route(
            "/admin/data-sources/test",
            post(datasource_routes::test_conn),
        )
        .route(
            "/admin/data-sources/{id}/test",
            post(datasource_routes::test),
        )
        .route(
            "/admin/data-sources/{id}/grants",
            get(datasource_routes::grants),
        )
        .route(
            "/admin/data-sources/{id}/grants/{workspace_id}",
            axum::routing::put(datasource_routes::grant).delete(datasource_routes::revoke),
        )
        .route("/kbs/{id}/mcp", post(mcp::handle))
        .route(
            "/me/tokens",
            get(token_routes::list).post(token_routes::issue),
        )
        .route(
            "/me/tokens/{token_id}",
            axum::routing::delete(token_routes::revoke),
        )
        .route(
            "/kbs/{id}/mappings",
            get(mapping_routes::list).post(mapping_routes::create),
        )
        // The static segment sits before `{mapping_id}`: run it once to see the numbers, nothing is persisted
        .route("/kbs/{id}/mappings/preview", post(mapping_routes::preview))
        // A count tied to a question (the same retrieval the question answering uses)
        .route("/kbs/{id}/mappings/relevant", get(mapping_routes::relevant))
        .route(
            "/kbs/{id}/mappings/{mapping_id}",
            axum::routing::patch(mapping_routes::revise),
        )
        .route(
            "/kbs/{id}/mappings/{mapping_id}/revisions",
            get(mapping_routes::revisions),
        )
        .route("/kbs/{id}/data-sources", get(datasource_routes::mounted))
        .route(
            "/kbs/{id}/data-sources/available",
            get(datasource_routes::mountable),
        )
        .route(
            "/kbs/{id}/data-sources/{ds_id}",
            axum::routing::put(datasource_routes::mount).delete(datasource_routes::unmount),
        )
        .route(
            "/kbs/{id}/data-sources/{ds_id}/sync-schema",
            post(datasource_routes::sync_schema),
        )
        .route(
            "/kbs/{id}/data-sources/explore",
            post(datasource_routes::explore),
        )
        .route(
            "/kbs/{id}/documents",
            get(documents_routes::list)
                .post(documents_routes::upload)
                .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        // One-click retry: extraction failures usually come in batches (the model endpoint dropped for a while, and everything that came in then failed)
        .route(
            "/kbs/{id}/documents/retry-failed",
            post(documents_routes::retry_failed),
        )
        .route(
            "/kbs/{id}/extraction-drops",
            get(documents_routes::extraction_drops),
        )
        .route("/kbs/{id}/ontology", get(ontology_routes::get))
        .route(
            "/kbs/{id}/ontology/regressions",
            get(ontology_regression_routes::list).post(ontology_regression_routes::create),
        )
        // Business rules (0021): reading rules needs Viewer, writing needs Editor
        .route(
            "/kbs/{id}/rules",
            get(rule_routes::list).post(rule_routes::create),
        )
        .route("/kbs/{id}/rules/run", post(rule_routes::run_now))
        .route(
            "/kbs/{id}/rules/{rule_id}",
            patch(rule_routes::update).delete(rule_routes::delete),
        )
        .route(
            "/kbs/{id}/rules/{rule_id}/matches",
            get(rule_routes::matches),
        )
        // Definition history (0060): how many times a rule was changed and what each version said
        .route(
            "/kbs/{id}/rules/{rule_id}/versions",
            get(rule_routes::versions),
        )
        .route(
            "/kbs/{id}/ontology/type-resolution/preview",
            post(ontology_routes::type_resolution_preview),
        )
        .route(
            "/kbs/{id}/ontology/type-resolution",
            post(ontology_routes::type_resolution_apply),
        )
        .route(
            "/kbs/{id}/ontology/type-resolution/approve",
            post(ontology_routes::approve_refinement),
        )
        .route(
            "/kbs/{id}/ontology/type-resolution/{batch_id}",
            axum::routing::delete(ontology_routes::type_resolution_undo),
        )
        .route(
            "/kbs/{id}/ontology/entity-types",
            post(ontology_routes::create_entity_type),
        )
        .route(
            "/kbs/{id}/ontology/entity-types/{type_id}",
            patch(ontology_routes::update_entity_type).delete(ontology_routes::delete_entity_type),
        )
        .route(
            "/kbs/{id}/ontology/entity-types/{type_id}/entities",
            get(ontology_routes::list_entity_instances),
        )
        .route(
            "/kbs/{id}/ontology/relation-types",
            post(ontology_routes::create_relation_type),
        )
        .route(
            "/kbs/{id}/ontology/relation-types/{type_id}",
            patch(ontology_routes::update_relation_type)
                .delete(ontology_routes::delete_relation_type),
        )
        // Declarations arrived late (#341): which side of whom has more than two open values; after declaring, reconcile the books
        .route(
            "/kbs/{id}/ontology/uniqueness",
            get(ontology_routes::uniqueness_candidates),
        )
        .route(
            "/kbs/{id}/ontology/relation-types/{type_id}/reconcile",
            post(ontology_routes::reconcile_relation_type),
        )
        // The ones computed last time that nobody has yet ruled on (see `ontology_proposals`). Page refresh relies on this, so the model need not be rerun
        .route(
            "/kbs/{id}/ontology/proposals",
            get(ontology_routes::stored_proposals).post(ontology_routes::decide_proposal),
        )
        // 0061: capability questions are the acceptance criteria for the ontology; the agent proposes an ontology from the open graph, and a human adopts it
        .route(
            "/kbs/{id}/questions",
            get(question_routes::list).post(question_routes::create),
        )
        .route(
            "/kbs/{id}/questions/{qid}",
            patch(question_routes::update).delete(question_routes::delete),
        )
        .route(
            "/kbs/{id}/questions/propose",
            post(question_routes::propose_questions),
        )
        .route("/kbs/{id}/questions/report", get(question_routes::report))
        .route(
            "/kbs/{id}/questions/{qid}/result",
            post(question_routes::record_result),
        )
        .route("/kbs/{id}/ontology/propose", post(question_routes::propose))
        .route(
            "/kbs/{id}/ontology/proposals/adopt",
            post(question_routes::adopt),
        )
        // OWL import: preview and persist are two separate endpoints, and an upload never changes the ontology directly.
        // Both run the same plan -- separate code paths diverge, and divergence means what happens after confirmation
        // differs from what was just reviewed
        .route(
            "/kbs/{id}/ontology/imports",
            get(ontology_routes::list_imports)
                .post(ontology_routes::apply_import)
                .layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route(
            "/kbs/{id}/ontology/imports/preview",
            post(ontology_routes::preview_import).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route("/kbs/{id}/search", post(search_routes::search))
        .route("/kbs/{id}/chat", post(chat::chat))
        .route("/kbs/{id}/chat/{conversation_id}/stop", post(chat::stop))
        // After a page refresh, reattach to the answer still being generated (see `live`)
        .route(
            "/kbs/{id}/conversations/{conversation_id}/stream",
            get(chat::reattach),
        )
        .route("/kbs/{id}/conversations", get(chat::list_conversations))
        .route(
            "/kbs/{id}/conversations/{conversation_id}",
            get(chat::conversation_detail)
                .patch(chat::rename_conversation)
                .delete(chat::delete_conversation),
        )
        .route(
            "/documents/{id}",
            get(documents_routes::detail).delete(documents_routes::delete),
        )
        .route("/documents/{id}/content", get(documents_routes::content))
        .route("/documents/{id}/versions", get(documents_routes::versions))
        // Undo delete (#268): deletion is a tombstone, so it can be undone
        .route("/documents/{id}/restore", post(documents_routes::restore))
        // Real delete (#268, second half): only for already-deleted items, database admins only
        .route("/documents/{id}/purge", post(documents_routes::purge))
        .route("/documents/{id}/extract", post(graph_routes::extract))
        .route("/kbs/{id}/graph/overview", get(graph_routes::overview))
        .route(
            "/kbs/{id}/graph/neighborhood",
            get(graph_routes::neighborhood),
        )
        .route("/kbs/{id}/entities", get(graph_routes::search_entities))
        .route(
            "/kbs/{id}/entities/{entity_id}",
            get(graph_routes::entity_detail).patch(graph_routes::update_entity),
        )
        .route(
            "/kbs/{id}/entities/{entity_id}/history",
            get(graph_routes::entity_history),
        )
        .route(
            "/kbs/{id}/facts/{fact_id}/evidence",
            get(graph_routes::fact_evidence),
        )
        // Manual correction of the valid interval (302): symmetric with the entity PATCH, goes through the ledger instead of editing in place
        .route(
            "/kbs/{id}/facts/{fact_id}",
            patch(graph_routes::update_fact_time),
        )
        // Proof of derived facts (0002 R2): premises expanded in order back to the original sentence
        .route(
            "/kbs/{id}/derived/{derived_id}/proof",
            get(graph_routes::derived_proof),
        )
        // Proof of a derivation that did not land (0017 §3): the premises are in the violation's path
        .route(
            "/kbs/{id}/violations/{violation_id}/proof",
            get(graph_routes::blocked_proof),
        )
        .route("/kbs/{id}/events", get(events_routes::kb_events))
        .route(
            "/kbs/{id}/sources",
            get(sources_routes::list).post(sources_routes::create),
        )
        .route(
            "/kbs/{id}/sources/{source_id}",
            patch(sources_routes::update).delete(sources_routes::delete),
        )
        .route(
            "/kbs/{id}/sources/{source_id}/sync",
            post(sources_routes::sync_now),
        )
        .route(
            "/kbs/{id}/sources/{source_id}/runs",
            get(sources_routes::runs),
        )
        // Read a document back by its external_id as pushed (#1276): the id may contain `/`, so it goes in a query parameter
        .route(
            "/kbs/{id}/sources/{source_id}/documents/by-external-id",
            get(documents_routes::by_external_id),
        )
        .route(
            "/kbs/{id}/sources/{source_id}/re-extract",
            post(sources_routes::re_extract),
        )
        .route("/kbs/{id}/graph/rebuild", post(graph_routes::rebuild))
        .route(
            "/kbs/{id}/sources/{source_id}/missing/cleanup",
            post(sources_routes::cleanup_missing),
        )
        // The source closes facts it no longer asserts (RC-004); the write token is usable
        .route(
            "/kbs/{id}/sources/{source_id}/close-unconfirmed",
            post(sources_routes::close_unconfirmed),
        )
        .route(
            "/documents/{id}/extractions",
            get(documents_routes::extractions),
        )
        .route("/kbs/{id}/ingest", post(sources_routes::ingest))
        // api source push: source-specific key authentication (Bearer), no session
        .route("/sources/{source_id}/ingest", post(sources_routes::push))
        // Push statements, not documents (0054): the request body is the open extraction contract, and extraction does not ask a model
        // The body is fully read before the key is checked, so a limit far below the 2 MiB default is applied here;
        // the 64 KiB check at the door is still a 422; this limit only blocks things that are clearly not a payload
        .route(
            "/sources/{source_id}/statements",
            post(sources_routes::push_statements).layer(DefaultBodyLimit::max(
                4 * sources_routes::STATEMENTS_MAX_BYTES,
            )),
        )
        .route(
            "/kbs/{id}/sources/{source_id}/google-drive/connect",
            post(google_drive_routes::connect),
        )
        .route(
            "/kbs/{id}/sources/{source_id}/google-drive/connection",
            axum::routing::delete(google_drive_routes::disconnect),
        )
        .route(
            "/kbs/{id}/sources/{source_id}/google-drive/folders",
            get(google_drive_routes::folders).put(google_drive_routes::select),
        )
        .route(
            "/kbs/{id}/sources/{source_id}/token",
            get(sources_routes::get_token),
        )
        .route(
            "/kbs/{id}/sources/{source_id}/rotate-token",
            post(sources_routes::rotate_token),
        )
        .route("/kbs/{id}/review", get(review_routes::list))
        .route("/kbs/{id}/review/history", get(review_routes::history))
        .route("/kbs/{id}/review/summary", get(review_routes::summary))
        .route("/kbs/{id}/review/batch", post(review_routes::batch))
        .route(
            "/kbs/{id}/review/agent/{decision_id}",
            post(review_routes::agent_answer),
        )
        // Facts extracted from memory, awaiting a human nod (0015): taken per sentence, judged one by one
        .route(
            "/kbs/{id}/review/pending",
            get(review_routes::pending_for_chunk),
        )
        .route(
            "/kbs/{id}/review/pending/{pending_id}",
            post(review_routes::decide_pending),
        )
        .route("/kbs/{id}/review/{review_id}", post(review_routes::decide))
        // Alignment queue (#725): the class of attribute and direction, and category words, which a person signs off on
        .route(
            "/kbs/{id}/review/alignment/phrases/{binding_id}",
            post(review_routes::decide_alignment_phrase),
        )
        .route(
            "/kbs/{id}/review/alignment/kind-words/{kind_word}",
            post(review_routes::decide_alignment_kind_word),
        )
        // Errata queue (0044 decision 7): a human approves or rejects an entry the agent gated
        .route(
            "/kbs/{id}/review/errata/{action_id}",
            post(review_routes::decide_errata),
        )
        // A human approves or rejects an entailment rule (0044 decision 3, fifth slice)
        .route(
            "/kbs/{id}/review/alignment/rules/{rule_id}",
            post(review_routes::decide_alignment_rule),
        )
        // Verdicts on semantic-layer mappings (0011). Alongside resolution review -- both are "the engine proposes, a human decides"
        .route(
            "/kbs/{id}/review/mappings/{mapping_id}",
            post(review_routes::decide_mapping),
        )
        // Consistency check (0002 R0): run once, one violation per decision point.
        // The check itself is pure computation, run synchronously
        .route(
            "/kbs/{id}/consistency/check",
            post(review_routes::run_consistency_check),
        )
        .route(
            "/kbs/{id}/review/violations/{violation_id}",
            post(review_routes::decide_violation),
        )
        .route(
            "/kbs/{id}/review/defects/{defect_id}",
            post(review_routes::decide_defect),
        )
        // R1 materialized inference. Governed by the materialize_inferences switch on the KB
        .route(
            "/kbs/{id}/inference/run",
            post(review_routes::run_inference),
        )
        .route(
            "/kbs/{id}/facts/{fact_id}/confirm",
            post(review_routes::confirm_fact),
        )
        .route(
            "/kbs/{id}/facts/{fact_id}/reject",
            post(review_routes::reject_fact),
        )
        .route(
            "/kbs/{id}/facts/{fact_id}/close",
            post(review_routes::close_fact),
        )
        .route(
            "/kbs/{id}/merges/{merge_id}/revert",
            post(review_routes::revert_merge),
        )
        .route(
            "/kbs/{id}/conflicts/{conflict_id}",
            post(review_routes::resolve_conflict),
        )
        .route(
            "/kbs/{id}/entities/merge",
            post(review_routes::manual_merge),
        )
        .route(
            "/documents/{id}/reprocess",
            post(documents_routes::reprocess),
        )
        .route("/jobs/noop", post(jobs_noop))
        .with_state(state);

    // Development CORS: the Vite dev server carries cookies across ports
    let cors = CorsLayer::new()
        .allow_origin("http://localhost:5173".parse::<HeaderValue>().unwrap())
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::PUT,
        ])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .allow_credentials(true);

    /* **Paths under `/api` that are not recognized are 404, not the home page.**
    The history fallback below is for page routes: refreshing /graph must get index.html.
    But it is attached to the **whole application**, so a misspelled or already-deleted endpoint also falls into the same net and gets back
    `200 text/html``200 text/html`. The client's `res.json()` then throws "Unexpected token '<'",
    and the error site is a world away from the cause; it is worse for integrators of the two external contracts, MCP and RDF:
    a generic HTTP client sees 200 and counts it as success, while the parser sees a web page.
    Give the nested API routes a fallback of their own, with a shape consistent with the other errors (in error.rs
    `NotFound` `NotFound` already maps to 404). */
    let api = api.fallback(|| async { ApiErr(AppError::NotFound) });
    let mut app = Router::new()
        .nest("/api/v1", api)
        /* A request with a wrong or missing version number (`/api/kbs`, `/api/v2/...`) cannot fall into the nested block above
        and still ends up on the home page. **There are no pages under `/api`**, so the whole prefix is caught;
        static segments are more specific than wildcards, so `/api/v1/...` still goes through the route above. */
        .route("/api/{*rest}", any(|| async { ApiErr(AppError::NotFound) }))
        .layer(cors);

    app = with_static_files(app, &cfg.web_dist);

    // Outermost layer: put the request origin into task-local first, so every layer after it can read it when writing audit
    app.layer(TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn(crate::client_ctx::capture))
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "name": "utopia", "version": build_version::build_version() }))
}

/// P0 queue verification endpoint: enqueues a noop task (removed in a later milestone).
async fn jobs_noop(
    axum::extract::State(state): axum::extract::State<AppState>,
    _user: crate::auth::AuthUser,
) -> crate::error::ApiResult<Json<serde_json::Value>> {
    let id = utopia_store::jobs::enqueue(&state.pool, "noop", json!({})).await?;
    Ok(Json(json!({ "job_id": id })))
}

#[cfg(test)]
mod rule_metadata_tests;

#[cfg(test)]
mod system_admin_tests;

#[cfg(test)]
mod oidc_exchange_tests;

#[cfg(test)]
mod token_setup_tests;

#[cfg(test)]
mod token_read_tests;

#[cfg(test)]
mod token_graph_view_tests;

#[cfg(test)]
mod token_ontology_tests;

#[cfg(test)]
mod token_export_tests;

#[cfg(test)]
mod search_routes_tests;

#[cfg(test)]
mod rule_expression_tests;
