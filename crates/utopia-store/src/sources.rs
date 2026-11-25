//! Ingest source repository: "a source is a folder" — a source is a container holding the documents it ingests, and can be synced on a schedule.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use utopia_core::models::{Role, Source, SourceKind, SourceView, SyncRun, SOURCE_SECRET_KEYS};
use utopia_core::{secrets, AppError, AppResult};
use uuid::Uuid;

use crate::decay::DecaySettings;

/// folder = plain container (files are uploaded/dragged in, no sync semantics); url/rss = pull-type; api = push-type.
/// Watching a local directory (watch_folder) was rejected — self-hosting users cannot see the server's disk;
/// object storage / WebDAV / Notion are its replacements (0013).
/// custom = custom puller: any URL that implements the Utopia ingest interface (returning items JSON) can be ingested on a schedule.
/// github_issues / gitlab_issues / jira_issues = tickets: one ticket, together with its status-change history, becomes one document.
///
/// The list of kinds is **not written here**: `SourceKind` (utopia-core) enumerates them all in one enum — the creation allowlist,
/// sync dispatch, and the front-end dropdown (a test checks them against each other). Previously a hand-written `KINDS` lived here, and five connectors
/// gained sync without being added to this table, so the UI let you pick them but creation failed (#247)
pub fn creatable_kinds() -> Vec<&'static str> {
    SourceKind::creatable().map(|k| k.as_str()).collect()
}

/// Validates and normalises a standard 5-field cron expression (internally the cron crate uses 6 fields: a seconds field is prepended).
pub fn validate_cron(expr: &str) -> AppResult<String> {
    let mut fields: Vec<String> = expr.split_whitespace().map(str::to_string).collect();
    if fields.len() != 5 {
        return Err(AppError::invalid_detail(
            "bad_cron_fields",
            "Cron expression must have 5 fields (minute hour day month weekday)",
            format!("got {}", fields.len()),
        ));
    }
    if let Some(names) = weekday_names(&fields[4]) {
        fields[4] = names;
    }
    let normalized = fields.join(" ");
    use std::str::FromStr;
    cron::Schedule::from_str(&format!("0 {normalized}")).map_err(|e| {
        AppError::invalid_detail("bad_cron", "Invalid cron expression", e.to_string())
    })?;
    Ok(normalized)
}

/// Numbers in the weekday field are read as standard cron (0–7, where 0 and 7 are both Sunday), and stored as names.
///
/// The cron crate's numeric weekdays follow the Quartz scheme, where 1 is Sunday: following the crontab.guru linked next to Advanced,
/// `1-5` is written, which syncs Sunday to Thursday, and `0` is rejected as out of range. Names are read the same way on both sides, so the stored
/// expression is unambiguous. An expression with no numbers, or one that cannot be read, is handed to the cron crate for validation as-is
fn weekday_names(field: &str) -> Option<String> {
    if !field.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut days = [false; 7];
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<usize>().ok().filter(|s| *s > 0)?),
            None => (part, 1),
        };
        let (from, to) = match range.split_once('-') {
            _ if range == "*" => (0, 6),
            Some((from, to)) => (from.parse().ok()?, to.parse().ok()?),
            None => {
                let day = range.parse().ok()?;
                (day, day)
            }
        };
        if from > to || to > 7 {
            return None;
        }
        for day in (from..=to).step_by(step) {
            days[day % 7] = true;
        }
    }
    let names = [1, 2, 3, 4, 5, 6, 0]
        .into_iter()
        .filter(|d| days[*d])
        .map(|d| ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][d]);
    Some(names.collect::<Vec<_>>().join(","))
}

pub const RSS_CONTENT_MODE_KEY: &str = "content_mode";
pub const RSS_FULL_CONTENT_MODE: &str = "full_new_items";

/// RSS mode is deliberately a small enum in the persisted source config. Old
/// rows without the key retain the legacy feed behavior.
pub fn rss_content_mode(config: &serde_json::Value) -> AppResult<&'static str> {
    let Some(value) = config.get(RSS_CONTENT_MODE_KEY) else {
        return Ok("feed");
    };
    let Some(value) = value.as_str() else {
        return Err(AppError::invalid(
            "rss_content_mode_invalid",
            "RSS content_mode must be feed or full_new_items",
        ));
    };
    match value {
        "feed" => Ok("feed"),
        RSS_FULL_CONTENT_MODE => Ok(RSS_FULL_CONTENT_MODE),
        _ => Err(AppError::invalid(
            "rss_content_mode_invalid",
            "RSS content_mode must be feed or full_new_items",
        )),
    }
}

pub fn rss_full_content_enabled(kind: &str, config: &serde_json::Value) -> AppResult<bool> {
    if SourceKind::parse(kind) != Some(SourceKind::Rss) {
        return Ok(false);
    }
    Ok(rss_content_mode(config)? == RSS_FULL_CONTENT_MODE)
}

/// The next trigger time of the cron (server local time zone).
fn cron_next_after(expr: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    use std::str::FromStr;
    // A source saved before names were stored still has numeric weekdays in its expression: these are likewise read by standard cron,
    // otherwise a source storing `1-5` would keep syncing Sunday to Thursday
    let mut fields: Vec<&str> = expr.split_whitespace().collect();
    let names = fields.get(4).copied().and_then(weekday_names);
    if let Some(names) = &names {
        fields[4] = names;
    }
    let schedule = cron::Schedule::from_str(&format!("0 {}", fields.join(" "))).ok()?;
    let local_after = after.with_timezone(&chrono::Local);
    schedule
        .after(&local_after)
        .next()
        .map(|t| t.with_timezone(&Utc))
}

pub async fn list(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<SourceView>> {
    // config strips credentials: the list is shown to Viewers, so no connector's secrets are ever sent down.
    // The keys live in one table, `SOURCE_SECRET_KEYS` — previously this only removed `auth_header`, and the secrets of five
    // connectors leaked out that way (#246)
    // The outer alias must not clash with the source alias in ENTRY_SELECT; the source and the current predicate must stay inside the projection,
    // otherwise PostgreSQL computes every observation in the deployment first and then discards the rows unrelated to the current source.
    let rows: Vec<SourceView> = sqlx::query_as(&format!(
        "SELECT listed_source.id, listed_source.kind, listed_source.name,
                listed_source.config - $2::text[] AS config, listed_source.icon,
                listed_source.sync_interval_minutes, listed_source.sync_cron, listed_source.decay,
                listed_source.last_sync_at, listed_source.last_sync_status,
                listed_source.last_sync_error, listed_source.last_sync_added,
                listed_source.sync_checkpoint,
                (SELECT count(*) FROM documents d
                 WHERE d.source_id = listed_source.id AND d.deleted_at IS NULL) AS doc_count,
                (SELECT count(*) FROM documents d
                 WHERE d.source_id = listed_source.id AND d.missing_since IS NOT NULL
                   AND d.deleted_at IS NULL) AS missing_count,
                -- 整块出去，不是八个平铺的列（0026 / #417）。**不是 RSS 的来源
                -- 这一格就是 NULL**：从前「不适用」在 state 上写作 NULL、在五个
                -- 计数上写作 0，同一件事两套说法，而 `queued_count: 0` 读起来
                -- 像「队列空着」。计数在块里仍然 COALESCE 成 0——这时候它真是
                -- 「这一档现在没有」，不是「不适用」。
                CASE WHEN listed_source.kind = 'rss' THEN jsonb_build_object(
                    'state', CASE
                        WHEN listed_source.config->>'content_mode' IS DISTINCT FROM 'full_new_items'
                            THEN 'disabled'
                        WHEN listed_source.rss_baselined_at IS NULL THEN 'pending'
                        ELSE 'active' END,
                    'pending', COALESCE(hydration.pending, 0),
                    'queued', COALESCE(hydration.queued, 0),
                    'retrying', COALESCE(hydration.retrying, 0),
                    'complete', COALESCE(hydration.complete, 0),
                    'terminal', COALESCE(hydration.terminal, 0)
                ) END AS rss_full_content
         FROM sources listed_source
         LEFT JOIN LATERAL (
             SELECT
                 (count(*) FILTER (WHERE projected.state = 'baseline'))::int AS baseline_count,
                 count(*) FILTER (WHERE projected.state = 'pending') AS pending,
                 count(*) FILTER (WHERE projected.state IN ('queued', 'hydrating')) AS queued,
                 count(*) FILTER (WHERE projected.state = 'retry_wait') AS retrying,
                 count(*) FILTER (WHERE projected.state = 'complete') AS complete,
                 count(*) FILTER (
                     WHERE projected.state IN ('terminal', 'deleted', 'superseded')
                 ) AS terminal
             FROM (
                 {}
                 WHERE listed_source.kind = 'rss'
                   AND e.source_id = listed_source.id
                   AND e.activation_generation = listed_source.rss_generation
             ) projected
         ) hydration ON listed_source.kind = 'rss'
         WHERE listed_source.kb_id = $1 ORDER BY listed_source.created_at",
        crate::rss_full_content::ENTRY_SELECT
    ))
    .bind(kb_id)
    .bind(SOURCE_SECRET_KEYS)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Opened on the way out of the store: credential keys and push secrets in the config are sealed in the DB (`utopia_core::secrets`).
/// Every query that returns a `Source` goes through here — `list` does not, it strips the credential keys in SQL.
fn opened(mut s: Source) -> AppResult<Source> {
    secrets::open_json_keys(&mut s.config, SOURCE_SECRET_KEYS).map_err(AppError::Other)?;
    s.ingest_token = secrets::open_opt(s.ingest_token.as_deref()).map_err(AppError::Other)?;
    Ok(s)
}

pub async fn get(pool: &PgPool, id: Uuid) -> AppResult<Source> {
    let row: Option<Source> = sqlx::query_as("SELECT * FROM sources WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    opened(row.ok_or(AppError::NotFound)?)
}

#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &PgPool,
    kb_id: Uuid,
    kind: &str,
    name: &str,
    config: &serde_json::Value,
    icon: Option<&str>,
    sync_interval_minutes: Option<i32>,
    sync_cron: Option<&str>,
) -> AppResult<Source> {
    if !SourceKind::parse(kind).is_some_and(|k| k.creatable_by_hand()) {
        return Err(AppError::Validation(format!(
            "kind must be one of: {}",
            creatable_kinds().join(", ")
        )));
    }
    if name.trim().is_empty() {
        return Err(AppError::invalid(
            "source_name_required",
            "Source name is required",
        ));
    }
    // Mutually exclusive: cron takes precedence (the UI only ever sends one of them)
    let cron_norm = sync_cron.map(validate_cron).transpose()?;
    let interval = if cron_norm.is_some() {
        None
    } else {
        sync_interval_minutes
    };
    // serde's default Value::Null lands in the DB as jsonb null, and the front end reading config.x blows up — normalise it to an empty object
    let mut config = if config.is_null() {
        serde_json::json!({})
    } else {
        config.clone()
    };
    secrets::seal_json_keys(&mut config, SOURCE_SECRET_KEYS);
    let full_content = rss_full_content_enabled(kind, &config)?;
    let source_id = Uuid::now_v7();
    let mut tx = pool.begin().await?;
    let source: Source = sqlx::query_as(
        "INSERT INTO sources (id, kb_id, kind, name, config, icon, sync_interval_minutes, sync_cron)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *",
    )
    .bind(source_id)
    .bind(kb_id)
    .bind(kind)
    .bind(name.trim())
    .bind(config)
    .bind(icon)
    .bind(interval)
    .bind(cron_norm)
    .fetch_one(&mut *tx)
    .await?;
    if full_content {
        crate::rss_full_content::initialize_source(&mut tx, source.id).await?;
    }
    tx.commit().await?;
    opened(source)
}

/// Sets the push secret for an api source (on creation / rotation).
pub async fn set_ingest_token(pool: &PgPool, source_id: Uuid, token: &str) -> AppResult<()> {
    let res = sqlx::query("UPDATE sources SET ingest_token = $2 WHERE id = $1")
        .bind(source_id)
        .bind(secrets::seal(token))
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Updating the schedule: interval and cron are mutually exclusive, and whichever is explicitly set overrides both.
fn rss_feed_url(config: &serde_json::Value) -> Option<&str> {
    config
        .get("feed_url")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[allow(clippy::too_many_arguments)]
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    name: Option<&str>,
    config: Option<&serde_json::Value>,
    icon: Option<&str>,
    schedule: Option<(Option<i32>, Option<String>)>,
    decay: Option<Option<&DecaySettings>>,
) -> AppResult<Source> {
    let decay = decay
        .map(|settings| {
            settings
                .map(|settings| {
                    settings.validate()?;
                    serde_json::to_value(settings).map_err(|e| AppError::Other(e.into()))
                })
                .transpose()
        })
        .transpose()?;
    let schedule = match schedule {
        Some((interval, cron)) => {
            let cron_norm = cron.as_deref().map(validate_cron).transpose()?;
            let interval = if cron_norm.is_some() { None } else { interval };
            Some((interval, cron_norm))
        }
        None => None,
    };

    let mut tx = pool.begin().await?;
    let previous: Source = sqlx::query_as("SELECT * FROM sources WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut next_config = config
        .map(|value| {
            if value.is_null() {
                serde_json::json!({})
            } else {
                value.clone()
            }
        })
        .unwrap_or_else(|| previous.config.clone());
    secrets::seal_json_keys(&mut next_config, SOURCE_SECRET_KEYS);
    let old_full = rss_full_content_enabled(&previous.kind, &previous.config)?;
    let new_full = rss_full_content_enabled(&previous.kind, &next_config)?;
    let feed_url_changed = previous.kind == "rss"
        && old_full
        && new_full
        && rss_feed_url(&previous.config) != rss_feed_url(&next_config);
    // gitlab_issues changed project, or merge requests were turned on: the tickets from before the checkpoint were never read, while the incremental query only asks
    // for those after it. Clear it, so the next run reads from the start; unchanged tickets are not recorded as new versions thanks to deduplication (0072). Turning off merge requests does not need a re-read
    let merge_requests =
        |config: &serde_json::Value| config["include_merge_requests"].as_bool().unwrap_or(false);
    let reread = previous.kind == "gitlab_issues"
        && (previous.config["project"].as_str().map(str::trim)
            != next_config["project"].as_str().map(str::trim)
            || (!merge_requests(&previous.config) && merge_requests(&next_config)));
    let source: Source = sqlx::query_as(
        "UPDATE sources SET
            name = COALESCE($2, name),
            config = COALESCE($3, config),
            icon = COALESCE($4, icon),
            sync_interval_minutes = CASE WHEN $5 THEN $6 ELSE sync_interval_minutes END,
            sync_cron = CASE WHEN $5 THEN $7 ELSE sync_cron END,
            decay = CASE WHEN $8 THEN $9 ELSE decay END,
            sync_checkpoint = CASE WHEN $10 THEN NULL ELSE sync_checkpoint END
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(Some(next_config))
    .bind(icon)
    .bind(schedule.is_some())
    .bind(schedule.as_ref().and_then(|(i, _)| *i))
    .bind(schedule.as_ref().and_then(|(_, c)| c.clone()))
    .bind(decay.is_some())
    .bind(decay.flatten())
    .bind(reread)
    .fetch_one(&mut *tx)
    .await?;
    if reread {
        // The run in progress was listed against the old config, so the checkpoint it is about to write does not count. Even when the checkpoint was already empty
        // (the first run), the clearing above cannot stop it: empty against empty, and `finish_timestamp_sync` still commits. Recording that run
        // as failed — it only accepts runs in the running state — rolls the checkpoint back along with it
        sqlx::query(
            "UPDATE source_sync_runs SET finished_at = now(), status = 'failed', error = $2
             WHERE source_id = $1 AND status = 'running'",
        )
        .bind(id)
        .bind("Source configuration changed during sync")
        .execute(&mut *tx)
        .await?;
    }

    if (!old_full && new_full) || feed_url_changed {
        crate::rss_full_content::enable_source(&mut tx, id).await?;
    }
    if !previous.names_are_identities() && source.names_are_identities() {
        crate::name_vectors::drop_for_source(&mut tx, id).await?;
    }
    tx.commit().await?;
    opened(source)
}

/// Only touches a few keys in the config: those in `set` are written in (secrets are still sealed), those in `remove` are deleted, and other keys are left as-is.
/// Completes in one statement, with no read-then-write — when two requests change different keys at once, neither overwrites the other's fresh write
pub async fn patch_config(
    pool: &PgPool,
    id: Uuid,
    set: &serde_json::Value,
    remove: &[&str],
) -> AppResult<Source> {
    let mut set = set.clone();
    secrets::seal_json_keys(&mut set, SOURCE_SECRET_KEYS);
    let source = sqlx::query_as(
        "UPDATE sources SET config =
            (CASE WHEN jsonb_typeof(config) = 'object' THEN config ELSE '{}'::jsonb END - $3::text[]) || $2
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(set)
    .bind(remove)
    .fetch_optional(pool)
    .await?
    .ok_or(AppError::NotFound)?;
    opened(source)
}

/// Deletes a source; its documents are kept (source_id set to NULL, falling back to the Uploads group).
pub async fn delete(pool: &PgPool, id: Uuid) -> AppResult<()> {
    let res = sqlx::query("DELETE FROM sources WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Sources due for a sync (the scheduler scans every minute).
/// interval-type is decided in SQL; cron-type is fetched and the next trigger time computed on the Rust side, then filtered.
pub async fn due_sources(pool: &PgPool) -> AppResult<Vec<Source>> {
    let rows: Vec<Source> = sqlx::query_as(
        "SELECT * FROM sources
         WHERE last_sync_status NOT IN ('queued', 'running')
           AND ((sync_interval_minutes IS NOT NULL
                 AND (last_sync_at IS NULL
                      OR last_sync_at + make_interval(mins => sync_interval_minutes) <= now()))
                OR sync_cron IS NOT NULL)",
    )
    .fetch_all(pool)
    .await?;

    let now = chrono::Utc::now();
    let rows = rows
        .into_iter()
        .map(opened)
        .collect::<AppResult<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|s| match &s.sync_cron {
            None => true, // interval type is already decided in SQL
            Some(expr) => {
                // The baseline is the last sync time (or the creation time if it has never synced): missed trigger points are caught up on the next scan
                let anchor = s.last_sync_at.unwrap_or(s.created_at);
                cron_next_after(expr, anchor).is_some_and(|next| next <= now)
            }
        })
        .collect())
}

/// Status, task and worker notification are committed together; if it is already queued or running, returns false.
pub async fn enqueue_sync(pool: &PgPool, id: Uuid) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    let queued = mark_queued_tx(&mut tx, id).await?;
    if queued {
        crate::jobs::enqueue_with_max_attempts_tx(
            &mut tx,
            "sync_source",
            serde_json::json!({ "source_id": id }),
            3,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(queued)
}

/// Changing the Drive folder re-queues a read to pick up the new config, even if the old sync is still running.
/// The caller saves the config first; here we only guarantee that the status and the follow-up task are committed together.
pub async fn enqueue_sync_after_config_change(pool: &PgPool, id: Uuid) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    mark_queued_tx(&mut tx, id).await?;
    crate::jobs::enqueue_with_max_attempts_tx(
        &mut tx,
        "sync_source",
        serde_json::json!({ "source_id": id }),
        3,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn mark_queued_tx(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> AppResult<bool> {
    let res = sqlx::query(
        "UPDATE sources SET last_sync_status = 'queued'
         WHERE id = $1 AND last_sync_status NOT IN ('queued', 'running')",
    )
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn mark_running(pool: &PgPool, id: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE sources SET last_sync_status = 'running' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Wraps up a sync. On failure it records one alert; on success it does nothing —
/// **"Is it fine now?" is not a question the alert centre should answer**, and the source page already says so.
///
/// The return value is "was it recorded", and the caller uses that to decide whether to emit the event.
pub async fn finish_sync(
    pool: &PgPool,
    id: Uuid,
    error: Option<&str>,
    added: i32,
) -> AppResult<bool> {
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "UPDATE sources SET last_sync_status = $2, last_sync_error = $3,
                last_sync_added = $4, last_sync_at = now()
         WHERE id = $1
         RETURNING kb_id, name",
    )
    .bind(id)
    .bind(if error.is_some() { "failed" } else { "ok" })
    .bind(error)
    .bind(added)
    .fetch_optional(pool)
    .await?;
    let Some((kb_id, name)) = row else {
        return Ok(false);
    };
    let Some(msg) = error else {
        return Ok(false);
    };
    crate::alerts::raise(
        pool,
        crate::alerts::NewAlert {
            kb_id: Some(kb_id),
            severity: "error",
            kind: crate::alerts::kind::SOURCE_SYNC_FAILED,
            // The content category goes to editors, not only admins: an admin needs to know the connection must be fixed,
            // but **the person who configured this source** needs more to know that your things did not get in
            min_role: Role::Editor,
            subject_type: Some("source"),
            subject_id: Some(id),
            // The name is stored too: once the source is deleted, subject_id cannot resolve a name, and the alert should still be able to show it
            detail: serde_json::json!({ "name": name, "error": msg }),
        },
    )
    .await?;
    Ok(true)
}

/// Records the sync time (to stop the scheduler re-triggering during a long sync and then immediately seeing it as due again).
pub async fn touch_sync_time(pool: &PgPool, id: Uuid, at: DateTime<Utc>) -> AppResult<()> {
    sqlx::query("UPDATE sources SET last_sync_at = $2 WHERE id = $1")
        .bind(id)
        .bind(at)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sync run records (the channel audit history)
// ---------------------------------------------------------------------------

/// Ticket-source progress is independent of the audit history, which keeps only 50 entries; NULL must also not fall back to an old ok row.
pub async fn sync_checkpoint(pool: &PgPool, source_id: Uuid) -> AppResult<Option<DateTime<Utc>>> {
    sqlx::query_scalar("SELECT sync_checkpoint FROM sources WHERE id = $1")
        .bind(source_id)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound)
}

/// is recorded as failed, **but no alert is sent**.
///
/// Hitting the cap in one run is not a broken connection: it has not finished reading, and the next run picks up from the cursor (0072).
/// `finish_sync` ties "record the status" to "send the alert", so every time the cap is hit an
/// source.sync_failed alert is sent — the same source reports the same thing every day (#1212).
pub async fn finish_sync_quiet(
    pool: &PgPool,
    id: Uuid,
    error: &str,
    added: i32,
) -> AppResult<bool> {
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "UPDATE sources SET last_sync_status = 'failed', last_sync_error = $2,
                last_sync_added = $3, last_sync_at = now()
         WHERE id = $1 RETURNING kb_id, name",
    )
    .bind(id)
    .bind(error)
    .bind(added)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// A full timestamp group's progress is committed together with the success record; if any write fails, the old cursor is kept.
///
/// **The two kinds of source use the same column, but the value they store differs**:
///
/// - Ticket sources (GitHub / Jira): the cursor is **the update time consumed up to**, and the whole group at that moment has been read
///   (ADR 0072). It is the lower bound for the next run.
/// - custom: the endpoint returns the whole window in one go, and the cursor is **the time this run started** — advance it to that rather than
///   to now, so changes arriving during the run stay in the next run's window (see the Custom section of ADR 0072).
///
/// For both, the read endpoints mean the same thing: **the lower bound of the next run**.
pub async fn finish_timestamp_sync(
    pool: &PgPool,
    run_id: Uuid,
    source_id: Uuid,
    previous_checkpoint: Option<DateTime<Utc>>,
    checkpoint: Option<DateTime<Utc>>,
    created_docs: i32,
    updated_docs: i32,
) -> AppResult<()> {
    if checkpoint < previous_checkpoint {
        return Err(AppError::Validation(
            "Source checkpoint cannot move backwards".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    let source = sqlx::query(
        "UPDATE sources SET sync_checkpoint = $3, last_sync_status = 'ok',
                last_sync_error = NULL, last_sync_added = $4, last_sync_at = now()
         WHERE id = $1 AND kind IN ('github_issues', 'gitlab_issues', 'jira_issues', 'custom')
           AND sync_checkpoint IS NOT DISTINCT FROM $2::timestamptz",
    )
    .bind(source_id)
    .bind(previous_checkpoint)
    .bind(checkpoint)
    .bind(created_docs + updated_docs)
    .execute(&mut *tx)
    .await?;
    if source.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "Source checkpoint changed during sync".into(),
        ));
    }
    let run = sqlx::query(
        "UPDATE source_sync_runs SET finished_at = now(), status = 'ok', error = NULL,
                created_docs = $3, updated_docs = $4
         WHERE id = $1 AND source_id = $2 AND status = 'running'",
    )
    .bind(run_id)
    .bind(source_id)
    .bind(created_docs)
    .bind(updated_docs)
    .execute(&mut *tx)
    .await?;
    if run.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "Source sync run is no longer running".into(),
        ));
    }
    sqlx::query(
        "DELETE FROM source_sync_runs WHERE source_id = $1 AND id NOT IN
         (SELECT id FROM source_sync_runs WHERE source_id = $1
          ORDER BY started_at DESC LIMIT 50)",
    )
    .bind(source_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn start_run(pool: &PgPool, source_id: Uuid) -> AppResult<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO source_sync_runs (id, source_id) VALUES ($1, $2)")
        .bind(id)
        .bind(source_id)
        .execute(pool)
        .await?;
    Ok(id)
}

pub async fn finish_run(
    pool: &PgPool,
    run_id: Uuid,
    source_id: Uuid,
    error: Option<&str>,
    created_docs: i32,
    updated_docs: i32,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE source_sync_runs SET finished_at = now(), status = $2, error = $3,
                created_docs = $4, updated_docs = $5
         WHERE id = $1",
    )
    .bind(run_id)
    .bind(if error.is_some() { "failed" } else { "ok" })
    .bind(error)
    .bind(created_docs)
    .bind(updated_docs)
    .execute(pool)
    .await?;
    // Keep only the latest 50 per source
    sqlx::query(
        "DELETE FROM source_sync_runs WHERE source_id = $1 AND id NOT IN
         (SELECT id FROM source_sync_runs WHERE source_id = $1
          ORDER BY started_at DESC LIMIT 50)",
    )
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_runs(pool: &PgPool, source_id: Uuid, limit: i64) -> AppResult<Vec<SyncRun>> {
    let rows: Vec<SyncRun> = sqlx::query_as(
        "SELECT id, started_at, finished_at, status, created_docs, updated_docs, error
         FROM source_sync_runs WHERE source_id = $1
         ORDER BY started_at DESC LIMIT $2",
    )
    .bind(source_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_validation() {
        assert_eq!(validate_cron("30 9 * * *").unwrap(), "30 9 * * *");
        assert_eq!(
            validate_cron("  0  9 * * Mon,Thu ").unwrap(),
            "0 9 * * Mon,Thu"
        );
        assert!(validate_cron("9 * * *").is_err()); // 4 fields
        assert!(validate_cron("99 9 * * *").is_err()); // minute out of range
        assert!(validate_cron("0 0 0 0 0 0").is_err()); // 6 fields
    }

    #[test]
    fn cron_weekday_numbers_read_as_standard_cron() {
        // crontab.guru's 1-5 is Monday to Friday, but the cron crate's numeric weekdays count from Sunday = 1
        assert_eq!(
            validate_cron("0 9 * * 1-5").unwrap(),
            "0 9 * * Mon,Tue,Wed,Thu,Fri"
        );
        assert_eq!(validate_cron("0 9 * * 0").unwrap(), "0 9 * * Sun");
        assert_eq!(validate_cron("0 9 * * 5-7").unwrap(), "0 9 * * Fri,Sat,Sun");
    }

    #[test]
    fn a_source_saved_with_weekday_numbers_runs_on_standard_cron_days() {
        use chrono::TimeZone;
        // 2026-10-31 is a Saturday; the next `1-5` is Monday, not Sunday
        let saturday = chrono::Local
            .with_ymd_and_hms(2026, 10, 31, 12, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        let monday = chrono::Local
            .with_ymd_and_hms(2026, 11, 2, 9, 0, 0)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(cron_next_after("0 9 * * 1-5", saturday), Some(monday));
    }

    #[test]
    fn cron_next_computes() {
        let after = chrono::Utc::now();
        let next = cron_next_after("*/5 * * * *", after).unwrap();
        assert!(next > after);
        assert!((next - after).num_minutes() <= 5);
    }

    #[test]
    fn rss_content_mode_defaults_and_validates() {
        assert_eq!(rss_content_mode(&serde_json::json!({})).unwrap(), "feed");
        assert_eq!(
            rss_content_mode(&serde_json::json!({ "content_mode": "feed" })).unwrap(),
            "feed"
        );
        assert_eq!(
            rss_content_mode(&serde_json::json!({ "content_mode": "full_new_items" })).unwrap(),
            "full_new_items"
        );
        assert!(rss_content_mode(&serde_json::json!({ "content_mode": "all" })).is_err());
        assert!(rss_content_mode(&serde_json::json!({ "content_mode": true })).is_err());
        assert!(!rss_full_content_enabled(
            "url",
            &serde_json::json!({
                "content_mode": "full_new_items"
            })
        )
        .unwrap());
    }
}
