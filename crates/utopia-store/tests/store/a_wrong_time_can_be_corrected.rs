//! Manually correcting the validity interval of a fact (302), run against a real store.
//!
//! Extraction read "first half of 2023" as 1 January, and before this one could only delete the document and extract it again.
//! All the risk of this path is that **it looks like a single UPDATE**: change a date, and the graph is right,
//! but nobody can see what the ledger lost. So the four things pinned here are ones `cargo check` cannot see at all:
//!
//! - The old row stays in the ledger with its void time recorded, and the correction row links back to it through `supersedes` — editing in place
//!   would make the change itself vanish, and that is exactly what the record axis must replay (0019)
//! - Evidence is copied along with the correction row. Without that step, a fact whose time was just edited instantly becomes "no one states it",
//!   and the stale check would grey it out
//! - After the start moves, reconcile again: having moved a successor's start date, the uniqueness invariant sees this collision
//!   for the first time
//! - A row already voided cannot be changed: it returns None rather than conjuring a correction hung off a dead row
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use sqlx::PgPool;
use utopia_store::graph::Validity;
use uuid::Uuid;

struct Fixture {
    kb: Uuid,
    zhang: Uuid,
    li: Uuid,
    project: Uuid,
    leads: Uuid,
    chunk: Uuid,
    doc: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let etype = Uuid::now_v7();
    let leads = Uuid::now_v7();
    let (zhang, li, project) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (src, doc, chunk) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'time-edit-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'time-edit-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'time-edit-test')",
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
    // inverse_functional: only one person leads a project at a time — the uniqueness on the object side,
    // which is the invariant a moved start date must collide with
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, temporal, inverse_functional)
         VALUES ($1, $2, 'leads', 'leads', 'state', TRUE)",
    )
    .bind(leads)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [(zhang, "Zhang San"), (li, "Li Si"), (project, "Phoenix")] {
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
    sqlx::query("INSERT INTO sources (id, kb_id, name) VALUES ($1, $2, 'time-edit-test')")
        .bind(src)
        .bind(kb)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status)
         VALUES ($1, $2, $3, 'memo.md', 'timeedit', 'ready')",
    )
    .bind(doc)
    .bind(kb)
    .bind(src)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text)
         VALUES ($1, $2, $3, 0, 'Zhang San led Phoenix in the first half of 2023.')",
    )
    .bind(chunk)
    .bind(kb)
    .bind(doc)
    .execute(pool)
    .await?;
    Ok(Fixture {
        kb,
        zhang,
        li,
        project,
        leads,
        chunk,
        doc,
    })
}

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

/// An open fact with evidence.
async fn assert_fact(
    pool: &PgPool,
    f: &Fixture,
    subject: Uuid,
    from: &str,
    precision: &str,
) -> anyhow::Result<Uuid> {
    let (id, _) = utopia_store::graph::insert_fact(
        pool,
        f.kb,
        subject,
        Some(f.leads),
        f.project,
        Validity::starting(Some(t(from)), Some(precision)),
        0.9,
    )
    .await?;
    sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, doc_version)
         VALUES ($1, $2, 'led Phoenix', $3, 1)",
    )
    .bind(id)
    .bind(f.chunk)
    .bind(f.doc)
    .execute(pool)
    .await?;
    Ok(id)
}

#[derive(sqlx::FromRow)]
struct Row {
    valid_from: Option<chrono::DateTime<chrono::Utc>>,
    valid_from_precision: Option<String>,
    valid_to: Option<chrono::DateTime<chrono::Utc>>,
    valid_to_precision: Option<String>,
    invalidated_at: Option<chrono::DateTime<chrono::Utc>>,
    supersedes: Option<Uuid>,
}

async fn row(pool: &PgPool, id: Uuid) -> anyhow::Result<Row> {
    Ok(sqlx::query_as(
        "SELECT valid_from, valid_from_precision, valid_to, valid_to_precision,
                invalidated_at, supersedes
         FROM facts WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await?)
}

async fn evidence_count(pool: &PgPool, id: Uuid) -> anyhow::Result<i64> {
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM fact_evidence WHERE fact_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(n)
}

/// Change a date, and the ledger must keep the trace of the change: the old row is voided but not deleted, the correction row links back to it, and evidence goes with it.
#[tokio::test]
async fn correcting_a_date_leaves_the_old_reading_in_the_ledger() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // Extraction read it as 1 January, to the day — the source actually said only "first half"
        let wrong = assert_fact(&pool, &f, f.zhang, "2023-01-01T00:00:00Z", "day").await?;

        let corrected = utopia_store::temporal::correct_interval(
            &pool,
            wrong,
            Validity::starting(Some(t("2023-06-01T00:00:00Z")), Some("month")),
        )
        .await?
        .expect("this one is still live and should be correctable");

        let old = row(&pool, wrong).await?;
        assert!(
            old.invalidated_at.is_some(),
            "旧行要记下何时被推翻——原地 UPDATE 会让这次修改自己消失"
        );
        assert_eq!(
            old.valid_from,
            Some(t("2023-01-01T00:00:00Z")),
            "旧行的世界区间一个字都不该动：它记录的是我们当时读成了什么"
        );

        let new = row(&pool, corrected).await?;
        assert_eq!(
            new.supersedes,
            Some(wrong),
            "the correction row must link back to the one it replaces"
        );
        assert!(
            new.invalidated_at.is_none(),
            "the correction row is the current one"
        );
        assert_eq!(new.valid_from, Some(t("2023-06-01T00:00:00Z")));
        assert_eq!(
            new.valid_from_precision.as_deref(),
            Some("month"),
            "精度跟着值一起改——写到月就是月，不该继承旧行那个「日」"
        );

        assert_eq!(
            evidence_count(&pool, corrected).await?,
            1,
            "证据要随修正行复制。少了它，这条事实立刻变成「无人陈述」被扫成灰的"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    run
}

/// All three states of the end must be editable, especially **reopening a closed interval** —
/// that is the only way out of "we wrongly judged it ended", and it looks like "filled nothing in".
#[tokio::test]
async fn an_end_that_was_never_there_can_be_taken_back() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let fact = assert_fact(&pool, &f, f.zhang, "2023-01-01T00:00:00Z", "day").await?;
        // First close to 2024
        let closed = utopia_store::temporal::correct_interval(
            &pool,
            fact,
            Validity {
                from: Some(t("2023-01-01T00:00:00Z")),
                from_precision: Some("day"),
                to: Some(t("2024-01-01T00:00:00Z")),
                to_precision: Some("year"),
                attested_at: None,
                from_grade: None,
                ..Default::default()
            },
        )
        .await?
        .expect("can still be corrected");
        assert_eq!(
            row(&pool, closed).await?.valid_to,
            Some(t("2024-01-01T00:00:00Z"))
        );

        // Judged wrongly: it did not end. Take back the whole end
        let reopened = utopia_store::temporal::correct_interval(
            &pool,
            closed,
            Validity::starting(Some(t("2023-01-01T00:00:00Z")), Some("day")),
        )
        .await?
        .expect("can still be corrected");
        let r = row(&pool, reopened).await?;
        assert!(r.valid_to.is_none(), "the end side was pulled back");
        assert!(
            r.valid_to_precision.is_none(),
            "精度也要一起收回：留着 'year' 而日期为空，CHECK 会拦，而三值逻辑下这种组合曾经静默放行"
        );
        assert_eq!(
            r.supersedes,
            Some(closed),
            "two corrections form a chain, not separate attachments"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    run
}

/// After changing the start, reconcile again. **This is the half most easily missed in this slice**: the interval is right, and the edge on the graph
/// looks right too, but nobody recomputes its relation with the successor.
///
/// The two people here swap identities before and after the edit: before the change, Zhang San took office in 2025 and was Li Si's successor;
/// once changed back to 2023 he becomes the predecessor, and should be closed at Li Si's start date. The uniqueness invariant sees this
/// for the first time only in this reconciliation.
#[tokio::test]
async fn moving_a_start_earlier_makes_it_the_predecessor() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // Both open: Li Si took over 2024-07, and Zhang San was extracted as 2025-01 (wrong,
        // he was in office already in 2023). At this point the invariant is already violated, only no one has computed it yet
        let li_fact = assert_fact(&pool, &f, f.li, "2024-07-01T00:00:00Z", "month").await?;
        let zhang_fact = assert_fact(&pool, &f, f.zhang, "2025-01-01T00:00:00Z", "month").await?;

        // Change Zhang San back to the real 2023
        let fixed = utopia_store::temporal::correct_interval(
            &pool,
            zhang_fact,
            Validity::starting(Some(t("2023-01-01T00:00:00Z")), Some("month")),
        )
        .await?
        .expect("can still be corrected");
        let report = utopia_store::temporal::reconcile_moved_facts(&pool, f.kb, &[fixed]).await?;

        assert!(
            !report.corrected.is_empty(),
            "对账要动手：张三 2023 起、李四 2024-07 接任，两条不能都挂着开放区间"
        );
        // The new fact starts earlier = it is the predecessor, closed at the old fact's start (the engine's criterion)
        assert!(
            row(&pool, fixed).await?.invalidated_at.is_some(),
            "被闭合的是张三自己那条，不是李四——早的那个是前任"
        );
        let current: Row = sqlx::query_as(
            "SELECT valid_from, valid_from_precision, valid_to, valid_to_precision,
                    invalidated_at, supersedes
             FROM facts WHERE supersedes = $1",
        )
        .bind(fixed)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            current.valid_to,
            Some(t("2024-07-01T00:00:00Z")),
            "张三的区间闭合在李四的上任日"
        );
        assert_eq!(
            current.valid_from,
            Some(t("2023-01-01T00:00:00Z")),
            "起点保持修正后的值"
        );
        assert!(
            row(&pool, li_fact).await?.invalidated_at.is_none(),
            "李四那条一动不动：他是现任，本来就该开放"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    run
}

/// A voided row cannot be changed. Without this gate, two people editing at once would produce two corrections each hung off a dead row,
/// adding a spurious edge to the graph.
#[tokio::test]
async fn a_row_that_is_already_gone_cannot_be_corrected() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let fact = assert_fact(&pool, &f, f.zhang, "2023-01-01T00:00:00Z", "day").await?;
        let first = utopia_store::temporal::correct_interval(
            &pool,
            fact,
            Validity::starting(Some(t("2023-06-01T00:00:00Z")), Some("month")),
        )
        .await?;
        assert!(first.is_some());

        // Edited again with the same (already voided) id the second time
        let second = utopia_store::temporal::correct_interval(
            &pool,
            fact,
            Validity::starting(Some(t("2022-01-01T00:00:00Z")), Some("year")),
        )
        .await?;
        assert!(
            second.is_none(),
            "作废行改不动，要让调用方知道没动手——而不是插一条挂在死行后面的修正"
        );
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM facts WHERE supersedes = $1")
            .bind(fact)
            .fetch_one(&pool)
            .await?;
        assert_eq!(n, 1, "a dead row should have only one successor");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    run
}
