//! The review desk overview (#377), run against a real database.
//!
//! All three sections are SQL aggregates, and `cargo check` cannot see a single character of them; what is pinned is the counting rules:
//!
//! - **Waiting to be handled** uses the same WHERE as the left-column count, and the extra "oldest" is taken from each table's own
//!   clock (conflicts use created_at, the fact queue uses recorded_at)
//! - **Handled** only counts the four families of actions in the review domain; the 7-day window is a subset of the 30-day window,
//!   and a row with no actor counts as self-retirement, and days are padded out to 14 entries
//! - **Quality** counts only living facts; "contested" means hanging on either end of an open conflict
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is unset. Creates and tears down its own; never touches an existing database.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

struct Seed {
    kb: Uuid,
    org: Uuid,
    ada: Uuid,
    facts: [Uuid; 3],
    conflict_at: DateTime<Utc>,
    review_at: DateTime<Utc>,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Seed> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (etype, rel) = (Uuid::now_v7(), Uuid::now_v7());
    let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (f1, f2, f3) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (ada, bo) = (Uuid::now_v7(), Uuid::now_v7());
    let now = Utc::now();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'review-summary-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'review-summary-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'review-summary-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'thing', 'Thing')",
    )
    .bind(etype)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, $3, $3)")
        .bind(rel)
        .bind(kb)
        .bind(format!("knows_{}", Uuid::now_v7().simple()))
        .execute(pool)
        .await?;
    for (id, name) in [(a, "Ada"), (b, "Bo"), (c, "Cy")] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(etype)
        .bind(name)
        .execute(pool)
        .await?;
    }
    // Three living facts: f2 is low-confidence (0.5 < 0.75), and an open conflict sits between f1 and f3
    for (id, s, o, conf) in [(f1, a, b, 0.9f32), (f2, a, c, 0.5), (f3, b, c, 0.9)] {
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(kb)
        .bind(s)
        .bind(rel)
        .bind(o)
        .bind(conf)
        .execute(pool)
        .await?;
    }
    let conflict_at = now - Duration::days(3);
    sqlx::query(
        "INSERT INTO fact_conflicts (id, kb_id, old_fact_id, new_fact_id, reason, created_at)
         VALUES ($1, $2, $3, $4, 'simultaneous', $5)",
    )
    .bind(Uuid::now_v7())
    .bind(kb)
    .bind(f1)
    .bind(f3)
    .bind(conflict_at)
    .execute(pool)
    .await?;
    let review_at = now - Duration::days(5);
    sqlx::query(
        "INSERT INTO resolution_reviews (id, kb_id, left_id, right_id, score, created_at)
         VALUES ($1, $2, $3, $4, 0.8, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(kb)
    .bind(a)
    .bind(b)
    .bind(review_at)
    .execute(pool)
    .await?;

    // Ledger: Ada has two within 7 days, one self-retirement within 7 days, Bo has one 20 days ago (within 30 days),
    // Ada also has one 40 days ago (outside the window); add one more action outside the review domain, which must not be counted
    let events: [(Option<Uuid>, Option<&str>, &str, i64); 6] = [
        (Some(ada), Some("Ada"), "review.merge", 1),
        (Some(ada), Some("Ada"), "fact.confirm", 2),
        (None, None, "review.keep", 3),
        (Some(bo), Some("Bo"), "conflict.close_old", 20),
        (Some(ada), Some("Ada"), "fact.reject", 40),
        (Some(ada), Some("Ada"), "document.upload", 1),
    ];
    for (actor, label, action, days_ago) in events {
        sqlx::query(
            "INSERT INTO audit_events (id, kb_id, actor_id, actor_label, action, target_kind, created_at)
             VALUES ($1, $2, $3, $4, $5, 'test', $6)",
        )
        .bind(Uuid::now_v7())
        .bind(kb)
        .bind(actor)
        .bind(label)
        .bind(action)
        .bind(now - Duration::days(days_ago))
        .execute(pool)
        .await?;
    }
    Ok(Seed {
        kb,
        org,
        ada,
        facts: [f1, f2, f3],
        conflict_at,
        review_at,
    })
}

async fn teardown(pool: &PgPool, s: &Seed) -> anyhow::Result<()> {
    // The ledger only grows and never deletes (a trigger blocks DELETE), so these rows stay in the database, pointing at a kb_id that no longer exists
    // that nobody can read; the organization does not cascade to the KB, so both must be deleted
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(s.kb)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(s.org)
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
async fn shared_conflict_endpoints_do_not_multiply_health_counts() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let s = seed(&pool).await?;
    let other = seed(&pool).await?;
    let run = async {
        let [f1, f2, f3] = s.facts;
        let (resolved_only, invalidated) = (Uuid::now_v7(), Uuid::now_v7());
        for (id, confidence, retired) in [(resolved_only, 0.9f32, false), (invalidated, 0.5, true)]
        {
            sqlx::query(
                "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                                    confidence, invalidated_at)
                 SELECT $1, kb_id, subject_id, predicate_id, object_id, $2,
                        CASE WHEN $3 THEN now() END
                   FROM facts WHERE id = $4",
            )
            .bind(id)
            .bind(confidence)
            .bind(retired)
            .bind(f2)
            .execute(&pool)
            .await?;
        }
        // f1 occurs on both ends and in several open conflicts. Every health
        // aggregate must still count the fact once, including low/unconfirmed.
        for (old, new, status) in [
            (f3, f1, "open"),
            (f2, f1, "open"),
            (resolved_only, f1, "resolved"),
        ] {
            sqlx::query(
                "INSERT INTO fact_conflicts (id, kb_id, old_fact_id, new_fact_id, reason, status)
                 VALUES ($1, $2, $3, $4, 'simultaneous', $5)",
            )
            .bind(Uuid::now_v7())
            .bind(s.kb)
            .bind(old)
            .bind(new)
            .bind(status)
            .execute(&pool)
            .await?;
        }
        let (doc, chunk) = (Uuid::now_v7(), Uuid::now_v7());
        sqlx::query(
            "INSERT INTO documents (id, kb_id, filename, sha256) VALUES ($1, $2, 'old.txt', $3)",
        )
        .bind(doc)
        .bind(s.kb)
        .bind(doc.to_string())
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO chunks (id, kb_id, document_id, seq, text, superseded_at)
             VALUES ($1, $2, $3, 0, 'old evidence', now())",
        )
        .bind(chunk)
        .bind(s.kb)
        .bind(doc)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO fact_evidence (fact_id, chunk_id, document_id) VALUES ($1, $2, $3)",
        )
        .bind(f1)
        .bind(chunk)
        .bind(doc)
        .execute(&pool)
        .await?;
        Ok::<_, anyhow::Error>(utopia_store::review_summary::summary(&pool, s.kb).await?)
    }
    .await;
    teardown(&pool, &s).await?;
    teardown(&pool, &other).await?;
    let sum = run?;
    assert_eq!(sum.health.facts, 4, "only this KB's live facts");
    assert_eq!(
        sum.health.contested, 3,
        "distinct live endpoints of open conflicts"
    );
    assert_eq!(sum.health.low_confidence, 1);
    assert_eq!(sum.health.unconfirmed, 1);
    assert_eq!(sum.waiting.conflicts.count, 3);
    assert_eq!(sum.waiting.lowconf.count, 1);
    assert_eq!(sum.waiting.unconfirmed.count, 1);
    Ok(())
}

fn close(a: Option<DateTime<Utc>>, b: DateTime<Utc>) -> bool {
    a.is_some_and(|a| (a - b).num_seconds().abs() < 2)
}

#[tokio::test]
async fn the_summary_reads_the_three_ledgers_with_one_yardstick() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let s = seed(&pool).await?;

    let got = utopia_store::review_summary::summary(&pool, s.kb).await;
    let counts = utopia_store::review::counts(&pool, s.kb).await;
    teardown(&pool, &s).await?;
    let sum = got?;
    let counts = counts?;

    // Waiting to be handled: same numbers as the left column, plus since when the oldest one has been waiting
    let w = &sum.waiting;
    assert_eq!(w.conflicts.count, 1);
    assert!(
        close(w.conflicts.oldest_at, s.conflict_at),
        "冲突按 created_at"
    );
    assert_eq!(w.duplicates.count, 1);
    assert!(
        close(w.duplicates.oldest_at, s.review_at),
        "重复按 created_at"
    );
    assert_eq!(w.lowconf.count, 1);
    assert!(
        w.lowconf.oldest_at.is_some(),
        "low confidence uses the fact's recorded_at"
    );
    assert_eq!(w.unconfirmed.count, 0);
    assert_eq!(w.pending.count, 0);
    assert_eq!(w.pending.oldest_at, None, "an empty queue has no oldest");
    assert_eq!(w.violations.count, 0);
    assert_eq!(w.defects.count, 0);
    assert_eq!(
        (
            w.conflicts.count,
            w.duplicates.count,
            w.lowconf.count,
            w.unconfirmed.count
        ),
        (
            counts.conflicts,
            counts.duplicates,
            counts.lowconf,
            counts.unconfirmed
        ),
        "总览与左栏计数是同一套口径"
    );

    // Quality: three living facts, one low-confidence, two hanging on open conflicts
    assert_eq!(sum.health.facts, 3);
    assert_eq!(sum.health.low_confidence, 1);
    assert_eq!(sum.health.contested, 2);
    assert_eq!(sum.health.unconfirmed, 0);

    // Handled: 7-day window three (two Ada, one self-retirement), 30-day window plus Bo's one;
    // the one 40 days ago and the non-review-domain actions do not count
    let d7 = &sum.decided.last_7d;
    assert_eq!(d7.total, 3);
    assert_eq!(d7.automatic, 1);
    let by_action: Vec<(&str, i64)> = d7
        .by_action
        .iter()
        .map(|a| (a.action.as_str(), a.count))
        .collect();
    assert_eq!(
        by_action,
        vec![("fact.confirm", 1), ("review.keep", 1), ("review.merge", 1)],
        "同数按动作名排序"
    );
    assert_eq!(d7.by_actor[0].actor_id, Some(s.ada));
    assert_eq!(d7.by_actor[0].label.as_deref(), Some("Ada"));
    assert_eq!(d7.by_actor[0].count, 2);
    assert!(
        d7.by_actor
            .iter()
            .any(|a| a.actor_id.is_none() && a.count == 1),
        "自裁的那条按「没有人」单列"
    );
    let d30 = &sum.decided.last_30d;
    assert_eq!(d30.total, 4);
    assert_eq!(d30.automatic, 1);
    assert!(
        d30.by_actor
            .iter()
            .any(|a| a.label.as_deref() == Some("Bo") && a.count == 1),
        "20 天前的那条进 30 天窗口"
    );

    // By day: fixed at 14 entries, today last; the sum is exactly the three from the last 14 days
    let daily = &sum.decided.daily;
    assert_eq!(daily.len() as i64, utopia_store::review_summary::DAILY_DAYS);
    assert_eq!(daily.last().map(|d| d.day), Some(Utc::now().date_naive()));
    assert_eq!(daily.iter().map(|d| d.count).sum::<i64>(), 3);
    for pair in daily.windows(2) {
        assert_eq!(
            pair[1].day - pair[0].day,
            Duration::days(1),
            "一天一条，不跳"
        );
    }
    Ok(())
}
