//! `exact = false`: a fragment of a canonical name or of a current alias finds the entity, in this knowledge base only,
//! and nothing else does: not another base's names, not a merged-away entity, not an invalidated alias, not a literal under another predicate.
//!
//! Guards the substring branch of `search_entities` while its filter is rewritten for the planner.
//!
//! Skipped without `UTOPIA_DATABASE_URL`. Each case builds and removes its own rows.

use chrono::{Duration, Utc};
use sqlx::PgPool;
use std::future::Future;
use utopia_store::{graph, names, resolution};
use uuid::Uuid;

#[derive(Clone)]
struct Base {
    pool: PgPool,
    workspace: Uuid,
    kb: Uuid,
}

impl Base {
    async fn another_kb(&self) -> anyhow::Result<Uuid> {
        let kb = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'substring-search-test')",
        )
        .bind(kb)
        .bind(self.workspace)
        .execute(&self.pool)
        .await?;
        Ok(kb)
    }

    async fn search(&self, text: &str) -> anyhow::Result<(Vec<Uuid>, i64)> {
        self.search_as_of(text, None).await
    }

    async fn search_as_of(
        &self,
        text: &str,
        as_of: Option<chrono::DateTime<Utc>>,
    ) -> anyhow::Result<(Vec<Uuid>, i64)> {
        let (nodes, total) =
            graph::search_entities(&self.pool, self.kb, text, 10, 0, as_of, false).await?;
        Ok((nodes.iter().map(|node| node.id).collect(), total))
    }
}

async fn entity(pool: &PgPool, kb: Uuid, name: &str) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(kb)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn in_base<Scenario, Outcome>(scenario: Scenario) -> anyhow::Result<()>
where
    Scenario: FnOnce(Base) -> Outcome,
    Outcome: Future<Output = anyhow::Result<()>>,
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, workspace, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'substring-search-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'substring-search-test')",
    )
    .bind(workspace)
    .bind(org)
    .execute(&pool)
    .await?;
    let base = Base {
        pool: pool.clone(),
        workspace,
        kb,
    };
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'substring-search-test')",
    )
    .bind(kb)
    .bind(workspace)
    .execute(&pool)
    .await?;

    let outcome = scenario(base).await;
    sqlx::query("DELETE FROM knowledge_bases WHERE workspace_id = $1")
        .bind(workspace)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    outcome
}

#[tokio::test]
async fn finds_an_entity_by_a_mixed_case_fragment_of_its_name() -> anyhow::Result<()> {
    in_base(|base| async move {
        let cluster = entity(&base.pool, base.kb, "Kubernetes Cluster").await?;
        entity(&base.pool, base.kb, "Postgres Primary").await?;

        assert_eq!(base.search("uBeRn").await?, (vec![cluster], 1));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn finds_an_entity_by_a_mixed_case_fragment_of_its_alias() -> anyhow::Result<()> {
    in_base(|base| async move {
        let company = entity(&base.pool, base.kb, "International Business Machines").await?;
        names::record(&base.pool, base.kb, company, "IBM", None, None).await?;
        entity(&base.pool, base.kb, "Postgres Primary").await?;

        assert_eq!(base.search("iB").await?, (vec![company], 1));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn returns_no_rows_and_total_0_when_nothing_matches() -> anyhow::Result<()> {
    in_base(|base| async move {
        let company = entity(&base.pool, base.kb, "International Business Machines").await?;
        names::record(&base.pool, base.kb, company, "IBM", None, None).await?;

        assert_eq!(base.search("zzz-no-such-name").await?, (vec![], 0));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn skips_an_entity_that_lives_in_another_knowledge_base() -> anyhow::Result<()> {
    in_base(|base| async move {
        let elsewhere = base.another_kb().await?;
        entity(&base.pool, elsewhere, "Kubernetes Cluster").await?;

        assert_eq!(base.search("kubern").await?, (vec![], 0));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn skips_an_alias_that_lives_in_another_knowledge_base() -> anyhow::Result<()> {
    in_base(|base| async move {
        let elsewhere = base.another_kb().await?;
        let company = entity(&base.pool, elsewhere, "International Business Machines").await?;
        names::record(&base.pool, elsewhere, company, "IBM", None, None).await?;

        assert_eq!(base.search("ibm").await?, (vec![], 0));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn returns_the_target_not_the_source_after_a_merge() -> anyhow::Result<()> {
    in_base(|base| async move {
        let source = entity(&base.pool, base.kb, "Initech Corporation").await?;
        let target = entity(&base.pool, base.kb, "Globex").await?;
        names::record(
            &base.pool,
            base.kb,
            source,
            "Initech Corporation",
            None,
            None,
        )
        .await?;
        resolution::merge_entities(&base.pool, base.kb, source, target, None, "test").await?;

        assert_eq!(base.search("nitech").await?, (vec![target], 1));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn returns_the_source_again_after_its_merge_is_reverted() -> anyhow::Result<()> {
    in_base(|base| async move {
        let source = entity(&base.pool, base.kb, "Initech Corporation").await?;
        let target = entity(&base.pool, base.kb, "Globex").await?;
        names::record(
            &base.pool,
            base.kb,
            source,
            "Initech Corporation",
            None,
            None,
        )
        .await?;
        let merge =
            resolution::merge_entities(&base.pool, base.kb, source, target, None, "test").await?;
        resolution::revert_merge(&base.pool, base.kb, merge).await?;

        assert_eq!(base.search("nitech").await?, (vec![source], 1));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn skips_an_alias_whose_name_fact_was_invalidated() -> anyhow::Result<()> {
    in_base(|base| async move {
        let company = entity(&base.pool, base.kb, "International Business Machines").await?;
        let alias = names::record(&base.pool, base.kb, company, "Big Blue", None, None)
            .await?
            .expect("alias fact");
        sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
            .bind(alias)
            .execute(&base.pool)
            .await?;

        assert_eq!(base.search("big blu").await?, (vec![], 0));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn skips_a_literal_under_a_predicate_other_than_known_as() -> anyhow::Result<()> {
    in_base(|base| async move {
        let company = entity(&base.pool, base.kb, "International Business Machines").await?;
        let launched: Uuid = sqlx::query_scalar(
            "INSERT INTO relation_types (id, kb_id, key, label, kind, datatype, temporal)
             VALUES ($1, $2, 'nickname', 'nickname', 'attribute', 'text', 'event') RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(base.kb)
        .fetch_one(&base.pool)
        .await?;
        graph::insert_value_fact(
            &base.pool,
            base.kb,
            company,
            Some(launched),
            &serde_json::json!({ "value": "Big Blue" }),
            graph::Validity::default(),
            0.9,
        )
        .await?;

        assert_eq!(base.search("big blu").await?, (vec![], 0));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn returns_an_entity_once_when_its_name_and_alias_both_match() -> anyhow::Result<()> {
    in_base(|base| async move {
        let cluster = entity(&base.pool, base.kb, "Kubernetes Cluster").await?;
        names::record(
            &base.pool,
            base.kb,
            cluster,
            "Kubernetes Cluster",
            None,
            None,
        )
        .await?;
        names::record(&base.pool, base.kb, cluster, "K8s Kubernetes", None, None).await?;

        assert_eq!(base.search("KUBERN").await?, (vec![cluster], 1));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn skips_an_entity_when_as_of_is_before_it_was_created() -> anyhow::Result<()> {
    in_base(|base| async move {
        let cluster = entity(&base.pool, base.kb, "Kubernetes Cluster").await?;
        names::record(&base.pool, base.kb, cluster, "K8s", None, None).await?;
        let yesterday = Utc::now() - Duration::days(1);

        assert_eq!(
            base.search_as_of("kubern", Some(yesterday)).await?,
            (vec![], 0)
        );
        assert_eq!(base.search_as_of("k8", Some(yesterday)).await?, (vec![], 0));
        Ok(())
    })
    .await
}
