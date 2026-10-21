//! 改来源时设置、保留、关掉衰减（RC-003），打在真库上。
//!
//! 要钉住的行为：
//!
//! - **不写就不动**：更新别的字段，衰减原样
//! - **写对象就打开**：存下完整的四个字段，之后默认值再变也不会动它
//! - **写 null 就关掉**
//! - **越界的值被拒绝**，库里的原值不变
//! - **列表看得到**
//!
//! 没有 `UTOPIA_DATABASE_URL` 时跳过而不是失败。自建自拆，绝不碰已有的库。

use sqlx::PgPool;
use utopia_store::decay::DecaySettings;
use utopia_store::sources;
use uuid::Uuid;

#[tokio::test]
async fn a_source_decay_is_set_and_cleared_by_update() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let (org, ws, kb, source) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'source-decay-update-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'source-decay-update-test')",
    )
    .bind(ws)
    .bind(org)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name)
         VALUES ($1, $2, 'source-decay-update-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO sources (id, kb_id, kind, name) VALUES ($1, $2, 'folder', 'f')")
        .bind(source)
        .bind(kb)
        .execute(&pool)
        .await?;

    let run = async {
        let renamed = sources::update(&pool, source, Some("g"), None, None, None, None).await?;
        assert_eq!(renamed.decay, None, "a source starts without decay");

        let settings = DecaySettings {
            max_items: 3,
            ..DecaySettings::default()
        };
        let on =
            sources::update(&pool, source, None, None, None, None, Some(Some(&settings))).await?;
        assert_eq!(
            on.decay,
            Some(serde_json::json!({
                "max_items": 3, "half_life_days": 60, "stale_after_days": 30, "min_age_days": 30
            }))
        );

        let kept = sources::update(&pool, source, Some("h"), None, None, None, None).await?;
        assert_eq!(kept.decay, on.decay, "another field leaves decay alone");

        let too_long = DecaySettings {
            half_life_days: 400,
            ..DecaySettings::default()
        };
        let refused =
            sources::update(&pool, source, None, None, None, None, Some(Some(&too_long))).await;
        assert!(refused.is_err());
        let listed = sources::list(&pool, kb).await?;
        assert_eq!(listed[0].decay, on.decay, "a refused value changes nothing");

        let off = sources::update(&pool, source, None, None, None, None, Some(None)).await?;
        assert_eq!(off.decay, None, "null turns decay off");
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
