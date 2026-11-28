//! 0070 决定 4：规则算出的行读出触发它的那条陈述的角色词，不只证据。
//!
//! 一条陈述在它的块上说 `lines 40-52`，触发一条批准的蕴含规则；物化之后隐含行读出来
//! 也是 `lines 40-52`。
//!
//! 没有 `UTOPIA_DATABASE_URL` 时跳过而不是失败。自建自拆，绝不碰已有的库。

use sqlx::PgPool;
use utopia_store::materialize::materialize;
use uuid::Uuid;

#[tokio::test]
async fn an_implied_row_reads_the_role_words_of_the_statement_that_implied_it() -> anyhow::Result<()>
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = 'implied-role-words-test'")
        .execute(&pool)
        .await?;
    let ids: Vec<Uuid> = (0..11).map(|_| Uuid::now_v7()).collect();
    let (org, ws, kb, person, company, works_for) =
        (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5]);
    let (lin, aster, statement, doc, chunk) = (ids[6], ids[7], ids[8], ids[9], ids[10]);
    // Only locally generated UUIDs are interpolated into fixture SQL.
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','implied-role-words-test');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','implied-role-words-test');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','implied-role-words-test');
         INSERT INTO entity_types(id,kb_id,key,label,color,shape) VALUES
             ('{person}','{kb}','person','person','#000','circle'),
             ('{company}','{kb}','organization','organization','#000','circle');
         INSERT INTO relation_types(id,kb_id,key,label,kind,temporal) VALUES
             ('{works_for}','{kb}','works_for','works for','relation','state');
         INSERT INTO entities(id,kb_id,canonical_name,type_id) VALUES
             ('{lin}','{kb}','Lin Zhao','{person}'),
             ('{aster}','{kb}','Aster Labs','{company}');
         INSERT INTO implication_rules(id,kb_id,trigger,phrase,subject_type_id,object_type_id,
                                       conclude_property_id,reading,status,decided_by)
             VALUES ('{rule}','{kb}','phrase','signed with','{person}','{company}','{works_for}',
                     NULL,'approved','person');
         INSERT INTO facts(id,kb_id,subject_id,object_id,layer,phrase,confidence)
             VALUES ('{statement}','{kb}','{lin}','{aster}','open','signed with',0.9);
         INSERT INTO documents(id,kb_id,filename,sha256) VALUES ('{doc}','{kb}','ci.json','ci');
         INSERT INTO chunks(id,kb_id,document_id,seq,text)
             VALUES ('{chunk}','{kb}','{doc}',0,'lines 40-52');
         INSERT INTO fact_evidence(fact_id,chunk_id,quote,document_id,quote_start,quote_end)
             VALUES ('{statement}','{chunk}','lines 40-52','{doc}',0,11);
         INSERT INTO statement_qualifiers(fact_id,role,value,chunk_id)
             VALUES ('{statement}','lines',to_jsonb('lines 40-52'::text),'{chunk}');",
        rule = Uuid::now_v7(),
    ))
    .execute(&pool)
    .await?;

    let run = async {
        materialize(&pool, kb).await?;
        let implied: Uuid = sqlx::query_scalar(
            "SELECT id FROM facts WHERE kb_id = $1 AND implied AND invalidated_at IS NULL",
        )
        .bind(kb)
        .fetch_one(&pool)
        .await?;

        let read = utopia_store::graph::statement_qualifiers_for(&pool, &[implied], None).await?;
        let said: Vec<(String, Option<serde_json::Value>)> = read
            .get(&implied)
            .map(|qualifiers| {
                qualifiers
                    .iter()
                    .map(|q| (q.role.clone(), q.value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            said,
            vec![("lines".to_string(), Some(serde_json::json!("lines 40-52")))]
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
