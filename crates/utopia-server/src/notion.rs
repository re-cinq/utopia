//! Notion source: syncs in the pages the integration can see.
//!
//! By the four criteria of [0013](../../docs/decisions/0013-a-source-should-hand-over-its-history.md),
//! it is one notch stronger than object storage:
//!
//! | Criterion | Notion |
//! |---|---|
//! | Real timestamp | `last_edited_time`, **the document's own edit time**, not the time we fetched it |
//! | Whether it overturns itself | Pages being rewritten again and again is its normal state |
//! | Stable identity | Page UUID, unchanged by renaming or moving |
//! | Whether enterprise knowledge lives there | Policies, meeting minutes, decision records: exactly what this system is for |
//!
//! **But it hands over only the present state, not the history.** Notion's version history is not in the public API,
//! so unlike the ticket systems, one sync can only see the present, and earlier edits accumulate only through repeated syncs.
//! This has the same shape as `url` / `rss`, whereas the two ticket sources can fetch the whole change history at once.
//!
//! ## Two pitfalls to avoid
//!
//! **The `Notion-Version` header is required**, and its value is a date. Without it the API returns 400 outright,
//! and the error only says "missing version", without telling you which one to fill in.
//!
//! **The rate limit is three requests per second** (the official wording is "three on average"). Sending requests in sequence is not enough: a 100-millisecond
//! response followed right away by the next request is ten per second. The re-test in #215 ran for three minutes on a workspace of several hundred pages,
//! then search returned 429 and the whole sync failed. So now two things are done together:
//! a minimum gap between requests ([`MIN_INTERVAL`]), and on a 429 waiting out `Retry-After` before sending again
//! ([`Paced::send`]). Still no concurrency. This backoff is local and small, and is not borrowed from the extraction path,
//! whose backoff is designed for model vendors, with different criteria and rhythm.
//!
//! Error messages are in English: they land verbatim in the source's sync status, shown to the user.

use std::time::{Duration, Instant};

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use reqwest::StatusCode;

/// The API version in the request header. **Hard-coded rather than left to config**: the response shape changes with it,
/// and letting a user fill in a version we have not adapted to would give silent parsing mismatches.
const NOTION_VERSION: &str = "2026-03-11";

/// The maximum number of pages fetched per sync. The reason is the same as for other sources: ingestion is irreversible, and a workspace
/// can have tens of thousands of pages.
const MAX_PAGES_PER_SYNC: usize = 500;

/// The maximum number of blocks fetched per page across all levels, including text-less containers.
/// A page deeper than this is truncated, which is better than letting one sync hang on one page.
const MAX_BLOCKS_PER_PAGE: usize = 500;

/// The minimum gap between two requests. Notion says "three per second on average"; 350 ms leaves a little margin:
/// total time also depends on each page's nesting depth and page count.
const MIN_INTERVAL: Duration = Duration::from_millis(350);
/// The most times a single request is retried after a 429. `Retry-After` is usually a single-digit number of seconds; still being rate limited after waiting several times
/// is no longer a pacing problem, so the error should be surfaced.
const MAX_RATE_LIMIT_RETRIES: u32 = 5;
/// How long to wait when `Retry-After` is missing or unreadable.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);
/// Upper bound on a single wait. We honour the value in the header, but an absurd value must not hang the sync.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(30);

/// A page waiting to be ingested.
pub struct NotionPage {
    /// `notion://{page_id}`: the page UUID is its most stable identity
    pub external_key: String,
    pub filename: String,
    pub text: String,
    pub last_edited: Option<DateTime<Utc>>,
}

fn client(token: &str) -> anyhow::Result<reqwest::Client> {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert("Notion-Version", NOTION_VERSION.parse()?);
    h.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {token}").parse()?,
    );
    Ok(reqwest::Client::builder()
        .default_headers(h)
        .timeout(Duration::from_secs(60))
        .build()?)
}

/// A client that sends requests at Notion's pace: a minimum gap between requests, and on a 429 it waits per `Retry-After`.
struct Paced {
    http: reqwest::Client,
    last: Option<Instant>,
    // The private-address entry point lets HTTP tests go through real page fetching and the rate-limit logic, without adding user config.
    api_root: String,
}

impl Paced {
    fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            last: None,
            api_root: "https://api.notion.com/v1".into(),
        }
    }

    /// Sends one request and parses the JSON. `what` goes into logs and error messages ("search" / "blocks").
    ///
    /// A 429 is absorbed here: wait for `Retry-After` and send again, up to [`MAX_RATE_LIMIT_RETRIES`] times;
    /// other non-2xx errors fail immediately, carrying Notion's own message, which is clearer than the status code.
    async fn send(
        &mut self,
        what: &str,
        build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    ) -> anyhow::Result<serde_json::Value> {
        let mut attempt = 0u32;
        loop {
            if let Some(last) = self.last {
                let since = last.elapsed();
                if since < MIN_INTERVAL {
                    tokio::time::sleep(MIN_INTERVAL - since).await;
                }
            }
            let resp = build(&self.http)
                .send()
                .await
                .with_context(|| format!("notion {what}"))?;
            self.last = Some(Instant::now());

            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS && attempt < MAX_RATE_LIMIT_RETRIES {
                let wait = retry_after(
                    resp.headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|h| h.to_str().ok()),
                );
                attempt += 1;
                tracing::info!(
                    what,
                    attempt,
                    wait_ms = wait.as_millis() as u64,
                    "notion rate limited, waiting before the next attempt"
                );
                tokio::time::sleep(wait).await;
                continue;
            }

            let v: serde_json::Value = resp
                .json()
                .await
                .with_context(|| format!("notion {what}: reading the response"))?;
            if !status.is_success() {
                anyhow::bail!(
                    "notion {what} returned {status}: {}",
                    v["message"].as_str().unwrap_or("no message")
                );
            }
            return Ok(v);
        }
    }
}

/// The `Retry-After` header → how long to wait. Notion gives seconds (possibly fractional); if missing, unreadable,
/// or negative, the default applies, and absurd values are capped at the upper bound. A pure function; the unit tests below pin down these cases.
fn retry_after(header: Option<&str>) -> Duration {
    header
        .and_then(|h| h.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(DEFAULT_RETRY_AFTER)
        .min(RETRY_AFTER_CAP)
}

/// Fetches all pages the integration can see.
///
/// **Searches pages only, not data sources.** A data source is the container of a table and has no body of its own;
/// each row of a table is a page, and it shows up in the same search.
///
/// The third item is pages whose body could not be read, with one reason per page. They are not in the first item; see `fetch_pages`.
pub async fn fetch(
    token: &str,
    query: Option<&str>,
) -> anyhow::Result<(Vec<NotionPage>, bool, Vec<String>)> {
    let mut http = Paced::new(client(token)?);
    fetch_pages(&mut http, query).await
}

async fn fetch_pages(
    http: &mut Paced,
    query: Option<&str>,
) -> anyhow::Result<(Vec<NotionPage>, bool, Vec<String>)> {
    let mut out = Vec::new();
    let mut unread: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut truncated = false;

    loop {
        let mut body = serde_json::json!({
            "filter": { "property": "object", "value": "page" },
            "page_size": 100,
        });
        if let Some(q) = query {
            body["query"] = serde_json::Value::String(q.to_string());
        }
        if let Some(c) = &cursor {
            body["start_cursor"] = serde_json::Value::String(c.clone());
        }

        let url = format!("{}/search", http.api_root);
        let v = http.send("search", |c| c.post(&url).json(&body)).await?;

        for p in v["results"].as_array().unwrap_or(&vec![]).clone() {
            // Items in the trash and archived ones are not wanted either: they no longer count on screen
            if p["in_trash"].as_bool() == Some(true) || p["is_archived"].as_bool() == Some(true) {
                continue;
            }
            if out.len() >= MAX_PAGES_PER_SYNC {
                truncated = true;
                break;
            }
            let Some(id) = p["id"].as_str() else { continue };
            let title = page_title(&p);
            // Pages whose body cannot be read are skipped whole: not ingested, and the body already in the store is left as is rather than replaced with a title-only
            // version. **Other pages still sync as usual**: aborting the whole round is the opposite error: some pages can never be read
            // (nested blocks not shared with the integration; Notion returns 404), and then the whole source would never move again.
            // The object-storage directory placeholder is the same lesson (#214).
            let text = match page_text(http, id).await {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(%id, error = %format!("{error:#}"), "notion page body could not be read, leaving the stored page as it is");
                    unread.push(format!("notion page {id}: {error:#}"));
                    continue;
                }
            };

            out.push(NotionPage {
                external_key: format!("notion://{id}"),
                filename: format!("{}.md", slug(&title)),
                text: format!("# {title}\n\n{text}"),
                last_edited: p["last_edited_time"]
                    .as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc)),
            });
        }

        if truncated || v["has_more"].as_bool() != Some(true) {
            break;
        }
        cursor = v["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    Ok((out, truncated, unread))
}

/// Page title.
///
/// **The title sits under the `properties` entry whose `type == "title"`, and its name is not fixed**:
/// a page in a database may be called `Name`, `名称` (name), or `任务` (task), while plain pages are called `title`.
/// Looking it up by name fails on someone else's workspace, so look it up by type.
fn page_title(page: &serde_json::Value) -> String {
    let props = page["properties"].as_object();
    let t = props.and_then(|m| {
        m.values()
            .find(|v| v["type"] == "title")
            .and_then(|v| v["title"].as_array())
    });
    let s = t
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r["plain_text"].as_str())
                .collect::<String>()
        })
        .unwrap_or_default();
    if s.trim().is_empty() {
        "untitled".into()
    } else {
        s
    }
}

/// Fetches a page's body, expanding blocks level by level.
async fn page_text(http: &mut Paced, page_id: &str) -> anyhow::Result<String> {
    let mut out = String::new();
    let mut block_count = 0;
    append_children(http, page_id, &mut out, &mut block_count).await?;
    Ok(out)
}

/// Each parent block is paged independently: write the parent first, then read its whole subtree, then continue with sibling blocks.
/// Output and budget are shared by the whole page; recursion depth is also bound by the same block cap.
async fn append_children(
    http: &mut Paced,
    parent_id: &str,
    out: &mut String,
    block_count: &mut usize,
) -> anyhow::Result<()> {
    let mut cursor: Option<String> = None;

    while *block_count < MAX_BLOCKS_PER_PAGE {
        let mut url = format!(
            "{}/blocks/{parent_id}/children?page_size=100",
            http.api_root
        );
        if let Some(c) = &cursor {
            url.push_str(&format!("&start_cursor={c}"));
        }
        let v = http.send("blocks", |c| c.get(&url)).await?;

        for b in v["results"].as_array().unwrap_or(&vec![]) {
            if *block_count >= MAX_BLOCKS_PER_PAGE {
                return Ok(());
            }
            *block_count += 1;
            if let Some(line) = render_block(b) {
                out.push_str(&line);
                out.push('\n');
            }
            // Do not skip containers such as column just because the parent has no rich_text.
            // Standalone pages are ingested separately by search, and databases are not part of the parent page's body.
            let independent_content =
                matches!(b["type"].as_str(), Some("child_page" | "child_database"));
            if *block_count < MAX_BLOCKS_PER_PAGE
                && b["has_children"].as_bool() == Some(true)
                && !independent_content
            {
                let id = b["id"]
                    .as_str()
                    .context("notion blocks: child block is missing id")?;
                if b["type"].as_str() == Some("table") {
                    let mut rows = String::new();
                    Box::pin(append_children(http, id, &mut rows, block_count)).await?;
                    out.push_str(&render_table(&b["table"], &rows)?);
                } else {
                    Box::pin(append_children(http, id, out, block_count)).await?;
                }
            }
        }
        if v["has_more"].as_bool() != Some(true) {
            break;
        }
        cursor = v["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    Ok(())
}

/// Renders one block as a line of text.
///
/// **Unrecognized block types return their plain text rather than being dropped.** Notion keeps adding block types,
/// and a hard-coded whitelist would make new types silently disappear; ordinary text lives in `{type}.rich_text`,
/// and table rows keep each cell via `table_row.cells`.
fn render_block(b: &serde_json::Value) -> Option<String> {
    let t = b["type"].as_str()?;
    let inner = &b[t];
    let text = rich_text(&inner["rich_text"]);

    Some(match t {
        "heading_1" => format!("## {text}"),
        "heading_2" => format!("### {text}"),
        "heading_3" => format!("#### {text}"),
        "bulleted_list_item" => format!("- {text}"),
        "numbered_list_item" => format!("1. {text}"),
        "to_do" => {
            let done = inner["checked"].as_bool() == Some(true);
            format!("- [{}] {text}", if done { "x" } else { " " })
        }
        "quote" => format!("> {text}"),
        "code" => {
            let lang = inner["language"].as_str().unwrap_or("");
            format!("```{lang}\n{text}\n```")
        }
        "table_row" => {
            let cells = inner["cells"].as_array()?;
            let cells: Vec<String> = cells
                .iter()
                .map(|cell| {
                    rich_text(cell)
                        .replace('\\', "\\\\")
                        .replace('|', "\\|")
                        .replace("\r\n", "\n")
                        .replace(['\r', '\n'], "<br>")
                })
                .collect();
            format!("| {} |", cells.join(" | "))
        }
        // Dividers and images have no rich_text, but they carry no information in the body either
        "divider" | "image" | "video" | "file" => return None,
        // child_page's title is in `title`, not in rich_text
        "child_page" => format!("- {}", inner["title"].as_str().unwrap_or("")),
        _ if text.trim().is_empty() => return None,
        _ => text,
    })
}

/// A table with no column header uses an empty header as a placeholder, so the first data row is not mislabeled as the header.
fn render_table(table: &serde_json::Value, rows: &str) -> anyhow::Result<String> {
    if rows.is_empty() {
        return Ok(String::new());
    }
    let width = table["table_width"]
        .as_u64()
        .context("notion table: missing column count")?;
    let width = usize::try_from(width)?;
    anyhow::ensure!(width > 0, "notion table: column count must be positive");
    let has_header = table["has_column_header"].as_bool() == Some(true);
    let separator = format!("|{}\n", " --- |".repeat(width));
    let mut out = String::from("\n");
    if !has_header {
        out.push_str(&format!("|{}\n", "  |".repeat(width)));
        out.push_str(&separator);
    }
    for (index, row) in rows.lines().enumerate() {
        out.push_str(row);
        out.push('\n');
        if index == 0 && has_header {
            out.push_str(&separator);
        }
    }
    out.push('\n');
    Ok(out)
}

/// Joins a rich_text array into plain text.
fn rich_text(v: &serde_json::Value) -> String {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r["plain_text"].as_str())
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// Turns a title into something usable as a file name.
fn slug(title: &str) -> String {
    let s: String = title
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "untitled".into()
    } else {
        s.chars().take(60).collect()
    }
}

#[cfg(test)]
#[path = "notion_tests.rs"]
mod traversal_tests;
#[cfg(test)]
mod tests {
    use super::*;

    /// **The title property's name is arbitrary.** A database page may call it `Name`, `名称`,
    /// or `任务`; looking it up by name returns untitled on someone else's workspace, which looks
    /// like "the page has no title" rather than "we looked in the wrong place".
    #[test]
    fn a_title_is_found_by_type_not_by_name() {
        for key in ["title", "Name", "名称", "任务"] {
            let page = serde_json::json!({
                "properties": {
                    key: { "type": "title", "title": [{ "plain_text": "季度复盘" }] },
                    "Status": { "type": "select", "select": { "name": "Done" } }
                }
            });
            assert_eq!(
                page_title(&page),
                "季度复盘",
                "no title found for property name {key}"
            );
        }
    }

    /// Rich text is split into segments: bold and links cut a sentence apart. Failing to join them fully loses text.
    #[test]
    fn rich_text_segments_join_back_into_one_line() {
        let v = serde_json::json!([
            { "plain_text": "把总部搬到" },
            { "plain_text": "深圳" },
            { "plain_text": "了" }
        ]);
        assert_eq!(rich_text(&v), "把总部搬到深圳了");
    }

    /// **Unrecognized block types must not be dropped.** Notion keeps adding types, and blocks that carry text
    /// all keep the text under `{type}.rich_text`; rendering through a whitelist would make new types
    /// silently disappear.
    #[test]
    fn an_unknown_block_keeps_its_text() {
        let b = serde_json::json!({
            "type": "some_new_block_type_2027",
            "some_new_block_type_2027": { "rich_text": [{ "plain_text": "还是有内容的" }] }
        });
        assert_eq!(render_block(&b).as_deref(), Some("还是有内容的"));
    }

    /// Decorative blocks with no text should disappear, or the body would be full of blank lines.
    #[test]
    fn a_divider_renders_to_nothing() {
        let b = serde_json::json!({ "type": "divider", "divider": {} });
        assert!(render_block(&b).is_none());
    }

    /// File names cannot contain path separators or newlines.
    #[test]
    fn a_slug_is_safe_as_a_filename() {
        assert_eq!(slug("2026 Q3 / 复盘"), "2026-Q3---复盘");
        assert_eq!(slug("///"), "untitled");
        assert!(slug(&"x".repeat(200)).chars().count() <= 60);
    }

    /// Connects to a real Notion workspace. **There is no emulator**: Notion is closed-source SaaS,
    /// and open-source stand-ins (AppFlowy, AFFiNE) do not speak this API. So this test is the only place where
    /// the `Retry-After` tiers are checked: seconds that Notion gives are honoured, missing and garbage values use the default, and absurd values are capped.
    #[test]
    fn retry_after_follows_the_header_within_bounds() {
        assert_eq!(retry_after(Some("3")), Duration::from_secs(3));
        assert_eq!(retry_after(Some(" 0.5 ")), Duration::from_millis(500));
        assert_eq!(retry_after(None), DEFAULT_RETRY_AFTER);
        assert_eq!(retry_after(Some("soon")), DEFAULT_RETRY_AFTER);
        assert_eq!(retry_after(Some("-2")), DEFAULT_RETRY_AFTER);
        assert_eq!(retry_after(Some("600")), RETRY_AFTER_CAP);
    }

    /// Runs only when someone provides a real token; always skipped on CI.
    ///
    /// ```text
    /// # Settings → My connections → New internal integration, then share a page with it
    /// UTOPIA_NOTION_TEST_TOKEN=ntn_xxx cargo test -p utopia-server notion
    /// ```
    #[tokio::test]
    async fn it_reads_from_a_real_workspace() -> anyhow::Result<()> {
        let Ok(token) = std::env::var("UTOPIA_NOTION_TEST_TOKEN") else {
            eprintln!("跳过：未设 UTOPIA_NOTION_TEST_TOKEN");
            return Ok(());
        };
        let (pages, _, _) = fetch(&token, None).await?;
        assert!(
            !pages.is_empty(),
            "一页都没有——integration 可能没有被分享任何页面"
        );
        let p = &pages[0];
        assert!(
            p.external_key.starts_with("notion://"),
            "{}",
            p.external_key
        );
        assert!(
            p.text.starts_with("# "),
            "body should start with the heading"
        );
        assert!(
            p.last_edited.is_some(),
            "last_edited_time 是 doc_time 的来源"
        );
        Ok(())
    }
}
