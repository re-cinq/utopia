//! Types a person has signed off on must not be changed by the engine: hit against a real database.
//!
//! This whole line of defence lives in SQL `WHERE` clauses: each of three read sides adds a `type_source <> 'human'`,
//! and missing one fails silently. `cargo check` cannot see a word of it,
//! and 0009 just tripped over the `NULL <> uuid` trap in this same area: the type system can count Rust,
//! but cannot count into the SQL.
//!
//! Three assertions map to three paths:
//!
//! - Type-resolution candidate selection (`entities_for_type_resolution`) must not pull in entities a person has signed off on
//! - Extraction promotion must not give a type to an entity a person said has no type ← the cross-section of 0009 × P4
//! - `retype_entities` uses the existing `actor` parameter to tell human from inferred
//!
//! Without `UTOPIA_DATABASE_URL` it skips rather than fails. It sets up and tears down its own data and never touches an existing database.

use sqlx::PgPool;
use uuid::Uuid;

struct Fx {
    org: Uuid,
    kb: Uuid,
    org_type: Uuid,
    sub_type: Uuid,
}

/// Build an ontology that is just enough to trigger the "third candidate-selection condition": `organization` has a subclass `startup`.
/// Once a person has set the entity to `organization`, that subclass is exactly what makes it eligible for re-judgment.
async fn seed(pool: &PgPool) -> anyhow::Result<Fx> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (org_type, sub_type) = (Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'p4a-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'p4a-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'p4a-test')")
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    for (id, key) in [(org_type, "organization"), (sub_type, "startup")] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $3)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .execute(pool)
            .await?;
    }
    sqlx::query("INSERT INTO entity_type_parents (child_id, parent_id) VALUES ($1, $2)")
        .bind(sub_type)
        .bind(org_type)
        .execute(pool)
        .await?;
    Ok(Fx {
        org,
        kb,
        org_type,
        sub_type,
    })
}

async fn entity(
    pool: &PgPool,
    f: &Fx,
    name: &str,
    type_id: Option<Uuid>,
    source: &str,
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entities (id, kb_id, type_id, canonical_name, type_source)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(type_id)
    .bind(name)
    .bind(source)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn source_of(pool: &PgPool, id: Uuid) -> anyhow::Result<String> {
    Ok(
        sqlx::query_scalar("SELECT type_source FROM entities WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

/// Main battleground: the "a current class with subclasses is included" condition in candidate selection would pull back entities that a person has signed off on.
#[tokio::test]
async fn type_resolution_leaves_human_decisions_alone() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let by_human = entity(&pool, &f, "Acme", Some(f.org_type), "human").await?;
        let by_engine = entity(&pool, &f, "Globex", Some(f.org_type), "extracted").await?;

        let picked =
            utopia_store::resolution::entities_for_type_resolution(&pool, f.kb, 100, false)
                .await?
                .into_iter()
                .map(|c| c.id)
                .collect::<std::collections::HashSet<_>>();

        assert!(
            picked.contains(&by_engine),
            "抽取定的类型该被重判——organization 有子类 startup，这正是消解的用武之地"
        );
        assert!(
            !picked.contains(&by_human),
            "what a person has signed off must not be re-judged"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    run
}

/// **The cross-section of 0009 × P4, the one most easily missed.**
///
/// After 0009, "no type" may be a person's decision: they looked at this entity and judged that no suitable class exists in the ontology.
/// But the guard for extraction promotion only checked `type_key.is_none()`, which could not tell "not yet judged" from "a person judged, and it is none",
/// so the next extraction would give it a type.
#[tokio::test]
async fn extraction_does_not_fill_in_a_type_a_human_left_empty() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // A person looked at it and judged that no suitable class exists in the ontology → no type, and this is a decision
        let decided = entity(&pool, &f, "Ambiguous Thing", None, "human").await?;
        // the one not yet judged
        let pending = entity(&pool, &f, "Other Thing", None, "extracted").await?;

        // **The promotion branch is reached only with a profile similarity:** with ctx empty, best is None and the whole section is skipped.
        // The first version was written exactly this way, and removing the guard made no test fail: the test did not check what it was meant to check
        let ctx: Vec<f32> = vec![1.0, 0.0, 0.0];
        for id in [decided, pending] {
            sqlx::query(
                "UPDATE entities SET profile_embedding = $2::vector, profile_n = 1 WHERE id = $1",
            )
            .bind(id)
            .bind("[1,0,0]")
            .execute(&pool)
            .await?;
        }

        // Extraction meets the same name mention again and judges organization. Cosine = 1.0, far above SIM_ATTACH
        for id in [decided, pending] {
            let name: String =
                sqlx::query_scalar("SELECT canonical_name FROM entities WHERE id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await?;
            let _ = utopia_store::resolution::resolve_mention(
                &pool,
                f.kb,
                Some(f.org_type),
                &name,
                Some(&ctx),
                None,
                None,
                &[],
            )
            .await?;
        }

        // Control: the one without a person's sign-off **should** be promoted, otherwise this test proves nothing about the guard working
        let after_pending: Option<Uuid> =
            sqlx::query_scalar("SELECT type_id FROM entities WHERE id = $1")
                .bind(pending)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            after_pending,
            Some(f.org_type),
            "还没判过的该被抽取升格——不然下面那条断言是空的"
        );

        let after_decided: Option<Uuid> =
            sqlx::query_scalar("SELECT type_id FROM entities WHERE id = $1")
                .bind(decided)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            after_decided, None,
            "人说过「就是没有类型」，抽取不该替他填一个"
        );
        assert_eq!(source_of(&pool, decided).await?, "human");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    run
}

/// `retype_entities` uses the existing `actor` parameter to tell the source apart: a person's click to approve is an endorsement, and protected;
/// an engine's automatic ruling is not. No new parameter is needed for this: it was already there when #112 added actor.
#[tokio::test]
async fn who_approved_a_retype_decides_whether_it_is_protected() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    let actor = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, org_id, email, display_name, password_hash)
         VALUES ($1, $2, $1 || '@p4a.test', 'p4a', 'x')",
    )
    .bind(actor)
    .bind(f.org)
    .execute(&pool)
    .await?;

    let run = async {
        let a = entity(&pool, &f, "Approved", Some(f.org_type), "extracted").await?;
        let b = entity(&pool, &f, "Auto", Some(f.org_type), "extracted").await?;

        utopia_store::resolution::retype_entities(&pool, f.kb, &[(a, f.sub_type)], Some(actor))
            .await?;
        utopia_store::resolution::retype_entities(&pool, f.kb, &[(b, f.sub_type)], None).await?;

        assert_eq!(
            source_of(&pool, a).await?,
            "human",
            "a person's approval click is an endorsement"
        );
        assert_eq!(
            source_of(&pool, b).await?,
            "inferred",
            "what the engine decided automatically is not"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(actor)
        .execute(&pool)
        .await?;
    run
}

/// **An entity the engine changed must still be retrievable in the next round.**
///
/// This one guards an accident that really happened: when type resolution was stored, it took "the person who clicked run" as
/// the actor of `retype_entities`, and an actor means writing `type_source = 'human'`.
/// So **an entity that had been resolved once was never resolved again from then on**: in a database with no manual PATCH record at all,
/// the preview returned an empty list after one round finished, with no error.
///
/// "Who clicked run" and "who judged what this entity is" are two different things. The former is recorded in the
/// `ontology.types_resolved` audit; only the latter should decide `type_source`.
#[tokio::test]
async fn an_engine_retype_does_not_lock_the_entity_out_of_the_next_round() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let e = entity(&pool, &f, "Initech", Some(f.org_type), "extracted").await?;
        // Keep one specific_type so that it is always eligible under "candidate selection", so that whether it is passed over
        // depends only on type_source; otherwise, once changed to a leaf class it would be passed over anyway, and the test would prove nothing
        sqlx::query("UPDATE entities SET specific_type = 'startup company' WHERE id = $1")
            .bind(e)
            .execute(&pool)
            .await?;
        // Automatic ruling by the engine: no person endorsed this one
        utopia_store::resolution::retype_entities(&pool, f.kb, &[(e, f.sub_type)], None).await?;

        assert_eq!(
            source_of(&pool, e).await?,
            "inferred",
            "引擎裁决不是人的背书"
        );

        let picked =
            utopia_store::resolution::entities_for_type_resolution(&pool, f.kb, 100, false)
                .await?
                .into_iter()
                .map(|c| c.id)
                .collect::<std::collections::HashSet<_>>();
        assert!(
            picked.contains(&e),
            "引擎改过一次不该把实体锁死——本体还会长大,它还得能被重判"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    run
}
