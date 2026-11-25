use sqlx::PgPool;
use utopia_core::models::LlmSettings;
use utopia_core::{secrets, AppError, AppResult};
use uuid::Uuid;

/// Sealed on leaving the store: the four API keys are sealed in the database (`utopia_core::secrets`).
/// Every query that returns `LlmSettings` passes through here
fn opened(mut s: LlmSettings) -> AppResult<LlmSettings> {
    s.chat_api_key = secrets::open_opt(s.chat_api_key.as_deref()).map_err(AppError::Other)?;
    s.embed_api_key = secrets::open_opt(s.embed_api_key.as_deref()).map_err(AppError::Other)?;
    s.ocr_api_key = secrets::open_opt(s.ocr_api_key.as_deref()).map_err(AppError::Other)?;
    s.transcribe_api_key =
        secrets::open_opt(s.transcribe_api_key.as_deref()).map_err(AppError::Other)?;
    Ok(s)
}

pub async fn get(pool: &PgPool, workspace_id: Uuid) -> AppResult<Option<LlmSettings>> {
    let row: Option<LlmSettings> =
        sqlx::query_as("SELECT * FROM llm_settings WHERE workspace_id = $1")
            .bind(workspace_id)
            .fetch_optional(pool)
            .await?;
    row.map(opened).transpose()
}

/// Upsert; passing None for api_key keeps the old value (the frontend does not send keys back).
#[allow(clippy::too_many_arguments)]
pub async fn upsert(
    pool: &PgPool,
    workspace_id: Uuid,
    chat_base_url: Option<&str>,
    chat_api_key: Option<&str>,
    chat_model: Option<&str>,
    embed_base_url: Option<&str>,
    embed_api_key: Option<&str>,
    embed_model: Option<&str>,
    embed_dim: Option<i32>,
) -> AppResult<LlmSettings> {
    // Sealed on entry to the database; None stays None (keeps the old value)
    let chat_api_key = secrets::seal_opt(chat_api_key);
    let embed_api_key = secrets::seal_opt(embed_api_key);
    let row: LlmSettings = sqlx::query_as(
        "INSERT INTO llm_settings
             (workspace_id, chat_base_url, chat_api_key, chat_model,
              embed_base_url, embed_api_key, embed_model, embed_dim, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
         ON CONFLICT (workspace_id) DO UPDATE SET
             chat_base_url  = EXCLUDED.chat_base_url,
             chat_api_key   = COALESCE(EXCLUDED.chat_api_key, llm_settings.chat_api_key),
             chat_model     = EXCLUDED.chat_model,
             embed_base_url = EXCLUDED.embed_base_url,
             embed_api_key  = COALESCE(EXCLUDED.embed_api_key, llm_settings.embed_api_key),
             embed_model    = EXCLUDED.embed_model,
             embed_dim      = EXCLUDED.embed_dim,
             updated_at     = now()
         RETURNING *",
    )
    .bind(workspace_id)
    .bind(chat_base_url)
    .bind(chat_api_key)
    .bind(chat_model)
    .bind(embed_base_url)
    .bind(embed_api_key)
    .bind(embed_model)
    .bind(embed_dim)
    .fetch_one(pool)
    .await?;
    opened(row)
}

/// The reasoning effort of the chat model, changed on its own: it follows the chat model's card, but the whole-row replacement in `upsert` does not know about it,
/// so an old caller that does not pass it should not clear it either. `None` = clear (back to the endpoint default)
pub async fn set_chat_reasoning_effort(
    pool: &PgPool,
    workspace_id: Uuid,
    effort: Option<&str>,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE llm_settings SET chat_reasoning_effort = $2, updated_at = now() WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .bind(effort)
    .execute(pool)
    .await?;
    Ok(())
}

/// Settings for the layout-recognition service, stored separately: it is its own card on the admin page, so saving it must not touch the chat and embedding columns
/// (and the reverse too — `upsert` does not write these three columns). `api_key` None keeps the old value; address None = switch it off.
///
/// `provider` is `mineru` or `ark` (0065). **The old key is not kept when the provider changes**: keeping the key when left blank is meant for
/// "change the address, switch the backend"; sending MinerU's proxy-layer key to Ark is not what anyone wants. The comparison and the write are in
/// the same SQL statement, so two concurrent saves cannot carry the old key across
/// An old client that does not pass `provider` keeps the existing provider and the model it did not pass; new settings default to MinerU.
pub async fn upsert_ocr(
    pool: &PgPool,
    workspace_id: Uuid,
    provider: Option<&str>,
    base_url: Option<&str>,
    api_key: Option<&str>,
    backend: Option<&str>,
    model: Option<&str>,
) -> AppResult<LlmSettings> {
    let api_key = secrets::seal_opt(api_key);
    let row: LlmSettings = sqlx::query_as(
        "INSERT INTO llm_settings
             (workspace_id, ocr_provider, ocr_base_url, ocr_api_key, ocr_backend, ocr_model, updated_at)
         VALUES ($1, COALESCE($2, 'mineru'), $3, $4, $5, $6, now())
         ON CONFLICT (workspace_id) DO UPDATE SET
             ocr_provider = COALESCE($2, llm_settings.ocr_provider),
             ocr_base_url = EXCLUDED.ocr_base_url,
             ocr_api_key  = CASE WHEN COALESCE($2, llm_settings.ocr_provider) <> llm_settings.ocr_provider
                                 THEN EXCLUDED.ocr_api_key
                                 ELSE COALESCE(EXCLUDED.ocr_api_key, llm_settings.ocr_api_key) END,
             ocr_backend  = EXCLUDED.ocr_backend,
             ocr_model    = CASE WHEN $2::text IS NULL THEN COALESCE($6, llm_settings.ocr_model)
                                 ELSE EXCLUDED.ocr_model END,
             updated_at   = now()
         RETURNING *",
    )
    .bind(workspace_id)
    .bind(provider)
    .bind(base_url)
    .bind(api_key)
    .bind(backend)
    .bind(model)
    .fetch_one(pool)
    .await?;
    opened(row)
}

/// Settings for the transcription model, stored separately like the layout-recognition service (each is its own card on the admin page)
pub async fn upsert_transcribe(
    pool: &PgPool,
    workspace_id: Uuid,
    base_url: Option<&str>,
    api_key: Option<&str>,
    model: Option<&str>,
) -> AppResult<LlmSettings> {
    let api_key = secrets::seal_opt(api_key);
    let row: LlmSettings = sqlx::query_as(
        "INSERT INTO llm_settings
             (workspace_id, transcribe_base_url, transcribe_api_key, transcribe_model, updated_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (workspace_id) DO UPDATE SET
             transcribe_base_url = EXCLUDED.transcribe_base_url,
             transcribe_api_key  = COALESCE(EXCLUDED.transcribe_api_key, llm_settings.transcribe_api_key),
             transcribe_model    = EXCLUDED.transcribe_model,
             updated_at          = now()
         RETURNING *",
    )
    .bind(workspace_id)
    .bind(base_url)
    .bind(api_key)
    .bind(model)
    .fetch_one(pool)
    .await?;
    opened(row)
}
