//! A row that "ended, date unknown" (0022 / #393): the end is anchored to the date of the document that states the ending. When a later-arriving,
//! earlier-dated observation that says it **holds** merges into this row, moving the start anchor earlier is right — that is earlier evidence that it held;
//! the end anchor should not move with it — that document says it held, not that it ended. If both anchors move, the interval shrinks to [t0, t0):
//! the reading then says it never held, and the derivations hanging on it go too.
//!
//! Hit during replay for #875: the item was on the table first, reconciliation closed the table-top span at the date of the observation where it was moved away (the anchor),
//! and an earlier "on the table" arriving late made the table-top span read as empty.
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is unset. Creates and tears down its own; never touches an existing database.

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgPool;
use utopia_store::graph::{self, Validity};
use uuid::Uuid;

#[tokio::test]
async fn an_earlier_mention_that_it_held_keeps_the_stated_end() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (class, location, cup) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'stated-end-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'stated-end-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'stated-end-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    let run = async {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'cup', 'Cup')")
            .bind(class)
            .bind(kb)
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, kind, datatype, temporal)
             VALUES ($1, $2, 'location', 'location', 'attribute', 'text', 'state')",
        )
        .bind(location)
        .bind(kb)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, 'cup-7')",
        )
        .bind(cup)
        .bind(kb)
        .bind(class)
        .execute(&pool)
        .await?;
        let t = |s: &str| s.parse::<DateTime<Utc>>();
        let (t0, t1, t2) = (
            t("2026-09-23T07:50:00Z")?,
            t("2026-09-23T08:00:00Z")?,
            t("2026-09-23T08:10:00Z")?,
        );
        let desk = json!({ "value": "desk" });
        // t1: on the table. No start point, so it holds from this evidence
        graph::insert_value_fact(
            &pool,
            kb,
            cup,
            Some(location),
            &desk,
            Validity::default().attested(Some(t1)),
            1.0,
        )
        .await?;
        // t2: it ended, date unknown — the end anchor is on this document
        let (closed, _) = graph::insert_value_fact(
            &pool,
            kb,
            cup,
            Some(location),
            &desk,
            Validity::default().attested(Some(t2)).ended_when_unknown(),
            1.0,
        )
        .await?;
        // t0 < t1: an earlier "on the table" arrives late. The same assertion merges into the row that is already closed
        let merged = graph::insert_value_fact(
            &pool,
            kb,
            cup,
            Some(location),
            &desk,
            Validity::default().attested(Some(t0)),
            1.0,
        )
        .await?;
        assert_eq!(merged, (closed, false));
        let (from, to, precision): (DateTime<Utc>, Option<DateTime<Utc>>, Option<String>) =
            sqlx::query_as(
                "SELECT attested_from, attested_to, valid_to_precision FROM facts WHERE id = $1",
            )
            .bind(closed)
            .fetch_one(&pool)
            .await?;
        assert_eq!(precision.as_deref(), Some(graph::ENDED_UNKNOWN));
        assert_eq!(from, t0, "the earlier mention is earlier evidence that it held");
        assert_eq!(
            to,
            Some(t2),
            "a mention that it held is no evidence that it ended earlier"
        );
        // The other path still has to move: this earlier evidence does say it has ended, not merely that it holds.
        let ended = graph::insert_value_fact(
            &pool,
            kb,
            cup,
            Some(location),
            &desk,
            Validity::default().attested(Some(t1)).ended_when_unknown(),
            1.0,
        )
        .await?;
        assert_eq!(ended, (closed, false));
        let anchors: (DateTime<Utc>, Option<DateTime<Utc>>) =
            sqlx::query_as("SELECT attested_from, attested_to FROM facts WHERE id = $1")
                .bind(closed)
                .fetch_one(&pool)
                .await?;
        assert_eq!(anchors, (t0, Some(t1)), "earlier ended evidence moves only the end here");
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
