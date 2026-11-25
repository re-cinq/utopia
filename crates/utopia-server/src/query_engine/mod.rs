//! Query engine for questions: trait seam (same technique as BlobStore) + engine-agnostic safety gate.
//!
//! Engines grow by protocol family, not by product name: the postgres wire protocol (`postgres.rs`), the MySQL wire protocol
//! (`mysql.rs`, one protocol also covers TiDB / OceanBase / Doris / StarRocks / MariaDB)
//! -> HTTP families --
//! `trino.rs` alone tops the whole Iceberg / Delta / Hive lakehouse ecosystem; `databricks.rs`,
//! `snowflake.rs` each go through their own SQL REST API; `clickhouse.rs` goes through ClickHouse's HTTP interface.
//! The mount model and the registry are engine-agnostic; adding an engine only relaxes one CHECK. The connection string is the only input: the engine is decided by its
//! scheme ([`engine_from_conn`]), and the rest is parsed by each engine itself (`conn.rs`);
//! credentials only ever flow on the server side.
//!
//! Safety gate (defense in depth, the model is not trusted):
//! 1. sqlparser parsing: only a single SELECT/WITH (including CTEs) is let through; DML/DDL/multi-statement/SELECT INTO are refused.
//!    The dialect is chosen per engine; sqlparser has no Trino dialect, so Generic, which is its superset, is used
//! 2. Always wrap in an outer LIMIT (cap+1 probe detects truncation)
//! 3. Session-level read-only + statement timeout (each engine's own mechanism, so that even if the parser slips, nothing can be written).
//!    HTTP families have no session, only a statement timeout -- read-only relies on layer 1, which is the layer they have fewer of than the wire protocols.
//!    ClickHouse is the exception: each request carries `readonly` itself, see `clickhouse.rs`
//! 4. Results are unified as JSON Lines: PG lets the database do the conversion; the HTTP families get column names and values and assemble here, preserving column order

mod clickhouse;
mod conn;
mod databricks;
mod mysql;
mod postgres;
mod snowflake;
mod trino;

use sqlparser::ast::Statement;
use sqlparser::dialect::{
    ClickHouseDialect, DatabricksDialect, GenericDialect, MySqlDialect, PostgreSqlDialect,
    SnowflakeDialect,
};
use sqlparser::parser::Parser;
use std::time::Duration;

/// Row-count cap (outer LIMIT cap+1; row 201 is used only to detect truncation).
pub const ROW_CAP: usize = 200;
pub(crate) const STATEMENT_TIMEOUT_SECS: u32 = 10;
/// HTTP families: timeout for a single request, and the polling budget from submitting the whole statement to fetching its result
pub(crate) const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const HTTP_POLL_BUDGET: Duration = Duration::from_secs(30);

/// Values of the `engine` column in the registry. The CHECK in the migration must match this table
pub const ENGINES: &[&str] = &[
    "postgres",
    "mysql",
    "trino",
    "databricks",
    "snowflake",
    "clickhouse",
];

#[derive(Debug)]
pub struct QueryResult {
    /// Each row as one JSON object text (key order = query column order)
    pub rows: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug)]
pub struct SchemaColumn {
    pub schema: String,
    pub table: String,
    pub column: String,
    pub data_type: String,
    pub comment: Option<String>,
    /// This column is itself the table's primary key (single-column primary key; in a composite key this bit is still false).
    /// Read from the catalog for Postgres (#671) and MySQL / MariaDB (#678); Trino / Snowflake /
    /// Databricks / ClickHouse are always false, meaning "unknown", not "not a key"
    /// (see the comment on `trino::schema_row` for whether each can read it).
    /// The exploration prompt relies on this to separate IDs from quantities; a wide table of eighty columns with no such marker
    /// can hardly tell which column is the key (#502)
    pub is_primary_key: bool,
    /// When this column is itself a single-column foreign key of some table, the table it points to (`schema.table`); members of a composite foreign key,
    /// and engines that cannot read foreign keys, are always None.
    ///
    /// Nullability is not included: neither exploration prompt uses it, and on a wide table one extra NOT NULL per column only pushes
    /// columns out (added when #502 really needs it)
    pub references_table: Option<String>,
}

#[async_trait::async_trait]
pub trait QueryEngine: Send + Sync {
    async fn test(&self) -> anyhow::Result<()>;
    async fn fetch_schema(&self) -> anyhow::Result<Vec<SchemaColumn>>;
    /// Execute a SELECT that has passed the gate. The implementation must still enforce a read-only session and timeout itself (defense in depth).
    async fn execute(&self, sql: &str) -> anyhow::Result<QueryResult>;
}

/// scheme -> engine name. Each engine's form in the UI ends up assembled into one connection string (`dsSpecs`); this is its only dispatch point.
pub fn engine_from_conn(conn: &str) -> Option<&'static str> {
    let scheme = conn.trim().split("://").next()?.to_ascii_lowercase();
    match scheme.as_str() {
        "postgres" | "postgresql" => Some("postgres"),
        // One wire protocol also covers TiDB / OceanBase / Doris / StarRocks -- they all speak
        // the MySQL protocol, so writing the mysql:// connection string is enough
        "mysql" | "mariadb" => Some("mysql"),
        "trino" | "presto" => Some("trino"),
        "databricks" => Some("databricks"),
        "snowflake" => Some("snowflake"),
        "clickhouse" => Some("clickhouse"),
        _ => None,
    }
}

/// Engine factory. Credentials only flow on the server side.
pub fn engine_for(engine: &str, conn: &str) -> anyhow::Result<Box<dyn QueryEngine>> {
    match engine {
        "postgres" => Ok(Box::new(postgres::PostgresEngine::new(conn))),
        "mysql" => Ok(Box::new(mysql::MysqlEngine::new(conn))),
        "trino" => Ok(Box::new(trino::TrinoEngine::new(conn::TrinoConn::parse(
            conn,
        )?))),
        "databricks" => Ok(Box::new(databricks::DatabricksEngine::new(
            conn::DatabricksConn::parse(conn)?,
        ))),
        "snowflake" => Ok(Box::new(snowflake::SnowflakeEngine::new(
            conn::SnowflakeConn::parse(conn)?,
        ))),
        "clickhouse" => Ok(Box::new(clickhouse::ClickHouseEngine::new(
            conn::ClickHouseConn::parse(conn)?,
        ))),
        other => anyhow::bail!("Unsupported engine: {other}"),
    }
}

/// Safety gate layer 1: parse and validate per the engine's dialect, returning the normalized statement text.
pub fn guard_sql_for(engine: &str, sql: &str) -> anyhow::Result<String> {
    let cleaned = sql.trim().trim_end_matches(';').trim();
    if cleaned.is_empty() {
        anyhow::bail!("Empty SQL");
    }
    let parsed = match engine {
        "databricks" => Parser::parse_sql(&DatabricksDialect {}, cleaned),
        "snowflake" => Parser::parse_sql(&SnowflakeDialect {}, cleaned),
        "clickhouse" => Parser::parse_sql(&ClickHouseDialect {}, cleaned),
        "trino" => Parser::parse_sql(&GenericDialect {}, cleaned),
        "mysql" => Parser::parse_sql(&MySqlDialect {}, cleaned),
        _ => Parser::parse_sql(&PostgreSqlDialect {}, cleaned),
    };
    let statements = parsed.map_err(|e| anyhow::anyhow!("SQL parse error: {e}"))?;
    if statements.len() != 1 {
        anyhow::bail!("Exactly one statement is allowed");
    }
    match &statements[0] {
        Statement::Query(_) => Ok(cleaned.to_string()),
        other => anyhow::bail!(
            "Read-only: only SELECT/WITH queries are allowed (got {})",
            statement_kind(other)
        ),
    }
}

fn statement_kind(s: &Statement) -> &'static str {
    match s {
        Statement::Insert { .. } => "INSERT",
        Statement::Update { .. } => "UPDATE",
        Statement::Delete { .. } => "DELETE",
        Statement::CreateTable { .. } => "CREATE TABLE",
        Statement::Drop { .. } => "DROP",
        Statement::AlterTable { .. } => "ALTER TABLE",
        Statement::Truncate { .. } => "TRUNCATE",
        _ => "a non-SELECT statement",
    }
}

/// Layer 2: wrap in an outer LIMIT. HTTP-family engines all accept this form; PG has its own row_to_json version
pub(crate) fn wrap_limit(sql: &str) -> String {
    format!("SELECT * FROM ( {sql} ) AS _q LIMIT {}", ROW_CAP + 1)
}

/// Row 201 is used only to detect truncation and is not handed to the model
pub(crate) fn truncate_rows<T>(mut rows: Vec<T>) -> (Vec<T>, bool) {
    let truncated = rows.len() > ROW_CAP;
    rows.truncate(ROW_CAP);
    (rows, truncated)
}

/// Shared by the HTTP families: column names plus row values are assembled into JSON Lines. Assembled by hand rather than `serde_json::Map`,
/// the latter sorts keys unless `preserve_order` is on, while column order is the order the query wrote it, which the model relies on to read tables
pub(crate) fn rows_to_json_lines(
    columns: &[String],
    rows: &[Vec<serde_json::Value>],
) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let mut line = String::from("{");
            for (i, col) in columns.iter().enumerate() {
                if i > 0 {
                    line.push(',');
                }
                line.push_str(&serde_json::to_string(col).unwrap_or_else(|_| "\"?\"".into()));
                line.push(':');
                let value = row.get(i).cloned().unwrap_or(serde_json::Value::Null);
                line.push_str(&value.to_string());
            }
            line.push('}');
            line
        })
        .collect()
}

/// Databricks' JSON_ARRAY and Snowflake's data give every value as a string (or null),
/// and ClickHouse's JSONCompactStrings does too (it hands only floats and booleans to this path;
/// see `clickhouse::cell`).
/// Restore numbers and booleans by column type; leave the rest as strings -- the model's arithmetic on `"42"` and `42` differs
pub(crate) fn coerce(type_name: &str, raw: &serde_json::Value) -> serde_json::Value {
    let serde_json::Value::String(s) = raw else {
        return raw.clone();
    };
    let ty = type_name.to_ascii_uppercase();
    const NUMERIC: &[&str] = &[
        "INT", "LONG", "SHORT", "BYTE", "FLOAT", "DOUBLE", "DECIMAL", "NUMBER", "FIXED", "REAL",
        "NUMERIC",
    ];
    // INTERVAL also contains "INT": if it does not parse as a number it is left as is, so nothing is harmed
    if NUMERIC.iter().any(|k| ty.contains(k)) {
        if let Ok(n) = s.parse::<i64>() {
            return n.into();
        }
        if let Some(f) = s.parse::<f64>().ok().filter(|_| fits_f64(&ty, s)) {
            if let Some(n) = serde_json::Number::from_f64(f) {
                return serde_json::Value::Number(n);
            }
        }
    }
    if ty.starts_with("BOOL") {
        match s.as_str() {
            "true" | "TRUE" => return true.into(),
            "false" | "FALSE" => return false.into(),
            _ => {}
        }
    }
    raw.clone()
}

/// Fixed-point numbers (DECIMAL / NUMERIC / NUMBER / FIXED) with more than 15 significant digits do not fit in f64;
/// converting them to a number would change the value: `12345678901234567.89` would become `12345678901234568`. So they are kept as strings
fn fits_f64(ty: &str, s: &str) -> bool {
    if !["DECIMAL", "NUMERIC", "NUMBER", "FIXED"]
        .iter()
        .any(|k| ty.contains(k))
    {
        return true;
    }
    let mantissa = s.split(['e', 'E']).next().unwrap_or_default();
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    digits.trim_start_matches('0').trim_end_matches('0').len() <= 15
}

/// Escaping of single-quoted literals: schema names go into the WHERE clause of information_schema
pub(crate) fn sql_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The client shared by the HTTP families.
///
/// **The proxy policy is explicit**: loopback addresses and hosts in `NO_PROXY` connect directly; the rest go through `HTTPS_PROXY` /
/// `HTTP_PROXY` / `ALL_PROXY`. Does not use reqwest's system-proxy detection -- on Windows it reads the registry,
/// and the registry's bypass syntax such as `127.*` is not fully understood by it, so a local stand-in service would be sent through a proxy and come back as 502.
/// The service process should read environment variables; this rule matches how docker-compose is written
pub(crate) fn http() -> anyhow::Result<reqwest::Client> {
    Ok(http_builder().build()?)
}

/// The text-reading service has its own timeout and redirect policy; the proxy rules are still shared with the other HTTP requests.
pub(crate) fn http_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(HTTP_REQUEST_TIMEOUT)
        .user_agent("utopia")
        .proxy(reqwest::Proxy::custom(|url: &reqwest::Url| proxy_for(url)))
}

fn proxy_for(url: &reqwest::Url) -> Option<reqwest::Url> {
    let host = url.host_str()?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(|c| c == '[' || c == ']')
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    if loopback || no_proxy_matches(host) {
        return None;
    }
    let keys: &[&str] = if url.scheme() == "https" {
        &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    };
    keys.iter()
        .find_map(|k| std::env::var(k).ok())
        .filter(|v| !v.trim().is_empty())
        .and_then(|v| reqwest::Url::parse(v.trim()).ok())
}

/// The common form of `NO_PROXY=localhost,127.0.0.1,.internal,corp.example`: whole-name equality,
/// or a suffix match with a leading dot
fn no_proxy_matches(host: &str) -> bool {
    let raw = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    raw.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "*")
        .any(|p| {
            let p = p.trim_start_matches('.');
            host.eq_ignore_ascii_case(p)
                || host
                    .to_ascii_lowercase()
                    .ends_with(&format!(".{}", p.to_ascii_lowercase()))
        })
        || raw.split(',').any(|p| p.trim() == "*")
}

#[cfg(test)]
mod tests {
    use super::{coerce, engine_from_conn, guard_sql_for, rows_to_json_lines};
    use serde_json::json;

    fn guard_sql(sql: &str) -> anyhow::Result<String> {
        guard_sql_for("postgres", sql)
    }

    #[test]
    fn allows_select_and_cte() {
        assert!(guard_sql("SELECT region, sum(amount) FROM orders GROUP BY 1").is_ok());
        assert!(guard_sql("WITH t AS (SELECT 1 AS x) SELECT * FROM t;").is_ok());
    }

    #[test]
    fn rejects_writes_and_ddl() {
        for bad in [
            "UPDATE orders SET amount = 0",
            "DELETE FROM orders",
            "INSERT INTO orders (region) VALUES ('east')",
            "DROP TABLE orders",
            "TRUNCATE orders",
            "CREATE TABLE t (id int)",
            "ALTER TABLE orders ADD COLUMN x int",
        ] {
            assert!(guard_sql(bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn rejects_multi_statement() {
        assert!(guard_sql("SELECT 1; DROP TABLE orders").is_err());
        assert!(guard_sql("").is_err());
    }

    #[test]
    fn every_dialect_keeps_the_same_gate() {
        for engine in [
            "postgres",
            "mysql",
            "trino",
            "databricks",
            "snowflake",
            "clickhouse",
        ] {
            assert!(
                guard_sql_for(engine, "SELECT a FROM t WHERE b > 1").is_ok(),
                "{engine}"
            );
            assert!(guard_sql_for(engine, "DELETE FROM t").is_err(), "{engine}");
            assert!(
                guard_sql_for(engine, "SELECT 1; SELECT 2").is_err(),
                "{engine}"
            );
        }
        // Each engine's dialect quirks: backticks and double-colon casts must pass
        assert!(guard_sql_for("databricks", "SELECT `region` FROM main.sales.orders").is_ok());
        assert!(guard_sql_for("snowflake", "SELECT amount::number FROM db.public.orders").is_ok());
        assert!(guard_sql_for("trino", "SELECT count(*) FROM hive.default.orders").is_ok());
        // MySQL's backticks are incompatible with the PG dialect, so it uses its own dialect to get through
        assert!(guard_sql_for("mysql", "SELECT `region` FROM `sales`.`orders`").is_ok());
        // ClickHouse's function names and the FINAL modifier must both pass. The SETTINGS gate at the statement level is not this layer's job:
        // when readonly=1 is in effect the server refuses it; an account with readonly=2 does not refuse, and then the timeout is enforced by KILL
        // (see clickhouse.rs)
        assert!(guard_sql_for(
            "clickhouse",
            "SELECT toStartOfMonth(placed_on) AS m, count() FROM sales.orders FINAL GROUP BY m"
        )
        .is_ok());
    }

    #[test]
    fn engine_follows_the_scheme() {
        assert_eq!(engine_from_conn("postgres://u:p@h/db"), Some("postgres"));
        assert_eq!(engine_from_conn("postgresql://u:p@h/db"), Some("postgres"));
        assert_eq!(engine_from_conn("trino://u@h:8443/hive"), Some("trino"));
        assert_eq!(engine_from_conn("presto://u@h/hive"), Some("trino"));
        assert_eq!(
            engine_from_conn("databricks://:t@h/sql/1.0/warehouses/x"),
            Some("databricks")
        );
        assert_eq!(
            engine_from_conn("snowflake://:t@a.snowflakecomputing.com/db"),
            Some("snowflake")
        );
        assert_eq!(engine_from_conn("mysql://u:p@h:3306/db"), Some("mysql"));
        // Another way of writing the same protocol; inside the engine it is rewritten to mysql:// before being handed to the driver
        assert_eq!(engine_from_conn("mariadb://u@h/db"), Some("mysql"));
        assert_eq!(
            engine_from_conn("clickhouse://u:p@h:8123/sales"),
            Some("clickhouse")
        );
        assert_eq!(engine_from_conn("garbage"), None);
    }

    #[test]
    fn json_lines_keep_column_order() {
        let cols = vec!["zeta".to_string(), "alpha".to_string()];
        let rows = vec![vec![json!(1), json!("x")], vec![json!(null)]];
        assert_eq!(
            rows_to_json_lines(&cols, &rows),
            vec![r#"{"zeta":1,"alpha":"x"}"#, r#"{"zeta":null,"alpha":null}"#]
        );
    }

    #[test]
    fn strings_come_back_as_numbers_when_the_column_says_so() {
        assert_eq!(coerce("DOUBLE", &json!("12.5")), json!(12.5));
        assert_eq!(coerce("fixed", &json!("42")), json!(42));
        assert_eq!(coerce("BOOLEAN", &json!("true")), json!(true));
        assert_eq!(coerce("STRING", &json!("42")), json!("42"));
        assert_eq!(coerce("INTERVAL", &json!("1 day")), json!("1 day"));
        assert_eq!(coerce("DOUBLE", &json!(null)), json!(null));
    }

    #[test]
    fn an_exact_decimal_longer_than_a_double_stays_exact() {
        assert_eq!(
            coerce("DECIMAL(20,2)", &json!("12345678901234567.89")),
            json!("12345678901234567.89")
        );
        assert_eq!(
            coerce("fixed", &json!("12345678901234567890")),
            json!("12345678901234567890")
        );
        assert_eq!(coerce("DECIMAL", &json!("1234.50")), json!(1234.5));
        assert_eq!(
            coerce("fixed", &json!("100000000000000000000")),
            json!(1e20)
        );
        assert_eq!(
            coerce("DOUBLE", &json!("0.30000000000000004")),
            json!(0.30000000000000004)
        );
    }
}
