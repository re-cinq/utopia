//! The engine redraws only the endpoints it drew itself (a reproduction of the #679 third-round review, kept as a regression).
//!
//! The second version recorded "endpoints the engine drew" on the row and recomputed them each time. The review found where else it broke: a relation unique on both sides
//! closed on one side and opened on the other; an old value the source says had already ended was taken as the successor and closed the current value; undoing a merge
//! made the record-axis replay of the merge window return a different answer; deleting a document read the roster before taking the lock; a rewrite withdrew a conflict nobody had ruled on and re-asked
//! one a person had ruled on; an end stated by the source was still recomputed after being merged into a row the engine had closed; a deleted document still ordered rows;
//! evidence was written onto an old row that had just been rewritten; for a value with no time, whichever arrived first gave a different result; rows closed before the migration were never recomputed.
//!
//! Without `UTOPIA_DATABASE_URL` it skips rather than fails. It sets up and tears down its own data and never touches an existing database.

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
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'redraw-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'redraw-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'redraw-test')",
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

/// One observation: value, start and end, the document's date it came from, and confidence
#[derive(Clone, Copy)]
struct Seen<'a> {
    subject: Uuid,
    value: &'a str,
    from: Option<&'a str>,
    to: Option<&'a str>,
    ended_unknown: bool,
    doc: Option<&'a str>,
    confidence: f32,
    /// This observation came from text describing a picture (0040 decision 4): it may not alone close a correct older span
    described: bool,
}

fn seen<'a>(f: &Fixture, value: &'a str, from: &'a str) -> Seen<'a> {
    Seen {
        subject: f.lease,
        value,
        from: Some(from),
        to: None,
        ended_unknown: false,
        doc: None,
        confidence: 0.9,
        described: false,
    }
}

/// One document that carries its own date, one chunk
async fn document(pool: &PgPool, kb: Uuid, day: &str, text: &str) -> anyhow::Result<(Uuid, Uuid)> {
    let (d, c) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, doc_time, doc_time_source)
         VALUES ($1, $2, $3, $3, $4, 'content')",
    )
    .bind(d)
    .bind(kb)
    .bind(format!("doc-{d}.html"))
    .bind(t(day))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, $4)",
    )
    .bind(c)
    .bind(kb)
    .bind(d)
    .bind(text)
    .execute(pool)
    .await?;
    Ok((d, c))
}

/// The extraction pattern: stored (or merged into an existing assertion), evidence written, reconciled
async fn observe(pool: &PgPool, f: &Fixture, x: Seen<'_>) -> anyhow::Result<Uuid> {
    let mut validity = Validity {
        from: x.from.map(t),
        from_precision: x.from.map(|_| "day"),
        from_grade: None,
        to: x.to.map(t),
        to_precision: if x.ended_unknown {
            Some("unknown")
        } else {
            x.to.map(|_| "day")
        },
        attested_at: None,
        ..Default::default()
    };
    let mut chunk = None;
    if let Some(day) = x.doc {
        let (_, c) = document(pool, f.kb, day, x.value).await?;
        validity = validity.attested(Some(t(day)));
        chunk = Some(c);
    }
    let object = json!({ "value": x.value });
    let (id, _) = utopia_store::graph::insert_value_fact(
        pool,
        f.kb,
        x.subject,
        Some(f.deadline),
        &object,
        validity,
        x.confidence,
    )
    .await?;
    if x.from.is_none() && x.doc.is_none() {
        // The simulated document has no time (0064), so it does not use the moment-of-entry witness of the manual entry point.
        sqlx::query("UPDATE facts SET attested_from = NULL WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(c) = chunk {
        if x.described {
            // A described chunk must carry an anchor (0058's CHECK): which chunk in the file the diagram is in
            sqlx::query(
                "UPDATE chunks SET origin = 'described', origin_model = 'test-vision',
                        anchor = jsonb_build_object('page', 1, 'image', 0) WHERE id = $1",
            )
            .bind(c)
            .execute(pool)
            .await?;
        }
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

/// (value, start, end, end precision, end anchor)
type Seg = (
    String,
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
                to_char(attested_to, 'YYYY-MM-DD')
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

async fn open_conflicts(pool: &PgPool, kb: Uuid) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT reason FROM fact_conflicts WHERE kb_id = $1 AND status = 'open' ORDER BY reason",
    )
    .bind(kb)
    .fetch_all(pool)
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
            .max_connections(16)
            .connect(&url)
            .await?,
    ))
}

/// "One person leads only one project at a time, and one project has only one leader": a row sits on two timelines.
/// Pat leads A first, then B; A closes when B starts; reading "Pat leads A" again changes nothing
#[tokio::test]
async fn a_relation_unique_on_both_sides_does_not_flip_between_them() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let leads = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind, temporal, functional, inverse_functional)
         VALUES ($1, $2, 'leads', 'leads', 'relation', 'state', TRUE, TRUE)",
    )
    .bind(leads)
    .bind(f.kb)
    .execute(&pool)
    .await?;
    let pat = entity(&pool, f.kb, f.etype, "Pat", Uuid::now_v7()).await?;
    let a = entity(&pool, f.kb, f.etype, "Project A", Uuid::now_v7()).await?;
    let b = entity(&pool, f.kb, f.etype, "Project B", Uuid::now_v7()).await?;
    let edge = |object: Uuid, from: &'static str| {
        let pool = pool.clone();
        let kb = f.kb;
        async move {
            let validity = Validity::starting(Some(t(from)), Some("day"));
            let (id, _) = utopia_store::graph::insert_fact(
                &pool,
                kb,
                pat,
                Some(leads),
                object,
                validity,
                0.9,
            )
            .await?;
            for side in [Uniqueness::SubjectSide, Uniqueness::ObjectSide] {
                utopia_store::temporal::reconcile_new_fact(
                    &pool,
                    kb,
                    id,
                    pat,
                    leads,
                    Some(object),
                    None,
                    side,
                    validity,
                    0.9,
                )
                .await?;
            }
            anyhow::Ok(())
        }
    };
    let state = || {
        let pool = pool.clone();
        let kb = f.kb;
        async move {
            let rows: Vec<(String, Option<String>)> = sqlx::query_as(
                "SELECT o.canonical_name, to_char(x.valid_to, 'YYYY-MM-DD')
                   FROM facts x JOIN entities o ON o.id = x.object_id
                  WHERE x.kb_id = $1 AND x.predicate_id = $2 AND x.invalidated_at IS NULL
                  ORDER BY x.valid_from",
            )
            .bind(kb)
            .bind(leads)
            .fetch_all(&pool)
            .await?;
            let total: i64 =
                sqlx::query_scalar("SELECT count(*) FROM facts WHERE predicate_id = $1")
                    .bind(leads)
                    .fetch_one(&pool)
                    .await?;
            anyhow::Ok((rows, total))
        }
    };
    edge(a, "2020-01-01").await?;
    edge(b, "2021-01-01").await?;
    let after_b = state().await?;
    edge(a, "2020-01-01").await?;
    edge(a, "2020-01-01").await?;
    let again = state().await?;
    cleanup(&pool, &f).await?;
    assert_eq!(
        after_b.0,
        vec![
            ("Project A".to_string(), Some("2021-01-01".to_string())),
            ("Project B".to_string(), None),
        ]
    );
    assert_eq!(
        again, after_b,
        "再读一遍没变的事实，时间线不该变，也不该多出行"
    );
    Ok(())
}

/// The source says an older value had already ended ("HPBB1 is no longer the landlord", from a 2021 filing): that file's date only shows
/// that it had ended before that day; it must not be placed in 2021 and used to close the current value
#[tokio::test]
async fn a_value_the_text_says_has_ended_does_not_close_the_current_one() -> anyhow::Result<()> {
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
            doc: Some("2020-09-01"),
            ..seen(&f, "BBHQ1", "2020-07-31")
        },
    )
    .await?;
    observe(
        &pool,
        &f,
        Seen {
            from: None,
            ended_unknown: true,
            doc: Some("2021-03-01"),
            ..seen(&f, "HPBB1", "")
        },
    )
    .await?;
    let recital = open_rows(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;

    // Ends on a stated day, no start, from a later filing
    let f = seed(&pool).await?;
    observe(&pool, &f, seen(&f, "NEW", "2020-01-01")).await?;
    observe(
        &pool,
        &f,
        Seen {
            from: None,
            to: Some("2019-12-31"),
            doc: Some("2021-03-01"),
            ..seen(&f, "OLD", "")
        },
    )
    .await?;
    let dated = open_rows(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!((recital, dated), (1, 1));
    Ok(())
}

/// A merge; after the merge a value arrived, closing the moved-in row; then undo. Asking about a moment in the replayed merge window,
/// the answer is the same before and after the undo
#[tokio::test]
async fn a_revert_does_not_rewrite_what_the_merge_window_held() -> anyhow::Result<()> {
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
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mid: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await?;
    let replay = || {
        let pool = pool.clone();
        let (kb, pred, lease) = (f.kb, f.deadline, f.lease);
        async move {
            let held = utopia_store::record_axis::facts_held_at("f", Some(2));
            let owner = utopia_store::record_axis::owner_at("f", "subject_id", Some(2), false);
            let rows: Vec<(String, Option<String>, bool)> = sqlx::query_as(&format!(
                "SELECT object_value #>> '{{value}}', to_char(valid_to, 'YYYY-MM-DD'), {owner} = $3
                 FROM facts f WHERE f.kb_id = $1 AND f.predicate_id = $4 AND {held}
                 ORDER BY valid_from"
            ))
            .bind(kb)
            .bind(mid)
            .bind(lease)
            .bind(pred)
            .fetch_all(&pool)
            .await?;
            anyhow::Ok(rows)
        }
    };
    let before = replay().await?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    utopia_store::resolution::revert_merge(&pool, f.kb, merge).await?;
    let after = replay().await?;
    let back = timeline(&pool, &f, source).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(after, before);
    assert_eq!(back.len(), 1, "D returns to the source entity");
    Ok(())
}

/// While deleting the document waited for the lock, that row was rewritten by a timeline recompute into a new row: after locking, reread the roster, and the new row is voided too
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deletion_that_waited_for_the_lock_still_takes_the_rewritten_row() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let a = observe(
        &pool,
        &f,
        Seen {
            doc: Some("2020-01-01"),
            ..seen(&f, "A", "2020-01-01")
        },
    )
    .await?;
    let doc: Uuid = sqlx::query_scalar("SELECT document_id FROM fact_evidence WHERE fact_id = $1")
        .bind(a)
        .fetch_one(&pool)
        .await?;
    // Holds the lock the way a timeline recompute does, rewriting A into a new row
    let key = format!("timeline:{}:{}:{}:SubjectSide", f.kb, f.lease, f.deadline);
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(&key)
        .execute(&mut *tx)
        .await?;
    let deleting = {
        let pool = pool.clone();
        let kb = f.kb;
        tokio::spawn(async move { utopia_store::documents::delete(&pool, kb, doc, None).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let rewritten = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value, valid_from,
                            valid_from_precision, valid_to, valid_to_precision, confidence,
                            supersedes, attested_from, end_derived)
         SELECT $1, kb_id, subject_id, predicate_id, object_value, valid_from, valid_from_precision,
                '2020-06-01', 'day', confidence, id, attested_from, TRUE FROM facts WHERE id = $2",
    )
    .bind(rewritten)
    .bind(a)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, doc_version)
         SELECT $1, chunk_id, quote, document_id, doc_version FROM fact_evidence WHERE fact_id = $2",
    )
    .bind(rewritten)
    .bind(a)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
        .bind(a)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    deleting.await??;
    let alive: bool = sqlx::query_scalar("SELECT invalidated_at IS NULL FROM facts WHERE id = $1")
        .bind(rewritten)
        .fetch_one(&pool)
        .await?;
    cleanup(&pool, &f).await?;
    assert!(
        !alive,
        "the only source was deleted, so the row rewritten from it should also be voided"
    );
    Ok(())
}

/// Parallel storage: evidence is not written onto an old row that has just been rewritten; every existing row has evidence
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn evidence_follows_a_row_rewritten_while_it_was_written() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    for _ in 0..30 {
        let f = seed(&pool).await?;
        let dated = |value: &'static str, day: &'static str, with_start: bool| Seen {
            from: with_start.then_some(day),
            doc: Some(day),
            ..seen(&f, value, day)
        };
        let (p, q, r, s, u) = tokio::join!(
            observe(&pool, &f, dated("v4", "2020-05-10", true)),
            observe(&pool, &f, dated("v2", "2020-03-17", true)),
            observe(&pool, &f, dated("v5", "2020-06-08", false)),
            observe(&pool, &f, dated("v1", "2020-02-18", true)),
            observe(&pool, &f, dated("v3", "2020-04-14", false)),
        );
        for result in [p, q, r, s, u] {
            result?;
        }
        let bare: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM facts f WHERE f.kb_id = $1 AND f.invalidated_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM fact_evidence fe WHERE fe.fact_id = f.id)",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        let n = overlaps(&pool, &f, f.lease).await?;
        cleanup(&pool, &f).await?;
        assert_eq!((bare, n), (0, 0));
    }
    Ok(())
}

/// A pair that a person ruled "keep both" on was later rewritten in both rows, and is not asked a second time
#[tokio::test]
async fn a_pair_a_person_kept_is_not_asked_again_after_a_rewrite() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    observe(&pool, &f, seen(&f, "A", "2020-01-01")).await?;
    observe(
        &pool,
        &f,
        Seen {
            described: true,
            doc: Some("2020-03-01"),
            ..seen(&f, "B", "2020-03-01")
        },
    )
    .await?;
    let asked: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM fact_conflicts WHERE kb_id = $1 AND status = 'open'")
            .bind(f.kb)
            .fetch_all(&pool)
            .await?;
    for conflict in &asked {
        utopia_store::temporal::resolve_conflict(&pool, f.kb, *conflict, "keep", None, "day")
            .await?;
    }
    observe(&pool, &f, seen(&f, "C", "2020-06-01")).await?;
    let again = open_conflicts(&pool, f.kb).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(asked.len(), 1);
    assert_eq!(again, Vec::<String>::new());
    Ok(())
}

/// Two values starting on the same day recorded a conflict pair; a later value rewrote both rows, and the conflict pair is still open
#[tokio::test]
async fn an_unresolved_conflict_stays_open_when_its_rows_are_rewritten() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    observe(&pool, &f, seen(&f, "P", "2020-01-01")).await?;
    observe(&pool, &f, seen(&f, "Q", "2020-01-01")).await?;
    let before = open_conflicts(&pool, f.kb).await?;
    observe(&pool, &f, seen(&f, "R", "2020-06-01")).await?;
    let after = open_conflicts(&pool, f.kb).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(before, vec!["simultaneous".to_string()]);
    assert_eq!(after, before);
    Ok(())
}

/// The engine closed A at B's start; a filing states A ran until that day. Later B was rejected, and A still runs until that day
#[tokio::test]
async fn an_end_the_text_states_on_a_row_the_engine_closed_is_kept() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    observe(&pool, &f, seen(&f, "A", "2020-01-01")).await?;
    let b = observe(&pool, &f, seen(&f, "B", "2020-03-01")).await?;
    observe(
        &pool,
        &f,
        Seen {
            to: Some("2020-03-01"),
            doc: Some("2020-04-01"),
            ..seen(&f, "A", "2020-01-01")
        },
    )
    .await?;
    utopia_store::graph::reject_fact(&pool, f.kb, b).await?;
    let rows = timeline(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2.as_deref(), Some("2020-03-01"));
    Ok(())
}

/// Delete the earliest document for a value with no start: it no longer sorts into the timeline on that day, and the predecessor's anchor moves later with it
#[tokio::test]
async fn a_deleted_document_no_longer_dates_a_row() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    observe(&pool, &f, seen(&f, "P", "2019-01-01")).await?;
    let s = observe(
        &pool,
        &f,
        Seen {
            from: None,
            doc: Some("2020-06-01"),
            ..seen(&f, "S", "")
        },
    )
    .await?;
    let early: Uuid =
        sqlx::query_scalar("SELECT document_id FROM fact_evidence WHERE fact_id = $1")
            .bind(s)
            .fetch_one(&pool)
            .await?;
    observe(
        &pool,
        &f,
        Seen {
            from: None,
            doc: Some("2021-06-01"),
            ..seen(&f, "S", "")
        },
    )
    .await?;
    utopia_store::documents::delete(&pool, f.kb, early, None).await?;
    let rows = timeline(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(rows[0].0, "P");
    assert_eq!(rows[0].4.as_deref(), Some("2021-06-01"), "{rows:?}");
    Ok(())
}

/// A value whose time cannot be stated (no start, and no evidence carrying its own date) and a dated value: whichever arrives first, they form the same pair
/// handed to a person for judgment, both sides still open
#[tokio::test]
async fn a_value_with_no_time_meets_a_dated_one_the_same_way_in_either_order() -> anyhow::Result<()>
{
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let other = entity(&pool, f.kb, f.etype, "Other Lease", Uuid::now_v7()).await?;
    let keyless = |subject| Seen {
        subject,
        from: None,
        ..seen(&f, "K", "")
    };
    observe(&pool, &f, keyless(f.lease)).await?;
    observe(&pool, &f, seen(&f, "N", "2020-01-01")).await?;
    observe(
        &pool,
        &f,
        Seen {
            subject: other,
            ..seen(&f, "N", "2020-01-01")
        },
    )
    .await?;
    observe(&pool, &f, keyless(other)).await?;
    let shape = |rows: Vec<Seg>| {
        rows.into_iter()
            .map(|r| (r.0, r.2, r.3))
            .collect::<std::collections::BTreeSet<_>>()
    };
    let k_first = shape(timeline(&pool, &f, f.lease).await?);
    let n_first = shape(timeline(&pool, &f, other).await?);
    let asked = open_conflicts(&pool, f.kb).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(k_first, n_first);
    assert_eq!(asked, vec!["no_time".to_string(), "no_time".to_string()]);
    Ok(())
}

/// Rows the engine closed before the migration are backfilled by 0057 as engine-drawn: an existing database gets two more late supplementary agreements, and
/// the timeline still lays out fine. Rows closed by a person with the end stated in the source are not backfilled
#[tokio::test]
async fn the_migration_marks_the_ends_the_old_engine_drew() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let backfill =
        include_str!("../../../migrations/0057_an_end_the_engine_drew_moves_with_what_follows.sql");
    let backfill = &backfill[backfill
        .find("UPDATE facts c")
        .expect("0057 backfill statement")..];

    let f = seed(&pool).await?;
    observe(&pool, &f, seen(&f, "v1", "2020-02-18")).await?;
    observe(&pool, &f, seen(&f, "v5", "2020-06-08")).await?;
    // A span a person closed by hand has the same shape as one the engine closed
    let other = entity(&pool, f.kb, f.etype, "Other Lease", Uuid::now_v7()).await?;
    let by_hand = observe(
        &pool,
        &f,
        Seen {
            subject: other,
            ..seen(&f, "h1", "2020-01-01")
        },
    )
    .await?;
    observe(
        &pool,
        &f,
        Seen {
            subject: other,
            described: true,
            doc: Some("2020-03-01"),
            ..seen(&f, "h2", "2020-03-01")
        },
    )
    .await?;
    utopia_store::temporal::close_superseded(&pool, by_hand, t("2020-03-01"), "day").await?;
    sqlx::query(
        "INSERT INTO audit_events (id, kb_id, action, target_kind, target_id)
         VALUES ($1, $2, 'fact.close', 'fact', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(f.kb)
    .bind(by_hand)
    .execute(&pool)
    .await?;
    // A database from before the migration: this column is all false
    sqlx::query("UPDATE facts SET end_derived = FALSE WHERE kb_id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    sqlx::raw_sql(backfill).execute(&pool).await?;
    let marked: Vec<String> = sqlx::query_scalar(
        "SELECT object_value #>> '{value}' FROM facts
          WHERE kb_id = $1 AND invalidated_at IS NULL AND end_derived ORDER BY 1",
    )
    .bind(f.kb)
    .fetch_all(&pool)
    .await?;
    observe(&pool, &f, seen(&f, "v2", "2020-03-17")).await?;
    observe(&pool, &f, seen(&f, "v3", "2020-04-14")).await?;
    let n = overlaps(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(
        marked,
        vec!["v1".to_string()],
        "only the span the engine closed"
    );
    assert_eq!(n, 0);
    Ok(())
}

/// Withdrawing the first link of a chained merge (#679 fourth-round review): S is merged into T, T into C; withdraw S→T.
/// S's facts now hang off C and must go back with S, leaving none on C's timeline
#[tokio::test]
async fn undoing_the_first_merge_of_a_chain_brings_its_facts_home() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let source = entity(&pool, f.kb, f.etype, "Lease S", Uuid::now_v7()).await?;
    let chain_end = entity(&pool, f.kb, f.etype, "Lease C", Uuid::now_v7()).await?;
    observe(
        &pool,
        &f,
        Seen {
            subject: source,
            ..seen(&f, "S1", "2020-01-01")
        },
    )
    .await?;
    let first =
        utopia_store::resolution::merge_entities(&pool, f.kb, source, f.lease, None, "test")
            .await?;
    utopia_store::resolution::merge_entities(&pool, f.kb, f.lease, chain_end, None, "test").await?;
    utopia_store::resolution::revert_merge(&pool, f.kb, first).await?;
    let home = timeline(&pool, &f, source).await?;
    let tail = timeline(&pool, &f, chain_end).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(home.len(), 1, "S1 returns to S: {home:?}");
    assert_eq!(home[0].0, "S1");
    assert!(
        tail.iter().all(|s| s.0 != "S1"),
        "no S1 left on C: {tail:?}"
    );
    Ok(())
}

/// When withdrawing a merge sends rows rewritten after the merge back to the source entity, the conflicts open on them go along (#679 fourth-round review)
#[tokio::test]
async fn a_revert_keeps_the_conflict_its_rows_carried() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let source = entity(&pool, f.kb, f.etype, "Lease Agreement", Uuid::now_v7()).await?;
    for value in ["D", "E"] {
        observe(
            &pool,
            &f,
            Seen {
                subject: source,
                ..seen(&f, value, "2020-01-01")
            },
        )
        .await?;
    }
    let merge =
        utopia_store::resolution::merge_entities(&pool, f.kb, source, f.lease, None, "test")
            .await?;
    observe(&pool, &f, seen(&f, "N", "2020-06-01")).await?;
    utopia_store::resolution::revert_merge(&pool, f.kb, merge).await?;
    let conflicts = open_conflicts(&pool, f.kb).await?;
    let back = timeline(&pool, &f, source).await?;
    let overlap = overlaps(&pool, &f, source).await?;
    cleanup(&pool, &f).await?;
    assert_eq!(
        back.len(),
        2,
        "D and E return to the source entity: {back:?}"
    );
    assert!(
        overlap == 0 || conflicts.contains(&"simultaneous".to_string()),
        "两行叠在一起就得有一条开着的冲突：{conflicts:?}"
    );
    Ok(())
}

/// Deleting a document that recorded one attribute each for hundreds of entities: it now takes a predicate-level lock, which is deletable, and all of them are voided.
/// Locking row by row would take thousands of locks for thousands of rows, and at twenty thousand the lock table cannot hold them (#679 fourth-round review)
#[tokio::test]
async fn deleting_a_document_that_dates_many_timelines_still_works() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    let (doc, chunk) = document(&pool, f.kb, "2020-01-01", "a table").await?;
    // By default 300 rows take the predicate-lock path; it can reproduce the scale at which the lock table overflowed during the review
    let n: usize = std::env::var("REDRAW_BULK_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    for i in 0..n {
        let holder = entity(&pool, f.kb, f.etype, &format!("Lease {i}"), Uuid::now_v7()).await?;
        let (id, _) = utopia_store::graph::insert_value_fact(
            &pool,
            f.kb,
            holder,
            Some(f.deadline),
            &json!({ "value": format!("2021-01-{:02}", i % 28 + 1) }),
            Validity {
                from: Some(t("2020-01-01")),
                from_precision: Some("day"),
                ..Default::default()
            },
            0.9,
        )
        .await?;
        utopia_store::graph::add_evidence(&pool, id, chunk, Some("row"), None).await?;
    }
    let report = utopia_store::documents::delete(&pool, f.kb, doc, None).await?;
    let restored = utopia_store::documents::restore(&pool, f.kb, doc).await;
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM facts WHERE kb_id = $1 AND predicate_id = $2 AND invalidated_at IS NULL",
    )
    .bind(f.kb)
    .bind(f.deadline)
    .fetch_one(&pool)
    .await?;
    cleanup(&pool, &f).await?;
    assert_eq!(report.invalidated_facts, n);
    assert!(restored.is_ok(), "{:?}", restored.err());
    assert_eq!(
        live, n as i64,
        "after undoing the deletion, everything comes back"
    );
    Ok(())
}

/// Delete the earliest evidence document for a row with no start: the engine's anchor moves to the second document, and the start read out moves with it,
/// so the two spans do not overlap; undoing the deletion brings both back (#679 fourth-round review)
#[tokio::test]
async fn deleting_the_first_document_moves_the_read_start_with_the_anchor() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let f = seed(&pool).await?;
    observe(&pool, &f, seen(&f, "P", "2019-01-01")).await?;
    for day in ["2020-06-01", "2021-06-01"] {
        observe(
            &pool,
            &f,
            Seen {
                from: None,
                doc: Some(day),
                ..seen(&f, "S", day)
            },
        )
        .await?;
    }
    let first: Uuid = sqlx::query_scalar(
        "SELECT d.id FROM fact_evidence fe JOIN chunks c ON c.id = fe.chunk_id
           JOIN documents d ON d.id = c.document_id
          WHERE d.kb_id = $1 ORDER BY d.doc_time LIMIT 1",
    )
    .bind(f.kb)
    .fetch_one(&pool)
    .await?;
    let before = overlaps(&pool, &f, f.lease).await?;
    utopia_store::documents::delete(&pool, f.kb, first, None).await?;
    let after_delete = overlaps(&pool, &f, f.lease).await?;
    utopia_store::documents::restore(&pool, f.kb, first).await?;
    let after_restore = overlaps(&pool, &f, f.lease).await?;
    cleanup(&pool, &f).await?;
    assert_eq!((before, after_delete, after_restore), (0, 0, 0));
    Ok(())
}
