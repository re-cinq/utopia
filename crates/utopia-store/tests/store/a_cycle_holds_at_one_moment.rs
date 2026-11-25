//! A transitive cycle must hold across its whole path at one moment (#636), run against a real DB.
//!
//! The pure-logic part — pairwise overlap with no triple intersection, pruning never losing a real cycle, random-graph differential checks — runs in
//! `utopia-reason`. What is pinned here is what only a pass through data-fetching and storage can see:
//!
//! 1. **A cycle whose times are staggered is not queued for Review; once the time is corrected, the cycle disappears.** A person fills in the end date of one of them
//!    (302's `correct_interval`, invalidate + rewrite), and the row from the re-run is cleared. Before, even after the fix
//!    the same cycle was still computed — the person fixed the data as prompted, yet the queue did not move.
//! 2. **Intervals are read from the way they are written in the DB** (`read_span`); every way of writing must be covered:
//!    - Ended on an unknown day: held up to the evidence that states it (0022) — read as open, a single "former"
//!      sentence could conjure from nothing a cycle that still holds today
//!    - No start: counted from the earliest evidence
//!    - An event holds within the bucket it names: intersects on the same day and the same month, and adjacent days do not intersect; an event with no date
//!      holds at no moment (0031)
//!    - An eternal predicate has no time to stagger, so it is reported as-is; turn the predicate into a state and the re-run clears it
//! 3. **Two edges between the same pair of nodes, one of which never held at the same time as any other edge**: the cycle is still found via the other one,
//!    and the path uses that one
//! 4. **Verdicts carry over**: the axiom was relaxed and the cycle still holds at once, so that row reopens; an approved one stays silent on re-run; remove one edge on the cycle,
//!    and the cycle is no longer computed, while the verdict stays
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is not set. It builds and tears down its own data and never touches an existing DB.

use sqlx::PgPool;
use utopia_store::graph::Validity;
use utopia_store::reasoning;
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    etype: Uuid,
    user: Uuid,
    /// state + transitive
    part_of: Uuid,
    /// event + transitive
    merged_into: Uuid,
    /// eternal + transitive
    located_in: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (etype, user) = (Uuid::now_v7(), Uuid::now_v7());
    let (part_of, merged_into, located_in) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'cycle-time-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'cycle-time-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'cycle-time-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO users (id, org_id, email, password_hash, display_name)
         VALUES ($1, $2, $3, '', 'Cycle Reviewer')",
    )
    .bind(user)
    .bind(org)
    .bind(format!("cycle-{user}@test.local"))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'thing', 'Thing')",
    )
    .bind(etype)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, key, temporal) in [
        (part_of, "part_of", "state"),
        (merged_into, "merged_into", "event"),
        (located_in, "located_in", "eternal"),
    ] {
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, temporal, is_transitive)
             VALUES ($1, $2, $3, $3, $4, TRUE)",
        )
        .bind(id)
        .bind(kb)
        .bind(key)
        .bind(temporal)
        .execute(pool)
        .await?;
    }
    Ok(Fixture {
        org,
        kb,
        etype,
        user,
        part_of,
        merged_into,
        located_in,
    })
}

async fn entity(pool: &PgPool, f: &Fixture, name: &str) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(f.etype)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn entities<const N: usize>(pool: &PgPool, f: &Fixture) -> anyhow::Result<[Uuid; N]> {
    let mut out = [Uuid::nil(); N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = entity(pool, f, &format!("E{i}")).await?;
    }
    Ok(out)
}

fn ts(d: &str) -> chrono::DateTime<chrono::Utc> {
    format!("{d}T00:00:00Z").parse().unwrap()
}

/// How a fact is written in time in the DB. The date is given as the precision-aligned day (the CHECK requires truncation first)
#[derive(Clone, Copy, Default)]
struct When<'a> {
    /// (date, precision)
    from: Option<(&'a str, &'a str)>,
    to: Option<(&'a str, &'a str)>,
    /// Ended on an unknown day: `valid_to_precision = 'unknown'`, anchored to this evidence's date
    ended_unknown_as_of: Option<&'a str>,
    /// The earliest day of evidence (`attested_from`); if not given, it is right now
    attested: Option<&'a str>,
}

fn span<'a>(from: &'a str, to: Option<&'a str>) -> When<'a> {
    When {
        from: Some((from, "day")),
        to: to.map(|t| (t, "day")),
        ..Default::default()
    }
}

async fn fact(
    pool: &PgPool,
    f: &Fixture,
    (s, p, o): (Uuid, Uuid, Uuid),
    w: When<'_>,
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    let to_precision = match (w.to, w.ended_unknown_as_of) {
        (Some((_, p)), _) => Some(p),
        (None, Some(_)) => Some("unknown"),
        (None, None) => None,
    };
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                            valid_from, valid_from_precision, valid_to, valid_to_precision,
                            attested_from, attested_to)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, COALESCE($10, now()), $11)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(s)
    .bind(p)
    .bind(o)
    .bind(w.from.map(|(d, _)| ts(d)))
    .bind(w.from.map(|(_, p)| p))
    .bind(w.to.map(|(d, _)| ts(d)))
    .bind(to_precision)
    .bind(w.attested.or(w.ended_unknown_as_of).map(ts))
    .bind(w.ended_unknown_as_of.map(ts))
    .execute(pool)
    .await?;
    Ok(id)
}

/// An open cycle; each is a sorted set of facts
async fn open_cycles(pool: &PgPool, f: &Fixture) -> anyhow::Result<Vec<Vec<Uuid>>> {
    let rows: Vec<Vec<Uuid>> = sqlx::query_scalar(
        "SELECT path FROM axiom_violations
          WHERE kb_id = $1 AND kind = 'cycle' AND status = 'open'",
    )
    .bind(f.kb)
    .fetch_all(pool)
    .await?;
    let mut out: Vec<Vec<Uuid>> = rows.into_iter().map(sorted).collect();
    out.sort();
    Ok(out)
}

fn sorted(mut v: Vec<Uuid>) -> Vec<Uuid> {
    v.sort();
    v
}

/// Bring the DB up, run, and tear down whether it succeeded or not
macro_rules! with_fixture {
    (|$pool:ident, $f:ident| $body:block) => {{
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(());
        };
        let $pool = PgPool::connect(&url).await?;
        let fixture = seed(&$pool).await?;
        let result = {
            let $f = &fixture;
            let $pool = &$pool;
            async move { $body }.await
        };
        // Tear down the DB first: a verdict row's decided_by points to a user of this org, and when the org is deleted, the cascade order between users and the KB
        // is not guaranteed
        sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
            .bind(fixture.kb)
            .execute(&$pool)
            .await?;
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(fixture.org)
            .execute(&$pool)
            .await?;
        result
    }};
}

#[tokio::test]
async fn a_cycle_is_one_moment_and_fixing_the_time_clears_it() -> anyhow::Result<()> {
    with_fixture!(|pool, f| {
        // ---- 1. Staggered-in-time cycles: two segments joined end to end; three segments overlapping pairwise but with no common point
        let [a, b, d, e, g] = entities(pool, f).await?;
        fact(
            pool,
            f,
            (a, f.part_of, b),
            span("2019-01-01", Some("2021-01-01")),
        )
        .await?;
        fact(pool, f, (b, f.part_of, a), span("2022-01-01", None)).await?;
        fact(
            pool,
            f,
            (d, f.part_of, e),
            span("2020-01-01", Some("2022-01-01")),
        )
        .await?;
        fact(
            pool,
            f,
            (e, f.part_of, g),
            span("2021-01-01", Some("2023-01-01")),
        )
        .await?;
        fact(
            pool,
            f,
            (g, f.part_of, d),
            span("2022-01-01", Some("2024-01-01")),
        )
        .await?;

        let r = reasoning::run(pool, f.kb).await?;
        assert_eq!(r.edges, 5);
        assert!(
            open_cycles(pool, f).await?.is_empty(),
            "没有哪一刻整条成立的环不该进 Review"
        );

        // ---- 2. Cycles that really do hold at the same time are still reported
        let [x, y] = entities(pool, f).await?;
        let xy = fact(pool, f, (x, f.part_of, y), span("2020-01-01", None)).await?;
        let yx = fact(pool, f, (y, f.part_of, x), span("2021-06-01", None)).await?;
        reasoning::run(pool, f.kb).await?;
        assert_eq!(open_cycles(pool, f).await?, vec![sorted(vec![xy, yx])]);

        // ---- 3. A person fills in the end date of X part_of Y: it ended 2021-06-01, which is exactly
        // the day Y part_of X started. Re-run after the change, and that row is cleared
        utopia_store::temporal::correct_interval(
            pool,
            xy,
            Validity {
                from: Some(ts("2020-01-01")),
                from_precision: Some("day"),
                to: Some(ts("2021-06-01")),
                to_precision: Some("day"),
                ..Default::default()
            },
        )
        .await?
        .expect("这条还活着，应当改得动");
        let after = reasoning::run(pool, f.kb).await?;
        assert!(
            open_cycles(pool, f).await?.is_empty(),
            "改对了时间，环就不在了"
        );
        assert_eq!(after.cleared, 1, "上一轮那一行是陈的");
        Ok::<_, anyhow::Error>(())
    })
}

/// Read the "former" one up to its evidence. Read as open, an edge that had long ended but never had its end day written
/// would combine with a reverse edge that only began today into a cycle
#[tokio::test]
async fn an_unknown_end_stops_where_the_evidence_does() -> anyhow::Result<()> {
    with_fixture!(|pool, f| {
        let former = When {
            from: Some(("2019-01-01", "day")),
            ended_unknown_as_of: Some("2020-06-01"),
            ..Default::default()
        };

        // Evidence from 2020-06 says it has already ended; the reverse edge only began in 2021
        let [a, b] = entities(pool, f).await?;
        fact(pool, f, (a, f.part_of, b), former).await?;
        fact(pool, f, (b, f.part_of, a), span("2021-01-01", None)).await?;
        reasoning::run(pool, f.kb).await?;
        assert!(
            open_cycles(pool, f).await?.is_empty(),
            "结束了不知哪天的不是还开着"
        );

        // The reverse edge began in early 2020: it predates the evidence, so the two really did hold together at one point
        let [c, d] = entities(pool, f).await?;
        let cd = fact(pool, f, (c, f.part_of, d), former).await?;
        let dc = fact(pool, f, (d, f.part_of, c), span("2020-01-01", None)).await?;
        reasoning::run(pool, f.kb).await?;
        assert_eq!(open_cycles(pool, f).await?, vec![sorted(vec![cd, dc])]);
        Ok::<_, anyhow::Error>(())
    })
}

/// Where no start is written, it counts from the earliest evidence, not from always being so
#[tokio::test]
async fn a_missing_start_reads_from_the_evidence() -> anyhow::Result<()> {
    with_fixture!(|pool, f| {
        let since = |d| When {
            attested: Some(d),
            ..Default::default()
        };

        // Evidence from 2023 says A belongs to B (not saying from when); B belonging to A ended in 2022
        let [a, b] = entities(pool, f).await?;
        fact(pool, f, (a, f.part_of, b), since("2023-01-01")).await?;
        fact(
            pool,
            f,
            (b, f.part_of, a),
            span("2020-01-01", Some("2022-01-01")),
        )
        .await?;
        reasoning::run(pool, f.kb).await?;
        assert!(open_cycles(pool, f).await?.is_empty());

        // The evidence is from 2021: it falls within the reverse edge's interval
        let [c, d] = entities(pool, f).await?;
        let cd = fact(pool, f, (c, f.part_of, d), since("2021-01-01")).await?;
        let dc = fact(
            pool,
            f,
            (d, f.part_of, c),
            span("2020-01-01", Some("2022-01-01")),
        )
        .await?;
        reasoning::run(pool, f.kb).await?;
        assert_eq!(open_cycles(pool, f).await?, vec![sorted(vec![cd, dc])]);
        Ok::<_, anyhow::Error>(())
    })
}

/// An event holds within the bucket it names: the day 2024-03-05 intersects March 2024, and does not intersect March 6.
/// An event with no date holds at no moment
#[tokio::test]
async fn an_event_holds_in_the_bucket_it_names() -> anyhow::Result<()> {
    with_fixture!(|pool, f| {
        let on = |d, p| When {
            from: Some((d, p)),
            ..Default::default()
        };

        let [a, b] = entities(pool, f).await?;
        let ab = fact(pool, f, (a, f.merged_into, b), on("2024-03-05", "day")).await?;
        let ba = fact(pool, f, (b, f.merged_into, a), on("2024-03-01", "month")).await?;

        let [c, d] = entities(pool, f).await?;
        fact(pool, f, (c, f.merged_into, d), on("2024-03-05", "day")).await?;
        fact(pool, f, (d, f.merged_into, c), on("2024-03-06", "day")).await?;

        let [e, g] = entities(pool, f).await?;
        fact(pool, f, (e, f.merged_into, g), When::default()).await?;
        fact(pool, f, (g, f.merged_into, e), When::default()).await?;

        reasoning::run(pool, f.kb).await?;
        assert_eq!(
            open_cycles(pool, f).await?,
            vec![sorted(vec![ab, ba])],
            "只有同一天落在同一个月里的那一对成环"
        );
        Ok::<_, anyhow::Error>(())
    })
}

/// An eternal predicate has no time to stagger: its dates are not consulted even if written, so it is reported. Turn the predicate into a state and a re-run clears it;
/// turn it back and the cycle returns
#[tokio::test]
async fn an_eternal_predicate_has_no_time_to_tell_apart() -> anyhow::Result<()> {
    with_fixture!(|pool, f| {
        let [a, b] = entities(pool, f).await?;
        let ab = fact(
            pool,
            f,
            (a, f.located_in, b),
            span("2019-01-01", Some("2021-01-01")),
        )
        .await?;
        let ba = fact(pool, f, (b, f.located_in, a), span("2022-01-01", None)).await?;
        reasoning::run(pool, f.kb).await?;
        assert_eq!(open_cycles(pool, f).await?, vec![sorted(vec![ab, ba])]);

        let set = |t: &'static str| {
            sqlx::query("UPDATE relation_types SET temporal = $1 WHERE id = $2")
                .bind(t)
                .bind(f.located_in)
        };
        set("state").execute(pool).await?;
        let r = reasoning::run(pool, f.kb).await?;
        assert!(open_cycles(pool, f).await?.is_empty(), "成了状态，就看日期");
        assert_eq!(r.cleared, 1);

        set("eternal").execute(pool).await?;
        reasoning::run(pool, f.kb).await?;
        assert_eq!(open_cycles(pool, f).await?, vec![sorted(vec![ab, ba])]);
        Ok::<_, anyhow::Error>(())
    })
}

/// Two edges between the same pair of nodes: the old one never held at the same time as the other edges on the cycle, and the new one holds. The cycle is found via
/// the new one — if the old one is hit first, it is pruned, and the whole cycle must not be lost along with it
#[tokio::test]
async fn a_parallel_edge_that_never_met_does_not_hide_the_one_that_did() -> anyhow::Result<()> {
    with_fixture!(|pool, f| {
        let [a, b, c] = entities(pool, f).await?;
        // Insert the old one first, so that the sequential scan reads it first
        fact(
            pool,
            f,
            (a, f.part_of, b),
            span("2019-01-01", Some("2020-01-01")),
        )
        .await?;
        let ab = fact(pool, f, (a, f.part_of, b), span("2021-01-01", None)).await?;
        let bc = fact(pool, f, (b, f.part_of, c), span("2021-01-01", None)).await?;
        let ca = fact(pool, f, (c, f.part_of, a), span("2021-01-01", None)).await?;
        reasoning::run(pool, f.kb).await?;
        assert_eq!(open_cycles(pool, f).await?, vec![sorted(vec![ab, bc, ca])]);
        Ok::<_, anyhow::Error>(())
    })
}

/// Verdicts carry over: the axiom was relaxed and the cycle still holds at once, so it reopens; an approved one stays silent; remove one, and the cycle is no longer computed
#[tokio::test]
async fn decisions_on_a_cycle_still_hold() -> anyhow::Result<()> {
    with_fixture!(|pool, f| {
        let [x, y] = entities(pool, f).await?;
        let xy = fact(pool, f, (x, f.part_of, y), span("2020-01-01", None)).await?;
        fact(pool, f, (y, f.part_of, x), span("2021-01-01", None)).await?;
        reasoning::run(pool, f.kb).await?;
        let id = || async {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM axiom_violations WHERE kb_id = $1 AND kind = 'cycle'",
            )
            .bind(f.kb)
            .fetch_one(pool)
            .await
        };

        reasoning::decide(pool, f.kb, id().await?, "axiom_relaxed", f.user).await?;
        let r = reasoning::run(pool, f.kb).await?;
        assert_eq!(r.reopened, 1, "公理没真放宽，环还同时成立，那一行回到 open");

        reasoning::decide(pool, f.kb, id().await?, "accepted", f.user).await?;
        let r = reasoning::run(pool, f.kb).await?;
        assert_eq!((r.inserted, r.reopened), (0, 0), "认可过的重跑沉默");
        assert!(open_cycles(pool, f).await?.is_empty());

        // Another cycle: remove one of its edges
        let [p, q] = entities(pool, f).await?;
        let pq = fact(pool, f, (p, f.part_of, q), span("2020-01-01", None)).await?;
        fact(pool, f, (q, f.part_of, p), span("2020-06-01", None)).await?;
        reasoning::run(pool, f.kb).await?;
        let open: Uuid = sqlx::query_scalar(
            "SELECT id FROM axiom_violations WHERE kb_id = $1 AND kind = 'cycle' AND status = 'open'",
        )
        .bind(f.kb)
        .fetch_one(pool)
        .await?;
        reasoning::retract_from_violation(pool, f.kb, open, Some(pq), f.user).await?;
        let r = reasoning::run(pool, f.kb).await?;
        assert_eq!(r.reopened, 0, "撤掉之后环不再算出来，承诺兑现了");
        assert!(open_cycles(pool, f).await?.is_empty());
        let resolved: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM axiom_violations
              WHERE kb_id = $1 AND kind = 'cycle' AND status = 'resolved'",
        )
        .bind(f.kb)
        .fetch_one(pool)
        .await?;
        assert_eq!(resolved, 2, "两次裁决都留着");
        let _ = xy;
        Ok::<_, anyhow::Error>(())
    })
}
