//! A push looks its document up by source, by content hash and, for a new external key, by file name
//! among the legacy documents. On the internal cluster both lookups read every document (a parallel
//! sequential scan of about a second each), and a burst of pushes held the whole connection pool.
//!
//! The plans target a temp copy of documents (statistics come only from seeding), so concurrent writes
//! in the shared database cannot flip them. `LIKE ... INCLUDING ALL` copies the indexes the migrations made.
use super::{LEGACY_BY_FILENAME_SQL, SOURCE_SHA_SQL};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

const DOCUMENTS: u32 = 200_000;
const SOURCES: u32 = 40;

/// One lookup: the source, the value it binds second, and whether a document is there to find.
struct Lookup {
    source: Uuid,
    value: String,
    found: bool,
}

/// Seeds the temp table once, then EXPLAINs `sql` for every lookup: (rows read from documents, rows returned).
async fn plans(sql: &str, lookups: &[Lookup]) -> anyhow::Result<Option<Vec<(u64, u64)>>> {
    let Some(url) = crate::test_db::url() else {
        return Ok(None);
    };
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let mut tx = pool.begin().await?;
    // Every source holds documents with an external key (pushed items) and without one (legacy),
    // a tenth of them deleted; content hashes and file names are unique. Source 7 also holds a
    // legacy document whose name is too long for a btree entry.
    sqlx::raw_sql(&format!(
        "CREATE TEMP TABLE documents (LIKE public.documents INCLUDING ALL) ON COMMIT DROP;
         INSERT INTO documents (id, kb_id, source_id, filename, sha256, external_key, deleted_at)
              SELECT gen_random_uuid(),
                     '00000000-0000-0000-0000-000000000001'::uuid,
                     ('00000000-0000-0000-0000-' || lpad((n % {SOURCES})::text, 12, '0'))::uuid,
                     'file-' || n || '.md',
                     'sha-' || n,
                     CASE WHEN n % 3 = 0 THEN NULL ELSE 'item:' || n END,
                     CASE WHEN n % 10 = 0 THEN now() END
                FROM generate_series(1, {DOCUMENTS}) n;
         INSERT INTO documents (id, kb_id, source_id, filename, sha256)
              SELECT gen_random_uuid(), '00000000-0000-0000-0000-000000000001'::uuid,
                     '00000000-0000-0000-0000-000000000007'::uuid,
                     (SELECT string_agg(to_hex((i * 2654435761) % 4294967296), '' ORDER BY i)
                        FROM generate_series(1, 400) i),
                     'sha-long-name';
         ANALYZE pg_temp.documents"
    ))
    .execute(&mut *tx)
    .await?;

    let mut out = Vec::with_capacity(lookups.len());
    for lookup in lookups {
        let plan: serde_json::Value =
            sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {sql}"))
                .persistent(false)
                .bind(lookup.source)
                .bind(&lookup.value)
                .fetch_one(&mut *tx)
                .await?;
        let top = &plan[0]["Plan"];
        out.push((
            document_rows_read(top),
            top["Actual Rows"].as_u64().unwrap_or(0),
        ));
    }
    tx.rollback().await?;
    Ok(Some(out))
}

fn check(lookups: &[Lookup], results: &[(u64, u64)]) {
    for (lookup, (read, returned)) in lookups.iter().zip(results) {
        assert!(
            *read < 1_000,
            "{read} document rows read for {}",
            lookup.value
        );
        assert_eq!(
            *returned,
            u64::from(lookup.found),
            "rows returned for {}",
            lookup.value
        );
    }
}

fn source(n: u32) -> Uuid {
    Uuid::parse_str(&format!("00000000-0000-0000-0000-{n:012}")).unwrap()
}

#[tokio::test]
async fn a_pushed_item_is_found_by_its_content_hash_without_reading_the_source(
) -> anyhow::Result<()> {
    let lookups = [
        Lookup {
            source: source(4_001 % SOURCES),
            value: "no such hash".into(),
            found: false,
        },
        Lookup {
            source: source(4_001 % SOURCES),
            value: "sha-4001".into(),
            found: true,
        },
    ];
    let Some(results) = plans(SOURCE_SHA_SQL, &lookups).await? else {
        return Ok(());
    };
    check(&lookups, &results);
    Ok(())
}

#[tokio::test]
async fn a_legacy_document_is_found_by_its_file_name_without_reading_the_source(
) -> anyhow::Result<()> {
    let lookups = [
        Lookup {
            source: source(3_003 % SOURCES),
            value: "no-such-file.md".into(),
            found: false,
        },
        Lookup {
            source: source(3_003 % SOURCES),
            value: "file-3003.md".into(),
            found: true,
        },
        // About 3,200 hex digits that hardly compress: too long for a btree entry, so it is indexed by its md5.
        Lookup {
            source: source(7),
            value: long_name(),
            found: true,
        },
    ];
    let Some(results) = plans(LEGACY_BY_FILENAME_SQL, &lookups).await? else {
        return Ok(());
    };
    check(&lookups, &results);
    Ok(())
}

/// The same name the seed writes for source 7.
fn long_name() -> String {
    (1..=400_u64)
        .map(|i| format!("{:x}", (i * 2_654_435_761) % 4_294_967_296))
        .collect()
}

/// Rows the plan read from `documents`, counting what filters threw away, over all loops.
fn document_rows_read(node: &serde_json::Value) -> u64 {
    let own_rows = if node["Relation Name"].as_str() == Some("documents") {
        ["Actual Rows", "Rows Removed by Filter"]
            .iter()
            .map(|field| node[*field].as_u64().unwrap_or(0))
            .sum::<u64>()
            * node["Actual Loops"].as_u64().unwrap_or(0)
    } else {
        0
    };
    own_rows
        + node["Plans"]
            .as_array()
            .map_or(0, |children| children.iter().map(document_rows_read).sum())
}
