//! MinerU layout (`content_list`) → one body text plus provenance segments per page (0040, second slice).
//!
//! MinerU detects the layout first, then recognizes text region by region, and returns the regions in reading order: body text, headings, tables, formulas,
//! captions, each with a `page_idx` and a `bbox` (mapped to 0–1000). Here they are joined into the Markdown the chunker understands:
//! headings stay headings and tables stay tables, so scanned-document chunks are cut by the same rules as native documents: a table header
//! follows its continuation chunks, and a heading sticks to the paragraph after it.
//!
//! **Segments are split by page, not by region.** A chunk holds only one provenance (decision 2), and every region has a different box; splitting by region
//! would make one chunk per paragraph, too fragmented for extraction to see context. The page is the unit a person turns to when checking, so one page
//! is one segment. After chunking, the boxes of the regions it covers are merged into one and written into the anchor.
//!
//! Page numbers count from 1 (`page_idx` + 1): the anchor is for a person paging through, and the viewer's `#page=` also counts from 1.

use crate::provenance::{Origin, Provenance, Segment};
use crate::reading::{Place, Reading, Region};
use serde_json::{json, Value};

/// Auxiliary layout regions: headers, footers, page numbers. They repeat on every page, and in the body they would add a line of noise to every chunk.
/// Footnotes and side notes are not here: in a contract, footnotes state clauses
const CHROME: &[&str] = &["header", "footer", "page_number"];

/// Reads `content_list`. `model` is recorded in the provenance: which version and which backend did the reading
pub fn reading(content_list: &Value, model: &str) -> Reading {
    let mut text = String::new();
    let mut regions: Vec<Region> = Vec::new();
    for item in content_list.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let kind = item["type"].as_str().unwrap_or("text");
        if CHROME.contains(&kind) {
            continue;
        }
        let Some(body) = render(kind, item) else {
            continue;
        };
        let body = without_nul(&body);
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        let start = text.len();
        text.push_str(body);
        regions.push(Region {
            range: start..text.len(),
            place: Place::Page {
                page: item["page_idx"].as_u64().unwrap_or(0) + 1,
                bbox: bbox(&item["bbox"]),
            },
        });
    }

    // One segment per page, end to end, covering the whole body: a segment starts at the first region of its page and ends where the next page's first region starts
    let mut segments: Vec<Segment> = Vec::new();
    for r in &regions {
        let Place::Page { page, .. } = r.place else {
            continue;
        };
        if segments
            .last()
            .is_some_and(|s| s.provenance.anchor.as_ref().map(|a| &a["page"]) == Some(&json!(page)))
        {
            continue;
        }
        if let Some(prev) = segments.last_mut() {
            prev.range.end = r.range.start;
        }
        segments.push(Segment {
            range: r.range.start..text.len(),
            provenance: Provenance {
                origin: Origin::Ocr,
                model: Some(model.to_string()),
                anchor: Some(json!({ "page": page })),
            },
        });
    }
    if let Some(first) = segments.first_mut() {
        first.range.start = 0;
    }
    Reading {
        text,
        segments,
        regions,
    }
}

/// Writes one region as Markdown. An unrecognized type is kept as text if it has any, and skipped if it has none
fn render(kind: &str, item: &Value) -> Option<String> {
    let s = |key: &str| item[key].as_str().map(str::trim).filter(|t| !t.is_empty());
    let lines = |key: &str| -> Vec<String> {
        item[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::trim).filter(|t| !t.is_empty()))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    let with_notes = |caption: &str, body: Option<String>, footnote: &str| -> Option<String> {
        let parts: Vec<String> = lines(caption)
            .into_iter()
            .chain(body)
            .chain(lines(footnote))
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    };
    match kind {
        "text" => {
            let t = s("text")?;
            match item["text_level"].as_u64().unwrap_or(0) {
                0 => Some(t.to_string()),
                // A heading is only recognized on its own line; line breaks inside recognized text become spaces
                n => Some(format!(
                    "{} {}",
                    "#".repeat(n.min(6) as usize),
                    t.split_whitespace().collect::<Vec<_>>().join(" ")
                )),
            }
        }
        "table" => {
            let body = s("table_body").map(|html| {
                crate::html::fragment_to_markdown(html)
                    .map(|md| md.trim().to_string())
                    .unwrap_or_else(|_| html.to_string())
            });
            with_notes("table_caption", body, "table_footnote")
        }
        "image" | "chart" => with_notes("image_caption", None, "image_footnote"),
        "code" => with_notes(
            "code_caption",
            s("code_body").map(|c| format!("```\n{c}\n```")),
            "code_footnote",
        ),
        "list" => {
            let items = lines("list_items");
            (!items.is_empty()).then(|| items.join("\n"))
        }
        _ => s("text").map(String::from),
    }
}

fn bbox(v: &Value) -> Option<[f64; 4]> {
    let a = v.as_array()?;
    if a.len() != 4 {
        return None;
    }
    let mut out = [0.0; 4];
    for (o, x) in out.iter_mut().zip(a) {
        *o = x.as_f64()?;
    }
    Some(out)
}

/// NUL bytes from recognized or transcribed text are stripped, as in the PDF text layer (#611)
pub(crate) fn without_nul(s: &str) -> String {
    s.replace('\0', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan() -> Value {
        json!([
            { "type": "header", "text": "ACME CONFIDENTIAL", "page_idx": 0, "bbox": [0, 0, 1000, 30] },
            { "type": "text", "text": "Lease Agreement", "text_level": 1, "page_idx": 0, "bbox": [100, 50, 900, 90] },
            { "type": "text", "text": "The tenant is Beta Robotics.", "page_idx": 0, "bbox": [100, 100, 900, 140] },
            { "type": "text", "text": "Rent is paid monthly.", "page_idx": 0, "bbox": [100, 150, 800, 190] },
            { "type": "page_number", "text": "1", "page_idx": 0, "bbox": [480, 970, 520, 990] },
            { "type": "table", "table_caption": ["Schedule of rent"],
              "table_body": "<table><tr><td>Year</td><td>Rent</td></tr><tr><td>2026</td><td>1,000</td></tr></table>",
              "table_footnote": [], "page_idx": 1, "bbox": [100, 60, 900, 300] },
            { "type": "image", "img_path": "images/stamp.png", "image_caption": [], "page_idx": 1, "bbox": [600, 800, 900, 950] },
            { "type": "text", "text": "Signed by both parties.", "page_idx": 1, "bbox": [100, 320, 900, 360] }
        ])
    }

    #[test]
    fn a_scan_reads_as_markdown_without_page_chrome() {
        let r = reading(&scan(), "mineru 2.5.4 vlm");
        assert!(r
            .text
            .starts_with("# Lease Agreement\n\nThe tenant is Beta Robotics."));
        assert!(
            !r.text.contains("ACME CONFIDENTIAL"),
            "the header repeats on every page"
        );
        assert!(!r.text.contains("\n\n1\n\n"), "the page number is not text");
        assert!(
            r.text.contains("Schedule of rent\n\n| Year | Rent |"),
            "{}",
            r.text
        );
        assert!(r.text.contains("| 2026 | 1,000 |"));
        assert!(
            !r.text.contains("stamp.png"),
            "an image without a caption says nothing yet"
        );
    }

    #[test]
    fn pages_are_segments_that_cover_the_text() {
        let r = reading(&scan(), "mineru 2.5.4 vlm");
        assert_eq!(r.segments.len(), 2);
        assert_eq!(r.segments[0].range.start, 0);
        assert_eq!(r.segments[0].range.end, r.segments[1].range.start);
        assert_eq!(r.segments[1].range.end, r.text.len());
        assert!(r.text[r.segments[1].range.clone()].starts_with("Schedule of rent"));
        assert_eq!(r.segments[1].provenance.anchor, Some(json!({ "page": 2 })));
    }

    #[test]
    fn a_chunk_stays_on_its_page_and_boxes_what_it_covers() {
        let r = reading(&scan(), "mineru 2.5.4 vlm");
        let pieces = r.chunk(300);
        assert_eq!(pieces.len(), 2, "{pieces:#?}");
        let first = &pieces[0];
        assert_eq!(first.provenance.origin, Origin::Ocr);
        assert_eq!(first.provenance.model.as_deref(), Some("mineru 2.5.4 vlm"));
        assert_eq!(
            first.provenance.anchor,
            Some(json!({ "page": 1, "bbox": [100.0, 50.0, 900.0, 190.0] }))
        );
        assert!(first.text.contains("Rent is paid monthly."));
        let second = &pieces[1];
        assert_eq!(second.provenance.anchor.as_ref().unwrap()["page"], json!(2));
        assert!(second.text.contains("Signed by both parties."));
        assert_eq!(
            second.heading.as_deref(),
            Some("Lease Agreement"),
            "the breadcrumb crosses the page"
        );
        for p in &pieces {
            assert!(p
                .text
                .ends_with(&r.text[p.char_start as usize..p.char_end as usize]));
        }
    }

    #[test]
    fn an_empty_or_odd_list_reads_as_nothing() {
        assert!(reading(&json!([]), "m").text.is_empty());
        assert!(reading(&json!({"not": "a list"}), "m").segments.is_empty());
        let r = reading(
            &json!([{ "type": "equation", "text": "$$E = mc^2$$", "page_idx": 3 }]),
            "m",
        );
        assert_eq!(r.text, "$$E = mc^2$$");
        assert_eq!(
            r.chunk(300)[0].provenance.anchor,
            Some(json!({ "page": 4 }))
        );
    }
}
