//! Axioms declared by the ontology must actually land in the database -- tested against a real database.
//!
//! The origin of this guard is a pre-existing defect: the `create_relation_types_bulk` documentation says
//! "`functional` / `inverse_functional` must be written as the vocabulary says, not defaulted to false",
//! but its SQL hard-codes `FALSE, FALSE` -- two arrays were bound but never made it into the `UNNEST`.
//! Measured after installing FOAF (which declares 17 FunctionalProperty): in the database, **not one**
//! functional flag is true.
//!
//! Those two flags are what the temporal engine uses to close facts automatically. A wrong write goes in a different direction with different consequences: marking one true wrongly
//! creates false conflicts in batches (59 of them for `part_of`), whereas this time the direction was reversed -- those that should be true all became false,
//! so **not a single temporal conflict that should have been detected was detected**, silently.
//!
//! `cargo check` cannot see this kind of error (the types are all correct), and unit tests cannot either (it lives in the SQL string).
//! Only writing one real row and reading it back can catch it.

use sqlx::PgPool;
use uuid::Uuid;

/// Build a minimal library: axiom storage does not depend on the ontology's size, so one is enough.
async fn kb(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid)> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'ax-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'ax-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'ax-test')")
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    Ok((kb, org))
}

#[tokio::test]
async fn every_axiom_survives_the_bulk_insert() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (kb_id, org) = kb(&pool).await?;

    let run = async {
        let row = |key: &str, f: bool, i: bool, t: bool, s: bool, a: bool, r: bool| {
            utopia_store::ontology::BulkRelation {
                key: key.into(),
                label: key.into(),
                description: String::new(),
                iri: format!("http://ax.test/#{key}"),
                kind: "relation",
                datatype: None,
                functional: f,
                inverse_functional: i,
                transitive: t,
                symmetric: s,
                asymmetric: a,
                irreflexive: r,
                temporal: "state".into(),
                display_hints: serde_json::json!({}),
            }
        };
        // One all-true and one all-false: the all-false one guards that "what is not declared must not be written as true"
        utopia_store::ontology::create_relation_types_bulk(
            &pool,
            kb_id,
            &[
                row("all_true", true, true, true, true, true, true),
                row("all_false", false, false, false, false, false, false),
            ],
        )
        .await?;

        let got: Vec<(String, bool, bool, bool, bool, bool, bool)> = sqlx::query_as(
            "SELECT key, functional, inverse_functional,
                    is_transitive, is_symmetric, is_asymmetric, is_irreflexive
             FROM relation_types WHERE kb_id = $1 ORDER BY key",
        )
        .bind(kb_id)
        .fetch_all(&pool)
        .await?;

        let t = got
            .iter()
            .find(|r| r.0 == "all_true")
            .expect("all_true 落库");
        assert!(
            t.1 && t.2 && t.3 && t.4 && t.5 && t.6,
            "六个公理位都该照写下去，实得 {t:?}"
        );
        let f = got
            .iter()
            .find(|r| r.0 == "all_false")
            .expect("all_false 落库");
        assert!(
            !f.1 && !f.2 && !f.3 && !f.4 && !f.5 && !f.6,
            "没声明的公理不该凭空为真，实得 {f:?}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
