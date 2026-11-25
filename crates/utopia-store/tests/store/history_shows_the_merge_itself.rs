//! The merge itself also enters this knowledge axis (the #337 clock is wired into `entity_history`), run against a real database.
//!
//! Previously History could only see the **facts the merge voided in passing** (`merged`), not the merge itself:
//! open a merged entity and the interface states the outcome, not what happened. On this axis a merge is
//! the largest single change of knowledge — from then on it and another entity count as one thing.
//!
//! Three things each break in their own way, so they are asserted separately:
//! - **Direction**: whom it absorbed (`merged_in`) and who absorbed it (`merged_away`) are two different things on the graph,
//!   and rollback is also done by direction; saying both ends in one sentence leaves the reader unable to tell who merged into whom
//! - **Attribution of a revert**: the person who reverts is often not the person who merged; an automatic merge's row keeps `merged_by`
//!   NULL — so the revert entry must take the person from the audit log, not borrow it from the merge row
//! - **Total count**: the paginated total misses this branch, so paging to the last page comes up a few short

use sqlx::PgPool;
use uuid::Uuid;

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

struct Fixture {
    org: Uuid,
    kb: Uuid,
    /// The one that remains
    keeper: Uuid,
    /// Merged into keeper in April, still merged
    absorbed: Uuid,
    /// Merged into keeper in May, reverted in June
    returned: Uuid,
    /// The person who did the revert
    reverter: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let person = Uuid::now_v7();
    let (keeper, absorbed, returned) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let reverter = Uuid::now_v7();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'history-merge-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'history-merge-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'history-merge-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO users (id, org_id, email, display_name, password_hash)
         VALUES ($1, $2, $3, 'Reverter', 'x')",
    )
    .bind(reverter)
    .bind(org)
    .bind(format!("reverter-{}@test.local", reverter.simple()))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'person', 'Person')",
    )
    .bind(person)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [
        (keeper, "Zhang Wei"),
        (absorbed, "Zhang Wei"),
        (returned, "Zhang Wei"),
    ] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name, created_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(kb)
        .bind(person)
        .bind(name)
        .bind(t("2026-02-01T00:00:00Z"))
        .execute(pool)
        .await?;
    }

    // April: absorbed merged into keeper, decided automatically (merged_by empty)
    utopia_store::resolution::merge_entities(pool, kb, absorbed, keeper, None, "test").await?;
    backdate(pool, absorbed, "2026-04-01T00:00:00Z", None).await?;

    // May: returned merged into keeper; reverted by a person in June
    let merge_id =
        utopia_store::resolution::merge_entities(pool, kb, returned, keeper, None, "test").await?;
    utopia_store::resolution::revert_merge(pool, kb, merge_id).await?;
    backdate(
        pool,
        returned,
        "2026-05-01T00:00:00Z",
        Some("2026-06-01T00:00:00Z"),
    )
    .await?;
    // The revert's person is recorded in the audit log (`merge.revert`); history takes attribution from there
    utopia_store::audit::record(
        pool,
        Some(kb),
        reverter,
        "merge.revert",
        "merge",
        Some(merge_id),
        serde_json::json!({}),
    )
    .await?;

    Ok(Fixture {
        org,
        kb,
        keeper,
        absorbed,
        returned,
        reverter,
    })
}

/// The merge moment is recorded by `now()`; the test wants a fixed date.
async fn backdate(
    pool: &PgPool,
    source: Uuid,
    created: &str,
    reverted: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE entity_merges SET created_at = $2, reverted_at = $3 WHERE source_id = $1")
        .bind(source)
        .bind(t(created))
        .bind(reverted.map(t))
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
async fn history_carries_the_merge_and_its_direction() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    // The side that absorbed them: two merges plus one revert
    let (events, total) = utopia_store::graph::entity_history(&pool, f.kb, f.keeper, 50, 0).await?;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "merged_in").count(),
        2,
        "两次都是别人并进来：{kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| **k == "merge_reverted").count(),
        1,
        "六月那次撤销要在：{kinds:?}"
    );
    assert!(
        !kinds.contains(&"merged_away"),
        "keeper 从没被并走过：{kinds:?}"
    );
    assert_eq!(total, events.len() as i64, "总数漏了合并这一支");

    // The other party's name must be present, otherwise the interface would only say "an entity was merged in"
    assert!(
        events
            .iter()
            .filter(|e| e.kind == "merged_in")
            .all(|e| e.other_name.as_deref() == Some("Zhang Wei")),
        "并入事件要带对方实体的名字"
    );

    // Attribution: the merge was decided automatically by the engine (merged_by empty), the revert was done by a person
    let merged_in = events.iter().find(|e| e.kind == "merged_in").unwrap();
    assert!(
        merged_in.actor_name.is_none(),
        "自动合并没有人，归因该是空（界面显示为引擎）"
    );
    let reverted = events.iter().find(|e| e.kind == "merge_reverted").unwrap();
    assert_eq!(
        reverted.actor_name.as_deref(),
        Some("Reverter"),
        "撤销的人从审计里取，而不是借合并行的 merged_by"
    );

    // The side that was merged away: the opposite direction, and it is visible that it disappeared in April
    let (events, _) = utopia_store::graph::entity_history(&pool, f.kb, f.absorbed, 50, 0).await?;
    let away: Vec<_> = events.iter().filter(|e| e.kind == "merged_away").collect();
    assert_eq!(away.len(), 1, "它并进了别人，只有这一次");
    assert_eq!(away[0].at, t("2026-04-01T00:00:00Z"));
    assert!(
        !events.iter().any(|e| e.kind == "merged_in"),
        "它没吸收过谁"
    );

    // The side that was reverted: merged away and back again, both entries are there
    let (events, _) = utopia_store::graph::entity_history(&pool, f.kb, f.returned, 50, 0).await?;
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains(&"merged_away") && kinds.contains(&"merge_reverted"),
        "并出去与撤销都要在：{kinds:?}"
    );

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(f.reverter)
        .execute(&pool)
        .await?;
    let gone = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    assert_eq!(gone.rows_affected(), 1, "一次性 org 没删掉");
    Ok(())
}
