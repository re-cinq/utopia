//! Cache of signature vectors (#1097, 0110): looked up by (store, model, hash); at the end of a round, delete the rows whose text no longer appears in this round,
//! and the rows embedded by another model. Other stores' rows are not touched.
use sqlx::PgPool;
use utopia_store::signature_vectors;
use uuid::Uuid;

#[tokio::test]
async fn a_signature_vector_lasts_while_its_text_does() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let (org, ws, kb, other) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','signature-vectors');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','signature-vectors');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES
             ('{kb}','{ws}','signature-vectors'), ('{other}','{ws}','signature-vectors-other');"
    ))
    .execute(&pool)
    .await?;
    let result = async {
        let (kept, gone) = (vec![1u8; 32], vec![2u8; 32]);
        signature_vectors::put(
            &pool,
            kb,
            "m1",
            &[
                (kept.clone(), vec![1.0, 0.0]),
                (gone.clone(), vec![0.0, 1.0]),
            ],
        )
        .await?;
        signature_vectors::put(&pool, kb, "m0", &[(kept.clone(), vec![0.5, 0.5])]).await?;
        signature_vectors::put(&pool, other, "m1", &[(gone.clone(), vec![0.0, 1.0])]).await?;
        // Key collision is not overwritten: same model and same text, the vector is the same one
        signature_vectors::put(&pool, kb, "m1", &[(kept.clone(), vec![9.0, 9.0])]).await?;

        let got = signature_vectors::get(&pool, kb, "m1", &[kept.clone(), gone.clone()]).await?;
        anyhow::ensure!(got.len() == 2, "both texts are cached: {got:?}");
        anyhow::ensure!(got[&kept] == vec![1.0, 0.0], "the first vector stays");
        let other_model =
            signature_vectors::get(&pool, kb, "m0", std::slice::from_ref(&kept)).await?;
        anyhow::ensure!(other_model[&kept] == vec![0.5, 0.5], "keyed by model too");

        let deleted =
            signature_vectors::prune(&pool, kb, "m1", std::slice::from_ref(&kept)).await?;
        anyhow::ensure!(
            deleted == 2,
            "the text that no longer occurs and the other model's row go, got {deleted}"
        );
        let left: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT model, text_hash FROM signature_vectors WHERE kb_id=$1 ORDER BY model",
        )
        .bind(kb)
        .fetch_all(&pool)
        .await?;
        anyhow::ensure!(left == vec![("m1".to_string(), kept.clone())], "{left:?}");
        let untouched =
            signature_vectors::get(&pool, other, "m1", std::slice::from_ref(&gone)).await?;
        anyhow::ensure!(untouched.len() == 1, "another base's vectors are its own");

        anyhow::ensure!(signature_vectors::prune(&pool, kb, "m1", &[]).await? == 1);
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(org)
        .execute(&pool)
        .await?;
    pool.close().await;
    result
}
