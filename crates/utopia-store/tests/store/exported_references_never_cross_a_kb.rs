//! Write side: every edge the export would touch is blocked by 0070's foreign keys and triggers; the export-side health check
//! covers **all** structural reference edges that 0070 protects. A row pointing into another KB, or a dangling one, must reject the whole export rather than
//! forge an IRI or silently drop a meaning. A row that lands in another KB ends up in one of two ways:
//!   - references minted as this KB's IRIs (entities, facts, derivations, documents, chunks, rules) → a forged identity;
//!   - references looked up by id in this KB's vocabulary (predicates, attribute types, entity types, parent classes) → silently disappear.
//!
//! Both kinds are bad rows; the trigger and the export side both reject them.
//!
//! Bad rows are built with `SET LOCAL session_replication_role='replica'`, which turns off only this transaction's triggers
//! (including FK enforcement) and is restored on commit. This also allows building dangling references whose target row no longer exists,
//! which is exactly the shape of a restore from a backup taken before 0070.

use sqlx::{Acquire, PgPool};
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    a: Uuid,
    b: Uuid,
    doc_a: Uuid,
    doc_b: Uuid,
    chunk_a: Uuid,
    chunk_b: Uuid,
    ent_a: Uuid,
    ent_b: Uuid,
    fact_a: Uuid,
    fact_b: Uuid,
    rel_a: Uuid,
    rel_b: Uuid,
    cls_a: Uuid,
    cls_b: Uuid,
    rule_a: Uuid,
    rule_b: Uuid,
    arule_a: Uuid,
    arule_b: Uuid,
    der_a: Uuid,
    der_b: Uuid,
    irule_a: Uuid,
    irule_b: Uuid,
    arv_a: Uuid,
    arv_b: Uuid,
    erun_a: Uuid,
    erun_b: Uuid,
    eact_a: Uuid,
}

/// Two KBs, each with one set of parts that can be referenced: documents / chunks / entities / facts / predicates / classes / axiom rules / business rules / derivations
async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, a, b) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (doc_a, doc_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (chunk_a, chunk_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (ent_a, ent_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (fact_a, fact_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (rel_a, rel_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (cls_a, cls_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (rule_a, rule_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (arule_a, arule_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (der_a, der_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (irule_a, irule_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (arv_a, arv_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (erun_a, erun_b) = (Uuid::now_v7(), Uuid::now_v7());
    let eact_a = Uuid::now_v7();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'xkb-ref-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'xkb-ref-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    for kb in [a, b] {
        sqlx::query(
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'xkb-ref-test')",
        )
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    }
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, status, external_key)
         VALUES ($1, $2, 'a.md', 'sha-a', 'ready', 'file:///a.md')",
    )
    .bind(doc_a)
    .bind(a)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, status, external_key)
         VALUES ($1, $2, 'b.md', 'sha-b', 'ready', 'file:///b.md')",
    )
    .bind(doc_b)
    .bind(b)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, 'x')",
    )
    .bind(chunk_a)
    .bind(a)
    .bind(doc_a)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, 'x')",
    )
    .bind(chunk_b)
    .bind(b)
    .bind(doc_b)
    .execute(pool)
    .await?;
    for (id, kb) in [(ent_a, a), (ent_b, b)] {
        sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'e')")
            .bind(id)
            .bind(kb)
            .execute(pool)
            .await?;
    }
    for (id, kb, s, o) in [(fact_a, a, ent_a, ent_a), (fact_b, b, ent_b, ent_b)] {
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence)
             VALUES ($1, $2, $3, $4, 0.9)",
        )
        .bind(id)
        .bind(kb)
        .bind(s)
        .bind(o)
        .execute(pool)
        .await?;
    }
    for (id, kb) in [(rel_a, a), (rel_b, b)] {
        sqlx::query("INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, $3, 'r')")
            .bind(id)
            .bind(kb)
            .bind(format!("r-{id}"))
            .execute(pool)
            .await?;
    }
    for (id, kb) in [(cls_a, a), (cls_b, b)] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, 'c')")
            .bind(id)
            .bind(kb)
            .bind(format!("c-{id}"))
            .execute(pool)
            .await?;
    }
    for (id, kb, pred) in [(rule_a, a, rel_a), (rule_b, b, rel_b)] {
        sqlx::query(
            "INSERT INTO rules (id, kb_id, predicate_id, kind) VALUES ($1, $2, $3, 'transitive')",
        )
        .bind(id)
        .bind(kb)
        .bind(pred)
        .execute(pool)
        .await?;
    }
    for (id, kb, ty, pred) in [(arule_a, a, cls_a, rel_a), (arule_b, b, cls_b, rel_b)] {
        sqlx::query(
            "INSERT INTO attribute_rules (id, kb_id, name, subject_type_id, conclusion,
                                         conclude_predicate_id, conclude_value)
             VALUES ($1, $2, $3, $4, 'attribute', $5, '42'::jsonb)",
        )
        .bind(id)
        .bind(kb)
        .bind(format!("ar-{id}"))
        .bind(ty)
        .bind(pred)
        .execute(pool)
        .await?;
    }
    for (id, kb, s, p, o, r) in [
        (der_a, a, ent_a, rel_a, ent_a, rule_a),
        (der_b, b, ent_b, rel_b, ent_b, rule_b),
    ] {
        sqlx::query(
            "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_id, rule_id)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(kb)
        .bind(s)
        .bind(p)
        .bind(o)
        .bind(r)
        .execute(pool)
        .await?;
    }
    // Same-KB family members added after 0073: entailment rules, reading cache, errata ledger, definition history —
    // one set per KB, for the cross-KB assertions to draw on
    for (id, kb, pred) in [(irule_a, a, rel_a), (irule_b, b, rel_b)] {
        sqlx::query(
            "INSERT INTO implication_rules (id, kb_id, trigger, phrase,
                                           conclude_property_id, status)
             VALUES ($1, $2, 'phrase', 'p', $3, 'approved')",
        )
        .bind(id)
        .bind(kb)
        .bind(pred)
        .execute(pool)
        .await?;
    }
    for (id, kb, rule) in [(arv_a, a, arule_a), (arv_b, b, arule_b)] {
        sqlx::query(
            "INSERT INTO attribute_rule_versions (id, kb_id, rule_id, seq, definition)
             VALUES ($1, $2, $3, 1, '{}'::jsonb)",
        )
        .bind(id)
        .bind(kb)
        .bind(rule)
        .execute(pool)
        .await?;
    }
    for (id, kb, doc) in [(erun_a, a, doc_a), (erun_b, b, doc_b)] {
        sqlx::query("INSERT INTO errata_runs (id, kb_id, document_id) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(kb)
            .bind(doc)
            .execute(pool)
            .await?;
    }
    for (id, kb, run, doc) in [(eact_a, a, erun_a, doc_a)] {
        sqlx::query(
            "INSERT INTO errata_actions (id, kb_id, run_id, document_id, action, status)
             VALUES ($1, $2, $3, $4, 'keep', 'held')",
        )
        .bind(id)
        .bind(kb)
        .bind(run)
        .bind(doc)
        .execute(pool)
        .await?;
    }
    // The reading cache has no id column — its key is (kb_id, reading, phrase)
    for (kb, ent) in [(a, ent_a), (b, ent_b)] {
        sqlx::query(
            "INSERT INTO phrase_readings (kb_id, reading, phrase, entity_id)
             VALUES ($1, 'nationality', 'r', $2)",
        )
        .bind(kb)
        .bind(ent)
        .execute(pool)
        .await?;
    }
    Ok(Fixture {
        org,
        a,
        b,
        doc_a,
        doc_b,
        chunk_a,
        chunk_b,
        ent_a,
        ent_b,
        fact_a,
        fact_b,
        rel_a,
        rel_b,
        cls_a,
        cls_b,
        rule_a,
        rule_b,
        arule_a,
        arule_b,
        der_a,
        der_b,
        irule_a,
        irule_b,
        arv_a,
        arv_b,
        erun_a,
        erun_b,
        eact_a,
    })
}

async fn cleanup(pool: &PgPool, f: &Fixture) -> anyhow::Result<()> {
    for kb in [f.a, f.b] {
        sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
            .bind(kb)
            .execute(pool)
            .await?;
    }
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(pool)
        .await?;
    Ok(())
}

/// A new cross-KB write: every edge is blocked on the spot by its own trigger
#[tokio::test]
async fn new_cross_kb_writes_are_rejected_on_every_exported_edge() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    // — Derivation premises: two kinds of premise, two shapes of cross-KB reference
    for (premise_fact, premise_derived, what) in [
        (Some(f.fact_b), None, "foreign fact premise"),
        (None, Some(f.der_b), "foreign derived premise"),
    ] {
        let err = sqlx::query(
            "INSERT INTO fact_derivations (derived_fact_id, premise_fact_id, premise_derived_id, seq)
             VALUES ($1, $2, $3, 0)",
        )
        .bind(f.der_a)
        .bind(premise_fact)
        .bind(premise_derived)
        .execute(&pool)
        .await;
        assert!(err.is_err(), "derivation premise: {what} must be rejected");
    }

    // — Attributes on edges: a type from another KB is silently skipped by the vocabulary, and an entity from another KB would be minted with this KB's IRI
    let err = sqlx::query(
        "INSERT INTO fact_qualifiers (fact_id, qualifier_type_id, value)
         VALUES ($1, $2, '\"lit\"'::jsonb)",
    )
    .bind(f.fact_a)
    .bind(f.rel_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "qualifier.type→foreign must be rejected");
    let err = sqlx::query(
        "INSERT INTO fact_qualifiers (fact_id, qualifier_type_id, entity_id)
         VALUES ($1, $2, $3)",
    )
    .bind(f.fact_a)
    .bind(f.rel_a)
    .bind(f.ent_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "qualifier.entity→foreign must be rejected");

    // — The five reference columns of the fact ontology (from_statement_id is a self-reference within the same table: the immediate end is
    //    on a BEFORE trigger, and is rejected on the spot once the target is already present)
    for (col, foreign_id, what) in [
        ("subject_id", f.ent_b, "fact.subject"),
        ("object_id", f.ent_b, "fact.object"),
        ("predicate_id", f.rel_b, "fact.predicate"),
        ("supersedes", f.fact_b, "fact.supersedes"),
        ("from_statement_id", f.fact_b, "fact.from_statement"),
    ] {
        let err = sqlx::query(&format!(
            "INSERT INTO facts (id, kb_id, subject_id, {col}) VALUES ($1, $2, $3, $4)"
        ))
        .bind(Uuid::now_v7())
        .bind(f.a)
        .bind(if col == "subject_id" {
            f.ent_b
        } else {
            f.ent_a
        })
        .bind(foreign_id)
        .execute(&pool)
        .await;
        assert!(err.is_err(), "{what}→foreign must be rejected");
    }

    // — Statement sources (typed_fact_sources, 0068): the row has no kb column of its own, so ownership is judged by the KB of its
    //    fact — A's fact consuming B's statement would serialise an edge that points at another KB's statement
    let err = sqlx::query("INSERT INTO typed_fact_sources (fact_id, statement_id) VALUES ($1, $2)")
        .bind(f.fact_a)
        .bind(f.fact_b)
        .execute(&pool)
        .await;
    assert!(
        err.is_err(),
        "factsource.statement→foreign must be rejected"
    );

    // — Attributes of open statements (statement_qualifiers, 0061): the row has no kb column, so ownership is judged by the KB
    //    of its fact; an entity value from another KB would be minted with this KB's IRI
    let err = sqlx::query(
        "INSERT INTO statement_qualifiers (fact_id, role, entity_id, chunk_id)
         VALUES ($1, 'as', $2, $3)",
    )
    .bind(f.fact_a)
    .bind(f.ent_b)
    .bind(f.chunk_a)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "squalifier.entity→foreign must be rejected");
    // A role word recorded on a chunk in another KB (0070 decision 1)
    let err = sqlx::query(
        "INSERT INTO statement_qualifiers (fact_id, role, value, chunk_id)
         VALUES ($1, 'lines', '\"1\"'::jsonb, $2)",
    )
    .bind(f.fact_a)
    .bind(f.chunk_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "squalifier.chunk→foreign must be rejected");

    // — Time mentions (0061/0064): the mention's own kb must be in the same KB as the fact and chunk it refers to
    let err = sqlx::query(
        "INSERT INTO time_mentions (id, kb_id, fact_id, chunk_id, text, char_start)
         VALUES ($1, $2, $3, $4, '去年', 0)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.fact_b) // fact in another KB
    .bind(f.chunk_a)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "timemention.fact→foreign must be rejected");
    let err = sqlx::query(
        "INSERT INTO time_mentions (id, kb_id, fact_id, chunk_id, text, char_start)
         VALUES ($1, $2, $3, $4, '去年', 0)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.fact_a)
    .bind(f.chunk_b) // chunk in another KB
    .execute(&pool)
    .await;
    assert!(err.is_err(), "timemention.chunk→foreign must be rejected");

    // — Type and phrase bindings (0065/0066): the bound class or attribute follows the bound row's own KB
    let err = sqlx::query(
        "INSERT INTO type_bindings (id, kb_id, kind_word, status, type_id)
         VALUES ($1, $2, 'corp', 'bound', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.cls_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "binding.type→foreign must be rejected");
    let err = sqlx::query(
        "INSERT INTO phrase_bindings (id, kb_id, phrase, subject_type_id, status)
         VALUES ($1, $2, 'runs', $3, 'none')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.cls_b)
    .execute(&pool)
    .await;
    assert!(
        err.is_err(),
        "pbinding.subject_type→foreign must be rejected"
    );
    let err = sqlx::query(
        "INSERT INTO phrase_bindings (id, kb_id, phrase, relation_type_id,
                                     direction, status)
         VALUES ($1, $2, 'runs', $3, 'forward', 'bound')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.rel_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "pbinding.relation→foreign must be rejected");

    // — The five reference columns of the derived fact ontology
    for (col, foreign_id, what) in [
        ("subject_id", f.ent_b, "derived.subject"),
        ("object_id", f.ent_b, "derived.object"),
        ("predicate_id", f.rel_b, "derived.predicate"),
        ("rule_id", f.rule_b, "derived.rule"),
        ("attribute_rule_id", f.arule_b, "derived.attribute_rule"),
    ] {
        // attribute_rule and rule are mutually exclusive: the test for the attribute_rule row carries no rule_id
        let err = if col == "attribute_rule_id" {
            sqlx::query(
                "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, attribute_rule_id)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(Uuid::now_v7())
            .bind(f.a)
            .bind(f.ent_a)
            .bind(f.rel_a)
            .bind(foreign_id)
            .execute(&pool)
            .await
        } else {
            sqlx::query(&format!(
                "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, rule_id, {col})
                 VALUES ($1, $2, $3, $4, $5, $6)"
            ))
            .bind(Uuid::now_v7())
            .bind(f.a)
            .bind(if col == "subject_id" {
                f.ent_b
            } else {
                f.ent_a
            })
            .bind(if col == "predicate_id" {
                f.rel_b
            } else {
                f.rel_a
            })
            .bind(if col == "rule_id" { f.rule_b } else { f.rule_a })
            .bind(foreign_id)
            .execute(&pool)
            .await
        };
        assert!(err.is_err(), "{what}→foreign must be rejected");
    }

    // — The entity's class, class hierarchy, disjointness, and domain/range
    let err = sqlx::query(
        "INSERT INTO entities (id, kb_id, canonical_name, type_id) VALUES ($1, $2, 'e', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.cls_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "entity.type→foreign must be rejected");

    let err = sqlx::query("INSERT INTO entity_type_parents (child_id, parent_id) VALUES ($1, $2)")
        .bind(f.cls_a)
        .bind(f.cls_b)
        .execute(&pool)
        .await;
    assert!(err.is_err(), "class.parent→foreign must be rejected");

    let err =
        sqlx::query("INSERT INTO entity_type_disjoint (kb_id, a_id, b_id) VALUES ($1, $2, $3)")
            .bind(f.a)
            .bind(f.cls_a)
            .bind(f.cls_b)
            .execute(&pool)
            .await;
    assert!(err.is_err(), "class.disjoint→foreign must be rejected");

    for (table, what) in [
        ("relation_type_domains", "relation.domain"),
        ("relation_type_ranges", "relation.range"),
    ] {
        let err = sqlx::query(&format!(
            "INSERT INTO {table} (relation_type_id, entity_type_id) VALUES ($1, $2)"
        ))
        .bind(f.rel_a)
        .bind(f.cls_b)
        .execute(&pool)
        .await;
        assert!(err.is_err(), "{what}→foreign must be rejected");
    }

    // — Self-reference of relations within the same table, and the declaration of edge attributes: owl:inverseOf /
    //    rdfs:subPropertyOf and the qualifier list are all semantics, so another KB's must not come in
    let err = sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, inverse_of) VALUES ($1, $2, 'inv', 'inv', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.rel_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "relation.inverse→foreign must be rejected");
    let err = sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, sub_property_of) VALUES ($1, $2, 'sub', 'sub', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.rel_b)
    .execute(&pool)
    .await;
    assert!(
        err.is_err(),
        "relation.sub_property→foreign must be rejected"
    );
    let err = sqlx::query(
        "INSERT INTO relation_type_qualifiers (relation_type_id, qualifier_type_id) VALUES ($1, $2)",
    )
    .bind(f.rel_a)
    .bind(f.rel_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "relation.qualifier→foreign must be rejected");

    // — Rule ontology: which predicate an axiom is written on, and which class a business rule looks at to reach which conclusion,
    //    all appear in the export now — their references must not cross KBs either
    let err = sqlx::query(
        "INSERT INTO rules (id, kb_id, predicate_id, kind) VALUES ($1, $2, $3, 'transitive')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.rel_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "rule.predicate→foreign must be rejected");
    // Each conclusion shape's own required columns (the CHECK pins the column groups of typing / attribute / computed),
    // one legal shape per reference column
    let err = sqlx::query(
        "INSERT INTO attribute_rules (id, kb_id, name, subject_type_id, conclusion,
                                      conclude_predicate_id, conclude_value)
         VALUES ($1, $2, 'ar', $3, 'attribute', $4, '42'::jsonb)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.cls_b) // subject_type in another KB
    .bind(f.rel_a)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "arule.subject_type→foreign must be rejected");
    let err = sqlx::query(
        "INSERT INTO attribute_rules (id, kb_id, name, subject_type_id, conclusion,
                                      conclude_type_id)
         VALUES ($1, $2, 'ar', $3, 'typing', $4)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.cls_a)
    .bind(f.cls_b) // conclude_type in another KB
    .execute(&pool)
    .await;
    assert!(err.is_err(), "arule.conclude_type→foreign must be rejected");
    let err = sqlx::query(
        "INSERT INTO attribute_rules (id, kb_id, name, subject_type_id, conclusion,
                                      conclude_predicate_id, conclude_value)
         VALUES ($1, $2, 'ar', $3, 'attribute', $4, '42'::jsonb)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.cls_a)
    .bind(f.rel_b) // conclude_predicate in another KB
    .execute(&pool)
    .await;
    assert!(
        err.is_err(),
        "arule.conclude_predicate→foreign must be rejected"
    );

    // — #901: tables added after 0070 also fall under the same invariant —
    // The three reference columns of the entailment rule ontology (composite foreign keys)
    for (col, foreign_id, what) in [
        ("subject_type_id", f.cls_b, "irule.subject_type"),
        ("object_type_id", f.cls_b, "irule.object_type"),
    ] {
        let err = sqlx::query(&format!(
            "INSERT INTO implication_rules (id, kb_id, trigger, phrase,
                                           conclude_property_id, status, {col})
             VALUES ($1, $2, 'phrase', 'p', $3, 'approved', $4)",
        ))
        .bind(Uuid::now_v7())
        .bind(f.a)
        .bind(f.rel_a)
        .bind(foreign_id)
        .execute(&pool)
        .await;
        assert!(err.is_err(), "{what}→foreign must be rejected");
    }
    let err = sqlx::query(
        "INSERT INTO implication_rules (id, kb_id, trigger, phrase,
                                       conclude_property_id, status)
         VALUES ($1, $2, 'phrase', 'p', $3, 'approved')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.rel_b)
    .execute(&pool)
    .await;
    assert!(
        err.is_err(),
        "irule.conclude_property→foreign must be rejected"
    );

    // The entity answered by the reading cache (composite foreign key); the UPDATE path is blocked likewise
    let err = sqlx::query(
        "INSERT INTO phrase_readings (kb_id, reading, phrase, entity_id)
         VALUES ($1, 'nationality', 'x', $2)",
    )
    .bind(f.a)
    .bind(f.ent_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "reading.entity→foreign must be rejected");
    let err =
        sqlx::query("UPDATE phrase_readings SET entity_id = $1 WHERE kb_id = $2 AND phrase = 'r'")
            .bind(f.ent_b)
            .bind(f.a)
            .execute(&pool)
            .await;
    assert!(
        err.is_err(),
        "changing reading.entity to another KB must also be rejected"
    );

    // Provenance of implied facts (owner-derived trigger): the rule, the triggering statement, and the triggering entity
    // are all judged by the KB of their owning fact; the CHECK requires exactly one of statement / entity to be non-empty
    let err = sqlx::query(
        "INSERT INTO implied_fact_sources (fact_id, rule_id, statement_id)
         VALUES ($1, $2, $3)",
    )
    .bind(f.fact_a)
    .bind(f.irule_b)
    .bind(f.fact_a)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "implied.rule→foreign must be rejected");
    let err = sqlx::query(
        "INSERT INTO implied_fact_sources (fact_id, rule_id, statement_id)
         VALUES ($1, $2, $3)",
    )
    .bind(f.fact_a)
    .bind(f.irule_a)
    .bind(f.fact_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "implied.statement→foreign must be rejected");
    let err = sqlx::query(
        "INSERT INTO implied_fact_sources (fact_id, rule_id, entity_id)
         VALUES ($1, $2, $3)",
    )
    .bind(f.fact_a)
    .bind(f.irule_a)
    .bind(f.ent_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "implied.entity→foreign must be rejected");
    // UPDATE path: first land a legal row, then change its rule to one from another KB — UPDATE OF watches it
    sqlx::query(
        "INSERT INTO implied_fact_sources (fact_id, rule_id, entity_id)
         VALUES ($1, $2, $3)",
    )
    .bind(f.fact_b)
    .bind(f.irule_b)
    .bind(f.ent_b)
    .execute(&pool)
    .await?;
    let err = sqlx::query("UPDATE implied_fact_sources SET rule_id = $1 WHERE fact_id = $2")
        .bind(f.irule_a)
        .bind(f.fact_b)
        .execute(&pool)
        .await;
    assert!(
        err.is_err(),
        "changing implied.rule to another KB must also be rejected"
    );

    // Errata ledger: the document watched by run and the six reference columns on action are all composite foreign keys
    let err = sqlx::query("INSERT INTO errata_runs (id, kb_id, document_id) VALUES ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(f.a)
        .bind(f.doc_b)
        .execute(&pool)
        .await;
    assert!(err.is_err(), "erratarun.document→foreign must be rejected");
    for (col, foreign_id, what) in [
        ("fact_id", f.fact_b, "errata.fact"),
        ("statement_id", f.fact_b, "errata.statement"),
        ("predicate_id", f.rel_b, "errata.predicate"),
        ("new_fact_id", f.fact_b, "errata.new_fact"),
    ] {
        let err = sqlx::query(&format!(
            "INSERT INTO errata_actions (id, kb_id, run_id, document_id, action, status, {col})
             VALUES ($1, $2, $3, $4, 'keep', 'held', $5)",
        ))
        .bind(Uuid::now_v7())
        .bind(f.a)
        .bind(f.erun_a)
        .bind(f.doc_a)
        .bind(foreign_id)
        .execute(&pool)
        .await;
        assert!(err.is_err(), "{what}→foreign must be rejected");
    }
    // The run and document columns are checked through the UPDATE path: moving an existing action onto a run or document
    // in another KB, the composite foreign key still rejects it on the spot
    for (col, foreign_id, what) in [
        ("run_id", f.erun_b, "errata.run"),
        ("document_id", f.doc_b, "errata.document"),
    ] {
        let err = sqlx::query(&format!(
            "UPDATE errata_actions SET {col} = $1 WHERE id = $2",
        ))
        .bind(foreign_id)
        .bind(f.eact_a)
        .execute(&pool)
        .await;
        assert!(
            err.is_err(),
            "changing {what} to another KB must also be rejected"
        );
    }

    // Rule definition history and the derivations inferred from it (the #913 table is also in scope)
    // superseded versions do not occupy the "one current per rule" unique index, so the row has only
    // the cross-KB fault left, and it is the composite foreign key that rejects it
    let err = sqlx::query(
        "INSERT INTO attribute_rule_versions (id, kb_id, rule_id, seq, definition,
                                              superseded_at)
         VALUES ($1, $2, $3, 9, '{}'::jsonb, now())",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.arule_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "arversion.rule→foreign must be rejected");
    // A derivation's "inferred from which version of the definition" must not point at another KB (the identity key does not include the rule column,
    // so changing object_value to a different value avoids colliding with an already-landed derivation)
    let err = sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_value,
                                    attribute_rule_id, attribute_rule_version_id)
         VALUES ($1, $2, $3, $4, '\"v\"'::jsonb, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.ent_a)
    .bind(f.rel_a)
    .bind(f.arule_a)
    .bind(f.arv_b)
    .execute(&pool)
    .await;
    assert!(
        err.is_err(),
        "derived.attribute_rule_version→foreign 必须被拒"
    );

    // Two edges that were born with composite keys in 0071/0080 but had never been registered in the health check
    let err = sqlx::query(
        "INSERT INTO attribute_rules (id, kb_id, name, subject_type_id, conclusion,
                                      conclude_predicate_id, join_predicate_id)
         VALUES ($1, $2, 'ar-r', $3, 'relation', $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.cls_a)
    .bind(f.rel_a)
    .bind(f.rel_b)
    .execute(&pool)
    .await;
    assert!(
        err.is_err(),
        "arule.join_predicate→foreign must be rejected"
    );
    let err = sqlx::query(
        "INSERT INTO name_vectors (fact_id, kb_id, entity_id, embedding)
         VALUES ($1, $2, $3, '[0]'::vector)",
    )
    .bind(f.fact_b)
    .bind(f.a)
    .bind(f.ent_a)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "namevector.fact→foreign must be rejected");

    // — KB transfer: moving an already-referenced row to another KB amounts to turning every row pointing at it into a bad row at once
    for (table, id, what) in [
        ("entities", f.ent_a, "entities.kb_id"),
        ("entity_types", f.cls_a, "entity_types.kb_id"),
        ("relation_types", f.rel_a, "relation_types.kb_id"),
        ("derived_facts", f.der_a, "derived_facts.kb_id"),
        ("rules", f.rule_a, "rules.kb_id"),
        ("attribute_rules", f.arule_a, "attribute_rules.kb_id"),
        ("implication_rules", f.irule_a, "implication_rules.kb_id"),
        ("errata_runs", f.erun_a, "errata_runs.kb_id"),
        ("errata_actions", f.eact_a, "errata_actions.kb_id"),
        (
            "attribute_rule_versions",
            f.arv_a,
            "attribute_rule_versions.kb_id",
        ),
    ] {
        let err = sqlx::query(&format!("UPDATE {table} SET kb_id = $2 WHERE id = $1"))
            .bind(id)
            .bind(f.b)
            .execute(&pool)
            .await;
        assert!(err.is_err(), "transferring {what} must be rejected");
    }
    // The reading cache's key is not an id — moving it between KBs by (kb_id, reading, phrase) is likewise not allowed
    let err =
        sqlx::query("UPDATE phrase_readings SET kb_id = $1 WHERE kb_id = $2 AND phrase = 'r'")
            .bind(f.b)
            .bind(f.a)
            .execute(&pool)
            .await;
    assert!(
        err.is_err(),
        "transferring phrase_readings.kb_id must be rejected"
    );

    // — Guard against false positives: legal writes within the same KB still go through as usual
    sqlx::query(
        "INSERT INTO fact_derivations (derived_fact_id, premise_fact_id, seq)
         VALUES ($1, $2, 0)",
    )
    .bind(f.der_a)
    .bind(f.fact_a)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fact_qualifiers (fact_id, qualifier_type_id, entity_id)
         VALUES ($1, $2, $3)",
    )
    .bind(f.fact_a)
    .bind(f.rel_a)
    .bind(f.ent_a)
    .execute(&pool)
    .await?;
    // Writes within the same KB for the new families must also go through as usual: provenance, readings, errata, definition history, name vectors
    sqlx::query(
        "INSERT INTO implied_fact_sources (fact_id, rule_id, statement_id)
         VALUES ($1, $2, $3)",
    )
    .bind(f.fact_a)
    .bind(f.irule_a)
    .bind(f.fact_a)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO phrase_readings (kb_id, reading, phrase, entity_id)
         VALUES ($1, 'nationality', 'y', $2)",
    )
    .bind(f.a)
    .bind(f.ent_a)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO errata_actions (id, kb_id, run_id, document_id, fact_id,
                                     statement_id, predicate_id, new_fact_id,
                                     action, status)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'revise', 'held')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.erun_a)
    .bind(f.doc_a)
    .bind(f.fact_a)
    .bind(f.fact_a)
    .bind(f.rel_a)
    .bind(f.fact_a)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_value,
                                    attribute_rule_id, attribute_rule_version_id)
         VALUES ($1, $2, $3, $4, '\"ok\"'::jsonb, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.ent_a)
    .bind(f.rel_a)
    .bind(f.arule_a)
    .bind(f.arv_a)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO name_vectors (fact_id, kb_id, entity_id, embedding)
         VALUES ($1, $2, $3, '[0]'::vector)",
    )
    .bind(f.fact_a)
    .bind(f.a)
    .bind(f.ent_a)
    .execute(&pool)
    .await?;

    cleanup(&pool, &f).await
}

/// Legacy bad rows: the health check reports the right edge, and the corresponding page read rejects it likewise
#[tokio::test]
async fn malformed_rows_fail_every_exported_edge_closed() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *tx)
        .await?;

    // Each edge builds one bad row — all of them land on KB A
    sqlx::query(
        "INSERT INTO fact_derivations (derived_fact_id, premise_fact_id, seq)
         VALUES ($1, $2, 0)",
    )
    .bind(f.der_a)
    .bind(f.fact_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO fact_derivations (derived_fact_id, premise_derived_id, seq)
         VALUES ($1, $2, 1)",
    )
    .bind(f.der_a)
    .bind(f.der_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO fact_qualifiers (fact_id, qualifier_type_id, value)
         VALUES ($1, $2, '\"lit\"'::jsonb)",
    )
    .bind(f.fact_a)
    .bind(f.rel_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO fact_qualifiers (fact_id, qualifier_type_id, entity_id)
         VALUES ($1, $2, $3)",
    )
    .bind(f.fact_a)
    .bind(f.rel_a)
    .bind(f.ent_b)
    .execute(&mut *tx)
    .await?;
    // A fact's supersedes points at a fact in another KB
    sqlx::query("UPDATE facts SET supersedes = $2 WHERE id = $1")
        .bind(f.fact_a)
        .bind(f.fact_b)
        .execute(&mut *tx)
        .await?;
    // A statement source points at a statement in another KB (both paths: typed_fact_sources and from_statement_id)
    sqlx::query("INSERT INTO typed_fact_sources (fact_id, statement_id) VALUES ($1, $2)")
        .bind(f.fact_a)
        .bind(f.fact_b)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE facts SET from_statement_id = $2 WHERE id = $1")
        .bind(f.fact_a)
        .bind(f.fact_b)
        .execute(&mut *tx)
        .await?;
    // An attribute value of an open statement points at an entity in another KB; another role word is recorded on a chunk in another KB
    sqlx::query(
        "INSERT INTO statement_qualifiers (fact_id, role, entity_id, chunk_id)
         VALUES ($1, 'as', $2, $3)",
    )
    .bind(f.fact_a)
    .bind(f.ent_b)
    .bind(f.chunk_a)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO statement_qualifiers (fact_id, role, value, chunk_id)
         VALUES ($1, 'lines', '\"1\"'::jsonb, $2)",
    )
    .bind(f.fact_a)
    .bind(f.chunk_b)
    .execute(&mut *tx)
    .await?;
    // Two bad ways for a time mention: owned by A but pointing at B's chunk (the health check can see this, since rows are counted by their own
    // kb); owned by B but hung off A's fact (when fetched by fact, the page validation catches it)
    sqlx::query(
        "INSERT INTO time_mentions (id, kb_id, fact_id, chunk_id, text, char_start)
         VALUES ($1, $2, $3, $4, '去年', 0)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.fact_a)
    .bind(f.chunk_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO time_mentions (id, kb_id, fact_id, chunk_id, text, char_start)
         VALUES ($1, $2, $3, $4, '前年', 2)",
    )
    .bind(Uuid::now_v7())
    .bind(f.b)
    .bind(f.fact_a)
    .bind(f.chunk_a)
    .execute(&mut *tx)
    .await?;
    // A type binding points at a class in another KB
    sqlx::query(
        "INSERT INTO type_bindings (id, kb_id, kind_word, status, type_id)
         VALUES ($1, $2, 'corp', 'bound', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.cls_b)
    .execute(&mut *tx)
    .await?;
    // A phrase binding points at an attribute in another KB
    sqlx::query(
        "INSERT INTO phrase_bindings (id, kb_id, phrase, relation_type_id,
                                     direction, status)
         VALUES ($1, $2, 'runs', $3, 'forward', 'bound')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.rel_b)
    .execute(&mut *tx)
    .await?;
    // A derivation's axiom rule changes KB
    sqlx::query("UPDATE derived_facts SET rule_id = $2 WHERE id = $1")
        .bind(f.der_a)
        .bind(f.rule_b)
        .execute(&mut *tx)
        .await?;
    // An entity's class changes KB
    sqlx::query("UPDATE entities SET type_id = $2 WHERE id = $1")
        .bind(f.ent_a)
        .bind(f.cls_b)
        .execute(&mut *tx)
        .await?;
    // Class hierarchy and domain/range
    sqlx::query("INSERT INTO entity_type_parents (child_id, parent_id) VALUES ($1, $2)")
        .bind(f.cls_a)
        .bind(f.cls_b)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO relation_type_domains (relation_type_id, entity_type_id) VALUES ($1, $2)",
    )
    .bind(f.rel_a)
    .bind(f.cls_b)
    .execute(&mut *tx)
    .await?;

    // — The health check now covers all structural edges that 0070 protects: each remaining edge also builds one bad row —
    // the evidence's chunk pointer and the redundant document pointer
    sqlx::query("INSERT INTO fact_evidence (fact_id, chunk_id) VALUES ($1, $2)")
        .bind(f.fact_a)
        .bind(f.chunk_b)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO fact_evidence (fact_id, chunk_id, document_id) VALUES ($1, $2, $3)")
        .bind(f.fact_a)
        .bind(f.chunk_a)
        .bind(f.doc_b)
        .execute(&mut *tx)
        .await?;
    // A chunk hangs under a document in another KB
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 9, 'x')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.doc_b)
    .execute(&mut *tx)
    .await?;
    // The subject / object / predicate of the fact ontology
    sqlx::query(
        "UPDATE facts SET subject_id = $2, object_id = $2, predicate_id = $3 WHERE id = $1",
    )
    .bind(f.fact_a)
    .bind(f.ent_b)
    .bind(f.rel_b)
    .execute(&mut *tx)
    .await?;
    // The subject / object / predicate of the derivation ontology; another derivation goes through a business rule — attribute_rule in another KB
    sqlx::query(
        "UPDATE derived_facts SET subject_id = $2, object_id = $2, predicate_id = $3 WHERE id = $1",
    )
    .bind(f.der_a)
    .bind(f.ent_b)
    .bind(f.rel_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_id, attribute_rule_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.ent_a)
    .bind(f.rel_a)
    .bind(f.ent_a)
    .bind(f.arule_b)
    .execute(&mut *tx)
    .await?;
    // The two reference columns of class disjointness are two structural edges sharing one error label —
    // a_id in one row of another KB, b_id in one row of another KB (the CHECK forbids a_id = b_id)
    sqlx::query("INSERT INTO entity_type_disjoint (kb_id, a_id, b_id) VALUES ($1, $2, $3)")
        .bind(f.a)
        .bind(f.cls_b)
        .bind(f.cls_a)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO entity_type_disjoint (kb_id, a_id, b_id) VALUES ($1, $2, $3)")
        .bind(f.a)
        .bind(f.cls_a)
        .bind(f.cls_b)
        .execute(&mut *tx)
        .await?;
    // The edge attribute on range and on the relation declaration
    sqlx::query(
        "INSERT INTO relation_type_ranges (relation_type_id, entity_type_id) VALUES ($1, $2)",
    )
    .bind(f.rel_a)
    .bind(f.cls_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO relation_type_qualifiers (relation_type_id, qualifier_type_id) VALUES ($1, $2)",
    )
    .bind(f.rel_a)
    .bind(f.rel_b)
    .execute(&mut *tx)
    .await?;
    // Self-reference of the relation
    sqlx::query("UPDATE relation_types SET inverse_of = $2, sub_property_of = $2 WHERE id = $1")
        .bind(f.rel_a)
        .bind(f.rel_b)
        .execute(&mut *tx)
        .await?;
    // The predicate of the axiom
    sqlx::query("UPDATE rules SET predicate_id = $2 WHERE id = $1")
        .bind(f.rule_a)
        .bind(f.rel_b)
        .execute(&mut *tx)
        .await?;
    // The two reference columns of the business rule, the class of the typing conclusion, and the predicate of the condition
    sqlx::query(
        "UPDATE attribute_rules SET subject_type_id = $2, conclude_predicate_id = $3 WHERE id = $1",
    )
    .bind(f.arule_a)
    .bind(f.cls_b)
    .bind(f.rel_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO attribute_rules (id, kb_id, name, subject_type_id, conclusion, conclude_type_id)
         VALUES ($1, $2, 'ar-t', $3, 'typing', $4)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.cls_a)
    .bind(f.cls_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO attribute_rule_conditions (id, rule_id, seq, predicate_id, op)
         VALUES ($1, $2, 0, $3, 'present')",
    )
    .bind(Uuid::now_v7())
    .bind(f.arule_a)
    .bind(f.rel_b)
    .execute(&mut *tx)
    .await?;
    // A time mention points at a fact in another KB (the chunk in the same KB) — timemention.fact is verified separately on its own
    sqlx::query(
        "INSERT INTO time_mentions (id, kb_id, fact_id, chunk_id, text, char_start)
         VALUES ($1, $2, $3, $4, '明年', 4)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.fact_b)
    .bind(f.chunk_a)
    .execute(&mut *tx)
    .await?;
    // The two type reference columns of a phrase binding
    sqlx::query(
        "INSERT INTO phrase_bindings (id, kb_id, phrase, subject_type_id, status)
         VALUES ($1, $2, 'runs-sub', $3, 'none')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.cls_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO phrase_bindings (id, kb_id, phrase, object_type_id, status)
         VALUES ($1, $2, 'runs-obj', $3, 'none')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.cls_b)
    .execute(&mut *tx)
    .await?;

    // — Bad rows for the #901 families: the 15 new edges protected by 0091, plus two that carried composite keys from the start but
    //    had never been registered in the health check (join_predicate, name_vectors) —
    // The three reference columns of the entailment rule ontology
    sqlx::query(
        "INSERT INTO implication_rules (id, kb_id, trigger, phrase, conclude_property_id,
                                        status, subject_type_id)
         VALUES ($1, $2, 'phrase', 'p', $3, 'approved', $4)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.rel_a)
    .bind(f.cls_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO implication_rules (id, kb_id, trigger, phrase, conclude_property_id,
                                        status, object_type_id)
         VALUES ($1, $2, 'phrase', 'p', $3, 'approved', $4)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.rel_a)
    .bind(f.cls_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO implication_rules (id, kb_id, trigger, phrase, conclude_property_id,
                                        status)
         VALUES ($1, $2, 'phrase', 'p', $3, 'approved')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.rel_b)
    .execute(&mut *tx)
    .await?;
    // The reading cache answered an entity from another KB
    sqlx::query(
        "INSERT INTO phrase_readings (kb_id, reading, phrase, entity_id)
         VALUES ($1, 'nationality', 'bad', $2)",
    )
    .bind(f.a)
    .bind(f.ent_b)
    .execute(&mut *tx)
    .await?;
    // Three bad ways for implied-fact provenance (CHECK: exactly one of statement / entity;
    // the primary key is (fact_id, rule_id), and landing another same-KB fact offsets the three rows)
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence)
         VALUES ($1, $2, $3, $3, 0.9)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.ent_a)
    .execute(&mut *tx)
    .await?;
    let fact_a2: Uuid = sqlx::query_scalar(
        "SELECT id FROM facts WHERE kb_id = $1 AND id <> $2 ORDER BY id DESC LIMIT 1",
    )
    .bind(f.a)
    .bind(f.fact_a)
    .fetch_one(&mut *tx)
    .await?;
    // Land another rule in this KB: the three rows each take a (fact_id, rule_id) primary key
    let irule_a2 = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO implication_rules (id, kb_id, trigger, phrase,
                                        conclude_property_id, status)
         VALUES ($1, $2, 'phrase', 'q', $3, 'approved')",
    )
    .bind(irule_a2)
    .bind(f.a)
    .bind(f.rel_a)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO implied_fact_sources (fact_id, rule_id, statement_id)
         VALUES ($1, $2, $3)",
    )
    .bind(f.fact_a)
    .bind(f.irule_b)
    .bind(f.fact_a)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO implied_fact_sources (fact_id, rule_id, statement_id)
         VALUES ($1, $2, $3)",
    )
    .bind(fact_a2)
    .bind(f.irule_a)
    .bind(f.fact_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO implied_fact_sources (fact_id, rule_id, entity_id)
         VALUES ($1, $2, $3)",
    )
    .bind(fact_a2)
    .bind(irule_a2)
    .bind(f.ent_b)
    .execute(&mut *tx)
    .await?;
    // Definition history hangs off a rule in another KB; the derivation records a version from another KB (the version row is marked superseded,
    // so it does not collide with the seeded current row under the "one per rule" unique index)
    sqlx::query(
        "INSERT INTO attribute_rule_versions (id, kb_id, rule_id, seq, definition,
                                              superseded_at)
         VALUES ($1, $2, $3, 9, '{}'::jsonb, now())",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.arule_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_value,
                                    attribute_rule_id, attribute_rule_version_id)
         VALUES ($1, $2, $3, $4, '\"bad\"'::jsonb, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.ent_a)
    .bind(f.rel_a)
    .bind(f.arule_a)
    .bind(f.arv_b)
    .execute(&mut *tx)
    .await?;
    // Errata ledger: run watches a document in another KB, one bad row each for run and document on action,
    // and the other four columns are stuffed into the same row
    sqlx::query("INSERT INTO errata_runs (id, kb_id, document_id) VALUES ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(f.a)
        .bind(f.doc_b)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO errata_actions (id, kb_id, run_id, document_id, action, status)
         VALUES ($1, $2, $3, $4, 'keep', 'held')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.erun_b)
    .bind(f.doc_a)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO errata_actions (id, kb_id, run_id, document_id, action, status)
         VALUES ($1, $2, $3, $4, 'keep', 'held')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.erun_a)
    .bind(f.doc_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO errata_actions (id, kb_id, run_id, document_id, fact_id,
                                     statement_id, predicate_id, new_fact_id,
                                     action, status)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'revise', 'held')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.erun_a)
    .bind(f.doc_a)
    .bind(f.fact_b)
    .bind(f.fact_b)
    .bind(f.rel_b)
    .bind(f.fact_b)
    .execute(&mut *tx)
    .await?;
    // The join predicate of relation shape points at another KB; name vectors each get one bad row at both ends
    sqlx::query(
        "INSERT INTO attribute_rules (id, kb_id, name, subject_type_id, conclusion,
                                      conclude_predicate_id, join_predicate_id)
         VALUES ($1, $2, 'ar-j', $3, 'relation', $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.cls_a)
    .bind(f.rel_a)
    .bind(f.rel_b)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO name_vectors (fact_id, kb_id, entity_id, embedding)
         VALUES ($1, $2, $3, '[0]'::vector)",
    )
    .bind(f.fact_b)
    .bind(f.a)
    .bind(f.ent_a)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO name_vectors (fact_id, kb_id, entity_id, embedding)
         VALUES ($1, $2, $3, '[0]'::vector)",
    )
    .bind(f.fact_a)
    .bind(f.a)
    .bind(f.ent_b)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    drop(conn);

    let err = utopia_store::export::provenance_integrity(&mut pool.begin().await?, f.a).await;
    let msg = format!("{err:?}");
    assert!(err.is_err(), "KB A's health check must refuse export");
    // The health check covers all structural edges protected by 0070 / 0091 — every kind of bad row buried must be named,
    // and so must the edges the export has not yet serialised (rules, bindings, time mentions, conditions): if a protected
    // same-KB reference is broken on the ledger, this export cannot be trusted. 57 error labels correspond to 58 structural edges
    // (class.disjoint's a_id and b_id share one label)
    for edge in [
        "evidence.chunk",
        "evidence.document",
        "chunk.document",
        "derivation.premise_fact",
        "derivation.premise_derived",
        "qualifier.type",
        "qualifier.entity",
        "fact.subject",
        "fact.object",
        "fact.predicate",
        "fact.supersedes",
        "fact.from_statement",
        "derived.subject",
        "derived.object",
        "derived.predicate",
        "derived.rule",
        "derived.attribute_rule",
        "derived.attribute_rule_version",
        "entity.type",
        "class.parent",
        "class.disjoint",
        "relation.domain",
        "relation.range",
        "relation.qualifier",
        "relation.inverse",
        "relation.sub_property",
        "rule.predicate",
        "arule.subject_type",
        "arule.conclude_type",
        "arule.conclude_predicate",
        "arule.join_predicate",
        "arversion.rule",
        "condition.predicate",
        "factsource.statement",
        "squalifier.entity",
        "squalifier.chunk",
        "timemention.fact",
        "timemention.chunk",
        "binding.type",
        "pbinding.subject_type",
        "pbinding.object_type",
        "pbinding.relation",
        "irule.subject_type",
        "irule.object_type",
        "irule.conclude_property",
        "reading.entity",
        "implied.rule",
        "implied.statement",
        "implied.entity",
        "erratarun.document",
        "errata.run",
        "errata.document",
        "errata.fact",
        "errata.statement",
        "errata.predicate",
        "errata.new_fact",
        "namevector.fact",
        "namevector.entity",
    ] {
        assert!(
            msg.contains(edge),
            "the health check should report {edge}: {msg}"
        );
    }

    // Page-by-page validation: wherever the bad row falls, that page is rejected — no forged IRI may leak out
    assert!(
        utopia_store::export::facts_page(&mut pool.begin().await?, f.a, None)
            .await
            .is_err()
    );
    assert!(
        utopia_store::export::derived_page(&mut pool.begin().await?, f.a, None)
            .await
            .is_err()
    );
    assert!(
        utopia_store::export::entities_page(&mut pool.begin().await?, f.a, None)
            .await
            .is_err()
    );
    assert!(utopia_store::export::classes(&mut pool.begin().await?, f.a)
        .await
        .is_err());
    assert!(
        utopia_store::export::relations(&mut pool.begin().await?, f.a)
            .await
            .is_err()
    );

    cleanup(&pool, &f).await
}

/// Only rows left behind between the two reads take part in validation: a referenced row that disappears between reads makes the violation
/// **the ownership selected atomically with the page**, not a JOIN at a later moment — a dangling reference is still rejected
#[tokio::test]
async fn retained_row_validation_survives_dangling_and_late_state() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    // Legal evidence first
    sqlx::query("INSERT INTO fact_evidence (fact_id, chunk_id, document_id) VALUES ($1, $2, $3)")
        .bind(f.fact_a)
        .bind(f.chunk_a)
        .bind(f.doc_a)
        .execute(&pool)
        .await?;
    utopia_store::export::facts_page(&mut pool.begin().await?, f.a, None).await?;

    // Delete the document by bypassing the trigger: the evidence row still points at it — dangling is not "does not exist, so skip"
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(f.doc_a)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    drop(conn);

    let err = utopia_store::export::facts_page(&mut pool.begin().await?, f.a, None).await;
    let msg = format!("{err:?}");
    assert!(err.is_err(), "a dangling document pointer must be rejected");
    assert!(
        msg.contains("evidence.document"),
        "该报 evidence.document: {msg}"
    );

    // Likewise: the premise row is deleted, and the derived's premise array still holds a dead pointer — page-by-page validation must catch it
    sqlx::query(
        "INSERT INTO fact_derivations (derived_fact_id, premise_fact_id, seq)
         VALUES ($1, $2, 0)",
    )
    .bind(f.der_a)
    .bind(f.fact_a)
    .execute(&pool)
    .await?;
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM facts WHERE id = $1")
        .bind(f.fact_a)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    drop(conn);

    let err = utopia_store::export::derived_page(&mut pool.begin().await?, f.a, None).await;
    let msg = format!("{err:?}");
    assert!(err.is_err(), "a dangling premise must be rejected");
    assert!(
        msg.contains("derivation.premise_fact"),
        "该报 derivation.premise_fact: {msg}"
    );

    cleanup(&pool, &f).await
}
