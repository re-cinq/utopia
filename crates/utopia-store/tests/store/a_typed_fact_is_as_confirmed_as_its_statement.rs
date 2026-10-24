//! 类型化行跟着陈述走，「待确认」也跟着陈述判（RC-005）。
//!
//! 物化时类型化行抄一份陈述的证据。来源重推、同一条陈述再被说出，新分块上的证据落在
//! 陈述上，类型化行手里只剩旧分块的那份——陈述现行、有活证据，它却被列成待确认。
//! 这里钉住：有陈述来源的类型化行，只要哪条来源陈述现行、开放、证据落在活分块上，就不算
//! 待确认；陈述的证据也全被取代了，两条一起算。没有陈述来源的行照旧判。

use sqlx::PgPool;
use utopia_store::{graph, materialize, review};
use uuid::Uuid;

async fn unconfirmed(pool: &PgPool, kb: Uuid) -> anyhow::Result<(i64, Vec<Uuid>)> {
    let mut stale: Vec<Uuid> = graph::stale_facts(pool, kb, 50, 0)
        .await?
        .into_iter()
        .map(|f| f.id)
        .collect();
    stale.sort();
    Ok((review::counts(pool, kb).await?.unconfirmed, stale))
}

#[tokio::test]
async fn a_typed_fact_is_unconfirmed_only_once_its_statement_is() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let [org, ws, kb, class, ada, port, oslo, based_in, binding] =
        std::array::from_fn(|_| Uuid::now_v7());
    let [source, doc, first, second, statement, lone] = std::array::from_fn(|_| Uuid::now_v7());
    let run = async {
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','typed-confirmed');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','typed-confirmed');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','typed-confirmed');
             INSERT INTO entity_types(id,kb_id,key,label) VALUES ('{class}','{kb}','thing','Thing');
             INSERT INTO entities(id,kb_id,type_id,canonical_name) VALUES
                  ('{ada}','{kb}','{class}','Ada'), ('{port}','{kb}','{class}','Port Ellen'),
                  ('{oslo}','{kb}','{class}','Oslo');
             INSERT INTO relation_types (id,kb_id,key,label,temporal)
                  VALUES ('{based_in}','{kb}','based_in','based in','state');
             INSERT INTO phrase_bindings
                  (id,kb_id,phrase,subject_type_id,object_type_id,object_is_value,
                   relation_type_id,direction,status)
                  VALUES ('{binding}','{kb}','is based in','{class}','{class}',false,
                          '{based_in}','forward','bound');
             INSERT INTO sources(id,kb_id,kind,name) VALUES ('{source}','{kb}','statements','mirror');
             INSERT INTO documents(id,kb_id,filename,sha256,source_id)
                  VALUES ('{doc}','{kb}','a.md','sha-a','{source}');
             INSERT INTO chunks(id,kb_id,document_id,seq,text,doc_version) VALUES
                  ('{first}','{kb}','{doc}',0,'v1','1'), ('{second}','{kb}','{doc}',1,'v2','2');
             INSERT INTO facts (id,kb_id,subject_id,object_id,layer,phrase,confidence)
                  VALUES ('{statement}','{kb}','{ada}','{port}','open','is based in',0.9);
             INSERT INTO facts (id,kb_id,subject_id,predicate_id,object_id,confidence)
                  VALUES ('{lone}','{kb}','{ada}','{based_in}','{oslo}',0.9);
             INSERT INTO fact_evidence (fact_id,chunk_id,document_id,quote) VALUES
                  ('{statement}','{first}','{doc}','q'), ('{lone}','{first}','{doc}','q');"
        ))
        .execute(&pool)
        .await?;
        // 物化：类型化行抄走陈述此刻的证据（第一版分块）
        materialize::materialize(&pool, kb).await?;
        let typed: Uuid = sqlx::query_scalar(
            "SELECT f.id FROM facts f JOIN typed_fact_sources s ON s.fact_id = f.id
              WHERE s.statement_id = $1 AND f.invalidated_at IS NULL",
        )
        .bind(statement)
        .fetch_one(&pool)
        .await?;

        // 重推：第一版被取代，同一条陈述在第二版上又被说出
        sqlx::raw_sql(&format!(
            "UPDATE chunks SET superseded_at = now() WHERE id = '{first}';
             INSERT INTO fact_evidence (fact_id,chunk_id,document_id,quote)
                  VALUES ('{statement}','{second}','{doc}','q');"
        ))
        .execute(&pool)
        .await?;
        assert_eq!(
            unconfirmed(&pool, kb).await?,
            (1, vec![lone]),
            "陈述还被说着，它的类型化行不待确认；没有陈述来源的行照旧"
        );

        sqlx::query("UPDATE chunks SET superseded_at = now() WHERE id = $1")
            .bind(second)
            .execute(&pool)
            .await?;
        let mut all = vec![statement, lone, typed];
        all.sort();
        assert_eq!(
            unconfirmed(&pool, kb).await?,
            (3, all),
            "陈述的证据也全被取代了，陈述和它的类型化行一起待确认"
        );
        anyhow::Ok(())
    }
    .await;
    sqlx::raw_sql(&format!(
        "DELETE FROM jobs WHERE payload->>'kb_id' = '{kb}';
         DELETE FROM knowledge_bases WHERE id = '{kb}';
         DELETE FROM organizations WHERE id = '{org}';"
    ))
    .execute(&pool)
    .await?;
    run
}
