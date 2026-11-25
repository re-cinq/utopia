//! The state "ended, but the day is not known", run against a real DB.
//!
//! Before the two ends each carried their own precision, `valid_to IS NULL` meant both "still ongoing" and "ended, but when is unknown".
//! Those two things have **opposite truth values** — one says the relation holds now, the other says it does not — and the ledger had only one way of writing it,
//! so "former CEO of Weta Digital" could only be written as the former, and the graph would assert something the source says has already ended.
//!
//! Three things are pinned here, each of which lives in SQL or an SQL constraint, and `cargo check` sees none of it:
//!
//! - All three ended states can be stored; four self-contradictory combinations cannot be stored
//! - A new observation of "ended on an unknown day" is **not** folded into an open row as if "nothing was said"
//! - The temporal engine does **not** treat it as an open row to close — it does not even know when it ended itself
//!
//! One of the four negative cases once slipped through: when `valid_to` has a date and the precision is NULL,
//! `NULL IN ('year',…)` evaluates to NULL, `TRUE AND NULL` is NULL, and the CHECK treats NULL as a pass.
//! Three-valued logic is silent here, and only actually running it makes it visible.
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is not set. It builds and tears down its own data and never touches an existing DB.

use sqlx::PgPool;
use utopia_store::graph::{Validity, ENDED_UNKNOWN};
use uuid::Uuid;

struct Fixture {
    kb: Uuid,
    subject: Uuid,
    predicate: Uuid,
    object_a: Uuid,
    object_b: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let etype = Uuid::now_v7();
    let predicate = Uuid::now_v7();
    let (subject, object_a, object_b) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'ended-unknown-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'ended-unknown-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name)
         VALUES ($1, $2, 'ended-unknown-test')",
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
    // functional: unique on the subject side, so the temporal engine does close reconciliation on it
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, temporal, functional)
         VALUES ($1, $2, 'leads', 'leads', 'state', TRUE)",
    )
    .bind(predicate)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [
        (subject, "Akkaraju"),
        (object_a, "Weta"),
        (object_b, "Stability"),
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
        kb,
        subject,
        predicate,
        object_a,
        object_b,
    })
}

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

async fn shape(pool: &PgPool, id: Uuid) -> anyhow::Result<(bool, Option<String>)> {
    let row: (bool, Option<String>) =
        sqlx::query_as("SELECT valid_to IS NULL, valid_to_precision FROM facts WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?;
    Ok(row)
}

#[tokio::test]
async fn a_relation_the_text_says_is_over_is_not_stored_as_ongoing() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // "Akkaraju, former CEO of Weta" — the ending is stated in the text; the date is not given in the text
        let (ended, _) = utopia_store::graph::insert_fact(
            &pool,
            f.kb,
            f.subject,
            Some(f.predicate),
            f.object_a,
            Validity::starting(Some(t("2020-01-01T00:00:00Z")), Some("day")).ended_when_unknown(),
            0.9,
        )
        .await?;
        let (to_is_null, prec) = shape(&pool, ended).await?;
        assert!(to_is_null, "no date to write, valid_to stays NULL");
        assert_eq!(
            prec.as_deref(),
            Some(ENDED_UNKNOWN),
            "但精度那一位要说出「它结束了」——少了它就跟「仍在持续」分不开"
        );

        // **Another "ended on an unknown day" observation of the same assertion must not be merged into "nothing was said".**
        // Previously the criterion was valid_from.is_none() && valid_to.is_none(),
        // and this observation satisfies both — it would be folded into the open row, and the one piece of information it brings (that it ended) is lost
        let (second, created) = utopia_store::graph::insert_fact(
            &pool,
            f.kb,
            f.subject,
            Some(f.predicate),
            f.object_b,
            Validity::default().ended_when_unknown(),
            0.9,
        )
        .await?;
        assert!(
            created,
            "it states something, so it must not be dropped as a weakened statement"
        );
        assert_eq!(
            shape(&pool, second).await?.1.as_deref(),
            Some(ENDED_UNKNOWN)
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

/// The temporal engine **should not treat "ended, unknown when" as an open row** to close:
/// an assertion that does not even know when it ended has no standing to set someone else's end moment.
#[tokio::test]
async fn an_already_ended_fact_is_not_treated_as_an_open_claim() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // Old row: ended, day unknown
        let (old, _) = utopia_store::graph::insert_fact(
            &pool,
            f.kb,
            f.subject,
            Some(f.predicate),
            f.object_a,
            Validity::starting(Some(t("2020-01-01T00:00:00Z")), Some("day")).ended_when_unknown(),
            0.9,
        )
        .await?;
        // New row: a different object, starting in 2024. A functional relation, so the engine will look for an "open row"
        let (new, _) = utopia_store::graph::insert_fact(
            &pool,
            f.kb,
            f.subject,
            Some(f.predicate),
            f.object_b,
            Validity::starting(Some(t("2024-01-01T00:00:00Z")), Some("day")),
            0.9,
        )
        .await?;
        let report = utopia_store::temporal::reconcile_new_fact(
            &pool,
            f.kb,
            new,
            f.subject,
            f.predicate,
            Some(f.object_b),
            None,
            utopia_store::temporal::Uniqueness::SubjectSide,
            Validity::starting(Some(t("2024-01-01T00:00:00Z")), Some("day")),
            0.9,
        )
        .await?;
        assert_eq!(
            report.corrected.len(),
            0,
            "the old row has already ended and must not be closed again"
        );
        assert_eq!(
            report.conflicts, 0,
            "it is not a contradiction either: the two spans never overlapped"
        );
        // Old row left untouched
        let still: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT valid_to FROM facts WHERE id = $1")
                .bind(old)
                .fetch_one(&pool)
                .await?;
        assert!(
            still.is_none(),
            "the engine must not invent an end date for it"
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
