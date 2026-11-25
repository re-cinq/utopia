//! When the profile cannot tell two same-named people apart, facts can (#331, continuing #270/#329).
//!
//! One database holds two "Zhang Wei"s: one in platform engineering, one in finance. The profile vector cannot tell which is which
//! — since #329 such a pairing creates entities and enters two manual review pairs, and the person on the review card looks at exactly those two
//! `works_for` facts. **The code could have looked for itself.** This file pins that step.
//!
//! Evidence only flows one way: agreement is weak evidence of "the same person", while disagreement is **nothing at all**. A person who works at Acme
//! and part-time at Zenith has two `works_for` facts, yet is still one person; two different people may also
//! belong to the same company. So this pins both "a hit settles it" and "no hit changes nothing":
//! the latter is the gate that stops weak evidence being treated as strong.
//!
//! Only testable against a database: whether there is a hit depends on which objects a SQL query with window functions and NOT EXISTS picks out,
//! which `cargo check` cannot see at all. Without `UTOPIA_DATABASE_URL` it skips rather than fails.

use sqlx::PgPool;
use uuid::Uuid;

struct Fx {
    kb: Uuid,
    person: Uuid,
    zhang_a: Uuid,
    zhang_b: Uuid,
}

/// Two same-named Zhang Weis. When `shared` is true, they also get a **shared** employer —
/// that fact is no help in telling them apart, yet it is the easiest one to collide with in documents.
async fn seed(pool: &PgPool, shared: bool) -> anyhow::Result<Fx> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (person, organization) = (Uuid::now_v7(), Uuid::now_v7());
    let works_for = Uuid::now_v7();
    let (platform, finance, both) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (zhang_a, zhang_b) = (Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'corroboration-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'corroboration-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name)
         VALUES ($1, $2, 'corroboration-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, key, label) in [
        (person, "person", "Person"),
        (organization, "organization", "Organization"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, temporal)
         VALUES ($1, $2, 'works_for', 'works for', 'state')",
    )
    .bind(works_for)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, type_id, name) in [
        (platform, organization, "Platform Engineering"),
        (finance, organization, "Finance"),
        (both, organization, "Nebula Holdings"),
        (zhang_a, person, "Zhang Wei"),
        (zhang_b, person, "Zhang Wei"),
    ] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(type_id)
        .bind(name)
        .execute(pool)
        .await?;
    }
    // Both people's profiles are **identical**: seeded from the same chunk (the norm since #221).
    // The vectors are doomed to fail to tell them apart in this scenario
    for (id, v) in [(zhang_a, "[1,0,0]"), (zhang_b, "[1,0,0]")] {
        sqlx::query(
            "UPDATE entities SET profile_embedding = $2::vector, profile_n = 1 WHERE id = $1",
        )
        .bind(id)
        .bind(v)
        .execute(pool)
        .await?;
    }
    let mut pairs = vec![(zhang_a, platform), (zhang_b, finance)];
    if shared {
        pairs.push((zhang_a, both));
        pairs.push((zhang_b, both));
    }
    for (subject, object) in pairs {
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
             VALUES ($1, $2, $3, $4, $5, 0.9)",
        )
        .bind(Uuid::now_v7())
        .bind(kb)
        .bind(subject)
        .bind(works_for)
        .bind(object)
        .execute(pool)
        .await?;
    }
    Ok(Fx {
        kb,
        person,
        zhang_a,
        zhang_b,
    })
}

async fn drop_kb(pool: &PgPool, kb: Uuid) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(pool)
        .await?;
    Ok(())
}

/// Tied + the source text pointed at one person's department → settled, no more coin flip and no bothering the person.
#[tokio::test]
async fn a_fact_in_the_text_settles_a_namesake_tie() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, false).await?;

    let run = async {
        let ctx: Vec<f32> = vec![1.0, 0.0, 0.0];
        let r = utopia_store::resolution::resolve_mention(
            &pool,
            f.kb,
            Some(f.person),
            "Zhang Wei",
            Some(&ctx),
            None,
            Some("Zhang Wei of Finance signed off on the quarterly report."),
            &[],
        )
        .await?;
        assert!(!r.created, "原文点名了财务，不该再新建第三个张伟");
        assert_eq!(r.entity_id, f.zhang_b, "该归到财务部那个张伟");
        assert!(
            r.reviews.is_empty(),
            "事实已经把人分开了，不该再入人工审核对"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    drop_kb(&pool, f.kb).await?;
    run
}

/// The source text mentions both departments → this clue cannot tell them apart either, so keep the #329 behaviour.
#[tokio::test]
async fn a_clue_that_points_at_both_settles_nothing() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, false).await?;

    let run = async {
        let ctx: Vec<f32> = vec![1.0, 0.0, 0.0];
        let r = utopia_store::resolution::resolve_mention(
            &pool,
            f.kb,
            Some(f.person),
            "Zhang Wei",
            Some(&ctx),
            None,
            Some("Platform Engineering and Finance both sent a Zhang Wei to the review."),
            &[],
        )
        .await?;
        assert!(r.created, "两边都命中就等于没命中，仍该新建实体");
        assert_eq!(r.reviews.len(), 2, "仍该对两个候选各入一条人工审核对");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    drop_kb(&pool, f.kb).await?;
    run
}

/// **A shared employer is not a clue.** Both Zhang Weis are at Xingyun Holdings, and mentioning Xingyun Holdings says nothing:
/// this fact should be excluded in SQL, otherwise it is the easiest kind of false judgment to hit.
#[tokio::test]
async fn an_employer_they_share_is_not_a_clue() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, true).await?;

    let run = async {
        let ctx: Vec<f32> = vec![1.0, 0.0, 0.0];
        let r = utopia_store::resolution::resolve_mention(
            &pool,
            f.kb,
            Some(f.person),
            "Zhang Wei",
            Some(&ctx),
            None,
            Some("Zhang Wei has worked at Nebula Holdings for six years."),
            &[],
        )
        .await?;
        assert!(
            r.created,
            "两人共有的雇主没有分辨力，不该拿它把 mention 判给其中一个"
        );
        assert_eq!(r.reviews.len(), 2, "仍是分不开，照旧入两条人工审核对");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    drop_kb(&pool, f.kb).await?;
    run
}

/// Neither is mentioned → everything as before. **This one is the gate**: a disagreement (or no mention) must never be treated as
/// evidence of "not the same person", or part-time people would be split into two.
#[tokio::test]
async fn text_that_names_neither_changes_nothing() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, false).await?;

    let run = async {
        let ctx: Vec<f32> = vec![1.0, 0.0, 0.0];
        let with_text = utopia_store::resolution::resolve_mention(
            &pool,
            f.kb,
            Some(f.person),
            "Zhang Wei",
            Some(&ctx),
            None,
            // The one mentioned is a third company: agreeing evidence is absent, and "disagreement" does not count as evidence either
            Some("Zhang Wei moonlights at Zenith Robotics on weekends."),
            &[],
        )
        .await?;
        assert!(with_text.created, "没有命中就该保持 #329 的行为");
        assert_eq!(with_text.reviews.len(), 2, "两条人工审核对照旧");
        assert_ne!(with_text.entity_id, f.zhang_a);
        assert_ne!(with_text.entity_id, f.zhang_b);
        Ok::<_, anyhow::Error>(())
    }
    .await;

    drop_kb(&pool, f.kb).await?;
    run
}

/// The grey zone (scores in `SIM_NEW..SIM_ATTACH`) is likewise swayed by side evidence: the vector says "not similar enough",
/// yet the source text named them. No threshold was moved; what changed is that one more question is asked beyond the threshold.
#[tokio::test]
async fn the_grey_zone_listens_to_the_facts_too() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, false).await?;

    let run = async {
        // Let the two candidates be apart and both land in the grey zone: against A about 0.45, against B about 0.40,
        // a gap of 0.05 > SIM_TIE_MARGIN, so it takes the grey-zone path rather than the tie path
        sqlx::query("UPDATE entities SET profile_embedding = '[0,1,0]'::vector WHERE id = $1")
            .bind(f.zhang_b)
            .execute(&pool)
            .await?;
        let ctx: Vec<f32> = vec![0.45, 0.40, 0.80];
        let r = utopia_store::resolution::resolve_mention(
            &pool,
            f.kb,
            Some(f.person),
            "Zhang Wei",
            Some(&ctx),
            None,
            Some("The Finance lead, Zhang Wei, approved it."),
            &[],
        )
        .await?;
        assert!(!r.created, "灰区里原文点了名，就不该再新建一个同名实体");
        assert_eq!(r.entity_id, f.zhang_b, "该归到财务部那个");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    drop_kb(&pool, f.kb).await?;
    run
}
