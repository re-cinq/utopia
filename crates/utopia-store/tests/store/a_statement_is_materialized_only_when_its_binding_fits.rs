//! A statement becomes a typed row only when its binding fits (0067): the binding must be bound and belong to this database,
//! the statement must be a live open statement, and its subject/object classes and whether the object is a literal must match the signature, with the direction making sense;
//! wording that differs only in case and whitespace counts as the same sentence. Each case starts from a fully matching statement and changes one thing.
//!
//! Without `UTOPIA_DATABASE_URL` it skips rather than fails. It sets up and tears down its own data and never touches an existing database.

use sqlx::PgPool;
use utopia_store::materialize::{count, materialize};
use uuid::Uuid;

#[derive(Clone, Copy)]
enum Class {
    Organization,
    Place,
    Untyped,
}

#[derive(Clone, Copy)]
enum Object {
    Entity(Class),
    Value,
}

#[derive(Clone, Copy)]
struct Case {
    binding_status: &'static str,
    binding_phrase: &'static str,
    binding_in_another_base: bool,
    binding_subject: Class,
    binding_object: Class,
    binding_expects_value: bool,
    binding_direction: &'static str,
    statement_phrase: &'static str,
    statement_subject: Class,
    statement_object: Object,
    statement_invalidated: bool,
    statement_layer: &'static str,
}

impl Case {
    /// "Harbor Bakery (organization) is based in Port Ellen (place)", binding exactly the same
    fn fitting() -> Self {
        Case {
            binding_status: "bound",
            binding_phrase: "is based in",
            binding_in_another_base: false,
            binding_subject: Class::Organization,
            binding_object: Class::Place,
            binding_expects_value: false,
            binding_direction: "forward",
            statement_phrase: "is based in",
            statement_subject: Class::Organization,
            statement_object: Object::Entity(Class::Place),
            statement_invalidated: false,
            statement_layer: "open",
        }
    }
}

async fn typed_rows_after_materializing(pool: &PgPool, case: Case) -> anyhow::Result<i64> {
    let (org, ws, kb, other_kb) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'binding-fit-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'binding-fit-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    for base in [kb, other_kb] {
        sqlx::query(
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3::text)",
        )
        .bind(base)
        .bind(ws)
        .bind(base.to_string())
        .execute(pool)
        .await?;
    }

    let run = async {
        let (organization, place, hq, founded_in, other_hq) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        for (id, key) in [(organization, "organization"), (place, "place")] {
            sqlx::query(
                "INSERT INTO entity_types (id, kb_id, key, label, color, shape)
                 VALUES ($1, $2, $3, $3, '#7fd0ff', 'circle')",
            )
            .bind(id)
            .bind(kb)
            .bind(key)
            .execute(pool)
            .await?;
        }
        // The bound classes and attributes must belong to the same database (foreign keys), so a binding from another database carries its own attributes, not its classes
        for (id, base, key) in [
            (hq, kb, "headquartered_in"),
            (founded_in, kb, "founded_in"),
            (other_hq, other_kb, "headquartered_in"),
        ] {
            sqlx::query(
                "INSERT INTO relation_types (id, kb_id, key, label, temporal)
                 VALUES ($1, $2, $3, $3, 'state')",
            )
            .bind(id)
            .bind(base)
            .bind(key)
            .execute(pool)
            .await?;
        }
        let class_id = |class: Class| match class {
            Class::Organization => Some(organization),
            Class::Place => Some(place),
            Class::Untyped => None,
        };

        let (bakery, port) = (Uuid::now_v7(), Uuid::now_v7());
        let object_class = match case.statement_object {
            Object::Entity(class) => class,
            Object::Value => Class::Place,
        };
        for (id, class, name) in [
            (bakery, case.statement_subject, "Harbor Bakery"),
            (port, object_class, "Port Ellen"),
        ] {
            sqlx::query(
                "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(kb)
            .bind(class_id(class))
            .bind(name)
            .execute(pool)
            .await?;
        }
        let (object_id, object_value) = match case.statement_object {
            Object::Entity(_) => (Some(port), None),
            Object::Value => (None, Some("\"Port Ellen\"")),
        };
        // An open statement carries no predicate_id; a typed-layer statement carries one that is not the bound one, so it does not merge into itself
        let predicate = (case.statement_layer == "typed").then_some(founded_in);
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, object_id, object_value, layer, phrase,
                                predicate_id, confidence, invalidated_at)
             VALUES ($1, $2, $3, $4, $5::text::jsonb, $6, $7, $8, 0.9,
                     CASE WHEN $9 THEN now() END)",
        )
        .bind(Uuid::now_v7())
        .bind(kb)
        .bind(bakery)
        .bind(object_id)
        .bind(object_value)
        .bind(case.statement_layer)
        .bind(case.statement_phrase)
        .bind(predicate)
        .bind(case.statement_invalidated)
        .execute(pool)
        .await?;

        let is_bound = case.binding_status == "bound";
        sqlx::query(
            "INSERT INTO phrase_bindings
                 (id, kb_id, phrase, subject_type_id, object_type_id, object_is_value,
                  relation_type_id, direction, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(Uuid::now_v7())
        .bind(if case.binding_in_another_base {
            other_kb
        } else {
            kb
        })
        .bind(case.binding_phrase)
        .bind(class_id(case.binding_subject))
        .bind(if case.binding_expects_value {
            None
        } else {
            class_id(case.binding_object)
        })
        .bind(case.binding_expects_value)
        .bind(is_bound.then_some(if case.binding_in_another_base {
            other_hq
        } else {
            hq
        }))
        .bind(is_bound.then_some(case.binding_direction))
        .bind(case.binding_status)
        .execute(pool)
        .await?;

        materialize(pool, kb).await?;
        Ok(count(pool, kb).await?)
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(pool)
        .await?;
    run
}

/// Each case: name, the expected number of typed rows, and the one place changed relative to "fully matching"
macro_rules! typed_rows_test {
    ($name:ident, $expected:expr, $($field:ident: $value:expr),+) => {
        #[tokio::test]
        async fn $name() -> anyhow::Result<()> {
            let Some(url) = utopia_store::test_db::url() else {
                return Ok(());
            };
            let pool = PgPool::connect(&url).await?;
            let case = Case { $($field: $value,)+ ..Case::fitting() };
            assert_eq!(typed_rows_after_materializing(&pool, case).await?, $expected);
            Ok(())
        }
    };
}

typed_rows_test!(a_phrase_in_other_case_and_spacing_is_materialized, 1,
    statement_phrase: "  Is   BASED\tin ");
typed_rows_test!(nothing_is_materialized_when_the_binding_is_none, 0,
    binding_status: "none");
typed_rows_test!(nothing_is_materialized_when_the_binding_is_undecided, 0,
    binding_status: "undecided");
typed_rows_test!(nothing_is_materialized_when_the_binding_belongs_to_another_base, 0,
    binding_in_another_base: true,
    binding_subject: Class::Untyped,
    binding_object: Class::Untyped,
    statement_subject: Class::Untyped,
    statement_object: Object::Entity(Class::Untyped));
typed_rows_test!(nothing_is_materialized_when_the_statement_is_invalidated, 0,
    statement_invalidated: true);
typed_rows_test!(nothing_is_materialized_when_the_statement_is_in_the_typed_layer, 0,
    statement_layer: "typed");
typed_rows_test!(nothing_is_materialized_when_the_subject_is_another_class, 0,
    statement_subject: Class::Place);
typed_rows_test!(nothing_is_materialized_when_the_binding_has_no_subject_class_but_the_subject_has_one, 0,
    binding_subject: Class::Untyped);
typed_rows_test!(nothing_is_materialized_when_the_binding_has_a_subject_class_but_the_subject_has_none, 0,
    statement_subject: Class::Untyped);
typed_rows_test!(nothing_is_materialized_when_the_object_is_another_class, 0,
    statement_object: Object::Entity(Class::Organization));
typed_rows_test!(nothing_is_materialized_when_the_binding_expects_a_value_and_the_object_is_an_entity, 0,
    binding_expects_value: true);
typed_rows_test!(nothing_is_materialized_when_the_binding_expects_an_entity_and_the_object_is_a_value, 0,
    statement_object: Object::Value);
typed_rows_test!(nothing_is_materialized_when_a_reverse_binding_meets_a_value_object, 0,
    binding_expects_value: true,
    binding_direction: "reverse",
    statement_object: Object::Value);
