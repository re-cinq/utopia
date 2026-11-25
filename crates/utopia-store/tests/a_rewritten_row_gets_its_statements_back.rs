//! Migration 0096: rows rewritten before #967 get back the statements they were derived from, and duplicate rows that materialization recomputed afterwards are voided.
//!
//! The bad state is built up the way it originally arose. A statement "Zhang San —leads→ Aurora, from 2023-01-10"
//! is bound to leads, and an approved phrase rule implies works_on from the same sentence. After materialization there is a typed row and an implied row.
//! Both rows have their start moved to 2023-02-01 through `correct_interval`, and then the link on the correction row is wiped -- which is exactly
//! what a person's interval edit wrote before #967. Materializing again, the old code's consequence still appears: the statement has no live row
//! representing it, so two rows (D and D2) are computed again over the original interval.
//!
//! After running 0096: the two correction rows get back their statement and rule sources, and D and D2 are voided but stay in the table; rows a person wrote and then edited,
//! and rows rewritten with a link after #967, are left alone; a fact a person approved that gets a start filled in on a bare row (a time refinement, another observation)
//! is also left alone -- it links to a materialized row, but it was not a rewrite; materializing again does nothing, and no one is managing the 2023-01-20 moment;
//! running the migration once more changes nothing.
//!
//! The migration's statement does not filter by kb and changes rows across the whole database, so this test is a binary of its own at the top of tests/
//! (for the same reason as the 0057 backfill, see `tests/store/main.rs`).
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. It creates and tears down its own data and never touches an existing database.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use utopia_store::graph::{self, Validity};
use utopia_store::materialize::{materialize, Outcome};
use uuid::Uuid;

const MIGRATION: &str =
    include_str!("../../../migrations/0096_a_rewritten_row_gets_its_statements_back.sql");

fn t(s: &str) -> DateTime<Utc> {
    s.parse().expect("fixed timestamp")
}

/// A row's links: from_statement_id, implied, and the statements in the two source tables
async fn links(
    pool: &PgPool,
    fact: Uuid,
) -> anyhow::Result<(Option<Uuid>, bool, Vec<Uuid>, Vec<Uuid>)> {
    let (from, implied): (Option<Uuid>, bool) =
        sqlx::query_as("SELECT from_statement_id, implied FROM facts WHERE id = $1")
            .bind(fact)
            .fetch_one(pool)
            .await?;
    let typed: Vec<Uuid> = sqlx::query_scalar(
        "SELECT statement_id FROM typed_fact_sources WHERE fact_id = $1 ORDER BY statement_id",
    )
    .bind(fact)
    .fetch_all(pool)
    .await?;
    let by_rule: Vec<Uuid> = sqlx::query_scalar(
        "SELECT statement_id FROM implied_fact_sources WHERE fact_id = $1 ORDER BY statement_id",
    )
    .bind(fact)
    .fetch_all(pool)
    .await?;
    Ok((from, implied, typed, by_rule))
}

async fn live(pool: &PgPool, fact: Uuid) -> anyhow::Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT invalidated_at IS NULL FROM facts WHERE id = $1")
            .bind(fact)
            .fetch_one(pool)
            .await?,
    )
}

/// The correction row's link wiped: this is what `correct_interval` wrote before #967
async fn as_before_967(pool: &PgPool, fact: Uuid) -> anyhow::Result<()> {
    sqlx::query("UPDATE facts SET from_statement_id = NULL, implied = FALSE WHERE id = $1")
        .bind(fact)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM typed_fact_sources WHERE fact_id = $1")
        .bind(fact)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM implied_fact_sources WHERE fact_id = $1")
        .bind(fact)
        .execute(pool)
        .await?;
    Ok(())
}

async fn correct_start(pool: &PgPool, fact: Uuid, from: &str) -> anyhow::Result<Uuid> {
    Ok(utopia_store::temporal::correct_interval(
        pool,
        fact,
        Validity {
            from: Some(t(from)),
            from_precision: Some("day"),
            ..Default::default()
        },
    )
    .await?
    .expect("the row was live"))
}

/// A typed row for one live predicate (subject and object given)
async fn rows(
    pool: &PgPool,
    kb: Uuid,
    subject: Uuid,
    predicate: Uuid,
) -> anyhow::Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM facts
          WHERE kb_id = $1 AND layer = 'typed' AND subject_id = $2 AND predicate_id = $3
            AND invalidated_at IS NULL
          ORDER BY id",
    )
    .bind(kb)
    .bind(subject)
    .bind(predicate)
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
async fn a_rewritten_row_gets_its_statements_back() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'rewritten-row-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'rewritten-row-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'rewritten-row-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    let run = async {
        let id = Uuid::now_v7;
        let (person, project, leads, works_on, rule) = (id(), id(), id(), id(), id());
        let (zhang, li, wang, zhou, aurora, helios, nova) =
            (id(), id(), id(), id(), id(), id(), id());
        let (charter, kickoff, bare, by_hand) = (id(), id(), id(), id());
        // Only locally generated UUIDs and fixed dates are inserted
        sqlx::raw_sql(&format!(
            r#"
            INSERT INTO entity_types (id, kb_id, key, label, color, shape) VALUES
                ('{person}','{kb}','person','person','#7fd0ff','circle'),
                ('{project}','{kb}','project','project','#7fd0ff','circle');
            INSERT INTO relation_types (id, kb_id, key, label, temporal) VALUES
                ('{leads}','{kb}','leads','leads','state'),
                ('{works_on}','{kb}','works_on','works on','state');
            INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES
                ('{zhang}','{kb}','{person}','Zhang San'),
                ('{li}','{kb}','{person}','Li Si'),
                ('{wang}','{kb}','{person}','Wang Wu'),
                ('{zhou}','{kb}','{person}','Zhou Qi'),
                ('{aurora}','{kb}','{project}','Aurora'),
                ('{helios}','{kb}','{project}','Helios'),
                ('{nova}','{kb}','{project}','Nova');
            -- 三条陈述：张三那条走 #967 之前的路，李四那条走 #967 之后的路，周七那条没有起点，
            -- 之后由人点头的事实补上
            INSERT INTO facts (id, kb_id, subject_id, object_id, layer, phrase, confidence,
                               valid_from, valid_from_precision, valid_from_grade) VALUES
                ('{charter}','{kb}','{zhang}','{aurora}','open','leads',0.9,'2023-01-10','day','A'),
                ('{kickoff}','{kb}','{li}','{helios}','open','leads',0.9,'2024-08-12','day','A'),
                ('{bare}','{kb}','{zhou}','{nova}','open','leads',0.9,NULL,NULL,NULL);
            INSERT INTO phrase_bindings (id, kb_id, phrase, subject_type_id, object_type_id,
                                         object_is_value, relation_type_id, direction, status) VALUES
                ('{binding}','{kb}','leads','{person}','{project}',false,'{leads}','forward','bound');
            INSERT INTO implication_rules (id, kb_id, trigger, phrase, subject_type_id, object_type_id,
                                           object_is_value, conclude_property_id, reading, status,
                                           decided_by) VALUES
                ('{rule}','{kb}','phrase','leads','{person}','{project}',false,'{works_on}',NULL,
                 'approved','person');
            -- 人手写的一行：链上没有物化出来的行
            INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, layer, confidence,
                               valid_from, valid_from_precision) VALUES
                ('{by_hand}','{kb}','{wang}','{leads}','{helios}','typed',1.0,'2022-01-01','day');
            "#,
            binding = id(),
        ))
        .execute(&pool)
        .await?;

        // Materialize: three statements give three leads rows, and each sentence has another row from the rule's implied works_on
        materialize(&pool, kb).await?;
        let typed = rows(&pool, kb, zhang, leads).await?;
        let by_rule = rows(&pool, kb, zhang, works_on).await?;
        assert_eq!((typed.len(), by_rule.len()), (1, 1));

        // Interval edited by a person before #967: the correction row was written without a link
        let corrected = correct_start(&pool, typed[0], "2023-02-01T00:00:00Z").await?;
        let corrected_rule = correct_start(&pool, by_rule[0], "2023-02-01T00:00:00Z").await?;
        as_before_967(&pool, corrected).await?;
        as_before_967(&pool, corrected_rule).await?;
        // Control: a row edited after #967 carries its link; a row a person wrote and then edited
        let after = rows(&pool, kb, li, leads).await?;
        let after = correct_start(&pool, after[0], "2024-08-01T00:00:00Z").await?;
        let by_hand = correct_start(&pool, by_hand, "2022-02-01T00:00:00Z").await?;
        let after_links = links(&pool, after).await?;
        assert_eq!(after_links.0, Some(kickoff), "#967 之后改写带着链接");
        // Control: a fact a person approved gets a start filled in on a bare row -- the new row links to that materialized row, but it is a different observation
        let (refined, _) = graph::insert_fact(
            &pool,
            kb,
            zhou,
            Some(leads),
            nova,
            Validity {
                from: Some(t("2023-05-01T00:00:00Z")),
                from_precision: Some("day"),
                ..Default::default()
            },
            1.0,
        )
        .await?;
        let (supersedes,): (Option<Uuid>,) =
            sqlx::query_as("SELECT supersedes FROM facts WHERE id = $1")
                .bind(refined)
                .fetch_one(&pool)
                .await?;
        assert!(supersedes.is_some(), "the confirmed fact refines the bare row");

        // The old code's consequence: materializing once more computes two rows from the statement again, beside the correction rows
        let second = materialize(&pool, kb).await?;
        assert_eq!(
            (second.added, second.implied, second.merged),
            (1, 1, 1),
            "the two duplicates, and the bare statement merged into the refined row: {second:?}"
        );
        let refined_links = links(&pool, refined).await?;
        assert_eq!(refined_links, (None, false, vec![bare], vec![]));
        let duplicate = *rows(&pool, kb, zhang, leads)
            .await?
            .iter()
            .find(|f| **f != corrected)
            .expect("a duplicate of the corrected leads row");
        let duplicate_rule = *rows(&pool, kb, zhang, works_on)
            .await?
            .iter()
            .find(|f| **f != corrected_rule)
            .expect("a duplicate of the corrected works_on row");

        sqlx::raw_sql(MIGRATION).execute(&pool).await?;

        // The two correction rows get back their statement and rule sources
        assert_eq!(
            links(&pool, corrected).await?,
            (Some(charter), false, vec![charter], vec![])
        );
        assert_eq!(
            links(&pool, corrected_rule).await?,
            (None, true, vec![], vec![charter])
        );
        // Duplicate rows are voided, not deleted
        assert!(!live(&pool, duplicate).await? && !live(&pool, duplicate_rule).await?);
        // Controls are left alone
        assert_eq!(links(&pool, after).await?, after_links);
        assert_eq!(links(&pool, by_hand).await?, (None, false, vec![], vec![]));
        assert_eq!(links(&pool, refined).await?, refined_links, "a refinement is not a rewrite");
        assert!(
            live(&pool, by_hand).await? && live(&pool, after).await? && live(&pool, refined).await?
        );

        // Materializing again does nothing: the statement is represented by the correction row, and the rule's source is there too
        assert_eq!(materialize(&pool, kb).await?, Outcome::default());
        let (nodes, edges) =
            graph::neighborhood(&pool, kb, aurora, 1, Some(t("2023-01-20T00:00:00Z")), None)
                .await?;
        let names: Vec<&str> = edges
            .iter()
            .filter_map(|e| nodes.iter().find(|n| n.id == e.source))
            .map(|n| n.name.as_str())
            .collect();
        assert!(names.is_empty(), "nobody on 2023-01-20: {names:?}");

        // Run again: still no change
        sqlx::raw_sql(MIGRATION).execute(&pool).await?;
        assert_eq!(
            links(&pool, corrected).await?,
            (Some(charter), false, vec![charter], vec![])
        );
        assert_eq!(
            rows(&pool, kb, zhang, leads).await?,
            vec![corrected],
            "one live leads row"
        );
        assert_eq!(rows(&pool, kb, zhang, works_on).await?, vec![corrected_rule]);
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
