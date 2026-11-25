//! A new value for a single-valued attribute arrives out of order, and the timeline still forms one segment after another.
//!
//! The option-exercise deadline of the Blackbaud headquarters lease was amended four times in the first half of 2020 by supplemental agreements. A dozen or so supplemental agreements
//! are extracted in parallel, and which one lands first depends entirely on how many chunks each has: the fifth (in effect from 18 February) arrives first, the ninth (in effect from 8 June)
//! arrives second, and the seventh and sixth arrive last. The temporal engine used to take the new value and compare it only with the one still "holding", so
//! the two that arrived late were both closed at 8 June, while the fifth's span [18 February, 8 June) was never cut —
//! asked for the deadline on 1 May, it returned three.
//!
//! This pins two things: out-of-order arrival finally forms the correct timeline; a span the source itself states as ending is not cut.
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use serde_json::json;
use sqlx::PgPool;
use utopia_store::graph::Validity;
use utopia_store::temporal::Uniqueness;
use uuid::Uuid;

struct Fixture {
    kb: Uuid,
    lease: Uuid,
    deadline: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (etype, deadline, lease) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'late-value-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'late-value-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'late-value-test')",
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
    sqlx::query(
        "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, 'HQ Lease')",
    )
    .bind(lease)
    .bind(kb)
    .bind(etype)
    .execute(pool)
    .await?;
    Ok(Fixture {
        kb,
        lease,
        deadline,
    })
}

fn t(day: &str) -> chrono::DateTime<chrono::Utc> {
    format!("{day}T00:00:00Z").parse().unwrap()
}

/// Stores one value, then reconciles it the way extraction does
async fn arrive(
    pool: &PgPool,
    f: &Fixture,
    value: &str,
    from: &str,
    to: Option<&str>,
) -> anyhow::Result<Uuid> {
    let mut validity = Validity::starting(Some(t(from)), Some("day"));
    if let Some(to) = to {
        validity.to = Some(t(to));
        validity.to_precision = Some("day");
    }
    let object = json!({ "value": value });
    let (id, _) = utopia_store::graph::insert_value_fact(
        pool,
        f.kb,
        f.lease,
        Some(f.deadline),
        &object,
        validity,
        0.9,
    )
    .await?;
    utopia_store::temporal::reconcile_new_fact(
        pool,
        f.kb,
        id,
        f.lease,
        f.deadline,
        None,
        Some(&object),
        Uniqueness::SubjectSide,
        validity,
        0.9,
    )
    .await?;
    Ok(id)
}

/// The existing segments: (value, start, end), sorted by start
async fn timeline(
    pool: &PgPool,
    f: &Fixture,
) -> anyhow::Result<Vec<(String, String, Option<String>)>> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT object_value #>> '{value}', to_char(valid_from, 'YYYY-MM-DD'),
                to_char(valid_to, 'YYYY-MM-DD')
         FROM facts
         WHERE kb_id = $1 AND subject_id = $2 AND predicate_id = $3 AND invalidated_at IS NULL
         ORDER BY valid_from",
    )
    .bind(f.kb)
    .bind(f.lease)
    .bind(f.deadline)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

fn seg(value: &str, from: &str, to: Option<&str>) -> (String, String, Option<String>) {
    (value.into(), from.into(), to.map(Into::into))
}

#[tokio::test]
async fn amendments_that_arrive_out_of_order_still_hand_over_one_to_the_next() -> anyhow::Result<()>
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // The fifth, ninth, seventh and sixth, in their real order of storage
        arrive(&pool, &f, "2020-03-17", "2020-02-18", None).await?;
        arrive(&pool, &f, "2020-06-23", "2020-06-08", None).await?;
        arrive(&pool, &f, "2020-05-26", "2020-04-14", None).await?;
        arrive(&pool, &f, "2020-04-14", "2020-03-17", None).await?;

        assert_eq!(
            timeline(&pool, &f).await?,
            vec![
                seg("2020-03-17", "2020-02-18", Some("2020-03-17")),
                seg("2020-04-14", "2020-03-17", Some("2020-04-14")),
                seg("2020-05-26", "2020-04-14", Some("2020-06-08")),
                seg("2020-06-23", "2020-06-08", None),
            ],
            "每一段都止于下一段的起点，任何一天只有一个截止日"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    run
}

/// A span the source states as "until 31 December" is not a boundary left by a succession: a value arriving mid-year does not cut it.
/// That kind of overlap is two sources disagreeing, which the engine should not pick one of on a person's behalf
#[tokio::test]
async fn an_end_the_text_states_is_not_cut_by_a_later_value() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        arrive(&pool, &f, "2020-09-30", "2020-01-01", Some("2020-12-31")).await?;
        arrive(&pool, &f, "2020-10-31", "2020-06-01", None).await?;

        let rows = timeline(&pool, &f).await?;
        assert!(
            rows.contains(&seg("2020-09-30", "2020-01-01", Some("2020-12-31"))),
            "原文写的终点原样留着：{rows:?}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    run
}

/// A value read from a dated document: the start may be absent, but the evidence document's date is always present
async fn arrive_from_document(
    pool: &PgPool,
    f: &Fixture,
    value: &str,
    from: Option<&str>,
    document_date: &str,
) -> anyhow::Result<Uuid> {
    arrive_object_from_document(pool, f, json!({ "value": value }), from, document_date).await
}

async fn arrive_object_from_document(
    pool: &PgPool,
    f: &Fixture,
    object: serde_json::Value,
    from: Option<&str>,
    document_date: &str,
) -> anyhow::Result<Uuid> {
    let value = object["value"].as_str().unwrap_or_default().to_string();
    let value = value.as_str();
    let (doc, chunk) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, doc_time, doc_time_source) VALUES ($1, $2, $3, $3, $4, 'content')",
    )
    .bind(doc)
    .bind(f.kb)
    .bind(format!("amendment-{document_date}-{value}.html"))
    .bind(t(document_date))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, $4)",
    )
    .bind(chunk)
    .bind(f.kb)
    .bind(doc)
    .bind(value)
    .execute(pool)
    .await?;
    let validity = match from {
        Some(from) => Validity::starting(Some(t(from)), Some("day")),
        None => Validity::default(),
    }
    .attested(Some(t(document_date)));
    let (id, _) = utopia_store::graph::insert_value_fact(
        pool,
        f.kb,
        f.lease,
        Some(f.deadline),
        &object,
        validity,
        0.9,
    )
    .await?;
    utopia_store::graph::add_evidence(pool, id, chunk, Some(value), None).await?;
    utopia_store::temporal::reconcile_new_fact(
        pool,
        f.kb,
        id,
        f.lease,
        f.deadline,
        None,
        Some(&object),
        Uniqueness::SubjectSide,
        validity,
        0.9,
    )
    .await?;
    Ok(id)
}

/// The fifth supplemental agreement had no start extracted, so it went to human review first; when the eighth arrived, previously it and the sixth
/// (starting 17 March) were closed together at 26 May, with two spans overlapping for over two months. Its successor is the nearest known start after it
#[tokio::test]
async fn a_value_with_no_start_ends_where_the_next_known_value_begins() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        arrive_from_document(&pool, &f, "2020-04-14", Some("2020-03-17"), "2020-03-17").await?;
        arrive_from_document(&pool, &f, "2020-03-17", None, "2020-02-18").await?;
        arrive_from_document(&pool, &f, "2020-06-09", Some("2020-05-26"), "2020-05-26").await?;

        let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT object_value #>> '{value}', to_char(valid_from, 'YYYY-MM-DD'),
                    to_char(valid_to, 'YYYY-MM-DD')
             FROM facts
             WHERE kb_id = $1 AND subject_id = $2 AND predicate_id = $3 AND invalidated_at IS NULL
             ORDER BY valid_to NULLS LAST",
        )
        .bind(f.kb)
        .bind(f.lease)
        .bind(f.deadline)
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            rows,
            vec![
                ("2020-03-17".into(), None, Some("2020-03-17".into())),
                (
                    "2020-04-14".into(),
                    Some("2020-03-17".into()),
                    Some("2020-05-26".into())
                ),
                ("2020-06-09".into(), Some("2020-05-26".into()), None),
            ]
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    run
}

/// The eleventh supplemental agreement (13 August) says the landlord is BBHQ1, without saying from when; another says HPBB1 has been the landlord since 2016.
/// BBHQ1 holds from 13 August, so it sorts after HPBB1: HPBB1 is written as "ended, date unknown",
/// anchored on 13 August (#681 §1, the 0022 shape). The date is not written into the date column; read back, HPBB1 runs up to 13 August, and
/// BBHQ1 holds from 13 August onward, touching end to start with no overlap
#[tokio::test]
async fn a_dated_value_with_no_start_takes_over_from_its_date() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let later = arrive_from_document(&pool, &f, "BBHQ1", None, "2020-08-13").await?;
        arrive_from_document(&pool, &f, "HPBB1", Some("2016-05-16"), "2016-05-16").await?;

        let (still_open,): (bool,) = sqlx::query_as(
            "SELECT valid_to IS NULL AND valid_to_precision IS NULL AND invalidated_at IS NULL
             FROM facts WHERE id = $1",
        )
        .bind(later)
        .fetch_one(&pool)
        .await?;
        assert!(still_open, "BBHQ1 is not confined to 2016");
        let earlier: (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT to_char(valid_from, 'YYYY-MM-DD'), to_char(valid_to, 'YYYY-MM-DD'),
                        valid_to_precision, to_char(attested_to, 'YYYY-MM-DD')
                 FROM facts
                 WHERE kb_id = $1 AND object_value #>> '{value}' = 'HPBB1'
                   AND invalidated_at IS NULL",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            earlier,
            (
                Some("2016-05-16".into()),
                None,
                Some("unknown".into()),
                Some("2020-08-13".into())
            ),
            "HPBB1 结束了、不知哪天，锚在 BBHQ1 那份文件的日期上"
        );
        let (conflicts,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM fact_conflicts WHERE kb_id = $1")
                .bind(f.kb)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            conflicts, 0,
            "if the order can be worked out, no human review is needed"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    run
}

/// The tenth supplemental agreement (26 June) changes the deadline to "45 days after the trigger date": there is no calendar date, so extraction marks it relative
/// and stores it as stated in the source (#681 §4). It is still the lease's new deadline, so the one ending 23 June stops at it —
/// it has no start, so the previous one is written as "ended, date unknown", anchored on the 26 June document (§1)
#[tokio::test]
async fn a_deadline_stated_relative_to_an_event_still_ends_the_dated_one_before_it(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        arrive_from_document(&pool, &f, "2020-06-23", Some("2020-06-08"), "2020-06-08").await?;
        let relative = arrive_object_from_document(
            &pool,
            &f,
            json!({ "value": "45 days after the Trigger Date", "relative": true }),
            None,
            "2020-06-26",
        )
        .await?;

        let (open,): (bool,) = sqlx::query_as(
            "SELECT valid_to IS NULL AND valid_to_precision IS NULL AND invalidated_at IS NULL
             FROM facts WHERE id = $1",
        )
        .bind(relative)
        .fetch_one(&pool)
        .await?;
        assert!(open, "the relative deadline is today's deadline");
        let before: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT to_char(valid_to, 'YYYY-MM-DD'), valid_to_precision,
                    to_char(attested_to, 'YYYY-MM-DD')
             FROM facts
             WHERE kb_id = $1 AND object_value #>> '{value}' = '2020-06-23'
               AND invalidated_at IS NULL",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            before,
            (None, Some("unknown".into()), Some("2020-06-26".into())),
            "6 月 23 日那个结束了、不知哪天，锚在第十份补充协议的日期"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    run
}
