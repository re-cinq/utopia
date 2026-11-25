//! Chunking: packs document blocks into units that extraction can see in a single pass.
//!
//! **A chunk's job is to hold everything extraction can see at once.** The model sees only this chunk, and anything cut apart cannot be patched back together,
//! so chunk boundaries are part of the extraction contract, not a text-processing detail. Rules derived from this:
//!
//! - A line is not split; a table header and its body are not split; an explanatory sentence (the "results are as follows:" line above a table) and its table are not split;
//!   a heading and the first paragraph under it are not split.
//! - A table that fits is kept whole. It is split by row only when it does not fit, and **every continuation chunk repeats the breadcrumb, the explanatory sentence and the header**
//!   — this is the rule that fixes "a continuation chunk with only one row of five dollar figures"; no cleverer splitter solves that on its own.
//! - The budget is counted in tokens, not characters. It used to be 1200 characters, with a comment saying "about 1000+ tokens", which was meant for Chinese;
//!   English got only around 300, and a wide table did not fit. Now both languages get the same budget.
//! - A horizontal rule is **not** a split point. The old splitter liked to split at horizontal rules, but in these documents a rule is a page break:
//!   in a vote result, one table was split in half by exactly that.
//! - No overlap. Chunks are semantic units; context comes from breadcrumbs and reattached headers, not from copying the tail of the previous chunk.
//!
//! Each chunk's body is a slice of the source with no character changed; the prefixes (breadcrumb, explanatory sentence, header) are also slices of the source,
//! just copied from elsewhere. `char_start` / `char_end` refer to the body segment, excluding the prefixes.

use crate::blocks::{blocks, Block, Kind};
use crate::provenance::{Provenance, Segment};
use std::ops::Range;
use std::sync::OnceLock;
use text_splitter::{ChunkConfig, TextSplitter};
use tiktoken_rs::CoreBPE;

#[derive(Debug, Clone)]
pub struct ChunkPiece {
    pub seq: i32,
    pub text: String,
    pub char_start: i32,
    pub char_end: i32,
    /// The section path this chunk sits in, separated by `›`; None for a document with no headings
    pub heading: Option<String>,
    /// Where the text of this chunk comes from (0040). A chunk has only one origin
    pub provenance: Provenance,
}

/// A chunk's budget (cl100k tokens).
///
/// **300, not 1000.** 1000 was the intent in the old character-budget comment; the first round measured it away: the acquisition
/// 8-K went from nine chunks to two, and the model wrote only seven facts from 4700 characters (the address, the phone number, "published
/// in the SEC"), and not one fact about the acquisition itself. The number of facts one call writes does not grow as the input grows;
/// with a bigger chunk it picks the easiest few to write. 300 is the real size the old splitting had in English, and it is the condition the 43/45 rounds
/// measured; Chinese gets less context than before at this number, which has not been measured yet (0039 open question).
pub const BUDGET_TOKENS: usize = 300;

/// The longest a paragraph can be and still count as an explanatory sentence: the paragraph above a table must be short, or end with a colon,
/// to be carried along as part of the table. A whole analysis paragraph is not an explanatory sentence. Full-width colons count too: 200 bytes is only sixty-odd Chinese characters,
/// so a longer sentence ending in "as follows:" is recognized only by its colon; before, only the half-width colon counted, and when the table fell in another chunk, the sentence was invisible.
const CAPTION_MAX_BYTES: usize = 200;
/// At most how many short paragraphs before a table are taken together as its explanatory sentence
const CAPTION_RUN_MAX: usize = 5;

/// A paragraph shorter than this many tokens (a page number, a footnote marker) does not form a chunk alone; it joins the previous chunk
const TINY_TOKENS: usize = 8;

pub fn chunk_text(text: &str) -> Vec<ChunkPiece> {
    chunk_with_budget(text, BUDGET_TOKENS)
}

fn bpe() -> &'static CoreBPE {
    static BPE: OnceLock<CoreBPE> = OnceLock::new();
    BPE.get_or_init(|| {
        tiktoken_rs::cl100k_base().expect("the cl100k rank table is embedded in the crate")
    })
}

fn tokens(s: &str) -> usize {
    bpe().encode_ordinary(s).len()
}

/// A packing unit: a heading goes with the next chunk; a table carries its own explanatory sentence.
enum Unit {
    Heading {
        level: u8,
        range: Range<usize>,
    },
    Text(Range<usize>),
    Table {
        caption: Option<Range<usize>>,
        head: Range<usize>,
        rows: Vec<Range<usize>>,
    },
}

fn units(text: &str, blocks: Vec<Block>) -> Vec<Unit> {
    let mut out: Vec<Unit> = Vec::with_capacity(blocks.len());
    let mut i = 0usize;
    while i < blocks.len() {
        let b = &blocks[i];
        match &b.kind {
            Kind::Heading(level) => out.push(Unit::Heading {
                level: *level,
                range: b.range.clone(),
            }),
            Kind::Rule => {}
            Kind::Paragraph => {
                // A short paragraph right after a table is its explanatory sentence and goes with the table. **Two general additions** (recall desk,
                // measured in the round after the 2026-09-13 top-up):
                //
                // - A horizontal rule between an explanatory sentence and its table still counts as right after. In these documents a rule is a page break,
                //   so in item five of the resolutions, the "results are as follows:" line and its table were split by a page: the sentence stayed in
                //   the previous chunk while the table went to the next one.
                // - An explanatory sentence introduces the **run of tables right after it**, not just the first one. "Nominees for ten directors,
                //   results are as follows:" is followed by ten tables, and before, only the first carried it. Jensen Huang's
                //   table was in another chunk, and its four vote counts are all right, but no sentence says what is being voted on,
                //   so the "elected director" edge was lost. A run lasts until something else (a paragraph, a heading) appears.
                //
                // - **Several consecutive short paragraphs** before a table count together: in a financial report, "NVIDIA CORPORATION", "CONDENSED
                //   CONSOLIDATED STATEMENTS OF INCOME", "(In millions)" and "(Unaudited)" are four paragraphs; carrying only the last one leaves the model
                //   seeing a table that does not know which report it is or what unit it is in.
                //   A run is at most five paragraphs, with nothing else in between
                let caption_like = |blk: &Block| {
                    matches!(blk.kind, Kind::Paragraph)
                        && (blk.range.len() <= CAPTION_MAX_BYTES
                            || text[blk.range.clone()].trim_end().ends_with([':', '：']))
                };
                let mut k = i + 1;
                while k - i < CAPTION_RUN_MAX && blocks.get(k).is_some_and(caption_like) {
                    k += 1;
                }
                let caption = b.range.start..blocks[k - 1].range.end;
                let mut j = k;
                let mut attached = 0usize;
                if caption_like(b) {
                    loop {
                        while matches!(
                            blocks.get(j),
                            Some(Block {
                                kind: Kind::Rule,
                                ..
                            })
                        ) {
                            j += 1;
                        }
                        let Some(Block {
                            kind: Kind::Table { head, rows },
                            ..
                        }) = blocks.get(j)
                        else {
                            break;
                        };
                        out.push(Unit::Table {
                            caption: Some(caption.clone()),
                            head: head.clone(),
                            rows: rows.clone(),
                        });
                        attached += 1;
                        j += 1;
                    }
                }
                if attached == 0 {
                    // Not an explanatory sentence: this paragraph goes through as usual; the paragraphs after it are each looked at again in the next round
                    out.push(Unit::Text(b.range.clone()));
                } else {
                    i = j - 1;
                }
            }
            Kind::Table { head, rows } => out.push(Unit::Table {
                caption: None,
                head: head.clone(),
                rows: rows.clone(),
            }),
            Kind::Other => out.push(Unit::Text(b.range.clone())),
        }
        i += 1;
    }
    out
}

/// One item in the body: a text paragraph, or a table (possibly only a few rows left)
enum Piece {
    Text(Range<usize>),
    Table {
        caption: Option<Range<usize>>,
        head: Range<usize>,
        rows: Vec<Range<usize>>,
    },
}

impl Piece {
    fn render(&self, text: &str) -> String {
        match self {
            Piece::Text(r) => text[r.clone()].to_string(),
            Piece::Table {
                caption,
                head,
                rows,
            } => {
                let mut s = String::new();
                if let Some(c) = caption {
                    s.push_str(&text[c.clone()]);
                    s.push_str("\n\n");
                }
                s.push_str(&text[head.clone()]);
                for r in rows {
                    s.push('\n');
                    s.push_str(&text[r.clone()]);
                }
                s
            }
        }
    }
    fn span(&self) -> Range<usize> {
        match self {
            Piece::Text(r) => r.clone(),
            Piece::Table {
                caption,
                head,
                rows,
            } => {
                let start = caption.as_ref().map_or(head.start, |c| c.start);
                let end = rows.last().map_or(head.end, |r| r.end);
                start..end
            }
        }
    }
}

struct Packer<'a> {
    text: &'a str,
    budget: usize,
    out: Vec<ChunkPiece>,
    /// Which chapter we are in now: (level, heading line)
    path: Vec<(u8, Range<usize>)>,
    /// Headings that have appeared but are not yet attached to body text
    pending: Vec<Range<usize>>,
    /// The `path` before these `pending` headings appeared: while they are not yet attached to body text, the chunk being collected still belongs to this chapter
    before_pending: Vec<(u8, Range<usize>)>,
    /// The chunk being collected: prefix (section headings) and body
    prefix: Vec<Range<usize>>,
    body: Vec<Piece>,
    /// The body, split into segments by origin; the chunk being collected belongs to the segment `current` points to
    segments: &'a [Segment],
    current: usize,
}

impl<'a> Packer<'a> {
    /// Which segment position `at` falls in (segments are sorted by position)
    fn segment_of(&self, at: usize) -> usize {
        self.segments
            .iter()
            .rposition(|s| s.range.start <= at)
            .unwrap_or(0)
    }

    /// The next item goes into the `seg` segment: if it is not the same segment as the chunk being collected, close that chunk first.
    /// Headings do not close with it: they belong to the following body text, and the breadcrumb keeps passing down as well
    fn enter_segment(&mut self, seg: usize) {
        if seg != self.current {
            self.flush();
            self.current = seg;
        }
    }

    fn render(&self, prefix: &[Range<usize>], body: &[Piece]) -> String {
        let mut parts: Vec<String> = prefix
            .iter()
            .map(|r| self.text[r.clone()].to_string())
            .collect();
        // Within one chunk, a run of tables shares one sentence: the explanation appears only once, before the first table.
        // Ten tables for ten directors packed into one chunk should not repeat "results are as follows" ten times
        let mut last_caption: Option<Range<usize>> = None;
        for p in body {
            match p {
                Piece::Table {
                    caption: Some(c),
                    head,
                    rows,
                } if last_caption.as_ref() == Some(c) => {
                    let bare = Piece::Table {
                        caption: None,
                        head: head.clone(),
                        rows: rows.clone(),
                    };
                    parts.push(bare.render(self.text));
                }
                Piece::Table { caption, .. } => {
                    last_caption = caption.clone();
                    parts.push(p.render(self.text));
                }
                Piece::Text(_) => {
                    last_caption = None;
                    parts.push(p.render(self.text));
                }
            }
        }
        parts.join("\n\n")
    }

    /// Prefix of a new chunk: the heading path of the current chapter, minus the headings that are about to appear as body text
    fn prefix_now(&self) -> Vec<Range<usize>> {
        self.path
            .iter()
            .map(|(_, r)| r.clone())
            .filter(|r| !self.pending.contains(r))
            .collect()
    }

    fn fits(&self, prefix: &[Range<usize>], body: &[Piece]) -> bool {
        tokens(&self.render(prefix, body)) <= self.budget
    }

    /// Try to put an item into the current chunk; if it does not fit, close the current chunk first and try again in an empty one.
    ///
    /// **A very small item does not form a chunk alone.** At the end of a press release there is a page-number line "4"; by budget it does not fit
    /// into the previous chunk, which is already full, so it became a chunk of its own: a one-character chunk that extraction still spends a model call on.
    /// Items of a few tokens are merged into the previous chunk; going a few tokens over budget for them does not matter.
    fn place(&mut self, piece: Piece) -> Option<Piece> {
        if self.pending.is_empty() {
            if let Piece::Text(r) = &piece {
                if tokens(&self.text[r.clone()]) <= TINY_TOKENS {
                    if !self.body.is_empty() {
                        self.body.push(piece);
                        return None;
                    }
                    // The previous chunk has already been emitted (the fallback for long paragraphs flushes right away): attach to its tail.
                    // Only attach to one of the same origin: a line of transcript must not be mixed into a source-text chunk and borrow its credibility
                    let here = &self.segments[self.current].provenance;
                    if let Some(last) = self.out.last_mut().filter(|l| &l.provenance == here) {
                        last.text.push_str(
                            "

",
                        );
                        last.text.push_str(&self.text[r.clone()]);
                        last.char_end = last.char_end.max(r.end as i32);
                        return None;
                    }
                }
            }
        }
        let mut with_headings: Vec<Piece> = self.pending.iter().cloned().map(Piece::Text).collect();
        with_headings.push(piece);
        if self.body.is_empty() {
            self.prefix = self.prefix_now();
        }
        let mut candidate: Vec<Piece> = std::mem::take(&mut self.body);
        candidate.extend(with_headings);
        if self.fits(&self.prefix.clone(), &candidate) {
            self.body = candidate;
            self.pending.clear();
            return None;
        }
        // Does not fit: take back the piece just added
        let n = candidate.len() - (self.pending.len() + 1);
        let mut rest = candidate.split_off(n);
        self.body = candidate;
        let piece = rest.pop().expect("the item just pushed");
        if !self.body.is_empty() {
            self.flush();
            return self.place(piece);
        }
        Some(piece)
    }

    fn flush(&mut self) {
        if self.body.is_empty() {
            return;
        }
        let body = std::mem::take(&mut self.body);
        let prefix = std::mem::take(&mut self.prefix);
        let text = self.render(&prefix, &body);
        let start = body.iter().map(|p| p.span().start).min().unwrap_or(0);
        let end = body.iter().map(|p| p.span().end).max().unwrap_or(0);
        // When a chunk is closed, any headings not yet attached to body text belong to the next item that did not fit (or changed origin): those headings
        // go along with the next chunk; `path` has already switched to the next chapter (the previous chapter at the same level has also been popped), so this chunk is named by the path from before the chapter change.
        // Previously `path` was always used, so chunks closed at a chapter boundary carried the next chapter's name.
        let path = if self.pending.is_empty() {
            &self.path
        } else {
            &self.before_pending
        };
        let heading = self.breadcrumb(path);
        self.out.push(ChunkPiece {
            seq: self.out.len() as i32,
            text,
            char_start: start as i32,
            char_end: end as i32,
            heading,
            provenance: self.segments[self.current].provenance.clone(),
        });
    }

    fn breadcrumb(&self, path: &[(u8, Range<usize>)]) -> Option<String> {
        if path.is_empty() {
            return None;
        }
        Some(
            path.iter()
                .map(|(_, r)| self.text[r.clone()].trim_start_matches('#').trim())
                .collect::<Vec<_>>()
                .join(" › "),
        )
    }

    fn heading(&mut self, level: u8, range: Range<usize>) {
        if self.pending.is_empty() {
            self.before_pending = self.path.clone();
        }
        while self.path.last().is_some_and(|(l, _)| *l >= level) {
            self.path.pop();
        }
        self.path.push((level, range.clone()));
        self.pending.push(range);
    }

    fn text_unit(&mut self, range: Range<usize>) {
        let Some(Piece::Text(range)) = self.place(Piece::Text(range)) else {
            return;
        };
        // A single paragraph is over budget: fall back to splitting by sentence, each small piece becomes a chunk of its own, with the prefix still applied
        self.prefix = self.prefix_now();
        let prefix_cost = tokens(&self.render(&self.prefix.clone(), &[]));
        let headings: Vec<Piece> = self.pending.drain(..).map(Piece::Text).collect();
        let heading_cost = tokens(&self.render(&[], &headings));
        let capacity = self
            .budget
            .saturating_sub(prefix_cost + heading_cost)
            .max(1);
        let config = ChunkConfig::new(capacity).with_sizer(bpe());
        let splitter = TextSplitter::new(config);
        let block = &self.text[range.clone()];
        let mut first = true;
        for (offset, piece) in splitter.chunk_indices(block) {
            let sub = range.start + offset..range.start + offset + piece.len();
            if first {
                // The first small piece carries the heading as body text, so it uses the prefix computed above: computed while the heading was still in `pending`, not including them.
                // Previously every small piece recomputed its prefix, by which time `pending` was empty, so the first chunk repeated the heading twice, and sometimes went over budget.
                self.body = headings
                    .iter()
                    .map(|h| match h {
                        Piece::Text(r) => Piece::Text(r.clone()),
                        _ => unreachable!(),
                    })
                    .collect();
                first = false;
            } else {
                // Continuation chunk: the heading is already attached to body text, so the prefix is the full path
                self.prefix = self.prefix_now();
            }
            self.body.push(Piece::Text(sub));
            self.flush();
        }
    }

    fn table_unit(
        &mut self,
        caption: Option<Range<usize>>,
        head: Range<usize>,
        rows: Vec<Range<usize>>,
    ) {
        let whole = Piece::Table {
            caption: caption.clone(),
            head: head.clone(),
            rows: rows.clone(),
        };
        let Some(_) = self.place(whole) else {
            return;
        };
        // Does not fit whole: split by row, and every chunk carries the explanatory sentence and the header. A row is never split;
        // a chunk made of the explanatory sentence, the header and one row that is still over budget is emitted anyway: there is no better way to cut it
        let mut rows = rows.into_iter().peekable();
        while rows.peek().is_some() {
            let mut taken: Vec<Range<usize>> = vec![rows.next().expect("already peeked")];
            while let Some(next) = rows.peek() {
                let mut trial = taken.clone();
                trial.push(next.clone());
                let piece = Piece::Table {
                    caption: caption.clone(),
                    head: head.clone(),
                    rows: trial,
                };
                let headings: Vec<Piece> = self.pending.iter().cloned().map(Piece::Text).collect();
                let mut body = headings;
                body.push(piece);
                if self.fits(&self.prefix_now(), &body) {
                    taken.push(rows.next().expect("already peeked"));
                } else {
                    break;
                }
            }
            self.prefix = self.prefix_now();
            self.body = self.pending.drain(..).map(Piece::Text).collect();
            self.body.push(Piece::Table {
                caption: caption.clone(),
                head: head.clone(),
                rows: taken,
            });
            self.flush();
        }
    }
}

pub fn chunk_with_budget(text: &str, budget: usize) -> Vec<ChunkPiece> {
    let whole = [Segment {
        range: 0..text.len(),
        provenance: Provenance::stated(),
    }];
    chunk_segments(text, &whole, budget)
}

/// Body chunking by origin-separated segments: a chunk does not cross segments (0040 decision 2), but the breadcrumb is carried across segments.
///
/// The engines that read scanned pages and transcribe recordings return text one page or one segment at a time; they are joined into one body and cut by the same rules,
/// with only one extra cut point: "the origin changed". `segments` is sorted by position and covers the whole body;
/// when empty, the whole thing is treated as source text
pub fn chunk_segments(text: &str, segments: &[Segment], budget: usize) -> Vec<ChunkPiece> {
    let whole = [Segment {
        range: 0..text.len(),
        provenance: Provenance::stated(),
    }];
    let segments = if segments.is_empty() {
        &whole[..]
    } else {
        segments
    };
    let mut p = Packer {
        text,
        budget,
        out: Vec::new(),
        path: Vec::new(),
        pending: Vec::new(),
        before_pending: Vec::new(),
        prefix: Vec::new(),
        body: Vec::new(),
        segments,
        current: 0,
    };
    for unit in units(text, blocks(text)) {
        let start = match &unit {
            Unit::Heading { range, .. } | Unit::Text(range) => range.start,
            Unit::Table { caption, head, .. } => caption.as_ref().map_or(head.start, |c| c.start),
        };
        // A heading is not a segment cut: it goes with the next item, which decides which segment it enters
        if !matches!(unit, Unit::Heading { .. }) {
            p.enter_segment(p.segment_of(start));
        }
        match unit {
            Unit::Heading { level, range } => p.heading(level, range),
            Unit::Text(range) => p.text_unit(range),
            Unit::Table {
                caption,
                head,
                rows,
            } => p.table_unit(caption, head, rows),
        }
    }
    // The document ends with a heading: the body collected so far is closed as a chunk first, then the heading becomes a chunk of its own, which is better than dropping it. Previously this replaced
    // the body with the heading, so the chunk not yet closed (the whole text of a short document) was lost along with its characters
    if !p.pending.is_empty() {
        p.flush();
        p.prefix = p.prefix_now();
        p.body = p.pending.drain(..).map(Piece::Text).collect();
    }
    p.flush();
    p.out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One chunk holds only one origin (0040 decision 2): source text next to a scanned page is also split into two chunks;
    /// the breadcrumb is carried across segments; the tail of a few tokens at the end of a page does not attach to a chunk of another origin
    #[test]
    fn a_chunk_holds_one_provenance_and_the_breadcrumb_crosses_it() {
        use crate::provenance::{Origin, Provenance, Segment};
        let stated = "# Lease

The tenant pays rent monthly.

";
        let page1 = "The rent is 1,000 per month.

";
        let page2 = "4

";
        let text = format!("{stated}{page1}{page2}");
        let ocr = |page: i64| Provenance {
            origin: Origin::Ocr,
            model: Some("mineru".into()),
            anchor: Some(serde_json::json!({ "page": page })),
        };
        let a = stated.len();
        let b = a + page1.len();
        let segments = vec![
            Segment {
                range: 0..a,
                provenance: Provenance::stated(),
            },
            Segment {
                range: a..b,
                provenance: ocr(1),
            },
            Segment {
                range: b..text.len(),
                provenance: ocr(2),
            },
        ];
        let chunks = chunk_segments(&text, &segments, BUDGET_TOKENS);
        let origins: Vec<_> = chunks.iter().map(|c| c.provenance.clone()).collect();
        assert_eq!(chunks.len(), 3, "{chunks:#?}");
        assert_eq!(origins[0], Provenance::stated());
        assert_eq!(origins[1], ocr(1));
        assert_eq!(origins[2], ocr(2), "the page number stays on its own page");
        assert!(chunks[1].text.contains("1,000"));
        assert!(!chunks[0].text.contains("1,000"));
        assert_eq!(chunks[1].heading.as_deref(), Some("Lease"));
        // The old path with no segments: the whole document is source text
        assert!(chunk_with_budget(&text, BUDGET_TOKENS)
            .iter()
            .all(|c| c.provenance == Provenance::stated()));
    }

    /// A chunk closed when the origin changes is named by its own chapter: the next chapter's heading has already been read, but it belongs to the body that follows
    #[test]
    fn a_chunk_closed_by_a_change_of_provenance_is_named_by_its_own_section() {
        use crate::provenance::{Origin, Provenance, Segment};
        let stated = "# Lease\n\nThe tenant pays rent monthly.\n\n## Schedule\n\n";
        let page = "The rent is 1,000 per month.\n\n";
        let text = format!("{stated}{page}");
        let segments = vec![
            Segment {
                range: 0..stated.len(),
                provenance: Provenance::stated(),
            },
            Segment {
                range: stated.len()..text.len(),
                provenance: Provenance {
                    origin: Origin::Ocr,
                    model: Some("mineru".into()),
                    anchor: Some(serde_json::json!({ "page": 1 })),
                },
            },
        ];
        let chunks = chunk_segments(&text, &segments, BUDGET_TOKENS);
        assert_eq!(chunks.len(), 2, "{chunks:#?}");
        assert!(chunks[0].text.contains("rent monthly"), "{chunks:#?}");
        assert_eq!(chunks[0].heading.as_deref(), Some("Lease"));
        assert!(chunks[1].text.contains("## Schedule"), "{chunks:#?}");
        assert_eq!(chunks[1].heading.as_deref(), Some("Lease › Schedule"));
    }

    fn table(n_rows: usize) -> String {
        let mut s = String::from("The results of the voting were as follows:\n\n| Nominee | For | Against |\n| --- | --- | --- |\n");
        for i in 0..n_rows {
            s.push_str(&format!("| Director {i} | {} | {} |\n", 1000 + i, 50 + i));
        }
        s
    }

    /// The shape of the 13 chunks in the earnings report: a table bigger than the budget; continuation chunks must still see the header and the explanatory sentence.
    #[test]
    fn a_wide_table_is_split_by_rows_and_every_piece_carries_its_header_and_caption() {
        let text = format!("# Item 5.07\n\n{}", table(40));
        let pieces = chunk_with_budget(&text, 120);
        assert!(
            pieces.len() > 2,
            "40 行在 120 token 里必然切成好几块: {}",
            pieces.len()
        );
        for (i, p) in pieces.iter().enumerate() {
            assert!(
                p.text.contains("| Nominee | For | Against |"),
                "第 {i} 块没有表头:\n{}",
                p.text
            );
            assert!(
                p.text.contains("| --- | --- | --- |"),
                "第 {i} 块没有分隔行"
            );
            assert!(
                p.text
                    .contains("The results of the voting were as follows:"),
                "第 {i} 块没有说明句"
            );
            assert!(
                p.text.starts_with("# Item 5.07"),
                "第 {i} 块没有章节面包屑:\n{}",
                p.text
            );
            assert_eq!(p.heading.as_deref(), Some("Item 5.07"));
        }
        // Each row appears only once; a row is not split
        let all: String = pieces
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for i in 0..40 {
            assert_eq!(
                all.matches(&format!("| Director {i} |")).count(),
                1,
                "Director {i}"
            );
        }
    }

    #[test]
    fn a_table_that_fits_stays_whole_with_its_caption() {
        let text = format!("Intro paragraph.\n\n{}\nClosing.", table(3));
        let pieces = chunk_with_budget(&text, 1000);
        assert_eq!(pieces.len(), 1);
        let t = &pieces[0].text;
        assert!(t.contains("The results of the voting were as follows:\n\n| Nominee"));
        assert!(t.ends_with("Closing."));
    }

    /// The explanatory sentence is already at the tail of the previous chunk and the table does not fit in: the sentence has to go with the table, not stay in the previous chunk.
    #[test]
    fn a_caption_never_parts_from_its_table() {
        let filler = "Words about nothing in particular. ".repeat(12);
        let text = format!("{filler}\n\n{}", table(4));
        let pieces = chunk_with_budget(&text, 140);
        assert!(pieces.len() >= 2);
        let with_table = pieces
            .iter()
            .find(|p| p.text.contains("| Nominee |"))
            .expect("the chunk with the table");
        assert!(with_table
            .text
            .contains("The results of the voting were as follows:"));
        let before: Vec<&ChunkPiece> = pieces
            .iter()
            .filter(|p| !p.text.contains("| Nominee |"))
            .collect();
        for p in before {
            assert!(
                !p.text.contains("were as follows"),
                "说明句留在了没有表的块里:\n{}",
                p.text
            );
        }
    }

    #[test]
    fn a_chunk_opens_with_the_headings_it_lives_under() {
        let text = "# Report\n\n## Part A\n\nFirst.\n\n## Part B\n\nSecond.\n\n### B.1\n\nThird.";
        let pieces = chunk_with_budget(text, 1000);
        assert_eq!(pieces.len(), 1);
        assert!(pieces[0]
            .text
            .starts_with("# Report\n\n## Part A\n\nFirst."));
        assert_eq!(pieces[0].heading.as_deref(), Some("Report › Part B › B.1"));

        let pieces = chunk_with_budget(text, 14);
        let third = pieces.iter().find(|p| p.text.contains("Third.")).unwrap();
        assert!(
            third.text.starts_with("# Report\n\n## Part B\n\n### B.1"),
            "{}",
            third.text
        );
        assert_eq!(third.heading.as_deref(), Some("Report › Part B › B.1"));
        assert_eq!(
            &text[third.char_start as usize..third.char_end as usize],
            "### B.1\n\nThird."
        );
    }

    /// A chunk's breadcrumb is the chapter it holds, not the chapter it was pushed out to the next chunk for not fitting
    #[test]
    fn a_chunk_is_named_by_the_section_it_holds() {
        let text = "# Report\n\n## Part A\n\nFirst.\n\n## Part B\n\nSecond.\n\n### B.1\n\nThird.";
        let named: Vec<(String, Option<String>)> = chunk_with_budget(text, 14)
            .into_iter()
            .map(|p| (p.text.lines().last().unwrap_or("").to_string(), p.heading))
            .collect();
        let expected = [
            ("First.", "Report › Part A"),
            ("Second.", "Report › Part B"),
            ("Third.", "Report › Part B › B.1"),
        ]
        .map(|(last, heading)| (last.to_string(), Some(heading.to_string())));
        assert_eq!(named, expected);
    }

    /// A document that ends in a heading: the body collected so far is closed as a chunk, and the heading becomes a chunk of its own
    #[test]
    fn a_document_that_ends_with_a_heading_keeps_its_last_text() {
        let text = "# Report\n\nFirst paragraph of the report.\n\nSecond paragraph of the report.\n\n## Appendix\n";
        let pieces = chunk_with_budget(text, BUDGET_TOKENS);
        assert_eq!(pieces.len(), 2, "{pieces:#?}");
        assert!(
            pieces[0].text.contains("First paragraph of the report.")
                && pieces[0].text.contains("Second paragraph of the report."),
            "{pieces:#?}"
        );
        assert_eq!(pieces[0].heading.as_deref(), Some("Report"));
        assert_eq!(pieces[1].text.trim_end(), "# Report\n\n## Appendix");
        assert_eq!(pieces[1].heading.as_deref(), Some("Report › Appendix"));
        assert_eq!(
            text[pieces[1].char_start as usize..pieces[1].char_end as usize].trim_end(),
            "## Appendix"
        );
    }

    #[test]
    fn a_long_paragraph_still_splits_by_sentence_and_offsets_are_verbatim() {
        let text = "One sentence here. Another sentence follows it. ".repeat(30);
        let pieces = chunk_with_budget(&text, 40);
        assert!(pieces.len() > 1);
        for p in &pieces {
            assert_eq!(&text[p.char_start as usize..p.char_end as usize], p.text);
            assert!(tokens(&p.text) <= 40);
        }
    }

    /// The first paragraph under a section is over budget on its own, so it falls back to splitting by sentence: the first chunk cut is "the parent level (prefix) · this section's heading ·
    /// first small piece", repeating each heading; continuation chunks still start with the full path as before
    #[test]
    fn the_first_chunk_of_a_split_paragraph_carries_its_headings_once() {
        let text = format!(
            "# Report\n\nIntro.\n\n## Part A\n\n{}",
            "One sentence here. Another sentence follows it. ".repeat(30)
        );
        let pieces = chunk_with_budget(&text, 40);
        let i = pieces
            .iter()
            .position(|p| p.text.contains("One sentence"))
            .expect("the first chunk split from a long paragraph");
        let source = |p: &ChunkPiece| &text[p.char_start as usize..p.char_end as usize];
        assert_eq!(
            pieces[i].text,
            format!("# Report\n\n{}", source(&pieces[i])),
            "{} token，预算 40",
            tokens(&pieces[i].text)
        );
        assert_eq!(
            pieces[i + 1].text,
            format!("# Report\n\n## Part A\n\n{}", source(&pieces[i + 1]))
        );
    }

    #[test]
    fn chunks_are_numbered_from_zero_and_never_empty() {
        let text = "# T\n\nA.\n\n* * *\n\nB.\n\n| x | y |\n| --- | --- |\n| 1 | 2 |";
        let pieces = chunk_text(text);
        assert_eq!(pieces.len(), 1);
        for (i, p) in pieces.iter().enumerate() {
            assert_eq!(p.seq as usize, i);
            assert!(!p.text.trim().is_empty());
        }
        assert!(
            !pieces[0].text.contains("* * *"),
            "horizontal rule does not go into a chunk"
        );
    }

    /// The page number "4" at the end of a press release: it does not form a chunk alone; it joins the previous chunk even if that chunk is already full.
    #[test]
    fn a_page_number_never_becomes_a_chunk_of_its_own() {
        let body = "A sentence that fills the budget nicely. ".repeat(6);
        let text = format!(
            "{body}

4"
        );
        let pieces = chunk_with_budget(&text, tokens(body.trim()));
        assert_eq!(
            pieces.len(),
            1,
            "{:?}",
            pieces.iter().map(|p| &p.text).collect::<Vec<_>>()
        );
        assert!(pieces[0].text.ends_with(
            "

4"
        ));
    }

    /// The shape of item five of the resolutions: explanatory sentence · page break · table. The page break does not separate the sentence from its table.
    #[test]
    fn a_page_break_between_a_caption_and_its_table_does_not_part_them() {
        let filler = "Words about nothing in particular. ".repeat(10);
        let text = format!(
            "{filler}\n\n5. Stockholders did not approve the proposal. The results of the voting were as follows:\n\n* * *\n\n| Number of shares For | 144 |\n| --- | --- |\n| Number of shares Against | 16 |\n"
        );
        let pieces = chunk_with_budget(&text, 110);
        let table = pieces
            .iter()
            .find(|p| p.text.contains("| Number of shares For |"))
            .expect("the chunk with the table");
        assert!(
            table
                .text
                .contains("The results of the voting were as follows:"),
            "{}",
            table.text
        );
        for p in pieces
            .iter()
            .filter(|p| !p.text.contains("| Number of shares For |"))
        {
            assert!(
                !p.text.contains("were as follows"),
                "说明句留在了没有表的块里:\n{}",
                p.text
            );
        }
    }

    /// The shape of a financial report: company name, report name, units, and "Unaudited": four short explanatory paragraphs, followed by a table.
    /// All four paragraphs are explanatory sentences, and every chunk the table is cut into carries all four.
    #[test]
    fn a_run_of_short_paragraphs_before_a_table_is_its_caption() {
        let text = format!(
            "Prose that is long enough not to be a caption, going on about the quarter and the outlook and the products and the customers.\n\n\
             NVIDIA CORPORATION\n\nCONDENSED CONSOLIDATED STATEMENTS OF INCOME\n\n(In millions)\n\n(Unaudited)\n\n{}",
            table(30)
        );
        let pieces = chunk_with_budget(&text, 120);
        let with_table: Vec<&ChunkPiece> = pieces
            .iter()
            .filter(|p| p.text.contains("| --- |"))
            .collect();
        assert!(
            with_table.len() >= 2,
            "table should be split into several chunks: {}",
            pieces.len()
        );
        for p in &with_table {
            assert!(
                p.text.contains("NVIDIA CORPORATION"),
                "每块都带公司名: {}",
                p.text
            );
            assert!(
                p.text.contains("STATEMENTS OF INCOME"),
                "每块都带报表名: {}",
                p.text
            );
            assert!(
                p.text.contains("(Unaudited)"),
                "每块都带最后一段: {}",
                p.text
            );
            assert!(
                !p.text.contains("Prose that is long"),
                "长段落不是说明句: {}",
                p.text
            );
        }
    }

    /// The shape of a Chinese annual report: a sentence "...main accounting data is as follows" that is longer than `CAPTION_MAX_BYTES` can only be recognized as an explanatory sentence by its trailing colon (full-width).
    /// Each chunk the table is cut into starts with this sentence and the header; the sentence does not form a chunk of its own.
    #[test]
    fn a_caption_ending_in_a_full_width_colon_stays_with_its_table_at_any_length() {
        let lead = "根据公司第三届董事会第十二次会议审议通过的二〇二五年半年度报告，本报告期内公司按照企业会计准则编制合并财务报表，经营情况总体平稳，现金流量充足，主要会计数据如下：";
        assert!(lead.len() > CAPTION_MAX_BYTES);
        let rows: String = (0..60)
            .map(|i| format!("| 科目{i} | {} |\n", 10_000 + i))
            .collect();
        let text = format!("{lead}\n\n| 项目 | 本期 |\n| --- | --- |\n{rows}");
        let pieces = chunk_with_budget(&text, BUDGET_TOKENS);
        assert!(
            pieces.len() >= 2,
            "table should be split into at least 2 chunks: {}",
            pieces.len()
        );
        for (i, p) in pieces.iter().enumerate() {
            assert!(
                p.text.starts_with(&format!("{lead}\n\n| 项目 | 本期 |")),
                "第 {i} 块不以说明句和表头开头:\n{}",
                p.text
            );
        }
    }

    /// The shape of ten directors: one sentence, then a run of tables. Every table, wherever it lands, can see that sentence;
    /// for several tables in the same chunk, the explanation is read only once.
    #[test]
    fn one_caption_introduces_the_whole_run_of_tables_after_it() {
        let mut text = String::from("Stockholders elected each of the director nominees. The results of the voting were as follows:\n\n");
        for (i, name) in [
            "Tench Coxe",
            "John Dabiri",
            "Jen-Hsun Huang",
            "Dawn Hudson",
            "Harvey Jones",
        ]
        .iter()
        .enumerate()
        {
            text.push_str(&format!(
                "| {name} |  |\n| --- | --- |\n| Number of shares For | {} |\n| Number of shares Against | {} |\n\n",
                1000 + i,
                50 + i
            ));
        }
        text.push_str(
            "Another matter entirely.\n\n| Unrelated | 1 |\n| --- | --- |\n| row | 2 |\n",
        );
        let pieces = chunk_with_budget(&text, 90);
        assert!(pieces.len() >= 3, "{}", pieces.len());
        let huang = pieces
            .iter()
            .find(|p| p.text.contains("Jen-Hsun Huang"))
            .expect("the Huang Renxun chunk");
        assert!(
            huang.text.contains("director nominees"),
            "续块丢了说明句:\n{}",
            huang.text
        );
        for p in &pieces {
            assert!(
                p.text.matches("were as follows").count() <= 1,
                "同一块里说明念了不止一遍:\n{}",
                p.text
            );
        }
        // The run lasts until something else appears: the unrelated table after it does not carry this sentence
        let unrelated = pieces
            .iter()
            .find(|p| p.text.contains("| Unrelated |"))
            .unwrap();
        let before = &unrelated.text[..unrelated.text.find("| Unrelated |").unwrap()];
        assert!(!before.ends_with("follows:\n\n"), "{}", unrelated.text);
        assert!(unrelated.text.contains("Another matter entirely."));
    }

    #[test]
    fn a_rule_is_not_a_boundary() {
        let text = "Alpha.\n\n* * *\n\nBeta.";
        assert_eq!(chunk_with_budget(text, 1000).len(), 1);
    }
}
