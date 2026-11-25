//! The declaration arrives late: in a KB whose ontology grew on its own, a successor does not close its predecessor (#341).
//!
//! Pins three things:
//! - **Candidates are computed, not guessed**: counted only when two or more holders of open values hang on the same end;
//!   an end where each person runs their own project does not report
//! - **After the declaration is added, reconciliation works**: walking the timeline — three open facts close into one chain, and the predecessor closes at
//!   **the earliest** successor's start, regardless of ingestion order; running again changes nothing
//! - **No declaration, no change**: the reconciliation endpoint refuses, and the engine does not infer on anyone's behalf
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is unset. Creates and tears down its own.

use chrono::{DateTime, TimeZone, Utc};
use sqlx::PgPool;
use utopia_store::temporal;
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    /// relation, with no axioms declared at all
    leads: Uuid,
    /// attribute, likewise undeclared
    salary: Uuid,
    aurora: Uuid,
    zhang: Uuid,
    li: Uuid,
    zhou: Uuid,
    lin: Uuid,
}

fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let etype = Uuid::now_v7();
    let (leads, salary) = (Uuid::now_v7(), Uuid::now_v7());
    let (aurora, zhang, li, zhou, lin) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'late-declaration-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'late-declaration-test')",
    )
    .bind(ws)
    .bind(org)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'late-declaration-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'thing', 'Thing')",
    )
    .bind(etype)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'leads', 'leads')",
    )
    .bind(leads)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind, datatype, unit)
         VALUES ($1, $2, 'salary', 'salary', 'attribute', 'number', 'CNY')",
    )
    .bind(salary)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [
        (aurora, "Project Aurora"),
        (zhang, "Zhang San"),
        (li, "Li Si"),
        (zhou, "Zhou Qi"),
        (lin, "Lin Zhao"),
    ] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(etype)
        .bind(name)
        .execute(pool)
        .await?;
    }
    Ok(Fixture {
        org,
        kb,
        leads,
        salary,
        aurora,
        zhang,
        li,
        zhou,
        lin,
    })
}

/// An open relation fact, with a start by day.
async fn edge(
    pool: &PgPool,
    kb: Uuid,
    s: Uuid,
    p: Uuid,
    o: Uuid,
    from: DateTime<Utc>,
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, valid_from, valid_from_precision)
         VALUES ($1, $2, $3, $4, $5, $6, 'day')",
    )
    .bind(id)
    .bind(kb)
    .bind(s)
    .bind(p)
    .bind(o)
    .bind(from)
    .execute(pool)
    .await?;
    Ok(id)
}

/// An open attribute fact, its value via `object_value`.
async fn attr(
    pool: &PgPool,
    kb: Uuid,
    s: Uuid,
    p: Uuid,
    value: serde_json::Value,
    from: DateTime<Utc>,
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value, valid_from, valid_from_precision)
         VALUES ($1, $2, $3, $4, $5, $6, 'day')",
    )
    .bind(id)
    .bind(kb)
    .bind(s)
    .bind(p)
    .bind(value)
    .bind(from)
    .execute(pool)
    .await?;
    Ok(id)
}

/// A fact still alive on a predicate: (subject name, start, end), sorted by start.
async fn live(
    pool: &PgPool,
    kb: Uuid,
    p: Uuid,
) -> anyhow::Result<Vec<(String, Option<DateTime<Utc>>, Option<DateTime<Utc>>)>> {
    Ok(sqlx::query_as(
        "SELECT e.canonical_name, f.valid_from, f.valid_to
         FROM facts f JOIN entities e ON e.id = f.subject_id
         WHERE f.kb_id = $1 AND f.predicate_id = $2 AND f.invalidated_at IS NULL
         ORDER BY f.valid_from",
    )
    .bind(kb)
    .bind(p)
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
async fn a_succession_closes_once_someone_declares_it() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // Ingestion order is deliberately shuffled: 李四 arrives first, 周七 second, 张三 last — the timeline differs from the ingestion order
        edge(&pool, f.kb, f.li, f.leads, f.aurora, day(2024, 7, 5)).await?;
        edge(&pool, f.kb, f.zhou, f.leads, f.aurora, day(2025, 9, 1)).await?;
        edge(&pool, f.kb, f.zhang, f.leads, f.aurora, day(2023, 2, 1)).await?;
        attr(
            &pool,
            f.kb,
            f.lin,
            f.salary,
            serde_json::json!({ "value": 28000, "unit": "CNY" }),
            day(2023, 6, 1),
        )
        .await?;
        attr(
            &pool,
            f.kb,
            f.lin,
            f.salary,
            serde_json::json!({ "value": 32000, "unit": "CNY" }),
            day(2024, 2, 20),
        )
        .await?;

        // —— Candidates ——
        let cands = temporal::uniqueness_candidates(&pool, f.kb).await?;
        let on_leads: Vec<_> = cands.iter().filter(|c| c.predicate_id == f.leads).collect();
        assert_eq!(on_leads.len(), 1, "each person owns one project: subject side should not be reported");
        let leads = on_leads[0];
        assert_eq!(leads.side, "object");
        assert!(!leads.declared);
        assert_eq!((leads.holders, leads.open_facts), (1, 3));
        assert_eq!((leads.would_close, leads.would_review), (2, 0));
        let ex = &leads.examples[0];
        assert_eq!(ex.holder, "Project Aurora");
        assert_eq!(
            ex.values.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            ["Zhang San", "Li Si", "Zhou Qi"],
            "例子按年表排，不按摄入顺序"
        );
        let salary = cands
            .iter()
            .find(|c| c.predicate_id == f.salary)
            .expect("two open salaries are candidates on the subject side");
        assert_eq!(salary.side, "subject");
        assert_eq!((salary.holders, salary.open_facts, salary.would_close), (1, 2, 1));
        assert_eq!(salary.examples[0].values[0].name, "28000 CNY", "literal value carries a unit");

        // —— No declaration, no change ——
        assert!(
            temporal::reconcile_predicate(&pool, f.kb, f.leads).await.is_err(),
            "没有唯一性声明的谓词不能对账：引擎不替人推断"
        );
        assert_eq!(live(&pool, f.kb, f.leads).await?.len(), 3);

        // —— Declare the object side unique, then reconcile ——
        sqlx::query("UPDATE relation_types SET inverse_functional = TRUE WHERE id = $1")
            .bind(f.leads)
            .execute(&pool)
            .await?;
        let r = temporal::reconcile_predicate(&pool, f.kb, f.leads).await?;
        assert_eq!((r.corrected.len(), r.conflicts), (2, 0));
        assert_eq!(
            live(&pool, f.kb, f.leads).await?,
            vec![
                ("Zhang San".to_string(), Some(day(2023, 2, 1)), Some(day(2024, 7, 5))),
                ("Li Si".to_string(), Some(day(2024, 7, 5)), Some(day(2025, 9, 1))),
                ("Zhou Qi".to_string(), Some(day(2025, 9, 1)), None),
            ],
            "前任闭合在最早的后任起点上：张三止于李四，李四止于周七"
        );
        // The original rewritten row is still there; the record axis rewinds to show it
        let invalidated: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM facts WHERE kb_id = $1 AND predicate_id = $2 AND invalidated_at IS NOT NULL",
        )
        .bind(f.kb)
        .bind(f.leads)
        .fetch_one(&pool)
        .await?;
        assert_eq!(invalidated, 2);

        // Running again changes nothing; it is no longer among the candidates either
        let again = temporal::reconcile_predicate(&pool, f.kb, f.leads).await?;
        assert!(again.corrected.is_empty() && again.conflicts == 0);
        assert!(temporal::uniqueness_candidates(&pool, f.kb)
            .await?
            .iter()
            .all(|c| c.predicate_id != f.leads));

        // —— Attributes go through the subject side ——
        sqlx::query("UPDATE relation_types SET functional = TRUE WHERE id = $1")
            .bind(f.salary)
            .execute(&pool)
            .await?;
        let r = temporal::reconcile_predicate(&pool, f.kb, f.salary).await?;
        assert_eq!((r.corrected.len(), r.conflicts), (1, 0));
        let closed: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT valid_to FROM facts
             WHERE kb_id = $1 AND predicate_id = $2 AND invalidated_at IS NULL
               AND object_value->>'value' = '28000'",
        )
        .bind(f.kb)
        .bind(f.salary)
        .fetch_one(&pool)
        .await?;
        assert_eq!(closed, Some(day(2024, 2, 20)), "old salary ends where the new salary starts");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Delete the KB first, then the org: pending_facts.proposed_by and the like do not cascade to the org
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
