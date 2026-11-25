//! The text-reading service's key follows its provider (0065 decision 2): leaving it empty for the same provider keeps it; leaving it empty after switching provider clears it.
//! The comparison and the write happen in the same SQL statement, so two concurrent saves cannot slip past each other.

use sqlx::PgPool;
use utopia_core::secrets;
use utopia_store::settings;
use uuid::Uuid;

#[tokio::test]
async fn a_reader_keeps_its_key_for_its_provider() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    secrets::init(secrets::generate_key());
    let pool = PgPool::connect(&url).await?;
    let (org, ws) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'reader-key-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    let run = async {
        sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'reader-key-test')")
            .bind(ws)
            .bind(org)
            .execute(&pool)
            .await?;
        let save = |provider: &'static str, key: Option<&'static str>| {
            let pool = pool.clone();
            async move {
                settings::upsert_ocr(
                    &pool,
                    ws,
                    Some(provider),
                    Some("https://reader.example"),
                    key,
                    None,
                    Some("m"),
                )
                .await
            }
        };
        let first = settings::upsert_ocr(&pool, ws, None, None, None, None, None).await?;
        assert_eq!(
            first.ocr_provider, "mineru",
            "old clients default on first save"
        );
        let s = save("mineru", Some("mineru-key")).await?;
        assert_eq!(s.ocr_provider, "mineru");
        assert_eq!(s.ocr_api_key.as_deref(), Some("mineru-key"));
        // Same provider, key left empty: kept
        assert_eq!(
            save("mineru", None).await?.ocr_api_key.as_deref(),
            Some("mineru-key")
        );
        // Switched provider, key left empty: cleared, so Ark does not get MinerU's key
        let switched = save("ark", None).await?;
        assert_eq!(
            (
                switched.ocr_provider.as_str(),
                switched.ocr_api_key.as_deref()
            ),
            ("ark", None)
        );
        assert!(!switched.ocr_ready(), "ark without a key is not ready");
        // Switched provider with a new key: the new one is used
        let keyed = save("ark", Some("ark-key")).await?;
        assert_eq!(keyed.ocr_api_key.as_deref(), Some("ark-key"));
        assert!(keyed.ocr_ready());
        // An old client that only changes the address: a provider and model that are absent must not clear an Ark setup that is already configured.
        let legacy = settings::upsert_ocr(
            &pool,
            ws,
            None,
            Some("https://reader.example/new"),
            None,
            None,
            None,
        )
        .await?;
        assert_eq!(legacy.ocr_provider, "ark");
        assert_eq!(legacy.ocr_model.as_deref(), Some("m"));
        assert_eq!(legacy.ocr_api_key.as_deref(), Some("ark-key"));
        assert!(legacy.ocr_ready());
        // A new client that explicitly chooses a provider can still clear the model; an empty key keeps only that provider's key.
        let cleared = settings::upsert_ocr(
            &pool,
            ws,
            Some("ark"),
            Some("https://reader.example"),
            None,
            None,
            None,
        )
        .await?;
        assert_eq!(cleared.ocr_model, None);
        assert_eq!(cleared.ocr_api_key.as_deref(), Some("ark-key"));
        assert!(!cleared.ocr_ready());
        // What is stored is sealed
        let stored: Option<String> =
            sqlx::query_scalar("SELECT ocr_api_key FROM llm_settings WHERE workspace_id = $1")
                .bind(ws)
                .fetch_one(&pool)
                .await?;
        assert!(secrets::is_sealed(stored.as_deref().unwrap_or_default()));
        // Switching back to MinerU clears it the same way
        assert_eq!(save("mineru", None).await?.ocr_api_key, None);
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
