//! #246: the source list carries no credentials at all.
//!
//! The list endpoint shown to Viewers used to strip only `auth_header`; object storage, WebDAV and Notion each sent their
//! secrets through as-is. Now the credential keys are listed in one table, `SOURCE_SECRET_KEYS`, and the list SQL strips by that table.
//! Two things are guarded here:
//!
//! 1. **No credential key appears in the list**, tried for every connector kind.
//! 2. **Identifiers remain** (bucket, username, account_name), so the UI can show which account this is;
//!    while the sync path (`sources::get`) still gets the full config — credentials are kept from leaving, not removed.
//!
//! Inserts the table directly rather than through `sources::create`: `KINDS` is missing five kinds (#247), a separate fix.
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is unset. Creates and tears down its own database, never touching an existing one.

use sqlx::PgPool;
use utopia_core::models::SOURCE_SECRET_KEYS;
use utopia_store::sources;
use uuid::Uuid;

async fn seed(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid)> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'secret-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'secret-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'secret-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    Ok((org, kb))
}

#[tokio::test]
async fn a_viewer_never_sees_a_credential() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, kb) = seed(&pool).await?;

    let run = async {
        // One per connector kind, with config keys exactly as each one's UI really writes them
        let fixtures: Vec<(&str, serde_json::Value)> = vec![
            (
                "custom",
                serde_json::json!({ "endpoint": "https://x.test/items", "auth_header": "Bearer c" }),
            ),
            (
                "github_issues",
                serde_json::json!({ "repo": "o/r", "auth_header": "Bearer g" }),
            ),
            (
                "github_repo",
                serde_json::json!({ "repo": "o/r", "auth_header": "Bearer r" }),
            ),
            (
                "gitlab_repo",
                serde_json::json!({ "project": "g/p", "auth_header": "Bearer l" }),
            ),
            (
                "gitlab_issues",
                serde_json::json!({ "project": "g/p", "auth_header": "Bearer i" }),
            ),
            (
                "jira_issues",
                serde_json::json!({ "base_url": "https://j.test", "project": "P", "auth_header": "Basic j" }),
            ),
            (
                "s3",
                serde_json::json!({ "bucket": "b", "region": "r", "access_key_id": "AKIA",
                                    "secret_access_key": "s3-secret" }),
            ),
            (
                "azure_blob",
                serde_json::json!({ "bucket": "c", "account_name": "acct", "account_key": "az-key" }),
            ),
            (
                "gcs",
                serde_json::json!({ "bucket": "g", "service_account_key": "{\"private_key\":\"x\"}" }),
            ),
            (
                "webdav",
                serde_json::json!({ "base_url": "https://d.test", "path": "/", "username": "u",
                                    "password": "dav-pass" }),
            ),
            ("notion", serde_json::json!({ "token": "secret_n", "query": "q" })),
        ];
        for (kind, config) in &fixtures {
            sqlx::query(
                "INSERT INTO sources (id, kb_id, kind, name, config) VALUES ($1, $2, $3, $3, $4)",
            )
            .bind(Uuid::now_v7())
            .bind(kb)
            .bind(kind)
            .bind(config)
            .execute(&pool)
            .await?;
        }

        let listed = sources::list(&pool, kb).await?;
        assert_eq!(listed.len(), fixtures.len());
        for s in &listed {
            let obj = s.config.as_object().expect("config is an object");
            for key in SOURCE_SECRET_KEYS {
                assert!(
                    !obj.contains_key(*key),
                    "{}: `{key}` must not reach a viewer, got {:?}",
                    s.kind,
                    obj
                );
            }
        }
        // Identifiers remain
        let by_kind = |k: &str| {
            listed
                .iter()
                .find(|s| s.kind == k)
                .map(|s| s.config.clone())
                .expect("listed")
        };
        assert_eq!(by_kind("s3")["bucket"], "b");
        assert_eq!(by_kind("s3")["access_key_id"], "AKIA");
        assert_eq!(by_kind("azure_blob")["account_name"], "acct");
        assert_eq!(by_kind("webdav")["username"], "u");
        assert_eq!(by_kind("custom")["endpoint"], "https://x.test/items");
        assert_eq!(by_kind("notion")["query"], "q");

        // The sync path still gets the full config: credentials only do not leave, they are not gone
        for s in &listed {
            let full = sources::get(&pool, s.id).await?;
            let want = &fixtures.iter().find(|(k, _)| *k == s.kind).unwrap().1;
            assert_eq!(&full.config, want, "{}: sync still sees the credentials", s.kind);
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await;
    run
}
