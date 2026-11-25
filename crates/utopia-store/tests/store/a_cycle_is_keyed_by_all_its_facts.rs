//! Cycles keyed by their whole path (#641), hit against a real database.
//!
//! A cycle's left is its smallest fact and right is the last fact after rotating it to start. Two cycles sharing these two, but
//! differing in the middle, used to collide into one row on `(kb_id, kind, left_fact, right_fact)`: the one computed later
//! overwrote the path of the one computed earlier, so one cycle vanished from Review, and which one survived depended on traversal order. Four things are pinned:
//!
//! 1. **Two cycles, two rows**: re-running neither inserts nor clears, and the row ids do not change
//! 2. **Rulings are separate**: accept one, and after relaxing the axiom the other reopens: the accepted row's path
//!    is still its own cycle, not overwritten by the other cycle's path
//! 3. **Retracting an edge that sits on only one cycle** affects only that one
//! 4. **Other kinds still key on the first and last edge**: re-running the antisymmetric pair is still the same row
//!
//! Without `UTOPIA_DATABASE_URL` it skips rather than fails. It sets up and tears down its own data and never touches an existing database.

use sqlx::PgPool;
use utopia_store::reasoning;
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    etype: Uuid,
    user: Uuid,
    /// state + transitive
    part_of: Uuid,
    /// state + asymmetric
    reports_to: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (etype, user, part_of, reports_to) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'cycle-key-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'cycle-key-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'cycle-key-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO users (id, org_id, email, password_hash, display_name)
         VALUES ($1, $2, $3, '', 'Cycle Key Reviewer')",
    )
    .bind(user)
    .bind(org)
    .bind(format!("cycle-key-{user}@test.local"))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'thing', 'Thing')",
    )
    .bind(etype)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, temporal, is_transitive, is_asymmetric)
         VALUES ($1, $2, 'part_of', 'part_of', 'state', TRUE, FALSE),
                ($3, $2, 'reports_to', 'reports_to', 'state', FALSE, TRUE)",
    )
    .bind(part_of)
    .bind(kb)
    .bind(reports_to)
    .execute(pool)
    .await?;
    Ok(Fixture {
        org,
        kb,
        etype,
        user,
        part_of,
        reports_to,
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

/// A fact that holds from now on: everyone holds at once, here only the shape is asked
async fn fact(pool: &PgPool, f: &Fixture, s: Uuid, p: Uuid, o: Uuid) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(s)
    .bind(p)
    .bind(o)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Cycle rows: (id, path, status, resolution), ordered by path
async fn cycle_rows(
    pool: &PgPool,
    f: &Fixture,
) -> anyhow::Result<Vec<(Uuid, Vec<Uuid>, String, Option<String>)>> {
    Ok(sqlx::query_as(
        "SELECT id, path, status, resolution FROM axiom_violations
          WHERE kb_id = $1 AND kind = 'cycle' ORDER BY path",
    )
    .bind(f.kb)
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
async fn two_cycles_sharing_their_ends_are_two_rows() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let result = async {
        let [a, b, c, d, x] = [
            entity(&pool, &f, "A").await?,
            entity(&pool, &f, "B").await?,
            entity(&pool, &f, "C").await?,
            entity(&pool, &f, "D").await?,
            entity(&pool, &f, "X").await?,
        ];
        // Inserted in order of id: A→B is the smallest, X→A closing is the last one. Both cycles' ends are these two edges
        let ab = fact(&pool, &f, a, f.part_of, b).await?;
        let bc = fact(&pool, &f, b, f.part_of, c).await?;
        let bd = fact(&pool, &f, b, f.part_of, d).await?;
        let cx = fact(&pool, &f, c, f.part_of, x).await?;
        let dx = fact(&pool, &f, d, f.part_of, x).await?;
        let xa = fact(&pool, &f, x, f.part_of, a).await?;
        let (via_c, via_d) = (vec![ab, bc, cx, xa], vec![ab, bd, dx, xa]);

        // ---- 1. Two cycles, two rows; stable on re-run
        let first = reasoning::run(&pool, f.kb).await?;
        assert_eq!(first.found, 2);
        assert_eq!(first.inserted, 2, "两个环各插一行，不是一行被另一行覆盖");
        let rows = cycle_rows(&pool, &f).await?;
        let paths: Vec<Vec<Uuid>> = rows.iter().map(|r| r.1.clone()).collect();
        assert_eq!(paths, vec![via_c.clone(), via_d.clone()]);
        for _ in 0..3 {
            let again = reasoning::run(&pool, f.kb).await?;
            assert_eq!((again.inserted, again.cleared), (0, 0));
            let ids: Vec<Uuid> = cycle_rows(&pool, &f).await?.iter().map(|r| r.0).collect();
            assert_eq!(
                ids,
                rows.iter().map(|r| r.0).collect::<Vec<_>>(),
                "行不换 id"
            );
        }
        let (id_c, id_d) = (rows[0].0, rows[1].0);

        // ---- 2. Rulings are separate
        reasoning::decide(&pool, f.kb, id_c, "accepted", f.user).await?;
        reasoning::decide(&pool, f.kb, id_d, "axiom_relaxed", f.user).await?;
        let r = reasoning::run(&pool, f.kb).await?;
        assert_eq!(r.reopened, 1, "放宽公理的那个环还在，重开它——只重开它");
        let rows = cycle_rows(&pool, &f).await?;
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (
                rows[0].0,
                &rows[0].1,
                rows[0].2.as_str(),
                rows[0].3.as_deref()
            ),
            (id_c, &via_c, "resolved", Some("accepted")),
            "认可的那一行仍是经 C 的环，没被经 D 的路径盖掉"
        );
        assert_eq!((rows[1].0, rows[1].2.as_str()), (id_d, "open"));

        // ---- 3. Retract B→D, which sits only on the cycle through D
        reasoning::retract_from_violation(&pool, f.kb, id_d, Some(bd), f.user).await?;
        let r = reasoning::run(&pool, f.kb).await?;
        assert_eq!(
            (r.found, r.reopened, r.inserted),
            (1, 0, 0),
            "只剩经 C 的环，而它认可过"
        );
        let rows = cycle_rows(&pool, &f).await?;
        assert_eq!(rows[0].3.as_deref(), Some("accepted"));
        assert_eq!(rows[1].3.as_deref(), Some("fact_retracted"));

        // ---- 4. Other kinds still key on the first and last edge
        let boss = fact(&pool, &f, c, f.reports_to, d).await?;
        let back = fact(&pool, &f, d, f.reports_to, c).await?;
        reasoning::run(&pool, f.kb).await?;
        let again = reasoning::run(&pool, f.kb).await?;
        assert_eq!(again.inserted, 0);
        let pair: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT left_fact, right_fact FROM axiom_violations
              WHERE kb_id = $1 AND kind = 'asymmetry'",
        )
        .bind(f.kb)
        .fetch_all(&pool)
        .await?;
        assert_eq!(pair, vec![(boss.min(back), boss.max(back))]);
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
    result
}
