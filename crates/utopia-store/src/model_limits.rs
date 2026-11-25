//! Per-model concurrency limits.
//!
//! The real constraint is the provider's rate limit, and that is per (base_url, model) -- a local Ollama
//! may only handle 2 concurrent calls, while a hosted API can take 50. Until now a single deployment-level `worker_concurrency`
//! governed all tasks, which means one number was managing two completely different things.
//!
//! A model with no configuration falls back to `deployment_settings.default_model_concurrency` (default 10).

use sqlx::PgPool;
use utopia_core::models::ModelLimit;
use utopia_core::AppResult;

/// How many concurrent calls this model allows. With no model-specific config it falls back to the deployment default.
///
/// Checked once before each LLM call -- compared to a call that often takes twenty-odd seconds, the cost of this query can
/// be ignored. The gate also keeps a copy: when an admin changes the limit, `ModelGates::refresh` re-reads it and immediately
/// updates the gate (#1302), so tasks waiting at the gate need not wait for the next call to read it.
pub async fn limit_for(pool: &PgPool, base_url: &str, model: &str) -> AppResult<usize> {
    let row: Option<(i32,)> = sqlx::query_as(
        "SELECT max_concurrent FROM model_concurrency WHERE base_url = $1 AND model = $2",
    )
    .bind(base_url)
    .bind(model)
    .fetch_optional(pool)
    .await?;
    if let Some((n,)) = row {
        return Ok(n.max(1) as usize);
    }
    let (dflt,): (i32,) =
        sqlx::query_as("SELECT default_model_concurrency FROM deployment_settings LIMIT 1")
            .fetch_optional(pool)
            .await?
            .unwrap_or((10,));
    Ok(dflt.max(1) as usize)
}

/// Configured models + deployment default (fetched in one go for the admin page).
pub async fn list(pool: &PgPool) -> AppResult<(Vec<ModelLimit>, i32)> {
    let rows: Vec<ModelLimit> = sqlx::query_as(
        "SELECT base_url, model, max_concurrent FROM model_concurrency ORDER BY base_url, model",
    )
    .fetch_all(pool)
    .await?;
    let (dflt,): (i32,) =
        sqlx::query_as("SELECT default_model_concurrency FROM deployment_settings LIMIT 1")
            .fetch_optional(pool)
            .await?
            .unwrap_or((10,));
    Ok((rows, dflt))
}

/// Set one model's concurrency. `max_concurrent` of None means remove the model-specific config and fall back to the default.
pub async fn set(
    pool: &PgPool,
    base_url: &str,
    model: &str,
    max_concurrent: Option<i32>,
) -> AppResult<()> {
    match max_concurrent {
        Some(n) => {
            if !(1..=256).contains(&n) {
                return Err(utopia_core::AppError::invalid(
                    "concurrency_range",
                    "max_concurrent must be between 1 and 256",
                ));
            }
            sqlx::query(
                "INSERT INTO model_concurrency (base_url, model, max_concurrent)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (base_url, model)
                 DO UPDATE SET max_concurrent = EXCLUDED.max_concurrent, updated_at = now()",
            )
            .bind(base_url)
            .bind(model)
            .bind(n)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM model_concurrency WHERE base_url = $1 AND model = $2")
                .bind(base_url)
                .bind(model)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// Deployment default concurrency (used by any model without its own setting).
pub async fn set_default(pool: &PgPool, value: i32) -> AppResult<()> {
    if !(1..=256).contains(&value) {
        return Err(utopia_core::AppError::invalid(
            "concurrency_range",
            "default_model_concurrency must be between 1 and 256",
        ));
    }
    sqlx::query("UPDATE deployment_settings SET default_model_concurrency = $1")
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

/// Models actually in use in the deployment (those that appeared in any workspace setting), for the admin page to list as configurable.
pub async fn models_in_use(pool: &PgPool) -> AppResult<Vec<(String, String, String)>> {
    Ok(sqlx::query_as(
        "SELECT DISTINCT chat_base_url, chat_model, 'chat' FROM llm_settings
          WHERE chat_base_url IS NOT NULL AND chat_model IS NOT NULL
         UNION
         SELECT DISTINCT embed_base_url, embed_model, 'embed' FROM llm_settings
          WHERE embed_base_url IS NOT NULL AND embed_model IS NOT NULL",
    )
    .fetch_all(pool)
    .await?)
}
