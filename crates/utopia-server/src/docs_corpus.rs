//! Charter corpus: the same batch of markdown as the frontend Docs page (single source of truth, upgraded with the binary).
//! Adding an article = one line in the frontend Docs.tsx list + one line in ARTICLES here.

use utopia_search::{DocsIndex, DocsSection};

/// (slug, title, body). The slug must match the frontend DOCS list (so that reference links /docs/{slug} line up).
const ARTICLES: &[(&str, &str, &str)] = &[
    (
        "ingest",
        "Ingest interfaces",
        include_str!("../../../web/src/docs/ingest.md"),
    ),
    (
        "mcp",
        "Agents over MCP",
        include_str!("../../../web/src/docs/mcp.md"),
    ),
];

/// The index is built at startup; the corpus is a compile-time constant, so a failure is a program error: die loudly.
pub fn build_index() -> DocsIndex {
    DocsIndex::build(&sections()).expect("failed to build Charter document index")
}

/// Splits by h2 sections: one index record per section, and a hit returns the specific section rather than the whole article.
/// Intro text before the first h2 goes into the "title section" (anchor is empty, so the link lands at the top of the article).
fn sections() -> Vec<DocsSection> {
    let mut out = Vec::new();
    for (slug, title, body) in ARTICLES {
        let mut heading = (*title).to_string();
        let mut anchor = String::new();
        let mut buf: Vec<&str> = Vec::new();
        let mut flush = |heading: &str, anchor: &str, buf: &mut Vec<&str>| {
            let text = buf.join("\n").trim().to_string();
            if !text.is_empty() {
                out.push(DocsSection {
                    slug: (*slug).to_string(),
                    title: (*title).to_string(),
                    heading: heading.to_string(),
                    anchor: anchor.to_string(),
                    body: text,
                });
            }
            buf.clear();
        };
        for line in body.lines() {
            if let Some(h) = line.strip_prefix("## ") {
                flush(&heading, &anchor, &mut buf);
                // Clean up the same way as the frontend tocOf: remove inline code and emphasis markers
                heading = h.replace(['`', '*'], "").trim().to_string();
                anchor = slugify(&heading);
            } else if !line.starts_with("# ") {
                buf.push(line);
            }
        }
        flush(&heading, &anchor, &mut buf);
    }
    out
}

/// Matches the frontend Docs.tsx slugify character for character (anchor jumps depend on both sides agreeing):
/// lowercase, then runs of characters outside [a-z0-9一-龥] fold into a single '-', and leading and trailing '-' are removed.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = false;
    for c in s.to_lowercase().chars() {
        let keep =
            c.is_ascii_lowercase() || c.is_ascii_digit() || ('\u{4e00}'..='\u{9fa5}').contains(&c);
        if keep {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else {
            dash = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_matches_frontend() {
        assert_eq!(slugify("Choosing between them"), "choosing-between-them");
        assert_eq!(
            slugify("Custom source — the pull interface"),
            "custom-source-the-pull-interface"
        );
        assert_eq!(
            slugify("API source — the push interface"),
            "api-source-the-push-interface"
        );
        assert_eq!(slugify("共享语义 Shared"), "共享语义-shared");
    }

    #[test]
    fn ingest_splits_into_sections() {
        let secs = sections();
        assert!(
            secs.len() >= 4,
            "ingest.md should split into an intro plus 3 or more sections"
        );
        assert!(secs.iter().any(|s| s.anchor == "shared-semantics"));
        // Intro section: empty anchor, heading uses the article title
        assert!(secs
            .iter()
            .any(|s| s.anchor.is_empty() && s.heading == "Ingest interfaces"));
    }

    /// Every corpus file must be in `ARTICLES`, or chat's search_docs tool cannot find it.
    /// This line checks `web/src/docs/*.md` against the source of truth, to prevent drift where "the frontend shows it but chat cannot find it"
    /// (`mcp.md` already fell into this once).
    ///
    /// **Do not** write "check as many as there are in ARTICLES": that would make adding files stop raising errors. The
    /// invariant here is a one-way "file → index" cover, and a file that is extra is a bug.
    #[test]
    fn every_corpus_md_file_is_indexed() {
        let docs_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/docs");
        let mut on_disk: Vec<String> = std::fs::read_dir(&docs_dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", docs_dir.display()))
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        on_disk.sort();

        let indexed: Vec<String> = ARTICLES
            .iter()
            .map(|(slug, _, _)| format!("{slug}.md"))
            .collect();
        // indexed must also be sorted so the two asserts pair up one to one
        let mut indexed_sorted = indexed.clone();
        indexed_sorted.sort();

        assert_eq!(
            on_disk, indexed_sorted,
            "web/src/docs/*.md 列表与 ARTICLES 不一致：\n  on disk: {on_disk:?}\n  indexed:  {indexed:?}\n\
             新增一份语料要在两边都登记；删一份同理。drift = chat 搜不到。"
        );
    }
}
