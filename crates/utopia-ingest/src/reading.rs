//! Body text read by a model (0040): the joined text, the segments split by provenance, and the position of each small piece in the source file.
//!
//! A scanned document returns regions with a page number and a box; a recording returns sentences with start and end times and a speaker. Both
//! are joined into body text and chunked by the same rules. After chunking, each chunk's anchor is computed from the small pieces it covers: for a scanned document, the
//! bounding rectangle of the boxes; for a recording, the earliest start, the latest end and the speakers who appear.
//!
//! Segments are coarser than the small pieces (a scanned page is one segment; a recording is one segment as a whole), because a chunk holds only one provenance (decision 2). Splitting by small piece
//! would make one sentence one chunk, too fragmented for extraction to see context.

use crate::chunker::{chunk_segments, ChunkPiece};
use crate::provenance::Segment;
use serde_json::json;
use std::ops::Range;

/// The joined body text, along with its provenance and the position of each small piece
#[derive(Debug, Clone)]
pub struct Reading {
    pub text: String,
    pub segments: Vec<Segment>,
    pub(crate) regions: Vec<Region>,
}

/// A small piece of body text, and its position in the source file
#[derive(Debug, Clone)]
pub(crate) struct Region {
    pub range: Range<usize>,
    pub place: Place,
}

#[derive(Debug, Clone)]
pub(crate) enum Place {
    /// A block on a scanned page: the page number counts from 1, and the box comes from the engine (normalized to 0–1000)
    Page { page: u64, bbox: Option<[f64; 4]> },
    /// A sentence in a recording
    Time {
        start_ms: u64,
        end_ms: u64,
        speaker: String,
    },
}

impl Reading {
    /// Chunks by segment, then writes each chunk's anchor from the small pieces it covers
    pub fn chunk(&self, budget: usize) -> Vec<ChunkPiece> {
        let mut pieces = chunk_segments(&self.text, &self.segments, budget);
        for p in &mut pieces {
            let span = p.char_start as usize..p.char_end as usize;
            let Some(anchor) = p.provenance.anchor.as_mut() else {
                continue;
            };
            let covered = self
                .regions
                .iter()
                .filter(|r| r.range.start < span.end && span.start < r.range.end);
            let page = anchor["page"].as_u64();
            let mut bbox: Option<[f64; 4]> = None;
            let mut time: Option<(u64, u64)> = None;
            let mut speakers: Vec<&str> = Vec::new();
            for r in covered {
                match &r.place {
                    Place::Page { page: pg, bbox: b } if Some(*pg) == page => {
                        if let Some(b) = b {
                            bbox = Some(bbox.map_or(*b, |a| {
                                [
                                    a[0].min(b[0]),
                                    a[1].min(b[1]),
                                    a[2].max(b[2]),
                                    a[3].max(b[3]),
                                ]
                            }));
                        }
                    }
                    Place::Page { .. } => {}
                    Place::Time {
                        start_ms,
                        end_ms,
                        speaker,
                    } => {
                        time = Some(time.map_or((*start_ms, *end_ms), |(s, e)| {
                            (s.min(*start_ms), e.max(*end_ms))
                        }));
                        if !speakers.contains(&speaker.as_str()) {
                            speakers.push(speaker);
                        }
                    }
                }
            }
            if let Some(b) = bbox {
                anchor["bbox"] = json!(b);
            }
            if let Some((start, end)) = time {
                anchor["start_ms"] = json!(start);
                anchor["end_ms"] = json!(end);
                anchor["speaker"] = json!(speakers);
            }
        }
        pieces
    }
}
