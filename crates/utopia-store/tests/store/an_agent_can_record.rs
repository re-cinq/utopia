//! A memory proposed through MCP must be recognizable on the review card as **which agent** it came from (#304 / 0026).
//!
//! Why this needs a database: the whole identity chain lives in SQL -- one foreign key column, one LEFT JOIN,
//! and one `AS` alias. A wrong column name, a wrong join table, or an alias that does not match the `FromRow` field
//! is said nothing about by `cargo check`; the interface just quietly shows half a line less.
//!
//! Both cases must be asserted, because they break in different ways:
//! - A proposal made with a token: the view must show the token's name (a missing join → always empty)
//! - A proposal made without a token (web chat): that field must be empty (an inner join → the whole row disappears)
//!
//! It creates and tears down its own data: a one-off org / workspace / kb, removed along with the org when done.

use sqlx::PgPool;
use utopia_store::graph::Validity;
use utopia_store::pending::{Outcome, Proposal};
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    /// The entry recorded by a token
    by_agent: Uuid,
    /// The entry recorded by a web chat
    by_person: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (user, token) = (Uuid::now_v7(), Uuid::now_v7());
    let (etype, doc, chunk) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (acme, zenith) = (Uuid::now_v7(), Uuid::now_v7());
    let tag = Uuid::now_v7();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'agent-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'agent-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'agent-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    // Email must be unique: a one-off suffix keeps it from colliding with accounts left in the database
    sqlx::query(
        "INSERT INTO users (id, org_id, email, password_hash, display_name)
         VALUES ($1, $2, $3, 'x', 'Zhang San')",
    )
    .bind(user)
    .bind(org)
    .bind(format!("agent-test-{tag}@utopia.test"))
    .execute(pool)
    .await?;
    // token_hash must be unique too
    sqlx::query(
        "INSERT INTO personal_tokens (id, user_id, name, token_hash, token_prefix, scope)
         VALUES ($1, $2, 'Meeting notes agent', $3, 'utp_pat_test', 'write')",
    )
    .bind(token)
    .bind(user)
    .bind(format!("hash-{tag}"))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'org', 'Organization')",
    )
    .bind(etype)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [(acme, "Acme"), (zenith, "Zenith")] {
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
    // Blobs are shared across libraries by sha, so the sha also gets a one-off suffix
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, status)
         VALUES ($1, $2, 'memory-log.md', $3, 'ready')",
    )
    .bind(doc)
    .bind(kb)
    .bind(format!("sha-{tag}"))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text)
         VALUES ($1, $2, $3, 0, 'Acme partnered with Zenith on 2026-03-01.')",
    )
    .bind(chunk)
    .bind(kb)
    .bind(doc)
    .execute(pool)
    .await?;

    let propose = |object_id: Uuid, proposed_token: Option<Uuid>| {
        let p = Proposal {
            kb_id: kb,
            subject_id: acme,
            predicate_id: None,
            object_id: Some(object_id),
            object_value: None,
            proposed_predicate: Some("partnered with"),
            validity: Validity::default(),
            confidence: 0.6,
            chunk_id: chunk,
            proposed_by: Some(user),
            proposed_token,
            phrase: None,
            qualifiers: None,
            time_words: None,
            quote_span: None,
        };
        async move {
            match utopia_store::pending::propose(pool, p).await? {
                Outcome::Proposed(id) => Ok::<Uuid, anyhow::Error>(id),
                other => anyhow::bail!("proposal was not queued: {other:?}"),
            }
        }
    };
    let by_agent = propose(zenith, Some(token)).await?;
    // The same (subject, predicate, object) would be judged AlreadyPending, so the second one uses a different object
    let by_person = propose(acme, None).await?;

    Ok(Fixture {
        org,
        kb,
        by_agent,
        by_person,
    })
}

#[tokio::test]
async fn a_pending_fact_names_the_agent_that_proposed_it() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let queue = utopia_store::pending::list(&pool, f.kb, 50, 0).await?;
    let find = |id: Uuid| {
        queue
            .iter()
            .find(|v| v.id == id)
            .expect("the item awaiting confirmation is not in the queue")
    };

    // 1. The entry recorded by a token: both a person and an agent can answer for it. The person is the identity; the agent is "which client" --
    //    when one person has three agents, only the latter is distinguishable on the card
    let agent = find(f.by_agent);
    assert_eq!(agent.proposed_by_name.as_deref(), Some("Zhang San"));
    assert_eq!(
        agent.proposed_token_name.as_deref(),
        Some("Meeting notes agent"),
        "令牌名没跟出来——多半是 VIEW_SELECT 少了那个 join"
    );

    // 2. The entry recorded by a web chat: there is no agent, but **this entry itself must not disappear**
    //    (if the join became an inner join the whole row would vanish, and that is the hardest kind of loss to spot)
    let person = find(f.by_person);
    assert_eq!(person.proposed_by_name.as_deref(), Some("Zhang San"));
    assert_eq!(person.proposed_token_name, None);

    // **Delete the library first, then the org.** Deleting the org cascades to users, and `pending_facts.proposed_by`
    // is a foreign key without ON DELETE (the ledger should block exactly this kind of delete); the library does not cascade with the org,
    // so if the order were reversed, the foreign key would push back
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    let gone = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    assert_eq!(gone.rows_affected(), 1, "one-off org was not deleted");
    Ok(())
}
