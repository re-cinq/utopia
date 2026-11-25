//! The ledger left by one mapping-exploration round — run against a real DB (#503).
//!
//! Both of these were impossible before:
//!
//! 1. **Coverage can be answered.** Previously a round of exploration left only a few rows in `concept_mappings`,
//!    and eleven proposals against a wide eighty-column table looked exactly the same as eleven covering a small KB.
//! 2. **A round that failed also has a ledger.** Previously, failure and "ran but proposed nothing" both showed as "no new proposals" on the page,
//!    while the things to do were entirely different.

use sqlx::PgPool;
use utopia_store::exploration_runs as runs;
use uuid::Uuid;

async fn fixture(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid)> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'run-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'run-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'run-test')")
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    Ok((org, kb))
}

#[tokio::test]
async fn a_run_says_what_it_scanned_and_what_it_dropped() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (_org, kb) = fixture(&pool).await?;

    let run = async {
        let id = runs::start(&pool, kb).await?;
        runs::scanned(
            &pool,
            id,
            &["warehouse".into()],
            8,
            61,
            false,
            24,
        )
        .await?;
        runs::finish(
            &pool,
            id,
            12,
            9,
            serde_json::json!({
                runs::drop_reason::SOURCE: { "n": 3, "example": "model said \"tpch\", mounted: tpch-2026" }
            }),
            &["tpch.lineitem".into(), "tpch.orders".into()],
        )
        .await?;

        let got = runs::recent(&pool, kb, 10).await?;
        assert_eq!(got.len(), 1);
        let r = &got[0];
        assert_eq!((r.tables_scanned, r.columns_scanned, r.cap), (8, 61, 24));
        // **Returned 12, stored 9**: the difference between them is what was dropped; the details are in dropped.
        // Previously none of these three numbers existed, and the page only showed the 9 stored
        assert_eq!((r.returned, r.accepted), (12, 9));
        assert_eq!(r.dropped[runs::drop_reason::SOURCE]["n"], 3);
        assert!(
            r.dropped[runs::drop_reason::SOURCE]["example"]
                .as_str()
                .is_some_and(|s| s.contains("mounted")),
            "光有计数诊断不动，例子要留住"
        );
        assert_eq!(r.tables_covered.len(), 2, "覆盖率的分子");
        assert!(r.finished_at.is_some());
        assert!(r.error.is_none());
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Delete only the KB, not the org — users are soft-deleted, and the test should not construct an action that does not exist in the product
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    run
}

#[tokio::test]
async fn a_failed_run_is_not_an_empty_one() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (_org, kb) = fixture(&pool).await?;

    let run = async {
        let id = runs::start(&pool, kb).await?;
        runs::fail(&pool, id, "Chat model not configured").await?;

        let got = runs::recent(&pool, kb, 10).await?;
        assert_eq!(got.len(), 1, "跑挂了的那一轮也该留下一行");
        // The difference between this row and "ran to completion but accepted = 0" lies here. On the page both
        // used to show as "no new proposals", while one has to go and configure the model and the other has to go and annotate the columns
        assert_eq!(got[0].accepted, 0);
        assert!(got[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Chat model")));
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    run
}
