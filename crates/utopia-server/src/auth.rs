//! Authentication: argon2 password hashing + JWT (HttpOnly cookie, Bearer also accepted).
//! The cookie itself is a session cookie; expiry is controlled by the JWT's exp (7 days).

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use chrono::Utc;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use utopia_core::models::User;
use utopia_core::AppError;
use uuid::Uuid;

use crate::error::ApiErr;
use crate::state::AppState;

pub const COOKIE_NAME: &str = "utopia_token";
const TOKEN_TTL_DAYS: i64 = 7;

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: Uuid,
    exp: i64,
}

pub fn hash_password(password: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Other(anyhow::anyhow!("Password hashing failed: {e}")))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    // An account created through single sign-on stores `!`, which cannot be parsed. Still run argon2 (against a dummy),
    // otherwise the response is much faster, which would let someone tell which accounts only use SSO
    let Ok(parsed) = PasswordHash::new(hash) else {
        let dummy = PasswordHash::new(dummy_password_hash()).expect("the dummy parses");
        let _ = Argon2::default().verify_password(password.as_bytes(), &dummy);
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// The constant-time partner for the "email does not exist" branch: an argon2 hash computed with real parameters, whose plaintext is random
/// bytes, computed and then thrown away. The key point is just one thing: it must be parseable by `PasswordHash::new` and carry the
/// same parameters as `hash_password`, so that branch and "email exists, wrong password" go through the same computation.
/// It matches no password; the result is discarded anyway. It is derived from `hash_password` rather than hard-coded, so that
/// the parameters never drift: if `Argon2::default()` changes, this changes with it, and no test silently goes stale.
pub fn dummy_password_hash() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        use argon2::password_hash::rand_core::RngCore;
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let plaintext: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        hash_password(&plaintext).expect("hashing random bytes cannot fail")
    })
}

pub fn issue_token(state: &AppState, user_id: Uuid) -> Result<String, AppError> {
    issue_session(state, user_id).map(|(token, _)| token)
}

/// Issue a session and give its expiry moment: where the caller needs the expiry time, use this,
/// so it is not recomputed from the TTL elsewhere and mismatched with the `exp` in the JWT
pub fn issue_session(
    state: &AppState,
    user_id: Uuid,
) -> Result<(String, chrono::DateTime<Utc>), AppError> {
    let expires_at = Utc::now() + chrono::Duration::days(TOKEN_TTL_DAYS);
    let claims = Claims {
        sub: user_id,
        exp: expires_at.timestamp(),
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .map_err(|e| AppError::Other(anyhow::anyhow!("Token issuance failed: {e}")))?;
    Ok((token, expires_at))
}

/// Whether the outer layer runs TLS. Reverse proxies always send `X-Forwarded-Proto`; without this header (direct local connection,
/// development) it is treated as plaintext: Secure is not set, and login still works.
///
/// No "trusted proxy" list is needed: forging this header only makes the attacker's own cookie Secure,
/// which is stricter rather than looser and has no attack value. `UTOPIA_COOKIE_SECURE=true` can force it on,
/// as a fallback for proxies that do not send this header.
pub fn behind_tls(headers: &axum::http::HeaderMap, forced: bool) -> bool {
    forced
        || headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            // Behind several layers of proxies this header is a comma-separated chain, and the leftmost is the very first hop
            .and_then(|v| v.split(',').next())
            .is_some_and(|p| p.trim().eq_ignore_ascii_case("https"))
}

/// The session cookie. `secure` is decided by [`behind_tls`]: under HTTPS it gets Secure,
/// so the browser will not send it again over a plaintext link.
pub fn auth_cookie(token: String, secure: bool) -> Cookie<'static> {
    Cookie::build((COOKIE_NAME, token))
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .build()
}

/// Delete directive for logout. **The attributes must match those used at issue time**: the browser matches name + domain + path
/// to recognise which cookie to delete; with only the name, path degrades to the current request path
/// (`/api/v1/auth`), which does not match the `/` used at issue time, so the cookie stays in the browser and the person thinks
/// they have logged out.
pub fn clear_auth_cookie(secure: bool) -> Cookie<'static> {
    Cookie::build((COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .build()
}

pub(crate) fn decode_user_id(state: &AppState, token: &str) -> Result<Uuid, AppError> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(state.jwt_secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| AppError::Unauthorized)?;
    Ok(data.claims.sub)
}

/// Credentials carried by the request: the Cookie `utopia_token` takes precedence, then `Authorization: Bearer`.
fn presented_credential(parts: &Parts) -> Result<String, AppError> {
    CookieJar::from_headers(&parts.headers)
        .get(COOKIE_NAME)
        .map(|c| c.value().to_string())
        .or_else(|| {
            parts
                .headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(|v| v.to_string())
        })
        .ok_or(AppError::Unauthorized)
}

/// Logged-in user extractor: Cookie `utopia_token` or `Authorization: Bearer`.
pub struct AuthUser(pub User);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiErr;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = presented_credential(parts)?;
        let user_id = decode_user_id(state, &token)?;
        let user = utopia_store::accounts::find_user_by_id(&state.pool, user_id)
            .await?
            .ok_or(AppError::Unauthorized)?;
        Ok(AuthUser(user))
    }
}

/// A session or a personal token; both land on the same `User` (0014 decision 6, #1243).
///
/// **Does not replace `AuthUser`.** `AuthUser` decodes every Bearer as a JWT, so a token is a 401 on all those hundreds of
/// routes. That is deliberate: which route accepts tokens has to be justified in 0014.
/// The only users of this extractor are document originals (0052), the "set a library up" routes and the graph-reading routes: entity search,
/// entity detail, cognitive history, fact evidence, graph overview, neighbourhood, ontology, the instances of a class, a document's extraction result,
/// and the whole-library export (decision 7).
///
/// Reading a document back by its `external_id` as pushed (#1276) is also a document original; the reason for opening it to tokens is in decision 7.
/// Chunk search (#1271) is likewise opened to tokens under decision 7.
///
/// Closing facts a source no longer states (`close-unconfirmed`, the fork's RC-004): the client that pushes statements
/// has only the token.
///
/// A token only narrows: when `pat` is present, the route first asks [`Self::require_write`] or
/// [`Self::reaches`]; the role is still judged by `require_kb` and the workspace role.
pub struct SessionOrToken {
    pub user: User,
    /// None = web session, narrows nothing
    pub pat: Option<utopia_store::tokens::Authenticated>,
}

impl FromRequestParts<AppState> for SessionOrToken {
    type Rejection = ApiErr;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let raw = presented_credential(parts)?;
        // Expiry and revocation are checked in the `authenticate` SQL; a deactivated person is blocked by `find_user_by_id`
        let (user_id, pat) = if raw.starts_with(utopia_store::tokens::PREFIX) {
            let pat = utopia_store::tokens::authenticate(&state.pool, raw.trim()).await?;
            (pat.user_id, Some(pat))
        } else {
            (decode_user_id(state, &raw)?, None)
        };
        let user = utopia_store::accounts::find_user_by_id(&state.pool, user_id)
            .await?
            .ok_or(AppError::Unauthorized)?;
        Ok(Self { user, pat })
    }
}

impl SessionOrToken {
    /// The token acting on someone's behalf; for a session it is None. The audit row tells which token it was by this (0014 decision 6)
    pub fn token_id(&self) -> Option<Uuid> {
        self.pat.as_ref().map(|pat| pat.token_id)
    }

    /// Whether the token excludes this library. A session excludes nothing; this is not permission, which belongs to the role
    pub fn reaches(&self, kb_id: Uuid) -> bool {
        self.pat.as_ref().is_none_or(|pat| pat.covers(kb_id))
    }

    /// Whether the token may write to this library. `None` is a library that does not exist yet: a token restricted by `kb_ids`
    /// cannot have it in its list, so only tokens not restricted to libraries may create one.
    ///
    /// Each of the two refusals carries its own code: both mean "use a different token", which is not the same as a 403 for an insufficient role
    pub fn require_write(&self, kb_id: Option<Uuid>) -> Result<(), AppError> {
        let Some(pat) = &self.pat else {
            return Ok(());
        };
        if !pat.can_write() {
            return Err(AppError::CodedForbidden {
                code: "token_read_only",
                message: "This token is read-only; issue one with the write scope".into(),
            });
        }
        let in_scope = match kb_id {
            Some(kb_id) => pat.covers(kb_id),
            None => pat.kb_ids.is_none(),
        };
        if !in_scope {
            return Err(AppError::CodedForbidden {
                code: "token_kb_out_of_scope",
                message: "This token reaches only the knowledge bases it lists".into(),
            });
        }
        Ok(())
    }
}

/// Generate a JWT signing key: 32 bytes from a CSPRNG, hex-encoded to 64 characters.
///
/// The length is 32 bytes because the HMAC block of HS256 is 32 bytes: anything longer is hashed first,
/// adding no strength. Hex rather than base64: this value appears in logs, environment variables and ops copy-paste,
/// and without `+/=` a whole class of escaping problems is avoided.
pub fn generate_jwt_secret() -> String {
    use argon2::password_hash::rand_core::RngCore;
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims_in(days: i64) -> Claims {
        Claims {
            sub: Uuid::now_v7(),
            exp: (Utc::now() + chrono::Duration::days(days)).timestamp(),
        }
    }

    /// Guard rails for JWT validation: the default config must accept HS256 issued by ourselves, and reject key swaps and expiry.
    /// Added during the jsonwebtoken 9 → 10 upgrade (CVE-2026-25537: type confusion below 10.3.0 can bypass authorisation):
    /// the library's default validation semantics are the part the compiler cannot see, and regressions are caught here.
    #[test]
    fn default_validation_accepts_own_token_and_rejects_the_rest() {
        let secret = b"test-secret-not-a-real-key";
        let claims = claims_in(7);
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret),
        )
        .expect("issuing must succeed");

        let decoded = decode::<Claims>(
            &token,
            &DecodingKey::from_secret(secret),
            &Validation::default(),
        )
        .expect("own token must verify under default validation");
        assert_eq!(decoded.claims.sub, claims.sub, "sub must survive the trip");

        assert!(
            decode::<Claims>(
                &token,
                &DecodingKey::from_secret(b"a-different-secret"),
                &Validation::default(),
            )
            .is_err(),
            "a token signed with another key must not verify"
        );

        let expired = encode(
            &Header::default(),
            &claims_in(-1),
            &EncodingKey::from_secret(secret),
        )
        .expect("issuing must succeed");
        assert!(
            decode::<Claims>(
                &expired,
                &DecodingKey::from_secret(secret),
                &Validation::default(),
            )
            .is_err(),
            "exp must be enforced by default"
        );
    }

    fn headers_with(proto: Option<&str>) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        if let Some(p) = proto {
            h.insert("x-forwarded-proto", p.parse().unwrap());
        }
        h
    }

    /// Secure must be true only when TLS is confirmed in use: if it is judged true wrongly, users of a plaintext deployment
    /// find the browser drops the cookie right after login, the symptom being "click login and land back on the login page", with no error reported.
    #[test]
    fn secure_only_when_tls_is_actually_in_front() {
        // No proxy: direct local connection, cargo run. Secure must not be set, or login fails over HTTP
        assert!(!behind_tls(&headers_with(None), false));
        assert!(!behind_tls(&headers_with(Some("http")), false));

        assert!(behind_tls(&headers_with(Some("https")), false));
        // The header's case is decided by the proxy and must not be assumed
        assert!(behind_tls(&headers_with(Some("HTTPS")), false));

        // Behind several layers of proxies this header is a comma-separated chain, and the leftmost is the very first hop:
        // picking the wrong end would judge "user goes HTTPS to the edge, edge goes HTTP to origin" as plaintext
        assert!(behind_tls(&headers_with(Some("https, http")), false));
        assert!(!behind_tls(&headers_with(Some("http, https")), false));

        // Forced on by configuration: a fallback for proxies that do not send this header
        assert!(behind_tls(&headers_with(None), true));
    }

    /// The dummy hash for the "email does not exist" branch: the only thing that matters is that it uses the same parameters as a real hash,
    /// so both branches run the same argon2 computation. What the plaintext is does not matter: `verify_password`
    /// runs in full once parsing succeeds, and matching or not takes the same time
    #[test]
    fn the_dummy_hash_costs_the_same_as_a_real_one() {
        let dummy = PasswordHash::new(dummy_password_hash()).expect("the dummy parses");
        let real_str = hash_password("anything").unwrap();
        let real = PasswordHash::new(&real_str).unwrap();
        assert_eq!(dummy.algorithm, real.algorithm);
        assert_eq!(
            dummy.params, real.params,
            "the dummy must cost what a real verify costs"
        );
        assert_eq!(
            dummy.salt.map(|s| s.len()),
            real.salt.map(|s| s.len()),
            "same salt length as a real hash"
        );
        assert!(!verify_password("anything", dummy_password_hash()));
    }
}
