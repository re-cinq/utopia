//! GitLab issue source (gitlab.com, #1327): one issue or one merge request = one document, with its
//! status-change history in the body. The layout is [`crate::ticket`], shared by all three ticket sources (0013 decision 7); this file only covers putting GitLab's
//! data in and how to fetch it. Sync runs in `ingest_sources`.
//!
//! Fetching follows the GitHub shape: the list is filtered incrementally by update time, and comments and events are read to the last page per issue.
//!
//! - `GET /projects/:id/issues?state=all&order_by=updated_at&sort=asc&updated_after=`
//! - `GET /projects/:id/merge_requests?…` (same parameters; read only when `include_merge_requests` is on)
//! - for each issue: `notes`, `resource_state_events`, `resource_label_events`,
//!   `resource_milestone_events`
//!
//! Three differences from GitHub:
//!
//! - **Issues and merge requests are two lists, but there is only one checkpoint.** So the checkpoint can only advance to the moment
//!   both lists have been read through; see [`fetch_window`].
//! - **History comes from resource events, not system notes.** GitLab records changes to state, labels and milestones as
//!   structured events, and also writes a system note for people to read; reading both would show every change twice in the history.
//! - **Notes and events without a token return 401**, even for public projects (checked on gitlab.com on 2026-10-22 and 2026-11-14).
//!   So this source requires a token; without one it says so, rather than sending requests that are bound to fail.
//!
//! - **Confidential issues are not collected.** With a token of enough permission, the list also returns `confidential` issues, including in public projects;
//!   collecting them would make a document every reader in the library can see. So the list is still read (checkpoint and re-read checks unchanged),
//!   but those few are not written, and their notes and events are not read either.
//!
//! Self-hosted GitLab has not been verified.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;

use crate::ticket::stamp;

/// The list budget may be extended at a complete time-group boundary; a single issue's details are not limited by this budget. Same basis as GitHub (0072)
const MAX_LISTING_PAGES: u32 = 10;
/// GitLab returns at most 100 entries per page
const PER_PAGE: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Issue,
    MergeRequest,
}

impl Kind {
    /// The segment in the API path
    pub fn path(self) -> &'static str {
        match self {
            Self::Issue => "issues",
            Self::MergeRequest => "merge_requests",
        }
    }

    /// GitLab's own notation: `#12` is an issue, `!7` is a merge request. Each one's iid counts from 1 separately,
    /// so the header and the logical identity both must carry the prefix
    pub fn sigil(self) -> char {
        match self {
            Self::Issue => '#',
            Self::MergeRequest => '!',
        }
    }
}

/// A row in the list. Issues and merge requests read the same set of fields; a merge request adds one `merged_at`
#[derive(Debug, Deserialize)]
pub struct Item {
    pub iid: i64,
    pub title: String,
    pub state: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub merged_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignees: Vec<User>,
    pub author: Option<User>,
    /// Only issues have this; merge requests do not have this field, and it is read as false
    #[serde(default)]
    pub confidential: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub username: String,
}

#[derive(Debug, Deserialize)]
pub struct Note {
    /// Of the kind GitLab writes itself ("closed", "added ~bug label"); what they say is already said by the resource events
    #[serde(default)]
    pub system: bool,
    pub author: Option<User>,
    pub created_at: DateTime<Utc>,
    pub body: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StateEvent {
    pub user: Option<User>,
    pub created_at: DateTime<Utc>,
    /// `closed` / `reopened` / `merged`
    pub state: String,
}

#[derive(Debug, Deserialize)]
pub struct Named {
    #[serde(alias = "title")]
    pub name: String,
}

/// Label and milestone events share one shape: who, when, `add` or `remove`, and which one.
/// When a label or milestone is deleted later, that item is null
#[derive(Debug, Deserialize)]
pub struct ChangeEvent {
    pub user: Option<User>,
    pub created_at: DateTime<Utc>,
    pub action: String,
    #[serde(alias = "label", alias = "milestone")]
    pub what: Option<Named>,
}

/// The four detail pieces of one issue; the field names are the endpoint names
#[derive(Debug, Default, Deserialize)]
pub struct Details {
    pub notes: Vec<Note>,
    pub resource_state_events: Vec<StateEvent>,
    pub resource_label_events: Vec<ChangeEvent>,
    pub resource_milestone_events: Vec<ChangeEvent>,
}

/// Lays out one issue and its details as a document. A pure function; no network calls.
///
/// The sentence order is the same as GitHub's (opened, status, closed, labels, assignees); merge requests add a "Merged on" sentence next to "Closed on".
/// Events use the same words as GitHub's (labeled / unlabeled /
/// milestoned / demilestoned), so the same thing reads as the same sentence in both sources
pub fn render(kind: Kind, item: &Item, details: &Details) -> String {
    fn who(u: &Option<User>) -> &str {
        u.as_ref().map(|u| u.username.as_str()).unwrap_or("?")
    }
    let mut sentences = Vec::new();
    if let Some(u) = &item.author {
        sentences.push(format!(
            "Opened by {} on {}.",
            u.username,
            stamp(item.created_at)
        ));
    }
    sentences.push(format!("Currently {}.", item.state));
    if let Some(t) = item.closed_at {
        sentences.push(format!("Closed on {}.", stamp(t)));
    }
    if let Some(t) = item.merged_at {
        sentences.push(format!("Merged on {}.", stamp(t)));
    }
    if !item.labels.is_empty() {
        sentences.push(format!("Labelled {}.", item.labels.join(", ")));
    }
    if !item.assignees.is_empty() {
        let names: Vec<&str> = item.assignees.iter().map(|a| a.username.as_str()).collect();
        sentences.push(format!("Assigned to {}.", names.join(", ")));
    }

    let change = |e: &ChangeEvent, add: &str, remove: &str| {
        let verb = match e.action.as_str() {
            "add" => add,
            "remove" => remove,
            other => other,
        };
        let detail = e
            .what
            .as_ref()
            .map(|w| format!(" ({})", w.name))
            .unwrap_or_default();
        (e.created_at, format!("{verb} by {}{detail}", who(&e.user)))
    };
    let history = details
        .resource_state_events
        .iter()
        .map(|e| (e.created_at, format!("{} by {}", e.state, who(&e.user))))
        .chain(
            details
                .resource_label_events
                .iter()
                .map(|e| change(e, "labeled", "unlabeled")),
        )
        .chain(
            details
                .resource_milestone_events
                .iter()
                .map(|e| change(e, "milestoned", "demilestoned")),
        )
        .collect();

    // The endpoint's order is not a contract, so sort it ourselves
    let mut notes: Vec<&Note> = details.notes.iter().filter(|n| !n.system).collect();
    notes.sort_by_key(|n| n.created_at);
    let comments = notes
        .into_iter()
        .map(|n| crate::ticket::Comment {
            author: who(&n.author).to_owned(),
            at: Some(n.created_at),
            body: n.body.clone().unwrap_or_default(),
        })
        .collect();

    crate::ticket::render(&crate::ticket::Ticket {
        heading: format!("{}{} {}", kind.sigil(), item.iid, item.title),
        sentences,
        description: item.description.clone(),
        history,
        comments,
    })
}

/// Appends a few segments after `…/projects/:id`. The `/` in the project path is already encoded as `%2F` in the `project` segment
fn at(project: &reqwest::Url, segments: &[&str]) -> anyhow::Result<reqwest::Url> {
    let mut url = project.clone();
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("API base cannot take a path"))?
        .extend(segments);
    Ok(url)
}

/// Read to the last page. Details must not overwrite a complete body that already exists with truncated notes or events (0072)
async fn fetch_all<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    base: &reqwest::Url,
    auth: Option<&str>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    let mut page = 1;
    loop {
        let mut url = base.clone();
        url.query_pairs_mut()
            .append_pair("per_page", &PER_PAGE.to_string())
            .append_pair("page", &page.to_string());
        let batch: Vec<T> =
            crate::ingest_sources::gitlab_checked(http, reqwest::Method::GET, url, auth)
                .await?
                .json()
                .await?;
        let n = batch.len();
        out.extend(batch);
        if n < PER_PAGE as usize {
            return Ok(out);
        }
        page += 1;
    }
}

pub async fn fetch_details(
    http: &reqwest::Client,
    project: &reqwest::Url,
    kind: Kind,
    iid: i64,
    auth: Option<&str>,
) -> anyhow::Result<Details> {
    let iid = iid.to_string();
    let of = |what: &str| at(project, &[kind.path(), &iid, what]);
    Ok(Details {
        notes: fetch_all(http, &of("notes")?, auth).await?,
        resource_state_events: fetch_all(http, &of("resource_state_events")?, auth).await?,
        resource_label_events: fetch_all(http, &of("resource_label_events")?, auth).await?,
        resource_milestone_events: fetch_all(http, &of("resource_milestone_events")?, auth).await?,
    })
}

pub struct Window {
    pub tickets: Vec<(Kind, Item)>,
    pub checkpoint: Option<DateTime<Utc>>,
}

struct Pass {
    items: Vec<Item>,
    /// Every row this pass lists (including skipped old rows); re-reading uses them to check whether anything moved
    positions: BTreeMap<i64, DateTime<Utc>>,
    checkpoint: Option<DateTime<Utc>>,
    /// Stopped at the budget, with time groups still unread after it
    more: bool,
}

/// Chooses the issues to write this round, and where the checkpoint should land after reading.
///
/// Each list is read per 0072: ascending, skipping those at or before the checkpoint, and reading the current time group to its end even when the budget runs out.
/// **The two lists share one checkpoint**, so it can only land at a moment both have been read through: if a list stopped at the budget,
/// take the earliest such stop point; issues later in the other list are not written this round and are left for the next round. Only when both are read to the end
/// is the latest one taken. Otherwise issues not read from the list that stopped at the budget would be jumped over by the other list's moment, and later
/// incremental reads would never see them.
///
/// Then each list is re-read up to this boundary, to check whether any issue moved position during paging (the 2026-10-23 revision of 0072)
pub async fn fetch_window(
    http: &reqwest::Client,
    project: &reqwest::Url,
    kinds: &[Kind],
    since: Option<DateTime<Utc>>,
    item_limit: usize,
    auth: Option<&str>,
) -> anyhow::Result<Window> {
    // The budget is for the whole round, split in half between the two lists; without splitting, turning on merge requests would read twice as many issues per round
    let item_limit = item_limit.div_ceil(kinds.len().max(1));
    let mut passes = Vec::new();
    for kind in kinds {
        let list = at(project, &[kind.path()])?;
        passes.push(fetch_pass(http, &list, since, item_limit, auth, None).await?);
    }
    let boundary = passes
        .iter()
        .filter(|p| p.more)
        .map(|p| p.checkpoint)
        .min()
        .unwrap_or_else(|| passes.iter().map(|p| p.checkpoint).max().flatten());
    let within = |t: DateTime<Utc>| boundary.is_some_and(|b| t <= b);

    let mut tickets = Vec::new();
    for (kind, pass) in kinds.iter().zip(passes) {
        if boundary != since {
            let list = at(project, &[kind.path()])?;
            let verified = fetch_pass(http, &list, since, item_limit, auth, boundary).await?;
            anyhow::ensure!(
                pass.positions
                    .iter()
                    .filter(|(_, t)| within(**t))
                    .eq(verified.positions.iter()),
                "GitLab {} changed during pagination; retry the previous checkpoint",
                kind.path()
            );
        }
        tickets.extend(
            pass.items
                .into_iter()
                .filter(|item| within(item.updated_at) && !item.confidential)
                .map(|item| (*kind, item)),
        );
    }
    Ok(Window {
        tickets,
        checkpoint: boundary,
    })
}

/// Reads one list. If `through` is given, this is a re-read: the budget is ignored, and it reads up to that moment
async fn fetch_pass(
    http: &reqwest::Client,
    list: &reqwest::Url,
    since: Option<DateTime<Utc>>,
    item_limit: usize,
    auth: Option<&str>,
    through: Option<DateTime<Utc>>,
) -> anyhow::Result<Pass> {
    let mut pass = Pass {
        items: Vec::new(),
        positions: BTreeMap::new(),
        checkpoint: since,
        more: false,
    };
    let mut previous_time = None;
    let mut stop_after = through;
    let mut listing_pages = 0;
    let mut page = 1;
    loop {
        let mut url = list.clone();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("state", "all")
                .append_pair("order_by", "updated_at")
                .append_pair("sort", "asc");
            if let Some(since) = since {
                q.append_pair("updated_after", &since.to_rfc3339());
            }
            q.append_pair("per_page", &PER_PAGE.to_string())
                .append_pair("page", &page.to_string());
        }
        let batch: Vec<Item> =
            crate::ingest_sources::gitlab_checked(http, reqwest::Method::GET, url, auth)
                .await?
                .json()
                .await?;
        let count = batch.len();
        let mut page_counted = false;
        for item in batch {
            let updated = item.updated_at;
            anyhow::ensure!(
                previous_time.is_none_or(|previous| updated >= previous),
                "GitLab tickets are not ordered by update time; retry the previous checkpoint"
            );
            previous_time = Some(updated);
            if stop_after.is_some_and(|boundary| updated > boundary) {
                pass.more = true;
                return Ok(pass);
            }
            anyhow::ensure!(
                pass.positions.insert(item.iid, updated).is_none(),
                "GitLab ticket repeated during pagination; retry the previous checkpoint"
            );
            // `updated_after` includes the boundary (checked on gitlab.com on 2026-11-14), so items already read come back once more
            if since.is_some_and(|checkpoint| updated <= checkpoint) {
                continue;
            }
            if !page_counted {
                listing_pages += 1;
                page_counted = true;
            }
            pass.checkpoint = Some(updated);
            pass.items.push(item);
            if through.is_none()
                && (pass.items.len() >= item_limit || listing_pages >= MAX_LISTING_PAGES)
            {
                stop_after = Some(updated);
            }
        }
        if count < PER_PAGE as usize {
            return Ok(pass);
        }
        page += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};

    #[derive(Deserialize)]
    struct Fixture {
        issues: Vec<Item>,
        merge_requests: Vec<Item>,
        details: std::collections::HashMap<String, Details>,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../tests/fixtures/gitlab_issues.json")).unwrap()
    }

    #[test]
    fn a_recorded_issue_tells_its_history_from_events_and_its_comments_from_people() {
        let f = fixture();
        let issue = f.issues.iter().find(|i| i.iid == 1).unwrap();
        let text = render(Kind::Issue, issue, &f.details["issues/1"]);
        assert!(text.starts_with(
            "# #1 A test\n\nOpened by quintinraptorbotes on 2026-10-22T13:50:23Z.\nCurrently opened.\nLabelled bug.\n"
        ));
        // Milliseconds truncated to seconds; events sorted by moment, three kinds of event on one timeline
        let history: Vec<&str> = text
            .split("## History\n\n")
            .nth(1)
            .unwrap()
            .split("\n\n")
            .next()
            .unwrap()
            .lines()
            .collect();
        assert_eq!(
            history,
            [
                "- 2026-10-22T13:51:28Z — closed by quintinraptorbotes",
                "- 2026-10-22T13:51:31Z — reopened by quintinraptorbotes",
                "- 2026-11-14T21:36:03Z — labeled by quintinraptorbotes (bug)",
                "- 2026-11-14T21:36:03Z — labeled by quintinraptorbotes (docs)",
                "- 2026-11-14T21:36:05Z — unlabeled by quintinraptorbotes (docs)",
                "- 2026-11-14T21:36:05Z — milestoned by quintinraptorbotes (v1)",
                "- 2026-11-14T21:36:06Z — closed by quintinraptorbotes",
                "- 2026-11-14T21:36:07Z — reopened by quintinraptorbotes",
            ]
        );
        assert!(text.contains(
            "### quintinraptorbotes on 2026-11-14T21:36:07Z\n\nStill happens on the nightly build, reopening.\n"
        ));
    }

    #[test]
    fn a_merged_request_says_when_it_merged_and_leaves_system_notes_out() {
        let f = fixture();
        let merged = f.merge_requests.iter().find(|m| m.iid == 1).unwrap();
        let details = &f.details["merge_requests/1"];
        assert!(
            details.notes.iter().any(|n| n.system),
            "the recording should hold a system note, or this test proves nothing"
        );
        let text = render(Kind::MergeRequest, merged, details);
        assert!(text.starts_with("# !1 Add the merged fixture file\n"));
        assert!(text.contains("Currently merged.\nMerged on 2026-11-14T21:36:15Z.\n"));
        assert!(text.contains("— merged by quintinraptorbotes\n"));
        assert!(text.contains("Looks right. Merging."));
        assert!(!text.contains("mentioned in commit"));

        let closed = f.merge_requests.iter().find(|m| m.iid == 2).unwrap();
        let text = render(Kind::MergeRequest, closed, &f.details["merge_requests/2"]);
        assert!(text.contains("Currently closed.\nClosed on 2026-11-14T21:36:19Z.\n"));
        assert!(!text.contains("Merged on"));
    }

    fn row(iid: i64, updated: &str) -> Value {
        json!({
            "iid": iid, "title": format!("Ticket {iid}"), "state": "opened", "description": null,
            "created_at": "2026-01-01T00:00:00Z", "updated_at": updated, "closed_at": null,
        })
    }

    /// The two lists each give a set of rows, paged by `page`
    async fn server(issues: Vec<Value>, merge_requests: Vec<Value>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(move |request: &Request| {
                let rows = if request.url.path().ends_with("/merge_requests") {
                    &merge_requests
                } else {
                    &issues
                };
                let page: usize = request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == "page")
                    .unwrap()
                    .1
                    .parse()
                    .unwrap();
                let rows: Vec<_> = rows.iter().skip((page - 1) * 100).take(100).collect();
                ResponseTemplate::new(200).set_body_json(rows)
            })
            .mount(&server)
            .await;
        server
    }

    async fn window(
        server: &MockServer,
        since: Option<&str>,
        item_limit: usize,
    ) -> anyhow::Result<Window> {
        let http = reqwest::Client::builder().no_proxy().build()?;
        let project = reqwest::Url::parse(&format!("{}/projects/g%2Fp", server.uri()))?;
        fetch_window(
            &http,
            &project,
            &[Kind::Issue, Kind::MergeRequest],
            since.map(|s| s.parse().unwrap()),
            item_limit,
            None,
        )
        .await
    }

    fn listed(window: &Window) -> Vec<String> {
        window
            .tickets
            .iter()
            .map(|(kind, item)| format!("{}{}", kind.sigil(), item.iid))
            .collect()
    }

    #[tokio::test]
    async fn a_list_that_stops_at_its_budget_holds_the_checkpoint_back_for_both_lists(
    ) -> anyhow::Result<()> {
        // The round budget is four entries, two per list. Issues use up the budget in the 01:00 group, and the 02:00 ones are left for the next round.
        // The merge requests have only two entries, read to the end, but the 03:00 entry is after the issue's stop point
        let server = server(
            vec![
                row(1, "2026-01-02T01:00:00Z"),
                row(2, "2026-01-02T01:00:00Z"),
                row(3, "2026-01-02T02:00:00Z"),
            ],
            vec![
                row(1, "2026-01-02T00:30:00Z"),
                row(2, "2026-01-02T03:00:00Z"),
            ],
        )
        .await;
        let first = window(&server, None, 4).await?;
        assert_eq!(listed(&first), ["#1", "#2", "!1"]);
        assert_eq!(
            first.checkpoint,
            Some("2026-01-02T01:00:00Z".parse().unwrap())
        );

        // The next round starts from that stop point: the three already-read entries that come back are not written again, and both remaining entries are read; the checkpoint goes to the latest
        let second = window(&server, Some("2026-01-02T01:00:00Z"), 200).await?;
        assert_eq!(listed(&second), ["#3", "!2"]);
        assert_eq!(
            second.checkpoint,
            Some("2026-01-02T03:00:00Z".parse().unwrap())
        );

        // Nothing new: the checkpoint stays where it is, and does not advance to the wall clock
        let third = window(&server, Some("2026-01-02T03:00:00Z"), 200).await?;
        assert!(third.tickets.is_empty());
        assert_eq!(third.checkpoint, second.checkpoint);
        Ok(())
    }

    #[tokio::test]
    async fn a_confidential_issue_is_not_written_but_the_checkpoint_moves_past_it(
    ) -> anyhow::Result<()> {
        let mut secret = row(2, "2026-01-02T02:00:00Z");
        secret["confidential"] = json!(true);
        let server = server(
            vec![row(1, "2026-01-02T01:00:00Z"), secret],
            vec![row(1, "2026-01-02T00:30:00Z")],
        )
        .await;
        let w = window(&server, None, 200).await?;
        assert_eq!(listed(&w), ["#1", "!1"]);
        assert_eq!(w.checkpoint, Some("2026-01-02T02:00:00Z".parse().unwrap()));
        Ok(())
    }

    #[tokio::test]
    async fn a_ticket_that_moves_between_the_two_readings_fails_the_window() -> anyhow::Result<()> {
        // The first pass read #1 at 01:00; on re-read it has already been changed to after 01:00, so one row inside the boundary is missing
        let server = MockServer::start().await;
        let calls = std::sync::atomic::AtomicUsize::new(0);
        Mock::given(method("GET"))
            .respond_with(move |request: &Request| {
                if request.url.path().ends_with("/merge_requests") {
                    return ResponseTemplate::new(200).set_body_json(json!([]));
                }
                let moved = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0;
                let first = if moved {
                    row(1, "2026-01-02T05:00:00Z")
                } else {
                    row(1, "2026-01-02T01:00:00Z")
                };
                let mut rows = vec![row(2, "2026-01-02T01:00:00Z"), first];
                rows.sort_by_key(|r| r["updated_at"].as_str().unwrap().to_owned());
                ResponseTemplate::new(200).set_body_json(rows)
            })
            .mount(&server)
            .await;
        let error = window(&server, None, 200).await.err().unwrap().to_string();
        assert!(error.contains("changed during pagination"), "{error}");
        Ok(())
    }
}
