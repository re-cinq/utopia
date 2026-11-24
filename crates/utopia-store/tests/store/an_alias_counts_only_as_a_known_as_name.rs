//! 0071 decision 2, by alias: an entity is a holder through a current builtin `known_as` fact of
//! its own base, and through no other attribute. Skipped without `UTOPIA_DATABASE_URL`.

use crate::identity_name_seed::*;
use uuid::Uuid;

#[tokio::test]
async fn a_merged_entity_is_not_attached_by_its_known_as_alias() -> anyhow::Result<()> {
    in_kb(|pool, kb, project, _| async move {
        let merged = known_as(&pool, kb, Some(project), &name("b")).await?;
        merge_into_another(&pool, kb, merged).await?;
        let resolution = resolve(&pool, kb, Some(project), &name("b"), &[]).await?;
        assert!(created_apart_from(&resolution, merged));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_entity_of_another_base_is_not_attached_by_its_known_as_alias() -> anyhow::Result<()> {
    in_kb(|pool, kb, project, _| async move {
        let elsewhere = other_kb(&pool, kb).await?;
        let foreign = known_as(&pool, elsewhere, None, &name("g")).await?;
        let resolution = resolve(&pool, kb, Some(project), &name("g"), &[]).await?;
        assert!(created_apart_from(&resolution, foreign));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_invalidated_known_as_alias_does_not_make_its_entity_a_holder() -> anyhow::Result<()> {
    in_kb(|pool, kb, project, _| async move {
        let renamed = known_as(&pool, kb, Some(project), &name("h")).await?;
        sqlx::query(
            "UPDATE facts SET invalidated_at = now()
              WHERE subject_id = $1 AND object_value->>'value' = $2",
        )
        .bind(renamed)
        .bind(name("h"))
        .execute(&pool)
        .await?;
        let resolution = resolve(&pool, kb, Some(project), &name("h"), &[]).await?;
        assert!(created_apart_from(&resolution, renamed));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_entity_whose_other_attribute_equals_the_name_is_not_attached() -> anyhow::Result<()> {
    in_kb(|pool, kb, project, _| async move {
        let labelled = valued_under(&pool, kb, project, "nickname", true, &name("k")).await?;
        let resolution = resolve(&pool, kb, Some(project), &name("k"), &[]).await?;
        assert!(created_apart_from(&resolution, labelled));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_entity_named_under_a_user_declared_known_as_is_not_attached() -> anyhow::Result<()> {
    in_kb(|pool, kb, project, _| async move {
        let declared = valued_under(&pool, kb, project, "known_as", false, &name("l")).await?;
        let resolution = resolve(&pool, kb, Some(project), &name("l"), &[]).await?;
        assert!(created_apart_from(&resolution, declared));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn the_name_lookup_returns_no_subject_of_another_base() -> anyhow::Result<()> {
    in_kb(|pool, kb, _, _| async move {
        let elsewhere = other_kb(&pool, kb).await?;
        known_as(&pool, elsewhere, None, &name("m")).await?;
        let found: Vec<(Uuid,)> = sqlx::query_as(&utopia_store::names::named_ids(1, 2))
            .bind(kb)
            .bind(vec![name("m")])
            .fetch_all(&pool)
            .await?;
        assert_eq!(found, vec![]);
        Ok(())
    })
    .await
}
