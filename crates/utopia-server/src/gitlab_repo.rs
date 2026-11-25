//! GitLab project file source (gitlab.com): a file tree at the default branch head, one document per path, with the same shape as
//! [`crate::github_repo`] (#1327). Sync runs in `ingest_sources`; this file only holds the half that makes no network calls.
//!
//! Three differences from GitHub: the tree is paginated (keyset, at most 100 entries per page, no `truncated`); tree entries
//! have a blob SHA but no size, so the size cap can only be checked once the `HEAD` file gets `X-Gitlab-Size`;
//! commit times carry milliseconds, which are truncated to seconds before storing (0013).

use chrono::{DateTime, SubsecRound as _, Utc};
use serde::Deserialize;
use std::collections::HashSet;

use crate::github_repo::FileFilter;

/// A tree is paged through at most this many times (100 entries per page). If paging does not finish, the tree counts as truncated: this round still takes the files listed,
/// but does not judge "not in source", since absence may only mean not listed yet, handled the same way as GitHub's `truncated`
pub const MAX_TREE_PAGES: usize = 100;

/// The git blob SHA of an empty file is fixed. The tree carries no size, so use it to drop empty files before asking for sizes; without this filter,
/// empty files are never stored, and every round would waste a `HEAD` request on them
pub const EMPTY_BLOB: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

#[derive(Debug, Clone, Deserialize)]
pub struct TreeEntry {
    /// git blob SHA, the same value as `sha` in a GitHub tree
    pub id: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub mode: String,
}

/// Sorted by path, keeping only the first entry per blob, consistent with [`crate::github_repo::choose`]. Size is not known yet at this point;
/// sync checks it after `HEAD`
pub fn choose(entries: Vec<TreeEntry>, filter: &FileFilter) -> Vec<TreeEntry> {
    let mut entries: Vec<_> = entries
        .into_iter()
        .filter(|e| e.id != EMPTY_BLOB && filter.path_ok(&e.path, &e.kind, &e.mode))
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut seen = HashSet::new();
    entries.retain(|e| seen.insert(e.id.clone()));
    entries
}

/// The page given by `rel="next"` in the `Link` header. Accepts only addresses from the same origin as the API: the address comes from the response,
/// and accepting it blindly would let the other side decide where we send our token
pub fn next_page(link: Option<&str>, api_base: &reqwest::Url) -> Option<reqwest::Url> {
    link?.split(',').find_map(|part| {
        let (target, params) = part.split_once(';')?;
        if !params.split(';').any(|p| p.trim() == r#"rel="next""#) {
            return None;
        }
        let url = reqwest::Url::parse(target.trim().strip_prefix('<')?.strip_suffix('>')?).ok()?;
        (url.origin() == api_base.origin() && url.path().starts_with(api_base.path()))
            .then_some(url)
    })
}

/// `committed_date` looks like `2026-10-05T14:30:00.000+00:00`, truncated to seconds
pub fn commit_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc).trunc_subsecs(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: &str, mode: &str, id: &str) -> TreeEntry {
        TreeEntry {
            id: id.into(),
            path: path.into(),
            kind: kind.into(),
            mode: mode.into(),
        }
    }

    #[test]
    fn the_tree_keeps_markdown_files_once_and_leaves_links_and_folders_out() {
        let f = FileFilter::from_config(&serde_json::json!({})).unwrap();
        let chosen = choose(
            vec![
                entry("docs/guide.md", "blob", "100644", "9292"),
                entry("docs/copy-of-guide.md", "blob", "100644", "9292"),
                entry("docs", "tree", "040000", "5964"),
                entry("link.md", "blob", "120000", "eafc"),
                entry(".gitlab/notes.md", "blob", "100644", "3e42"),
                entry("node_modules/pkg/README.md", "blob", "100644", "08e8"),
                entry("src/main.rs", "blob", "100644", "f328"),
                entry("README.md", "blob", "100644", "9e4e"),
                entry("notes/empty.md", "blob", "100644", EMPTY_BLOB),
            ],
            &f,
        );
        let paths: Vec<_> = chosen.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["README.md", "docs/copy-of-guide.md"]);
    }

    #[test]
    fn only_a_next_page_on_the_same_api_is_followed() {
        let base = reqwest::Url::parse("https://gitlab.com/api/v4").unwrap();
        let link = r#"<https://gitlab.com/api/v4/projects/a%2Fb/repository/tree?page_token=x&pagination=keyset>; rel="next""#;
        assert_eq!(
            next_page(Some(link), &base).map(|u| u
                .query_pairs()
                .find(|(k, _)| k == "page_token")
                .unwrap()
                .1
                .into_owned()),
            Some("x".into())
        );
        assert!(next_page(
            Some(r#"<https://evil.example/api/v4/x>; rel="next""#),
            &base
        )
        .is_none());
        assert!(next_page(Some(r#"<https://gitlab.com/other>; rel="next""#), &base).is_none());
        assert!(next_page(Some(r#"<https://gitlab.com/api/v4/x>; rel="first""#), &base).is_none());
        assert!(next_page(None, &base).is_none());
    }

    #[test]
    fn a_commit_time_is_cut_to_the_second() {
        let t = commit_time("2026-10-05T14:30:00.987+02:00").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-10-05T12:30:00+00:00");
        assert!(commit_time("yesterday").is_none());
    }
}
