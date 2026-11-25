//! An unknown date is not an open date (0022 / #345 / #352), hit against a real database.
//!
//! Why it must hit a database: this defence lives entirely inside SQL strings (`world_axis`), the same kind of danger as the record axis in 0019:
//! `cargo check` cannot see a reversed CASE, and no one reports a missed read point.
//!
//! Asserts the four kinds of rows in the table, in both directions:
//! - A fact with no start is **absent before its document's date** and **present on that day** (reading it as "always has been" would
//!   show a 2024 pay-rise slip to someone looking at January 2023)
//! - A fact that ended on an unknown day is **present before its document's date** and **absent from that day on** (reading it as
//!   "still is" would leave a title the original text says has already been given up still shown in 2026)
//! - A fact with both ends given in the original keeps both ends as before: this change does not touch rows with dates
//! - A row unknown at both ends **holds at no moment**, but the entity's timeline still lists it
//!
//! Two more write-side conventions are pinned: when the same assertion is observed again in an earlier document, the anchor moves earlier;
//! a correction row inherits the anchor from the row it replaces, rather than being re-anchored at the moment of the correction.
//!
//! Without `UTOPIA_DATABASE_URL` it skips rather than fails. It sets up and tears down its own data and never touches an existing database.

use std::collections::HashSet;

use sqlx::PgPool;
use utopia_store::graph::Validity;
use uuid::Uuid;

/// The row after a bare row is closed: start, end precision, two anchors
type BareClosedRow = (
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// The row after closing: start, end, end precision, supersedes, anchor
type ClosedRow = (
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
    Option<Uuid>,
    chrono::DateTime<chrono::Utc>,
);

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

struct Fixture {
    org: Uuid,
    kb: Uuid,
    lin: Uuid,
    meridian: Uuid,
    platform: Uuid,
    staff_engineer: Uuid,
    lead: Uuid,
    works_for: Uuid,
    approved: Uuid,
    holds_title: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (person, organization, title) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (lin, meridian, platform) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (staff_engineer, lead) = (Uuid::now_v7(), Uuid::now_v7());
    let (works_for, approved, holds_title) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'unknown-date-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'unknown-date-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'unknown-date-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, key, label) in [
        (person, "person", "Person"),
        (organization, "organization", "Organization"),
        (title, "title", "Title"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    for (id, key, label) in [
        (works_for, "works_for", "works for"),
        (approved, "approved", "approved"),
        (holds_title, "holds_title", "holds title"),
    ] {
        sqlx::query("INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    for (id, ty, name) in [
        (lin, person, "Lin Zhao"),
        (meridian, organization, "Meridian Systems"),
        (platform, organization, "Platform Group"),
        (staff_engineer, title, "Staff Engineer"),
        (lead, title, "Tech Lead"),
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
        lin,
        meridian,
        platform,
        staff_engineer,
        lead,
        works_for,
        approved,
        holds_title,
    })
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

/// Edges in Lin Zhao's one-hop neighborhood at moment T (by fact id). `None` = every moment.
async fn edges_at(pool: &PgPool, f: &Fixture, at: Option<&str>) -> anyhow::Result<HashSet<Uuid>> {
    let (_, edges) =
        utopia_store::graph::neighborhood(pool, f.kb, f.lin, 1, at.map(t), None).await?;
    Ok(edges.into_iter().map(|e| e.id).collect())
}

async fn attested_at(pool: &PgPool, id: Uuid) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
    Ok(
        sqlx::query_scalar("SELECT attested_from FROM facts WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

#[tokio::test]
async fn an_unknown_bound_reaches_as_far_as_the_evidence() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // A: offer letter, from 2023-06-01, still continuing: both ends given in the original (the start)
        let (a, _) = fact(
            &pool,
            &f,
            f.lin,
            f.works_for,
            f.meridian,
            Validity::starting(Some(t("2023-06-01T00:00:00Z")), Some("day"))
                .attested(Some(t("2023-06-01T00:00:00Z"))),
        )
        .await?;
        // B: the 2024-02-20 pay-rise slip says "raise approved by the platform team": no start
        let (b, _) = fact(
            &pool,
            &f,
            f.platform,
            f.approved,
            f.lin,
            Validity::default().attested(Some(t("2024-02-20T00:00:00Z"))),
        )
        .await?;
        // C: the 2025-10-15 role note says "no longer serving as Staff Engineer, date not recorded": both ends unknown
        let (c, _) = fact(
            &pool,
            &f,
            f.lin,
            f.holds_title,
            f.staff_engineer,
            Validity::default()
                .ended_when_unknown()
                .attested(Some(t("2025-10-15T00:00:00Z"))),
        )
        .await?;
        // D: serving as Tech Lead from 2023-06-01; the same 2025-10-15 note says no longer serving: start known, end on an unknown day
        let (d, _) = fact(
            &pool,
            &f,
            f.lin,
            f.holds_title,
            f.lead,
            Validity::starting(Some(t("2023-06-01T00:00:00Z")), Some("day"))
                .ended_when_unknown()
                .attested(Some(t("2025-10-15T00:00:00Z"))),
        )
        .await?;

        // January 2023: nothing yet. B's evidence is in 2024: the "always been so" reading would make it appear
        let jan_2023 = edges_at(&pool, &f, Some("2023-01-01T00:00:00Z")).await?;
        assert!(
            jan_2023.is_empty(),
            "证据出现之前的时刻，一条也不该成立：{jan_2023:?}"
        );

        // 2023-06-01: the two with starts hold from that day; B has no evidence yet; C never holds
        let jun_2023 = edges_at(&pool, &f, Some("2023-06-01T00:00:00Z")).await?;
        assert_eq!(jun_2023, HashSet::from([a, d]));

        // 2024-02-20: B holds from its document's date
        let feb_2024 = edges_at(&pool, &f, Some("2024-02-20T00:00:00Z")).await?;
        assert_eq!(feb_2024, HashSet::from([a, b, d]));
        let day_before = edges_at(&pool, &f, Some("2024-02-19T00:00:00Z")).await?;
        assert!(
            !day_before.contains(&b),
            "文档日期前一天，没起点的事实还不成立"
        );

        // 2025-10-15: D holds until the document that says it ended; the day before, it still does
        let oct_2025 = edges_at(&pool, &f, Some("2025-10-15T00:00:00Z")).await?;
        assert_eq!(
            oct_2025,
            HashSet::from([a, b]),
            "结束了不知哪天的，到它的文档日期为止"
        );
        let eve = edges_at(&pool, &f, Some("2025-10-14T00:00:00Z")).await?;
        assert!(
            eve.contains(&d),
            "文档日期前一天它还成立——保守包含到证据为止"
        );

        // 2026-01-01: the #345 question: the title should no longer be attached
        let jan_2026 = edges_at(&pool, &f, Some("2026-01-01T00:00:00Z")).await?;
        assert!(
            !jan_2026.contains(&c) && !jan_2026.contains(&d),
            "原文说结束了，就不该在之后成立"
        );
        assert_eq!(jan_2026, HashSet::from([a, b]));

        // C, unknown at both ends: holds at no moment; but without a moment given it is present (the canvas draws history)
        let every = edges_at(&pool, &f, None).await?;
        assert_eq!(
            every,
            HashSet::from([a, b, c, d]),
            "at = None 是每一刻，四条都在"
        );

        // Edges carry the interval read out; the frontend filters by it and does not interpret NULL itself
        let (_, edges) =
            utopia_store::graph::neighborhood(&pool, f.kb, f.lin, 1, None, None).await?;
        let by_id = |id: Uuid| edges.iter().find(|e| e.id == id).unwrap();
        assert_eq!(by_id(b).holds_from, Some(t("2024-02-20T00:00:00Z")));
        assert_eq!(by_id(b).holds_to, None);
        assert_eq!(by_id(d).holds_from, Some(t("2023-06-01T00:00:00Z")));
        assert_eq!(
            by_id(d).holds_to,
            Some(t("2025-10-15T00:00:00Z")),
            "结束端读到文档日期"
        );
        assert_eq!(by_id(a).holds_to, None, "ongoing ones are still open");
        assert_eq!(by_id(c).holds_from, Some(t("2025-10-15T00:00:00Z")));
        assert_eq!(
            by_id(c).holds_to,
            Some(t("2025-10-15T00:00:00Z")),
            "两端都不知道：区间为空"
        );

        // Entity panel: without a moment the whole timeline is there (C is listed too, marked as ended); with a moment it is filtered by the rules
        let (_, all) = utopia_store::graph::entity_detail(&pool, f.kb, f.lin, None, None).await?;
        assert_eq!(all.len(), 4);
        let (_, now_ish) = utopia_store::graph::entity_detail(
            &pool,
            f.kb,
            f.lin,
            Some(t("2026-01-01T00:00:00Z")),
            None,
        )
        .await?;
        let ids: HashSet<Uuid> = now_ish.iter().map(|x| x.id).collect();
        assert_eq!(ids, HashSet::from([a, b]));
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

/// The anchor only moves earlier: an earlier document is earlier evidence. A correction row inherits the anchor: changing the interval is restating the same evidence.
#[tokio::test]
async fn the_anchor_moves_earlier_and_is_inherited() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let (b, created) = fact(
            &pool,
            &f,
            f.platform,
            f.approved,
            f.lin,
            Validity::default().attested(Some(t("2024-02-20T00:00:00Z"))),
        )
        .await?;
        assert!(created);
        assert!(!edges_at(&pool, &f, Some("2023-12-01T00:00:00Z"))
            .await?
            .contains(&b));

        // Same assertion, from an earlier document (out-of-order backfill of the corpus is normal): the same row, the anchor moves earlier
        let (again, created) = fact(
            &pool,
            &f,
            f.platform,
            f.approved,
            f.lin,
            Validity::default().attested(Some(t("2023-12-01T00:00:00Z"))),
        )
        .await?;
        assert_eq!(again, b, "the same assertion reuses the same row");
        assert!(!created);
        assert_eq!(attested_at(&pool, b).await?, t("2023-12-01T00:00:00Z"));
        assert!(edges_at(&pool, &f, Some("2023-12-01T00:00:00Z"))
            .await?
            .contains(&b));

        // A later document mentions it again: nothing changes
        fact(
            &pool,
            &f,
            f.platform,
            f.approved,
            f.lin,
            Validity::default().attested(Some(t("2025-01-01T00:00:00Z"))),
        )
        .await?;
        assert_eq!(
            attested_at(&pool, b).await?,
            t("2023-12-01T00:00:00Z"),
            "只往早挪"
        );

        // A person gives it an end: the correction row inherits the anchor, rather than being anchored at the moment of the correction
        let corrected = utopia_store::temporal::correct_interval(
            &pool,
            b,
            Validity {
                from: None,
                from_precision: None,
                to: Some(t("2025-06-01T00:00:00Z")),
                to_precision: Some("day"),
                attested_at: None,
                from_grade: None,
                ..Default::default()
            },
        )
        .await?
        .expect("can still be corrected");
        assert_eq!(
            attested_at(&pool, corrected).await?,
            t("2023-12-01T00:00:00Z")
        );
        let (_, edges) =
            utopia_store::graph::neighborhood(&pool, f.kb, f.lin, 1, None, None).await?;
        let row = edges.iter().find(|e| e.id == corrected).unwrap();
        assert_eq!(row.holds_from, Some(t("2023-12-01T00:00:00Z")));
        assert_eq!(
            row.holds_to,
            Some(t("2025-06-01T00:00:00Z")),
            "原文给了终点就用终点"
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

/// "No longer serving, date not recorded" collides with an open row "serving since 2023-06-01": close it, rather than creating
/// a separate row so the two talk past each other. This is what the #345 question hinged on: the row with the end read correctly, but the open row
/// still said "still is".
#[tokio::test]
async fn an_ending_without_a_date_closes_the_dated_row_it_ends() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // The 2023-06-01 offer: Staff Engineer, still continuing
        let (open, _) = fact(
            &pool,
            &f,
            f.lin,
            f.holds_title,
            f.staff_engineer,
            Validity::starting(Some(t("2023-06-01T00:00:00Z")), Some("day"))
                .attested(Some(t("2023-06-01T00:00:00Z"))),
        )
        .await?;
        assert!(edges_at(&pool, &f, Some("2026-01-01T00:00:00Z"))
            .await?
            .contains(&open));

        // The 2025-10-15 role note: no longer serving, date not recorded: the same assertion, with no start
        let (closed, created) = fact(
            &pool,
            &f,
            f.lin,
            f.holds_title,
            f.staff_engineer,
            Validity::default()
                .ended_when_unknown()
                .attested(Some(t("2025-10-15T00:00:00Z"))),
        )
        .await?;
        assert!(created, "closing is a correction row, not a merge");
        assert_ne!(closed, open);

        // The old row is voided and the correction row chains on: start from the original text, end empty, precision unknown, anchored to the document that says it ended
        let (invalidated, supersedes): (Option<chrono::DateTime<chrono::Utc>>, Option<Uuid>) =
            sqlx::query_as("SELECT invalidated_at, supersedes FROM facts WHERE id = $1")
                .bind(open)
                .fetch_one(&pool)
                .await?;
        assert!(invalidated.is_some(), "the old row is voided, not deleted");
        assert!(supersedes.is_none());
        let row: ClosedRow = sqlx::query_as(
            "SELECT valid_from, valid_to, valid_to_precision, supersedes, attested_to
               FROM facts WHERE id = $1",
        )
        .bind(closed)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            row.0,
            Some(t("2023-06-01T00:00:00Z")),
            "start point unchanged"
        );
        assert_eq!(row.1, None);
        assert_eq!(row.2.as_deref(), Some("unknown"));
        assert_eq!(row.3, Some(open), "on the supersedes chain");
        assert_eq!(
            row.4,
            t("2025-10-15T00:00:00Z"),
            "结束端锚在说出结束的那份文档"
        );

        // The #345 question: January 2026 should no longer have Staff Engineer; June 2024 still does
        assert!(!edges_at(&pool, &f, Some("2026-01-01T00:00:00Z"))
            .await?
            .contains(&closed));
        assert!(edges_at(&pool, &f, Some("2024-06-01T00:00:00Z"))
            .await?
            .contains(&closed));
        let (_, panel) = utopia_store::graph::entity_detail(
            &pool,
            f.kb,
            f.lin,
            Some(t("2026-01-01T00:00:00Z")),
            None,
        )
        .await?;
        assert!(
            panel.is_empty(),
            "the panel no longer lists it in 2026 either: {panel:?}"
        );

        // The same assertion says "ended" again: an already-closed row is not closed again, and no new one is created
        let (again, created) = fact(
            &pool,
            &f,
            f.lin,
            f.holds_title,
            f.staff_engineer,
            Validity::default()
                .ended_when_unknown()
                .attested(Some(t("2025-12-01T00:00:00Z"))),
        )
        .await?;
        assert!(
            !created,
            "there is no open row to close, and it is not new information"
        );
        assert_eq!(again, closed);
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

/// The interval a derivation's premise **reads out** (0022 point 4): in a part_of chain, a link with no start counts from its
/// evidence date, and a link ended on an unknown day runs up to the document that says it. The derived row's ends are therefore
/// the stretch jointly supported by the premises; an end that comes from an anchor has no precision.
#[tokio::test]
async fn a_derivation_reads_no_further_than_its_premises() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // part_of is transitive; four organizations a ⊂ b ⊂ c ⊂ d
        let part_of = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, is_transitive)
             VALUES ($1, $2, 'part_of', 'part of', TRUE)",
        )
        .bind(part_of)
        .bind(f.kb)
        .execute(&pool)
        .await?;
        let org_type: Uuid = sqlx::query_scalar("SELECT type_id FROM entities WHERE id = $1")
            .bind(f.meridian)
            .fetch_one(&pool)
            .await?;
        let mut ids = Vec::new();
        for name in ["Team A", "Group B", "Division C", "Company D"] {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(f.kb)
            .bind(org_type)
            .bind(name)
            .execute(&pool)
            .await?;
            ids.push(id);
        }
        let (a, b, c, d) = (ids[0], ids[1], ids[2], ids[3]);
        // a ⊂ b: the original gives the start 2022-01-01
        fact(
            &pool,
            &f,
            a,
            part_of,
            b,
            Validity::starting(Some(t("2022-01-01T00:00:00Z")), Some("day"))
                .attested(Some(t("2022-01-01T00:00:00Z"))),
        )
        .await?;
        // b ⊂ c: no start, from the 2024-02-20 document
        fact(
            &pool,
            &f,
            b,
            part_of,
            c,
            Validity::default().attested(Some(t("2024-02-20T00:00:00Z"))),
        )
        .await?;
        // c ⊂ d: from 2020-01-01, the 2025-10-15 document says it no longer holds (date not recorded)
        fact(
            &pool,
            &f,
            c,
            part_of,
            d,
            Validity::starting(Some(t("2020-01-01T00:00:00Z")), Some("day"))
                .ended_when_unknown()
                .attested(Some(t("2025-10-15T00:00:00Z"))),
        )
        .await?;

        utopia_store::reasoning::materialize(&pool, f.kb).await?;

        type DerivedRow = (
            Option<chrono::DateTime<chrono::Utc>>,
            Option<String>,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<String>,
        );
        let derived = |s: Uuid, o: Uuid| {
            let pool = pool.clone();
            async move {
                let row: Option<DerivedRow> = sqlx::query_as(
                    "SELECT valid_from, valid_from_precision, valid_to, valid_to_precision
                       FROM derived_facts
                      WHERE kb_id = $1 AND subject_id = $2 AND object_id = $3
                        AND invalidated_at IS NULL",
                )
                .bind(f.kb)
                .bind(s)
                .bind(o)
                .fetch_optional(&pool)
                .await?;
                Ok::<_, anyhow::Error>(row)
            }
        };

        // a ⊂ c: the start is the b ⊂ c evidence date (an anchor), no precision; the end is open
        let ac = derived(a, c).await?.expect("a ⊂ c follows");
        assert_eq!(
            ac.0,
            Some(t("2024-02-20T00:00:00Z")),
            "从没起点那一环的证据起算"
        );
        assert_eq!(ac.1, None, "the end that comes from the anchor has no precision");
        assert_eq!(ac.2, None);

        // a ⊂ d: the start as above; the end is the c ⊂ d anchor (the document that says it ended), also no precision
        let ad = derived(a, d).await?.expect("a ⊂ d follows");
        assert_eq!(ad.0, Some(t("2024-02-20T00:00:00Z")));
        assert_eq!(ad.1, None);
        assert_eq!(
            ad.2,
            Some(t("2025-10-15T00:00:00Z")),
            "到结束的那份文档为止"
        );
        assert_eq!(ad.3, None, "'unknown' is not a granularity, and the end pulled up from the anchor has no precision");

        // b ⊂ d: the start is b ⊂ c's anchor, the end is c ⊂ d's anchor: both ends lack precision
        let bd = derived(b, d).await?.expect("b ⊂ d follows");
        assert_eq!(
            (bd.0, bd.2),
            (
                Some(t("2024-02-20T00:00:00Z")),
                Some(t("2025-10-15T00:00:00Z"))
            )
        );
        assert_eq!((bd.1, bd.3), (None, None));

        // On the graph, lit and dark by the read-out interval: in 2023 a ⊂ c does not yet exist; in mid-2024 it does; in 2026 a ⊂ d has gone out
        // From b two hops out: a ⊂ b and c ⊂ d pull a and d in, so the derived edges are all "between these nodes"
        let at = |when: &str| {
            let pool = pool.clone();
            let when = when.to_string();
            async move {
                let (_, edges) =
                    utopia_store::graph::neighborhood(&pool, f.kb, b, 2, Some(t(&when)), None)
                        .await?;
                Ok::<_, anyhow::Error>(
                    edges
                        .into_iter()
                        .filter(|e| e.derived)
                        .map(|e| (e.source, e.target))
                        .collect::<HashSet<_>>(),
                )
            }
        };
        assert!(!at("2023-06-01T00:00:00Z").await?.contains(&(a, c)));
        assert!(at("2024-06-01T00:00:00Z").await?.contains(&(a, c)));
        assert!(at("2024-06-01T00:00:00Z").await?.contains(&(a, d)));
        assert!(
            !at("2026-01-01T00:00:00Z").await?.contains(&(a, d)),
            "链经过已结束的一环，之后不成立"
        );
        assert!(
            at("2026-01-01T00:00:00Z").await?.contains(&(a, c)),
            "另一条链还开着"
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

/// #393: a bare row with no start can still be closed: the start keeps the anchor of the first evidence, the end is anchored to the document that says it ended.
/// Previously dedup merged this ended-sentence into the bare row, so "it ended" was lost and the row kept reading "still is".
#[tokio::test]
async fn a_bare_row_closes_with_an_anchor_at_each_end() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // A 2024-02-20 note: serving as Staff Engineer, no start date written
        let (bare, _) = fact(
            &pool,
            &f,
            f.lin,
            f.holds_title,
            f.staff_engineer,
            Validity::default().attested(Some(t("2024-02-20T00:00:00Z"))),
        )
        .await?;
        // The 2025-10-15 role note: no longer serving, date not recorded: the same assertion, also no start
        let (closed, created) = fact(
            &pool,
            &f,
            f.lin,
            f.holds_title,
            f.staff_engineer,
            Validity::default()
                .ended_when_unknown()
                .attested(Some(t("2025-10-15T00:00:00Z"))),
        )
        .await?;
        assert!(
            created,
            "closing is now a correction row, no longer a merge"
        );
        assert_ne!(closed, bare);

        let (from, to_prec, attested_from, attested_to): BareClosedRow = sqlx::query_as(
            "SELECT valid_from, valid_to_precision, attested_from, attested_to
               FROM facts WHERE id = $1",
        )
        .bind(closed)
        .fetch_one(&pool)
        .await?;
        assert_eq!(from, None, "the source gives no start, and still none");
        assert_eq!(to_prec.as_deref(), Some("unknown"));
        assert_eq!(
            attested_from,
            t("2024-02-20T00:00:00Z"),
            "起点的锚点是第一份证据"
        );
        assert_eq!(
            attested_to,
            Some(t("2025-10-15T00:00:00Z")),
            "终点的锚点是说出结束的那份文档"
        );

        // Read out: [first evidence, the document that says it ended)
        assert!(!edges_at(&pool, &f, Some("2023-12-01T00:00:00Z"))
            .await?
            .contains(&closed));
        assert!(edges_at(&pool, &f, Some("2024-06-01T00:00:00Z"))
            .await?
            .contains(&closed));
        assert!(!edges_at(&pool, &f, Some("2026-01-01T00:00:00Z"))
            .await?
            .contains(&closed));
        let (_, edges) =
            utopia_store::graph::neighborhood(&pool, f.kb, f.lin, 1, None, None).await?;
        let row = edges.iter().find(|e| e.id == closed).unwrap();
        assert_eq!(row.holds_from, Some(t("2024-02-20T00:00:00Z")));
        assert_eq!(row.holds_to, Some(t("2025-10-15T00:00:00Z")));
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
