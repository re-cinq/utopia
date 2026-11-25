//! Entity search results are replayed on the record axis (#307 / 0019).
//!
//! Previously `search_entities` hardcoded `node_sql(None, None)`: the degree subquery took "edges still alive now".
//! On a graph being replayed, a search box's `degree` results still counted today's edges —
//! the same entity read two numbers under two views.
//!
//! All three directions must be asserted, because they can break in different ways:
//! - A retracted edge is **not counted** in `degree` at `as_of = now`
//! - A retracted edge **is counted** in `degree` before the moment it was invalidated (when the predicate is not wired in, it always looks only at "now",
//!   so replay stays empty as before)
//! - An edge whose `recorded_at` is later than T is **not counted** in `degree` at T (writing only a lower bound would show March
//!   the corrections made in April)
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is not set. It builds and tears down its own data and never touches an existing DB.

use sqlx::PgPool;
use uuid::Uuid;

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

#[tokio::test]
async fn search_entities_degree_respects_as_of() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let person_type = Uuid::now_v7();
    let project_type = Uuid::now_v7();
    let leads = Uuid::now_v7();
    // Two person entities, so the search term can hit both at once; results are sorted by degree descending
    let alpha = Uuid::now_v7();
    let beta = Uuid::now_v7();
    // One project entity (the object being pointed at)
    let project_entity = Uuid::now_v7();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'as-of-search-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'as-of-search-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'as-of-search-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    for (id, key, label) in [
        (person_type, "person", "Person"),
        (project_type, "project", "Project"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(&pool)
            .await?;
    }
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'leads', 'leads')",
    )
    .bind(leads)
    .bind(kb)
    .execute(&pool)
    .await?;
    for (id, type_id, name) in [
        (alpha, person_type, "Alpha Person"),
        (beta, person_type, "Beta Person"),
        (project_entity, project_type, "Project Phoenix"),
    ] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name, created_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(kb)
        .bind(type_id)
        .bind(name)
        .bind(t("2026-01-01T00:00:00Z"))
        .execute(&pool)
        .await?;
    }

    // alpha: recorded "leads project_entity" in January, not invalidated
    let f_alpha = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                           confidence, recorded_at)
         VALUES ($1, $2, $3, $4, $5, 0.9, $6)",
    )
    .bind(f_alpha)
    .bind(kb)
    .bind(alpha)
    .bind(leads)
    .bind(project_entity)
    .bind(t("2026-01-15T00:00:00Z"))
    .execute(&pool)
    .await?;

    // beta: recorded "leads project_entity" in January → invalidated in March; a second one recorded in May
    let f_beta_old = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                           confidence, recorded_at)
         VALUES ($1, $2, $3, $4, $5, 0.9, $6)",
    )
    .bind(f_beta_old)
    .bind(kb)
    .bind(beta)
    .bind(leads)
    .bind(project_entity)
    .bind(t("2026-01-20T00:00:00Z"))
    .execute(&pool)
    .await?;
    sqlx::query("UPDATE facts SET invalidated_at = $2 WHERE id = $1 AND invalidated_at IS NULL")
        .bind(f_beta_old)
        .bind(t("2026-03-10T00:00:00Z"))
        .execute(&pool)
        .await?;
    let f_beta_new = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                           confidence, recorded_at)
         VALUES ($1, $2, $3, $4, $5, 0.9, $6)",
    )
    .bind(f_beta_new)
    .bind(kb)
    .bind(beta)
    .bind(leads)
    .bind(project_entity)
    .bind(t("2026-05-10T00:00:00Z"))
    .execute(&pool)
    .await?;

    let find_degree = |rows: &[utopia_core::models::GraphNode], id: Uuid| -> i64 {
        rows.iter()
            .find(|n| n.id == id)
            .map(|n| n.degree)
            .unwrap_or(-1)
    };

    // Now: alpha=1, beta=1 (the old one is invalidated, the new one is alive)
    let now_rows =
        utopia_store::graph::search_entities(&pool, kb, "Person", 50, 0, None, false).await?;
    assert_eq!(find_degree(&now_rows.0, alpha), 1, "当下 alpha degree=1");
    assert_eq!(
        find_degree(&now_rows.0, beta),
        1,
        "当下 beta degree=1（新的那条）"
    );

    // February: alpha has recorded 1; beta's old edge is still there before its invalidation = 1
    let feb_rows = utopia_store::graph::search_entities(
        &pool,
        kb,
        "Person",
        50,
        0,
        Some(t("2026-02-01T00:00:00Z")),
        false,
    )
    .await?;
    assert_eq!(find_degree(&feb_rows.0, alpha), 1, "2 月 alpha degree=1");
    assert_eq!(
        find_degree(&feb_rows.0, beta),
        1,
        "2 月 beta 旧边在作废之前 degree=1"
    );

    // April: alpha is still 1; beta's old edge is invalidated and the new one not yet recorded → 0
    let apr_rows = utopia_store::graph::search_entities(
        &pool,
        kb,
        "Person",
        50,
        0,
        Some(t("2026-04-01T00:00:00Z")),
        false,
    )
    .await?;
    assert_eq!(find_degree(&apr_rows.0, alpha), 1, "4 月 alpha degree=1");
    assert_eq!(
        find_degree(&apr_rows.0, beta),
        0,
        "4 月 beta 旧边已作废、新边还没记下 degree=0"
    );

    // June: alpha is still 1; beta's new edge has been recorded = 1
    let jun_rows = utopia_store::graph::search_entities(
        &pool,
        kb,
        "Person",
        50,
        0,
        Some(t("2026-06-01T00:00:00Z")),
        false,
    )
    .await?;
    assert_eq!(find_degree(&jun_rows.0, alpha), 1, "6 月 alpha degree=1");
    assert_eq!(
        find_degree(&jun_rows.0, beta),
        1,
        "6 月 beta 新边已记下 degree=1"
    );

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    Ok(())
}

/// On replay the list and total are also computed over the entities visible at that time, along the real merge path. A is merged into B in April:
/// searching "Zhang" in March should see A and B, each with one degree, total 2; now only B remains.
/// If only the degree were replayed while the list still filtered by `merged_into IS NULL`, March would see only B
#[tokio::test]
async fn a_search_in_replay_lists_the_entities_of_that_moment() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb, works_at) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (a, b, later, acme, other) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'as-of-search-merge')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'as-of-search-merge')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'as-of-search-merge')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    let run = async {
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'works_at', 'works at')",
        )
        .bind(works_at)
        .bind(kb)
        .execute(&pool)
        .await?;
        for (id, name, created) in [
            (a, "Zhang Wei A", "2026-01-01T00:00:00Z"),
            (b, "Zhang Wei B", "2026-01-01T00:00:00Z"),
            // Created only in May: it should not be in March's search
            (later, "Zhang Wei C", "2026-05-01T00:00:00Z"),
            (acme, "Acme", "2026-01-01T00:00:00Z"),
            (other, "Other Co", "2026-01-01T00:00:00Z"),
        ] {
            sqlx::query(
                "INSERT INTO entities (id, kb_id, canonical_name, created_at) VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(kb)
            .bind(name)
            .bind(t(created))
            .execute(&pool)
            .await?;
        }
        for (subject, object) in [(a, acme), (b, other)] {
            sqlx::query(
                "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence, recorded_at)
                 VALUES ($1, $2, $3, $4, $5, 0.9, $6)",
            )
            .bind(Uuid::now_v7())
            .bind(kb)
            .bind(subject)
            .bind(works_at)
            .bind(object)
            .bind(t("2026-01-10T00:00:00Z"))
            .execute(&pool)
            .await?;
        }
        utopia_store::resolution::merge_entities(&pool, kb, a, b, None, "test").await?;
        sqlx::query("UPDATE entity_merges SET created_at = $2 WHERE source_id = $1")
            .bind(a)
            .bind(t("2026-04-01T00:00:00Z"))
            .execute(&pool)
            .await?;

        let (march, total) = utopia_store::graph::search_entities(
            &pool,
            kb,
            "Zhang",
            10,
            0,
            Some(t("2026-03-01T00:00:00Z")),
            false,
        )
        .await?;
        let mut seen: Vec<(Uuid, i64)> = march.iter().map(|n| (n.id, n.degree)).collect();
        seen.sort();
        let mut want = vec![(a, 1), (b, 1)];
        want.sort();
        assert_eq!(seen, want, "三月：A 还没并进 B，各一度；C 还没建");
        assert_eq!(total, 2);

        let (march_exact, march_exact_total) = utopia_store::graph::search_entities(
            &pool,
            kb,
            "Zhang Wei A",
            10,
            0,
            Some(t("2026-03-01T00:00:00Z")),
            true,
        )
        .await?;
        assert_eq!(
            march_exact.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![a],
            "三月按名字精确找：后来并掉的 A 当时还在"
        );
        assert_eq!(march_exact_total, 1);

        let (now, total) = utopia_store::graph::search_entities(&pool, kb, "Zhang", 10, 0, None, false).await?;
        let ids: Vec<Uuid> = now.iter().map(|n| n.id).collect();
        assert!(ids.contains(&b) && ids.contains(&later) && !ids.contains(&a));
        assert_eq!(total, 2);
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
