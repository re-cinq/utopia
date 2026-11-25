//! Derivations also rewind with the second clock (0019 / #549), run against a real database.
//!
//! 0019 wrote "`derived_facts` carries `derived_at` and `invalidated_at`, and derivations follow the same
//! rule". The graph overview does this, but the single-entity read does not: `derived_for_entity` hard-codes
//! `invalidated_at IS NULL`, so `as_of` on the March panel brings the assertion half back while the derivation half is still today's —
//! no premise exists, yet the conclusion hangs there, stitching two moments into one snapshot.
//! MCP's `entity_facts` and the web entity panel both go through this one function.
//!
//! Both directions are pinned, because each breaks on its own:
//! - **The moment before the derivation**: the conclusion should not appear (the half that #549 reported)
//! - **The moment before the retraction**: the conclusion should still appear — the replayed graph keeps the edges derived at that time (the half 0019 promised)

use sqlx::PgPool;
use utopia_store::reasoning;
use uuid::Uuid;

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

struct Fixture {
    org: Uuid,
    kb: Uuid,
    atlas: Uuid,
    systems: Uuid,
    derived: Uuid,
}

/// Two assertions recorded on March 10: Atlas ⊂ Acme Robotics ⊂ Acme Systems.
/// On March 11 the engine derives Atlas ⊂ Acme Systems from transitivity. The timestamp is written straight into the row,
/// not via the materializer — what is tested here is reading, not deriving.
async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let etype = Uuid::now_v7();
    let part_of = Uuid::now_v7();
    let (atlas, robotics, systems) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (premise_a, premise_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (rule, derived) = (Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'derived-rewind-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'derived-rewind-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name)
         VALUES ($1, $2, 'derived-rewind-test')",
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
        "INSERT INTO relation_types (id, kb_id, key, label, is_transitive)
         VALUES ($1, $2, 'part_of', 'part of', TRUE)",
    )
    .bind(part_of)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [
        (atlas, "Atlas"),
        (robotics, "Acme Robotics"),
        (systems, "Acme Systems"),
    ] {
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
    for (id, s, o) in [(premise_a, atlas, robotics), (premise_b, robotics, systems)] {
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, recorded_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(kb)
        .bind(s)
        .bind(part_of)
        .bind(o)
        .bind(t("2026-03-10T00:00:00Z"))
        .execute(pool)
        .await?;
    }
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
                                    derived_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(derived)
    .bind(kb)
    .bind(atlas)
    .bind(part_of)
    .bind(systems)
    .bind(rule)
    .bind(t("2026-03-11T00:00:00Z"))
    .execute(pool)
    .await?;
    for (seq, premise) in [(0, premise_a), (1, premise_b)] {
        sqlx::query(
            "INSERT INTO fact_derivations (derived_fact_id, premise_fact_id, seq)
             VALUES ($1, $2, $3)",
        )
        .bind(derived)
        .bind(premise)
        .bind(seq)
        .execute(pool)
        .await?;
    }
    Ok(Fixture {
        org,
        kb,
        atlas,
        systems,
        derived,
    })
}

#[tokio::test]
async fn a_derivation_is_absent_before_it_was_drawn_and_kept_until_it_was_withdrawn(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let derived_of = |entity: Uuid, as_of: Option<&'static str>| {
        let pool = pool.clone();
        async move {
            let rows =
                reasoning::derived_for_entity(&pool, f.kb, entity, None, as_of.map(t)).await?;
            Ok::<Vec<String>, anyhow::Error>(rows.into_iter().map(|d| d.object).collect())
        }
    };

    let run = async {
        // ---- 1. Now: the conclusion is there, and it is that one
        assert_eq!(
            derived_of(f.atlas, None).await?,
            vec!["Acme Systems".to_string()],
            "现在：引擎推出的结论要在"
        );

        // ---- 2. March 1: the premises were not yet recorded, so the conclusion should not be there either — this is the half #549 reported.
        // The assertion half is also empty at the same moment: the two halves must name the same moment
        let (_, facts) = utopia_store::graph::entity_detail(
            &pool,
            f.kb,
            f.atlas,
            None,
            Some(t("2026-03-01T00:00:00Z")),
        )
        .await?;
        assert!(facts.is_empty(), "三月一日：两条前提都还没记下");
        assert!(
            derived_of(f.atlas, Some("2026-03-01T00:00:00Z"))
                .await?
                .is_empty(),
            "推出之前的时刻不该有这条结论：它的前提一条都不在"
        );
        assert!(
            derived_of(f.systems, Some("2026-03-01T00:00:00Z"))
                .await?
                .is_empty(),
            "从宾语那一侧看回去也一样"
        );

        // A day when the premises were recorded but the conclusion not yet derived: the assertion is there, the derivation is not
        let (_, facts) = utopia_store::graph::entity_detail(
            &pool,
            f.kb,
            f.atlas,
            None,
            Some(t("2026-03-10T12:00:00Z")),
        )
        .await?;
        assert_eq!(facts.len(), 1, "三月十日中午：Atlas 自己的那条前提已经记下");
        assert!(
            derived_of(f.atlas, Some("2026-03-10T12:00:00Z"))
                .await?
                .is_empty(),
            "推出是次日的事"
        );

        // ---- 3. April 1 retraction. At moments between derivation and retraction the conclusion must still be there — the half 0019 promised:
        // the replayed graph keeps the edges **as derived at the time**, not the conclusions of today's rules
        sqlx::query("UPDATE derived_facts SET invalidated_at = $2 WHERE id = $1")
            .bind(f.derived)
            .bind(t("2026-04-01T00:00:00Z"))
            .execute(&pool)
            .await?;
        assert_eq!(
            derived_of(f.atlas, Some("2026-03-20T00:00:00Z")).await?,
            vec!["Acme Systems".to_string()],
            "推翻之前的时刻，当时推出的边要留在回放的图上"
        );
        assert!(
            derived_of(f.atlas, None).await?.is_empty(),
            "现在：已推翻的结论不再出现"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
