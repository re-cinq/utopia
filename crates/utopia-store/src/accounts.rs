use sqlx::PgPool;
use utopia_core::models::{Role, User, Workspace};
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// What one registration creates.
pub struct Registered {
    pub user: User,
    pub workspace: Workspace,
    /// The first user also creates the General KB; later sign-ups join the existing workspace, so this is None here.
    ///
    /// **It is handed out so the caller can install the cold-start ontology package.** Installing the package reads the embedded package files
    /// and goes through OWL import, which needs `AppState` and cannot run inside this transaction (#322).
    pub general_kb: Option<Uuid>,
}

/// Registration (single-tenant model):
/// - No organization exists in the deployment yet → first user: create the organization and default workspace, becoming owner and system admin;
/// - Organization already exists → join it and enter the default (earliest) workspace as a viewer;
///   Rejected when `open_registration = false` (the bootstrap first user is the only exception).
pub async fn register(
    pool: &PgPool,
    email: &str,
    password_hash: &str,
    display_name: &str,
    org_name: Option<&str>,
    open_registration: bool,
) -> AppResult<Registered> {
    let mut tx = pool.begin().await?;
    let registered = register_in(
        &mut tx,
        email,
        password_hash,
        display_name,
        org_name,
        open_registration,
    )
    .await?;
    tx.commit().await?;
    Ok(registered)
}

async fn register_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    email: &str,
    password_hash: &str,
    display_name: &str,
    org_name: Option<&str>,
    open_registration: bool,
) -> AppResult<Registered> {
    let existing_org: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM organizations ORDER BY created_at LIMIT 1")
            .fetch_optional(&mut **tx)
            .await?;

    let result = match existing_org {
        None => {
            // First user: bootstrap the whole deployment
            let org_id = Uuid::now_v7();
            sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, $2)")
                .bind(org_id)
                .bind(org_name.unwrap_or("Default Organization"))
                .execute(&mut **tx)
                .await?;

            let user = insert_user(tx, org_id, email, password_hash, display_name, true).await?;

            let workspace: Workspace = sqlx::query_as(
                "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, $3) RETURNING *",
            )
            .bind(Uuid::now_v7())
            .bind(org_id)
            .bind("Default Workspace")
            .fetch_one(&mut **tx)
            .await?;

            insert_membership(tx, user.id, workspace.id, Role::Owner).await?;
            let kb_id = insert_general_kb(tx, workspace.id, user.id).await?;
            (user, workspace, Some(kb_id))
        }
        Some((org_id,)) => {
            if !open_registration {
                return Err(AppError::invalid(
                    "registration_closed",
                    "Registration is closed for this deployment. Contact your administrator.",
                ));
            }
            let user = insert_user(tx, org_id, email, password_hash, display_name, false).await?;

            // Join the default (earliest) workspace; if the organization has no workspace in this abnormal case, create one
            let default_ws: Option<Workspace> = sqlx::query_as(
                "SELECT * FROM workspaces WHERE org_id = $1 ORDER BY created_at LIMIT 1",
            )
            .bind(org_id)
            .fetch_optional(&mut **tx)
            .await?;

            match default_ws {
                Some(ws) => {
                    insert_membership(tx, user.id, ws.id, Role::Viewer).await?;
                    (user, ws, None)
                }
                None => {
                    let ws: Workspace = sqlx::query_as(
                        "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, $3) RETURNING *",
                    )
                    .bind(Uuid::now_v7())
                    .bind(org_id)
                    .bind("Default Workspace")
                    .fetch_one(&mut **tx)
                    .await?;
                    insert_membership(tx, user.id, ws.id, Role::Owner).await?;
                    (user, ws, None)
                }
            }
        }
    };

    let (user, workspace, general_kb) = result;
    Ok(Registered {
        user,
        workspace,
        general_kb,
    })
}

async fn insert_user(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org_id: Uuid,
    email: &str,
    password_hash: &str,
    display_name: &str,
    is_admin: bool,
) -> AppResult<User> {
    sqlx::query_as(
        "INSERT INTO users (id, org_id, email, password_hash, display_name, is_admin)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(org_id)
    .bind(email)
    .bind(password_hash)
    .bind(display_name)
    .bind(is_admin)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            AppError::Conflict("This email is already registered".into())
        }
        _ => AppError::Db(e),
    })
}

async fn insert_membership(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    workspace_id: Uuid,
    role: Role,
) -> AppResult<()> {
    sqlx::query("INSERT INTO memberships (user_id, workspace_id, role) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(workspace_id)
        .bind(role.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// The deployment's public space. Created when the first user registers, in the same transaction as the organization and workspace — otherwise a new deployment's
/// first screen is an empty shell: Graph stays stuck on Loading, the switcher has no KB to pick, and the "create the first KB" step
/// is never explained to the user.
///
/// The name is General rather than Public: visibility is expressed by the visibility field, so repeating it in the name is redundant,
/// and it would let self-hosted users misread it as "public on the internet". is_default keeps it always open and undeletable
/// (the 0012 CHECK is a second safeguard at the DB level).
async fn insert_general_kb(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    owner_id: Uuid,
) -> AppResult<Uuid> {
    let kb_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO knowledge_bases
            (id, workspace_id, name, kind, description, is_default, visibility)
         VALUES ($1, $2, 'General', 'knowledge', $3, TRUE, 'open')",
    )
    .bind(kb_id)
    .bind(workspace_id)
    .bind("Shared space for the whole deployment. Everyone can read it.")
    .execute(&mut **tx)
    .await?;
    // The creator is recorded as this KB's admin, consistent with the manual KB creation path. The first user is already a system admin and
    // can get in without this row, but system admin status can be revoked later, and this KB's permission should not evaporate along with it.
    sqlx::query(
        "INSERT INTO kb_members (kb_id, user_id, role, added_by)
         VALUES ($1, $2, 'admin', $2)",
    )
    .bind(kb_id)
    .bind(owner_id)
    .execute(&mut **tx)
    .await?;
    Ok(kb_id)
}

/// Admin creates an account on someone's behalf: joins the existing organization and default workspace; the deployment role is set by the admin.
pub async fn admin_create_user(
    pool: &PgPool,
    email: &str,
    password_hash: &str,
    display_name: &str,
    role: Role,
) -> AppResult<User> {
    let mut tx = pool.begin().await?;
    let (org_id,): (Uuid,) =
        sqlx::query_as("SELECT id FROM organizations ORDER BY created_at LIMIT 1")
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::Validation("Deployment not bootstrapped yet".into()))?;
    let user = insert_user(&mut tx, org_id, email, password_hash, display_name, false).await?;
    let ws: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM workspaces WHERE org_id = $1 ORDER BY created_at LIMIT 1")
            .bind(org_id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some((ws_id,)) = ws {
        insert_membership(&mut tx, user.id, ws_id, role).await?;
    }
    tx.commit().await?;
    Ok(user)
}

/// Find an account by email — **active accounts only**.
///
/// This one `deactivated_at IS NULL` clause handles two things: a deactivated person cannot log in, and
/// when one email may be duplicated among deactivated accounts, it never returns one of them at random (the unique index
/// is now partial and only constrains active accounts; see `users.deactivated_at`).
pub async fn find_user_by_email(pool: &PgPool, email: &str) -> AppResult<Option<User>> {
    let user = sqlx::query_as("SELECT * FROM users WHERE email = $1 AND deactivated_at IS NULL")
        .bind(email)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

/// Find an account by id — **active accounts only**.
///
/// Session validation goes through here, so deactivation takes effect **immediately**, without waiting for the token to expire: an already-issued
/// token cannot find its person on the next request. This is the one path soft delete must block; the other places that read users
/// (audit, merge log, type-change ledger) **must still find them as before** — the thing happened because that person did it, and deactivating the person
/// does not mean the event did not happen.
pub async fn find_user_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<User>> {
    let user = sqlx::query_as("SELECT * FROM users WHERE id = $1 AND deactivated_at IS NULL")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

/// Change display name (profile page).
pub async fn update_display_name(pool: &PgPool, id: Uuid, display_name: &str) -> AppResult<User> {
    sqlx::query_as("UPDATE users SET display_name = $2 WHERE id = $1 RETURNING *")
        .bind(id)
        .bind(display_name)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound)
}

/// Change password (the server layer has already verified the old password and done the hashing).
pub async fn update_password(pool: &PgPool, id: Uuid, password_hash: &str) -> AppResult<()> {
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(id)
        .bind(password_hash)
        .execute(pool)
        .await?;
    Ok(())
}

/// Deactivate an account (soft delete, see `users.deactivated_at`).
///
/// **Rows are not deleted.** Audit events, the merge log, the type-change ledger, and the scope-confirmation `actor_id` all point at this person,
/// and those are audit material — after the person leaves, it must still be possible to answer "who did it at the time". Deactivation only cuts access.
///
/// **An admin cannot deactivate themselves**: once an admin deactivates themselves, nobody can bring them back (this system has no
/// "super admin" layer). The block lives here rather than only in the UI — the UI can stop an accidental click,
/// but it cannot stop a direct API call.
///
/// **The last admin cannot be deactivated**: otherwise the organization would forever have no one who can manage members. Same reason.
pub async fn deactivate_user(pool: &PgPool, target: Uuid, actor: Uuid) -> AppResult<()> {
    if target == actor {
        return Err(AppError::Validation(
            "you cannot deactivate your own account".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    lock_account_organization(&mut tx, target).await?;
    let victim: Option<(bool, Uuid, Option<chrono::DateTime<chrono::Utc>>)> =
        sqlx::query_as("SELECT is_admin, org_id, deactivated_at FROM users WHERE id = $1")
            .bind(target)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((is_admin, org_id, already)) = victim else {
        tx.rollback().await?;
        return Err(AppError::NotFound);
    };
    if already.is_some() {
        // Idempotent: deactivating twice is not an error, and it must not change deactivated_by to a second person
        tx.rollback().await?;
        return Ok(());
    }
    if is_admin {
        let (others,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM users
              WHERE org_id = $1 AND is_admin AND deactivated_at IS NULL AND id <> $2",
        )
        .bind(org_id)
        .bind(target)
        .fetch_one(&mut *tx)
        .await?;
        if others == 0 {
            tx.rollback().await?;
            return Err(AppError::Validation(
                "this is the organization's last admin; after deactivating them nobody could manage members".into(),
            ));
        }
    }
    sqlx::query("UPDATE users SET deactivated_at = now(), deactivated_by = $2 WHERE id = $1")
        .bind(target)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Restore a deactivated account.
///
/// **May fail, and the failure is justified**: while deactivated, someone created a new account with the same email,
/// and that partial unique index blocks the restore. What should happen then is for the admin to see the conflict, not
/// for two active accounts to quietly share one email.
pub async fn reactivate_user(pool: &PgPool, target: Uuid) -> AppResult<()> {
    let res = sqlx::query(
        "UPDATE users SET deactivated_at = NULL, deactivated_by = NULL
          WHERE id = $1 AND deactivated_at IS NOT NULL",
    )
    .bind(target)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Grant or revoke system admin (`users.is_admin`).
pub async fn set_system_admin(
    pool: &PgPool,
    target: Uuid,
    actor: Uuid,
    value: bool,
) -> AppResult<()> {
    if target == actor && !value {
        return Err(AppError::Validation(
            "you cannot revoke your own system administrator role".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    let org_id = lock_account_organization(&mut tx, target).await?;
    let is_admin: bool =
        sqlx::query_scalar("SELECT is_admin FROM users WHERE id = $1 AND deactivated_at IS NULL")
            .bind(target)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
    if is_admin && !value {
        let others: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM users
             WHERE org_id = $1 AND is_admin AND deactivated_at IS NULL AND id <> $2",
        )
        .bind(org_id)
        .bind(target)
        .fetch_one(&mut *tx)
        .await?;
        if others == 0 {
            return Err(AppError::Validation(
                "this is the organization's last admin; after revoking the role nobody could manage members".into(),
            ));
        }
    }
    let res =
        sqlx::query("UPDATE users SET is_admin = $2 WHERE id = $1 AND deactivated_at IS NULL")
            .bind(target)
            .bind(value)
            .execute(&mut *tx)
            .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    tx.commit().await?;
    Ok(())
}

async fn lock_account_organization(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    target: Uuid,
) -> AppResult<Uuid> {
    // Revoking rights and deactivation share the organization lock; after taking it, re-read the account and count admins, do not trust the snapshot from request auth.
    sqlx::query_scalar(
        "SELECT id FROM organizations
         WHERE id = (SELECT org_id FROM users WHERE id = $1) FOR NO KEY UPDATE",
    )
    .bind(target)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)
}

/// When single sign-on allows automatic account creation, find the account by the email the identity provider verified; if none is found, create one by the
/// registered rule (viewer of the default workspace; the first admin if nobody is in the deployment yet), and bind this
/// subject. Account creation and binding happen in the same transaction: when binding collides (`taken` / `already_linked`),
/// no account that nobody can log into is left behind.
///
/// Case-insensitive email matching must yield exactly one active account; multiple matches return a conflict and neither create nor bind anything.
///
/// When this email has only one deactivated account left, returns `None`: a deactivated person cannot come back under a new account —
/// the email unique index only covers active accounts and does not block this step, so it must be checked here.
///
/// An account created this way has **no usable password**: `!` is not a valid hash, so password login can never succeed.
pub async fn account_for_sso(
    pool: &PgPool,
    issuer: &str,
    subject: &str,
    email: &str,
    display_name: &str,
) -> AppResult<Option<User>> {
    let mut tx = pool.begin().await?;
    // This call only rejects ambiguity already present in the snapshot; preventing concurrent account creation from producing duplicate emails needs a separate migration that also handles existing duplicates.
    let mut active: Vec<User> = sqlx::query_as(
        "SELECT * FROM users WHERE lower(email) = lower($1) AND deactivated_at IS NULL LIMIT 2",
    )
    .bind(email)
    .fetch_all(&mut *tx)
    .await?;
    if active.len() > 1 {
        return Err(AppError::CodedConflict {
            code: "unlinked",
            message: "Multiple active accounts match the SSO email".into(),
        });
    }
    let user = match active.pop() {
        Some(u) => u,
        None => {
            let deactivated: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM users WHERE lower(email) = lower($1))",
            )
            .bind(email)
            .fetch_one(&mut *tx)
            .await?;
            if deactivated {
                return Ok(None);
            }
            register_in(&mut tx, email, "!", display_name, None, true)
                .await?
                .user
        }
    };
    sqlx::query("INSERT INTO oidc_identities (issuer, subject, user_id) VALUES ($1, $2, $3)")
        .bind(issuer)
        .bind(subject)
        .bind(user.id)
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            // The two constraints each match one kind of collision: the subject is already bound to someone else, or this account already has a
            sqlx::Error::Database(db) if db.is_unique_violation() => AppError::CodedConflict {
                code: if db.constraint().is_some_and(|n| n.contains("user")) {
                    "already_linked"
                } else {
                    "taken"
                },
                message: "This SSO identity cannot be linked to that account".into(),
            },
            _ => AppError::Db(e),
        })?;
    tx.commit().await?;
    Ok(Some(user))
}
