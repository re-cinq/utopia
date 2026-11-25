//! A timeline depends only on which rows it has (regression kept from the reproduction of the #679 second-round review).
//!
//! The first version recognized the engine's drawn boundary by "the end exactly equals another row's start": if the later span moved (a late-arriving source pushed dates earlier and filled in the start), the boundary went unrecognized, stayed in place, and the two spans overlapped; tidying ran one round at a time,
//! and when the rounds ran out it silently stopped. Retracting a merge edited rows before taking the lock, which deadlocked against reconciliation with the database, and it brought back rows a person had rejected and dropped
//! intervals a person had edited. Now the end the engine draws is recorded on the row (0057),
//! and it is recomputed once per call from the rows as they stand now.
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. It creates and tears down its own data and never touches an existing database.

use serde_json::json;
use sqlx::PgPool;
use utopia_store::graph::Validity;
use utopia_store::temporal::Uniqueness;
use uuid::Uuid;

struct Fixture {
    kb: Uuid,
    etype: Uuid,
    lease: Uuid,
    deadline: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (etype, deadline, lease) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'recompute-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'recompute-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'recompute-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'lease', 'Lease')",
    )
    .bind(etype)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind, datatype, temporal, functional)
         VALUES ($1, $2, 'option_deadline', 'option deadline', 'attribute', 'date', 'state', TRUE)",
    )
    .bind(deadline)
    .bind(kb)
    .execute(pool)
    .await?;
    let lease = entity(pool, kb, etype, "HQ Lease", lease).await?;
    Ok(Fixture {
        kb,
        etype,
        lease,
        deadline,
    })
}

async fn entity(
    pool: &PgPool,
    kb: Uuid,
    etype: Uuid,
    name: &str,
    id: Uuid,
) -> anyhow::Result<Uuid> {
    sqlx::query(
        "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(kb)
    .bind(etype)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(id)
}

fn t(day: &str) -> chrono::DateTime<chrono::Utc> {
    format!("{day}T00:00:00Z").parse().unwrap()
}

/// One observation: value, start and end, which kind of document on which day it came from, confidence, and where the start came from
#[derive(Clone, Copy)]
struct Seen<'a> {
    subject: Uuid,
    value: &'a str,
    from: Option<&'a str>,
    to: Option<&'a str>,
    doc: Option<&'a str>,
    confidence: f32,
    /// `None` = no time parsing happened (the default for these tests), `Some("C")` = a time word was present but could not be anchored
    grade: Option<&'a str>,
}

fn seen<'a>(f: &Fixture, value: &'a str, from: &'a str) -> Seen<'a> {
    Seen {
        subject: f.lease,
        value,
        from: Some(from),
        to: None,
        doc: None,
        confidence: 0.9,
        grade: None,
    }
}

/// The extraction path: write (or merge into an existing assertion), write the evidence, and reconcile -- **merged assertions are reconciled too**
async fn observe(pool: &PgPool, f: &Fixture, x: Seen<'_>) -> anyhow::Result<Uuid> {
    let mut validity = Validity {
        from: x.from.map(t),
        from_precision: x.from.map(|_| "day"),
        to: x.to.map(t),
        to_precision: x.to.map(|_| "day"),
        attested_at: None,
        from_grade: x.grade,
        ..Default::default()
    };
    let mut chunk = None;
    if let Some(doc_time) = x.doc {
        let (d, c) = (Uuid::now_v7(), Uuid::now_v7());
        sqlx::query(
            "INSERT INTO documents (id, kb_id, filename, sha256, doc_time, doc_time_source)
             VALUES ($1, $2, $3, $3, $4, 'content')",
        )
        .bind(d)
        .bind(f.kb)
        .bind(format!("doc-{d}.html"))
        .bind(t(doc_time))
        .execute(pool)
        .await?;
        sqlx::query(
            "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, $4)",
        )
        .bind(c)
        .bind(f.kb)
        .bind(d)
        .bind(x.value)
        .execute(pool)
        .await?;
        validity = validity.attested(Some(t(doc_time)));
        chunk = Some(c);
    }
    let object = json!({ "value": x.value });
    let (id, _created) = utopia_store::graph::insert_value_fact(
        pool,
        f.kb,
        x.subject,
        Some(f.deadline),
        &object,
        validity,
        x.confidence,
    )
    .await?;
    if let Some(c) = chunk {
        utopia_store::graph::add_evidence(pool, id, c, Some(x.value), None).await?;
    }
    utopia_store::temporal::reconcile_new_fact(
        pool,
        f.kb,
        id,
        x.subject,
        f.deadline,
        None,
        Some(&object),
        Uniqueness::SubjectSide,
        validity,
        x.confidence,
    )
    .await?;
    Ok(id)
}

/// (value, start, end, end precision, end anchor, start read out, end read out)
type Seg = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

async fn timeline(pool: &PgPool, f: &Fixture, subject: Uuid) -> anyhow::Result<Vec<Seg>> {
    let hf = utopia_store::world_axis::facts_holds_from("f");
    let ht = utopia_store::world_axis::facts_holds_to("f");
    Ok(sqlx::query_as(&format!(
        "SELECT object_value #>> '{{value}}', to_char(valid_from, 'YYYY-MM-DD'),
                to_char(valid_to, 'YYYY-MM-DD'), valid_to_precision,
                to_char(attested_to, 'YYYY-MM-DD'),
                to_char({hf}, 'YYYY-MM-DD'), to_char({ht}, 'YYYY-MM-DD')
         FROM facts f
         WHERE kb_id = $1 AND subject_id = $2 AND predicate_id = $3 AND invalidated_at IS NULL
         ORDER BY {hf} NULLS FIRST, {ht} NULLS LAST, object_value #>> '{{value}}'"
    ))
    .bind(f.kb)
    .bind(subject)
    .bind(f.deadline)
    .fetch_all(pool)
    .await?)
}

/// How many pairs of rows have overlapping read-out intervals but different values
async fn overlaps(pool: &PgPool, f: &Fixture, subject: Uuid) -> anyhow::Result<i64> {
    let hf = utopia_store::world_axis::facts_holds_from("f");
    let ht = utopia_store::world_axis::facts_holds_to("f");
    Ok(sqlx::query_scalar(&format!(
        "WITH r AS (
            SELECT f.id, f.object_value, {hf} AS hf, {ht} AS ht FROM facts f
            WHERE f.kb_id = $1 AND f.subject_id = $2 AND f.predicate_id = $3
              AND f.invalidated_at IS NULL)
         SELECT count(*) FROM r a JOIN r b ON a.id < b.id
            AND a.object_value IS DISTINCT FROM b.object_value
         WHERE COALESCE(a.hf, '-infinity') < COALESCE(b.ht, 'infinity')
           AND COALESCE(b.hf, '-infinity') < COALESCE(a.ht, 'infinity')"
    ))
    .bind(f.kb)
    .bind(subject)
    .bind(f.deadline)
    .fetch_one(pool)
    .await?)
}

async fn open_rows(pool: &PgPool, f: &Fixture, subject: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM facts WHERE kb_id = $1 AND subject_id = $2 AND predicate_id = $3
           AND invalidated_at IS NULL AND valid_to IS NULL AND valid_to_precision IS NULL",
    )
    .bind(f.kb)
    .bind(subject)
    .bind(f.deadline)
    .fetch_one(pool)
    .await?)
}

async fn live_id(pool: &PgPool, f: &Fixture, subject: Uuid, value: &str) -> anyhow::Result<Uuid> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM facts WHERE kb_id = $1 AND subject_id = $2
           AND object_value #>> '{value}' = $3 AND invalidated_at IS NULL",
    )
    .bind(f.kb)
    .bind(subject)
    .bind(value)
    .fetch_one(pool)
    .await?)
}

async fn cleanup(pool: &PgPool, f: &Fixture) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(pool)
        .await?;
    Ok(())
}

async fn pool() -> anyhow::Result<Option<PgPool>> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(None);
    };
    Ok(Some(
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(12)
            .connect(&url)
            .await?,
    ))
}

/// A later span's start was filled in by a late-arriving file (after the eleventh supplementary agreement, the delivery file says BBHQ1 has been the landlord since July 31),
/// so HPBB1 ends on that day and no longer overlaps to August 13
#[tokio::test]
async fn a_start_that_arrives_later_moves_the_end_before_it() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    observe(
        &pool,
        &f,
        Seen {
            doc: Some("2016-05-16"),
            ..seen(&f, "HPBB1", "2016-05-16")
        },
    )
    .await?;
    observe(
        &pool,
        &f,
        Seen {
            from: None,
            doc: Some("2020-08-13"),
            ..seen(&f, "BBHQ1", "")
        },
    )
    .await?;
    observe(
        &pool,
        &f,
        Seen {
            doc: Some("2020-09-01"),
            ..seen(&f, "BBHQ1", "2020-07-31")
        },
    )
    .await?;
    let rows = timeline(&pool, &f, f.lease).await?;
    let n = overlaps(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(n, 0, "{rows:?}");
    assert_eq!(rows[0].0, "HPBB1");
    assert_eq!(rows[0].2.as_deref(), Some("2020-07-31"), "{rows:?}");
    Ok(())
}

/// A successor with no start was raised by an earlier file: the predecessor's anchor moves earlier with it
#[tokio::test]
async fn earlier_evidence_for_a_value_with_no_start_moves_the_end_before_it() -> anyhow::Result<()>
{
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    observe(
        &pool,
        &f,
        Seen {
            doc: Some("2019-01-01"),
            ..seen(&f, "P", "2019-01-01")
        },
    )
    .await?;
    let startless = |doc| Seen {
        from: None,
        doc: Some(doc),
        ..seen(&f, "S", "")
    };
    observe(&pool, &f, startless("2021-06-01")).await?;
    observe(&pool, &f, startless("2020-06-01")).await?;
    let rows = timeline(&pool, &f, f.lease).await?;
    let n = overlaps(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(n, 0, "{rows:?}");
    assert_eq!(rows[0].0, "P");
    assert_eq!(rows[0].4.as_deref(), Some("2020-06-01"), "{rows:?}");
    Ok(())
}

/// A value with no start arrives first and one with a start arrives later: the document date is never written into the date columns, and nothing overlaps
#[tokio::test]
async fn a_value_with_no_start_that_arrives_first_still_takes_its_place() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    observe(
        &pool,
        &f,
        Seen {
            from: None,
            doc: Some("2021-01-01"),
            ..seen(&f, "S", "")
        },
    )
    .await?;
    observe(&pool, &f, seen(&f, "P", "2019-01-01")).await?;
    observe(&pool, &f, seen(&f, "Q", "2020-01-01")).await?;
    let n = overlaps(&pool, &f, f.lease).await?;
    let in_date_columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM facts
          WHERE kb_id = $1 AND (valid_from = '2021-01-01' OR valid_to = '2021-01-01')",
    )
    .bind(f.kb)
    .fetch_one(&pool)
    .await?;
    let open = open_rows(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!((n, in_date_columns, open), (0, 0, 1));
    Ok(())
}

/// A new value that has already ended and an old value still open: the order of arrival makes no difference
#[tokio::test]
async fn a_closed_value_and_an_open_one_meet_the_same_way_in_either_order() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let other = entity(&pool, f.kb, f.etype, "Other Lease", Uuid::now_v7()).await?;
    let closed = |subject| Seen {
        subject,
        to: Some("2021-01-01"),
        ..seen(&f, "B", "2019-01-01")
    };
    observe(&pool, &f, seen(&f, "A", "2018-01-01")).await?;
    observe(&pool, &f, closed(f.lease)).await?;
    observe(&pool, &f, closed(other)).await?;
    observe(
        &pool,
        &f,
        Seen {
            subject: other,
            ..seen(&f, "A", "2018-01-01")
        },
    )
    .await?;
    let a_then_b = timeline(&pool, &f, f.lease).await?;
    let b_then_a = timeline(&pool, &f, other).await?;
    let n = overlaps(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(a_then_b, b_then_a);
    assert_eq!(n, 0);
    Ok(())
}

/// Seventy values with insufficient confidence come first, then one qualifying value arrives: each one is closed, and there is no such thing as running out of rounds
#[tokio::test]
async fn unanchored_values_do_not_keep_a_placed_one_from_closing_them() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    for i in 0..70 {
        let (value, day) = (
            format!("w{i:02}"),
            format!("{}-{:02}-01", 2000 + i / 12, i % 12 + 1),
        );
        observe(
            &pool,
            &f,
            Seen {
                grade: Some("C"),
                ..seen(&f, &value, &day)
            },
        )
        .await?;
    }
    observe(&pool, &f, seen(&f, "Z", "2010-01-01")).await?;
    let open = open_rows(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(open, 1, "只有 Z 还开着");
    Ok(())
}

/// A successor whose start cannot be anchored does not close its predecessor (0045 cut 3): it can enter the graph and be placed on the timeline, but its position on the axis
/// comes from the document date, not from the sentence, so rewriting history with it means rewriting history with a guessed position
#[tokio::test]
async fn an_unanchored_successor_closes_nothing() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    observe(&pool, &f, seen(&f, "first", "2020-01-01")).await?;
    observe(
        &pool,
        &f,
        Seen {
            grade: Some("C"),
            ..seen(&f, "second", "2021-01-01")
        },
    )
    .await?;
    let open = open_rows(&pool, &f, f.lease).await?;
    // Another one that can be anchored: this one is what closes the two before it
    observe(&pool, &f, seen(&f, "third", "2022-01-01")).await?;
    let after = open_rows(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(open, 2, "锚不到的那一行关不了它前面的");
    assert_eq!(after, 1, "锚得住的一来，前面两行都关上");
    Ok(())
}

/// Two timelines of a hundred values each, interleaved and merged: one recompute sorts it out, with no round limit
#[tokio::test]
async fn a_merge_of_two_long_timelines_leaves_one_timeline() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let source = entity(&pool, f.kb, f.etype, "Lease Agreement", Uuid::now_v7()).await?;
    for i in 0..200 {
        let (value, day) = (
            format!("v{i:03}"),
            format!("{}-{:02}-01", 2010 + i / 12, i % 12 + 1),
        );
        let subject = if i % 2 == 0 { f.lease } else { source };
        observe(
            &pool,
            &f,
            Seen {
                subject,
                ..seen(&f, &value, &day)
            },
        )
        .await?;
    }
    utopia_store::resolution::merge_entities(&pool, f.kb, source, f.lease, None, "test").await?;
    let n = overlaps(&pool, &f, f.lease).await?;
    let open = open_rows(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!((n, open), (0, 1));
    Ok(())
}

/// Merge two interleaved timelines and then retract: both sides return to how they were before the merge
#[tokio::test]
async fn a_revert_gives_both_timelines_back_as_they_were() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let source = entity(&pool, f.kb, f.etype, "Lease Agreement", Uuid::now_v7()).await?;
    for (v, d) in [
        ("A", "2020-01-01"),
        ("C", "2020-05-01"),
        ("E", "2020-09-01"),
    ] {
        observe(&pool, &f, seen(&f, v, d)).await?;
    }
    for (v, d) in [
        ("B", "2020-03-01"),
        ("D", "2020-07-01"),
        ("F", "2020-11-01"),
    ] {
        observe(
            &pool,
            &f,
            Seen {
                subject: source,
                ..seen(&f, v, d)
            },
        )
        .await?;
    }
    let (target_before, source_before) = (
        timeline(&pool, &f, f.lease).await?,
        timeline(&pool, &f, source).await?,
    );
    let merge =
        utopia_store::resolution::merge_entities(&pool, f.kb, source, f.lease, None, "test")
            .await?;
    let merged = timeline(&pool, &f, f.lease).await?;
    let merged_overlaps = overlaps(&pool, &f, f.lease).await?;
    utopia_store::resolution::revert_merge(&pool, f.kb, merge).await?;
    let (target_after, source_after) = (
        timeline(&pool, &f, f.lease).await?,
        timeline(&pool, &f, source).await?,
    );
    cleanup(&pool, &f).await?;
    assert_eq!((merged.len(), merged_overlaps), (6, 0));
    assert_eq!(target_after, target_before);
    assert_eq!(source_after, source_before);
    Ok(())
}

/// After merging, another value arrives and closes the moved row: retracting restores that row (together with the rewrite that closed it) back to its source entity
#[tokio::test]
async fn a_revert_takes_back_a_moved_value_closed_after_the_merge() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let source = entity(&pool, f.kb, f.etype, "Lease Agreement", Uuid::now_v7()).await?;
    observe(&pool, &f, seen(&f, "A", "2019-01-01")).await?;
    observe(
        &pool,
        &f,
        Seen {
            subject: source,
            ..seen(&f, "D", "2020-04-01")
        },
    )
    .await?;
    let merge =
        utopia_store::resolution::merge_entities(&pool, f.kb, source, f.lease, None, "test")
            .await?;
    observe(&pool, &f, seen(&f, "N", "2020-06-01")).await?;
    utopia_store::resolution::revert_merge(&pool, f.kb, merge).await?;
    let target = timeline(&pool, &f, f.lease).await?;
    let back = timeline(&pool, &f, source).await?;
    cleanup(&pool, &f).await?;
    assert!(target.iter().all(|r| r.0 != "D"), "{target:?}");
    assert_eq!(back.len(), 1);
    assert_eq!((back[0].0.as_str(), back[0].2.as_deref()), ("D", None));
    assert_eq!(target[0].2.as_deref(), Some("2020-06-01"), "A 止于 N");
    Ok(())
}

/// Merge closed A when D starts, then a person changed A's start to 2019-12-01: after retracting, the row a person edited is still there
#[tokio::test]
async fn a_revert_keeps_the_interval_a_person_corrected() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let source = entity(&pool, f.kb, f.etype, "Lease Agreement", Uuid::now_v7()).await?;
    observe(&pool, &f, seen(&f, "A", "2020-01-01")).await?;
    observe(
        &pool,
        &f,
        Seen {
            subject: source,
            ..seen(&f, "D", "2020-04-01")
        },
    )
    .await?;
    let merge =
        utopia_store::resolution::merge_entities(&pool, f.kb, source, f.lease, None, "test")
            .await?;
    let a = live_id(&pool, &f, f.lease, "A").await?;
    let fixed = utopia_store::temporal::correct_interval(
        &pool,
        a,
        Validity {
            from: Some(t("2019-12-01")),
            from_precision: Some("day"),
            to: Some(t("2020-04-01")),
            to_precision: Some("day"),
            attested_at: None,
            from_grade: None,
            ..Default::default()
        },
    )
    .await?
    .unwrap();
    utopia_store::temporal::reconcile_moved_facts(&pool, f.kb, &[fixed]).await?;
    utopia_store::resolution::revert_merge(&pool, f.kb, merge).await?;
    let rows = timeline(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(
        rows,
        vec![(
            "A".to_string(),
            Some("2019-12-01".to_string()),
            Some("2020-04-01".to_string()),
            Some("day".to_string()),
            None,
            Some("2019-12-01".to_string()),
            Some("2020-04-01".to_string()),
        )]
    );
    Ok(())
}

/// After the merge closed A, a person rejected A: retracting the merge does not bring it back
#[tokio::test]
async fn a_revert_does_not_bring_back_a_value_a_person_rejected() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let source = entity(&pool, f.kb, f.etype, "Lease Agreement", Uuid::now_v7()).await?;
    observe(&pool, &f, seen(&f, "A", "2020-01-01")).await?;
    observe(
        &pool,
        &f,
        Seen {
            subject: source,
            ..seen(&f, "D", "2020-04-01")
        },
    )
    .await?;
    let merge =
        utopia_store::resolution::merge_entities(&pool, f.kb, source, f.lease, None, "test")
            .await?;
    let a = live_id(&pool, &f, f.lease, "A").await?;
    utopia_store::graph::reject_fact(&pool, f.kb, a).await?;
    utopia_store::resolution::revert_merge(&pool, f.kb, merge).await?;
    let rows = timeline(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert!(rows.iter().all(|r| r.0 != "A"), "{rows:?}");
    Ok(())
}

/// A person rejected the successor: the predecessor that was closed at its start is open again
#[tokio::test]
async fn rejecting_a_successor_reopens_the_value_it_closed() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    observe(&pool, &f, seen(&f, "A", "2020-01-01")).await?;
    let b = observe(&pool, &f, seen(&f, "B", "2020-04-01")).await?;
    utopia_store::graph::reject_fact(&pool, f.kb, b).await?;
    let open = open_rows(&pool, &f, f.lease).await?;
    let rows = timeline(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(open, 1);
    assert_eq!((rows[0].0.as_str(), rows[0].2.as_deref()), ("A", None));
    Ok(())
}

/// An end a person wrote is not recomputed by the timeline: the successor left, yet it stays closed
#[tokio::test]
async fn an_end_a_person_wrote_is_never_recomputed() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let a = observe(&pool, &f, seen(&f, "A", "2020-01-01")).await?;
    utopia_store::temporal::close_superseded(&pool, a, t("2020-03-01"), "day").await?;
    let b = observe(&pool, &f, seen(&f, "B", "2020-04-01")).await?;
    utopia_store::graph::reject_fact(&pool, f.kb, b).await?;
    let rows = timeline(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2.as_deref(), Some("2020-03-01"));
    Ok(())
}

/// Deleting the document the successor came from reopens the predecessor; restoring the document closes it again
#[tokio::test]
async fn deleting_and_restoring_a_document_moves_the_end_it_drew() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    observe(&pool, &f, seen(&f, "A", "2020-01-01")).await?;
    let b = observe(
        &pool,
        &f,
        Seen {
            doc: Some("2020-04-01"),
            ..seen(&f, "B", "2020-04-01")
        },
    )
    .await?;
    let doc: Uuid = sqlx::query_scalar("SELECT document_id FROM fact_evidence WHERE fact_id = $1")
        .bind(b)
        .fetch_one(&pool)
        .await?;
    let a_end = || async {
        let (end,): (Option<String>,) = sqlx::query_as(
            "SELECT to_char(valid_to, 'YYYY-MM-DD') FROM facts
              WHERE kb_id = $1 AND object_value #>> '{value}' = 'A' AND invalidated_at IS NULL",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        anyhow::Ok(end)
    };
    assert_eq!(a_end().await?.as_deref(), Some("2020-04-01"));
    utopia_store::documents::delete(&pool, f.kb, doc, None).await?;
    let deleted = a_end().await?;
    utopia_store::documents::restore(&pool, f.kb, doc).await?;
    let restored = a_end().await?;
    cleanup(&pool, &f).await?;
    assert_eq!(deleted, None);
    assert_eq!(restored.as_deref(), Some("2020-04-01"));
    Ok(())
}

/// Parallel writes: three, then five values arrive at once, and in the end they form one timeline
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn values_that_arrive_together_leave_one_timeline() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    for _ in 0..10 {
        let f = seed(&pool).await?;
        observe(&pool, &f, seen(&f, "v1", "2020-02-18")).await?;
        observe(&pool, &f, seen(&f, "v5", "2020-06-08")).await?;
        let (x, y, z) = tokio::join!(
            observe(&pool, &f, seen(&f, "v4", "2020-05-10")),
            observe(&pool, &f, seen(&f, "v2", "2020-03-17")),
            observe(&pool, &f, seen(&f, "v3", "2020-04-14")),
        );
        x?;
        y?;
        z?;
        let three = (
            overlaps(&pool, &f, f.lease).await?,
            open_rows(&pool, &f, f.lease).await?,
        );
        cleanup(&pool, &f).await?;
        assert_eq!(three, (0, 1));

        let f = seed(&pool).await?;
        let (p, q, r, s, u) = tokio::join!(
            observe(&pool, &f, seen(&f, "v4", "2020-05-10")),
            observe(&pool, &f, seen(&f, "v2", "2020-03-17")),
            observe(&pool, &f, seen(&f, "v5", "2020-06-08")),
            observe(&pool, &f, seen(&f, "v1", "2020-02-18")),
            observe(&pool, &f, seen(&f, "v3", "2020-04-14")),
        );
        for result in [p, q, r, s, u] {
            result?;
        }
        let five = (
            timeline(&pool, &f, f.lease).await?.len(),
            overlaps(&pool, &f, f.lease).await?,
            open_rows(&pool, &f, f.lease).await?,
        );
        cleanup(&pool, &f).await?;
        assert_eq!(five, (5, 0, 1));
    }
    Ok(())
}

/// Retracting a merge and writing to the database happen at the same time: neither waits on the other to death, and in the end no intervals overlap
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revert_and_arriving_values_do_not_deadlock() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let mut errors = Vec::new();
    for i in 0..40 {
        let f = seed(&pool).await?;
        let source = entity(&pool, f.kb, f.etype, "Lease Agreement", Uuid::now_v7()).await?;
        for (v, d) in [
            ("A", "2020-01-01"),
            ("C", "2020-05-01"),
            ("E", "2020-09-01"),
        ] {
            observe(&pool, &f, seen(&f, v, d)).await?;
        }
        for (v, d) in [("B", "2020-03-01"), ("D", "2020-07-01")] {
            observe(
                &pool,
                &f,
                Seen {
                    subject: source,
                    ..seen(&f, v, d)
                },
            )
            .await?;
        }
        let merge =
            utopia_store::resolution::merge_entities(&pool, f.kb, source, f.lease, None, "test")
                .await?;
        let delay = std::time::Duration::from_millis(i % 6);
        let (reverted, n, m) = tokio::join!(
            utopia_store::resolution::revert_merge(&pool, f.kb, merge),
            async {
                tokio::time::sleep(delay).await;
                observe(&pool, &f, seen(&f, "N", "2020-02-01")).await
            },
            observe(&pool, &f, seen(&f, "M", "2020-08-01")),
        );
        if let Err(e) = reverted {
            errors.push(format!("revert: {e}"));
        }
        for arrived in [n, m] {
            if let Err(e) = arrived {
                errors.push(format!("arrive: {e}"));
            }
        }
        let (target, back) = (
            overlaps(&pool, &f, f.lease).await?,
            overlaps(&pool, &f, source).await?,
        );
        cleanup(&pool, &f).await?;
        assert_eq!((target, back), (0, 0), "trial {i}");
    }
    assert!(errors.is_empty(), "{errors:?}");
    Ok(())
}
