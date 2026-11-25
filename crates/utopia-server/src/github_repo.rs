//! GitHub repository file source: a file tree at the head of the default branch, one document per path.
//! Sync happens in `ingest_sources`; this file holds only the half that does not touch the network, so it can be unit-tested.

use serde::Deserialize;
use sha1::Digest as _;
use std::collections::HashSet;

#[derive(Debug, Clone, Deserialize)]
pub struct TreeEntry {
    pub path: String,
    pub mode: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub sha: String,
    pub size: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TreeResponse {
    pub tree: Vec<TreeEntry>,
    #[serde(default)]
    pub truncated: bool,
}

/// Which paths are collected and how large the files can be. Only markdown is collected, so the whole code repo is not dumped in as corpus.
pub struct FileFilter {
    include: Vec<glob::Pattern>,
    exclude: Vec<glob::Pattern>,
    max_file_bytes: i64,
}

impl FileFilter {
    pub fn from_config(config: &serde_json::Value) -> anyhow::Result<Self> {
        // If the config gives this column, replace the default entirely
        let compile = |key: &str, defaults: &[&str]| -> anyhow::Result<Vec<glob::Pattern>> {
            let list: Vec<&str> = match config[key].as_array() {
                Some(a) => a.iter().filter_map(|v| v.as_str()).collect(),
                None => defaults.to_vec(),
            };
            list.into_iter()
                .map(|p| {
                    glob::Pattern::new(p)
                        .map_err(|e| anyhow::anyhow!("config.{key} has an invalid glob {p:?}: {e}"))
                })
                .collect()
        };
        Ok(Self {
            include: compile("include", &["**/*.md", "**/*.markdown", "**/*.mdx"])?,
            exclude: compile(
                "exclude",
                &["**/.*/**", "**/node_modules/**", "**/vendor/**"],
            )?,
            max_file_bytes: config["max_file_bytes"].as_i64().unwrap_or(1 << 20),
        })
    }

    fn eligible(&self, e: &TreeEntry) -> bool {
        self.path_ok(&e.path, &e.kind, &e.mode) && e.size.is_some_and(|s| self.size_ok(s))
    }

    /// Only the half that can be judged from the tree entry itself. GitLab's tree carries no size (#1327), so the size is asked for separately,
    /// which is why the two halves are split
    pub(crate) fn path_ok(&self, path: &str, kind: &str, mode: &str) -> bool {
        let opts = glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        // Submodules are commits and directories are trees, and symlinks point elsewhere -- none of them are file bodies themselves
        kind == "blob"
            && mode != "120000"
            && self.include.iter().any(|p| p.matches_with(path, opts))
            && !self.exclude.iter().any(|p| p.matches_with(path, opts))
    }

    /// An empty file cannot become a document (`ingest_item` creates nothing for empty bytes), so admitting it would refetch it every round
    pub(crate) fn size_ok(&self, size: i64) -> bool {
        size > 0 && size <= self.max_file_bytes
    }
}

/// Sort by path, keep only the first entry per blob: when the same file appears at several paths, the document should not hop between paths.
pub fn choose(entries: Vec<TreeEntry>, filter: &FileFilter) -> Vec<TreeEntry> {
    let mut entries: Vec<_> = entries.into_iter().filter(|e| filter.eligible(e)).collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut seen = HashSet::new();
    entries.retain(|e| seen.insert(e.sha.clone()));
    entries
}

/// A blob's SHA-1 in git (feed `blob <byte count>\0` first, then the body). The tree gives this value;
/// computing it over the stored bytes shows it is unchanged, without downloading the file again.
pub fn git_blob_sha(bytes: &[u8]) -> String {
    let mut h = sha1::Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).as_bytes());
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_globs_take_markdown_outside_hidden_and_vendored_folders() {
        let f = FileFilter::from_config(&serde_json::json!({})).unwrap();
        let entry = |path: &str, mode: &str| TreeEntry {
            path: path.into(),
            mode: mode.into(),
            kind: "blob".into(),
            sha: "s".into(),
            size: Some(10),
        };
        for path in ["README.md", "docs/a/b.mdx"] {
            assert!(f.eligible(&entry(path, "100644")), "{path}");
        }
        for path in [
            ".github/PULL_REQUEST_TEMPLATE.md",
            "web/node_modules/x/README.md",
            "vendor/a.md",
            "Cargo.toml",
        ] {
            assert!(!f.eligible(&entry(path, "100644")), "{path}");
        }
        assert!(!f.eligible(&entry("link.md", "120000")));
    }

    #[test]
    fn an_empty_file_is_not_taken() {
        let f = FileFilter::from_config(&serde_json::json!({})).unwrap();
        let empty = TreeEntry {
            path: "notes/empty.md".into(),
            mode: "100644".into(),
            kind: "blob".into(),
            sha: "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".into(),
            size: Some(0),
        };
        assert!(!f.eligible(&empty));
    }
}
