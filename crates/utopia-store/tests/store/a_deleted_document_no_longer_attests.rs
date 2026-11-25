//! Recomputing evidence when the document it sits in was deleted (deletion undone, the fact's witness still there): 0064 decisions 3 and 5; the gap left by 0022 revision
//! 2026-10-03.
//!
//! - Each piece of evidence records the moment its passage of original text spoke; delete the earliest document, and the statement's witness moves to the earliest
//!   section date among the evidence that remains, together with the name, not the date of that document;
//! - Remaining evidence that states no dates at all has an empty witness: a deleted document's date no longer vouches for it;
//! - Undo the deletion and the witness comes back exactly as before;
//! - Evidence with no recorded section date (written before this column existed) is read by the document's own date, as before;
//! - Typed rows move with the statement they follow; source statements voided along with a document no longer vouch for them.
//!
//! Without `UTOPIA_DATABASE_URL` it skips rather than fails. It sets up and tears down its own data and never touches an existing database.

use chrono::{DateTime, TimeZone, Utc};
use sqlx::PgPool;
use utopia_store::graph::{self, FactObject};
use uuid::Uuid;

const ORG: &str = "deleted-document-no-longer-attests-test";

fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
}

type Anchor = (Option<DateTime<Utc>>, Option<String>);
/// The date of the section that one piece of evidence sits in, and its name; `None` when that section states no date
type Witness<'a> = Option<(DateTime<Utc>, &'a str)>;

async fn anchor(pool: &PgPool, fact: Uuid) -> anyhow::Result<Anchor> {
    Ok(
        sqlx::query_as("SELECT attested_from, attested_by FROM facts WHERE id = $1")
            .bind(fact)
            .fetch_one(pool)
            .await?,
    )
}

/// One document and one block of its body. `own` is the document's own date (stated in the body); None if none was stated
async fn document(
    pool: &PgPool,
    kb: Uuid,
    own: Option<DateTime<Utc>>,
) -> anyhow::Result<(Uuid, Uuid)> {
    let (doc, chunk) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, doc_time, doc_time_source)
         VALUES ($1, $2, $3, $3, $4, $5)",
    )
    .bind(doc)
    .bind(kb)
    .bind(format!("doc-{doc}.md"))
    .bind(own)
    .bind(if own.is_some() { "content" } else { "none" })
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, $4)",
    )
    .bind(chunk)
    .bind(kb)
    .bind(doc)
    .bind("# 年报\n码表所属市场为全球。")
    .execute(pool)
    .await?;
    Ok((doc, chunk))
}

/// One open statement, with evidence in these blocks; the entries of evidence that are `Some` record a section date, and the statement is witnessed by the earliest
async fn statement(
    pool: &PgPool,
    kb: Uuid,
    subject: Uuid,
    object: Uuid,
    phrase: &str,
    evidence: &[(Uuid, Witness<'_>)],
) -> anyhow::Result<Uuid> {
    let (id, _) = graph::insert_open_statement(
        pool,
        kb,
        subject,
        phrase,
        FactObject::Entity(object),
        None,
        0.9,
    )
    .await?;
    for (chunk, witness) in evidence {
        graph::add_evidence(pool, id, *chunk, Some("码表所属市场为全球"), None).await?;
        graph::witness_evidence(pool, id, *chunk, *witness).await?;
        if let Some((at, by)) = witness {
            graph::attest_statement(pool, id, *at, by).await?;
        }
    }
    Ok(id)
}

/// One typed-fact row materialized from these statements: records the source, copies their evidence
async fn typed(
    pool: &PgPool,
    kb: Uuid,
    subject: Uuid,
    object: Uuid,
    predicate: Uuid,
    sources: &[Uuid],
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, predicate_id, layer, from_statement_id)
         VALUES ($1, $2, $3, $4, $5, 'typed', $6)",
    )
    .bind(id)
    .bind(kb)
    .bind(subject)
    .bind(object)
    .bind(predicate)
    .bind(sources[0])
    .execute(pool)
    .await?;
    for source in sources {
        sqlx::query("INSERT INTO typed_fact_sources (fact_id, statement_id) VALUES ($1, $2)")
            .bind(id)
            .bind(source)
            .execute(pool)
            .await?;
        sqlx::query(
            "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, doc_version,
                                        proposed_predicate, quote_start, quote_end)
             SELECT $1, chunk_id, quote, document_id, doc_version, proposed_predicate,
                    quote_start, quote_end
               FROM fact_evidence WHERE fact_id = $2
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(source)
        .execute(pool)
        .await?;
    }
    utopia_store::materialize::sync_typed_attestation(pool, kb, None).await?;
    Ok(id)
}

#[tokio::test]
async fn attestation_follows_the_evidence_that_remains() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(ORG)
        .execute(&pool)
        .await?;
    let (org, ws, kb, watch, market, region, sold_in) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','{ORG}');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','{ORG}');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','{ORG}');
         INSERT INTO entities(id,kb_id,canonical_name) VALUES
             ('{watch}','{kb}','码表'), ('{market}','{kb}','全球'), ('{region}','{kb}','欧洲');
         INSERT INTO relation_types(id,kb_id,key,label,kind,temporal,description) VALUES
             ('{sold_in}','{kb}','sold_in','sold in','relation','state','');"
    ))
    .execute(&pool)
    .await?;

    let run = async {
        let (chapter_2015, chapter_2020) = (day(2015, 6, 1), day(2020, 6, 1));
        let (by_2015, by_2020) = ("2015 年年报", "2020 年年报");
        let late_own_date = day(2026, 3, 1);
        // The earlier document's own date is January 2026, its section says 2015; the later one is March 2026, its section says 2020;
        // the third states no date at all
        let (early, early_chunk) = document(&pool, kb, Some(day(2026, 1, 10))).await?;
        let (_late, late_chunk) = document(&pool, kb, Some(late_own_date)).await?;
        let (_bare, bare_chunk) = document(&pool, kb, None).await?;
        let from_2015 = Some((chapter_2015, by_2015));
        let from_2020 = Some((chapter_2020, by_2020));

        // Both state one: the earliest section vouches
        let both = statement(
            &pool,
            kb,
            watch,
            market,
            "所属市场为",
            &[(early_chunk, from_2015), (late_chunk, from_2020)],
        )
        .await?;
        // The other source states no date at all
        let beside_undated = statement(
            &pool,
            kb,
            watch,
            market,
            "销往",
            &[(early_chunk, from_2015), (bare_chunk, None)],
        )
        .await?;
        // Only in the earlier document: voided along with it
        let only_early = statement(
            &pool,
            kb,
            watch,
            region,
            "所属市场为",
            &[(early_chunk, from_2015)],
        )
        .await?;
        // Evidence written before this column existed: no section date recorded, the witness was given by the document's date at the time
        let legacy = statement(
            &pool,
            kb,
            watch,
            region,
            "销往",
            &[(early_chunk, None), (late_chunk, None)],
        )
        .await?;
        sqlx::query("UPDATE facts SET attested_from = $2 WHERE id = $1")
            .bind(legacy)
            .bind(day(2026, 1, 10))
            .execute(&pool)
            .await?;
        // Typed rows: one row follows the one both documents state; a row whose two sources include one that is only in the earlier document
        let follows_both = typed(&pool, kb, watch, market, sold_in, &[both]).await?;
        let late_only = statement(
            &pool,
            kb,
            watch,
            region,
            "在售于",
            &[(late_chunk, from_2020)],
        )
        .await?;
        let mixed_sources = typed(&pool, kb, watch, region, sold_in, &[only_early, late_only]).await?;

        let earliest: Anchor = (Some(chapter_2015), Some(by_2015.to_string()));
        for fact in [both, beside_undated, follows_both, mixed_sources] {
            assert_eq!(anchor(&pool, fact).await?, earliest, "before the deletion");
        }

        utopia_store::documents::delete(&pool, kb, early, None).await?;

        let remaining: Anchor = (Some(chapter_2020), Some(by_2020.to_string()));
        assert_eq!(
            anchor(&pool, both).await?,
            remaining,
            "the chapter date of the evidence that remains, with its name, not that document's own date"
        );
        assert_eq!(
            anchor(&pool, beside_undated).await?,
            (None, None),
            "what remains states no date: the deleted document's date no longer attests"
        );
        assert_eq!(
            anchor(&pool, legacy).await?,
            (Some(late_own_date), None),
            "evidence with no recorded chapter date reads the document's own date, as before"
        );
        assert_eq!(anchor(&pool, follows_both).await?, remaining, "a typed row follows its statement");
        let gone: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT invalidated_at FROM facts WHERE id = $1")
                .bind(only_early)
                .fetch_one(&pool)
                .await?;
        assert!(gone.is_some(), "a statement whose only source is deleted is invalidated");
        assert_eq!(
            anchor(&pool, mixed_sources).await?,
            remaining,
            "a source statement invalidated with the document no longer attests the typed row"
        );

        utopia_store::documents::restore(&pool, kb, early).await?;
        for fact in [both, beside_undated, follows_both, mixed_sources] {
            assert_eq!(anchor(&pool, fact).await?, earliest, "after the restore");
        }
        assert_eq!(
            anchor(&pool, legacy).await?,
            (Some(day(2026, 1, 10)), None),
            "legacy evidence reads the earliest own date again"
        );
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
