//! A fact with no predicate **must not vanish from the read paths** (see `facts.predicate_id`).
//!
//! Why a real database is required: after `facts.predicate_id` was made nullable, the twenty-odd read queries'
//! `JOIN relation_types` all became silent filters — an inner join drops NULL rows without an error and
//! without a warning, and `cargo check` and clippy cannot see a single character of it. This is the same trap as
//! `NULL <> uuid` from 0009, in two guises: under three-valued logic, "no value" gets treated as "no match".
//!
//! This test guards that line: **create a fact with no predicate, then require every read path to still see it**.
//! If any `LEFT JOIN relation_types` is changed back to `JOIN`, this must go red.
//!
//! Incidentally it also guards the display rule: `fact_surface_predicate` takes the most frequent phrasing,
//! so the same fact has the same name on the graph, the entity panel, and the change history.
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is unset. Creates and tears down its own; never touches an existing database.

use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    kb: Uuid,
    subject: Uuid,
    doc: Uuid,
    /// A fact with no predicate, but with the original phrasing kept in its evidence
    surfaced: Uuid,
    /// A fact with no predicate and no original phrasing either (older history left over looks like this)
    mute: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let etype = Uuid::now_v7();
    let (subject, object) = (Uuid::now_v7(), Uuid::now_v7());
    let (src, doc, chunk_a, chunk_b) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (surfaced, mute) = (Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'no-predicate-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'no-predicate-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'no-predicate-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'org', '组织')")
        .bind(etype)
        .bind(kb)
        .execute(pool)
        .await?;
    for (id, name) in [(subject, "Acme"), (object, "Beta")] {
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

    sqlx::query("INSERT INTO sources (id, kb_id, name) VALUES ($1, $2, '临时来源')")
        .bind(src)
        .bind(kb)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status)
         VALUES ($1, $2, $3, 'note.md', 'nopredicate', 'ready')",
    )
    .bind(doc)
    .bind(kb)
    .bind(src)
    .execute(pool)
    .await?;
    for (id, seq, text) in [
        (chunk_a, 0i32, "Acme acquired Beta."),
        (chunk_b, 1i32, "Acme acquired Beta again."),
    ] {
        sqlx::query(
            "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(kb)
        .bind(doc)
        .bind(seq)
        .bind(text)
        .execute(pool)
        .await?;
    }

    // Neither fact has a predicate — the ontology has no matching relation, which is exactly the state a nullable predicate exists to express
    for (id, from) in [
        (surfaced, "2020-01-01T00:00:00Z"),
        (mute, "2021-01-01T00:00:00Z"),
    ] {
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                                valid_from, valid_from_precision, confidence)
             VALUES ($1, $2, $3, NULL, $4, $5, 'day', 0.9)",
        )
        .bind(id)
        .bind(kb)
        .bind(subject)
        .bind(object)
        .bind(from.parse::<chrono::DateTime<chrono::Utc>>()?)
        .execute(pool)
        .await?;
    }

    // surfaced has three pieces of evidence: acquired twice, bought once. The majority is acquired —
    // this also checks fact_surface_predicate's "take the most frequent one".
    // mute has only one piece of evidence with no original phrasing, simulating old data from before add_evidence recorded unconditionally
    for (fact, chunk, pred) in [
        (surfaced, chunk_a, Some("acquired")),
        (surfaced, chunk_b, Some("acquired")),
        (surfaced, chunk_a, Some("bought")),
        (mute, chunk_a, None),
    ] {
        sqlx::query(
            "INSERT INTO fact_evidence (fact_id, chunk_id, document_id, proposed_predicate, quote)
             VALUES ($1, $2, $3, $4, 'Acme acquired Beta.')
             ON CONFLICT DO NOTHING",
        )
        .bind(fact)
        .bind(chunk)
        .bind(doc)
        .bind(pred)
        .execute(pool)
        .await?;
    }

    Ok(Fixture {
        kb,
        subject,
        doc,
        surfaced,
        mute,
    })
}

#[tokio::test]
async fn a_fact_without_a_predicate_is_still_visible_everywhere() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    // **Sweep up before starting.** An assertion panic skips the teardown below (it only catches the Err path),
    // so a failed run would leave garbage for the next one. Cleaning up is cheaper than changing every assertion to return Err
    sqlx::query("DELETE FROM organizations WHERE name = 'no-predicate-test'")
        .execute(&pool)
        .await?;
    let f = seed(&pool).await?;

    let run = async {
        // 1. Display rule: the majority wins, and it is not a random one
        let word: Option<String> = sqlx::query_scalar("SELECT fact_surface_predicate($1)")
            .bind(f.surfaced)
            .fetch_one(&pool)
            .await?;
        assert_eq!(word.as_deref(), Some("acquired"), "should take the most frequent phrasing");

        let none: Option<String> = sqlx::query_scalar("SELECT fact_surface_predicate($1)")
            .bind(f.mute)
            .fetch_one(&pool)
            .await?;
        assert!(none.is_none(), "when there is not even a source phrasing, it should be empty, not an invented word");

        // 2. The graph edges: both must be present, and both marked inferred
        let (_, edges) =
            utopia_store::graph::neighborhood(&pool, f.kb, f.subject, 1, None, None).await?;
        let ours: Vec<_> = edges
            .iter()
            .filter(|e| e.id == f.surfaced || e.id == f.mute)
            .collect();
        assert_eq!(ours.len(), 2, "an inner join would swallow the whole edge that has no predicate");
        let e = ours.iter().find(|e| e.id == f.surfaced).unwrap();
        assert_eq!(e.label.as_deref(), Some("acquired"));
        assert!(e.inferred, "the ontology did not confirm it, so the UI must show the word comes from the source");
        let m = ours.iter().find(|e| e.id == f.mute).unwrap();
        assert!(m.label.is_none(), "if it cannot be named, it is just unnamed; it must not fall back to 'related'");

        // 3. Entity panel
        let (_, facts) =
            utopia_store::graph::entity_detail(&pool, f.kb, f.subject, None, None).await?;
        let panel: Vec<_> = facts
            .iter()
            .filter(|x| x.id == f.surfaced || x.id == f.mute)
            .collect();
        assert_eq!(panel.len(), 2, "the entity panel missed facts without a predicate");
        let s = panel.iter().find(|x| x.id == f.surfaced).unwrap();
        assert_eq!(s.predicate_label.as_deref(), Some("acquired"));
        assert!(s.inferred);
        assert!(s.temporal.is_none(), "without a predicate there is no temporal category");

        // 4. Document output (DocViewer reviewing block by block)
        let chunk_facts = utopia_store::graph::document_extractions(&pool, f.doc).await?;
        assert!(
            chunk_facts.iter().any(|c| c.fact_id == f.surfaced),
            "文档页看不见没有谓词的事实"
        );
        assert!(
            chunk_facts.iter().any(|c| c.fact_id == f.mute),
            "文档页看不见没有说法的事实"
        );

        assert!(chunk_facts.iter().all(|c| !c.is_open_statement),
            "历史投影即便有原文说法，也不能用投影 ID 添加陈述案例");

        // The one whose object is a literal: when the reading page's right column only recognized canonical_name, it showed up on screen as
        // subject plus phrase with the rest blank ("Hugging Face, Inc. known as")
        let valued = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, layer, phrase, object_value, confidence)
             VALUES ($1, $2, $3, 'open', 'known as', jsonb_build_object('value', 'Acme Inc.'), 0.9)",
        )
        .bind(valued)
        .bind(f.kb)
        .bind(f.subject)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO fact_evidence (fact_id, chunk_id, document_id, quote)
             SELECT $1, chunk_id, document_id, 'Acme Inc.' FROM fact_evidence WHERE fact_id = $2 LIMIT 1",
        )
        .bind(valued)
        .bind(f.surfaced)
        .execute(&pool)
        .await?;
        let chunk_facts = utopia_store::graph::document_extractions(&pool, f.doc).await?;
        let v = chunk_facts.iter().find(|c| c.fact_id == valued).unwrap();
        assert_eq!(v.object.as_deref(), Some("Acme Inc."), "the value object is empty on the document page");
        assert!(v.object_id.is_none(), "a literal value has no entity to jump to");
        assert!(v.is_open_statement, "a raw statement with a phrase can have cases added");
        assert_eq!(v.predicate.as_deref(), Some("known as"), "confirmed cases must show the source phrase");

        // 5. Entity history (the outer FROM is a CTE; a wrong alias fails directly with missing FROM-clause)
        let (hist, _) = utopia_store::graph::entity_history(&pool, f.kb, f.subject, 50, 0).await?;
        let h = hist.iter().find(|e| e.fact_id == Some(f.surfaced));
        assert!(h.is_some(), "entity history missed facts without a predicate");
        assert_eq!(h.unwrap().predicate_label.as_deref(), Some("acquired"));

        // 6. Ledger change feed
        let changes = utopia_store::graph::graph_changes(
            &pool,
            f.kb,
            "2000-01-01T00:00:00Z".parse()?,
            "2100-01-01T00:00:00Z".parse()?,
            None,
            None,
            50,
            0,
        )
        .await?;
        let c = changes.iter().find(|c| c.fact_id == f.surfaced);
        assert!(c.is_some(), "the change feed missed facts without a predicate");
        assert_eq!(c.unwrap().predicate_label.as_deref(), Some("acquired"));

        // 7. **No mechanism can bring it back any more.**
        //
        // The history of this one is worth keeping: the first version was an empty assertion (it only ran `SELECT … WHERE builtin`,
        // and the fixture built the database with bare SQL, never seeded, so the assertion held on bare ground);
        // the second version added a control group — first call `ensure_default_ontology` to plant the seed,
        // then confirm `related_to` is not in it. Because seven minutes after deletion, the code had planted the row back once.
        //
        // Now **even the seeding function is gone**: 0009 removed the built-in entity classes, 0010 and `#125` removed the seed relations,
        // 0011 moved `mapped_to` into `concept_mappings`, and `ensure_default_ontology`
        // retired along with them. So here we guard the stronger property: **no code on the database-creation path forces any relation in**,
        // and the `builtin` column is always empty in a new database.
        let seeded: Vec<String> =
            sqlx::query_scalar("SELECT key FROM relation_types WHERE kb_id = $1 AND builtin")
                .bind(f.kb)
                .fetch_all(&pool)
                .await?;
        assert!(
            seeded.is_empty(),
            "本体里不该有代码硬塞进去的关系，实得 {seeded:?}"
        );

        // Scoped to the KB only, not the whole database: a whole-database count would be polluted by other tests running in parallel and by leftovers from the previous run,
        // which would be an assertion that turns red for no reason. This one already guards what it should —
        // once the seed table is planted, the ontology contains no "says nothing" relation for the model to pick

        Ok::<(), anyhow::Error>(())
    }
    .await;

    // Clean up the temporary data whether or not the assertion blew up (everything below is ON DELETE CASCADE)
    sqlx::query("DELETE FROM organizations WHERE name = 'no-predicate-test'")
        .execute(&pool)
        .await?;
    run
}
