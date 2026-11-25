//! Trino (formerly Presto): REST protocol `POST /v1/statement`, then fetches page by page along `nextUri`.
//! One engine tops the whole lakehouse -- Iceberg / Delta / Hive / Hudi are all its catalogs,
//! changing the format does not change the protocol. Starburst uses the same protocol.
//!
//! There is no session to set read-only: the timeout is set via `X-Trino-Session: query_max_execution_time`,
//! and read-only relies on `guard_sql_for`.

use super::conn::TrinoConn;
use super::{
    coerce, rows_to_json_lines, sql_literal, truncate_rows, wrap_limit, QueryEngine, QueryResult,
    SchemaColumn, HTTP_POLL_BUDGET, STATEMENT_TIMEOUT_SECS,
};
use base64::Engine as _;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;
use std::time::Instant;

pub struct TrinoEngine {
    conn: TrinoConn,
}

#[derive(Deserialize)]
struct Column {
    name: String,
    #[serde(rename = "type", default)]
    ty: String,
}

#[derive(Deserialize)]
struct Page {
    #[serde(rename = "nextUri")]
    next_uri: Option<String>,
    columns: Option<Vec<Column>>,
    data: Option<Vec<Vec<serde_json::Value>>>,
    error: Option<TrinoError>,
}

#[derive(Deserialize)]
struct TrinoError {
    message: String,
    #[serde(rename = "errorName")]
    error_name: Option<String>,
}

impl TrinoEngine {
    pub fn new(conn: TrinoConn) -> Self {
        Self { conn }
    }

    fn headers(&self) -> anyhow::Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert("X-Trino-User", HeaderValue::from_str(&self.conn.user)?);
        h.insert("X-Trino-Source", HeaderValue::from_static("utopia"));
        h.insert(
            "X-Trino-Session",
            HeaderValue::from_str(&format!(
                "query_max_execution_time={STATEMENT_TIMEOUT_SECS}s"
            ))?,
        );
        if let Some(c) = &self.conn.catalog {
            h.insert("X-Trino-Catalog", HeaderValue::from_str(c)?);
        }
        if let Some(s) = &self.conn.schema {
            h.insert("X-Trino-Schema", HeaderValue::from_str(s)?);
        }
        if let Some(p) = &self.conn.password {
            let raw = format!("{}:{p}", self.conn.user);
            let token = base64::engine::general_purpose::STANDARD.encode(raw);
            h.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {token}"))?,
            );
        }
        Ok(h)
    }

    /// Submit and drain along nextUri: columns come on the first page that carries them, and data accumulates across pages
    async fn run(&self, sql: &str) -> anyhow::Result<(Vec<String>, Vec<Vec<serde_json::Value>>)> {
        let client = super::http()?;
        let headers = self.headers()?;
        let mut page: Page = client
            .post(format!("{}/v1/statement", self.conn.base))
            .headers(headers.clone())
            .body(sql.to_string())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let started = Instant::now();
        let mut columns: Option<Vec<Column>> = None;
        let mut rows = Vec::new();
        loop {
            if let Some(e) = page.error {
                let name = e.error_name.map(|n| format!("{n}: ")).unwrap_or_default();
                anyhow::bail!("{name}{}", e.message);
            }
            if columns.is_none() {
                columns = page.columns.take();
            }
            if let Some(d) = page.data.take() {
                rows.extend(d);
            }
            let Some(next) = page.next_uri.take() else {
                break;
            };
            if started.elapsed() > HTTP_POLL_BUDGET {
                anyhow::bail!(
                    "Trino query did not finish within {}s",
                    HTTP_POLL_BUDGET.as_secs()
                );
            }
            page = client
                .get(&next)
                .headers(headers.clone())
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
        }
        let columns = columns.unwrap_or_default();
        // The protocol writes decimal as a string (other numbers are JSON numbers), restored by column type
        for row in &mut rows {
            for (v, c) in row.iter_mut().zip(&columns) {
                if c.ty.starts_with("decimal") {
                    *v = coerce("DECIMAL", v);
                }
            }
        }
        Ok((columns.into_iter().map(|c| c.name).collect(), rows))
    }
}

#[async_trait::async_trait]
impl QueryEngine for TrinoEngine {
    async fn test(&self) -> anyhow::Result<()> {
        self.run("SELECT 1").await.map(|_| ())
    }

    async fn fetch_schema(&self) -> anyhow::Result<Vec<SchemaColumn>> {
        let catalog = self.conn.catalog.as_deref().ok_or_else(|| {
            anyhow::anyhow!("trino://: put the catalog in the connection string (trino://user@host/CATALOG) so the schema can be read")
        })?;
        let schema_filter = self
            .conn
            .schema
            .as_deref()
            .map(|s| format!(" AND table_schema = {}", sql_literal(s)))
            .unwrap_or_default();
        let sql = format!(
            "SELECT table_schema, table_name, column_name, data_type, comment \
             FROM \"{}\".information_schema.columns \
             WHERE table_schema <> 'information_schema'{schema_filter} \
             ORDER BY table_schema, table_name, ordinal_position",
            catalog.replace('"', "\"\"")
        );
        let (_, rows) = self.run(&sql).await?;
        Ok(rows.into_iter().map(schema_row).collect())
    }

    async fn execute(&self, sql: &str) -> anyhow::Result<QueryResult> {
        let (columns, rows) = self.run(&wrap_limit(sql)).await?;
        let (rows, truncated) = truncate_rows(rows);
        Ok(QueryResult {
            rows: rows_to_json_lines(&columns, &rows),
            truncated,
        })
    }
}

/// One row of information_schema -> SchemaColumn (a value may be null; comment often is)
pub(crate) fn schema_row(row: Vec<serde_json::Value>) -> SchemaColumn {
    let text = |i: usize| -> String {
        row.get(i)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    SchemaColumn {
        schema: text(0),
        table: text(1),
        column: text(2),
        data_type: text(3),
        comment: row
            .get(4)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        // Keys are always "unknown" here (#502). The four engines differ, so do not read them by the SQL standard:
        // - Trino / Presto: each catalog's `information_schema` is provided by the built-in connector, with only
        //   columns, tables, views, schemata, and the privilege and role tables; there is **no**
        //   `table_constraints` / `key_column_usage` -- querying them gives TABLE_NOT_FOUND (#682);
        // - Snowflake: no `key_column_usage`; column-level keys come from `SHOW PRIMARY KEYS` /
        //   `SHOW IMPORTED KEYS`;
        // - Databricks: Unity Catalog records informational primary and foreign keys in `information_schema.table_constraints` /
        //   `key_column_usage` / `referential_constraints`, which can be read;
        // - ClickHouse: `system.columns` has `is_in_primary_key`, but the primary key is a prefix of the sorting key,
        //   not guaranteed unique -- MergeTree accepts two rows with the same primary key, and marking it as a key would make the exploration prompt take
        //   a column that can repeat as an ID
        is_primary_key: false,
        references_table: None,
    }
}

#[cfg(test)]
mod live_tests {
    use super::super::conn::TrinoConn;
    use super::super::QueryEngine;
    use super::TrinoEngine;

    /// Runs against a real Trino -- the tier that does so. Without `UTOPIA_TEST_TRINO_URL` the test is skipped:
    /// wiremock playback proves protocol paging and column order, but cannot prove what the real cluster's information_schema looks like, or that values fetched page by page via REST decode correctly.
    /// Only a real server has an answer to those two.
    ///
    /// Start one to run against (the built-in tpch catalog: data ready-made, schema fixed):
    /// `docker run -d -p 8080:8080 trinodb/trino`
    /// then `UTOPIA_TEST_TRINO_URL=trino://probe@127.0.0.1:8080/tpch/tiny`.
    fn live_url() -> Option<String> {
        std::env::var("UTOPIA_TEST_TRINO_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
    }

    #[tokio::test]
    async fn a_live_cluster_reads_schema_and_answers() {
        let Some(url) = live_url() else {
            return;
        };
        let engine = TrinoEngine::new(TrinoConn::parse(&url).expect("parse url"));
        engine.test().await.expect("SELECT 1");

        // fetch_schema queries "<catalog>".information_schema.columns -- the column names and the spelling
        // can only be proven by a real cluster. The schema segment limits the scope (tpch has many sfN; only tiny is taken)
        let schema = engine.fetch_schema().await.expect("schema");
        let name = schema
            .iter()
            .find(|c| c.table == "region" && c.column == "name")
            .expect("tpch.tiny.region.name in information_schema");
        assert_eq!(name.schema, "tiny", "schema segment should be qualified");
        assert!(
            name.data_type.starts_with("varchar"),
            "data_type 要给出真形态，拿到的是 {}",
            name.data_type
        );

        // Value round-trip: QUEUED -> fetch page by page along nextUri until data arrives. tpch.tiny.region
        // is a TPC-H standard table, five rows with fixed content, exactly right as a criterion
        let r = engine
            .execute("SELECT regionkey, name FROM tpch.tiny.region ORDER BY regionkey")
            .await
            .expect("execute");
        assert_eq!(r.rows.len(), 5, "region has five rows");
        let first: serde_json::Value = serde_json::from_str(&r.rows[0]).unwrap();
        assert_eq!(first["regionkey"], serde_json::json!(0));
        assert_eq!(first["name"], serde_json::json!("AFRICA"));
        let last: serde_json::Value = serde_json::from_str(&r.rows[4]).unwrap();
        assert_eq!(last["name"], serde_json::json!("MIDDLE EAST"));

        // The write path is still blocked by the gate (layer 1), symmetric with the mysql tier
        assert!(super::super::guard_sql_for("trino", "DROP TABLE tpch.tiny.region").is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::super::conn::TrinoConn;
    use super::super::QueryEngine;
    use super::TrinoEngine;
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn follows_next_uri_and_keeps_column_order() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/statement"))
            .and(header("X-Trino-User", "alice"))
            .and(header("X-Trino-Catalog", "hive"))
            .and(body_string_contains("LIMIT 201"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "q1",
                "nextUri": format!("{}/v1/statement/q1/1", server.uri()),
                "stats": { "state": "QUEUED" }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/statement/q1/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "q1",
                "columns": [ { "name": "region", "type": "varchar" }, { "name": "total", "type": "double" } ],
                "data": [ ["east", 12.5], ["west", 3] ],
                "stats": { "state": "FINISHED" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let uri = server.uri();
        let conn = TrinoConn::parse(&format!(
            "trino://alice@{}/hive/default?ssl=false",
            uri.trim_start_matches("http://")
        ))
        .unwrap();
        let out = TrinoEngine::new(conn)
            .execute("SELECT region, total FROM orders")
            .await
            .unwrap();
        assert_eq!(
            out.rows,
            vec![
                r#"{"region":"east","total":12.5}"#,
                r#"{"region":"west","total":3}"#
            ]
        );
        assert!(!out.truncated);
    }

    /// Trino's REST protocol writes decimal as a string (`SqlDecimal` goes through `writeString`),
    /// other numbers are JSON numbers
    #[tokio::test]
    async fn a_decimal_reads_as_a_number() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/statement"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "q3",
                "columns": [
                    { "name": "region", "type": "varchar(5)" },
                    { "name": "total", "type": "decimal(12,2)" },
                    { "name": "orders", "type": "bigint" }
                ],
                "data": [ ["east", "1234.50", 7], ["west", null, 0] ],
                "stats": { "state": "FINISHED" }
            })))
            .mount(&server)
            .await;
        let conn = TrinoConn::parse(&format!(
            "trino://alice@{}/hive?ssl=false",
            server.uri().trim_start_matches("http://")
        ))
        .unwrap();
        let out = TrinoEngine::new(conn)
            .execute(
                "SELECT region, SUM(amount) AS total, COUNT(*) AS orders FROM orders GROUP BY 1",
            )
            .await
            .unwrap();
        assert_eq!(
            out.rows,
            vec![
                r#"{"region":"east","total":1234.5,"orders":7}"#,
                r#"{"region":"west","total":null,"orders":0}"#
            ]
        );
    }

    #[tokio::test]
    async fn a_trino_error_page_becomes_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/statement"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "q2",
                "error": { "message": "line 1:8: Table 'hive.default.nope' does not exist", "errorName": "TABLE_NOT_FOUND" },
                "stats": { "state": "FAILED" }
            })))
            .mount(&server)
            .await;
        let conn = TrinoConn::parse(&format!(
            "trino://alice@{}/hive?ssl=false",
            server.uri().trim_start_matches("http://")
        ))
        .unwrap();
        let err = TrinoEngine::new(conn)
            .execute("SELECT * FROM nope")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("TABLE_NOT_FOUND"), "{err}");
    }
}
