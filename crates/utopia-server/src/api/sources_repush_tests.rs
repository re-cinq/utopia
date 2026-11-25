//! Re-pushing pushed entries (RC-006): once the new version is extracted, facts it no longer states are closed; a fact that another entry still states is kept.
//! Walks the real flow: push, process, extract, re-push, extract again.
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

/// The fact the live version of `doc` remembers for the statement with this memo key
async fn remembered_fact(f: &Fixture, doc: Uuid, key: &str) -> anyhow::Result<Uuid> {
    Ok(sqlx::query_scalar(
        "SELECT w.fact_id FROM pushed_statement_writes w
         JOIN chunks c ON c.id = w.chunk_id
         WHERE c.document_id = $1 AND c.superseded_at IS NULL AND w.key = $2",
    )
    .bind(doc)
    .bind(key)
    .fetch_one(&f.pool)
    .await?)
}

/// The entity the live version of `doc` remembers for the name with this memo key
async fn remembered_entity(f: &Fixture, doc: Uuid, key: &str) -> anyhow::Result<Uuid> {
    Ok(sqlx::query_scalar(
        "SELECT h.entity_id FROM pushed_entity_handles h
         JOIN chunks c ON c.id = h.chunk_id
         WHERE c.document_id = $1 AND c.superseded_at IS NULL AND h.key = $2",
    )
    .bind(doc)
    .bind(key)
    .fetch_one(&f.pool)
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

/// `item` with one unnamed (described) place the statements can point at
fn item_with_harbour(external_id: &str, statements: &[(&str, &str)]) -> Value {
    let mut body = item(external_id, statements);
    body["e"]
        .as_array_mut()
        .unwrap()
        .push(json!(["a small harbour town", "town", false]));
    body
}

#[tokio::test]
async fn a_repush_keeps_the_fact_of_a_statement_it_did_not_change() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let unchanged = ("grew up in", "a small harbour town");
    let v1 = item_with_harbour("obs-1", &[unchanged, ("visited", "Oslo")]);
    let doc = push_and_extract(&f, &v1, "created").await?;
    let key = serde_json::to_string(&v1["s"][0]).unwrap();

    let first_fact = remembered_fact(&f, doc, &key).await?;

    let v2 = item_with_harbour("obs-1", &[unchanged, ("is based in", "Port Ellen")]);
    push_and_extract(&f, &v2, "updated").await?;

    let second_fact = remembered_fact(&f, doc, &key).await?;
    let (is_live, evidence_on_new_chunk): (bool, i64) = sqlx::query_as(
        "SELECT f.invalidated_at IS NULL,
                (SELECT count(*) FROM fact_evidence ev
                 JOIN chunks c ON c.id = ev.chunk_id
                 WHERE ev.fact_id = f.id AND c.document_id = $2 AND c.superseded_at IS NULL)
         FROM facts f WHERE f.id = $1",
    )
    .bind(second_fact)
    .bind(doc)
    .fetch_one(&f.pool)
    .await?;

    assert_eq!(
        (second_fact, is_live, evidence_on_new_chunk),
        (first_fact, true, 1)
    );
    f.cleanup().await
}

#[tokio::test]
async fn a_repush_keeps_the_time_of_a_statement_it_did_not_change() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    let with_time = |extra: (&str, &str)| {
        let mut body = item("obs-1", &[extra]);
        body["s"].as_array_mut().unwrap().insert(
            0,
            json!([
                null,
                "Ada",
                "lived in",
                "Oslo",
                null,
                {},
                "2019-05-01",
                null
            ]),
        );
        body
    };
    let doc = push_and_extract(&f, &with_time(("visited", "Oslo")), "created").await?;
    crate::time_resolution::resolve_document(&f.state, doc).await?;
    push_and_extract(&f, &with_time(("is based in", "Port Ellen")), "updated").await?;
    crate::time_resolution::resolve_document(&f.state, doc).await?;

    let (valid_from, mentions_on_live_chunk): (Option<chrono::DateTime<chrono::Utc>>, i64) =
        sqlx::query_as(
            "SELECT f.valid_from,
                    (SELECT count(*) FROM time_mentions m JOIN chunks c ON c.id = m.chunk_id
                     WHERE m.fact_id = f.id AND c.document_id = $2 AND c.superseded_at IS NULL)
             FROM facts f WHERE f.kb_id = $1 AND f.layer = 'open' AND f.phrase = 'lived in'
               AND f.invalidated_at IS NULL",
        )
        .bind(f.kb)
        .bind(doc)
        .fetch_one(&f.pool)
        .await?;

    assert_eq!(
        (
            valid_from.map(|at| at.date_naive().to_string()),
            mentions_on_live_chunk
        ),
        (Some("2019-05-01".to_string()), 1)
    );
    f.cleanup().await
}

#[tokio::test]
async fn a_repush_keeps_the_entity_of_a_name_it_did_not_change() -> anyhow::Result<()> {
    let Some(f) = Fixture::new().await? else {
        return Ok(());
    };
    sqlx::query("UPDATE sources SET config = '{\"names\": \"identity\"}'::jsonb WHERE id = $1")
        .bind(f.source)
        .execute(&f.pool)
        .await?;
    let doc = push_and_extract(&f, &item("obs-1", &[("visited", "Oslo")]), "created").await?;
    let ada_key = serde_json::to_string(&json!(["Ada", "person", true]))?;
    let first_ada = remembered_entity(&f, doc, &ada_key).await?;

    let namesake = Uuid::now_v7();
    sqlx::query("INSERT INTO entities(id, kb_id, canonical_name) VALUES ($1, $2, 'Ada')")
        .bind(namesake)
        .bind(f.kb)
        .execute(&f.pool)
        .await?;
    for phrase in ["wrote", "built", "taught"] {
        sqlx::query(
            "INSERT INTO facts(id, kb_id, subject_id, object_value, layer, phrase, confidence)
             VALUES ($1, $2, $3, to_jsonb('x'::text), 'open', $4, 1.0)",
        )
        .bind(Uuid::now_v7())
        .bind(f.kb)
        .bind(namesake)
        .bind(phrase)
        .execute(&f.pool)
        .await?;
    }
    push_and_extract(
        &f,
        &item("obs-1", &[("is based in", "Port Ellen")]),
        "updated",
    )
    .await?;

    assert_eq!(remembered_entity(&f, doc, &ada_key).await?, first_ada);
    f.cleanup().await
}
