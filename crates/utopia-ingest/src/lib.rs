//! utopia-ingest: parse matrix and chunking.
//! Principle: the text layer is handled natively in Rust (fast, zero dependencies); scanned files and complex layouts go later through the docling sidecar.

#[cfg(test)]
mod html_tests {
    #[test]
    fn html_fragment_preserves_scoped_content() {
        let raw = "<nav>Scoped feed navigation</nav><p>Short <em>entry</em>.</p>";
        let markdown = super::html::fragment_to_markdown(raw).unwrap();
        assert!(markdown.contains("Scoped feed navigation"));
        assert!(markdown.contains("*entry*"));
    }
    #[test]
    fn html_readability_failure_uses_raw_conversion() {
        let raw = "<p>Short <strong>fallback</strong> body.</p>";
        // An invalid base URL deterministically fails Readability setup.
        assert!(dom_smoothie::Readability::new(raw, Some("not a URL"), None).is_err());
        assert_eq!(
            super::html::page_to_markdown(raw, Some("not a URL")).unwrap(),
            super::html::fragment_to_markdown(raw).unwrap()
        );
    }
    #[test]
    fn html_empty_output_remains_parse_error() {
        assert!(super::parse("empty.html", b"<html><body></body></html>").is_err());
    }
    #[test]
    fn html_interstitial_errors_are_stable_for_pages_and_fragments() {
        for raw in [
            "<form><input type='email'></form>",
            "<video data-player='player'></video>",
        ] {
            assert_eq!(
                super::html::page_to_markdown(raw, Some("not a URL")),
                Err(super::html::HtmlError::Interstitial)
            );
            assert_eq!(
                super::html::fragment_to_markdown(raw),
                Err(super::html::HtmlError::Interstitial)
            );
        }
    }
    #[test]
    fn html_markdown_normalization_removes_unsafe_destinations() {
        let normalized = super::html::normalize_markdown("  [bad](javascript:evil) [encoded](%64ata:text/plain,evil) [safe](https://example.com)\n\n\n\n  ").unwrap();
        assert_eq!(normalized, "bad encoded [safe](https://example.com)");
    }
    #[test]
    fn html_interstitial_is_not_raw_fallback() {
        let html = "<form><input type='password'></form><p>Readable shell</p>";
        assert!(super::parse("page.html", html.as_bytes()).is_err());
    }
    #[test]
    fn html_long_login_only_body_is_rejected_after_extraction_and_on_fallback() {
        let raw = format!("<html><body><h1>Sign in to continue</h1><form><input type='email'><input type='password'></form><p>{}</p></body></html>", "Account access requires verification of your identity before proceeding. ".repeat(40));
        for base in [Some("https://example.com/login"), Some("not a URL")] {
            assert_eq!(
                super::html::page_to_markdown(&raw, base),
                Err(super::html::HtmlError::Interstitial)
            );
        }
    }

    #[test]
    fn html_substantive_fallback_survives_newsletter_controls() {
        let body =
            "Detailed reporting on the community with sources and supporting evidence. ".repeat(30);
        let raw = format!("<form><input type='email'></form><article><p>{body}</p></article>");
        assert!(dom_smoothie::Readability::new(raw.as_str(), Some("not a URL"), None).is_err());
        let markdown = super::html::page_to_markdown(&raw, Some("not a URL")).unwrap();
        assert!(markdown.contains(body.trim()));
    }

    #[test]
    fn html_articles_survive_newsletter_and_login_chrome() {
        let body = "Reporting on password security with detailed supporting evidence. ".repeat(80);
        for chrome in [
            "<aside><form><label>Newsletter</label><input type='email'></form></aside>",
            "<aside><form><label>Login</label><input type='password'></form></aside>",
        ] {
            let raw = format!("<html><body>{chrome}<article><h1>Security reporting</h1><p>{body}</p></article></body></html>");
            let parsed = super::parse("page.html", raw.as_bytes()).unwrap();
            assert!(parsed.text.contains(body.trim()));
        }
    }

    #[test]
    fn html_page_removes_chrome() {
        let body = "Substantive reporting with detailed evidence. ".repeat(80);
        let html = format!("<html><head><title>Story</title></head><body><nav>Navigation noise</nav><article><h1>Story</h1><p>{body}</p></article><footer>Footer noise</footer></body></html>");
        let parsed = super::parse("page.html", html.as_bytes()).unwrap();
        assert!(parsed.text.contains("Story"));
        assert!(parsed.text.contains(body.trim()));
        assert!(!parsed.text.contains("Navigation noise"), "{}", parsed.text);
        assert!(!parsed.text.contains("Footer noise"));
    }
    #[test]
    fn html_preserves_markdown_structure() {
        let parsed = super::parse(
            "page.html",
            b"<article><h1>Story</h1><p>Useful <strong>body</strong>.</p></article>",
        )
        .unwrap();
        assert!(parsed.text.contains("**body**"), "{}", parsed.text);
    }
}

#[cfg(test)]
mod reader_tests {
    use super::*;

    const PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D,
    ];
    const MP3: &[u8] = &[b'I', b'D', b'3', 4, 0, 0, 0, 0, 0, 0];
    const WAV: &[u8] = &[
        b'R', b'I', b'F', b'F', 0x24, 0, 0, 0, b'W', b'A', b'V', b'E', b'f', b'm', b't', b' ',
    ];

    fn needs(filename: &str, bytes: &[u8]) -> Option<Reader> {
        parse(filename, bytes)
            .err()
            .and_then(|e| e.downcast_ref::<NeedsReader>().map(|n| n.reader))
    }

    fn unreadable(filename: &str, bytes: &[u8]) -> bool {
        parse(filename, bytes)
            .err()
            .is_some_and(|e| e.downcast_ref::<Unreadable>().is_some())
    }

    #[test]
    fn an_image_or_a_recording_waits_for_its_reader() {
        assert_eq!(needs("scan.png", PNG), Some(Reader::Ocr));
        // The extension lies: the file header decides
        assert_eq!(needs("notes.txt", PNG), Some(Reader::Ocr));
        assert_eq!(needs("meeting.mp3", MP3), Some(Reader::Transcribe));
        assert_eq!(needs("meeting.wav", WAV), Some(Reader::Transcribe));
        // When the header is not recognized, look at the extension
        assert_eq!(needs("photo.heic", &[0, 1, 2, 3]), Some(Reader::Ocr));
        assert_eq!(needs("call.amr", &[0, 1, 2, 3]), Some(Reader::Transcribe));
    }

    #[test]
    fn a_binary_is_not_decoded_as_text() {
        // Old-style .doc and the like: NUL bytes near the start; these used to be decoded into garbage and stored
        assert!(unreadable(
            "old.doc",
            &[0xD0, 0xCF, 0x11, 0xE0, 0, 0, 0, 0, b'x']
        ));
        assert!(unreadable(
            "clip.mp4",
            &[0, 0, 0, 0x18, b'f', b't', b'y', b'p', b'm', b'p', b'4', b'2']
        ));
        assert!(unreadable("empty.txt", b"   \n\n  "));
        // Real text is read as-is; UTF-16 with a byte order mark does not count as binary
        assert!(parse("a.txt", "你好，世界".as_bytes()).is_ok());
        assert!(!looks_binary(&[0xFF, 0xFE, b'h', 0, b'i', 0]));
        // Body text copied from a PDF text layer can contain NUL but is still valid UTF-8: read it (#611)
        assert!(parse("layer.md", "Revenue grew\0 twelve percent.".as_bytes()).is_ok());
    }
}

mod blocks;
mod chunker;
pub mod html;
mod lookalikes;
pub mod mineru;
pub mod ontology_rdf;
mod parsers;
pub mod provenance;
mod reading;
mod table;
pub mod transcript;

pub use chunker::{chunk_segments, chunk_text, chunk_with_budget, ChunkPiece, BUDGET_TOKENS};
/// Decode fetched text with the same encoding detection as file ingestion.
pub use parsers::plain_text as decode_text;
pub use provenance::{Origin, Provenance, Segment};
pub use reading::Reading;

/// Parse output: plain text plus optional structure info and observable parse warnings.
#[derive(Debug)]
pub struct ParsedDoc {
    pub text: String,
    pub warnings: Vec<ParseWarning>,
}

/// The parser hit its safety limit, so the body intentionally keeps only part of it; `detail` explains the omitted range.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseWarning {
    pub kind: &'static str,
    pub detail: serde_json::Value,
}

impl ParseWarning {
    pub const SPREADSHEET_ROWS_TRUNCATED: &'static str = "spreadsheet.rows_truncated";
    pub const CSV_RECORDS_TRUNCATED: &'static str = "csv.records_truncated";
}

/// Which kind of model is needed to read the characters (0040)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    /// Scanned documents and images: layout recognition (services such as MinerU)
    Ocr,
    /// Recordings: transcription with speaker labels
    Transcribe,
}

impl Reader {
    pub fn as_str(self) -> &'static str {
        match self {
            Reader::Ocr => "ocr",
            Reader::Transcribe => "transcribe",
        }
    }

    /// What the error says is missing
    fn wanted(self) -> &'static str {
        match self {
            Reader::Ocr => "a document-reading (OCR) service",
            Reader::Transcribe => "a transcription model that labels speakers",
        }
    }
}

/// This file has no text that can be read directly; it needs the model behind [`Reader`] to read it.
///
/// **Not a parse failure**: the file is not broken, this step just has no text parser. Images and recordings used to fall into
/// the "decode as text" branch, which produced a pile of garbage that was still chunked, embedded and extracted; scanned PDFs got a generic
/// "no text extracted" error and were retried three times. Now they stop here and go to a configured reader model; without one, they degrade and raise a warning
#[derive(Debug, Clone, thiserror::Error)]
#[error("This {what} has no text layer; reading it needs {}", reader.wanted())]
pub struct NeedsReader {
    pub reader: Reader,
    /// What kind of file the person sees: scanned PDF / image / recording
    pub what: &'static str,
}

/// The recording was transcribed, but the speakers could not be told apart (0040 decision 5).
///
/// Treated the same as having no transcription model configured: not read, the document stops and a warning says why. A meeting transcript
/// that cannot tell speakers apart reads "A says B delivered in Q3" and "B says A delivered in Q3" as the same line of text, so the commitment lands on the wrong person,
/// which is worse than extracting nothing. Once a model that labels speakers is configured, saving the setting queues it again.
#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "The transcription model did not say who spoke; a recording is read only with speaker labels"
)]
pub struct NoSpeakers;

/// This file cannot be read, and no model can read it (video, executables, old binary formats). Retrying does not help
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct Unreadable(pub String);

const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif",
];
const AUDIO_EXTS: &[&str] = &[
    "mp3", "wav", "m4a", "flac", "ogg", "oga", "opus", "aac", "amr", "wma",
];
const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "avi", "webm", "m4v", "wmv"];

/// Decides from the file header (when recognizable) or the extension whether this is a media file that a model has to read.
///
/// The header takes priority: the extension may lie. Only files whose header is not recognized fall back to the extension
fn media_kind(bytes: &[u8], ext: &str) -> Option<Result<NeedsReader, Unreadable>> {
    use infer::MatcherType;
    let by_header = infer::get(bytes).map(|t| t.matcher_type());
    let is = |m: MatcherType, exts: &[&str]| match by_header {
        Some(h) => h == m,
        None => exts.contains(&ext),
    };
    if is(MatcherType::Image, IMAGE_EXTS) {
        return Some(Ok(NeedsReader {
            reader: Reader::Ocr,
            what: "image",
        }));
    }
    if is(MatcherType::Audio, AUDIO_EXTS) {
        return Some(Ok(NeedsReader {
            reader: Reader::Transcribe,
            what: "recording",
        }));
    }
    if is(MatcherType::Video, VIDEO_EXTS) {
        return Some(Err(Unreadable(
            "Video files are not read yet; upload its audio track or a transcript".into(),
        )));
    }
    None
}

/// Before decoding as text, check whether the content is binary: it contains NUL near the start **and** is not valid UTF-8.
///
/// Both conditions are needed: body text copied from a PDF text layer can contain NUL but is valid UTF-8 (#611: read it, strip the NUL);
/// Chinese encodings such as GBK are not valid UTF-8 but contain no NUL. Old .doc files and archives hit both conditions.
/// Content with a UTF-16 byte order mark does not count as binary
fn looks_binary(bytes: &[u8]) -> bool {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return false;
    }
    let head = &bytes[..bytes.len().min(8192)];
    if !head.contains(&0) {
        return false;
    }
    // Truncation may cut in the middle of a multi-byte character: only check validity of the complete part
    match std::str::from_utf8(head) {
        Ok(_) => false,
        Err(e) => e.error_len().is_some(),
    }
}

/// Supported formats (P1): pdf / docx / xlsx·xls·ods / pptx / md / txt / html / csv / json / yaml / xml / log.
///
/// Those that need a model (scanned documents, images, recordings) return an error with [`NeedsReader`] attached; the ones that cannot be read at all get [`Unreadable`]
pub fn parse(filename: &str, bytes: &[u8]) -> anyhow::Result<ParsedDoc> {
    let ext = filename
        .rsplit('.')
        .next()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    match media_kind(bytes, &ext) {
        Some(Ok(needs)) => return Err(needs.into()),
        Some(Err(unreadable)) => return Err(unreadable.into()),
        None => {}
    }

    // Magic-number detection takes priority over the extension (the extension may lie)
    let kind = infer::get(bytes).map(|t| t.extension()).unwrap_or("");

    let (text, warnings) = match (kind, ext.as_str()) {
        ("pdf", _) | (_, "pdf") => {
            let text = parsers::pdf(bytes)?;
            // The text layer is empty: a scanned document, the text is in the image
            if text.trim().is_empty() {
                return Err(NeedsReader {
                    reader: Reader::Ocr,
                    what: "scanned PDF",
                }
                .into());
            }
            (text, Vec::new())
        }
        ("docx", _) | (_, "docx") => (parsers::docx(bytes)?, Vec::new()),
        ("xlsx", _) | (_, "xlsx") | (_, "xls") | (_, "ods") => parsers::spreadsheet(bytes)?,
        ("pptx", _) | (_, "pptx") => (parsers::pptx(bytes)?, Vec::new()),
        (_, "html") | (_, "htm") => (parsers::html(bytes)?, Vec::new()),
        (_, "csv") | (_, "tsv") => parsers::csv_text(bytes, ext == "tsv")?,
        // md/json/yaml/xml/log/txt and every unrecognized format: decode as text (encoding detection covers GBK and so on).
        // Check for binary content before decoding: old-style .doc, archives and executables decode to garbage,
        // which used to be chunked, embedded and extracted as-is
        _ if looks_binary(bytes) => {
            return Err(
                Unreadable("This file is not in a format that can be read as text".into()).into(),
            )
        }
        _ => (parsers::plain_text(bytes), Vec::new()),
    };

    let text = normalize(&text);
    if text.trim().is_empty() {
        return Err(Unreadable("No text could be extracted from this file".into()).into());
    }
    Ok(ParsedDoc { text, warnings })
}

/// Collapse consecutive blank lines and normalize line endings.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.replace("\r\n", "\n").replace('\r', "\n").lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out
}
