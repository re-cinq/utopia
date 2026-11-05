//! 新版本写下了、还没被读：它引用的事实不算过期，也不被关。
//! 新版本被读过而没再提这条事实，才算它不再被陈述。

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use utopia_store::{graph, unconfirmed};
use uuid::Uuid;

#[tokio::test]
async fn a_fact_is_neither_stale_nor_closed_until_the_new_version_has_been_read(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let [org, ws, kb, class, ada, port, based_in, binding] =
        std::array::from_fn(|_| Uuid::now_v7());
    let [source, doc, old, fresh, said] = std::array::from_fn(|_| Uuid::now_v7());
    let run = async {
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','fact-waits');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','fact-waits');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','fact-waits');
             INSERT INTO entity_types(id,kb_id,key,label) VALUES ('{class}','{kb}','thing','Thing');
             INSERT INTO entities(id,kb_id,type_id,canonical_name) VALUES
                  ('{ada}','{kb}','{class}','Ada'), ('{port}','{kb}','{class}','Port Ellen');
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
             INSERT INTO chunks(id,kb_id,document_id,seq,text,doc_version,superseded_at,extracted_at) VALUES
                  ('{old}','{kb}','{doc}',0,'v1','1','2025-06-01T12:34:56Z','2025-06-01T12:00:00Z'),
                  ('{fresh}','{kb}','{doc}',1,'v2','2',NULL,NULL);
             INSERT INTO facts (id,kb_id,subject_id,object_id,layer,phrase,confidence,
                                valid_from,valid_from_precision)
                  VALUES ('{said}','{kb}','{ada}','{port}','open','is based in',0.9,'2020-01-01','day');
             INSERT INTO fact_evidence (fact_id,chunk_id,document_id,quote)
                  VALUES ('{said}','{old}','{doc}','q');"
        ))
        .execute(&pool)
        .await?;

        let is_stale = || async {
            let (_, facts) = graph::entity_detail(&pool, kb, ada, None, None).await?;
            let fact = facts.iter().find(|fact| fact.id == said).expect("the fact is on Ada");
            anyhow::Ok(fact.stale)
        };
        let is_listed_stale = || async {
            let listed = graph::stale_facts(&pool, kb, 100, 0).await?;
            anyhow::Ok(listed.iter().any(|item| item.id == said))
        };

        assert!(
            !is_stale().await?,
            "a new version was written and not yet read: nothing has been dropped yet"
        );
        assert!(
            !is_listed_stale().await?,
            "the review queue must not list a fact whose new version is unread"
        );
        let waiting = unconfirmed::close_of_source(&pool, kb, source, 1000).await?;
        assert_eq!(
            (waiting.closed, waiting.remaining),
            (0, 0),
            "a close run in the gap must close nothing"
        );

        let read_at: DateTime<Utc> = Utc::now();
        sqlx::query("UPDATE chunks SET extracted_at = $2 WHERE id = $1")
            .bind(fresh)
            .bind(read_at)
            .execute(&pool)
            .await?;
        assert!(is_stale().await?, "the new version spoke and did not restate the fact");
        assert!(is_listed_stale().await?, "the review queue lists it once the version has spoken");
        let closing = unconfirmed::close_of_source(&pool, kb, source, 1000).await?;
        assert_eq!(closing.closed, 1, "the close run closes what the read version dropped");
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
