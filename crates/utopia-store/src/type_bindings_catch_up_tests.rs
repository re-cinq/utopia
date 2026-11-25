//! An entity created after a judgement has landed also gets the class its category word is bound to (`apply_bound`).
//!
//! The projection at the moment of judgement (`apply`) only sees the entities that exist at that moment; a company created in the second half of a half-extracted document ends up with no class,
//! so its attribute statements never reach any attribute candidate whose domain is declared. Skipped when `UTOPIA_DATABASE_URL` is not set.

use super::apply_bound;
use sqlx::PgPool;
use uuid::Uuid;

struct Fx {
    org: Uuid,
    kb: Uuid,
    class: Uuid,
    late: Uuid,
    human: Uuid,
    other: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fx> {
    let (org, ws, kb, class, late, human, other) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations(id,name) VALUES($1,'catch-up')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces(id,org_id,name) VALUES($1,$2,'catch-up')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases(id,workspace_id,name) VALUES($1,$2,'catch-up')")
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO entity_types(id,kb_id,key,label) VALUES($1,$2,'company','企业')")
        .bind(class)
        .bind(kb)
        .execute(pool)
        .await?;
    // "Company" is already bound to the enterprise class; the entities below did not exist at the time of judgement
    sqlx::query(
        "INSERT INTO type_bindings(id,kb_id,kind_word,type_id,status) VALUES($1,$2,'公司',$3,'bound')",
    )
    .bind(Uuid::now_v7())
    .bind(kb)
    .bind(class)
    .execute(pool)
    .await?;
    // A company extracted after the judgement: the category word is written with whitespace, and after normalisation it is the same word
    sqlx::query(
        "INSERT INTO entities(id,kb_id,canonical_name,specific_type) VALUES($1,$2,'蓝海物流有限公司',' 公司 ')",
    )
    .bind(late)
    .bind(kb)
    .execute(pool)
    .await?;
    // The person said it has no class: a binding does not override a human decision
    sqlx::query(
        "INSERT INTO entities(id,kb_id,canonical_name,specific_type,type_source) VALUES($1,$2,'某公司','公司','human')",
    )
    .bind(human)
    .bind(kb)
    .execute(pool)
    .await?;
    // Other category words: no binding, left alone
    sqlx::query(
        "INSERT INTO entities(id,kb_id,canonical_name,specific_type,proposed_type) VALUES($1,$2,'王海','董事长','chairman')",
    )
    .bind(other)
    .bind(kb)
    .execute(pool)
    .await?;
    Ok(Fx {
        org,
        kb,
        class,
        late,
        human,
        other,
    })
}

async fn class_of(
    pool: &PgPool,
    id: Uuid,
) -> anyhow::Result<(Option<Uuid>, String, Option<String>)> {
    Ok(
        sqlx::query_as("SELECT type_id, type_source, proposed_type FROM entities WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

#[tokio::test]
async fn an_entity_built_after_the_decision_still_gets_its_class() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    crate::db::migrate(&pool).await?;
    let f = seed(&pool).await?;
    let run = async {
        assert_eq!(apply_bound(&pool, f.kb).await?, 1);
        assert_eq!(
            class_of(&pool, f.late).await?,
            (Some(f.class), "aligned".into(), None)
        );
        assert_eq!(
            class_of(&pool, f.human).await?,
            (None, "human".into(), None)
        );
        assert_eq!(
            class_of(&pool, f.other).await?,
            (None, "extracted".into(), Some("chairman".into()))
        );
        // Idempotent: entities already on that class do not count as a change
        assert_eq!(apply_bound(&pool, f.kb).await?, 0);
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
