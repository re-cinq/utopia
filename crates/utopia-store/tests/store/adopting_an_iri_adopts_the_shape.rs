//! Once a class is claimed by a vocabulary, its **shape must also tell the truth**.
//!
//! The shape carries the provenance: square = declared by the vocabulary, circle = grown out of the corpus. But `adopt_iri_onto_key`
//! was written in the era when the shape did not yet express provenance; it only writes the IRI — so importing schema.org into a KB that already has `person` /
//! `organization`, those classes **got an IRI but stayed circles**.
//! An IRI with a circle shape: the picture is lying.
//!
//! This seam only shows up on a real import: looking at `adopt_iri_onto_key`'s signature and call sites alone,
//! nothing hints that "another field should change along with it". It was hit in practice (5 classes in the demo KB were affected).
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is unset. Creates and tears down its own; never touches an existing database.

use sqlx::PgPool;
use uuid::Uuid;

#[tokio::test]
async fn adopting_an_iri_turns_the_class_square() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    // Sweep up before starting: a panicking assertion skips teardown, and one failure leaves garbage for the next run
    sqlx::query("DELETE FROM organizations WHERE name = 'adopt-shape-test'")
        .execute(&pool)
        .await?;

    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'adopt-shape-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'adopt-shape-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'adopt-shape-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    let run = async {
        // A "grown out of the corpus" class: no IRI, so it is a circle
        let grown = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO entity_types (id, kb_id, key, label, color, shape)
             VALUES ($1, $2, 'person', 'Person', '#7fd0ff', 'circle')",
        )
        .bind(grown)
        .bind(kb)
        .execute(&pool)
        .await?;

        let shape: String = sqlx::query_scalar("SELECT shape FROM entity_types WHERE id = $1")
            .bind(grown)
            .fetch_one(&pool)
            .await?;
        assert_eq!(shape, "circle", "还没被认领时该是圆的");

        // Import a vocabulary, adopting schema.org's Person onto this key
        let adopted = utopia_store::ontology::adopt_iri_onto_key(
            &pool,
            kb,
            "person",
            "https://schema.org/Person",
        )
        .await?;
        assert_eq!(adopted, Some(grown), "该认到那个已有的类上");

        let (iri, shape, color): (Option<String>, String, String) =
            sqlx::query_as("SELECT iri, shape, color FROM entity_types WHERE id = $1")
                .bind(grown)
                .fetch_one(&pool)
                .await?;
        assert_eq!(iri.as_deref(), Some("https://schema.org/Person"));
        assert_eq!(
            shape, "square",
            "认领之后是词表声明的类了，形状要跟着说实话"
        );
        // **The color must not change**: color is identity (the same key is always the same color),
        // claiming changes the provenance, not the identity
        assert_eq!(color, "#7fd0ff", "认领不该动用户看惯的颜色");

        // Idempotent: claiming again must change nothing (the iri is already non-empty, so the UPDATE matches nothing)
        let again = utopia_store::ontology::adopt_iri_onto_key(
            &pool,
            kb,
            "person",
            "https://example.org/OtherPerson",
        )
        .await?;
        assert_eq!(again, None, "已被认领的类不该被另一个词汇表抢走");
        let iri2: Option<String> = sqlx::query_scalar("SELECT iri FROM entity_types WHERE id = $1")
            .bind(grown)
            .fetch_one(&pool)
            .await?;
        assert_eq!(iri2.as_deref(), Some("https://schema.org/Person"));

        Ok::<(), anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE name = 'adopt-shape-test'")
        .execute(&pool)
        .await?;
    run
}
