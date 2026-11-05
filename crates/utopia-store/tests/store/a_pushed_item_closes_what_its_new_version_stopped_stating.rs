//! 推来的条目抽取完新版本之后，关上新版本不再陈述的事实（RC-006）：证据全停在这份文档被
//! 取代的分块上的事实按最后一次取代的那一刻闭合；新版本仍在说的、别的文档也在说的、
//! 名字事实，都不动。

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use utopia_store::unconfirmed;
use uuid::Uuid;

#[tokio::test]
async fn a_pushed_item_closes_only_the_facts_its_new_version_alone_stopped_stating(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let [org, ws, kb, class, ada, bea, cy, port, oslo, based_in, known_as, binding] =
        std::array::from_fn(|_| Uuid::now_v7());
    let [source, doc, other_doc, old, fresh, elsewhere] = std::array::from_fn(|_| Uuid::now_v7());
    let [dropped, kept, elsewhere_fact, name] = std::array::from_fn(|_| Uuid::now_v7());
    let ended: DateTime<Utc> = "2025-06-01T12:34:56Z".parse()?;
    let run = async {
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','close-of-document');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','close-of-document');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','close-of-document');
             INSERT INTO entity_types(id,kb_id,key,label) VALUES ('{class}','{kb}','thing','Thing');
             INSERT INTO entities(id,kb_id,type_id,canonical_name) VALUES
                  ('{ada}','{kb}','{class}','Ada'), ('{bea}','{kb}','{class}','Bea'),
                  ('{cy}','{kb}','{class}','Cy'), ('{port}','{kb}','{class}','Port Ellen'),
                  ('{oslo}','{kb}','{class}','Oslo');
             INSERT INTO relation_types (id,kb_id,key,label,temporal)
                  VALUES ('{based_in}','{kb}','based_in','based in','state');
             INSERT INTO relation_types (id,kb_id,key,label,kind,datatype,temporal,builtin)
                  VALUES ('{known_as}','{kb}','known_as','known as','attribute','text','state',true);
             INSERT INTO phrase_bindings
                  (id,kb_id,phrase,subject_type_id,object_type_id,object_is_value,
                   relation_type_id,direction,status)
                  VALUES ('{binding}','{kb}','is based in','{class}','{class}',false,
                          '{based_in}','forward','bound');
             INSERT INTO sources(id,kb_id,kind,name) VALUES ('{source}','{kb}','statements','mirror');
             INSERT INTO documents(id,kb_id,filename,sha256,source_id) VALUES
                  ('{doc}','{kb}','a.md','sha-a','{source}'),
                  ('{other_doc}','{kb}','b.md','sha-b','{source}');
             INSERT INTO chunks(id,kb_id,document_id,seq,text,doc_version,superseded_at,extracted_at) VALUES
                  ('{old}','{kb}','{doc}',0,'v1','1','2025-06-01T12:34:56.789Z','2025-06-01T12:00:00Z'),
                  ('{fresh}','{kb}','{doc}',1,'v2','2',NULL,'2025-06-01T12:40:00Z'),
                  ('{elsewhere}','{kb}','{other_doc}',0,'b','1',NULL,'2025-06-01T12:00:00Z');
             INSERT INTO facts (id,kb_id,subject_id,object_id,layer,phrase,confidence) VALUES
                  ('{dropped}','{kb}','{ada}','{port}','open','is based in',0.9),
                  ('{kept}','{kb}','{bea}','{port}','open','is based in',0.9),
                  ('{elsewhere_fact}','{kb}','{cy}','{port}','open','is based in',0.9);
             INSERT INTO facts (id,kb_id,subject_id,predicate_id,object_value,confidence) VALUES
                  ('{name}','{kb}','{ada}','{known_as}','{{\"value\":\"Ada\"}}',0.9);
             INSERT INTO fact_evidence (fact_id,chunk_id,document_id,quote) VALUES
                  ('{dropped}','{old}','{doc}','q'), ('{kept}','{old}','{doc}','q'),
                  ('{kept}','{fresh}','{doc}','q'), ('{elsewhere_fact}','{old}','{doc}','q'),
                  ('{elsewhere_fact}','{elsewhere}','{other_doc}','q'), ('{name}','{old}','{doc}','q');"
        ))
        .execute(&pool)
        .await?;

        let closing = unconfirmed::close_of_document(&pool, kb, doc).await?;
        assert_eq!(
            (closing.closed, closing.skipped, closing.remaining),
            (1, 0, 0),
            "only the fact whose evidence is the document's superseded chunks alone closes"
        );

        let successor = |fact: Uuid| {
            let pool = pool.clone();
            async move {
                sqlx::query_as::<_, (Option<DateTime<Utc>>, Option<String>, String)>(
                    "SELECT n.valid_to, n.valid_to_precision, n.layer FROM facts n
                       JOIN facts o ON o.id = n.supersedes
                      WHERE n.supersedes = $1 AND n.invalidated_at IS NULL
                        AND o.invalidated_at IS NOT NULL",
                )
                .bind(fact)
                .fetch_optional(&pool)
                .await
            }
        };
        assert_eq!(
            successor(dropped).await?,
            Some((Some(ended), Some("second".into()), "open".into())),
            "the dropped statement ends when its last evidence was superseded"
        );
        for untouched in [kept, elsewhere_fact, name] {
            assert_eq!(successor(untouched).await?, None);
        }
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
