//! The typed graph is a view (0044 decision 3, 0067, 0068): an open statement under a bound signature becomes a typed row;
//! several statements saying the same thing are one row; when the binding changes the row follows; the result is the same however many times it runs.
//!
//! One statement "Harbor Bakery —is based in→ Port Ellen", with signature (is based in, organization,
//! place) bound to headquartered_in: a typed row is computed, its predicate is that, evidence and qualifiers are copied over,
//! `from_statement_id` points back to the statement; running again changes nothing; statements with a mood do not count; the same sentence from another document
//! merges into the same row (two sources, two pieces of evidence); the other `since` it states does not follow in, the row keeps the one from the creating statement, and the `until` it states, which the row did not yet have, does follow; voiding one of the statements leaves the row, with one fewer source;
//! the binding flipped to reverse: the old row is voided, and the new row has subject and object swapped; the binding changed to none: the row is voided and not filled in again;
//! a statement with a mood re-pushed, where the new chunk no longer states the mood, becomes a row.
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use sqlx::PgPool;
use utopia_store::materialize::{materialize, Outcome};
use uuid::Uuid;

#[tokio::test]
async fn a_bound_statement_becomes_a_typed_fact() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = 'materialize-test'")
        .execute(&pool)
        .await?;

    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'materialize-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'materialize-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'materialize-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    let run = async {
        // Two classes, one attribute
        let (organization, place, hq) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        for (id, key) in [(organization, "organization"), (place, "place")] {
            sqlx::query(
                "INSERT INTO entity_types (id, kb_id, key, label, color, shape)
                 VALUES ($1, $2, $3, $3, '#7fd0ff', 'circle')",
            )
            .bind(id)
            .bind(kb)
            .bind(key)
            .execute(&pool)
            .await?;
        }
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, temporal)
             VALUES ($1, $2, 'headquartered_in', 'headquartered in', 'state')",
        )
        .bind(hq)
        .bind(kb)
        .execute(&pool)
        .await?;

        // Two things; three statements: an ordinary one, one with a mood, and one from another document saying the same thing
        let (bakery, port) = (Uuid::now_v7(), Uuid::now_v7());
        for (id, type_id, name) in [(bakery, organization, "Harbor Bakery"), (port, place, "Port Ellen")] {
            sqlx::query(
                "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(kb)
            .bind(type_id)
            .bind(name)
            .execute(&pool)
            .await?;
        }
        // First a bare statement with no time arrives, then two with times: the bare row is superseded by the timed row (supersedes),
        // and the sources must move along to the new row
        let bare = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, object_id, layer, phrase, confidence)
             VALUES ($1, $2, $3, $4, 'open', 'operates from', 0.9)",
        )
        .bind(bare)
        .bind(kb)
        .bind(bakery)
        .bind(port)
        .execute(&pool)
        .await?;
        let (stated, planned, again) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        for (id, phrase) in [(stated, "is based in"), (planned, "will move to"), (again, "is based in")] {
            sqlx::query(
                "INSERT INTO facts (id, kb_id, subject_id, object_id, layer, phrase, confidence,
                                    valid_from, valid_from_precision, valid_from_grade)
                 VALUES ($1, $2, $3, $4, 'open', $5, 0.9, '2019-01-01', 'year', 'B')",
            )
            .bind(id)
            .bind(kb)
            .bind(bakery)
            .bind(port)
            .bind(phrase)
            .execute(&pool)
            .await?;
        }
        let evidence_of = |statement: Uuid, name: &'static str| {
            let pool = pool.clone();
            async move {
                let (doc, chunk) = (Uuid::now_v7(), Uuid::now_v7());
                sqlx::query("INSERT INTO documents (id, kb_id, filename, sha256) VALUES ($1, $2, $3, $3)")
                    .bind(doc)
                    .bind(kb)
                    .bind(name)
                    .execute(&pool)
                    .await?;
                sqlx::query(
                    "INSERT INTO chunks (id, kb_id, document_id, seq, text)
                     VALUES ($1, $2, $3, 0, 'Harbor Bakery is based in Port Ellen.')",
                )
                .bind(chunk)
                .bind(kb)
                .bind(doc)
                .execute(&pool)
                .await?;
                sqlx::query(
                    "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, quote_start, quote_end)
                     VALUES ($1, $2, 'Harbor Bakery is based in Port Ellen.', $3, 0, 37)",
                )
                .bind(statement)
                .bind(chunk)
                .bind(doc)
                .execute(&pool)
                .await?;
                anyhow::Ok(chunk)
            }
        };
        let stated_chunk = evidence_of(stated, "bakery.txt").await?;
        let again_chunk = evidence_of(again, "bakery-2.txt").await?;
        let planned_chunk = evidence_of(planned, "bakery-plan.txt").await?;
        // The statement merged in also states an until: the row does not have this role, so it must follow in
        for (statement, chunk, role, value) in [
            (stated, stated_chunk, "since", "2019"),
            (again, again_chunk, "since", "2021"),
            (again, again_chunk, "until", "2024"),
        ] {
            sqlx::query(
                "INSERT INTO statement_qualifiers (fact_id, role, value, chunk_id)
                 VALUES ($1, $3, to_jsonb($4::text), $2)",
            )
            .bind(statement)
            .bind(chunk)
            .bind(role)
            .bind(value)
            .execute(&pool)
            .await?;
        }
        sqlx::query(
            "INSERT INTO statement_qualifiers (fact_id, role, value, chunk_id)
             VALUES ($1, 'mood', '\"will\"'::jsonb, $2)",
        )
        .bind(planned)
        .bind(planned_chunk)
        .execute(&pool)
        .await?;

        // Both signatures bound to headquartered_in
        let binding = |phrase: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO phrase_bindings
                         (id, kb_id, phrase, subject_type_id, object_type_id, object_is_value,
                          relation_type_id, direction, status)
                     VALUES ($1, $2, $3, $4, $5, false, $6, 'forward', 'bound')",
                )
                .bind(Uuid::now_v7())
                .bind(kb)
                .bind(phrase)
                .bind(organization)
                .bind(place)
                .bind(hq)
                .execute(&pool)
                .await
            }
        };
        binding("is based in").await?;
        binding("will move to").await?;
        binding("operates from").await?;

        // Three ordinary statements saying the same thing: the bare one becomes a row first, the timed one supersedes it (added 2, old row voided,
        // sources moved over), the third merges in (merged 1) — final: one row, three sources, two pieces of evidence; the one with a mood
        // does not count
        let first = materialize(&pool, kb).await?;
        assert_eq!(first, Outcome { retired: 0, added: 2, merged: 1, implied: 0, corrected: 0, conflicts: 0 });
        let live = |pool: PgPool| async move {
            sqlx::query_as::<_, (Uuid, Uuid, Uuid, Uuid, Uuid, Option<chrono::DateTime<chrono::Utc>>, Option<String>)>(
                "SELECT id, subject_id, object_id, predicate_id, from_statement_id, valid_from,
                        valid_from_grade
                   FROM facts WHERE kb_id = $1 AND layer = 'typed' AND invalidated_at IS NULL",
            )
            .bind(kb)
            .fetch_all(&pool)
            .await
        };
        let rows = live(pool.clone()).await?;
        assert_eq!(rows.len(), 1, "同一个三元组只有一行");
        let (typed_id, subject, object, predicate, from, vf, grade) = rows[0].clone();
        assert_eq!((subject, object, predicate, from), (bakery, port, hq, stated));
        assert!(vf.is_some(), "世界轴时间抄过来");
        // How the start came about is copied over as well: the temporal engine judges on typed rows, and without a level it would have to guess again (0045 slice 3)
        assert_eq!(grade.as_deref(), Some("B"), "起点的来历随日期一起抄过来");
        let sources: i64 =
            sqlx::query_scalar("SELECT count(*) FROM typed_fact_sources WHERE fact_id = $1")
                .bind(typed_id)
                .fetch_one(&pool)
                .await?;
        let evidence: i64 =
            sqlx::query_scalar("SELECT count(*) FROM fact_evidence WHERE fact_id = $1")
                .bind(typed_id)
                .fetch_one(&pool)
                .await?;
        // The statement merged in says since 2021: the row already has a since, so it is not copied; only the 2019 of the creating statement is read;
        // the until it states, which the row does not yet have, follows in
        let qualifiers: Vec<(String, String, Uuid)> = sqlx::query_as(
            "SELECT role, value #>> '{}', chunk_id FROM statement_qualifiers
              WHERE fact_id = $1 ORDER BY role",
        )
        .bind(typed_id)
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            (sources, evidence, qualifiers),
            (
                3,
                2,
                vec![
                    ("since".to_string(), "2019".to_string(), stated_chunk),
                    ("until".to_string(), "2024".to_string(), again_chunk),
                ]
            )
        );

        // Run again: nothing changes
        assert_eq!(materialize(&pool, kb).await?, Outcome::default());

        // One document retracts its sentence: the row stays, with one fewer source
        sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
            .bind(again)
            .execute(&pool)
            .await?;
        assert_eq!(materialize(&pool, kb).await?, Outcome::default());
        let sources: i64 =
            sqlx::query_scalar("SELECT count(*) FROM typed_fact_sources WHERE fact_id = $1")
                .bind(typed_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(sources, 2);
        assert_eq!(live(pool.clone()).await?.len(), 1, "还有来源，行留着");

        // Binding flipped to reverse: old row voided, new row has subject and object swapped
        sqlx::query("UPDATE phrase_bindings SET direction = 'reverse' WHERE kb_id = $1 AND phrase IN ('is based in', 'operates from')")
            .bind(kb)
            .execute(&pool)
            .await?;
        // All sources of the old row no longer hold: 1 voided; on the reverse recompute the bare one becomes a row first, and the timed one then supersedes it: 2 created
        assert_eq!(materialize(&pool, kb).await?, Outcome { retired: 1, added: 2, merged: 0, implied: 0, corrected: 0, conflicts: 0 });
        let rows = live(pool.clone()).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].1, rows[0].2), (port, bakery), "方向反了主宾对调");
        let retired: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM facts WHERE from_statement_id = $1 AND invalidated_at IS NOT NULL",
        )
        .bind(stated)
        .fetch_one(&pool)
        .await?;
        assert_eq!(retired, 1, "旧行留着，只是作废了");

        // Binding changed to none: row voided, not filled in again
        sqlx::query(
            "UPDATE phrase_bindings SET status = 'none', relation_type_id = NULL, direction = NULL
              WHERE kb_id = $1 AND phrase IN ('is based in', 'operates from')",
        )
        .bind(kb)
        .execute(&pool)
        .await?;
        assert_eq!(materialize(&pool, kb).await?, Outcome { retired: 1, added: 0, merged: 0, implied: 0, corrected: 0, conflicts: 0 });
        assert_eq!(utopia_store::materialize::count(&pool, kb).await?, 0);

        // The statement with a mood is re-pushed, and the new chunk no longer states the mood: the mood on the old chunk no longer blocks it, and it becomes a row (0070 decision 2)
        sqlx::query("UPDATE chunks SET superseded_at = now() WHERE id = $1")
            .bind(planned_chunk)
            .execute(&pool)
            .await?;
        evidence_of(planned, "bakery-plan-2.txt").await?;
        assert_eq!(materialize(&pool, kb).await?.added, 1);
        assert_eq!(utopia_store::materialize::count(&pool, kb).await?, 1);
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
