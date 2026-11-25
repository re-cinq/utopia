//! 0016 B3: `owl:disjointWith` feeds resolution: when the ontology declares two classes disjoint, even same-named things do not enter the review queue.
//!
//! Resolution has three layers for judging "can these two classes refer to the same thing": the hard table `CONFUSABLE_TYPE_KEYS`, the class hierarchy
//! (same branch counts as confusable, #226), and declared disjointness from the ontology. This guards that **a declaration takes precedence over the first two layers**:
//!
//! 1. With no declaration, behaviour is unchanged: organization vs project still enters the queue by the hard table; corporation vs
//!    federal_agency still enters by class hierarchy (sharing the non-root ancestor organization).
//! 2. After declaring organization ⟂ project, same-named organization / project are kept apart and do not enter the queue.
//! 3. After declaring corporation ⟂ agency, federal_agency (a subclass of agency) is also kept apart from corporation
//!    too: **disjointness is inherited**, declaring it on the parent is enough.
//!
//! Without `UTOPIA_DATABASE_URL` it skips rather than fails. It sets up and tears down its own data and never touches an existing database.

use sqlx::PgPool;
use utopia_store::{name_vectors, names, ontology, resolution};
use uuid::Uuid;

struct Fx {
    org: Uuid,
    kb: Uuid,
    organization: Uuid,
    project: Uuid,
    corporation: Uuid,
    agency: Uuid,
    federal_agency: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fx> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    // The top needs a root: under the class-hierarchy rule a "shared ancestor" does not count as a root (in schema.org everything is
    // Thing, so counting it would make Person and Organization kin), hence organization needs a parent class
    let (thing, organization, project, corporation, agency, federal_agency) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'disjoint-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'disjoint-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'disjoint-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, key) in [
        (thing, "thing"),
        (organization, "organization"),
        (project, "project"),
        (corporation, "corporation"),
        (agency, "agency"),
        (federal_agency, "federal_agency"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $3)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .execute(pool)
            .await?;
    }
    for (child, parent) in [
        (organization, thing),
        (project, thing),
        (corporation, organization),
        (agency, organization),
        (federal_agency, agency),
    ] {
        sqlx::query("INSERT INTO entity_type_parents (child_id, parent_id) VALUES ($1, $2)")
            .bind(child)
            .bind(parent)
            .execute(pool)
            .await?;
    }
    Ok(Fx {
        org,
        kb,
        organization,
        project,
        corporation,
        agency,
        federal_agency,
    })
}

async fn entity(pool: &PgPool, f: &Fx, name: &str, type_id: Uuid) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(type_id)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Resolves one mention, returning which "type drift" review pair it points to
async fn drift_reviews(
    pool: &PgPool,
    f: &Fx,
    name: &str,
    type_id: Uuid,
) -> anyhow::Result<Vec<Uuid>> {
    let r =
        resolution::resolve_mention(pool, f.kb, Some(type_id), name, None, None, None, &[]).await?;
    assert!(
        r.created,
        "a cross-type same name is a new entity: keep apart, never merge"
    );
    Ok(r.reviews
        .iter()
        .filter(|x| x.reason.starts_with("type_drift|"))
        .map(|x| x.other_id)
        .collect())
}

#[tokio::test]
async fn a_declared_disjointness_keeps_names_apart() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // 1. No declaration: the hard table and the class hierarchy as before
        let orion = entity(&pool, &f, "Orion", f.organization).await?;
        assert_eq!(
            drift_reviews(&pool, &f, "Orion", f.project).await?,
            vec![orion],
            "organization vs project is confusable by the hard-coded list"
        );
        let acme = entity(&pool, &f, "Acme", f.corporation).await?;
        assert_eq!(
            drift_reviews(&pool, &f, "Acme", f.federal_agency).await?,
            vec![acme],
            "corporation vs federal_agency share the ancestor organization: kin, so Review"
        );

        // 2. Declared organization ⟂ project: the hard table says confusable, the ontology says disjoint: the ontology wins
        ontology::set_disjoint_for(&pool, f.kb, f.organization, &[f.project]).await?;
        let _vega = entity(&pool, &f, "Vega", f.organization).await?;
        assert!(
            drift_reviews(&pool, &f, "Vega", f.project)
                .await?
                .is_empty(),
            "a declared disjointness wins over the hard-coded list"
        );

        // 3. Declared corporation ⟂ agency: federal_agency is a subclass of agency, so disjointness is inherited and
        //    the class hierarchy saying they are kin does not count
        ontology::set_disjoint_for(&pool, f.kb, f.corporation, &[f.agency]).await?;
        let _beta = entity(&pool, &f, "Beta", f.corporation).await?;
        assert!(
            drift_reviews(&pool, &f, "Beta", f.federal_agency)
                .await?
                .is_empty(),
            "a disjointness declared on the parent reaches the child and wins over kinship"
        );
        // Asked the other way round too: the table has a row in each direction, and inheritance walks the ancestor chain of the other end
        let _gamma = entity(&pool, &f, "Gamma", f.federal_agency).await?;
        assert!(
            drift_reviews(&pool, &f, "Gamma", f.corporation)
                .await?
                .is_empty(),
            "the declaration holds from either side"
        );

        // 4. Remove the declaration and return to behaviour with no declaration: an edit must be able to undo it
        ontology::set_disjoint_for(&pool, f.kb, f.corporation, &[]).await?;
        let delta = entity(&pool, &f, "Delta", f.corporation).await?;
        assert_eq!(
            drift_reviews(&pool, &f, "Delta", f.federal_agency).await?,
            vec![delta],
            "with the declaration gone, kinship sends the pair to Review again"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await;
    run
}

/// The name-vector channel also honours the declaration (#1293): project cannot recognize the broad class, so before, even with disjointness declared it still got queued correctly
#[tokio::test]
async fn a_declared_disjointness_keeps_near_names_apart() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let orion = entity(&pool, &f, "Orion Labs", f.organization).await?;
        let fact = names::record(&pool, f.kb, orion, "Orion Labs", None, None)
            .await?
            .expect("a named entity gets a name fact");
        let near = vec![1.0, 0.0, 0.0];
        name_vectors::set(&pool, f.kb, &[(fact, orion, near.clone())]).await?;
        let proposed = |mention: &'static str| {
            let (pool, near) = (&pool, &near);
            async move {
                let r = resolution::resolve_mention(
                    pool,
                    f.kb,
                    Some(f.project),
                    mention,
                    None,
                    Some(near),
                    None,
                    &[],
                )
                .await?;
                anyhow::Ok(
                    r.reviews
                        .iter()
                        .filter(|v| v.reason.starts_with("name_vector|"))
                        .map(|v| v.other_id)
                        .collect::<Vec<Uuid>>(),
                )
            }
        };

        assert_eq!(
            proposed("Nebula Rollout").await?,
            vec![orion],
            "without a declaration a near organization name is proposed for a project"
        );
        ontology::set_disjoint_for(&pool, f.kb, f.organization, &[f.project]).await?;
        assert!(
            proposed("Nebula Migration").await?.is_empty(),
            "organization declared disjoint from project: no name_vector pair"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await;
    run
}
