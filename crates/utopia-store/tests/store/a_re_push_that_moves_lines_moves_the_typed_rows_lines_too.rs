//! 0070 决定 4：每次物化都把来源陈述的证据与角色词补到活着的类型化行上，不只在建行/并行时。
//!
//! 一条绑定的陈述说 `lines: 40-52`，物化成类型化行；来源重推同一个 external_id，
//! 旧块被取代、新块说 `lines: 58-70`；再物化一遍，类型化行读出来只有 58-70。
//!
//! 没有 `UTOPIA_DATABASE_URL` 时跳过而不是失败。自建自拆，绝不碰已有的库。

use sqlx::PgPool;
use utopia_store::materialize::materialize;
use uuid::Uuid;

#[tokio::test]
async fn a_re_push_that_moves_lines_moves_the_typed_rows_lines_too() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = 'materialize-repush-test'")
        .execute(&pool)
        .await?;
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'materialize-repush-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'materialize-repush-test')",
    )
    .bind(ws)
    .bind(org)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name)
         VALUES ($1, $2, 'materialize-repush-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    let run = async {
        let (test_class, file_class, covers) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        for (id, key) in [(test_class, "test"), (file_class, "file")] {
            sqlx::query(
                "INSERT INTO entity_types (id, kb_id, key, label, color, shape)
                 VALUES ($1, $2, $3, $3, '#7fd0ff', 'circle')",
            )
            .bind(id)
            .bind(kb)
            .bind(key)
            .execute(&pool)
            .await?;
        }
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, temporal)
             VALUES ($1, $2, 'covers_file', 'covers file', 'state')",
        )
        .bind(covers)
        .bind(kb)
        .execute(&pool)
        .await?;
        let (parses_date, parse_rs, statement) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        for (id, type_id, name) in [
            (parses_date, test_class, "test-parses-a-date"),
            (parse_rs, file_class, "parse.rs"),
        ] {
            sqlx::query(
                "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(kb)
            .bind(type_id)
            .bind(name)
            .execute(&pool)
            .await?;
        }
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, object_id, layer, phrase, confidence)
             VALUES ($1, $2, $3, $4, 'open', 'covers', 0.9)",
        )
        .bind(statement)
        .bind(kb)
        .bind(parses_date)
        .bind(parse_rs)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO phrase_bindings
                 (id, kb_id, phrase, subject_type_id, object_type_id, object_is_value,
                  relation_type_id, direction, status)
             VALUES ($1, $2, 'covers', $3, $4, false, $5, 'forward', 'bound')",
        )
        .bind(Uuid::now_v7())
        .bind(kb)
        .bind(test_class)
        .bind(file_class)
        .bind(covers)
        .execute(&pool)
        .await?;

        // 一份文档的一块：陈述在这块上说 lines
        let doc = Uuid::now_v7();
        sqlx::query("INSERT INTO documents (id, kb_id, filename, sha256) VALUES ($1, $2, 'ci.json', 'ci')")
            .bind(doc)
            .bind(kb)
            .execute(&pool)
            .await?;
        let says_lines_on = |chunk: Uuid, seq: i32, lines: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(chunk)
                .bind(kb)
                .bind(doc)
                .bind(seq)
                .bind(lines)
                .execute(&pool)
                .await?;
                sqlx::query(
                    "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, quote_start, quote_end)
                     VALUES ($1, $2, $3, $4, 0, 11)",
                )
                .bind(statement)
                .bind(chunk)
                .bind(lines)
                .bind(doc)
                .execute(&pool)
                .await?;
                sqlx::query(
                    "INSERT INTO statement_qualifiers (fact_id, role, value, chunk_id)
                     VALUES ($1, 'lines', to_jsonb($3::text), $2)",
                )
                .bind(statement)
                .bind(chunk)
                .bind(lines)
                .execute(&pool)
                .await?;
                anyhow::Ok(())
            }
        };
        let (old_chunk, new_chunk) = (Uuid::now_v7(), Uuid::now_v7());
        says_lines_on(old_chunk, 0, "lines 40-52").await?;
        materialize(&pool, kb).await?;
        let typed: Uuid = sqlx::query_scalar(
            "SELECT id FROM facts WHERE kb_id = $1 AND layer = 'typed' AND invalidated_at IS NULL",
        )
        .bind(kb)
        .fetch_one(&pool)
        .await?;

        // 重推：旧块被取代，新块说 58-70
        sqlx::query("UPDATE chunks SET superseded_at = now() WHERE id = $1")
            .bind(old_chunk)
            .execute(&pool)
            .await?;
        says_lines_on(new_chunk, 1, "lines 58-70").await?;
        materialize(&pool, kb).await?;

        let read = utopia_store::graph::statement_qualifiers_for(&pool, &[typed], None).await?;
        let said: Vec<(String, Option<serde_json::Value>)> = read[&typed]
            .iter()
            .map(|q| (q.role.clone(), q.value.clone()))
            .collect();
        assert_eq!(
            said,
            vec![("lines".to_string(), Some(serde_json::json!("lines 58-70")))]
        );
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
