//! Alert generation side (0005). Storage lives in `utopia_store::alerts`; this module only decides **when to report**.
//!
//! There is no "when to clear": one failure is one row, and it is never changed once written. See that module's docs.

use utopia_core::models::Role;
use utopia_store::alerts;

use crate::state::AppState;

/// How many days alerts are kept. **Pure atomicity has its cost right here:** a broken source syncs hourly,
/// writing 24 rows a day. Without cleanup this table would grow into a second log file.
///
/// 30 days is enough to answer "what happened last month"; anything older belongs in `audit_events` and the logs.
const RETAIN_DAYS: i32 = 30;

/// When a task fails, check whether the model endpoint is unavailable.
///
/// **Only report when the task finally gives up**: the attempts are used up, or this failure was marked as not worth retrying
/// ([`hopeless`], #195). Reporting the middle retries would report the same thing three times: retries exist so nobody gets bothered,
/// and reporting them would undo that. A task raises at most one alert,
/// so it needs no dedup state.
///
/// The criterion is the error's **type**, not its text: adding one context sentence anywhere in the call chain
/// changes the text. Classification itself lives in [`alert_for`].
pub async fn observe_job_failure(
    state: &AppState,
    job: &utopia_store::jobs::Job,
    err: &anyhow::Error,
) {
    // Tasks marked as not worth retrying: **this attempt is the last one**, so report now (#195).
    // Leaving out this line means failing faster but nobody being told, which is worse than not changing anything
    if job.attempts < job.max_attempts && !utopia_core::is_terminal(err) {
        return;
    }
    let Some((kind, severity)) = alert_for(err) else {
        return;
    };
    // System-level: it does not matter which library's task hit it; the endpoint and quota are shared by the whole deployment
    if let Err(e) = alerts::raise(
        &state.pool,
        alerts::NewAlert {
            kb_id: None,
            severity,
            kind,
            min_role: Role::Admin,
            subject_type: Some("system"),
            subject_id: None,
            detail: serde_json::json!({ "job": job.kind, "error": err.to_string() }),
        },
    )
    .await
    {
        tracing::warn!(error = %e, %kind, "failed to report alert");
        return;
    }
    state.emit_alert();
}

/// The parser read only part of the body: keep the usable prefix, but record the truncated range in the alert details.
/// Store the name too, so the alert stays readable after the document is deleted.
pub async fn observe_document_truncated(
    state: &AppState,
    kb_id: uuid::Uuid,
    document_id: uuid::Uuid,
    filename: &str,
    warnings: &[utopia_ingest::ParseWarning],
) {
    if warnings.is_empty() {
        return;
    }
    if let Err(e) = alerts::raise(
        &state.pool,
        alerts::NewAlert {
            kb_id: Some(kb_id),
            severity: "warning",
            kind: alerts::kind::DOCUMENT_CONTENTS_TRUNCATED,
            min_role: Role::Editor,
            subject_type: Some("document"),
            subject_id: Some(document_id),
            detail: document_truncation_detail(filename, warnings),
        },
    )
    .await
    {
        tracing::warn!(error = %e, "failed to report alert");
        return;
    }
    state.emit_alert();
}

fn document_truncation_detail(
    filename: &str,
    warnings: &[utopia_ingest::ParseWarning],
) -> serde_json::Value {
    serde_json::json!({
        "name": filename,
        "warnings": warnings
            .iter()
            .map(|warning| serde_json::json!({ "kind": warning.kind, "detail": warning.detail }))
            .collect::<Vec<_>>(),
    })
}

/// Reading a file's text needs a model that is not configured (0040): report one library-level alert, storing the name in it,
/// so the alert stays readable after the file is deleted
pub async fn observe_document_needs_reader(
    state: &AppState,
    kb_id: uuid::Uuid,
    document_id: uuid::Uuid,
    filename: &str,
    reader: utopia_ingest::Reader,
    error: &str,
) {
    if let Err(e) = alerts::raise(
        &state.pool,
        alerts::NewAlert {
            kb_id: Some(kb_id),
            severity: "warning",
            kind: alerts::kind::DOCUMENT_NEEDS_READER,
            min_role: Role::Editor,
            subject_type: Some("document"),
            subject_id: Some(document_id),
            detail: serde_json::json!({
                "name": filename,
                "reader": reader.as_str(),
                "error": error,
            }),
        },
    )
    .await
    {
        tracing::warn!(error = %e, "failed to report alert");
        return;
    }
    state.emit_alert();
}

/// Whether this failure is worth retrying.
///
/// **Only insufficient credit is unrecoverable.** Rate limits recover on their own, an unreachable endpoint may be a restarting service,
/// and other errors have no reason to decide life or death on the first try. Only the balance does not regrow within seven minutes (#195).
///
/// It uses the same source as [`alert_for`] (`utopia_llm::out_of_credit`, a type not text),
/// so "reported as error" and "not retried" are always the same class and never diverge.
pub fn hopeless(err: &anyhow::Error) -> bool {
    utopia_llm::out_of_credit(err).is_some()
}

/// Which class of alert a task failure raises, and whether to raise one at all.
///
/// **Extracted as a pure function so it can be tested:** `observe_job_failure` needs `AppState` and a real database,
/// but what needs guarding here is the classification itself. The three classes need completely different actions: insufficient credit needs a person to top up,
/// rate limiting needs lower concurrency or a higher quota, and an unreachable endpoint needs a network and address check. **Mixing them would point people at the wrong place, which wastes more time than not reporting.**
///
/// The order is "say the more urgent thing first"; the criteria do not overlap: the types are mutually exclusive, and the overlap is in status codes
/// (OpenAI balance exhaustion also returns 429), which is already separated in `utopia_llm::failure`.
fn alert_for(err: &anyhow::Error) -> Option<(&'static str, &'static str)> {
    // Insufficient credit comes before rate limiting. Their status codes overlap (OpenAI balance exhaustion also returns 429),
    // but classification is already done in `utopia_llm::failure`; the order here only puts "the thing to say first"
    // first: running out of money needs a person to act, while rate limiting does not.
    if utopia_llm::out_of_credit(err).is_some() {
        // `error`: it will not fix itself; until someone tops up, extraction stays stopped
        return Some((alerts::kind::LLM_OUT_OF_CREDIT, "error"));
    }
    if utopia_llm::rate_limited(err).is_some() {
        // `warning` is not `error`: the quota recovers on its own, an endpoint outage does not
        return Some((alerts::kind::LLM_RATE_LIMITED, "warning"));
    }
    if utopia_llm::is_unreachable(err) {
        return Some((alerts::kind::LLM_UNREACHABLE, "error"));
    }
    None
}

/// Clean up expired alerts once a day.
pub fn spawn_retention_sweep(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match alerts::purge_older_than(&state.pool, RETAIN_DAYS).await {
                Ok(n) if n > 0 => {
                    tracing::info!(count = n, days = RETAIN_DAYS, "clearing expired alerts");
                    state.emit_alert();
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "failed to clear expired alerts"),
            }
        }
    });
}

/// The source is mounted, but its table structure was never ingested.
///
/// **The report is about the state the failure left behind, not the failure itself.** Mounting has two steps: write `kb_data_sources`,
/// then ingest the schema-generated document into the store. When the first step succeeded and the second did not, the source is really mounted:
/// `query_data` gets queued, but the model finds no table structure and can only guess column names.
///
/// The error at mount time is seen only by the person who clicked the button. After that this store is silently missing data,
/// and that is exactly what 0009 says: what really hurts is not the failure, but that the failure is silent.
pub async fn observe_schema_sync_failure(
    state: &AppState,
    kb_id: uuid::Uuid,
    source_id: uuid::Uuid,
    source_name: &str,
    err: &anyhow::Error,
) {
    if let Err(e) = alerts::raise(
        &state.pool,
        alerts::NewAlert {
            kb_id: Some(kb_id),
            severity: "warning",
            kind: alerts::kind::SCHEMA_SYNC_FAILED,
            // Configuration class: the fix is the connection string or network, not the content. Find an admin
            min_role: Role::Admin,
            subject_type: Some("data_source"),
            subject_id: Some(source_id),
            // The name is stored here too: once the source is deleted, subject_id cannot be resolved to a name,
            // yet the alert should stay readable
            detail: serde_json::json!({
                "source": source_name,
                "error": err.to_string(),
            }),
        },
    )
    .await
    {
        tracing::warn!(error = %e, "failed to report data_source.schema_sync_failed");
        return;
    }
    state.emit_alert();
}

#[cfg(test)]
mod tests {
    use super::alert_for;
    use utopia_ingest::ParseWarning;
    use utopia_llm::{OutOfCredit, RateLimited};
    use utopia_store::alerts::kind;

    /// Rate limiting and an unreachable endpoint are two different things, so their alerts must be separate.
    ///
    /// Mixing them is not just "an ugly label": `llm.unreachable` says the endpoint cannot be reached,
    /// so an admin goes to check the network and address, while what is needed is lower concurrency or a higher quota. **The alert
    /// points people at the wrong place**, which wastes more time than not reporting.
    #[test]
    fn a_rate_limit_is_not_an_unreachable_endpoint() {
        let err = anyhow::Error::new(RateLimited {
            status: 429,
            retry_after: None,
            detail: "TPM limit reached".into(),
        });
        assert_eq!(alert_for(&err), Some((kind::LLM_RATE_LIMITED, "warning")));
    }

    /// **The check must go through the context layer.** The extraction path adds at least one sentence,
    /// "rate limit backoff still failed after 5 attempts", so a text-matching check is useless the same day.
    #[test]
    fn it_survives_context_layers() {
        let err = anyhow::Error::new(RateLimited {
            status: 429,
            retry_after: None,
            detail: "slow down".into(),
        })
        .context("rate-limit backoff still failing after 5 attempts")
        .context("extract_document failed");
        assert_eq!(alert_for(&err), Some((kind::LLM_RATE_LIMITED, "warning")));
    }

    /// Insufficient credit and rate limiting must be separate: one needs a person to top up, the other just waits a while.
    ///
    /// Misjudging the direction has a real cost: measured, 14 documents failed entirely due to insufficient credit,
    /// and the only way to learn the cause at the time was to look up `graph_error` in the database.
    #[test]
    fn out_of_credit_is_not_a_rate_limit() {
        let err = anyhow::Error::new(OutOfCredit {
            status: 402,
            detail: "Sorry, your account balance is insufficient".into(),
        });
        assert_eq!(alert_for(&err), Some((kind::LLM_OUT_OF_CREDIT, "error")));
    }

    /// The check must likewise go through the context layer.
    #[test]
    fn out_of_credit_survives_context_layers() {
        let err = anyhow::Error::new(OutOfCredit {
            status: 402,
            detail: "insufficient balance".into(),
        })
        .context("bootstrap_ontology failed");
        assert_eq!(alert_for(&err), Some((kind::LLM_OUT_OF_CREDIT, "error")));
    }

    /// Counter-case: an ordinary failure should not raise an alert, or every parse error would pop one up and people would learn to ignore it.
    #[test]
    fn an_ordinary_failure_raises_nothing() {
        let err = anyhow::anyhow!("结果解析失败").context("extraction failed");
        assert_eq!(alert_for(&err), None);
    }

    /// Counter-case: a clean 4xx is neither rate limiting nor unreachable. A wrong key needs replacing,
    /// which neither lowering concurrency nor checking the network would fix.
    #[test]
    fn an_auth_failure_raises_nothing() {
        let err = anyhow::anyhow!("LLM request failed (401 Unauthorized): bad key");
        assert_eq!(alert_for(&err), None);
    }

    /// **Balance does not come back within seven minutes on its own**, so those three backoff retries just repeat the same error
    /// three times and delay the failure that should be seen by seven minutes (#195).
    #[test]
    fn an_empty_balance_is_not_worth_retrying() {
        let err = anyhow::Error::new(OutOfCredit {
            status: 402,
            detail: "insufficient balance".into(),
        })
        .context("extraction failed");
        assert!(super::hopeless(&err));
    }

    /// Rate limiting is exactly what this backoff serves; the reason for separating the two classes (#176) lies here.
    #[test]
    fn a_rate_limit_still_gets_its_retries() {
        let err = anyhow::Error::new(RateLimited {
            status: 429,
            retry_after: Some(std::time::Duration::from_secs(30)),
            detail: "TPM limit reached".into(),
        });
        assert!(!super::hopeless(&err));
    }

    /// Counter-case: ordinary failures are still retried. A network blip should not decide life or death on one try.
    #[test]
    fn an_ordinary_failure_still_gets_its_retries() {
        assert!(!super::hopeless(&anyhow::anyhow!("failed to parse result")));
    }

    #[test]
    fn a_truncation_alert_says_what_was_left_out() {
        let warning = ParseWarning {
            kind: ParseWarning::CSV_RECORDS_TRUNCATED,
            detail: serde_json::json!({
                "records_read": 10_000,
                "records_total": 10_001,
                "records_omitted": 1,
            }),
        };

        assert_eq!(
            super::document_truncation_detail("customers.csv", &[warning]),
            serde_json::json!({
                "name": "customers.csv",
                "warnings": [{
                    "kind": "csv.records_truncated",
                    "detail": {
                            "records_read": 10_000,
                            "records_total": 10_001,
                            "records_omitted": 1,
                        },
                }],
            })
        );
    }
}
