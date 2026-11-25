//! Facts extracted from memory wait for a person's nod first (`docs/decisions/0015`, table in migration 0018) — run against a real DB.
//!
//! It guards against the reverse of what was observed: the conversation says "Acme moved its headquarters to Shenzhen", and the graph must **not** immediately gain
//! a live edge. A proposal goes only into `pending_facts`; only after a person nods does it enter the ledger through the extraction path (fact +
//! evidence pointing back at that sentence + temporal reconciliation); after a shake of the head, the same triple is no longer proposed.

use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    /// The org built by the fixture, deleted from first at teardown (cascading)
    org: Uuid,
    kb: Uuid,
    acme: Uuid,
    shenzhen: Uuid,
    shanghai: Uuid,
    hq: Uuid,
    chunk: Uuid,
}

async fn fixture(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'nod-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'nod-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'nod-test')")
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    // There is only one headquarters: functional makes temporal reconciliation have something to do
    let hq = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, functional)
         VALUES ($1, $2, 'headquartered_in', 'headquartered in', TRUE)",
    )
    .bind(hq)
    .bind(kb)
    .execute(pool)
    .await?;
    let (acme, shenzhen, shanghai) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for (id, name) in [
        (acme, "Acme"),
        (shenzhen, "Shenzhen"),
        (shanghai, "Shanghai"),
    ] {
        sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(kb)
            .bind(name)
            .execute(pool)
            .await?;
    }
    // That memory sentence becomes a chunk through the product's own path
    let (_doc, chunk) = utopia_store::memory::append_episode(
        pool,
        kb,
        "Acme moved its headquarters to Shenzhen on 2026-03-15.",
        chrono::Utc::now(),
    )
    .await?;
    Ok(Fixture {
        org,
        kb,
        acme,
        shenzhen,
        shanghai,
        hq,
        chunk,
    })
}

fn day(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
}

async fn live_facts(pool: &PgPool, kb: Uuid) -> anyhow::Result<i64> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM facts WHERE kb_id = $1 AND invalidated_at IS NULL")
            .bind(kb)
            .fetch_one(pool)
            .await?;
    Ok(n)
}

#[tokio::test]
async fn a_remembered_fact_waits_for_a_nod() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = fixture(&pool).await?;

    let run = async {
        use utopia_store::pending::{self, Outcome, Proposal};
        let propose = |object: Uuid, from: &'static str| {
            pending::propose(
                &pool,
                Proposal {
                    kb_id: f.kb,
                    subject_id: f.acme,
                    predicate_id: Some(f.hq),
                    object_id: Some(object),
                    object_value: None,
                    proposed_predicate: Some("moved headquarters to"),
                    validity: utopia_store::graph::Validity {
                        from: Some(day(from)),
                        from_precision: Some("day"),
                        to: None,
                        to_precision: None,
                        attested_at: None,
                        from_grade: None,
                        ..Default::default()
                    },
                    confidence: 0.9,
                    chunk_id: f.chunk,
                    proposed_by: None,
                    proposed_token: None,
                    phrase: None,
                    qualifiers: None,
                    time_words: None,
                    quote_span: None,
                },
            )
        };

        // 1. The proposal does not go onto the graph
        let first = propose(f.shenzhen, "2026-03-15").await?;
        assert!(matches!(first, Outcome::Proposed(_)), "第一次该进队列");
        assert_eq!(live_facts(&pool, f.kb).await?, 0, "提议阶段图上不能有活边");
        let queued = pending::for_chunk(&pool, f.kb, f.chunk).await?;
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].subject_name, "Acme");
        assert!(queued[0].quote.contains("Acme moved its headquarters"), "原句要跟着提议一起给人看");

        // 2. Re-extracting the same sentence does not propose it again
        assert_eq!(propose(f.shenzhen, "2026-03-15").await?, Outcome::AlreadyPending);

        // 3. Nod: it enters the ledger, evidence points back at the sentence, and the queue is emptied
        let Outcome::Proposed(id) = first else { unreachable!() };
        let done = pending::confirm(&pool, f.kb, id).await?;
        assert!(done.created, "确认该落一条新事实");
        assert_eq!(live_facts(&pool, f.kb).await?, 1);
        let (ev_chunk, ev_proposed): (Uuid, Option<String>) = sqlx::query_as(
            "SELECT chunk_id, proposed_predicate FROM fact_evidence WHERE fact_id = $1",
        )
        .bind(done.fact_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(ev_chunk, f.chunk, "证据要指回那句记忆");
        assert_eq!(ev_proposed.as_deref(), Some("moved headquarters to"));
        assert!(pending::for_chunk(&pool, f.kb, f.chunk).await?.is_empty());

        // 4. Something already on the graph is not asked about again
        assert_eq!(propose(f.shenzhen, "2026-03-15").await?, Outcome::AlreadyAsserted);

        // 5. A shake of the head is remembered: a triple that was refused is not proposed in the next round
        let Outcome::Proposed(bad) = propose(f.shanghai, "2020-01-01").await? else {
            panic!("换一个宾语该是新提议");
        };
        pending::reject(&pool, f.kb, bad, None).await?;
        assert_eq!(propose(f.shanghai, "2020-01-01").await?, Outcome::Rejected);
        assert_eq!(live_facts(&pool, f.kb).await?, 1, "拒绝不碰图");

        // 6. A nod goes through the extraction path: for a functional relation, the new value closes the old one.
        //    Refusal records are looked up by (subject, predicate, object), so the same object with a changed date is still blocked —
        //    so the demo of a successor uses a different object entity
        let beijing = Uuid::now_v7();
        sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'Beijing')")
            .bind(beijing)
            .bind(f.kb)
            .execute(&pool)
            .await?;
        let Outcome::Proposed(next) = propose(beijing, "2027-01-01").await? else {
            panic!("接任该是新提议");
        };
        let moved = pending::confirm(&pool, f.kb, next).await?;
        assert!(moved.created);
        assert_eq!(moved.conflicts, 0, "先后清楚、置信够高，该自动闭合而不是进冲突");
        let (closed,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM facts
              WHERE kb_id = $1 AND object_id = $2 AND invalidated_at IS NULL AND valid_to IS NOT NULL",
        )
        .bind(f.kb)
        .bind(f.shenzhen)
        .fetch_one(&pool)
        .await?;
        assert_eq!(closed, 1, "深圳那条该被闭合成有终点的区间");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Delete the org the fixture itself created; the cascade takes the workspace, the KB and everything else with it.
    // Previously only the KB was deleted, so every run left behind an empty org / workspace pair in the DB —
    // "no production code deletes an org" is right, but a fixture is not production code, so it cleans up what it creates itself
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
