//! Alert center (0005). **Review decides whether knowledge is right or wrong, alerts decide whether the system is alive or dead.**
//!
//! **One failure per row, never edited once written.** This table deliberately has no state machine: no "resolved",
//! no self-healing, and no folding several failures into one row.
//!
//! We used to have one, but each new alert kind had to work out its own definition of "fixed" --
//! `source.sync_failed` has a natural success signal (the sync succeeded), `llm.unreachable` does not,
//! so it needs its own background probe; a third kind needs a third such setup, and a missing clear is invisible at compile time,
//! and the symptom is an alert that stays lit forever. The deeper point is **that this is not the question the alert center should answer**: whether something is still broken right now
//! is written on the source page and in the document status. An alert's job is to get a person to take a look, not to act as a live dashboard.
//!
//! No dedup either. "The same source failing every hour for 24 hours" is 24 rows, not 1 --
//! collapsing it into 1 would mean deciding whether this is a recurrence or a failure that never recovered, and the data **cannot tell those apart**,
//! unless a clock or a success signal is introduced. A missed alert costs far more than extra rows, so we would rather write more rows
//! and rely on [`purge_older_than`] to clean up.

use sqlx::PgPool;
use utopia_core::models::{Role, User};
use utopia_core::AppResult;
use uuid::Uuid;

/// Alert kind. **The strings are hardcoded here rather than scattered across call sites**: the UI looks up wording by them,
/// and a single typo falls back to showing the raw code, a mistake the compiler cannot catch.
pub mod kind {
    /// Library level: a source failed to sync. `min_role = editor`
    pub const SOURCE_SYNC_FAILED: &str = "source.sync_failed";
    /// System level: the model endpoint gave no usable answer -- it could not be reached, or it was reached but did not return this API.
    /// A clean 4xx from the endpoint does not count: that means it is the model API, just with a wrong key or quota.
    /// The quota case is covered by [`LLM_RATE_LIMITED`]
    pub const LLM_UNREACHABLE: &str = "llm.unreachable";
    /// System level: the endpoint is rate-limiting, and it still fails after backoff retries are exhausted. `min_role = admin`
    ///
    /// **Kept separate from [`LLM_UNREACHABLE`] because the fix differs**: an unreachable endpoint means checking the network or address,
    /// while a saturated quota means lowering concurrency or upgrading the plan. The person and the action are different.
    ///
    /// severity is `warning`, not `error`: quota resets on its own, an endpoint that is down does not.
    ///
    /// This fills the other half that "backoff retry" leaves behind. After retries no data is lost, but when a document
    /// is actually kept out by the quota, without this alert nobody would know --
    /// in one measured test run, 4 documents failed: half were unreachable (alerted), half were rate-limited (silent).
    pub const LLM_RATE_LIMITED: &str = "llm.rate_limited";
    /// System level: the account cannot pay for requests -- overdue, or the plan quota is used up. `min_role = admin`
    ///
    /// **`error`, not `warning`, precisely because the difference from rate-limiting is "will it fix itself"**:
    /// quota resets at a set time, overdue billing does not. Until someone tops up, extraction and vectorization for this deployment
    /// stay stopped.
    pub const LLM_OUT_OF_CREDIT: &str = "llm.out_of_credit";
    /// Library level: the data source is attached, but its table structure was never ingested. `min_role = admin`
    ///
    /// **This one describes not the failure itself but the state it left behind**: the source is attached, but `query_data` cannot see which
    /// tables it has -- `query_data` still gets queued, yet the model can only guess column names. The error from the moment of attaching
    /// is visible only to the person who clicked the button, and after that this library silently stays incomplete.
    pub const SCHEMA_SYNC_FAILED: &str = "data_source.schema_sync_failed";
    /// Library level: mapping exploration finished and proposed no mappings at all. `severity = info`, `min_role = editor` --
    /// not a failure, but "the thing you were waiting for has no result", and no other place on the page says this (#223)
    pub const MAPPING_EXPLORATION_EMPTY: &str = "mapping.exploration_empty";
    /// Library level: the governance fuse blew -- within seven days a person reverted the agent's automatic merges twice, so the switch turned itself off
    /// (0025 decision 9). `severity = warning`, `min_role = editor`: the person who prunes duplicates is
    /// the one who should look at the Agent queue and decide whether to turn it back on
    pub const GOVERNANCE_TRIPPED: &str = "governance.tripped";
    /// Library level: a file's text has to be read by a model (scans need layout recognition, audio needs transcription), and that kind of model
    /// is not configured (0040). `severity = warning`, `min_role = editor`: the file is kept and the document stays at failed,
    /// and it reprocesses itself once the model is configured under "Models" on the admin page. Without this entry, an uploaded scan just quietly
    /// ends up with nothing -- the `document.no_text_layer` that has sat unused since 0005 is exactly this
    pub const DOCUMENT_NEEDS_READER: &str = "document.needs_reader";
    /// Library level: the parser's safety limit was hit, so the body is only part of the file. `severity = warning`,
    /// `min_role = editor`: the file is already searchable, but search and answers do not include the part that was cut off.
    pub const DOCUMENT_CONTENTS_TRUNCATED: &str = "document.contents_truncated";
}

/// One failure. It is packed into a struct not only to limit the parameter count -- a call site that writes `severity: "error"`
/// reads much better than "the third positional argument is error", and alert sources will only keep multiplying.
pub struct NewAlert<'a> {
    /// None = system level
    pub kb_id: Option<Uuid>,
    pub severity: &'a str,
    /// Use the constants from [`kind`], do not write literals
    pub kind: &'a str,
    pub min_role: Role,
    /// document / source / system
    pub subject_type: Option<&'a str>,
    pub subject_id: Option<Uuid>,
    /// The copy for people: name and raw error text. **The name must be stored here** --
    /// once the object is deleted, `subject_id` can no longer be resolved to a name, and the alert should survive that.
    pub detail: serde_json::Value,
}

/// Record one failure. It is just an INSERT, no conflict handling, no read-back.
pub async fn raise(pool: &PgPool, a: NewAlert<'_>) -> AppResult<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO alerts
             (id, kb_id, severity, kind, min_role, subject_type, subject_id, detail)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(a.kb_id)
    .bind(a.severity)
    .bind(a.kind)
    .bind(a.min_role.as_str())
    .bind(a.subject_type)
    .bind(a.subject_id)
    .bind(a.detail)
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

/// Retention cleanup. **The cost of being purely atomic shows up right here**: a broken source syncs hourly,
/// writing 24 rows a day; without cleanup this table would grow into a second log file.
///
/// Also delete the read records (foreign key CASCADE).
pub async fn purge_older_than(pool: &PgPool, days: i32) -> AppResult<u64> {
    let n = sqlx::query("DELETE FROM alerts WHERE created_at < now() - make_interval(days => $1)")
        .bind(days)
        .execute(pool)
        .await?;
    Ok(n.rows_affected())
}

/// The visibility predicate, **written only once**. The list, the unread count and mark-all-read each check it separately,
/// but "who can see what" is one rule; copying it three times would drift sooner or later.
///
/// `$1` = user_id, `$2` = is_admin, `$3` = visible kb array, `$4` = rank of the corresponding role.
const VISIBLE: &str = "
    CASE
        WHEN a.kb_id IS NULL THEN $2::bool
        ELSE EXISTS (
            SELECT 1 FROM unnest($3::uuid[], $4::int[]) AS v(kb, rank)
            WHERE v.kb = a.kb_id
              AND v.rank >= CASE a.min_role
                    WHEN 'viewer' THEN 0
                    WHEN 'editor' THEN 1
                    WHEN 'admin'  THEN 2
                    ELSE 3 END)
    END";

/// Search matches **library name, object details, and kind code**, not the headline text shown in the UI.
///
/// The headline wording lives on the client (0004: the server does not produce display copy), so the server cannot search it.
/// This is not a shortcut: what people search for is the source name, library name, and raw error text -- language-neutral,
/// and right there in the detail. To find things by category, use a filter, not the search box.
const SEARCH: &str = "
    ($5::text IS NULL
     OR a.kind ILIKE '%' || $5 || '%'
     OR COALESCE(k.name, '') ILIKE '%' || $5 || '%'
     OR a.detail::text ILIKE '%' || $5 || '%')";

/// A group: **consecutive** failures of the same `(kb, kind)`.
///
/// Storage is atomic (one failure per row), and folding only affects reads. Grouping is done on the server rather than the frontend
/// because **paging has to be by group**: if folded on the frontend, a page could only take a fixed number of rows, and a run of consecutive failures
/// crossing a page boundary would split into two groups, and clicking would only mark up to the boundary.
#[derive(sqlx::FromRow)]
pub struct AlertGroup {
    pub kb_id: Option<Uuid>,
    pub kb_name: Option<String>,
    pub kind: String,
    /// The most severe level in the group
    pub severity: String,
    /// How many failures are in this group
    pub count: i64,
    /// Of those, how many I have not read
    pub unread: i64,
    /// The newest and oldest timestamps in the group. **Mark-as-read is scoped by this range** rather than sending the id list to the frontend --
    /// a group can have several hundred entries.
    pub latest_at: chrono::DateTime<chrono::Utc>,
    pub earliest_at: chrono::DateTime<chrono::Utc>,
    /// Details, at most [`GROUP_LINES`] entries, newest first
    pub lines: Vec<serde_json::Value>,
}

/// At most this many details come back per group. The panel cannot show more, and a group can have several hundred entries --
/// sending them all only slows down the first screen
const GROUP_LINES: i64 = 5;

/// One page of groups, plus the total group count.
pub struct GroupPage {
    pub items: Vec<AlertGroup>,
    pub total: i64,
}

/// Adjacent same-kind folding: subtract two `row_number()` values (gaps and islands).
///
/// The global sequence number minus the sequence number within the same (kb, kind) gives consecutive same-kind rows the same difference;
/// a different failure in between changes the difference -- so the difference is the group id.
/// `PARTITION BY kb_id` treats NULL as equal, so system-level alerts naturally fall into one group.
const ISLANDS: &str = "
    SELECT v.*,
           row_number() OVER (ORDER BY v.created_at DESC, v.id DESC)
         - row_number() OVER (PARTITION BY v.kb_id, v.kind
                              ORDER BY v.created_at DESC, v.id DESC) AS grp
    FROM v";

/// The alerts this person can see, **paged by group**, newest first.
pub async fn list_groups(
    pool: &PgPool,
    user: &User,
    q: Option<&str>,
    limit: i64,
    offset: i64,
) -> AppResult<GroupPage> {
    let (kb_ids, kb_roles) = visible(pool, user).await?;
    // An empty string counts as no search: when the frontend clears the search box it should not turn into "search for an empty string"
    let q = q.map(str::trim).filter(|s| !s.is_empty());
    let base = format!(
        "WITH v AS (
             SELECT a.id, a.kb_id, k.name AS kb_name, a.severity, a.kind,
                    a.detail, a.created_at, (r.user_id IS NOT NULL) AS read
             FROM alerts a
             LEFT JOIN knowledge_bases k ON k.id = a.kb_id
             LEFT JOIN alert_reads r ON r.alert_id = a.id AND r.user_id = $1
             WHERE ({VISIBLE}) AND ({SEARCH})
         ),
         isl AS ({ISLANDS})"
    );
    let sql = format!(
        "{base}
         SELECT kb_id, max(kb_name) AS kb_name, kind,
                -- severity 按轻重取最大，不能按字典序：那样 warning 会压过 error
                CASE max(CASE severity WHEN 'error' THEN 3 WHEN 'warning' THEN 2 ELSE 1 END)
                    WHEN 3 THEN 'error' WHEN 2 THEN 'warning' ELSE 'info' END AS severity,
                count(*) AS count,
                count(*) FILTER (WHERE NOT read) AS unread,
                max(created_at) AS latest_at,
                min(created_at) AS earliest_at,
                (array_agg(detail ORDER BY created_at DESC))[1:{GROUP_LINES}] AS lines
         FROM isl
         GROUP BY kb_id, kind, grp
         ORDER BY max(created_at) DESC, kb_id, kind, grp
         LIMIT $6 OFFSET $7"
    );
    let items: Vec<AlertGroup> = sqlx::query_as(&sql)
        .bind(user.id)
        .bind(user.is_admin)
        .bind(&kb_ids)
        .bind(&kb_roles)
        .bind(q)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    // Total number of **groups**, not total rows -- the pager counts groups
    let count_sql =
        format!("{base} SELECT count(*) FROM (SELECT 1 FROM isl GROUP BY kb_id, kind, grp) g");
    let (total,): (i64,) = sqlx::query_as(&count_sql)
        .bind(user.id)
        .bind(user.is_admin)
        .bind(&kb_ids)
        .bind(&kb_roles)
        .bind(q)
        .fetch_one(pool)
        .await?;
    Ok(GroupPage { items, total })
}

/// Mark a whole group as read. **Scoped by time range**, not by id list -- a group can have several hundred entries,
/// and sending all the ids to the frontend and back is wasted work.
///
/// Visibility is still checked: a person must not be able to mark invisible alerts read by guessing the kind.
pub async fn mark_group_read(
    pool: &PgPool,
    user: &User,
    kb_id: Option<Uuid>,
    kind: &str,
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
) -> AppResult<u64> {
    let (kb_ids, kb_roles) = visible(pool, user).await?;
    let sql = format!(
        "INSERT INTO alert_reads (alert_id, user_id)
         SELECT a.id, $1 FROM alerts a
         WHERE ({VISIBLE})
           AND a.kind = $5
           -- IS NOT DISTINCT FROM：系统级告警的 kb_id 是 NULL，= 比不出来
           AND a.kb_id IS NOT DISTINCT FROM $6
           AND a.created_at BETWEEN $7 AND $8
         ON CONFLICT DO NOTHING"
    );
    let n = sqlx::query(&sql)
        .bind(user.id)
        .bind(user.is_admin)
        .bind(&kb_ids)
        .bind(&kb_roles)
        .bind(kind)
        .bind(kb_id)
        .bind(from)
        .bind(to)
        .execute(pool)
        .await?;
    Ok(n.rows_affected())
}

/// My unread count.
pub async fn unread_count(pool: &PgPool, user: &User) -> AppResult<i64> {
    let (kb_ids, kb_roles) = visible(pool, user).await?;
    let sql = format!(
        "SELECT count(*) FROM alerts a
         LEFT JOIN alert_reads r ON r.alert_id = a.id AND r.user_id = $1
         WHERE ({VISIBLE}) AND r.user_id IS NULL"
    );
    let (n,): (i64,) = sqlx::query_as(&sql)
        .bind(user.id)
        .bind(user.is_admin)
        .bind(&kb_ids)
        .bind(&kb_roles)
        .fetch_one(pool)
        .await?;
    Ok(n)
}

/// Mark everything I can see as read.
///
/// One SQL statement rather than an insert per row: per-row would first fetch the list back, and "what is visible"
/// is already written in [`VISIBLE`], so fetching and then looping would apply the same rule twice.
pub async fn mark_all_read(pool: &PgPool, user: &User) -> AppResult<u64> {
    let (kb_ids, kb_roles) = visible(pool, user).await?;
    let sql = format!(
        "INSERT INTO alert_reads (alert_id, user_id)
         SELECT a.id, $1 FROM alerts a
         WHERE ({VISIBLE})
         ON CONFLICT DO NOTHING"
    );
    let n = sqlx::query(&sql)
        .bind(user.id)
        .bind(user.is_admin)
        .bind(&kb_ids)
        .bind(&kb_roles)
        .execute(pool)
        .await?;
    Ok(n.rows_affected())
}

/// The visible KBs are split into two parallel arrays: Postgres has no convenient way to write an array of tuples,
/// and `unnest(a, b)` expanding two columns side by side is the standard approach.
async fn visible(pool: &PgPool, user: &User) -> AppResult<(Vec<Uuid>, Vec<i32>)> {
    let roles = crate::access::visible_kb_roles(pool, user).await?;
    Ok(roles.into_iter().map(|(id, r)| (id, rank(r))).unzip())
}

/// The ordering of roles, used to compare against `alerts.min_role`.
/// Same order as the `PartialOrd` of [`Role`], and the same order as the CASE in [`VISIBLE`] --
/// all three must agree
fn rank(r: Role) -> i32 {
    match r {
        Role::Viewer => 0,
        Role::Editor => 1,
        Role::Admin => 2,
        Role::Owner => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use utopia_core::models::Role;

    /// The "three places agree" rule for role order: this module's `rank()`, the derived `PartialOrd` (from the `Role`
    /// declaration order), and the CASE clause in [`VISIBLE`] written in one string.
    ///
    /// This group of invariants is locked down so nobody has to run git blame: if any one place drifts, at least one of the three assertions
    /// will blow up. The cost is that the test itself is verbose -- but this is the lifeline for `alerts.min_role` comparisons,
    /// and being off by one level means "a viewer sees alerts only editors should see", or the reverse.
    #[test]
    fn role_rank_order_is_the_same_in_rust_and_sql() {
        // 1. Rust `rank()` has the same order as the derived PartialOrd
        assert!(rank(Role::Viewer) < rank(Role::Editor));
        assert!(rank(Role::Editor) < rank(Role::Admin));
        assert!(rank(Role::Admin) < rank(Role::Owner));
        assert!(Role::Viewer < Role::Editor);
        assert!(Role::Editor < Role::Admin);
        assert!(Role::Admin < Role::Owner);

        // 2. In SQL, the WHEN clauses of CASE are ordered low to high, with values equal to `rank()`
        //    -- go through `as_str()` as the single source of truth, rather than writing lowercase literals again in the test
        //    -- strip extra whitespace before comparing: the actual SQL has "'admin'  THEN" (two spaces for alignment),
        //       a plain contains on a whole line will not match, so the test would be pointless
        let visible_compact: String = VISIBLE.split_whitespace().collect::<Vec<_>>().join(" ");
        let expected_lines = [(Role::Viewer, 0), (Role::Editor, 1), (Role::Admin, 2)];
        for (role, want_rank) in expected_lines {
            let needle = format!("WHEN '{}' THEN {}", role.as_str(), want_rank);
            assert!(
                visible_compact.contains(&needle),
                "{needle:?} 不在 VISIBLE 里。Role 的序数改了或 CASE 没跟上时这条会爆——\n{VISIBLE}"
            );
        }
        // Owner does not appear explicitly: it takes `ELSE 3` in the CASE, and the value must equal rank(Owner)
        // -- otherwise Owner would see alerts Admin cannot see, or the reverse
        assert!(
            VISIBLE.contains("ELSE 3 END"),
            "Owner 在 VISIBLE 里走 ELSE 兜底，期望 'ELSE 3 END'。drift 后这条会爆"
        );
        assert_eq!(
            rank(Role::Owner),
            3,
            "rank(Owner) != 3：要么 rank() 改了，要么 ELSE 那条数没跟上"
        );

        // 3. The declaration order of `Role` is the order of `PartialOrd` -- anyone who manually adjusts `Role`
        //    will see the two checks above fail first; this one is the final fallback
        assert!(
            (Role::Viewer as usize) < (Role::Editor as usize)
                && (Role::Editor as usize) < (Role::Admin as usize)
                && (Role::Admin as usize) < (Role::Owner as usize),
            "Role 的声明顺序与 PartialOrd 不再一致——上游 impl 改了？"
        );
    }
}
