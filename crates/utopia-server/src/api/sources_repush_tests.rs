//! 推来的条目重推之后（RC-006）：新版本抽取完，它不再陈述的事实闭合；还有别的条目在陈述
//! 的那一条留着。走真流程：推、处理、抽取、再推、再抽取。
use super::statements_tests::Fixture;
use axum::http::StatusCode;
use serde_json::{json, Value};
use utopia_core::models::Proposer;
use utopia_store::documents;
use uuid::Uuid;

fn item(external_id: &str, statements: &[(&str, &str)]) -> Value {
    json!({
        "external_id": external_id,
        "e": [["Ada", "person", true], ["Port Ellen", "town", true], ["Oslo", "town", true]],
        "s": statements
            .iter()
            .map(|(phrase, object)| json!([null, "Ada", phrase, object, null, {}, null, null]))
            .collect::<Vec<_>>(),
        "n": []
    })
}

async fn push_and_extract(f: &Fixture, body: &Value, action: &str) -> anyhow::Result<Uuid> {
    let (status, reply) = f.push(f.source, &f.token, body).await?;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["action"], action);
    let key = format!("statements:{}", body["external_id"].as_str().unwrap());
    let doc = documents::find_by_external_key(&f.pool, f.source, &key)
        .await?
        .expect("the push made a document under its identity");
    crate::pipeline::process_document(&f.state, doc.id).await?;
    crate::extraction::extract_document(&f.state, doc.id, Proposer::default()).await?;
    Ok(doc.id)
}

/// `(still open, ended)` of the statement `Ada <phrase>`: open means no end and no
/// invalidation; ended means its live successor carries a `valid_to`
async fn standing(
    f: &Fixture,
    phrase: &str,
) -> anyhow::Result<Vec<Option<chrono::DateTime<chrono::Utc>>>> {
    Ok(sqlx::query_scalar(
        "SELECT valid_to FROM facts WHERE kb_id = $1 AND layer = 'open' AND phrase = $2
            AND invalidated_at IS NULL ORDER BY valid_to NULLS FIRST",
    )
    .bind(f.kb)
    .bind(phrase)
    .fetch_all(&f.pool)
    .await?)
}

#[tokio::test]
async fn a_repush_closes_the_statement_its_new_version_dropped() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let both = item(
        "obs-1",
        &[("is based in", "Port Ellen"), ("visited", "Oslo")],
    );
    let doc = push_and_extract(&f, &both, "created").await?;
    assert_eq!(standing(&f, "visited").await?, vec![None]);

    let only_based = item("obs-1", &[("is based in", "Port Ellen")]);
    push_and_extract(&f, &only_based, "updated").await?;

    let v1_ended: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT date_trunc('second', max(superseded_at), 'UTC') FROM chunks WHERE document_id = $1",
    )
    .bind(doc)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        standing(&f, "visited").await?,
        vec![Some(v1_ended)],
        "the dropped statement ends when its version was replaced"
    );
    assert_eq!(
        standing(&f, "is based in").await?,
        vec![None],
        "the statement the new version restates stays open"
    );
    f.cleanup().await
}

#[tokio::test]
async fn a_repush_keeps_a_statement_another_item_still_states() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    push_and_extract(
        &f,
        &item(
            "obs-1",
            &[("is based in", "Port Ellen"), ("visited", "Oslo")],
        ),
        "created",
    )
    .await?;
    push_and_extract(&f, &item("obs-2", &[("visited", "Oslo")]), "created").await?;

    push_and_extract(
        &f,
        &item("obs-1", &[("is based in", "Port Ellen")]),
        "updated",
    )
    .await?;

    assert_eq!(
        standing(&f, "visited").await?,
        vec![None],
        "obs-2 still attests it: nothing closes"
    );
    f.cleanup().await
}

#[tokio::test]
async fn an_extracted_item_remembers_the_fact_of_each_statement() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let body = item(
        "obs-1",
        &[("is based in", "Port Ellen"), ("visited", "Oslo")],
    );
    let doc = push_and_extract(&f, &body, "created").await?;

    let rows: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT w.ord, w.key, f.phrase
         FROM pushed_statement_writes w
         JOIN chunks c ON c.id = w.chunk_id
         JOIN facts f ON f.id = w.fact_id
         WHERE c.document_id = $1 AND c.superseded_at IS NULL AND f.invalidated_at IS NULL
         ORDER BY w.ord",
    )
    .bind(doc)
    .fetch_all(&f.pool)
    .await?;

    let expected: Vec<(i32, String, String)> = body["s"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(ord, statement)| {
            (
                ord as i32,
                serde_json::to_string(statement).unwrap(),
                statement[2].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(rows, expected);
    f.cleanup().await
}

#[tokio::test]
async fn an_extracted_item_remembers_the_entity_of_each_name() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let body = item("obs-1", &[("is based in", "Port Ellen")]);
    let doc = push_and_extract(&f, &body, "created").await?;

    let mut rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT h.key, e.canonical_name
         FROM pushed_entity_handles h
         JOIN chunks c ON c.id = h.chunk_id
         JOIN entities e ON e.id = h.entity_id
         WHERE c.document_id = $1 AND c.superseded_at IS NULL",
    )
    .bind(doc)
    .fetch_all(&f.pool)
    .await?;
    rows.sort();

    let mut expected: Vec<(String, String)> = body["e"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entity| {
            (
                serde_json::to_string(entity).unwrap(),
                entity[0].as_str().unwrap().to_string(),
            )
        })
        .collect();
    expected.sort();
    assert_eq!(rows, expected);
    f.cleanup().await
}
