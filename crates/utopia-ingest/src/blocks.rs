//! Document blocks: reads the Markdown the parser produces as a sequence of **top-level blocks**, each carrying its own
//! byte range in the source text. The chunker packs on top of this layer instead of searching for boundaries in the string directly.
//!
//! Why this layer exists: a string carries no information such as "this is the third row of a table", so the splitter can only guess from
//! line breaks and blank lines. The guesses were measured (recall desk, 2026-09-13): of 35 blocks in an earnings report, 13 start
//! in the middle of a table with no header, and faced with a row of five dollar figures the model can only guess which column is which quarter; in a vote
//! result, one table was split in two by a page break in the source, and the second half became an orphan table with no name.
//!
//! This layer does only two things: reads out the blocks, and reattaches tables that a page break split. How big to cut and how to add the header
//! belongs to `chunker`.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::ops::Range;

/// What a top-level block is. Ranges are always aligned to whole lines; the text that is cut out is not altered by a single character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Number of `#` characters
    Heading(u8),
    Paragraph,
    /// `head` is the header row together with the separator row; `rows` are the body rows, one range per row.
    /// A reattached continuation table makes `head`/`rows` non-contiguous, so a table's text is always assembled from these two,
    /// not sliced out of `Block::range` in one piece
    Table {
        head: Range<usize>,
        rows: Vec<Range<usize>>,
    },
    /// A horizontal rule. In these documents it is usually a **page break**, not a section boundary
    Rule,
    /// Lists, code blocks, quotes, raw HTML: each is one unit as a whole, not looked into
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: Kind,
    pub range: Range<usize>,
}

/// Reads out the top-level blocks, and reattaches the tables that a page break split.
pub fn blocks(text: &str) -> Vec<Block> {
    join_continuations(text, parse(text))
}

/// The table being collected: the full table range, the header range, the body row ranges
type OpenTable = (Range<usize>, Option<Range<usize>>, Vec<Range<usize>>);

fn parse(text: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut depth = 0usize;
    let mut table: Option<OpenTable> = None;
    for (event, range) in Parser::new_ext(text, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    let kind = match &tag {
                        Tag::Heading { level, .. } => Some(Kind::Heading(*level as u8)),
                        Tag::Paragraph => Some(Kind::Paragraph),
                        Tag::Table(_) => {
                            table = Some((range.clone(), None, Vec::new()));
                            None
                        }
                        _ => Some(Kind::Other),
                    };
                    if let Some(kind) = kind {
                        out.push(Block {
                            kind,
                            range: lines(text, range),
                        });
                    }
                } else if depth == 1 {
                    if let Some((_, head, rows)) = table.as_mut() {
                        match tag {
                            Tag::TableHead => *head = Some(lines(text, range)),
                            Tag::TableRow => rows.push(lines(text, range)),
                            _ => {}
                        }
                    }
                }
                depth += 1;
            }
            Event::End(tag) => {
                depth = depth.saturating_sub(1);
                if depth == 0 && tag == TagEnd::Table {
                    if let Some((whole, head, rows)) = table.take() {
                        let whole = lines(text, whole);
                        // The header row together with the separator row under it: from the start of the table to before the first body row
                        let head = match (head, rows.first()) {
                            (Some(_), Some(first)) => whole.start..line_before(text, first.start),
                            _ => whole.clone(),
                        };
                        out.push(Block {
                            kind: Kind::Table { head, rows },
                            range: whole,
                        });
                    }
                }
            }
            Event::Rule if depth == 0 => out.push(Block {
                kind: Kind::Rule,
                range: lines(text, range),
            }),
            _ => {}
        }
    }
    out
}

/// Reattaches a table that a page break split.
///
/// The shape is very specific: `table A · separator · table B`, two tables of equal width, where the first cell of B's "header" shares its first two words with the first cell of a body row of A:
/// `Number of shares Abstaining` comes right after
/// `Number of shares For / Against`. That is not B's header; it is the next row of A, and
/// the parser only treated it as a header because a separator row sits above it.
///
/// The test is deliberately narrow: it looks at the leading label only, not at the numbers. "Same width, numbers in the header" would merge two financial tables laid out by year into one,
/// since `2025 | 2024` is numeric too.
fn join_continuations(text: &str, blocks: Vec<Block>) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
    let mut i = 0usize;
    while i < blocks.len() {
        let b = blocks[i].clone();
        let joined = match (&b.kind, out.last()) {
            (
                Kind::Rule,
                Some(Block {
                    kind: Kind::Table { .. },
                    ..
                }),
            ) => match blocks.get(i + 1) {
                Some(Block {
                    kind: Kind::Table { head, rows },
                    range,
                }) if continues(text, out.last().unwrap(), head) => {
                    let Some(Block {
                        kind:
                            Kind::Table {
                                rows: prev_rows, ..
                            },
                        range: prev_range,
                    }) = out.last_mut()
                    else {
                        unreachable!()
                    };
                    // B's "header row" is one of A's body rows; drop the separator row
                    prev_rows.push(first_line(text, head));
                    prev_rows.extend(rows.iter().cloned());
                    prev_range.end = range.end;
                    i += 2;
                    true
                }
                _ => false,
            },
            _ => false,
        };
        if !joined {
            out.push(b);
            i += 1;
        }
    }
    out
}

fn continues(text: &str, prev: &Block, next_head: &Range<usize>) -> bool {
    let Kind::Table { head, rows } = &prev.kind else {
        return false;
    };
    let width = |r: &Range<usize>| cell_count(&text[first_line(text, r)]);
    if width(head) != width(next_head) {
        return false;
    }
    let lead = |r: &Range<usize>| {
        first_cell(&text[first_line(text, r)])
            .split_whitespace()
            .take(2)
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
    };
    let next = lead(next_head);
    next.len() == 2 && rows.iter().any(|r| lead(r) == next)
}

/// Aligns the range to whole lines: backs up to the start of the line, and moves forward to the end of the line (excluding the newline).
fn lines(text: &str, r: Range<usize>) -> Range<usize> {
    let start = text[..r.start.min(text.len())]
        .rfind('\n')
        .map_or(0, |i| i + 1);
    let mut end = r.end.min(text.len());
    while end > start && matches!(text.as_bytes()[end - 1], b'\n' | b' ' | b'\t') {
        end -= 1;
    }
    let mut end = text[end..].find('\n').map_or(text.len(), |i| end + i);
    // Extending to the end of the line pulls back trailing spaces on it; block text carries no trailing whitespace
    while end > start && matches!(text.as_bytes()[end - 1], b' ' | b'\t') {
        end -= 1;
    }
    start..end
}

/// End of the previous line (excluding the newline): where one range ends, the next one starts
fn line_before(text: &str, at: usize) -> usize {
    let mut end = at;
    while end > 0 && matches!(text.as_bytes()[end - 1], b'\n' | b' ' | b'\t') {
        end -= 1;
    }
    end
}

fn first_line(text: &str, r: &Range<usize>) -> Range<usize> {
    let end = text[r.start..r.end]
        .find('\n')
        .map_or(r.end, |i| r.start + i);
    r.start..end
}

fn cell_count(line: &str) -> usize {
    let t = line.trim();
    if t.starts_with('|') && t.ends_with('|') && t.len() > 1 {
        t[1..t.len() - 1].split('|').count()
    } else {
        0
    }
}

fn first_cell(line: &str) -> &str {
    let t = line.trim();
    t.trim_start_matches('|')
        .split('|')
        .next()
        .unwrap_or("")
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<String> {
        blocks(text)
            .into_iter()
            .map(|b| match b.kind {
                Kind::Heading(l) => format!("h{l}"),
                Kind::Paragraph => "p".into(),
                Kind::Table { rows, .. } => format!("table({})", rows.len()),
                Kind::Rule => "rule".into(),
                Kind::Other => "other".into(),
            })
            .collect()
    }

    #[test]
    fn a_table_keeps_its_header_and_rows_apart() {
        let text = "Intro.\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n\nAfter.";
        let b = blocks(text);
        assert_eq!(kinds(text), ["p", "table(2)", "p"]);
        let Kind::Table { head, rows } = &b[1].kind else {
            panic!()
        };
        assert_eq!(&text[head.clone()], "| a | b |\n| --- | --- |");
        assert_eq!(&text[rows[0].clone()], "| 1 | 2 |");
        assert_eq!(&text[rows[1].clone()], "| 3 | 4 |");
        assert_eq!(
            &text[b[1].range.clone()],
            "| a | b |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |"
        );
    }

    /// The shape of the Stephen C. Neal table in a vote-result 8-K: the page break in the source lands in the middle of the table,
    /// and the second half starts with a separator row, so the parser treated the "Abstaining" row as the header of a new table.
    #[test]
    fn a_headerless_table_after_a_rule_continues_the_one_before() {
        let text = "| g. Stephen C. Neal |  |\n| --- | --- |\n| Number of shares For | 14,573 |\n| Number of shares Against | 2,234 |\n\n* * *\n\n| Number of shares Abstaining | 49,064 |\n| --- | --- |\n| Number of Broker Non-Votes | 2,829 |\n";
        assert_eq!(kinds(text), ["table(4)"]);
        let Kind::Table { rows, .. } = &blocks(text)[0].kind else {
            panic!()
        };
        let rows: Vec<&str> = rows.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(rows[2], "| Number of shares Abstaining | 49,064 |");
        assert_eq!(rows[3], "| Number of Broker Non-Votes | 2,829 |");
    }

    /// Two tables split by year across a page break: the header is numbers like `2025 | 2024`, so they are not joined.
    #[test]
    fn two_tables_with_numeric_headers_stay_two_tables() {
        let text = "| Revenue | 2025 |\n| --- | --- |\n| Total | 10 |\n\n* * *\n\n| 2024 | 2023 |\n| --- | --- |\n| Total | 9 |\n";
        assert_eq!(kinds(text), ["table(1)", "rule", "table(1)"]);
    }

    #[test]
    fn headings_rules_and_lists_are_their_own_blocks() {
        let text = "# Title\n\nPara one.\n\n- a\n- b\n\n---\n\n## Sub\n\nPara two.";
        assert_eq!(kinds(text), ["h1", "p", "other", "rule", "h2", "p"]);
        let b = blocks(text);
        assert_eq!(&text[b[0].range.clone()], "# Title");
        assert_eq!(&text[b[2].range.clone()], "- a\n- b");
    }
}
