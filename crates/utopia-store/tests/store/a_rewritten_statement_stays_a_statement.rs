//! Rewriting an open statement yields a statement (a postscript to RC-004): `layer` and `phrase` follow the corrected row.
//!
//! Human interval correction (`correct_interval`, PATCH /facts/{id}) and rehoming subject and object when retracting a merge (`rehome_tx`) used to
//! copy only the typed columns. So an open statement corrected into a typed row with no predicate could no longer be read by materialization, and its typed row
//! is voided along with the old statement and never comes back. Here each path is changed once, then a materialization round runs, and the typed rows are rebuilt from the changed statement.
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is unset. Creates and tears down its own database, never touching an existing one.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use utopia_store::graph::Validity;
use utopia_store::materialize::materialize;
use uuid::Uuid;

/// One knowledge base: Ada and Bea as two people, one Port Ellen, and one open statement bound to based_in
/// "Ada is based in Port Ellen, since 2020-01-01", whose typed row has already been materialized from it
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

    /// Which layer a row is in, which sentence it says, and whether it has a predicate
    async fn layer_of(&self, fact: Uuid) -> sqlx::Result<(String, Option<String>, Option<Uuid>)> {
        sqlx::query_as("SELECT layer, phrase, predicate_id FROM facts WHERE id = $1")
            .bind(fact)
            .fetch_one(&self.pool)
            .await
    }

    /// Live based_in typed rows after a materialization round: subject and start
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
