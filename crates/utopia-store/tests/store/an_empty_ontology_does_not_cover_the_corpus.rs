//! How many things the document says that the ontology still cannot hold: category words not bound to a class, and phrase shapes not bound to a property.
//!
//! The workbench used to say "the ontology covers the corpus" by reading 0003's `ontology_misses`, but open-graph extraction does not write to that table.
//! In a library with not one class, twenty-two category words had no home, yet the page said "covered". These two numbers are
//! what the ontology agent reads (0061 decision 2); other libraries' numbers do not count.
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. It creates and tears down its own data and never touches an existing database.

use sqlx::PgPool;
use utopia_store::ontology::{uncovered, Uncovered};
use uuid::Uuid;

const ORG: &str = "empty-ontology-does-not-cover-test";

#[tokio::test]
async fn kind_words_and_phrases_with_no_place_are_counted_per_base() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(ORG)
        .execute(&pool)
        .await?;
    let (org, ws, kb, other, person, leads) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','{ORG}');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','{ORG}');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','a'), ('{other}','{ws}','b');"
    ))
    .execute(&pool)
    .await?;

    let run = async {
        // Nothing has been read yet: nothing to wait for
        let none = Uncovered {
            kind_words: 0,
            phrases: 0,
        };
        assert_eq!(uncovered(&pool, kb).await?, none);

        // Documents read, yet not a single class: every category word is left without a home
        for word in ["person", "project", "city"] {
            sqlx::query(
                "INSERT INTO type_bindings (id, kb_id, kind_word, status) VALUES ($1, $2, $3, 'none')",
            )
            .bind(Uuid::now_v7())
            .bind(kb)
            .bind(word)
            .execute(&pool)
            .await?;
        }
        assert_eq!(uncovered(&pool, kb).await?.kind_words, 3);

        // One class and one attribute added: those bound do not count; those judged none and those with inconsistent votes do count
        sqlx::raw_sql(&format!(
            "INSERT INTO entity_types(id,kb_id,key,label) VALUES ('{person}','{kb}','person','person');
             INSERT INTO relation_types(id,kb_id,key,label,kind,temporal,description)
                 VALUES ('{leads}','{kb}','leads','leads','relation','state','');
             UPDATE type_bindings SET status='bound', type_id='{person}'
              WHERE kb_id='{kb}' AND kind_word='person';
             UPDATE type_bindings SET status='undecided' WHERE kb_id='{kb}' AND kind_word='city';
             INSERT INTO phrase_bindings (id, kb_id, phrase, subject_type_id, status, relation_type_id, direction)
                 VALUES ('{}','{kb}','负责','{person}','bound','{leads}','forward'),
                        ('{}','{kb}','汇报给','{person}','none',NULL,NULL),
                        ('{}','{kb}','调任','{person}','undecided',NULL,NULL);
             INSERT INTO type_bindings (id, kb_id, kind_word, status) VALUES ('{}','{other}','supplier','none');",
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        ))
        .execute(&pool)
        .await?;
        assert_eq!(
            uncovered(&pool, kb).await?,
            Uncovered {
                kind_words: 2,
                phrases: 2,
            }
        );
        assert_eq!(uncovered(&pool, other).await?.kind_words, 1, "each base counts its own");
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
