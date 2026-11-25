//! Single sign-on: a narrow OIDC authorization code flow (PKCE, one-time state, nonce,
//! issuer/audience and RS256 checks).
//!
//! **A subject at the identity provider is bound by the account owner themselves.** The person first logs in with a password, then goes through
//! SSO on the account page; the callback checks that "the person who completed this flow is the logged-in person who started the binding", and only then writes `oidc_identities`.
//! Before, an admin could fill any subject into any account, which was a route to impersonate anyone (including other admins): the binding did not go through the person, the person could not see it, and a password change did not cut it off; later
//! actions were recorded in the ledger as done by the impersonated person (0014: identity comes from the person).
//! Admins can now only view and unbind.
//!
//! By default, auto-linking by a mutable claim such as the identity provider's email never happens, and a first login does not auto-create an account.
//! The exception is when the deployment lists `UTOPIA_OIDC_ALLOWED_DOMAINS`: for those domains, an email verified by the provider on first login
//! links to an existing account or opens one (0069: the identity provider decides for those domains).
//!
//! **Another application can exchange the ID token it has just logged in with for a session** (`exchange`, 0066): same
//! identity provider, trusted audience list, issued within ten minutes, exchanged only once; the mapping rules are the same as for login.

use crate::{
    auth::{self, AuthUser},
    error::ApiResult,
    state::AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    response::Redirect,
    Json,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use utopia_core::AppError;
use uuid::Uuid;

const FLOW_COOKIE: &str = "utopia_oidc_state";
/// With `__Host-` prefix over HTTPS: browsers accept only this name when it is Secure, Path=/ and has no Domain,
/// so a sibling subdomain of the same site or a plaintext link cannot plant it. Otherwise an attacker plants their own state first,
/// then leads the victim to the attacker's own callback address, and the victim logs into the attacker's account (login CSRF).
const FLOW_COOKIE_HOST: &str = "__Host-utopia_oidc_state";
const FLOW_TTL_SECS: u64 = 600;
/// Cap on login flows in flight at the same time. `start` needs no login, and every call writes a row and asks the identity provider once;
/// without a cap, one script could bloat the table and exhaust the identity provider's rate limit for this deployment.
const MAX_PENDING_FLOWS: i64 = 1000;
/// Size cap for the discovery document, JWKS and token responses. They come from the identity provider, but a misbehaving
/// identity provider must not be able to eat the server's memory.
const DOC_LIMIT: usize = 256 * 1024;
const TOKEN_LIMIT: usize = 64 * 1024;
/// Cache lifetime for the discovery document and JWKS. When rotating keys, a `kid` that is not found forces one refresh
const CACHE_TTL: Duration = Duration::from_secs(600);
/// Minimum gap between two forced refreshes of the same address. `kid` is written by the requester, and `exchange` needs no login:
/// without a cooldown, making up a `kid` would let the server ask the identity provider every time.
const FORCE_COOLDOWN: Duration = Duration::from_secs(60);

struct Config {
    issuer: String,
    client_id: String,
    secret: Option<String>,
    redirect: String,
    /// Which clients' ID tokens `exchange` accepts. Empty = exchange is not offered. A deployment whose client is shared with Utopia
    /// writes Utopia's own client id here
    exchange_audiences: Vec<String>,
    /// An email verified among these domains, on first login, is matched by email to an existing account, or one is opened if none exists
    /// (`UTOPIA_OIDC_ALLOWED_DOMAINS`). Empty = only trust subjects the person bound themselves
    allowed_domains: Vec<String>,
    /// Local development: allows the issuer and each endpoint to use plaintext HTTP on a loopback address. Only localhost /
    /// 127.0.0.1 / ::1 are accepted; public addresses must still be HTTPS
    loopback_http: bool,
    /// Scope of the authorization request (`UTOPIA_OIDC_SCOPES`, default `openid profile email`). Some identity providers'
    /// admins only approve `openid profile`; asking for an extra `email` makes every login stick on the consent page (#1355)
    scopes: String,
}

/// `openid` is required: without it this is not OIDC and no ID token is returned. If it is missing it is prepended rather than raising an error
fn scopes(raw: Option<&str>) -> String {
    let mut out: Vec<&str> = Vec::new();
    for s in raw
        .filter(|r| !r.trim().is_empty())
        .unwrap_or("openid profile email")
        .split_whitespace()
    {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if !out.contains(&"openid") {
        out.insert(0, "openid");
    }
    out.join(" ")
}

fn config() -> Result<Config, AppError> {
    let get = |key: &str| {
        std::env::var(key)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| AppError::Validation("SSO is not configured".into()))
    };
    let c = Config {
        issuer: get("UTOPIA_OIDC_ISSUER")?,
        client_id: get("UTOPIA_OIDC_CLIENT_ID")?,
        secret: std::env::var("UTOPIA_OIDC_CLIENT_SECRET")
            .ok()
            .filter(|s| !s.is_empty()),
        redirect: get("UTOPIA_OIDC_REDIRECT_URI")?,
        exchange_audiences: std::env::var("UTOPIA_OIDC_EXCHANGE_AUDIENCES")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(String::from)
            .collect(),
        allowed_domains: std::env::var("UTOPIA_OIDC_ALLOWED_DOMAINS")
            .unwrap_or_default()
            .split(',')
            .map(|d| d.trim().to_lowercase())
            .filter(|d| !d.is_empty())
            .collect(),
        loopback_http: std::env::var("UTOPIA_OIDC_ALLOW_LOOPBACK_HTTP")
            .is_ok_and(|v| matches!(v.trim(), "1" | "true")),
        scopes: scopes(std::env::var("UTOPIA_OIDC_SCOPES").ok().as_deref()),
    };
    secure_url(&c.issuer, c.loopback_http)?;
    secure_url(&c.redirect, true)?;
    Ok(c)
}

fn secure_url(s: &str, loopback: bool) -> Result<reqwest::Url, AppError> {
    let url = reqwest::Url::parse(s).map_err(|_| AppError::Validation("Invalid SSO URL".into()))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https"
            || (loopback
                && url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))))
    {
        return Err(AppError::Validation(
            "SSO endpoints require HTTPS (localhost callback may use HTTP)".into(),
        ));
    }
    Ok(url)
}

/// One rejection: `code` is returned to the UI (the login and account pages word their messages by it), and `reason` goes only to the log and the ledger
struct Refusal {
    code: &'static str,
    reason: String,
}

fn refuse(code: &'static str, reason: impl Into<String>) -> Refusal {
    Refusal {
        code,
        reason: reason.into(),
    }
}

impl From<sqlx::Error> for Refusal {
    fn from(e: sqlx::Error) -> Self {
        refuse("unavailable", format!("database: {e}"))
    }
}

fn client() -> Result<reqwest::Client, Refusal> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| refuse("unavailable", format!("http client: {e}")))
}

/// Read a size-capped JSON response. Check Content-Length first, then count while reading;
/// a response that reports no length is still cut off
async fn read_json(mut resp: reqwest::Response, limit: usize) -> Result<Value, String> {
    if !resp.status().is_success() {
        return Err(format!("status {}", resp.status()));
    }
    if resp.content_length().is_some_and(|n| n > limit as u64) {
        return Err(format!("response larger than {limit} bytes"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > limit {
            return Err(format!("response larger than {limit} bytes"));
        }
    }
    serde_json::from_slice(&body).map_err(|e| format!("not JSON: {e}"))
}

type Cache = tokio::sync::Mutex<HashMap<String, (Instant, Value)>>;

fn cache() -> &'static Cache {
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The discovery document and JWKS are cached per address. `start` needs no login, and without caching every anonymous request
/// would make the server ask the identity provider again
async fn cached_json(url: &reqwest::Url, force: bool) -> Result<Value, Refusal> {
    let key = url.to_string();
    let force = force && force_allowed(&key).await;
    if !force {
        if let Some((at, v)) = cache().lock().await.get(&key) {
            if at.elapsed() < CACHE_TTL {
                return Ok(v.clone());
            }
        }
    }
    let resp = client()?
        .get(url.clone())
        .send()
        .await
        .map_err(|e| refuse("unavailable", format!("fetch {key}: {e}")))?;
    let v = read_json(resp, DOC_LIMIT)
        .await
        .map_err(|e| refuse("unavailable", format!("fetch {key}: {e}")))?;
    cache()
        .lock()
        .await
        .insert(key, (Instant::now(), v.clone()));
    Ok(v)
}

/// Whether this forced refresh is allowed: during the cooldown everything falls back to the cache. Record the time first, then fetch,
/// so that of a batch of requests arriving together only one actually asks the identity provider
async fn force_allowed(key: &str) -> bool {
    static LAST: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, Instant>>> =
        std::sync::OnceLock::new();
    let mut last = LAST.get_or_init(Default::default).lock().await;
    if last
        .get(key)
        .is_some_and(|at| at.elapsed() < FORCE_COOLDOWN)
    {
        return false;
    }
    last.insert(key.to_string(), Instant::now());
    true
}

async fn metadata(c: &Config) -> Result<Value, Refusal> {
    let url = secure_url(
        &format!(
            "{}/.well-known/openid-configuration",
            c.issuer.trim_end_matches('/')
        ),
        c.loopback_http,
    )
    .map_err(|_| refuse("unavailable", "issuer URL"))?;
    let meta = cached_json(&url, false).await?;
    if meta["issuer"] != c.issuer {
        return Err(refuse(
            "unavailable",
            "discovery document names a different issuer",
        ));
    }
    Ok(meta)
}

fn endpoint(c: &Config, meta: &Value, key: &str) -> Result<reqwest::Url, Refusal> {
    let raw = meta[key]
        .as_str()
        .ok_or_else(|| refuse("unavailable", format!("discovery has no {key}")))?;
    secure_url(raw, c.loopback_http)
        .map_err(|_| refuse("unavailable", format!("{key} is not HTTPS")))
}

fn flow_cookie(value: String, secure: bool) -> Cookie<'static> {
    let (name, path) = if secure {
        (FLOW_COOKIE_HOST, "/")
    } else {
        (FLOW_COOKIE, "/api/v1/auth/oidc")
    };
    Cookie::build((name, value))
        .path(path)
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(secure)
        .max_age(
            Duration::from_secs(FLOW_TTL_SECS)
                .try_into()
                .expect("ten minutes fits cookie duration"),
        )
        .build()
}

/// The logged-in session the current request carries (cookie); a deactivated account does not count
async fn session_user(s: &AppState, jar: &CookieJar) -> Option<Uuid> {
    let token = jar.get(auth::COOKIE_NAME)?.value().to_string();
    let id = auth::decode_user_id(s, &token).ok()?;
    utopia_store::accounts::find_user_by_id(&s.pool, id)
        .await
        .ok()
        .flatten()
        .map(|u| u.id)
}

/// RFC 6749 section 2.3.1: client id and secret are form-encoded first, then put into the Basic header.
/// Without encoding, a secret containing `:`, `%` or `+` fails authentication at a strict identity provider
fn form_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

pub async fn status(State(s): State<AppState>) -> Json<Value> {
    Json(json!({"enabled": config().is_ok(), "password_login": password_login(&s)}))
}

/// Whether password login is actually enabled: `UTOPIA_PASSWORD_LOGIN=false` only takes effect when SSO is configured.
/// Turning off the password with no SSO means nobody can get in, so in that case the switch is treated as unset, with the same rule as the login page
pub fn password_login(s: &AppState) -> bool {
    s.password_login || config().is_err()
}

/// Stated at startup: the allowlist links accounts by the email verified in the ID token, and when `email` is not in the scope
/// most identity providers do not send that claim at all, so the allowlist is effectively useless
pub fn warn_about_scopes() {
    if let Ok(c) = config() {
        if !c.allowed_domains.is_empty() && !c.scopes.split_whitespace().any(|s| s == "email") {
            tracing::warn!("UTOPIA_OIDC_SCOPES 里没有 email，而 UTOPIA_OIDC_ALLOWED_DOMAINS 已设：身份提供方多半不发 email，按域名关联和开户不会发生")
        }
    }
}

/// Two consequences of turning off password login are stated at startup: without SSO the switch has no effect;
/// without an allowlist only people with a bound identity can get in, and new people can neither register nor have an account opened
pub fn warn_about_password_login(s: &AppState) {
    if s.password_login {
        return;
    }
    match config() {
        Err(_) => {
            tracing::warn!("UTOPIA_PASSWORD_LOGIN=false 被忽略：没配单点登录，关掉密码就谁都进不来")
        }
        Ok(c) if c.allowed_domains.is_empty() => tracing::warn!(
            "密码登录已关、UTOPIA_OIDC_ALLOWED_DOMAINS 为空：只有绑定过身份的人能登录"
        ),
        Ok(_) => {}
    }
}

#[derive(Deserialize)]
pub struct StartQuery {
    #[serde(default)]
    link: Option<String>,
}

/// Reached by browser navigation: errors always redirect back to the page with a code, rather than showing the person a JSON blob
fn back_to(page: &str, code: &str) -> Redirect {
    Redirect::to(&format!("{page}?sso_error={code}"))
}

pub async fn start(
    State(s): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    Query(q): Query<StartQuery>,
) -> (CookieJar, Redirect) {
    let linking = q.link.as_deref().is_some_and(|v| matches!(v, "1" | "true"));
    let page = if linking { "/account" } else { "/login" };
    match begin(&s, &jar, linking).await {
        Ok((state, url)) => {
            let secure = auth::behind_tls(&headers, s.cookie_secure);
            (
                jar.add(flow_cookie(state, secure)),
                Redirect::to(url.as_str()),
            )
        }
        Err(r) => {
            tracing::warn!(code = r.code, reason = %r.reason, "单点登录没能开始");
            (jar, back_to(page, r.code))
        }
    }
}

async fn begin(
    s: &AppState,
    jar: &CookieJar,
    linking: bool,
) -> Result<(String, reqwest::Url), Refusal> {
    let c = config().map_err(|_| refuse("unavailable", "not configured"))?;
    // Binding is started only on behalf of the logged-in person: the flow records who it was, and the callback checks again
    let link_user = if linking {
        Some(
            session_user(s, jar)
                .await
                .ok_or_else(|| refuse("session", "link started without a session"))?,
        )
    } else {
        None
    };
    sqlx::query("DELETE FROM oidc_flows WHERE expires_at < now()")
        .execute(&s.pool)
        .await?;
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM oidc_flows")
        .fetch_one(&s.pool)
        .await?;
    if pending >= MAX_PENDING_FLOWS {
        return Err(refuse("busy", format!("{pending} sign-ins in progress")));
    }
    let meta = metadata(&c).await?;
    let mut url = endpoint(&c, &meta, "authorization_endpoint")?;
    let state = auth::generate_jwt_secret();
    let nonce = auth::generate_jwt_secret();
    let verifier = auth::generate_jwt_secret();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    sqlx::query(
        "INSERT INTO oidc_flows (state, nonce, verifier, link_user_id, expires_at)
         VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5))",
    )
    .bind(&state)
    .bind(&nonce)
    .bind(&verifier)
    .bind(link_user)
    .bind(FLOW_TTL_SECS as f64)
    .execute(&s.pool)
    .await?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("scope", c.scopes.as_str()),
        ("client_id", &c.client_id),
        ("redirect_uri", &c.redirect),
        ("state", &state),
        ("nonce", &nonce),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ]);
    Ok((state, url))
}

#[derive(Deserialize)]
pub struct Callback {
    code: Option<String>,
    state: Option<String>,
    /// The person clicked cancel at the identity provider, or it rejected the request
    error: Option<String>,
}

#[derive(Clone, Deserialize)]
struct Claims {
    sub: String,
    /// A login token carries the nonce of this flow; a token for exchange was signed by someone else's flow, and may not have one
    #[serde(default)]
    nonce: Option<String>,
    iat: i64,
    #[serde(default)]
    azp: Option<String>,
    aud: Value,
    #[serde(default)]
    email: Option<String>,
    /// Every token passes through here: some identity providers send the string `"true"`, and strict bool parsing would stop everyone
    /// from logging in. Only `true` / `"true"` count as verified; anything else does not
    #[serde(default, deserialize_with = "lenient_true")]
    email_verified: bool,
    /// Microsoft Entra sends no `email_verified`. Its optional claim `xms_edov` says the
    /// owner of the email's domain is verified, which is what an allowed domain needs.
    #[serde(default, rename = "xms_edov", deserialize_with = "lenient_true")]
    email_domain_owner_verified: bool,
    #[serde(default)]
    name: Option<String>,
}

fn lenient_true<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Bool(b) => b,
        Value::String(v) => v == "true",
        _ => false,
    })
}

/// Display name for the account being opened: the `name` claim with leading and trailing whitespace removed, at most 64 characters; if empty, use the local part of the email
fn display_name(claim: Option<&str>, email: &str) -> String {
    let name: String = claim.unwrap_or_default().trim().chars().take(64).collect();
    if name.is_empty() {
        email.split('@').next().unwrap_or_default().to_string()
    } else {
        name
    }
}

/// The email that can be used to open an account automatically or link by email: verified by the identity provider, and the domain is on the allowlist.
/// With the list empty it is always `None`, since by default only subjects the person bound themselves are trusted
fn provisioned_email(claims: &Claims, allowed_domains: &[String]) -> Option<String> {
    let email = claims.email.as_deref()?.trim().to_lowercase();
    let (_, domain) = email.rsplit_once('@')?;
    let verified = claims.email_verified || claims.email_domain_owner_verified;
    (verified && allowed_domains.iter().any(|d| d == domain)).then_some(email)
}

/// Find the key by `kid`; when the token has no `kid`, use the one RSA key only if the JWKS contains exactly one
fn pick_key<'a>(keys: &'a JwkSet, kid: Option<&str>) -> Option<&'a jsonwebtoken::jwk::Jwk> {
    match kid {
        Some(kid) => keys.find(kid),
        None => {
            let mut rsa = keys
                .keys
                .iter()
                .filter(|k| matches!(k.algorithm, jsonwebtoken::jwk::AlgorithmParameters::RSA(_)));
            match (rsa.next(), rsa.next()) {
                (Some(only), None) => Some(only),
                _ => None,
            }
        }
    }
}

fn verify_token(
    c: &Config,
    id_token: &str,
    keys: &JwkSet,
    nonce: &str,
) -> Result<Claims, AppError> {
    let claims = verify_for(c, id_token, keys, &[c.client_id.as_str()])?;
    if claims.nonce.as_deref() != Some(nonce) {
        return Err(AppError::Unauthorized);
    }
    Ok(claims)
}

/// Signature, issuer, validity period and audience: the token must be signed by the configured issuer for one of `audiences`.
/// When `azp` is present (or there is more than one audience) it must also be among them
fn verify_for(
    c: &Config,
    id_token: &str,
    keys: &JwkSet,
    audiences: &[&str],
) -> Result<Claims, AppError> {
    let header = decode_header(id_token).map_err(|_| AppError::Unauthorized)?;
    if header.alg != Algorithm::RS256 {
        return Err(AppError::Unauthorized);
    }
    let key = pick_key(keys, header.kid.as_deref()).ok_or(AppError::Unauthorized)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_nbf = true;
    validation.set_issuer(&[&c.issuer]);
    validation.set_audience(audiences);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub", "iat"]);
    let claims = decode::<Claims>(
        id_token,
        &DecodingKey::from_jwk(key).map_err(|_| AppError::Unauthorized)?,
        &validation,
    )
    .map_err(|_| AppError::Unauthorized)?
    .claims;
    let trusted = |a: &String| audiences.contains(&a.as_str());
    if claims.sub.is_empty()
        || claims.iat > chrono::Utc::now().timestamp() + 60
        || claims.azp.as_ref().is_some_and(|a| !trusted(a))
        || (claims.aud.as_array().is_some_and(|a| a.len() > 1)
            && !claims.azp.as_ref().is_some_and(trusted))
    {
        return Err(AppError::Unauthorized);
    }
    Ok(claims)
}

/// How old an exchanged ID token may be at most: it is used for the exchange right after login completes
const EXCHANGE_MAX_AGE_SECS: i64 = 600;

/// A freshly issued ID token signed for one of the trusted audiences
fn verify_exchange_token(c: &Config, id_token: &str, keys: &JwkSet) -> Result<Claims, AppError> {
    let audiences: Vec<&str> = c.exchange_audiences.iter().map(String::as_str).collect();
    let claims = verify_for(c, id_token, keys, &audiences)?;
    if claims.iat < chrono::Utc::now().timestamp() - EXCHANGE_MAX_AGE_SECS {
        return Err(AppError::Unauthorized);
    }
    Ok(claims)
}

#[derive(Deserialize)]
pub struct Exchange {
    id_token: String,
}

/// Another application (whose client id is in `UTOPIA_OIDC_EXCHANGE_AUDIENCES`) lets its user log in at the same
/// identity provider, then exchanges the ID token it just received for a Utopia session, and calls the API here as that person.
/// The mapping is the same as SSO login: an identity the person bound themselves, or a domain on the allowlist (0069).
/// If SSO or a trusted audience is not configured, this endpoint is treated as if it does not exist
pub async fn exchange(
    State(s): State<AppState>,
    Json(body): Json<Exchange>,
) -> ApiResult<Json<Value>> {
    let Some(c) = config().ok().filter(|c| !c.exchange_audiences.is_empty()) else {
        return Err(AppError::NotFound.into());
    };
    let unavailable = |r: Refusal| {
        tracing::warn!(reason = %r.reason, "SSO exchange: identity provider unavailable");
        AppError::invalid("oidc_unavailable", "The identity provider is unavailable")
    };
    let meta = metadata(&c).await.map_err(unavailable)?;
    let jwks_url = endpoint(&c, &meta, "jwks_uri").map_err(unavailable)?;
    let parse = |v: Value| {
        serde_json::from_value::<JwkSet>(v).map_err(|e| refuse("unavailable", format!("jwks: {e}")))
    };
    let mut keys =
        parse(cached_json(&jwks_url, false).await.map_err(unavailable)?).map_err(unavailable)?;
    let kid = decode_header(&body.id_token).ok().and_then(|h| h.kid);
    if pick_key(&keys, kid.as_deref()).is_none() {
        keys =
            parse(cached_json(&jwks_url, true).await.map_err(unavailable)?).map_err(unavailable)?;
    }
    let claims = verify_exchange_token(&c, &body.id_token, &keys)?;

    // One use only: a hash of the token is stored as a row, for every moment the token would still be accepted; if the insert fails, it has already been exchanged.
    // Counted by `iat` only: the ten-minute freshness has no grace, while the `exp` check has a 60-second grace. If recorded by `exp`,
    // in the minute after expiry the row has already been swept while the token still validates, so it could be exchanged again. The extra minute covers clock skew between the app and the library
    sqlx::query("DELETE FROM oidc_exchanges WHERE expires_at < now()")
        .execute(&s.pool)
        .await?;
    let until = claims.iat + EXCHANGE_MAX_AGE_SECS + 60;
    let spent = sqlx::query(
        "INSERT INTO oidc_exchanges (token_hash, expires_at) VALUES ($1, to_timestamp($2))
         ON CONFLICT DO NOTHING",
    )
    .bind(
        Sha256::digest(body.id_token.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )
    .bind(until as f64)
    .execute(&s.pool)
    .await?;
    if spent.rows_affected() == 0 {
        return Err(
            AppError::invalid("oidc_replayed", "This ID token has already been exchanged").into(),
        );
    }

    let user_id = account_of(&s, &c, &claims).await.map_err(|r| {
        tracing::warn!(code = r.code, reason = %r.reason, "SSO exchange refused");
        AppError::invalid(
            "oidc_unavailable",
            "The identity could not be mapped to an account",
        )
    })?;
    let user_id = user_id.ok_or_else(|| {
        AppError::invalid(
            "oidc_unlinked",
            "This SSO identity is not linked to a Utopia account",
        )
    })?;
    let (token, expires_at) = auth::issue_session(&s, user_id)?;
    let _ = utopia_store::audit::record(
        &s.pool,
        None,
        user_id,
        "auth.oidc_exchange",
        "user",
        Some(user_id),
        json!({"issuer": c.issuer, "client": claims.azp.or_else(|| claims.aud.as_str().map(String::from))}),
    )
    .await;
    Ok(Json(json!({"token": token, "expires_at": expires_at})))
}

/// Which action the callback performed
enum Done {
    SignedIn(Uuid),
    Linked,
}

pub async fn callback(
    State(s): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    Query(q): Query<Callback>,
) -> (CookieJar, Redirect) {
    let secure = auth::behind_tls(&headers, s.cookie_secure);
    let mut linking = false;
    // Read first, then delete: a cookie removed by CookieJar can no longer be fetched with `get`
    let outcome = finish(&s, &jar, q, &mut linking).await;
    let jar = jar
        .remove(flow_cookie(String::new(), true))
        .remove(flow_cookie(String::new(), false));
    match outcome {
        Ok(Done::SignedIn(user_id)) => match auth::issue_token(&s, user_id) {
            Ok(token) => (jar.add(auth::auth_cookie(token, secure)), Redirect::to("/")),
            Err(e) => {
                tracing::warn!(error = %e, "单点登录签发会话失败");
                (jar, back_to("/login", "unavailable"))
            }
        },
        Ok(Done::Linked) => (jar, Redirect::to("/account?sso=linked")),
        Err(r) => {
            tracing::warn!(code = r.code, reason = %r.reason, "单点登录被拒");
            // Failures go into the ledger too, under the same convention as the password login's `auth.login_failed`
            let _ = utopia_store::audit::record_opt(
                &s.pool,
                None,
                None,
                if linking {
                    "auth.oidc_link_failed"
                } else {
                    "auth.oidc_login_failed"
                },
                "user",
                None,
                json!({"reason": r.code, "detail": r.reason}),
            )
            .await;
            (
                jar,
                back_to(if linking { "/account" } else { "/login" }, r.code),
            )
        }
    }
}

async fn finish(
    s: &AppState,
    jar: &CookieJar,
    q: Callback,
    linking: &mut bool,
) -> Result<Done, Refusal> {
    if let Some(e) = q.error {
        return Err(refuse("cancelled", format!("provider returned {e}")));
    }
    let c = config().map_err(|_| refuse("unavailable", "not configured"))?;
    let (code, state) = match (q.code, q.state) {
        (Some(code), Some(state)) => (code, state),
        _ => return Err(refuse("invalid", "callback without code or state")),
    };
    let cookie = jar
        .get(FLOW_COOKIE_HOST)
        .or_else(|| jar.get(FLOW_COOKIE))
        .ok_or_else(|| refuse("session", "no state cookie in this browser"))?;
    use subtle::ConstantTimeEq;
    if !bool::from(cookie.value().as_bytes().ct_eq(state.as_bytes())) {
        return Err(refuse("session", "state does not match this browser"));
    }
    let flow: Option<(String, String, Option<Uuid>)> = sqlx::query_as(
        "DELETE FROM oidc_flows WHERE state = $1 AND expires_at > now()
         RETURNING nonce, verifier, link_user_id",
    )
    .bind(&state)
    .fetch_optional(&s.pool)
    .await?;
    let (nonce, verifier, link_user) =
        flow.ok_or_else(|| refuse("expired", "unknown or expired state"))?;
    *linking = link_user.is_some();

    let meta = metadata(&c).await?;
    let mut req = client()?
        .post(endpoint(&c, &meta, "token_endpoint")?)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", c.redirect.as_str()),
            ("client_id", c.client_id.as_str()),
            ("code_verifier", verifier.as_str()),
        ]);
    if let Some(secret) = &c.secret {
        req = req.basic_auth(form_encode(&c.client_id), Some(form_encode(secret)));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| refuse("unavailable", format!("token endpoint: {e}")))?;
    let tokens = read_json(resp, TOKEN_LIMIT)
        .await
        .map_err(|e| refuse("denied", format!("token endpoint: {e}")))?;
    let id_token = tokens["id_token"]
        .as_str()
        .ok_or_else(|| refuse("denied", "token response has no id_token"))?;

    let jwks_url = endpoint(&c, &meta, "jwks_uri")?;
    let parse = |v: Value| {
        serde_json::from_value::<JwkSet>(v).map_err(|e| refuse("unavailable", format!("jwks: {e}")))
    };
    let mut keys = parse(cached_json(&jwks_url, false).await?)?;
    // The identity provider rotated keys: if the cache lacks this kid, force one refresh; if still not recognized, it is truly unrecognized
    let kid = decode_header(id_token).ok().and_then(|h| h.kid);
    if pick_key(&keys, kid.as_deref()).is_none() {
        keys = parse(cached_json(&jwks_url, true).await?)?;
    }
    let claims = verify_token(&c, id_token, &keys, &nonce)
        .map_err(|_| refuse("denied", "id_token failed verification"))?;

    match link_user {
        Some(user_id) => {
            // The person who completes the binding must still be the one who started it: the flow began while he was logged in,
            // so at callback time the same browser must still hold his session
            if session_user(s, jar).await != Some(user_id) {
                return Err(refuse(
                    "session",
                    "link finished outside the starting session",
                ));
            }
            link(s, &c, user_id, &claims.sub).await?;
            Ok(Done::Linked)
        }
        None => {
            let user_id = account_of(s, &c, &claims).await?.ok_or_else(|| {
                refuse("unlinked", format!("subject {} is not linked", claims.sub))
            })?;
            let _ = utopia_store::audit::record(
                &s.pool,
                None,
                user_id,
                "auth.oidc_login",
                "user",
                Some(user_id),
                json!({"issuer": c.issuer}),
            )
            .await;
            Ok(Done::SignedIn(user_id))
        }
    }
}

/// Which account this subject logs in to: the person's own bound one first; if none is bound and the email falls in an allowed domain,
/// look up the account by email (open one if not found) and bind it for them
async fn account_of(s: &AppState, c: &Config, claims: &Claims) -> Result<Option<Uuid>, Refusal> {
    let linked: Option<Uuid> = sqlx::query_scalar(
        "SELECT i.user_id FROM oidc_identities i JOIN users u ON u.id = i.user_id
          WHERE i.issuer = $1 AND i.subject = $2 AND u.deactivated_at IS NULL",
    )
    .bind(&c.issuer)
    .bind(&claims.sub)
    .fetch_optional(&s.pool)
    .await?;
    let Some(email) = linked
        .is_none()
        .then(|| provisioned_email(claims, &c.allowed_domains))
        .flatten()
    else {
        return Ok(linked);
    };
    let provisioned = utopia_store::accounts::account_for_sso(
        &s.pool,
        &c.issuer,
        &claims.sub,
        &email,
        &display_name(claims.name.as_deref(), &email),
    )
    .await
    .map_err(|e| match e {
        AppError::CodedConflict { code, message } => refuse(code, message),
        e => refuse("unavailable", format!("provision: {e}")),
    })?;
    let Some(user) = provisioned else {
        tracing::warn!(%email, "单点登录：这个 email 属于一个停用的账号，不开新号");
        return Ok(None);
    };
    let _ = utopia_store::audit::record(
        &s.pool,
        None,
        user.id,
        "auth.oidc_link",
        "user",
        Some(user.id),
        json!({"issuer": c.issuer, "subject": claims.sub, "by": "allowed_domain"}),
    )
    .await;
    Ok(Some(user.id))
}

async fn link(s: &AppState, c: &Config, user_id: Uuid, subject: &str) -> Result<(), Refusal> {
    let holder: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM oidc_identities WHERE issuer = $1 AND subject = $2",
    )
    .bind(&c.issuer)
    .bind(subject)
    .fetch_optional(&s.pool)
    .await?;
    match holder {
        Some(h) if h == user_id => return Ok(()),
        Some(_) => return Err(refuse("taken", "subject already linked to another account")),
        None => {}
    }
    let inserted =
        sqlx::query("INSERT INTO oidc_identities (issuer, subject, user_id) VALUES ($1, $2, $3)")
            .bind(&c.issuer)
            .bind(subject)
            .bind(user_id)
            .execute(&s.pool)
            .await;
    match inserted {
        Ok(_) => {}
        // The two constraints each guard against one kind of collision: the subject was taken first by someone else, or this account already has one
        Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
            let code = if db.constraint().is_some_and(|n| n.contains("user")) {
                "already_linked"
            } else {
                "taken"
            };
            return Err(refuse(code, db.message().to_string()));
        }
        Err(e) => return Err(e.into()),
    }
    let _ = utopia_store::audit::record(
        &s.pool,
        None,
        user_id,
        "auth.oidc_link",
        "user",
        Some(user_id),
        json!({"issuer": c.issuer, "subject": subject}),
    )
    .await;
    Ok(())
}

/// My own bindings: used by the account page
pub async fn me(State(s): State<AppState>, AuthUser(u): AuthUser) -> ApiResult<Json<Value>> {
    let Ok(c) = config() else {
        return Ok(Json(json!({"enabled": false, "linked": false})));
    };
    let subject: Option<String> = sqlx::query_scalar(
        "SELECT subject FROM oidc_identities WHERE issuer = $1 AND user_id = $2",
    )
    .bind(&c.issuer)
    .bind(u.id)
    .fetch_optional(&s.pool)
    .await?;
    Ok(Json(
        json!({"enabled": true, "linked": subject.is_some(), "subject": subject}),
    ))
}

/// Unbind my own identity
pub async fn unlink_me(State(s): State<AppState>, AuthUser(u): AuthUser) -> ApiResult<Json<Value>> {
    let c = config()?;
    let done = sqlx::query("DELETE FROM oidc_identities WHERE issuer = $1 AND user_id = $2")
        .bind(&c.issuer)
        .bind(u.id)
        .execute(&s.pool)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound.into());
    }
    let _ = utopia_store::audit::record(
        &s.pool,
        None,
        u.id,
        "auth.oidc_unlink",
        "user",
        Some(u.id),
        json!({"issuer": c.issuer, "by": "self"}),
    )
    .await;
    Ok(Json(json!({"ok": true})))
}

pub async fn identities(
    State(s): State<AppState>,
    AuthUser(u): AuthUser,
) -> ApiResult<Json<Value>> {
    if !u.is_admin {
        return Err(AppError::Forbidden.into());
    };
    let c = config()?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('user_id', i.user_id, 'subject', i.subject, 'email', u.email)
           FROM oidc_identities i JOIN users u ON u.id = i.user_id
          WHERE i.issuer = $1 AND u.org_id = $2 ORDER BY u.email",
    )
    .bind(&c.issuer)
    .bind(u.org_id)
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(
        json!({"issuer": c.issuer, "client_id": c.client_id, "redirect_uri": c.redirect, "identities": rows}),
    ))
}

/// Admin unbind. **Unbinding only, no binding on someone's behalf** (see the module notes). Unbinding does not invalidate sessions already issued;
/// sessions are stateless JWTs, so cutting off access immediately requires deactivating the account
pub async fn unbind(
    State(s): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    if !u.is_admin {
        return Err(AppError::Forbidden.into());
    };
    let c = config()?;
    let done = sqlx::query(
        "DELETE FROM oidc_identities WHERE issuer = $1 AND user_id = $2
          AND user_id IN (SELECT id FROM users WHERE org_id = $3)",
    )
    .bind(&c.issuer)
    .bind(id)
    .bind(u.org_id)
    .execute(&s.pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound.into());
    }
    let _ = utopia_store::audit::record(
        &s.pool,
        None,
        u.id,
        "auth.oidc_unlink",
        "user",
        Some(id),
        json!({"issuer": c.issuer, "by": "admin"}),
    )
    .await;
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    #[test]
    fn email_verified_is_true_only_for_true_or_the_string_true() {
        let verified = |v: serde_json::Value| {
            let mut claims = serde_json::json!({"sub": "p", "iat": 0, "aud": "a"});
            if !v.is_null() {
                claims["email_verified"] = v;
            }
            serde_json::from_value::<super::Claims>(claims)
                .map(|c| c.email_verified)
                .ok()
        };
        let cases = [
            serde_json::json!(true),
            serde_json::json!("true"),
            serde_json::json!("TRUE"),
            serde_json::json!(false),
            serde_json::json!("false"),
            serde_json::json!(1),
            serde_json::Value::Null,
        ];
        assert_eq!(
            cases.map(verified),
            [
                Some(true),
                Some(true),
                Some(false),
                Some(false),
                Some(false),
                Some(false),
                Some(false)
            ]
        );
    }

    #[test]
    fn a_display_name_is_trimmed_capped_at_64_and_falls_back_to_the_local_part() {
        let long = "名".repeat(70);
        assert_eq!(
            [
                super::display_name(Some("  Ann Lee \n"), "ann@x.test"),
                super::display_name(Some("   "), "bea@x.test"),
                super::display_name(None, "cid@x.test"),
                super::display_name(Some(&long), "dee@x.test"),
            ],
            [
                "Ann Lee".to_string(),
                "bea".into(),
                "cid".into(),
                "名".repeat(64)
            ]
        );
    }

    #[test]
    fn only_local_callbacks_can_use_http() {
        assert!(super::secure_url("http://localhost:1516/callback", true).is_ok());
        assert!(super::secure_url("http://[::1]:1516/callback", true).is_ok());
        assert!(super::secure_url("http://example.org/callback", true).is_err());
        assert!(super::secure_url("http://localhost:1516", false).is_err());
        assert!(super::secure_url("https://user:password@example.org", false).is_err());
    }

    #[test]
    fn a_client_secret_is_form_encoded_before_basic_auth() {
        assert_eq!(super::form_encode("abc"), "abc");
        assert_eq!(super::form_encode("a:b%c+d e"), "a%3Ab%25c%2Bd+e");
    }

    #[tokio::test]
    async fn a_response_over_its_limit_is_refused() {
        let big = axum::http::Response::new(vec![b' '; 2048]);
        assert!(super::read_json(reqwest::Response::from(big), 1024)
            .await
            .is_err());
        let ok = axum::http::Response::new(br#"{"a":1}"#.to_vec());
        assert_eq!(
            super::read_json(reqwest::Response::from(ok), 1024)
                .await
                .unwrap()["a"],
            1
        );
    }

    #[tokio::test]
    async fn a_forced_refresh_waits_out_its_cooldown() {
        let key = "https://id.example.test/cooldown-test";
        assert!(super::force_allowed(key).await);
        assert!(!super::force_allowed(key).await);
        assert!(super::force_allowed("https://id.example.test/another").await);
    }

    #[test]
    fn only_a_verified_email_in_an_allowed_domain_gets_an_account() {
        let claims = |email: &str, verified: bool| {
            serde_json::from_value::<super::Claims>(serde_json::json!({
                "sub": "person-1", "iat": 0, "aud": "utopia-test",
                "email": email, "email_verified": verified,
            }))
            .unwrap()
        };
        let allowed = vec!["re-cinq.com".to_string()];
        assert_eq!(
            super::provisioned_email(&claims("Ann@Re-Cinq.com", true), &allowed),
            Some("ann@re-cinq.com".to_string())
        );
        assert_eq!(
            super::provisioned_email(&claims("ann@re-cinq.com", false), &allowed),
            None
        );
        assert_eq!(
            super::provisioned_email(&claims("ann@gmail.com", true), &allowed),
            None
        );
        assert_eq!(
            super::provisioned_email(&claims("ann@evil-re-cinq.com", true), &allowed),
            None
        );
        assert_eq!(
            super::provisioned_email(&claims("ann@re-cinq.com", true), &[]),
            None
        );
    }

    #[test]
    fn an_entra_email_counts_as_verified_only_when_its_domain_owner_is_verified() {
        let provisioned = |domain_owner_verified: serde_json::Value| {
            let mut claims = serde_json::json!({
                "sub": "person-1", "iat": 0, "aud": "utopia-test", "email": "ann@re-cinq.com",
            });
            if !domain_owner_verified.is_null() {
                claims["xms_edov"] = domain_owner_verified;
            }
            super::provisioned_email(
                &serde_json::from_value::<super::Claims>(claims).unwrap(),
                &["re-cinq.com".to_string()],
            )
        };
        assert_eq!(
            [
                serde_json::json!(true),
                serde_json::json!("true"),
                serde_json::json!(false),
                serde_json::Value::Null,
            ]
            .map(provisioned),
            [
                Some("ann@re-cinq.com".to_string()),
                Some("ann@re-cinq.com".to_string()),
                None,
                None
            ]
        );
    }

    fn config() -> super::Config {
        super::Config {
            issuer: "https://id.example.test".into(),
            client_id: "utopia-test".into(),
            secret: None,
            redirect: "https://utopia.example.test/api/v1/auth/oidc/callback".into(),
            exchange_audiences: vec!["sibling-app".into(), "utopia-test".into()],
            allowed_domains: vec![],
            loopback_http: false,
            scopes: super::scopes(None),
        }
    }

    #[test]
    fn scopes_default_to_openid_profile_email_and_always_keep_openid() {
        assert_eq!(super::scopes(None), "openid profile email");
        assert_eq!(super::scopes(Some("  ")), "openid profile email");
        assert_eq!(super::scopes(Some("openid profile")), "openid profile");
        assert_eq!(
            super::scopes(Some("profile  email")),
            "openid profile email"
        );
        assert_eq!(
            super::scopes(Some("profile openid profile")),
            "profile openid"
        );
    }

    // This key is generated solely for these tests and is never used by the app.
    #[test]
    fn verifies_signature_issuer_audience_nonce_and_expiry() {
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        use serde_json::json;
        let c = config();
        let key = EncodingKey::from_rsa_pem(include_bytes!(
            "../../tests/fixtures/oidc_test_only_key.pem"
        ))
        .unwrap();
        let keys: jsonwebtoken::jwk::JwkSet = serde_json::from_str(include_str!(
            "../../tests/fixtures/oidc_test_only_jwks.json"
        ))
        .unwrap();
        let now = chrono::Utc::now().timestamp();
        let valid = json!({"iss":c.issuer,"aud":c.client_id,"sub":"person-123","nonce":"expected","iat":now,"exp":now+300});
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("fixture".into());
        let token = encode(&header, &valid, &key).unwrap();
        assert!(super::verify_token(&c, &token, &keys, "expected").is_ok());
        assert!(super::verify_token(&c, &token, &keys, "wrong").is_err());
        for (field, value) in [
            ("iss", json!("https://other.example.test")),
            ("aud", json!("other-client")),
            ("exp", json!(now - 3600)),
            ("iat", json!(now + 3600)),
            ("azp", json!("other-client")),
            ("sub", json!("")),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            let token = encode(&header, &invalid, &key).unwrap();
            assert!(
                super::verify_token(&c, &token, &keys, "expected").is_err(),
                "accepted invalid {field}"
            );
        }
        let fake = encode(
            &Header::default(),
            &valid,
            &EncodingKey::from_secret(b"not-an-rsa-key"),
        )
        .unwrap();
        assert!(super::verify_token(&c, &fake, &keys, "expected").is_err());
        header.kid = Some("unknown-key".into());
        assert!(super::verify_token(
            &c,
            &encode(&header, &valid, &key).unwrap(),
            &keys,
            "expected"
        )
        .is_err());

        // Without a `kid`: use it when the JWKS has exactly one RSA key; if more than one, do not guess
        header.kid = None;
        let no_kid = encode(&header, &valid, &key).unwrap();
        assert!(super::verify_token(&c, &no_kid, &keys, "expected").is_ok());
        let mut two = keys.clone();
        let mut second = two.keys[0].clone();
        second.common.key_id = Some("other".into());
        two.keys.push(second);
        assert!(super::verify_token(&c, &no_kid, &two, "expected").is_err());
    }

    #[test]
    fn exchange_accepts_only_fresh_tokens_of_a_trusted_client() {
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        use serde_json::json;
        let c = config();
        let key = EncodingKey::from_rsa_pem(include_bytes!(
            "../../tests/fixtures/oidc_test_only_key.pem"
        ))
        .unwrap();
        let keys: jsonwebtoken::jwk::JwkSet = serde_json::from_str(include_str!(
            "../../tests/fixtures/oidc_test_only_jwks.json"
        ))
        .unwrap();
        let now = chrono::Utc::now().timestamp();
        let valid = json!({"iss":c.issuer,"aud":c.client_id,"sub":"person-123","nonce":"theirs","iat":now,"exp":now+300});
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("fixture".into());
        let sign = |claims: &serde_json::Value| encode(&header, claims, &key).unwrap();
        assert_eq!(
            super::verify_exchange_token(&c, &sign(&valid), &keys)
                .unwrap()
                .sub,
            "person-123"
        );
        for (field, value) in [
            ("aud", json!("other-client")),
            ("aud", json!([c.client_id, "other-client"])),
            ("azp", json!("other-client")),
            ("iat", json!(now - 3600)),
            ("exp", json!(now - 3600)),
            ("iss", json!("https://other.example.test")),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(
                super::verify_exchange_token(&c, &sign(&invalid), &keys).is_err(),
                "accepted invalid {field}"
            );
        }
        let mut multi = valid.clone();
        multi["aud"] = json!([c.client_id, "other-client"]);
        multi["azp"] = json!(c.client_id);
        assert!(super::verify_exchange_token(&c, &sign(&multi), &keys).is_ok());

        // Another application uses its own client id; one not in the trusted list is still refused
        let mut sibling = valid.clone();
        sibling["aud"] = json!("sibling-app");
        assert!(super::verify_exchange_token(&c, &sign(&sibling), &keys).is_ok());
        let closed = super::Config {
            exchange_audiences: Vec::new(),
            ..config()
        };
        assert!(super::verify_exchange_token(&closed, &sign(&valid), &keys).is_err());
    }
}
