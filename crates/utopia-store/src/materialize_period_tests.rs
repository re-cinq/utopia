//! A moment's statement landing on a status attribute is read as the binding says (0053 revision 2026-10-22): with `none` or nothing said, go by that date
//! and the period it names is written as one span (start at it, end at the end of the bucket, marked `end_after_period`), and `start` stays open as before.
//! Skipped when `UTOPIA_DATABASE_URL` is not set.

use super::materialize_in_tx;
use crate::phrase_bindings;
use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

struct Fx {
    org: Uuid,
    kb: Uuid,
    property: Uuid,
    sig: phrase_bindings::PhraseSignature,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fx> {
    let (org, ws, kb, company, property, statement) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations(id,name) VALUES($1,'period')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces(id,org_id,name) VALUES($1,$2,'period')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases(id,workspace_id,name) VALUES($1,$2,'period')")
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO entities(id,kb_id,canonical_name) VALUES($1,$2,'启明半导体')")
        .bind(company)
        .bind(kb)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO relation_types(id,kb_id,key,label,temporal,kind,datatype,unit) VALUES($1,$2,'revenue','营业收入','state','attribute','number','亿元')",
    )
    .bind(property)
    .bind(kb)
    .execute(pool)
    .await?;
    // "2023 revenue 5.8 billion yuan": time parsing writes the year as a moment with the same start and end, precision year
    let year: DateTime<Utc> = "2023-01-01T00:00:00Z".parse()?;
    sqlx::query(
        "INSERT INTO facts(id,kb_id,subject_id,object_value,layer,phrase,valid_from,valid_from_precision,valid_to,valid_to_precision)
         VALUES($1,$2,$3,$4,'open','营业收入',$5,'year',$5,'year')",
    )
    .bind(statement)
    .bind(kb)
    .bind(company)
    .bind(json!({ "value": "58亿元" }))
    .bind(year)
    .execute(pool)
    .await?;
    let sig = phrase_bindings::signatures(pool, kb).await?.remove(0);
    Ok(Fx {
        org,
        kb,
        property,
        sig,
    })
}

async fn bind(pool: &PgPool, f: &Fx, marks: Option<&str>) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    phrase_bindings::decide_on(
        &mut tx,
        f.kb,
        &f.sig,
        phrase_bindings::Decision {
            relation_type_id: Some(f.property),
            direction: Some("forward"),
            status: "bound",
            votes: &serde_json::Value::Null,
            decided_by: "person",
            basis: None,
            marks,
            marks_asked: true,
        },
    )
    .await?;
    tx.commit().await?;
    let mut tx = pool.begin().await?;
    materialize_in_tx(&mut tx, f.kb).await?;
    tx.commit().await?;
    Ok(())
}

type Row = (
    serde_json::Value,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<String>,
    bool,
);

async fn typed(pool: &PgPool, kb: Uuid) -> anyhow::Result<Vec<Row>> {
    Ok(sqlx::query_as(
        "SELECT object_value, valid_from, valid_from_precision, valid_to, valid_to_precision, end_after_period
           FROM facts WHERE kb_id=$1 AND layer='typed' AND invalidated_at IS NULL",
    )
    .bind(kb)
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
async fn a_single_date_that_marks_neither_end_names_its_period() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    crate::db::migrate(&pool).await?;
    let f = seed(&pool).await?;
    let run = async {
        let start: DateTime<Utc> = "2023-01-01T00:00:00Z".parse()?;
        let end: DateTime<Utc> = "2024-01-01T00:00:00Z".parse()?;
        // The binding does not say which end the moment marks: read it as the year 2023
        bind(&pool, &f, None).await?;
        let rows = typed(&pool, f.kb).await?;
        assert_eq!(rows.len(), 1, "{rows:?}");
        let (value, from, from_p, to, to_p, after) = &rows[0];
        assert_eq!(value["value"], json!(58.0));
        assert_eq!((*from, from_p.as_deref()), (Some(start), Some("year")));
        assert_eq!((*to, to_p.as_deref()), (Some(end), Some("year")));
        assert!(
            after,
            "the end is the moment after the period the document wrote"
        );
        // "Neither" is the same reading; recomputation is a set operation, still that one row
        bind(&pool, &f, Some("none")).await?;
        let again = typed(&pool, f.kb).await?;
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].3, Some(end));
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
