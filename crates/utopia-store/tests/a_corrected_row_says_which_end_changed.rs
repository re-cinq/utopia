//! Migration 0099: for rows whose interval a person edited before this column existed, work out from the audit which end changed.
//!
//! Built the way the correction route does it: `correct_interval` changes an interval, the audit is recorded on the row that was changed, and `detail` carries the before
//! (`from`) and after (`to`) intervals; then wipe this column, which is exactly how it looked before the column appeared.
//!
//! - Zhang San: changed the start → start; later closed by hand (one rewrite) still start
//! - Li Si: changed the start, then the end → both
//! - Wang Wu: changed the end → end
//! - Zhao Liu: only closed by hand, no interval-change audit → empty
//! - Qian Qi: the audit did not record before and after → both (cannot tell which end changed; neither end is from the original sentence)
//! - Sun Ba: changed only the start, but the original end was derived from the timeline → both (the person pinned that end)
//!
//! Only fills the blanks: a row that already has a marker is left alone whatever the audit says. Running the migration again changes nothing.
//!
//! The migration statements do not filter by kb; they change rows across the whole database, so this test is its own binary at the top of tests/
//! (same reasoning as 0096). Without `UTOPIA_DATABASE_URL` it skips rather than fails. It sets up and tears down its own data and never touches an existing database.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use utopia_store::graph::{self, Validity};
use uuid::Uuid;

const MIGRATION: &str =
    include_str!("../../../migrations/0099_a_corrected_row_says_which_end_a_person_changed.sql");

fn day(s: &str) -> DateTime<Utc> {
    format!("{s}T00:00:00Z").parse().expect("fixed day")
}

fn between(from: &str, to: Option<&str>) -> Validity<'static> {
    Validity {
        from: Some(day(from)),
        from_precision: Some("day"),
        to: to.map(day),
        to_precision: to.map(|_| "day"),
        attested_at: None,
        from_grade: None,
        ..Default::default()
    }
}

/// The end recorded by the correction route: written with the same RFC 3339 date format
fn side(v: &Validity<'_>) -> Value {
    json!({
        "valid_from": v.from.map(|t| t.to_rfc3339()),
        "valid_from_precision": v.from_precision,
        "valid_to": v.to.map(|t| t.to_rfc3339()),
        "valid_to_precision": v.to_precision,
    })
}

/// The marker for the one live version of a row
async fn ends(pool: &PgPool, kb: Uuid, subject: Uuid) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT corrected_ends FROM facts
          WHERE kb_id = $1 AND subject_id = $2 AND invalidated_at IS NULL",
    )
    .bind(kb)
    .bind(subject)
    .fetch_one(pool)
    .await?)
}

#[tokio::test]
async fn a_row_corrected_before_the_column_gets_its_ends_back() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let ids: Vec<Uuid> = (0..11).map(|_| Uuid::now_v7()).collect();
    let (org, ws, kb, user, person, project, leads) =
        (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5], ids[6]);
    let people = [
        ids[7],
        ids[8],
        ids[9],
        ids[10],
        Uuid::now_v7(),
        Uuid::now_v7(),
    ];
    let aurora = Uuid::now_v7();
    // Only locally generated UUIDs are interpolated into fixture SQL.
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name) VALUES ('{org}', 'corrected-ends');
         INSERT INTO workspaces (id, org_id, name) VALUES ('{ws}', '{org}', 'corrected-ends');
         INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ('{kb}', '{ws}', 'corrected-ends');
         INSERT INTO users (id, org_id, email, display_name, password_hash)
              VALUES ('{user}', '{org}', '{user}@ends.test', 'editor', 'unused');
         INSERT INTO entity_types (id, kb_id, key, label, color, shape) VALUES
             ('{person}', '{kb}', 'person', 'person', '#7fd0ff', 'circle'),
             ('{project}', '{kb}', 'project', 'project', '#7fd0ff', 'circle');
         INSERT INTO relation_types (id, kb_id, key, label, temporal)
              VALUES ('{leads}', '{kb}', 'leads', 'leads', 'state');
         INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES
             ('{aurora}', '{kb}', '{project}', 'Aurora'),
             ('{zhang}', '{kb}', '{person}', 'Zhang San'), ('{li}', '{kb}', '{person}', 'Li Si'),
             ('{wang}', '{kb}', '{person}', 'Wang Wu'), ('{zhao}', '{kb}', '{person}', 'Zhao Liu'),
             ('{qian}', '{kb}', '{person}', 'Qian Qi'), ('{sun}', '{kb}', '{person}', 'Sun Ba');",
        zhang = people[0],
        li = people[1],
        wang = people[2],
        zhao = people[3],
        qian = people[4],
        sun = people[5],
    ))
    .execute(&pool)
    .await?;
    let [zhang, li, wang, zhao, qian, sun] = people;

    let run = async {
        let row = |who: Uuid, v: Validity<'static>| {
            let pool = pool.clone();
            async move {
                anyhow::Ok(
                    graph::insert_fact(&pool, kb, who, Some(leads), aurora, v, 0.9)
                        .await?
                        .0,
                )
            }
        };
        // A person changes an interval once: a correction row, plus the audit the correction route recorded on the changed row
        let correct =
            |fact: Uuid, before: Validity<'static>, after: Validity<'static>, detail: bool| {
                let pool = pool.clone();
                async move {
                    let corrected = utopia_store::temporal::correct_interval(&pool, fact, after)
                        .await?
                        .expect("the row was live");
                    let detail = if detail {
                        json!({ "from": side(&before), "to": side(&after) })
                    } else {
                        json!({})
                    };
                    utopia_store::audit::record(
                        &pool,
                        Some(kb),
                        user,
                        "fact.time_corrected",
                        "fact",
                        Some(fact),
                        detail,
                    )
                    .await?;
                    anyhow::Ok(corrected)
                }
            };

        let first = between("2023-01-10", Some("2024-07-05"));
        let z = row(zhang, first).await?;
        let z = correct(z, first, between("2023-02-01", Some("2024-07-05")), true).await?;
        utopia_store::temporal::close_superseded(&pool, z, day("2024-06-30"), "day").await?;

        let l = row(li, first).await?;
        let moved = between("2023-02-01", Some("2024-07-05"));
        let l = correct(l, first, moved, true).await?;
        correct(l, moved, between("2023-02-01", Some("2024-08-01")), true).await?;

        let w = row(wang, first).await?;
        correct(w, first, between("2023-01-10", Some("2024-06-01")), true).await?;

        let open = between("2023-01-10", None);
        let h = row(zhao, open).await?;
        utopia_store::temporal::close_superseded(&pool, h, day("2024-01-01"), "day").await?;

        let q = row(qian, first).await?;
        correct(q, first, between("2023-03-01", Some("2024-07-05")), false).await?;

        // The end the timeline derived (set directly here); the person changes only the start, and the end is copied as is
        let s = row(sun, first).await?;
        sqlx::query("UPDATE facts SET end_derived = TRUE WHERE id = $1")
            .bind(s)
            .execute(&pool)
            .await?;
        correct(s, first, between("2023-02-01", Some("2024-07-05")), true).await?;

        let now: Vec<Option<String>> = {
            let mut v = Vec::new();
            for who in people {
                v.push(ends(&pool, kb, who).await?);
            }
            v
        };
        assert_eq!(
            now,
            [
                Some("start"),
                Some("both"),
                Some("end"),
                None,
                Some("start"),
                Some("both")
            ]
            .map(|e| e.map(str::to_string)),
            "what correct_interval wrote"
        );

        // The state before this column existed: no marker. Wang Wu's row already has one (which disagrees with the audit),
        // and backfill leaves it alone
        sqlx::query("UPDATE facts SET corrected_ends = NULL WHERE kb_id = $1 AND subject_id <> $2")
            .bind(kb)
            .bind(wang)
            .execute(&pool)
            .await?;
        sqlx::query(
            "UPDATE facts SET corrected_ends = 'both'
              WHERE kb_id = $1 AND subject_id = $2 AND invalidated_at IS NULL",
        )
        .bind(kb)
        .bind(wang)
        .execute(&pool)
        .await?;

        sqlx::raw_sql(MIGRATION).execute(&pool).await?;
        assert_eq!(ends(&pool, kb, zhang).await?.as_deref(), Some("start"));
        assert_eq!(ends(&pool, kb, li).await?.as_deref(), Some("both"));
        assert_eq!(
            ends(&pool, kb, wang).await?.as_deref(),
            Some("both"),
            "a row that has its mark keeps it"
        );
        assert_eq!(ends(&pool, kb, zhao).await?, None, "closed by hand");
        assert_eq!(
            ends(&pool, kb, qian).await?.as_deref(),
            Some("both"),
            "an audit without the two intervals cannot say which end"
        );
        assert_eq!(
            ends(&pool, kb, sun).await?.as_deref(),
            Some("both"),
            "the derived end the person kept"
        );

        // Run again: still unchanged
        sqlx::raw_sql(MIGRATION).execute(&pool).await?;
        assert_eq!(ends(&pool, kb, zhang).await?.as_deref(), Some("start"));
        assert_eq!(ends(&pool, kb, li).await?.as_deref(), Some("both"));
        assert_eq!(ends(&pool, kb, zhao).await?, None);
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
