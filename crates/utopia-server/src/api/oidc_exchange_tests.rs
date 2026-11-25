//! Exchanging a session goes through the full route: the discovery document and JWKS come from a fake identity provider on this machine; mapping and single-use checks go through the real database.
use crate::state::AppState;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::get,
    Json, Router,
};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

// This key is generated only for the test; the application never uses it
#[tokio::test]
async fn exchange_maps_only_linked_active_people_and_spends_each_token_once() -> anyhow::Result<()>
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let issuer = format!("http://{}", listener.local_addr()?);
    let discovery = json!({"issuer": issuer, "jwks_uri": format!("{issuer}/jwks")});
    let jwks: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/oidc_test_only_jwks.json"
    ))?;
    let provider = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || async move { Json(discovery) }),
        )
        .route("/jwks", get(move || async move { Json(jwks) }));
    let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });

    // Turning off password login only takes effect once SSO is configured: the config is read from process environment variables, so this sits in the same test as the session exchange
    let dir = tempfile::tempdir()?;
    let no_password = AppState::new(
        pool.clone(),
        &utopia_core::config::AppConfig {
            data_dir: dir.path().to_string_lossy().into_owned(),
            password_login: false,
            ..Default::default()
        },
        Arc::new(utopia_search::SearchIndex::open(
            &dir.path().join("search"),
        )?),
        "test-only".into(),
    );
    let call = |method: &'static str, path: &'static str, body: Value| {
        let state = no_password.clone();
        async move {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?;
            let response = super::router(state, &Default::default())
                .oneshot(request)
                .await?;
            let body = axum::body::to_bytes(response.into_body(), 65536).await?;
            anyhow::Ok(serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null))
        }
    };
    let credentials = json!({"email": format!("{}@example.test", Uuid::now_v7()), "password": "wrong-but-long-1"});
    let password_rule = || async {
        let login = call("POST", "/api/v1/auth/login", credentials.clone()).await?;
        let status = call("GET", "/api/v1/auth/oidc/status", Value::Null).await?;
        anyhow::Ok((login["code"].clone(), status["password_login"].clone()))
    };
    assert_eq!(
        password_rule().await?,
        (Value::Null, json!(true)),
        "without SSO the switch is ignored"
    );

    // Only this test reads and writes these variables
    std::env::set_var("UTOPIA_OIDC_ISSUER", &issuer);
    std::env::set_var("UTOPIA_OIDC_CLIENT_ID", "utopia-test");
    std::env::set_var(
        "UTOPIA_OIDC_REDIRECT_URI",
        "http://localhost/api/v1/auth/oidc/callback",
    );
    std::env::set_var("UTOPIA_OIDC_ALLOW_LOOPBACK_HTTP", "1");
    std::env::remove_var("UTOPIA_OIDC_EXCHANGE_AUDIENCES");
    assert_eq!(
        password_rule().await?,
        (json!("password_login_disabled"), json!(false)),
        "with SSO configured the switch holds"
    );

    let (org, user) = (Uuid::now_v7(), Uuid::now_v7());
    let subject = format!("person-{user}");
    let bea_email = format!("{}@example.test", Uuid::now_v7().simple());
    let dir = tempfile::tempdir()?;
    let cfg = utopia_core::config::AppConfig {
        data_dir: dir.path().to_string_lossy().into_owned(),
        ..Default::default()
    };
    let state = AppState::new(
        pool.clone(),
        &cfg,
        Arc::new(utopia_search::SearchIndex::open(
            &dir.path().join("search"),
        )?),
        "test-only".into(),
    );
    let result = async {
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES('{org}','exchange-test');
             INSERT INTO users(id,org_id,email,password_hash,display_name) VALUES('{user}','{org}','{user}@example.test','unused','Test');
             INSERT INTO oidc_identities(issuer,subject,user_id) VALUES('{issuer}','{subject}','{user}');"
        ))
        .execute(&pool)
        .await?;
        let key = EncodingKey::from_rsa_pem(include_bytes!(
            "../../tests/fixtures/oidc_test_only_key.pem"
        ))?;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("fixture".into());
        // `jti` only makes each issued token different; Utopia does not read it
        let sign_at = |sub: &str, issued_ago: i64, expires_in: i64| {
            let now = chrono::Utc::now().timestamp();
            encode(
                &header,
                &json!({"iss": issuer, "aud": "sibling-app", "sub": sub, "iat": now - issued_ago, "exp": now + expires_in, "jti": Uuid::now_v7()}),
                &key,
            )
        };
        let sign = |sub: &str| sign_at(sub, 0, 300);
        let exchange = |id_token: String| {
            let state = state.clone();
            async move {
                let request = Request::builder()
                    .method("POST")
                    .uri("/api/v1/auth/oidc/exchange")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"id_token": id_token}).to_string()))?;
                let response = super::router(state, &Default::default())
                    .oneshot(request)
                    .await?;
                let status = response.status();
                let body = axum::body::to_bytes(response.into_body(), 65536).await?;
                anyhow::Ok((status, serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null)))
            }
        };

        // No trusted audience configured: the endpoint does not exist
        let (status, _) = exchange(sign(&subject)?).await?;
        anyhow::ensure!(status == StatusCode::NOT_FOUND, "closed: {status}");
        std::env::set_var("UTOPIA_OIDC_EXCHANGE_AUDIENCES", "other-app, sibling-app");

        let token = sign(&subject)?;
        let (status, body) = exchange(token.clone()).await?;
        anyhow::ensure!(status == StatusCode::OK, "linked: {status} {body}");
        anyhow::ensure!(crate::auth::decode_user_id(&state, body["token"].as_str().unwrap())? == user);
        let expires_at: chrono::DateTime<chrono::Utc> =
            serde_json::from_value(body["expires_at"].clone())?;
        anyhow::ensure!(expires_at > chrono::Utc::now() + chrono::Duration::days(6));
        let audited: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE actor_id = $1 AND action = 'auth.oidc_exchange'",
        )
        .bind(user)
        .fetch_one(&pool)
        .await?;
        anyhow::ensure!(audited == 1);

        let (status, body) = exchange(token).await?;
        anyhow::ensure!(body["code"] == "oidc_replayed", "replay: {status} {body}");

        // A token just past its `exp` is still inside the validation grace period and passes: a record of an exchange already used must not expire before it,
        // otherwise within that minute it could be exchanged again
        let late = sign_at(&subject, 100, -30)?;
        let (status, body) = exchange(late.clone()).await?;
        anyhow::ensure!(status == StatusCode::OK, "within leeway: {status} {body}");
        let (status, body) = exchange(late).await?;
        anyhow::ensure!(body["code"] == "oidc_replayed", "replay within leeway: {status} {body}");

        let (status, body) = exchange(sign("nobody-linked-this")?).await?;
        anyhow::ensure!(body["code"] == "oidc_unlinked", "unlinked: {status} {body}");

        sqlx::query("UPDATE users SET deactivated_at = now() WHERE id = $1")
            .bind(user)
            .execute(&pool)
            .await?;
        let (status, body) = exchange(sign(&subject)?).await?;
        anyhow::ensure!(body["code"] == "oidc_unlinked", "deactivated: {status} {body}");

        // Domains on the allow-list: a verified email links to an existing account; if there is none, one is opened; the person is recognised by subject afterwards
        std::env::set_var("UTOPIA_OIDC_ALLOWED_DOMAINS", "example.test");
        let sign_email = |sub: &str, email: &str, verified: bool| {
            let now = chrono::Utc::now().timestamp();
            encode(
                &header,
                &json!({"iss": issuer, "aud": "sibling-app", "sub": sub, "iat": now, "exp": now + 300, "jti": Uuid::now_v7(), "email": email, "email_verified": verified, "name": "Bea"}),
                &key,
            )
        };
        let ann = Uuid::now_v7();
        sqlx::query("INSERT INTO users(id,org_id,email,password_hash,display_name) VALUES($1,$2,$3,'unused','Ann')")
            .bind(ann)
            .bind(org)
            .bind(format!("{ann}@example.test"))
            .execute(&pool)
            .await?;
        let user_of = |body: &Value| crate::auth::decode_user_id(&state, body["token"].as_str().unwrap_or_default());

        let (status, body) = exchange(sign_email(&format!("sub-{ann}"), &format!("{ann}@EXAMPLE.test"), true)?).await?;
        anyhow::ensure!(status == StatusCode::OK && user_of(&body)? == ann, "existing by email: {status} {body}");

        let duplicate = Uuid::now_v7();
        sqlx::query("INSERT INTO users(id,org_id,email,password_hash,display_name) VALUES($1,$2,$3,'unused','Duplicate Ann')")
            .bind(duplicate)
            .bind(org)
            .bind(format!("{ann}@EXAMPLE.TEST"))
            .execute(&pool)
            .await?;
        let (status, body) = exchange(sign_email("ambiguous-ann", &format!("{ann}@example.test"), true)?).await?;
        anyhow::ensure!(
            status == StatusCode::UNPROCESSABLE_ENTITY
                && body["code"] == "oidc_unavailable"
                && body["error"] == "The identity could not be mapped to an account",
            "ambiguous email: {status} {body}"
        );
        let linked: i64 = sqlx::query_scalar("SELECT count(*) FROM oidc_identities WHERE issuer = $1 AND (subject = 'ambiguous-ann' OR user_id = $2)")
            .bind(&issuer).bind(duplicate).fetch_one(&pool).await?;
        anyhow::ensure!(linked == 0);

        let bea_sub = format!("sub-{}", Uuid::now_v7());
        let (status, body) = exchange(sign_email(&bea_sub, &bea_email, true)?).await?;
        anyhow::ensure!(status == StatusCode::OK, "opened: {status} {body}");
        let bea = user_of(&body)?;
        let (_, body) = exchange(sign_email(&bea_sub, "changed@elsewhere.test", false)?).await?;
        anyhow::ensure!(user_of(&body)? == bea, "linked by subject afterwards: {body}");

        let (status, body) = exchange(sign_email("sub-unverified", &format!("{}@example.test", Uuid::now_v7()), false)?).await?;
        anyhow::ensure!(body["code"] == "oidc_unlinked", "unverified: {status} {body}");

        let accounts = |email: String| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users WHERE lower(email) = lower($1)")
                    .bind(email)
                    .fetch_one(&pool)
                    .await
            }
        };
        // A deactivated person comes back with a new subject: the email is still held by their deactivated account, so no new account is opened
        let (status, body) = exchange(sign_email("sub-comeback", &format!("{user}@example.test"), true)?).await?;
        anyhow::ensure!(body["code"] == "oidc_unlinked", "deactivated email: {status} {body}");
        anyhow::ensure!(accounts(format!("{user}@example.test")).await? == 1);

        // The subject is already bound to a deactivated account, and opening an account with the new email collides: leave nothing behind, and do not return the internal reason to the caller
        let stray = format!("{}@example.test", Uuid::now_v7().simple());
        let (status, body) = exchange(sign_email(&subject, &stray, true)?).await?;
        anyhow::ensure!(
            body["code"] == "oidc_unavailable"
                && body["error"] == "The identity could not be mapped to an account",
            "taken: {status} {body}"
        );
        anyhow::ensure!(accounts(stray).await? == 0);
        anyhow::Ok(())
    }
    .await;
    std::env::remove_var("UTOPIA_OIDC_ALLOWED_DOMAINS");
    server.abort();
    sqlx::query("DELETE FROM users WHERE email = $1")
        .bind(&bea_email)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(org)
        .execute(&pool)
        .await?;
    result
}
