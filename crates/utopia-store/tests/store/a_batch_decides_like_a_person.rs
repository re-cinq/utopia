//! Batch judgment and filtering duplicates by type (#428), run against a real store.
//!
//! Behaviour to pin down:
//!
//! - **Filtering has three tiers by type**: all / both sides the same type (both have a type and equal) / the two sides' types conflict
//!   (both have a type and differ). A side with no type falls into none of the tiers — an unknown cannot be treated as the same,
//!   nor as different
//! - **Counts and filters use the same basis**: the two numbers in the left column are how many fall in each of these tiers
//! - **A batch = person judgments one by one**: each one goes through `decide_review`; merge direction, status,
//!   decided_by are exactly the same as for a single one; one failure (missing, already judged) does not drag down the rest,
//!   and the result explains each one; an action it does not recognize rejects the whole batch
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. It sets up and tears down its own data, and never touches an existing store.

use sqlx::PgPool;
use utopia_store::resolution::TypeFilter;
use uuid::Uuid;

struct Seed {
    kb: Uuid,
    org: Uuid,
    /// Same name, same class (two Person both named Zhang Wei)
    same_a: Uuid,
    /// Same name, class conflict (Apple's Organization and Apple's Person)
    conflict: Uuid,
    /// Same name, same class (two Person both named Li Si)
    same_b: Uuid,
    /// One side has no type: falls into no tier
    untyped: Uuid,
    pair_a: (Uuid, Uuid),
    /// The person deciding: entity_merges.merged_by and resolution_reviews.decided_by both point to users
    user: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Seed> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (person, company) = (Uuid::now_v7(), Uuid::now_v7());
    let user = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'batch-decide-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO users (id, org_id, email, display_name, password_hash)
         VALUES ($1, $2, $3, 'Batch Tester', 'x')",
    )
    .bind(user)
    .bind(org)
    .bind(format!("batch-{}@test.local", user.simple()))
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'batch-decide-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'batch-decide-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, key, label) in [
        (person, "person", "Person"),
        (company, "org", "Organization"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    let entity = |type_id: Option<Uuid>, name: &str| {
        let id = Uuid::now_v7();
        let name = name.to_string();
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(kb)
            .bind(type_id)
            .bind(name)
            .execute(&pool)
            .await
            .map(|_| id)
        }
    };
    let a1 = entity(Some(person), "Zhang Wei").await?;
    let a2 = entity(Some(person), "Zhang Wei").await?;
    let b1 = entity(Some(company), "Apple").await?;
    let b2 = entity(Some(person), "Apple").await?;
    let c1 = entity(Some(person), "Li Si").await?;
    let c2 = entity(Some(person), "Li Si").await?;
    let d1 = entity(Some(person), "Wang Wu").await?;
    let d2 = entity(None, "Wang Wu").await?;

    let review = |l: Uuid, r: Uuid, score: f32| {
        let id = Uuid::now_v7();
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO resolution_reviews (id, kb_id, left_id, right_id, score, reason, stage)
                 VALUES ($1, $2, $3, $4, $5, 'namesake', 'human')",
            )
            .bind(id)
            .bind(kb)
            .bind(l)
            .bind(r)
            .bind(score)
            .execute(&pool)
            .await
            .map(|_| id)
        }
    };
    let same_a = review(a1, a2, 1.0).await?;
    let conflict = review(b1, b2, 1.0).await?;
    let same_b = review(c1, c2, 1.0).await?;
    let untyped = review(d1, d2, 1.0).await?;
    Ok(Seed {
        kb,
        org,
        same_a,
        conflict,
        same_b,
        untyped,
        pair_a: (a1, a2),
        user,
    })
}

async fn teardown(pool: &PgPool, s: &Seed) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(s.kb)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(s.org)
        .execute(pool)
        .await?;
    Ok(())
}

async fn run(pool: &PgPool, s: &Seed) -> anyhow::Result<()> {
    // Three-tier filter
    let ids = |items: Vec<utopia_core::models::ReviewItem>| {
        let mut v: Vec<Uuid> = items.into_iter().map(|i| i.id).collect();
        v.sort();
        v
    };
    let all =
        ids(utopia_store::resolution::list_reviews(pool, s.kb, TypeFilter::Any, 100, 0).await?);
    assert_eq!(all.len(), 4, "all four pairs are still waiting");
    let mut same =
        ids(utopia_store::resolution::list_reviews(pool, s.kb, TypeFilter::Same, 100, 0).await?);
    let mut want_same = vec![s.same_a, s.same_b];
    same.sort();
    want_same.sort();
    assert_eq!(same, want_same, "same kind: two same-named Person pairs");
    let conflict =
        ids(
            utopia_store::resolution::list_reviews(pool, s.kb, TypeFilter::Conflict, 100, 0)
                .await?,
        );
    assert_eq!(conflict, vec![s.conflict], "clash: Organization vs Person");
    assert!(
        !same.contains(&s.untyped) && !conflict.contains(&s.untyped),
        "一侧没类型的哪档都不进"
    );

    // Counts and filters on the same basis
    let counts = utopia_store::review::counts(pool, s.kb).await?;
    assert_eq!(counts.duplicates, 4);
    assert_eq!(counts.duplicates_same_type, 2);
    assert_eq!(counts.duplicates_type_conflict, 1);

    // Unknown action: whole batch rejected, nothing touched
    assert!(utopia_store::resolution::decide_reviews(
        pool,
        s.kb,
        &[s.same_a],
        "smash",
        Uuid::now_v7(),
        None,
    )
    .await
    .is_err());

    // Batch merge: two real ones and one that does not exist
    let user = s.user;
    let ghost = Uuid::now_v7();
    let outcomes = utopia_store::resolution::decide_reviews(
        pool,
        s.kb,
        &[s.same_a, ghost, s.same_b],
        "merge",
        user,
        None,
    )
    .await?;
    assert_eq!(outcomes.len(), 3, "one result per id, order unchanged");
    assert!(
        outcomes[0].error.is_none(),
        "第一对合了: {:?}",
        outcomes[0].error
    );
    assert!(
        outcomes[1].error.is_some(),
        "the nonexistent one is reported"
    );
    assert!(
        outcomes[2].error.is_none(),
        "the third pair is not dragged down by the second"
    );

    let (status, decided_by): (String, Option<Uuid>) =
        sqlx::query_as("SELECT status, decided_by FROM resolution_reviews WHERE id = $1")
            .bind(s.same_a)
            .fetch_one(pool)
            .await?;
    assert_eq!(status, "merged");
    assert_eq!(
        decided_by,
        Some(user),
        "records the person, same as a single decision"
    );
    let merged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM entities WHERE id = ANY($1) AND merged_into IS NOT NULL",
    )
    .bind(vec![s.pair_a.0, s.pair_a.1])
    .fetch_one(pool)
    .await?;
    assert_eq!(merged, 1, "exactly one of a pair is merged into the other");

    // Deciding again something already decided: error, no duplicate merge
    let again =
        utopia_store::resolution::decide_reviews(pool, s.kb, &[s.same_a], "keep", user, None)
            .await?;
    assert!(again[0].error.is_some());

    // Batch split
    let kept =
        utopia_store::resolution::decide_reviews(pool, s.kb, &[s.conflict], "keep", user, None)
            .await?;
    assert!(kept[0].error.is_none());
    let counts = utopia_store::review::counts(pool, s.kb).await?;
    assert_eq!(counts.duplicates, 1, "only the untyped pair is left");
    assert_eq!(counts.duplicates_same_type, 0);
    assert_eq!(counts.duplicates_type_conflict, 0);
    Ok(())
}

#[tokio::test]
async fn a_batch_decides_like_a_person() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let s = seed(&pool).await?;
    let outcome = run(&pool, &s).await;
    teardown(&pool, &s).await?;
    outcome
}
