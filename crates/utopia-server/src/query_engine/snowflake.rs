//! Snowflake SQL API v2 (`/api/v2/statements`). A statement that does not finish within the synchronous submission (`async=false`) returns 202,
//! and is polled along statementHandle. Values are all strings, restored to numbers and booleans by rowType.
//!
//! Accepts only tokens, not passwords: a programmatic access token or OAuth. Key-pair JWTs would need local signing,
//! which this version does not do: see `conn.rs`.

use super::conn::SnowflakeConn;
use super::{
    coerce, rows_to_json_lines, sql_literal, truncate_rows, wrap_limit, QueryEngine, QueryResult,
    SchemaColumn, HTTP_POLL_BUDGET, STATEMENT_TIMEOUT_SECS,
};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, Instant};

pub struct SnowflakeEngine {
    conn: SnowflakeConn,
}

#[derive(Deserialize)]
struct StatementResponse {
    #[serde(rename = "resultSetMetaData")]
    meta: Option<Meta>,
    data: Option<Vec<Vec<serde_json::Value>>>,
    message: Option<String>,
    code: Option<String>,
    #[serde(rename = "statementHandle")]
    handle: Option<String>,
}

#[derive(Deserialize, Clone)]
struct Meta {
    #[serde(rename = "rowType")]
    row_type: Vec<RowType>,
    #[serde(rename = "numRows")]
    num_rows: Option<u64>,
    /// Each describes only the row count and size of a partition; the partition number is the array index, and later requests use statementHandle.
    #[serde(rename = "partitionInfo")]
    partition_info: Option<Vec<PartitionInfo>>,
}

#[derive(Deserialize, Clone)]
struct RowType {
    name: String,
    #[serde(rename = "type")]
    ty: String,
}

/// Row count of each partition, used to check the complete result set fetched by array index.
#[derive(Deserialize, Clone)]
struct PartitionInfo {
    #[serde(rename = "rowCount")]
    row_count: u64,
}

/// The SQL API gives dates and times as numbers counted from the epoch: DATE is a day count (`18262`), TIME and
/// TIMESTAMP_* are seconds with nine decimal places (`1616173619.000000000`), and TIMESTAMP_TZ adds
/// a space and the time zone as a count of minutes from 1440. Handing these over as-is would leave the model seeing only a big number. Here they are converted back to ISO, and
/// the other types follow `coerce`
fn cell(ty: &str, raw: &serde_json::Value) -> serde_json::Value {
    use chrono::SecondsFormat::AutoSi;
    let serde_json::Value::String(s) = raw else {
        return raw.clone();
    };
    let ty = ty.to_ascii_lowercase();
    let text = match ty.as_str() {
        "date" => s
            .parse::<i64>()
            .ok()
            .and_then(|days| chrono::DateTime::from_timestamp(days.checked_mul(86_400)?, 0))
            .map(|t| t.date_naive().to_string()),
        "time" => epoch(s)
            .and_then(|(secs, nanos)| {
                chrono::NaiveTime::from_num_seconds_from_midnight_opt(
                    u32::try_from(secs).ok()?,
                    nanos,
                )
            })
            .map(|t| t.to_string()),
        "timestamp_ntz" => epoch(s)
            .and_then(|(secs, nanos)| chrono::DateTime::from_timestamp(secs, nanos))
            .map(|t| t.naive_utc().to_string()),
        "timestamp_ltz" => epoch(s)
            .and_then(|(secs, nanos)| chrono::DateTime::from_timestamp(secs, nanos))
            .map(|t| t.to_rfc3339_opts(AutoSi, true)),
        "timestamp_tz" => s.split_once(' ').and_then(|(at, offset)| {
            let minutes = offset.trim().parse::<i32>().ok()? - 1440;
            let zone = chrono::FixedOffset::east_opt(minutes * 60)?;
            let (secs, nanos) = epoch(at)?;
            let t = chrono::DateTime::from_timestamp(secs, nanos)?;
            Some(t.with_timezone(&zone).to_rfc3339_opts(AutoSi, false))
        }),
        _ => return coerce(&ty, raw),
    };
    text.map_or_else(|| raw.clone(), serde_json::Value::String)
}

/// `-1.500000000` → (-2, 500000000): seconds round down, and the nanoseconds are always positive
fn epoch(s: &str) -> Option<(i64, u32)> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let secs = whole.parse::<i64>().ok()?;
    if frac.len() > 9 || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let nanos = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<9}").parse::<u32>().ok()?
    };
    if nanos > 0 && whole.starts_with('-') {
        Some((secs - 1, 1_000_000_000 - nanos))
    } else {
        Some((secs, nanos))
    }
}

impl SnowflakeEngine {
    pub fn new(conn: SnowflakeConn) -> Self {
        Self { conn }
    }

    fn request(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        r.bearer_auth(&self.conn.token)
            .header("X-Snowflake-Authorization-Token-Type", self.conn.token_type)
            .header("Accept", "application/json")
    }

    async fn run(&self, sql: &str) -> anyhow::Result<(Vec<String>, Vec<Vec<serde_json::Value>>)> {
        let client = super::http()?;
        let mut body = json!({
            "statement": sql,
            "timeout": STATEMENT_TIMEOUT_SECS,
            "parameters": { "MULTI_STATEMENT_COUNT": "1" },
        });
        for (key, value) in [
            ("database", &self.conn.database),
            ("schema", &self.conn.schema),
            ("warehouse", &self.conn.warehouse),
            ("role", &self.conn.role),
        ] {
            if let Some(v) = value {
                body[key] = json!(v);
            }
        }
        let mut http = self
            .request(client.post(format!("{}/api/v2/statements?async=false", self.conn.base)))
            .json(&body)
            .send()
            .await?;
        let started = Instant::now();
        // 202 = still running; any other non-2xx body carries a message
        while http.status() == StatusCode::ACCEPTED {
            let partial: StatementResponse = http.json().await?;
            let handle = partial.handle.ok_or_else(|| {
                anyhow::anyhow!("Snowflake returned 202 without a statementHandle")
            })?;
            if started.elapsed() > HTTP_POLL_BUDGET {
                anyhow::bail!(
                    "Snowflake statement did not finish within {}s",
                    HTTP_POLL_BUDGET.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            http = self
                .request(client.get(format!("{}/api/v2/statements/{handle}", self.conn.base)))
                .send()
                .await?;
        }
        if !http.status().is_success() {
            let status = http.status();
            let text = http.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<StatementResponse>(&text)
                .ok()
                .and_then(|r| r.message)
                .unwrap_or(text);
            anyhow::bail!("Snowflake {status}: {msg}");
        }
        let resp: StatementResponse = http.json().await?;
        if let (Some(code), Some(message)) = (&resp.code, &resp.message) {
            // A 2xx can also carry a business error code; 090001 is "statement executed successfully"
            if code != "090001" && resp.meta.is_none() {
                anyhow::bail!("Snowflake {code}: {message}");
            }
        }
        // Copy the meta out before consuming it: below we need both to fetch chunks from partition_info
        // and to restore types from row_type
        let meta = resp.meta.clone();
        let types: Vec<RowType> = meta
            .as_ref()
            .map(|m| m.row_type.clone())
            .unwrap_or_default();
        let partitions: Vec<PartitionInfo> = meta
            .as_ref()
            .and_then(|m| m.partition_info.clone())
            .unwrap_or_default();
        let expected_total_rows: Option<u64> = meta.as_ref().and_then(|m| m.num_rows);
        let mut rows: Vec<Vec<serde_json::Value>> = resp
            .data
            .unwrap_or_default()
            .into_iter()
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(i, v)| cell(types.get(i).map(|t| t.ty.as_str()).unwrap_or(""), v))
                    .collect()
            })
            .collect();
        // partitionInfo has no URL. The first partition is already in the response; later ones are read by statementHandle and the
        // 1-based partition parameter; later responses no longer carry column metadata, so the first chunk's types are reused.
        let mut seen_partition_rows: u64 = rows.len() as u64;
        if partitions.len() > 1 {
            let handle = resp.handle.as_deref().ok_or_else(|| {
                anyhow::anyhow!("Snowflake returned multiple partitions without a statementHandle")
            })?;
            for partition in 1..partitions.len() {
                let chunk_resp: StatementResponse = self
                    .request(client.get(format!(
                        "{}/api/v2/statements/{handle}?partition={partition}",
                        self.conn.base
                    )))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                let chunk_rows = chunk_resp.data.unwrap_or_default();
                seen_partition_rows = seen_partition_rows.saturating_add(chunk_rows.len() as u64);
                rows.extend(chunk_rows.into_iter().map(|row| {
                    row.iter()
                        .enumerate()
                        .map(|(i, v)| cell(types.get(i).map(|t| t.ty.as_str()).unwrap_or(""), v))
                        .collect()
                }));
            }
        }
        // numRows is the total of the whole result set (not the current chunk); the sum of the partition rowCounts should equal it too
        // — any mismatch means the server quietly changed the protocol or dropped packets mid-way, so incomplete data must not be poured into the structured document
        if let Some(expected) = expected_total_rows {
            if seen_partition_rows != expected {
                anyhow::bail!(
                    "Snowflake schema read returned {seen_partition_rows} rows across partitions but numRows reports {expected}"
                );
            }
        }
        let partition_total: u64 = partitions.iter().map(|p| p.row_count).sum();
        if !partitions.is_empty() && partition_total != seen_partition_rows {
            anyhow::bail!(
                "Snowflake partition row counts sum to {partition_total} but received {seen_partition_rows} rows"
            );
        }
        Ok((types.into_iter().map(|t| t.name).collect(), rows))
    }
}

#[async_trait::async_trait]
impl QueryEngine for SnowflakeEngine {
    async fn test(&self) -> anyhow::Result<()> {
        self.run("SELECT 1").await.map(|_| ())
    }

    async fn fetch_schema(&self) -> anyhow::Result<Vec<SchemaColumn>> {
        let database = self.conn.database.as_deref().ok_or_else(|| {
            anyhow::anyhow!("snowflake://: put the database in the connection string (snowflake://:TOKEN@account/DATABASE) so the schema can be read")
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
             WHERE table_schema <> 'INFORMATION_SCHEMA'{schema_filter} \
             ORDER BY table_schema, table_name, ordinal_position",
            database.replace('"', "\"\"")
        );
        let (_, rows) = self.run(&sql).await?;
        Ok(rows.into_iter().map(super::trino::schema_row).collect())
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

#[cfg(test)]
mod tests {
    use super::super::conn::SnowflakeConn;
    use super::super::QueryEngine;
    use super::SnowflakeEngine;
    use serde_json::json;
    use std::io::Write;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn conn(server: &MockServer) -> SnowflakeConn {
        SnowflakeConn::parse(&format!(
            "snowflake://:pat-test@{}/ANALYTICS/PUBLIC?warehouse=WH&ssl=false",
            server.uri().trim_start_matches("http://")
        ))
        .unwrap()
    }

    // The SQL API compresses later partitions and omits their metadata.
    fn compressed_partition(data: serde_json::Value) -> ResponseTemplate {
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(
            serde_json::to_string(&json!({ "data": data }))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        ResponseTemplate::new(200)
            .insert_header("Content-Encoding", "gzip")
            .set_body_bytes(gzip.finish().unwrap())
    }

    #[tokio::test]
    async fn a_synchronous_answer_is_typed_by_row_type() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/statements"))
            .and(header("authorization", "Bearer pat-test"))
            .and(header(
                "X-Snowflake-Authorization-Token-Type",
                "PROGRAMMATIC_ACCESS_TOKEN",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "resultSetMetaData": { "numRows": 1, "rowType": [
                    { "name": "REGION", "type": "text" },
                    { "name": "TOTAL", "type": "fixed", "scale": 2 }
                ] },
                "data": [ ["east", "42.10"] ],
                "code": "090001",
                "statementHandle": "h1",
                "message": "Statement executed successfully."
            })))
            .expect(1)
            .mount(&server)
            .await;
        let out = SnowflakeEngine::new(conn(&server))
            .execute("SELECT region, total FROM orders")
            .await
            .unwrap();
        assert_eq!(out.rows, vec![r#"{"REGION":"east","TOTAL":42.1}"#]);
    }

    /// The SQL API's dates and times are counts from the epoch: DATE is days, TIME and TIMESTAMP_* are seconds,
    /// and TIMESTAMP_TZ is followed by 1440 plus the time zone in minutes
    #[tokio::test]
    async fn dates_and_times_read_as_dates_and_times() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/statements"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "resultSetMetaData": { "numRows": 2, "rowType": [
                    { "name": "D", "type": "date" },
                    { "name": "T", "type": "time" },
                    { "name": "NTZ", "type": "timestamp_ntz" },
                    { "name": "LTZ", "type": "timestamp_ltz" },
                    { "name": "TZ", "type": "timestamp_tz" }
                ] },
                "data": [
                    ["18262", "82919.000000000", "1616173619.000000000", "1616173619.500000000", "1616173619.000000000 1500"],
                    ["-1", "0.000000000", "-1.500000000", null, "1616173619.000000000 1080"]
                ],
                "code": "090001",
                "statementHandle": "h1"
            })))
            .mount(&server)
            .await;
        let out = SnowflakeEngine::new(conn(&server))
            .execute("SELECT d, t, ntz, ltz, tz FROM events")
            .await
            .unwrap();
        assert_eq!(
            out.rows,
            vec![
                r#"{"D":"2020-01-01","T":"23:01:59","NTZ":"2021-03-19 17:06:59","LTZ":"2021-03-19T17:06:59.500Z","TZ":"2021-03-19T18:06:59+01:00"}"#,
                r#"{"D":"1969-12-31","T":"00:00:00","NTZ":"1969-12-31 23:59:58.500","LTZ":null,"TZ":"2021-03-19T11:06:59-06:00"}"#,
            ]
        );
    }

    #[tokio::test]
    async fn a_202_is_polled_and_its_partitions_keep_the_first_rows_types() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/statements"))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({
                "code": "333334", "statementHandle": "h2", "message": "Asynchronous execution in progress."
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/statements/h2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "resultSetMetaData": {
                    "numRows": 3,
                    "rowType": [ { "name": "N", "type": "fixed" } ],
                    "partitionInfo": [ { "rowCount": 1 }, { "rowCount": 2 } ]
                },
                "data": [ ["1"] ], "code": "090001", "statementHandle": "h2"
            })))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/statements/h2"))
            .and(query_param("partition", "1"))
            .and(header("authorization", "Bearer pat-test"))
            .and(header(
                "X-Snowflake-Authorization-Token-Type",
                "PROGRAMMATIC_ACCESS_TOKEN",
            ))
            .respond_with(compressed_partition(json!([["2"], [null]])))
            .expect(1)
            .mount(&server)
            .await;
        let out = SnowflakeEngine::new(conn(&server))
            .execute("SELECT 1 AS n")
            .await
            .unwrap();
        assert_eq!(out.rows, vec![r#"{"N":1}"#, r#"{"N":2}"#, r#"{"N":null}"#]);
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn an_error_body_is_surfaced() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/statements"))
            .respond_with(ResponseTemplate::new(422).set_body_json(json!({
                "code": "002003", "message": "SQL compilation error: Object 'NOPE' does not exist"
            })))
            .mount(&server)
            .await;
        let err = SnowflakeEngine::new(conn(&server))
            .execute("SELECT * FROM nope")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    /// partitionInfo only describes row counts and sizes. Partition zero is
    /// inline; subsequent partitions use the statement handle and their index.
    #[tokio::test]
    async fn a_schema_read_follows_partitions_until_exhausted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/statements"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "resultSetMetaData": {
                    "numRows": 3,
                    "rowType": [
                        { "name": "TABLE_SCHEMA", "type": "text" },
                        { "name": "TABLE_NAME", "type": "text" },
                        { "name": "COLUMN_NAME", "type": "text" },
                        { "name": "DATA_TYPE", "type": "text" },
                        { "name": "COMMENT", "type": "text" }
                    ],
                    "partitionInfo": [
                        { "rowCount": 1, "uncompressedSize": 120 },
                        { "rowCount": 1, "uncompressedSize": 130, "compressedSize": 80 },
                        { "rowCount": 1, "uncompressedSize": 140, "compressedSize": 90 }
                    ]
                },
                "data": [
                    ["default", "wide_orders", "id", "BIGINT", "主键"]
                ],
                "code": "090001",
                "statementHandle": "h3",
                "message": "Statement executed successfully."
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/statements/h3"))
            .and(query_param("partition", "1"))
            .and(header("authorization", "Bearer pat-test"))
            .respond_with(compressed_partition(json!([[
                "default",
                "wide_orders",
                "region",
                "STRING",
                null
            ]])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/statements/h3"))
            .and(query_param("partition", "2"))
            .and(header("authorization", "Bearer pat-test"))
            .respond_with(compressed_partition(json!([[
                "default",
                "wide_orders",
                "amount",
                "DECIMAL(12,2)",
                null
            ]])))
            .expect(1)
            .mount(&server)
            .await;

        let cols = SnowflakeEngine::new(conn(&server))
            .fetch_schema()
            .await
            .unwrap();
        assert_eq!(
            cols.len(),
            3,
            "want 3 columns across partitions, got {cols:?}"
        );
        assert_eq!(cols[0].column, "id");
        assert_eq!(cols[1].column, "region");
        assert_eq!(cols[2].column, "amount");
    }

    #[tokio::test]
    async fn multiple_partitions_require_a_statement_handle() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/statements"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "resultSetMetaData": {
                    "rowType": [{ "name": "N", "type": "fixed" }],
                    "partitionInfo": [{ "rowCount": 1 }, { "rowCount": 1 }]
                },
                "data": [["1"]]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let err = SnowflakeEngine::new(conn(&server))
            .execute("SELECT n FROM numbers")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("multiple partitions without a statementHandle"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn failed_or_incomplete_partitions_do_not_return_partial_rows() {
        for (response, expected_error) in [
            (ResponseTemplate::new(503), "503"),
            (compressed_partition(json!([])), "numRows reports 2"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/v2/statements"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "resultSetMetaData": {
                        "numRows": 2,
                        "rowType": [{ "name": "N", "type": "fixed" }],
                        "partitionInfo": [{ "rowCount": 1 }, { "rowCount": 1 }]
                    },
                    "data": [["1"]], "statementHandle": "h4"
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v2/statements/h4"))
                .and(query_param("partition", "1"))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            let err = SnowflakeEngine::new(conn(&server))
                .execute("SELECT n FROM numbers")
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected_error), "{err}");
        }
    }

    /// 401/403 kinds (auth, permissions) also take the status path: the message field is still sent through.
    /// Apart from the status code, the difference from 422 is nothing; the shape of the error the caller gets should stay the same (all
    /// `Snowflake {status}: {message}`), so that callers above can classify by error shape
    #[tokio::test]
    async fn auth_failure_body_is_surfaced() {
        for status in [401u16, 403, 503] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/v2/statements"))
                .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                    "code": "390100",
                    "message": format!("HTTP {status}: not authorized — token expired or missing role"),
                })))
                .mount(&server)
                .await;
            let err = SnowflakeEngine::new(conn(&server))
                .execute("SELECT 1")
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(&status.to_string()), "{err}");
            assert!(err.contains("token expired"), "{err}");
        }
    }

    /// For 5xx kinds (such as Snowflake's internal 504 / 503, or a gateway mismatch) the body is not JSON:
    /// the existing logic tries JSON.parse and then falls back to the raw text, so the "message wording" still reaches
    /// the caller. Pin this fallback path so no one "beautifies" it away
    #[tokio::test]
    async fn non_json_error_body_falls_back_to_plain_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/statements"))
            .respond_with(
                ResponseTemplate::new(500).set_body_string("Internal error: warehouse suspended\n"),
            )
            .mount(&server)
            .await;
        let err = SnowflakeEngine::new(conn(&server))
            .execute("SELECT 1")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("500"), "{err}");
        assert!(
            err.contains("warehouse suspended"),
            "非 JSON body 应原样透传，给上层一份能落库的描述：{err}"
        );
    }

    /// If the 2xx body carries a code other than "success" (such as the query-canceled signal), it must be thrown in the business
    /// error shape and must not be hidden in a response that looks OK: this is Snowflake's two-layer error
    /// model: it should share the same message channel as 4xx/5xx
    #[tokio::test]
    async fn business_error_code_in_a_2xx_body_is_bubbled_up() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/statements"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "000605",
                "message": "Statement was canceled by user.",
                // No resultSetMetaData, consistent with the status: a success response must carry either meta or data
            })))
            .mount(&server)
            .await;
        let err = SnowflakeEngine::new(conn(&server))
            .execute("SELECT 1")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("000605"), "{err}");
        assert!(err.contains("canceled"), "{err}");
    }
}
