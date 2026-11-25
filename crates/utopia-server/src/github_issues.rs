//! GitHub issue source: one ticket = one document, whose body carries **its status-change history**.
//!
//! ## Why not capture only the current state
//!
//! The most valuable part of a ticket is not "it is closed now" but "it was opened on Aug 18, closed on Aug 20,
//! with who it was assigned to in between and how its labels changed". Capturing only the current state, this timeline would have to be built up slowly over repeated syncs
//! -- the first sync can only see the present moment, and everything that happened before is lost.
//!
//! It is the same judgment we made for the Wikipedia history snapshots: **take the changes, not the present**. The difference is that
//! GitHub hands you the changes directly, with no need to sample them from a revision list as Wikipedia requires.
//!
//! ## Incremental ticket-list filtering; comments and events read per ticket
//!
//! The first version tried to have all three go through repository-level endpoints, paginated in full, to avoid 401 requests for 200 tickets
//! (unauthenticated, GitHub gives only 60 requests an hour). **Running it on real data showed the events path was wrong**:
//!
//! - `issues/comments` supports `since`, but returns only comments that changed. The document is replaced as a whole,
//!   so changing only the title would delete all the old comments from the current document; newly added comments would leave only the new part.
//! - `issues/events` **does not support `since`**; it can only be paged backwards from the newest. And in GitHub's model
//!   PRs also produce issue events -- measured here, the events for tickets sit on page 5; in a repo where PRs are busy,
//!   they would be pushed past the paging limit. **So the "status-change history" silently comes back empty,
//!   which is the very reason this source exists.**
//!
//! So comments and events are both fetched per ticket, without `since`. Only tickets this round will write after filtering are requested,
//! so comments from the whole repo that are irrelevant do not use up the paging budget. The extra requests are a real cost, but N is only
//! **the number of tickets this round will write**: the `since` on the ticket list covers it afterwards, usually single digits.
//!
//! The three fetches are therefore:
//!
//! - `GET /repos/{repo}/issues?state=all&since=` -- the tickets themselves (paginated)
//! - `GET /repos/{repo}/issues/{n}/comments`    -- comments on each ticket (paginated)
//! - `GET /repos/{repo}/issues/{n}/events`       -- once per ticket
//!
//! ## About PRs
//!
//! In GitHub's data model PRs are also issues, and the `/issues` endpoint returns them together, told apart by
//! the `pull_request` field. They are excluded by default: a ticket system wants tickets. But a switch is provided --
//! some repos (including this one) actually write their decision records in PR descriptions.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::ticket::stamp;

/// The list budget can be extended at a complete time-group boundary; a single ticket's detail is not limited by this budget.
const MAX_LISTING_PAGES: u32 = 10;
const PER_PAGE: u32 = 100;
const MAX_RATE_LIMIT_RETRIES: u32 = 3;
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
pub struct Issue {
    pub number: i64,
    pub title: String,
    pub state: String,
    pub body: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub assignees: Vec<Actor>,
    pub user: Option<Actor>,
    /// Presence means this is actually a PR (GitHub stores both in one table)
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Actor {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct Comment {
    /// Which ticket a comment is on: only the URL has the number, so parse it from the last segment of `.../issues/18`
    pub issue_url: String,
    pub user: Option<Actor>,
    pub created_at: DateTime<Utc>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub event: String,
    pub created_at: DateTime<Utc>,
    pub actor: Option<Actor>,
    pub label: Option<Label>,
    pub assignee: Option<Actor>,
}

/// The last segment of a comment's `issue_url` is the ticket number.
///
/// Parse failure returns None rather than panicking: if GitHub changes the URL shape, the cost should be
/// "this comment was not attached to a ticket", not the whole sync blowing up.
fn issue_number_from_url(url: &str) -> Option<i64> {
    url.rsplit('/').next()?.parse().ok()
}

/// Arrange a ticket together with its comments and events into one document ([`crate::ticket::render`]).
///
/// **A pure function, no network** -- fetching is separated from organizing, so the organizing half can be tested.
/// The assembly logic of the three paginated fetches (who belongs to whom, sorted how) is exactly the part most prone to error.
pub fn render(issue: &Issue, comments: &[&Comment], events: &[&Event]) -> String {
    // The heading is written as a dated declarative sentence, not as key-value pairs: the extractor reads sentences.
    // "opened by X on 2026-08-18T16:18:27Z" can extract a fact with valid_from, whereas
    // "created_at: 2026-08-18" makes it guess what that means
    let mut sentences = Vec::new();
    if let Some(u) = &issue.user {
        sentences.push(format!(
            "Opened by {} on {}.",
            u.login,
            stamp(issue.created_at)
        ));
    }
    sentences.push(format!("Currently {}.", issue.state));
    if let Some(c) = issue.closed_at {
        sentences.push(format!("Closed on {}.", stamp(c)));
    }
    if !issue.labels.is_empty() {
        let names: Vec<&str> = issue.labels.iter().map(|l| l.name.as_str()).collect();
        sentences.push(format!("Labelled {}.", names.join(", ")));
    }
    if !issue.assignees.is_empty() {
        let names: Vec<&str> = issue.assignees.iter().map(|a| a.login.as_str()).collect();
        sentences.push(format!("Assigned to {}.", names.join(", ")));
    }
    // **The status-change history is the reason this source exists.** Every line carries a date,
    // so the ledger gets "what became what, and when", not a static current value
    let history = events
        .iter()
        .map(|e| {
            let who = e.actor.as_ref().map(|a| a.login.as_str()).unwrap_or("?");
            let detail = match (e.event.as_str(), &e.label, &e.assignee) {
                ("labeled" | "unlabeled", Some(l), _) => format!(" ({})", l.name),
                ("assigned" | "unassigned", _, Some(a)) => format!(" ({})", a.login),
                _ => String::new(),
            };
            (e.created_at, format!("{} by {}{}", e.event, who, detail))
        })
        .collect();
    let comments = comments
        .iter()
        .map(|c| crate::ticket::Comment {
            author: c
                .user
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_else(|| "?".into()),
            at: Some(c.created_at),
            body: c.body.clone().unwrap_or_default(),
        })
        .collect();
    crate::ticket::render(&crate::ticket::Ticket {
        heading: format!("#{} {}", issue.number, issue.title),
        sentences,
        description: issue.body.clone(),
        history,
        comments,
    })
}

/// Keep only this ticket's comments, sorted by time ascending; comments that cannot be attributed to it in the response must not be mixed into the body.
pub fn comments_for_issue<'a>(issue: &Issue, comments: &'a [Comment]) -> Vec<&'a Comment> {
    let mut comments: Vec<_> = comments
        .iter()
        .filter(|comment| issue_number_from_url(&comment.issue_url) == Some(issue.number))
        .collect();
    comments.sort_by_key(|comment| comment.created_at);
    comments
}

/// Events sorted by time ascending. The per-ticket endpoint returns something that **looks** ascending, but the order is not a contract,
/// and getting it wrong here means the history is told backwards.
pub fn sort_events(mut events: Vec<Event>) -> Vec<Event> {
    events.sort_by_key(|e| e.created_at);
    events
}

/// Sends a GET with a token and checks the status. github_issues and github_repo share this one path,
/// so the rate-limit hint is not written twice across two sources and drifts apart over time.
pub(crate) async fn get_checked(
    http: &reqwest::Client,
    url: reqwest::Url,
    auth: Option<&str>,
    accept: Option<&str>,
) -> anyhow::Result<reqwest::Response> {
    let mut retries = 0;
    loop {
        let mut req = http.get(url.clone());
        if let Some(a) = auth {
            req = req.header(reqwest::header::AUTHORIZATION, a);
        }
        if let Some(a) = accept {
            req = req.header(reqwest::header::ACCEPT, a);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        if matches!(
            status,
            reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::TOO_MANY_REQUESTS
        ) && retries < MAX_RATE_LIMIT_RETRIES
        {
            if let Some(delay) = rate_limit_delay(resp.headers(), Utc::now()) {
                // Waiting stays inside the current request; otherwise, for very large time groups, every retry would exhaust the prefix's budget again.
                let delay = delay.max(Duration::from_secs(1 << retries));
                retries += 1;
                tracing::info!(
                    retries,
                    wait_seconds = delay.as_secs(),
                    "GitHub rate limited; waiting before retry"
                );
                drop(resp);
                tokio::time::sleep(delay).await;
                continue;
            }
        }
        // Name the rate limit clearly as a rate limit: unauthenticated it is 60 an hour, and one sync of a mid-sized repo can eat all of it.
        // Reporting it as a generic HTTP error would send people to check the network, when the right action is to configure a token
        let remaining = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("?");
        if status == reqwest::StatusCode::FORBIDDEN && remaining == "0" {
            anyhow::bail!(
                "GitHub 限流：本小时配额已用尽。未认证时每小时 60 次，配一个令牌可提到 5000"
            );
        }
        anyhow::bail!("HTTP {status} from GitHub");
    }
}

fn rate_limit_delay(headers: &reqwest::header::HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .trim()
                .parse::<u64>()
                .ok()
                .map(Duration::from_secs)
                .or_else(|| {
                    DateTime::parse_from_rfc2822(value)
                        .ok()
                        .map(|at| at.signed_duration_since(now).to_std().unwrap_or_default())
                })
        });
    let delay = retry_after.or_else(|| {
        if headers.get("x-ratelimit-remaining")?.to_str().ok()? != "0" {
            return None;
        }
        let reset = headers
            .get("x-ratelimit-reset")?
            .to_str()
            .ok()?
            .parse::<i64>()
            .ok()?;
        let reset = DateTime::from_timestamp(reset, 0)?;
        Some(
            reset
                .signed_duration_since(now)
                .to_std()
                .unwrap_or_default(),
        )
    })?;
    // A long wait must not hold a worker; refuse this retry rather than resend before the server's allowed time.
    (delay <= MAX_RATE_LIMIT_WAIT).then_some(delay)
}

pub struct IssueWindow {
    pub issues: Vec<Issue>,
    pub checkpoint: Option<DateTime<Utc>>,
}

/// Offset paging is not a snapshot; re-read the same prefix, and if it changed, have the caller keep the old checkpoint and retry.
pub async fn fetch_window(
    http: &reqwest::Client,
    base: &str,
    since: Option<DateTime<Utc>>,
    item_limit: usize,
    include_prs: bool,
    auth: Option<&str>,
) -> anyhow::Result<IssueWindow> {
    let (window, positions) =
        fetch_window_pass(http, base, since, item_limit, include_prs, auth, None).await?;
    if let Some(checkpoint) = window
        .checkpoint
        .filter(|checkpoint| Some(*checkpoint) != since)
    {
        let (verified, verified_positions) = fetch_window_pass(
            http,
            base,
            since,
            item_limit,
            include_prs,
            auth,
            Some(checkpoint),
        )
        .await?;
        anyhow::ensure!(
            verified.checkpoint == Some(checkpoint) && positions == verified_positions,
            "GitHub issues changed during pagination; retry the previous checkpoint"
        );
    }
    Ok(window)
}

async fn fetch_window_pass(
    http: &reqwest::Client,
    base: &str,
    since: Option<DateTime<Utc>>,
    item_limit: usize,
    include_prs: bool,
    auth: Option<&str>,
    through: Option<DateTime<Utc>>,
) -> anyhow::Result<(IssueWindow, BTreeMap<i64, DateTime<Utc>>)> {
    let mut query = vec![
        ("state", "all".to_owned()),
        ("sort", "updated".to_owned()),
        ("direction", "asc".to_owned()),
    ];
    if let Some(since) = since {
        query.push(("since", since.to_rfc3339()));
    }
    let mut window = IssueWindow {
        issues: Vec::new(),
        checkpoint: since,
    };
    let mut positions = BTreeMap::new();
    let mut previous_time = None;
    let mut stop_after = through;
    let mut listing_pages = 0;
    let mut page = 1;
    loop {
        let mut url = reqwest::Url::parse(base)?;
        {
            let mut parameters = url.query_pairs_mut();
            for (key, value) in &query {
                parameters.append_pair(key, value);
            }
            parameters.append_pair("per_page", &PER_PAGE.to_string());
            parameters.append_pair("page", &page.to_string());
        }
        let batch: Vec<Issue> = get_checked(http, url, auth, None).await?.json().await?;
        let count = batch.len();
        let mut page_counted = false;
        for issue in batch {
            let updated = issue.updated_at;
            anyhow::ensure!(
                previous_time.is_none_or(|previous| updated >= previous),
                "GitHub issues are not ordered by update time; retry the previous checkpoint"
            );
            previous_time = Some(updated);
            if stop_after.is_some_and(|boundary| updated > boundary) {
                return Ok((window, positions));
            }
            // Consumed entries and excluded PRs can also move the page boundary, so they must take part in the re-read check.
            anyhow::ensure!(
                positions.insert(issue.number, updated).is_none(),
                "GitHub issue repeated during pagination; retry the previous checkpoint"
            );
            if since.is_some_and(|checkpoint| updated <= checkpoint) {
                continue;
            }
            if !page_counted {
                listing_pages += 1;
                page_counted = true;
            }
            window.checkpoint = Some(updated);
            if include_prs || issue.pull_request.is_none() {
                window.issues.push(issue);
            }
            if through.is_none()
                && (window.issues.len() >= item_limit || listing_pages >= MAX_LISTING_PAGES)
            {
                stop_after = Some(updated);
            }
        }
        if count < PER_PAGE as usize {
            return Ok((window, positions));
        }
        page += 1;
    }
}

/// Details must be read to the last page; a truncated comment or event list must not overwrite an existing complete body.
pub async fn fetch_all<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    base: &str,
    query: &[(&str, String)],
    auth: Option<&str>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    let mut page = 1;
    loop {
        // Build the query string myself: this reqwest feature combination has no RequestBuilder::query,
        // and sync_custom already builds it this way
        let mut url = reqwest::Url::parse(base)?;
        {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
            q.append_pair("per_page", &PER_PAGE.to_string());
            q.append_pair("page", &page.to_string());
        }
        let resp = get_checked(http, url, auth, None).await?;
        let batch: Vec<T> = resp.json().await?;
        let n = batch.len();
        out.extend(batch);
        if n < PER_PAGE as usize {
            break;
        }
        page += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};

    fn listing_row(number: i64, updated: &str) -> Value {
        json!({
            "number": number, "title": format!("Issue {number}"), "state": "open",
            "created_at": "2026-01-01T00:00:00Z", "updated_at": updated,
        })
    }

    fn requested_page(request: &Request) -> usize {
        request
            .url
            .query_pairs()
            .find(|(key, _)| key == "page")
            .unwrap()
            .1
            .parse()
            .unwrap()
    }

    #[test]
    fn rate_limit_headers_supply_a_bounded_wait_without_guessing() {
        let now: DateTime<Utc> = "2026-01-02T00:00:00Z".parse().unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        for (value, expected) in [
            ("2", Some(2)),
            ("0", Some(0)),
            ("30", Some(30)),
            ("31", None),
            ("-1", None),
            ("broken", None),
            ("86401", None),
        ] {
            headers.insert(reqwest::header::RETRY_AFTER, value.parse().unwrap());
            assert_eq!(
                rate_limit_delay(&headers, now),
                expected.map(Duration::from_secs)
            );
        }
        for (seconds, expected) in [(3, Some(3)), (31, None)] {
            headers.insert(
                reqwest::header::RETRY_AFTER,
                (now + chrono::Duration::seconds(seconds))
                    .to_rfc2822()
                    .parse()
                    .unwrap(),
            );
            assert_eq!(
                rate_limit_delay(&headers, now),
                expected.map(Duration::from_secs)
            );
        }
        headers.remove(reqwest::header::RETRY_AFTER);
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert(
            "x-ratelimit-reset",
            (now.timestamp() + 30).to_string().parse().unwrap(),
        );
        assert_eq!(
            rate_limit_delay(&headers, now),
            Some(Duration::from_secs(30))
        );
        headers.insert("x-ratelimit-remaining", "1".parse().unwrap());
        assert_eq!(rate_limit_delay(&headers, now), None);
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert(
            "x-ratelimit-reset",
            (now.timestamp() + 31).to_string().parse().unwrap(),
        );
        assert_eq!(rate_limit_delay(&headers, now), None);
    }

    #[tokio::test]
    async fn a_rate_limited_request_waits_and_preserves_its_url_and_headers() {
        let server = MockServer::start().await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        for status in [403, 429] {
            server.reset().await;
            let attempts = AtomicUsize::new(0);
            Mock::given(method("GET"))
                .respond_with(move |_: &Request| {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        ResponseTemplate::new(status).insert_header("Retry-After", "0")
                    } else {
                        ResponseTemplate::new(200)
                    }
                })
                .mount(&server)
                .await;
            let url = reqwest::Url::parse(&format!("{}/resource?page=2", server.uri())).unwrap();
            let started = std::time::Instant::now();
            get_checked(
                &http,
                url.clone(),
                Some("Bearer fixture"),
                Some("application/vnd.github.raw+json"),
            )
            .await
            .unwrap();
            assert!(started.elapsed() >= Duration::from_secs(1));
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2);
            for request in &requests {
                // wiremock adds localhost to origin-form URIs; the real target host is kept in the Host header.
                assert_eq!(request.url.path(), url.path());
                assert_eq!(request.url.query(), url.query());
                assert_eq!(
                    request.headers.get("host").unwrap().to_str().unwrap(),
                    server.address().to_string()
                );
                assert_eq!(
                    request.headers.get("authorization").unwrap(),
                    "Bearer fixture"
                );
                assert_eq!(
                    request.headers.get("accept").unwrap(),
                    "application/vnd.github.raw+json"
                );
            }
        }
    }

    #[tokio::test]
    async fn persistent_rate_limits_stop_after_a_bounded_number_of_requests() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .mount(&server)
            .await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        assert!(
            get_checked(&http, server.uri().parse().unwrap(), None, None)
                .await
                .is_err()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn long_rate_limit_waits_fail_without_sleeping_or_retrying() {
        let server = MockServer::start().await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        for status in [403, 429] {
            let responses = [
                ResponseTemplate::new(status).insert_header("Retry-After", "31"),
                ResponseTemplate::new(status).insert_header(
                    "Retry-After",
                    (Utc::now() + chrono::Duration::seconds(60)).to_rfc2822(),
                ),
                ResponseTemplate::new(status)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header(
                        "x-ratelimit-reset",
                        (Utc::now().timestamp() + 60).to_string(),
                    ),
            ];
            for response in responses {
                server.reset().await;
                Mock::given(method("GET"))
                    .respond_with(response)
                    .mount(&server)
                    .await;
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    get_checked(&http, server.uri().parse().unwrap(), None, None),
                )
                .await
                .expect("a long rate-limit wait must fail immediately");
                assert!(result.is_err());
                assert_eq!(server.received_requests().await.unwrap().len(), 1);
            }
        }
    }

    #[tokio::test]
    async fn an_empty_window_retains_the_previous_checkpoint_without_verification() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let since = Some("2026-01-02T00:00:00Z".parse().unwrap());
        let window = fetch_window(&http, &server.uri(), since, 200, false, None)
            .await
            .unwrap();
        assert!(window.issues.is_empty());
        assert_eq!(window.checkpoint, since);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_large_timestamp_group_finishes_and_the_next_run_passes_its_old_pages() {
        let server = MockServer::start().await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let group_time = "2026-01-02T00:00:00Z";
        let later_time = "2026-01-03T00:00:00Z";
        // When there are many PRs, exhaust the ten-page budget first; when all are tickets, exhaust the 200-entry budget first.
        for issue_interval in [1, 100] {
            server.reset().await;
            let mut rows: Vec<_> = (1..=1001)
                .map(|number| {
                    let mut row = listing_row(number, group_time);
                    if (number - 1) % issue_interval as i64 != 0 {
                        row["pull_request"] = json!({});
                    }
                    row
                })
                .collect();
            rows.push(listing_row(1002, later_time));
            Mock::given(method("GET"))
                .respond_with(move |request: &Request| {
                    let page = requested_page(request);
                    let rows: Vec<_> = rows
                        .iter()
                        .skip((page - 1) * 100)
                        .take(100)
                        .cloned()
                        .collect();
                    ResponseTemplate::new(200).set_body_json(rows)
                })
                .mount(&server)
                .await;
            let first = fetch_window(&http, &server.uri(), None, 200, false, None)
                .await
                .unwrap();
            let numbers: Vec<_> = first.issues.iter().map(|issue| issue.number).collect();
            assert_eq!(
                numbers,
                (1..=1001).step_by(issue_interval).collect::<Vec<_>>()
            );
            assert_eq!(first.checkpoint, Some(group_time.parse().unwrap()));

            // The mock endpoint still returns an inclusive lower bound; ten pages of old records must not exhaust either budget of the new round.
            let next = fetch_window(&http, &server.uri(), first.checkpoint, 200, false, None)
                .await
                .unwrap();
            assert_eq!(next.issues.len(), 1);
            assert_eq!(next.issues[0].number, 1002);
            assert_eq!(next.checkpoint, Some(later_time.parse().unwrap()));
            let requests = server.received_requests().await.unwrap();
            assert!(requests.iter().all(|request| {
                request
                    .url
                    .query_pairs()
                    .any(|(key, value)| key == "sort" && value == "updated")
                    && request
                        .url
                        .query_pairs()
                        .any(|(key, value)| key == "direction" && value == "asc")
            }));
            assert!(requests.iter().any(|request| requested_page(request) == 11));
        }
    }

    #[tokio::test]
    async fn a_moved_issue_rejects_the_window_and_is_read_after_pagination_stabilizes() {
        let server = MockServer::start().await;
        let moved = AtomicBool::new(false);
        Mock::given(method("GET"))
            .respond_with(move |request: &Request| {
                let page = requested_page(request);
                if page == 2 {
                    moved.store(true, Ordering::SeqCst);
                }
                let mut rows: Vec<_> = (1..=201)
                    .map(|number| listing_row(number, "2026-01-02T00:00:00Z"))
                    .collect();
                if moved.load(Ordering::SeqCst) {
                    rows.remove(0);
                    rows.push(listing_row(1, "2026-01-03T00:00:00Z"));
                }
                let rows: Vec<_> = rows.into_iter().skip((page - 1) * 100).take(100).collect();
                ResponseTemplate::new(200).set_body_json(rows)
            })
            .mount(&server)
            .await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let result = fetch_window(&http, &server.uri(), None, 200, false, None).await;
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("changed during pagination"));

        let recovered = fetch_window(&http, &server.uri(), None, 200, false, None)
            .await
            .unwrap();
        let recovered_numbers: Vec<_> = recovered.issues.iter().map(|issue| issue.number).collect();
        assert_eq!(recovered_numbers, (2..=201).collect::<Vec<_>>());
        assert_eq!(
            recovered.checkpoint,
            Some("2026-01-02T00:00:00Z".parse().unwrap())
        );
        let next = fetch_window(&http, &server.uri(), recovered.checkpoint, 200, false, None)
            .await
            .unwrap();
        assert_eq!(next.issues.len(), 1);
        assert_eq!(next.issues[0].number, 1);
        assert_eq!(
            next.checkpoint,
            Some("2026-01-03T00:00:00Z".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn a_complete_discussion_is_read_beyond_ten_pages() {
        let server = MockServer::start().await;
        for count in [1000, 1001, 1100] {
            server.reset().await;
            Mock::given(method("GET"))
                .respond_with(move |request: &Request| {
                    let page = requested_page(request);
                    let rows: Vec<_> = (0..count).skip((page - 1) * 100).take(100).collect();
                    ResponseTemplate::new(200).set_body_json(rows)
                })
                .mount(&server)
                .await;
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            let result = fetch_all::<u32>(&http, &server.uri(), &[], None)
                .await
                .unwrap();
            assert_eq!(result, (0..count).collect::<Vec<_>>());
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), count as usize / 100 + 1);
        }
    }

    #[tokio::test]
    async fn a_failed_discussion_page_does_not_return_a_partial_history() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(|request: &Request| {
                let page = requested_page(request);
                if page == 11 {
                    return ResponseTemplate::new(503);
                }
                let rows: Vec<_> = (0..1001).skip((page - 1) * 100).take(100).collect();
                ResponseTemplate::new(200).set_body_json(rows)
            })
            .mount(&server)
            .await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let result = fetch_all::<u32>(&http, &server.uri(), &[], None).await;
        assert!(result.unwrap_err().to_string().contains("503"));
        assert_eq!(server.received_requests().await.unwrap().len(), 11);
    }

    fn issue(json: serde_json::Value) -> Issue {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn a_comment_url_yields_its_issue_number() {
        assert_eq!(
            issue_number_from_url("https://api.github.com/repos/a/b/issues/18"),
            Some(18)
        );
        // If the shape changed, it cannot be attributed, but that should not blow up
        assert_eq!(issue_number_from_url("https://example.com/"), None);
        assert_eq!(issue_number_from_url("nonsense"), None);
    }

    /// **The status-change history must go into the body.** This is the whole difference between this source and "scraping a web page":
    /// without it, a ticket in the ledger is just a static current value.
    #[test]
    fn the_history_lands_in_the_document_with_dates() {
        let i = issue(serde_json::json!({
            "number": 18, "title": "Deleting a conversation removes all turns",
            "state": "closed", "body": "Steps to reproduce…",
            "created_at": "2026-08-18T16:18:27Z", "updated_at": "2026-08-20T22:31:47Z",
            "closed_at": "2026-08-20T22:31:47Z",
            "labels": [{"name": "bug"}], "assignees": [{"login": "WaylandYang"}],
            "user": {"login": "Danmushu"}
        }));
        let e1: Event = serde_json::from_value(serde_json::json!({
            "event": "labeled", "created_at": "2026-08-19T01:00:00Z",
            "actor": {"login": "WaylandYang"}, "label": {"name": "bug"}
        }))
        .unwrap();
        let e2: Event = serde_json::from_value(serde_json::json!({
            "event": "closed", "created_at": "2026-08-20T22:31:47Z",
            "actor": {"login": "WaylandYang"}
        }))
        .unwrap();
        let out = render(&i, &[], &[&e1, &e2]);

        assert!(
            out.contains("Opened by Danmushu on 2026-08-18T16:18:27Z."),
            "{out}"
        );
        assert!(out.contains("Closed on 2026-08-20T22:31:47Z."), "{out}");
        assert!(out.contains("Labelled bug."), "{out}");
        assert!(out.contains("Assigned to WaylandYang."), "{out}");
        // Event lines carry a date and the actor; labeled also carries which label
        assert!(
            out.contains("- 2026-08-19T01:00:00Z — labeled by WaylandYang (bug)"),
            "{out}"
        );
        assert!(
            out.contains("- 2026-08-20T22:31:47Z — closed by WaylandYang"),
            "{out}"
        );
    }

    /// The timestamps in the body are readable by the extractor, at second precision: an event at UTC 23:59:59 has a start
    /// at that second, not midnight of that day (#691)
    #[test]
    fn a_time_in_the_document_reads_back_to_the_second() {
        let i = issue(serde_json::json!({
            "number": 18, "title": "Anything", "state": "open",
            "created_at": "2026-09-05T23:59:59Z", "updated_at": "2026-09-05T23:59:59Z",
            "user": {"login": "x"}
        }));
        let c: Comment = serde_json::from_value(serde_json::json!({
            "issue_url": "https://api.github.com/repos/x/y/issues/18",
            "user": {"login": "x"}, "created_at": "2026-09-05T23:59:59Z", "body": "late"
        }))
        .unwrap();
        let out = render(&i, &[&c], &[]);
        let written = "2026-09-05T23:59:59Z";
        assert!(out.contains(&format!("Opened by x on {written}.")), "{out}");
        assert!(out.contains(&format!("### x on {written}")), "{out}");
        let (at, precision) = utopia_extract::parse_time(written).expect("reads back");
        assert_eq!(precision, "second");
        assert_eq!(at, i.created_at);
    }

    /// Comments are still verified by ticket number, and sorted by time ascending.
    /// The consequence of misattribution is subtle: another ticket's discussion appears in this ticket's body.
    #[test]
    fn comments_belong_to_the_issue_and_are_chronological() {
        let issues = [
            issue(serde_json::json!({
                "number": 1, "title": "one", "state": "open", "body": null,
                "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-02T00:00:00Z",
                "closed_at": null, "user": {"login": "a"}
            })),
            issue(serde_json::json!({
                "number": 2, "title": "two", "state": "open", "body": null,
                "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-02T00:00:00Z",
                "closed_at": null, "user": {"login": "b"}
            })),
        ];
        let comments: Vec<Comment> = serde_json::from_value(serde_json::json!([
            {"issue_url": "https://api.github.com/repos/a/b/issues/2",
             "user": {"login": "x"}, "created_at": "2026-01-05T00:00:00Z", "body": "第二条"},
            {"issue_url": "https://api.github.com/repos/a/b/issues/2",
             "user": {"login": "y"}, "created_at": "2026-01-03T00:00:00Z", "body": "第一条"},
            // Not in this batch's ticket set: it must be dropped, not attached to someone else
            {"issue_url": "https://api.github.com/repos/a/b/issues/99",
             "user": {"login": "z"}, "created_at": "2026-01-04T00:00:00Z", "body": "别人的"},
            {"issue_url": "nonsense",
             "user": null, "created_at": "2026-01-02T00:00:00Z", "body": "无法归属"}
        ]))
        .unwrap();
        let one_comments = comments_for_issue(&issues[0], &comments);
        assert!(one_comments.is_empty(), "1 号没有评论");

        let two_comments = comments_for_issue(&issues[1], &comments);
        assert_eq!(two_comments.len(), 2, "其他工单和无法归属的评论不该混进来");
        // Ascending: first "the first one" (01-03), then "the second one" (01-05)
        assert_eq!(two_comments[0].body.as_deref(), Some("第一条"));
    }

    /// The events returned by the per-ticket endpoint have **no issue field** (the context is already in the URL),
    /// so that field must be optional -- the first version was written as required from the repo-level response, and after switching endpoints it could not be decoded.
    #[test]
    fn per_issue_events_parse_without_an_issue_field() {
        let evs: Vec<Event> = serde_json::from_value(serde_json::json!([
            {"event": "closed", "created_at": "2026-01-06T00:00:00Z", "actor": {"login": "x"}},
            {"event": "labeled", "created_at": "2026-01-02T00:00:00Z",
             "actor": {"login": "y"}, "label": {"name": "bug"}}
        ]))
        .unwrap();
        let sorted = sort_events(evs);
        // The endpoint's order is not a contract; sort it ourselves
        assert_eq!(sorted[0].event, "labeled");
        assert_eq!(sorted[1].event, "closed");
    }

    /// A PR is also an issue in GitHub's model, and this field identifies it.
    #[test]
    fn a_pull_request_is_recognisable_among_the_issues() {
        let pr = issue(serde_json::json!({
            "number": 3, "title": "a pr", "state": "open", "body": null,
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
            "closed_at": null, "user": {"login": "a"},
            "pull_request": {"url": "https://api.github.com/repos/a/b/pulls/3"}
        }));
        assert!(pr.pull_request.is_some());
        let plain = issue(serde_json::json!({
            "number": 4, "title": "an issue", "state": "open", "body": null,
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
            "closed_at": null, "user": {"login": "a"}
        }));
        assert!(plain.pull_request.is_none());
    }

    /// **Pin the field shapes with real responses.**
    ///
    /// A hand-written JSON can only prove that "the shape I imagined" decodes. A GitHub issue has over a hundred fields,
    /// and we declare only ten; which field is actually named something else, and which is null in some cases,
    /// only real data can say. The fixture comes from deeplethe/utopia, with fields trimmed to the ones we declare
    /// (the trimming itself also shows that undeclared fields do not make serde fail).
    ///
    /// In particular pin one thing: **the per-ticket events endpoint does not return an `issue` field**. The first version was written as required from the repo-level
    /// response, and after switching endpoints a whole batch failed to decode.
    #[test]
    fn the_real_github_shapes_still_parse() {
        #[derive(serde::Deserialize)]
        struct Fixture {
            issues: Vec<Issue>,
            comments: Vec<Comment>,
            events_by_issue: std::collections::HashMap<String, Vec<Event>>,
        }
        let raw = include_str!("../tests/fixtures/github_issues.json");
        let f: Fixture = serde_json::from_str(raw).expect("真实响应该解得出来");

        assert!(!f.issues.is_empty(), "夹具是空的，这条测试就什么都没验");
        // If all are PRs, the PR-filter path is not covered by this fixture
        assert!(
            f.issues.iter().all(|i| i.pull_request.is_none()),
            "夹具应当只含真工单"
        );

        // Every ticket yields a non-empty document, and the heading sentence carries a real date
        for issue in &f.issues {
            let cs = comments_for_issue(issue, &f.comments);
            let events = sort_events(
                f.events_by_issue
                    .get(&issue.number.to_string())
                    .cloned()
                    .unwrap_or_default(),
            );
            let es: Vec<&Event> = events.iter().collect();
            let doc = render(issue, &cs, &es);
            assert!(
                doc.contains(&format!("# #{} ", issue.number)),
                "#{} 的抬头不对：{doc}",
                issue.number
            );
            assert!(
                doc.contains(&super::stamp(issue.created_at)),
                "#{} 正文里没有创建日期",
                issue.number
            );
        }

        // Every ticket in this fixture was closed at some point, so the history section must appear --
        // it is the reason this source exists; if it is empty, it is back to just "scraping a web page"
        let first = &f.issues[0];
        let cs = comments_for_issue(first, &f.comments);
        let events = sort_events(
            f.events_by_issue
                .get(&first.number.to_string())
                .cloned()
                .unwrap_or_default(),
        );
        assert!(!events.is_empty(), "夹具里 #{} 没有事件", first.number);
        let es: Vec<&Event> = events.iter().collect();
        let doc = render(first, &cs, &es);
        assert!(doc.contains("## History"), "历史一节缺失：{doc}");
        assert!(doc.contains("— closed by"), "关闭事件没写进历史：{doc}");
    }

    /// An empty comment should not leave an empty section with only a heading in the body.
    #[test]
    fn an_empty_comment_leaves_no_stub() {
        let i = issue(serde_json::json!({
            "number": 1, "title": "t", "state": "open", "body": null,
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
            "closed_at": null, "user": {"login": "a"}
        }));
        let c: Comment = serde_json::from_value(serde_json::json!({
            "issue_url": "https://api.github.com/repos/a/b/issues/1",
            "user": {"login": "x"}, "created_at": "2026-01-02T00:00:00Z", "body": "   "
        }))
        .unwrap();
        let out = render(&i, &[&c], &[]);
        assert!(!out.contains("### x"), "空评论不该留下小节：{out}");
    }
}
