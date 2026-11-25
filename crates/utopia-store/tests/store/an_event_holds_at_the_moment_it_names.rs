//! An event holds at the moment it names; a constant holds at every moment (0031 / #486), run against a real store.
//!
//! Why this must touch a database: the write-side normalization (`Validity::under`) and the read-side predicate (`world_axis`)
//! each live in one place, and the two must say the same thing; and the read side lives entirely inside SQL strings — a CASE written backwards,
//! or a misspelled interval literal, would be invisible to `cargo check`.
//!
//! What is pinned down:
//! - An event written to the store has the same value at both ends; it holds in the bucket it names (that day, that month), and not outside it
//! - Giving an event a span, only an end, or saying it "ended, date unknown" — what is stored is still a moment or has no date
//! - An event with no date holds at no moment, but is still listed on the entity; once a date arrives it is refined, and another sentence
//!   with no date merges into it
//! - A constant strips the dates from the source text, and the evidence date does not gate it either — it holds in both 1800 and 2100
//! - Event rows written before 0031 (with an empty end) are read by their start's bucket, with no backfill needed
//! - A person changes an event's interval, and what is stored is still a moment
//! - A derivation takes an event's bucket through it (both ends carry precision); through a constant, both ends are open
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use std::collections::HashSet;

use sqlx::PgPool;
use utopia_store::graph::Validity;
use uuid::Uuid;

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

/// The two ends and the precision of one fact row
type Span = (
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
);

struct Fixture {
    org: Uuid,
    kb: Uuid,
    nova: Uuid,
    orion: Uuid,
    vega: Uuid,
    paris: Uuid,
    france: Uuid,
    /// State: leads an organization
    leads: Uuid,
    /// Event: acquisition
    acquired: Uuid,
    /// Event, symmetric: merger
    merged_with: Uuid,
    /// Constant: capital city
    capital_of: Uuid,
    /// Constant, symmetric: borders
    borders: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (organization, city, country) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (nova, orion, vega) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (paris, france) = (Uuid::now_v7(), Uuid::now_v7());
    let (leads, acquired, merged_with) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (capital_of, borders) = (Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'event-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'event-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'event-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, key, label) in [
        (organization, "organization", "Organization"),
        (city, "city", "City"),
        (country, "country", "Country"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    for (id, key, temporal, symmetric) in [
        (leads, "leads", "state", false),
        (acquired, "acquired", "event", false),
        (merged_with, "merged_with", "event", true),
        (capital_of, "capital_of", "eternal", false),
        (borders, "borders", "eternal", true),
    ] {
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, temporal, is_symmetric)
             VALUES ($1, $2, $3, $3, $4, $5)",
        )
        .bind(id)
        .bind(kb)
        .bind(key)
        .bind(temporal)
        .bind(symmetric)
        .execute(pool)
        .await?;
    }
    for (id, ty, name) in [
        (nova, organization, "Nova Systems"),
        (orion, organization, "Orion Labs"),
        (vega, organization, "Vega Analytics"),
        (paris, city, "Paris"),
        (france, country, "France"),
    ] {
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
        nova,
        orion,
        vega,
        paris,
        france,
        leads,
        acquired,
        merged_with,
        capital_of,
        borders,
    })
}

async fn teardown(pool: &PgPool, f: &Fixture) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(pool)
        .await?;
    Ok(())
}

async fn fact(
    pool: &PgPool,
    f: &Fixture,
    subject: Uuid,
    predicate: Uuid,
    object: Uuid,
    validity: Validity<'_>,
) -> anyhow::Result<(Uuid, bool)> {
    Ok(utopia_store::graph::insert_fact(
        pool,
        f.kb,
        subject,
        Some(predicate),
        object,
        validity,
        0.9,
    )
    .await?)
}

async fn span(pool: &PgPool, id: Uuid) -> anyhow::Result<Span> {
    Ok(sqlx::query_as(
        "SELECT valid_from, valid_from_precision, valid_to, valid_to_precision FROM facts WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await?)
}

/// The facts on an entity's panel at moment T (by id). `None` = every moment
async fn facts_at(
    pool: &PgPool,
    f: &Fixture,
    entity: Uuid,
    at: Option<&str>,
) -> anyhow::Result<HashSet<Uuid>> {
    let (_, facts) =
        utopia_store::graph::entity_detail(pool, f.kb, entity, at.map(t), None).await?;
    Ok(facts.into_iter().map(|x| x.id).collect())
}

async fn holds(
    pool: &PgPool,
    f: &Fixture,
    entity: Uuid,
    id: Uuid,
    at: &str,
) -> anyhow::Result<bool> {
    Ok(facts_at(pool, f, entity, Some(at)).await?.contains(&id))
}

#[tokio::test]
async fn an_event_is_one_moment_and_holds_through_the_bucket_it_names() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // A document dated 2024-04-01 says: Nova acquired Orion on 2024-03-15
        let (day, _) = fact(
            &pool,
            &f,
            f.nova,
            f.acquired,
            f.orion,
            Validity::starting(Some(t("2024-03-15T00:00:00Z")), Some("day"))
                .attested(Some(t("2024-04-01T00:00:00Z"))),
        )
        .await?;
        assert_eq!(
            span(&pool, day).await?,
            (
                Some(t("2024-03-15T00:00:00Z")),
                Some("day".into()),
                Some(t("2024-03-15T00:00:00Z")),
                Some("day".into()),
            ),
            "事件写进库是两端同一个值、同一个精度"
        );
        // Holds at every moment of that day; the day before and the day after do not; the evidence date (April) does not either
        assert!(!holds(&pool, &f, f.nova, day, "2024-03-14T23:59:59Z").await?);
        assert!(holds(&pool, &f, f.nova, day, "2024-03-15T00:00:00Z").await?);
        assert!(holds(&pool, &f, f.nova, day, "2024-03-15T23:59:59Z").await?);
        assert!(!holds(&pool, &f, f.nova, day, "2024-03-16T00:00:00Z").await?);
        assert!(
            !holds(&pool, &f, f.nova, day, "2024-04-01T00:00:00Z").await?,
            "证据日期不是事件的日期，状态那套「从证据起」不适用"
        );
        assert!(
            facts_at(&pool, &f, f.nova, None).await?.contains(&day),
            "不限时刻时列出"
        );

        // The interval read back follows the bucket: from that day, up to the next day
        let (_, all) = utopia_store::graph::entity_detail(&pool, f.kb, f.nova, None, None).await?;
        let read = all.iter().find(|x| x.id == day).expect("在面板上");
        assert_eq!(read.holds_from, Some(t("2024-03-15T00:00:00Z")));
        assert_eq!(read.holds_to, Some(t("2024-03-16T00:00:00Z")));
        assert_eq!(read.temporal.as_deref(), Some("event"));

        // A month-precision bucket is the whole month
        let (month, _) = fact(
            &pool,
            &f,
            f.nova,
            f.acquired,
            f.vega,
            Validity::starting(Some(t("2024-06-01T00:00:00Z")), Some("month")),
        )
        .await?;
        assert!(holds(&pool, &f, f.nova, month, "2024-06-30T12:00:00Z").await?);
        assert!(!holds(&pool, &f, f.nova, month, "2024-07-01T00:00:00Z").await?);

        // The state is unchanged: leads from March, open — still holds in April
        let (lead, _) = fact(
            &pool,
            &f,
            f.nova,
            f.leads,
            f.orion,
            Validity::starting(Some(t("2024-03-15T00:00:00Z")), Some("day")),
        )
        .await?;
        assert!(holds(&pool, &f, f.nova, lead, "2024-04-01T00:00:00Z").await?);
        Ok::<_, anyhow::Error>(())
    }
    .await;

    teardown(&pool, &f).await?;
    run
}

#[tokio::test]
async fn a_span_an_ending_or_an_unknown_end_given_to_an_event_collapses() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // The model gave a span: take the start
        let (ranged, _) = fact(
            &pool,
            &f,
            f.nova,
            f.acquired,
            f.orion,
            Validity {
                from: Some(t("2024-03-15T00:00:00Z")),
                from_precision: Some("day"),
                to: Some(t("2025-01-01T00:00:00Z")),
                to_precision: Some("day"),
                attested_at: None,
                from_grade: None,
                ..Default::default()
            },
        )
        .await?;
        let s = span(&pool, ranged).await?;
        assert_eq!(
            (s.0, s.2),
            (
                Some(t("2024-03-15T00:00:00Z")),
                Some(t("2024-03-15T00:00:00Z"))
            )
        );

        // Only an end was given: that is when it happened
        let (ended, _) = fact(
            &pool,
            &f,
            f.nova,
            f.acquired,
            f.vega,
            Validity {
                from: None,
                from_precision: None,
                to: Some(t("2024-05-01T00:00:00Z")),
                to_precision: Some("month"),
                attested_at: None,
                from_grade: None,
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(
            span(&pool, ended).await?,
            (
                Some(t("2024-05-01T00:00:00Z")),
                Some("month".into()),
                Some(t("2024-05-01T00:00:00Z")),
                Some("month".into()),
            )
        );

        // "Ended, date unknown" means nothing for a moment: what lands is an event with no date, not a row waiting to be closed
        let (unknown, _) = fact(
            &pool,
            &f,
            f.orion,
            f.acquired,
            f.vega,
            Validity::default()
                .ended_when_unknown()
                .attested(Some(t("2024-04-01T00:00:00Z"))),
        )
        .await?;
        assert_eq!(span(&pool, unknown).await?, (None, None, None, None));
        let attested_to: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT attested_to FROM facts WHERE id = $1")
                .bind(unknown)
                .fetch_one(&pool)
                .await?;
        assert_eq!(attested_to, None, "没有「结束」这回事，也就没有结束的锚点");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    teardown(&pool, &f).await?;
    run
}

#[tokio::test]
async fn an_undated_event_holds_at_no_moment_and_takes_a_date_when_one_arrives(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // A document dated 2024-04-01 says Nova acquired Orion, without saying which day
        let (bare, created) = fact(
            &pool,
            &f,
            f.nova,
            f.acquired,
            f.orion,
            Validity::default().attested(Some(t("2024-04-01T00:00:00Z"))),
        )
        .await?;
        assert!(created);
        assert!(
            facts_at(&pool, &f, f.nova, None).await?.contains(&bare),
            "实体上列着"
        );
        for at in [
            "2024-04-01T00:00:00Z",
            "2024-04-02T00:00:00Z",
            "2030-01-01T00:00:00Z",
        ] {
            assert!(
                !holds(&pool, &f, f.nova, bare, at).await?,
                "不知何时发生的事件，{at} 不算成立（状态会从证据起算，事件不）"
            );
        }

        // A later document gives the date: refined — the bare row is voided, and the new row's two ends are that moment
        let (dated, created) = fact(
            &pool,
            &f,
            f.nova,
            f.acquired,
            f.orion,
            Validity::starting(Some(t("2024-03-15T00:00:00Z")), Some("day"))
                .attested(Some(t("2024-05-01T00:00:00Z"))),
        )
        .await?;
        assert!(created);
        assert_ne!(dated, bare);
        let (supersedes, old_dead): (Option<Uuid>, bool) = sqlx::query_as(
            "SELECT supersedes, (SELECT invalidated_at IS NOT NULL FROM facts WHERE id = $2)
               FROM facts WHERE id = $1",
        )
        .bind(dated)
        .bind(bare)
        .fetch_one(&pool)
        .await?;
        assert_eq!(supersedes, Some(bare));
        assert!(old_dead);
        assert!(holds(&pool, &f, f.nova, dated, "2024-03-15T12:00:00Z").await?);

        // Hearing "acquired" again with no date: merges into the existing moment, not a second acquisition
        let (again, created) = fact(
            &pool,
            &f,
            f.nova,
            f.acquired,
            f.orion,
            Validity::default().attested(Some(t("2024-06-01T00:00:00Z"))),
        )
        .await?;
        assert!(!created);
        assert_eq!(again, dated);
        Ok::<_, anyhow::Error>(())
    }
    .await;

    teardown(&pool, &f).await?;
    run
}

#[tokio::test]
async fn an_eternal_relation_drops_its_dates_and_holds_at_every_moment() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // Source: "Paris has been the capital of France since 1990" — the date refers to something else; the document is from 2024
        let (cap, _) = fact(
            &pool,
            &f,
            f.paris,
            f.capital_of,
            f.france,
            Validity::starting(Some(t("1990-01-01T00:00:00Z")), Some("year"))
                .attested(Some(t("2024-04-01T00:00:00Z"))),
        )
        .await?;
        assert_eq!(
            span(&pool, cap).await?,
            (None, None, None, None),
            "恒常不存日期"
        );
        for at in [
            "1800-01-01T00:00:00Z",
            "2023-12-31T00:00:00Z",
            "2100-01-01T00:00:00Z",
        ] {
            assert!(
                holds(&pool, &f, f.paris, cap, at).await?,
                "{at} 成立——证据日期不闸恒常"
            );
        }
        let (_, all) = utopia_store::graph::entity_detail(&pool, f.kb, f.paris, None, None).await?;
        let read = all.iter().find(|x| x.id == cap).expect("在面板上");
        assert_eq!((read.holds_from, read.holds_to), (None, None), "两端开放");

        // Said again: merged into the same row
        let (again, created) = fact(
            &pool,
            &f,
            f.paris,
            f.capital_of,
            f.france,
            Validity::starting(Some(t("2000-01-01T00:00:00Z")), Some("year")),
        )
        .await?;
        assert!(!created);
        assert_eq!(again, cap);
        Ok::<_, anyhow::Error>(())
    }
    .await;

    teardown(&pool, &f).await?;
    run
}

#[tokio::test]
async fn a_row_written_before_the_rule_reads_as_its_start_bucket() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // Shape from before 0031: an event predicate with a start and an empty end — it was read back then as "so from that day onward"
        let legacy = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                                valid_from, valid_from_precision, confidence, attested_from)
             VALUES ($1, $2, $3, $4, $5, '2024-03-15T00:00:00Z', 'day', 0.9, '2024-04-01T00:00:00Z')",
        )
        .bind(legacy)
        .bind(f.kb)
        .bind(f.nova)
        .bind(f.acquired)
        .bind(f.orion)
        .execute(&pool)
        .await?;
        assert!(holds(&pool, &f, f.nova, legacy, "2024-03-15T12:00:00Z").await?);
        assert!(!holds(&pool, &f, f.nova, legacy, "2024-03-16T00:00:00Z").await?);
        assert!(!holds(&pool, &f, f.nova, legacy, "2025-01-01T00:00:00Z").await?, "不再是开放的");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    teardown(&pool, &f).await?;
    run
}

#[tokio::test]
async fn a_person_correcting_an_event_leaves_it_one_moment() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let (original, _) = fact(
            &pool,
            &f,
            f.nova,
            f.acquired,
            f.orion,
            Validity::starting(Some(t("2024-03-15T00:00:00Z")), Some("day")),
        )
        .await?;
        // A person filled in a span 3-20 ~ 6-01 on the panel: what lands is the moment 3-20
        let corrected = utopia_store::temporal::correct_interval(
            &pool,
            original,
            Validity {
                from: Some(t("2024-03-20T00:00:00Z")),
                from_precision: Some("day"),
                to: Some(t("2024-06-01T00:00:00Z")),
                to_precision: Some("day"),
                attested_at: None,
                from_grade: None,
                ..Default::default()
            },
        )
        .await?
        .expect("改得动");
        assert_eq!(
            span(&pool, corrected).await?,
            (
                Some(t("2024-03-20T00:00:00Z")),
                Some("day".into()),
                Some(t("2024-03-20T00:00:00Z")),
                Some("day".into()),
            )
        );
        assert!(holds(&pool, &f, f.nova, corrected, "2024-03-20T08:00:00Z").await?);
        assert!(!holds(&pool, &f, f.nova, corrected, "2024-04-15T00:00:00Z").await?);
        Ok::<_, anyhow::Error>(())
    }
    .await;

    teardown(&pool, &f).await?;
    run
}

#[tokio::test]
async fn a_derivation_reads_an_event_as_its_bucket_and_an_eternal_as_open() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // merged_with, symmetric, event: Nova and Orion merged in 2024-03
        fact(
            &pool,
            &f,
            f.nova,
            f.merged_with,
            f.orion,
            Validity::starting(Some(t("2024-03-01T00:00:00Z")), Some("month"))
                .attested(Some(t("2024-04-01T00:00:00Z"))),
        )
        .await?;
        // borders, symmetric, constant: France and … Paris border each other (an absurd fact used by the test; the document is from 2024)
        fact(
            &pool,
            &f,
            f.france,
            f.borders,
            f.paris,
            Validity::starting(Some(t("2020-01-01T00:00:00Z")), Some("year"))
                .attested(Some(t("2024-04-01T00:00:00Z"))),
        )
        .await?;
        // An event with no date: Orion and Vega merged, at an unknown time — the reverse is not inferred
        fact(
            &pool,
            &f,
            f.orion,
            f.merged_with,
            f.vega,
            Validity::default().attested(Some(t("2024-04-01T00:00:00Z"))),
        )
        .await?;

        utopia_store::reasoning::materialize(&pool, f.kb).await?;

        let derived = |s: Uuid, p: Uuid, o: Uuid| {
            let pool = pool.clone();
            async move {
                let row: Option<Span> = sqlx::query_as(
                    "SELECT valid_from, valid_from_precision, valid_to, valid_to_precision
                       FROM derived_facts
                      WHERE kb_id = $1 AND subject_id = $2 AND predicate_id = $3 AND object_id = $4
                        AND invalidated_at IS NULL",
                )
                .bind(f.kb)
                .bind(s)
                .bind(p)
                .bind(o)
                .fetch_optional(&pool)
                .await?;
                Ok::<_, anyhow::Error>(row)
            }
        };

        // The reverse merger: that month, both ends carry month precision
        let back = derived(f.orion, f.merged_with, f.nova)
            .await?
            .expect("对称推得出");
        assert_eq!(
            back,
            (
                Some(t("2024-03-01T00:00:00Z")),
                Some("month".into()),
                Some(t("2024-04-01T00:00:00Z")),
                Some("month".into()),
            ),
            "派生取事件的桶：从 3 月起，到 4 月为止"
        );
        // The reverse border: both ends open, the evidence date does not come in
        let back = derived(f.paris, f.borders, f.france)
            .await?
            .expect("对称推得出");
        assert_eq!(back, (None, None, None, None));
        // A merger at an unknown time: an empty interval, nothing inferred
        assert!(
            derived(f.vega, f.merged_with, f.orion).await?.is_none(),
            "没日期的事件任何时刻都不成立，链经过它推不出东西"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    teardown(&pool, &f).await?;
    run
}
