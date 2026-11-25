use figment::{
    providers::{Env, Serialized},
    Figment,
};
use serde::{Deserialize, Serialize};

/// Global configuration. Precedence: environment variables (prefix `UTOPIA_`) > defaults.
/// The `.env` file is preloaded into the environment by the binary entry point via dotenvy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub database_url: String,
    /// Connection string for running migrations. Migrations create tables and triggers, which the runtime does not need permissions for, so splitting them off
    /// lets the app connect with a restricted role that can read and write business tables but only appends to the ledger, never updates it.
    /// Unset falls back to `database_url`, so existing deployments upgrade without changes.
    pub migration_url: Option<String>,
    pub bind_addr: String,
    /// JWT signing key. If empty, a key is generated on first start and stored in deployment_settings.
    /// Requiring the deployer to fill in a random string means in practice the default value ends up in production as is.
    /// When given explicitly it takes precedence over the one stored in the database: this is the path for key rotation and explicit alignment across instances.
    pub jwt_secret: Option<String>,
    /// Credential sealing key (32 bytes, 64 hex chars or base64). If empty, the `secret.key` in the data directory is used,
    /// generated on first start. **The key is not stored in the database:** a database leak is not a credential leak, which is the whole point of encryption at rest;
    /// when backing up the data directory, take it along. Without it the credentials in the database cannot be read.
    pub secret_key: Option<String>,
    /// Frontend build output directory; when present the server serves the SPA (history fallback).
    pub web_dist: String,
    /// Data directory: raw files (files/) and the Tantivy index (index/).
    pub data_dir: String,
    /// Token budget per chunk (cl100k tokens). It is a knob rather than a constant, because it has to be measurable:
    /// structure (headers follow along) and size are two different things, and the recall bench must be able to change only one. `UTOPIA_CHUNK_TOKENS`
    pub chunk_tokens: usize,
    /// Whether the extraction prompt includes "entities already recognized by earlier chunks of this document" (`known`). On by default.
    /// Also a knob for measuring (#588): after chunks call the model concurrently, later chunks cannot see what earlier chunks recognized;
    /// turning it off and running a recall bench round gives the output for that case. It only changes the prompt; when writing to the database, entities are still matched to earlier chunks by name.
    /// `UTOPIA_EXTRACT_KNOWN_IN_PROMPT`
    pub extract_known_in_prompt: bool,
    /// Database connection pool cap. Default 32, aligned with the default worker concurrency: when the pool is smaller than concurrency,
    /// the symptom is slower requests, not any single place saying "pool too small", so it must be tunable.
    pub db_max_connections: Option<u32>,
    /// Forces the Secure flag on the session cookie. Default false: it is decided by the request's X-Forwarded-Proto header,
    /// set only when over TLS. Only needs to be forced on here when the proxy does not send that header.
    pub cookie_secure: bool,
    /// Whether registration is open. When false, only the first user (bootstrap deployment) can register; others need an admin to open it.
    pub open_registration: bool,
    /// Whether password login and registration are allowed. When off, only single sign-on is used (together with
    /// `UTOPIA_OIDC_ALLOWED_DOMAINS` for auto-provisioning); when on, it is a backdoor for when the identity provider goes down.
    /// `UTOPIA_PASSWORD_LOGIN`
    pub password_login: bool,
    /// Superseded chunks keep their vectors for a few days. Default unset: vectors are kept forever (0019 open question ②), so as-of
    /// vector search can look back as far as needed. When set, it is cleaned once a day; rows and bodies stay. Value must be between 1 and 3650
    /// (`retention::window`). `UTOPIA_SUPERSEDED_VECTOR_DAYS`
    pub superseded_vector_days: Option<i64>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            database_url: "postgres://utopia:utopia@localhost:1517/utopia".into(),
            migration_url: None,
            bind_addr: "0.0.0.0:1516".into(),
            jwt_secret: None,
            secret_key: None,
            web_dist: "web/dist".into(),
            data_dir: "data".into(),
            chunk_tokens: 300,
            extract_known_in_prompt: true,
            db_max_connections: None,
            cookie_secure: false,
            open_registration: true,
            password_login: true,
            superseded_vector_days: None,
        }
    }
}

impl AppConfig {
    pub fn load() -> anyhow::Result<Self> {
        let cfg = Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(blank_is_unset(Env::prefixed("UTOPIA_")))
            .extract()?;
        Ok(cfg)
    }
}

/// Environment variables with an empty value are treated as **unset** (#343).
///
/// In container orchestration, a form like `UTOPIA_X: ${UTOPIA_X:-}` passes an empty string into the container when the variable is unset,
/// not "not passed". Figment accepts it as is, so `Option<String>` gets `Some("")`, `String`
/// fields get their default overwritten by the empty string, and `bool` and numeric fields fail even to deserialize.
///
/// The check lives at the env-variable read layer, not at each use site: an empty string is not a valid value for any of these fields.
/// Guarding field by field only misses the next field added — `jwt_secret` and `secret_key` each guard themselves,
/// and `migration_url` is the one that was missed. The symptom is the empty string handed to sqlx, which reports "relative URL without a
/// base", and a deployment started by following the README quick start exits at the first step. `init-app-role.sh` already
/// checks this with `[ -z ]`; the Rust side was missing it.
fn blank_is_unset(env: Env) -> Env {
    // iter() gives keys with the prefix stripped and lowercased; filter gets the key with the prefix stripped but not yet lowercased,
    // so this compares case-insensitively.
    let blank: Vec<String> = env
        .iter()
        .filter(|(_, value)| value.trim().is_empty())
        .map(|(key, _)| key.to_string())
        .collect();
    env.filter(move |key| {
        !blank
            .iter()
            .any(|blank_key| key.as_str().eq_ignore_ascii_case(blank_key))
    })
}

impl AppConfig {
    /// Migration connection string: when not configured separately, the runtime one is used.
    pub fn migration_url(&self) -> &str {
        self.migration_url.as_deref().unwrap_or(&self.database_url)
    }
}

#[cfg(test)]
// The Jail closure must return figment::Result; the size of that Err variant is not ours to decide.
#[allow(clippy::result_large_err)]
mod tests {
    use super::AppConfig;
    use figment::Jail;

    /// #343: when `UTOPIA_MIGRATION_URL: ${UTOPIA_MIGRATION_URL:-}` in `docker-compose.yml`
    /// is unset, it passes an **empty string** into the container, not "not passed". After the empty string overrides the fallback,
    /// `migration_url()` returns "", sqlx reports "relative URL without a base",
    /// and a deployment started by following the README quick start exits at the first step.
    #[test]
    fn a_blank_migration_url_falls_back_to_the_database_url() {
        Jail::expect_with(|jail| {
            jail.set_env("UTOPIA_DATABASE_URL", "postgres://u:p@db:5432/utopia");
            jail.set_env("UTOPIA_MIGRATION_URL", "");
            let cfg = AppConfig::load().unwrap();
            assert_eq!(cfg.migration_url, None, "空串要当作没设");
            assert_eq!(cfg.migration_url(), cfg.database_url);
            Ok(())
        });
    }

    /// Whitespace only also counts as empty: `UTOPIA_MIGRATION_URL=" "` is likewise not a connection string.
    #[test]
    fn whitespace_counts_as_blank() {
        Jail::expect_with(|jail| {
            jail.set_env("UTOPIA_DATABASE_URL", "postgres://u:p@db:5432/utopia");
            jail.set_env("UTOPIA_MIGRATION_URL", "   ");
            let cfg = AppConfig::load().unwrap();
            assert_eq!(cfg.migration_url, None);
            Ok(())
        });
    }

    /// When a real value is given it still takes effect as before. This one guards against the filter above overreaching.
    #[test]
    fn a_migration_url_that_is_set_still_wins() {
        Jail::expect_with(|jail| {
            jail.set_env("UTOPIA_DATABASE_URL", "postgres://app:p@db:5432/utopia");
            jail.set_env("UTOPIA_MIGRATION_URL", "postgres://owner:p@db:5432/utopia");
            let cfg = AppConfig::load().unwrap();
            assert_eq!(
                cfg.migration_url.as_deref(),
                Some("postgres://owner:p@db:5432/utopia")
            );
            assert_ne!(cfg.migration_url(), cfg.database_url);
            Ok(())
        });
    }

    /// Empty strings do more than trip `Option<String>`: a `String` field has no fallback, so the empty string overwrites the default.
    #[test]
    fn a_blank_string_field_keeps_its_default() {
        Jail::expect_with(|jail| {
            jail.set_env("UTOPIA_DATA_DIR", "");
            jail.set_env("UTOPIA_WEB_DIST", "");
            jail.set_env("UTOPIA_BIND_ADDR", "");
            let cfg = AppConfig::load().unwrap();
            assert_eq!(cfg.data_dir, "data");
            assert_eq!(cfg.chunk_tokens, 300);
            assert_eq!(cfg.web_dist, "web/dist");
            assert_eq!(cfg.bind_addr, "0.0.0.0:1516");
            Ok(())
        });
    }

    /// Non-string fields fail even earlier: an empty string fails deserialization, and `load()` returns an error directly,
    /// reporting a figment type error, which does not show which environment variable was set to empty.
    #[test]
    fn a_blank_typed_field_does_not_break_loading() {
        Jail::expect_with(|jail| {
            jail.set_env("UTOPIA_DB_MAX_CONNECTIONS", "");
            jail.set_env("UTOPIA_COOKIE_SECURE", "");
            jail.set_env("UTOPIA_SUPERSEDED_VECTOR_DAYS", "");
            let cfg = AppConfig::load().unwrap();
            assert_eq!(cfg.db_max_connections, None);
            assert!(!cfg.cookie_secure);
            assert_eq!(cfg.superseded_vector_days, None);
            Ok(())
        });
    }

    /// The converse of the above: when a value is given, it still parses.
    #[test]
    fn a_typed_field_that_is_set_still_parses() {
        Jail::expect_with(|jail| {
            jail.set_env("UTOPIA_DB_MAX_CONNECTIONS", "8");
            jail.set_env("UTOPIA_COOKIE_SECURE", "true");
            jail.set_env("UTOPIA_SUPERSEDED_VECTOR_DAYS", "30");
            let cfg = AppConfig::load().unwrap();
            assert_eq!(cfg.db_max_connections, Some(8));
            assert!(cfg.cookie_secure);
            assert_eq!(cfg.superseded_vector_days, Some(30));
            Ok(())
        });
    }
}
