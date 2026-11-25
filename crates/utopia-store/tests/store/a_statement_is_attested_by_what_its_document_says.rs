//! A statement's attestation comes from the date the document itself states; if it states none, there is none (0064 decisions 3 and 5).
//!
//! - An open statement written by extraction no longer takes the moment the document was processed as its attestation: if the document says nothing, the field is left empty;
//! - A row left empty holds at any point on the world axis (open lower bound); events without a date are the exception, and still hold at no point;
//! - `attest_statement` writes the attestation and the name of that date, and only ever moves it earlier;
//! - A materialized typed row follows its source statement;
//! - A row computed by a rule also follows the statement it reads; a row a category-word rule derives from an entity has no attestation; a row a person wrote gets an attestation
//!     only when a rule's conclusion merges into it -- the attestation is still the moment the person wrote it.
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. It creates and tears down its own data and never touches an existing database.

use chrono::{DateTime, TimeZone, Utc};
use sqlx::PgPool;
use utopia_store::graph::{self, FactObject, Validity};
use uuid::Uuid;

const ORG: &str = "statement-attested-by-document-test";
const RULE_ORG: &str = "rule-row-attested-by-its-statement-test";

async fn anchor(
    pool: &PgPool,
    fact: Uuid,
) -> anyhow::Result<(Option<DateTime<Utc>>, Option<String>)> {
    Ok(
        sqlx::query_as("SELECT attested_from, attested_by FROM facts WHERE id = $1")
            .bind(fact)
            .fetch_one(pool)
            .await?,
    )
}

/// Does this row hold at time T? Judged by the read path's own predicate
async fn holds_at(pool: &PgPool, fact: Uuid, at: DateTime<Utc>) -> anyhow::Result<bool> {
    let sql = format!(
        "SELECT {} FROM facts f WHERE f.id = $1",
        utopia_store::world_axis::facts_hold_at("f", 2)
    );
    Ok(sqlx::query_scalar(&sql)
        .bind(fact)
        .bind(at)
        .fetch_one(pool)
        .await?)
}

#[tokio::test]
async fn an_unattested_statement_holds_at_every_moment_and_an_attested_one_from_its_date(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(ORG)
        .execute(&pool)
        .await?;
    let (org, ws, kb, watch, market, launch) = (
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
             ('{watch}','{kb}','码表'), ('{market}','{kb}','全球');
         INSERT INTO relation_types(id,kb_id,key,label,kind,temporal,description) VALUES
             ('{launch}','{kb}','launched_in','launched in','relation','event','');"
    ))
    .execute(&pool)
    .await?;

    let run = async {
        let long_ago = Utc.with_ymd_and_hms(2015, 1, 1, 0, 0, 0).unwrap();
        let report_day = Utc.with_ymd_and_hms(2026, 9, 4, 0, 0, 0).unwrap();
        let earlier_report = Utc.with_ymd_and_hms(2026, 8, 28, 0, 0, 0).unwrap();

        // The document does not say which day it is from: no attestation, so it holds at any time
        let (statement, _) = graph::insert_open_statement(
            &pool,
            kb,
            watch,
            "所属市场为",
            FactObject::Entity(market),
            None,
            0.9,
        )
        .await?;
        assert_eq!(anchor(&pool, statement).await?, (None, None));
        assert!(holds_at(&pool, statement, long_ago).await?, "no time at all: holds at every moment");
        assert!(holds_at(&pool, statement, Utc::now()).await?);

        // Attested by the date of the section it sits in: readable from that day on, not before (the lower bound from 0022)
        assert!(graph::attest_statement(&pool, statement, report_day, "提报日期 2026年9月4日").await?);
        assert_eq!(
            anchor(&pool, statement).await?,
            (Some(report_day), Some("提报日期 2026年9月4日".to_string()))
        );
        assert!(!holds_at(&pool, statement, long_ago).await?);
        assert!(holds_at(&pool, statement, report_day).await?);
        // An earlier document also states it: the attestation moves earlier and the name follows; a later one does not move it
        assert!(graph::attest_statement(&pool, statement, earlier_report, "提报日期 2026年8月28日").await?);
        assert!(!graph::attest_statement(&pool, statement, report_day, "提报日期 2026年9月4日").await?);
        assert_eq!(
            anchor(&pool, statement).await?,
            (Some(earlier_report), Some("提报日期 2026年8月28日".to_string()))
        );

        // A materialized typed row follows its source statement; if the source has no attestation, neither does it
        let (typed, bare_typed) = (Uuid::now_v7(), Uuid::now_v7());
        let (bare, _) = graph::insert_open_statement(
            &pool,
            kb,
            watch,
            "上市于",
            FactObject::Entity(market),
            None,
            0.9,
        )
        .await?;
        for (id, source) in [(typed, statement), (bare_typed, bare)] {
            sqlx::query(
                "INSERT INTO facts (id, kb_id, subject_id, object_id, predicate_id, layer, from_statement_id)
                 VALUES ($1, $2, $3, $4, $5, 'typed', $6)",
            )
            .bind(id)
            .bind(kb)
            .bind(watch)
            .bind(market)
            .bind(launch)
            .bind(source)
            .execute(&pool)
            .await?;
            sqlx::query("INSERT INTO typed_fact_sources (fact_id, statement_id) VALUES ($1, $2)")
                .bind(id)
                .bind(source)
                .execute(&pool)
                .await?;
        }
        utopia_store::materialize::sync_typed_attestation(&pool, kb, None).await?;
        assert_eq!(
            anchor(&pool, typed).await?,
            (Some(earlier_report), Some("提报日期 2026年8月28日".to_string()))
        );
        assert_eq!(anchor(&pool, bare_typed).await?, (None, None));
        // An event with no date holds at no time (0022), and the same goes without an attestation
        assert!(!holds_at(&pool, bare_typed, long_ago).await?);
        assert!(!holds_at(&pool, bare_typed, Utc::now()).await?);
        Ok::<(), anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(ORG)
        .execute(&pool)
        .await?;
    run
}

/// The live typed fact between subject and object
async fn typed_row(pool: &PgPool, kb: Uuid, subject: Uuid, object: Uuid) -> anyhow::Result<Uuid> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM facts
          WHERE kb_id = $1 AND subject_id = $2 AND object_id = $3
            AND layer = 'typed' AND invalidated_at IS NULL",
    )
    .bind(kb)
    .bind(subject)
    .bind(object)
    .fetch_one(pool)
    .await?)
}

/// A row computed by a rule (the implied row from 0073) follows the statement it reads: if the statement states no time, it has no attestation and holds at any time;
/// once the statement is later attested, it follows that day. A row a category-word rule derives from an entity has no statement to follow, and no attestation.
/// A row a person wrote, when a rule's conclusion merges into it, keeps the attestation from the moment the person wrote it
#[tokio::test]
async fn a_row_a_rule_implies_follows_the_statement_it_read() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(RULE_ORG)
        .execute(&pool)
        .await?;
    let ids: Vec<Uuid> = (0..14).map(|_| Uuid::now_v7()).collect();
    let (org, ws, kb, person, company, works_for, origin) =
        (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5], ids[6]);
    let (lin, aster, meridian, film, uk, signed_rule, kind_rule) =
        (ids[7], ids[8], ids[9], ids[10], ids[11], ids[12], ids[13]);
    // Two approved rules: "signed with" (person → organization) implies works_for; the category word "british film"
    // implies country_of_origin, with the object supplied by the reading cache
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','{RULE_ORG}');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','{RULE_ORG}');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','{RULE_ORG}');
         INSERT INTO entity_types(id,kb_id,key,label,color,shape) VALUES
             ('{person}','{kb}','person','person','#000','circle'),
             ('{company}','{kb}','organization','organization','#000','circle');
         INSERT INTO relation_types(id,kb_id,key,label,kind,temporal) VALUES
             ('{works_for}','{kb}','works_for','works for','relation','state'),
             ('{origin}','{kb}','country_of_origin','country of origin','relation','state');
         INSERT INTO entities(id,kb_id,canonical_name,type_id,specific_type) VALUES
             ('{lin}','{kb}','Lin Zhao','{person}',NULL),
             ('{aster}','{kb}','Aster Labs','{company}',NULL),
             ('{meridian}','{kb}','Meridian Systems','{company}',NULL),
             ('{film}','{kb}','Loud Tour',NULL,'British film'),
             ('{uk}','{kb}','United Kingdom',NULL,NULL);
         INSERT INTO implication_rules(id,kb_id,trigger,phrase,subject_type_id,object_type_id,
                                       conclude_property_id,reading,status,decided_by) VALUES
             ('{signed_rule}','{kb}','phrase','signed with','{person}','{company}','{works_for}',
              NULL,'approved','person'),
             ('{kind_rule}','{kb}','kind_word','british film',NULL,NULL,'{origin}',
              'country_of_nationality','approved','person');
         INSERT INTO phrase_readings(kb_id,reading,phrase,entity_id) VALUES
             ('{kb}','country_of_nationality','british film','{uk}');"
    ))
    .execute(&pool)
    .await?;

    let run = async {
        let long_ago = Utc.with_ymd_and_hms(2015, 1, 1, 0, 0, 0).unwrap();
        let signed_day = Utc.with_ymd_and_hms(2026, 9, 4, 0, 0, 0).unwrap();
        let written_day = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();

        // A statement that states no time: the row a rule derives from it has no attestation and holds at any time -- the same as the row
        // materialized from that statement, not the moment it was derived. Rows derived from a category word are the same
        let (signed, _) = graph::insert_open_statement(
            &pool,
            kb,
            lin,
            "signed with",
            FactObject::Entity(aster),
            None,
            0.9,
        )
        .await?;
        let outcome = utopia_store::materialize::materialize(&pool, kb).await?;
        assert_eq!(outcome.implied, 2, "{outcome:?}");
        let by_rule = typed_row(&pool, kb, lin, aster).await?;
        let by_kind = typed_row(&pool, kb, film, uk).await?;
        assert_eq!(anchor(&pool, by_rule).await?, (None, None));
        assert_eq!(anchor(&pool, by_kind).await?, (None, None));
        for row in [by_rule, by_kind] {
            assert!(
                holds_at(&pool, row, long_ago).await?,
                "no time at all: holds at every moment"
            );
            assert!(holds_at(&pool, row, Utc::now()).await?);
        }

        // The statement is later attested by the date of the section it sits in, the time-resolution path: attest the statement first, then sync the typed rows.
        // A row derived by a rule follows that day, together with the name of that date; a category-word row has no statement, so it still has none
        assert!(graph::attest_statement(&pool, signed, signed_day, "签约日 2026年9月4日").await?);
        utopia_store::materialize::sync_typed_attestation(&pool, kb, None).await?;
        let followed = (Some(signed_day), Some("签约日 2026年9月4日".to_string()));
        assert_eq!(anchor(&pool, by_rule).await?, followed);
        assert!(!holds_at(&pool, by_rule, long_ago).await?);
        assert!(holds_at(&pool, by_rule, signed_day).await?);
        assert_eq!(anchor(&pool, by_kind).await?, (None, None));
        // Materialize one more round, leaving them alone
        utopia_store::materialize::materialize(&pool, kb).await?;
        assert_eq!(anchor(&pool, by_rule).await?, followed);
        assert_eq!(anchor(&pool, by_kind).await?, (None, None));

        // A row a person wrote is attested by the moment the person wrote it. A rule derives the same fact from a statement that states no time, and merges into this row:
        // it gains the rule as its source, and the attestation is unchanged
        let (by_hand, _) = graph::insert_fact(
            &pool,
            kb,
            lin,
            Some(works_for),
            meridian,
            Validity {
                attested_at: Some(written_day),
                ..Validity::default()
            },
            0.9,
        )
        .await?;
        graph::insert_open_statement(
            &pool,
            kb,
            lin,
            "signed with",
            FactObject::Entity(meridian),
            None,
            0.9,
        )
        .await?;
        let outcome = utopia_store::materialize::materialize(&pool, kb).await?;
        assert_eq!(
            outcome.implied, 0,
            "merged into the person's row: {outcome:?}"
        );
        let rule_sources: i64 =
            sqlx::query_scalar("SELECT count(*) FROM implied_fact_sources WHERE fact_id = $1")
                .bind(by_hand)
                .fetch_one(&pool)
                .await?;
        assert_eq!(rule_sources, 1);
        assert_eq!(
            anchor(&pool, by_hand).await?,
            (Some(written_day), None),
            "a person's row keeps the moment it was written"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(RULE_ORG)
        .execute(&pool)
        .await?;
    run
}
