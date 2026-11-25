//! Tables are rendered straight from the DOM as Markdown tables with real headers (work of the 0039/0040 layer).
//!
//! Why not hand this to htmd: the tables in SEC/XBRL reports have no `<th>` at all. Title rows (company name, report name,
//! units) are ordinary rows spanning the full width; the column headers ("July 26," / "2026") are split across two rows, and each number has a
//! "$" or ")" cell before or after it, with empty placeholder cells between columns. The earlier patch promoted the first row to the header, so every piece of the split table
//! got the header "| NVIDIA CORPORATION | | |", and none carried the real column headers. Faced with
//! "Accounts receivable, net | 63,059 | 38,466", the model could only write "phrase = number", or nothing. In the #743 long financial report runs,
//! 38 of the 56 wrong statements and 43 of the 64 unreadable ones came from here.
//!
//! This only looks at structure and recognizes no words: spanning cells, empty columns, cells with only symbols, rows with a single cell, the left inner
//! margin of row labels, and whether a cell has digits or letters. It does four things: promotes the title row to an explanatory sentence before the table; joins multi-line column headers per column into
//! one row; a row with only a label and no values is treated as a section and folded into the label of every row below it ("Current assets ›
//! Accounts receivable, net"); symbol cells such as "$" and ")" are merged back into the adjacent number cell. The rendered table is handed to
//! `blocks` / `chunker`, which already keep the explanatory sentence and the header with every chunk.
//!
//! Only the outermost table is handled, and only if it contains no inner table; nested tables are left to htmd as before.
//!
//! Tables from docx and spreadsheets do not go through the DOM: the parser collects the cells into a grid and hands it to [`render_grid`]; after that the
//! classification and rendering are exactly the same as for HTML tables. By convention, the first row of a spreadsheet (the row with at least two cells that have text)
//! is the column header, even if the column headers are numbers such as years. csv/tsv keep record and field boundaries and are handed to [`render_records`].

use dom_query::{Document, Selection};

const PLACEHOLDER: &str = "UTOPIATABLE";
const PATH_SEP: &str = " › ";

/// One cell: its text, how many columns it spans, the left inner padding (pt, rounded), and its starting column in the grid.
#[derive(Clone, Debug, Default)]
struct Cell {
    text: String,
    span: usize,
    pad: u32,
    col: usize,
}

#[derive(Clone, Debug)]
struct Row {
    cells: Vec<Cell>,
    /// Total grid width (including spans)
    width: usize,
}

impl Row {
    /// The non-empty cell covering column k
    fn covering(&self, k: usize) -> Option<&Cell> {
        self.cells
            .iter()
            .find(|c| !c.text.is_empty() && c.col <= k && k < c.col + c.span)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Skip,
    Caption,
    Header,
    Section,
    Data,
}

/// Replaces the outermost table that has no inner table with a placeholder paragraph; returns the changed HTML and the Markdown each placeholder corresponds to.
pub fn lift_tables(html: &str) -> (String, Vec<String>) {
    let doc = Document::from(html);
    let mut rendered: Vec<String> = Vec::new();
    let mut targets: Vec<Selection<'_>> = Vec::new();
    // The column header of the previous table: when a page break splits a report into two <table>s, the second half has no header,
    // but the same column count, so it is a continuation of the same table and reuses the previous table's column header
    let mut last_headers: Vec<String> = Vec::new();
    for tbl in doc.select("table").iter() {
        let nested_in = tbl.nodes().first().is_some_and(|n| {
            n.ancestors(None)
                .iter()
                .any(|a| a.node_name().as_deref() == Some("table"))
        });
        if nested_in || tbl.select("table").exists() {
            continue;
        }
        if let Some((md, headers)) = render_table(&tbl, &last_headers) {
            rendered.push(md);
            targets.push(tbl);
            last_headers = headers;
        }
    }
    if rendered.is_empty() {
        return (html.to_string(), rendered);
    }
    for (i, tbl) in targets.iter().enumerate() {
        tbl.replace_with_html(format!("<p>{PLACEHOLDER}{i}</p>"));
    }
    (doc.html().to_string(), rendered)
}

/// Swaps the placeholder rows in the Markdown converted by htmd back for the rendered tables.
pub fn restore_tables(markdown: &str, tables: &[String]) -> String {
    if tables.is_empty() {
        return markdown.to_string();
    }
    let extra: usize = tables.iter().map(String::len).sum();
    let mut out = String::with_capacity(markdown.len() + extra);
    for line in markdown.lines() {
        let hit = line
            .trim()
            .strip_prefix(PLACEHOLDER)
            .and_then(|rest| rest.parse::<usize>().ok())
            .and_then(|i| tables.get(i));
        match hit {
            Some(md) => {
                out.push('\n');
                out.push_str(md);
                out.push('\n');
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

fn hidden(style: &str) -> bool {
    let s: String = style.chars().filter(|c| !c.is_whitespace()).collect();
    s.to_ascii_lowercase().contains("display:none")
}

/// Left inner padding: `padding-left: 12pt`, `text-indent: 6pt`, or the fourth value of `padding: a b c d`.
fn padding_left(style: &str) -> u32 {
    let lower = style.to_ascii_lowercase();
    let number_at = |s: &str| -> Option<f32> {
        let s = s.trim_start();
        let end = s
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(s.len());
        s[..end].parse::<f32>().ok()
    };
    for key in ["padding-left:", "text-indent:"] {
        if let Some(i) = lower.find(key) {
            if let Some(v) = number_at(&lower[i + key.len()..]) {
                return v.round() as u32;
            }
        }
    }
    if let Some(i) = lower.find("padding:") {
        let rest = lower[i + "padding:".len()..]
            .split(';')
            .next()
            .unwrap_or("");
        let parts: Vec<&str> = rest.split_whitespace().collect();
        let left = match parts.len() {
            4 => parts.get(3),
            2 | 3 => parts.get(1),
            1 => parts.first(),
            _ => None,
        };
        if let Some(v) = left.and_then(|p| number_at(p)) {
            return v.round() as u32;
        }
    }
    0
}

fn clean(text: &str) -> String {
    text.replace('\u{a0}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn attr_usize(cell: &Selection<'_>, name: &str) -> usize {
    cell.attr(name)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .max(1)
}

fn is_numeric(text: &str) -> bool {
    text.chars().any(|c| c.is_ascii_digit()) && !text.chars().any(char::is_alphabetic)
}

/// Symbol cells that merge into the neighbouring cell: currency symbols and opening parentheses attach to the front of the next cell, and closing parentheses and percent signs attach to the back of the previous one.
/// Dash-like "empty value" placeholders do not count; they are the content of their own cell, and merging them would shift the values of the whole row.
fn merge_direction(text: &str) -> Option<bool> {
    let mut chars = text.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    if first == '(' || is_currency(first) {
        Some(true)
    } else if first == ')' || first == '%' {
        Some(false)
    } else {
        None
    }
}

fn is_currency(c: char) -> bool {
    matches!(c, '$' | '€' | '£' | '¥' | '₩' | '₹' | '¢' | '￥')
}

/// Reads out the grid: row by row through `tr`, with cells that span rows carried down.
fn grid(tbl: &Selection<'_>) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    // (column, cell, rows remaining)
    let mut carry: Vec<(usize, Cell, usize)> = Vec::new();
    for tr in tbl.select("tr").iter() {
        let mut cells: Vec<Cell> = Vec::new();
        let mut col = 0usize;
        for cell in tr.select("td, th").iter() {
            let style = cell
                .attr("style")
                .map(|s| s.to_string())
                .unwrap_or_default();
            if hidden(&style) {
                continue;
            }
            place_carried(&mut col, &mut cells, &mut carry);
            let span = attr_usize(&cell, "colspan");
            let rowspan = attr_usize(&cell, "rowspan");
            let c = Cell {
                // `text()` joins only the text nodes end to end, so 10<br>20 reads as 1020; `formatted_text()` breaks lines at
                // <br>, <p>, <div>, <li> and similar places, while inline <b> and <span> are still joined end to end;
                // `clean` then collapses the breaks into one space
                text: clean(&cell.formatted_text()),
                span,
                pad: padding_left(&style),
                col,
            };
            if rowspan > 1 {
                carry.push((col, c.clone(), rowspan - 1));
            }
            col += span;
            cells.push(c);
        }
        place_carried(&mut col, &mut cells, &mut carry);
        rows.push(Row { cells, width: col });
    }
    rows
}

fn place_carried(col: &mut usize, cells: &mut Vec<Cell>, carry: &mut Vec<(usize, Cell, usize)>) {
    while let Some(pos) = carry.iter().position(|(c, _, _)| *c == *col) {
        let (_, mut cell, left) = carry[pos].clone();
        cell.col = *col;
        *col += cell.span;
        cells.push(cell);
        if left <= 1 {
            carry.remove(pos);
        } else {
            carry[pos].2 = left - 1;
        }
    }
}

/// What a row is: looks only at the count, position and span of non-empty cells, and whether the cells contain digits.
fn kind(row: &Row, headers_seen: bool, data_seen: bool, table_width: usize) -> Kind {
    let nonempty: Vec<&Cell> = row.cells.iter().filter(|c| !c.text.is_empty()).collect();
    let Some(first) = nonempty.first() else {
        return Kind::Skip;
    };
    if nonempty.len() == 1 {
        let wide = first.span >= 2 && first.span * 2 >= table_width.max(2);
        if is_numeric(&first.text) {
            return if data_seen { Kind::Data } else { Kind::Header };
        }
        if !headers_seen && !data_seen {
            // Neither header nor data has come yet: left-aligned or full-width cell text is a title, and text that is not left-aligned is a column header
            return if wide || first.col == 0 {
                Kind::Caption
            } else {
                Kind::Header
            };
        }
        return Kind::Section;
    }
    let label = !is_numeric(&first.text);
    let values_after = nonempty[1..].iter().any(|c| is_numeric(&c.text));
    // A row with a label followed by numbers is a data row; a row whose leftmost cell already has content and that is all numbers also counts (tables with a year column).
    // A row whose leftmost cells are empty and that is all numbers ("2026 | 2026") is the second line of a column header
    if values_after && (label || first.col == 0 || data_seen) {
        return Kind::Data;
    }
    if !data_seen {
        return Kind::Header;
    }
    if label {
        // After a data row, another row with no numbers in its cells: treated as a section
        return Kind::Section;
    }
    Kind::Data
}

/// Merges symbol-only cells back into a neighbour: "$" and "(" attach to the front of the next non-empty cell, and ")" and "%" attach to the back of the previous one.
fn merge_symbols(row: &mut Row) {
    let n = row.cells.len();
    for i in 0..n {
        let Some(opening) = merge_direction(&row.cells[i].text) else {
            continue;
        };
        let sym = row.cells[i].text.clone();
        let target = if opening {
            (i + 1..n).find(|&j| !row.cells[j].text.is_empty())
        } else {
            (0..i).rev().find(|&j| !row.cells[j].text.is_empty())
        };
        let Some(j) = target else { continue };
        if opening {
            row.cells[j].text = format!("{sym}{}", row.cells[j].text);
        } else {
            row.cells[j].text = format!("{}{sym}", row.cells[j].text);
        }
        row.cells[i].text.clear();
    }
}

fn escape(text: &str) -> String {
    text.replace('|', "\\|")
}

/// Empty fields in csv/tsv are only missing values: they must not make the record count as a section, or merge separate fields into a financial report's label column.
pub(crate) fn render_records(rows: &[Vec<String>]) -> Option<String> {
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|s| clean(s)).collect())
        .collect();
    let header = rows.first()?;
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return None;
    }
    let render = |row: &[String]| {
        let cells: Vec<String> = row
            .iter()
            .map(|s| escape(s))
            .chain(std::iter::repeat(String::new()))
            .take(width)
            .collect();
        format!("| {} |", cells.join(" | "))
    };
    let mut lines = vec![render(header), format!("|{}", " --- |".repeat(width))];
    lines.extend(
        rows.iter()
            .skip(1)
            .filter(|r| r.iter().any(|s| !s.is_empty()))
            .map(|r| render(r)),
    );
    Some(lines.join("\n"))
}

/// Indent depth: first count the empty columns before the label cell (Workiva reports indent with empty cells), then the left padding
/// (other reports use padding-left). The two are combined into one number, columns first and padding after.
fn depth_of(cell: &Cell) -> u32 {
    cell.col as u32 * 1000 + cell.pad
}

/// A table → an explanatory sentence (optional) + the Markdown table with a real header, along with the column header it uses (for continuation tables to reuse).
/// Returns None when the table contains nothing at all.
fn render_table(tbl: &Selection<'_>, inherited: &[String]) -> Option<(String, Vec<String>)> {
    let (md, headers) = render_rows(grid(tbl), inherited, None)?;
    // A <caption> is not inside any tr, so it is invisible when reading the grid; and since the whole table is swapped for a placeholder paragraph, the sentence htmd would have written
    // is lost too. It is written back before the table the way htmd and Word captions are, as its own paragraph, without a forced colon like the title row has:
    // a short one is carried by the chunker into every chunk of the table as an explanatory sentence, while a long one (a table name followed by several explanatory sentences) is written once, like any other long paragraph.
    // A forced colon would have to repeat in every chunk if the caption is long, and the 300-token budget would leave little room for rows.
    // Uses formatted_text: text() joins the text on either side of a <br> end to end
    let captions: String = tbl
        .select("caption")
        .iter()
        .map(|c| clean(&c.formatted_text()))
        .filter(|t| !t.is_empty())
        .map(|t| as_paragraph(&t) + "\n\n")
        .collect();
    Some((captions + &md, headers))
}

/// Explanatory sentences that start with a Markdown block marker ("# of wells", "1. Demographics", "- Totals", "> 5 years",
/// "~~~ Notes") would be read as a heading, list, quote or code block: the chunker only accepts a paragraph as an explanatory sentence, so the table's continuation chunks would not have it;
/// read as a heading it would also replace the breadcrumb of every chunk after it, and read as a code block it would never close, swallowing the table and all the text after it to the end. Following how htmd
/// escapes body text, a backslash is put in front of the marker; ones that only look like markers ("#1 well", "~5 km") are written as-is
fn as_paragraph(line: &str) -> String {
    let b = line.as_bytes();
    let spaced = |i: usize| b.get(i).is_none_or(|&c| c == b' ');
    let hashes = b.iter().take_while(|&&c| c == b'#').count();
    let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
    let at = match b.first() {
        Some(b'>') => 0,
        Some(b'-' | b'+' | b'*') if spaced(1) => 0,
        Some(b'#') if spaced(hashes) => 0,
        Some(b'`' | b'~') if b.get(1) == b.first() && b.get(2) == b.first() => 0,
        Some(b'0'..=b'9')
            if digits <= 9 && matches!(b.get(digits), Some(b'.' | b')')) && spaced(digits + 1) =>
        {
            digits
        }
        _ => return line.to_string(),
    };
    format!("{}\\{}", &line[..at], &line[at..])
}

/// The entry point for other parsers: a row is a list of (text, columns spanned, left padding in pt). When `first_is_header` is true,
/// the first row with at least two cells that have text counts as the column header (spreadsheet convention); other rows are classified as usual.
pub(crate) fn render_grid(
    rows: &[Vec<(String, usize, u32)>],
    first_is_header: bool,
) -> Option<String> {
    let rows = grid_rows(rows);
    let forced = first_is_header
        .then(|| {
            rows.iter()
                .position(|r| r.cells.iter().filter(|c| !c.text.is_empty()).count() >= 2)
        })
        .flatten()
        .map(|row| row..row + 1);
    render_rows(rows, &[], forced).map(|(md, _)| md)
}

/// The entry point for docx and pptx: a grid that [`render_grid`] does not recognize as a table must still not lose text. A grid with no column header and no data rows
/// (single-column text, one cell with text per row, a notice with one cell spanning the row) is written out line by line: one paragraph per row, with the cells that have text joined by
/// ` | `. HTML tables are followed up by htmd, and spreadsheets and csv each have a fallback, so this step is not put in the renderer.
pub(crate) fn grid_or_lines(
    rows: &[Vec<(String, usize, u32)>],
    first_is_header: bool,
) -> Option<String> {
    render_grid(rows, first_is_header).or_else(|| {
        let lines: Vec<String> = rows
            .iter()
            .filter_map(|row| {
                let cells: Vec<String> = row
                    .iter()
                    .map(|(text, _, _)| clean(text))
                    .filter(|text| !text.is_empty())
                    .collect();
                (!cells.is_empty()).then(|| cells.join(" | "))
            })
            .collect();
        (!lines.is_empty()).then(|| lines.join("\n\n"))
    })
}

/// Merged cells in a spreadsheet can give a header spanning more than one row. The caller has already projected the rows into a grid;
/// this only handles the explicit header range as a header.
pub(crate) fn render_grid_with_headers(
    rows: &[Vec<(String, usize, u32)>],
    headers: Option<std::ops::Range<usize>>,
) -> Option<String> {
    let rows = grid_rows(rows);
    let forced = headers
        .map(|mut range| {
            range.end = range.end.min(rows.len());
            range
        })
        .filter(|range| !range.is_empty());
    render_rows(rows, &[], forced).map(|(md, _)| md)
}

fn grid_rows(rows: &[Vec<(String, usize, u32)>]) -> Vec<Row> {
    rows.iter()
        .map(|cells| {
            let mut col = 0usize;
            let mut out = Vec::with_capacity(cells.len());
            for (text, span, pad) in cells {
                let span = (*span).max(1);
                out.push(Cell {
                    text: clean(text),
                    span,
                    pad: *pad,
                    col,
                });
                col += span;
            }
            Row {
                cells: out,
                width: col,
            }
        })
        .collect()
}

fn render_rows(
    mut rows: Vec<Row>,
    inherited: &[String],
    forced_header: Option<std::ops::Range<usize>>,
) -> Option<(String, Vec<String>)> {
    let width = rows.iter().map(|r| r.width).max().unwrap_or(0);
    if width == 0 {
        return None;
    }
    let mut captions: Vec<String> = Vec::new();
    let mut headers: Vec<Row> = Vec::new();
    // (section depth, section name, whether a row deeper than it has already been seen)
    // A section is ended by a data row at the same depth only after it already has deeper child rows. In a balance sheet
    // the child rows of "Current assets:" are indented, so the same-depth "Property and equipment" is the next item;
    // in a cash flow statement the child rows of "Cash flows from operating activities:" are not indented, so the same-depth
    // "Net income" is its first row
    let mut sections: Vec<(u32, String, bool)> = Vec::new();
    // (label path, the row)
    let mut data: Vec<(String, Row)> = Vec::new();
    let (mut headers_seen, mut data_seen) = (false, false);
    for (i, row) in rows.iter_mut().enumerate() {
        let filled = row.cells.iter().filter(|c| !c.text.is_empty()).count();
        let k = match forced_header.as_ref() {
            Some(header) if header.contains(&i) => Kind::Header,
            // A row after the header is a record even if no cell has a number; a row with only one number filled in (a sequence column) is also a record,
            // and only a row with a single word filled in counts as a section
            Some(header)
                if i >= header.end
                    && (filled >= 2
                        || row
                            .cells
                            .iter()
                            .find(|c| !c.text.is_empty())
                            .is_some_and(|c| is_numeric(&c.text))) =>
            {
                Kind::Data
            }
            _ => kind(row, headers_seen, data_seen, width),
        };
        match k {
            Kind::Skip => {}
            Kind::Caption => {
                if let Some(c) = row.cells.iter().find(|c| !c.text.is_empty()) {
                    captions.push(c.text.clone());
                }
            }
            Kind::Header => {
                merge_symbols(row);
                headers.push(row.clone());
                headers_seen = true;
            }
            Kind::Section => {
                let Some(cell) = row.cells.iter().find(|c| !c.text.is_empty()) else {
                    continue;
                };
                let depth = depth_of(cell);
                while sections.last().is_some_and(|(d, _, _)| *d >= depth) {
                    sections.pop();
                }
                sections.push((
                    depth,
                    cell.text.trim_end_matches([':', '：']).to_string(),
                    false,
                ));
            }
            Kind::Data => {
                merge_symbols(row);
                let label_cell = row.cells.iter().find(|c| !c.text.is_empty());
                let depth = label_cell.map(depth_of).unwrap_or(0);
                while sections
                    .last()
                    .is_some_and(|(d, _, deeper)| *d > depth || (*d == depth && *deeper))
                {
                    sections.pop();
                }
                if let Some(top) = sections.last_mut() {
                    if top.0 < depth {
                        top.2 = true;
                    }
                }
                let path: Vec<&str> = sections.iter().map(|(_, s, _)| s.as_str()).collect();
                data.push((path.join(PATH_SEP), row.clone()));
                data_seen = true;
            }
        }
    }
    // A table with only column headers and no data rows does not exist: a table with no numeric cells at all has its first row as the header and the rest as data rows
    if data.is_empty() && headers.len() >= 2 {
        let rest = headers.split_off(1);
        data.extend(rest.into_iter().map(|r| (String::new(), r)));
    }
    if data.is_empty() && headers.is_empty() {
        return None;
    }

    // Label column = all columns to the left of the first column that ever contained a number; in a table with no numbers at all, the label column is just the leftmost cell.
    // Value columns = columns to the right of the label column that have content in some data row (a header column with text but no data at all does not count)
    let label_end: usize = (0..width)
        .find(|&k| {
            data.iter()
                .any(|(_, r)| r.covering(k).is_some_and(|c| is_numeric(&c.text)))
        })
        .or_else(|| {
            data.iter()
                .filter_map(|(_, r)| {
                    r.cells
                        .iter()
                        .find(|c| !c.text.is_empty())
                        .map(|c| c.col + c.span)
                })
                .min()
        })
        .or_else(|| {
            headers
                .iter()
                .filter_map(|h| h.cells.iter().find(|c| !c.text.is_empty()).map(|c| c.col))
                .min()
        })
        .unwrap_or(0);
    let has_content = |k: usize| -> bool {
        if data.is_empty() {
            headers.iter().any(|h| h.covering(k).is_some())
        } else {
            data.iter().any(|(_, r)| r.covering(k).is_some())
        }
    };
    let value_cols: Vec<usize> = (label_end..width).filter(|&k| has_content(k)).collect();
    let value_cols: Vec<Vec<usize>> = collapse_spans(value_cols, &headers, &data);
    // A value column is a group of grid columns: take the first non-empty cell in this row that falls in the group
    let at = |r: &Row, group: &[usize]| -> String {
        group
            .iter()
            .find_map(|&k| r.covering(k))
            .map(|c| c.text.clone())
            .unwrap_or_default()
    };
    // The row's label: the cells in the label column are joined in order of appearance
    let own_label = |r: &Row| -> String {
        let mut parts: Vec<&str> = Vec::new();
        for c in r
            .cells
            .iter()
            .filter(|c| !c.text.is_empty() && c.col < label_end)
        {
            if parts.last() != Some(&c.text.as_str()) {
                parts.push(c.text.as_str());
            }
        }
        parts.join(" ")
    };
    let column_header = |group: &[usize]| -> String {
        let mut parts: Vec<String> = Vec::new();
        for h in &headers {
            let t = at(h, group);
            if !t.is_empty() && parts.last() != Some(&t) {
                parts.push(t);
            }
        }
        parts.join(" ")
    };
    let mut header_texts: Vec<String> = value_cols.iter().map(|g| column_header(g)).collect();
    if headers.is_empty() && !data.is_empty() && inherited.len() == header_texts.len() {
        header_texts = inherited.to_vec();
    }
    // Header of the label column: only header cells that fall entirely within the label column count; a header that spans into the value columns belongs to the value column
    let label_header: String = {
        let mut parts: Vec<String> = Vec::new();
        for h in &headers {
            for c in h
                .cells
                .iter()
                .filter(|c| !c.text.is_empty() && c.col + c.span <= label_end)
            {
                if parts.last() != Some(&c.text) {
                    parts.push(c.text.clone());
                }
            }
        }
        parts.join(" ")
    };

    let mut lines: Vec<String> = Vec::new();
    if !captions.is_empty() {
        let mut caption = captions.join(" · ");
        if !caption.ends_with(':') && !caption.ends_with('：') {
            caption.push(':');
        }
        lines.push(caption);
        lines.push(String::new());
    }
    // When the leftmost column is itself numbers (a table with a year column): there is no label column, and the row is its value
    let has_label_col = label_end > 0;
    let ncols = usize::from(has_label_col) + value_cols.len().max(1);
    let header_cells: Vec<String> = has_label_col
        .then(|| escape(&label_header))
        .into_iter()
        .chain(header_texts.iter().map(|h| escape(h)))
        .chain(std::iter::repeat(String::new()))
        .take(ncols)
        .collect();
    lines.push(format!("| {} |", header_cells.join(" | ")));
    lines.push(format!("|{}", " --- |".repeat(ncols)));
    for (sections, r) in &data {
        let mut cells: Vec<String> = Vec::new();
        if has_label_col {
            let own = own_label(r);
            let label = match (sections.is_empty(), own.is_empty()) {
                (true, _) => own,
                (false, true) => sections.clone(),
                (false, false) => format!("{sections}{PATH_SEP}{own}"),
            };
            cells.push(escape(&label));
        }
        cells.extend(value_cols.iter().map(|g| escape(&at(r, g))));
        while cells.len() < ncols {
            cells.push(String::new());
        }
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    Some((lines.join("\n"), header_texts))
}

/// Spanning cells make the same value land on several grid columns, and "$" often takes a cell of its own: two adjacent columns that, in every row,
/// are either covered by the same cell or have at least one side empty are the same value column, and are grouped together.
fn collapse_spans(cols: Vec<usize>, headers: &[Row], data: &[(String, Row)]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    for k in cols {
        let same_as_prev = out.last().and_then(|g| g.last().copied()).is_some_and(|p| {
            headers.iter().chain(data.iter().map(|(_, r)| r)).all(|r| {
                match (r.covering(p), r.covering(k)) {
                    (Some(a), Some(b)) => a.col == b.col,
                    _ => true,
                }
            })
        });
        match out.last_mut() {
            Some(g) if same_as_prev => g.push(k),
            _ => out.push(vec![k]),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(html: &str) -> String {
        let (_, tables) = lift_tables(html);
        tables.join("\n=====\n")
    }

    #[test]
    fn a_grid_that_is_not_a_table_keeps_its_words_as_lines() {
        let cell = |text: &str, span: usize| (text.to_string(), span, 0);
        // Single-column text with a run of blank lines in between
        let one_column = vec![
            vec![cell("Action", 1)],
            vec![cell("", 1)],
            vec![cell("Approve contract", 1)],
            vec![cell("Renew license", 1)],
        ];
        for first_is_header in [false, true] {
            assert_eq!(render_grid(&one_column, first_is_header), None);
            assert_eq!(
                grid_or_lines(&one_column, first_is_header).as_deref(),
                Some("Action\n\nApprove contract\n\nRenew license")
            );
        }
        // Two columns, only the left one has text; a notice with one cell spanning the row
        let left_only = vec![
            vec![cell("Action", 1), cell("", 1)],
            vec![cell("Approve  contract", 1), cell("", 1)],
        ];
        assert_eq!(
            grid_or_lines(&left_only, false).as_deref(),
            Some("Action\n\nApprove contract")
        );
        assert_eq!(
            grid_or_lines(&[vec![cell("Notice for the board", 2)]], false).as_deref(),
            Some("Notice for the board")
        );
        // A grid with no text at all writes nothing; a real table is still a table
        assert_eq!(grid_or_lines(&[vec![cell("", 1)]], false), None);
        let table = vec![
            vec![cell("Item", 1), cell("2025", 1)],
            vec![cell("Revenue", 1), cell("10", 1)],
        ];
        assert_eq!(grid_or_lines(&table, true), render_grid(&table, true));
        assert!(grid_or_lines(&table, true)
            .unwrap()
            .starts_with("| Item | 2025 |"));
    }

    /// A single-column HTML table is usually for layout: it is left to htmd, so the headings, lists and links in its cells are kept
    #[test]
    fn a_single_column_html_table_is_left_to_the_page_converter() {
        let html = "<table><tr><td><h2>To our shareholders</h2><p>Revenue grew.</p></td></tr><tr><td><p>We opened three offices.</p></td></tr></table>";
        let (out, tables) = lift_tables(html);
        assert!(tables.is_empty(), "{tables:?}");
        assert_eq!(out, html);
    }

    #[test]
    fn a_filing_table_gets_its_real_headers_and_its_section_path() {
        let html = r#"<table>
          <tr><td style="width:0.1%"></td><td style="width:40%"></td><td style="width:0.1%"></td><td style="width:1%"></td><td style="width:20%"></td><td style="width:1%"></td><td style="width:1%"></td><td style="width:20%"></td><td style="width:0.1%"></td><td colspan="3" style="display:none"></td></tr>
          <tr><td colspan="9" style="text-align:center">NVIDIA CORPORATION</td><td colspan="3" style="display:none"></td></tr>
          <tr><td colspan="9" style="text-align:center">CONDENSED CONSOLIDATED BALANCE SHEETS</td></tr>
          <tr><td colspan="9" style="text-align:center">(In millions)</td></tr>
          <tr><td colspan="3"></td><td colspan="3" style="text-align:center">July 26,</td><td colspan="3" style="text-align:center">January 25,</td></tr>
          <tr><td colspan="3"></td><td colspan="3" style="text-align:center">2026</td><td colspan="3" style="text-align:center">2026</td></tr>
          <tr><td colspan="3" style="padding:2pt 1pt 2pt 6pt">Current assets:</td><td colspan="3"></td><td colspan="3"></td></tr>
          <tr><td colspan="3" style="padding:2pt 1pt 2pt 12pt">Accounts receivable, net</td><td>$</td><td>63,059</td><td></td><td>$</td><td>38,466</td><td></td></tr>
          <tr><td colspan="3" style="padding:2pt 1pt 2pt 12pt">Inventories</td><td></td><td>31,575</td><td></td><td></td><td>21,403</td><td></td></tr>
          <tr><td colspan="3" style="padding:2pt 1pt 2pt 6pt">Total current assets</td><td></td><td>197,412</td><td></td><td></td><td>125,605</td><td></td></tr>
        </table>"#;
        let md = render(html);
        assert!(
            md.starts_with(
                "NVIDIA CORPORATION · CONDENSED CONSOLIDATED BALANCE SHEETS · (In millions):\n"
            ),
            "标题行提成表前的说明句: {md}"
        );
        assert!(
            md.contains("|  | July 26, 2026 | January 25, 2026 |"),
            "两行列头拼成一行: {md}"
        );
        assert!(
            md.contains("| Current assets › Accounts receivable, net | $63,059 | $38,466 |"),
            "小节折进标签、美元符并回数字: {md}"
        );
        assert!(
            md.contains("| Current assets › Inventories | 31,575 | 21,403 |"),
            "{md}"
        );
        assert!(
            md.contains("| Total current assets | 197,412 | 125,605 |"),
            "合计行回到小节外: {md}"
        );
        assert!(
            !md.contains("Current assets:"),
            "section line should no longer appear by itself: {md}"
        );
    }

    #[test]
    fn a_lone_label_before_everything_is_the_caption_and_rows_keep_their_values() {
        let html = "<table><tr><td colspan=\"2\">a. Tench Coxe</td></tr>\
                    <tr><td>Number of shares For</td><td>15,411,252,412</td></tr>\
                    <tr><td>Number of shares Against</td><td>1,399,727</td></tr></table>";
        let md = render(html);
        assert!(md.starts_with("a. Tench Coxe:\n"), "{md}");
        assert!(
            md.contains("| Number of shares For | 15,411,252,412 |"),
            "{md}"
        );
        assert!(
            md.contains("| Number of shares Against | 1,399,727 |"),
            "{md}"
        );
    }

    /// A <caption> is the name the table states: it is written as its own paragraph before the title row, like a Word caption, with no colon;
    /// an empty <caption> left by a template writes nothing
    #[test]
    fn an_html_table_keeps_its_caption() {
        let table = |caption: &str| {
            render(&format!(
                "<table><caption>{caption}</caption>\
                 <tr><td colspan=\"2\">Spring survey 2026</td></tr>\
                 <tr><th>Well</th><th>Depth</th></tr><tr><td>North</td><td>1200</td></tr></table>"
            ))
        };
        let rest = "Spring survey 2026:\n\n| Well | Depth |\n| --- | --- |\n| North | 1200 |";
        assert_eq!(
            table("Table 2<br>Readings by well."),
            format!("Table 2 Readings by well.\n\n{rest}")
        );
        assert_eq!(table("&nbsp;"), rest);
    }

    #[test]
    fn a_caption_that_starts_like_markdown_stays_a_paragraph() {
        let table = |caption: &str| {
            render(&format!(
                "<table><caption>{caption}</caption>\
                 <tr><th>Well</th><th>Depth</th></tr><tr><td>North</td><td>1200</td></tr></table>"
            ))
        };
        for caption in [
            "# of wells",
            "1. Demographics",
            "2) Totals",
            "- Totals",
            "+ Totals",
            "* Preliminary",
            "&gt; 5 years in service",
            "~~~ Notes",
            "``` Notes",
        ] {
            let md = table(caption);
            assert_eq!(
                crate::blocks::blocks(&md)[0].kind,
                crate::blocks::Kind::Paragraph,
                "{md}"
            );
        }
        // Only those that merely look like markers are written as-is, so the original text in the block gains no backslashes
        for caption in ["#1 well", "~5 km"] {
            let md = table(caption);
            assert!(md.starts_with(&format!("{caption}\n\n")), "{md}");
        }
    }

    #[test]
    fn a_first_row_of_words_is_the_header_when_the_table_has_none() {
        let html = "<table><tr><td>Revenue</td><td>Q2 FY27</td><td>Q1 FY27</td></tr>\
                    <tr><td>total</td><td>$96,221</td><td>$81,615</td></tr></table>";
        let md = render(html);
        assert!(md.contains("| Revenue | Q2 FY27 | Q1 FY27 |"), "{md}");
        assert!(md.contains("| total | $96,221 | $81,615 |"), "{md}");
    }

    #[test]
    fn hidden_cells_and_empty_columns_do_not_survive() {
        let html = "<table><tr><td>Margin</td><td style=\"display:none\">ghost</td><td></td><td>75.0</td><td></td></tr></table>";
        let md = render(html);
        assert!(!md.contains("ghost"), "{md}");
        assert!(md.contains("| Margin | 75.0 |"), "{md}");
    }

    #[test]
    fn a_closing_paren_in_its_own_cell_rejoins_the_number() {
        let html =
            "<table><tr><td>Other</td><td>(5,497</td><td>)</td><td>387</td><td></td></tr></table>";
        let md = render(html);
        assert!(md.contains("| Other | (5,497) | 387 |"), "{md}");
    }

    #[test]
    fn a_dash_that_means_nothing_stays_in_its_own_cell() {
        let html =
            "<table><tr><td>Other</td><td>112</td><td>—</td><td>31</td><td>—</td></tr></table>";
        let md = render(html);
        assert!(md.contains("| Other | 112 | — | 31 | — |"), "{md}");
    }

    /// <br>, <p>, <div>, <li> in a cell separate the text on either side, the same as Word (#813) and slide (#1042) cells:
    /// the two readings 10 and 20 must not read as 1020, and the column headers "Depth" and "(m)" must not stick together as "Depth(m)".
    /// Inline <b> still does not separate
    #[test]
    fn a_line_break_inside_a_cell_keeps_the_words_on_either_side_apart() {
        let html = "<table><tr><th>Well</th><th>Depth<br>(m)</th><th>Readings</th></tr>\
                    <tr><td><p>North</p><p>Field</p></td><td>1200</td><td>10<br>20</td></tr>\
                    <tr><td><div>South</div><div>Ridge</div></td><td><b>9</b>50</td><td><ul><li>7</li><li>9</li></ul></td></tr></table>";
        let md = render(html);
        assert!(md.contains("| Well | Depth (m) | Readings |"), "{md}");
        assert!(md.contains("| North Field | 1200 | 10 20 |"), "{md}");
        assert!(md.contains("| South Ridge | 950 | 7 9 |"), "{md}");
    }

    #[test]
    fn a_headerless_table_after_a_page_break_takes_the_headers_before_it() {
        let html = "<table>\
            <tr><td></td><td>Q2 FY27</td><td>Q1 FY27</td></tr>\
            <tr><td>Revenue</td><td>96,221</td><td>81,615</td></tr></table>\
            <hr>\
            <table><tr><td colspan=\"3\">Cash flows from financing activities:</td></tr>\
            <tr><td>Dividends paid</td><td>(6,047)</td><td>(244)</td></tr></table>";
        let (_, tables) = lift_tables(html);
        assert_eq!(tables.len(), 2, "{tables:?}");
        assert!(
            tables[1].starts_with("Cash flows from financing activities:\n"),
            "{}",
            tables[1]
        );
        assert!(
            tables[1].contains("|  | Q2 FY27 | Q1 FY27 |"),
            "续表接着用前一张的列头: {}",
            tables[1]
        );
        assert!(
            tables[1].contains("| Dividends paid | (6,047) | (244) |"),
            "{}",
            tables[1]
        );
    }

    #[test]
    fn nested_tables_are_left_to_the_markdown_converter() {
        let html =
            "<table><tr><td><table><tr><td>inner</td><td>1</td></tr></table></td></tr></table>";
        let (out, tables) = lift_tables(html);
        assert!(tables.is_empty(), "{tables:?}");
        assert_eq!(out, html);
    }

    #[test]
    fn placeholders_come_back_as_tables() {
        let md = "before\n\nUTOPIATABLE0\n\nafter";
        let table = "cap:\n\n|  | a |\n| --- | --- |\n| x | 1 |".to_string();
        let out = restore_tables(md, &[table]);
        assert!(
            out.contains("before\n\n\ncap:\n\n|  | a |\n| --- | --- |\n| x | 1 |\n\n\nafter"),
            "{out}"
        );
    }
}
