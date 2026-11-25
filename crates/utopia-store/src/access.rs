//! Per-KB access decision: the single authentication entry point for all KB-scoped routes.
//! Decision chain: system admin → full access; a kb_members matrix record → its matrix role;
//! open KB → deployment role (the membership on the hidden workspace);
//! restricted KB with no record → NotFound (does not reveal that the KB exists).

use sqlx::PgPool;
use utopia_core::models::{KbMemberView, KnowledgeBase, MyKbInfo, Role, User};
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// A user's effective role in a KB (None = not visible).
pub async fn kb_role(pool: &PgPool, user: &User, kb: &KnowledgeBase) -> AppResult<Option<Role>> {
    if user.is_admin {
        return Ok(Some(Role::Owner));
    }
    let matrix: Option<(String,)> =
        sqlx::query_as("SELECT role FROM kb_members WHERE kb_id = $1 AND user_id = $2")
            .bind(kb.id)
            .bind(user.id)
            .fetch_optional(pool)
            .await?;
    if let Some((r,)) = matrix {
        return Ok(Role::parse(&r));
    }
    if kb.visibility == "open" {
        // An open KB = readable by everyone in the deployment; write permission always comes from this KB's matrix
        //(system admins are already Owner at the head of the chain; deployment roles no longer map to write permission in the KB)
        let ws: Option<(String,)> =
            sqlx::query_as("SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2")
                .bind(kb.workspace_id)
                .bind(user.id)
                .fetch_optional(pool)
                .await?;
        return Ok(ws.map(|_| Role::Viewer));
    }
    Ok(None)
}

/// The set version of `kb_role`: every KB this person can see **across all workspaces** → effective role.
///
/// **If you change the function above, you must change this one too.** It is not built by calling `kb_role` in a loop:
/// the alert list spans KBs, so a query per entry would be N+1, and the number of alerts grows with deployment size.
/// The three branches map one-to-one to `kb_role`, in the same order:
///   1. is_admin → all KBs, Owner
///   2. kb_members has a record → matrix role
///   3. open KB + this workspace's membership → Viewer
///
/// Branch 3 explicitly queries `memberships`, whereas `kbs::list_visible` does not —
/// that function is already scoped by workspace routing, this one is not.
pub async fn visible_kb_roles(pool: &PgPool, user: &User) -> AppResult<Vec<(Uuid, Role)>> {
    if user.is_admin {
        let ids: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM knowledge_bases")
            .fetch_all(pool)
            .await?;
        return Ok(ids.into_iter().map(|(id,)| (id, Role::Owner)).collect());
    }
    let rows: Vec<(Uuid, Option<String>, bool)> = sqlx::query_as(
        "SELECT k.id,
                m.role,
                (k.visibility = 'open'
                 AND EXISTS (SELECT 1 FROM memberships ws
                             WHERE ws.workspace_id = k.workspace_id AND ws.user_id = $1))
         FROM knowledge_bases k
         LEFT JOIN kb_members m ON m.kb_id = k.id AND m.user_id = $1",
    )
    .bind(user.id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, matrix, open_member)| {
            // The matrix takes precedence over open — as in kb_role, once the matrix has a record visibility is no longer checked
            match matrix {
                Some(r) => Role::parse(&r).map(|r| (id, r)),
                None if open_member => Some((id, Role::Viewer)),
                None => None,
            }
        })
        .collect())
}

/// Requires at least `min` role on the KB, and returns the KB.
pub async fn require_kb(
    pool: &PgPool,
    user: &User,
    kb_id: Uuid,
    min: Role,
) -> AppResult<KnowledgeBase> {
    let kb = crate::kbs::get(pool, kb_id).await?;
    match kb_role(pool, user, &kb).await? {
        Some(r) if r >= min => Ok(kb),
        Some(_) => Err(AppError::Forbidden),
        None => Err(AppError::NotFound),
    }
}

// ---------------------------------------------------------------------------
// KB member matrix
// ---------------------------------------------------------------------------

pub async fn kb_members(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<KbMemberView>> {
    let rows: Vec<KbMemberView> = sqlx::query_as(
        "SELECT m.user_id, u.email, u.display_name, m.role
         FROM kb_members m JOIN users u ON u.id = m.user_id
         -- 停用的人不出现在成员列表里（见 `users.deactivated_at`）；成员关系那一行留着
         WHERE m.kb_id = $1 AND u.deactivated_at IS NULL ORDER BY u.display_name",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn set_kb_member(
    pool: &PgPool,
    kb_id: Uuid,
    user_id: Uuid,
    role: &str,
    added_by: Option<Uuid>,
) -> AppResult<()> {
    if !matches!(role, "viewer" | "editor" | "admin") {
        return Err(AppError::Validation(
            "role must be viewer, editor or admin".into(),
        ));
    }
    // Changing a role does not overwrite the original inviter (the join record stores "who brought them in")
    sqlx::query(
        "INSERT INTO kb_members (kb_id, user_id, role, added_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT (kb_id, user_id)
         DO UPDATE SET role = $3, added_by = COALESCE(kb_members.added_by, $4)",
    )
    .bind(kb_id)
    .bind(user_id)
    .bind(role)
    .bind(added_by)
    .execute(pool)
    .await?;
    Ok(())
}

/// My member info in each visible KB + overview stats (the account-level Knowledge bases page).
pub async fn my_kb_infos(
    pool: &PgPool,
    kb_ids: &[Uuid],
    user_id: Uuid,
) -> AppResult<Vec<MyKbInfo>> {
    let rows: Vec<MyKbInfo> = sqlx::query_as(
        "SELECT k.id AS kb_id,
                m.role AS member_role,
                m.created_at AS joined_at,
                inviter.display_name AS added_by_name,
                (SELECT count(*) FROM documents d
                  WHERE d.kb_id = k.id AND d.deleted_at IS NULL) AS doc_count,
                (SELECT count(*) FROM kb_members mm WHERE mm.kb_id = k.id) AS member_count
         FROM knowledge_bases k
         LEFT JOIN kb_members m ON m.kb_id = k.id AND m.user_id = $2
         LEFT JOIN users inviter ON inviter.id = m.added_by
         WHERE k.id = ANY($1)",
    )
    .bind(kb_ids)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn remove_kb_member(pool: &PgPool, kb_id: Uuid, user_id: Uuid) -> AppResult<()> {
    let res = sqlx::query("DELETE FROM kb_members WHERE kb_id = $1 AND user_id = $2")
        .bind(kb_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deployment config
// ---------------------------------------------------------------------------

pub async fn open_registration(pool: &PgPool) -> AppResult<bool> {
    let row: Option<(bool,)> =
        sqlx::query_as("SELECT open_registration FROM deployment_settings LIMIT 1")
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(v,)| v).unwrap_or(true))
}

pub async fn set_open_registration(pool: &PgPool, value: bool) -> AppResult<()> {
    sqlx::query("UPDATE deployment_settings SET open_registration = $1")
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

/// Default value of `ontology_lang` when creating a knowledge base.
///
/// **Deliberately not called "system language"**: with that name, someone would sooner or later try to attach the UI language,
/// and find it cannot be attached (the UI language lives on the client). The name itself should block that misuse. See docs/decisions/0004.
pub async fn default_ontology_lang(pool: &PgPool) -> AppResult<String> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT default_ontology_lang FROM deployment_settings LIMIT 1")
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(v,)| v).unwrap_or_else(|| "en".into()))
}

pub async fn set_default_ontology_lang(pool: &PgPool, value: &str) -> AppResult<()> {
    if !matches!(value, "en" | "zh") {
        return Err(AppError::invalid("bad_lang", "language must be en or zh"));
    }
    sqlx::query("UPDATE deployment_settings SET default_ontology_lang = $1")
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

/// Task worker concurrency (changeable in system settings, takes effect immediately — see jobs::run_worker).
pub async fn worker_concurrency(pool: &PgPool) -> AppResult<i32> {
    let row: Option<(i32,)> =
        sqlx::query_as("SELECT worker_concurrency FROM deployment_settings LIMIT 1")
            .fetch_optional(pool)
            .await?;
    // Keep in step with the column default of `deployment_settings.worker_concurrency` (migration 0011).
    // It is written in two places because one is in SQL and one is in Rust; changing one does not carry the other —
    // the test `the_backstop_can_be_raised` guards exactly this
    Ok(row.map(|(v,)| v).unwrap_or(64))
}

pub async fn set_worker_concurrency(pool: &PgPool, value: i32) -> AppResult<()> {
    if !(1..=256).contains(&value) {
        return Err(AppError::invalid(
            "concurrency_range",
            "worker_concurrency must be between 1 and 256",
        ));
    }
    sqlx::query("UPDATE deployment_settings SET worker_concurrency = $1")
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

/// Auto-generated JWT key persisted to the DB; returns the one that ends up in effect.
///
/// `COALESCE` converges several instances starting at once on a single value: whoever writes first wins, and latecomers get back
/// the row already in the DB rather than the one they just generated. A "read first, write if empty" two-step cannot do this —
/// two instances would both read NULL and each write its own; the first writer would then use a key no longer in the DB,
/// and every token it issued would be invalid on the other instance.
pub async fn ensure_jwt_secret(pool: &PgPool, generated: &str) -> AppResult<String> {
    let (secret,): (Option<String>,) = sqlx::query_as(
        "UPDATE deployment_settings SET jwt_secret = COALESCE(jwt_secret, $1)
         RETURNING jwt_secret",
    )
    .bind(generated)
    .fetch_one(pool)
    .await?;
    secret.ok_or_else(|| {
        AppError::Other(anyhow::anyhow!(
            "deployment_settings has no singleton row, so there is nowhere to store the JWT key"
        ))
    })
}
