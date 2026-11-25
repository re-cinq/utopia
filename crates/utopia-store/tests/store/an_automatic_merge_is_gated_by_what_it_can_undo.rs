//! Execution gate (0027): whether an automatic merge can take effect depends on what it can undo. Run against a real database.
//!
//! Three kinds of thing that leave the graph, each built as a minimal example and each undone once, to prove the gate looks at "now":
//! - **Contradiction**: both sides carry a fact on the same functional predicate pointing at different entities — merging would make one
//!   `functional` violation. Facts pointing at the same entity do not count; voided facts do not count
//! - **Derivation**: a still-holding derived fact in which either side takes part — merging rewrites premises, so the derivation would be rewritten
//! - **Answer**: either side has been confirmed in a conversation — it is something a person is asking about
//!
//! A pair that touches none of these is not blocked by the gate: if confident enough, it still merges automatically as before

use sqlx::PgPool;
use utopia_store::execution_gate::{hold, impact_of, Hold};
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    user: Uuid,
    ceo_of: Uuid,
    part_of: Uuid,
    a: Uuid,
    b: Uuid,
    c1: Uuid,
    c2: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (user, company) = (Uuid::now_v7(), Uuid::now_v7());
    let (ceo_of, part_of) = (Uuid::now_v7(), Uuid::now_v7());
    let (a, b, c1, c2) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'gate-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'gate-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'gate-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO users (id, org_id, email, display_name, password_hash)
         VALUES ($1, $2, $3, 'Asker', 'x')",
    )
    .bind(user)
    .bind(org)
    .bind(format!("asker-{}@test.local", user.simple()))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'company', 'Company')",
    )
    .bind(company)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, functional)
         VALUES ($1, $2, 'ceo_of', 'CEO of', TRUE)",
    )
    .bind(ceo_of)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'part_of', 'part of')",
    )
    .bind(part_of)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [(a, "Acme"), (b, "Acme Corp"), (c1, "Alice"), (c2, "Bob")] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(company)
        .bind(name)
        .execute(pool)
        .await?;
    }
    Ok(Fixture {
        org,
        kb,
        user,
        ceo_of,
        part_of,
        a,
        b,
        c1,
        c2,
    })
}

async fn fact(pool: &PgPool, kb: Uuid, s: Uuid, p: Uuid, o: Uuid) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, attested_from)
         VALUES ($1, $2, $3, $4, $5, NULL)",
    )
    .bind(id)
    .bind(kb)
    .bind(s)
    .bind(p)
    .bind(o)
    .execute(pool)
    .await?;
    Ok(id)
}

#[tokio::test]
async fn a_merge_that_would_leave_the_graph_is_held() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    // Nothing at all: not blocked
    let none = impact_of(&pool, f.kb, f.a, f.b).await?;
    assert_eq!(
        hold(&none),
        None,
        "a pair with both sides empty should not be blocked by the gate: {none:?}"
    );

    // Contradiction: a's CEO is Alice, b's CEO is Bob. Merged, one entity would have two CEOs
    fact(&pool, f.kb, f.a, f.ceo_of, f.c1).await?;
    let clash = fact(&pool, f.kb, f.b, f.ceo_of, f.c2).await?;
    let i = impact_of(&pool, f.kb, f.a, f.b).await?;
    assert_eq!(i.contradictions, vec!["CEO of".to_string()]);
    assert_eq!(hold(&i), Some(Hold::Contradiction("CEO of".into())));
    // The reverse question too
    let i = impact_of(&pool, f.kb, f.b, f.a).await?;
    assert_eq!(hold(&i), Some(Hold::Contradiction("CEO of".into())));

    // Undo b's fact: the contradiction is gone. Then give b one pointing at the same Alice: the same value is not a contradiction
    sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
        .bind(clash)
        .execute(&pool)
        .await?;
    fact(&pool, f.kb, f.b, f.ceo_of, f.c1).await?;
    let i = impact_of(&pool, f.kb, f.a, f.b).await?;
    assert!(
        i.contradictions.is_empty(),
        "voided ones and same-value ones do not count: {i:?}"
    );
    assert_eq!(hold(&i), None);

    // Derivation: a still-holding derivation that was derived from a
    let rule = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO rules (id, kb_id, predicate_id, kind) VALUES ($1, $2, $3, 'transitive')",
    )
    .bind(rule)
    .bind(f.kb)
    .bind(f.part_of)
    .execute(&pool)
    .await?;
    let derived = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_id, rule_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(derived)
    .bind(f.kb)
    .bind(f.a)
    .bind(f.part_of)
    .bind(f.c2)
    .bind(rule)
    .execute(&pool)
    .await?;
    let i = impact_of(&pool, f.kb, f.a, f.b).await?;
    assert_eq!(i.derived, 1);
    assert_eq!(hold(&i), Some(Hold::Derived(1)));
    sqlx::query("UPDATE derived_facts SET invalidated_at = now() WHERE id = $1")
        .bind(derived)
        .execute(&pool)
        .await?;
    assert_eq!(
        hold(&impact_of(&pool, f.kb, f.a, f.b).await?),
        None,
        "推翻了的派生不算"
    );

    // Answer: b was confirmed in one round of an answer. Neither a human's round nor a round that confirmed someone else counts
    let conv = Uuid::now_v7();
    sqlx::query("INSERT INTO conversations (id, kb_id, user_id) VALUES ($1, $2, $3)")
        .bind(conv)
        .bind(f.kb)
        .bind(f.user)
        .execute(&pool)
        .await?;
    for (role, who) in [("user", f.b), ("assistant", f.c2), ("assistant", f.b)] {
        sqlx::query(
            "INSERT INTO conversation_messages (id, conversation_id, role, content, resolved)
             VALUES ($1, $2, $3, 'hi', $4)",
        )
        .bind(Uuid::now_v7())
        .bind(conv)
        .bind(role)
        .bind(serde_json::json!([{ "id": who.to_string(), "name": "x", "type": null }]))
        .execute(&pool)
        .await?;
    }
    let i = impact_of(&pool, f.kb, f.a, f.b).await?;
    assert_eq!(
        i.answered, 1,
        "only count the answers that confirmed a or b: {i:?}"
    );
    assert_eq!(hold(&i), Some(Hold::Answered(1)));
    assert_eq!(hold(&i).unwrap().to_string(), "answered 1");

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(f.user)
        .execute(&pool)
        .await?;
    let gone = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    assert_eq!(gone.rows_affected(), 1, "one-off org was not deleted");
    Ok(())
}

/// One value succeeds another: two names each hang one, and together they form a successive timeline, not a contradiction.
/// Only two values starting on the same day cannot be ordered (the Blackbaud headquarters lease: the landlord was first HPBB1, then BBHQ1,
/// with "Lease Agreement" and "Lease Agreement dated May 16, 2016" each hanging one; before, the gate left that pair to a person as a contradiction,
/// so the same lease stayed two entities)
#[tokio::test]
async fn a_value_that_took_over_from_another_is_not_a_contradiction() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    let dated = |s: Uuid, o: Uuid, from: &'static str| {
        let pool = pool.clone();
        let (kb, p) = (f.kb, f.ceo_of);
        async move {
            sqlx::query(
                "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                                    valid_from, valid_from_precision)
                 VALUES ($1, $2, $3, $4, $5, $6::timestamptz, 'day')",
            )
            .bind(Uuid::now_v7())
            .bind(kb)
            .bind(s)
            .bind(p)
            .bind(o)
            .bind(format!("{from}T00:00:00Z"))
            .execute(&pool)
            .await?;
            anyhow::Ok(())
        }
    };
    dated(f.a, f.c1, "2019-01-01").await?;
    dated(f.b, f.c2, "2021-01-01").await?;
    let succession = impact_of(&pool, f.kb, f.a, f.b).await?;

    let g = seed(&pool).await?;
    let same_day = |s: Uuid, o: Uuid| {
        let pool = pool.clone();
        let (kb, p) = (g.kb, g.ceo_of);
        async move {
            sqlx::query(
                "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                                    valid_from, valid_from_precision)
                 VALUES ($1, $2, $3, $4, $5, '2020-01-01T00:00:00Z', 'day')",
            )
            .bind(Uuid::now_v7())
            .bind(kb)
            .bind(s)
            .bind(p)
            .bind(o)
            .execute(&pool)
            .await?;
            anyhow::Ok(())
        }
    };
    same_day(g.a, g.c1).await?;
    same_day(g.b, g.c2).await?;
    let clash = impact_of(&pool, g.kb, g.a, g.b).await?;

    for fx in [&f, &g] {
        sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
            .bind(fx.kb)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(fx.user)
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(fx.org)
            .execute(&pool)
            .await?;
    }
    assert_eq!(
        hold(&succession),
        None,
        "successive takeovers are not a contradiction: {succession:?}"
    );
    assert_eq!(hold(&clash), Some(Hold::Contradiction("CEO of".into())));
    Ok(())
}

#[tokio::test]
async fn a_clash_one_side_already_had_is_not_the_merges() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    // b itself already hangs two undated values; a hangs only one of them
    fact(&pool, f.kb, f.b, f.ceo_of, f.c1).await?;
    fact(&pool, f.kb, f.b, f.ceo_of, f.c2).await?;
    fact(&pool, f.kb, f.a, f.ceo_of, f.c1).await?;
    let impact = impact_of(&pool, f.kb, f.a, f.b).await?;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(f.user)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    assert!(
        impact.contradictions.is_empty(),
        "the clash was on b before the merge: {impact:?}"
    );
    Ok(())
}
