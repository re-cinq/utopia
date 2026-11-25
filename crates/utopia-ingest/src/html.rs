//! Canonical HTML conversion shared by file, URL and feed ingestion.
#[cfg(test)]
mod table_tests {
    use super::{markdown_from_html, promote_first_row_headers};

    #[test]
    fn a_table_without_headers_still_keeps_its_rows() {
        let html = "<table><tr><td>Revenue</td><td>Q2 FY27</td><td>Q1 FY27</td></tr>\
                    <tr><td>total</td><td>$96,221</td><td>$81,615</td></tr></table>";
        let md = markdown_from_html(html).expect("converts");
        // Row and column relations survive: the three cells of one row stay on one row of text
        let row = md
            .lines()
            .find(|l| l.contains("96,221"))
            .expect("the numbers survive");
        assert!(
            row.contains("81,615"),
            "the two numbers on the same row should be on one row: {md}"
        );
        assert!(
            row.contains("total"),
            "row label should be on the same row as its numbers: {md}"
        );
    }

    #[test]
    fn empty_spacer_columns_do_not_survive() {
        let html = "<table><tr><td>Revenue</td><td></td><td>$96,221</td><td></td></tr>\
                    <tr><td>Margin</td><td></td><td>75.0</td><td></td></tr></table>";
        let md = markdown_from_html(html).expect("converts");
        let row = md
            .lines()
            .find(|l| l.contains("96,221"))
            .expect("row survives");
        assert!(
            !row.contains("|  |"),
            "empty column should be dropped: {row}"
        );
        assert!(
            row.contains("Revenue"),
            "a column with content must not be lost: {row}"
        );
    }

    #[test]
    fn a_blank_header_row_gives_way_to_the_real_one() {
        let md = super::prune_empty_table_columns(
            "|  |  |\n| --- | --- |\n| a. Tench Coxe |  |\n| shares For | 15,411 |",
        );
        let first = md.lines().next().expect("a line");
        assert!(
            first.contains("Tench Coxe"),
            "the header should be the real column heading: {md}"
        );
        assert!(md.contains("15,411"), "rows must not be lost: {md}");
    }

    #[test]
    fn a_column_with_any_content_is_kept() {
        let md = super::prune_empty_table_columns("| a |  | c |\n| --- | --- | --- |\n|  | b |  |");
        assert!(md.contains("| a |  | c |") || md.contains("a"), "{md}");
        assert!(
            md.contains("b"),
            "a column with content must not be dropped: {md}"
        );
    }

    /// The page title "Pricing" also appears in the table name "Pricing tiers": that is the table's name, not a heading in the body text
    #[test]
    fn a_caption_that_repeats_the_page_title_keeps_the_title_heading() {
        let page = "<html><head><title>Pricing</title></head><body><article>\
            <p>Every plan includes unlimited projects, daily backups and email support. Teams that \
            pay for a year are charged for ten months, and the discount applies again at each \
            renewal. Amounts are in US dollars before tax; invoices list the tax of the country \
            the account is billed in. A plan can be changed at any time, prorated to the day.</p>\
            <table><caption>Pricing tiers</caption><tr><th>Plan</th><th>Monthly</th></tr>\
            <tr><td>Basic</td><td>10</td></tr><tr><td>Team</td><td>25</td></tr></table>\
            </article></body></html>";
        let md = super::page_to_markdown(page, Some("https://example.com/pricing")).unwrap();
        assert!(md.starts_with("# Pricing\n\n"), "{md}");
        assert!(md.contains("Pricing tiers\n\n| Plan | Monthly |"), "{md}");
    }

    #[test]
    fn a_table_that_already_has_headers_is_untouched() {
        let html = "<table><thead><tr><th>a</th></tr></thead><tr><td>1</td></tr></table>";
        assert_eq!(promote_first_row_headers(html), html);
    }

    #[test]
    fn a_nested_table_does_not_promote_the_outer_row() {
        // The inner table is processed first; the outer one therefore sees <th> and skips, rather than guessing which row belongs to whom
        let html = "<table><tr><td><table><tr><td>inner</td></tr></table></td></tr></table>";
        let out = promote_first_row_headers(html);
        assert_eq!(out.to_ascii_lowercase().matches("<th").count(), 1, "{out}");
    }

    #[test]
    fn text_outside_tables_is_left_alone() {
        let html = "<p>plain</p>";
        assert_eq!(promote_first_row_headers(html), html);
    }
}

#[cfg(test)]
mod evidence_tests {
    use super::*;

    const REPORT: &str = "Researchers examined captcha verification across public services and found that older residents struggled with distorted images. Their study followed participants through several independent tasks, recording completion times and interviewing people about accessibility barriers. The published results describe how alternative authentication methods improved access without increasing fraudulent registrations. Local authorities are now reviewing procurement standards, while independent security specialists recommend measuring actual abuse rather than assuming every additional challenge provides protection.";

    #[test]
    fn substantive_marker_prose_survives_every_html_entry_point() {
        let raw = format!("<html><body><article><p>{REPORT}</p></article></body></html>");
        for markdown in [
            page_to_markdown(&raw, Some("https://example.com/story")),
            fragment_to_markdown(&raw),
            crate::parse("story.html", raw.as_bytes())
                .map(|p| p.text)
                .map_err(|e| HtmlError::Conversion(e.to_string())),
        ] {
            assert!(markdown.unwrap().contains("captcha verification"));
        }
    }

    #[test]
    fn substantive_raw_fallback_survives_newsletter_marker() {
        let raw = format!("<article><p>{REPORT}</p></article><form><p>Subscribe to read our newsletter</p><input type='email'></form>");
        assert!(dom_smoothie::Readability::new(raw.as_str(), Some("not a URL"), None).is_err());
        assert!(page_to_markdown(&raw, Some("not a URL"))
            .unwrap()
            .contains(REPORT));
        assert!(fragment_to_markdown(&raw).unwrap().contains(REPORT));
    }

    #[test]
    fn normal_paragraph_breaks_preserve_article_evidence() {
        let split = REPORT.replace(". ", ".</p><p>");
        for prose in [REPORT.to_string(), split] {
            let raw = format!("<article><p>{prose}</p></article><form><p>Subscribe to read our newsletter</p><input type='email'></form>");
            for result in [
                page_to_markdown(&raw, Some("https://example.com/story")),
                page_to_markdown(&raw, Some("not a URL")),
                fragment_to_markdown(&raw),
                crate::parse("story.html", raw.as_bytes())
                    .map(|p| p.text)
                    .map_err(|e| HtmlError::Conversion(e.to_string())),
            ] {
                assert!(result.unwrap().contains("captcha verification"));
            }
        }
    }

    #[test]
    fn short_and_padded_marker_shells_remain_rejected() {
        for marker in [
            "captcha verification",
            "Subscribe to read",
            "Sign in to continue",
        ] {
            for padding in [
                String::new(),
                "Account access requires verification of your identity before proceeding. "
                    .repeat(100),
            ] {
                let raw = format!("<h1>{marker}</h1><p>{padding}</p>");
                assert_eq!(fragment_to_markdown(&raw), Err(HtmlError::Interstitial));
                assert_eq!(
                    page_to_markdown(&raw, Some("https://example.com")),
                    Err(HtmlError::Interstitial)
                );
            }
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HtmlError {
    #[error("HTML conversion failed: {0}")]
    Conversion(String),
    #[error("HTML is an authentication or player interstitial")]
    Interstitial,
}

/// Extract a full page; Readability failure uses one raw conversion fallback.
pub fn page_to_markdown(html: &str, base_url: Option<&str>) -> Result<String, HtmlError> {
    // Judge the extracted body, not newsletter/login controls in page chrome.
    // The raw fallback still goes through the same fail-closed converter.
    let article = dom_smoothie::Readability::new(html, base_url, None)
        .and_then(|mut readability| readability.parse());
    match article {
        Ok(article) => {
            // Readability can discard every control on a login-only page and
            // return its tiny label as an article. Such a result is not body
            // evidence; use the guarded fallback rather than laundering it.
            if article
                .text_content
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .count()
                < 200
            {
                return markdown_from_html(html);
            }
            let (markdown, prose) = markdown_and_prose(&article.content)?;
            let title = article.title.trim();
            // Look for headings only in the body text htmd wrote, not in the reattached tables: the explanatory sentence before a table (<caption>, header row)
            // often repeats the page title; counting it would skip adding `# Title`, and no chunk's breadcrumb would carry it
            if title.is_empty() || prose.contains(title) {
                Ok(markdown)
            } else {
                Ok(sanitize_markdown_links(&format!("# {title}\n\n{markdown}")))
            }
        }
        Err(_) => markdown_from_html(html),
    }
}

/// Feed fragments are already scoped; never run Readability over them.
pub fn fragment_to_markdown(html: &str) -> Result<String, HtmlError> {
    markdown_from_html(html)
}

fn looks_like_challenge_html(html: &str) -> bool {
    let lowered = html.to_ascii_lowercase();
    let auth_form = lowered.contains("<form")
        && (lowered.contains("password")
            || lowered.contains("type=\"email\"")
            || lowered.contains("type='email'"));
    let player_shell = [
        "jwplayer",
        "brightcove",
        "data-player",
        "<video",
        "youtube.com/embed",
        "player.vimeo.com",
    ]
    .iter()
    .filter(|marker| lowered.contains(**marker))
    .count()
        >= 2;
    auth_form || player_shell
}

// A marker is not a verdict: reporting may quote challenge text, and raw
// fallback may retain newsletter controls. Require developed, varied prose,
// not merely a long body (repeated instructions and word-number padding fail).
fn has_substantive_prose(markdown: &str) -> bool {
    // News paragraph layout is presentation, not evidence quality. Aggregate
    // prose across blocks, retaining vocabulary diversity to reject repeated
    // instruction padding. Exclude heading/control-link lines, not entire
    // paragraphs: inserting a blank line must not change their contribution.
    let prose = markdown
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#') && !line.contains("]("))
        .collect::<Vec<_>>()
        .join(" ");
    let sentences = prose.matches(['.', '!', '?', '。', '！', '？']).count();
    let words = prose
        .split(|ch: char| !ch.is_alphabetic())
        .filter(|word| word.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect::<std::collections::HashSet<_>>();
    let cjk = prose
        .chars()
        .filter(|ch| {
            matches!(*ch as u32,
        0x3400..=0x4dbf | 0x4e00..=0x9fff | 0x3040..=0x30ff | 0xac00..=0xd7af)
        })
        .collect::<std::collections::HashSet<_>>();
    sentences >= 3
        && prose.chars().filter(|ch| !ch.is_whitespace()).count() >= 400
        && (words.len() >= 40 || cjk.len() >= 40)
}

/// Detect authentication/consent shells in extracted text, not page chrome.
pub fn looks_like_challenge_shell(markdown: &str) -> bool {
    let lowered = markdown.to_ascii_lowercase();
    const HIGH_CONFIDENCE_MARKERS: &[&str] = &[
        "verify you are human",
        "checking your browser",
        "enable javascript and cookies",
        "accept cookies to continue",
        "subscribe to continue",
        "subscribe to read",
        "sign in to continue",
        "log in to continue",
        "this content is behind a paywall",
        "content is behind a paywall",
        "unlock this article",
        "captcha challenge",
        "complete the captcha",
        "solve the captcha",
        "enter the captcha",
        "captcha verification",
    ];
    if HIGH_CONFIDENCE_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
        || lowered
            .lines()
            .take(3)
            .any(|line| line.trim().trim_start_matches('#').trim() == "access denied")
    {
        return !has_substantive_prose(markdown);
    }

    // Readability can retain a long login/consent shell without any of the
    // exact phrases above. Require two short, control-like lines so ordinary
    // articles that mention one of these topics are not rejected.
    const STRUCTURAL_MARKERS: &[&str] = &[
        "sign in",
        "log in",
        "login",
        "create account",
        "register",
        "cookie settings",
        "consent preferences",
        "accept all cookies",
        "enable javascript",
        "access denied",
        "play video",
        "watch now",
    ];
    let structural_hits = lowered
        .lines()
        .take(40)
        .filter(|line| {
            let line = line.trim().trim_start_matches('#').trim();
            line.len() <= 96
                && STRUCTURAL_MARKERS
                    .iter()
                    .any(|marker| line.contains(marker))
        })
        .count();
    let has_auth_fields = (lowered.contains("password")
        && (lowered.contains("email") || lowered.contains("username")))
        || lowered.contains("<form");
    (structural_hits >= 2 || (structural_hits >= 1 && has_auth_fields))
        && !has_substantive_prose(markdown)
}

/// A table with no `<th>`: promote the `<td>` cells of the first row to `<th>`.
///
/// **The reason: a whole class of documents loses the row and column relations at the parsing step.** htmd's table handler requires
/// an explicit header (`<th>` or `<thead>`), otherwise the whole table falls back to flattening, one cell per line.
/// SEC XBRL reports have none of them: measured, the NVIDIA earnings report has 11 tables and the vote-result 8-K has 21,
/// all with `<th>` counts of 0. After flattening, the model sees a column of isolated labels next to a column of
/// isolated numbers, and can only guess by order which number goes with which column, so it extracts edges like `NVIDIA net_worth Net income`,
/// and the four vote categories for ten directors all degrade into bare numbers that cannot be told apart.
///
/// Promoting the first row to the header is what these tables actually mean ("Q2 FY27 | Q1 FY27 | Q2 FY26"),
/// and the rule is **deliberately narrow**: it only applies when the whole table has no `<th>`/`<thead>` at all. A table that already has a header
/// is left exactly as it is.
///
/// Nested tables are processed **inside out** (the order `</table>` appears gives exactly that): after the inner one is changed,
/// the outer one already carries `<th>`, so the outer one is skipped. Better to change one table less than to guess which row belongs to whom.
fn promote_first_row_headers(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = html.to_string();
    // (end position of the opening tag, end position of the table), in closing order = inside out
    let mut opens: Vec<usize> = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < lower.len() {
        let next_open = lower[i..].find("<table").map(|x| i + x);
        let next_close = lower[i..].find("</table").map(|x| i + x);
        match (next_open, next_close) {
            (Some(o), Some(c)) if o < c => {
                opens.push(o);
                i = o + 6;
            }
            (_, Some(c)) => {
                if let Some(o) = opens.pop() {
                    spans.push((o, c));
                }
                i = c + 7;
            }
            (Some(o), None) => {
                opens.push(o);
                i = o + 6;
            }
            (None, None) => break,
        }
    }
    // Positions shift as rewriting happens, so rewrite from the back to the front
    spans.sort_by_key(|(o, _)| std::cmp::Reverse(*o));
    for (open, close) in spans {
        let seg = &out[open..close.min(out.len())];
        let seg_lower = seg.to_ascii_lowercase();
        if seg_lower.contains("<th") || seg_lower.contains("<thead") {
            continue;
        }
        let Some(tr) = seg_lower.find("<tr") else {
            continue;
        };
        let Some(tr_end) = seg_lower[tr..].find("</tr").map(|x| tr + x) else {
            continue;
        };
        let row = &seg[tr..tr_end];
        if !row.to_ascii_lowercase().contains("<td") {
            continue;
        }
        let rewritten = replace_td_with_th(row);
        out.replace_range(open + tr..open + tr_end, &rewritten);
    }
    out
}

/// Only the tag name is swapped; attributes are kept as they are.
fn replace_td_with_th(row: &str) -> String {
    let mut out = String::with_capacity(row.len());
    let lower = row.to_ascii_lowercase();
    let mut i = 0usize;
    while i < row.len() {
        if lower[i..].starts_with("<td") {
            out.push_str("<th");
            i += 3;
        } else if lower[i..].starts_with("</td") {
            out.push_str("</th");
            i += 4;
        } else {
            let ch = row[i..].chars().next().expect("char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Columns that are empty throughout the table are dropped; alignment spaces are not kept either.
///
/// **Not dropping them costs triple.** Empty cells used for layout (XBRL indent columns, currency-symbol columns) take up no room in
/// HTML, but once converted to a Markdown pipe table every row must write a `|` and a run of alignment spaces for each of them.
/// Measured on that earnings report: 24 chunks totaling 26k characters became 78 chunks totaling 92k characters; the extra is all `|  |  |  |`.
/// Chunk count feeds the cost of extraction, and a column of spaces is not worth that cost.
///
/// The test is "this column is empty in **every** row", so columns with content are never touched; the separator row does not take part in the decision
/// (it is only dashes anyway), but its column is dropped along with the rest.
fn prune_empty_table_columns(markdown: &str) -> String {
    let is_row = |l: &str| {
        let t = l.trim();
        t.starts_with('|') && t.ends_with('|') && t.len() > 1
    };
    let cells = |l: &str| -> Vec<String> {
        let t = l.trim();
        t[1..t.len() - 1]
            .split('|')
            .map(|c| c.trim().to_string())
            .collect()
    };
    let is_sep = |c: &[String]| {
        !c.is_empty()
            && c.iter()
                .all(|x| !x.is_empty() && x.chars().all(|ch| ch == '-' || ch == ':'))
    };

    let lines: Vec<&str> = markdown.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0usize;
    while i < lines.len() {
        if !is_row(lines[i]) {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && is_row(lines[i]) {
            i += 1;
        }
        let rows: Vec<Vec<String>> = lines[start..i].iter().map(|l| cells(l)).collect();
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        let keep: Vec<bool> = (0..width)
            .map(|c| {
                rows.iter()
                    .any(|r| !is_sep(r) && r.get(c).is_some_and(|x| !x.is_empty()))
            })
            .collect();
        // If no column is left, keep the table as it is, rather than guessing
        if !keep.iter().any(|k| *k) {
            out.extend(lines[start..i].iter().map(|l| l.to_string()));
            continue;
        }
        let render = |r: &Vec<String>| {
            let sep = is_sep(r);
            let kept: Vec<String> = (0..width)
                .filter(|c| keep[*c])
                .map(|c| {
                    let v = r.get(c).cloned().unwrap_or_default();
                    if sep && v.is_empty() {
                        "---".to_string()
                    } else {
                        v
                    }
                })
                .collect();
            format!("| {} |", kept.join(" | "))
        };
        // **If the header row is entirely empty, step aside.** The first row that gets promoted is sometimes only a layout placeholder row
        // (every table in the SEC vote-result 8-K looks like this). Keeping it means the model sees
        // a table whose column names are all empty: no information, while still occupying the "header" slot. The next row takes its place, and
        // the table's first row is then its real heading ("a. Tench Coxe").
        let head_blank = rows.first().is_some_and(|r| {
            (0..width)
                .filter(|c| keep[*c])
                .all(|c| r.get(c).is_none_or(String::is_empty))
        });
        let body_start = if head_blank && rows.len() > 2 { 2 } else { 0 };
        if body_start == 2 {
            out.push(render(&rows[2]));
            if let Some(sep) = rows.get(1) {
                out.push(render(sep));
            }
            for r in &rows[3..] {
                out.push(render(r));
            }
        } else {
            for r in &rows {
                out.push(render(r));
            }
        }
    }
    out.join("\n")
}

fn markdown_from_html(html: &str) -> Result<String, HtmlError> {
    markdown_and_prose(html).map(|(markdown, _)| markdown)
}

/// The Markdown produced by conversion, and the version htmd wrote before the table was swapped back (the table was still a placeholder paragraph)
fn markdown_and_prose(html: &str) -> Result<(String, String), HtmlError> {
    // The outermost table is first rendered from the DOM as Markdown with a real header (see `table`); htmd only sees a placeholder paragraph;
    // nested tables still take the old route, so the first-row header promotion patch stays for them
    let (html, tables) = crate::table::lift_tables(html);
    let html = &promote_first_row_headers(&html);
    let markdown = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec![
            "script", "style", "iframe", "object", "embed", "img", "svg", "math",
        ])
        .scripting_enabled(false)
        .build()
        .convert(html)
        .map_err(|e| HtmlError::Conversion(e.to_string()))?;
    // Controls alone are not an interstitial: even the raw fallback may
    // contain a complete article alongside a newsletter or login modal.
    if looks_like_challenge_shell(&markdown)
        || (markdown.chars().filter(|ch| !ch.is_whitespace()).count() < 200
            && looks_like_challenge_html(html))
    {
        return Err(HtmlError::Interstitial);
    }
    let restored = crate::table::restore_tables(&markdown, &tables);
    Ok((
        normalize_markdown(&prune_empty_table_columns(&restored))?,
        markdown,
    ))
}

/// Normalize direct Markdown with the same link policy as HTML conversion.
pub fn normalize_markdown(markdown: &str) -> Result<String, HtmlError> {
    Ok(sanitize_markdown_links(&post_process(markdown)))
}

fn post_process(markdown: &str) -> String {
    let mut result = String::with_capacity(markdown.len());
    let mut newlines = 0;
    for ch in markdown.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 3 {
                result.push(ch);
            }
        } else {
            newlines = 0;
            result.push(ch);
        }
    }
    result.trim().to_string()
}

fn sanitize_markdown_links(markdown: &str) -> String {
    let mut result = String::with_capacity(markdown.len());
    let bytes = markdown.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some(relative_start) = markdown[cursor..].find('[') else {
            result.push_str(&markdown[cursor..]);
            break;
        };
        let start = cursor + relative_start;
        result.push_str(&markdown[cursor..start]);
        if start > 0 && bytes[start - 1] == b'!' {
            result.push('[');
            cursor = start + 1;
            continue;
        }
        let Some(relative_close_label) = markdown[start + 1..].find("](") else {
            result.push('[');
            cursor = start + 1;
            continue;
        };
        let close_label = start + 1 + relative_close_label;
        let Some(close_link) = find_unescaped_byte(bytes, close_label + 2, b')') else {
            result.push('[');
            cursor = start + 1;
            continue;
        };
        let destination = &markdown[close_label + 2..close_link];
        if !is_unsafe_markdown_destination(destination) {
            result.push_str(&markdown[start..=close_link]);
        } else {
            result.push_str(&markdown[start + 1..close_label]);
        }
        cursor = close_link + 1;
    }
    result
}

fn find_unescaped_byte(bytes: &[u8], start: usize, wanted: u8) -> Option<usize> {
    let mut escaped = false;
    for (offset, byte) in bytes.iter().enumerate().skip(start) {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == wanted {
            return Some(offset);
        }
    }
    None
}

fn is_unsafe_markdown_destination(destination: &str) -> bool {
    let destination = destination
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    let decoded = percent_encoding::percent_decode_str(destination).decode_utf8_lossy();
    let decoded =
        decoded.trim_start_matches(|ch: char| ch.is_ascii_control() || ch.is_ascii_whitespace());
    let Some((scheme, _)) = decoded.split_once(':') else {
        return false;
    };
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "javascript" | "vbscript" | "data" | "file" | "blob"
    )
}
