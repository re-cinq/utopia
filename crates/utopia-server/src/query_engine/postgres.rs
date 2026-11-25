//! Postgres family (also covers Greenplum / Timescale and other PG-compatible systems). Direct wire-protocol connection,
//! the only one of the four engines that can set a read-only session.

use super::{QueryEngine, QueryResult, SchemaColumn, ROW_CAP, STATEMENT_TIMEOUT_SECS};
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use std::time::Duration;

pub struct PostgresEngine {
    conn: String,
}

impl PostgresEngine {
    pub fn new(conn: &str) -> Self {
        Self {
            conn: conn.to_string(),
        }
    }

    async fn pool(&self) -> anyhow::Result<sqlx::PgPool> {
        Ok(PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect(&self.conn)
            .await?)
    }
}

#[async_trait::async_trait]
impl QueryEngine for PostgresEngine {
    async fn test(&self) -> anyhow::Result<()> {
        let pool = self.pool().await?;
        sqlx::query("SELECT 1").execute(&pool).await?;
        pool.close().await;
        Ok(())
    }

    async fn fetch_schema(&self) -> anyhow::Result<Vec<SchemaColumn>> {
        let pool = self.pool().await?;
        let cols: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT c.table_schema, c.table_name, c.column_name,
                    c.data_type, pgd.description
             FROM information_schema.columns c
             LEFT JOIN pg_catalog.pg_statio_all_tables st
               ON st.schemaname = c.table_schema AND st.relname = c.table_name
             LEFT JOIN pg_catalog.pg_description pgd
               ON pgd.objoid = st.relid AND pgd.objsubid = c.ordinal_position
             WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema')
             ORDER BY c.table_schema, c.table_name, c.ordinal_position",
        )
        .fetch_all(&pool)
        .await?;
        // Keys are a nice-to-have: if they cannot be read, fall back to columns only as before, and do not fail the whole schema fetch (#502)
        let keys = match keys(&pool).await {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(error = %e, "could not read primary/foreign keys, schema has no key markers");
                Keys::default()
            }
        };
        pool.close().await;

        Ok(cols
            .into_iter()
            .map(|(schema, table, column, data_type, comment)| {
                let key = (schema, table, column);
                let is_primary_key = keys.primary.contains(&key);
                let references_table = keys.foreign.get(&key).cloned();
                let (schema, table, column) = key;
                SchemaColumn {
                    schema,
                    table,
                    column,
                    data_type,
                    comment,
                    is_primary_key,
                    references_table,
                }
            })
            .collect())
    }

    async fn execute(&self, sql: &str) -> anyhow::Result<QueryResult> {
        let pool = self.pool().await?;
        // Defense in depth layer 3: session-level read-only + timeout (even if the parser slips, nothing can be written or run the database to death)
        sqlx::query("SET default_transaction_read_only = on")
            .execute(&pool)
            .await?;
        sqlx::query(&format!(
            "SET statement_timeout = '{STATEMENT_TIMEOUT_SECS}s'"
        ))
        .execute(&pool)
        .await?;
        // Layer 2: outer LIMIT; row_to_json lets PG handle the type -> JSON conversion entirely (text key order preserves column order)
        let wrapped = format!(
            "SELECT row_to_json(_q)::text AS _j FROM ( {sql} ) AS _q LIMIT {}",
            ROW_CAP + 1
        );
        let fetched = sqlx::query(&wrapped).fetch_all(&pool).await?;
        pool.close().await;

        let truncated = fetched.len() > ROW_CAP;
        let rows = fetched
            .into_iter()
            .take(ROW_CAP)
            .map(|r| r.try_get::<String, _>("_j").unwrap_or_else(|_| "{}".into()))
            .collect();
        Ok(QueryResult { rows, truncated })
    }
}

type ColumnKey = (String, String, String);

/// Single-column primary keys and single-column foreign keys in a database, looked up by (schema, table, column)
#[derive(Default)]
struct Keys {
    primary: std::collections::HashSet<ColumnKey>,
    /// Foreign key column -> the `schema.table` it points to
    foreign: std::collections::HashMap<ColumnKey, String>,
}

/// Keys are read from `pg_constraint`, **not from `information_schema`**. Three reasons, each measured:
///
/// - `information_schema.table_constraints` / `key_column_usage` only show to a table's owner, or to roles with
///   privileges beyond SELECT. BI connections usually have only SELECT, so no key can be read, and no error is raised;
///   `pg_catalog` is visible to all roles.
/// - The constraint views in `information_schema` join by constraint name; with a thousand tables that takes seconds to minutes, whereas here
///   the join is by oid, at millisecond speed. Schema fetching runs on the mount request and must not hang.
/// - Constraint names are unique only within one table. Two tables each with a foreign key named `fk_ref` -- joining by schema + constraint name
///   would attach the target to another table; `conrelid` / `confrelid` are this constraint's own table.
///
/// Only **single-column** keys are recognized: members of composite primary keys and composite foreign keys are not marked. The exploration prompt uses a PK as "what a row is"
/// as the ID, and an FK as the relationship path; marking one column of a composite key on its own would be misleading. A column in two single-column foreign keys at once
/// (rare) takes the one that sorts first, so the result is stable
async fn keys(pool: &sqlx::PgPool) -> anyhow::Result<Keys> {
    let rows: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT n.nspname, c.relname, a.attname, k.contype::text,
                CASE WHEN k.contype = 'f' THEN rn.nspname || '.' || rc.relname END
           FROM pg_catalog.pg_constraint k
           JOIN pg_catalog.pg_class c ON c.oid = k.conrelid
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
           JOIN pg_catalog.pg_attribute a
             ON a.attrelid = k.conrelid AND a.attnum = k.conkey[1]
           LEFT JOIN pg_catalog.pg_class rc ON rc.oid = k.confrelid
           LEFT JOIN pg_catalog.pg_namespace rn ON rn.oid = rc.relnamespace
          WHERE k.contype IN ('p', 'f')
            AND cardinality(k.conkey) = 1
            AND n.nspname NOT IN ('pg_catalog', 'information_schema')
          ORDER BY 1, 2, 3, 4, 5",
    )
    .fetch_all(pool)
    .await?;
    let mut keys = Keys::default();
    for (schema, table, column, kind, target) in rows {
        let key = (schema, table, column);
        match (kind.as_str(), target) {
            ("p", _) => {
                keys.primary.insert(key);
            }
            ("f", Some(target)) => {
                keys.foreign.entry(key).or_insert(target);
            }
            _ => {}
        }
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::{PostgresEngine, QueryEngine, SchemaColumn};
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    /// Each test's own schema, with a random suffix in its name: parallel runs do not collide, and existing tables in the database are not touched.
    /// After the test, `DROP SCHEMA ... CASCADE`
    struct Fx {
        url: String,
        pool: sqlx::PgPool,
        schemas: Vec<String>,
    }

    impl Fx {
        async fn new(schemas: usize) -> Option<Self> {
            let url = utopia_store::test_db::url()?;
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .expect("connect");
            let suffix = Uuid::now_v7().simple().to_string();
            let schemas: Vec<String> = (0..schemas)
                .map(|i| format!("schema_keys_{i}_{}", &suffix[suffix.len() - 12..]))
                .collect();
            for s in &schemas {
                sqlx::query(&format!("CREATE SCHEMA {s}"))
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
            let mut cols = PostgresEngine::new(url)
                .fetch_schema()
                .await
                .expect("schema");
            cols.retain(|c| self.schemas.contains(&c.schema));
            cols
        }

        async fn cleanup(self) {
            for s in &self.schemas {
                sqlx::query(&format!("DROP SCHEMA IF EXISTS {s} CASCADE"))
                    .execute(&self.pool)
                    .await
                    .expect("drop schema");
            }
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
    async fn a_single_column_key_is_marked_and_a_composite_one_is_not() {
        let Some(fx) = Fx::new(1).await else { return };
        let a = fx.schemas[0].clone();
        fx.exec(&format!(
            "CREATE TABLE {a}.parent (id serial PRIMARY KEY, name text NOT NULL, note text);
             CREATE TABLE {a}.line (
                 order_id int NOT NULL,
                 ordinal int NOT NULL,
                 payload text,
                 PRIMARY KEY (order_id, ordinal)
             );
             CREATE TABLE {a}.line_note (
                 order_id int,
                 ordinal int,
                 FOREIGN KEY (order_id, ordinal) REFERENCES {a}.line (order_id, ordinal)
             );"
        ))
        .await;

        let cols = fx.columns(&fx.url).await;
        let id = col(&cols, &a, "parent", "id");
        assert!(id.is_primary_key && id.references_table.is_none());
        let name = col(&cols, &a, "parent", "name");
        assert!(!name.is_primary_key);
        let note = col(&cols, &a, "parent", "note");
        assert!(!note.is_primary_key && note.references_table.is_none());
        for c in ["order_id", "ordinal"] {
            let member = col(&cols, &a, "line", c);
            assert!(
                !member.is_primary_key,
                "composite primary key member line.{c} is not marked"
            );
            let fk_member = col(&cols, &a, "line_note", c);
            assert!(
                fk_member.references_table.is_none(),
                "组合外键的成员 line_note.{c} 不标"
            );
        }
        fx.cleanup().await;
    }

    /// A foreign key carries the table it points to: self-referencing, cross-schema, and a **same-named** constraint on two tables each point to their own.
    /// Constraint names are unique only within one table; joining by name would attach the target elsewhere
    #[tokio::test]
    async fn a_foreign_key_points_at_its_own_target() {
        let Some(fx) = Fx::new(2).await else { return };
        let (a, b) = (fx.schemas[0].clone(), fx.schemas[1].clone());
        fx.exec(&format!(
            "CREATE TABLE {a}.p1 (id int PRIMARY KEY, parent int REFERENCES {a}.p1 (id));
             CREATE TABLE {a}.p2 (id int PRIMARY KEY);
             CREATE TABLE {a}.c1 (x int CONSTRAINT fk_ref REFERENCES {a}.p1 (id));
             CREATE TABLE {a}.c2 (y int CONSTRAINT fk_ref REFERENCES {a}.p2 (id));
             -- 与 p2 的主键约束同名的外键
             CREATE TABLE {a}.c3 (z int CONSTRAINT p2_pkey REFERENCES {a}.p1 (id));
             CREATE TABLE {b}.orders (item_id int NOT NULL REFERENCES {a}.p2 (id));"
        ))
        .await;

        let cols = fx.columns(&fx.url).await;
        let target = |s: &str, t: &str, c: &str| col(&cols, s, t, c).references_table.clone();
        let p1 = format!("{a}.p1");
        let p2 = format!("{a}.p2");
        assert_eq!(
            target(&a, "p1", "parent"),
            Some(p1.clone()),
            "self-reference"
        );
        assert_eq!(target(&a, "c1", "x"), Some(p1.clone()));
        assert_eq!(
            target(&a, "c2", "y"),
            Some(p2.clone()),
            "constraints with the same name do not leak across tables"
        );
        assert_eq!(target(&a, "c3", "z"), Some(p1.clone()));
        assert_eq!(
            target(&b, "orders", "item_id"),
            Some(p2.clone()),
            "跨 schema"
        );
        let p2_id = col(&cols, &a, "p2", "id");
        assert!(
            p2_id.is_primary_key && p2_id.references_table.is_none(),
            "同名的外键不影响 p2 的主键"
        );
        fx.cleanup().await;
    }

    /// A connection with only SELECT privilege still reads keys. The constraint views in `information_schema` are empty for such a role --
    /// most BI connections are exactly that kind of role. If a role cannot be created (the database user lacks CREATEROLE), skip it
    #[tokio::test]
    async fn a_read_only_login_still_sees_the_keys() {
        let Some(fx) = Fx::new(1).await else { return };
        let a = fx.schemas[0].clone();
        fx.exec(&format!(
            "CREATE TABLE {a}.p (id int PRIMARY KEY);
             CREATE TABLE {a}.c (p_id int REFERENCES {a}.p (id));"
        ))
        .await;
        let role = format!("{a}_ro");
        let password = Uuid::now_v7().simple().to_string();
        if let Err(e) = sqlx::query(&format!("CREATE ROLE {role} LOGIN PASSWORD '{password}'"))
            .execute(&fx.pool)
            .await
        {
            eprintln!("跳过：建不了只读角色（{e}）");
            fx.cleanup().await;
            return;
        }
        fx.exec(&format!(
            "GRANT USAGE ON SCHEMA {a} TO {role};
             GRANT SELECT ON ALL TABLES IN SCHEMA {a} TO {role};"
        ))
        .await;
        let mut ro = url::Url::parse(&fx.url).expect("database url");
        ro.set_username(&role).expect("username");
        ro.set_password(Some(&password)).expect("password");

        let cols = fx.columns(ro.as_str()).await;
        assert!(col(&cols, &a, "p", "id").is_primary_key);
        assert_eq!(
            col(&cols, &a, "c", "p_id").references_table,
            Some(format!("{a}.p"))
        );

        fx.exec(&format!("DROP OWNED BY {role}; DROP ROLE {role};"))
            .await;
        fx.cleanup().await;
    }
}
