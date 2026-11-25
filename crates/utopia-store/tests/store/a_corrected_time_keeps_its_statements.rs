//! A person edits the interval of a row materialized from a statement (302); after editing it is still the row that the statement computes (#899's third rewriter).
//!
//! One statement "Zhang San —leads→ Aurora, since 2023-01-10" bound to leads, materializes into one row; another says
//! Li Si took over from 2024-07-05, and reconciliation after materialization closes Zhang San's span at the handover date. Then the person changes Zhang San's start to
//! 2023-02-01 (the charter date is the approval date; he actually took over on 1 February), with the end unchanged.
//!
//! If the corrected row produced by the rewrite does not carry which statements it was computed from (`typed_fact_sources`, `from_statement_id`),
//! two readings go wrong at the same time: the five Aurora questions on the temporal test bench went wrong exactly this way:
//!
//! - The statement is no longer represented by a live typed row, so the canvas, the entity panel and the conversation tool draw it back **as it was**:
//!   since 2023-01-10, with no end. So on 20 January Zhang San is in charge (the span the person just edited), and in August 2024
//!   Zhang San and Li Si are both in charge (the span the engine closed)
//! - The next materialization round sees the statement has no live row, computes one again from the statement, and the start the person edited is computed back
//!
//! The engine's own rewrites (closing, moving) have carried these since #911; a person editing an interval goes through a different function.
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use utopia_store::graph::{self, Validity};
use utopia_store::materialize::{materialize, Outcome};
use uuid::Uuid;

fn t(s: &str) -> DateTime<Utc> {
    s.parse().expect("fixed timestamp")
}

#[tokio::test]
async fn a_corrected_time_keeps_its_statements() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'corrected-time-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'corrected-time-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'corrected-time-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    let run = async {
        let (person, project, leads) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        for (id, key) in [(person, "person"), (project, "project")] {
            sqlx::query(
                "INSERT INTO entity_types (id, kb_id, key, label, color, shape)
                 VALUES ($1, $2, $3, $3, '#7fd0ff', 'circle')",
            )
            .bind(id)
            .bind(kb)
            .bind(key)
            .execute(&pool)
            .await?;
        }
        // One project can have only one lead at a time: a successor closes its predecessor, consistent with the axiom table of the bench
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, temporal, inverse_functional)
             VALUES ($1, $2, 'leads', 'leads', 'state', TRUE)",
        )
        .bind(leads)
        .bind(kb)
        .execute(&pool)
        .await?;
        let (zhang, li, aurora) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        for (id, type_id, name) in [
            (zhang, person, "Zhang San"),
            (li, person, "Li Si"),
            (aurora, project, "Aurora"),
        ] {
            sqlx::query(
                "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(kb)
            .bind(type_id)
            .bind(name)
            .execute(&pool)
            .await?;
        }
        let (charter, handover) = (Uuid::now_v7(), Uuid::now_v7());
        for (id, who, from) in [(charter, zhang, "2023-01-10"), (handover, li, "2024-07-05")] {
            sqlx::query(
                "INSERT INTO facts (id, kb_id, subject_id, object_id, layer, phrase, confidence,
                                    valid_from, valid_from_precision, valid_from_grade)
                 VALUES ($1, $2, $3, $4, 'open', 'leads', 0.9, $5::date, 'day', 'A')",
            )
            .bind(id)
            .bind(kb)
            .bind(who)
            .bind(aurora)
            .bind(from)
            .execute(&pool)
            .await?;
        }
        sqlx::query(
            "INSERT INTO phrase_bindings
                 (id, kb_id, phrase, subject_type_id, object_type_id, object_is_value,
                  relation_type_id, direction, status)
             VALUES ($1, $2, 'leads', $3, $4, false, $5, 'forward', 'bound')",
        )
        .bind(Uuid::now_v7())
        .bind(kb)
        .bind(person)
        .bind(project)
        .bind(leads)
        .execute(&pool)
        .await?;

        // Two statements, two rows; after materialization, reconciliation closes Zhang San's span on the day Li Si takes over
        let first = materialize(&pool, kb).await?;
        assert_eq!((first.added, first.corrected), (2, 1), "{first:?}");
        let zhang_rows = |pool: PgPool| async move {
            sqlx::query_as::<_, (Uuid, Option<DateTime<Utc>>, Option<DateTime<Utc>>)>(
                "SELECT id, valid_from, valid_to FROM facts
                  WHERE kb_id = $1 AND layer = 'typed' AND subject_id = $2 AND invalidated_at IS NULL",
            )
            .bind(kb)
            .bind(zhang)
            .fetch_all(&pool)
            .await
        };
        let rows = zhang_rows(pool.clone()).await?;
        assert_eq!(rows.len(), 1);
        let (closed, _, to) = rows[0];
        assert_eq!(to, Some(t("2024-07-05T00:00:00Z")), "交接日关上前任");

        // A person changes the start, the end unchanged; the same path as PATCH /facts/{id}: after editing, reconcile again through the move
        let corrected = utopia_store::temporal::correct_interval(
            &pool,
            closed,
            Validity {
                from: Some(t("2023-02-01T00:00:00Z")),
                from_precision: Some("day"),
                to: Some(t("2024-07-05T00:00:00Z")),
                to_precision: Some("day"),
                ..Default::default()
            },
        )
        .await?
        .expect("the row was live");
        utopia_store::temporal::reconcile_moved_facts(&pool, kb, &[corrected]).await?;

        // The corrected row is still the one the statement computes
        let (from_statement,): (Option<Uuid>,) =
            sqlx::query_as("SELECT from_statement_id FROM facts WHERE id = $1")
                .bind(corrected)
                .fetch_one(&pool)
                .await?;
        let sources: Vec<Uuid> = sqlx::query_scalar(
            "SELECT statement_id FROM typed_fact_sources WHERE fact_id = $1",
        )
        .bind(corrected)
        .fetch_all(&pool)
        .await?;
        assert_eq!((from_statement, sources), (Some(charter), vec![charter]));

        // On screen one statement draws only one edge: the span that was edited. On 20 January no one is in charge yet, and in August 2024 only Li Si is
        let leads_of_aurora = |at: &'static str| {
            let pool = pool.clone();
            async move {
                let (nodes, edges) =
                    graph::neighborhood(&pool, kb, aurora, 1, Some(t(at)), None).await?;
                let mut names: Vec<String> = edges
                    .iter()
                    .filter(|e| e.target == aurora)
                    .filter_map(|e| nodes.iter().find(|n| n.id == e.source))
                    .map(|n| n.name.clone())
                    .collect();
                names.sort();
                anyhow::Ok(names)
            }
        };
        assert_eq!(leads_of_aurora("2023-01-20T00:00:00Z").await?, Vec::<String>::new());
        assert_eq!(leads_of_aurora("2023-03-01T00:00:00Z").await?, vec!["Zhang San"]);
        assert_eq!(leads_of_aurora("2024-08-01T00:00:00Z").await?, vec!["Li Si"]);

        // The next materialization round has nothing to add: the start a person edited is not computed back
        assert_eq!(materialize(&pool, kb).await?, Outcome::default());
        let rows = zhang_rows(pool.clone()).await?;
        assert_eq!(
            rows.iter().map(|r| (r.1, r.2)).collect::<Vec<_>>(),
            vec![(Some(t("2023-02-01T00:00:00Z")), Some(t("2024-07-05T00:00:00Z")))],
            "张三只有人改过的那一段"
        );
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
