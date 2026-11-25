//! A plan step's premise is written as a business rule (0021); observed changes land on the premise; derivation (0002 R1) makes the steps that depend
//! on the old value retire, unrelated steps stay put, and history is kept on both axes (#875).
//!
//! Step S_A "take cup-7 from the desk" has the premise that cup-7's `location` is desk; S_B "take
//! box-3 from the shelf" reads box-3's own `location`, as a control. The conclusion only says the modeled semantic premise holds; it does not guarantee the action is
//! safe or successful.
//!
//! The first two tests bypass extraction and alignment: attribute facts go through the gate of the typed graph (`graph::insert_value_fact`),
//! and are then reconciled along the uniqueness direction — the same path as a person confirming a remembered fact (`pending::confirm`). The third
//! starts from an open statement, goes through an explicit binding, typed materialization and an explicit timeline reconciliation, and verifies the subsequent retraction of the derivation.
//! All three tests use the real store path, depend on no model or HTTP replay, and do not claim that the automatic alignment loop is closed.
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgPool;
use utopia_core::models::RelationAxioms;
use utopia_store::business_rules::{self, ConditionInput};
use utopia_store::graph::{self, FactObject, Validity};
use utopia_store::phrase_bindings::{self, Decision, PhraseSignature};
use utopia_store::temporal::{self, Uniqueness};
use utopia_store::{materialize, ontology, reasoning};
use uuid::Uuid;

/// World time. t0 is before everything, arriving late; t1 is the initial observation; an occlusion lies between t1 and t2; t2 is the move;
/// t_end is the moment a person closed B's span; NOW is the world time at "now"
const T0: &str = "2026-09-23T07:50:00Z";
const T1: &str = "2026-09-23T08:00:00Z";
const T_OCC: &str = "2026-09-23T08:05:00Z";
const T_MID: &str = "2026-09-23T08:07:00Z";
const T2: &str = "2026-09-23T08:10:00Z";
const T_END: &str = "2026-09-23T08:30:00Z";
const NOW: &str = "2026-09-23T09:00:00Z";

fn t(s: &str) -> DateTime<Utc> {
    s.parse().expect("fixed timestamp")
}

struct Fixture {
    org: Uuid,
    kb: Uuid,
    cup: Uuid,
    boxed: Uuid,
    sa_ready: Uuid,
    sb_ready: Uuid,
    location: Uuid,
    rfid_zone: Uuid,
    visibility: Uuid,
    a: Uuid,
    b: Uuid,
}

async fn seed(pool: &PgPool, name: &str) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(name)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(org)
        .bind(name)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3)")
        .bind(kb)
        .bind(ws)
        .bind(name)
        .execute(pool)
        .await?;
    let class = |key: &'static str, label: &'static str| {
        let pool = pool.clone();
        async move {
            ontology::create_entity_type(&pool, kb, key, label, "#7fd0ff", "circle", &[], "").await
        }
    };
    let cup = class("cup", "Cup").await?;
    let boxed = class("box", "Box").await?;
    // Steps executable are written as a classification conclusion: rules only infer types or attributes, and no syntax is added for this experiment (0021)
    let sa_ready = class("step_sa_ready", "S_A precondition holds").await?;
    let sb_ready = class("step_sb_ready", "S_B precondition holds").await?;
    let attribute = |key: &'static str, functional: bool| {
        let pool = pool.clone();
        async move {
            ontology::create_relation_type(
                &pool,
                kb,
                key,
                key,
                "state",
                RelationAxioms {
                    functional,
                    ..Default::default()
                },
                "",
                "attribute",
                &[cup, boxed],
                &[],
                Some("text"),
                None,
            )
            .await
        }
    };
    // One thing is in one place at a time: declare functional, so the timeline lets the later place close the earlier one
    let location = attribute("location", true).await?;
    let rfid_zone = attribute("rfid_zone", false).await?;
    let visibility = attribute("visibility", false).await?;
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    for (id, type_id, name) in [(a, cup, "cup-7"), (b, boxed, "box-3")] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(type_id)
        .bind(name)
        .execute(pool)
        .await?;
    }
    Ok(Fixture {
        org,
        kb,
        cup,
        boxed,
        sa_ready,
        sb_ready,
        location,
        rfid_zone,
        visibility,
        a,
        b,
    })
}

async fn cleanup(pool: &PgPool, org: Uuid) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(pool)
        .await?;
    Ok(())
}

/// A step rule: the `location` (or another attribute) of main class `class` falls within `places` → classified as `ready`.
/// `groups` are the groups joined by "or" between groups, each group a condition (0029)
async fn step_rule(
    pool: &PgPool,
    f: &Fixture,
    name: &str,
    class: Uuid,
    ready: Uuid,
    groups: &[(Uuid, &str)],
) -> anyhow::Result<Uuid> {
    let conditions: Vec<ConditionInput> = groups
        .iter()
        .enumerate()
        .map(|(g, (predicate, place))| ConditionInput {
            group: g as i32,
            predicate_id: *predicate,
            op: "in".into(),
            operand: Some(json!([place])),
            side: "x".into(),
        })
        .collect();
    Ok(business_rules::create(
        pool,
        f.kb,
        name,
        "the modelled precondition of a plan step; not a safety or success guarantee",
        class,
        "typing",
        Some(ready),
        None,
        None,
        None,
        None,
        &conditions,
    )
    .await?)
}

/// One observation: attribute facts go through the gate of the typed graph, and the state relation with uniqueness is reconciled afterwards — the same order
/// as `pending::confirm` storing one fact
async fn observe(
    pool: &PgPool,
    f: &Fixture,
    subject: Uuid,
    predicate: Uuid,
    value: &str,
    at: &str,
) -> anyhow::Result<Uuid> {
    let validity = Validity {
        from: Some(t(at)),
        from_precision: Some("second"),
        attested_at: Some(t(at)),
        ..Default::default()
    };
    let value = json!({ "value": value });
    let (id, _) =
        graph::insert_value_fact(pool, f.kb, subject, Some(predicate), &value, validity, 1.0)
            .await?;
    let (functional, temporal): (bool, String) =
        sqlx::query_as("SELECT functional, temporal FROM relation_types WHERE id = $1")
            .bind(predicate)
            .fetch_one(pool)
            .await?;
    if functional && temporal == "state" {
        temporal::reconcile_new_fact(
            pool,
            f.kb,
            id,
            subject,
            predicate,
            None,
            Some(&value),
            Uniqueness::SubjectSide,
            validity,
            1.0,
        )
        .await?;
    }
    Ok(id)
}

#[derive(Debug, sqlx::FromRow)]
struct Row {
    id: Uuid,
    valid_from: Option<DateTime<Utc>>,
    valid_to: Option<DateTime<Utc>>,
    valid_to_precision: Option<String>,
}

/// The rows alive at this moment (subject, attribute, value)
async fn live_rows(
    pool: &PgPool,
    f: &Fixture,
    subject: Uuid,
    predicate: Uuid,
    value: &str,
) -> anyhow::Result<Vec<Row>> {
    Ok(sqlx::query_as(
        "SELECT id, valid_from, valid_to, valid_to_precision FROM facts
          WHERE kb_id = $1 AND subject_id = $2 AND predicate_id = $3
            AND object_value = $4 AND invalidated_at IS NULL
          ORDER BY valid_from NULLS FIRST, recorded_at",
    )
    .bind(f.kb)
    .bind(subject)
    .bind(predicate)
    .bind(json!({ "value": value }))
    .fetch_all(pool)
    .await?)
}

/// The conclusions one rule holds on an entity at this moment (record axis = now), sorted by world start
async fn conclusions(
    pool: &PgPool,
    f: &Fixture,
    entity: Uuid,
    rule: Uuid,
) -> anyhow::Result<Vec<Row>> {
    Ok(sqlx::query_as(
        "SELECT id, valid_from, valid_to, valid_to_precision FROM derived_facts
          WHERE kb_id = $1 AND subject_id = $2 AND attribute_rule_id = $3
            AND invalidated_at IS NULL
          ORDER BY valid_from, id",
    )
    .bind(f.kb)
    .bind(entity)
    .bind(rule)
    .fetch_all(pool)
    .await?)
}

/// Whether the rule holds on this entity at world moment `at` — the same function that the entity panel and MCP `entity_facts` read
async fn holds_at(
    pool: &PgPool,
    f: &Fixture,
    entity: Uuid,
    rule: Uuid,
    at: &str,
    as_of: Option<DateTime<Utc>>,
) -> anyhow::Result<bool> {
    Ok(
        reasoning::derived_for_entity(pool, f.kb, entity, Some(t(at)), as_of)
            .await?
            .iter()
            .any(|d| d.attribute_rule_id == Some(rule) && d.subject_id == entity),
    )
}

/// A premise of a derivation (`fact_derivations`, by seq)
async fn premises(pool: &PgPool, derived: Uuid) -> anyhow::Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT premise_fact_id FROM fact_derivations
          WHERE derived_fact_id = $1 AND premise_fact_id IS NOT NULL ORDER BY seq",
    )
    .bind(derived)
    .fetch_all(pool)
    .await?)
}

/// A moment on the record axis: the store's own clock, so that the clocks of two containers do not drift apart
async fn db_now(pool: &PgPool) -> anyhow::Result<DateTime<Utc>> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pool)
        .await?)
}

/// Initial → repeated → occluded → moved → late-arriving old observation → explicit end → retracted: one timeline runs through all of them
#[tokio::test]
async fn a_step_leaves_with_its_premise_and_the_other_step_stays() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "issue875-step-premise").await?;

    let run = async {
        let rule_a = step_rule(
            &pool,
            &f,
            "S_A pick cup from desk",
            f.cup,
            f.sa_ready,
            &[(f.location, "desk")],
        )
        .await?;
        let rule_b = step_rule(
            &pool,
            &f,
            "S_B pick box from shelf",
            f.boxed,
            f.sb_ready,
            &[(f.location, "shelf")],
        )
        .await?;

        // ---- Initial observation: A is on the desk, B is on the shelf
        let a_desk = observe(&pool, &f, f.a, f.location, "desk", T1).await?;
        let b_shelf = observe(&pool, &f, f.b, f.location, "shelf", T1).await?;
        let r = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(
            (r.attribute_rules, r.rule_hits, r.inserted, r.invalidated),
            (2, 2, 2, 0),
            "{r:?}"
        );
        let sa = conclusions(&pool, &f, f.a, rule_a).await?;
        let sb = conclusions(&pool, &f, f.b, rule_b).await?;
        assert_eq!((sa.len(), sb.len()), (1, 1));
        assert_eq!(sa[0].valid_from, Some(t(T1)));
        assert_eq!(sa[0].valid_to, None, "前提还开着，结论也开着");
        // The premise can be located: the derivation points to that reading, proving it can be read out
        assert_eq!(premises(&pool, sa[0].id).await?, vec![a_desk]);
        assert_eq!(premises(&pool, sb[0].id).await?, vec![b_shelf]);
        let proof = reasoning::proof(&pool, f.kb, sa[0].id)
            .await?
            .expect("a live conclusion has a proof");
        assert_eq!(proof.steps.len(), 1);
        assert_eq!(proof.steps[0].fact_id, a_desk);
        assert!(!proof.steps[0].retracted);
        let (matches, total) = business_rules::matches(&pool, f.kb, rule_a, 20, 0).await?;
        assert_eq!(total, 1);
        assert_eq!(matches[0]["entity_id"], json!(f.a));
        assert!(holds_at(&pool, &f, f.a, rule_a, NOW, None).await?);
        assert!(holds_at(&pool, &f, f.b, rule_b, NOW, None).await?);

        // ---- The same observation again: same assertion, same start reuses that row, and derivation has nothing to do
        assert_eq!(
            observe(&pool, &f, f.a, f.location, "desk", T1).await?,
            a_desk
        );
        let r = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!((r.inserted, r.invalidated, r.reproved), (0, 0, 0), "{r:?}");

        // ---- Occlusion: nobody writes location, another attribute records "not visible". Not seen ≠ not on the desk
        observe(&pool, &f, f.a, f.visibility, "occluded", T_OCC).await?;
        let r = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!((r.inserted, r.invalidated), (0, 0), "{r:?}");
        assert_eq!(conclusions(&pool, &f, f.a, rule_a).await?[0].id, sa[0].id);
        assert!(holds_at(&pool, &f, f.a, rule_a, NOW, None).await?);

        // ---- Move: A is seen directly on the shelf. The timeline closes the desk span at t2 (void + rewrite)
        let before_move = db_now(&pool).await?;
        observe(&pool, &f, f.a, f.location, "shelf", T2).await?;
        let desk = live_rows(&pool, &f, f.a, f.location, "desk").await?;
        assert_eq!(desk.len(), 1);
        assert_ne!(desk[0].id, a_desk, "关上的是改写出来的新行，旧行作废");
        assert_eq!(desk[0].valid_to, Some(t(T2)));
        let r = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!((r.inserted, r.invalidated), (1, 1), "{r:?}");
        assert!(
            !holds_at(&pool, &f, f.a, rule_a, NOW, None).await?,
            "S_A 在现在不再成立"
        );
        assert!(
            holds_at(&pool, &f, f.a, rule_a, T_MID, None).await?,
            "t1..t2 之间它成立过：世界轴上的历史"
        );
        let sa_now = conclusions(&pool, &f, f.a, rule_a).await?;
        assert_eq!(sa_now.len(), 1);
        assert_eq!(
            (sa_now[0].valid_from, sa_now[0].valid_to),
            (Some(t(T1)), Some(t(T2)))
        );
        assert_eq!(premises(&pool, sa_now[0].id).await?, vec![desk[0].id]);
        // The old conclusion is voided, not deleted; on the record axis rolled back to before the move it still exists, and the premise chain still points to the reading of that time
        let (old_invalidated,): (Option<DateTime<Utc>>,) =
            sqlx::query_as("SELECT invalidated_at FROM derived_facts WHERE id = $1")
                .bind(sa[0].id)
                .fetch_one(&pool)
                .await?;
        assert!(old_invalidated.is_some());
        assert_eq!(premises(&pool, sa[0].id).await?, vec![a_desk]);
        assert!(
            holds_at(&pool, &f, f.a, rule_a, NOW, Some(before_move)).await?,
            "当时所知：移动之前的库认为 S_A 至今成立"
        );
        // The unrelated step is unchanged: same row, not voided
        let sb_after = conclusions(&pool, &f, f.b, rule_b).await?;
        assert_eq!(sb_after.len(), 1);
        assert_eq!(sb_after[0].id, sb[0].id);
        assert!(holds_at(&pool, &f, f.b, rule_b, NOW, None).await?);

        // ---- Late-arriving old observation: at t0 A was on the desk, but it arrived only after the move. The timeline sorts by world time,
        //      it stops at t2 and does not let the desk come back to life at "now"
        observe(&pool, &f, f.a, f.location, "desk", T0).await?;
        reasoning::materialize(&pool, f.kb).await?;
        assert!(!holds_at(&pool, &f, f.a, rule_a, NOW, None).await?);
        assert!(holds_at(&pool, &f, f.a, rule_a, "2026-09-23T07:55:00Z", None).await?);
        let a_rows_before_end: Vec<Uuid> = conclusions(&pool, &f, f.a, rule_a)
            .await?
            .into_iter()
            .map(|r| r.id)
            .collect();

        // ---- Explicit end: the segment with B on the shelf stops at t_end (the step where a person closes it in Review)
        let b_rows = live_rows(&pool, &f, f.b, f.location, "shelf").await?;
        assert_eq!(b_rows.len(), 1);
        temporal::close_superseded(&pool, b_rows[0].id, t(T_END), "second").await?;
        let r = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!((r.inserted, r.invalidated), (1, 1), "{r:?}");
        assert!(!holds_at(&pool, &f, f.b, rule_b, NOW, None).await?);
        assert!(holds_at(&pool, &f, f.b, rule_b, T_MID, None).await?);
        let a_rows_after_end: Vec<Uuid> = conclusions(&pool, &f, f.a, rule_a)
            .await?
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(a_rows_after_end, a_rows_before_end, "关 B 不碰 A 的结论");

        // ---- Retraction: B's reading is judged a misreading. The whole conclusion is voided, with no replacement; the proof can still read what it relied on at the time
        let b_closed = live_rows(&pool, &f, f.b, f.location, "shelf").await?;
        let sb_closed = conclusions(&pool, &f, f.b, rule_b).await?;
        assert_eq!((b_closed.len(), sb_closed.len()), (1, 1));
        graph::reject_fact(&pool, f.kb, b_closed[0].id).await?;
        let r = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!((r.inserted, r.invalidated), (0, 1), "{r:?}");
        assert!(conclusions(&pool, &f, f.b, rule_b).await?.is_empty());
        assert!(!holds_at(&pool, &f, f.b, rule_b, T_MID, None).await?);
        let history = reasoning::proof(&pool, f.kb, sb_closed[0].id)
            .await?
            .expect("an invalidated conclusion keeps its proof");
        assert!(history.derived.invalidated_at.is_some());
        assert_eq!(history.steps[0].fact_id, b_closed[0].id);
        assert!(history.steps[0].retracted, "撤掉的前提照样列出并打上标记");
        anyhow::Ok(())
    }
    .await;

    cleanup(&pool, f.org).await?;
    run
}

/// One step has two independent proofs (or between groups): if one is retracted and still holds, the proof becomes the other;
/// it retires only when the last one is also gone
#[tokio::test]
async fn a_step_with_two_proofs_stays_until_its_last_support_goes() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "issue875-two-proofs").await?;

    let run = async {
        let rule = step_rule(
            &pool,
            &f,
            "S_A pick cup from desk, either sensor",
            f.cup,
            f.sa_ready,
            &[(f.location, "desk"), (f.rfid_zone, "desk")],
        )
        .await?;
        let seen = observe(&pool, &f, f.a, f.location, "desk", T1).await?;
        let tagged = observe(&pool, &f, f.a, f.rfid_zone, "desk", T1).await?;
        let r = reasoning::materialize(&pool, f.kb).await?;
        // Two groups derive the same interval, which lands as one row; the proof kept is the one whose group order comes first (0029)
        assert_eq!((r.rule_hits, r.inserted), (1, 1), "{r:?}");
        let step = conclusions(&pool, &f, f.a, rule).await?;
        assert_eq!(step.len(), 1);
        assert_eq!(premises(&pool, step[0].id).await?, vec![seen]);

        graph::reject_fact(&pool, f.kb, seen).await?;
        let r = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!(
            (r.inserted, r.invalidated, r.reproved),
            (0, 0, 1),
            "结论没变、理由变了：同一行重写证明（0030）{r:?}"
        );
        let still = conclusions(&pool, &f, f.a, rule).await?;
        assert_eq!(still.len(), 1);
        assert_eq!(still[0].id, step[0].id);
        assert_eq!(premises(&pool, still[0].id).await?, vec![tagged]);
        assert!(holds_at(&pool, &f, f.a, rule, NOW, None).await?);

        graph::reject_fact(&pool, f.kb, tagged).await?;
        let r = reasoning::materialize(&pool, f.kb).await?;
        assert_eq!((r.inserted, r.invalidated), (0, 1), "{r:?}");
        assert!(conclusions(&pool, &f, f.a, rule).await?.is_empty());
        assert!(!holds_at(&pool, &f, f.a, rule, NOW, None).await?);
        anyhow::Ok(())
    }
    .await;

    cleanup(&pool, f.org).await?;
    run
}

// ===================== Starting from an open statement: the alignment segment =====================

/// A dated document and one of its chunks (a pushed statement is exactly such a document, `doc_time_source = 'source'`)
async fn document(
    pool: &PgPool,
    f: &Fixture,
    name: &str,
    doc_time: &str,
    text: &str,
) -> anyhow::Result<(Uuid, Uuid)> {
    let (doc, chunk) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, doc_time, doc_time_source)
         VALUES ($1, $2, $3, $4, $5, 'source')",
    )
    .bind(doc)
    .bind(f.kb)
    .bind(name)
    .bind(format!("{name}-{doc}"))
    .bind(t(doc_time))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, $4)",
    )
    .bind(chunk)
    .bind(f.kb)
    .bind(doc)
    .bind(text)
    .execute(pool)
    .await?;
    Ok((doc, chunk))
}

/// One open statement "cup-7 —is on→ <place>", with evidence being that chunk and no quote (0054 decision 4)
async fn statement(
    pool: &PgPool,
    f: &Fixture,
    chunk: Uuid,
    place: &str,
    attested: &str,
) -> anyhow::Result<Uuid> {
    let value = json!({ "value": place });
    let (id, _) = graph::insert_open_statement(
        pool,
        f.kb,
        f.a,
        "is on",
        FactObject::Value(&value),
        Some(t(attested)),
        1.0,
    )
    .await?;
    graph::add_evidence_located(pool, id, chunk, None, Some("is on"), None).await?;
    Ok(id)
}

/// A person binds the signature (is on × cup × value) to `location`, through the same function as the one clicked in Review
async fn bind_is_on(pool: &PgPool, f: &Fixture) -> anyhow::Result<()> {
    let sig = PhraseSignature {
        phrase: "is on".into(),
        subject_type_id: Some(f.cup),
        subject_type_key: Some("cup".into()),
        object_type_id: None,
        object_type_key: None,
        object_is_value: true,
        count: 1,
        examples: Vec::new(),
        quotes: Vec::new(),
        example_statement_ids: Vec::new(),
    };
    let written = phrase_bindings::decide(
        pool,
        f.kb,
        &sig,
        Decision {
            relation_type_id: Some(f.location),
            direction: Some("forward"),
            status: "bound",
            votes: &json!({ "person": { "property": "location", "direction": "forward" } }),
            decided_by: "person",
            basis: None,
            marks: None,
            marks_asked: false,
        },
    )
    .await?;
    assert!(written);
    Ok(())
}

/// In typed rows, the row for A's location = desk (live)
async fn typed_desk(pool: &PgPool, f: &Fixture) -> anyhow::Result<Vec<Row>> {
    Ok(sqlx::query_as(
        "SELECT id, valid_from, valid_to, valid_to_precision FROM facts
          WHERE kb_id = $1 AND layer = 'typed' AND subject_id = $2 AND predicate_id = $3
            AND object_value = '{\"value\": \"desk\"}'::jsonb AND invalidated_at IS NULL",
    )
    .bind(f.kb)
    .bind(f.a)
    .bind(f.location)
    .fetch_all(pool)
    .await?)
}

/// Each of the two observations is one document (one observation per identity). Materialization reconciles on the way, the desk span closes at the second document's date,
/// and the step retires with it — `POST /kbs/{id}/ontology/relation-types/{type_id}/reconcile` is that step
#[tokio::test]
async fn an_explicit_reconcile_closes_the_earlier_place_and_the_step_leaves() -> anyhow::Result<()>
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "issue875-reconcile").await?;

    let run = async {
        let rule = step_rule(
            &pool,
            &f,
            "S_A pick cup from desk",
            f.cup,
            f.sa_ready,
            &[(f.location, "desk")],
        )
        .await?;
        let (_, c1) = document(&pool, &f, "obs-1.json", T1, "cup-7 is on desk").await?;
        let (_, c2) = document(&pool, &f, "obs-2.json", T2, "cup-7 is on shelf").await?;
        statement(&pool, &f, c1, "desk", T1).await?;
        statement(&pool, &f, c2, "shelf", T2).await?;
        bind_is_on(&pool, &f).await?;
        let typed = materialize::materialize(&pool, f.kb).await?;
        // Materialization already reconciled (#899): the desk span is closed here. An explicit reconcile is still available, it just has nothing left
        // to change
        assert_eq!((typed.added, typed.corrected), (2, 1), "{typed:?}");

        let report = temporal::reconcile_predicate(&pool, f.kb, f.location).await?;
        assert_eq!(
            (report.corrected.len(), report.conflicts),
            (0, 0),
            "nothing left for the explicit reconcile: {report:?}"
        );
        let desk = typed_desk(&pool, &f).await?;
        assert_eq!(desk.len(), 1);
        assert_eq!(desk[0].valid_from, None, "没有模型读时间词：起点留空");
        assert_eq!(
            desk[0].valid_to_precision.as_deref(),
            Some("unknown"),
            "没起点的后任：前任写成「结束了，不知哪天」，锚在后任那份文档的日期上"
        );
        reasoning::materialize(&pool, f.kb).await?;
        assert!(!holds_at(&pool, &f, f.a, rule, NOW, None).await?);
        assert!(holds_at(&pool, &f, f.a, rule, T_MID, None).await?);
        anyhow::Ok(())
    }
    .await;

    cleanup(&pool, f.org).await?;
    run
}

/// #899: each of the two observations is one document; after binding, **materialization itself** closes the desk span, with no need for an explicit reconcile —
/// the rows materialization produces are new observations, as those written by extraction and confirmation are, and after writing they recompute along the uniqueness timeline
#[tokio::test]
async fn a_later_bound_statement_closes_the_earlier_place() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "issue899-materialize-timeline").await?;

    let run = async {
        let rule = step_rule(
            &pool,
            &f,
            "S_A pick cup from desk",
            f.cup,
            f.sa_ready,
            &[(f.location, "desk")],
        )
        .await?;
        let (_, c1) = document(&pool, &f, "obs-1.json", T1, "cup-7 is on desk").await?;
        let (_, c2) = document(&pool, &f, "obs-2.json", T2, "cup-7 is on shelf").await?;
        statement(&pool, &f, c1, "desk", T1).await?;
        statement(&pool, &f, c2, "shelf", T2).await?;
        bind_is_on(&pool, &f).await?;
        let typed = materialize::materialize(&pool, f.kb).await?;
        assert_eq!(
            (typed.added, typed.corrected, typed.conflicts),
            (2, 1, 0),
            "{typed:?}"
        );
        let desk = typed_desk(&pool, &f).await?;
        assert_eq!(desk.len(), 1);
        assert_eq!(
            desk[0].valid_to_precision.as_deref(),
            Some("unknown"),
            "location is functional and a later place was materialized: the desk row is closed \
             without an explicit reconcile: {desk:?}"
        );
        // Running again is a no-op: no new rows, and no reconcile
        let again = materialize::materialize(&pool, f.kb).await?;
        assert_eq!(
            (again.added, again.merged, again.corrected, again.conflicts),
            (0, 0, 0, 0),
            "{again:?}"
        );
        reasoning::materialize(&pool, f.kb).await?;
        assert!(!holds_at(&pool, &f, f.a, rule, NOW, None).await?);
        assert!(holds_at(&pool, &f, f.a, rule, T_MID, None).await?);
        anyhow::Ok(())
    }
    .await;

    cleanup(&pool, f.org).await?;
    run
}

/// #900: the same identity pushes new content again (replaced in place, version recorded, `doc_time` replaced with the new one, and the new chunk supersedes the old one).
/// Evidence that stopped on the old version is dated by **that version's** date, so the later observation closes the earlier span, instead of being recorded as
/// A "simultaneous" conflict
#[tokio::test]
async fn a_same_identity_update_closes_the_earlier_place() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "issue900-same-identity").await?;

    let run = async {
        let (doc, c1) = document(&pool, &f, "cup-7.json", T1, "cup-7 is on desk").await?;
        // A document written directly into the store has no version row: the first version is backfilled with today's date, as the ingest path writes it
        utopia_store::documents::record_version(&pool, doc, "cup-7-v1", 17).await?;
        statement(&pool, &f, c1, "desk", T1).await?;
        bind_is_on(&pool, &f).await?;
        let first = materialize::materialize(&pool, f.kb).await?;
        assert_eq!(first.added, 1, "{first:?}");

        // Second push: replaced in place, version recorded, doc_time replaced with the new one (that step of ingest_item), and the new chunk supersedes the old one
        utopia_store::documents::replace_content_and_enqueue_processing(
            &pool,
            doc,
            "cup-7.json",
            "application/json",
            17,
            "cup-7-v2",
            Some(t(T2)),
        )
        .await?;
        // The processing task queued by that step does not run here: the extraction result is written by the lines below
        sqlx::query("DELETE FROM jobs WHERE payload->>'document_id' = $1")
            .bind(doc.to_string())
            .execute(&pool)
            .await?;
        let text = "cup-7 is on shelf".to_string();
        let piece = utopia_ingest::ChunkPiece {
            seq: 0,
            char_start: 0,
            char_end: text.chars().count() as i32,
            heading: None,
            provenance: utopia_ingest::Provenance::stated(),
            text: text.clone(),
        };
        utopia_store::documents::replace_chunks(&pool, f.kb, doc, &[piece]).await?;
        let (c2,): (Uuid,) = sqlx::query_as(
            "SELECT id FROM chunks WHERE document_id = $1 AND superseded_at IS NULL",
        )
        .bind(doc)
        .fetch_one(&pool)
        .await?;
        statement(&pool, &f, c2, "shelf", T2).await?;
        let second = materialize::materialize(&pool, f.kb).await?;
        assert_eq!(
            (second.added, second.corrected, second.conflicts),
            (1, 1, 0),
            "the desk row dates at version 1's time, the shelf row at version 2's: {second:?}"
        );
        let desk = typed_desk(&pool, &f).await?;
        assert_eq!(desk.len(), 1);
        assert_eq!(
            desk[0].valid_to_precision.as_deref(),
            Some("unknown"),
            "after a same-identity update the desk row is closed: {desk:?}"
        );
        // The version table records each one's date
        let dates: Vec<(i32, Option<DateTime<Utc>>)> = sqlx::query_as(
            "SELECT version, doc_time FROM document_versions WHERE document_id = $1 ORDER BY version",
        )
        .bind(doc)
        .fetch_all(&pool)
        .await?;
        assert_eq!(dates, vec![(1, Some(t(T1))), (2, Some(t(T2)))]);
        anyhow::Ok(())
    }
    .await;

    cleanup(&pool, f.org).await?;
    run
}
