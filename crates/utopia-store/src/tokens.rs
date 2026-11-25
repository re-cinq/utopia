//! Personal access tokens (see `docs/decisions/0014`).
//!
//! **A token acts under the identity of the person who issued it, but need not be all of that person:**
//!
//! ```text
//! Effective permission = the person's role ∩ the token's scope
//! ```
//!
//! It is an intersection, not a union -- a viewer's token with write ticked is still read-only. So this module only answers
//! "which person this string of characters belongs to, and how far this token lets them go" -- **whether they may touch a given library is still decided by
//! `access::require_kb`**, and nothing there needs to change.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use utopia_core::models::TokenView;
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// The prefix of a plaintext token. It is distinct from the `utp_` in `sources.ingest_token` --
/// the two can do very different things, so you should be able to tell which kind it is at a glance in a log or config file
pub const PREFIX: &str = "utp_pat_";
/// The short piece shown in listings for people to recognize (including the prefix). Enough to match the string in a config file, but not enough to reconstruct
const SHOWN: usize = 16;

/// What you get after successful validation: who, and how far this token lets them go.
pub struct Authenticated {
    pub user_id: Uuid,
    pub token_id: Uuid,
    /// read | write
    pub scope: String,
    /// None = all libraries this person can enter
    pub kb_ids: Option<Vec<Uuid>>,
}

impl Authenticated {
    /// Whether this token may touch this library.
    ///
    /// **This is a scope decision, not a permission check.** Returning true only means "the token has not excluded it";
    /// what role the person has in this library still has to be asked of `access::require_kb` as usual.
    pub fn covers(&self, kb_id: Uuid) -> bool {
        match &self.kb_ids {
            None => true,
            Some(ids) => ids.contains(&kb_id),
        }
    }

    pub fn can_write(&self) -> bool {
        self.scope == "write"
    }
}

/// **SHA-256, not argon2.** This differs from how passwords are stored, for two reasons:
///
/// 1. **Tokens are high-entropy random strings, not passwords a person picked.** argon2 is slow so that brute-forcing
///       "password123" becomes unprofitable; against a 244-bit random number, being a million times slower still gets nowhere,
///       and buys nothing.
/// 2. **argon2 salts differ per row, so you cannot look up by them.** Verification is on the hot path (once per tool call),
///       and `WHERE token_hash = $1` on a unique index is a single hit; with argon2 you would have to
///       fetch every token back and verify each one -- the more that are issued, the slower it gets.
fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Issue one. **The plaintext returned is the only time it appears** -- only the hash is stored in the database, and a lost token can only be reissued.
pub async fn issue(
    pool: &PgPool,
    user_id: Uuid,
    name: &str,
    scope: &str,
    kb_ids: Option<&[Uuid]>,
    expires_at: Option<DateTime<Utc>>,
) -> AppResult<(TokenView, String)> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return AppResult::Err(AppError::invalid(
            "bad_token_name",
            "Token name must be 1-64 characters",
        ));
    }
    if !matches!(scope, "read" | "write") {
        return Err(AppError::invalid(
            "bad_token_scope",
            "Scope must be read or write",
        ));
    }
    // Two v4 UUIDs concatenated ≈ 244 bits of entropy. Same approach as `new_ingest_token`,
    // a different prefix so the two can be told apart in logs
    let plain = format!(
        "{PREFIX}{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    );
    let id = Uuid::now_v7();
    let view: TokenView = sqlx::query_as(
        "INSERT INTO personal_tokens
             (id, user_id, name, token_hash, token_prefix, scope, kb_ids, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id, name, token_prefix, scope, kb_ids, expires_at,
                   last_used_at, revoked_at, created_at",
    )
    .bind(id)
    .bind(user_id)
    .bind(name)
    .bind(hash(&plain))
    .bind(&plain[..SHOWN])
    .bind(scope)
    .bind(kb_ids)
    .bind(expires_at)
    .fetch_one(pool)
    .await?;
    Ok((view, plain))
}

/// The ones I have issued. Revoked ones are listed too -- **the fact of revocation itself must stay visible**.
pub async fn list(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<TokenView>> {
    Ok(sqlx::query_as(
        "SELECT id, name, token_prefix, scope, kb_ids, expires_at,
                last_used_at, revoked_at, created_at
           FROM personal_tokens WHERE user_id = $1 ORDER BY created_at DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// Revoke one. **Stamp, do not delete the row**: deleting would make "this key once existed" unqueryable,
/// and that is the first thing a later investigation needs to ask.
pub async fn revoke(pool: &PgPool, user_id: Uuid, token_id: Uuid) -> AppResult<()> {
    let res = sqlx::query(
        "UPDATE personal_tokens SET revoked_at = now()
          WHERE id = $2 AND user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .bind(token_id)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Plaintext → who this is and how far they may go.
///
/// **Expiry and revocation are judged in SQL, not in Rust.** If you fetch and then compare,
/// a token revoked in the gap between "fetch" and "compare" would still pass -- and MCP
/// connections are long-lived, so that gap can grow to matter.
///
/// Also write `last_used_at` in passing: before revoking, a person needs to be able to answer "is this still in use?",
/// and without this number nobody dares to revoke.
pub async fn authenticate(pool: &PgPool, plain: &str) -> AppResult<Authenticated> {
    if !plain.starts_with(PREFIX) {
        return Err(AppError::Unauthorized);
    }
    let row: Option<(Uuid, Uuid, String, Option<Vec<Uuid>>)> = sqlx::query_as(
        "UPDATE personal_tokens SET last_used_at = now()
          WHERE token_hash = $1
            AND revoked_at IS NULL
            AND (expires_at IS NULL OR expires_at > now())
        RETURNING id, user_id, scope, kb_ids",
    )
    .bind(hash(plain))
    .fetch_optional(pool)
    .await?;
    let Some((token_id, user_id, scope, kb_ids)) = row else {
        return Err(AppError::Unauthorized);
    };
    Ok(Authenticated {
        user_id,
        token_id,
        scope,
        kb_ids,
    })
}
