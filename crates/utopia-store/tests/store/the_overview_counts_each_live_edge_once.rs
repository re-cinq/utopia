//! `graph::overview` at the present: each entity's degree is the number of live, non-name facts that touch it, counted once per fact,
//! within its own knowledge base; merged-away entities are not listed.
//!
//! Guards the node query of the present path while its per-entity count is rewritten into one pass over the base's facts.
//!
//! Skipped without `UTOPIA_DATABASE_URL`. Each case builds and removes its own rows.

use sqlx::PgPool;
use std::future::Future;
use utopia_store::{graph, names, resolution, temporal};
use uuid::Uuid;

#[derive(Clone)]
struct Base {
    pool: PgPool,
    workspace: Uuid,
    kb: Uuid,
    works_with: Uuid,
}

impl Base {
    async fn entity(&self, name: &str) -> anyhow::Result<Uuid> {
        entity_in(&self.pool, self.kb, name).await
    }

    async fn link(&self, subject: Uuid, object: Uuid) -> anyhow::Result<Uuid> {
        let (fact, _) = graph::insert_fact(
            &self.pool,
            self.kb,
            subject,
            Some(self.works_with),
            object,
            graph::Validity::default(),
            0.9,
        )
        .await?;
        Ok(fact)
    }

    async fn another_kb(&self) -> anyhow::Result<(Uuid, Uuid)> {
        let kb = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'overview-degree-test')",
        )
        .bind(kb)
        .bind(self.workspace)
        .execute(&self.pool)
        .await?;
        let works_with = relation_in(&self.pool, kb).await?;
        Ok((kb, works_with))
    }

    async fn degrees(&self, limit: i64) -> anyhow::Result<Vec<(Uuid, i64)>> {
        Ok(self.overview(limit).await?.0)
    }

    async fn overview(&self, limit: i64) -> anyhow::Result<(Vec<(Uuid, i64)>, i64)> {
        let (nodes, _, total_nodes, _) =
            graph::overview(&self.pool, self.kb, limit, None, None).await?;
        let degrees = nodes.iter().map(|node| (node.id, node.degree)).collect();
        Ok((degrees, total_nodes))
    }
}

async fn entity_in(pool: &PgPool, kb: Uuid, name: &str) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(kb)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn relation_in(pool: &PgPool, kb: Uuid) -> anyhow::Result<Uuid> {
    Ok(sqlx::query_scalar(
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'works_with', 'works with') RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(kb)
    .fetch_one(pool)
    .await?)
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
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'overview-degree-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'overview-degree-test')",
    )
    .bind(workspace)
    .bind(org)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'overview-degree-test')",
    )
    .bind(kb)
    .bind(workspace)
    .execute(&pool)
    .await?;
    let works_with = relation_in(&pool, kb).await?;
    let base = Base {
        pool: pool.clone(),
        workspace,
        kb,
        works_with,
    };

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
async fn lists_entities_by_degree_highest_first_with_each_exact_degree() -> anyhow::Result<()> {
    in_base(|base| async move {
        let leaf = base.entity("Leaf").await?;
        let hub = base.entity("Hub").await?;
        let middle = base.entity("Middle").await?;
        let spare = base.entity("Spare").await?;
        base.link(hub, leaf).await?;
        base.link(hub, middle).await?;
        base.link(hub, spare).await?;
        base.link(middle, spare).await?;

        assert_eq!(
            base.degrees(10).await?,
            vec![(hub, 3), (middle, 2), (spare, 2), (leaf, 1)]
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn gives_degree_2_to_an_entity_that_is_subject_of_one_fact_and_object_of_another(
) -> anyhow::Result<()> {
    in_base(|base| async move {
        let manager = base.entity("Manager").await?;
        let engineer = base.entity("Engineer").await?;
        let intern = base.entity("Intern").await?;
        base.link(manager, engineer).await?;
        base.link(engineer, intern).await?;

        assert_eq!(
            base.degrees(10).await?,
            vec![(engineer, 2), (manager, 1), (intern, 1)]
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn counts_a_fact_whose_subject_and_object_are_the_same_entity_once() -> anyhow::Result<()> {
    in_base(|base| async move {
        let ouroboros = base.entity("Ouroboros").await?;
        base.link(ouroboros, ouroboros).await?;

        assert_eq!(base.degrees(10).await?, vec![(ouroboros, 1)]);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn gives_degree_0_to_an_entity_whose_only_facts_are_names() -> anyhow::Result<()> {
    in_base(|base| async move {
        let company = base.entity("International Business Machines").await?;
        names::record(
            &base.pool,
            base.kb,
            company,
            "International Business Machines",
            None,
            None,
        )
        .await?;
        names::record(&base.pool, base.kb, company, "IBM", None, None).await?;

        assert_eq!(base.degrees(10).await?, vec![(company, 0)]);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn gives_degree_1_to_an_entity_with_two_names_and_one_edge() -> anyhow::Result<()> {
    in_base(|base| async move {
        let company = base.entity("International Business Machines").await?;
        let partner = base.entity("Globex").await?;
        names::record(&base.pool, base.kb, company, "IBM", None, None).await?;
        names::record(&base.pool, base.kb, company, "Big Blue", None, None).await?;
        base.link(company, partner).await?;

        assert_eq!(base.degrees(10).await?, vec![(company, 1), (partner, 1)]);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn gives_degree_0_to_both_ends_of_an_invalidated_fact() -> anyhow::Result<()> {
    in_base(|base| async move {
        let source = base.entity("Source").await?;
        let target = base.entity("Target").await?;
        let fact = base.link(source, target).await?;
        temporal::retract(&base.pool, base.kb, fact).await?;

        assert_eq!(base.degrees(10).await?, vec![(source, 0), (target, 0)]);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn adds_a_literal_fact_to_its_subject_only() -> anyhow::Result<()> {
    in_base(|base| async move {
        let city = base.entity("Lisbon").await?;
        let country = base.entity("Portugal").await?;
        graph::insert_value_fact(
            &base.pool,
            base.kb,
            city,
            Some(base.works_with),
            &serde_json::json!({ "value": "545000" }),
            graph::Validity::default(),
            0.9,
        )
        .await?;

        assert_eq!(base.degrees(10).await?, vec![(city, 1), (country, 0)]);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn ignores_facts_and_entities_of_another_knowledge_base() -> anyhow::Result<()> {
    in_base(|base| async move {
        let searched = base.entity("Searched").await?;
        let (elsewhere, elsewhere_relation) = base.another_kb().await?;
        let stranger = entity_in(&base.pool, elsewhere, "Stranger").await?;
        let neighbour = entity_in(&base.pool, elsewhere, "Neighbour").await?;
        graph::insert_fact(
            &base.pool,
            elsewhere,
            stranger,
            Some(elsewhere_relation),
            neighbour,
            graph::Validity::default(),
            0.9,
        )
        .await?;

        assert_eq!(base.overview(10).await?, (vec![(searched, 0)], 1));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn lists_the_merge_target_with_the_source_facts_and_not_the_source() -> anyhow::Result<()> {
    in_base(|base| async move {
        let source = base.entity("Initech Corporation").await?;
        let target = base.entity("Globex").await?;
        let partner = base.entity("Hooli").await?;
        base.link(source, partner).await?;
        resolution::merge_entities(&base.pool, base.kb, source, target, None, "test").await?;

        assert_eq!(
            base.overview(10).await?,
            (vec![(target, 1), (partner, 1)], 2)
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn lists_an_entity_without_facts_with_degree_0() -> anyhow::Result<()> {
    in_base(|base| async move {
        let loner = base.entity("Loner").await?;

        assert_eq!(base.overview(10).await?, (vec![(loner, 0)], 1));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn returns_only_limit_nodes_and_breaks_equal_degrees_by_earlier_creation(
) -> anyhow::Result<()> {
    in_base(|base| async move {
        let first = base.entity("First").await?;
        let second = base.entity("Second").await?;
        let third = base.entity("Third").await?;
        let fourth = base.entity("Fourth").await?;
        base.link(third, fourth).await?;
        base.link(second, first).await?;

        assert_eq!(
            base.overview(3).await?,
            (vec![(first, 1), (second, 1), (third, 1)], 4)
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn returns_no_nodes_and_total_0_for_an_empty_knowledge_base() -> anyhow::Result<()> {
    in_base(|base| async move {
        assert_eq!(base.overview(10).await?, (vec![], 0));
        Ok(())
    })
    .await
}
