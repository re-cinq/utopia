//! Personal access tokens (0014 / migration 0017).
//!
//! This token is for MCP clients: long-lived, configured in a file on someone else's machine, acting with the identity of the
//! person who issued it. So it must be revocable, must expire, and its **scope can only be narrower than that person's**.
//!
//! Pins five things:
//!
//! - **The plaintext appears only once.** The store holds only a hash; even with the whole table, the string cannot be recovered
//! - **Revocation takes effect at once**, and it stamps rather than deletes the row — "this key once existed" must be findable
//! - **Expiry takes effect at once**, and the check is done in SQL, not after fetching
//! - **`kb_ids` only narrows**. A token limited to one store cannot reach another
//! - **`last_used_at` is written**. Before revoking, a person must be able to answer "is this key still in use?"

use chrono::{Duration, Utc};
use sqlx::PgPool;
use utopia_store::tokens;
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    user: Uuid,
    kb_a: Uuid,
    kb_b: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let org = Uuid::now_v7();
    let ws = Uuid::now_v7();
    let user = Uuid::now_v7();
    let (kb_a, kb_b) = (Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'token-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'token-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    for (kb, name) in [(kb_a, "kb-a"), (kb_b, "kb-b")] {
        sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3)")
            .bind(kb)
            .bind(ws)
            .bind(name)
            .execute(pool)
            .await?;
    }
    sqlx::query(
        "INSERT INTO users (id, org_id, email, password_hash, display_name)
         VALUES ($1, $2, $3, 'x', 'Token Test')",
    )
    .bind(user)
    .bind(org)
    .bind(format!("{user}@token.test"))
    .execute(pool)
    .await?;

    Ok(Fixture {
        org,
        user,
        kb_a,
        kb_b,
    })
}

#[tokio::test]
async fn a_token_is_the_person_but_not_all_of_them() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // ---- 1. Issue one; the plaintext is available only this once
        let (view, plain) = tokens::issue(&pool, f.user, "我的笔记本", "read", None, None).await?;
        assert!(
            plain.starts_with("utp_pat_"),
            "the prefix must identify which kind of token it is"
        );
        assert_eq!(
            view.scope, "read",
            "**read-only by default**: write access must be explicitly ticked"
        );
        assert!(view.revoked_at.is_none());

        let stored: String =
            sqlx::query_scalar("SELECT token_hash FROM personal_tokens WHERE id = $1")
                .bind(view.id)
                .fetch_one(&pool)
                .await?;
        assert_ne!(stored, plain, "the database must store a hash");
        assert!(
            !stored.contains(&plain[8..24]),
            "**明文的任何一段都不该出现在库里**——拿到整张表也复原不出来"
        );

        // ---- 2. Recognized on return, and last_used_at written along the way
        let auth = tokens::authenticate(&pool, &plain).await?;
        assert_eq!(
            auth.user_id, f.user,
            "a token acts as the identity of the person who issued it"
        );
        assert!(!auth.can_write(), "a read token must not be able to write");
        let used: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT last_used_at FROM personal_tokens WHERE id = $1")
                .bind(view.id)
                .fetch_one(&pool)
                .await?;
        assert!(
            used.is_some(),
            "**before revoking, the person must be able to answer 'is this key still in use?'**"
        );

        // ---- 3. Forged ones, or ones with the wrong prefix, are never recognized
        assert!(
            tokens::authenticate(&pool, "utp_pat_deadbeef")
                .await
                .is_err(),
            "编一串出来不该认"
        );
        assert!(
            tokens::authenticate(&pool, &plain.replace("utp_pat_", "utp_"))
                .await
                .is_err(),
            "摄入令牌的前缀不该走这条路"
        );

        // ---- 4. kb_ids only narrows
        let (_, scoped) =
            tokens::issue(&pool, f.user, "只给 A 库", "write", Some(&[f.kb_a]), None).await?;
        let auth = tokens::authenticate(&pool, &scoped).await?;
        assert!(
            auth.covers(f.kb_a),
            "the library that was granted access can reach it"
        );
        assert!(
            !auth.covers(f.kb_b),
            "**限定到一个库的令牌够不着另一个**——哪怕这个人两个库都能进"
        );
        assert!(auth.can_write(), "a write token can write");
        // The unrestricted one still covers everything
        assert!(tokens::authenticate(&pool, &plain).await?.covers(f.kb_b));

        // ---- 5. Revocation takes effect at once, and the row remains
        tokens::revoke(&pool, f.user, view.id).await?;
        assert!(
            tokens::authenticate(&pool, &plain).await.is_err(),
            "**撤了就立刻不认**——判断在 SQL 里，不在取回来之后"
        );
        let still_there: i64 =
            sqlx::query_scalar("SELECT count(*) FROM personal_tokens WHERE id = $1")
                .bind(view.id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            still_there, 1,
            "stamping does not delete the row: this key existed and must stay queryable"
        );
        assert!(
            tokens::revoke(&pool, f.user, view.id).await.is_err(),
            "撤两次的第二次该说没有这一行可撤"
        );

        // ---- 6. Expiry
        let (expired_view, expired) = tokens::issue(
            &pool,
            f.user,
            "早就过期的",
            "read",
            None,
            Some(Utc::now() - Duration::hours(1)),
        )
        .await?;
        assert!(
            tokens::authenticate(&pool, &expired).await.is_err(),
            "过了期就不认"
        );

        // ---- 7. The list shows both the revoked and the expired
        let all = tokens::list(&pool, f.user).await?;
        assert_eq!(
            all.len(),
            3,
            "revoked ones must be listed too; the revocation itself must be visible"
        );
        assert!(
            all.iter()
                .any(|t| t.id == view.id && t.revoked_at.is_some()),
            "列表要标出哪一把被撤了"
        );
        assert!(
            all.iter().all(|t| t.token_prefix.starts_with("utp_pat_")),
            "列表只给前缀，不给明文"
        );
        assert!(all.iter().any(|t| t.id == expired_view.id));

        // Someone else's token cannot be revoked
        let other = Uuid::now_v7();
        assert!(
            tokens::revoke(&pool, other, expired_view.id).await.is_err(),
            "**令牌是谁的谁才撤得动**"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
