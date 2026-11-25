//! An embedded ontology row must not be judged stale forever just because its description has trailing newlines (#672), tested against a real database.
//!
//! Staleness is judged by whether the text embedded at the time matches the text to embed now. Previously the latter was rebuilt in SQL with `btrim`,
//! while the former was produced by Rust's `trim`: `btrim` removes only spaces, but `trim` also removes newlines and tabs.
//! schema.org descriptions end in "\n      ", so these rows stayed stale even after embedding -- the backfill task re-embedded them over and over, extraction
//! gated on an index that could never be filled, and documents eventually failed on deadline. Three things are pinned:
//!
//! 1. **Once embedded, not stale**: classes whose description ends in a newline and spaces, and whose label has tabs at both ends (the whole text and the label
//!       both), and relations; after `set_type_embeddings` writes back, `types_needing_embedding` is empty
//! 2. **Changing the description still re-embeds**: the criterion was not loosened by this
//! 3. **Changing the model re-embeds everything**
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. It creates and tears down its own data and never touches an existing database.

use sqlx::PgPool;
use utopia_store::ontology::{self, EmbedField, TypeKind};
use uuid::Uuid;

#[tokio::test]
async fn an_embedded_row_with_surrounding_whitespace_stays_embedded() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (place, located) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'trim-stale-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'trim-stale-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'trim-stale-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    let result = async {
        // What schema.org imports looks like: the description ends with a newline plus a run of spaces; the label has a tab at both ends
        sqlx::query(
            "INSERT INTO entity_types (id, kb_id, key, label, description)
             VALUES ($1, $2, 'place', E'\\tPlace\\t', E'Entities that have a somewhat fixed, physical extension.\\n      ')",
        )
        .bind(place)
        .bind(kb)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, description)
             VALUES ($1, $2, 'located_in', 'located in', E'\\n  The place something is in.\\n')",
        )
        .bind(located)
        .bind(kb)
        .execute(&pool)
        .await?;

        let embed_all = |model: &'static str| {
            let pool = pool.clone();
            async move {
                let stale = ontology::types_needing_embedding(&pool, kb, model, None).await?;
                let items: Vec<_> = stale.iter().cloned().map(|t| (t, vec![0.1f32, 0.2, 0.3])).collect();
                ontology::set_type_embeddings(&pool, model, &items).await?;
                anyhow::Ok(stale)
            }
        };

        // ---- Part 1: on the first round all three texts must be embedded: the class's whole text and label, and the relation's whole text
        let first = embed_all("model-a").await?;
        let mut shape: Vec<(Uuid, &str, &str)> = first
            .iter()
            .map(|t| {
                (
                    t.id,
                    match t.kind {
                        TypeKind::Entity => "entity",
                        TypeKind::Relation => "relation",
                    },
                    match t.field {
                        EmbedField::Full => "full",
                        EmbedField::Label => "label",
                    },
                )
            })
            .collect();
        shape.sort();
        let mut want = vec![
            (place, "entity", "full"),
            (place, "entity", "label"),
            (located, "relation", "full"),
        ];
        want.sort();
        assert_eq!(shape, want);
        let label = first
            .iter()
            .find(|t| t.field == EmbedField::Label)
            .map(|t| t.text.clone());
        assert_eq!(label.as_deref(), Some("Place"), "送去嵌的是去掉首尾空白的 label");

        // Embedded, so not stale: before this fix, these three were handed back unchanged every round, and the backfill never caught up
        let again = ontology::types_needing_embedding(&pool, kb, "model-a", None).await?;
        assert!(again.is_empty(), "嵌过的行又判成陈的：{again:?}");

        // ---- Part 2: the description changed, so only that one is re-embedded
        sqlx::query("UPDATE relation_types SET description = 'Where something is.' WHERE id = $1")
            .bind(located)
            .execute(&pool)
            .await?;
        let changed = ontology::types_needing_embedding(&pool, kb, "model-a", None).await?;
        assert_eq!(changed.len(), 1);
        assert_eq!(
            (changed[0].id, changed[0].text.as_str()),
            (located, "located in\nWhere something is.")
        );
        embed_all("model-a").await?;

        // ---- Part 3: the model changed, so all three are re-embedded
        let switched = ontology::types_needing_embedding(&pool, kb, "model-b", None).await?;
        assert_eq!(switched.len(), 3);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    result
}
