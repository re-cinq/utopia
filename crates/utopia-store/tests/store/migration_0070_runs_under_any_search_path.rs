//! DDL for the invariant families (0070, 0091) must not depend on the session's search_path: `CREATE FUNCTION`,
//! `CREATE TRIGGER ... ON`, and `EXECUTE FUNCTION` are all qualified to `public.*`.
//! pg_restore empties the session search_path before loading data, and even a session with a first-position hijack must not be able
//! to build a function into some other schema. A trigger that lands elsewhere is no trigger at all.
//!
//! Verified one by one under three sessions (each of 0070 and 0091 is run through all three):
//!   - a normal search_path (the default) → installs;
//!   - `SET LOCAL search_path = ''` → installs just the same, with the function landing in public;
//!   - `SET LOCAL search_path = 'decoy'` (with a same-named decoy placed first) →
//!     still installs into public, and the decoy is never called.
//!
//! The scratch DB for the completeness guard migrates to **the current end of the chain**, not stopping at 0070: wherever the chain stops, the responsibility surface
//! stops there too — tables added to the DB after 0073 are structurally invisible to a guard that stops at 0070 (this is where
//! the blind spot of #901 came from).
//!
//! Skipped when `UTOPIA_DATABASE_URL` is not set; if an address is set but cannot be connected to, or the DB cannot be created — **fails**,
//! rather than skipping: to be given an address and still say "no DB" is a lie, and that green tick would mean this check never ran.

use sqlx::{Acquire, PgPool};
use std::collections::{BTreeSet, HashMap, HashSet};
use uuid::Uuid;

fn admin_url() -> Option<String> {
    let url = utopia_store::test_db::url()?;
    let (head, _) = url.rsplit_once('/')?;
    Some(format!("{head}/postgres"))
}

/// Runs the migrations with `after < version <= through` in file order, each in its own transaction
/// (same shape as sqlx::migrate)
async fn migrate_range(pool: &PgPool, after: i64, through: i64) -> anyhow::Result<()> {
    let migrator = sqlx::migrate!("../../migrations");
    let mut conn = pool.acquire().await?;
    for m in migrator
        .iter()
        .filter(|m| m.version > after && m.version <= through)
    {
        let mut tx = conn.begin().await?;
        sqlx::raw_sql(&m.sql).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    Ok(())
}

async fn migrate_to(pool: &PgPool, through: i64) -> anyhow::Result<()> {
    migrate_range(pool, 0, through).await
}

/// Runs the body of migration `version` in a transaction whose search_path is `path`
async fn migration_under(pool: &PgPool, version: i64, path: &str) -> Result<(), sqlx::Error> {
    let migrator = sqlx::migrate!("../../migrations");
    let m = migrator
        .iter()
        .find(|m| m.version == version)
        .unwrap_or_else(|| panic!("{version} 必须在迁移集里"));
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query(&format!("SET LOCAL search_path = {path}"))
        .execute(&mut *tx)
        .await?;
    let r = sqlx::raw_sql(&m.sql).execute(&mut *tx).await;
    match r {
        Ok(_) => tx.commit().await,
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

async fn migration_70_under(pool: &PgPool, path: &str) -> Result<(), sqlx::Error> {
    migration_under(pool, 70, path).await
}

/// Isolated DB: create → migrate to 0069 → return (DB name, connection pool).
/// Skipping has only one case: **`UTOPIA_DATABASE_URL` is not set at all**. If an address is set but cannot be connected to,
/// the DB cannot be created, or the migration chain cannot be run, all of these return an error — having been given an address yet getting no DB, this check
/// simply did not run, and it should not be recorded as green
async fn scratch(suffix: &str) -> anyhow::Result<Option<(String, PgPool)>> {
    let Some(admin) = admin_url() else {
        return Ok(None);
    };
    let admin_pool = PgPool::connect(&admin).await?;
    let name = format!("xkb70sp_{}_{}", suffix, Uuid::now_v7().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin_pool)
        .await?;
    admin_pool.close().await;
    let Some(url) = utopia_store::test_db::url() else {
        drop_scratch(&name).await;
        return Ok(None);
    };
    let (head, _) = url.rsplit_once('/').expect("UTOPIA_DATABASE_URL 缺库名段");
    let pool = PgPool::connect(&format!("{head}/{name}")).await?;
    if let Err(e) = migrate_to(&pool, 69).await {
        pool.close().await;
        drop_scratch(&name).await;
        return Err(e);
    }
    Ok(Some((name, pool)))
}

async fn drop_scratch(name: &str) {
    if let Some(admin) = admin_url() {
        if let Ok(pool) = PgPool::connect(&admin).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&pool)
                .await;
            pool.close().await;
        }
    }
}

/// Runs `f` on the isolated DB — whether it succeeds, fails, or an assertion panics, the DB is cleaned up before returning:
/// the evidence of a failing test must not depend on operators picking up abandoned DBs
async fn with_scratch<Fut>(suffix: &str, f: impl FnOnce(PgPool) -> Fut) -> anyhow::Result<()>
where
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    use futures_util::FutureExt;
    use std::panic::AssertUnwindSafe;
    let Some((name, pool)) = scratch(suffix).await? else {
        return Ok(());
    };
    // catch_unwind: an assertion panic lands on Err; the cleanup still runs as usual, then the panic is re-raised
    let r = AssertUnwindSafe(f(pool.clone())).catch_unwind().await;
    pool.close().await;
    drop_scratch(&name).await;
    match r {
        Ok(inner) => inner,
        Err(p) => std::panic::resume_unwind(p),
    }
}

/// Isolated DB for the completeness guard: after scratch migrates to 0069, run everything remaining on the chain to the end —
/// a guard that stops at 0070 is structurally blind to tables added later, so post-0070 reference columns can only be counted
/// by "migrating to the current end of the chain".
async fn with_full_chain<Fut>(suffix: &str, f: impl FnOnce(PgPool) -> Fut) -> anyhow::Result<()>
where
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    with_scratch(suffix, |pool| async move {
        migrate_range(&pool, 69, i64::MAX).await?;
        f(pool).await
    })
    .await
}

/// After installing 0070, advance the chain to 0090, then install 0091 under the search_path being tested.
/// The search_path of the middle stretch is not part of what is under test, so it takes the default path, in the same order as a real upgrade.
async fn finish_chain_then_91_under(pool: &PgPool, path: &str) -> anyhow::Result<()> {
    migrate_range(pool, 70, 90).await?;
    migration_under(pool, 91, path).await?;
    Ok(())
}

/// What to name after installation: 26 declarative composite foreign keys on edges, 10 triggers for parent-row attested edges,
/// 12 triggers for DB non-transferability, and 8 supporting unique constraints, with all new functions in the public schema.
/// The commit-boundary check for same-table self-referencing edges is carried by DEFERRABLE INITIALLY DEFERRED composite foreign keys
/// (supersedes / from_statement_id / inverse_of / sub_property_of)
async fn assert_installed(pool: &PgPool) -> anyhow::Result<()> {
    let fns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'public' AND p.proname IN (
            'fact_evidence_stays_inside_its_kb',
            'derivation_premise_stays_inside_its_kb','qualifier_stays_inside_its_facts_kb',
            'type_parent_stays_inside_its_kb',
            'relation_scope_stays_inside_its_kb',
            'relation_qualifier_stays_inside_the_kb',
            'rule_condition_refs_stay_inside_the_kb','kb_ownership_is_not_reassigned',
            'typed_source_stays_inside_its_kb',
            'squalifier_stays_inside_its_facts_kb')",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(fns, 10, "全部触发器函数都必须落在 public schema");

    // Composite foreign keys: one for each edge whose source row carries its own kb_id, 26 in all
    let fks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_constraint WHERE contype = 'f' AND conname IN (
           'chunks_document_same_kb',
           'facts_subject_same_kb','facts_object_same_kb','facts_predicate_same_kb',
           'facts_supersedes_same_kb','facts_from_statement_same_kb',
           'derived_facts_subject_same_kb','derived_facts_object_same_kb',
           'derived_facts_predicate_same_kb','derived_facts_rule_same_kb',
           'derived_facts_attribute_rule_same_kb',
           'entities_type_same_kb',
           'entity_type_disjoint_a_same_kb','entity_type_disjoint_b_same_kb',
           'relation_types_inverse_same_kb','relation_types_sub_property_same_kb',
           'rules_predicate_same_kb',
           'attribute_rules_subject_type_same_kb','attribute_rules_conclude_type_same_kb',
           'attribute_rules_conclude_predicate_same_kb',
           'time_mentions_fact_same_kb','time_mentions_chunk_same_kb',
           'type_bindings_type_same_kb',
           'phrase_bindings_subject_type_same_kb','phrase_bindings_object_type_same_kb',
           'phrase_bindings_relation_type_same_kb')",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(fks, 26, "每条声明式边一个复合外键，一条都不能少");

    // Supporting unique constraints: one (kb_id, id) per referenced table family
    let uniques: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_constraint WHERE contype = 'u' AND conname IN (
           'documents_kb_id_key','chunks_kb_id_key','entities_kb_id_key',
           'entity_types_kb_id_key','relation_types_kb_id_key','facts_kb_id_key',
           'rules_kb_id_key','attribute_rules_kb_id_key')",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(uniques, 8, "每个被引表一条 (kb_id, id) 唯一约束");

    // Self-referencing edges within the same table are really deferred: both condeferrable and condeferred bits are set
    let deferred: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_constraint
          WHERE contype = 'f' AND condeferrable AND condeferred
            AND conname IN ('facts_supersedes_same_kb',
                            'facts_from_statement_same_kb',
                            'relation_types_inverse_same_kb',
                            'relation_types_sub_property_same_kb')",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(deferred, 4, "同表自指边的提交边界检查必须在场");

    // Edges attested by the parent row and DB non-transferability: trigger counts (the FK's own internal triggers are not counted)
    let trg: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_trigger WHERE NOT tgisinternal AND tgname IN (
           'fact_evidence_same_kb','fact_derivations_same_kb',
           'fact_qualifiers_same_kb','entity_type_parents_same_kb',
           'relation_type_domains_same_kb','relation_type_ranges_same_kb',
           'relation_type_qualifiers_same_kb','attribute_rule_conditions_same_kb',
           'typed_fact_sources_same_kb','statement_qualifiers_same_kb',
           'facts_keep_their_kb','derived_facts_keep_their_kb','documents_keep_their_kb',
           'entities_keep_their_kb','entity_types_keep_their_kb','relation_types_keep_their_kb',
           'rules_keep_their_kb','attribute_rules_keep_their_kb',
           'entity_type_disjoint_keep_their_kb',
           'time_mentions_keep_their_kb','type_bindings_keep_their_kb',
           'phrase_bindings_keep_their_kb')",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(trg, 22, "父行作证的边与不可过户，一条都不能少");
    Ok(())
}

/// The assembly that 0091 must complete: the new trigger functions land in public, 13 new composite keys,
/// 3 `(kb_id,id)` supporting unique constraints, 5 KB-non-transferability triggers, and
/// the ownership-derivation trigger of implied_fact_sources — none of them may be missing.
async fn assert_0091_installed(pool: &PgPool) -> anyhow::Result<()> {
    let fns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'public' AND p.proname = 'implied_source_stays_inside_its_kb'",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(fns, 1, "触发器函数必须落在 public schema");

    let fks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_constraint WHERE contype = 'f' AND conname IN (
           'implication_rules_subject_type_same_kb','implication_rules_object_type_same_kb',
           'implication_rules_conclude_property_same_kb',
           'phrase_readings_entity_same_kb',
           'attribute_rule_versions_rule_same_kb',
           'derived_facts_attribute_rule_version_same_kb',
           'errata_runs_document_same_kb',
           'errata_actions_run_same_kb','errata_actions_document_same_kb',
           'errata_actions_fact_same_kb','errata_actions_statement_same_kb',
           'errata_actions_predicate_same_kb','errata_actions_new_fact_same_kb')",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(fks, 13, "0091 的复合外键要么全在要么全不在");

    let uniques: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_constraint WHERE contype = 'u' AND conname IN (
           'implication_rules_kb_id_key','attribute_rule_versions_kb_id_key',
           'errata_runs_kb_id_key')",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(uniques, 3, "每个新被引表一条 (kb_id, id) 唯一约束");

    let trg: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_trigger WHERE NOT tgisinternal AND tgname IN (
           'implied_fact_sources_same_kb',
           'implication_rules_keep_their_kb','phrase_readings_keep_their_kb',
           'errata_runs_keep_their_kb','errata_actions_keep_their_kb',
           'attribute_rule_versions_keep_their_kb')",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(trg, 6, "归属推导与不可过户触发器,一个都不能少");
    Ok(())
}

// =====================================================================
// Completeness guard: the coverage of 0070 is not maintained by "counting constraint names / trigger names" — that style of counting
// would stay fully green when someone adds a reference column to the ledger without anyone coming back to change 0070.
// Here it is the other way round: count every column-level reference edge of the responsibility surface afresh from pg_catalog, classify each one,
// and go red if any fails to fall into a class; a registry entry with no counterpart in the catalog is also red.
// =====================================================================

/// 0070's responsibility surface: the tables of the semantic ledger — those whose references the export resolves edge by edge.
/// This is a declaration of scope rather than a coverage assertion: a new table entering the ledger must first have its name registered,
/// and only then do its reference edges come up for classification one by one; tables outside the surface (audit, staging, operations) are not governed by this invariant.
const LEDGER_TABLES: &[&str] = &[
    // Semantic rows that carry their own kb_id
    "attribute_rule_versions",
    "attribute_rules",
    "chunks",
    "derived_facts",
    "documents",
    "entities",
    "entity_type_disjoint",
    "entity_types",
    "errata_actions",
    "errata_runs",
    "facts",
    "implication_rules",
    "name_vectors",
    "ontology_regression_cases",
    "phrase_bindings",
    "phrase_readings",
    "relation_types",
    "rules",
    "time_mentions",
    "type_bindings",
    // Join rows whose own row has no kb_id, where the kb authority is attested by the parent row
    "attribute_rule_conditions",
    "entity_type_parents",
    "fact_derivations",
    "fact_evidence",
    "fact_qualifiers",
    "implied_fact_sources",
    "relation_type_domains",
    "relation_type_qualifiers",
    "relation_type_ranges",
    "statement_qualifiers",
    "typed_fact_sources",
];

/// Parent-row attestation table: (table, owner column, owner table). The owner edge gives that row's kb authority;
/// it is not itself a "same-KB semantic reference", and is automatically classed as EXPLICIT_NON_SCOPE; every other edge pointing at a
/// table with a kb_id must be registered in TRIGGER_EDGES.
const OWNER_ROWS: &[(&str, &str, &str)] = &[
    ("attribute_rule_conditions", "rule_id", "attribute_rules"),
    ("entity_type_parents", "child_id", "entity_types"),
    ("fact_derivations", "derived_fact_id", "derived_facts"),
    ("fact_evidence", "fact_id", "facts"),
    ("fact_qualifiers", "fact_id", "facts"),
    ("implied_fact_sources", "fact_id", "facts"),
    (
        "relation_type_domains",
        "relation_type_id",
        "relation_types",
    ),
    (
        "relation_type_qualifiers",
        "relation_type_id",
        "relation_types",
    ),
    ("relation_type_ranges", "relation_type_id", "relation_types"),
    ("statement_qualifiers", "fact_id", "facts"),
    ("typed_fact_sources", "fact_id", "facts"),
];

/// Trigger-edge registry: (table, column, trigger name). Registration is column-level — "this table has a trigger installed"
/// does not imply "the newly added column is checked by it": beyond registration, it is also checked that the column sits in that trigger's
/// UPDATE OF list. A new reference column added to a table that points at a kb_id table, without being registered,
/// falls into no class.
const TRIGGER_EDGES: &[(&str, &str, &str)] = &[
    (
        "attribute_rule_conditions",
        "predicate_id",
        "attribute_rule_conditions_same_kb",
    ),
    (
        "entity_type_parents",
        "parent_id",
        "entity_type_parents_same_kb",
    ),
    (
        "fact_derivations",
        "premise_derived_id",
        "fact_derivations_same_kb",
    ),
    (
        "fact_derivations",
        "premise_fact_id",
        "fact_derivations_same_kb",
    ),
    ("fact_evidence", "chunk_id", "fact_evidence_same_kb"),
    ("fact_evidence", "document_id", "fact_evidence_same_kb"),
    ("fact_qualifiers", "entity_id", "fact_qualifiers_same_kb"),
    (
        "fact_qualifiers",
        "qualifier_type_id",
        "fact_qualifiers_same_kb",
    ),
    (
        "implied_fact_sources",
        "entity_id",
        "implied_fact_sources_same_kb",
    ),
    (
        "implied_fact_sources",
        "rule_id",
        "implied_fact_sources_same_kb",
    ),
    (
        "implied_fact_sources",
        "statement_id",
        "implied_fact_sources_same_kb",
    ),
    (
        "relation_type_domains",
        "entity_type_id",
        "relation_type_domains_same_kb",
    ),
    (
        "relation_type_qualifiers",
        "qualifier_type_id",
        "relation_type_qualifiers_same_kb",
    ),
    (
        "relation_type_ranges",
        "entity_type_id",
        "relation_type_ranges_same_kb",
    ),
    (
        "statement_qualifiers",
        "chunk_id",
        "statement_qualifiers_same_kb",
    ),
    (
        "statement_qualifiers",
        "entity_id",
        "statement_qualifiers_same_kb",
    ),
    (
        "typed_fact_sources",
        "statement_id",
        "typed_fact_sources_same_kb",
    ),
];

/// Edges within the surface that do not belong to same-KB semantic references — one sentence of reason each. Owner edges are not listed here;
/// they are automatically exempted by OWNER_ROWS.
const NON_SCOPE: &[(&str, &str, &str)] = &[
    // Collection-source ownership: the export never resolves this reference into an IRI
    (
        "documents",
        "source_id",
        "ingest attribution, not an export-resolved reference",
    ),
    // Merge bookkeeping: rows with a non-empty merged_into are not exported, so this pointer never enters an IRI
    (
        "entities",
        "merged_into",
        "merge bookkeeping — merged rows never reach export",
    ),
];

/// One column-level reference edge counted from pg_catalog. `pairs` is all the column pairs that the same constraint aligns by
/// conkey/confkey ordinal — one composite foreign key yields several rows of edges,
/// which share the same pairs.
struct RefEdge {
    src_table: String,
    src_col: String,
    tgt_table: String,
    tgt_col: String,
    conname: String,
    deferrable: bool,
    deferred: bool,
    pairs: Vec<(String, String)>,
}

impl RefEdge {
    fn label(&self) -> String {
        format!(
            "{}.{}\u{2192}{}.{}",
            self.src_table, self.src_col, self.tgt_table, self.tgt_col
        )
    }
}

/// Counts afresh every foreign-key column pair of the public schema from pg_catalog — it does not read the migration source,
/// and aligns conkey/confkey by ordinal rather than guessing from array position.
async fn catalog_edges(pool: &PgPool) -> anyhow::Result<Vec<RefEdge>> {
    let rows = sqlx::query_as::<_, (i64, String, String, String, String, String, bool, bool)>(
        "SELECT c.oid::bigint,
                src.relname, sa.attname, tgt.relname, ta.attname,
                c.conname, c.condeferrable, c.condeferred
           FROM pg_constraint c
           JOIN pg_class src ON src.oid = c.conrelid
           JOIN pg_namespace sn
             ON sn.oid = src.relnamespace AND sn.nspname = 'public'
           JOIN pg_class tgt ON tgt.oid = c.confrelid
           JOIN pg_namespace tn
             ON tn.oid = tgt.relnamespace AND tn.nspname = 'public'
           JOIN unnest(c.conkey) WITH ORDINALITY AS ck(attnum, ord) ON true
           JOIN pg_attribute sa
             ON sa.attrelid = c.conrelid AND sa.attnum = ck.attnum
           JOIN pg_attribute ta
             ON ta.attrelid = c.confrelid AND ta.attnum = c.confkey[ck.ord]
          WHERE c.contype = 'f'
          ORDER BY c.oid, ck.ord",
    )
    .fetch_all(pool)
    .await?;
    let mut pairs: HashMap<i64, Vec<(String, String)>> = HashMap::new();
    for (oid, _, sc, _, tc, _, _, _) in &rows {
        pairs
            .entry(*oid)
            .or_default()
            .push((sc.clone(), tc.clone()));
    }
    Ok(rows
        .into_iter()
        .map(|(oid, st, sc, tt, tc, cn, dfr, dfd)| RefEdge {
            pairs: pairs.get(&oid).cloned().unwrap_or_default(),
            src_table: st,
            src_col: sc,
            tgt_table: tt,
            tgt_col: tc,
            conname: cn,
            deferrable: dfr,
            deferred: dfd,
        })
        .collect())
}

/// A table with a kb_id column = a row that has KB ownership.
async fn kb_scoped_tables(pool: &PgPool) -> anyhow::Result<HashSet<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT c.relname FROM pg_class c
           JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'public' AND c.relkind = 'r'
            AND EXISTS (SELECT 1 FROM pg_attribute a
                         WHERE a.attrelid = c.oid AND a.attname = 'kb_id'
                           AND a.attnum > 0 AND NOT a.attisdropped)",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect())
}

/// User trigger -> the set of column names it watches with UPDATE OF (tgattr).
async fn trigger_watch_lists(
    pool: &PgPool,
) -> anyhow::Result<HashMap<(String, String), HashSet<String>>> {
    let rows = sqlx::query_as::<_, (String, String, Vec<String>)>(
        "SELECT cls.relname, t.tgname, COALESCE(w.cols, '{}'::text[])
           FROM pg_trigger t
           JOIN pg_class cls ON cls.oid = t.tgrelid
           JOIN pg_namespace n ON n.oid = cls.relnamespace AND n.nspname = 'public'
           LEFT JOIN LATERAL (
               SELECT array_agg(a.attname) AS cols
                 FROM pg_attribute a
                WHERE a.attrelid = t.tgrelid
                  AND a.attnum = ANY (string_to_array(t.tgattr::text, ' ')::smallint[])
           ) w ON true
          WHERE NOT t.tgisinternal",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(t, g, c)| ((t, g), c.into_iter().collect()))
        .collect())
}

/// Tables carrying the 0070 mechanisms (same-KB constraints/triggers, non-transferability triggers, supporting unique constraints) —
/// used to cross-check against LEDGER_TABLES: a mechanism outside the surface, or an in-surface table with no mechanism at all, both count as drift.
async fn mechanism_tables(pool: &PgPool) -> anyhow::Result<HashSet<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT cls.relname FROM pg_constraint c
           JOIN pg_class cls ON cls.oid = c.conrelid
           JOIN pg_namespace n ON n.oid = cls.relnamespace AND n.nspname = 'public'
          WHERE c.conname::text ~ '(_same_kb|_kb_id_key)$'
          UNION
         SELECT cls.relname FROM pg_trigger t
           JOIN pg_class cls ON cls.oid = t.tgrelid
           JOIN pg_namespace n ON n.oid = cls.relnamespace AND n.nspname = 'public'
          WHERE NOT t.tgisinternal
            AND t.tgname::text ~ '(_same_kb|_keep_their_kb)$'",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect())
}

/// The result of classifying each edge. Beyond declarative / trigger_covered / non_scope,
/// if any bucket is non-empty, coverage is incomplete.
#[derive(Debug, Default)]
struct Coverage {
    declarative: Vec<String>,
    trigger_covered: Vec<String>,
    non_scope: Vec<String>,
    /// The structural identity of edges classed as declarative / trigger_covered
    /// (src_table, src_col, tgt_table, tgt_col) — the health check matches by the four-tuple,
    /// so two edges sharing one error label must each count as one
    protected: BTreeSet<EdgeKey>,
    /// Edges that catalog has on owner-derived tables but registration does not
    unknown: Vec<String>,
    /// Edges where registration / exemption / owner declarations do not match in the catalog
    stale_registry: Vec<String>,
    /// Edges on KB-self-owned rows with no legal composite foreign key
    unprotected_direct: Vec<String>,
    /// Edges that are registered, but whose trigger is not installed or does not watch that column
    unprotected_trigger: Vec<String>,
    /// The responsibility surface does not match the installed mechanisms
    surface_drift: Vec<String>,
    /// Edges protected by the schema, but for which preflight has no corresponding scan branch
    preflight_missing: Vec<String>,
    /// Edges declared by a preflight scan that do not match the catalog protected set —
    /// rotten branches left behind after a table or column rename
    stale_preflight: Vec<String>,
}

impl Coverage {
    /// Complete = no unclassified edges, no rotten registrations, and the surface and mechanisms cross-check; every protected edge
    /// enters the export health check, and the health check has no rotten branches,
    /// and the counts of the two covered-edge categories match 44 / 17 on the current migration chain.
    fn assert_complete(&self) -> anyhow::Result<()> {
        let mut problems = String::new();
        let mut dump = |name: &str, xs: &[String]| {
            for x in xs {
                problems.push_str(&format!("  {name}: {x}\n"));
            }
        };
        dump("UNKNOWN", &self.unknown);
        dump("STALE_REGISTRY", &self.stale_registry);
        dump("UNPROTECTED_DIRECT", &self.unprotected_direct);
        dump("UNPROTECTED_TRIGGER", &self.unprotected_trigger);
        dump("SURFACE_DRIFT", &self.surface_drift);
        dump("PREFLIGHT_MISSING", &self.preflight_missing);
        dump("STALE_PREFLIGHT", &self.stale_preflight);
        // 0109 adds the statement and expected-property references on regression cases.
        if self.declarative.len() != 44 {
            problems.push_str(&format!(
                "  DECLARATIVE_EDGES = {} (expected 44)\n",
                self.declarative.len()
            ));
        }
        // 0121 adds the chunk a role word is recorded against.
        if self.trigger_covered.len() != 17 {
            problems.push_str(&format!(
                "  TRIGGER_EDGES = {} (expected 17)\n",
                self.trigger_covered.len()
            ));
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "catalog-derived coverage is incomplete:\n{problems}"
            ))
        }
    }
}

/// The four-tuple marking one structural edge in the preflight SQL
/// (src_table, src_col, tgt_table, tgt_col)
type EdgeKey = (String, String, String, String);

/// The coverage surface parsed from export_provenance_integrity.sql — the test reads
/// the very copy executed at runtime (the same path via include_str!), so there is no seam where one copy is registered for the test to see and
/// another copy is run. edges = the structural edges each cross_kb branch declares;
/// labels = structural edge → error label; filters = the "same KB but not in the export set"
/// merged check — it guards the export filter's scope, not schema reference edges
#[derive(Default)]
struct PreflightSurface {
    edges: BTreeSet<EdgeKey>,
    labels: HashMap<EdgeKey, String>,
    filters: BTreeSet<String>,
}

/// The string literals in a block of SQL text, in order of appearance — the first two literals of each scan branch
/// are its error label and kind ('cross_kb' / 'unexported')
fn quoted_literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\'' {
            continue;
        }
        let mut lit = String::new();
        for c2 in chars.by_ref() {
            if c2 == '\'' {
                break;
            }
            lit.push(c2);
        }
        out.push(lit);
    }
    out
}

/// 'src_table.src_col -> tgt_table.tgt_col' → four-tuple
fn parse_edge_key(marker: &str) -> Option<EdgeKey> {
    let (src, tgt) = marker.split_once("->")?;
    let (st, sc) = src.trim().rsplit_once('.')?;
    let (tt, tc) = tgt.trim().rsplit_once('.')?;
    Some((st.into(), sc.into(), tt.into(), tc.into()))
}

/// Splits the preflight SQL into scan branches on UNION ALL, and checks the markers branch by branch:
/// a cross_kb branch carries exactly one @edge, an unexported branch exactly one @filter with the same name as its label;
/// the table/column reported by @edge must genuinely appear in the branch body — a marker pointing in the wrong direction,
/// a branch missing its marker, or one edge appearing in two branches, all blow up here
fn preflight_surface() -> anyhow::Result<PreflightSurface> {
    const SRC: &str = include_str!("../../src/export_provenance_integrity.sql");
    let mut surface = PreflightSurface::default();
    let mut problems = String::new();
    for (i, chunk) in SRC.split("UNION ALL").enumerate() {
        let mut edge_markers = Vec::new();
        let mut filter_markers = Vec::new();
        for line in chunk.lines() {
            let l = line.trim();
            if let Some(m) = l.strip_prefix("-- @edge ") {
                edge_markers.push(m.trim().to_string());
            } else if let Some(m) = l.strip_prefix("-- @filter ") {
                filter_markers.push(m.trim().to_string());
            }
        }
        let literals = quoted_literals(chunk);
        // A comment that writes "UNION ALL" would also split out a piece — such a fragment has no SELECT, so skip it;
        // a real branch missing its literals is still an error
        let (Some(label), Some(kind)) = (literals.first(), literals.get(1)) else {
            if chunk.contains("SELECT") {
                problems.push_str(&format!("  branch {i}: no 'label'/'kind' literals found\n"));
            }
            continue;
        };
        match kind.as_str() {
            "cross_kb" => {
                if edge_markers.len() != 1 || !filter_markers.is_empty() {
                    problems.push_str(&format!(
                        "  {label}: a cross_kb branch must carry exactly one @edge marker\n"
                    ));
                    continue;
                }
                let Some(key) = parse_edge_key(&edge_markers[0]) else {
                    problems.push_str(&format!(
                        "  {label}: malformed @edge marker '{}'\n",
                        edge_markers[0]
                    ));
                    continue;
                };
                for needle in [&key.0, &key.1, &key.2] {
                    if !chunk.contains(needle.as_str()) {
                        problems.push_str(&format!(
                            "  {label}: @edge names '{needle}' but the branch never mentions it\n"
                        ));
                    }
                }
                if !surface.edges.insert(key.clone()) {
                    problems.push_str(&format!(
                        "  {label}: @edge '{}' duplicates an earlier branch\n",
                        edge_markers[0]
                    ));
                }
                surface.labels.insert(key, label.clone());
            }
            "unexported" => {
                if filter_markers.len() != 1 || !edge_markers.is_empty() {
                    problems.push_str(&format!(
                        "  {label}: an unexported branch must carry exactly one @filter marker\n"
                    ));
                    continue;
                }
                if filter_markers[0] != *label {
                    problems.push_str(&format!(
                        "  {label}: @filter names '{}' — the marker must equal the branch label\n",
                        filter_markers[0]
                    ));
                }
                surface.filters.insert(label.clone());
            }
            other => problems.push_str(&format!("  {label}: unknown kind '{other}'\n")),
        }
    }
    if problems.is_empty() {
        Ok(surface)
    } else {
        Err(anyhow::anyhow!(
            "preflight scan markers are inconsistent:\n{problems}"
        ))
    }
}

/// Every catalog edge classified as a protected edge must have, in the same preflight SQL,
/// a scan branch declaring the same structural identity; conversely, every edge preflight claims must
/// still be a member of the catalog protected set — a stale branch left behind after a table or column rename is still red
fn check_preflight(cov: &mut Coverage) -> anyhow::Result<()> {
    let surface = preflight_surface()?;
    for key in &surface.edges {
        if !cov.protected.contains(key) {
            cov.stale_preflight.push(format!(
                "{}.{}\u{2192}{}.{} ('{}'): preflight watches an edge the catalog guard does not protect",
                key.0, key.1, key.2, key.3, surface.labels[key]
            ));
        }
    }
    for key in &cov.protected {
        if !surface.edges.contains(key) {
            cov.preflight_missing.push(format!(
                "{}.{}\u{2192}{}.{}: protected in the schema but absent from export preflight",
                key.0, key.1, key.2, key.3
            ));
        }
    }
    Ok(())
}

/// Each relevant edge is placed into exactly one class. Relevant = the source table is in the responsibility surface, the referencing column is not kb_id
/// itself, and the target table carries a kb_id — a container edge such as kb_id→knowledge_bases is naturally
/// kept outside the surface by `src_col != kb_id`, and the kb_id→kb_id leg inside a composite foreign key
/// is likewise only a binding mechanism, not an independent semantic reference.
fn classify(
    edges: &[RefEdge],
    kb_scoped: &HashSet<String>,
    triggers: &HashMap<(String, String), HashSet<String>>,
    mechanism: &HashSet<String>,
) -> Coverage {
    let ledger: HashSet<&str> = LEDGER_TABLES.iter().copied().collect();
    let owner: HashMap<&str, (&str, &str)> =
        OWNER_ROWS.iter().map(|(t, c, p)| (*t, (*c, *p))).collect();
    let registry: HashMap<(&str, &str), &str> = TRIGGER_EDGES
        .iter()
        .map(|(t, c, g)| ((*t, *c), *g))
        .collect();
    let exemptions: HashMap<(&str, &str), &str> =
        NON_SCOPE.iter().map(|(t, c, r)| ((*t, *c), *r)).collect();
    let mut cov = Coverage::default();

    for t in mechanism.iter().filter(|t| !ledger.contains(t.as_str())) {
        cov.surface_drift.push(format!(
            "{t}: carries 0070 mechanism but is outside the declared surface"
        ));
    }
    for t in ledger.iter().filter(|t| !mechanism.contains(**t)) {
        cov.surface_drift.push(format!(
            "{t}: declared in the surface but carries no 0070 mechanism"
        ));
    }
    for (t, _, _) in OWNER_ROWS {
        if kb_scoped.contains(*t) {
            cov.surface_drift.push(format!(
                "{t}: owner-derived row grew its own kb_id — family changed"
            ));
        }
    }
    for t in ledger.iter().filter(|t| !owner.contains_key(**t)) {
        if !kb_scoped.contains(*t) {
            cov.surface_drift.push(format!(
                "{t}: direct-kb surface table lost its kb_id column"
            ));
        }
    }

    // Drift in all three directions of the registry: owner edges, trigger edges, and exempt edges must all still exist in the catalog
    for (t, col, parent) in OWNER_ROWS {
        if !edges
            .iter()
            .any(|e| e.src_table == *t && e.src_col == *col && e.tgt_table == *parent)
        {
            cov.stale_registry.push(format!(
                "{t}.{col}\u{2192}{parent}: declared owner edge absent from catalog"
            ));
        }
    }
    for (t, c, g) in TRIGGER_EDGES {
        if !edges.iter().any(|e| e.src_table == *t && e.src_col == *c) {
            cov.stale_registry.push(format!(
                "{t}.{c} ({g}): registered trigger edge absent from catalog"
            ));
        }
    }
    for (t, c, _) in NON_SCOPE {
        if !edges.iter().any(|e| e.src_table == *t && e.src_col == *c) {
            cov.stale_registry
                .push(format!("{t}.{c}: exempted edge absent from catalog"));
        }
    }

    for e in edges.iter().filter(|e| {
        ledger.contains(e.src_table.as_str())
            && e.src_col != "kb_id"
            && kb_scoped.contains(e.tgt_table.as_str())
    }) {
        let label = e.label();
        if let Some((owner_col, owner_table)) = owner.get(e.src_table.as_str()) {
            if e.src_col == *owner_col {
                if e.tgt_table == *owner_table {
                    cov.non_scope.push(format!("{label} (owner edge)"));
                } else {
                    cov.stale_registry.push(format!(
                        "{label}: owner column now points at {}, not {owner_table}",
                        e.tgt_table
                    ));
                }
            } else if let Some(tg) = registry.get(&(e.src_table.as_str(), e.src_col.as_str())) {
                match triggers.get(&(e.src_table.clone(), (*tg).to_string())) {
                    Some(cols) if cols.contains(&e.src_col) => {
                        cov.trigger_covered.push(label);
                        cov.protected.insert(edge_key(e));
                    }
                    Some(_) => cov
                        .unprotected_trigger
                        .push(format!("{label}: trigger {tg} does not watch this column")),
                    None => cov
                        .unprotected_trigger
                        .push(format!("{label}: trigger {tg} is not installed")),
                }
            } else {
                cov.unknown.push(label);
            }
        } else if let Some(reason) = exemptions.get(&(e.src_table.as_str(), e.src_col.as_str())) {
            cov.non_scope.push(format!("{label} ({reason})"));
        } else {
            // Direct kb edge: within one constraint it must bind both (kb_id→kb_id) and (ref→id) at once;
            // a single-column FOREIGN KEY (ref) REFERENCES t(id) does not count as same-KB coverage
            let composite = e.tgt_col == "id"
                && e.pairs.len() == 2
                && e.pairs.iter().any(|(s, t)| s == "kb_id" && t == "kb_id");
            if !composite {
                cov.unprotected_direct.push(format!(
                    "{label} via {} — no composite (kb_id, ref) \u{2192} (kb_id, id)",
                    e.conname
                ));
            } else if e.src_table == e.tgt_table && !(e.deferrable && e.deferred) {
                cov.unprotected_direct.push(format!(
                    "{label} via {} — same-table self reference must be \
                     DEFERRABLE INITIALLY DEFERRED",
                    e.conname
                ));
            } else {
                cov.declarative.push(label);
                cov.protected.insert(edge_key(e));
            }
        }
    }
    cov
}

fn edge_key(e: &RefEdge) -> EdgeKey {
    (
        e.src_table.clone(),
        e.src_col.clone(),
        e.tgt_table.clone(),
        e.tgt_col.clone(),
    )
}

async fn classify_reference_edges(pool: &PgPool) -> anyhow::Result<Coverage> {
    let (edges, kb_scoped, triggers, mechanism) = tokio::try_join!(
        catalog_edges(pool),
        kb_scoped_tables(pool),
        trigger_watch_lists(pool),
        mechanism_tables(pool),
    )?;
    let mut cov = classify(&edges, &kb_scoped, &triggers, &mechanism);
    check_preflight(&mut cov)?;
    Ok(cov)
}

#[tokio::test]
async fn every_reference_edge_on_the_ledger_is_classified() -> anyhow::Result<()> {
    with_full_chain("cover", |pool| async move {
        classify_reference_edges(&pool).await?.assert_complete()
    })
    .await
}

/// Drift probe A: add a single-column foreign key to a KB-self-owned row (without kb_id).
/// "The name is still there" does not stop it — the catalog would count it out, and it would fall into no class.
#[tokio::test]
async fn a_new_reference_on_a_kb_owned_row_fails_the_guard() -> anyhow::Result<()> {
    with_full_chain("driftd", |pool| async move {
        sqlx::query(
            "ALTER TABLE public.time_mentions
             ADD COLUMN probe_ref uuid REFERENCES public.relation_types(id)",
        )
        .execute(&pool)
        .await?;
        let cov = classify_reference_edges(&pool).await?;
        assert!(
            cov.unprotected_direct
                .iter()
                .any(|e| e.starts_with("time_mentions.probe_ref\u{2192}")),
            "新加的单列引用必须落进 UNPROTECTED_DIRECT: {cov:?}"
        );
        assert!(cov.assert_complete().is_err());
        Ok(())
    })
    .await
}

/// Drift probe B: add a reference column pointing at a kb table to a parent-row-attested table, without touching its
/// trigger. "The table has a trigger" must not make this new column appear covered — registration is column-level.
#[tokio::test]
async fn a_new_reference_on_an_owner_derived_row_fails_the_guard() -> anyhow::Result<()> {
    with_full_chain("driftt", |pool| async move {
        sqlx::query(
            "ALTER TABLE public.fact_evidence
             ADD COLUMN probe_rel uuid REFERENCES public.relation_types(id)",
        )
        .execute(&pool)
        .await?;
        let cov = classify_reference_edges(&pool).await?;
        assert!(
            cov.unknown
                .iter()
                .any(|e| e.starts_with("fact_evidence.probe_rel\u{2192}")),
            "未登记的新边必须落进 UNKNOWN: {cov:?}"
        );
        assert!(cov.assert_complete().is_err());
        Ok(())
    })
    .await
}

/// Drift probe C: add a composite foreign key to a KB-self-owned row whose protection is completely correct —
/// the schema side finds nothing wrong (it is classed as DECLARATIVE), but nobody has added an export health check for it.
/// A guard that checks only the schema would let it through; this edge must land in PREFLIGHT_MISSING,
/// with the error naming which structural edge it is
#[tokio::test]
async fn a_protected_edge_missing_from_preflight_fails_the_guard() -> anyhow::Result<()> {
    with_full_chain("driftp", |pool| async move {
        sqlx::query(
            "ALTER TABLE public.time_mentions
             ADD COLUMN probe_ref uuid,
             ADD CONSTRAINT time_mentions_probe_same_kb
                 FOREIGN KEY (kb_id, probe_ref) REFERENCES public.relation_types (kb_id, id)",
        )
        .execute(&pool)
        .await?;
        let cov = classify_reference_edges(&pool).await?;
        assert!(
            cov.declarative
                .iter()
                .any(|e| e.starts_with("time_mentions.probe_ref\u{2192}")),
            "保护方式正确的探针边必须归进 DECLARATIVE——schema 侧没有问题: {cov:?}"
        );
        assert!(
            cov.preflight_missing
                .iter()
                .any(|e| e.contains("time_mentions.probe_ref")),
            "漏登记的探针边必须落进 PREFLIGHT_MISSING: {cov:?}"
        );
        let err = cov.assert_complete().unwrap_err().to_string();
        assert!(
            err.contains("PREFLIGHT_MISSING") && err.contains("time_mentions.probe_ref"),
            "报错必须点名缺体检的结构边: {err}"
        );
        Ok(())
    })
    .await
}

/// The comparator's own sensitivity: one fewer declared edge → MISSING; one extra catalog-protected edge
/// with no declaration → STALE. The guard itself must not be a fixture that passes no matter what
#[test]
fn the_preflight_check_notices_a_dropped_or_stray_edge() -> anyhow::Result<()> {
    let surface = preflight_surface()?;
    // The same file is counted once in each direction: 61 structural edges + 5 merged filter checks
    assert_eq!(surface.edges.len(), 61, "preflight 必须覆盖全部保护边");
    assert_eq!(
        surface.filters,
        [
            "derived.object(merged)",
            "derived.subject(merged)",
            "fact.object(merged)",
            "fact.subject(merged)",
            "qualifier.entity(merged)",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        "merged 过滤完整性检查是单独一类,不许静默增减"
    );

    // With the protected set equal to the declared set of the health check, both buckets are empty
    let mut cov = Coverage {
        protected: surface.edges.clone(),
        ..Coverage::default()
    };
    check_preflight(&mut cov)?;
    assert!(
        cov.preflight_missing.is_empty() && cov.stale_preflight.is_empty(),
        "声明集与保护集一致时不许误报: {cov:?}"
    );

    // Drop one: suppose some edge is removed from the SQL while the schema still protects it
    let mut missing = Coverage {
        protected: surface.edges.clone(),
        ..Coverage::default()
    };
    missing.protected.insert((
        "phantom_table".into(),
        "phantom_col".into(),
        "entities".into(),
        "id".into(),
    ));
    check_preflight(&mut missing)?;
    assert_eq!(
        missing.preflight_missing.len(),
        1,
        "catalog 有而 preflight 没有的边必须落进 MISSING"
    );

    // The reverse: preflight still checks an edge that the catalog no longer protects
    let mut stale = Coverage::default();
    check_preflight(&mut stale)?;
    assert_eq!(
        stale.stale_preflight.len(),
        surface.edges.len(),
        "catalog 保护集之外的分支必须全部落进 STALE"
    );
    Ok(())
}

#[tokio::test]
async fn the_migration_installs_under_an_empty_search_path() -> anyhow::Result<()> {
    with_scratch("empty", |pool| async move {
        let r = migration_70_under(&pool, "''").await;
        assert!(r.is_ok(), "search_path 为空时装得上 0070: {r:?}");
        assert_installed(&pool).await?;
        let r = finish_chain_then_91_under(&pool, "''").await;
        assert!(r.is_ok(), "search_path 为空时装得上 0091: {r:?}");
        assert_0091_installed(&pool).await
    })
    .await
}

#[tokio::test]
async fn the_migration_installs_under_a_hostile_search_path() -> anyhow::Result<()> {
    with_scratch("evil", |pool| async move {
        // Put a decoy schema first: a same-named entities table and a same-named function — bare-name resolution would see it first.
        // DDL qualified to public.* should ignore it
        sqlx::query("CREATE SCHEMA decoy").execute(&pool).await?;
        sqlx::query("CREATE TABLE decoy.entities (id uuid, kb_id uuid, merged_into uuid)")
            .execute(&pool)
            .await?;
        sqlx::query(
            "CREATE FUNCTION decoy.kb_ownership_is_not_reassigned() RETURNS trigger
             LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END; $$",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE FUNCTION decoy.implied_source_stays_inside_its_kb() RETURNS trigger
             LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END; $$",
        )
        .execute(&pool)
        .await?;

        let r = migration_70_under(&pool, "decoy, public").await;
        assert!(r.is_ok(), "首位被占的 search_path 下也装得上 0070: {r:?}");
        assert_installed(&pool).await?;
        let r = finish_chain_then_91_under(&pool, "decoy, public").await;
        assert!(r.is_ok(), "首位被占的 search_path 下也装得上 0091: {r:?}");
        assert_0091_installed(&pool).await?;
        // The decoy is left as it is: it was never selected once
        let decoy_fn: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
              WHERE n.nspname = 'decoy' AND p.proname IN
                ('kb_ownership_is_not_reassigned','implied_source_stays_inside_its_kb')",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(decoy_fn, 2, "decoy 函数不该被覆盖也不该被删掉");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn the_migration_installs_under_a_normal_search_path() -> anyhow::Result<()> {
    with_scratch("norm", |pool| async move {
        let r = migration_70_under(&pool, "public").await;
        assert!(r.is_ok(), "正常 search_path 下装得上 0070: {r:?}");
        assert_installed(&pool).await?;
        let r = finish_chain_then_91_under(&pool, "public").await;
        assert!(r.is_ok(), "正常 search_path 下装得上 0091: {r:?}");
        assert_0091_installed(&pool).await
    })
    .await
}

/// The precondition check for 0091: a DB that already has cross-KB provenance lying in its ledger must abort the install of 0091 entirely —
/// installing the invariant while reporting success on a ledger that already breaks it would be vouching for bad data. The abort must be complete:
/// it reports which edge it was, and must leave not a single new mechanism behind.
#[tokio::test]
async fn migration_91_refuses_a_ledger_with_cross_kb_sources() -> anyhow::Result<()> {
    with_scratch("dirty", |pool| async move {
        migration_70_under(&pool, "public").await?;
        migrate_range(&pool, 70, 90).await?;

        // Two KBs, each with one entity / attribute / rule / fact; the provenance row's fact is in this KB, its rule is in another KB
        sqlx::query(
            "WITH org AS (
                INSERT INTO organizations (id, name) VALUES (gen_random_uuid(), 't')
                RETURNING id
             ), ws AS (
                INSERT INTO workspaces (id, org_id, name)
                SELECT gen_random_uuid(), id, 't' FROM org RETURNING id
             ), kbs AS (
                INSERT INTO knowledge_bases (id, workspace_id, name)
                SELECT gen_random_uuid(), ws.id, 'kb' || i FROM ws, generate_series(1, 2) i
                RETURNING id
             ), a AS (SELECT id FROM kbs ORDER BY id LIMIT 1),
                  b AS (SELECT id FROM kbs ORDER BY id OFFSET 1),
             ents AS (
                INSERT INTO entities (id, kb_id, canonical_name)
                SELECT gen_random_uuid(), id, 'e' FROM kbs RETURNING id, kb_id
             ), rels AS (
                INSERT INTO relation_types (id, kb_id, key, label)
                SELECT gen_random_uuid(), id, 'r', 'r' FROM kbs RETURNING id, kb_id
             ), fcts AS (
                INSERT INTO facts (id, kb_id, subject_id, object_id, confidence)
                SELECT gen_random_uuid(), e.kb_id, e.id, e.id, 0.9 FROM ents e
                RETURNING id, kb_id
             ), rules_ AS (
                INSERT INTO implication_rules
                    (id, kb_id, trigger, phrase, conclude_property_id, status)
                SELECT gen_random_uuid(), r.kb_id, 'phrase', 'p', r.id, 'approved' FROM rels r
                RETURNING id, kb_id
             )
             -- statement_id 是本库事实(CHECK 要求 statement/entity 恰好一个):
             -- 只坏 rule 那一头,报错的边名才好对
             INSERT INTO implied_fact_sources (fact_id, rule_id, statement_id)
             SELECT f.id, i.id, f.id FROM fcts f, rules_ i
              WHERE f.kb_id = (SELECT id FROM a) AND i.kb_id = (SELECT id FROM b)",
        )
        .execute(&pool)
        .await?;

        let r = migration_under(&pool, 91, "public").await;
        let err = r.expect_err("已有越库出处的账本必须装不上 0091");
        let msg = err.to_string();
        assert!(
            msg.contains("cross-KB references already present") && msg.contains("implied.rule"),
            "报错必须点名是哪条边: {msg}"
        );

        // The abort must be clean: not a single row of new mechanism may remain
        let leftovers: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_constraint
              WHERE contype = 'f' AND conname LIKE 'implication_rules%_same_kb'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(leftovers, 0, "失败的迁移不许留半套装上的机制");
        Ok(())
    })
    .await
}
