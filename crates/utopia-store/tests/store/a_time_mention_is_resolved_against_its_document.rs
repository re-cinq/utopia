//! A time mention is read against its document (the ledger side of 0045 slices 1 and 2).
//!
//! The `when` and `ended` of an open statement are each one mention; fetching by document returns them with the sentence they sit in as evidence,
//! ordered by chunk order and then position within the chunk; the model's interpretation and the code's resolution each land on the mention and can be read back; the resolution result is
//! written to the open row's world axis by `set_open_validity` — only open rows are written, typed rows are not touched at all;
//! an end resolved as "ended, date unknown" can be stored; a value and precision that disagree are rejected by the database.
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use chrono::{DateTime, TimeZone, Utc};
use sqlx::PgPool;
use utopia_store::graph::{self, FactObject};
use utopia_store::time_mentions;
use uuid::Uuid;

const ORG: &str = "time-resolution-test";
const TEXT_A: &str = "Acme acquired Beta on March 4, 2011 and sold it in 2019.";
const TEXT_B: &str = "Beta had been founded two years earlier by Carol.";

struct Fixture {
    kb: Uuid,
    acme: Uuid,
    beta: Uuid,
    carol: Uuid,
    doc: Uuid,
    chunk_a: Uuid,
    chunk_b: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (acme, beta, carol) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (src, doc, chunk_a, chunk_b) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(ORG)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(org)
        .bind(ORG)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3)")
        .bind(kb)
        .bind(ws)
        .bind(ORG)
        .execute(pool)
        .await?;
    for (id, name) in [(acme, "Acme"), (beta, "Beta"), (carol, "Carol")] {
        sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(kb)
            .bind(name)
            .execute(pool)
            .await?;
    }
    sqlx::query("INSERT INTO sources (id, kb_id, name) VALUES ($1, $2, '临时来源')")
        .bind(src)
        .bind(kb)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status)
         VALUES ($1, $2, $3, 'deal.md', 'timeresolution', 'ready')",
    )
    .bind(doc)
    .bind(kb)
    .bind(src)
    .execute(pool)
    .await?;
    for (id, seq, text) in [(chunk_a, 0i32, TEXT_A), (chunk_b, 1i32, TEXT_B)] {
        sqlx::query(
            "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(kb)
        .bind(doc)
        .bind(seq)
        .bind(text)
        .execute(pool)
        .await?;
    }
    Ok(Fixture {
        kb,
        acme,
        beta,
        carol,
        doc,
        chunk_a,
        chunk_b,
    })
}

/// The character start the server found by search within the chunk (the text here is ASCII, so bytes are characters)
fn start_of(text: &str, words: &str) -> i32 {
    let byte = text.find(words).expect("words are in the chunk");
    text[..byte].chars().count() as i32
}

fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct Span {
    valid_from: Option<DateTime<Utc>>,
    valid_from_precision: Option<String>,
    valid_to: Option<DateTime<Utc>>,
    valid_to_precision: Option<String>,
}

async fn span_of(pool: &PgPool, fact: Uuid) -> anyhow::Result<Span> {
    Ok(sqlx::query_as(
        "SELECT valid_from, valid_from_precision, valid_to, valid_to_precision
         FROM facts WHERE id = $1",
    )
    .bind(fact)
    .fetch_one(pool)
    .await?)
}

#[tokio::test]
async fn a_mention_is_read_then_computed_then_written_to_the_open_row() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(ORG)
        .execute(&pool)
        .await?;
    let f = seed(&pool).await?;

    let run = async {
        // An open statement, with evidence carrying quotes; one mention each for start and end
        let (fact, _) = graph::insert_open_statement(
            &pool,
            f.kb,
            f.acme,
            "acquired",
            FactObject::Entity(f.beta),
            None,
            0.9,
        )
        .await?;
        graph::add_evidence_located(
            &pool,
            fact,
            f.chunk_a,
            Some(TEXT_A),
            Some("acquired"),
            Some((0, TEXT_A.len() as i32)),
        )
        .await?;
        let when_start = start_of(TEXT_A, "March 4, 2011");
        let ended_start = start_of(TEXT_A, "2019");
        let when = time_mentions::record(
            &pool,
            f.kb,
            fact,
            f.chunk_a,
            "March 4, 2011",
            when_start,
            "when",
        )
        .await?;
        let ended =
            time_mentions::record(&pool, f.kb, fact, f.chunk_a, "2019", ended_start, "ended")
                .await?;

        // 1. Fetch by document: two, with the sentence in the evidence, ordered by position
        let todo = time_mentions::for_document(&pool, f.doc).await?;
        assert_eq!(todo.len(), 2, "{todo:?}");
        assert_eq!(
            (
                todo[0].id,
                todo[0].role.as_str(),
                todo[0].text.as_str(),
                todo[0].char_start
            ),
            (when, "when", "March 4, 2011", when_start)
        );
        assert_eq!(
            (
                todo[1].id,
                todo[1].role.as_str(),
                todo[1].text.as_str(),
                todo[1].char_start
            ),
            (ended, "ended", "2019", ended_start)
        );
        assert!(todo
            .iter()
            .all(|m| m.sentence == TEXT_A && m.fact_id == fact && m.chunk_id == f.chunk_a));

        // The statement on the second chunk sorts later; evidence without a stored quote falls back to the whole chunk's text
        let (founded, _) = graph::insert_open_statement(
            &pool,
            f.kb,
            f.beta,
            "founded by",
            FactObject::Entity(f.carol),
            None,
            0.9,
        )
        .await?;
        graph::add_evidence(&pool, founded, f.chunk_b, None, Some("founded by")).await?;
        let earlier_start = start_of(TEXT_B, "two years earlier");
        let earlier = time_mentions::record(
            &pool,
            f.kb,
            founded,
            f.chunk_b,
            "two years earlier",
            earlier_start,
            "when",
        )
        .await?;
        let todo = time_mentions::for_document(&pool, f.doc).await?;
        assert_eq!(
            todo.iter().map(|m| m.id).collect::<Vec<_>>(),
            [when, ended, earlier],
            "chunk order, then position"
        );
        assert_eq!(
            todo[2].sentence, TEXT_B,
            "no quote on the evidence: the chunk's text"
        );

        // A voided statement is not on the list waiting to be read
        sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
            .bind(founded)
            .execute(&pool)
            .await?;
        let todo = time_mentions::for_document(&pool, f.doc).await?;
        assert_eq!(todo.iter().map(|m| m.id).collect::<Vec<_>>(), [when, ended]);

        // 2. Interpretation and resolution each land on the mention, and can be fetched back
        time_mentions::set_interpretation(
            &pool,
            when,
            "point",
            &serde_json::json!({ "absolute": "2011-03-04" }),
            "day",
        )
        .await?;
        // The resolved value is truncated to precision: 15:00 becomes midnight of that day
        time_mentions::set_resolution(
            &pool,
            when,
            "A",
            Some(Utc.with_ymd_and_hms(2011, 3, 4, 15, 0, 0).unwrap()),
            Some("day"),
            None,
            None,
        )
        .await?;
        time_mentions::set_interpretation(
            &pool,
            ended,
            "ended",
            &serde_json::json!({ "absolute": "2019" }),
            "year",
        )
        .await?;
        time_mentions::set_resolution(
            &pool,
            ended,
            "A",
            None,
            None,
            Some(day(2019, 6, 1)),
            Some("year"),
        )
        .await?;
        let mentions = time_mentions::for_facts(&pool, &[fact]).await?;
        let m = &mentions[&fact];
        assert_eq!(m.len(), 2);
        let w = m.iter().find(|x| x.id == when).unwrap();
        assert_eq!(
            (
                w.shape.as_deref(),
                w.granularity.as_deref(),
                w.grade.as_deref()
            ),
            (Some("point"), Some("day"), Some("A"))
        );
        assert_eq!(
            w.reference,
            Some(serde_json::json!({ "absolute": "2011-03-04" }))
        );
        assert_eq!(
            (w.resolved_from, w.resolved_from_precision.as_deref()),
            (Some(day(2011, 3, 4)), Some("day"))
        );
        assert!(w.resolved_to.is_none() && w.resolved_to_precision.is_none());
        let e = m.iter().find(|x| x.id == ended).unwrap();
        assert_eq!(
            (e.shape.as_deref(), e.grade.as_deref()),
            (Some("ended"), Some("A"))
        );
        assert_eq!(
            (e.resolved_to, e.resolved_to_precision.as_deref()),
            (Some(day(2019, 1, 1)), Some("year"))
        );
        let resolved_at: Vec<Option<DateTime<Utc>>> =
            sqlx::query_scalar("SELECT resolved_at FROM time_mentions WHERE fact_id = $1")
                .bind(fact)
                .fetch_all(&pool)
                .await?;
        assert!(resolved_at.iter().all(Option::is_some));

        // "Ended, date unknown" can be stored
        time_mentions::set_resolution(&pool, ended, "B", None, None, None, Some("unknown")).await?;
        let e = &time_mentions::for_facts(&pool, &[fact]).await?[&fact]
            .iter()
            .find(|x| x.id == ended)
            .cloned()
            .unwrap();
        assert_eq!(
            (
                e.grade.as_deref(),
                e.resolved_to,
                e.resolved_to_precision.as_deref()
            ),
            (Some("B"), None, Some("unknown"))
        );
        // Not computed: all four columns empty
        time_mentions::set_resolution(&pool, ended, "C", None, None, None, None).await?;
        let e = &time_mentions::for_facts(&pool, &[fact]).await?[&fact]
            .iter()
            .find(|x| x.id == ended)
            .cloned()
            .unwrap();
        assert_eq!(e.grade.as_deref(), Some("C"));
        assert!(
            e.resolved_from.is_none()
                && e.resolved_from_precision.is_none()
                && e.resolved_to.is_none()
                && e.resolved_to_precision.is_none()
        );
        // A value and precision that disagree are rejected by the database: an end with a date cannot say unknown, and the level is only A / B / C
        assert!(time_mentions::set_resolution(
            &pool,
            ended,
            "A",
            None,
            None,
            Some(day(2019, 1, 1)),
            Some("unknown"),
        )
        .await
        .is_err());
        assert!(
            time_mentions::set_resolution(&pool, ended, "D", None, None, None, None)
                .await
                .is_err()
        );

        // 3. The resolution is written to the open row's world axis, truncated to precision
        graph::set_open_validity(
            &pool,
            fact,
            Some(Utc.with_ymd_and_hms(2011, 3, 4, 15, 0, 0).unwrap()),
            Some("day"),
            Some(day(2019, 6, 1)),
            Some("year"),
            Some("A"),
        )
        .await?;
        assert_eq!(
            span_of(&pool, fact).await?,
            Span {
                valid_from: Some(day(2011, 3, 4)),
                valid_from_precision: Some("day".into()),
                valid_to: Some(day(2019, 1, 1)),
                valid_to_precision: Some("year".into()),
            }
        );
        // Ended, date unknown
        graph::set_open_validity(
            &pool,
            fact,
            Some(day(2011, 3, 4)),
            Some("day"),
            None,
            Some("unknown"),
            Some("A"),
        )
        .await?;
        assert_eq!(
            span_of(&pool, fact).await?,
            Span {
                valid_from: Some(day(2011, 3, 4)),
                valid_from_precision: Some("day".into()),
                valid_to: None,
                valid_to_precision: Some("unknown".into()),
            }
        );
        // "Ended, date unknown" has its own anchor: the document that states the end is this statement's own document (#393).
        // This document did not say what day it is, so the statement has no attestation (0064 decision 5); the anchor is the moment the ledger recorded it
        let anchored: (bool,) = sqlx::query_as(
            "SELECT attested_to IS NOT NULL AND attested_to = COALESCE(attested_from, recorded_at)
             FROM facts WHERE id = $1",
        )
        .bind(fact)
        .fetch_one(&pool)
        .await?;
        assert!(
            anchored.0,
            "an unknown ending is anchored on its own document"
        );
        // Once it cannot be computed again: cleared, and the anchor goes with it
        graph::set_open_validity(&pool, fact, None, None, None, None, None).await?;
        assert_eq!(
            span_of(&pool, fact).await?,
            Span {
                valid_from: None,
                valid_from_precision: None,
                valid_to: None,
                valid_to_precision: None,
            }
        );
        let anchored: (bool,) =
            sqlx::query_as("SELECT attested_to IS NOT NULL FROM facts WHERE id = $1")
                .bind(fact)
                .fetch_one(&pool)
                .await?;
        assert!(!anchored.0);

        // 4. Typed rows are not written by it: none changed
        let typed = Uuid::now_v7();
        sqlx::query("INSERT INTO facts (id, kb_id, subject_id, object_id) VALUES ($1, $2, $3, $4)")
            .bind(typed)
            .bind(f.kb)
            .bind(f.acme)
            .bind(f.beta)
            .execute(&pool)
            .await?;
        let refused = graph::set_open_validity(
            &pool,
            typed,
            Some(day(2011, 3, 4)),
            Some("day"),
            None,
            None,
            None,
        )
        .await;
        assert!(refused.is_err(), "a typed row keeps to insert_fact_inner");
        assert_eq!(
            span_of(&pool, typed).await?,
            Span {
                valid_from: None,
                valid_from_precision: None,
                valid_to: None,
                valid_to_precision: None,
            }
        );
        // A voided open row is history, and is not changed either
        assert!(graph::set_open_validity(
            &pool,
            founded,
            Some(day(2009, 1, 1)),
            Some("year"),
            None,
            None,
            None
        )
        .await
        .is_err());
        Ok::<(), anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(ORG)
        .execute(&pool)
        .await?;
    run
}
