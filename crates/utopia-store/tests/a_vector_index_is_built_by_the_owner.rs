//! Building an index needs table ownership (#1120): when the runtime identity is a restricted role, it cannot build the index itself,
//! so it must use a connection with the migration identity to build it.
//!
//! Only testable against a live database: creating a login role with read-write grants that owns no tables, in the same position as `utopia_app`
//! in deployment. Without `UTOPIA_DATABASE_URL` it skips; the role and index are created and dropped by the test itself.
use utopia_store::vector_index::{self, Target};

const ROLE: &str = "utopia_index_probe";
const PASSWORD: &str = "index-probe-only";
// An index dimension no other test uses, so index names do not collide
const DIMS: usize = 19;

/// Swap the identity in the connection string for the restricted role: `postgres://owner:pw@host/db` → `postgres://probe:pw@host/db`
fn as_restricted(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("a postgres URL");
    let host_and_path = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    format!("{scheme}://{ROLE}:{PASSWORD}@{host_and_path}")
}

#[tokio::test]
async fn a_restricted_role_cannot_build_and_the_owner_can() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let owner = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await?;
    utopia_store::db::migrate(&owner).await?;
    sqlx::raw_sql(&format!(
        "DO $$ BEGIN
           IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{ROLE}') THEN
             CREATE ROLE {ROLE} LOGIN PASSWORD '{PASSWORD}';
           END IF;
         END $$;
         GRANT USAGE ON SCHEMA public TO {ROLE};
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {ROLE};"
    ))
    .execute(&owner)
    .await?;
    vector_index::drop(&owner, Target::NameVectors, DIMS).await?;

    let app = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&as_restricted(&url))
        .await?;
    let run = async {
        // The old path when the identity is not split: the restricted role builds it itself, and the database rejects it
        let refused = vector_index::build_as_owner(&app, None, Target::NameVectors, DIMS)
            .await
            .expect_err("a role that does not own the table cannot index it");
        assert!(
            refused.to_string().contains("must be owner"),
            "the refusal is about ownership: {refused}"
        );
        assert_eq!(
            vector_index::status(&owner, Target::NameVectors, DIMS).await?,
            None,
            "nothing was built"
        );

        // Given the owner's connection string: the same task can build it, while the application pool is still the restricted role
        let built =
            vector_index::build_as_owner(&app, Some(&url), Target::NameVectors, DIMS).await?;
        assert!(built.created);
        assert_eq!(
            vector_index::status(&app, Target::NameVectors, DIMS).await?,
            Some(true),
            "the restricted role sees a valid index it can use"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    app.close().await;
    vector_index::drop(&owner, Target::NameVectors, DIMS).await?;
    sqlx::raw_sql(&format!("DROP OWNED BY {ROLE}; DROP ROLE {ROLE};"))
        .execute(&owner)
        .await?;
    owner.close().await;
    run
}
