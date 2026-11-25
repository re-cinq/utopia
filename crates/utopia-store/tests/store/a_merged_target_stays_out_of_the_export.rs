//! Being in the same library is not the same as being in the export set: `entities_page` filters out rows where `merged_into`
//! is non-empty, but a merge only rewrites a fact's subject and object -- an entity value in an attribute, and a derived
//! subject or object, can still point at a merged-away row. Serializing it with the IRI minted for it leaves a dangling edge.
//!
//! The rule: **same library but not in the export set** also counts as out of bounds -- the same treatment as crossing libraries: refuse the whole export. Do not
//! rewrite the entities that the pointers left behind (that is a different semantic action), and do not silently omit them.

use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    survivor: Uuid,
    merged: Uuid,
    rel: Uuid,
    attr: Uuid,
    rule: Uuid,
    fact: Uuid,
}

/// One library, an entity kept and an entity merged away, one fact, and one
/// derivation pointing at the merged entity -- merged is a legal state (no trigger blocks it); what must be blocked is the export side
async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'merged-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'merged-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'merged-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;

    let (survivor, merged) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'survivor')")
        .bind(survivor)
        .bind(kb)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO entities (id, kb_id, canonical_name, merged_into) VALUES ($1, $2, 'gone', $3)",
    )
    .bind(merged)
    .bind(kb)
    .bind(survivor)
    .execute(pool)
    .await?;

    let (rel, attr) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'knows', 'knows')",
    )
    .bind(rel)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind) VALUES ($1, $2, 'since', 'since', 'attribute')",
    )
    .bind(attr)
    .bind(kb)
    .execute(pool)
    .await?;

    let fact = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, $3, 0.9)",
    )
    .bind(fact)
    .bind(kb)
    .bind(survivor)
    .bind(rel)
    .execute(pool)
    .await?;

    let rule = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO rules (id, kb_id, predicate_id, kind) VALUES ($1, $2, $3, 'transitive')",
    )
    .bind(rule)
    .bind(kb)
    .bind(rel)
    .execute(pool)
    .await?;

    Ok(Fixture {
        org,
        kb,
        survivor,
        merged,
        rel,
        attr,
        rule,
        fact,
    })
}

async fn cleanup(pool: &PgPool, f: &Fixture) -> anyhow::Result<()> {
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

async fn insert_derived(
    pool: &PgPool,
    f: &Fixture,
    subject: Uuid,
    object: Uuid,
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_id, rule_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(subject)
    .bind(f.rel)
    .bind(object)
    .bind(f.rule)
    .execute(pool)
    .await?;
    Ok(id)
}

/// A merged entity does not enter the export set -- it does not appear on the entities page
#[tokio::test]
async fn a_merged_entity_is_not_emitted() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    let page = utopia_store::export::entities_page(&mut pool.begin().await?, f.kb, None).await?;
    assert!(page.iter().any(|e| e.id == f.survivor));
    assert!(
        !page.iter().any(|e| e.id == f.merged),
        "merged 实体必须不在导出集里"
    );

    cleanup(&pool, &f).await
}

/// An attribute value pointing at a merged entity: same library but absent -- the health check and the fact page both must block it
#[tokio::test]
async fn a_qualifier_on_a_merged_entity_fails_closed() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    sqlx::query(
        "INSERT INTO fact_qualifiers (fact_id, qualifier_type_id, entity_id)
         VALUES ($1, $2, $3)",
    )
    .bind(f.fact)
    .bind(f.attr)
    .bind(f.merged)
    .execute(&pool)
    .await?;

    let err = utopia_store::export::provenance_integrity(&mut pool.begin().await?, f.kb).await;
    let msg = format!("{err:?}");
    assert!(
        err.is_err(),
        "health check for qualifier→merged must refuse the export"
    );
    assert!(
        msg.contains("qualifier.entity(merged)"),
        "要报 qualifier.entity(merged): {msg}"
    );
    assert!(
        utopia_store::export::facts_page(&mut pool.begin().await?, f.kb, None)
            .await
            .is_err(),
        "事实页也要拦下同一条边"
    );

    sqlx::query("DELETE FROM fact_qualifiers WHERE fact_id = $1")
        .bind(f.fact)
        .execute(&pool)
        .await?;
    cleanup(&pool, &f).await
}

/// A derived subject or object pointing at a merged entity: same library but absent
#[tokio::test]
async fn a_derived_on_a_merged_entity_fails_closed() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    let d_subj = insert_derived(&pool, &f, f.merged, f.survivor).await?;
    let err = utopia_store::export::provenance_integrity(&mut pool.begin().await?, f.kb).await;
    let msg = format!("{err:?}");
    assert!(
        err.is_err(),
        "health check for derived.subject→merged must refuse the export"
    );
    assert!(
        msg.contains("derived.subject(merged)"),
        "要报 derived.subject(merged): {msg}"
    );
    sqlx::query("DELETE FROM derived_facts WHERE id = $1")
        .bind(d_subj)
        .execute(&pool)
        .await?;

    let d_obj = insert_derived(&pool, &f, f.survivor, f.merged).await?;
    let err = utopia_store::export::provenance_integrity(&mut pool.begin().await?, f.kb).await;
    let msg = format!("{err:?}");
    assert!(
        err.is_err(),
        "health check for derived.object→merged must refuse the export"
    );
    assert!(
        msg.contains("derived.object(merged)"),
        "要报 derived.object(merged): {msg}"
    );
    sqlx::query("DELETE FROM derived_facts WHERE id = $1")
        .bind(d_obj)
        .execute(&pool)
        .await?;

    // Clean after this -- what is refused is the absent reference, not the library itself
    utopia_store::export::provenance_integrity(&mut pool.begin().await?, f.kb).await?;
    cleanup(&pool, &f).await
}

/// A fact's subject or object pointing at a merged entity (an old write that the merge did not reach, or a write that bypassed the store):
/// both the fact page and the health check must block it
#[tokio::test]
async fn a_fact_on_a_merged_entity_fails_closed() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    let bad_fact = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, $5, 0.9)",
    )
    .bind(bad_fact)
    .bind(f.kb)
    .bind(f.survivor)
    .bind(f.rel)
    .bind(f.merged)
    .execute(&pool)
    .await?;

    let err = utopia_store::export::provenance_integrity(&mut pool.begin().await?, f.kb).await;
    let msg = format!("{err:?}");
    assert!(
        err.is_err(),
        "health check for fact.object→merged must refuse the export"
    );
    assert!(
        msg.contains("fact.object(merged)"),
        "要报 fact.object(merged): {msg}"
    );
    assert!(
        utopia_store::export::facts_page(&mut pool.begin().await?, f.kb, None)
            .await
            .is_err(),
        "事实页也要拦下同一条边"
    );

    sqlx::query("DELETE FROM facts WHERE id = $1")
        .bind(bad_fact)
        .execute(&pool)
        .await?;
    utopia_store::export::provenance_integrity(&mut pool.begin().await?, f.kb).await?;
    cleanup(&pool, &f).await
}
