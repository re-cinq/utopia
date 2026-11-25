//! A statement awaiting a person's nod also keeps the document's own words (memory side of 0044 slice 1, migration 0062), run against a real DB.
//!
//! The conversation says "Acme moved its headquarters to Shenzhen", and what is extracted is no longer a triple with an ontology predicate, but an
//! open statement: the phrase is kept as written, with role-word attributes, time words copied verbatim, and the quote's position in the block. It first
//! waits in `pending_facts`; once a person nods, it enters the ledger along the `insert_open_statement` path —
//! `layer = 'open'`, no predicate, no `valid_*` written, evidence carries offsets, and attributes and time words each go to their own table;
//! after a shake of the head, what is blocked is **this sentence** (the phrase is part of the key), so the same pair of entities can still be proposed with a different sentence.
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is not set. It builds and tears down its own data and never touches an existing DB.

use sqlx::PgPool;
use utopia_store::pending::{self, Outcome, Proposal};
use uuid::Uuid;

const SENTENCE: &str = "Acme moved its headquarters to Shenzhen on 2026-03-15.";
const PHRASE: &str = "moved its headquarters to";
const TIME_WORDS: &str = "on 2026-03-15";

struct Fixture {
    /// The org built by the fixture, deleted from first at teardown (cascading)
    org: Uuid,
    kb: Uuid,
    acme: Uuid,
    shenzhen: Uuid,
    chunk: Uuid,
    /// The full text of that memory once stored — with the `[YYYY-MM-DD HH:MM] ` prefix; the offsets are computed against it
    text: String,
}

async fn fixture(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'pending-words-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'pending-words-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'pending-words-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    let (acme, shenzhen) = (Uuid::now_v7(), Uuid::now_v7());
    for (id, name) in [(acme, "Acme"), (shenzhen, "Shenzhen")] {
        sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(kb)
            .bind(name)
            .execute(pool)
            .await?;
    }
    // That memory sentence becomes a chunk through the product's own path
    let (_doc, chunk) =
        utopia_store::memory::append_episode(pool, kb, SENTENCE, chrono::Utc::now()).await?;
    let text: String = sqlx::query_scalar("SELECT text FROM chunks WHERE id = $1")
        .bind(chunk)
        .fetch_one(pool)
        .await?;
    Ok(Fixture {
        org,
        kb,
        acme,
        shenzhen,
        chunk,
        text,
    })
}

/// The character offset of the quote within the block: the number the server computes by searching the text — computed from the text as stored, not from a literal
fn span_of(text: &str, quote: &str) -> (i32, i32) {
    let start = text.find(quote).expect("quote is in the chunk");
    let chars_before = text[..start].chars().count() as i32;
    (chars_before, chars_before + quote.chars().count() as i32)
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
async fn a_pending_statement_keeps_the_documents_words() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = fixture(&pool).await?;

    let run = async {
        let (quote_start, quote_end) = span_of(&f.text, SENTENCE);
        let (time_start, _) = span_of(&f.text, TIME_WORDS);
        assert!(
            quote_start > 0,
            "stored text carries a timestamp prefix, offsets cannot start at 0"
        );
        let qualifiers = serde_json::json!([{ "role": "on", "value": "2026-03-15" }]);
        let time_words = serde_json::json!([{ "text": TIME_WORDS, "char_start": time_start }]);
        let propose = |phrase: &'static str| {
            pending::propose(
                &pool,
                Proposal {
                    kb_id: f.kb,
                    subject_id: f.acme,
                    predicate_id: None,
                    object_id: Some(f.shenzhen),
                    object_value: None,
                    proposed_predicate: Some(phrase),
                    validity: utopia_store::graph::Validity::default(),
                    confidence: 0.9,
                    chunk_id: f.chunk,
                    proposed_by: None,
                    proposed_token: None,
                    phrase: Some(phrase),
                    qualifiers: Some(&qualifiers),
                    time_words: Some(&time_words),
                    quote_span: Some((quote_start, quote_end)),
                },
            )
        };

        // 1. The proposal does not go onto the graph; the one in the queue carries the document's own words
        let first = propose(PHRASE).await?;
        assert!(
            matches!(first, Outcome::Proposed(_)),
            "first time should enter the queue"
        );
        assert_eq!(
            live_facts(&pool, f.kb).await?,
            0,
            "no live edges in the graph during the proposal stage"
        );
        let queued = pending::for_chunk(&pool, f.kb, f.chunk).await?;
        assert_eq!(queued.len(), 1);
        let q = &queued[0];
        assert_eq!(q.subject_name, "Acme");
        assert_eq!(q.object_name.as_deref(), Some("Shenzhen"));
        assert_eq!(q.phrase.as_deref(), Some(PHRASE));
        assert!(q.predicate_id.is_none(), "open statement has no predicate");
        assert_eq!(q.qualifiers.as_ref(), Some(&qualifiers));
        assert_eq!(q.time_words.as_ref(), Some(&time_words));
        assert_eq!(
            (q.quote_start, q.quote_end),
            (Some(quote_start), Some(quote_end))
        );
        assert_eq!(
            q.quote, f.text,
            "the original sentence must be shown to the person along with the proposal"
        );

        // 2. Re-extracting the same sentence does not propose it again
        assert_eq!(propose(PHRASE).await?, Outcome::AlreadyPending);

        // 3. Nod: enters the ledger as an open statement
        let Outcome::Proposed(id) = first else {
            unreachable!()
        };
        let done = pending::confirm(&pool, f.kb, id).await?;
        assert!(done.created, "confirmation should land one new fact");
        assert_eq!(
            done.conflicts, 0,
            "open statements do no temporal reconciliation"
        );
        assert_eq!(done.snapshot["phrase"], PHRASE);
        assert_eq!(done.snapshot["qualifiers"], qualifiers);
        assert_eq!(live_facts(&pool, f.kb).await?, 1);

        #[derive(sqlx::FromRow)]
        struct Row {
            layer: String,
            phrase: Option<String>,
            predicate_id: Option<Uuid>,
            object_id: Option<Uuid>,
            valid_from: Option<chrono::DateTime<chrono::Utc>>,
            valid_to: Option<chrono::DateTime<chrono::Utc>>,
            attested_from: Option<chrono::DateTime<chrono::Utc>>,
        }
        let row: Row = sqlx::query_as(
            "SELECT layer, phrase, predicate_id, object_id, valid_from, valid_to, attested_from
               FROM facts WHERE id = $1",
        )
        .bind(done.fact_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.layer, "open");
        assert_eq!(row.phrase.as_deref(), Some(PHRASE));
        assert!(
            row.predicate_id.is_none(),
            "open statement has no predicate"
        );
        assert_eq!(row.object_id, Some(f.shenzhen));
        assert!(
            row.valid_from.is_none() && row.valid_to.is_none(),
            "开放陈述在世界轴上还没有位置"
        );
        assert!(
            row.attested_from.is_some(),
            "记录轴总是有：记忆文档没有文档日期，落成此刻"
        );

        // Evidence points back at that memory sentence, with the quote's position in the block
        let (ev_chunk, ev_proposed, ev_start, ev_end): (
            Uuid,
            Option<String>,
            Option<i32>,
            Option<i32>,
        ) = sqlx::query_as(
            "SELECT chunk_id, proposed_predicate, quote_start, quote_end
               FROM fact_evidence WHERE fact_id = $1",
        )
        .bind(done.fact_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            ev_chunk, f.chunk,
            "evidence must point back to that memory sentence"
        );
        assert_eq!(
            ev_proposed.as_deref(),
            Some(PHRASE),
            "the surface predicate is the phrase"
        );
        assert_eq!((ev_start, ev_end), (Some(quote_start), Some(quote_end)));
        // The quote is exactly the sentence cut out by the offsets, not the whole block (the whole block carries a timestamp prefix, and would also be cut to 120 characters)
        let (ev_quote, at_span): (Option<String>, String) = sqlx::query_as(
            "SELECT fe.quote, substr(c.text, fe.quote_start + 1, fe.quote_end - fe.quote_start)
               FROM fact_evidence fe JOIN chunks c ON c.id = fe.chunk_id WHERE fe.fact_id = $1",
        )
        .bind(done.fact_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            ev_quote.as_deref(),
            Some(at_span.as_str()),
            "证据引文要和偏移截出的字一样"
        );
        assert_eq!(ev_quote.as_deref(), Some(SENTENCE));

        // Role-word attributes are written with the document's role words
        let qualifier_rows: Vec<(String, Option<serde_json::Value>, Option<Uuid>)> =
            sqlx::query_as(
                "SELECT role, value, entity_id FROM statement_qualifiers WHERE fact_id = $1",
            )
            .bind(done.fact_id)
            .fetch_all(&pool)
            .await?;
        assert_eq!(qualifier_rows.len(), 1);
        assert_eq!(qualifier_rows[0].0, "on");
        assert_eq!(qualifier_rows[0].1, Some(serde_json::json!("2026-03-15")));
        assert!(qualifier_rows[0].2.is_none());

        // Time words are copied verbatim; the offsets can recover those characters within the block
        let mentions: Vec<(String, i32, String)> = sqlx::query_as(
            "SELECT m.text, m.char_start, substr(c.text, m.char_start + 1, length(m.text))
               FROM time_mentions m JOIN chunks c ON c.id = m.chunk_id
              WHERE m.fact_id = $1",
        )
        .bind(done.fact_id)
        .fetch_all(&pool)
        .await?;
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].0, TIME_WORDS);
        assert_eq!(mentions[0].1, time_start);
        assert_eq!(
            mentions[0].2, TIME_WORDS,
            "the offsets point back to those characters in the chunk"
        );

        assert!(
            pending::for_chunk(&pool, f.kb, f.chunk).await?.is_empty(),
            "点头后队列清空"
        );

        // 4. Something already on the graph is not asked about again
        assert_eq!(propose(PHRASE).await?, Outcome::AlreadyAsserted);

        // 5. The shake of the head blocks this sentence: the phrase went into the refusal record; the same pair of entities can still be proposed with another sentence
        let Outcome::Proposed(bad) = propose("closed its office in").await? else {
            panic!("a different phrase should be a new proposal");
        };
        pending::reject(&pool, f.kb, bad, None).await?;
        let rejected: Vec<Option<String>> =
            sqlx::query_scalar("SELECT phrase FROM rejected_facts WHERE kb_id = $1")
                .bind(f.kb)
                .fetch_all(&pool)
                .await?;
        assert_eq!(rejected, vec![Some("closed its office in".to_string())]);
        assert_eq!(propose("closed its office in").await?, Outcome::Rejected);
        assert!(
            matches!(propose("opened a lab in").await?, Outcome::Proposed(_)),
            "拒绝只挡那一句话，不挡这一对实体之间的一切"
        );
        assert_eq!(
            live_facts(&pool, f.kb).await?,
            1,
            "rejection does not touch the graph"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Delete the org the fixture itself created; the cascade takes the workspace, the KB and everything else with it.
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
