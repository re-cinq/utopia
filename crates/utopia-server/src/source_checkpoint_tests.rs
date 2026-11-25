//! The incremental cursor represents the window that has been successfully covered; a failed attempt must not skip entries that have not yet been read.
use super::sync_source;
use std::sync::Arc;
use uuid::Uuid;
use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};

async fn retry_reads_the_uncovered_window(
    prior_success: bool,
    fail_before_retry: bool,
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    let (org, ws, kb, source) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations(id,name) VALUES($1,'checkpoint-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces(id,org_id,name) VALUES($1,$2,'checkpoint-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases(id,workspace_id,name) VALUES($1,$2,'checkpoint-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    let server = MockServer::start().await;
    sqlx::query(
        "INSERT INTO sources(id,kb_id,kind,name,config) VALUES($1,$2,'custom','fixture',$3)",
    )
    .bind(source)
    .bind(kb)
    .bind(serde_json::json!({"endpoint":server.uri()}))
    .execute(&pool)
    .await?;
    let started = chrono::Utc::now() - chrono::Duration::hours(2);
    let changed = started + chrono::Duration::minutes(30);
    if prior_success {
        let run = utopia_store::sources::start_run(&pool, source).await?;
        utopia_store::sources::finish_run(&pool, run, source, None, 0, 0).await?;
        sqlx::query("UPDATE source_sync_runs SET started_at=$2,finished_at=$3 WHERE id=$1")
            .bind(run)
            .bind(started)
            .bind(started + chrono::Duration::hours(1))
            .execute(&pool)
            .await?;
        // **The source's own cursor lands at that moment** (ADR 0072).
        //
        // Previously the lower bound was derived from the run -- the `started_at` of the last `ok`. That had two problems:
        // it cannot prove that window was fully read (the round that hit the ceiling also recorded `ok`), and it cannot be cleared, so old sources cannot start over from scratch in one go
        // (0072, the Recovery section).
        sqlx::query("UPDATE sources SET sync_checkpoint=$2 WHERE id=$1")
            .bind(source)
            .bind(started)
            .execute(&pool)
            .await?;
        utopia_store::sources::touch_sync_time(&pool, source, started + chrono::Duration::hours(1))
            .await?;
    }
    let dir = std::env::temp_dir().join(format!("utopia-checkpoint-{source}"));
    let cfg = utopia_core::config::AppConfig {
        data_dir: dir.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let search = Arc::new(utopia_search::SearchIndex::open(&dir.join("search"))?);
    let state = crate::state::AppState::new(pool.clone(), &cfg, search, "test-only".into());
    let result = async {
        if fail_before_retry {
            Mock::given(method("GET")).respond_with(ResponseTemplate::new(503)).mount(&server).await;
            assert!(sync_source(&state, source).await.is_err());
            let failed = utopia_store::sources::get(&pool, source).await?;
            assert_eq!(failed.last_sync_status, "failed");
            assert!(failed.last_sync_at.is_some(), "attempt time still drives scheduling and diagnostics");
            server.reset().await;
        }
        // **Record the lower bound that was actually sent**: asserting only that the missed update came back is not enough --
        // if the lower bound was never sent at all (every time a full pull), this assertion would still pass, and the incremental fetch would quietly be gone
        let asked: std::sync::Arc<std::sync::Mutex<Vec<Option<chrono::DateTime<chrono::Utc>>>>> =
            Default::default();
        let seen = asked.clone();
        Mock::given(method("GET")).respond_with(move |request: &Request| {
            let since = request.url.query_pairs().find(|(k,_)| k == "since")
                .map(|(_,v)| chrono::DateTime::parse_from_rfc3339(&v).unwrap().with_timezone(&chrono::Utc));
            seen.lock().unwrap().push(since);
            let items = if since.is_none_or(|s| s <= changed) {
                serde_json::json!([{"id":"missed", "title":"Missed update", "content":"An update from the uncovered window"}])
            } else { serde_json::json!([]) };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"items":items}))
        }).mount(&server).await;
        sync_source(&state, source).await?;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM documents WHERE source_id=$1")
            .bind(source).fetch_one(&pool).await?;
        anyhow::ensure!(count == 1, "incremental cursor skipped an unimported update: {count} documents");
        let ok = utopia_store::sources::get(&pool, source).await?;
        assert_eq!(ok.last_sync_status, "ok");
        // The lower bound is taken from the start of the last **successful** round, not the end of the last attempt, and not "no lower bound"
        let asked = asked.lock().unwrap().clone();
        let last = asked.last().copied().flatten();
        if prior_success {
            let last = last.expect("没有带下界：增量拉取整个没了");
            assert!(
                (last - started).num_seconds().abs() <= 1,
                "下界应当是源自己的游标（上一轮成功那轮的开始）{started}，实得 {last}"
            );
            assert!(last < changed, "下界晚于那次改动，漏掉的窗口又被跳过了");
        } else {
            assert!(last.is_none(), "没有成功过的源不该带下界，实得 {last:?}");
        }
        Ok::<_, anyhow::Error>(())
    }.await;
    sqlx::query("DELETE FROM knowledge_bases WHERE id=$1")
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(org)
        .execute(&pool)
        .await?;
    drop(state);
    let _ = std::fs::remove_dir_all(dir);
    result
}

#[tokio::test]
async fn a_failed_first_sync_does_not_skip_the_existing_items() -> anyhow::Result<()> {
    retry_reads_the_uncovered_window(false, true).await
}

#[tokio::test]
async fn retry_keeps_updates_from_the_previous_successful_run_window() -> anyhow::Result<()> {
    retry_reads_the_uncovered_window(true, true).await
}

#[tokio::test]
async fn updates_during_a_successful_sync_remain_in_the_next_window() -> anyhow::Result<()> {
    retry_reads_the_uncovered_window(true, false).await
}

/// #1212 / ADR 0072 (Custom section): all 201 entries changed; the first round fits 200 and stops,
/// the second round lets the unchanged prefix pass and reaches entry 201.
///
/// Both key properties are here:
///
/// 1. The round that hits the ceiling is `incomplete` -> the run writes `failed` -> the lower bound does not advance -> round two gets the same
///    since, so the first 200 come back as they were, but they are now all `Unchanged` and take no budget, and entry 201 is now reachable.
///    This is what the ADR lists under Rejected: "taking the same first 200 again".
/// 2. `stats.discovered` records the count in the response (201), not the count ingested (200) --
///    otherwise the log and that warning would read `(0 > 200)`.
#[tokio::test]
async fn a_truncated_custom_batch_resumes_on_the_next_run() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    let (org, ws, kb, source) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations(id,name) VALUES($1,'truncation-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces(id,org_id,name) VALUES($1,$2,'truncation-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases(id,workspace_id,name) VALUES($1,$2,'truncation-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    // **The source is inserted first, `start_run` runs after**: source_sync_runs.source_id has a foreign key;
    // inserting the other way round fails with `source_sync_runs_source_id_fkey`.
    let server = MockServer::start().await;
    sqlx::query(
        "INSERT INTO sources(id,kb_id,kind,name,config) VALUES($1,$2,'custom','fixture',$3)",
    )
    .bind(source)
    .bind(kb)
    .bind(serde_json::json!({ "endpoint": server.uri() }))
    .execute(&pool)
    .await?;

    // 201 entries, all new: each round receives them again
    let all: Vec<serde_json::Value> = (0..201u32)
        .map(|i| {
            serde_json::json!({
                "id": format!("item-{i}"),
                "title": format!("Item {i}"),
                "content": format!("Body {i}"),
            })
        })
        .collect();
    let served = all.clone();
    Mock::given(method("GET"))
        .respond_with(move |_req: &Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "items": served }))
        })
        .mount(&server)
        .await;

    let dir = std::env::temp_dir().join(format!("utopia-truncation-{source}"));
    let cfg = utopia_core::config::AppConfig {
        data_dir: dir.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let search = Arc::new(utopia_search::SearchIndex::open(&dir.join("search"))?);
    let state = crate::state::AppState::new(pool.clone(), &cfg, search, "test-only".into());

    let result = async {
        // Round one: 201 > 200, ceiling hit
        let stats = sync_source(&state, source).await?;
        assert_eq!(
            stats.discovered, 201,
            "整条响应都读了，不是只读到预算那一段"
        );
        let first = utopia_store::sources::get(&pool, source).await?;
        assert_eq!(
            first.last_sync_status, "failed",
            "触顶的那一轮不能记为 'ok'，否则下界会越过还没读到的条目"
        );
        assert!(
            first
                .last_sync_error
                .as_deref()
                .unwrap_or_default()
                .contains("stopped with more"),
            "错误要说明为什么停：{:?}",
            first.last_sync_error
        );
        let imported: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM documents WHERE source_id=$1 AND deleted_at IS NULL",
        )
        .bind(source)
        .fetch_one(&pool)
        .await?;
        assert_eq!(imported, 200, "第一轮装得下 200 条，第 201 条得等下一轮");

        // Round two: the same batch comes back as they were; the first 200 are Unchanged and take no budget, and entry 201 is reachable this time
        let stats = sync_source(&state, source).await?;
        assert_eq!(
            stats.discovered, 201,
            "整条响应都读了，不是只读到预算那一段"
        );
        let imported: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM documents WHERE source_id=$1 AND deleted_at IS NULL",
        )
        .bind(source)
        .fetch_one(&pool)
        .await?;
        assert_eq!(imported, 201, "第二轮之后 201 条都在库里了");
        let last = utopia_store::sources::get(&pool, source).await?;
        assert_eq!(
            last.last_sync_status, "ok",
            "这一轮走完了，可以记 ok 让游标前进"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id=$1")
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(org)
        .execute(&pool)
        .await?;
    drop(state);
    let _ = std::fs::remove_dir_all(dir);
    result
}

/// **450 entries read in three rounds** (an acceptance item of 0072).
///
/// The budget is 200, and unchanged entries take no budget: so round two can let the first 200 pass as an unchanged prefix,
/// and round three collects the remaining 50. Every round reads the whole response -- `discovered` is always 450,
/// not just the part within the budget.
#[tokio::test]
async fn a_capped_custom_batch_is_read_whole_over_three_rounds() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    let (org, ws, kb, source) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations(id,name) VALUES($1,'three-round-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces(id,org_id,name) VALUES($1,$2,'three-round-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases(id,workspace_id,name) VALUES($1,$2,'three-round-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    // **The source is inserted first, `start_run` runs after**: source_sync_runs.source_id has a foreign key;
    // inserting the other way round fails with `source_sync_runs_source_id_fkey`.
    let server = MockServer::start().await;
    sqlx::query(
        "INSERT INTO sources(id,kb_id,kind,name,config) VALUES($1,$2,'custom','fixture',$3)",
    )
    .bind(source)
    .bind(kb)
    .bind(serde_json::json!({ "endpoint": server.uri() }))
    .execute(&pool)
    .await?;

    // 450 entries, all new: each round receives them again
    let all: Vec<serde_json::Value> = (0..450u32)
        .map(|i| {
            serde_json::json!({
                "id": format!("item-{i}"),
                "title": format!("Item {i}"),
                "content": format!("Body {i}"),
            })
        })
        .collect();
    let served = all.clone();
    Mock::given(method("GET"))
        .respond_with(move |_req: &Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "items": served }))
        })
        .mount(&server)
        .await;

    let dir = std::env::temp_dir().join(format!("utopia-three-round-{source}"));
    let cfg = utopia_core::config::AppConfig {
        data_dir: dir.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let search = Arc::new(utopia_search::SearchIndex::open(&dir.join("search"))?);
    let state = crate::state::AppState::new(pool.clone(), &cfg, search, "test-only".into());

    let result = async {
        // Round one: fits 200 entries, hits the ceiling, the cursor stays where it is
        let stats = sync_source(&state, source).await?;
        assert_eq!(
            stats.discovered, 450,
            "整条响应都读了，不是只读到预算那一段"
        );
        let imported: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM documents WHERE source_id=$1 AND deleted_at IS NULL",
        )
        .bind(source)
        .fetch_one(&pool)
        .await?;
        assert_eq!(imported, 200, "第一轮装得下 200 条，剩下的留给下一轮");
        let first = utopia_store::sources::get(&pool, source).await?;
        assert_eq!(
            first.last_sync_status, "failed",
            "触顶的那一轮不能记为 'ok'，否则游标会越过还没读到的条目"
        );
        assert!(
            utopia_store::sources::sync_checkpoint(&pool, source)
                .await?
                .is_none(),
            "触顶那一轮游标留在原处，下一轮从那儿接着读"
        );
        // Hitting the ceiling is not a broken connection: this round is recorded as `failed`, but it does **not** send the source.sync_failed alert
        // (the same thing reported daily, #1212).
        let alerts: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM alerts WHERE subject_id=$1 AND kind='source.sync_failed'",
        )
        .bind(source)
        .fetch_one(&pool)
        .await?;
        assert_eq!(alerts, 0, "触顶的那一轮不该发同步失败告警");

        // Round two: the first 200 come back as they were (Unchanged, taking no budget), then 200 more are fetched, and it hits the ceiling again
        let stats = sync_source(&state, source).await?;
        assert_eq!(
            stats.discovered, 450,
            "整条响应都读了，不是只读到预算那一段"
        );
        let imported: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM documents WHERE source_id=$1 AND deleted_at IS NULL",
        )
        .bind(source)
        .fetch_one(&pool)
        .await?;
        assert_eq!(imported, 400, "第二轮之后 400 条都在库里了");
        let second = utopia_store::sources::get(&pool, source).await?;
        assert_eq!(
            second.last_sync_status, "failed",
            "第二轮又触顶，游标仍然不前进"
        );
        assert!(
            utopia_store::sources::sync_checkpoint(&pool, source)
                .await?
                .is_none(),
            "触顶那一轮游标留在原处，下一轮从那儿接着读"
        );

        // Round three: the first 400 come back as they were, the remaining 50 are collected, and this round completes
        let stats = sync_source(&state, source).await?;
        assert_eq!(
            stats.discovered, 450,
            "整条响应都读了，不是只读到预算那一段"
        );
        let imported: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM documents WHERE source_id=$1 AND deleted_at IS NULL",
        )
        .bind(source)
        .fetch_one(&pool)
        .await?;
        assert_eq!(imported, 450, "第三轮之后 450 条都在库里了");
        let third = utopia_store::sources::get(&pool, source).await?;
        assert_eq!(
            third.last_sync_status, "ok",
            "这一轮走完了，可以记 ok 让游标前进"
        );
        assert!(
            utopia_store::sources::sync_checkpoint(&pool, source)
                .await?
                .is_some(),
            "走完的那一轮把游标推进了"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id=$1")
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(org)
        .execute(&pool)
        .await?;
    drop(state);
    let _ = std::fs::remove_dir_all(dir);
    result
}

/// **The endpoint's own ledger**: each ticket's id, body, and when it was last changed. The protocol has no such
/// field -- this is the endpoint's own ledger, used to decide which entries to return.
type Feed = Arc<std::sync::Mutex<Vec<(String, String, chrono::DateTime<chrono::Utc>)>>>;

/// **Changes arriving during the run are still reachable in the next round** (ADR 0072, Custom section:
/// "preserving the existing protection for changes during the run").
///
/// The cursor advances to **the start of this round**, not to now: the endpoint changed one ticket in round one (its update time is after that round started),
/// and round two's lower bound must still be before that change,
/// otherwise it can never be read.
///
/// This assertion has teeth: if the cursor were moved to "now", the endpoint would return nothing because the lower bound is too late,
/// and this ticket would never come in.
#[tokio::test]
async fn an_item_changed_during_a_round_is_still_read_by_the_next_one() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    let (org, ws, kb, source) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations(id,name) VALUES($1,'concurrent-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces(id,org_id,name) VALUES($1,$2,'concurrent-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases(id,workspace_id,name) VALUES($1,$2,'concurrent-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    let server = MockServer::start().await;
    sqlx::query(
        "INSERT INTO sources(id,kb_id,kind,name,config) VALUES($1,$2,'custom','fixture',$3)",
    )
    .bind(source)
    .bind(kb)
    .bind(serde_json::json!({ "endpoint": server.uri() }))
    .execute(&pool)
    .await?;

    // **The endpoint records when each ticket was changed itself** (not a field in the protocol; the endpoint's own ledger).
    // On the first ask, it quietly changed one and added another -- the change times fall after this round started.
    let feed: Feed = Arc::new(std::sync::Mutex::new(vec![(
        "item-0".to_string(),
        "Body 0".to_string(),
        chrono::Utc::now() - chrono::Duration::hours(1),
    )]));
    let served = feed.clone();
    let asked = Arc::new(std::sync::Mutex::new(Vec::<
        Option<chrono::DateTime<chrono::Utc>>,
    >::new()));
    let seen = asked.clone();
    Mock::given(method("GET"))
        .respond_with(move |req: &Request| {
            let since = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "since")
                .map(|(_, v)| {
                    chrono::DateTime::parse_from_rfc3339(&v)
                        .unwrap()
                        .with_timezone(&chrono::Utc)
                });
            seen.lock().unwrap().push(since);
            let mut feed = served.lock().unwrap();
            // The endpoint gives only the ones changed after the lower bound (custom protocol: the lower bound is interpreted by the endpoint itself)
            let items: Vec<serde_json::Value> = feed
                .iter()
                .filter(|(_, _, changed_at)| since.is_none_or(|s| *changed_at > s))
                .map(|(id, content, changed_at)| {
                    serde_json::json!({
                        "id": id,
                        "title": format!("Item {id}"),
                        "content": content,
                        "doc_time": changed_at.to_rfc3339(),
                    })
                })
                .collect();
            // After answering this round it quietly changes one and adds another -- the change times fall after this round started,
            // so only **the next round** can see them (this round answered with the state before the change)
            if feed.len() == 1 {
                let now = chrono::Utc::now();
                feed[0].1 = "Body 0, changed".to_string();
                feed[0].2 = now;
                feed.push(("item-1".to_string(), "Body 1".to_string(), now));
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "items": items }))
        })
        .mount(&server)
        .await;

    let dir = std::env::temp_dir().join(format!("utopia-concurrent-{source}"));
    let cfg = utopia_core::config::AppConfig {
        data_dir: dir.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let search = Arc::new(utopia_search::SearchIndex::open(&dir.join("search"))?);
    let state = crate::state::AppState::new(pool.clone(), &cfg, search, "test-only".into());

    let result = async {
        // Round one: only item-0; once read, it completes
        let first = sync_source(&state, source).await?;
        assert_eq!(first.created, 1, "第一轮只有 item-0");
        let checkpoint = utopia_store::sources::sync_checkpoint(&pool, source).await?;
        assert!(checkpoint.is_some(), "这一轮走完了，游标该落下来");

        // Round two: the endpoint now has two changed entries, and the lower bound must still be before their changes
        let second = sync_source(&state, source).await?;
        let imported: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM documents WHERE source_id=$1 AND deleted_at IS NULL",
        )
        .bind(source)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            imported, 2,
            "运行期间改的那一条、添的那一条，下一轮都得进来（游标要停在那一轮**开始**的时刻，不是结束的时刻）"
        );
        assert_eq!(second.updated, 1, "item-0 是改过的，item-1 是新的");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id=$1")
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(org)
        .execute(&pool)
        .await?;
    drop(state);
    let _ = std::fs::remove_dir_all(dir);
    result
}
