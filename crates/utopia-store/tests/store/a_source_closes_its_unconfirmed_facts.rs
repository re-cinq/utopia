//! 一个来源关上自己不再陈述的事实（RC-004）：证据全停在被取代的分块上、证据全出自这个
//! 来源的现行事实，按最后一条证据被取代的那一刻闭合，走作废+改写，历史留着。
//! 还有活证据的、带着别的来源证据的、名字事实、起点在那一刻之后的，都不动。

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use utopia_store::{materialize, phrase_bindings, unconfirmed};
use uuid::Uuid;

#[tokio::test]
async fn a_source_closes_only_the_facts_it_alone_stopped_stating() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let [org, ws, kb, class, ada, bea, cy, port, oslo, based_in, known_as, binding] =
        std::array::from_fn(|_| Uuid::now_v7());
    let [source, other, doc, other_doc, old, live, elsewhere] =
        std::array::from_fn(|_| Uuid::now_v7());
    let [statement, still_stated, shared, name, late, lone] =
        std::array::from_fn(|_| Uuid::now_v7());
    let ended: DateTime<Utc> = "2025-06-01T12:34:56Z".parse()?;
    let run = async {
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','close-unconfirmed');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','close-unconfirmed');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','close-unconfirmed');
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
             INSERT INTO sources(id,kb_id,kind,name) VALUES
                  ('{source}','{kb}','statements','mirror'), ('{other}','{kb}','statements','other');
             INSERT INTO documents(id,kb_id,filename,sha256,source_id) VALUES
                  ('{doc}','{kb}','a.md','sha-a','{source}'),
                  ('{other_doc}','{kb}','b.md','sha-b','{other}');
             INSERT INTO chunks(id,kb_id,document_id,seq,text,doc_version,superseded_at) VALUES
                  ('{old}','{kb}','{doc}',0,'v1','1','2025-06-01T12:34:56.789Z'),
                  ('{live}','{kb}','{doc}',1,'v2','2',NULL),
                  ('{elsewhere}','{kb}','{other_doc}',0,'b','1','2025-06-01T12:00:00Z');
             INSERT INTO facts (id,kb_id,subject_id,object_id,layer,phrase,confidence,
                                valid_from,valid_from_precision) VALUES
                  ('{statement}','{kb}','{ada}','{port}','open','is based in',0.9,'2020-01-01','day'),
                  ('{still_stated}','{kb}','{bea}','{port}','open','is based in',0.9,NULL,NULL),
                  ('{shared}','{kb}','{cy}','{port}','open','is based in',0.9,NULL,NULL);
             INSERT INTO facts (id,kb_id,subject_id,predicate_id,object_id,object_value,confidence,
                                valid_from,valid_from_precision) VALUES
                  ('{name}','{kb}','{ada}','{known_as}',NULL,'{{\"value\":\"Ada\"}}',0.9,NULL,NULL),
                  ('{late}','{kb}','{bea}','{based_in}','{oslo}',NULL,0.9,'2026-01-01','day'),
                  ('{lone}','{kb}','{cy}','{based_in}','{oslo}',NULL,0.9,NULL,NULL);
             INSERT INTO fact_evidence (fact_id,chunk_id,document_id,quote) VALUES
                  ('{statement}','{old}','{doc}','q'), ('{still_stated}','{old}','{doc}','q'),
                  ('{still_stated}','{live}','{doc}','q'), ('{shared}','{old}','{doc}','q'),
                  ('{shared}','{elsewhere}','{other_doc}','q'), ('{name}','{old}','{doc}','q'),
                  ('{late}','{old}','{doc}','q'), ('{lone}','{old}','{doc}','q');"
        ))
        .execute(&pool)
        .await?;
        // 陈述先物化出类型化行：它跟着陈述走，不由这里关
        materialize::materialize(&pool, kb).await?;

        let first = unconfirmed::close_of_source(&pool, kb, source, 1).await?;
        assert_eq!(
            (first.closed, first.skipped, first.remaining),
            (1, 1, 1),
            "开放陈述先关；起点在终点之后的算跳过；还剩那条孤零零的类型化行"
        );
        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = $1 AND payload->>'kb_id' = $2 AND status = 'queued'",
        )
        .bind(phrase_bindings::MATERIALIZE_KIND)
        .bind(kb.to_string())
        .fetch_one(&pool)
        .await?;
        assert_eq!(queued, 1, "关了陈述就排一次类型化重算");
        let second = unconfirmed::close_of_source(&pool, kb, source, 1000).await?;
        assert_eq!((second.closed, second.skipped, second.remaining), (1, 1, 0));

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
        let closed_at = |layer: &str| Some((Some(ended), Some("second".into()), layer.into()));
        assert_eq!(successor(statement).await?, closed_at("open"), "陈述关上后还是陈述");
        assert_eq!(successor(lone).await?, closed_at("typed"));
        for untouched in [still_stated, shared, name, late] {
            assert_eq!(successor(untouched).await?, None);
        }

        materialize::materialize(&pool, kb).await?;
        let typed_end: Vec<Option<DateTime<Utc>>> = sqlx::query_scalar(
            "SELECT valid_to FROM facts WHERE kb_id = $1 AND layer = 'typed' AND subject_id = $2
               AND predicate_id = $3 AND invalidated_at IS NULL",
        )
        .bind(kb)
        .bind(ada)
        .bind(based_in)
        .fetch_all(&pool)
        .await?;
        assert_eq!(typed_end, vec![Some(ended)], "类型化行按关上的陈述重建");
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
