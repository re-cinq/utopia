//! When the criterion moves, the open violations that rely on it must be reconciled on the spot (#564 / 0062).
//!
//! The promise of `axiom_violations.status = 'open'` is "reconciled against the **current** ontology".
//! Previously `update_relation_type` only wrote: relaxing one axiom left the open rows found in the last round
//! untouched — so the next export would present a finding that held only under the old criterion as if it were current.
//! What is pinned here are three things done in the same transaction:
//!
//! 1. **Detections under the old criterion are recorded first** — before writing, the DB still holds the old ontology
//! 2. **After writing, re-detect against the new ontology** — new violations enter in the same commit
//! 3. **Open rows that held only under the old criterion receive `criterion_changed`** — not decided by a person
//!    (decided_by is empty), but because the axiom or signature changed; those that were already not holding are deleted as before, following the same
//!    clearing rule as `reasoning::run`
//!
//! Edits that do not move the criterion (label, description) do not run detection — reconciliation is a cost that need not be paid there.

use sqlx::PgPool;
use utopia_core::models::RelationAxioms;
use utopia_store::{ontology, reasoning};
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    thing: Uuid,
    person: Uuid,
    /// state + asymmetric
    reports_to: Uuid,
    /// state, with domain=[person]
    works_for: Uuid,
}

const NONE: RelationAxioms = RelationAxioms {
    functional: false,
    inverse_functional: false,
    transitive: false,
    symmetric: false,
    asymmetric: false,
    irreflexive: false,
    inverse_of: None,
    sub_property_of: None,
};

fn ax() -> RelationAxioms {
    RelationAxioms {
        asymmetric: true,
        ..NONE
    }
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb, thing, person, reports_to, works_for) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'moved-axiom')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'moved-axiom')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'moved-axiom')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES
            ($1, $2, 'thing', 'Thing'), ($3, $2, 'person', 'Person')",
    )
    .bind(thing)
    .bind(kb)
    .bind(person)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_types
             (id, kb_id, key, label, temporal, is_asymmetric)
         VALUES ($1, $2, 'reports_to', 'reportsTo', 'state', TRUE)",
    )
    .bind(reports_to)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, temporal)
         VALUES ($1, $2, 'works_for', 'worksFor', 'state')",
    )
    .bind(works_for)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_type_domains (relation_type_id, entity_type_id)
         VALUES ($1, $2)",
    )
    .bind(works_for)
    .bind(person)
    .execute(pool)
    .await?;
    Ok(Fixture {
        org,
        kb,
        thing,
        person,
        reports_to,
        works_for,
    })
}

async fn entity(pool: &PgPool, f: &Fixture, ty: Uuid, name: &str) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(ty)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn fact(pool: &PgPool, f: &Fixture, (s, p, o): (Uuid, Uuid, Uuid)) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(s)
    .bind(p)
    .bind(o)
    .execute(pool)
    .await?;
    Ok(id)
}

/// The current state of a row: (status, resolution, decided_at)
async fn row_state(
    pool: &PgPool,
    id: Uuid,
) -> anyhow::Result<Option<(String, Option<String>, bool)>> {
    Ok(sqlx::query_as::<_, (String, Option<String>, bool)>(
        "SELECT status, resolution, decided_at IS NOT NULL FROM axiom_violations WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

async fn open_count(pool: &PgPool, f: &Fixture) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM axiom_violations WHERE kb_id = $1 AND status = 'open'",
    )
    .bind(f.kb)
    .fetch_one(pool)
    .await?)
}

async fn cleanup(pool: &PgPool, f: &Fixture) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(pool)
        .await?;
    Ok(())
}

/// Relaxing an axiom: open rows receive criterion_changed, neither deleted nor left open;
/// put the axiom back and the same row reopens — `criterion_changed` is a member of the broken-contract family
#[tokio::test]
async fn relaxing_an_axiom_closes_its_finding_and_reinstating_reopens_it() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let result = async {
        let (a, b) = (
            entity(&pool, &f, f.thing, "A").await?,
            entity(&pool, &f, f.thing, "B").await?,
        );
        fact(&pool, &f, (a, f.reports_to, b)).await?;
        fact(&pool, &f, (b, f.reports_to, a)).await?;
        let r = reasoning::run(&pool, f.kb).await?;
        anyhow::ensure!(r.found == 1, "expected one asymmetry finding, got {r:?}");
        let (id,): (Uuid,) =
            sqlx::query_as("SELECT id FROM axiom_violations WHERE kb_id = $1 AND status = 'open'")
                .bind(f.kb)
                .fetch_one(&pool)
                .await?;

        // Change the criterion: asymmetric turned off. In the same transaction this row should be absorbed into criterion_changed
        ontology::update_relation_type(
            &pool,
            f.kb,
            f.reports_to,
            "reportsTo",
            "state",
            NONE,
            "",
            None,
            None,
            None,
            None,
        )
        .await?;
        anyhow::ensure!(
            open_count(&pool, &f).await? == 0,
            "open finding outlived its axiom"
        );
        let (status, resolution, decided) = row_state(&pool, id).await?.unwrap();
        anyhow::ensure!(
            status == "resolved" && resolution.as_deref() == Some("criterion_changed"),
            "finding was not closed as criterion_changed: {status}/{resolution:?}"
        );
        anyhow::ensure!(decided, "decided_at should mark when it stopped being open");

        // Change it back: the same contradiction holds again, and the same row reopens — not a separately inserted row
        ontology::update_relation_type(
            &pool,
            f.kb,
            f.reports_to,
            "reportsTo",
            "state",
            ax(),
            "",
            None,
            None,
            None,
            None,
        )
        .await?;
        let (status, resolution, _) = row_state(&pool, id).await?.unwrap();
        anyhow::ensure!(
            status == "open" && resolution.is_none(),
            "finding should reopen under the restored axiom, got {status}/{resolution:?}"
        );
        anyhow::ensure!(open_count(&pool, &f).await? == 1);
        Ok(())
    }
    .await;
    let cleanup = cleanup(&pool, &f).await;
    result.and(cleanup)
}

/// Changing a domain changes the signature: an open signature violation is resolved under the new criterion
#[tokio::test]
async fn a_domain_change_closes_the_signature_finding() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let result = async {
        // works_for domain=person, and a thing asserted it — a signature violation
        let (acme, alice) = (
            entity(&pool, &f, f.thing, "Acme").await?,
            entity(&pool, &f, f.thing, "Alice").await?,
        );
        fact(&pool, &f, (alice, f.works_for, acme)).await?;
        let r = reasoning::run(&pool, f.kb).await?;
        anyhow::ensure!(
            r.found == 1
                && reasoning::open_violations(&pool, f.kb, 10, 0).await?[0].kind == "signature",
            "expected one signature finding"
        );

        // Relax domain to thing: the criterion is gone, so this row receives criterion_changed
        ontology::update_relation_type(
            &pool,
            f.kb,
            f.works_for,
            "worksFor",
            "state",
            NONE,
            "",
            None,
            None,
            Some(&[f.thing]),
            None,
        )
        .await?;
        anyhow::ensure!(
            open_count(&pool, &f).await? == 0,
            "signature finding outlived its domain"
        );
        let (status, resolution, _) = sqlx::query_as::<_, (String, Option<String>, bool)>(
            "SELECT status, resolution, decided_at IS NOT NULL FROM axiom_violations
              WHERE kb_id = $1 AND kind = 'signature'",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        anyhow::ensure!(
            status == "resolved" && resolution.as_deref() == Some("criterion_changed"),
            "signature finding was not closed as criterion_changed: {status}/{resolution:?}"
        );
        Ok(())
    }
    .await;
    let cleanup = cleanup(&pool, &f).await;
    result.and(cleanup)
}

/// An edit that does not move the criterion does not run reconciliation: an open row that held only under the old detection,
/// touched by changing the label, is not affected; when the criterion really changes, it is handled as stale long ago — deleted, not criterion_changed
#[tokio::test]
async fn a_label_edit_leaves_the_ledger_alone() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let result = async {
        let (a, b) = (
            entity(&pool, &f, f.thing, "A").await?,
            entity(&pool, &f, f.thing, "B").await?,
        );
        let f1 = fact(&pool, &f, (a, f.works_for, b)).await?;
        // A row that the detection function would never produce (a self-loop carrying a functional key):
        // it is stale long ago, so the next reconciliation should delete it, not absorb it
        let stray = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO axiom_violations (id, kb_id, kind, left_fact, right_fact)
             VALUES ($1, $2, 'functional', $3, $3)",
        )
        .bind(stray)
        .bind(f.kb)
        .bind(f1)
        .execute(&pool)
        .await?;

        // Only the label changes: the criterion is unchanged, so detection is not run — stale rows are left as they are (detection has a cost,
        // which should not be paid for a rename)
        ontology::update_relation_type(
            &pool,
            f.kb,
            f.works_for,
            "employs",
            "state",
            NONE,
            "",
            None,
            None,
            None,
            None,
        )
        .await?;
        anyhow::ensure!(
            row_state(&pool, stray).await?.map(|r| r.0) == Some("open".into()),
            "a label-only edit must not touch violation state"
        );

        // Really changing the criterion (domain from person to thing): this row is not in the old baseline
        //(detection never produced it) — delete it, the same rule as stale clearing in run()
        ontology::update_relation_type(
            &pool,
            f.kb,
            f.works_for,
            "employs",
            "state",
            NONE,
            "",
            None,
            None,
            Some(&[f.thing]),
            None,
        )
        .await?;
        anyhow::ensure!(
            row_state(&pool, stray).await?.is_none(),
            "a stale row that never held under the old ontology should be deleted, not adjudicated"
        );
        Ok(())
    }
    .await;
    let cleanup = cleanup(&pool, &f).await;
    result.and(cleanup)
}

/// The parent chain is also part of the criterion: after attaching a parent class so the signature matches, an open violation is absorbed on the spot
#[tokio::test]
async fn a_new_parent_closes_the_signature_finding() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let result = async {
        // works_for domain=person; thing ⊏ person — thing asserting it is a violation
        let (acme, alice) = (
            entity(&pool, &f, f.thing, "Acme").await?,
            entity(&pool, &f, f.thing, "Alice").await?,
        );
        fact(&pool, &f, (alice, f.works_for, acme)).await?;
        reasoning::run(&pool, f.kb).await?;
        anyhow::ensure!(open_count(&pool, &f).await? == 1);

        // Attach person as thing's parent class: the signature is judged along the ancestor chain, and now it matches
        ontology::update_entity_type(
            &pool,
            f.kb,
            f.thing,
            "Thing",
            None,
            "circle",
            &[f.person],
            "",
        )
        .await?;
        anyhow::ensure!(
            open_count(&pool, &f).await? == 0,
            "signature finding should close once the parent makes the signature hold"
        );
        let (status, resolution, _) = sqlx::query_as::<_, (String, Option<String>, bool)>(
            "SELECT status, resolution, decided_at IS NOT NULL FROM axiom_violations
              WHERE kb_id = $1 AND kind = 'signature'",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        anyhow::ensure!(
            status == "resolved" && resolution.as_deref() == Some("criterion_changed"),
            "finding was not closed as criterion_changed: {status}/{resolution:?}"
        );
        Ok(())
    }
    .await;
    let cleanup = cleanup(&pool, &f).await;
    result.and(cleanup)
}

/// Deleting a relation that another's `sub_property_of` points at is also a change of criterion:
/// 0070 made that foreign key DEFERRABLE SET NULL — it only lands on commit, so within the transaction one must
/// first explicitly break the link and then reconcile. After the link is broken, findings derived through it should no longer exist
#[tokio::test]
async fn deleting_a_linked_relation_reconciles_what_depended_on_it() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let result = async {
        // manages is a functional super-property of supervises ⊑ manages — zero assertions,
        // it lives only on the derived chain, so it can be deleted
        let (manages, supervises) = (Uuid::now_v7(), Uuid::now_v7());
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, temporal, functional)
             VALUES ($1, $2, 'manages', 'manages', 'state', TRUE)",
        )
        .bind(manages)
        .bind(f.kb)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, temporal, sub_property_of)
             VALUES ($1, $2, 'supervises', 'supervises', 'state', $3)",
        )
        .bind(supervises)
        .bind(f.kb)
        .bind(manages)
        .execute(&pool)
        .await?;
        let (alice, carol, dave) = (
            entity(&pool, &f, f.thing, "Alice").await?,
            entity(&pool, &f, f.thing, "Carol").await?,
            entity(&pool, &f, f.thing, "Dave").await?,
        );
        fact(&pool, &f, (alice, supervises, carol)).await?;
        fact(&pool, &f, (alice, supervises, dave)).await?;
        reasoning::run(&pool, f.kb).await?;
        // supervises ⊑ manages derives manages(alice→carol) and manages(alice→dave):
        // the two derivations collide with functional — the rule's findings for the layer go into ontology_defects
        let defects: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ontology_defects
              WHERE kb_id = $1 AND kind = 'rules_disagree'",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        anyhow::ensure!(
            defects == 1,
            "expected one rules_disagree defect, got {defects}"
        );

        // manages has zero assertions and can be deleted: first break the chain from supervises, then delete and reconcile — the findings disappear with it,
        // so no finding "that holds only for a criterion already deleted" survives past the commit point
        ontology::delete_relation_type(&pool, f.kb, manages).await?;
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ontology_defects WHERE kb_id = $1")
                .bind(f.kb)
                .fetch_one(&pool)
                .await?;
        anyhow::ensure!(left == 0, "defect survived the deletion of its criterion");
        let dangling: i64 =
            sqlx::query_scalar("SELECT count(*) FROM relation_types WHERE sub_property_of = $1")
                .bind(manages)
                .fetch_one(&pool)
                .await?;
        anyhow::ensure!(
            dangling == 0,
            "deferred SET NULL would have hidden the link"
        );
        Ok(())
    }
    .await;
    let cleanup = cleanup(&pool, &f).await;
    result.and(cleanup)
}

/// Evidence drift is not identity drift: the `path` kept in the row is the member set photographed in the last round, and re-computation in detection
/// may change it — tiering compares `(kind, left, right)`, not that old photograph. One
/// violation that really was detectable under the old criterion must not be deleted as "stale long ago" just because the stored evidence now has a different group
#[tokio::test]
async fn drifted_evidence_still_closes_as_criterion_changed() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let result = async {
        let (a, b, c) = (
            entity(&pool, &f, f.thing, "A").await?,
            entity(&pool, &f, f.thing, "B").await?,
            entity(&pool, &f, f.thing, "C").await?,
        );
        let f1 = fact(&pool, &f, (a, f.reports_to, b)).await?;
        fact(&pool, &f, (b, f.reports_to, a)).await?;
        let f3 = fact(&pool, &f, (a, f.reports_to, c)).await?;
        reasoning::run(&pool, f.kb).await?;
        let (id,): (Uuid,) =
            sqlx::query_as("SELECT id FROM axiom_violations WHERE kb_id = $1 AND status = 'open'")
                .bind(f.kb)
                .fetch_one(&pool)
                .await?;

        // Simulate old evidence left by the last detection: the path members drifted, but the endpoints did not change —
        // it is still the same violation (0054: non-cycle kinds are identified by (kind,left,right))
        sqlx::query("UPDATE axiom_violations SET path = $2 WHERE id = $1")
            .bind(id)
            .bind(&[f1, f3][..])
            .execute(&pool)
            .await?;

        ontology::update_relation_type(
            &pool,
            f.kb,
            f.reports_to,
            "reportsTo",
            "state",
            NONE,
            "",
            None,
            None,
            None,
            None,
        )
        .await?;
        let (status, resolution, _) = row_state(&pool, id).await?.unwrap();
        anyhow::ensure!(
            status == "resolved" && resolution.as_deref() == Some("criterion_changed"),
            "evidence drift must not turn a baseline finding into a deletion: \
             {status}/{resolution:?} (drifted path had f3={f3})"
        );
        Ok(())
    }
    .await;
    let cleanup = cleanup(&pool, &f).await;
    result.and(cleanup)
}

/// Breaking the link also changes the criterion: `update_relation_type` removes `sub_property_of`,
/// and the findings derived through it are cleared on the spot — the same rule as `delete_relation_type`
#[tokio::test]
async fn unlinking_a_sub_property_reconciles_what_depended_on_it() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let result = async {
        let (manages, supervises) = (Uuid::now_v7(), Uuid::now_v7());
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, temporal, functional)
             VALUES ($1, $2, 'manages', 'manages', 'state', TRUE)",
        )
        .bind(manages)
        .bind(f.kb)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, temporal, sub_property_of)
             VALUES ($1, $2, 'supervises', 'supervises', 'state', $3)",
        )
        .bind(supervises)
        .bind(f.kb)
        .bind(manages)
        .execute(&pool)
        .await?;
        let (alice, carol, dave) = (
            entity(&pool, &f, f.thing, "Alice").await?,
            entity(&pool, &f, f.thing, "Carol").await?,
            entity(&pool, &f, f.thing, "Dave").await?,
        );
        fact(&pool, &f, (alice, supervises, carol)).await?;
        fact(&pool, &f, (alice, supervises, dave)).await?;
        reasoning::run(&pool, f.kb).await?;
        let defects: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ontology_defects
              WHERE kb_id = $1 AND kind = 'rules_disagree'",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        anyhow::ensure!(defects == 1, "expected one rules_disagree defect");

        // Remove supervises ⊑ manages: the criterion changed, the derivation chain is broken — the findings are cleared with the commit
        ontology::update_relation_type(
            &pool,
            f.kb,
            supervises,
            "supervises",
            "state",
            NONE,
            "",
            None,
            None,
            None,
            None,
        )
        .await?;
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ontology_defects WHERE kb_id = $1")
                .bind(f.kb)
                .fetch_one(&pool)
                .await?;
        anyhow::ensure!(
            left == 0,
            "defect survived the unlinking of its derivation chain"
        );
        Ok(())
    }
    .await;
    let cleanup = cleanup(&pool, &f).await;
    result.and(cleanup)
}

/// Deleting a class that a criterion hangs on: in the same transaction, the findings depending on it are absorbed
#[tokio::test]
async fn deleting_a_domain_class_closes_the_signature_finding() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let result = async {
        let (acme, alice) = (
            entity(&pool, &f, f.thing, "Acme").await?,
            entity(&pool, &f, f.thing, "Alice").await?,
        );
        fact(&pool, &f, (alice, f.works_for, acme)).await?;
        reasoning::run(&pool, f.kb).await?;
        anyhow::ensure!(open_count(&pool, &f).await? == 1);

        // person is works_for's domain: deleting it, and the criterion goes with it
        ontology::delete_entity_type(&pool, f.kb, f.person).await?;
        anyhow::ensure!(
            open_count(&pool, &f).await? == 0,
            "finding should close when its domain class is deleted"
        );
        let (status, resolution, _) = sqlx::query_as::<_, (String, Option<String>, bool)>(
            "SELECT status, resolution, decided_at IS NOT NULL FROM axiom_violations
              WHERE kb_id = $1 AND kind = 'signature'",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        anyhow::ensure!(
            status == "resolved" && resolution.as_deref() == Some("criterion_changed"),
            "finding was not closed as criterion_changed: {status}/{resolution:?}"
        );
        Ok(())
    }
    .await;
    let cleanup = cleanup(&pool, &f).await;
    result.and(cleanup)
}
