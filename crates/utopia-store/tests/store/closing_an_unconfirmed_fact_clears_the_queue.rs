//! The "close" exit must genuinely clear a fact that "is no longer stated", run against a real DB.
//!
//! Closing is invalidate + rewrite (`temporal::close_superseded` / `rewrite_end_tx`), and the correction row copies the evidence along
//! (`copy_evidence`) — so it still "has all its evidence on superseded chunks". This bucket's criterion lacked
//! the "still holds today" clause, and two consequences surfaced together:
//!
//! 1. The row closed and returned still shows in the list and the count does not drop, so it looks as if closing did not take effect;
//! 2. Closing again: the correction row already has an end, so the interface, which accepts only "open intervals may be closed", judges it not found and errors.
//!
//! What is pinned here: this bucket lists only **current** facts (0022: "ended on an unknown day" is not open), and a row that has been closed
//! leaves at once, with `counts` zeroed together with the list under the same WHERE.
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is not set. It builds and tears down its own data and never touches an existing DB.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

struct Fx {
    org: Uuid,
    kb: Uuid,
    fact: Uuid,
    chunk: Uuid,
}

fn t(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fx> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (etype, rel) = (Uuid::now_v7(), Uuid::now_v7());
    let (subject, object) = (Uuid::now_v7(), Uuid::now_v7());
    let (doc, chunk, fact) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'stale-close-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'stale-close-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name)
         VALUES ($1, $2, 'stale-close-test')",
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
        "INSERT INTO relation_types (id, kb_id, key, label, temporal)
         VALUES ($1, $2, 'leads', 'leads', 'state')",
    )
    .bind(rel)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [(subject, "Akkaraju"), (object, "Weta")] {
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
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256) VALUES ($1, $2, 'a.md', 'sha-a')",
    )
    .bind(doc)
    .bind(kb)
    .execute(pool)
    .await?;
    // First-version chunk: the evidence lands here, and it will be superseded once the document gets a new version
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text, doc_version)
         VALUES ($1, $2, $3, 0, 'Akkaraju leads Weta', 1)",
    )
    .bind(chunk)
    .bind(kb)
    .bind(doc)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, $5, 0.9)",
    )
    .bind(fact)
    .bind(kb)
    .bind(subject)
    .bind(rel)
    .bind(object)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, quote)
         VALUES ($1, $2, 'Akkaraju leads Weta')",
    )
    .bind(fact)
    .bind(chunk)
    .execute(pool)
    .await?;
    Ok(Fx {
        org,
        kb,
        fact,
        chunk,
    })
}

async fn stale_ids(pool: &PgPool, kb: Uuid) -> anyhow::Result<Vec<Uuid>> {
    Ok(utopia_store::graph::stale_facts(pool, kb, 50, 0)
        .await?
        .into_iter()
        .map(|f| f.id)
        .collect())
}

#[tokio::test]
async fn a_fact_closed_from_that_queue_leaves_it() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // The document got a new version and the old chunk was superseded — the only evidence left is the one stuck on the old version, and this bucket should list it
        sqlx::query("UPDATE chunks SET superseded_at = now() WHERE id = $1")
            .bind(f.chunk)
            .execute(&pool)
            .await?;
        assert_eq!(stale_ids(&pool, f.kb).await?, vec![f.fact]);
        assert_eq!(
            utopia_store::review::counts(&pool, f.kb).await?.unconfirmed,
            1
        );

        // A person decides "it ended in January 2021" — month precision is written as-is, and the server truncates it to the 1st of that month
        let corrected = utopia_store::temporal::close_superseded(
            &pool,
            f.fact,
            t("2021-01-01T00:00:00Z"),
            "month",
        )
        .await?
        .expect("closing must rewrite out a correction row");

        // **The correction row carries the same batch of evidence, but it already has an end, so it is no longer "current".**
        // Without this condition it would go straight back into the list — what the user sees is exactly "I clicked close, and the entry is still there"
        assert!(
            stale_ids(&pool, f.kb).await?.is_empty(),
            "闭合过的行要离开这一档"
        );
        assert_eq!(
            utopia_store::review::counts(&pool, f.kb).await?.unconfirmed,
            0,
            "左栏的计数与列表同一套 WHERE"
        );

        // The correction row itself: the evidence was copied across and the end is stated — the criterion relies on the upper-bound clause, not on the evidence being gone
        let copied: i64 =
            sqlx::query_scalar("SELECT count(*) FROM fact_evidence WHERE fact_id = $1")
                .bind(corrected)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            copied, 1,
            "evidence is copied along with the correction row"
        );
        let end: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT valid_to FROM facts WHERE id = $1")
                .bind(corrected)
                .fetch_one(&pool)
                .await?;
        assert!(
            end.is_some(),
            "the correction row's end is stated explicitly"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Deleting an org does not cascade to the KB: delete the KB first, then the org and workspace
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// The converse: a row that "ended on an unknown day" also does not count as current (0022).
/// Looking only at `valid_to IS NULL` would leave this one in the queue — it no longer holds, so asking about it is pointless.
#[tokio::test]
async fn an_ended_unknown_row_is_not_current_either() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        sqlx::query("UPDATE chunks SET superseded_at = now() WHERE id = $1")
            .bind(f.chunk)
            .execute(&pool)
            .await?;
        assert_eq!(stale_ids(&pool, f.kb).await?.len(), 1);

        // The anchor must be supplied together: the 0022 CHECK ties "ended on an unknown day" to the document that states the ending
        sqlx::query(
            "UPDATE facts SET valid_to_precision = 'unknown', attested_to = now() WHERE id = $1",
        )
        .bind(f.fact)
        .execute(&pool)
        .await?;
        assert!(
            stale_ids(&pool, f.kb).await?.is_empty(),
            "结束不知哪天的行不是现行"
        );
        assert_eq!(
            utopia_store::review::counts(&pool, f.kb).await?.unconfirmed,
            0
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Deleting an org does not cascade to the KB: delete the KB first, then the org and workspace
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
