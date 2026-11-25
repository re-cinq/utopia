//! Databricks SQL Statement Execution API (`/api/2.0/sql/statements`).
//! Behind a SQL warehouse sits the whole Unity Catalog lakehouse (mainly Delta),
//! and the token is a personal access token. Results are fetched as INLINE + JSON_ARRAY: every value is a string,
//! restored to numbers and booleans per the column types in the manifest.

use super::conn::DatabricksConn;
use super::{
    coerce, rows_to_json_lines, sql_literal, truncate_rows, wrap_limit, QueryEngine, QueryResult,
    SchemaColumn, HTTP_POLL_BUDGET, ROW_CAP,
};
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, Instant};

pub struct DatabricksEngine {
    conn: DatabricksConn,
}

#[derive(Deserialize)]
struct StatementResponse {
    statement_id: Option<String>,
    status: Status,
    manifest: Option<Manifest>,
    result: Option<ResultData>,
}

#[derive(Deserialize)]
struct Status {
    state: String,
    error: Option<StatusError>,
}

#[derive(Deserialize)]
struct StatusError {
    message: Option<String>,
    error_code: Option<String>,
}

#[derive(Deserialize)]
struct Manifest {
    schema: Option<Schema>,
    /// INLINE disposition with a `row_limit` set: true means the result was cut off by rows.
    /// The schema read passes no row_limit, so in theory this never fires; it is kept as an explicit fail-fast rather than silently returning
    /// incomplete data
    truncated: Option<bool>,
    /// Total row count of the whole result set (also present for chunked INLINE). Used after the schema read to validate
    /// `rows.len() == total_row_count`
    total_row_count: Option<u64>,
}

#[derive(Deserialize, Clone)]
struct Schema {
    columns: Vec<ColumnInfo>,
}

#[derive(Deserialize, Clone)]
struct ColumnInfo {
    name: String,
    type_text: Option<String>,
}

#[derive(Deserialize)]
struct ResultData {
    data_array: Option<Vec<Vec<serde_json::Value>>>,
    /// The first chunk is in StatementResponse.result; later GETs return this object directly.
    /// The server supplies the link, and both the path and the query string must be carried to the next request unchanged.
    next_chunk_internal_link: Option<String>,
}

impl DatabricksEngine {
    pub fn new(conn: DatabricksConn) -> Self {
        Self { conn }
    }

    /// Candidate forms of the information_schema.columns query `(description, SQL)`; the description is used only for error messages.
    /// With no catalog only one remains: the session default one.
    fn schema_queries(&self) -> Vec<(&'static str, String)> {
        let schema_filter = self
            .conn
            .schema
            .as_deref()
            .map(|s| format!(" AND table_schema = {}", sql_literal(s)))
            .unwrap_or_default();
        let select = "SELECT table_schema, table_name, column_name, data_type, comment";
        let tail = format!(
            "table_schema <> 'information_schema'{schema_filter} \
             ORDER BY table_schema, table_name, ordinal_position"
        );
        match self.conn.catalog.as_deref() {
            Some(catalog) => vec![
                (
                    "catalog information_schema",
                    format!(
                        "{select} FROM `{}`.information_schema.columns WHERE {tail}",
                        catalog.replace('`', "``")
                    ),
                ),
                (
                    "system information_schema",
                    format!(
                        "{select} FROM system.information_schema.columns \
                         WHERE table_catalog = {} AND {tail}",
                        sql_literal(catalog)
                    ),
                ),
            ],
            None => vec![(
                "session information_schema",
                format!("{select} FROM information_schema.columns WHERE {tail}"),
            )],
        }
    }

    /// Submits one statement, polls until SUCCEEDED, and assembles the result into (column names, row values). All column values are
    /// restored to numbers or booleans per the manifest's `type_text`.
    ///
    /// `row_limit`: pass `None` for no row limit (used by schema reads). An INLINE JSON_ARRAY
    /// result may be split into chunks; after the first chunk, keep GETting through `next_chunk_internal_link`,
    /// looping until the result gives no next page. After a schema read, do one `rows.len()` check against
    /// `manifest.total_row_count`: if rows are missing, the result counts as incomplete
    /// (docs mark truncated=true with another flag, which a schema read should never see)
    ///
    /// Previously `row_limit: ROW_CAP + 1` was passed for every statement (#703): chat queries need it as a
    /// safety valve, schema reads do not; information_schema.columns has one row per column, so schema docs on a wide catalog
    /// were silently truncated at 201 rows
    async fn run(
        &self,
        sql: &str,
        row_limit: Option<usize>,
    ) -> anyhow::Result<(Vec<String>, Vec<Vec<serde_json::Value>>)> {
        let client = super::http()?;
        let mut body = json!({
            "warehouse_id": self.conn.warehouse_id,
            "statement": sql,
            "wait_timeout": "30s",
            "on_wait_timeout": "CONTINUE",
            "disposition": "INLINE",
            "format": "JSON_ARRAY",
        });
        if let Some(n) = row_limit {
            body["row_limit"] = json!(n);
        }
        if let Some(c) = &self.conn.catalog {
            body["catalog"] = json!(c);
        }
        if let Some(s) = &self.conn.schema {
            body["schema"] = json!(s);
        }
        let mut resp: StatementResponse = client
            .post(format!("{}/api/2.0/sql/statements", self.conn.base))
            .bearer_auth(&self.conn.token)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let started = Instant::now();
        loop {
            match resp.status.state.as_str() {
                "SUCCEEDED" => break,
                "PENDING" | "RUNNING" => {
                    let id = resp
                        .statement_id
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("Databricks returned no statement_id"))?;
                    if started.elapsed() > HTTP_POLL_BUDGET {
                        anyhow::bail!(
                            "Databricks statement did not finish within {}s",
                            HTTP_POLL_BUDGET.as_secs()
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    resp = client
                        .get(format!("{}/api/2.0/sql/statements/{id}", self.conn.base))
                        .bearer_auth(&self.conn.token)
                        .send()
                        .await?
                        .error_for_status()?
                        .json()
                        .await?;
                }
                other => {
                    let e = resp.status.error.as_ref();
                    let code = e
                        .and_then(|e| e.error_code.clone())
                        .map(|c| format!("{c}: "))
                        .unwrap_or_default();
                    let msg = e
                        .and_then(|e| e.message.clone())
                        .unwrap_or_else(|| format!("statement ended in state {other}"));
                    anyhow::bail!("{code}{msg}");
                }
            }
        }
        let columns: Vec<ColumnInfo> = resp
            .manifest
            .as_ref()
            .and_then(|m| m.schema.as_ref())
            .map(|s| s.columns.clone())
            .unwrap_or_default();
        let mut rows: Vec<Vec<serde_json::Value>> = resp
            .result
            .as_ref()
            .and_then(|r| r.data_array.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let ty = columns
                            .get(i)
                            .and_then(|c| c.type_text.as_deref())
                            .unwrap_or("");
                        coerce(ty, v)
                    })
                    .collect()
            })
            .collect();
        // The manifest describes the whole result set, but the link to the next chunk is in result. Later GETs return
        // ResultData itself, with no statement/status/manifest wrapper.
        let manifest = resp.manifest.as_ref();
        let truncated = manifest.and_then(|m| m.truncated).unwrap_or(false);
        if truncated {
            anyhow::bail!(
                "Databricks returned a truncated result; row_limit must be raised or the query rewritten"
            );
        }
        let mut next_link = resp
            .result
            .as_ref()
            .and_then(|r| r.next_chunk_internal_link.clone());
        let base = reqwest::Url::parse(&self.conn.base)?;
        while let Some(link) = next_link.take() {
            // internal_link is a path relative to the workspace, not a URL reqwest can send directly.
            // After resolving it, it must stay on the same workspace so it can be requested with its bearer token.
            let url = base.join(&link)?;
            anyhow::ensure!(
                url.origin() == base.origin(),
                "Databricks chunk link points outside the workspace"
            );
            let chunk: ResultData = client
                .get(url)
                .bearer_auth(&self.conn.token)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let chunk_rows = chunk
                .data_array
                .ok_or_else(|| anyhow::anyhow!("Databricks result chunk returned no data_array"))?;
            rows.extend(chunk_rows.into_iter().map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let ty = columns
                            .get(i)
                            .and_then(|c| c.type_text.as_deref())
                            .unwrap_or("");
                        coerce(ty, v)
                    })
                    .collect()
            }));
            next_link = chunk.next_chunk_internal_link;
        }
        // Schema reads should not see truncated=true per the docs; total_row_count is given every time,
        // so the check goes after all chunks are collected: the total in the first response is the count of the whole result set,
        // not the count of the first chunk
        if let (Some(manifest), None) = (manifest, row_limit) {
            let expected = manifest.total_row_count;
            if let Some(expected) = expected {
                let got: u64 = rows.len() as u64;
                if got != expected {
                    anyhow::bail!(
                        "Databricks schema read returned {got} rows but manifest reports {expected}"
                    );
                }
            }
        }
        Ok((columns.into_iter().map(|c| c.name).collect(), rows))
    }
}

/// The engine says "this catalog / table does not exist (or is not visible)". It recognises Databricks' error class names, since they
/// are part of the protocol (`[TABLE_OR_VIEW_NOT_FOUND] …`), not wording
fn is_not_found(e: &anyhow::Error) -> bool {
    const CLASSES: [&str; 4] = [
        "TABLE_OR_VIEW_NOT_FOUND",
        "NO_SUCH_CATALOG_EXCEPTION",
        "CATALOG_NOT_FOUND",
        "SCHEMA_NOT_FOUND",
    ];
    let text = e.to_string();
    CLASSES.iter().any(|c| text.contains(c))
}

#[async_trait::async_trait]
impl QueryEngine for DatabricksEngine {
    async fn test(&self) -> anyhow::Result<()> {
        self.run("SELECT 1", Some(ROW_CAP + 1)).await.map(|_| ())
    }

    async fn fetch_schema(&self) -> anyhow::Result<Vec<SchemaColumn>> {
        // Two candidates, precise first and broad second. On a real cluster, `main`.information_schema.columns returned
        // TABLE_OR_VIEW_NOT_FOUND (#241), so when the first one says **not found**, fall back to the system one.
        //
        // Fall back only on "not found": a wrong token, a stopped warehouse, or a timeout would fail the same way with another statement, and rerunning
        // would only make the person who clicked the button wait another half minute. Reading 0 rows after falling back also counts as failure: both information_schema
        // views list only objects the token can see, and the system one does not yet include hive_metastore, so an invisible catalog
        // there reads as "successfully, no rows"; treating that as success would overwrite the structure document with an empty one.
        // On failure, each form tried is included together with the engine's own words
        //
        // row_limit is not passed (#703): information_schema.columns has one row per column, so on a wide catalog
        // schema docs were silently truncated at 201 rows; now `run` itself follows `next_chunk_internal_link`
        // until it reaches `manifest.total_row_count` rows, so multi-chunk results are collected in full
        let queries = self.schema_queries();
        let mut tried: Vec<String> = Vec::new();
        for (i, (label, sql)) in queries.iter().enumerate() {
            match self.run(sql, None).await {
                Ok((_, rows)) if rows.is_empty() && i > 0 => {
                    tried.push(format!(
                        "{label}: no columns for this catalog — the token cannot see it, \
                         or it is not managed by Unity Catalog"
                    ));
                    break;
                }
                Ok((_, rows)) => {
                    return Ok(rows.into_iter().map(super::trino::schema_row).collect())
                }
                Err(e) => {
                    let missing = is_not_found(&e);
                    tried.push(format!("{label}: {e}"));
                    if !missing {
                        break;
                    }
                }
            }
        }
        anyhow::bail!("no readable information_schema ({})", tried.join("; "))
    }

    async fn execute(&self, sql: &str) -> anyhow::Result<QueryResult> {
        let (columns, rows) = self.run(&wrap_limit(sql), Some(ROW_CAP + 1)).await?;
        let (rows, truncated) = truncate_rows(rows);
        Ok(QueryResult {
            rows: rows_to_json_lines(&columns, &rows),
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::conn::DatabricksConn;
    use super::super::QueryEngine;
    use super::{DatabricksEngine, ROW_CAP};
    use serde_json::json;
    use wiremock::matchers::{
        body_partial_json, body_string_contains, header, method, path, query_param,
    };
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn conn(server: &MockServer) -> DatabricksConn {
        DatabricksConn::parse(&format!(
            "databricks://:dapi-test@{}/sql/1.0/warehouses/wh1?catalog=main&ssl=false",
            server.uri().trim_start_matches("http://")
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn polls_until_succeeded_and_restores_types() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .and(header("authorization", "Bearer dapi-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "statement_id": "s1",
                "status": { "state": "PENDING" }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/2.0/sql/statements/s1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "statement_id": "s1",
                "status": { "state": "SUCCEEDED" },
                "manifest": { "schema": { "columns": [
                    { "name": "region", "type_text": "STRING", "position": 0 },
                    { "name": "total", "type_text": "DECIMAL(12,2)", "position": 1 },
                    { "name": "active", "type_text": "BOOLEAN", "position": 2 }
                ] } },
                "result": { "data_array": [ ["east", "12.50", "true"], ["west", null, "false"] ] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let out = DatabricksEngine::new(conn(&server))
            .execute("SELECT region, total, active FROM orders")
            .await
            .unwrap();
        assert_eq!(
            out.rows,
            vec![
                r#"{"region":"east","total":12.5,"active":true}"#,
                r#"{"region":"west","total":null,"active":false}"#
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_statement_reports_the_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "statement_id": "s2",
                "status": { "state": "FAILED", "error": { "error_code": "BAD_REQUEST", "message": "TABLE_OR_VIEW_NOT_FOUND: nope" } }
            })))
            .mount(&server)
            .await;
        let err = DatabricksEngine::new(conn(&server))
            .execute("SELECT * FROM nope")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("TABLE_OR_VIEW_NOT_FOUND"), "{err}");
    }

    /// If the catalog-level read fails (a real cluster returned TABLE_OR_VIEW_NOT_FOUND), fall back to the system one.
    #[tokio::test]
    async fn the_schema_read_falls_back_to_the_system_information_schema() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .and(body_string_contains("`main`.information_schema.columns"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "statement_id": "s3",
                "status": { "state": "FAILED", "error": {
                    "error_code": "BAD_REQUEST",
                    "message": "[TABLE_OR_VIEW_NOT_FOUND] The table or view `main`.`information_schema`.`columns` cannot be found."
                } }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .and(body_string_contains("system.information_schema.columns"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "statement_id": "s4",
                "status": { "state": "SUCCEEDED" },
                "manifest": { "schema": { "columns": [
                    { "name": "table_schema", "type_text": "STRING", "position": 0 },
                    { "name": "table_name", "type_text": "STRING", "position": 1 },
                    { "name": "column_name", "type_text": "STRING", "position": 2 },
                    { "name": "data_type", "type_text": "STRING", "position": 3 },
                    { "name": "comment", "type_text": "STRING", "position": 4 }
                ] } },
                "result": { "data_array": [
                    ["default", "orders", "amount", "DECIMAL(12,2)", "订单金额"]
                ] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let cols = DatabricksEngine::new(conn(&server))
            .fetch_schema()
            .await
            .unwrap();
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].schema, "default");
        assert_eq!(cols[0].table, "orders");
        assert_eq!(cols[0].column, "amount");
        assert_eq!(cols[0].data_type, "DECIMAL(12,2)");
        assert_eq!(cols[0].comment.as_deref(), Some("订单金额"));
    }

    fn failed(code: &str, message: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "status": { "state": "FAILED", "error": { "error_code": code, "message": message } }
        }))
    }

    /// When both fail, the error carries each form tried and its own wording.
    #[tokio::test]
    async fn two_dead_ends_say_which_ones_were_tried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .and(body_string_contains("`main`.information_schema.columns"))
            .respond_with(failed(
                "BAD_REQUEST",
                "[TABLE_OR_VIEW_NOT_FOUND] first words",
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .and(body_string_contains("system.information_schema.columns"))
            .respond_with(failed("PERMISSION_DENIED", "second words"))
            .mount(&server)
            .await;

        let err = DatabricksEngine::new(conn(&server))
            .fetch_schema()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("catalog information_schema"), "{err}");
        assert!(err.contains("first words"), "{err}");
        assert!(err.contains("system information_schema"), "{err}");
        assert!(err.contains("second words"), "{err}");
    }

    /// A wrong token or a stopped warehouse is not "not found": another statement would fail the same way, so send only once
    #[tokio::test]
    async fn an_error_that_is_not_a_missing_table_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .respond_with(failed("PERMISSION_DENIED", "token lacks USE CATALOG"))
            .expect(1)
            .mount(&server)
            .await;

        let err = DatabricksEngine::new(conn(&server))
            .fetch_schema()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("token lacks USE CATALOG"), "{err}");
        assert!(!err.contains("system information_schema"), "{err}");
    }

    /// The fallback to the system one reads 0 rows: this catalog is not visible, or it is not managed by Unity Catalog.
    /// This is a failure, not an empty structure, since otherwise a refresh would overwrite the existing structure document with an empty one
    #[tokio::test]
    async fn a_fallback_that_reads_no_columns_is_a_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .and(body_string_contains("`main`.information_schema.columns"))
            .respond_with(failed(
                "BAD_REQUEST",
                "[TABLE_OR_VIEW_NOT_FOUND] cannot be found",
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .and(body_string_contains("system.information_schema.columns"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "statement_id": "s5",
                "status": { "state": "SUCCEEDED" },
                "manifest": { "schema": { "columns": [
                    { "name": "table_schema", "type_text": "STRING", "position": 0 }
                ] } },
                "result": { "data_array": [] }
            })))
            .mount(&server)
            .await;

        let err = DatabricksEngine::new(conn(&server))
            .fetch_schema()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no columns for this catalog"), "{err}");
        assert!(err.contains("cannot be found"), "{err}");
    }

    /// A schema read on a wide catalog uses INLINE multi-chunk per the docs: the first chunk gives
    /// `next_chunk_internal_link`; GET it to reach the second chunk, and stop when there is no next page.
    /// Previously row_limit=201 was passed for every statement (#703), so on a wide catalog information_schema.columns
    /// has one row per column, and each table's columns after the first 200 were dropped; now `fetch_schema` passes no
    /// row_limit and pulls all chunks following the link
    #[tokio::test]
    async fn a_schema_read_follows_chunks_until_exhausted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .and(body_string_contains("`main`.information_schema.columns"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "statement_id": "s_chunk1",
                "status": { "state": "SUCCEEDED" },
                "manifest": {
                    "schema": { "columns": [
                        { "name": "table_schema", "type_text": "STRING", "position": 0 },
                        { "name": "table_name", "type_text": "STRING", "position": 1 },
                        { "name": "column_name", "type_text": "STRING", "position": 2 },
                        { "name": "data_type", "type_text": "STRING", "position": 3 },
                        { "name": "comment", "type_text": "STRING", "position": 4 }
                    ] },
                    "total_row_count": 3,
                    "total_chunk_count": 3,
                    "truncated": false
                },
                "result": {
                    "chunk_index": 0, "row_offset": 0, "row_count": 1,
                    "next_chunk_index": 1,
                    "next_chunk_internal_link": "/api/2.0/sql/statements/s_chunk1/result/chunks/1?row_offset=1&cursor=a%2Fb%2Bc",
                    "data_array": [["default", "wide_orders", "id", "BIGINT", "主键"]]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/2.0/sql/statements/s_chunk1/result/chunks/1"))
            .and(query_param("row_offset", "1"))
            .and(query_param("cursor", "a/b+c"))
            .and(header("authorization", "Bearer dapi-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "chunk_index": 1, "row_offset": 1, "row_count": 1,
                "next_chunk_index": 2,
                "next_chunk_internal_link": "/api/2.0/sql/statements/s_chunk1/result/chunks/2?row_offset=2",
                "data_array": [["default", "wide_orders", "region", "STRING", null]]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/2.0/sql/statements/s_chunk1/result/chunks/2"))
            .and(query_param("row_offset", "2"))
            .and(header("authorization", "Bearer dapi-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "chunk_index": 2, "row_offset": 2, "row_count": 1,
                "data_array": [["default", "wide_orders", "amount", "DECIMAL(12,2)", null]]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let cols = DatabricksEngine::new(conn(&server))
            .fetch_schema()
            .await
            .unwrap();
        assert_eq!(cols.len(), 3, "want 3 columns across chunks, got {cols:?}");
        assert_eq!(cols[0].column, "id");
        assert_eq!(cols[1].column, "region");
        assert_eq!(cols[2].column, "amount");
        let requests = server.received_requests().await.unwrap();
        assert!(requests[0]
            .body_json::<serde_json::Value>()
            .unwrap()
            .get("row_limit")
            .is_none());
        assert_eq!(
            requests[1].url.query(),
            Some("row_offset=1&cursor=a%2Fb%2Bc")
        );
    }

    #[tokio::test]
    async fn a_chat_query_reads_later_chunks_before_applying_the_row_cap() {
        for total in [1, 3, ROW_CAP + 1] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/2.0/sql/statements"))
                .and(body_partial_json(json!({"row_limit": ROW_CAP + 1})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "statement_id": "s_chat",
                    "status": { "state": "SUCCEEDED" },
                    "manifest": {
                        "schema": { "columns": [{ "name": "n", "type_text": "BIGINT" }] },
                        "total_row_count": total,
                        "truncated": false
                    },
                    "result": {
                        "data_array": [["0"]],
                        "next_chunk_internal_link": "/api/2.0/sql/statements/s_chat/result/chunks/1?row_offset=1"
                    }
                })))
                .expect(1)
                .mount(&server)
                .await;
            let remaining: Vec<_> = (1..total).map(|n| vec![n.to_string()]).collect();
            Mock::given(method("GET"))
                .and(path("/api/2.0/sql/statements/s_chat/result/chunks/1"))
                .and(query_param("row_offset", "1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "chunk_index": 1, "row_offset": 1, "row_count": total - 1,
                    "data_array": remaining
                })))
                .expect(1)
                .mount(&server)
                .await;
            let out = DatabricksEngine::new(conn(&server))
                .execute("SELECT n FROM numbers")
                .await
                .unwrap();
            let expected: Vec<_> = (0..total.min(ROW_CAP))
                .map(|n| format!(r#"{{"n":{n}}}"#))
                .collect();
            assert_eq!(out.rows, expected);
            assert_eq!(out.truncated, total > ROW_CAP);
        }
    }

    #[tokio::test]
    async fn a_later_chunk_failure_does_not_return_a_partial_query() {
        for (response, expected) in [
            (ResponseTemplate::new(503), "503"),
            (
                ResponseTemplate::new(200).set_body_json(json!({})),
                "data_array",
            ),
            (
                ResponseTemplate::new(200).set_body_json(json!({
                    "status": { "state": "SUCCEEDED" },
                    "result": { "data_array": [["second"]] }
                })),
                "data_array",
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/2.0/sql/statements"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "status": { "state": "SUCCEEDED" },
                    "result": {
                        "data_array": [["first"]],
                        "next_chunk_internal_link": "/api/2.0/sql/statements/s_failed/result/chunks/1"
                    }
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/2.0/sql/statements/s_failed/result/chunks/1"))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            let error = DatabricksEngine::new(conn(&server))
                .execute("SELECT n FROM numbers")
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[tokio::test]
    async fn an_internal_chunk_link_cannot_send_the_token_to_another_origin() {
        let foreign = MockServer::start().await;
        for link in [
            format!("{}/result/chunks/1", foreign.uri()),
            format!("//{}/result/chunks/1", foreign.address()),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/2.0/sql/statements"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "status": { "state": "SUCCEEDED" },
                    "result": {
                        "data_array": [["first"]],
                        "next_chunk_internal_link": link
                    }
                })))
                .expect(1)
                .mount(&server)
                .await;
            let error = DatabricksEngine::new(conn(&server))
                .execute("SELECT n FROM numbers")
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("outside the workspace"),
                "{error}"
            );
        }
        assert!(foreign.received_requests().await.unwrap().is_empty());
    }

    /// When a chat query runs (with row_limit), the server really cuts rows: manifest.truncated=true
    /// should fail directly, rather than pretending "query succeeded" with fewer than ROW_CAP rows
    #[tokio::test]
    async fn a_chat_query_bails_loudly_when_the_service_says_truncated() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "statement_id": "s_trunc",
                "status": { "state": "SUCCEEDED" },
                "manifest": {
                    "schema": { "columns": [
                        { "name": "n", "type_text": "BIGINT", "position": 0 }
                    ] },
                    "total_row_count": 1000,
                    "truncated": true
                },
                "result": { "data_array": [ ["1"], ["2"] ] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let err = DatabricksEngine::new(conn(&server))
            .execute("SELECT n FROM big")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("truncated"), "{err}");
        assert!(err.contains("row_limit"), "{err}");
    }

    /// The schema read's total_row_count from the manifest does not match the rows actually received: the server
    /// silently changed the protocol, or dropped packets midway. Fail fast, so an incomplete schema is not fed into the structure document
    #[tokio::test]
    async fn a_schema_read_fails_when_total_row_count_does_not_match() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .and(body_string_contains("`main`.information_schema.columns"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "statement_id": "s_count_mismatch",
                "status": { "state": "SUCCEEDED" },
                "manifest": {
                    "schema": { "columns": [
                        { "name": "table_schema", "type_text": "STRING", "position": 0 },
                        { "name": "table_name", "type_text": "STRING", "position": 1 },
                        { "name": "column_name", "type_text": "STRING", "position": 2 },
                        { "name": "data_type", "type_text": "STRING", "position": 3 },
                        { "name": "comment", "type_text": "STRING", "position": 4 }
                    ] },
                    "total_row_count": 99
                },
                "result": { "data_array": [
                    ["default", "orders", "id", "BIGINT", null]
                ] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let err = DatabricksEngine::new(conn(&server))
            .fetch_schema()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("schema read returned 1 rows"), "{err}");
        assert!(err.contains("99"), "{err}");
    }

    /// 401/403 class: the token has expired or the PAT was revoked. `error_for_status()` turns non-2xx
    /// into a reqwest error, which looks like "connection failed / auth failed". Passing this layer up to the caller
    /// while still telling "reqwest" apart from a business message is the boundary
    #[tokio::test]
    async fn an_unauthorized_http_status_is_surfaced_as_a_connection_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;
        let err = DatabricksEngine::new(conn(&server))
            .execute("SELECT 1")
            .await
            .unwrap_err()
            .to_string();
        // reqwest's error_for_status wraps the status in a builder error; make sure
        // the caller sees "HTTP 401", not the malformed "Snowflake: …" shape
        assert!(err.contains("401"), "{err}");
    }

    /// 5xx class (warehouse suspended / cluster down) goes through the same error_for_status
    /// path: on the surface it is still a reqwest error, and it needs a recognizable status code
    #[tokio::test]
    async fn a_5xx_response_is_surfaced_with_status_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .respond_with(
                ResponseTemplate::new(503)
                    .set_body_string("Service Unavailable — warehouse suspended"),
            )
            .mount(&server)
            .await;
        let err = DatabricksEngine::new(conn(&server))
            .execute("SELECT 1")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("503"), "{err}");
    }

    /// A user-initiated cancel (cancel button or statement cancellation) is a legitimate Databricks
    /// state: `CANCELED`. Once the business error carries a message, the caller should be able to tell it is "cancelled"
    /// rather than "failed", so the message must be passed through in full
    #[tokio::test]
    async fn a_canceled_statement_carries_a_distinct_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "statement_id": "s3",
                "status": { "state": "CANCELED", "error": { "error_code": "CANCELED", "message": "Statement was canceled by user." } }
            })))
            .mount(&server)
            .await;
        let err = DatabricksEngine::new(conn(&server))
            .execute("SELECT 1")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("CANCELED"), "{err}");
        assert!(err.contains("canceled by user"), "{err}");
    }

    /// If `statement_id` is missing from a PENDING response, the existing logic throws "Databricks returned
    /// no statement_id"; pin that fallback so that changing polling does not swallow it
    #[tokio::test]
    async fn pending_state_without_statement_id_is_surfaced() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": { "state": "PENDING" }
            })))
            .mount(&server)
            .await;
        let err = DatabricksEngine::new(conn(&server))
            .execute("SELECT 1")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no statement_id"),
            "PENDING 但没 statement_id 应被显式提示，避免 polling 失效时不报错：{err}"
        );
    }

    /// `SUCCEEDED` but `result.data_array` is empty (a 0-row query / DDL taking the execute path),
    /// rows are [] and columns is an empty vector too: `rows_to_json_lines` must still return an empty set without crashing
    #[tokio::test]
    async fn a_succeeded_statement_with_no_rows_returns_empty() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/2.0/sql/statements"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "statement_id": "s4",
                "status": { "state": "SUCCEEDED" },
                "manifest": { "schema": { "columns": [] } },
                "result": { "data_array": [] }
            })))
            .mount(&server)
            .await;
        let out = DatabricksEngine::new(conn(&server))
            .execute("SELECT 1 WHERE 1=0")
            .await
            .unwrap();
        assert!(out.rows.is_empty(), "{out:?}");
        assert!(!out.truncated);
    }
}
