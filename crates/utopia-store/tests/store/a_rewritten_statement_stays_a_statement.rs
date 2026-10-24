//! 改写一条开放陈述，改出来的还是陈述（RC-004 的后记）：`layer` 和 `phrase` 随修正行走。
//!
//! 人改区间（`correct_interval`，PATCH /facts/{id}）和撤回合并搬主宾（`rehome_tx`）从前
//! 只复制类型化那几列，开放陈述改完成了没有谓词的类型化行：物化再也读不到它，它的类型化行
//! 随旧陈述作废后就不再回来。这里两条路各改一次，再物化一轮，类型化行按改过的陈述重建。
//!
//! 没有 `UTOPIA_DATABASE_URL` 时跳过而不是失败。自建自拆，绝不碰已有的库。

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use utopia_store::graph::Validity;
use utopia_store::materialize::materialize;
use uuid::Uuid;

/// 一个库：Ada 和 Bea 两个人、一处 Port Ellen，一条绑到 based_in 的开放陈述
/// 「Ada is based in Port Ellen，自 2020-01-01」，已经物化出它的类型化行
struct Statement {
    pool: PgPool,
    org: Uuid,
    kb: Uuid,
    ada: Uuid,
    bea: Uuid,
    based_in: Uuid,
    id: Uuid,
}

impl Statement {
    async fn new(pool: PgPool, name: &str) -> anyhow::Result<Self> {
        let [org, ws, kb, class, ada, bea, port, based_in, binding, id] =
            std::array::from_fn(|_| Uuid::now_v7());
        // Only locally generated UUIDs and a fixed name are interpolated into fixture SQL.
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','{name}');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','{name}');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','{name}');
             INSERT INTO entity_types(id,kb_id,key,label) VALUES ('{class}','{kb}','thing','Thing');
             INSERT INTO entities(id,kb_id,type_id,canonical_name) VALUES
                  ('{ada}','{kb}','{class}','Ada'), ('{bea}','{kb}','{class}','Bea'),
                  ('{port}','{kb}','{class}','Port Ellen');
             INSERT INTO relation_types (id,kb_id,key,label,temporal)
                  VALUES ('{based_in}','{kb}','based_in','based in','state');
             INSERT INTO phrase_bindings
                  (id,kb_id,phrase,subject_type_id,object_type_id,object_is_value,
                   relation_type_id,direction,status)
                  VALUES ('{binding}','{kb}','is based in','{class}','{class}',false,
                          '{based_in}','forward','bound');
             INSERT INTO facts (id,kb_id,subject_id,object_id,layer,phrase,confidence,
                                valid_from,valid_from_precision)
                  VALUES ('{id}','{kb}','{ada}','{port}','open','is based in',0.9,
                          '2020-01-01','day');"
        ))
        .execute(&pool)
        .await?;
        materialize(&pool, kb).await?;
        Ok(Self {
            pool,
            org,
            kb,
            ada,
            bea,
            based_in,
            id,
        })
    }

    /// 一行是哪一层、说的是哪句话、有没有谓词
    async fn layer_of(&self, fact: Uuid) -> sqlx::Result<(String, Option<String>, Option<Uuid>)> {
        sqlx::query_as("SELECT layer, phrase, predicate_id FROM facts WHERE id = $1")
            .bind(fact)
            .fetch_one(&self.pool)
            .await
    }

    /// 物化一轮之后活着的 based_in 类型化行：主语和起点
    async fn typed_after_materialize(&self) -> anyhow::Result<Vec<(Uuid, Option<DateTime<Utc>>)>> {
        materialize(&self.pool, self.kb).await?;
        Ok(sqlx::query_as(
            "SELECT subject_id, valid_from FROM facts
              WHERE kb_id = $1 AND layer = 'typed' AND predicate_id = $2 AND invalidated_at IS NULL",
        )
        .bind(self.kb)
        .bind(self.based_in)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn remove(&self) -> sqlx::Result<()> {
        sqlx::query("DELETE FROM jobs WHERE payload->>'kb_id' = $1")
            .bind(self.kb.to_string())
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(self.org)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn statement_layer() -> (String, Option<String>, Option<Uuid>) {
    ("open".into(), Some("is based in".into()), None)
}

#[tokio::test]
async fn a_statement_with_a_corrected_start_stays_open_and_its_typed_row_follows(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let statement = Statement::new(PgPool::connect(&url).await?, "corrected-statement").await?;
    let run = async {
        let start: DateTime<Utc> = "2021-03-01T00:00:00Z".parse()?;
        let corrected = utopia_store::temporal::correct_interval(
            &statement.pool,
            statement.id,
            Validity::starting(Some(start), Some("day")),
        )
        .await?
        .expect("the statement was live");
        assert_eq!(statement.layer_of(corrected).await?, statement_layer());
        assert_eq!(
            statement.typed_after_materialize().await?,
            vec![(statement.ada, Some(start))],
            "类型化行按改过起点的陈述重建"
        );
        anyhow::Ok(())
    }
    .await;
    statement.remove().await?;
    run
}

#[tokio::test]
async fn a_statement_rehomed_to_bea_stays_open_and_its_typed_row_follows() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let statement = Statement::new(PgPool::connect(&url).await?, "rehomed-statement").await?;
    let run = async {
        let mut tx = statement.pool.begin().await?;
        let moved =
            utopia_store::temporal::rehome_tx(&mut tx, statement.id, Some(statement.bea), None)
                .await?
                .expect("the statement was live");
        tx.commit().await?;
        assert_eq!(statement.layer_of(moved).await?, statement_layer());
        assert_eq!(
            statement.typed_after_materialize().await?,
            vec![(statement.bea, Some("2020-01-01T00:00:00Z".parse()?))],
            "类型化行按搬到 Bea 的陈述重建"
        );
        anyhow::Ok(())
    }
    .await;
    statement.remove().await?;
    run
}
