//! A typed row's attestation counts only the live statements in its source, taking the earliest one together with its name (0064 decision 5):
//! a voided statement does not attest, an entity source of a category-word rule has no attestation, and rows of another library are left alone.
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. It creates and tears down its own data and never touches an existing database.

use chrono::{DateTime, TimeZone, Utc};
use sqlx::PgPool;
use utopia_store::graph::{self, FactObject};
use utopia_store::materialize::sync_typed_attestation;
use uuid::Uuid;

type Anchor = (Option<DateTime<Utc>>, Option<String>);

struct Base {
    org: Uuid,
    kb: Uuid,
    subject: Uuid,
    predicate: Uuid,
}

fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
}

fn anchored(date: DateTime<Utc>, name: &str) -> Anchor {
    (Some(date), Some(name.to_string()))
}

async fn pool() -> anyhow::Result<Option<PgPool>> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(None);
    };
    Ok(Some(PgPool::connect(&url).await?))
}

async fn base(pool: &PgPool) -> anyhow::Result<Base> {
    let (org, ws, kb, subject, predicate) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let name = format!("typed-row-live-statements-{org}");
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','{name}');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','{name}');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','{name}');
         INSERT INTO entities(id,kb_id,canonical_name) VALUES ('{subject}','{kb}','码表');
         INSERT INTO relation_types(id,kb_id,key,label,kind,temporal,description)
             VALUES ('{predicate}','{kb}','sold_in','sold in','relation','state','');"
    ))
    .execute(pool)
    .await?;
    Ok(Base {
        org,
        kb,
        subject,
        predicate,
    })
}

async fn cleanup(pool: &PgPool, bases: &[&Base]) -> anyhow::Result<()> {
    for base in bases {
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(base.org)
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn entity(pool: &PgPool, base: &Base) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO entities(id,kb_id,canonical_name) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(base.kb)
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(id)
}

/// One open statement; `witness` is the date and name of the section it sits in, and if there is none, it was not stated
async fn statement(
    pool: &PgPool,
    base: &Base,
    witness: Option<(DateTime<Utc>, &str)>,
) -> anyhow::Result<Uuid> {
    let object = entity(pool, base).await?;
    let (id, _) = graph::insert_open_statement(
        pool,
        base.kb,
        base.subject,
        "sold in",
        FactObject::Entity(object),
        None,
        0.9,
    )
    .await?;
    if let Some((at, by)) = witness {
        graph::attest_statement(pool, id, at, by).await?;
    }
    Ok(id)
}

async fn invalidate(pool: &PgPool, fact: Uuid) -> anyhow::Result<()> {
    sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
        .bind(fact)
        .execute(pool)
        .await?;
    Ok(())
}

/// One typed fact row; `stale` is the attestation it currently records (the sync is going to change it)
async fn typed_row(
    pool: &PgPool,
    base: &Base,
    implied: bool,
    stale: Option<(DateTime<Utc>, &str)>,
) -> anyhow::Result<Uuid> {
    let (id, object) = (Uuid::now_v7(), entity(pool, base).await?);
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, predicate_id, layer, implied,
                            attested_from, attested_by)
         VALUES ($1, $2, $3, $4, $5, 'typed', $6, $7, $8)",
    )
    .bind(id)
    .bind(base.kb)
    .bind(base.subject)
    .bind(object)
    .bind(base.predicate)
    .bind(implied)
    .bind(stale.map(|(at, _)| at))
    .bind(stale.map(|(_, by)| by))
    .execute(pool)
    .await?;
    Ok(id)
}

async fn materialized_from(pool: &PgPool, base: &Base, sources: &[Uuid]) -> anyhow::Result<Uuid> {
    let row = typed_row(pool, base, false, None).await?;
    for source in sources {
        sqlx::query("INSERT INTO typed_fact_sources (fact_id, statement_id) VALUES ($1, $2)")
            .bind(row)
            .bind(source)
            .execute(pool)
            .await?;
    }
    Ok(row)
}

/// One approved rule: `trigger` is `phrase` or `kind_word`
async fn rule(pool: &PgPool, base: &Base, trigger: &str) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO implication_rules(id,kb_id,trigger,phrase,conclude_property_id,reading,status,decided_by)
         VALUES ($1,$2,$3,$4,$5,$6,'approved','person')",
    )
    .bind(id)
    .bind(base.kb)
    .bind(trigger)
    .bind(format!("rule {id}"))
    .bind(base.predicate)
    .bind((trigger == "kind_word").then_some("country_of_nationality"))
    .execute(pool)
    .await?;
    Ok(id)
}

async fn implied_from_statement(
    pool: &PgPool,
    base: &Base,
    row: Uuid,
    statement: Uuid,
) -> anyhow::Result<()> {
    let rule = rule(pool, base, "phrase").await?;
    sqlx::query(
        "INSERT INTO implied_fact_sources (fact_id, rule_id, statement_id) VALUES ($1, $2, $3)",
    )
    .bind(row)
    .bind(rule)
    .bind(statement)
    .execute(pool)
    .await?;
    Ok(())
}

async fn implied_from_entity(pool: &PgPool, base: &Base, row: Uuid) -> anyhow::Result<()> {
    let rule = rule(pool, base, "kind_word").await?;
    let source = entity(pool, base).await?;
    sqlx::query(
        "INSERT INTO implied_fact_sources (fact_id, rule_id, entity_id) VALUES ($1, $2, $3)",
    )
    .bind(row)
    .bind(rule)
    .bind(source)
    .execute(pool)
    .await?;
    Ok(())
}

async fn anchor(pool: &PgPool, fact: Uuid) -> anyhow::Result<Anchor> {
    Ok(
        sqlx::query_as("SELECT attested_from, attested_by FROM facts WHERE id = $1")
            .bind(fact)
            .fetch_one(pool)
            .await?,
    )
}

#[tokio::test]
async fn an_invalidated_source_statement_does_not_attest_its_typed_row() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = base(&pool).await?;
    let live = statement(&pool, &base, Some((day(2026, 9, 4), "live report"))).await?;
    let dead = statement(&pool, &base, Some((day(2015, 1, 1), "dead report"))).await?;
    invalidate(&pool, dead).await?;
    let row = materialized_from(&pool, &base, &[live, dead]).await?;
    sync_typed_attestation(&pool, base.kb, None).await?;
    let attested = anchor(&pool, row).await?;
    cleanup(&pool, &[&base]).await?;
    assert_eq!(attested, anchored(day(2026, 9, 4), "live report"));
    Ok(())
}

#[tokio::test]
async fn an_invalidated_statement_behind_an_implied_source_does_not_attest_its_row(
) -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = base(&pool).await?;
    let live = statement(&pool, &base, Some((day(2026, 9, 4), "live report"))).await?;
    let dead = statement(&pool, &base, Some((day(2015, 1, 1), "dead report"))).await?;
    invalidate(&pool, dead).await?;
    let row = typed_row(&pool, &base, true, None).await?;
    implied_from_statement(&pool, &base, row, live).await?;
    implied_from_statement(&pool, &base, row, dead).await?;
    sync_typed_attestation(&pool, base.kb, None).await?;
    let attested = anchor(&pool, row).await?;
    cleanup(&pool, &[&base]).await?;
    assert_eq!(attested, anchored(day(2026, 9, 4), "live report"));
    Ok(())
}

#[tokio::test]
async fn an_entity_implied_source_beside_a_statement_leaves_the_statements_attestation(
) -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = base(&pool).await?;
    let live = statement(&pool, &base, Some((day(2026, 9, 4), "live report"))).await?;
    let row = typed_row(&pool, &base, true, None).await?;
    implied_from_entity(&pool, &base, row).await?;
    implied_from_statement(&pool, &base, row, live).await?;
    sync_typed_attestation(&pool, base.kb, None).await?;
    let attested = anchor(&pool, row).await?;
    cleanup(&pool, &[&base]).await?;
    assert_eq!(attested, anchored(day(2026, 9, 4), "live report"));
    Ok(())
}

#[tokio::test]
async fn a_row_implied_only_from_an_entity_loses_its_stale_attestation() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = base(&pool).await?;
    let row = typed_row(&pool, &base, true, Some((day(2020, 1, 1), "stale report"))).await?;
    implied_from_entity(&pool, &base, row).await?;
    sync_typed_attestation(&pool, base.kb, None).await?;
    let attested = anchor(&pool, row).await?;
    cleanup(&pool, &[&base]).await?;
    assert_eq!(attested, (None, None));
    Ok(())
}

#[tokio::test]
async fn two_live_sources_attest_the_row_with_the_earliest_date_and_its_name() -> anyhow::Result<()>
{
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = base(&pool).await?;
    let later = statement(&pool, &base, Some((day(2026, 9, 4), "later report"))).await?;
    let earlier = statement(&pool, &base, Some((day(2015, 1, 1), "earlier report"))).await?;
    let row = materialized_from(&pool, &base, &[later, earlier]).await?;
    sync_typed_attestation(&pool, base.kb, None).await?;
    let attested = anchor(&pool, row).await?;
    cleanup(&pool, &[&base]).await?;
    assert_eq!(attested, anchored(day(2015, 1, 1), "earlier report"));
    Ok(())
}

#[tokio::test]
async fn a_sync_for_one_base_leaves_the_typed_rows_of_another_base_alone() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let (synced, other) = (base(&pool).await?, base(&pool).await?);
    let source = statement(&pool, &other, Some((day(2026, 9, 4), "other report"))).await?;
    let row = materialized_from(&pool, &other, &[source]).await?;
    sqlx::query("UPDATE facts SET attested_from = NULL, attested_by = NULL WHERE id = $1")
        .bind(row)
        .execute(&pool)
        .await?;
    sync_typed_attestation(&pool, synced.kb, None).await?;
    let attested = anchor(&pool, row).await?;
    cleanup(&pool, &[&synced, &other]).await?;
    assert_eq!(attested, (None, None));
    Ok(())
}

#[tokio::test]
async fn a_sync_for_one_base_leaves_the_implied_rows_of_another_base_alone() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let (synced, other) = (base(&pool).await?, base(&pool).await?);
    let source = statement(&pool, &other, Some((day(2026, 9, 4), "other report"))).await?;
    let row = typed_row(&pool, &other, true, None).await?;
    implied_from_statement(&pool, &other, row, source).await?;
    sync_typed_attestation(&pool, synced.kb, None).await?;
    let attested = anchor(&pool, row).await?;
    cleanup(&pool, &[&synced, &other]).await?;
    assert_eq!(attested, (None, None));
    Ok(())
}

/// Four stale rows of every kind the sync knows, with what the whole-base sync makes of each.
async fn four_stale_rows(pool: &PgPool, base: &Base) -> anyhow::Result<Vec<(Uuid, Anchor)>> {
    let live = statement(pool, base, Some((day(2026, 9, 4), "live report"))).await?;
    let earlier = statement(pool, base, Some((day(2024, 3, 1), "earlier report"))).await?;
    let dead = statement(pool, base, Some((day(2015, 1, 1), "dead report"))).await?;
    invalidate(pool, dead).await?;
    let unstated = statement(pool, base, None).await?;
    let materialized = materialized_from(pool, base, &[live, earlier, dead]).await?;
    let implied = typed_row(pool, base, true, Some((day(2030, 1, 1), "stale"))).await?;
    implied_from_statement(pool, base, implied, live).await?;
    implied_from_entity(pool, base, implied).await?;
    let from_the_dead = materialized_from(pool, base, &[dead]).await?;
    sqlx::query("UPDATE facts SET attested_from = $2, attested_by = 'stale' WHERE id = $1")
        .bind(from_the_dead)
        .bind(day(2030, 1, 1))
        .execute(pool)
        .await?;
    let from_the_unstated = materialized_from(pool, base, &[unstated]).await?;
    sqlx::query("UPDATE facts SET attested_from = $2, attested_by = 'stale' WHERE id = $1")
        .bind(from_the_unstated)
        .bind(day(2030, 1, 1))
        .execute(pool)
        .await?;
    Ok(vec![
        (materialized, anchored(day(2024, 3, 1), "earlier report")),
        (implied, anchored(day(2026, 9, 4), "live report")),
        (from_the_dead, (None, None)),
        (from_the_unstated, (None, None)),
    ])
}

#[tokio::test]
async fn a_sync_of_given_rows_attests_them_as_the_whole_base_sync_does() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let (whole, given) = (base(&pool).await?, base(&pool).await?);
    let by_whole_base = four_stale_rows(&pool, &whole).await?;
    let by_given_rows = four_stale_rows(&pool, &given).await?;
    let rows: Vec<Uuid> = by_given_rows.iter().map(|(row, _)| *row).collect();

    let mut moved_whole = sync_typed_attestation(&pool, whole.kb, None).await?;
    let mut moved_given = sync_typed_attestation(&pool, given.kb, Some(&rows)).await?;

    let mut expected = Vec::new();
    let mut attested = Vec::new();
    for ((whole_row, anchor_expected), (given_row, _)) in by_whole_base.iter().zip(&by_given_rows) {
        expected.push((anchor(&pool, *whole_row).await?, anchor_expected.clone()));
        attested.push(anchor(&pool, *given_row).await?);
    }
    cleanup(&pool, &[&whole, &given]).await?;
    for ((by_whole, wanted), by_given) in expected.iter().zip(&attested) {
        assert_eq!(by_whole, wanted, "the whole-base sync");
        assert_eq!(by_given, wanted, "the sync of given rows");
    }
    moved_whole.sort();
    moved_given.sort();
    let mut all_whole: Vec<Uuid> = by_whole_base.iter().map(|(row, _)| *row).collect();
    let mut all_given = rows;
    all_whole.sort();
    all_given.sort();
    assert_eq!(
        moved_whole, all_whole,
        "rows the whole-base sync reports as moved"
    );
    assert_eq!(
        moved_given, all_given,
        "rows the sync of given rows reports as moved"
    );
    Ok(())
}

#[tokio::test]
async fn a_sync_of_given_rows_leaves_the_rows_not_given_alone() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = base(&pool).await?;
    let source = statement(&pool, &base, Some((day(2026, 9, 4), "live report"))).await?;
    let given = materialized_from(&pool, &base, &[source]).await?;
    let not_given = materialized_from(&pool, &base, &[source]).await?;

    let moved = sync_typed_attestation(&pool, base.kb, Some(&[given])).await?;

    let (attested, left) = (anchor(&pool, given).await?, anchor(&pool, not_given).await?);
    cleanup(&pool, &[&base]).await?;
    assert_eq!(attested, anchored(day(2026, 9, 4), "live report"));
    assert_eq!(left, (None, None));
    assert_eq!(moved, vec![given]);
    Ok(())
}

#[tokio::test]
async fn a_sync_of_given_rows_leaves_a_given_row_of_another_base_alone() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let (synced, other) = (base(&pool).await?, base(&pool).await?);
    let source = statement(&pool, &other, Some((day(2026, 9, 4), "other report"))).await?;
    let row = materialized_from(&pool, &other, &[source]).await?;

    let moved = sync_typed_attestation(&pool, synced.kb, Some(&[row])).await?;

    let attested = anchor(&pool, row).await?;
    cleanup(&pool, &[&synced, &other]).await?;
    assert_eq!(attested, (None, None));
    assert_eq!(moved, Vec::<Uuid>::new());
    Ok(())
}

#[tokio::test]
async fn a_sync_following_statements_attests_the_rows_materialized_or_implied_from_them(
) -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = base(&pool).await?;
    let followed = statement(&pool, &base, Some((day(2026, 9, 4), "live report"))).await?;
    let other = statement(&pool, &base, Some((day(2024, 3, 1), "earlier report"))).await?;
    let materialized = materialized_from(&pool, &base, &[followed, other]).await?;
    let implied = typed_row(&pool, &base, true, None).await?;
    implied_from_statement(&pool, &base, implied, followed).await?;
    let of_the_other = materialized_from(&pool, &base, &[other]).await?;

    let mut moved =
        utopia_store::materialize::sync_typed_attestation_following(&pool, base.kb, &[followed])
            .await?;
    let none =
        utopia_store::materialize::sync_typed_attestation_following(&pool, base.kb, &[]).await?;

    let attested = (
        anchor(&pool, materialized).await?,
        anchor(&pool, implied).await?,
        anchor(&pool, of_the_other).await?,
    );
    cleanup(&pool, &[&base]).await?;
    moved.sort();
    let mut expected = vec![materialized, implied];
    expected.sort();
    assert_eq!(moved, expected);
    assert_eq!(none, Vec::<Uuid>::new());
    // the materialized row takes the earliest of all its sources, here the one that is not followed
    assert_eq!(attested.0, anchored(day(2024, 3, 1), "earlier report"));
    assert_eq!(attested.1, anchored(day(2026, 9, 4), "live report"));
    assert_eq!(attested.2, (None, None));
    Ok(())
}

/// A timeline correction puts a new row in place of a typed row and carries its source links over. A sync
/// following the statements finds the new row, because it resolves the rows in the same statement (RC-012).
#[tokio::test]
async fn a_sync_following_statements_reaches_a_row_put_in_place_of_a_followed_one(
) -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = base(&pool).await?;
    let source = statement(&pool, &base, Some((day(2026, 9, 4), "live report"))).await?;
    let replaced = materialized_from(&pool, &base, &[source]).await?;
    // what the correction leaves: the old row invalidated, a successor carrying its source link
    let successor = materialized_from(&pool, &base, &[source]).await?;
    invalidate(&pool, replaced).await?;

    let moved =
        utopia_store::materialize::sync_typed_attestation_following(&pool, base.kb, &[source])
            .await?;

    let attested = anchor(&pool, successor).await?;
    cleanup(&pool, &[&base]).await?;
    assert_eq!(moved, vec![successor]);
    assert_eq!(attested, anchored(day(2026, 9, 4), "live report"));
    Ok(())
}
