//! Business rules fired against a real database (0021 / #277).
//!
//! The evaluation cases themselves live in `utopia-reason::rules`: pure logic, no database. What is pinned here is the four things
//! that layer cannot see:
//!
//! - A hit really lands in `derived_facts` on the **literal-value channel**, with `attribute_rule_id` attached
//! - The premise chain goes into `fact_derivations` -- "which two readings this rests on"
//! - **Rerunning does not produce a second row**: the identity index treats NULL objects as the same one (this is the spot most easily missed when widening the table:
//!   miss it and every round adds another row)
//! - Raising the threshold and rerunning **retracts the old conclusion rather than deleting it**; the record axis keeps "was once derived from this"
//!
//! Also pinned is how this coexists with the axioms pass: the two passes merge into one reconciliation, and neither should mark the other's rows
//! as stale.

use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    well: Uuid,
    gas_bearing: Uuid,
    thc: Uuid,
    category: Uuid,
    is_a: Uuid,
    w1: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (well, gas_bearing) = (Uuid::now_v7(), Uuid::now_v7());
    let (thc, category, is_a) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let w1 = Uuid::now_v7();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'rule-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'rule-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'rule-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, key, label) in [
        (well, "well", "Well"),
        (gas_bearing, "gas_well", "GasBearingWell"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    // Two attribute predicates + built-in is_a
    for (id, key, label, datatype) in [
        (thc, "total_hydrocarbon", "全烃", "number"),
        (category, "interpretation_category", "解释结论", "text"),
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
        thc,
        category,
        is_a,
        w1,
    })
}

/// One attribute fact.
async fn attr(
    pool: &PgPool,
    f: &Fixture,
    predicate: Uuid,
    value: serde_json::Value,
    from: &str,
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value,
                            valid_from, valid_from_precision, confidence)
         VALUES ($1, $2, $3, $4, $5, $6, 'day', 0.9)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(f.w1)
    .bind(predicate)
    .bind(serde_json::json!({ "value": value }))
    .bind(from.parse::<chrono::DateTime<chrono::Utc>>()?)
    .execute(pool)
    .await?;
    Ok(id)
}

/// A rule: total hydrocarbons > threshold and the interpreted conclusion ∈ {gas-test anomaly, post-gas-test anomaly} → GasBearingWell
async fn rule(pool: &PgPool, f: &Fixture, threshold: f64) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO attribute_rules (id, kb_id, name, subject_type_id, conclusion, conclude_type_id)
         VALUES ($1, $2, 'gas-bearing', $3, 'typing', $4)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(f.well)
    .bind(f.gas_bearing)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO attribute_rule_conditions (id, rule_id, seq, predicate_id, op, operand)
         VALUES ($1, $2, 0, $3, 'gt', $4), ($5, $2, 1, $6, 'in', $7)",
    )
    .bind(Uuid::now_v7())
    .bind(id)
    .bind(f.thc)
    .bind(serde_json::json!(threshold))
    .bind(Uuid::now_v7())
    .bind(f.category)
    .bind(serde_json::json!(["气测异常", "气测异常后效"]))
    .execute(pool)
    .await?;
    Ok(id)
}

#[derive(sqlx::FromRow)]
struct Derived {
    id: Uuid,
    object_value: Option<serde_json::Value>,
    object_id: Option<Uuid>,
    attribute_rule_id: Option<Uuid>,
    valid_from: Option<chrono::DateTime<chrono::Utc>>,
    invalidated_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn derived(pool: &PgPool, f: &Fixture) -> anyhow::Result<Vec<Derived>> {
    Ok(sqlx::query_as(
        "SELECT id, object_value, object_id, attribute_rule_id, valid_from, invalidated_at
           FROM derived_facts WHERE kb_id = $1 ORDER BY derived_at",
    )
    .bind(f.kb)
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
async fn a_rule_types_a_well_and_says_which_readings_did_it() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let a = attr(
            &pool,
            &f,
            f.thc,
            serde_json::json!(12.3),
            "2023-06-01T00:00:00Z",
        )
        .await?;
        let b = attr(
            &pool,
            &f,
            f.category,
            serde_json::json!("气测异常"),
            "2023-06-01T00:00:00Z",
        )
        .await?;
        let r = rule(&pool, &f, 8.0).await?;

        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(report.attribute_rules, 1, "the rule must be read in");
        assert_eq!(report.rule_hits, 1, "two readings satisfy the conjunction");

        let rows = derived(&pool, &f).await?;
        assert_eq!(rows.len(), 1, "one derived classification");
        let d = &rows[0];
        assert!(
            d.object_id.is_none(),
            "classification goes through the literal-value channel, has no entity object"
        );
        assert_eq!(
            d.object_value
                .as_ref()
                .and_then(|v| v.get("class"))
                .and_then(|v| v.as_str()),
            Some("gas_well"),
            "没有 IRI 时退回 key，记的是类而不是标签"
        );
        assert_eq!(
            d.attribute_rule_id,
            Some(r),
            "the conclusion points back to its rule"
        );
        assert_eq!(
            d.valid_from.map(|t| t.to_rfc3339()),
            Some("2023-06-01T00:00:00+00:00".into()),
            "区间取两条前提的交集"
        );

        // Premise chain: which two readings
        let premises: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT premise_fact_id FROM fact_derivations WHERE derived_fact_id = $1 ORDER BY seq",
        )
        .bind(d.id)
        .fetch_all(&pool)
        .await?;
        let got: Vec<Uuid> = premises.into_iter().map(|(x,)| x).collect();
        assert_eq!(got.len(), 2, "both premises must be recorded");
        assert!(got.contains(&a) && got.contains(&b));

        // **Rerunning must not add a row.** The object_id of a literal-value conclusion is NULL, and Postgres by default
        // treats NULLs as distinct -- if the identity index is written wrong, every round adds one more row here
        let again = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(
            again.inserted, 0,
            "rerunning the same conclusion does not insert again"
        );
        assert_eq!(again.invalidated, 0, "nor should it judge itself stale");
        assert_eq!(
            derived(&pool, &f).await?.len(),
            1,
            "still one row in the database"
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

/// Two groups of "or" (0029 / #476): when the second group holds on its own, the resulting conclusion is attached only to **its own** reading.
///
/// Pins something the pure-logic layer cannot see: the premise chain written to `fact_derivations` is this group's
/// premises, not every fact that all the rule's conditions touched -- "which reading it rests on" must also be real in the database.
#[tokio::test]
async fn either_group_concludes_and_names_only_its_own_reading() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // Only one total-hydrocarbon reading, and no interpreted conclusion: group 0 (total > 8 and interpretation ∈ {…})
        // does not hold, group 1 (total > 20) does
        let a = attr(
            &pool,
            &f,
            f.thc,
            serde_json::json!(25.0),
            "2023-06-01T00:00:00Z",
        )
        .await?;
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO attribute_rules (id, kb_id, name, subject_type_id, conclusion, conclude_type_id)
             VALUES ($1, $2, 'gas-bearing (two ways)', $3, 'typing', $4)",
        )
        .bind(id)
        .bind(f.kb)
        .bind(f.well)
        .bind(f.gas_bearing)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO attribute_rule_conditions
                (id, rule_id, group_seq, seq, predicate_id, op, operand)
             VALUES ($1, $2, 0, 0, $3, 'gt', $4),
                    ($5, $2, 0, 1, $6, 'in', $7),
                    ($8, $2, 1, 0, $3, 'gt', $9)",
        )
        .bind(Uuid::now_v7())
        .bind(id)
        .bind(f.thc)
        .bind(serde_json::json!(8.0))
        .bind(Uuid::now_v7())
        .bind(f.category)
        .bind(serde_json::json!(["气测异常"]))
        .bind(Uuid::now_v7())
        .bind(serde_json::json!(20.0))
        .execute(&pool)
        .await?;

        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(report.rule_hits, 1, "the second group holds on its own, one hit");

        let rows = derived(&pool, &f).await?;
        assert_eq!(rows.len(), 1);
        let premises: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT premise_fact_id FROM fact_derivations WHERE derived_fact_id = $1",
        )
        .bind(rows[0].id)
        .fetch_all(&pool)
        .await?;
        let got: Vec<Uuid> = premises.into_iter().map(|(x,)| x).collect();
        assert_eq!(got, vec![a], "a reading used only by this group as a premise");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// Rerun with the threshold raised above the reading: the conclusion is **retracted rather than deleted** -- the record axis keeps "we once derived this from"
#[tokio::test]
async fn raising_the_threshold_invalidates_without_deleting() -> anyhow::Result<()> {
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
        )
        .await?;
        attr(
            &pool,
            &f,
            f.category,
            serde_json::json!("气测异常"),
            "2023-06-01T00:00:00Z",
        )
        .await?;
        let r = rule(&pool, &f, 8.0).await?;
        utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(derived(&pool, &f).await?.len(), 1);

        // Threshold raised to 20: the same data no longer satisfies it
        sqlx::query(
            "UPDATE attribute_rule_conditions SET operand = '20' WHERE rule_id = $1 AND seq = 0",
        )
        .bind(r)
        .execute(&pool)
        .await?;
        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(report.rule_hits, 0, "after raising it, no longer hits");
        assert_eq!(report.invalidated, 1);

        let rows = derived(&pool, &f).await?;
        assert_eq!(rows.len(), 1, "row still there: voiding is not deletion");
        assert!(rows[0].invalidated_at.is_some(), "voiding time is marked");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// Turning a rule off is the same as its premise disappearing: the conclusion goes invalid next round, but the rule row stays
#[tokio::test]
async fn disabling_a_rule_retires_what_it_concluded() -> anyhow::Result<()> {
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
        )
        .await?;
        attr(
            &pool,
            &f,
            f.category,
            serde_json::json!("气测异常"),
            "2023-06-01T00:00:00Z",
        )
        .await?;
        let r = rule(&pool, &f, 8.0).await?;
        utopia_store::reasoning::materialize(&pool, f.kb).await?;

        sqlx::query("UPDATE attribute_rules SET enabled = FALSE WHERE id = $1")
            .bind(r)
            .execute(&pool)
            .await?;
        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(
            report.attribute_rules, 0,
            "a disabled rule does not take part in evaluation"
        );
        assert_eq!(report.invalidated, 1, "what it derived retires with it");
        let rows = derived(&pool, &f).await?;
        assert!(rows[0].invalidated_at.is_some());
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// Retracting one premise fact invalidates the conclusion that rests on it -- the same path the axioms pass takes
#[tokio::test]
async fn retracting_a_reading_retires_the_conclusion() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let a = attr(
            &pool,
            &f,
            f.thc,
            serde_json::json!(12.3),
            "2023-06-01T00:00:00Z",
        )
        .await?;
        attr(
            &pool,
            &f,
            f.category,
            serde_json::json!("气测异常"),
            "2023-06-01T00:00:00Z",
        )
        .await?;
        rule(&pool, &f, 8.0).await?;
        utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(derived(&pool, &f).await?[0].invalidated_at, None);

        sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
            .bind(a)
            .execute(&pool)
            .await?;
        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(
            report.rule_hits, 0,
            "without the total hydrocarbon premise it does not hold"
        );
        assert_eq!(report.invalidated, 1);
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// A derived conclusion **is not written to `entities.type_id`** (0021 decision 2): an asserted type belongs to a person,
/// and a rule's conclusion is a layer stacked on top of it
#[tokio::test]
async fn the_asserted_type_is_left_alone() -> anyhow::Result<()> {
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
        )
        .await?;
        attr(
            &pool,
            &f,
            f.category,
            serde_json::json!("气测异常"),
            "2023-06-01T00:00:00Z",
        )
        .await?;
        rule(&pool, &f, 8.0).await?;
        utopia_store::reasoning::materialize(&pool, f.kb).await?;

        let (t,): (Option<Uuid>,) = sqlx::query_as("SELECT type_id FROM entities WHERE id = $1")
            .bind(f.w1)
            .fetch_one(&pool)
            .await?;
        assert_eq!(t, Some(f.well), "the entity's asserted type is still Well");
        let rows = derived(&pool, &f).await?;
        assert_eq!(rows.len(), 1, "GasBearingWell exists only as a derivation");
        assert_eq!(rows[0].object_id, None);
        // The conclusion lands on built-in is_a
        let (p,): (Uuid,) = sqlx::query_as("SELECT predicate_id FROM derived_facts WHERE id = $1")
            .bind(rows[0].id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(p, f.is_a);
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// Changing a conclusion: from "infer a class" to "infer an attribute value".
///
/// **The old conclusion must be retracted, not left in place to hold alongside the new one.** The three slots are defined in terms of each other,
/// so this also pins that `conclude_type_id` is cleared -- a half-finished state (saying attribute
/// while still carrying a class) looks fine in the database and only surfaces in exports and the canvas.
#[tokio::test]
async fn changing_the_conclusion_retires_the_old_one() -> anyhow::Result<()> {
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
        )
        .await?;
        attr(
            &pool,
            &f,
            f.category,
            serde_json::json!("气测异常"),
            "2023-06-01T00:00:00Z",
        )
        .await?;
        let r = rule(&pool, &f, 8.0).await?;
        utopia_store::reasoning::materialize(&pool, f.kb).await?;
        let rows = derived(&pool, &f).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].object_value,
            Some(serde_json::json!({"class": "gas_well"}))
        );

        // Conclusion changed to an attribute: containing gas = gas
        let verdict = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, kind, datatype)
             VALUES ($1, $2, 'gas_verdict', '含气性', 'attribute', 'text')",
        )
        .bind(verdict)
        .bind(f.kb)
        .execute(&pool)
        .await?;
        utopia_store::business_rules::update(
            &pool,
            f.kb,
            r,
            None,
            None,
            None,
            None,
            Some(&utopia_store::business_rules::ConclusionInput {
                kind: "attribute".into(),
                type_id: None,
                predicate_id: Some(verdict),
                value: Some(serde_json::json!("含气")),
                expr: None,
                join_predicate_id: None,
            }),
        )
        .await?;

        let (kind, class): (String, Option<Uuid>) = sqlx::query_as(
            "SELECT conclusion, conclude_type_id FROM attribute_rules WHERE id = $1",
        )
        .bind(r)
        .fetch_one(&pool)
        .await?;
        assert_eq!(kind, "attribute");
        assert!(
            class.is_none(),
            "shape changed, the old cell must be cleared"
        );

        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(
            report.rule_hits, 1,
            "same readings still hit, only the conclusion changed"
        );
        assert_eq!(report.invalidated, 1, "old conclusion retires");

        let rows = derived(&pool, &f).await?;
        assert_eq!(
            rows.len(),
            2,
            "two rows: voided old conclusion + new conclusion"
        );
        let old = rows.iter().find(|d| d.invalidated_at.is_some()).unwrap();
        let new = rows.iter().find(|d| d.invalidated_at.is_none()).unwrap();
        assert_eq!(
            old.object_value,
            Some(serde_json::json!({"class": "gas_well"}))
        );
        assert_eq!(new.object_value, Some(serde_json::json!({"value": "含气"})));
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// Combinations that cannot be expanded must **stay on the rule**, not just be mentioned once at the moment of the run.
///
/// A well with two attributes of nine readings each is 81 combinations; past the cap the entity is skipped wholesale,
/// and "skipped" looks exactly like "not satisfied" in the result. The reading count is wound back, and this number must drop
/// back to zero with it -- otherwise it only piles up monotonically into a scary, false statement.
#[tokio::test]
async fn a_capped_expansion_is_remembered() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        let mut thc_facts = Vec::new();
        for i in 0..9 {
            thc_facts.push(
                attr(
                    &pool,
                    &f,
                    f.thc,
                    serde_json::json!(10.0 + i as f64),
                    &format!("2023-06-{:02}T00:00:00Z", i + 1),
                )
                .await?,
            );
            attr(
                &pool,
                &f,
                f.category,
                serde_json::json!("气测异常"),
                &format!("2023-07-{:02}T00:00:00Z", i + 1),
            )
            .await?;
        }
        let r = rule(&pool, &f, 8.0).await?;

        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(report.rule_capped, 1);
        assert_eq!(
            report.rule_hits, 0,
            "展不完就整个跳过——这正是要说出来的原因"
        );
        assert_eq!(
            capped_of(&pool, f.kb, r).await?,
            1,
            "落在规则上，卡片读得到"
        );

        // Readings wound back to 1 × 9: now it can be expanded
        for id in thc_facts.iter().skip(1) {
            sqlx::query("DELETE FROM facts WHERE id = $1")
                .bind(id)
                .execute(&pool)
                .await?;
        }
        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(report.rule_capped, 0);
        assert!(report.rule_hits > 0);
        assert_eq!(
            capped_of(&pool, f.kb, r).await?,
            0,
            "reset to zero, not accumulated"
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

/// What you see after clicking the number on the card: who is flagged, and from which readings.
#[tokio::test]
async fn the_matches_list_says_who_and_why() -> anyhow::Result<()> {
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
        )
        .await?;
        attr(
            &pool,
            &f,
            f.category,
            serde_json::json!("气测异常"),
            "2023-06-01T00:00:00Z",
        )
        .await?;
        let r = rule(&pool, &f, 8.0).await?;
        utopia_store::reasoning::materialize(&pool, f.kb).await?;

        let (rows, total) = utopia_store::business_rules::matches(&pool, f.kb, r, 20, 0).await?;
        assert_eq!(total, 1);
        assert_eq!(rows[0]["entity"], "W-1");
        assert_eq!(
            rows[0]["concluded"], "GasBearingWell",
            "读出来是标签，不是库里的 key"
        );
        let premises: Vec<String> = rows[0]["premises"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            premises,
            vec!["全烃 = 12.3", "解释结论 = 气测异常"],
            "前提按顺序，读得出「凭什么」"
        );

        // Once the rule is turned off its conclusions retire and the list empties too -- the number and the list are the same fact
        utopia_store::business_rules::update(&pool, f.kb, r, None, None, Some(false), None, None)
            .await?;
        utopia_store::reasoning::materialize(&pool, f.kb).await?;
        let (rows, total) = utopia_store::business_rules::matches(&pool, f.kb, r, 20, 0).await?;
        assert_eq!(total, 0);
        assert!(rows.is_empty());
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// How many entities were not fully expanded the last time the rule ran.
async fn capped_of(pool: &PgPool, kb: Uuid, rule_id: Uuid) -> anyhow::Result<i64> {
    let rules = utopia_store::business_rules::list(pool, kb).await?;
    let r = rules
        .iter()
        .find(|r| r["id"].as_str() == Some(&rule_id.to_string()))
        .expect("the rule is in the list");
    Ok(r["capped"].as_i64().expect("capped is a number"))
}

/// "Not in" written to the database, read back, and actually evaluated (0029 / #476).
///
/// Pins the compile step: `parse_operand` once only had a branch for `in`, so `not_in` fell through to the
/// numeric branch and could not be parsed, and **the whole rule was skipped as badly written** -- it inferred nothing,
/// while on the rules page it looked completely normal. Only a test run against a real database can see this bug: the pure-logic cases
/// construct `Operand::Set` directly, bypassing the step that was going wrong.
#[tokio::test]
async fn a_rule_that_says_not_one_of_is_not_silently_skipped() -> anyhow::Result<()> {
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
        )
        .await?;
        attr(
            &pool,
            &f,
            f.category,
            serde_json::json!("水层"),
            "2023-06-01T00:00:00Z",
        )
        .await?;

        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO attribute_rules (id, kb_id, name, subject_type_id, conclusion, conclude_type_id)
             VALUES ($1, $2, 'not-water', $3, 'typing', $4)",
        )
        .bind(id)
        .bind(f.kb)
        .bind(f.well)
        .bind(f.gas_bearing)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO attribute_rule_conditions (id, rule_id, seq, predicate_id, op, operand)
             VALUES ($1, $2, 0, $3, 'not_in', $4)",
        )
        .bind(Uuid::now_v7())
        .bind(id)
        .bind(f.category)
        .bind(serde_json::json!(["气测异常"]))
        .execute(&pool)
        .await?;

        let report = utopia_store::reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(report.attribute_rules, 1, "the rule must be read in");
        assert_eq!(
            report.rule_hits, 1,
            "「水层」不属于 {{气测异常}}，这条规则应当命中——从前它整条被跳过"
        );
        assert_eq!(derived(&pool, &f).await?.len(), 1);
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

#[tokio::test]
async fn renaming_a_rule_uses_the_creation_name_limits_before_any_write() -> anyhow::Result<()> {
    use utopia_store::business_rules::{self, ConclusionInput, ConditionInput};
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;
    let conditions = [ConditionInput {
        group: 0,
        side: "x".into(),
        predicate_id: f.thc,
        op: "gt".into(),
        operand: Some(serde_json::json!(5)),
    }];
    let id = business_rules::create(
        &pool,
        f.kb,
        "original",
        "",
        f.well,
        "typing",
        Some(f.gas_bearing),
        None,
        None,
        None,
        None,
        &conditions,
    )
    .await?;
    let changed = [ConditionInput {
        group: 9,
        side: "x".into(),
        predicate_id: f.thc,
        op: "lt".into(),
        operand: Some(serde_json::json!(10)),
    }];
    let conclusion = ConclusionInput {
        kind: "attribute".into(),
        type_id: None,
        predicate_id: Some(f.category),
        value: Some(serde_json::json!({"value":"changed"})),
        expr: None,
        join_predicate_id: None,
    };
    for invalid in [
        String::new(),
        " \t\n ".into(),
        "a".repeat(81),
        "界".repeat(81),
    ] {
        let before = business_rules::list(&pool, f.kb).await?;
        let result = business_rules::update(
            &pool,
            f.kb,
            id,
            Some(&invalid),
            Some("changed"),
            Some(false),
            Some(&changed),
            Some(&conclusion),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(utopia_core::AppError::Invalid {
                    code: "bad_rule_name",
                    ..
                })
            ),
            "invalid name {:?}: {:?}",
            invalid,
            result
        );
        assert_eq!(
            business_rules::list(&pool, f.kb).await?,
            before,
            "a rejected combined update must leave name, conditions and conclusion unchanged"
        );
    }
    for valid in ["a".repeat(80), "界".repeat(80), "  trimmed  ".into()] {
        business_rules::update(&pool, f.kb, id, Some(&valid), None, None, None, None).await?;
        let rows = business_rules::list(&pool, f.kb).await?;
        assert_eq!(rows[0]["name"], valid.trim());
        // Saving the same name remains valid.
        business_rules::update(&pool, f.kb, id, Some(&valid), None, None, None, None).await?;
    }
    business_rules::update(
        &pool,
        f.kb,
        id,
        None,
        Some("changed"),
        Some(false),
        Some(&changed),
        Some(&conclusion),
    )
    .await?;
    let rows = business_rules::list(&pool, f.kb).await?;
    assert_eq!(rows[0]["name"], "trimmed");
    assert_eq!(rows[0]["description"], "changed");
    assert_eq!(rows[0]["enabled"], false);
    assert_eq!(rows[0]["conditions"][0]["group"], 9);
    assert_eq!(rows[0]["conditions"][0]["op"], "lt");
    assert_eq!(rows[0]["conclusion"], "attribute");
    assert_eq!(
        rows[0]["conclude_value"],
        serde_json::json!({"value":"changed"})
    );
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    Ok(())
}
