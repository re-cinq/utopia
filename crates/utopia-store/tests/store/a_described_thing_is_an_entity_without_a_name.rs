//! A described thing is an entity with no name (0044 slice 1, #729).
//!
//! `create_described` writes `description` and does not write `known_as`: a name fact is the bridge for recall (0041),
//! and a description cannot serve as a bridge — two things that happen to be described identically in two documents should not be linked for that reason.
//! So recall by name cannot find it, its name cell is empty, and `facts` has no row with it as subject.

use sqlx::PgPool;
use utopia_store::{names, resolution};
use uuid::Uuid;

const ORG: &str = "described-thing-test";

struct Fixture {
    org: Uuid,
    kb: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
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
    Ok(Fixture { org, kb })
}

#[tokio::test]
async fn a_described_thing_has_a_description_and_no_name() -> anyhow::Result<()> {
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
        let description = "the buyer's Shanghai subsidiary";
        let id = resolution::create_described(&pool, f.kb, description, Some("subsidiary"))
            .await?;

        let (canonical, desc, type_id, specific): (
            String,
            Option<String>,
            Option<Uuid>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT canonical_name, description, type_id, specific_type FROM entities WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(desc.as_deref(), Some(description), "the description is its identity");
        assert_eq!(canonical, description, "the display name is the description itself");
        assert!(type_id.is_none(), "the document's category word is not the entity's class");
        assert_eq!(specific.as_deref(), Some("subsidiary"));

        // No fact has it as subject — in particular no known_as
        let facts: i64 = sqlx::query_scalar("SELECT count(*) FROM facts WHERE subject_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(facts, 0, "a description is not written as a name fact");
        assert!(
            names::for_entity(&pool, f.kb, id, None).await?.is_empty(),
            "名字栏是空的"
        );

        // Recall by name cannot find it: a description is not a bridge
        assert_eq!(
            resolution::existing_by_name(&pool, f.kb, description).await?,
            None,
            "一段描述不能把后来的提及接到它身上"
        );
        // The name-fact recall cannot find it either: it counts only when the KB holds a name fact with the same lowercased value
        let recalled: Option<Uuid> = sqlx::query_scalar(&format!(
            "SELECT e.id FROM entities e
             WHERE e.kb_id = $1 AND e.merged_into IS NULL AND {}",
            names::has_name_in("e", 1, 2)
        ))
        .bind(f.kb)
        .bind(vec![description.to_lowercase()])
        .fetch_optional(&pool)
        .await?;
        assert_eq!(recalled, None);
        // A later mention with the same wording goes through resolution: a separate one is created, not attached to the described one
        let later =
            resolution::resolve_mention(&pool, f.kb, None, description, None, None, None, &[]).await?;
        assert!(later.created, "a description is not a bridge, mentions are not attributed to it");
        assert_ne!(later.entity_id, id);

        // Creating the same description again is another entity: the KB does not deduplicate; merging within the same document is the caller's job
        let twin = resolution::create_described(&pool, f.kb, description, None).await?;
        assert_ne!(twin, id);
        let specific: Option<String> =
            sqlx::query_scalar("SELECT specific_type FROM entities WHERE id = $1")
                .bind(twin)
                .fetch_one(&pool)
                .await?;
        assert!(specific.is_none());

        assert!(
            resolution::create_described(&pool, f.kb, "   ", None)
                .await
                .is_err(),
            "空描述不是一个东西"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
