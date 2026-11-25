//! The consistency check against a real store: fetch the data, judge, store, rerun.
//!
//! The pure-logic part already has 12 cases in `utopia-reason`, and runs without a store. What is pinned here is
//! the four things that layer cannot see, each of which lives in SQL or table constraints:
//!
//! - **The three filters on the data fetched**. Facts that have been overturned, facts with no predicate, and attribute facts whose object is a literal
//!   should take no part — axioms speak of relations between entities
//! - **No axioms, no criterion**. A store with no ontology package loaded runs to zero, which is the truth, not a fault
//! - **Rerunning is idempotent**. The same contradiction is not stored twice, and a row a person has already judged is not wiped by a rerun
//!   (`ontology_proposals` hit a pitfall here: a rerun flipped rejected proposals back to pending)
//! - **Stale ones are cleared**. Once a fact is retracted, its violation should not still hang on the Review page
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data.

use sqlx::PgPool;
use utopia_store::reasoning;
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    /// Declares asymmetric + irreflexive
    owns: Uuid,
    /// Declares no axioms at all
    mentions: Uuid,
    a: Uuid,
    b: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let etype = Uuid::now_v7();
    let (owns, mentions) = (Uuid::now_v7(), Uuid::now_v7());
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'axioms-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'axioms-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'axioms-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'thing', 'Thing')",
    )
    .bind(etype)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, is_asymmetric, is_irreflexive)
         VALUES ($1, $2, 'owns', 'owns', TRUE, TRUE)",
    )
    .bind(owns)
    .bind(kb)
    .execute(pool)
    .await?;
    // Not a single axiom — its edges should never be judged a contradiction
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'mentions', 'mentions')",
    )
    .bind(mentions)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [(a, "Acme"), (b, "Beta")] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(etype)
        .bind(name)
        .execute(pool)
        .await?;
    }
    Ok(Fixture {
        org,
        kb,
        owns,
        mentions,
        a,
        b,
    })
}

/// Stores one relation fact, returns its id.
async fn fact(pool: &PgPool, kb: Uuid, s: Uuid, p: Uuid, o: Uuid) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(kb)
    .bind(s)
    .bind(p)
    .bind(o)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn open_kinds(pool: &PgPool, kb: Uuid) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT kind FROM axiom_violations WHERE kb_id = $1 AND status = 'open' ORDER BY kind",
    )
    .bind(kb)
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
async fn the_ontology_is_the_only_judge() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // ---- 1. A store with no contradictions runs to zero
        let quiet = fact(&pool, f.kb, f.a, f.owns, f.b).await?;
        let r = reasoning::run(&pool, f.kb).await?;
        assert_eq!(r.edges, 1);
        assert_eq!(r.predicates_with_axioms, 1, "no mentions are declared, so it must not be included");
        assert_eq!(r.found, 0, "a one-way owns relation constitutes no contradiction");

        // ---- 2. Axioms only count when declared
        // B owns A — together with the one above, forms an asymmetry violation
        let back = fact(&pool, f.kb, f.b, f.owns, f.a).await?;
        // A mentions B / B mentions A — two-way, but mentions has no axiom, so it should not be reported
        fact(&pool, f.kb, f.a, f.mentions, f.b).await?;
        fact(&pool, f.kb, f.b, f.mentions, f.a).await?;
        // A owns A — a reflexivity violation
        let loop_fact = fact(&pool, f.kb, f.a, f.owns, f.a).await?;

        let r = reasoning::run(&pool, f.kb).await?;
        assert_eq!(r.edges, 5);
        assert_eq!(
            open_kinds(&pool, f.kb).await?,
            vec!["asymmetry", "self_loop"],
            "双向的 mentions 不该被报——它的谓词没有任何公理"
        );
        assert_eq!(r.inserted, 2);

        // The reflexivity case: the two columns refer to the same fact
        let (l, rr): (Uuid, Uuid) = sqlx::query_as(
            "SELECT left_fact, right_fact FROM axiom_violations
              WHERE kb_id = $1 AND kind = 'self_loop'",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        assert_eq!(l, loop_fact);
        assert_eq!(l, rr, "a fact that contradicts itself needs no second fact");

        // ---- 3. A rerun does not store it again
        let again = reasoning::run(&pool, f.kb).await?;
        assert_eq!(again.found, 2);
        assert_eq!(again.inserted, 0, "a second run over the same contradiction must not insert another row");
        assert_eq!(again.cleared, 0, "nor clear the previous round's");

        // ---- 4. A row a person has judged is not wiped by a rerun
        sqlx::query(
            "UPDATE axiom_violations SET status = 'resolved', resolution = 'accepted'
              WHERE kb_id = $1 AND kind = 'asymmetry'",
        )
        .bind(f.kb)
        .execute(&pool)
        .await?;
        let after = reasoning::run(&pool, f.kb).await?;
        assert_eq!(after.inserted, 0, "a row someone already took a stance on must not be inserted again");
        let resolved: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM axiom_violations WHERE kb_id = $1 AND status = 'resolved'",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        assert_eq!(resolved, 1, "a person's decision must survive a re-run");

        // ---- 5. Once a fact is retracted, the stale open row must be cleared
        sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
            .bind(loop_fact)
            .execute(&pool)
            .await?;
        let swept = reasoning::run(&pool, f.kb).await?;
        assert_eq!(swept.cleared, 1, "an overturned fact must not still carry a violation");
        assert!(
            !open_kinds(&pool, f.kb).await?.contains(&"self_loop".to_string()),
            "撤掉那条事实之后自反违规就不成立了"
        );

        // ---- 6. Attribute facts do not take part: the object is a literal, while axioms speak of relations between entities
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value)
             VALUES ($1, $2, $3, $4, '\"2015\"'::jsonb)",
        )
        .bind(Uuid::now_v7())
        .bind(f.kb)
        .bind(f.a)
        .bind(f.owns)
        .execute(&pool)
        .await?;
        let attrs = reasoning::run(&pool, f.kb).await?;
        assert_eq!(attrs.edges, 4, "the one with a literal-value object must not be taken as an edge");

        // ---- 7. A store with no ontology package: the conclusion is "no criterion", not "no contradiction"
        sqlx::query("UPDATE relation_types SET is_asymmetric = FALSE, is_irreflexive = FALSE WHERE kb_id = $1")
            .bind(f.kb)
            .execute(&pool)
            .await?;
        let blind = reasoning::run(&pool, f.kb).await?;
        assert_eq!(blind.predicates_with_axioms, 0);
        assert_eq!(blind.found, 0);
        assert!(
            open_kinds(&pool, f.kb).await?.is_empty(),
            "公理撤了,据它报出来的违规也该跟着走"
        );
        let _ = (quiet, back);
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Delete the org along with it — deleting only the kb would leave organizations / workspaces behind in the dev store
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
