//! Rules reading rules (0030 / #477).
//!
//! The read-only assertions of 0021 rules run in a single pass, so `A → B → C` has to be flattened into one rule rewritten all at once.
//! Now one round's conclusions feed the next round's inputs; what is pinned here are four consequences of that on a real database:
//!
//! - **The links connect**: the second rule reads the first rule's conclusion; one premise is an assertion, the other a derivation,
//!   each stored in its own column
//! - **A derived classification carries its interval**: it enters as a condition, not a sieve — 2020–2021 are A entities,
//!   and a 2022 reading should not satisfy a rule on A
//! - **A changed leaf retires the whole chain**: raising the first rule's threshold voids the row the second rule derived along with it.
//!   Nothing chases it; it is the natural result of "full recomputation + reconciliation"
//! - **Self-feedback stops**: a rule that derives a class which is its own subject class gets the same key in the second round,
//!   and the fixed point converges there
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is unset. Creates and tears down its own; never touches an existing database.

use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    well: Uuid,
    gas_bearing: Uuid,
    producer: Uuid,
    thc: Uuid,
    completed: Uuid,
    w1: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (well, gas_bearing, producer) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (thc, completed, is_a) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let w1 = Uuid::now_v7();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'chain-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'chain-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'chain-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, key, label) in [
        (well, "well", "Well"),
        (gas_bearing, "gas_well", "GasBearingWell"),
        (producer, "producer", "Producer"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    for (id, key, label, datatype) in [
        (thc, "total_hydrocarbon", "全烃", "number"),
        (completed, "completion", "完井", "text"),
        (is_a, "is_a", "is a", "text"),
    ] {
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, kind, datatype, builtin)
             VALUES ($1, $2, $3, $4, 'attribute', $5, $6)",
        )
        .bind(id)
        .bind(kb)
        .bind(key)
        .bind(label)
        .bind(datatype)
        .bind(key == "is_a")
        .execute(pool)
        .await?;
    }
    // The entity's assertion is of class Well. GasBearingWell can only be derived — this is exactly the path being pinned
    sqlx::query(
        "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, 'W-1')",
    )
    .bind(w1)
    .bind(kb)
    .bind(well)
    .execute(pool)
    .await?;
    Ok(Fixture {
        org,
        kb,
        well,
        gas_bearing,
        producer,
        thc,
        completed,
        w1,
    })
}

/// An attribute fact with both interval ends set — the interval on the chain is handed over, and with fuzzy endpoints you cannot tell whether it was handed over
async fn attr(
    pool: &PgPool,
    f: &Fixture,
    predicate: Uuid,
    value: serde_json::Value,
    from: &str,
    to: Option<&str>,
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value,
                            valid_from, valid_from_precision,
                            valid_to, valid_to_precision, confidence)
         VALUES ($1, $2, $3, $4, $5, $6, 'day', $7, $8, 0.9)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(f.w1)
    .bind(predicate)
    .bind(serde_json::json!({ "value": value }))
    .bind(from.parse::<chrono::DateTime<chrono::Utc>>()?)
    .bind(
        to.map(|t| t.parse::<chrono::DateTime<chrono::Utc>>())
            .transpose()?,
    )
    .bind(to.map(|_| "day"))
    .execute(pool)
    .await?;
    Ok(id)
}

/// A classification rule: one attribute condition on the subject class → conclusion class. The three cells of the condition are bundled into one parameter —
/// they are one sentence to begin with
async fn rule(
    pool: &PgPool,
    f: &Fixture,
    name: &str,
    subject: Uuid,
    condition: (Uuid, &str, Option<serde_json::Value>),
    concludes: Uuid,
) -> anyhow::Result<Uuid> {
    let (predicate, op, operand) = condition;
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO attribute_rules (id, kb_id, name, subject_type_id, conclusion, conclude_type_id)
         VALUES ($1, $2, $3, $4, 'typing', $5)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(name)
    .bind(subject)
    .bind(concludes)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO attribute_rule_conditions (id, rule_id, seq, predicate_id, op, operand)
         VALUES ($1, $2, 0, $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(id)
    .bind(predicate)
    .bind(op)
    .bind(operand)
    .execute(pool)
    .await?;
    Ok(id)
}

#[derive(sqlx::FromRow)]
struct Derived {
    id: Uuid,
    class: Option<String>,
    valid_to: Option<chrono::DateTime<chrono::Utc>>,
    invalidated_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn derived(pool: &PgPool, f: &Fixture) -> anyhow::Result<Vec<Derived>> {
    Ok(sqlx::query_as(
        "SELECT id, object_value ->> 'class' AS class,
                valid_to, invalidated_at
           FROM derived_facts WHERE kb_id = $1 ORDER BY derived_at, id",
    )
    .bind(f.kb)
    .fetch_all(pool)
    .await?)
}

/// `Well → GasBearingWell → Producer`: the second rule reads the first rule's conclusion.
///
/// What is pinned is **what that row's premises look like on the chain**: one assertion (completion), one derivation (classification),
/// landing in `premise_fact_id` and `premise_derived_id` respectively. If folded into one column,
/// "whether this step needs to ask further down" could not be answered in the database.
#[tokio::test]
async fn a_conclusion_is_a_premise_for_the_next_rule() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let thc_fact = attr(
            &pool,
            &f,
            f.thc,
            serde_json::json!(12.3),
            "2023-06-01T00:00:00Z",
            None,
        )
        .await?;
        let done = attr(
            &pool,
            &f,
            f.completed,
            serde_json::json!("yes"),
            "2023-06-01T00:00:00Z",
            None,
        )
        .await?;
        rule(
            &pool,
            &f,
            "gas-bearing",
            f.well,
            (f.thc, "gt", Some(serde_json::json!(8.0))),
            f.gas_bearing,
        )
        .await?;
        rule(
            &pool,
            &f,
            "producer",
            f.gas_bearing,
            (f.completed, "present", None),
            f.producer,
        )
        .await?;

        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(report.attribute_rules, 2);
        assert_eq!(report.rule_hits, 2, "两条结论：含气井，以及在产井");
        assert!(
            report.rule_rounds >= 2,
            "链上一环一轮，一轮跑不出第二条：{}",
            report.rule_rounds
        );
        assert!(!report.rule_rounds_capped, "两环没到上限");

        let rows = derived(&pool, &f).await?;
        assert_eq!(rows.len(), 2);
        let gas = rows
            .iter()
            .find(|d| d.class.as_deref() == Some("gas_well"))
            .expect("含气井那一行");
        let prod = rows
            .iter()
            .find(|d| d.class.as_deref() == Some("producer"))
            .expect("在产井那一行");

        // Gas-bearing well: both premises are assertions
        let leaf: Vec<(Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
            "SELECT premise_fact_id, premise_derived_id
               FROM fact_derivations WHERE derived_fact_id = $1 ORDER BY seq",
        )
        .bind(gas.id)
        .fetch_all(&pool)
        .await?;
        assert_eq!(leaf, vec![(Some(thc_fact), None)], "含气井只凭那条全烃读数");

        // Producing well: one assertion + one derivation. **This one is the chain**
        let chained: Vec<(Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
            "SELECT premise_fact_id, premise_derived_id
               FROM fact_derivations WHERE derived_fact_id = $1 ORDER BY seq",
        )
        .bind(prod.id)
        .fetch_all(&pool)
        .await?;
        assert_eq!(chained.len(), 2, "完井那条读数，加上「它是含气井」那条结论");
        assert!(
            chained.contains(&(Some(done), None)),
            "断言前提落在 premise_fact_id 上"
        );
        assert!(
            chained.contains(&(None, Some(gas.id))),
            "派生前提落在 premise_derived_id 上，指的是含气井那一行"
        );

        // Proof that one level is walked down the chain: producing well → gas-bearing well → the whole-hydrocarbon statement
        let proof = utopia_store::reasoning::proof(&pool, f.kb, prod.id)
            .await?
            .expect("在产井有证明");
        let step = proof
            .steps
            .iter()
            .find(|s| s.derived)
            .expect("证明里有一步是推出来的");
        assert_eq!(step.premises.len(), 1, "那一步自己也有前提，再往下一层");
        assert!(!step.premises[0].derived, "再下一层是断言，链到此为止");

        // A rerun should add no rows, and should not judge it stale either — the row on the chain identifies the row id left by the previous round.
        // If it identified the wrong one, you would see "void one, insert one" here
        let again = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(again.inserted, 0, "同一条链重跑不再插");
        assert_eq!(again.invalidated, 0, "也不该把自己判成陈旧");
        assert_eq!(derived(&pool, &f).await?.len(), 2);
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// The derived class **carries an interval**: it enters as a condition, not a sieve.
///
/// A well that was gas-bearing in 2020–2021 and whose completion reading is in 2022 — the two spans do not intersect, so there should be no
/// producing well. If the range were still a sieve with no interval, one extra row would be derived here, and that row's interval
/// could not say why it holds.
#[tokio::test]
async fn a_concluded_class_only_holds_where_it_was_concluded() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        attr(
            &pool,
            &f,
            f.thc,
            serde_json::json!(12.3),
            "2020-01-01T00:00:00Z",
            Some("2021-01-01T00:00:00Z"),
        )
        .await?;
        attr(
            &pool,
            &f,
            f.completed,
            serde_json::json!("yes"),
            "2022-01-01T00:00:00Z",
            None,
        )
        .await?;
        rule(
            &pool,
            &f,
            "gas-bearing",
            f.well,
            (f.thc, "gt", Some(serde_json::json!(8.0))),
            f.gas_bearing,
        )
        .await?;
        rule(
            &pool,
            &f,
            "producer",
            f.gas_bearing,
            (f.completed, "present", None),
            f.producer,
        )
        .await?;

        utopia_store::reasoning::materialize(&pool, f.kb).await?;
        let rows = derived(&pool, &f).await?;
        assert_eq!(
            rows.len(),
            1,
            "只有含气井那一行；完井那条读数落在它成立的区间之外"
        );
        assert_eq!(rows[0].class.as_deref(), Some("gas_well"));
        assert_eq!(
            rows[0].valid_to.map(|t| t.to_rfc3339()),
            Some("2021-01-01T00:00:00+00:00".into()),
            "结论的区间就是那条读数的区间"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// Change the leaf and the whole chain retires — **and not one line of code was written for this**.
///
/// Raise the first rule's threshold and the gas-bearing well no longer holds; the producing well stands on top of it, and this round it was never
/// recomputed, so it drops out of `wanted` and is voided in the same statement as the gas-bearing well.
#[tokio::test]
async fn a_chain_retires_with_the_reading_it_stood_on() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        attr(
            &pool,
            &f,
            f.thc,
            serde_json::json!(12.3),
            "2023-06-01T00:00:00Z",
            None,
        )
        .await?;
        attr(
            &pool,
            &f,
            f.completed,
            serde_json::json!("yes"),
            "2023-06-01T00:00:00Z",
            None,
        )
        .await?;
        let first = rule(
            &pool,
            &f,
            "gas-bearing",
            f.well,
            (f.thc, "gt", Some(serde_json::json!(8.0))),
            f.gas_bearing,
        )
        .await?;
        rule(
            &pool,
            &f,
            "producer",
            f.gas_bearing,
            (f.completed, "present", None),
            f.producer,
        )
        .await?;
        utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(derived(&pool, &f).await?.len(), 2);

        // Raise to 20: that 12.3 reading no longer satisfies the first rule
        sqlx::query("UPDATE attribute_rule_conditions SET operand = '20.0' WHERE rule_id = $1")
            .bind(first)
            .execute(&pool)
            .await?;
        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(report.invalidated, 2, "含气井与站在它上面的在产井一起退场");
        assert_eq!(report.inserted, 0);

        let rows = derived(&pool, &f).await?;
        assert_eq!(rows.len(), 2, "作废不是删除：记录轴上留着「曾据此推出」");
        assert!(
            rows.iter().all(|d| d.invalidated_at.is_some()),
            "两行都作废"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// A rule reads the class it derived itself; the fixed point still converges.
///
/// The first rule pushes the well to gas-bearing; the second is written on gas-bearing and derives gas-bearing again. The second round computes
/// the same key — same subject, predicate, value and interval — so it no longer enters the pool, and the third round has nothing to do.
/// **The cap is a fallback, not what stops it**: here `rule_rounds` is far below `MAX_DEPTH`.
#[tokio::test]
async fn a_rule_that_feeds_itself_settles() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        attr(
            &pool,
            &f,
            f.thc,
            serde_json::json!(12.3),
            "2023-06-01T00:00:00Z",
            None,
        )
        .await?;
        rule(
            &pool,
            &f,
            "gas-bearing",
            f.well,
            (f.thc, "gt", Some(serde_json::json!(8.0))),
            f.gas_bearing,
        )
        .await?;
        // Subject class is gas-bearing well, and the conclusion is gas-bearing well too: it reads exactly the one it derived itself
        rule(
            &pool,
            &f,
            "still gas-bearing",
            f.gas_bearing,
            (f.thc, "gt", Some(serde_json::json!(8.0))),
            f.gas_bearing,
        )
        .await?;

        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(report.rule_hits, 1, "两条规则推出的是同一条结论，只算一条");
        assert!(
            report.rule_rounds < utopia_reason::MAX_DEPTH,
            "收敛靠的是「这一轮没有新键」，不是撞上上限：{}",
            report.rule_rounds
        );
        assert!(!report.rule_rounds_capped);
        assert_eq!(derived(&pool, &f).await?.len(), 1, "库里就一行");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// The conclusion is unchanged, the reason changed: that row stays, **and the proof follows the change**.
///
/// The reconciliation key is subject, predicate and interval; the premises are not in it — the same sentence supported by a different reading leaves the row untouched.
/// If not rewritten, it would keep hanging on the reading from the previous round that has since been withdrawn, and the chain makes this more likely to collide:
/// the premise standing on it may have just been voided.
#[tokio::test]
async fn a_conclusion_that_stands_on_a_new_reading_gets_a_new_proof() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let first = attr(
            &pool,
            &f,
            f.thc,
            serde_json::json!(12.3),
            "2023-06-01T00:00:00Z",
            None,
        )
        .await?;
        rule(
            &pool,
            &f,
            "gas-bearing",
            f.well,
            (f.thc, "gt", Some(serde_json::json!(8.0))),
            f.gas_bearing,
        )
        .await?;
        utopia_store::reasoning::materialize(&pool, f.kb).await?;
        let rows = derived(&pool, &f).await?;
        assert_eq!(rows.len(), 1);
        let row = rows[0].id;

        // Withdraw that reading and substitute one with the same interval and a value that also clears the threshold: the conclusion is word for word the same, but is supported by
        // another sentence
        sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
            .bind(first)
            .execute(&pool)
            .await?;
        let second = attr(
            &pool,
            &f,
            f.thc,
            serde_json::json!(30.0),
            "2023-06-01T00:00:00Z",
            None,
        )
        .await?;

        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(report.inserted, 0, "结论没变，不该多一行");
        assert_eq!(report.invalidated, 0, "也不该把它判成陈旧");
        assert_eq!(report.reproved, 1, "证明换了一条");

        let now: Vec<(Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
            "SELECT premise_fact_id, premise_derived_id
               FROM fact_derivations WHERE derived_fact_id = $1 ORDER BY seq",
        )
        .bind(row)
        .fetch_all(&pool)
        .await?;
        assert_eq!(now, vec![(Some(second), None)], "挂的是现在成立的那条读数");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
