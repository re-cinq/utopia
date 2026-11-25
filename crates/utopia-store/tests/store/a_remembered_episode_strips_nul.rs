//! The `remember` tool strips NUL bytes before writing a memory sentence into a chunk (#665).
//!
//! It shares `utopia_core::without_nul` with `pipeline::process_document`: the two
//! call the same function, so it is tested in one place. The memory path deserves its own test —
//! it does not go through `utopia_ingest::parse`, which the NUL test in `pipeline` does not cover.
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use sqlx::PgPool;
use uuid::Uuid;

#[tokio::test]
async fn a_nul_in_a_remembered_episode_is_stripped_before_chunks_text() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for (q, vals) in [
        (
            "INSERT INTO organizations (id, name) VALUES ($1, 'remember-nul-test')",
            vec![org],
        ),
        (
            "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'remember-nul-test')",
            vec![ws, org],
        ),
        (
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'remember-nul-test')",
            vec![kb, ws],
        ),
    ] {
        let mut stmt = sqlx::query(q);
        for v in vals {
            stmt = stmt.bind(v);
        }
        stmt.execute(&pool).await?;
    }

    // Three kinds of NUL: inside a word, between words, and runs of NUL. `serde_json` turns JSON's \u0000 back into `\0`,
    // and that is the path the model / MCP write-back takes.
    let raw = "Acme\0moved\0its\0\0HQ to Shenzhen on 2026-03-15.";
    let (_doc, chunk) =
        utopia_store::memory::append_episode(&pool, kb, raw, chrono::Utc::now()).await?;

    let (text, char_end): (String, i32) =
        sqlx::query_as("SELECT text, char_end FROM chunks WHERE id = $1")
            .bind(chunk)
            .fetch_one(&pool)
            .await?;

    assert!(!text.contains('\0'), "入库后还剩 NUL：{text:?}");
    // The `[YYYY-MM-DD HH:MM] ` prefix is generated from the time at the moment; the assertion checks only what comes **after** it:
    // once the words are stripped they should be joined back as they were, with the whole NUL run gone, and the NULs between words gone too.
    let stripped = text.split_once("] ").map(|(_, rest)| rest).unwrap_or("");
    assert_eq!(
        stripped, "AcmemoveditsHQ to Shenzhen on 2026-03-15.",
        "剥完 NUL 之后词该照原来拼回去"
    );
    assert_eq!(
        char_end as usize,
        text.chars().count(),
        "char_end 用的是剥完的那份，不是入库前的那份"
    );

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    Ok(())
}
