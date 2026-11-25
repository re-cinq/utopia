//! R1, run against a real store. The pure-logic part (`utopia-reason::derive`) already has 12 cases;
//! what is pinned here are the four things it cannot see:
//!
//! - **An assertion beats a derivation**. A triple already asserted is not derived a second time
//! - **When a premise is retracted, the derivation falls with it** — and it sets `invalidated_at` rather than deleting the row,
//!   so the record axis keeps the trace "we once derived this from it, and later the premise was gone" (0002 section 3)
//! - **The proof can be stored**. `fact_derivations` records the direct premises by seq, which R2 expands
//! - **Rule identity is stable across reruns**. Otherwise every run points `rule_id` at a new id, and the history breaks
//!
//! There is also one that only a database can check: derived facts must pass the two precision CHECKs on `facts`.
//! When the intersection makes one end unbounded, that end's precision must be cleared along with it, or the whole INSERT is rejected.

use sqlx::PgPool;
use utopia_store::reasoning;
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    part_of: Uuid,
    a: Uuid,
    b: Uuid,
    c: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let etype = Uuid::now_v7();
    let part_of = Uuid::now_v7();
    let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'derive-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'derive-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'derive-test')",
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
        "INSERT INTO relation_types (id, kb_id, key, label, is_transitive)
         VALUES ($1, $2, 'part_of', 'part of', TRUE)",
    )
    .bind(part_of)
    .bind(kb)
    .execute(pool)
    .await?;
    for (id, name) in [(a, "Acme"), (b, "Beta"), (c, "Cyrus")] {
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
        part_of,
        a,
        b,
        c,
    })
}

/// All the assertions here are witnessed from the same moment. A fact with no start is read from the moment of its witness (0064), and the witness defaults to
/// the moment of insertion, read by the second: once the two premises and the later assertion cross a second boundary, the assertion's interval starts later than the derivation and does not cover it,
/// and the "assertion first" step would occasionally leave one extra derivation behind
const ATTESTED: &str = "2026-01-01T00:00:00Z";

/// Stores one asserted fact, with an interval and precision optional.
async fn assert_fact(
    pool: &PgPool,
    f: &Fixture,
    s: Uuid,
    o: Uuid,
    span: Option<(&str, &str)>,
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    match span {
        Some((from, prec)) => {
            sqlx::query(
                "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                                    valid_from, valid_from_precision, attested_from)
                 VALUES ($1, $2, $3, $4, $5, $6::timestamptz, $7, $8::timestamptz)",
            )
            .bind(id)
            .bind(f.kb)
            .bind(s)
            .bind(f.part_of)
            .bind(o)
            .bind(from)
            .bind(prec)
            .bind(ATTESTED)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query(
                "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, attested_from)
                 VALUES ($1, $2, $3, $4, $5, $6::timestamptz)",
            )
            .bind(id)
            .bind(f.kb)
            .bind(s)
            .bind(f.part_of)
            .bind(o)
            .bind(ATTESTED)
            .execute(pool)
            .await?;
        }
    }
    Ok(id)
}

/// Live derived facts: (subject, object)
async fn live_derived(pool: &PgPool, kb: Uuid) -> anyhow::Result<Vec<(Uuid, Uuid)>> {
    Ok(sqlx::query_as(
        "SELECT subject_id, object_id FROM derived_facts
          WHERE kb_id = $1 AND invalidated_at IS NULL
          ORDER BY subject_id, object_id",
    )
    .bind(kb)
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
async fn what_the_engine_adds_it_can_also_take_back() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // ---- 1. A ⊂ B ⊂ C ⟹ A ⊂ C
        let ab = assert_fact(&pool, &f, f.a, f.b, None).await?;
        let bc = assert_fact(&pool, &f, f.b, f.c, None).await?;
        let r = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(r.rules, 1, "一条 transitive 规则");
        assert_eq!(r.edges, 2);
        assert_eq!(r.inserted, 1);
        assert_eq!(live_derived(&pool, f.kb).await?, vec![(f.a, f.c)]);

        // Proof: the two premises, in order
        let derived: Uuid = sqlx::query_scalar("SELECT id FROM derived_facts WHERE kb_id = $1")
            .bind(f.kb)
            .fetch_one(&pool)
            .await?;
        let premises: Vec<Uuid> = sqlx::query_scalar(
            "SELECT premise_fact_id FROM fact_derivations
              WHERE derived_fact_id = $1 ORDER BY seq",
        )
        .bind(derived)
        .fetch_all(&pool)
        .await?;
        assert_eq!(premises, vec![ab, bc], "证明要按推导顺序记直接前提");

        // ---- 2. Rerun is idempotent, the rule id is unchanged
        let rule_before: Uuid = sqlx::query_scalar("SELECT id FROM rules WHERE kb_id = $1")
            .bind(f.kb)
            .fetch_one(&pool)
            .await?;
        let again = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(again.inserted, 0, "同一条派生第二次跑不该再插一份");
        assert_eq!(again.invalidated, 0);
        let rule_after: Uuid = sqlx::query_scalar("SELECT id FROM rules WHERE kb_id = $1")
            .bind(f.kb)
            .fetch_one(&pool)
            .await?;
        assert_eq!(rule_before, rule_after, "重编译要认得出还是那条规则");

        // ---- 3. Assertion first: when A ⊂ C is also asserted, the derived one should yield
        assert_fact(&pool, &f, f.a, f.c, None).await?;
        let asserted = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(asserted.derived, 0, "断言过的三元组不该再派生");
        assert_eq!(asserted.invalidated, 1, "此前派生的那条要作废");
        assert!(live_derived(&pool, f.kb).await?.is_empty());
        // Voiding is not deleting — it stays on the record axis
        let ghost: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM derived_facts
              WHERE kb_id = $1 AND invalidated_at IS NOT NULL",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        assert_eq!(ghost, 1, "派生失效要留痕，与拒绝一条事实同构");

        // ---- 4. Premise retracted, derivation follows
        sqlx::query(
            "UPDATE facts SET invalidated_at = now() WHERE subject_id = $1 AND object_id = $2",
        )
        .bind(f.a)
        .bind(f.c)
        .execute(&pool)
        .await?;
        let back = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(back.inserted, 1, "断言撤了，派生该回来");
        sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
            .bind(bc)
            .execute(&pool)
            .await?;
        let gone = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(gone.invalidated, 1, "前提没了，派生必须跟着失效");
        assert!(live_derived(&pool, f.kb).await?.is_empty());

        // ---- 5. Precision: the intersection makes the end unbounded, so that end's precision must be cleared as well,
        // otherwise it hits facts_to_precision_matches_date
        sqlx::query("UPDATE facts SET invalidated_at = NULL WHERE id = $1")
            .bind(bc)
            .execute(&pool)
            .await?;
        sqlx::query(
            "UPDATE facts SET valid_from = '2020-01-01'::timestamptz,
                                      valid_from_precision = 'year' WHERE id = $1",
        )
        .bind(ab)
        .execute(&pool)
        .await?;
        sqlx::query(
            "UPDATE facts SET valid_from = '2022-06-01'::timestamptz,
                                      valid_from_precision = 'day' WHERE id = $1",
        )
        .bind(bc)
        .execute(&pool)
        .await?;
        let dated = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(dated.inserted, 1);
        let (from, fp, tp): (
            Option<chrono::DateTime<chrono::Utc>>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT valid_from, valid_from_precision, valid_to_precision FROM derived_facts
              WHERE kb_id = $1 AND invalidated_at IS NULL",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            from.map(|x| x.format("%Y-%m-%d").to_string()).as_deref(),
            Some("2022-06-01"),
            "交集取两个起点里晚的那个"
        );
        // The precision follows the premise that wins that end (0024): the start is b ⊂ c's 1 June, so the precision is
        // day. Previously the coarsest of all premises was taken — year marked on a 1 June value, the value and its label contradicting each other
        // which the database CHECK now also refuses
        assert_eq!(
            fp.as_deref(),
            Some("day"),
            "精度跟赢下这一端的前提走，不是最粗的那个"
        );
        assert_eq!(tp, None, "结束端无界，精度必须是空");

        // ---- 6. Axiom retracted: the facts derived from it are voided, and the rule row is **kept**
        sqlx::query("UPDATE relation_types SET is_transitive = FALSE WHERE id = $1")
            .bind(f.part_of)
            .execute(&pool)
            .await?;
        let blind = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(blind.rules, 0, "没有公理就编不出规则");
        assert_eq!(blind.invalidated, 1, "据它推出来的事实要作废");
        assert!(live_derived(&pool, f.kb).await?.is_empty());
        let rules_left: i64 = sqlx::query_scalar("SELECT count(*) FROM rules WHERE kb_id = $1")
            .bind(f.kb)
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            rules_left, 1,
            "规则行留着——刚作废的那些派生仍指着它，解释「当时靠哪条规则推的」需要它还在"
        );
        // Axiom added back: the derivation must come back too (the rule id is still the same one)
        sqlx::query("UPDATE relation_types SET is_transitive = TRUE WHERE id = $1")
            .bind(f.part_of)
            .execute(&pool)
            .await?;
        let revived = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(revived.rules, 1);
        assert_eq!(revived.inserted, 1, "公理回来，推导也回来");
        let rule_now: Uuid = sqlx::query_scalar("SELECT id FROM rules WHERE kb_id = $1")
            .bind(f.kb)
            .fetch_one(&pool)
            .await?;
        assert_eq!(rule_now, rule_before, "撤了又加回来，还是同一条规则");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
