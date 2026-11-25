//! A verdict records "why" (0026), run against a real DB.
//!
//! Previously `resolution_reviews` recorded status / decided_by / decided_at, and `reason` recorded
//! why **this pair was queued**. The sentence a person had in mind when deciding had nowhere to go, so precedents had only outcomes:
//! one wrong merge was read as "this kind gets merged", laundering the mistake into policy.
//!
//! Three things can each break in a different way:
//! - **Storage**: the reason is written onto this row; blank or whitespace-only counts as not written (a run of spaces is not stored)
//! - **Precedents carry the reason**: governance and the adjudicator read precedents from the audit ledger, not from this table — so the reason
//!   must also go into `detail.why`, for `precedents_for` to read it and `render_lines` to print it
//! - **Old rows do not blow up**: decisions made before 0026 have no why, so `Precedent.why` is None and rendering proceeds as before

use sqlx::PgPool;
use utopia_core::models::{ReviewItem, ReviewSide};
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    user: Uuid,
    a: Uuid,
    b: Uuid,
    review: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (user, person) = (Uuid::now_v7(), Uuid::now_v7());
    let (a, b, review) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'why-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'why-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'why-test')")
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO users (id, org_id, email, display_name, password_hash)
         VALUES ($1, $2, $3, 'Decider', 'x')",
    )
    .bind(user)
    .bind(org)
    .bind(format!("decider-{}@test.local", user.simple()))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'company', 'Company')",
    )
    .bind(person)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [(a, "Acme"), (b, "Acme Corp")] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(person)
        .bind(name)
        .execute(pool)
        .await?;
    }
    sqlx::query(
        "INSERT INTO resolution_reviews (id, kb_id, left_id, right_id, score, reason, stage)
         VALUES ($1, $2, $3, $4, 0.6, 'contains', 'human')",
    )
    .bind(review)
    .bind(kb)
    .bind(a)
    .bind(b)
    .execute(pool)
    .await?;
    Ok(Fixture {
        org,
        kb,
        user,
        a,
        b,
        review,
    })
}

fn item(f: &Fixture) -> ReviewItem {
    let side = |id: Uuid, name: &str| ReviewSide {
        id,
        name: name.into(),
        type_label: Some("Company".into()),
        color: "#888888".into(),
        degree: 0,
        disambiguator: None,
        top_facts: vec![],
    };
    ReviewItem {
        id: f.review,
        left: side(f.a, "Acme"),
        right: side(f.b, "Acme Corp"),
        score: 0.6,
        reason: Some("contains".into()),
        stage: "human".into(),
        created_at: chrono::Utc::now(),
        proposal: None,
    }
}

#[tokio::test]
async fn a_decision_keeps_its_reason_and_the_precedent_carries_it() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    // A person separated them, and said why
    utopia_store::resolution::decide_review(
        &pool,
        f.kb,
        f.review,
        "keep",
        f.user,
        Some("  Acme Corp is the parent company; Acme is the product line  "),
    )
    .await?;
    let stored: (String, Option<String>) =
        sqlx::query_as("SELECT status, rationale FROM resolution_reviews WHERE id = $1")
            .bind(f.review)
            .fetch_one(&pool)
            .await?;
    assert_eq!(stored.0, "kept");
    assert_eq!(
        stored.1.as_deref(),
        Some("Acme Corp is the parent company; Acme is the product line"),
        "理由落在这一行上，两头的空格去掉"
    );

    // Precedents go through the audit: the route writes why into detail, so here we record an entry the same way the route does
    utopia_store::audit::record(
        &pool,
        Some(f.kb),
        f.user,
        "review.keep",
        "review",
        Some(f.review),
        serde_json::json!({
            "left": "Acme", "right": "Acme Corp", "score": 0.6,
            "why": "Acme Corp is the parent company; Acme is the product line",
        }),
    )
    .await?;
    let p = utopia_store::governance::precedents_for(&pool, f.kb, &item(&f)).await?;
    assert_eq!(p.same_pair.len(), 1, "one precedent for the same pair");
    assert_eq!(
        p.same_pair[0].why.as_deref(),
        Some("Acme Corp is the parent company; Acme is the product line"),
        "先例带着人写的理由"
    );
    let lines = utopia_store::governance::render_lines(&p);
    assert!(
        lines.iter().any(
            |l| l.contains("kept apart") && l.contains("they wrote: \"Acme Corp is the parent")
        ),
        "提示词里那一行要引用理由：{lines:?}"
    );

    // Old rows: a decision with no why is still a precedent, just without a reason
    utopia_store::audit::record(
        &pool,
        Some(f.kb),
        f.user,
        "review.keep",
        "review",
        Some(f.review),
        serde_json::json!({ "left": "Acme", "right": "Acme Corp", "score": 0.6 }),
    )
    .await?;
    let p = utopia_store::governance::precedents_for(&pool, f.kb, &item(&f)).await?;
    assert_eq!(p.same_pair.len(), 2);
    assert!(
        p.same_pair.iter().any(|x| x.why.is_none()),
        "0026 之前的行没有理由，读出来是 None 而不是报错"
    );
    let lines = utopia_store::governance::render_lines(&p);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("kept apart") && !l.contains("they wrote")),
        "没理由的那条不该凭空长出一句引文：{lines:?}"
    );

    // An empty reason is not stored
    let review2 = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO resolution_reviews (id, kb_id, left_id, right_id, score, reason, stage)
         VALUES ($1, $2, $3, $4, 0.6, 'contains', 'human')",
    )
    .bind(review2)
    .bind(f.kb)
    .bind(f.a)
    .bind(f.b)
    .execute(&pool)
    .await?;
    utopia_store::resolution::decide_review(&pool, f.kb, review2, "keep", f.user, Some("   "))
        .await?;
    let blank: Option<String> =
        sqlx::query_scalar("SELECT rationale FROM resolution_reviews WHERE id = $1")
            .bind(review2)
            .fetch_one(&pool)
            .await?;
    assert!(blank.is_none(), "whitespace only counts as not written");

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(f.user)
        .execute(&pool)
        .await?;
    let gone = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    assert_eq!(gone.rows_affected(), 1, "one-off org was not deleted");
    Ok(())
}
