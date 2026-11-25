//! One category word bound to one class (the first slice of 0044 decisions 3-4, ledger side, see 0065).
//!
//! Open extraction records only the document's own category words, and does not choose a class. The library must be able to count each category word's signature ("Company" and
//! "company" are one word: two spellings, two entities, and the relation phrases they take part in). Once a binding is judged, it is written to the `type_id` of every
//! entity under that category word (`type_source = 'aligned'`); anything a person decided is left alone. A binding goes stale when the class's
//! `updated_at` is compared against the newest class in the library. A person's judgment is never overwritten by an agent, though the reverse is allowed. Unbinding only touches
//! `aligned` rows. Words with no matching class fall through to the old flow and become a "suggest adding a class".
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. It creates and tears down its own data and never touches an existing database.

use sqlx::PgPool;
use std::collections::HashSet;
use std::time::Duration;
use utopia_store::graph::{self, FactObject};
use utopia_store::{ontology, type_bindings};
use uuid::Uuid;

const ORG: &str = "kind-word-binding-test";
const PROPOSAL: &str = "the stockholder proposal on declassifying the board";

struct Fixture {
    kb: Uuid,
    organization: Uuid,
    person: Uuid,
    acme: Uuid,
    beta: Uuid,
    carol: Uuid,
    dave: Uuid,
    proposal: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (organization, person) = (Uuid::now_v7(), Uuid::now_v7());
    let (acme, beta, carol, dave, proposal) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(ORG)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(org)
        .bind(ORG)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3)")
        .bind(kb)
        .bind(ws)
        .bind(ORG)
        .execute(pool)
        .await?;
    for (id, key, label) in [
        (organization, "organization", "Organization"),
        (person, "person", "Person"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    // Named ones: the category word as the document writes it, class left empty
    for (id, name, kind) in [
        (acme, "Acme", "Company"),
        (beta, "Beta", "company"),
        (carol, "Carol", "person"),
    ] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, canonical_name, specific_type)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(name)
        .bind(kind)
        .execute(pool)
        .await?;
    }
    // A person looked at it and said no suitable class: type_id empty, type_source = human
    sqlx::query(
        "INSERT INTO entities (id, kb_id, canonical_name, specific_type, type_source)
         VALUES ($1, $2, 'Dave', 'person', 'human')",
    )
    .bind(dave)
    .bind(kb)
    .execute(pool)
    .await?;
    // A described thing with no name
    sqlx::query(
        "INSERT INTO entities (id, kb_id, canonical_name, description, specific_type)
         VALUES ($1, $2, $3, $3, 'stockholder proposal')",
    )
    .bind(proposal)
    .bind(kb)
    .bind(PROPOSAL)
    .execute(pool)
    .await?;
    // Several open statements: the subject is the company's
    for (subject, phrase, object) in [
        (acme, "acquired", FactObject::Entity(beta)),
        (
            acme,
            "acquired",
            FactObject::Value(&serde_json::json!({ "value": "a rival" })),
        ),
        (
            beta,
            "is headquartered in",
            FactObject::Value(&serde_json::json!({ "value": "Austin" })),
        ),
    ] {
        graph::insert_open_statement(pool, kb, subject, phrase, object, None, 0.9).await?;
    }
    Ok(Fixture {
        kb,
        organization,
        person,
        acme,
        beta,
        carol,
        dave,
        proposal,
    })
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct Typed {
    type_id: Option<Uuid>,
    type_source: String,
    proposed_type: Option<String>,
}

async fn typed(pool: &PgPool, id: Uuid) -> anyhow::Result<Typed> {
    Ok(
        sqlx::query_as("SELECT type_id, type_source, proposed_type FROM entities WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

/// The two `now()` calls must be told apart: staleness goes by which timestamp is later
async fn tick() {
    tokio::time::sleep(Duration::from_millis(5)).await;
}

#[tokio::test]
async fn a_kind_word_is_counted_once_bound_once_and_applied_to_its_entities() -> anyhow::Result<()>
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(ORG)
        .execute(&pool)
        .await?;
    let f = seed(&pool).await?;

    let run = async {
        // 1. Signature: "Company" and "company" are one word
        let sigs = type_bindings::signatures(&pool, f.kb).await?;
        let words: Vec<&str> = sigs.iter().map(|s| s.kind_word.as_str()).collect();
        assert_eq!(
            words,
            ["company", "person", "stockholder proposal"],
            "按实体数再按词序：{sigs:?}"
        );
        let company = &sigs[0];
        assert_eq!(company.count, 2);
        assert_eq!(
            company.words.iter().map(String::as_str).collect::<HashSet<_>>(),
            HashSet::from(["Company", "company"]),
            "两种写法都留着"
        );
        assert_eq!(
            company.examples.iter().map(String::as_str).collect::<HashSet<_>>(),
            HashSet::from(["Acme", "Beta"])
        );
        assert_eq!(
            company.phrases,
            ["acquired", "is headquartered in"],
            "以公司为主语的开放陈述的短语，按次数"
        );
        let proposal = &sigs[2];
        assert_eq!(proposal.count, 1);
        assert_eq!(proposal.words, ["stockholder proposal"]);
        assert_eq!(proposal.examples, [PROPOSAL], "没有名字的以描述为例");
        assert!(proposal.phrases.is_empty());
        assert_eq!(sigs[1].count, 2, "Carol 与 Dave");

        // 2. Bind and write onto the entities: both companies become organization; what a person decided is left alone
        assert!(
            type_bindings::decide(
                &pool,
                f.kb,
                "company",
                &company.words,
                Some(f.organization),
                "bound",
                &serde_json::json!({ "votes": ["organization", "organization"] }),
                "agent",
            )
            .await?
        );
        assert_eq!(
            type_bindings::apply(&pool, f.kb, "company", f.organization).await?,
            2
        );
        for id in [f.acme, f.beta] {
            assert_eq!(
                typed(&pool, id).await?,
                Typed {
                    type_id: Some(f.organization),
                    type_source: "aligned".into(),
                    proposed_type: None,
                }
            );
        }
        assert_eq!(
            type_bindings::apply(&pool, f.kb, "company", f.organization).await?,
            0,
            "再写一次没有改动"
        );
        assert!(
            type_bindings::decide(
                &pool,
                f.kb,
                "Person",
                &[],
                Some(f.person),
                "bound",
                &serde_json::json!({}),
                "agent",
            )
            .await?,
            "写法归一后是同一个词"
        );
        assert_eq!(
            type_bindings::apply(&pool, f.kb, "person", f.person).await?,
            1,
            "只有 Carol；Dave 是人定的"
        );
        assert_eq!(typed(&pool, f.carol).await?.type_id, Some(f.person));
        assert_eq!(
            typed(&pool, f.dave).await?,
            Typed {
                type_id: None,
                type_source: "human".into(),
                proposed_type: None,
            },
            "人说没有类，就没有类"
        );

        // 3. The table used during extraction
        let bound = type_bindings::bound_map(&pool, f.kb).await?;
        assert_eq!(bound.get("company"), Some(&f.organization));
        assert_eq!(bound.get("person"), Some(&f.person));
        assert_eq!(bound.len(), 2);

        // 4. Staleness: the class it is bound to has changed
        assert!(type_bindings::stale(&pool, f.kb).await?.is_empty());
        tick().await;
        ontology::update_entity_type(
            &pool,
            f.kb,
            f.organization,
            "Organisation",
            None,
            "square",
            &[],
            "a company or any other body",
        )
        .await?;
        assert_eq!(type_bindings::stale(&pool, f.kb).await?, ["company"]);
        tick().await;
        assert!(
            type_bindings::decide(
                &pool,
                f.kb,
                "company",
                &company.words,
                Some(f.organization),
                "bound",
                &serde_json::json!({ "votes": ["organization", "organization"] }),
                "agent",
            )
            .await?,
            "代理可以改代理的"
        );
        assert!(type_bindings::stale(&pool, f.kb).await?.is_empty());

        // 5. No class matches: record none, and go down the old flow to suggest adding a class
        assert!(
            type_bindings::decide(
                &pool,
                f.kb,
                "stockholder proposal",
                &proposal.words,
                None,
                "none",
                &serde_json::json!({ "votes": [null, null] }),
                "agent",
            )
            .await?
        );
        assert_eq!(
            type_bindings::propose(&pool, f.kb, "stockholder proposal", "stockholder proposal")
                .await?,
            1
        );
        assert_eq!(
            typed(&pool, f.proposal).await?.proposed_type.as_deref(),
            Some("stockholder proposal")
        );
        assert_eq!(
            type_bindings::propose(&pool, f.kb, "stockholder proposal", "Stockholder Proposal")
                .await?,
            0,
            "只写第一次，与 set_proposed_type 同一条"
        );
        // Shape of a binding: once bound, it must have a class
        assert!(type_bindings::decide(
            &pool,
            f.kb,
            "thing",
            &[],
            None,
            "bound",
            &serde_json::json!({}),
            "agent",
        )
        .await
        .is_err());

        // 6. Staleness: after judging none, a new class has appeared in the library
        assert!(type_bindings::stale(&pool, f.kb).await?.is_empty());
        tick().await;
        sqlx::query(
            "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'proposal', 'Proposal')",
        )
        .bind(Uuid::now_v7())
        .bind(f.kb)
        .execute(&pool)
        .await?;
        assert_eq!(
            type_bindings::stale(&pool, f.kb).await?,
            ["stockholder proposal"],
            "绑上的不受新类影响，none 的要重判"
        );
        tick().await;
        assert!(
            type_bindings::decide(
                &pool,
                f.kb,
                "stockholder proposal",
                &[],
                None,
                "none",
                &serde_json::json!({ "votes": [null, null] }),
                "agent",
            )
            .await?
        );
        assert!(type_bindings::stale(&pool, f.kb).await?.is_empty());

        // 7. A person's judgment is never overwritten by an agent, though the reverse is allowed
        assert!(
            type_bindings::decide(
                &pool,
                f.kb,
                "company",
                &[],
                Some(f.organization),
                "bound",
                &serde_json::json!({ "by": "a person" }),
                "person",
            )
            .await?
        );
        assert!(
            !type_bindings::decide(
                &pool,
                f.kb,
                "company",
                &[],
                None,
                "none",
                &serde_json::json!({}),
                "agent",
            )
            .await?,
            "代理改不了人的"
        );
        let b = type_bindings::bindings(&pool, f.kb).await?;
        let company_b = b.iter().find(|b| b.kind_word == "company").unwrap();
        assert_eq!(
            (company_b.status.as_str(), company_b.type_id, company_b.decided_by.as_str()),
            ("bound", Some(f.organization), "person")
        );
        assert_eq!(
            b.iter().map(|b| b.kind_word.as_str()).collect::<Vec<_>>(),
            ["company", "person", "stockholder proposal"]
        );
        assert!(
            type_bindings::decide(
                &pool,
                f.kb,
                "company",
                &[],
                None,
                "none",
                &serde_json::json!({}),
                "person",
            )
            .await?,
            "人可以改人的"
        );
        let b = type_bindings::bindings(&pool, f.kb).await?;
        let company_b = b.iter().find(|b| b.kind_word == "company").unwrap();
        assert_eq!((company_b.status.as_str(), company_b.type_id), ("none", None));
        assert!(!type_bindings::bound_map(&pool, f.kb)
            .await?
            .contains_key("company"));

        // 8. Unbinding only touches aligned rows
        sqlx::query("UPDATE entities SET type_source = 'human' WHERE id = $1")
            .bind(f.beta)
            .execute(&pool)
            .await?;
        assert_eq!(
            type_bindings::unapply(&pool, f.kb, "company").await?,
            1,
            "Beta 已被人认下，留着"
        );
        assert_eq!(
            typed(&pool, f.acme).await?,
            Typed {
                type_id: None,
                type_source: "extracted".into(),
                proposed_type: None,
            }
        );
        assert_eq!(typed(&pool, f.beta).await?.type_id, Some(f.organization));
        assert_eq!(typed(&pool, f.carol).await?.type_id, Some(f.person));
        assert_eq!(type_bindings::unapply(&pool, f.kb, "company").await?, 0);
        // A class appears on the bound word later → on alignment, clear the suggestion
        assert_eq!(
            type_bindings::apply(&pool, f.kb, "stockholder proposal", f.organization).await?,
            1
        );
        assert_eq!(typed(&pool, f.proposal).await?.proposed_type, None);

        // 9. The class is deleted, and the binding follows it
        assert_eq!(type_bindings::unapply(&pool, f.kb, "person").await?, 1);
        sqlx::query("DELETE FROM entity_types WHERE id = $1")
            .bind(f.person)
            .execute(&pool)
            .await?;
        let b = type_bindings::bindings(&pool, f.kb).await?;
        assert!(b.iter().all(|b| b.kind_word != "person"));
        assert!(type_bindings::stale(&pool, f.kb).await?.is_empty());
        Ok::<(), anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(ORG)
        .execute(&pool)
        .await?;
    run
}

/// The organization's own name: the test above clears the field by name when it starts, and a shared name would delete the other test's data
const BASIS_ORG: &str = "kind-word-basis-test";

/// An agent's judgment is accepted only while the inputs it read still hold (#795); a person's judgment carries no fingerprint, and an agent does not overwrite it;
/// a person's judgment is committed in the same transaction as its phrase-alignment job
#[tokio::test]
async fn an_agent_decision_is_accepted_only_for_the_inputs_it_read() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(BASIS_ORG)
        .execute(&pool)
        .await?;
    let (org, ws, kb, class, acme) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(BASIS_ORG)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(org)
        .bind(BASIS_ORG)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3)")
        .bind(kb)
        .bind(ws)
        .bind(BASIS_ORG)
        .execute(&pool)
        .await?;

    let run = async {
        sqlx::query(
            "INSERT INTO entity_types (id, kb_id, key, label, description)
             VALUES ($1, $2, 'organization', 'Organization', 'OLD definition')",
        )
        .bind(class)
        .bind(kb)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO entities (id, kb_id, canonical_name, specific_type)
             VALUES ($1, $2, 'Acme', 'company')",
        )
        .bind(acme)
        .bind(kb)
        .execute(&pool)
        .await?;
        let shown = [class];
        let before = type_bindings::class_snapshot(&pool, kb)
            .await?
            .basis(&shown);

        // The definition changed while the model was answering: this reply answers the old definition, so it is not accepted, and nothing is written
        sqlx::query(
            "UPDATE entity_types SET description = 'NEW definition', updated_at = clock_timestamp()
              WHERE id = $1",
        )
        .bind(class)
        .execute(&pool)
        .await?;
        let votes = serde_json::json!({ "first": "organization", "second": "organization" });
        assert_eq!(
            type_bindings::decide_and_apply_if_current(
                &pool,
                kb,
                "company",
                &[],
                Some(class),
                "bound",
                &votes,
                &before,
                &shown,
            )
            .await?,
            type_bindings::Acceptance::Moved
        );
        assert!(type_bindings::bindings(&pool, kb).await?.is_empty());
        assert_eq!(typed(&pool, acme).await?.type_id, None);

        // Asked with the new inputs: accepted, and the fingerprint is stored with it
        let current = type_bindings::class_snapshot(&pool, kb)
            .await?
            .basis(&shown);
        assert_ne!(before, current);
        assert_eq!(
            type_bindings::decide_and_apply_if_current(
                &pool,
                kb,
                "company",
                &[],
                Some(class),
                "bound",
                &votes,
                &current,
                &shown,
            )
            .await?,
            type_bindings::Acceptance::Written
        );
        let b = type_bindings::bindings(&pool, kb).await?.remove(0);
        assert_eq!(b.basis.as_deref(), Some(current.as_str()));
        assert_eq!(typed(&pool, acme).await?.type_id, Some(class));

        // Judged by a person: no fingerprint; its phrase-alignment job is committed together with the judgment
        assert!(type_bindings::decide_and_apply_human(&pool, kb, "company", None, &votes).await?);
        let b = type_bindings::bindings(&pool, kb).await?.remove(0);
        assert_eq!((b.decided_by.as_str(), b.basis), ("person", None));
        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs
              WHERE kind = 'align_phrases' AND payload->>'kb_id' = $1 AND status = 'queued'",
        )
        .bind(kb.to_string())
        .fetch_one(&pool)
        .await?;
        assert_eq!(queued, 1);

        // A late agent reply does not overwrite a person
        assert_eq!(
            type_bindings::decide_and_apply_if_current(
                &pool,
                kb,
                "company",
                &[],
                Some(class),
                "bound",
                &votes,
                &current,
                &shown,
            )
            .await?,
            type_bindings::Acceptance::KeptPerson
        );
        assert_eq!(
            type_bindings::bindings(&pool, kb)
                .await?
                .remove(0)
                .decided_by,
            "person"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM jobs WHERE payload->>'kb_id' = $1")
        .bind(kb.to_string())
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(BASIS_ORG)
        .execute(&pool)
        .await?;
    run
}

/// Used by two lock-order tests: one class, one entity carrying a category word, and one agent judgment bound to this class.
/// The judgment is not projected -- when a class is deleted, `entities.type_id` is RESTRICT, so once an entity points at the class the delete itself fails,
/// and the test would not be testing the lock. Returns (library, class)
async fn seed_lock_order(pool: &PgPool, name: &str) -> anyhow::Result<(Uuid, Uuid)> {
    let (org, ws, kb, class) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
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
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'organization', 'Organization')",
    )
    .bind(class)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO entities (id, kb_id, canonical_name, specific_type)
         VALUES ($1, $2, 'Acme', 'company')",
    )
    .bind(Uuid::now_v7())
    .bind(kb)
    .execute(pool)
    .await?;
    type_bindings::decide(
        pool,
        kb,
        "company",
        &[],
        Some(class),
        "bound",
        &serde_json::json!({}),
        "agent",
    )
    .await?;
    Ok((kb, class))
}

/// Wait until someone is queued behind `pid` for the lock. With `chain`, wait for a longer run: someone queued behind
/// a person who is queued behind `pid`
async fn wait_behind(pool: &PgPool, pid: i32, chain: bool) -> anyhow::Result<()> {
    let sql = if chain {
        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity p
                         WHERE EXISTS (SELECT 1 FROM unnest(pg_blocking_pids(p.pid)) b(pid)
                                        WHERE $1 = ANY(pg_blocking_pids(b.pid))))"
    } else {
        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))"
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(sql).bind(pid).fetch_one(pool).await?;
            if waiting {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

/// When an agent records a judgment it locks the candidate class first, then writes the judgment row, in the same order as class deletion (deletion takes the class row first, then cascades to the judgment row).
/// The reverse order deadlocks: the agent holds the judgment row and waits for the class, while the delete holds the class and waits for the judgment row. Here the agent is paused between the two steps,
/// the class delete queues up behind it, and then it is released: both sides must run to completion, and neither reports 40P01
#[tokio::test]
async fn an_acceptance_and_a_class_delete_wait_for_each_other_instead_of_deadlocking(
) -> anyhow::Result<()> {
    const NAME: &str = "kind-word-lock-order-test";
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(NAME)
        .execute(&pool)
        .await?;
    let (kb, class) = seed_lock_order(&pool, NAME).await?;

    let run = async {
        let shown = [class];
        let basis = type_bindings::class_snapshot(&pool, kb)
            .await?
            .basis(&shown);
        // The gate holds the judgment row: the agent has locked the candidate class and computed the fingerprint, and it stops at the step of writing the judgment
        let mut gate = pool.begin().await?;
        let gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate)
            .await?;
        sqlx::query("SELECT id FROM type_bindings WHERE kb_id = $1 FOR UPDATE")
            .bind(kb)
            .fetch_one(&mut *gate)
            .await?;
        let agent = {
            let pool = pool.clone();
            tokio::spawn(async move {
                type_bindings::decide_and_apply_if_current(
                    &pool,
                    kb,
                    "company",
                    &[],
                    None,
                    "undecided",
                    &serde_json::json!({}),
                    &basis,
                    &shown,
                )
                .await
            })
        };
        wait_behind(&pool, gate_pid, false).await?;
        let deleter = {
            let pool = pool.clone();
            tokio::spawn(async move {
                sqlx::query("DELETE FROM entity_types WHERE id = $1")
                    .bind(class)
                    .execute(&pool)
                    .await
            })
        };
        // The class delete is queued behind the agent (the agent holds a shared lock on the class), and the agent is queued behind the gate
        wait_behind(&pool, gate_pid, true).await?;
        gate.rollback().await?;
        let accepted = tokio::time::timeout(Duration::from_secs(10), agent).await???;
        tokio::time::timeout(Duration::from_secs(10), deleter).await???;
        assert_eq!(accepted, type_bindings::Acceptance::Written);
        let b = type_bindings::bindings(&pool, kb).await?;
        assert_eq!(
            b.len(),
            1,
            "the undecided row no longer points at the class"
        );
        assert_eq!((b[0].status.as_str(), b[0].type_id), ("undecided", None));
        let classes: i64 = sqlx::query_scalar("SELECT count(*) FROM entity_types WHERE kb_id = $1")
            .bind(kb)
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            classes, 0,
            "the delete went through after the agent committed"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(NAME)
        .execute(&pool)
        .await?;
    run
}

/// The candidate class is deleted before the judgment is accepted: the agent waits for the class delete to commit, reads a class that is gone, the fingerprint no longer matches, and the reply
/// is not accepted; the judgment row went with the cascade, and the agent does not write it back either
#[tokio::test]
async fn a_candidate_deleted_while_the_reply_waits_moves_it() -> anyhow::Result<()> {
    const NAME: &str = "kind-word-deleted-candidate-test";
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(NAME)
        .execute(&pool)
        .await?;
    let (kb, class) = seed_lock_order(&pool, NAME).await?;

    let run = async {
        let shown = [class];
        let basis = type_bindings::class_snapshot(&pool, kb)
            .await?
            .basis(&shown);
        let mut deleter = pool.begin().await?;
        let deleter_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *deleter)
            .await?;
        sqlx::query("DELETE FROM entity_types WHERE id = $1")
            .bind(class)
            .execute(&mut *deleter)
            .await?;
        let agent = {
            let pool = pool.clone();
            tokio::spawn(async move {
                type_bindings::decide_and_apply_if_current(
                    &pool,
                    kb,
                    "company",
                    &[],
                    None,
                    "none",
                    &serde_json::json!({}),
                    &basis,
                    &shown,
                )
                .await
            })
        };
        wait_behind(&pool, deleter_pid, false).await?;
        deleter.commit().await?;
        let accepted = tokio::time::timeout(Duration::from_secs(10), agent).await???;
        assert_eq!(accepted, type_bindings::Acceptance::Moved);
        assert!(
            type_bindings::bindings(&pool, kb).await?.is_empty(),
            "the cascade took the row and the moved reply did not write it back"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(NAME)
        .execute(&pool)
        .await?;
    run
}
