//! Parsers for each format. All output plain text (structure is kept with markdown-style headings).

use anyhow::Context;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::io::{Cursor, Read, Write};
use std::process::Command;

const SPREADSHEET_ROW_LIMIT: usize = 2_000;
// Each retained cell passes through a 40-byte GridRow entry and two 48-byte renderer
// Cells, besides source values, strings and Vec spare capacity (~370 B/cell in a
// full dense import). Bound actual records/segments, not their empty column spans:
// a 2,000 x 150 sheet uses 300,000 entries and fits without losing any rows.
const XLSX_SEGMENT_LIMIT: usize = 1_000_000;
const CSV_RECORD_LIMIT: usize = 10_000;

/// Text decoding: chardetng detects the encoding (covers common Chinese encodings such as GBK/GB18030/BIG5).
pub fn plain_text(bytes: &[u8]) -> String {
    use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    let encoding = detector.guess(None, Utf8Detection::Allow);
    let (text, _, _) = encoding.decode(bytes);
    text.into_owned()
}

/// Whether this PDF's content stream draws any glyphs: any text-painting operator (`Tj`, `TJ`, `'`, `"`)
/// with a non-empty string counts.
///
/// **This is the only reliable boundary between "scanned document" and "we cannot read it".** In both cases `pdftotext` gives
/// empty output with exit code 0: a scanned image has no text to extract, while a file that uses a predefined CMap such as `GBK-EUC-H`,
/// which the text extractor has no table for (#739). Failing to read any structure counts as no text: it is better to send a file through one more OCR pass
/// than to keep it out of the library.
fn draws_text(bytes: &[u8]) -> bool {
    let Ok(doc) = lopdf::Document::load_mem(bytes) else {
        return false;
    };
    let drawn = |object: &lopdf::Object| match object {
        lopdf::Object::String(s, _) => !s.is_empty(),
        lopdf::Object::Array(items) => items
            .iter()
            .any(|i| matches!(i, lopdf::Object::String(s, _) if !s.is_empty())),
        _ => false,
    };
    let pages: Vec<lopdf::ObjectId> = doc.page_iter().collect();
    pages.into_iter().any(|id| {
        doc.get_page_content(id)
            .ok()
            .and_then(|data| lopdf::content::Content::decode(&data).ok())
            .is_some_and(|content| {
                content.operations.iter().any(|op| {
                    matches!(op.operator.as_str(), "Tj" | "TJ" | "'" | "\"")
                        && op.operands.iter().any(drawn)
                })
            })
    })
}

/// The PDF text layer. A font's ToUnicode often maps glyphs to look-alike radical and ligature code points; whatever `pdf_extract`
/// reads out is accepted and mapped back to the character it displays ([`crate::lookalikes`])
pub fn pdf(bytes: &[u8]) -> anyhow::Result<String> {
    let text = text_layer(bytes)?;
    Ok(match crate::lookalikes::plain(&text) {
        std::borrow::Cow::Borrowed(_) => text,
        std::borrow::Cow::Owned(plain) => plain,
    })
}

/// Takes the text layer. If that fails, hand off to the external `pdftotext`: it runs as a separate process, so even if it crashes, it cannot take
/// a worker thread down with it.
///
/// When the fallback also comes back empty-handed, [`draws_text`] decides what to say: a file that never drew any text is a scanned document and goes to
/// the OCR path (`NeedsReader`); a file that drew text but yielded none is one we cannot read, so the first parser's own message is raised
/// as-is. That message needs to be accurate: saying "no text layer" would send the person off to configure an OCR service, while
/// the file's text layer is right there (#739: two programs share the name `pdftotext`: Xpdf's, and Poppler's built without
/// CJK CMap data, both silently return nothing)
fn text_layer(bytes: &[u8]) -> anyhow::Result<String> {
    let extracted = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));
    let original_error = match extracted {
        Ok(Ok(text)) if !text.trim().is_empty() => return Ok(text),
        Ok(Ok(_)) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(panic) => Some(
            panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| {
                    panic
                        .downcast_ref::<&str>()
                        .map(|message| (*message).to_owned())
                })
                .unwrap_or_else(|| "PDF parser panicked".to_owned()),
        ),
    };

    let fallback = pdf_with_poppler(bytes);
    if let Ok(text) = &fallback {
        if !text.trim().is_empty() {
            return Ok(text.clone());
        }
    }
    // The first extractor read the file and found no text layer: the OCR path takes it from here.
    let Some(original) = original_error else {
        return Ok(String::new());
    };
    // It could not read the file. A file that draws no glyphs is a scan even so.
    if !draws_text(bytes) {
        return Ok(String::new());
    }
    match fallback {
        Ok(_) => anyhow::bail!(
            "PDF text-layer extraction failed: {original}; the fallback read no text from a file \
that draws it (is pdftotext Poppler, with its CJK CMap data?)"
        ),
        Err(error) => anyhow::bail!(
            "PDF text-layer extraction failed: {original}; Poppler fallback failed: {error:#}"
        ),
    }
}

fn pdf_with_poppler(bytes: &[u8]) -> anyhow::Result<String> {
    let mut input = tempfile::NamedTempFile::new().context("Could not create a temporary PDF")?;
    input
        .write_all(bytes)
        .context("Could not write the temporary PDF")?;
    input.flush().context("Could not flush the temporary PDF")?;

    let output = Command::new("pdftotext")
        .args(["-enc", "UTF-8", "-nopgbrk"])
        .arg(input.path())
        .arg("-")
        .output()
        .context("Could not run pdftotext")?;
    if !output.status.success() {
        anyhow::bail!(
            "pdftotext exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("pdftotext returned invalid UTF-8")
}

/// docx: unzips word/document.xml. Body text is taken from w:t; a w:p outside tables is one paragraph, followed by a blank line;
/// w:br/w:cr inside a paragraph are still single line breaks. Tables (w:tbl) are collected into a grid and handed to `table::grid_or_lines`, going through the same rendering as HTML tables
/// (a grid that is not a table is written out line by line): w:gridSpan spans columns, the continuation cells of vertical merges (w:vMerge) are left empty, and a paragraph's left indent in a cell (w:ind)
/// counts as padding (section lines are folded into their labels by it). A table nested in a cell is treated as that cell's text. A heading (a paragraph with an outline level,
/// see [`docx_heading_styles`]) is written as a Markdown heading.
pub fn docx(bytes: &[u8]) -> anyhow::Result<String> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).context("Malformed docx structure")?;
    let xml =
        read_zip_entry(&mut archive, "word/document.xml").context("Malformed docx structure")?;
    // If the style sheet is missing or unreadable, the body is still read; only headings defined by style go unrecognized
    let headings = read_zip_entry(&mut archive, "word/styles.xml")
        .ok()
        .and_then(|styles| docx_heading_styles(&styles).ok())
        .unwrap_or_default();
    docx_xml_to_text(&xml, &headings)
}

/// Which paragraph styles are headings, and at which level (style id → 1–9).
///
/// In Word, headings are paragraphs with an outline level: the navigation pane and the table of contents list them by it, and the style definition sets `w:outlineLvl`
/// (counted from 0; 9 is body text). The style id is unreliable: English Word writes `Heading1`, Chinese Word writes `1` and `2`,
/// and the body style is `a`. Built-in style names, however, are always written in English as `heading 1`. So detection goes in this order: the style's
/// own `w:outlineLvl`, then a name of `heading N`, then up through `w:basedOn` (custom heading styles are usually based on a built-in heading).
/// `Title` and `toc 1` have no outline level, so they do not count as headings
pub(crate) fn docx_heading_styles(
    xml: &str,
) -> anyhow::Result<std::collections::HashMap<String, u8>> {
    #[derive(Default)]
    struct Style {
        name: Option<String>,
        based_on: Option<String>,
        outline: Option<u8>,
    }
    let attr = |e: &quick_xml::events::BytesStart<'_>, name: &str| -> Option<String> {
        e.attributes()
            .flatten()
            .find(|a| a.key.as_ref() == name)
            .map(|a| a.value.to_string())
    };
    let mut reader = Reader::from_str(xml);
    let mut styles: std::collections::HashMap<String, Style> = std::collections::HashMap::new();
    let mut current: Option<(String, Style)> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == "w:style" => {
                current = (attr(&e, "w:type").as_deref() == Some("paragraph"))
                    .then(|| attr(&e, "w:styleId"))
                    .flatten()
                    .map(|id| (id, Style::default()));
            }
            Ok(Event::End(e)) if e.name().as_ref() == "w:style" => {
                if let Some((id, style)) = current.take() {
                    styles.insert(id, style);
                }
            }
            Ok(Event::Start(e) | Event::Empty(e)) => {
                if let Some((_, style)) = current.as_mut() {
                    match e.name().as_ref() {
                        "w:name" => style.name = attr(&e, "w:val"),
                        "w:basedOn" => style.based_on = attr(&e, "w:val"),
                        "w:outlineLvl" => {
                            style.outline = attr(&e, "w:val").and_then(|v| v.parse().ok());
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!("XML parse error: {e}"),
            _ => {}
        }
    }
    let level_of = |id: &str| -> Option<u8> {
        let mut id = id;
        // basedOn is followed up to ten steps at most: Word itself never chains this long, and a cyclic one that was written wrong stops here too
        for _ in 0..10 {
            let style = styles.get(id)?;
            if let Some(outline) = style.outline {
                return outline_level(outline);
            }
            if let Some(n) = style.name.as_deref().and_then(heading_number) {
                return Some(n);
            }
            id = style.based_on.as_deref()?;
        }
        None
    };
    Ok(styles
        .keys()
        .filter_map(|id| Some((id.clone(), level_of(id)?)))
        .collect())
}

/// Outline levels 0–8 are heading levels 1–9; 9 is body text
fn outline_level(outline: u8) -> Option<u8> {
    (outline <= 8).then_some(outline + 1)
}

/// Built-in heading style names `heading 1` … `heading 9`, case-insensitive
fn heading_number(name: &str) -> Option<u8> {
    let name = name.trim().to_ascii_lowercase();
    let n: u8 = name.strip_prefix("heading")?.trim().parse().ok()?;
    (1..=9).contains(&n).then_some(n)
}

type GridRow = Vec<(String, usize, u32)>;

pub(crate) fn docx_xml_to_text(
    xml: &str,
    headings: &std::collections::HashMap<String, u8>,
) -> anyhow::Result<String> {
    let mut reader = Reader::from_str(xml);
    let mut out = String::new();
    let mut in_text = false;
    // Several layers of w:tbl deep; only the outermost is collected into a grid
    let mut depth = 0usize;
    let mut rows: Vec<GridRow> = Vec::new();
    let mut row: GridRow = Vec::new();
    let mut cell: Option<(String, usize, u32)> = None;
    // Whether a paragraph in the body (not in a table, not a paragraph inside a text box) is a heading. `paragraphs` counts how many layers of
    // w:p are nested, and `para_start` is where this paragraph starts in out. The level first looks at the paragraph's own w:outlineLvl,
    // then at its style; each is only honored at first occurrence, and a style recorded in w:pPrChange (from before a revision) does not count.
    // A second stack records where each paragraph starts, so that at the paragraph's end an empty paragraph is dropped and only one blank line is kept between non-empty ones. A text box
    // can nest w:p inside its host paragraph again; the stack lets the inner paragraphs each close on their own, while the heading check still looks only at the outermost one.
    let mut paragraphs = 0usize;
    let mut para_start = 0usize;
    let mut paragraph_starts: Vec<usize> = Vec::new();
    let mut own_level: Option<Option<u8>> = None;
    let mut style_level: Option<Option<u8>> = None;
    let mut in_revision = false;
    let attr = |e: &quick_xml::events::BytesStart<'_>, name: &str| -> Option<String> {
        e.attributes()
            .flatten()
            .find(|a| a.key.as_ref() == name)
            .map(|a| a.value.to_string())
    };
    // Whether each mc:AlternateContent has already read one drawing form; once read, the other forms are skipped as a whole subtree,
    // `skipping` is how many levels of the skipped subtree are still unclosed
    let mut alternates: Vec<bool> = Vec::new();
    let mut skipping = 0usize;
    loop {
        let event = reader.read_event();
        if skipping > 0 {
            match event {
                Ok(Event::Start(_)) => skipping += 1,
                Ok(Event::End(_)) => skipping -= 1,
                Ok(Event::Eof) => break,
                Err(e) => anyhow::bail!("XML parse error: {e}"),
                _ => {}
            }
            continue;
        }
        match event {
            // Since Word 2010, a text box is written twice: `mc:Choice` holds the wps shape, and `mc:Fallback` holds the one for
            // older Word's VML, with the text in each. Reading both brings the same text in twice, and extraction counts it twice.
            // Following the markup compatibility rules, only one form is taken: the first Choice,
            // and the later Choice and Fallback are not read
            Ok(Event::Start(e)) if e.name().as_ref() == "mc:AlternateContent" => {
                alternates.push(false);
            }
            Ok(Event::End(e)) if e.name().as_ref() == "mc:AlternateContent" => {
                alternates.pop();
            }
            Ok(Event::Start(e)) if matches!(e.name().as_ref(), "mc:Choice" | "mc:Fallback") => {
                match alternates.last_mut() {
                    Some(taken) if *taken => skipping = 1,
                    Some(taken) => *taken = true,
                    None => {}
                }
            }
            Ok(Event::Start(e) | Event::Empty(e))
                if matches!(e.name().as_ref(), "w:pStyle" | "w:outlineLvl") =>
            {
                if paragraphs == 1 && cell.is_none() && !in_revision {
                    if e.name().as_ref() == "w:pStyle" {
                        style_level.get_or_insert_with(|| {
                            attr(&e, "w:val").and_then(|id| headings.get(&id).copied())
                        });
                    } else {
                        own_level.get_or_insert_with(|| {
                            attr(&e, "w:val")
                                .and_then(|v| v.parse().ok())
                                .and_then(outline_level)
                        });
                    }
                }
            }
            Ok(Event::Start(e)) if e.name().as_ref() == "w:pPrChange" => in_revision = true,
            Ok(Event::End(e)) if e.name().as_ref() == "w:pPrChange" => in_revision = false,
            Ok(Event::Start(e) | Event::Empty(e))
                if matches!(e.name().as_ref(), "w:br" | "w:cr") =>
            {
                // Cells are flattened later, but an explicit break still separates
                // text: dropping it turns two readings such as 10 and 20 into 1020.
                match cell.as_mut() {
                    Some(c) => c.0.push(' '),
                    None => out.push('\n'),
                }
            }
            Ok(Event::Start(e)) => match e.name().as_ref() {
                "w:t" => in_text = true,
                "w:p" => {
                    paragraphs += 1;
                    if cell.is_none() {
                        paragraph_starts.push(out.len());
                    }
                    if paragraphs == 1 && cell.is_none() {
                        para_start = out.len();
                        own_level = None;
                        style_level = None;
                    }
                }
                "w:tbl" => {
                    depth += 1;
                    if depth == 1 {
                        rows.clear();
                    }
                }
                "w:tr" if depth == 1 => row = Vec::new(),
                "w:tc" if depth == 1 => cell = Some((String::new(), 1, 0)),
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.name().as_ref() {
                "w:gridSpan" => {
                    if let Some(c) = cell.as_mut() {
                        if let Some(v) = attr(&e, "w:val").and_then(|v| v.parse::<usize>().ok()) {
                            c.1 = v.max(1);
                        }
                    }
                }
                // Left indent of the first paragraph in a cell (twips, 1/20 pt)
                "w:ind" => {
                    if let Some(c) = cell.as_mut().filter(|c| c.0.is_empty()) {
                        if let Some(v) = attr(&e, "w:left")
                            .or_else(|| attr(&e, "w:start"))
                            .and_then(|v| v.parse::<u32>().ok())
                        {
                            c.2 = v / 20;
                        }
                    }
                }
                "w:tab" => match cell.as_mut() {
                    Some(c) => c.0.push(' '),
                    None => out.push(' '),
                },
                // Word's non-breaking hyphen (Ctrl+Shift+-) is not a w:t character; it is an element: it is still drawn on the page as a
                // hyphen, just not used to break a line here. Dropping it would read 2024‑01‑15 as 20240115, and 010‑62345678 as
                // 01062345678. Written as a regular hyphen, dates and phone numbers are recognized.
                "w:noBreakHyphen" => match cell.as_mut() {
                    Some(c) => c.0.push('-'),
                    None => out.push('-'),
                },
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                "w:t" => in_text = false,
                "w:p" => {
                    match cell.as_mut() {
                        Some(c) => c.0.push(' '),
                        None => {
                            let start = paragraph_starts.pop().unwrap_or(out.len());
                            // A heading is written as a one-line Markdown heading: the chunker uses it to prefix each chunk with the headings of every level it sits under,
                            // and date interpretation splits sections by the heading lines in a chunk (0064 decision 1, the section revised in cut 2). Without
                            // it, every section's date in a Word file would be computed as the first section's
                            let level = own_level.unwrap_or(style_level.flatten());
                            if let Some(level) = level.filter(|_| paragraph_starts.is_empty()) {
                                let title = out[para_start..]
                                    .split_whitespace()
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                if !title.is_empty() {
                                    out.truncate(para_start);
                                    out.push_str(&"#".repeat(usize::from(level.min(6))));
                                    out.push(' ');
                                    out.push_str(&title);
                                }
                            }
                            // One Word paragraph is one Markdown paragraph. On close, trailing whitespace inside the paragraph is removed first;
                            // an empty paragraph leaves no trace; a non-empty one adds only one blank line, and single line breaks inside it from w:br are unaffected.
                            let end = start + out[start..].trim_end().len();
                            if end == start {
                                out.truncate(start);
                            } else {
                                out.truncate(end);
                                out.push_str("\n\n");
                            }
                        }
                    }
                    paragraphs = paragraphs.saturating_sub(1);
                }
                "w:tc" if depth == 1 => {
                    if let Some(c) = cell.take() {
                        row.push(c);
                    }
                }
                "w:tr" if depth == 1 => rows.push(std::mem::take(&mut row)),
                "w:tbl" => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        if let Some(md) = crate::table::grid_or_lines(&rows, false) {
                            if !out.is_empty() {
                                let end = out.trim_end().len();
                                out.truncate(end);
                                out.push_str("\n\n");
                            }
                            out.push_str(&md);
                            out.push_str("\n\n");
                        }
                        rows.clear();
                    }
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_text => {
                let s = t.xml_content(quick_xml::XmlVersion::Implicit1_0);
                match cell.as_mut() {
                    Some(c) => c.0.push_str(&s),
                    None => out.push_str(&s),
                }
            }
            Ok(Event::CData(t)) if in_text => {
                let s = t.xml_content(quick_xml::XmlVersion::Implicit1_0);
                match cell.as_mut() {
                    Some(c) => c.0.push_str(&s),
                    None => out.push_str(&s),
                }
            }
            // Character references are separate events; dropping them would turn body text like R&amp;D into RD.
            // An unrecognized reference is kept as-is, and must not make the whole document fail to import.
            Ok(Event::GeneralRef(e)) if in_text => {
                let reference = format!("&{};", e.into_inner());
                let s = quick_xml::escape::unescape(&reference)
                    .unwrap_or(std::borrow::Cow::Borrowed(&reference));
                match cell.as_mut() {
                    Some(c) => c.0.push_str(&s),
                    None => out.push_str(&s),
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!("XML parse error: {e}"),
            _ => {}
        }
    }
    Ok(out)
}

/// PPTX: extract a:t text in the presentation's logical slide order. Tables under a:tbl
/// become Markdown grids through the same renderer as DOCX and spreadsheet tables.
///
/// The first slide is the opening of this deck and is written before any heading; from the second slide on, each slide is a section (`## Slide N`). The date on the cover
/// states for the whole deck, while a date's scope follows the heading it sits under (0064). If the cover were a section too, its date would cover only the cover, and none of the later slides would have a source.
/// Word's heading block also sits before the first heading, and the cover does the same here.
pub fn pptx(bytes: &[u8]) -> anyhow::Result<String> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes.to_vec())).context("Failed to unzip pptx")?;
    let slides = if let Some(names) = pptx_order(&mut archive)? {
        names
            .into_iter()
            .enumerate()
            .map(|(i, name)| (i as u32 + 1, name))
            .collect()
    } else {
        let mut slides: Vec<(u32, String)> = Vec::new();
        for i in 0..archive.len() {
            let name = archive.by_index(i)?.name().to_string();
            if let Some(num) = name
                .strip_prefix("ppt/slides/slide")
                .and_then(|s| s.strip_suffix(".xml"))
                .and_then(|s| s.parse::<u32>().ok())
            {
                slides.push((num, name));
            }
        }
        slides.sort();

        slides
    };

    let mut out = String::new();
    for (num, name) in slides {
        let xml = pptx_part(&mut archive, &name)?;
        let text = pptx_xml_to_text(&xml)?;
        if text.trim().is_empty() {
            continue;
        }
        if num == 1 {
            out.push_str(&format!("{text}\n"));
        } else {
            out.push_str(&format!("\n## Slide {num}\n{text}\n"));
        }
    }
    Ok(out)
}

const PPT_NS: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE_REL_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

fn pptx_part(archive: &mut zip::ZipArchive<Cursor<Vec<u8>>>, name: &str) -> anyhow::Result<String> {
    let mut xml = String::new();
    archive
        .by_name(name)
        .with_context(|| format!("Missing PPTX part: {name}"))?
        .read_to_string(&mut xml)?;
    Ok(xml)
}

// Relationship targets are package URIs, never paths to open or URLs to fetch.
fn pptx_target(source: &str, target: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !target.contains([':', '\\', '?', '#']) && !target.starts_with("//"),
        "Invalid PPTX part target"
    );
    let target = percent_encoding::percent_decode_str(target).decode_utf8()?;
    anyhow::ensure!(
        !target.contains([':', '\\', '?', '#', '\0']) && !target.starts_with("//"),
        "Invalid PPTX part target"
    );
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        source
            .rsplit_once('/')
            .map(|(dir, _)| dir.split('/').collect())
            .unwrap_or_default()
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                anyhow::ensure!(parts.pop().is_some(), "PPTX target escapes package");
            }
            _ => parts.push(part),
        }
    }
    anyhow::ensure!(!parts.is_empty(), "Empty PPTX part target");
    Ok(parts.join("/"))
}

// Only the requested relationship type is returned. Other types (notes, masters,
// hyperlinks) cannot become slides. Reject ambiguity rather than silently losing pages.
fn pptx_relationships(
    xml: &str,
    kind: &str,
    source: &str,
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    use quick_xml::name::{Namespace, ResolveResult};
    let mut reader = quick_xml::NsReader::from_str(xml);
    let mut depth = 0;
    let mut root = false;
    let mut ids = std::collections::HashSet::new();
    let mut targets = std::collections::HashMap::new();
    loop {
        let event = reader.read_event()?;
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                let (ns, local) = reader.resolver().resolve_element(e.name());
                let package = ns == ResolveResult::Bound(Namespace(PACKAGE_REL_NS));
                if depth == 0 {
                    anyhow::ensure!(
                        !root && package && local.as_ref() == "Relationships",
                        "Invalid PPTX relationships root"
                    );
                    root = true;
                } else if depth == 1 && package && local.as_ref() == "Relationship" {
                    let attrs: std::collections::HashMap<_, _> = e
                        .attributes()
                        .map(|a| {
                            let a = a?;
                            Ok((
                                a.key.as_ref().to_string(),
                                a.normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                                    .into_owned(),
                            ))
                        })
                        .collect::<anyhow::Result<_>>()?;
                    let id = attrs.get("Id").context("PPTX relationship has no Id")?;
                    anyhow::ensure!(ids.insert(id.clone()), "Duplicate PPTX relationship Id");
                    let ty = attrs.get("Type").context("PPTX relationship has no Type")?;
                    if ty == &format!("{REL_NS}/{kind}")
                        || ty
                            == &format!(
                                "http://purl.oclc.org/ooxml/officeDocument/relationships/{kind}"
                            )
                    {
                        anyhow::ensure!(
                            attrs.get("TargetMode").is_none_or(|m| m == "Internal"),
                            "External PPTX {kind} relationship is unsupported"
                        );
                        let target = attrs
                            .get("Target")
                            .context("PPTX relationship has no Target")?;
                        targets.insert(id.clone(), pptx_target(source, target)?);
                    }
                }
                if matches!(event, Event::Start(_)) {
                    depth += 1;
                }
            }
            Event::End(_) => depth -= 1,
            Event::Eof => {
                anyhow::ensure!(root && depth == 0, "Incomplete PPTX relationships");
                break;
            }
            _ => {}
        }
    }
    Ok(targets)
}

fn pptx_slide_ids(xml: &str) -> anyhow::Result<Vec<String>> {
    use quick_xml::name::{Namespace, ResolveResult};
    let mut reader = quick_xml::NsReader::from_str(xml);
    let mut depth = 0;
    let mut root = false;
    let mut in_list = false;
    let mut seen_list = false;
    let mut ids = Vec::new();
    loop {
        let event = reader.read_event()?;
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                let (ns, local) = reader.resolver().resolve_element(e.name());
                let presentation = matches!(
                    ns,
                    ResolveResult::Bound(Namespace(
                        PPT_NS | "http://purl.oclc.org/ooxml/presentationml/main"
                    ))
                );
                if depth == 0 {
                    anyhow::ensure!(
                        !root && presentation && local.as_ref() == "presentation",
                        "Invalid PPTX presentation root"
                    );
                    root = true;
                } else if depth == 1 && presentation && local.as_ref() == "sldIdLst" {
                    anyhow::ensure!(!seen_list, "Duplicate PPTX slide list");
                    seen_list = true;
                    in_list = matches!(event, Event::Start(_));
                } else if depth == 2 && in_list {
                    anyhow::ensure!(
                        presentation && local.as_ref() == "sldId",
                        "Invalid PPTX slide-list entry"
                    );
                    let mut id = None;
                    for a in e.attributes() {
                        let a = a?;
                        let (ns, local) = reader.resolver().resolve_attribute(a.key);
                        if local.as_ref() == "id"
                            && matches!(
                                ns,
                                ResolveResult::Bound(Namespace(
                                    REL_NS
                                        | "http://purl.oclc.org/ooxml/officeDocument/relationships"
                                ))
                            )
                        {
                            anyhow::ensure!(id.is_none(), "Duplicate PPTX slide relationship");
                            id = Some(
                                a.normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                                    .into_owned(),
                            );
                        }
                    }
                    ids.push(id.context("PPTX slide has no relationship id")?);
                }
                if matches!(event, Event::Start(_)) {
                    depth += 1;
                }
            }
            Event::End(_) => {
                depth -= 1;
                if depth == 1 {
                    in_list = false;
                }
            }
            Event::Eof => {
                anyhow::ensure!(root && depth == 0, "Incomplete PPTX presentation");
                break;
            }
            _ => {}
        }
    }
    Ok(ids)
}

fn pptx_order(
    archive: &mut zip::ZipArchive<Cursor<Vec<u8>>>,
) -> anyhow::Result<Option<Vec<String>>> {
    let has_root = archive.file_names().any(|n| n == "_rels/.rels");
    let main = if has_root {
        let xml = pptx_part(archive, "_rels/.rels")?;
        let mut roots = pptx_relationships(&xml, "officeDocument", "")?.into_values();
        let main = roots
            .next()
            .context("PPTX has no presentation relationship")?;
        anyhow::ensure!(roots.next().is_none(), "PPTX has multiple presentations");
        main
    } else if archive.file_names().any(|n| n == "ppt/presentation.xml") {
        "ppt/presentation.xml".to_string()
    } else {
        // Preserve the existing behavior for legacy partial packages. If a
        // manifest exists, errors must never fall back to guessed filename order.
        return Ok(None);
    };
    let ids = pptx_slide_ids(&pptx_part(archive, &main)?)?;
    if ids.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let (dir, file) = main.rsplit_once('/').unwrap_or(("", main.as_str()));
    let rels = if dir.is_empty() {
        format!("_rels/{file}.rels")
    } else {
        format!("{dir}/_rels/{file}.rels")
    };
    let relationships = pptx_relationships(&pptx_part(archive, &rels)?, "slide", &main)?;
    let slides = ids
        .into_iter()
        .map(|id| {
            relationships
                .get(&id)
                .cloned()
                .with_context(|| format!("Missing PPTX slide relationship: {id}"))
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(Some(slides))
}

/// xlsx / xls / ods: calamine reads every format, and each sheet is output as a tab-separated table (first 2000 rows only).
pub fn spreadsheet(bytes: &[u8]) -> anyhow::Result<(String, Vec<crate::ParseWarning>)> {
    use calamine::Reader as _;
    let mut workbook = calamine::open_workbook_auto_from_rs(Cursor::new(bytes.to_vec()))
        .context("Failed to open spreadsheet")?;
    let mut out = String::new();
    let mut warnings = Vec::new();
    for sheet_name in workbook.sheet_names() {
        let (text, rows_read, rows_total) = match &mut workbook {
            calamine::Sheets::Xlsx(book) => {
                let Ok(sheet) = xlsx_sheet(book, &sheet_name, XLSX_SEGMENT_LIMIT) else {
                    continue;
                };
                sheet
            }
            _ => {
                let Ok(range) = workbook.worksheet_range(&sheet_name) else {
                    continue;
                };
                let merges = match &mut workbook {
                    calamine::Sheets::Xls(book) => book
                        .merge_cells_by_sheet_name(&sheet_name)
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                let (rows, headers) = spreadsheet_grid(&range, &merges);
                let text = crate::table::render_grid_with_headers(&rows, headers)
                    .map(|text| format!("{text}\n"))
                    .unwrap_or_else(|| {
                        let mut text = String::new();
                        for row in &rows {
                            let cells: Vec<&str> = row.iter().map(|(s, _, _)| s.as_str()).collect();
                            text.push_str(&cells.join("\t"));
                            text.push('\n');
                        }
                        text
                    });
                (
                    text,
                    range.height().min(SPREADSHEET_ROW_LIMIT),
                    range.height(),
                )
            }
        };
        if rows_read == 0 {
            continue;
        }
        out.push_str(&format!("\n# Sheet: {sheet_name}\n\n"));
        if rows_total > rows_read {
            warnings.push(crate::ParseWarning {
                kind: crate::ParseWarning::SPREADSHEET_ROWS_TRUNCATED,
                detail: serde_json::json!({
                    "sheet": sheet_name,
                    "rows_read": rows_read,
                    "rows_total": rows_total,
                    "rows_omitted": rows_total - rows_read,
                }),
            });
        }
        out.push_str(&text);
    }
    Ok((out, warnings))
}

fn xlsx_sheet(
    book: &mut calamine::Xlsx<Cursor<Vec<u8>>>,
    sheet: &str,
    budget: usize,
) -> anyhow::Result<(String, usize, usize)> {
    let mut start_row = u32::MAX;
    let mut end_row = 0;
    let mut row_counts = vec![0usize; SPREADSHEET_ROW_LIMIT];
    let mut reader = book.worksheet_cells_reader(sheet)?;
    // Keep only the first 2,000 physical rows' counts, even if XML cells arrive
    // out of order. The full row bounds survive so truncation remains honest.
    while let Some(cell) = reader.next_cell()? {
        if cell.get_value() == &calamine::DataRef::Empty {
            continue;
        }
        let (row, col) = cell.get_position();
        anyhow::ensure!(
            row < 1_048_576 && col < 16_384,
            "Cell exceeds the Excel worksheet limits"
        );
        if row < start_row {
            let shift = (start_row - row).min(SPREADSHEET_ROW_LIMIT as u32) as usize;
            row_counts.rotate_right(shift);
            row_counts[..shift].fill(0);
            start_row = row;
        }
        end_row = end_row.max(row);
        let offset = (row - start_row) as usize;
        if let Some(count) = row_counts.get_mut(offset) {
            // Repeated coordinates overwrite values below, so even malformed
            // XML cannot retain more than one physical row's worth of cells.
            *count = (*count + 1).min(16_384);
        }
    }
    drop(reader);
    if start_row == u32::MAX {
        return Ok((String::new(), 0, 0));
    }
    let rows_total = (end_row - start_row + 1) as usize;
    let mut height = rows_total.min(SPREADSHEET_ROW_LIMIT);
    let mut count = 0;
    for (row, &cells) in row_counts[..height].iter().enumerate() {
        if cells > budget - count {
            height = row;
            break;
        }
        count += cells;
    }
    // Counts bound this allocation before any source values are retained. A
    // distant note adds one entry, not thousands of empty Data/String objects.
    let mut values = vec![std::collections::BTreeMap::<u32, String>::new(); height];
    let mut reader = book.worksheet_cells_reader(sheet)?;
    while let Some(cell) = reader.next_cell()? {
        let (row, col) = cell.get_position();
        let offset = row.saturating_sub(start_row) as usize;
        if row >= start_row && offset < height && cell.get_value() != &calamine::DataRef::Empty {
            let text = spreadsheet_cell_text(Some(&calamine::Data::from(cell.get_value().clone())));
            values[offset].insert(col, text);
        }
    }
    drop(reader);
    let merges = book.merge_cells_by_sheet_name(sheet).unwrap_or_default();
    loop {
        let start_col = values
            .iter()
            .filter_map(|row| row.keys().next())
            .min()
            .copied()
            .unwrap();
        let end_col = values
            .iter()
            .filter_map(|row| row.keys().next_back())
            .max()
            .copied()
            .unwrap();
        let width = (end_col - start_col + 1) as usize;
        let text_at = |row: usize, col: usize| {
            values[row]
                .get(&(start_col + col as u32))
                .cloned()
                .unwrap_or_default()
        };
        let regions = spreadsheet_regions(
            (start_row, start_col),
            (start_row + height as u32 - 1, end_col),
            &merges,
            text_at,
        );
        let mut owner = vec![usize::MAX; width];
        let mut rows = Vec::with_capacity(height);
        let mut segments = 0;
        for row in 0..height {
            let cells = spreadsheet_row(
                row,
                width,
                &regions,
                &mut owner,
                |col| text_at(row, col),
                true,
            );
            if cells.len() > budget - segments {
                break;
            }
            segments += cells.len();
            rows.push(cells);
        }
        if rows.len() != height {
            height = rows.len();
            values.truncate(height);
            // Dropped rows cannot leave a distant column in the kept window's
            // geometry. Reproject merges too, retaining their physical spans.
            continue;
        }
        let headers = spreadsheet_headers(&regions, &rows);
        let text = match crate::table::render_grid_with_headers(&rows, headers) {
            Some(text) => format!("{text}\n"),
            None => {
                // Preserve the old TSV fallback exactly, but materialize only
                // one dense row at a time rather than the whole rectangle.
                let mut text = String::new();
                for row in 0..height {
                    let cells = spreadsheet_row(
                        row,
                        width,
                        &regions,
                        &mut owner,
                        |col| text_at(row, col),
                        false,
                    );
                    let cells: Vec<&str> = cells.iter().map(|(s, _, _)| s.as_str()).collect();
                    text.push_str(&cells.join("\t"));
                    text.push('\n');
                }
                text
            }
        };
        return Ok((text, height, rows_total));
    }
}

#[derive(Clone)]
struct GridMerge {
    start: (usize, usize),
    end: (usize, usize),
    text: String,
}

/// Lays the calamine rectangular range and merge info out as [`GridRow`]. A horizontal merge becomes a cell with one
/// span, and a vertical merge carries the anchor's value into every row it covers; values outside the anchor are no longer separate cells.
fn spreadsheet_grid(
    range: &calamine::Range<calamine::Data>,
    merges: &[calamine::Dimensions],
) -> (Vec<GridRow>, Option<std::ops::Range<usize>>) {
    let Some((start_row, start_col)) = range.start() else {
        return (Vec::new(), None);
    };
    let height = range.height().min(SPREADSHEET_ROW_LIMIT);
    let width = range.width();
    if height == 0 || width == 0 {
        return (Vec::new(), None);
    }
    let text_at = |row, col| spreadsheet_cell_text(range.get((row, col)));
    let regions = spreadsheet_regions(
        (start_row, start_col),
        (start_row + height as u32 - 1, start_col + width as u32 - 1),
        merges,
        text_at,
    );
    let mut owner = vec![usize::MAX; width];
    let rows: Vec<_> = (0..height)
        .map(|row| {
            spreadsheet_row(
                row,
                width,
                &regions,
                &mut owner,
                |col| text_at(row, col),
                false,
            )
        })
        .collect();
    let headers = spreadsheet_headers(&regions, &rows);
    (rows, headers)
}

fn spreadsheet_regions(
    (start_row, start_col): (u32, u32),
    (end_row, end_col): (u32, u32),
    merges: &[calamine::Dimensions],
    text_at: impl Fn(usize, usize) -> String,
) -> Vec<GridMerge> {
    let mut regions = Vec::new();
    for merge in merges {
        if merge.start.0 > merge.end.0
            || merge.start.1 > merge.end.1
            || merge.end.0 < start_row
            || merge.end.1 < start_col
            || merge.start.0 > end_row
            || merge.start.1 > end_col
        {
            continue;
        }
        let start = (
            (merge.start.0.max(start_row) - start_row) as usize,
            (merge.start.1.max(start_col) - start_col) as usize,
        );
        let end = (
            (merge.end.0.min(end_row) - start_row) as usize,
            (merge.end.1.min(end_col) - start_col) as usize,
        );
        regions.push(GridMerge {
            start,
            end,
            text: text_at(start.0, start.1),
        });
    }
    regions
}

fn spreadsheet_row(
    row: usize,
    width: usize,
    regions: &[GridMerge],
    owner: &mut [usize],
    text_at: impl Fn(usize) -> String,
    compact_empty: bool,
) -> GridRow {
    // One owner row keeps the old last-merge-wins projection and anchor jumps,
    // without allocating height * width indices. XLSX bounds width at XFD.
    owner.fill(usize::MAX);
    for (index, region) in regions.iter().enumerate() {
        if region.start.0 <= row && row <= region.end.0 {
            owner[region.start.1..=region.end.1].fill(index);
        }
    }
    let mut cells: GridRow = Vec::new();
    let mut col = 0;
    while col < width {
        let index = owner[col];
        if index == usize::MAX {
            let text = text_at(col);
            match (compact_empty && text.is_empty(), cells.last_mut()) {
                (true, Some((previous, span, _))) if previous.is_empty() => *span += 1,
                _ => cells.push((text, 1, 0)),
            }
            col += 1;
        } else {
            let region = &regions[index];
            if col == region.start.1 {
                cells.push((region.text.clone(), region.end.1 - region.start.1 + 1, 0));
                col = region.end.1 + 1;
            } else {
                col += 1;
            }
        }
    }
    cells
}

fn spreadsheet_cell_text(cell: Option<&calamine::Data>) -> String {
    match cell {
        None | Some(calamine::Data::Empty) => String::new(),
        Some(calamine::Data::DateTime(d)) => excel_date(d),
        Some(calamine::Data::Float(f)) => excel_number(*f),
        Some(calamine::Data::DurationIso(d)) => ods_time(d).unwrap_or_else(|| d.clone()),
        Some(other) => other.to_string(),
    }
}

/// ODS time cells store an ISO duration (`office:time-value="PT09H30M00S"`), and it does not tell a clock time within a day from
/// a cumulative duration. Following the two xlsx cell forms: under a day it is written as a clock time (`09:30`), and at a day or more it is written as cumulative
/// h:mm:ss (`36:15:00`). LibreOffice often writes seconds as `59.999999998S`, which is rounded to the nearest whole second
fn ods_time(iso: &str) -> Option<String> {
    let iso = iso.trim();
    let (sign, body) = match iso.strip_prefix('-') {
        Some(rest) => ("-", rest.strip_prefix('P')?),
        None => ("", iso.strip_prefix('P')?),
    };
    let (days, mut clock) = body.split_once('T').unwrap_or((body, ""));
    let mut seconds = match days.strip_suffix('D') {
        Some(n) => n.parse::<f64>().ok()? * 86_400.0,
        None if days.is_empty() => 0.0,
        None => return None,
    };
    for (unit, scale) in [('H', 3_600.0), ('M', 60.0), ('S', 1.0)] {
        if let Some((n, tail)) = clock.split_once(unit) {
            seconds += n.parse::<f64>().ok()? * scale;
            clock = tail;
        }
    }
    if !clock.is_empty() || !seconds.is_finite() {
        return None;
    }
    let seconds = seconds.round() as u64;
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    Some(if h >= 24 || !sign.is_empty() {
        format!("{sign}{h}:{m:02}:{s:02}")
    } else if s == 0 {
        format!("{h:02}:{m:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    })
}

/// The header starts at the first row where at least two cells have text; a vertical merge in this block that reaches a row also makes that row a header.
fn spreadsheet_headers(regions: &[GridMerge], rows: &[GridRow]) -> Option<std::ops::Range<usize>> {
    let start = rows
        .iter()
        .position(|row| row.iter().filter(|(text, _, _)| !text.is_empty()).count() >= 2)?;
    let mut end = start + 1;
    while let Some(next) = regions
        .iter()
        .filter(|region| region.start.0 >= start && region.start.0 < end && region.end.0 >= end)
        .map(|region| region.end.0 + 1)
        .max()
    {
        end = next;
    }
    Some(start..end.min(rows.len()))
}

/// Date cells are written as the date they display.
///
/// calamine recognized the date from the number format attached to the cell, but handed over the number Excel stores: a day count from 1899-12-30
/// (for workbooks in the 1904 date system, from 1904-01-01), and the fractional part is the time of day. The
/// Display writes this number as-is, so 2024-01-15 became 45306: the document no longer has the date, and time extraction cannot see
/// it; the model takes it for a plain quantity. The same day in a 1904-system workbook is another number (43844).
///
/// Written as ISO: a whole day writes only the date; one with a time adds the time; an integer part of 0 writes only the time (formats like `h:mm`),
/// and a cumulative duration (`[h]:mm:ss`) writes cumulative h:mm:ss. The display format is not handed over by calamine, so a cell that shows only year and month
/// is also written out to the day. A number outside Excel's calendar is written as-is
fn excel_date(d: &calamine::ExcelDateTime) -> String {
    let value = d.as_f64();
    if d.is_duration() {
        let seconds = (value.abs() * 86_400.0).round() as u64;
        let sign = if value < 0.0 { "-" } else { "" };
        return format!(
            "{sign}{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        );
    }
    // Excel's calendar ends at 9999-12-31 (serial number 2958465); a negative number also shows as #### by itself
    if !(0.0..2_958_466.0).contains(&value) {
        return value.to_string();
    }
    let (year, month, day, hour, minute, second, _) = d.to_ymd_hms_milli();
    let time = if second == 0 {
        format!("{hour:02}:{minute:02}")
    } else {
        format!("{hour:02}:{minute:02}:{second:02}")
    };
    if value < 1.0 {
        return time;
    }
    let date = format!("{year:04}-{month:02}-{day:02}");
    if (hour, minute, second) == (0, 0, 0) {
        date
    } else {
        format!("{date} {time}")
    }
}

/// Number cells are written at the precision Excel uses: 15 significant digits.
///
/// Excel compares and displays a number using only 15 significant digits, but a formula's cached value is written to the file with 17 digits at double precision:
/// `=0.1+0.2` is stored as 0.30000000000000004, and `=1.1*1.1-1` as 0.21000000000000019, which Excel shows as
/// 0.3 and 0.21. Written out as-is, body text, quotes and extracted values all carry that binary tail. First round to
/// 15 significant digits, then write the shortest decimal; numbers that are already clean (19.9, 1200, 45306) stay unchanged
fn excel_number(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    format!("{value:.14e}")
        .parse::<f64>()
        .unwrap_or(value)
        .to_string()
}

/// Decode before conversion so legacy HTML encodings remain supported.
pub fn html(bytes: &[u8]) -> anyhow::Result<String> {
    Ok(crate::html::page_to_markdown(&plain_text(bytes), None)?)
}

pub fn csv_text(bytes: &[u8], tsv: bool) -> anyhow::Result<(String, Vec<crate::ParseWarning>)> {
    let decoded = plain_text(bytes);
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(if tsv { b'\t' } else { b',' })
        .flexible(true)
        .has_headers(false)
        .from_reader(decoded.as_bytes());
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut records_total = 0usize;
    for record in reader.records() {
        let record = record?;
        records_total += 1;
        if rows.len() < CSV_RECORD_LIMIT {
            rows.push(record.iter().map(str::to_string).collect());
        }
    }
    let mut warnings = Vec::new();
    if records_total > CSV_RECORD_LIMIT {
        let records_read = rows.len();
        warnings.push(crate::ParseWarning {
            kind: crate::ParseWarning::CSV_RECORDS_TRUNCATED,
            detail: serde_json::json!({
                "records_read": records_read,
                "records_total": records_total,
                "records_omitted": records_total - records_read,
            }),
        });
    }
    // The first record is the column header (csv convention); if the table can't be rendered, fall back to pipe-separated lines
    let text = match crate::table::render_records(&rows) {
        Some(md) => md + "\n",
        None => {
            rows.iter()
                .map(|r| r.iter().map(String::as_str).collect::<Vec<_>>().join(" | "))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        }
    };
    Ok((text, warnings))
}

// ---- helpers ----

fn read_zip_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> anyhow::Result<String> {
    let mut entry = archive.by_name(name)?;
    let mut content = String::new();
    entry.read_to_string(&mut content)?;
    Ok(content)
}

/// Reads text from a PPTX slide XML. Outside tables, `a:p`/`a:br` still follow the original line-break rules;
/// each `a:tr` in an `a:tbl` becomes a row, each `a:tc` becomes a cell, and paragraphs inside a cell are joined with spaces.
fn pptx_xml_to_text(xml: &str) -> anyhow::Result<String> {
    #[derive(Default)]
    struct TableCell {
        text: String,
        span: usize,
        horizontal_merge: bool,
        vertical_merge: bool,
    }

    fn attr(e: &quick_xml::events::BytesStart<'_>, name: &str) -> Option<String> {
        e.attributes()
            .flatten()
            .find(|a| a.key.as_ref() == name)
            .map(|a| a.value.to_string())
    }

    fn truthy(e: &quick_xml::events::BytesStart<'_>, name: &str) -> bool {
        matches!(attr(e, name).as_deref(), Some("1" | "true" | "on"))
    }

    let mut reader = Reader::from_str(xml);
    let mut out = String::new();
    let mut in_text = false;
    let mut table_depth = 0usize;
    let mut rows: Vec<GridRow> = Vec::new();
    let mut row: GridRow = Vec::new();
    let mut cell: Option<TableCell> = None;
    let mut first_is_header = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                "a:tbl" => {
                    table_depth += 1;
                    if table_depth == 1 {
                        rows.clear();
                        row.clear();
                        cell = None;
                        first_is_header = false;
                    }
                }
                "a:tblPr" if table_depth == 1 => {
                    first_is_header = truthy(&e, "firstRow");
                }
                "a:tr" if table_depth == 1 => row.clear(),
                "a:tc" if table_depth == 1 => {
                    // DrawingML's merge attributes belong to a:tc; the a:tcPr that follows only describes styling.
                    cell = Some(TableCell {
                        span: attr(&e, "gridSpan")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(1),
                        horizontal_merge: truthy(&e, "hMerge"),
                        vertical_merge: truthy(&e, "vMerge"),
                        ..TableCell::default()
                    });
                }
                "a:t" => {
                    if cell.is_some() || table_depth == 0 {
                        in_text = true;
                    }
                }
                "a:br" => match cell.as_mut() {
                    Some(c) => c.text.push(' '),
                    None if table_depth == 0 => out.push('\n'),
                    _ => {}
                },
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.name().as_ref() {
                "a:tblPr" if table_depth == 1 => {
                    first_is_header = truthy(&e, "firstRow");
                }
                "a:br" => match cell.as_mut() {
                    Some(c) => c.text.push(' '),
                    None if table_depth == 0 => out.push('\n'),
                    _ => {}
                },
                "a:tab" => {
                    if let Some(c) = cell.as_mut() {
                        c.text.push(' ');
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                "a:t" => in_text = false,
                "a:p" => match cell.as_mut() {
                    Some(c) => c.text.push(' '),
                    None if table_depth == 0 => out.push('\n'),
                    _ => {}
                },
                "a:tc" if table_depth == 1 => {
                    if let Some(mut c) = cell.take() {
                        if !c.horizontal_merge {
                            if c.vertical_merge {
                                c.text.clear();
                            }
                            row.push((c.text, c.span.max(1), 0));
                        }
                    }
                }
                "a:tr" if table_depth == 1 => rows.push(std::mem::take(&mut row)),
                "a:tbl" => {
                    table_depth = table_depth.saturating_sub(1);
                    if table_depth == 0 {
                        if let Some(md) = crate::table::grid_or_lines(&rows, first_is_header) {
                            out.push('\n');
                            out.push_str(&md);
                            out.push_str("\n\n");
                        }
                        rows.clear();
                        row.clear();
                        cell = None;
                        first_is_header = false;
                    }
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_text => {
                let text = t.xml_content(quick_xml::XmlVersion::Implicit1_0);
                match cell.as_mut() {
                    Some(c) => c.text.push_str(&text),
                    None => out.push_str(&text),
                }
            }
            Ok(Event::CData(t)) if in_text => {
                let text = t.xml_content(quick_xml::XmlVersion::Implicit1_0);
                match cell.as_mut() {
                    Some(c) => c.text.push_str(&text),
                    None => out.push_str(&text),
                }
            }
            Ok(Event::GeneralRef(e)) if in_text => {
                let reference = format!("&{};", e.into_inner());
                let text = quick_xml::escape::unescape(&reference)
                    .unwrap_or(std::borrow::Cow::Borrowed(&reference));
                match cell.as_mut() {
                    Some(c) => c.text.push_str(&text),
                    None => out.push_str(&text),
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!("XML parse error: {e}"),
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget_book(sheet_data: &str, merges: &str) -> calamine::Xlsx<Cursor<Vec<u8>>> {
        let worksheet = format!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{sheet_data}</sheetData>{merges}</worksheet>"#
        );
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, xml) in [
            (
                "[Content_Types].xml",
                r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            ("xl/worksheets/sheet1.xml", &worksheet),
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(xml.as_bytes()).unwrap();
        }
        calamine::open_workbook_from_rs(Cursor::new(zip.finish().unwrap().into_inner())).unwrap()
    }

    #[test]
    fn a_segment_budget_keeps_only_complete_rows() {
        // Four source cells fit; two rows of three emitted segments do not.
        let rows = r#"<row r="1"><c r="A1" t="inlineStr"><is><t>left</t></is></c><c r="C1" t="inlineStr"><is><t>right</t></is></c></row><row r="2"><c r="A2"><v>1</v></c><c r="C2"><v>2</v></c></row>"#;
        let (_, rows_read, rows_total) =
            xlsx_sheet(&mut budget_book(rows, ""), "Budget", 5).unwrap();
        assert_eq!((rows_read, rows_total), (1, 2));
    }

    #[test]
    fn dropping_a_row_reprojects_the_retained_title_and_merged_span() {
        let title = r#"<row r="1"><c r="B1" t="inlineStr"><is><t>Title</t></is></c></row>"#;
        let rest = r#"<row r="2"><c r="A2" t="inlineStr"><is><t xml:space="preserve"> </t></is></c><c r="C2" t="inlineStr"><is><t xml:space="preserve"> </t></is></c></row><row r="3"><c r="Z3" t="inlineStr"><is><t>note</t></is></c></row>"#;
        let merges = r#"<mergeCells count="1"><mergeCell ref="B1:C1"/></mergeCells>"#;
        // Four source cells fit, but the first two rows use 3 + 4 segments.
        let (text, rows_read, rows_total) = xlsx_sheet(
            &mut budget_book(&format!("{title}{rest}"), merges),
            "Budget",
            5,
        )
        .unwrap();
        let (expected, _, _) = xlsx_sheet(&mut budget_book(title, merges), "Budget", 5).unwrap();
        assert_eq!((rows_read, rows_total), (1, 3));
        assert!(text.contains("Title"));
        assert_eq!(text, expected);
    }

    #[test]
    fn empty_runs_keep_the_physical_cell_and_merge_coordinates() {
        let regions = [GridMerge {
            start: (0, 2),
            end: (0, 4),
            text: "Merged".into(),
        }];
        let row = spreadsheet_row(
            0,
            10,
            &regions,
            &mut [usize::MAX; 10],
            |col| match col {
                0 => "Item".into(),
                7 => "3".into(),
                _ => String::new(),
            },
            true,
        );
        let expected = [
            ("Item", 1),
            ("", 1),
            ("Merged", 3),
            ("", 2),
            ("3", 1),
            ("", 2),
        ]
        .map(|(text, span)| (text.to_owned(), span, 0));
        assert_eq!(row, expected);
    }

    const DOC: &str = r#"<?xml version="1.0"?><w:document xmlns:w="x"><w:body>
<w:p><w:r><w:t>Segment results</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Segment</w:t></w:r></w:p></w:tc><w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>Revenue</w:t></w:r></w:p></w:tc></w:tr>
<w:tr><w:tc><w:p><w:r><w:t>Cloud</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>$</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>1,200</w:t></w:r></w:p></w:tc></w:tr>
<w:tr><w:tc><w:p><w:r><w:t>Devices</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>$</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>300</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
<w:p><w:r><w:t>After the table.</w:t></w:r></w:p></w:body></w:document>"#;

    /// docx tables: column headers spanning several columns are joined per column, "$" is merged back into its number, and paragraphs before and after the table are unchanged
    #[test]
    fn a_docx_table_is_rendered_under_its_headings() {
        let text = docx_xml_to_text(DOC, &Default::default()).unwrap();
        assert!(text.starts_with("Segment results\n\n"), "{text}");
        assert!(
            text.contains(
                "| Segment | Revenue |\n| --- | --- |\n| Cloud | $1,200 |\n| Devices | $300 |"
            ),
            "{text}"
        );
        assert!(text.ends_with("After the table.\n\n"), "{text}");
    }

    /// A table nested in a cell is not a table of its own: its text counts as the outer cell's text
    #[test]
    fn a_nested_docx_table_is_its_cells_text() {
        let xml = r#"<w:document xmlns:w="x"><w:body><w:tbl>
<w:tr><w:tc><w:p><w:r><w:t>Region</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Notes</w:t></w:r></w:p></w:tc></w:tr>
<w:tr><w:tc><w:p><w:r><w:t>North</w:t></w:r></w:p></w:tc><w:tc><w:tbl><w:tr><w:tc><w:p><w:r><w:t>inner</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>7</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:tc></w:tr>
</w:tbl></w:body></w:document>"#;
        let text = docx_xml_to_text(xml, &Default::default()).unwrap();
        assert_eq!(text.matches("| --- |").count(), 1, "{text}");
        assert!(text.contains("| North | inner 7 |"), "{text}");
    }

    /// Heading styles are recognized by outline level, then by built-in name, then by following basedOn; the style id does not count
    #[test]
    fn heading_styles_are_known_by_outline_level_then_name_then_parent() {
        let styles = r#"<w:styles xmlns:w="x">
<w:style w:type="paragraph" w:default="1" w:styleId="a"><w:name w:val="Normal"/></w:style>
<w:style w:type="paragraph" w:styleId="1"><w:name w:val="heading 1"/><w:basedOn w:val="a"/><w:pPr><w:keepNext/><w:outlineLvl w:val="0"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="2"><w:name w:val="heading 2"/><w:basedOn w:val="a"/></w:style>
<w:style w:type="paragraph" w:styleId="Heading3"><w:name w:val="Heading 3"/></w:style>
<w:style w:type="paragraph" w:styleId="ReportSection"><w:name w:val="Report Section"/><w:basedOn w:val="2"/></w:style>
<w:style w:type="paragraph" w:styleId="Outlined"><w:name w:val="Outlined"/><w:pPr><w:outlineLvl w:val="3"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="Demoted"><w:name w:val="Demoted"/><w:basedOn w:val="1"/><w:pPr><w:outlineLvl w:val="9"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="a3"><w:name w:val="Title"/><w:basedOn w:val="a"/></w:style>
<w:style w:type="paragraph" w:styleId="TOC1"><w:name w:val="toc 1"/><w:basedOn w:val="a"/></w:style>
<w:style w:type="character" w:styleId="10"><w:name w:val="Heading 1 Char"/><w:basedOn w:val="a0"/></w:style>
<w:style w:type="paragraph" w:styleId="Loop1"><w:name w:val="Loop 1"/><w:basedOn w:val="Loop2"/></w:style>
<w:style w:type="paragraph" w:styleId="Loop2"><w:name w:val="Loop 2"/><w:basedOn w:val="Loop1"/></w:style>
</w:styles>"#;
        let levels = docx_heading_styles(styles).unwrap();
        let mut found: Vec<(&str, u8)> = levels.iter().map(|(id, l)| (id.as_str(), *l)).collect();
        found.sort();
        assert_eq!(
            found,
            [
                ("1", 1),
                ("2", 2),
                ("Heading3", 3),
                ("Outlined", 4),
                ("ReportSection", 2)
            ]
        );
    }

    /// The first record of a csv is the column header, even if the column headers are years
    #[test]
    fn a_csv_is_a_table_whose_first_record_is_the_header() {
        let text = csv_text(b"Item,2025,2024\nRevenue,10,8\nCost,4,3\n", false)
            .unwrap()
            .0;
        assert_eq!(
            text,
            "| Item | 2025 | 2024 |\n| --- | --- | --- |\n| Revenue | 10 | 8 |\n| Cost | 4 | 3 |\n"
        );
    }

    /// A row that has only a sequence number filled in after the header is also a record, not a second header row
    #[test]
    fn a_lone_number_row_after_the_header_is_a_record() {
        let text = csv_text(
            b"no,name,score
1,,
2,Ada,9
",
            false,
        )
        .unwrap()
        .0;
        assert!(text.starts_with("| no | name | score |"), "{text}");
        assert!(text.contains("| 1 |  |  |"), "{text}");
        assert!(text.contains("| 2 | Ada | 9 |"), "{text}");
    }

    /// A csv with no numeric cells at all is still a table; its fields are still kept apart by their original columns
    #[test]
    fn a_csv_of_words_is_still_a_table() {
        let text = csv_text(b"name,role\nAda,engineer\nGrace,admiral\n", false)
            .unwrap()
            .0;
        assert!(
            text.starts_with("| name | role |\n| --- | --- |\n| Ada | engineer |"),
            "{text}"
        );
    }

    #[test]
    fn a_missing_email_does_not_attach_the_name_to_the_next_record() {
        let text = csv_text(b"name,email\nAlice,\nBob,bob@example.com\n", false)
            .unwrap()
            .0;
        assert_eq!(
            text,
            "| name | email |\n| --- | --- |\n| Alice |  |\n| Bob | bob@example.com |\n"
        );
    }

    #[test]
    fn sparse_delimited_records_keep_their_rows_and_columns() {
        let csv = "name,email,city\nAlice,,\n,,\n,bob@example.com,\n,,Sydney\nCarol,,Melbourne\nDave,dave@example.com,\nEve,,\n,,\n";
        let expected = "| name | email | city |\n| --- | --- | --- |\n| Alice |  |  |\n|  | bob@example.com |  |\n|  |  | Sydney |\n| Carol |  | Melbourne |\n| Dave | dave@example.com |  |\n| Eve |  |  |\n";
        for tsv in [false, true] {
            let input = if tsv {
                csv.replace(',', "\t")
            } else {
                csv.to_string()
            };
            assert_eq!(csv_text(input.as_bytes(), tsv).unwrap().0, expected);
        }
    }

    #[test]
    fn a_single_text_column_keeps_the_header_and_each_record() {
        for tsv in [false, true] {
            assert_eq!(
                csv_text(b"name\nAlice\nBob\n", tsv).unwrap().0,
                "| name |\n| --- |\n| Alice |\n| Bob |\n"
            );
        }
    }

    #[test]
    fn delimited_fields_keep_their_columns_whether_text_or_numbers() {
        for (csv, expected) in [
            (
                "name,role,team\nAda,engineer,platform\nGrace,admiral,navy\n",
                "| name | role | team |\n| --- | --- | --- |\n| Ada | engineer | platform |\n| Grace | admiral | navy |\n",
            ),
            (
                "name,role,score\nAda,engineer,9\nGrace,admiral,8\n",
                "| name | role | score |\n| --- | --- | --- |\n| Ada | engineer | 9 |\n| Grace | admiral | 8 |\n",
            ),
        ] {
            for tsv in [false, true] {
                let input = if tsv {
                    csv.replace(',', "\t")
                } else {
                    csv.to_string()
                };
                assert_eq!(csv_text(input.as_bytes(), tsv).unwrap().0, expected);
            }
        }
    }

    #[test]
    fn a_delimited_header_can_have_empty_columns_and_records_can_vary_in_width() {
        let csv = "name,,email\nAlice\nBob,,bob@example.com,extra\n,,eve@example.com\n";
        let expected = "| name |  | email |  |\n| --- | --- | --- | --- |\n| Alice |  |  |  |\n| Bob |  | bob@example.com | extra |\n|  |  | eve@example.com |  |\n";
        for tsv in [false, true] {
            let input = if tsv {
                csv.replace(',', "\t")
            } else {
                csv.to_string()
            };
            assert_eq!(csv_text(input.as_bytes(), tsv).unwrap().0, expected);
        }
        for (csv, expected) in [
            (
                "name,\nAlice,alice@example.com\n",
                "| name |  |\n| --- | --- |\n| Alice | alice@example.com |\n",
            ),
            (
                ",email\n,alice@example.com\n",
                "|  | email |\n| --- | --- |\n|  | alice@example.com |\n",
            ),
        ] {
            for tsv in [false, true] {
                let input = if tsv {
                    csv.replace(',', "\t")
                } else {
                    csv.to_string()
                };
                assert_eq!(csv_text(input.as_bytes(), tsv).unwrap().0, expected);
            }
        }
    }

    #[test]
    fn symbols_and_quoted_text_stay_in_their_delimited_fields() {
        let expected = "| kind | symbol | value |\n| --- | --- | --- |\n| price, retail | $ | 10 |\n| rate | % | 20 |\n| pipe \\| and line | hello | 30 |\n";
        for (input, tsv) in [
            (
                "kind,symbol,value\n\"price, retail\",$,10\nrate,%,20\n\"pipe | and\nline\",hello,30\n",
                false,
            ),
            (
                "kind\tsymbol\tvalue\n\"price, retail\"\t$\t10\nrate\t%\t20\n\"pipe | and\nline\"\thello\t30\n",
                true,
            ),
        ] {
            assert_eq!(csv_text(input.as_bytes(), tsv).unwrap().0, expected);
        }
    }
}
