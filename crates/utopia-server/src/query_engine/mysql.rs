//! MySQL wire protocol family. **One protocol also covers a whole group**: TiDB, OceanBase, Doris, StarRocks
//! all speak it, and so does MariaDB, so this file gives the best value of the four engines.
//!
//! Two differences from `postgres.rs`, both coming from MySQL itself:
//!
//! - **No `row_to_json`.** On the PG side the library turns the whole row into JSON text, and column order is kept naturally;
//!   here values must be read column by column and assembled by hand (as `rows_to_json_lines` does in the HTTP family), so an extra
//!   type-mapping table is needed. That table's criterion is the driver's `ColumnType::name` and each type's
//!   `compatible`, not the MySQL manual: two mismatches silently turn a whole column into null:
//!   unsigned integers' type names carry an ` UNSIGNED` suffix, and `DECIMAL` is blocked from both `f64`
//!   and `String`, so only `BigDecimal` can read it (see `Cell`).
//! - **Timeouts are written two ways.** MySQL uses `max_execution_time` (milliseconds), and MariaDB uses
//!   `max_statement_time` (seconds). Try both, and error only if neither is recognized: this layer guards against a full table scan
//!   dragging the database down, and an outer LIMIT cannot stop that (it scans everything before truncating), so it must not degrade silently.

use super::{
    coerce, rows_to_json_lines, truncate_rows, wrap_limit, QueryEngine, QueryResult, SchemaColumn,
    STATEMENT_TIMEOUT_SECS,
};
use sqlx::mysql::MySqlPoolOptions;
use sqlx::{Column, Row, TypeInfo};
use std::time::Duration;

pub struct MysqlEngine {
    conn: String,
}

impl MysqlEngine {
    pub fn new(conn: &str) -> Self {
        // sqlx only accepts mysql://, while mariadb:// is another way to write the same protocol
        let conn = match conn.strip_prefix("mariadb://") {
            Some(rest) => format!("mysql://{rest}"),
            None => conn.to_string(),
        };
        Self { conn }
    }

    async fn pool(&self) -> anyhow::Result<sqlx::MySqlPool> {
        Ok(MySqlPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect(&self.conn)
            .await?)
    }
}

/// Column type → which tier to read it with.
///
/// **Dispatch and reading are separate, because only the former can be tested**: building a `MySqlRow` requires actually connecting to
/// a server, and this table is exactly the most error-prone part of this file.
#[derive(Debug, PartialEq, Eq)]
enum Cell {
    Bool,
    /// Signed integers. The driver's `i64` explicitly excludes UNSIGNED, so unsigned types take their own tier
    Int,
    /// `BIGINT UNSIGNED` and the like. The driver's type name **carries the UNSIGNED suffix**,
    /// so if it is not recognized it falls into the fallback tier, which cannot read integers: the whole column becomes null
    UnsignedInt,
    Float,
    /// `DECIMAL`. The driver blocks it from both `f64` ("floating-point numbers have
    /// different semantics") and `String` (not in the string-compatible table),
    /// so it can only be read with `BigDecimal`. Almost every money column is this type, so falling to the wrong tier costs the most
    Decimal,
    Json,
    Date,
    Time,
    DateTime,
    Timestamp,
    /// Fallback: string first, then bytes. Integers do not go here: once `42` becomes `"42"`, the model
    /// does arithmetic on it differently
    Text,
}

/// Type name → tier. The names come from the driver's `ColumnType::name`, and that table is the only basis for this function:
/// `TINYINT(1)` reports `BOOLEAN`, both `DECIMAL` and `NEWDECIMAL` report `DECIMAL`,
/// and unsigned integers carry the ` UNSIGNED` suffix.
fn cell_kind(type_name: &str) -> Cell {
    let ty = type_name.to_ascii_uppercase();
    // Check the suffix first: `INT UNSIGNED` falling into the `INT` tier would be read as i64, which the driver rejects outright
    if let Some(base) = ty.strip_suffix(" UNSIGNED") {
        return match base {
            "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => Cell::UnsignedInt,
            "FLOAT" | "DOUBLE" => Cell::Float,
            "DECIMAL" => Cell::Decimal,
            _ => Cell::Text,
        };
    }
    match ty.as_str() {
        "BOOLEAN" | "BOOL" => Cell::Bool,
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => Cell::Int,
        // The name has no UNSIGNED, but the driver accepts only u64: neither i64 nor String can read it, so the whole column becomes null
        "YEAR" | "BIT" => Cell::UnsignedInt,
        "FLOAT" | "DOUBLE" => Cell::Float,
        "DECIMAL" => Cell::Decimal,
        "JSON" => Cell::Json,
        "DATE" => Cell::Date,
        "TIME" => Cell::Time,
        "DATETIME" => Cell::DateTime,
        "TIMESTAMP" => Cell::Timestamp,
        _ => Cell::Text,
    }
}

/// One cell's value → JSON. If it cannot be read, fall back to a string; only if that fails too, null.
fn cell_to_json(row: &sqlx::mysql::MySqlRow, i: usize, type_name: &str) -> serde_json::Value {
    use serde_json::Value;
    // Every branch takes `Option<T>`, so NULL becomes JSON null within each tier;
    // there is no uniform pre-check: a binary column read as `Option<String>` errors rather than giving None,
    // and such a pre-check would judge a BLOB that has a value as empty
    let ty = type_name.to_ascii_uppercase();
    match cell_kind(&ty) {
        Cell::Bool => row
            .try_get::<Option<bool>, _>(i)
            .map(|v| v.map_or(Value::Null, Value::Bool))
            .unwrap_or(Value::Null),
        Cell::Int => row
            .try_get::<Option<i64>, _>(i)
            .map(|v| v.map_or(Value::Null, |n| n.into()))
            .unwrap_or(Value::Null),
        Cell::UnsignedInt => row
            .try_get::<Option<u64>, _>(i)
            .map(|v| v.map_or(Value::Null, |n| n.into()))
            .unwrap_or(Value::Null),
        // BigDecimal → text → coerce: the same route as Databricks / Snowflake DECIMAL.
        // Numbers go through f64 in coerce; values beyond f64 precision are kept as strings rather than becoming an approximate number
        Cell::Decimal => row
            .try_get::<Option<sqlx::types::BigDecimal>, _>(i)
            .ok()
            .flatten()
            .map_or(Value::Null, |d| {
                coerce("DECIMAL", &Value::String(d.to_string()))
            }),
        Cell::Float => row
            .try_get::<Option<f64>, _>(i)
            .ok()
            .flatten()
            .and_then(serde_json::Number::from_f64)
            .map_or(Value::Null, Value::Number),
        Cell::Json => row
            .try_get::<Option<serde_json::Value>, _>(i)
            .ok()
            .flatten()
            .unwrap_or(Value::Null),
        Cell::Date => try_text(row, i, |r| {
            r.try_get::<Option<chrono::NaiveDate>, _>(i)
                .map(|v| v.map(|d| d.to_string()))
        }),
        // TIME is a duration (-838:59:59 to 838:59:59); NaiveTime cannot hold more than 24 hours and also drops the sign
        Cell::Time => try_text(row, i, |r| {
            r.try_get::<Option<sqlx::mysql::types::MySqlTime>, _>(i)
                .map(|v| v.map(time_text))
        }),
        Cell::DateTime => try_text(row, i, |r| {
            r.try_get::<Option<chrono::NaiveDateTime>, _>(i)
                .map(|v| v.map(|t| t.to_string()))
        }),
        Cell::Timestamp => try_text(row, i, |r| {
            r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(i)
                .map(|v| v.map(|t| t.to_rfc3339()))
        }),
        Cell::Text => {
            // CHAR / VARCHAR / TEXT / ENUM / SET come here. Still run through coerce:
            // servers sometimes declare a numeric column as a string, which is not rare
            let raw = row
                .try_get::<Option<String>, _>(i)
                .ok()
                .flatten()
                .map(Value::String);
            match raw {
                Some(v) => coerce(&ty, &v),
                // Binary columns (BLOB / VARBINARY / GEOMETRY) are not valid UTF-8,
                // so give a length rather than stuffing a pile of escaped bytes into the model's context
                None => match row.try_get::<Option<Vec<u8>>, _>(i) {
                    Ok(Some(bytes)) => Value::String(format!("<{} bytes>", bytes.len())),
                    _ => Value::Null,
                },
            }
        }
    }
}

/// Follow MySQL's own notation: `-01:30:00`, `30:15:00`; write decimals only when there are some
fn time_text(t: sqlx::mysql::types::MySqlTime) -> String {
    // sqlx 0.8's `MySqlTime::is_negative` returns is_positive, so check the sign itself
    let sign = if t.sign().is_negative() { "-" } else { "" };
    let mut s = format!(
        "{sign}{:02}:{:02}:{:02}",
        t.hours(),
        t.minutes(),
        t.seconds()
    );
    // Fractions follow chrono's NaiveTime: whole milliseconds use three digits (`.500`), otherwise six (`.000500`),
    // and TIME output within one day is the same as before
    match t.microseconds() {
        0 => {}
        us if us % 1000 == 0 => s.push_str(&format!(".{:03}", us / 1000)),
        us => s.push_str(&format!(".{us:06}")),
    }
    s
}

/// Reads text by type, falling back to a generic string if that fails (this is reached when the server configures time columns as strings).
fn try_text<F>(row: &sqlx::mysql::MySqlRow, i: usize, f: F) -> serde_json::Value
where
    F: Fn(&sqlx::mysql::MySqlRow) -> Result<Option<String>, sqlx::Error>,
{
    match f(row) {
        Ok(Some(s)) => serde_json::Value::String(s),
        Ok(None) => serde_json::Value::Null,
        Err(_) => row
            .try_get::<Option<String>, _>(i)
            .ok()
            .flatten()
            .map_or(serde_json::Value::Null, serde_json::Value::String),
    }
}

#[async_trait::async_trait]
impl QueryEngine for MysqlEngine {
    async fn test(&self) -> anyhow::Result<()> {
        let pool = self.pool().await?;
        sqlx::query("SELECT 1").execute(&pool).await?;
        pool.close().await;
        Ok(())
    }

    async fn fetch_schema(&self) -> anyhow::Result<Vec<SchemaColumn>> {
        let pool = self.pool().await?;
        // In MySQL the schema is the database. System databases are excluded by name: information_schema
        // here is **the thing to exclude**, and it is not the same concept as the namesake on the PG side.
        //
        // **Every column needs CAST(... AS CHAR).** From MySQL 8.0 on, information_schema is a view built on
        // the data dictionary, and these columns are reported as VARBINARY over the binary protocol, while sqlx's strict type decoding
        // refuses to read them into a String (`VARCHAR is not compatible with VARBINARY`).
        // The raw SQL looks fine on the command line; the command line does no strict type decoding, so this shows up only against a real
        // server. On MariaDB CAST is harmless, so one statement works on both.
        let cols: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT CAST(table_schema AS CHAR), CAST(table_name AS CHAR),
                    CAST(column_name AS CHAR), CAST(column_type AS CHAR),
                    CAST(column_comment AS CHAR)
             FROM information_schema.columns
             WHERE table_schema NOT IN
                   ('information_schema', 'mysql', 'performance_schema', 'sys')
             ORDER BY table_schema, table_name, ordinal_position",
        )
        .fetch_all(&pool)
        .await?;
        // Keys are a nice-to-have: if they cannot be read, only columns are given as before, so the whole schema fetch does not fail (#502).
        // Visibility: `statistics` and `key_column_usage` still have rows for a user with only SELECT
        // (measured on MariaDB 11.4); `table_constraints` / `referential_constraints` are empty for such
        // a user, so keys are read from the first two views; do not switch to the latter two
        let keys = match keys(&pool).await {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(error = %e, "could not read MySQL primary/foreign keys, schema has no key markers");
                Keys::default()
            }
        };
        pool.close().await;

        Ok(cols
            .into_iter()
            .map(|(schema, table, column, data_type, comment)| {
                let key = (schema.clone(), table.clone(), column.clone());
                let is_primary_key = keys.primary.contains(&key);
                let references_table = keys.foreign.get(&key).cloned();
                SchemaColumn {
                    schema,
                    table,
                    column,
                    data_type,
                    // Without a comment this column is an empty string rather than NULL, so copying it would give every table an empty comment
                    comment: comment.filter(|c| !c.trim().is_empty()),
                    is_primary_key,
                    references_table,
                }
            })
            .collect())
    }

    async fn execute(&self, sql: &str) -> anyhow::Result<QueryResult> {
        let pool = self.pool().await?;
        // Defence in depth, layer 3: session read-only + statement timeout. Read-only takes effect per statement under autocommit,
        // so even if the parser slips through, nothing can be written
        sqlx::query("SET SESSION TRANSACTION READ ONLY")
            .execute(&pool)
            .await?;
        let millis = STATEMENT_TIMEOUT_SECS * 1000;
        let mysql_form = sqlx::query(&format!("SET SESSION max_execution_time = {millis}"))
            .execute(&pool)
            .await;
        if mysql_form.is_err() {
            // MariaDB: seconds, and a float
            sqlx::query(&format!(
                "SET SESSION max_statement_time = {STATEMENT_TIMEOUT_SECS}"
            ))
            .execute(&pool)
            .await
            .map_err(|e| {
                anyhow::anyhow!("Could not set a statement timeout on this server: {e}")
            })?;
        }

        let fetched = sqlx::query(&wrap_limit(sql)).fetch_all(&pool).await?;
        pool.close().await;

        let (fetched, truncated) = truncate_rows(fetched);
        let Some(first) = fetched.first() else {
            return Ok(QueryResult {
                rows: Vec::new(),
                truncated,
            });
        };
        // Column names and types are fetched once: the columns of every row in the same result set are identical
        let columns: Vec<String> = first
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect();
        let types: Vec<String> = first
            .columns()
            .iter()
            .map(|c| c.type_info().name().to_string())
            .collect();
        let values: Vec<Vec<serde_json::Value>> = fetched
            .iter()
            .map(|row| {
                (0..columns.len())
                    .map(|i| cell_to_json(row, i, &types[i]))
                    .collect()
            })
            .collect();
        Ok(QueryResult {
            rows: rows_to_json_lines(&columns, &values),
            truncated,
        })
    }
}

type ColumnKey = (String, String, String);

/// The return shape of `keys()`'s UNION ALL: the first three columns are (schema, table, column); the last two are
/// filled only for FK rows (PK rows hold NULL placeholders)
type KeyRow = (String, String, String, Option<String>, Option<String>);

/// Single-column primary keys and single-column foreign keys in one database, looked up by (schema, table, column)
#[derive(Default)]
struct Keys {
    primary: std::collections::HashSet<ColumnKey>,
    /// Foreign key column → the `schema.table` it points to
    foreign: std::collections::HashMap<ColumnKey, String>,
}

/// Reads keys from information_schema, with two UNION ALL branches:
///
/// - single-column primary keys: `index_name = 'PRIMARY'` in `statistics`, grouped by table, only those with one column;
/// - single-column foreign keys: `referenced_table_name` non-empty in `key_column_usage`, grouped by constraint, only those
///   with one column.
///
/// Members of composite primary keys and composite foreign keys are not marked: the exploration prompt treats PK as ID and FK as the join path,
/// and marking one column of a composite key on its own would be misleading.
///
/// **Each view is scanned once, counting columns with GROUP BY, not with per-row correlated subqueries.** MariaDB (and
/// MySQL 5.7) rebuilds this table on every read of information_schema: a per-row subquery takes 19 seconds at 1,000 tables and
/// 4 minutes at 2,000, and schema fetch runs inside a mounting request with no statement timeout, so slow means hung.
/// The grouped form takes 0.6 seconds at 2,000 tables.
///
/// Foreign key constraint names are unique within a database (InnoDB), so grouping by (database, table, constraint name) keeps them apart. A column that sits in
/// two single-column foreign keys at once (rare): sort on all five columns, first come first served, so the result does not depend on the server's return order.
///
/// Column names are reported as VARBINARY over the binary protocol (MySQL 8.0+), so as in fetch_schema's column query
/// they must be CAST(... AS CHAR) before sqlx can read them as String
async fn keys(pool: &sqlx::MySqlPool) -> anyhow::Result<Keys> {
    let rows: Vec<KeyRow> = sqlx::query_as(
        "SELECT CAST(table_schema AS CHAR), CAST(table_name AS CHAR),
                CAST(MIN(column_name) AS CHAR),
                NULL AS ref_schema, NULL AS ref_table
           FROM information_schema.statistics
          WHERE index_name = 'PRIMARY'
            AND table_schema NOT IN
                ('information_schema', 'mysql', 'performance_schema', 'sys')
          GROUP BY table_schema, table_name
         HAVING COUNT(*) = 1
         UNION ALL
         SELECT CAST(table_schema AS CHAR), CAST(table_name AS CHAR),
                CAST(MIN(column_name) AS CHAR),
                CAST(MIN(referenced_table_schema) AS CHAR),
                CAST(MIN(referenced_table_name) AS CHAR)
           FROM information_schema.key_column_usage
          WHERE referenced_table_name IS NOT NULL
            AND table_schema NOT IN
                ('information_schema', 'mysql', 'performance_schema', 'sys')
          GROUP BY table_schema, table_name, constraint_name
         HAVING COUNT(*) = 1
         ORDER BY 1, 2, 3, 4, 5",
    )
    .fetch_all(pool)
    .await?;
    let mut keys = Keys::default();
    for (schema, table, column, ref_schema, ref_table) in rows {
        let key = (schema, table, column);
        match (ref_schema, ref_table) {
            (None, _) => {
                // The first branch of the UNION is the PK row
                keys.primary.insert(key);
            }
            (Some(rs), Some(rt)) => {
                keys.foreign.entry(key).or_insert(format!("{rs}.{rt}"));
            }
            _ => {}
        }
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::{cell_kind, time_text, Cell, MysqlEngine, QueryEngine, SchemaColumn};
    use sqlx::mysql::MySqlPoolOptions;
    use uuid::Uuid;

    /// The tier that runs against a real server. Skipped without `UTOPIA_TEST_MYSQL_URL`:
    /// these three things (information_schema column names, the two timeout forms, value round-trips) are
    /// the only parts beyond the type table that cannot be tested, and they only have answers on a real server.
    ///
    /// Start one to run it:
    /// `docker run -d -e MARIADB_ROOT_PASSWORD=pw -p 13306:3306 mariadb:11.4`
    /// then `UTOPIA_TEST_MYSQL_URL=mysql://root:pw@127.0.0.1:13306/sales`.
    /// The table DDL is in #316.
    fn live_url() -> Option<String> {
        std::env::var("UTOPIA_TEST_MYSQL_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
    }

    #[tokio::test]
    async fn a_live_server_answers_with_typed_values() {
        let Some(url) = live_url() else {
            return;
        };
        let engine = MysqlEngine::new(&url);
        engine.test().await.expect("SELECT 1");

        // information_schema's column names differ from PG's (column_type / column_comment),
        // and getting them wrong raises no error; it just halves the schema document
        let schema = engine.fetch_schema().await.expect("schema");
        let amount = schema
            .iter()
            .find(|c| c.table == "orders" && c.column == "amount")
            .expect("orders.amount");
        assert!(
            amount.data_type.starts_with("decimal"),
            "column_type 要给出带精度的形态，拿到的是 {}",
            amount.data_type
        );
        assert_eq!(
            amount.comment.as_deref(),
            Some("Order total in CNY"),
            "注释要跟着列走"
        );
        assert!(
            schema.iter().all(|c| c.comment.as_deref() != Some("")),
            "空注释是空串不是 NULL，要过滤掉"
        );

        // Value round-trip: every kind of tier loss shows up here, whether numbers become strings or a whole column becomes null
        let r = engine
            .execute(
                "SELECT id, region, amount, qty, flag, placed_on FROM sales.orders ORDER BY id",
            )
            .await
            .expect("execute");
        assert_eq!(r.rows.len(), 3);
        let first: serde_json::Value = serde_json::from_str(&r.rows[0]).unwrap();
        assert_eq!(first["id"], serde_json::json!(1));
        assert_eq!(first["region"], serde_json::json!("east"));
        // DECIMAL: without the BigDecimal cell, it is null
        assert_eq!(first["amount"], serde_json::json!(1234.56));
        // BIGINT UNSIGNED: i64 cannot hold it, and the driver refuses to read it as i64
        assert_eq!(first["qty"], serde_json::json!(18446744073709551615u64));
        // The type name reported for TINYINT(1) is BOOLEAN
        assert_eq!(first["flag"], serde_json::json!(true));
        assert_eq!(first["placed_on"], serde_json::json!("2023-06-01"));

        // The row with all NULLs: every cell should be JSON null, not a zero value of some type
        let third: serde_json::Value = serde_json::from_str(&r.rows[2]).unwrap();
        for k in ["region", "amount", "qty", "flag", "placed_on"] {
            assert_eq!(third[k], serde_json::Value::Null, "{k} should be null");
        }

        // The write path is still blocked by the gate (layer 1); the read-only session is layer 3
        assert!(super::super::guard_sql_for("mysql", "DELETE FROM sales.orders").is_err());
    }

    /// DECIMAL beyond f64 precision is kept as a string, exact to the digit; those that fit are still numbers
    #[tokio::test]
    async fn a_decimal_longer_than_a_double_keeps_every_digit_mysql() {
        let Some(fx) = Fx::new(1).await else { return };
        let a = fx.schemas[0].clone();
        fx.exec(&format!(
            "CREATE TABLE {a}.ledger (total DECIMAL(20,2), fee DECIMAL(10,2));
             INSERT INTO {a}.ledger VALUES (12345678901234567.89, 1234.50);"
        ))
        .await;
        let r = MysqlEngine::new(&fx.url)
            .execute(&format!("SELECT total, fee FROM {a}.ledger"))
            .await
            .expect("execute");
        assert_eq!(
            r.rows,
            vec![r#"{"total":"12345678901234567.89","fee":1234.5}"#]
        );
        fx.cleanup().await;
    }

    /// Only unsigned integers can read YEAR and BIT: strings and bytes do not accept them, and falling into the fallback tier makes the whole column null
    #[tokio::test]
    async fn a_year_or_bit_column_reads_as_its_number_mysql() {
        let Some(fx) = Fx::new(1).await else { return };
        let a = fx.schemas[0].clone();
        fx.exec(&format!(
            "CREATE TABLE {a}.budget (fiscal YEAR, active BIT(1), mask BIT(8));
             INSERT INTO {a}.budget VALUES (2024, b'1', b'00000101');"
        ))
        .await;
        let r = MysqlEngine::new(&fx.url)
            .execute(&format!("SELECT fiscal, active, mask FROM {a}.budget"))
            .await
            .expect("execute");
        assert_eq!(r.rows, vec![r#"{"fiscal":2024,"active":1,"mask":5}"#]);
        fx.cleanup().await;
    }

    /// TIME is a duration: a TIMEDIFF past 24 hours or negative is still itself
    #[tokio::test]
    async fn a_time_past_a_day_or_below_zero_reads_as_itself_mysql() {
        let Some(url) = live_url() else { return };
        let r = MysqlEngine::new(&url)
            .execute(
                "SELECT TIMEDIFF('2024-01-02 06:15:00', '2024-01-01 00:00:00') AS late, \
                 TIMEDIFF('2024-01-01 00:00:00', '2024-01-01 01:30:00') AS early, \
                 CAST('08:05:00' AS TIME) AS start",
            )
            .await
            .expect("execute");
        assert_eq!(
            r.rows,
            vec![r#"{"late":"30:15:00","early":"-01:30:00","start":"08:05:00"}"#]
        );
    }

    /// Each test gets its own database, with a random suffix in the name: parallel runs do not collide, and existing databases on the server are not touched.
    /// Each is dropped with `DROP DATABASE` after the test. **This is MySQL's "schema", which differs from PG's namespace:
    /// in MySQL the schema is the database** (see the comment at the top of fetch_schema)
    struct Fx {
        url: String,
        pool: sqlx::MySqlPool,
        schemas: Vec<String>,
    }

    impl Fx {
        async fn new(schemas: usize) -> Option<Self> {
            let url = live_url()?;
            // The connection string is used as is: with or without a database name works, since CREATE DATABASE needs no current database
            let pool = MySqlPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .expect("connect");
            let suffix = Uuid::now_v7().simple().to_string();
            let schemas: Vec<String> = (0..schemas)
                .map(|i| format!("`mysql_keys_{i}_{}`", &suffix[suffix.len() - 12..]))
                .collect();
            for s in &schemas {
                sqlx::query(&format!("CREATE DATABASE {s}"))
                    .execute(&pool)
                    .await
                    .expect("create schema");
            }
            Some(Self { url, pool, schemas })
        }

        async fn exec(&self, sql: &str) {
            sqlx::raw_sql(sql).execute(&self.pool).await.expect(sql);
        }

        async fn columns(&self, url: &str) -> Vec<SchemaColumn> {
            let mut cols = MysqlEngine::new(url).fetch_schema().await.expect("schema");
            // Keep only the databases this test created. The names are backticked, so strip them first
            let our: std::collections::HashSet<String> = self
                .schemas
                .iter()
                .map(|s| s.trim_matches('`').to_string())
                .collect();
            cols.retain(|c| our.contains(&c.schema));
            cols
        }

        async fn cleanup(self) {
            // Cross-database foreign keys make DROP DATABASE order-sensitive: a table in database B referencing database A
            // would fail with an FK constraint if A is dropped first. Temporarily disable the session-level foreign key check, then delete in turn
            sqlx::query("SET FOREIGN_KEY_CHECKS = 0")
                .execute(&self.pool)
                .await
                .expect("disable fk checks");
            for s in &self.schemas {
                let unquoted = s.trim_matches('`');
                sqlx::query(&format!("DROP DATABASE IF EXISTS `{unquoted}`"))
                    .execute(&self.pool)
                    .await
                    .expect("drop schema");
            }
            sqlx::query("SET FOREIGN_KEY_CHECKS = 1")
                .execute(&self.pool)
                .await
                .expect("enable fk checks");
            self.pool.close().await;
        }
    }

    fn col<'a>(
        cols: &'a [SchemaColumn],
        schema: &str,
        table: &str,
        column: &str,
    ) -> &'a SchemaColumn {
        cols.iter()
            .find(|c| c.schema == schema && c.table == table && c.column == column)
            .unwrap_or_else(|| panic!("{schema}.{table}.{column}"))
    }

    /// Single-column primary keys are marked; members of composite primary keys and composite foreign keys are not
    #[tokio::test]
    async fn a_single_column_key_is_marked_and_a_composite_one_is_not_mysql() {
        let Some(fx) = Fx::new(1).await else { return };
        let a = fx.schemas[0].clone();
        fx.exec(&format!(
            "CREATE TABLE {a}.parent (id INT PRIMARY KEY AUTO_INCREMENT, name VARCHAR(100) NOT NULL, note TEXT);
             CREATE TABLE {a}.line (
                 order_id INT NOT NULL,
                 ordinal INT NOT NULL,
                 payload TEXT,
                 PRIMARY KEY (order_id, ordinal)
             );
             CREATE TABLE {a}.line_note (
                 order_id INT,
                 ordinal INT,
                 FOREIGN KEY (order_id, ordinal) REFERENCES {a}.line (order_id, ordinal)
             );"
        ))
        .await;

        let cols = fx.columns(&fx.url).await;
        let id = col(&cols, a.trim_matches('`'), "parent", "id");
        assert!(id.is_primary_key && id.references_table.is_none());
        let name = col(&cols, a.trim_matches('`'), "parent", "name");
        assert!(!name.is_primary_key);
        let note = col(&cols, a.trim_matches('`'), "parent", "note");
        assert!(!note.is_primary_key && note.references_table.is_none());
        for c in ["order_id", "ordinal"] {
            let member = col(&cols, a.trim_matches('`'), "line", c);
            assert!(
                !member.is_primary_key,
                "composite primary key member line.{c} is not marked"
            );
            let fk_member = col(&cols, a.trim_matches('`'), "line_note", c);
            assert!(
                fk_member.references_table.is_none(),
                "组合外键的成员 line_note.{c} 不标"
            );
        }
        fx.cleanup().await;
    }

    /// The foreign key carries the table it points to: self-referencing or cross-database; when a column sits in two single-column foreign keys, the one that sorts first is taken
    #[tokio::test]
    async fn a_foreign_key_points_at_its_own_target_mysql() {
        let Some(fx) = Fx::new(2).await else { return };
        let (a, b) = (fx.schemas[0].clone(), fx.schemas[1].clone());
        let au = a.trim_matches('`');
        let bu = b.trim_matches('`');
        fx.exec(&format!(
            "CREATE TABLE {a}.p1 (id INT PRIMARY KEY AUTO_INCREMENT, parent INT, FOREIGN KEY (parent) REFERENCES {a}.p1 (id));
             CREATE TABLE {a}.p2 (id INT PRIMARY KEY AUTO_INCREMENT);
             CREATE TABLE {a}.c1 (x INT, FOREIGN KEY (x) REFERENCES {a}.p1 (id));
             CREATE TABLE {a}.c2 (y INT, FOREIGN KEY (y) REFERENCES {a}.p2 (id));
             CREATE TABLE {a}.c3 (z INT, FOREIGN KEY (z) REFERENCES {a}.p1 (id));
             CREATE TABLE {b}.orders (item_id INT NOT NULL, FOREIGN KEY (item_id) REFERENCES {a}.p2 (id));
             CREATE TABLE {a}.dual_fk (x INT,
                 FOREIGN KEY (x) REFERENCES {a}.p2 (id),
                 FOREIGN KEY (x) REFERENCES {a}.p1 (id));"
        ))
        .await;

        let cols = fx.columns(&fx.url).await;
        let target = |s: &str, t: &str, c: &str| col(&cols, s, t, c).references_table.clone();
        let p1 = format!("{au}.p1");
        let p2 = format!("{au}.p2");
        assert_eq!(
            target(au, "p1", "parent"),
            Some(p1.clone()),
            "self-reference"
        );
        assert_eq!(target(au, "c1", "x"), Some(p1.clone()));
        assert_eq!(
            target(au, "c2", "y"),
            Some(p2.clone()),
            "c2 points to p2, not p1"
        );
        assert_eq!(target(au, "c3", "z"), Some(p1.clone()));
        assert_eq!(
            target(bu, "orders", "item_id"),
            Some(p2.clone()),
            "cross-database"
        );
        assert_eq!(
            target(au, "dual_fk", "x"),
            Some(p1.clone()),
            "两条外键取排序在前的"
        );
        let p2_id = col(&cols, au, "p2", "id");
        assert!(
            p2_id.is_primary_key && p2_id.references_table.is_none(),
            "p2.id 是 PK，不是 FK"
        );
        fx.cleanup().await;
    }

    /// A connection with only SELECT permission still reads keys: BI connections are mostly this kind of user.
    /// If a user cannot be created (the test account has no CREATE USER / GRANT), skip
    #[tokio::test]
    async fn a_read_only_login_still_sees_the_keys_mysql() {
        let Some(fx) = Fx::new(1).await else { return };
        let a = fx.schemas[0].clone();
        let au = a.trim_matches('`');
        fx.exec(&format!(
            "CREATE TABLE {a}.p (id INT PRIMARY KEY AUTO_INCREMENT);
             CREATE TABLE {a}.c (p_id INT, FOREIGN KEY (p_id) REFERENCES {a}.p (id));"
        ))
        .await;
        let user = format!("ro_{}", &Uuid::now_v7().simple().to_string()[..16]);
        let password = &Uuid::now_v7().simple().to_string()[..16];
        if let Err(e) = sqlx::query(&format!(
            "CREATE USER '{user}'@'%' IDENTIFIED BY '{password}'"
        ))
        .execute(&fx.pool)
        .await
        {
            eprintln!("跳过：建不了只读角色（{e}）");
            fx.cleanup().await;
            return;
        }
        let grant = format!("GRANT SELECT ON {a}.* TO '{user}'@'%';");
        match sqlx::query(&grant).execute(&fx.pool).await {
            Ok(_) => {}
            Err(e) => {
                // The test user has no GRANT privilege (other than root): skip this tier
                eprintln!("跳过：GRANT 失败（{e}）");
                let _ = sqlx::query(&format!("DROP USER '{user}'@'%'"))
                    .execute(&fx.pool)
                    .await;
                fx.cleanup().await;
                return;
            }
        }

        // Switch to a read-only user and drop the database name: it has SELECT only on the test database, and connecting to the one
        // named in the connection string (such as `/sales`) gets rejected
        let ro_url = {
            let mut parsed = url::Url::parse(&fx.url).expect("parse mysql url");
            parsed.set_username(&user).expect("username");
            parsed.set_password(Some(password)).expect("password");
            parsed.set_path("/");
            parsed.to_string()
        };

        let cols = fx.columns(&ro_url).await;
        assert!(col(&cols, au, "p", "id").is_primary_key);
        assert_eq!(
            col(&cols, au, "c", "p_id").references_table,
            Some(format!("{au}.p"))
        );

        sqlx::query(&format!("DROP USER '{user}'@'%'"))
            .execute(&fx.pool)
            .await
            .expect("drop user");
        fx.cleanup().await;
    }

    /// CI has no MySQL, so the duration forms are checked here directly: over 24 hours, negative, with decimals
    #[test]
    fn a_time_is_written_as_the_duration_it_holds() {
        use sqlx::mysql::types::{MySqlTime, MySqlTimeSign};
        let t = |sign, h, m, s, us| time_text(MySqlTime::new(sign, h, m, s, us).unwrap());
        assert_eq!(t(MySqlTimeSign::Positive, 30, 15, 0, 0), "30:15:00");
        assert_eq!(t(MySqlTimeSign::Negative, 1, 30, 0, 0), "-01:30:00");
        assert_eq!(t(MySqlTimeSign::Positive, 8, 5, 0, 500_000), "08:05:00.500");
        assert_eq!(t(MySqlTimeSign::Positive, 8, 5, 0, 500), "08:05:00.000500");
    }

    #[test]
    fn mariadb_is_the_same_protocol_under_another_name() {
        // sqlx accepts only mysql://, while the UI allows mariadb:// — without rewriting, the driver rejects
        // a perfectly valid connection string as "unknown scheme"
        assert_eq!(
            MysqlEngine::new("mariadb://u:p@h:3306/db").conn,
            "mysql://u:p@h:3306/db"
        );
        assert_eq!(
            MysqlEngine::new("mysql://u:p@h:3306/db").conn,
            "mysql://u:p@h:3306/db"
        );
        // Strip only the prefix: the same characters appearing in the password must not be touched
        assert_eq!(
            MysqlEngine::new("mysql://u:mariadb://x@h/db").conn,
            "mysql://u:mariadb://x@h/db"
        );
    }

    /// Each of these three assertions corresponds to one way of "a whole column turns null". The names come from the driver's `ColumnType::name`:
    /// that table is the criterion, not a guess.
    #[test]
    fn every_number_shape_lands_in_a_readable_slot() {
        for t in ["TINYINT", "SMALLINT", "MEDIUMINT", "INT", "BIGINT", "int"] {
            assert_eq!(cell_kind(t), Cell::Int, "{t}");
        }
        // The driver's i64 explicitly excludes UNSIGNED, while the type name arrives with the suffix.
        // Without this tier, a BIGINT UNSIGNED column falls into the fallback, which cannot read integers
        for t in [
            "TINYINT UNSIGNED",
            "SMALLINT UNSIGNED",
            "MEDIUMINT UNSIGNED",
            "INT UNSIGNED",
            "BIGINT UNSIGNED",
        ] {
            assert_eq!(cell_kind(t), Cell::UnsignedInt, "{t}");
        }
        // YEAR and BIT type names carry no UNSIGNED, yet the driver accepts only unsigned integers to read them
        for t in ["YEAR", "BIT"] {
            assert_eq!(cell_kind(t), Cell::UnsignedInt, "{t}");
        }
        // DECIMAL is blocked by the driver from both f64 and String; only BigDecimal reads it.
        // Money columns are almost all this type, so falling to the wrong tier costs the most
        assert_eq!(cell_kind("DECIMAL"), Cell::Decimal);
        assert_eq!(cell_kind("DECIMAL UNSIGNED"), Cell::Decimal);
        assert_eq!(cell_kind("DOUBLE"), Cell::Float);
        assert_eq!(cell_kind("DOUBLE UNSIGNED"), Cell::Float);
        // TINYINT(1) reports BOOLEAN in this driver, not TINYINT
        assert_eq!(cell_kind("BOOLEAN"), Cell::Bool);
        assert_eq!(cell_kind("VARCHAR"), Cell::Text);
        assert_eq!(cell_kind("BLOB"), Cell::Text);
    }

    #[test]
    fn each_time_type_keeps_its_own_shape() {
        // Each of the four time kinds reads separately. Reading DATE as DATETIME errors and falls back to a string,
        // so you get not 2023-06-01 but the driver's raw form
        assert_eq!(cell_kind("DATE"), Cell::Date);
        assert_eq!(cell_kind("TIME"), Cell::Time);
        assert_eq!(cell_kind("DATETIME"), Cell::DateTime);
        assert_eq!(cell_kind("TIMESTAMP"), Cell::Timestamp);
    }
}
