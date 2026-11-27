mod adjudication;
mod alerting;
mod api;
mod auth;
mod blob;
mod client_ctx;
mod decay_sweep;
mod docs_corpus;
mod errata;
mod error;
mod extraction;
mod extraction_open;
mod github_issues;
mod github_repo;
mod gitlab_issues;
mod gitlab_repo;
mod google_drive;
mod governance;
mod http_fetch;
mod implication;
mod ingest_sources;
mod jira_issues;
mod live;
mod llm_util;
mod mapping_index;
mod mappings;
mod memory_use;
#[cfg(test)]
mod namesake_scripted_cases;
mod notion;
mod object_storage;
mod ontology_agent;
mod ontology_index;
mod ontology_packs;
mod owl_import;
mod pack_alignment;
mod phrase_alignment;
mod pipeline;
mod pushed_keys;
mod query_engine;
mod rdf;
mod readers;
mod retrieval;
mod rss_full_content;
mod scenario_packs;
mod search_recovery;
mod state;
mod ticket;
#[cfg(test)]
mod ticket_golden_tests;
mod time_resolution;
mod time_text;
mod type_alignment;
mod type_resolution;
mod webdav;

use state::AppState;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use utopia_core::config::AppConfig;
use utopia_search::SearchIndex;
use uuid::Uuid;

/// Postgres cancels any interactive read (chat/MCP tool call, graph route) that runs longer than this.
const INTERACTIVE_STATEMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The memory path carries "who said it" (0015, 0026); other tasks do not have these two fields,
/// and reading the default is exactly right for them
fn payload_proposer(payload: &serde_json::Value) -> utopia_core::models::Proposer {
    let uuid = |key: &str| {
        payload
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
    };
    utopia_core::models::Proposer {
        user_id: uuid("proposed_by"),
        token_id: uuid("proposed_token"),
    }
}

fn payload_document_id(payload: &serde_json::Value) -> anyhow::Result<Uuid> {
    payload
        .get("document_id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("payload missing document_id"))
}

/// The most common misunderstanding among database connection failures, stated on the spot (#456): the compose deployment's database password
/// only takes effect when the data volume is first initialised; changing .env afterwards changes only the app's side. Whoever reads the log
/// should get the command to run straight away, not have to open an issue.
fn explain_db_error(e: anyhow::Error) -> anyhow::Error {
    if format!("{e:#}").contains("password authentication failed") {
        e.context(
            "The database rejected the password. In a docker compose deployment the password \
             is set only when the data volume is first initialised, so changing UTOPIA_DB_PASSWORD \
             later only changes what the app sends. Either set it in the database too — \
             docker compose exec db psql -U utopia -c \"ALTER USER utopia PASSWORD '<new password>'\" \
             — or start over with `docker compose --profile app down -v` (this deletes all data).",
        )
    } else {
        e
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,utopia=debug".into()),
        )
        .init();

    let cfg = AppConfig::load()?;

    // Migrations need to create tables and triggers, which the runtime does not need those privileges for. The two are kept apart, so the app can connect with a
    // restricted role that reads and writes business tables and only appends to the ledger. The migration pool is released right after use,
    // so the high-privilege connection does not stay resident at runtime.
    let migration_url = cfg.migration_url().to_string();
    let separate_migration_role = cfg.migration_url.is_some();
    {
        let mig_pool = utopia_store::db::connect(&migration_url, Some(2))
            .await
            .map_err(explain_db_error)?;
        utopia_store::db::migrate(&mig_pool).await?;
        mig_pool.close().await;
    }
    if separate_migration_role {
        tracing::info!(
            "database migrations complete (migration identity separated from runtime identity)"
        );
    } else {
        tracing::info!("database migrations complete");
    }

    let pool = utopia_store::db::connect(&cfg.database_url, cfg.db_max_connections)
        .await
        .map_err(explain_db_error)?;

    // Credential sealing key: the environment variable takes precedence, otherwise secret.key in the data directory (generated on first start).
    // **The key is not stored in the database**: a database leak is not a credential leak, which is the whole point of this layer. An empty string is treated as unset;
    // the reasoning is the same as for the JWT secret below
    let secret_key = match cfg.secret_key.clone().filter(|s| !s.trim().is_empty()) {
        Some(text) => utopia_core::secrets::parse_key(&text).ok_or_else(|| {
            anyhow::anyhow!("UTOPIA_SECRET_KEY must be 32 bytes, as 64 hex characters or base64")
        })?,
        None => secret_key_file(std::path::Path::new(&cfg.data_dir))?,
    };
    utopia_core::secrets::init(secret_key);
    // Plaintext credentials stored before the upgrade are sealed here
    let sealed = utopia_store::sealing::backfill(&pool).await?;
    if sealed > 0 {
        tracing::info!(rows = sealed, "credential re-sealing complete");
    }

    let index_dir = std::path::Path::new(&cfg.data_dir).join("index");
    let search = Arc::new(search_recovery::open(&index_dir)?);
    tracing::info!("full-text index ready: {}", index_dir.display());

    // **If the index comes up empty, rebuild it automatically; no button is offered.**
    //
    // The index is a file separate from the database: changing machines, a volume not mounted, or a corrupted directory, any of these makes it
    // come up empty. And once it is empty, retrieval silently returns zero: nothing looks wrong on the screen, and users think it is
    // "genuinely no match". This kind of failure should not rely on a person noticing and then pressing a button.
    //
    // The criterion is "the database has chunks but the index is empty", not "the counts do not match": the latter also briefly disagrees during normal running
    // (a document is being indexed), and using it as the criterion would rebuild on every start.
    reindex_if_empty(&pool, &search, &index_dir).await;

    // JWT secret: the environment variable takes precedence (rotation and explicit alignment across instances go this way); otherwise use the one in the database;
    // if the database has none either, generate one now and store it. Generation is here rather than in the store because
    // OsRng is already in the server's dependencies alongside argon2, so the store need not add a dependency for it.
    // An empty string is treated as unset: with `${UTOPIA_JWT_SECRET:-}` in compose the variable exists but is empty, and read literally it
    // would give Some(""), an empty secret shared by every deployment, which is worse than a default.
    let jwt_secret = match cfg.jwt_secret.clone().filter(|s| !s.trim().is_empty()) {
        Some(s) => s,
        None => {
            let secret =
                utopia_store::access::ensure_jwt_secret(&pool, &auth::generate_jwt_secret())
                    .await?;
            tracing::info!(
                "JWT key taken from deployment settings (UTOPIA_JWT_SECRET not set explicitly)"
            );
            secret
        }
    };

    let interactive_pool_size =
        utopia_store::db::interactive_pool_size(pool.options().get_max_connections());
    let interactive_pool = utopia_store::db::connect_interactive(
        &cfg.database_url,
        interactive_pool_size,
        INTERACTIVE_STATEMENT_TIMEOUT,
    )
    .await
    .map_err(explain_db_error)?;
    tracing::info!(
        max_connections = interactive_pool_size,
        "interactive database connection pool established"
    );

    let state = AppState::new(pool.clone(), &cfg, search, jwt_secret)
        .with_interactive_pool(interactive_pool);
    api::oidc_routes::warn_about_password_login(&state);
    api::oidc_routes::warn_about_scopes();

    // Worker concurrency: system settings are persisted and loaded at startup; at runtime they are adjusted live through the same AtomicUsize
    let n = utopia_store::access::worker_concurrency(&pool)
        .await
        // Kept consistent with the column default of `deployment_settings.worker_concurrency` (migration 0011).
        // access::worker_concurrency already falls back to 64 when the row does not exist; this extra layer
        // is because **the function itself erroring** (database unreachable, query timeout) also lands here: on this path
        // the system is degraded, and running with 64 rather than 32 is the under-concurrency that migration 0011 wanted to avoid.
        .unwrap_or(64);
    state.worker_concurrency.store(
        n.clamp(1, 256) as usize,
        std::sync::atomic::Ordering::Relaxed,
    );

    // Task dispatch: new task kinds are registered here
    let worker_state = state.clone();
    tokio::spawn(utopia_store::jobs::run_worker(
        pool,
        state.worker_concurrency.clone(),
        memory_use::read,
        move |job| {
            let st = worker_state.clone();
            async move {
                // When a task fails, check whether the model endpoint is unreachable: that is a system-level failure,
                // yet it would only stay in jobs.last_error, which no screen shows
                //
                // An unrecoverable failure is marked here, so the queue stops backing off and retrying it (#195):
                // the criterion is on this side, because `utopia-store` cannot see the LLM's error types
                let result = dispatch(&st, &job)
                    .await
                    .map_err(|e| match alerting::hopeless(&e) {
                        true => e.context(utopia_core::Terminal),
                        false => e,
                    });
                if let Err(e) = &result {
                    alerting::observe_job_failure(&st, &job, e).await;
                }
                result
            }
        },
    ));

    alerting::spawn_retention_sweep(state.clone());

    // Scheduled ingestion scheduler: scan due sources once a minute, enqueue sync tasks
    let sched_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match utopia_store::sources::due_sources(&sched_state.pool).await {
                Ok(due) => {
                    for s in due {
                        if let Err(e) =
                            utopia_store::sources::enqueue_sync(&sched_state.pool, s.id).await
                        {
                            tracing::warn!(source_id = %s.id, error = %e, "failed to enqueue sync task");
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "failed to scan due sources"),
            }
        }
    });

    // Scheduled inference scheduler (0002 R1).
    //
    // **It must run on a schedule, not only when pressed by hand**: facts keep changing (every document's extraction adds edges), and
    // derivations are computed only at the moment they run. Without a schedule, after the next document comes in the derivations on the graph are missing,
    // and this kind of gap is invisible on screen (it is not an error; the new chain simply was not inferred).
    //
    // Shares one beat with source sync: scan every minute and enqueue the due ones. The real derivation runs in the task,
    // not in this loop: a full re-derivation of a large library may take seconds, and blocking the scheduling loop would hold up other libraries
    let infer_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Err(e) = utopia_store::reasoning::queue_due_inferences(&infer_state.pool).await {
                tracing::warn!(error = %e, "failed to scan due reasoning runs");
            }
        }
    });

    // Scheduled governance scan (0025): when the switch is on and the queue still has libraries with correct data that the agent has not looked at, queue a round every hour.
    // Extraction completion and human rulings each queue immediately; this round catches the backlog left after failure retries are exhausted,
    // so it is hourly rather than per minute: when the model is down, hitting it every minute is pointless
    let gov_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match utopia_store::governance::due(&gov_state.pool).await {
                Ok(due) => {
                    for kb_id in due {
                        if let Err(e) = utopia_store::jobs::enqueue_unless_queued(
                            &gov_state.pool,
                            "govern",
                            serde_json::json!({ "kb_id": kb_id }),
                        )
                        .await
                        {
                            tracing::warn!(kb_id = %kb_id, error = %e, "failed to enqueue governance task");
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "failed to scan governance backlog"),
            }
        }
    });

    // Daily decay sweep (RC-003): for sources with decay on, where live documents exceed max_items, queue an eviction task
    let decay_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(86_400));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match utopia_store::decay::sources_over_cap(&decay_state.pool).await {
                Ok(sources) => {
                    for source_id in sources {
                        if let Err(e) = utopia_store::jobs::enqueue_unless_queued(
                            &decay_state.pool,
                            "decay_sweep",
                            serde_json::json!({ "source_id": source_id }),
                        )
                        .await
                        {
                            tracing::warn!(source_id = %source_id, error = %e, "failed to enqueue decay task");
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "failed to scan sources over capacity"),
            }
        }
    });

    // Whether superseded chunks keep their vectors is decided by the deployment: without `UTOPIA_SUPERSEDED_VECTOR_DAYS` nothing is queued
    if let Some(days) = cfg.superseded_vector_days {
        let prune_state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(86_400));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let bases = match utopia_store::retention::bases(&prune_state.pool).await {
                    Ok(bases) => bases,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to list databases to clean up");
                        continue;
                    }
                };
                for kb_id in bases {
                    if let Err(e) = utopia_store::jobs::enqueue_unless_queued(
                        &prune_state.pool,
                        utopia_store::retention::JOB_KIND,
                        serde_json::json!({ "kb_id": kb_id, "days": days }),
                    )
                    .await
                    {
                        tracing::warn!(kb_id = %kb_id, error = %e, "failed to enqueue cleanup task");
                    }
                }
            }
        });
    }

    let app = api::router(state, &cfg);
    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    tracing::info!("Utopia server started at http://{}", cfg.bind_addr);

    // Browsers may resolve localhost to ::1 (IPv6): when the configured address is IPv4, add an IPv6 loopback listener on the same port,
    // to avoid "cannot find localhost". If binding fails (port in use or no IPv6), only warn.
    if let Ok(addr) = cfg.bind_addr.parse::<std::net::SocketAddrV4>() {
        let v6_addr = format!("[::1]:{}", addr.port());
        match tokio::net::TcpListener::bind(&v6_addr).await {
            Ok(v6_listener) => {
                tracing::info!("also listening on http://{v6_addr}");
                let app_v6 = app.clone();
                tokio::spawn(async move {
                    let svc = app_v6.into_make_service_with_connect_info::<std::net::SocketAddr>();
                    if let Err(e) = axum::serve(v6_listener, svc).await {
                        tracing::warn!(error = %e, "IPv6 listener exited");
                    }
                });
            }
            Err(e) => tracing::warn!(error = %e, "IPv6 loopback bind failed (IPv4 unaffected)"),
        }
    }

    // with_connect_info: the audit needs the real TCP peer address. For direct deployments it is the only true value:
    // the X-Forwarded-For headers do not exist in that case, and are not to be trusted anyway.
    let svc = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, svc).await?;
    Ok(())
}

/// Index empty while the database has content: rebuild it from the database.
///
/// Failure only warns and does not block startup: retrieval degrades to "temporarily unsearchable", and the whole service failing to start is the worse
/// outcome. The line in the log says what happened; the next restart will try again.
async fn reindex_if_empty(pool: &sqlx::PgPool, search: &SearchIndex, index_dir: &std::path::Path) {
    if !search.is_empty() {
        return;
    }
    let rows = match utopia_store::documents::all_chunks_for_index(pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read chunks, index not rebuilt");
            return;
        }
    };
    let total = rows.len();
    if total > 0 {
        tracing::info!(
            chunks = total,
            "full-text index is empty, rebuilding from database"
        );
    }
    // Even an empty database completes an interrupted rebuild: otherwise the
    // marker would make every subsequent startup discard an empty generation.
    match search_recovery::rebuild(index_dir, search, rows) {
        Ok(()) if total > 0 => tracing::info!(chunks = total, "full-text index rebuild finished"),
        Ok(()) => {}
        Err(e) => {
            tracing::warn!(error = %e, "full-text index rebuild did not finish, will retry on next startup")
        }
    }
}
///
/// Task dispatch: new task kinds are registered here.
///
/// Pulled out into a function rather than inlined in the closure, so that failure has **one uniform exit**:
/// the layer above needs to look at the error chain on every failure, and inlined, every branch would have to remember to do it.
async fn dispatch(st: &state::AppState, job: &utopia_store::jobs::Job) -> anyhow::Result<()> {
    match job.kind.as_str() {
        "noop" => {
            tracing::info!(job_id = job.id, "noop task ran successfully");
            Ok(())
        }
        "process_document" => {
            let id = payload_document_id(&job.payload)?;
            pipeline::process_document(st, id).await
        }
        "memory_ingest" => {
            let id = payload_document_id(&job.payload)?;
            pipeline::memory_ingest(st, id, payload_proposer(&job.payload)).await
        }
        "explore_mappings" => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            mappings::explore_mappings(st, kb_id).await
        }
        // Scheduled re-derivation (0002 R1). **Record the time first, then derive**: even if derivation errors, this library should not
        // be rescheduled the next minute, which would become a loop that fails once a minute
        "materialize_inferences" => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            utopia_store::reasoning::mark_inference_ran(&st.pool, kb_id).await?;
            let report = utopia_store::reasoning::materialize(&st.pool, kb_id).await?;
            // **Compare at the due time**: `materialize` already compares what this round computed against what the library holds,
            // so "comparison" is not a new mechanism. Here we only keep that comparison's result for people to see:
            // not written when nothing changed, so the ledger is not flooded by one "nothing changed" entry an hour
            if report.inserted > 0 || report.invalidated > 0 {
                let _ = utopia_store::audit::record(
                    &st.pool,
                    Some(kb_id),
                    Uuid::nil(),
                    "inference.materialized",
                    "knowledge_base",
                    Some(kb_id),
                    serde_json::json!({
                        "scheduled": true,
                        "inserted": report.inserted,
                        "invalidated": report.invalidated,
                        "rules": report.rules,
                    }),
                )
                .await;
                st.emit_graph(kb_id);
            }
            Ok(())
        }
        // RC-010: the witness recomputation left by deletion and undo-deletion is done in a transaction outside the source row
        "reattest_facts" => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            let fact_ids: Vec<Uuid> =
                serde_json::from_value(job.payload.get("fact_ids").cloned().unwrap_or_default())?;
            utopia_store::documents::reattest_facts(&st.pool, kb_id, &fact_ids).await?;
            st.emit_graph(kb_id);
            Ok(())
        }
        utopia_store::jobs::EXTRACT_DOCUMENT | utopia_store::jobs::EXTRACT_STATEMENTS => {
            let id = payload_document_id(&job.payload)?;
            extraction::extract_document(st, id, payload_proposer(&job.payload)).await
        }
        // Ontology vector index: **built in the background, not blocking requests**.
        // A first embedding of an ontology with 965 classes takes 2600 rows, six to eight minutes; if placed
        // in an interactive request, the first person to use retrieval after the import would wait
        "embed_ontology" => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            ontology_index::refresh(st, kb_id).await.map(|_| ())
        }
        // Vector index (0035 / #512): queued when the first vector of some dimension is written; built CONCURRENTLY outside the transaction.
        // Dimensions above the HNSW limit stop here: retrying cannot build them either
        "build_vector_index" => {
            let target = job
                .payload
                .get("table")
                .and_then(|v| v.as_str())
                .and_then(utopia_store::vector_index::Target::parse)
                .ok_or_else(|| anyhow::anyhow!("payload is missing table"))?;
            let dims = job
                .payload
                .get("dims")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("payload is missing dims"))?
                as usize;
            let built = utopia_store::vector_index::build_as_owner(
                &st.pool,
                st.owner_database_url.as_deref(),
                target,
                dims,
            )
            .await
            .map_err(|e| match e {
                utopia_core::AppError::Validation(_) => {
                    anyhow::Error::from(e).context(utopia_core::Terminal)
                }
                other => anyhow::Error::from(other),
            })?;
            tracing::info!(
                index = %built.name,
                created = built.created,
                seconds = format!("{:.1}", built.seconds),
                "向量索引就绪"
            );
            Ok(())
        }
        // Time mentions are parsed per document (0045): queue one after each extraction; re-queuing it means re-parsing
        // Category words bind to classes (first slice of 0044 alignment): a library-level task, queued once per extracted document; re-queued when the ontology changes
        // The recomputation queued together with a short-phrase signature the person decided, in the same transaction as the decision (0051). Try-lock without waiting: if it cannot get the lock it
        // parks as `Deferred` and retries in ten seconds, so the pool is not held by a connection queuing for someone else; if it gets it, it is one complete run
        // recomputes by binding, reading the current binding rather than the payload at decision time
        utopia_store::phrase_bindings::MATERIALIZE_KIND => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            match utopia_store::materialize::try_materialize(&st.pool, kb_id).await? {
                Some(typed) => {
                    if typed.added > 0 || typed.merged > 0 || typed.retired > 0 {
                        let _ = utopia_store::audit::record(
                            &st.pool,
                            Some(kb_id),
                            Uuid::nil(),
                            "alignment.materialized",
                            "knowledge_base",
                            Some(kb_id),
                            serde_json::json!({
                                "job_id": job.id,
                                "added": typed.added,
                                "merged": typed.merged,
                                "retired": typed.retired,
                            }),
                        )
                        .await;
                        st.emit_graph(kb_id);
                    }
                    // Only new rows warrant a look from the correction agent; a recompute with no new rows is not queued
                    if typed.added > 0 {
                        utopia_store::jobs::enqueue_unless_queued(
                            &st.pool,
                            utopia_store::errata::JOB_KIND,
                            serde_json::json!({ "kb_id": kb_id }),
                        )
                        .await?;
                    }
                    // The queue card shows status by binding; only once the recompute is done is this decision "landed"
                    st.emit_review(kb_id);
                    Ok(())
                }
                None => Err(anyhow::anyhow!("typed projection busy").context(
                    utopia_core::Deferred::new(std::time::Duration::from_secs(10)),
                )),
            }
        }
        // The readings the approved entailment rule needs (0044 decision 3, fifth slice): ask the model, fill the cache, queue materialisation
        utopia_store::implication_rules::READ_KIND => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            implication::read_phrases(st, kb_id).await
        }
        // Correction agent (0044 decision 7): for documents whose materialisation produced new rows, re-review the typed graph per document
        utopia_store::errata::JOB_KIND => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            errata::review(st, kb_id).await
        }
        "align_types" => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            // The attempt count is carried by the job queued by itself; documents and class creation queue without this field
            let reask = job
                .payload
                .get("reask")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0);
            type_alignment::align_types_reasking(st, kb_id, reask).await
        }
        // Relation phrases bind to attributes by signature (second slice of 0044 alignment): queue one after category words finish binding; re-queue when attributes change
        "align_phrases" => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            // The attempt count is carried by the job queued by itself; the document, ontology and category-word alignment queues do not have this field
            let reask = job
                .payload
                .get("reask")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0);
            phrase_alignment::align_phrases_reasking(st, kb_id, reask).await
        }
        "resolve_time" => {
            let id = payload_document_id(&job.payload)?;
            time_resolution::resolve_document(st, id).await
        }
        "embed_names" => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            extraction_open::embed_names_job(st, kb_id).await
        }
        "resolve_types" => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            type_resolution::resolve_types_job(st, kb_id).await
        }
        "adjudicate_entities" => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            adjudication::adjudicate_entities(st, kb_id).await
        }
        "decay_sweep" => {
            let source_id: Uuid = job
                .payload
                .get("source_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing source_id"))?;
            decay_sweep::sweep_source(st, source_id).await
        }
        utopia_store::retention::JOB_KIND => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            let days = job
                .payload
                .get("days")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| anyhow::anyhow!("payload is missing days"))?;
            let vectors_cleared =
                utopia_store::retention::prune_superseded(&st.pool, kb_id, days).await?;
            let mut files_deleted = 0;
            let mut after = String::new();
            loop {
                let shas = utopia_store::retention::superseded_blobs(&st.pool, kb_id, days, &after)
                    .await?;
                let Some(last) = shas.last() else { break };
                after = last.clone();
                for sha in &shas {
                    st.blob.delete(sha).await?;
                }
                files_deleted += shas.len();
            }
            tracing::info!(
                %kb_id,
                days,
                vectors_cleared,
                files_deleted,
                "被取代的分块清理完成"
            );
            Ok(())
        }
        "govern" => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            governance::govern(st, kb_id).await
        }
        "propose_ontology" => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            // For measurement: when the payload's glossary = full, the full glossary is sent with each batch (bench/agent.mjs)
            let full = job.payload.get("glossary").and_then(|v| v.as_str()) == Some("full");
            ontology_agent::propose_with(st, kb_id, full).await
        }
        "propose_questions" => {
            let kb_id: Uuid = job
                .payload
                .get("kb_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing kb_id"))?;
            ontology_agent::propose_questions(st, kb_id)
                .await
                .map(|_| ())
        }
        "sync_source" => {
            let source_id: Uuid = job
                .payload
                .get("source_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing source_id"))?;
            ingest_sources::sync_source(st, source_id).await.map(|_| ())
        }
        "hydrate_rss_entry" => {
            let source_id: Uuid = job
                .payload
                .get("source_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing source_id"))?;
            let entry_id: Uuid = job
                .payload
                .get("rss_entry_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("payload is missing rss_entry_id"))?;
            rss_full_content::hydrate_entry(st, job.id, source_id, entry_id).await
        }
        other => anyhow::bail!("unknown task type: {other}"),
    }
}

/// The secret key under the data directory: read it if present; if absent, generate one and write it there (0600 on unix).
/// If what is read cannot be parsed, error and stop: silently generating a new key would make every sealed credential in the database unopenable
fn secret_key_file(data_dir: &std::path::Path) -> anyhow::Result<[u8; 32]> {
    let path = data_dir.join("secret.key");
    if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        return utopia_core::secrets::parse_key(&text).ok_or_else(|| {
            anyhow::anyhow!(
                "{} does not hold a 32-byte key (64 hex characters or base64)",
                path.display()
            )
        });
    }
    std::fs::create_dir_all(data_dir)?;
    let key = utopia_core::secrets::generate_key();
    std::fs::write(&path, utopia_core::secrets::key_to_hex(&key))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!(
        "生成了凭据封印钥匙: {}（备份数据目录时一起带走；UTOPIA_SECRET_KEY 可覆盖）",
        path.display()
    );
    Ok(key)
}
