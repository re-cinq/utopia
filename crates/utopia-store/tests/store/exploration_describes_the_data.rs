//! A store's data description and its conventions are two columns, two write paths — run against a real store (#570).
//!
//! Exploration rewrites the description on every run; conventions a person wrote are not touched by exploration. Mixed into one field, the next exploration would overwrite
//! a person's answer. This test guards exactly "each writes its own, and neither overwrites the other".

use sqlx::PgPool;
use uuid::Uuid;

async fn base(pool: &PgPool) -> anyhow::Result<Uuid> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'describe-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'describe-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'describe-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    Ok(kb)
}

#[tokio::test]
async fn the_description_and_the_conventions_do_not_overwrite_each_other() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let kb = base(&pool).await?;

    let run = async {
        // A person writes the convention first
        let no: Option<&str> = None;
        utopia_store::kbs::update(
            &pool,
            kb,
            no,
            no,
            no,
            None,
            no,
            None,
            None,
            None,
            None,
            Some("Amounts are in cents.\nis_test = 1 is excluded."),
        )
        .await?;
        // Exploration writes the description and the questions
        utopia_store::kbs::set_data_description(
            &pool,
            kb,
            "dw.dwd_ord_dtl: one row per order line.",
            &["Which ord_st values count?".into()],
        )
        .await?;
        let got = utopia_store::kbs::get(&pool, kb).await?;
        assert_eq!(
            got.data_conventions.as_deref(),
            Some("Amounts are in cents.\nis_test = 1 is excluded.")
        );
        assert_eq!(
            got.data_description.as_deref(),
            Some("dw.dwd_ord_dtl: one row per order line.")
        );
        assert_eq!(
            got.data_questions,
            serde_json::json!(["Which ord_st values count?"])
        );

        // Exploration runs again: the description changed, the convention as it was
        utopia_store::kbs::set_data_description(&pool, kb, "second run", &[]).await?;
        let got = utopia_store::kbs::get(&pool, kb).await?;
        assert_eq!(got.data_description.as_deref(), Some("second run"));
        assert_eq!(got.data_questions, serde_json::json!([]));
        assert!(
            got.data_conventions
                .as_deref()
                .is_some_and(|c| c.contains("is_test")),
            "探索不碰人写的约定"
        );

        // A person edits the convention (PATCH sends only this one): the description is unchanged. Sending null is the same as not changing — the same convention as description
        utopia_store::kbs::update(
            &pool,
            kb,
            no,
            no,
            no,
            None,
            no,
            None,
            None,
            None,
            None,
            Some("cents only"),
        )
        .await?;
        let got = utopia_store::kbs::get(&pool, kb).await?;
        assert_eq!(got.data_conventions.as_deref(), Some("cents only"));
        assert_eq!(got.data_description.as_deref(), Some("second run"));
        utopia_store::kbs::update(
            &pool,
            kb,
            Some("renamed"),
            no,
            no,
            None,
            no,
            None,
            None,
            None,
            None,
            no,
        )
        .await?;
        assert_eq!(
            utopia_store::kbs::get(&pool, kb)
                .await?
                .data_conventions
                .as_deref(),
            Some("cents only")
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Delete only the knowledge base, not the org — users are soft-deleted, and the test should not invent an action the product does not have
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    run
}
