//! Seal backfill: plaintext credentials stored before the upgrade become ciphertext on the first start after the key is installed.
//!
//! Idempotent, runs on every start: the test is whether the value lacks the `enc:v1:` prefix, and sealed values are left alone. Four places hold credentials
//! (the four `llm_settings` keys, `data_sources.conn_string`, the credential keys of `sources.config`,
//! `sources.ingest_token`) are each scanned here — adding a new storage place means adding it here too, otherwise that one
//! stays plaintext with no alarm anywhere.

use sqlx::PgPool;
use utopia_core::models::SOURCE_SECRET_KEYS;
use utopia_core::{secrets, AppResult};
use uuid::Uuid;

/// Returns the number of rows sealed. When no key is installed, does nothing
pub async fn backfill(pool: &PgPool) -> AppResult<usize> {
    if !secrets::is_ready() {
        return Ok(0);
    }
    Ok(seal_llm_settings(pool, None).await?
        + seal_data_sources(pool, None).await?
        + seal_sources(pool, None).await?)
}

/// `only` = backfill only this one workspace (for tests; at startup pass None to scan all)
pub async fn seal_llm_settings(pool: &PgPool, only: Option<Uuid>) -> AppResult<usize> {
    type Keys = (
        Uuid,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Keys> = sqlx::query_as(
        "SELECT workspace_id, chat_api_key, embed_api_key, ocr_api_key, transcribe_api_key
           FROM llm_settings
          WHERE ($1::uuid IS NULL OR workspace_id = $1)
            AND ((chat_api_key IS NOT NULL AND chat_api_key NOT LIKE 'enc:v1:%')
              OR (embed_api_key IS NOT NULL AND embed_api_key NOT LIKE 'enc:v1:%')
              OR (ocr_api_key IS NOT NULL AND ocr_api_key NOT LIKE 'enc:v1:%')
              OR (transcribe_api_key IS NOT NULL AND transcribe_api_key NOT LIKE 'enc:v1:%'))",
    )
    .bind(only)
    .fetch_all(pool)
    .await?;
    let n = rows.len();
    for (ws, chat, embed, ocr, transcribe) in rows {
        sqlx::query(
            "UPDATE llm_settings SET chat_api_key = $2, embed_api_key = $3, ocr_api_key = $4,
                    transcribe_api_key = $5
              WHERE workspace_id = $1",
        )
        .bind(ws)
        .bind(secrets::seal_opt(chat.as_deref()))
        .bind(secrets::seal_opt(embed.as_deref()))
        .bind(secrets::seal_opt(ocr.as_deref()))
        .bind(secrets::seal_opt(transcribe.as_deref()))
        .execute(pool)
        .await?;
    }
    Ok(n)
}

pub async fn seal_data_sources(pool: &PgPool, only: Option<Uuid>) -> AppResult<usize> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, conn_string FROM data_sources
          WHERE ($1::uuid IS NULL OR id = $1) AND conn_string NOT LIKE 'enc:v1:%'",
    )
    .bind(only)
    .fetch_all(pool)
    .await?;
    let n = rows.len();
    for (id, conn) in rows {
        sqlx::query("UPDATE data_sources SET conn_string = $2 WHERE id = $1")
            .bind(id)
            .bind(secrets::seal(&conn))
            .execute(pool)
            .await?;
    }
    Ok(n)
}

/// The sources table is small, so read the whole table back and judge in Rust: credential keys hide inside JSON, which makes SQL awkward here
pub async fn seal_sources(pool: &PgPool, only: Option<Uuid>) -> AppResult<usize> {
    let rows: Vec<(Uuid, serde_json::Value, Option<String>)> = sqlx::query_as(
        "SELECT id, config, ingest_token FROM sources WHERE ($1::uuid IS NULL OR id = $1)",
    )
    .bind(only)
    .fetch_all(pool)
    .await?;
    let mut n = 0;
    for (id, mut config, token) in rows {
        let plain_key = config.as_object().is_some_and(|o| {
            SOURCE_SECRET_KEYS.iter().any(|k| {
                o.get(*k)
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !secrets::is_sealed(s))
            })
        });
        let plain_token = token.as_deref().is_some_and(|t| !secrets::is_sealed(t));
        if !plain_key && !plain_token {
            continue;
        }
        secrets::seal_json_keys(&mut config, SOURCE_SECRET_KEYS);
        sqlx::query("UPDATE sources SET config = $2, ingest_token = $3 WHERE id = $1")
            .bind(id)
            .bind(config)
            .bind(secrets::seal_opt(token.as_deref()))
            .execute(pool)
            .await?;
        n += 1;
    }
    Ok(n)
}
