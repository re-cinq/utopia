//! The same-name column is replayed on the record axis (#307 / 0019).
//!
//! Previously `same_name_peers` hardcoded `e.merged_into IS NULL`: a "Zhang Wei" merged away in March
//! disappeared at every replay moment, yet what the panel wants to tell a person is precisely "what it was called before the March merge,
//! and whom it shared a name with" — hiding the ambiguity of identity outside of time would make a person treat two independent entities as the
//! same one to merge.
//!
//! Both directions must be pinned:
//!
//! - **The entity merged away in March** reappears when `as_of` is earlier than March (`entity_visible_at`
//!   replay semantics)
//! - **The entity still alive now** appears at every moment
//! - **Facts invalidated now** do not count towards degree, while a fact from March that is still alive should be counted
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is not set. It builds and tears down its own data and never touches an existing DB.

use sqlx::PgPool;
use uuid::Uuid;

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

#[tokio::test]
async fn same_name_peers_respects_as_of() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb, user, person_type) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    // Zhang Wei a: recorded "works_at project_entity" in January, not invalidated; merged into Zhang Wei b in March
    let zhang_wei_a = Uuid::now_v7();
    // Zhang Wei b: recorded "works_at other_entity" in January, not invalidated — it is the target, so it never disappears
    let zhang_wei_b = Uuid::now_v7();
    // Zhang Wei c: recorded "works_at other_entity" in May, not invalidated — never merged
    let zhang_wei_c = Uuid::now_v7();
    let project_entity = Uuid::now_v7();
    let other_entity = Uuid::now_v7();
    let works_at = Uuid::now_v7();
    let merge_id = Uuid::now_v7();
    // Facts on Zhang Wei a: recorded in January, invalidated in March
    let f_a_old = Uuid::now_v7();
    // Facts on Zhang Wei b: recorded in January
    let f_b = Uuid::now_v7();
    // Facts on Zhang Wei c: recorded in May (after the merge)
    let f_c_new = Uuid::now_v7();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'as-of-peers-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'as-of-peers-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'as-of-peers-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO users (id, org_id, email, display_name, password_hash)
         VALUES ($1, $2, $3, 'Peers Tester', 'x')",
    )
    .bind(user)
    .bind(org)
    .bind(format!("peers-{}@test.local", user.simple()))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'person', 'Person')",
    )
    .bind(person_type)
    .bind(kb)
    .execute(&pool)
    .await?;
    for (id, name) in [
        (zhang_wei_a, "Zhang Wei"),
        (zhang_wei_b, "Zhang Wei"),
        (zhang_wei_c, "Zhang Wei"),
    ] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name, created_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(kb)
        .bind(person_type)
        .bind(name)
        .bind(t("2026-01-01T00:00:00Z"))
        .execute(&pool)
        .await?;
    }
    for (id, name) in [
        (project_entity, "Project Phoenix"),
        (other_entity, "Other Co"),
    ] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, canonical_name, created_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(name)
        .bind(t("2026-01-01T00:00:00Z"))
        .execute(&pool)
        .await?;
    }
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'works_at', 'works at')",
    )
    .bind(works_at)
    .bind(kb)
    .execute(&pool)
    .await?;
    for (fid, subject, object, recorded) in [
        (f_a_old, zhang_wei_a, project_entity, "2026-01-15T00:00:00Z"),
        (f_b, zhang_wei_b, other_entity, "2026-01-20T00:00:00Z"),
        (f_c_new, zhang_wei_c, other_entity, "2026-05-10T00:00:00Z"),
    ] {
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                               confidence, recorded_at)
             VALUES ($1, $2, $3, $4, $5, 0.9, $6)",
        )
        .bind(fid)
        .bind(kb)
        .bind(subject)
        .bind(works_at)
        .bind(object)
        .bind(t(recorded))
        .execute(&pool)
        .await?;
    }
    // The fact on Zhang Wei a recorded in January is invalidated in March (cleared before the later merge overwrites it)
    sqlx::query("UPDATE facts SET invalidated_at = $2 WHERE id = $1 AND invalidated_at IS NULL")
        .bind(f_a_old)
        .bind(t("2026-03-10T00:00:00Z"))
        .execute(&pool)
        .await?;
    // Merge a → b in March (writes merged_into directly; this step is normally done inside resolution::merge_entities,
    // but this test is about the visibility of the same-name column, so there is no need to walk the whole merge path)
    sqlx::query(
        "INSERT INTO entity_merges (id, kb_id, source_id, target_id, merged_by, reason, created_at)
         VALUES ($1, $2, $3, $4, $5, 'duplicate person', $6)",
    )
    .bind(merge_id)
    .bind(kb)
    .bind(zhang_wei_a)
    .bind(zhang_wei_b)
    .bind(user)
    .bind(t("2026-03-15T00:00:00Z"))
    .execute(&pool)
    .await?;
    sqlx::query("UPDATE entities SET merged_into = $2 WHERE id = $1")
        .bind(zhang_wei_a)
        .bind(zhang_wei_b)
        .execute(&pool)
        .await?;

    let peers_at = |when: Option<chrono::DateTime<chrono::Utc>>| {
        let pool = pool.clone();
        async move { utopia_store::graph::same_name_peers(&pool, kb, zhang_wei_b, when).await }
    };
    let names = |rows: Vec<utopia_core::models::GraphNode>| -> Vec<String> {
        let mut v: Vec<String> = rows.into_iter().map(|n| n.name).collect();
        v.sort();
        v
    };

    // Now: a has been merged, leaving only b and c
    let now = peers_at(None).await?;
    assert_eq!(
        names(now),
        vec!["Zhang Wei".to_string()],
        "当下只剩独立的 Zhang Wei"
    );

    // February: a is still active, and c has been created → a and c both share a name with b
    let feb = peers_at(Some(t("2026-02-01T00:00:00Z"))).await?;
    assert_eq!(
        names(feb),
        vec!["Zhang Wei".to_string(), "Zhang Wei".to_string()],
        "2 月：合并之前，a 与 b 同名"
    );

    // April: a has been merged and c has been created → only c
    let apr = peers_at(Some(t("2026-04-01T00:00:00Z"))).await?;
    assert_eq!(
        names(apr),
        vec!["Zhang Wei".to_string()],
        "4 月：a 已合并，只剩 c"
    );

    // June: after the merge, c is the only independent same-name entity left
    let jun = peers_at(Some(t("2026-06-01T00:00:00Z"))).await?;
    assert_eq!(
        names(jun),
        vec!["Zhang Wei".to_string()],
        "6 月：a 已合并，c 已创建"
    );

    // Tear down the fixture: the merge record's merged_by points at a user, so delete that user first, then the organisation (cascading to the KB)
    sqlx::query("DELETE FROM entity_merges WHERE id = $1")
        .bind(merge_id)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    Ok(())
}

/// Takes the real merge path: `merge_entities` moves the merged entity's facts onto the target. On replay to before the merge,
/// the merged-away entity's degree in the same-name column must be counted by who held the facts **at that time** — consistent with the canvas and the entity panel.
/// Filtering facts on the record axis alone, without rewinding the subject, would show 0 for it even at moments before the merge
#[tokio::test]
async fn a_merged_peer_keeps_its_degree_before_the_merge() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb, person, works_at) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (a, b, acme, other) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'as-of-peers-merge')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'as-of-peers-merge')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'as-of-peers-merge')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    let run = async {
        sqlx::query(
            "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'person', 'Person')",
        )
        .bind(person)
        .bind(kb)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'works_at', 'works at')",
        )
        .bind(works_at)
        .bind(kb)
        .execute(&pool)
        .await?;
        for (id, name, typed) in [
            (a, "Zhang Wei", true),
            (b, "Zhang Wei", true),
            (acme, "Acme", false),
            (other, "Other Co", false),
        ] {
            sqlx::query(
                "INSERT INTO entities (id, kb_id, type_id, canonical_name, created_at)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(id)
            .bind(kb)
            .bind(typed.then_some(person))
            .bind(name)
            .bind(t("2026-01-01T00:00:00Z"))
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

        let feb = Some(t("2026-02-01T00:00:00Z"));
        let peers = utopia_store::graph::same_name_peers(&pool, kb, b, feb).await?;
        let merged = peers
            .iter()
            .find(|n| n.id == a)
            .expect("合并之前 a 在同名列里");
        let (_, panel) = utopia_store::graph::entity_detail(&pool, kb, a, None, feb).await?;
        assert_eq!(panel.len(), 1, "面板上二月的 a 有一条事实");
        assert_eq!(merged.degree, 1, "同名列里的度数与面板一致");
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
