//! The panel's "derivation that did not land" is replayed on the record axis (#307 / 0019).
//!
//! Previously `blocked_for_entity` hardcoded `v.status = 'open'`: a violation detected in March and dismissed by a person in April
//! still hung on the panel during replay, although that row is now `resolved` in `axiom_violations`.
//! `violation_open_at` is the answer, in the same style as 0031's "judge whether it is
//! still open at moment T".
//!
//! All three directions must be pinned:
//!
//! - **Still open now** appears at every moment (`status = 'open'` and held_at are both true)
//! - **Dismissed now** still appears before `decided_at`, and disappears after
//! - **Not yet detected now** does not appear before `detected_at`
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is not set. It builds and tears down its own data and never touches an existing DB.

use sqlx::PgPool;
use uuid::Uuid;

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

#[tokio::test]
async fn blocked_for_entity_respects_as_of() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb, user, person_type, works_at) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let mira = Uuid::now_v7();
    let acme = Uuid::now_v7();
    let ceo_of = Uuid::now_v7();
    let ceo_fact_a = Uuid::now_v7();
    let ceo_fact_b = Uuid::now_v7();
    let works_at_fact = Uuid::now_v7();
    let still_open_violation = Uuid::now_v7();
    let resolved_violation = Uuid::now_v7();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'as-of-blocked-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'as-of-blocked-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'as-of-blocked-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO users (id, org_id, email, display_name, password_hash)
         VALUES ($1, $2, $3, 'Blocked Tester', 'x')",
    )
    .bind(user)
    .bind(org)
    .bind(format!("blocked-{}@test.local", user.simple()))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'person', 'Person')",
    )
    .bind(person_type)
    .bind(kb)
    .execute(&pool)
    .await?;
    for (id, name) in [(mira, "Mira"), (acme, "Acme")] {
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
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'ceo_of', 'ceo of')",
    )
    .bind(ceo_of)
    .bind(kb)
    .execute(&pool)
    .await?;
    // Assertion fact on Mira: used as left_fact by two violations
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, $5, 0.9)",
    )
    .bind(works_at_fact)
    .bind(kb)
    .bind(mira)
    .bind(works_at)
    .bind(acme)
    .execute(&pool)
    .await?;
    // Derived fact: used as right_fact by two violations (must be a fact to fill the FK)
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, $5, 0.9)",
    )
    .bind(ceo_fact_a)
    .bind(kb)
    .bind(mira)
    .bind(ceo_of)
    .bind(acme)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, $5, 0.9)",
    )
    .bind(ceo_fact_b)
    .bind(kb)
    .bind(mira)
    .bind(ceo_of)
    .bind(acme)
    .execute(&pool)
    .await?;
    // "Still open now": detected in March and not dismissed to date
    sqlx::query(
        "INSERT INTO axiom_violations (id, kb_id, kind, left_fact, right_fact, status,
                                      detail, detected_at)
         VALUES ($1, $2, 'derived_contradiction', $3, $4, 'open',
                 jsonb_build_object('subject_id', $5::text, 'object_id', $6::text,
                                    'predicate', 'works at'),
                 $7)",
    )
    .bind(still_open_violation)
    .bind(kb)
    .bind(works_at_fact)
    .bind(ceo_fact_a)
    .bind(mira)
    .bind(acme)
    .bind(t("2026-03-15T00:00:00Z"))
    .execute(&pool)
    .await?;
    // "Detected in March, dismissed by a person in April": currently status=resolved, decided_at=April
    sqlx::query(
        "INSERT INTO axiom_violations (id, kb_id, kind, left_fact, right_fact, status,
                                      resolution, decided_at,
                                      detail, detected_at)
         VALUES ($1, $2, 'derived_contradiction', $3, $4, 'resolved',
                 'fact_retracted', $7,
                 jsonb_build_object('subject_id', $5::text, 'object_id', $6::text,
                                    'predicate', 'works at'),
                 $8)",
    )
    .bind(resolved_violation)
    .bind(kb)
    .bind(works_at_fact)
    .bind(ceo_fact_b)
    .bind(mira)
    .bind(acme)
    .bind(t("2026-04-15T00:00:00Z"))
    .bind(t("2026-03-20T00:00:00Z"))
    .execute(&pool)
    .await?;

    let ids = |rows: Vec<utopia_core::models::BlockedDerivation>| -> Vec<Uuid> {
        rows.into_iter().map(|r| r.violation_id).collect()
    };
    let ids_sorted = |rows: Vec<utopia_core::models::BlockedDerivation>| -> Vec<Uuid> {
        let mut v = ids(rows);
        v.sort();
        v
    };

    let blocks = |when: Option<chrono::DateTime<chrono::Utc>>| {
        let pool = pool.clone();
        async move { utopia_store::reasoning::blocked_for_entity(&pool, kb, mira, when).await }
    };

    // Now: the resolved one has been dismissed, the open one remains → only still_open
    let now = blocks(None).await?;
    assert_eq!(
        ids_sorted(now),
        vec![still_open_violation],
        "当下：open 一条"
    );

    // February: neither has been detected yet → neither appears
    let feb = blocks(Some(t("2026-02-01T00:00:00Z"))).await?;
    assert!(feb.is_empty(), "2 月：检测之前都不到");

    // 25 March: resolved is still open before decided_at; still_open is there → both
    let late_mar = blocks(Some(t("2026-03-25T00:00:00Z"))).await?;
    assert_eq!(
        ids_sorted(late_mar),
        vec![still_open_violation, resolved_violation],
        "3 月 25 日：resolved 还在、still_open 还在"
    );

    // May: resolved has disappeared after decided_at, still_open remains → one
    let may = blocks(Some(t("2026-05-01T00:00:00Z"))).await?;
    assert_eq!(
        ids_sorted(may),
        vec![still_open_violation],
        "5 月：resolved 已裁、still_open 还在"
    );

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
