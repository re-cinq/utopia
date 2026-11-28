//! Section attestation after materialization reconciles the real timeline without an LLM.
use super::*;
use serde_json::json;
use std::sync::Arc;
use utopia_store::graph::{self, FactObject};

struct Fixture {
    state: AppState,
    org: Uuid,
    kb: Uuid,
    doc: Uuid,
    subject: Uuid,
    _dir: tempfile::TempDir,
}

async fn fixture() -> anyhow::Result<Option<Fixture>> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(None);
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let (org, workspace, kb, doc, subject, property, binding) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    // Only fixture UUIDs are interpolated into SQL.
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','section-attestation');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{workspace}','{org}','section-attestation');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{workspace}','section-attestation');
         INSERT INTO documents(id,kb_id,filename,sha256,doc_time,doc_time_source)
             VALUES ('{doc}','{kb}','chapters.md','{doc}','2030-01-01','source');
         INSERT INTO entities(id,kb_id,canonical_name) VALUES ('{subject}','{kb}','Widget');
         INSERT INTO relation_types(id,kb_id,key,label,kind,datatype,temporal,functional)
             VALUES ('{property}','{kb}','location','Location','attribute','text','state',TRUE);
         INSERT INTO phrase_bindings(id,kb_id,phrase,object_is_value,relation_type_id,direction,status)
             VALUES ('{binding}','{kb}','is on',TRUE,'{property}','forward','bound');"
    ))
    .execute(&pool)
    .await?;
    utopia_store::documents::record_version(&pool, doc, "chapter-v1", 100).await?;
    let dir = tempfile::tempdir()?;
    let config = utopia_core::config::AppConfig {
        data_dir: dir.path().to_string_lossy().into_owned(),
        ..Default::default()
    };
    let search = Arc::new(utopia_search::SearchIndex::open(
        &dir.path().join("search"),
    )?);
    Ok(Some(Fixture {
        state: AppState::new(pool, &config, search, "test-only".into()),
        org,
        kb,
        doc,
        subject,
        _dir: dir,
    }))
}

async fn chapter(
    fixture: &Fixture,
    seq: i32,
    heading: &str,
    value: &str,
    attested: Option<DateTime<Utc>>,
) -> anyhow::Result<(Uuid, Uuid)> {
    let pool = &fixture.state.pool;
    let chunk = Uuid::now_v7();
    let quote = format!("Widget is on {value}.");
    let prefix = format!("# {heading}\n");
    let text = format!("{prefix}{quote}");
    sqlx::query(
        "INSERT INTO chunks(id,kb_id,document_id,seq,text,doc_version)
         VALUES ($1,$2,$3,$4,$5,
             (SELECT COALESCE(max(version),1) FROM document_versions WHERE document_id=$3))",
    )
    .bind(chunk)
    .bind(fixture.kb)
    .bind(fixture.doc)
    .bind(seq)
    .bind(&text)
    .execute(pool)
    .await?;
    let object = json!({"value": value});
    let (statement, _) = graph::insert_open_statement(
        pool,
        fixture.kb,
        fixture.subject,
        "is on",
        FactObject::Value(&object),
        attested,
        1.0,
    )
    .await?;
    graph::add_evidence_located(
        pool,
        statement,
        chunk,
        Some(&quote),
        Some("is on"),
        Some((
            i32::try_from(prefix.chars().count())?,
            i32::try_from(text.chars().count())?,
        )),
    )
    .await?;
    Ok((statement, chunk))
}

fn day(year: i32) -> DateTime<Utc> {
    format!("{year}-01-01T00:00:00Z").parse().unwrap()
}

type StateEnd = (
    String,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<DateTime<Utc>>,
);

#[tokio::test]
async fn section_dates_reconcile_values_materialized_before_attestation() -> anyhow::Result<()> {
    let Some(fixture) = fixture().await? else {
        return Ok(());
    };
    let pool = &fixture.state.pool;
    let run = async {
        chapter(&fixture, 0, "Earlier", "desk", None).await?;
        chapter(&fixture, 1, "Later", "shelf", None).await?;
        let materialized = utopia_store::materialize::materialize(pool, fixture.kb).await?;
        assert_eq!(materialized.added, 2);
        let dated: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM facts WHERE kb_id=$1 AND layer='typed'
                AND invalidated_at IS NULL AND attested_from IS NOT NULL",
        )
        .bind(fixture.kb)
        .fetch_one(pool)
        .await?;
        assert_eq!(dated, 0, "materialization preserves the statements' NULL dates");
        let context: DocumentDating = serde_json::from_value(json!({"entries": [
            {"kind":"now","name":"Report date","words":"2015-01-01",
             "from":{"year":2015,"month":1,"day":1},"scope":["Earlier"]},
            {"kind":"now","name":"Report date","words":"2026-01-01",
             "from":{"year":2026,"month":1,"day":1},"scope":["Later"]}
        ]}))?;
        let doc = utopia_store::documents::get(pool, fixture.doc).await?;
        attest_statements(&fixture.state, &doc, &context).await?;
        let rows: Vec<StateEnd> =
            sqlx::query_as(
                "SELECT object_value->>'value', valid_from, valid_to_precision, attested_to
                   FROM facts WHERE kb_id=$1 AND layer='typed' AND invalidated_at IS NULL
                   ORDER BY object_value->>'value'",
            )
            .bind(fixture.kb)
            .fetch_all(pool)
            .await?;
        assert_eq!(rows, vec![
            ("desk".into(), None, Some("unknown".into()), Some(day(2026))),
            ("shelf".into(), None, None, None),
        ]);
        let query = format!(
            "SELECT f.object_value->>'value' FROM facts f WHERE f.kb_id=$1
               AND f.layer='typed' AND f.invalidated_at IS NULL AND {}",
            utopia_store::world_axis::facts_hold_at("f", 2)
        );
        for (at, expected) in [(day(2014), vec![]), (day(2020), vec!["desk"]), (day(2026), vec!["shelf"])] {
            let values: Vec<String> = sqlx::query_scalar(&query)
                .bind(fixture.kb)
                .bind(at)
                .fetch_all(pool)
                .await?;
            assert_eq!(values, expected);
        }
        let revisions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM facts WHERE kb_id=$1 AND layer='typed' AND supersedes IS NOT NULL",
        )
        .bind(fixture.kb)
        .fetch_one(pool)
        .await?;
        assert_eq!(revisions, 1, "the predecessor closes through the ledger");
        attest_statements(&fixture.state, &doc, &context).await?;
        let after_retry: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM facts WHERE kb_id=$1 AND layer='typed' AND supersedes IS NOT NULL",
        )
        .bind(fixture.kb)
        .fetch_one(pool)
        .await?;
        assert_eq!(after_retry, revisions);
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(fixture.org)
        .execute(pool)
        .await?;
    run
}

/// Each piece of evidence records the date and name of its section; a section that states no date is recorded as empty. When a document is re-parsed and states no dates at all,
/// the previous recording is cleared. After a document is deleted, recomputing attestation from the remaining evidence (`documents::reattest_tx`) reads exactly these two columns
#[tokio::test]
async fn each_piece_of_evidence_records_the_date_its_section_speaks_from() -> anyhow::Result<()> {
    let Some(fixture) = fixture().await? else {
        return Ok(());
    };
    let pool = &fixture.state.pool;
    let run = async {
        let (earlier, _) = chapter(&fixture, 0, "Earlier", "desk", None).await?;
        let (later, _) = chapter(&fixture, 1, "Later", "shelf", None).await?;
        let (appendix, _) = chapter(&fixture, 2, "Appendix", "floor", None).await?;
        let context: DocumentDating = serde_json::from_value(json!({"entries": [
            {"kind":"now","name":"Report date","words":"2015-01-01",
             "from":{"year":2015,"month":1,"day":1},"scope":["Earlier"]},
            {"kind":"now","name":"","words":"2026-01-01",
             "from":{"year":2026,"month":1,"day":1},"scope":["Later"]}
        ]}))?;
        let doc = utopia_store::documents::get(pool, fixture.doc).await?;
        attest_statements(&fixture.state, &doc, &context).await?;
        type Witness = (Option<DateTime<Utc>>, Option<String>);
        let witness = |statement: Uuid| async move {
            sqlx::query_as::<_, Witness>(
                "SELECT attested_at, attested_by FROM fact_evidence WHERE fact_id=$1",
            )
            .bind(statement)
            .fetch_one(pool)
            .await
        };
        assert_eq!(
            witness(earlier).await?,
            (Some(day(2015)), Some("Report date 2015-01-01".into()))
        );
        assert_eq!(
            witness(later).await?,
            (Some(day(2026)), Some("2026-01-01".into()))
        );
        assert_eq!(
            witness(appendix).await?,
            (None, None),
            "no date governs this section"
        );

        let silent: DocumentDating = serde_json::from_value(json!({"entries": []}))?;
        attest_statements(&fixture.state, &doc, &silent).await?;
        for statement in [earlier, later, appendix] {
            assert_eq!(
                witness(statement).await?,
                (None, None),
                "the document now states no date"
            );
        }
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(fixture.org)
        .execute(pool)
        .await?;
    run
}

#[tokio::test]
async fn current_context_does_not_reattest_superseded_chunks() -> anyhow::Result<()> {
    let Some(fixture) = fixture().await? else {
        return Ok(());
    };
    let pool = &fixture.state.pool;
    let run = async {
        let (old, old_chunk) = chapter(&fixture, 0, "Report", "desk", Some(day(2026))).await?;
        sqlx::query("UPDATE chunks SET superseded_at=now() WHERE id=$1")
            .bind(old_chunk)
            .execute(pool)
            .await?;
        utopia_store::documents::record_version(pool, fixture.doc, "chapter-v2", 100).await?;
        let (current, _) = chapter(&fixture, 1, "Report", "shelf", None).await?;
        let context: DocumentDating = serde_json::from_value(json!({"entries": [
            {"kind":"now","name":"Report date","words":"2025-01-01",
             "from":{"year":2025,"month":1,"day":1},"scope":["Report"]}
        ]}))?;
        let doc = utopia_store::documents::get(pool, fixture.doc).await?;
        attest_statements(&fixture.state, &doc, &context).await?;
        for (statement, expected) in [(old, day(2026)), (current, day(2025))] {
            let at: Option<DateTime<Utc>> =
                sqlx::query_scalar("SELECT attested_from FROM facts WHERE id=$1")
                    .bind(statement)
                    .fetch_one(pool)
                    .await?;
            assert_eq!(at, Some(expected), "statement {statement}");
        }
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(fixture.org)
        .execute(pool)
        .await?;
    run
}

async fn typed_attestations(
    pool: &sqlx::PgPool,
    kb: Uuid,
) -> anyhow::Result<Vec<(String, Option<DateTime<Utc>>)>> {
    Ok(sqlx::query_as(
        "SELECT object_value->>'value', attested_from FROM facts
          WHERE kb_id=$1 AND layer='typed' AND invalidated_at IS NULL
          ORDER BY object_value->>'value'",
    )
    .bind(kb)
    .fetch_all(pool)
    .await?)
}

/// A pass interrupted after it attested the statements finds nothing left to move when it is retried.
/// The typed rows that follow the document's statements are synced all the same (RC-012).
#[tokio::test]
async fn a_retried_pass_attests_the_typed_rows_an_interrupted_pass_left_behind(
) -> anyhow::Result<()> {
    let Some(fixture) = fixture().await? else {
        return Ok(());
    };
    let pool = &fixture.state.pool;
    let run = async {
        chapter(&fixture, 0, "Report", "desk", None).await?;
        utopia_store::materialize::materialize(pool, fixture.kb).await?;
        let context: DocumentDating = serde_json::from_value(json!({"entries": [
            {"kind":"now","name":"Report date","words":"2025-01-01",
             "from":{"year":2025,"month":1,"day":1},"scope":["Report"]}
        ]}))?;
        let doc = utopia_store::documents::get(pool, fixture.doc).await?;
        attest_statements(&fixture.state, &doc, &context).await?;
        assert_eq!(
            typed_attestations(pool, fixture.kb).await?,
            vec![("desk".to_string(), Some(day(2025)))]
        );
        // the state an interrupted pass leaves: the statement attested, its typed row not yet
        sqlx::query(
            "UPDATE facts SET attested_from=NULL, attested_by=NULL WHERE kb_id=$1 AND layer='typed'",
        )
        .bind(fixture.kb)
        .execute(pool)
        .await?;

        attest_statements(&fixture.state, &doc, &context).await?;

        assert_eq!(
            typed_attestations(pool, fixture.kb).await?,
            vec![("desk".to_string(), Some(day(2025)))]
        );
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(fixture.org)
        .execute(pool)
        .await?;
    run
}

/// A dated document's pass syncs the typed rows that follow its own statements, not the base (RC-012):
/// a row that follows another document's statement is left to the base's next whole-base sync.
#[tokio::test]
async fn a_dated_pass_leaves_the_typed_rows_of_other_documents_to_the_whole_base_sync(
) -> anyhow::Result<()> {
    let Some(fixture) = fixture().await? else {
        return Ok(());
    };
    let pool = &fixture.state.pool;
    let run = async {
        chapter(&fixture, 0, "Report", "desk", None).await?;
        // another document's statement, attested, and its typed row not yet synced
        let elsewhere = json!({"value": "shelf"});
        let (other_statement, _) = graph::insert_open_statement(
            pool,
            fixture.kb,
            fixture.subject,
            "is on",
            FactObject::Value(&elsewhere),
            Some(day(2020)),
            1.0,
        )
        .await?;
        utopia_store::materialize::materialize(pool, fixture.kb).await?;
        sqlx::query(
            "UPDATE facts t SET attested_from=NULL, attested_by=NULL
              WHERE t.kb_id=$1 AND t.layer='typed'
                AND EXISTS (SELECT 1 FROM typed_fact_sources s WHERE s.fact_id=t.id AND s.statement_id=$2)",
        )
        .bind(fixture.kb)
        .bind(other_statement)
        .execute(pool)
        .await?;
        let context: DocumentDating = serde_json::from_value(json!({"entries": [
            {"kind":"now","name":"Report date","words":"2025-01-01",
             "from":{"year":2025,"month":1,"day":1},"scope":["Report"]}
        ]}))?;
        let doc = utopia_store::documents::get(pool, fixture.doc).await?;

        attest_statements(&fixture.state, &doc, &context).await?;

        assert_eq!(
            typed_attestations(pool, fixture.kb).await?,
            vec![
                ("desk".to_string(), Some(day(2025))),
                ("shelf".to_string(), None)
            ]
        );
        utopia_store::materialize::sync_typed_attestation(pool, fixture.kb, None).await?;
        assert_eq!(
            typed_attestations(pool, fixture.kb).await?,
            vec![
                ("desk".to_string(), Some(day(2025))),
                ("shelf".to_string(), Some(day(2020)))
            ]
        );
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(fixture.org)
        .execute(pool)
        .await?;
    run
}
