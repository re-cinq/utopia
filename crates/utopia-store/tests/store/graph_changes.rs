//! The ledger derivation of `graph_changes`, run against a real database.
//!
//! Why a real database is required: this logic **lives entirely inside SQL strings**, and `cargo check` and clippy
//! cannot see a single character of it. This project has already tripped on it several times (the merge dedup missed object_value,
//! `UPDATE … RETURNING` returned the new value, and a CHECK constraint quietly rejected one kind),
//! each time discovered only at runtime.
//!
//! Without `UTOPIA_DATABASE_URL` it **skips rather than fails**: this is the first database test in this repo,
//! and it should not turn `cargo test` red for someone who has not started a database.
//!
//! Creates and tears down its own: builds a one-off org/workspace/KB, and after the run deletes the KB along with it (facts/entities
//! all ON DELETE CASCADE). Never touches an existing database.

use sqlx::PgPool;
use uuid::Uuid;

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

/// Builds a minimal ledger, returning (kb_id, subject entity, object entity).
///
/// Four facts cover all four event kinds, plus one event that **should not appear**:
/// - A written 03-10, voided 03-20, but B took it over → only asserted; the voiding is not recorded twice
/// - B written 03-20 with supersedes=A → corrected
/// - C written 03-12, voided 03-25 with no successor → asserted + rejected
/// - D written 03-13, voided 03-26 with no successor, with a merged adoption record → asserted + merged
async fn seed(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid, Uuid)> {
    let org = Uuid::now_v7();
    let ws = Uuid::now_v7();
    let kb = Uuid::now_v7();
    let etype = Uuid::now_v7();
    let pred = Uuid::now_v7();
    let (subj, obj, other) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (a, b, c, d) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'changes-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'changes-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'changes-test')",
    )
    .bind(kb)
    .bind(ws)
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
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'located_in', 'located in')",
    )
    .bind(pred)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [(subj, "Acme"), (obj, "Berlin"), (other, "Paris")] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(etype)
        .bind(name)
        .execute(pool)
        .await?;
    }

    let fact = |id: Uuid, o: Uuid, rec: &str, inv: Option<&str>, sup: Option<Uuid>| {
        let (rec, inv) = (t(rec), inv.map(t));
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                                recorded_at, invalidated_at, supersedes)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(kb)
        .bind(subj)
        .bind(pred)
        .bind(o)
        .bind(rec)
        .bind(inv)
        .bind(sup)
    };
    fact(
        a,
        obj,
        "2026-03-10T00:00:00Z",
        Some("2026-03-20T00:00:00Z"),
        None,
    )
    .execute(pool)
    .await?;
    fact(b, obj, "2026-03-20T00:00:00Z", None, Some(a))
        .execute(pool)
        .await?;
    fact(
        c,
        obj,
        "2026-03-12T00:00:00Z",
        Some("2026-03-25T00:00:00Z"),
        None,
    )
    .execute(pool)
    .await?;
    // D's object is changed to other: used to verify that the entity_id filter matches the object side
    fact(
        d,
        other,
        "2026-03-13T00:00:00Z",
        Some("2026-03-26T00:00:00Z"),
        None,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO fact_adoptions (batch_id, kb_id, predicate_id, old_fact_id, new_fact_id, mode)
         VALUES ($1, $2, $3, $4, $5, 'merged')",
    )
    .bind(Uuid::now_v7())
    .bind(kb)
    .bind(pred)
    .bind(d)
    .bind(b)
    .execute(pool)
    .await?;

    Ok((kb, subj, other))
}

/// Multiset of (kind, date), sorted so it can be compared
fn shape(rows: &[utopia_core::models::GraphChange]) -> Vec<String> {
    let mut v: Vec<String> = rows
        .iter()
        .map(|c| format!("{} {}", c.at.format("%m-%d"), c.kind))
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn ledger_events_are_derived_as_specified() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (kb, subj, other) = seed(&pool).await?;

    let run = |since: &'static str,
               until: &'static str,
               entity: Option<Uuid>,
               kinds: Option<Vec<String>>| {
        let pool = pool.clone();
        async move {
            utopia_store::graph::graph_changes(
                &pool,
                kb,
                t(since),
                t(until),
                entity,
                kinds.as_deref(),
                100,
                0,
            )
            .await
        }
    };

    // 1. Whole window: four facts produce six events, and A's death **does not appear** —
    //    it was already explained by B's corrected; recording it again would be saying the same thing twice
    let all = run("2026-03-01T00:00:00Z", "2026-04-01T00:00:00Z", None, None).await?;
    assert_eq!(
        shape(&all),
        vec![
            "03-10 asserted",
            "03-12 asserted",
            "03-13 asserted",
            "03-20 corrected",
            "03-25 rejected",
            "03-26 merged",
        ],
        "四种事件各就各位，且 03-20 没有 rejected"
    );

    // 2. Each of the two branches opens its window on **its own time column**: cut at 03-15, writes get in, voidings do not
    let early = run("2026-03-01T00:00:00Z", "2026-03-15T00:00:00Z", None, None).await?;
    assert_eq!(
        shape(&early),
        vec!["03-10 asserted", "03-12 asserted", "03-13 asserted"]
    );

    // 3. kinds filter (text[] binding)
    let only = run(
        "2026-03-01T00:00:00Z",
        "2026-04-01T00:00:00Z",
        None,
        Some(vec!["rejected".into(), "merged".into()]),
    )
    .await?;
    assert_eq!(shape(&only), vec!["03-25 rejected", "03-26 merged"]);

    // 4. entity_id filter (uuid binding) matches the object side: other is the object only on D,
    //    yet it must pull out D's two events
    let by_object = run(
        "2026-03-01T00:00:00Z",
        "2026-04-01T00:00:00Z",
        Some(other),
        None,
    )
    .await?;
    assert_eq!(shape(&by_object), vec!["03-13 asserted", "03-26 merged"]);

    // 5. The subject side matches all six
    let by_subject = run(
        "2026-03-01T00:00:00Z",
        "2026-04-01T00:00:00Z",
        Some(subj),
        None,
    )
    .await?;
    assert_eq!(by_subject.len(), 6);

    // 6. Subject, object and predicate are all spelled out, not a pile of uuids
    let sample = all.iter().find(|c| c.kind == "corrected").unwrap();
    assert_eq!(sample.subject_name, "Acme");
    assert_eq!(sample.predicate_label.as_deref(), Some("located in"));
    assert_eq!(sample.object_name.as_deref(), Some("Berlin"));

    // Teardown: facts/entities/… are all ON DELETE CASCADE
    let gone = sqlx::query(
        "DELETE FROM organizations WHERE id = (
             SELECT w.org_id FROM workspaces w
             JOIN knowledge_bases k ON k.workspace_id = w.id WHERE k.id = $1)",
    )
    .bind(kb)
    .execute(&pool)
    .await?;
    assert_eq!(gone.rows_affected(), 1, "一次性 org 没删掉");
    Ok(())
}
