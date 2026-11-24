//! Seeds and verdicts shared by the tests of `resolve_identity_name`: a kb to hold the
//! entities, the entities themselves, and what became of the mention. Skipped without
//! `UTOPIA_DATABASE_URL`.

use sqlx::PgPool;
use std::sync::LazyLock;
use utopia_store::names::record;
use utopia_store::resolution::{resolve_identity_name, Resolution};
use uuid::Uuid;

/// The shared database keeps what earlier runs left, so names carry this run's mark.
static RUN: LazyLock<String> = LazyLock::new(|| Uuid::now_v7().simple().to_string());

pub fn name(stem: &str) -> String {
    format!("acme/{stem}-{}.ts", *RUN)
}

/// A kb with a project and an organization class, a body to run in it, then teardown.
pub async fn in_kb<F, Fut>(body: F) -> anyhow::Result<()>
where
    F: FnOnce(PgPool, Uuid, Uuid, Uuid) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let ids: [Uuid; 5] = std::array::from_fn(|_| Uuid::now_v7());
    let [org, ws, kb, project, organization] = ids;
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name) VALUES ('{org}', 'skips');
         INSERT INTO workspaces (id, org_id, name) VALUES ('{ws}', '{org}', 'skips');
         INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ('{kb}', '{ws}', 'skips');
         INSERT INTO entity_types (id, kb_id, key, label) VALUES
             ('{project}', '{kb}', 'project', 'Project'),
             ('{organization}', '{kb}', 'organization', 'Organization');"
    ))
    .execute(&pool)
    .await?;
    let run = body(pool.clone(), kb, project, organization).await;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}

/// A live entity whose canonical name is `canonical`, with no name fact.
pub async fn entity(
    pool: &PgPool,
    kb: Uuid,
    type_id: Option<Uuid>,
    canonical: &str,
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entities (id, kb_id, type_id, canonical_name, profile_embedding, profile_n)
         VALUES ($1, $2, $3, $4, '[1,0,0]'::vector, 1)",
    )
    .bind(id)
    .bind(kb)
    .bind(type_id)
    .bind(canonical)
    .execute(pool)
    .await?;
    Ok(id)
}

/// A live entity named `name`, by its own name and by the name fact that goes with it.
pub async fn holder(
    pool: &PgPool,
    kb: Uuid,
    type_id: Option<Uuid>,
    name: &str,
) -> anyhow::Result<Uuid> {
    let id = entity(pool, kb, type_id, name).await?;
    record(pool, kb, id, name, None, None).await?;
    Ok(id)
}

pub async fn merge_into_another(pool: &PgPool, kb: Uuid, entity: Uuid) -> anyhow::Result<()> {
    let survivor = holder(pool, kb, None, &name("survivor")).await?;
    sqlx::query("UPDATE entities SET merged_into = $2 WHERE id = $1")
        .bind(entity)
        .bind(survivor)
        .execute(pool)
        .await?;
    Ok(())
}

/// A second base in the same workspace, holding nothing yet.
pub async fn other_kb(pool: &PgPool, kb: Uuid) -> anyhow::Result<Uuid> {
    let other = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name)
         SELECT $2, workspace_id, 'elsewhere' FROM knowledge_bases WHERE id = $1",
    )
    .bind(kb)
    .bind(other)
    .execute(pool)
    .await?;
    Ok(other)
}

pub async fn resolve(
    pool: &PgPool,
    kb: Uuid,
    type_id: Option<Uuid>,
    name: &str,
    exclude: &[Uuid],
) -> anyhow::Result<Resolution> {
    Ok(resolve_identity_name(pool, kb, type_id, name, None, None, exclude).await?)
}

/// Whether the mention was attached to `holder` rather than given an entity of its own.
pub fn attached_to(resolution: &Resolution, holder: Uuid) -> bool {
    !resolution.created && resolution.entity_id == holder
}

/// Whether the mention got an entity of its own instead of `holder`.
pub fn created_apart_from(resolution: &Resolution, holder: Uuid) -> bool {
    resolution.created && resolution.entity_id != holder
}
