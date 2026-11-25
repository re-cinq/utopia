//! Where a piece of text comes from (0040): written in the source, recognized from a scanned page, transcribed from a recording, or described by a model looking at an image.
//!
//! Everything after chunking only consumes text and cannot tell whether a sentence was written in the file or paraphrased by a model. These four are not equal evidence:
//! the first three are "verbatim, read as-is", which a person can check against the page or recording; the fourth is a model's paraphrase of an image, with no original wording to check against.
//! So the origin follows the chunk, not the evidence line: evidence cites the chunk, and extraction reads the chunk too.

use std::ops::Range;

/// Origin of the text. The database stores the word from [`Origin::as_str`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Written in the file
    Stated,
    /// Characters recognized from a scanned page or screenshot (a digit may be misread)
    Ocr,
    /// Recording transcript (names are the easiest to mishear, and names are exactly what extraction is meant to pull out)
    Transcribed,
    /// A model's description of an image: no person wrote or said this sentence
    Described,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Stated => "stated",
            Origin::Ocr => "ocr",
            Origin::Transcribed => "transcribed",
            Origin::Described => "described",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stated" => Some(Origin::Stated),
            "ocr" => Some(Origin::Ocr),
            "transcribed" => Some(Origin::Transcribed),
            "described" => Some(Origin::Described),
            _ => None,
        }
    }
}

/// Provenance of a piece of text: its origin, the engine or model that read it, and the anchor pointing back to the bytes of the source file.
///
/// The anchor's shape is set by the origin (the database enforces it with a CHECK): source text has no anchor (`char_start` / `char_end`
/// already give the position); a scanned page is `{"page": n}`, with `"bbox"` added if the engine gave a box; a recording is
/// `{"start_ms", "end_ms", "speaker"}`; an image description points to the page and image
#[derive(Debug, Clone, PartialEq)]
pub struct Provenance {
    pub origin: Origin,
    pub model: Option<String>,
    pub anchor: Option<serde_json::Value>,
}

impl Provenance {
    pub fn stated() -> Self {
        Provenance {
            origin: Origin::Stated,
            model: None,
            anchor: None,
        }
    }
}

/// A segment of body text, along with its provenance. Engines that read scanned pages and recordings return text one page or one segment at a time; these are joined into one body
/// passed to chunking; chunking splits by segment, and **a chunk holds only one provenance** (0040 decision 2)
#[derive(Debug, Clone)]
pub struct Segment {
    /// Byte range within the joined body
    pub range: Range<usize>,
    pub provenance: Provenance,
}
