//! Record-axis rewind (0019 / #307), hit against a real database.
//!
//! Why it must hit a database: this defence **lives entirely inside SQL strings**. `cargo check` cannot see
//! a missing WHERE clause, and clippy will not say a word either: 0009 tripped exactly here (`NULL <> uuid`
//! reports no error and simply selects nothing), and the `human_type_decisions` test is what that episode left behind.
//!
//! Both directions must be asserted, because they break in different ways:
//! - A retracted row **does not appear** at `as_of = now` (a reversed predicate would resurrect every voided row)
//! - A retracted row **appears before the moment it was voided** (if the predicate is not wired up it is always "now", and replay stays empty-handed)
//! - A row with `recorded_at` later than T **does not appear** at T (writing only a lower bound would show March the April correction)
//!
//! Sets up and tears down its own data: a one-off org/workspace/kb, removed together with the org after the run. Never touches an existing database.

use sqlx::PgPool;
use uuid::Uuid;

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

struct Fixture {
    kb: Uuid,
    zhang: Uuid,
    /// The assertion recorded on 03-01 and corrected away on 03-20
    retracted: Uuid,
    /// The correction row recorded on 03-20
    correction: Uuid,
    /// A derivation inferred on 03-05 whose premise went away on 03-22
    derived: Uuid,
}

/// A person took over a project, and in March we changed our mind; the engine inferred an edge in between.
async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (person, project) = (Uuid::now_v7(), Uuid::now_v7());
    let (leads, part_of) = (Uuid::now_v7(), Uuid::now_v7());
    let (zhang, phoenix, program) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (retracted, correction, derived, rule) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'rewind-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'rewind-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'rewind-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, key, label) in [
        (person, "person", "Person"),
        (project, "project", "Project"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    for (id, key, label) in [(leads, "leads", "leads"), (part_of, "part_of", "part of")] {
        sqlx::query("INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    for (id, type_id, name) in [
        (zhang, person, "Zhang San"),
        (phoenix, project, "Project Phoenix"),
        (program, project, "Phoenix Program"),
    ] {
        // **Entity birth times must be backfilled too**: the fact is recorded in March, but the entity was created "just now"; that kind of ledger
        // does not exist in reality, and after #336 an entity not yet born at moment T no longer appears on the graph
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name, created_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(kb)
        .bind(type_id)
        .bind(name)
        .bind(t("2026-01-01T00:00:00Z"))
        .execute(pool)
        .await?;
    }

    let fact = |id: Uuid, rec: &str, inv: Option<&str>, sup: Option<Uuid>| {
        let (rec, inv) = (t(rec), inv.map(t));
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                                recorded_at, invalidated_at, supersedes)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(kb)
        .bind(zhang)
        .bind(leads)
        .bind(phoenix)
        .bind(rec)
        .bind(inv)
        .bind(sup)
    };
    fact(
        retracted,
        "2026-03-01T00:00:00Z",
        Some("2026-03-20T00:00:00Z"),
        None,
    )
    .execute(pool)
    .await?;
    fact(correction, "2026-03-20T00:00:00Z", None, Some(retracted))
        .execute(pool)
        .await?;

    // Derivations also have two axes (derived_at / invalidated_at): the replayed graph keeps the edges inferred **at that time**,
    // rather than the conclusions of today's rules
    sqlx::query(
        "INSERT INTO rules (id, kb_id, predicate_id, kind) VALUES ($1, $2, $3, 'transitive')",
    )
    .bind(rule)
    .bind(kb)
    .bind(part_of)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_id, rule_id,
                                    derived_at, invalidated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(derived)
    .bind(kb)
    .bind(phoenix)
    .bind(part_of)
    .bind(program)
    .bind(rule)
    .bind(t("2026-03-05T00:00:00Z"))
    .bind(t("2026-03-22T00:00:00Z"))
    .execute(pool)
    .await?;

    Ok(Fixture {
        kb,
        zhang,
        retracted,
        correction,
        derived,
    })
}

fn edge_ids(edges: &[utopia_core::models::GraphEdge]) -> Vec<Uuid> {
    let mut v: Vec<Uuid> = edges.iter().map(|e| e.id).collect();
    v.sort();
    v
}

fn sorted(mut v: Vec<Uuid>) -> Vec<Uuid> {
    v.sort();
    v
}

#[tokio::test]
async fn the_recording_axis_rewinds_on_every_graph_read() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let overview = |as_of: Option<&'static str>| {
        let pool = pool.clone();
        async move { utopia_store::graph::overview(&pool, f.kb, 50, None, as_of.map(t)).await }
    };

    // 1. Now: the correction row is there, the assertion it replaced is not, and the derivation whose premise went away is not either
    let (nodes, edges, _, total_edges) = overview(None).await?;
    assert_eq!(
        edge_ids(&edges),
        vec![f.correction],
        "现在只该有修正行那条边"
    );
    assert_eq!(total_edges, 1, "边总数跟着画布数，不是历史全量");
    let degree = |nodes: &[utopia_core::models::GraphNode], id: Uuid| {
        nodes.iter().find(|n| n.id == id).map(|n| n.degree)
    };
    assert_eq!(degree(&nodes, f.zhang), Some(1));

    // 2. Rewind to 03-10: the pre-void assertion is back, and the derivation inferred in March is also there,
    //    but the correction row recorded on 03-20 **should not appear**: that is something that had not yet happened at that time
    let (nodes, edges, _, total_edges) = overview(Some("2026-03-10T00:00:00Z")).await?;
    assert_eq!(
        edge_ids(&edges),
        sorted(vec![f.retracted, f.derived]),
        "03-10 该看见当时持有的断言与派生"
    );
    assert_eq!(total_edges, 2);
    assert_eq!(degree(&nodes, f.zhang), Some(1), "度数按当时的边数");
    assert!(
        edges
            .iter()
            .find(|e| e.id == f.derived)
            .is_some_and(|e| e.derived),
        "派生边要带 derived 位，界面靠它画出区别"
    );

    // 3. Rewind to 03-25: the correction row is already recorded, and the derivation has been overturned
    let (_, edges, _, _) = overview(Some("2026-03-25T00:00:00Z")).await?;
    assert_eq!(edge_ids(&edges), vec![f.correction]);

    // 4. Earlier than everything: the graph is empty, rather than "degrading to now"
    let (_, edges, _, total_edges) = overview(Some("2026-02-01T00:00:00Z")).await?;
    assert!(edges.is_empty(), "02-01 我们还什么都没记下");
    assert_eq!(total_edges, 0);

    // 5. Spreading the neighborhood also finds neighbors by the edges as they were then
    let (_, edges) = utopia_store::graph::neighborhood(
        &pool,
        f.kb,
        f.zhang,
        1,
        None,
        Some(t("2026-03-10T00:00:00Z")),
    )
    .await?;
    assert_eq!(edge_ids(&edges), vec![f.retracted]);
    let (_, edges) = utopia_store::graph::neighborhood(&pool, f.kb, f.zhang, 1, None, None).await?;
    assert_eq!(edge_ids(&edges), vec![f.correction]);

    // 6. Open a node: the panel speaks of the facts **as they were then**; on a replayed graph, opening a node
    //    while the sidebar still answers today's facts puts the two claims side by side on one screen
    let facts = |as_of: Option<&'static str>| {
        let pool = pool.clone();
        async move {
            utopia_store::graph::entity_detail(&pool, f.kb, f.zhang, None, as_of.map(t))
                .await
                .map(|(_, facts)| facts)
        }
    };
    let now = facts(None).await?;
    assert_eq!(now.len(), 1);
    assert_eq!(now[0].id, f.correction);
    assert!(now[0].corrected, "修正行认得出自己改写了谁");

    let march = facts(Some("2026-03-10T00:00:00Z")).await?;
    assert_eq!(march.len(), 1);
    assert_eq!(march[0].id, f.retracted, "03-10 面板该给出当时那条");

    let before = facts(Some("2026-02-01T00:00:00Z")).await?;
    assert!(before.is_empty(), "记下之前不该有事实");

    // #351: within the same second, record first and then correct. Going back a full second from the correction moment would skip the old record,
    // while the correction moment itself is already the new record; the tooltip's "previous microsecond" must be able to read back the old fact
    let first = t("2026-03-20T02:43:53.382000Z");
    let changed = t("2026-03-20T02:43:53.382002Z");
    sqlx::query("UPDATE facts SET recorded_at = $2, invalidated_at = $3 WHERE id = $1")
        .bind(f.retracted)
        .bind(first)
        .bind(changed)
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE facts SET recorded_at = $2 WHERE id = $1")
        .bind(f.correction)
        .bind(changed)
        .execute(&pool)
        .await?;

    let events = utopia_store::graph::graph_changes(
        &pool,
        f.kb,
        t("2026-03-20T00:00:00Z"),
        t("2026-03-21T00:00:00Z"),
        Some(f.zhang),
        None,
        10,
        0,
    )
    .await?;
    assert_eq!(events.len(), 2);
    assert_eq!(
        (events[0].at, events[0].kind.as_str()),
        (changed, "corrected")
    );
    assert_eq!((events[1].at, events[1].kind.as_str()), (first, "asserted"));

    for (moment, expected) in [
        (changed - chrono::Duration::seconds(1), None),
        (
            changed - chrono::Duration::microseconds(1),
            Some(f.retracted),
        ),
        (changed, Some(f.correction)),
    ] {
        let (_, rows) =
            utopia_store::graph::entity_detail(&pool, f.kb, f.zhang, None, Some(moment)).await?;
        let ids: Vec<_> = rows.iter().map(|row| row.id).collect();
        assert_eq!(
            ids,
            expected.into_iter().collect::<Vec<_>>(),
            "as_of={moment}"
        );
    }

    // Teardown: facts/entities/… all use ON DELETE CASCADE
    let gone = sqlx::query(
        "DELETE FROM organizations WHERE id = (
             SELECT w.org_id FROM workspaces w
             JOIN knowledge_bases k ON k.workspace_id = w.id WHERE k.id = $1)",
    )
    .bind(f.kb)
    .execute(&pool)
    .await?;
    assert_eq!(gone.rows_affected(), 1, "一次性 org 没删掉");
    Ok(())
}
