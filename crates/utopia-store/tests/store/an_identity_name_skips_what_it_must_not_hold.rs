//! 0071 decision 2, by name: only a live, undescribed holder of the declared class (or no class)
//! in this kb, named by its own name, is attached to. Skipped without `UTOPIA_DATABASE_URL`.

use crate::identity_name_seed::*;
use utopia_store::resolution::resolve_identity_name;

#[tokio::test]
async fn a_merged_entity_is_not_attached_by_its_canonical_name() -> anyhow::Result<()> {
    in_kb(|pool, kb, project, _| async move {
        let merged = holder(&pool, kb, Some(project), &name("a")).await?;
        merge_into_another(&pool, kb, merged).await?;
        let resolution = resolve(&pool, kb, Some(project), &name("a"), &[]).await?;
        assert!(created_apart_from(&resolution, merged));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn a_described_entity_is_not_attached_by_its_name() -> anyhow::Result<()> {
    in_kb(|pool, kb, project, _| async move {
        let described = holder(&pool, kb, Some(project), &name("c")).await?;
        sqlx::query("UPDATE entities SET description = 'the c file' WHERE id = $1")
            .bind(described)
            .execute(&pool)
            .await?;
        let resolution = resolve(&pool, kb, Some(project), &name("c"), &[]).await?;
        assert!(created_apart_from(&resolution, described));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_entity_of_another_class_is_not_attached_by_its_name() -> anyhow::Result<()> {
    in_kb(|pool, kb, project, organization| async move {
        let other = holder(&pool, kb, Some(organization), &name("d")).await?;
        let resolution = resolve(&pool, kb, Some(project), &name("d"), &[]).await?;
        assert!(!attached_to(&resolution, other));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn a_classed_entity_is_not_attached_by_an_untyped_mention() -> anyhow::Result<()> {
    in_kb(|pool, kb, project, _| async move {
        let typed = holder(&pool, kb, Some(project), &name("e")).await?;
        let resolution = resolve(&pool, kb, None, &name("e"), &[]).await?;
        assert!(!attached_to(&resolution, typed));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_entity_of_another_base_is_not_attached_by_its_canonical_name() -> anyhow::Result<()> {
    in_kb(|pool, kb, project, _| async move {
        let elsewhere = other_kb(&pool, kb).await?;
        let foreign = holder(&pool, elsewhere, None, &name("f")).await?;
        let resolution = resolve(&pool, kb, Some(project), &name("f"), &[]).await?;
        assert!(created_apart_from(&resolution, foreign));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_entity_named_both_by_canonical_name_and_alias_is_one_holder() -> anyhow::Result<()> {
    in_kb(|pool, kb, project, _| async move {
        let both = holder(&pool, kb, Some(project), &name("i")).await?;
        let unrelated = [0.0, 1.0, 0.0];
        let resolution = resolve_identity_name(
            &pool,
            kb,
            Some(project),
            &name("i"),
            Some(&unrelated),
            None,
            &[],
        )
        .await?;
        assert!(attached_to(&resolution, both));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_excluded_entity_is_not_attached_by_its_name() -> anyhow::Result<()> {
    in_kb(|pool, kb, project, _| async move {
        let excluded = holder(&pool, kb, Some(project), &name("j")).await?;
        let resolution = resolve(&pool, kb, Some(project), &name("j"), &[excluded]).await?;
        assert!(created_apart_from(&resolution, excluded));
        Ok(())
    })
    .await
}
