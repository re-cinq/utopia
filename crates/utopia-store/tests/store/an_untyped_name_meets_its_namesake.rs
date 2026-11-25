//! Entities with no class must also meet their namesakes (measured after the first slice of 0044).
//!
//! Entities in the open graph all have no class: the class is set by alignment, and extraction only records the document's characters. The candidate
//! query in `resolve_mention` used to be written `e.type_id = $2`, and when NULL was passed that condition never held — so in one store
//! "Securities and Exchange Commission" and its all-caps spelling became two entities, and only exact same-name matching at entity creation caught it.
//! Here what is guarded: two mentions with no class, whose spellings differ only in case, land on the same entity.
//!
//! Skipped when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use sqlx::PgPool;
use uuid::Uuid;

#[tokio::test]
async fn an_untyped_mention_attaches_to_its_untyped_namesake() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'untyped-namesake-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'untyped-namesake-test')",
    )
    .bind(ws)
    .bind(org)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'untyped-namesake-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    let first = utopia_store::resolution::resolve_mention(
        &pool,
        kb,
        None,
        "Securities and Exchange Commission",
        None,
        None,
        None,
        &[],
    )
    .await?;
    assert!(first.created, "the first mention must create an entity");
    let again = utopia_store::resolution::resolve_mention(
        &pool,
        kb,
        None,
        "SECURITIES AND EXCHANGE COMMISSION",
        None,
        None,
        None,
        &[],
    )
    .await?;
    assert_eq!(
        again.entity_id, first.entity_id,
        "只差大小写的同名提及要落到同一个没有类的实体上"
    );
    assert!(!again.created);
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM entities WHERE kb_id = $1 AND merged_into IS NULL",
    )
    .bind(kb)
    .fetch_one(&pool)
    .await?;
    assert_eq!(n, 1);

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    Ok(())
}
