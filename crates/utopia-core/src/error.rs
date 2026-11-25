#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("Not found")]
    NotFound,
    #[error("Not signed in or invalid credentials")]
    Unauthorized,
    #[error("You don't have permission to do that")]
    Forbidden,
    /// A 403 with a code: the kind of "no" a person can change (swap a token). Clients must be able to tell it apart,
    /// rather than merging it into the same sentence as "your role is not enough"
    #[error("{message}")]
    CodedForbidden { code: &'static str, message: String },
    #[error("{0}")]
    Conflict(String),
    /// Localizable conflict; legacy internal callers may still use Conflict.
    #[error("{message}")]
    CodedConflict { code: &'static str, message: String },
    #[error("{0}")]
    Validation(String),
    /// Validation error with a stable code. **The message stays the original English sentence.** It is for clients that do not localize
    /// (MCP, CLI) and for logs; the UI looks up the wording from the code in i18n.
    ///
    /// Localization happens at the client layer; the backend no longer owns the locale (see docs/decisions/0004),
    /// so strings kept here are permanently English. Anything a user can hit should carry a code.
    #[error("{message}")]
    Invalid {
        code: &'static str,
        message: String,
        /// Machine-supplied detail (such as cron parser errors). The wording belongs to the UI, the details belong here
        detail: Option<String>,
    },
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl AppError {
    pub fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        AppError::Invalid {
            code,
            message: message.into(),
            detail: None,
        }
    }
    pub fn invalid_detail(
        code: &'static str,
        message: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        AppError::Invalid {
            code,
            message: message.into(),
            detail: Some(detail.into()),
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;

/// Marks a failure that **retrying will not make better** (see issue #195).
///
/// The queue's default assumption is "wait a bit and it may get better", which is true for most failures: an endpoint blips,
/// the database is busy for a moment, a rate limit clears within a minute. Insufficient balance is not: three retries spaced 30 s, 2 min,
/// and 4.5 min apart over seven minutes, nobody tops up in that time. Retrying just repeats the same error three times, and ops needs
/// to see that "failure" now, not seven minutes later.
///
/// **The judgment stays on the handler side, not in the queue.** What counts as unrecoverable depends on the domain:
/// `utopia-store` cannot see `utopia-llm`'s error types and should not. The handler attaches this marker
/// (`err.context(Terminal)`), and the queue only asks whether it is attached.
///
/// Attaching it changes nothing else: alerts still fire (`observe_job_failure` also honors this marker,
/// otherwise failing faster would mean nobody is told), and `last_error` is still written.
#[derive(Debug, Clone, Copy)]
pub struct Terminal;

impl std::fmt::Display for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("will not recover by retrying")
    }
}

impl std::error::Error for Terminal {}

/// Whether this failure was marked as not to be retried.
///
/// **Ask `anyhow::Error::is`, do not walk `chain()` one by one.** When the marker is attached as context
/// (`e.context(Terminal)`, which `main.rs` does in two places), the concrete type of that link in the chain is
/// anyhow's internal `ContextError<Terminal, _>`, which `dyn Error::is::<Terminal>()` does not recognize,
/// so "don't back off unrecoverable failures" never took effect. `anyhow::Error::is` checks both the context and the wrapped error,
/// and sees through an outer `context(...)`, so it recognizes both attachment styles
pub fn is_terminal(err: &anyhow::Error) -> bool {
    err.is::<Terminal>()
}

/// The failure is not wrong; it should be **tried again later**. Put the task back to `queued` without burning budget (#526).
///
/// Paired with [`Terminal`]: one means "won't get better, don't retry", the other "can't do it now,
/// try again later". Both are domain judgments. The extractor knows the ontology vectors are not yet filled, and the queue cannot see
/// that graph. The handler attaches the marker (`err.context(Deferred::new(Duration::from_secs(30)))`),
/// and `mark_failed` honors it by pushing `run_at` into the future and rolling back `attempts`.
///
/// Difference from the default backoff: the default `30s × attempts²` is wrong. It delays failures where "retrying is worth it"
/// exponentially by attempt count, while the semantics of `Deferred` are "come back after this time point",
/// independent of the failure count. Re-queued twice for the same wait, the next `run_at` is the same offset each time,
/// with no growing 60s, 120s.
///
/// **When both `Terminal` and `Deferred` are attached, `Terminal` wins:** `mark_failed` asks [`is_terminal`] first.
/// Waiting also has a deadline. If still waiting more than `jobs::DEFER_WINDOW_SECS` after the first re-queue, it no longer counts as waiting,
/// and it backs off like a normal failure and burns budget, so an index that can never be completed does not keep the task queued forever.
#[derive(Debug, Clone, Copy)]
pub struct Deferred {
    pub retry_in: std::time::Duration,
}

impl Deferred {
    pub fn new(retry_in: std::time::Duration) -> Self {
        Self { retry_in }
    }
}

impl std::fmt::Display for Deferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // When this lands in `last_error` it must be directly readable, so the seconds are spelled out
        write!(f, "deferred; retry in {:?}", self.retry_in)
    }
}

impl std::error::Error for Deferred {}

/// How long a pending `Deferred` should wait. As with [`is_terminal`], it uses anyhow's own downcast,
/// recognizing it whether it is attached as context or as the root
pub fn is_deferred(err: &anyhow::Error) -> Option<std::time::Duration> {
    err.downcast_ref::<Deferred>().map(|d| d.retry_in)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use std::time::Duration;

    /// Marker as the root, with an explanatory message wrapped around it (the style of `rss_full_content.rs`)
    #[test]
    fn terminal_is_recognised_through_context() {
        let err: anyhow::Error = anyhow::Error::new(Terminal).context("balance gone: third retry");
        assert!(is_terminal(&err));
        assert!(is_deferred(&err).is_none());
    }

    #[test]
    fn deferred_is_recognised_through_context() {
        let err: anyhow::Error = anyhow::Error::new(Deferred::new(Duration::from_secs(30)))
            .context("waiting on ontology index");
        assert!(!is_terminal(&err));
        assert_eq!(is_deferred(&err), Some(Duration::from_secs(30)));
    }

    /// Marker attached as context (the style of `main.rs` and `extraction::run`), still recognized when wrapped in another layer
    #[test]
    fn marker_attached_via_context_method_works() {
        let marked = anyhow::Error::msg("waiting")
            .context(Deferred::new(Duration::from_secs(7)))
            .context("job 42");
        assert_eq!(is_deferred(&marked), Some(Duration::from_secs(7)));

        let marked_t = anyhow::Error::msg("balance gone")
            .context(Terminal)
            .context("job 42");
        assert!(is_terminal(&marked_t));
    }

    /// If both are attached, both are recognized; which one wins is decided by the order `mark_failed` asks (`Terminal` first)
    #[test]
    fn both_marks_are_seen() {
        let err = anyhow::Error::msg("balance gone after waiting")
            .context(Deferred::new(Duration::from_secs(10)))
            .context(Terminal);
        assert!(is_terminal(&err));
        assert_eq!(is_deferred(&err), Some(Duration::from_secs(10)));
    }

    /// An ordinary error should not be recognized as `Deferred` or `Terminal`
    #[test]
    fn plain_error_is_neither() {
        let err: anyhow::Error = anyhow::Error::msg("network blip");
        assert!(!is_terminal(&err));
        assert!(is_deferred(&err).is_none());
    }

    #[test]
    fn terminal_as_the_context() {
        let err = anyhow::anyhow!("model endpoint gone").context(Terminal);
        assert!(is_terminal(&err));
    }

    #[test]
    fn terminal_as_the_root() {
        let err = anyhow::Error::new(Terminal).context("source_mismatch");
        assert!(is_terminal(&err));
    }

    #[test]
    fn terminal_under_more_context() {
        let err = anyhow::anyhow!("balance gone")
            .context(Terminal)
            .context("job 42");
        assert!(is_terminal(&err));
        let io: Result<(), _> = Err(std::io::Error::other("io"));
        assert!(is_terminal(&io.context(Terminal).unwrap_err()));
    }

    #[test]
    fn a_plain_failure_is_not_terminal() {
        let err = anyhow::anyhow!("network blip").context("job 42");
        assert!(!is_terminal(&err));
    }
}
