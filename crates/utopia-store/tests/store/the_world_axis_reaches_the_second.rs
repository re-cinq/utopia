//! The world axis runs to the second (0024), run against a real DB.
//!
//! Three things are pinned, each of which lives in SQL or an SQL constraint:
//! - Facts with a precision can be stored, and the stored value is truncated to the whole minute — the write path truncates, and the DB CHECK guards it again
//! - Rows whose value and precision disagree (day precision but carrying 14:00) are blocked at the door by the CHECK: three-valued logic does not let them through
//! - A derived row's precision follows **the premise that wins this end**, rather than taking the coarsest of all premises — year tagged on
//!   a value of 15 June is exactly the contradiction this invariant is meant to eliminate
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is not set. It builds and tears down its own data and never touches an existing DB.

use sqlx::PgPool;
use utopia_store::graph::Validity;
use uuid::Uuid;

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

struct Fixture {
    org: Uuid,
    kb: Uuid,
    part_of: Uuid,
    a: Uuid,
    b: Uuid,
    c: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (ty, part_of) = (Uuid::now_v7(), Uuid::now_v7());
    let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'second-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'second-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'second-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'org', 'Org')")
        .bind(ty)
        .bind(kb)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, is_transitive)
         VALUES ($1, $2, 'part_of', 'part of', TRUE)",
    )
    .bind(part_of)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [(a, "Team A"), (b, "Group B"), (c, "Division C")] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(ty)
        .bind(name)
        .execute(pool)
        .await?;
    }
    Ok(Fixture {
        org,
        kb,
        part_of,
        a,
        b,
        c,
    })
}

type Bounds = (
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
);

async fn bounds(pool: &PgPool, table: &str, id: Uuid) -> anyhow::Result<Bounds> {
    Ok(sqlx::query_as(&format!(
        "SELECT valid_from, valid_from_precision, valid_to, valid_to_precision FROM {table} WHERE id = $1"
    ))
    .bind(id)
    .fetch_one(pool)
    .await?)
}

#[tokio::test]
async fn a_stored_value_is_truncated_to_its_precision() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // That memory-log row: stamped at 14:32, precision to the minute. It comes in with milliseconds, and the stored value is the whole minute
        let (id, _) = utopia_store::graph::insert_fact(
            &pool,
            f.kb,
            f.a,
            Some(f.part_of),
            f.b,
            Validity::starting(Some(t("2026-06-01T14:32:07.382Z")), Some("minute"))
                .attested(Some(t("2026-06-01T14:32:07.382Z"))),
            0.9,
        )
        .await?;
        let (from, fp, _, _) = bounds(&pool, "facts", id).await?;
        assert_eq!(from, Some(t("2026-06-01T14:32:00Z")), "截到整分");
        assert_eq!(fp.as_deref(), Some("minute"));

        // A person closes it at some hour: the closing point is truncated to the whole hour
        let closed =
            utopia_store::temporal::close_superseded(&pool, id, t("2026-06-01T18:45:00Z"), "hour")
                .await?
                .expect("开放行关得上");
        let (_, _, to, tp) = bounds(&pool, "facts", closed).await?;
        assert_eq!(to, Some(t("2026-06-01T18:00:00Z")), "小时精度就是整点");
        assert_eq!(tp.as_deref(), Some("hour"));

        // A row whose value and precision disagree is inserted directly, bypassing the write path: the CHECK blocks it. **This one must actually run** —
        // in three-valued logic a NULL lets a CHECK pass silently; only inserting once tells you it did not
        let bad = sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                                valid_from, valid_from_precision)
             VALUES ($1, $2, $3, $4, $5, '2026-06-01T14:00:00Z', 'day')",
        )
        .bind(Uuid::now_v7())
        .bind(f.kb)
        .bind(f.b)
        .bind(f.part_of)
        .bind(f.c)
        .execute(&pool)
        .await;
        let err = bad
            .expect_err("day 精度带着 14 点，不该存得进去")
            .to_string();
        assert!(err.contains("facts_from_precision_matches_date"), "{err}");
        // Precision not on the ladder is also blocked
        let bad = sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                                valid_from, valid_from_precision)
             VALUES ($1, $2, $3, $4, $5, '2026-06-01T14:00:00.500Z', 'millisecond')",
        )
        .bind(Uuid::now_v7())
        .bind(f.kb)
        .bind(f.b)
        .bind(f.part_of)
        .bind(f.c)
        .execute(&pool)
        .await;
        assert!(bad.is_err(), "账本到秒为止");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// A derived start is the latest of the premises' starts; the precision is that premise's precision, not the coarsest of all premises
#[tokio::test]
async fn a_derived_bound_carries_the_precision_of_the_premise_that_set_it() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // a ⊂ b from 2020 (year); b ⊂ c from 2023-06-15 (day)
        utopia_store::graph::insert_fact(
            &pool,
            f.kb,
            f.a,
            Some(f.part_of),
            f.b,
            Validity::starting(Some(t("2020-01-01T00:00:00Z")), Some("year"))
                .attested(Some(t("2020-01-01T00:00:00Z"))),
            0.9,
        )
        .await?;
        utopia_store::graph::insert_fact(
            &pool,
            f.kb,
            f.b,
            Some(f.part_of),
            f.c,
            Validity::starting(Some(t("2023-06-15T00:00:00Z")), Some("day"))
                .attested(Some(t("2023-06-15T00:00:00Z"))),
            0.9,
        )
        .await?;
        utopia_store::reasoning::materialize(&pool, f.kb).await?;

        let row: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM derived_facts
              WHERE kb_id = $1 AND subject_id = $2 AND object_id = $3 AND invalidated_at IS NULL",
        )
        .bind(f.kb)
        .bind(f.a)
        .bind(f.c)
        .fetch_optional(&pool)
        .await?;
        let (id,) = row.expect("a ⊂ c 推得出");
        let (from, fp, _, _) = bounds(&pool, "derived_facts", id).await?;
        assert_eq!(
            from,
            Some(t("2023-06-15T00:00:00Z")),
            "起点是最晚的那条前提的起点"
        );
        assert_eq!(
            fp.as_deref(),
            Some("day"),
            "精度跟赢下这一端的前提走——从前取最粗会把 year 标在 6 月 15 日上"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
