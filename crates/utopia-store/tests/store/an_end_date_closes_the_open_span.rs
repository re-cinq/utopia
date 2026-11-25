//! A span whose start was observed first, then a later document says it ended on some day: the ledger closes that span rather than creating a new row.
//!
//! After "2024-11-05 put on the blacklist" comes "2025-03-12 removed from the list": the removal has no start,
//! only an end; it states the end of the same span. The old row is voided, the correction row chains back, and attributes on the edge travel with the row (0037).

use sqlx::PgPool;
use uuid::Uuid;

type Stamp = chrono::DateTime<chrono::Utc>;
/// A live row: id, start, end, and which row it chains back to
type LiveRow = (Uuid, Option<Stamp>, Option<Stamp>, Option<Uuid>);

fn day(s: &str) -> Stamp {
    format!("{s}T00:00:00Z").parse().unwrap()
}

fn span(
    from: Option<&str>,
    to: Option<&str>,
    attested: &str,
) -> utopia_store::graph::Validity<'static> {
    utopia_store::graph::Validity {
        from: from.map(day),
        from_precision: from.map(|_| "day"),
        to: to.map(day),
        to_precision: to.map(|_| "day"),
        attested_at: Some(day(attested)),
        from_grade: None,
        ..Default::default()
    }
}

async fn live(
    pool: &PgPool,
    kb: Uuid,
    pred: Uuid,
    object: Uuid,
) -> Result<Vec<LiveRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, valid_from, valid_to, supersedes FROM facts
         WHERE kb_id = $1 AND predicate_id = $2 AND object_id = $3 AND invalidated_at IS NULL",
    )
    .bind(kb)
    .bind(pred)
    .bind(object)
    .fetch_all(pool)
    .await
}

fn ends(to: &str, attested: &str) -> utopia_store::graph::Validity<'static> {
    utopia_store::graph::Validity {
        from: None,
        from_precision: None,
        to: Some(day(to)),
        to_precision: Some("day"),
        attested_at: Some(day(attested)),
        from_grade: None,
        ..Default::default()
    }
}

#[tokio::test]
async fn an_end_date_closes_the_open_span() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let tag = Uuid::now_v7();
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(format!("span-{tag}"))
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(org)
        .bind(format!("span-{tag}"))
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3)")
        .bind(kb)
        .bind(ws)
        .bind(format!("span-{tag}"))
        .execute(&pool)
        .await?;
    let class = Uuid::now_v7();
    sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
        .bind(class)
        .bind(kb)
        .bind("org")
        .bind("Org")
        .execute(&pool)
        .await?;
    let title = utopia_store::ontology::create_relation_type(
        &pool,
        kb,
        "title",
        "title",
        "state",
        Default::default(),
        "",
        "attribute",
        &[class],
        &[],
        Some("text"),
        None,
    )
    .await?;
    let works_at = utopia_store::ontology::create_relation_type(
        &pool,
        kb,
        "works_at",
        "works at",
        "state",
        Default::default(),
        "",
        "relation",
        &[],
        &[],
        None,
        None,
    )
    .await?;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    for (id, name) in [(a, "李文博"), (b, "澜图科技")] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(class)
        .bind(format!("{name}-{tag}"))
        .execute(&pool)
        .await?;
    }

    // First document: serving as director from 2020-01-10
    let (first, created) = utopia_store::graph::insert_fact(
        &pool,
        kb,
        a,
        Some(works_at),
        b,
        utopia_store::graph::Validity {
            from: Some(day("2020-01-10")),
            from_precision: Some("day"),
            to: None,
            to_precision: None,
            attested_at: Some(day("2025-08-01")),
            from_grade: None,
            ..Default::default()
        },
        0.9,
    )
    .await?;
    assert!(created);
    let value = serde_json::json!({ "value": "董事" });
    utopia_store::graph::upsert_fact_qualifier(&pool, first, title, &value).await?;

    // Second document: resigned as director on 2024-04-30: no start, only an end
    let (closed, created) = utopia_store::graph::insert_fact(
        &pool,
        kb,
        a,
        Some(works_at),
        b,
        ends("2024-04-30", "2024-04-18"),
        0.9,
    )
    .await?;
    assert!(created, "关上一段是一次改写，不是复用");
    assert_ne!(closed, first);

    let live: Vec<LiveRow> = sqlx::query_as(
        "SELECT id, valid_from, valid_to, supersedes FROM facts
         WHERE kb_id = $1 AND predicate_id = $2 AND invalidated_at IS NULL",
    )
    .bind(kb)
    .bind(works_at)
    .fetch_all(&pool)
    .await?;
    assert_eq!(live.len(), 1, "只剩一条活着的：那一段，关上了");
    let (id, from, to, supersedes) = live[0];
    assert_eq!(id, closed);
    assert_eq!(from, Some(day("2020-01-10")), "起点照旧");
    assert_eq!(to, Some(day("2024-04-30")), "终点是离任那天");
    assert_eq!(supersedes, Some(first), "修正行链回旧行");
    let old_dead: bool =
        sqlx::query_scalar("SELECT invalidated_at IS NOT NULL FROM facts WHERE id = $1")
            .bind(first)
            .fetch_one(&pool)
            .await?;
    assert!(old_dead, "旧行在认知轴上作废，不删");
    let quals = utopia_store::graph::fact_qualifiers_for(&pool, &[closed]).await?;
    assert_eq!(quals[&closed].len(), 1, "职务随修正行");
    assert_eq!(quals[&closed][0].value, Some(value));

    // A third document states the same departure day: the same event, so reuse that row
    let (again, created) = utopia_store::graph::insert_fact(
        &pool,
        kb,
        a,
        Some(works_at),
        b,
        ends("2024-04-30", "2024-05-02"),
        0.9,
    )
    .await?;
    assert_eq!(again, closed);
    assert!(!created);

    // The penalty decision mentions "director Li Wenbo" with no date; the document date falls within the tenure: this speaks of this span
    let (mentioned, created) = utopia_store::graph::insert_fact(
        &pool,
        kb,
        a,
        Some(works_at),
        b,
        span(None, None, "2024-04-17"),
        0.9,
    )
    .await?;
    assert_eq!(mentioned, closed, "任期内的无日期提及并进关上的那一段");
    assert!(!created);
    // A mention with no date after the tenure: could be a re-appointment, so a separate row, left for a person to rule
    let (later, created) = utopia_store::graph::insert_fact(
        &pool,
        kb,
        a,
        Some(works_at),
        b,
        span(None, None, "2025-06-01"),
        0.9,
    )
    .await?;
    assert!(created);
    assert_ne!(later, closed);

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    Ok(())
}

/// Three ways the same span arrives, and only one live row remains:
/// the start comes first and a later document states the whole span; or the end comes first (the document that says it ended is stored first during parallel extraction) and the start arrives later.
#[tokio::test]
async fn a_span_told_in_two_halves_is_one_row() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let tag = Uuid::now_v7();
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(format!("halves-{tag}"))
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(org)
        .bind(format!("halves-{tag}"))
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3)")
        .bind(kb)
        .bind(ws)
        .bind(format!("halves-{tag}"))
        .execute(&pool)
        .await?;
    let class = Uuid::now_v7();
    sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
        .bind(class)
        .bind(kb)
        .bind("org")
        .bind("Org")
        .execute(&pool)
        .await?;
    let listed = utopia_store::ontology::create_relation_type(
        &pool,
        kb,
        "listed",
        "listed",
        "state",
        Default::default(),
        "",
        "relation",
        &[],
        &[],
        None,
        None,
    )
    .await?;
    let mut ids = Vec::new();
    for name in ["法院", "澜图新材料", "第二家"] {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(class)
        .bind(format!("{name}-{tag}"))
        .execute(&pool)
        .await?;
        ids.push(id);
    }
    let (court, a, b) = (ids[0], ids[1], ids[2]);

    // Start first, a later document states the whole span (same start + end) → closed, no new row
    let (first, _) = utopia_store::graph::insert_fact(
        &pool,
        kb,
        court,
        Some(listed),
        a,
        span(Some("2024-11-05"), None, "2024-11-07"),
        0.9,
    )
    .await?;
    let (closed, created) = utopia_store::graph::insert_fact(
        &pool,
        kb,
        court,
        Some(listed),
        a,
        span(Some("2024-11-05"), Some("2025-03-12"), "2025-03-14"),
        0.9,
    )
    .await?;
    assert!(created);
    let rows = live(&pool, kb, listed, a).await?;
    assert_eq!(rows.len(), 1, "同起点带终点：关上原来那行");
    assert_eq!(rows[0].0, closed);
    assert_eq!(rows[0].2, Some(day("2025-03-12")));
    assert_eq!(rows[0].3, Some(first));

    // End first (the document that says it ended is stored first), start arrives later → one row, with both ends present
    let (end_only, _) = utopia_store::graph::insert_fact(
        &pool,
        kb,
        court,
        Some(listed),
        b,
        ends("2025-03-12", "2025-03-14"),
        0.9,
    )
    .await?;
    let (merged, created) = utopia_store::graph::insert_fact(
        &pool,
        kb,
        court,
        Some(listed),
        b,
        span(Some("2024-11-05"), None, "2024-11-07"),
        0.9,
    )
    .await?;
    assert!(created);
    let rows = live(&pool, kb, listed, b).await?;
    assert_eq!(rows.len(), 1, "起点后到：并进只知道终点的那行");
    assert_eq!(rows[0].0, merged);
    assert_eq!(rows[0].1, Some(day("2024-11-05")), "起点是后到的");
    assert_eq!(rows[0].2, Some(day("2025-03-12")), "终点沿用先到的");
    assert_eq!(rows[0].3, Some(end_only));

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    Ok(())
}
