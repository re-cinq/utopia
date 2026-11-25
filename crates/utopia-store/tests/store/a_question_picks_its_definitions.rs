//! Storage side of definition-scoped retrieval — run against a real store (#574).
//!
//! Three properties:
//! 1. **Embed only what needs it**: rows with no vector, rows whose model changed, and rows whose text changed get re-embedded; those already embedded and unchanged do not.
//! 2. **Vectors are ranked by distance throughout**, and rows whose dimension does not match are skipped.
//! 3. **Fetching by id keeps the given order** — the order is what retrieval ranked.

use sqlx::PgPool;
use utopia_store::mappings as m;
use uuid::Uuid;

async fn base(pool: &PgPool) -> anyhow::Result<Uuid> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'pick-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'pick-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'pick-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    Ok(kb)
}

async fn definition(pool: &PgPool, kb: Uuid, name: &str, summary: &str) -> anyhow::Result<Uuid> {
    let ent = Uuid::now_v7();
    sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, $3)")
        .bind(ent)
        .bind(kb)
        .bind(name)
        .execute(pool)
        .await?;
    let (id, _) = m::propose(
        pool,
        kb,
        ent,
        "wide",
        Some("dw.t"),
        Some("sum(x)"),
        None,
        None,
        Some(summary),
        false,
    )
    .await?;
    sqlx::query("UPDATE concept_mappings SET status = 'confirmed' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(id)
}

#[tokio::test]
async fn only_what_changed_gets_embedded_and_neighbours_come_back_in_order() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let kb = base(&pool).await?;

    let run = async {
        let gmv = definition(&pool, kb, "GMV", "paid, non-test, yuan").await?;
        let net = definition(&pool, kb, "Net sales", "GMV minus refunds").await?;
        let freight = definition(&pool, kb, "Freight", "freight collected").await?;

        // All three need embedding at the start
        let stale = m::needing_embedding(&pool, kb, "bge-m3", 100).await?;
        assert_eq!(stale.len(), 3);
        let text_of = |id: Uuid| stale.iter().find(|t| t.id == id).unwrap().clone();
        assert_eq!(text_of(gmv).embed_text(), "GMV: paid, non-test, yuan");

        // Embedded in (three dimensions is enough to test the ordering)
        m::set_embeddings(
            &pool,
            "bge-m3",
            &[
                (text_of(gmv), vec![1.0, 0.0, 0.0]),
                (text_of(net), vec![0.9, 0.1, 0.0]),
                (text_of(freight), vec![0.0, 0.0, 1.0]),
            ],
        )
        .await?;
        assert!(
            m::needing_embedding(&pool, kb, "bge-m3", 100)
                .await?
                .is_empty(),
            "嵌过且没变的不该再嵌"
        );

        // Change the model: everything must be re-embedded; change the text: only that one row must be re-embedded
        assert_eq!(
            m::needing_embedding(&pool, kb, "other-model", 100)
                .await?
                .len(),
            3
        );
        // Backfilling embeddings is capped: one question embeds only this many, and the rest are backfilled by the next question
        assert_eq!(
            m::needing_embedding(&pool, kb, "other-model", 2)
                .await?
                .len(),
            2
        );
        sqlx::query(
            "UPDATE concept_mappings SET summary = 'freight, paid orders only' WHERE id = $1",
        )
        .bind(freight)
        .execute(&pool)
        .await?;
        let again = m::needing_embedding(&pool, kb, "bge-m3", 100).await?;
        assert_eq!(
            again.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![freight],
            "改了说明的那条要重嵌"
        );

        // Vector ranking: nearest to (1,0,0) is GMV, then Net sales; Freight is furthest
        let near = m::vector_search(&pool, kb, "bge-m3", &[1.0, 0.0, 0.0], 3).await?;
        assert_eq!(near, vec![gmv, net, freight]);
        // A query whose dimension does not match returns nothing (the state after switching models, before re-embedding)
        assert!(m::vector_search(&pool, kb, "bge-m3", &[1.0, 0.0], 3)
            .await?
            .is_empty());
        // Rows under a switched model that have not been re-embedded do not take part: even at equal dimension they are not compared
        assert!(
            m::vector_search(&pool, kb, "other-model", &[1.0, 0.0, 0.0], 3)
                .await?
                .is_empty()
        );

        // Fetching by id keeps the given order
        let got = m::by_ids(&pool, kb, &[freight, gmv]).await?;
        assert_eq!(
            got.iter().map(|x| x.id).collect::<Vec<_>>(),
            vec![freight, gmv]
        );
        assert_eq!(got[1].concept_name, "GMV");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Delete only the knowledge base, not the org — users are soft-deleted, and the test should not invent an action the product does not have
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    run
}
