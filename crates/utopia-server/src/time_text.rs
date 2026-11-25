//! How time is written for the model: **one rule, which every tool line goes through**.
//!
//! - Endpoints on the world axis are written at **their own precision**: year → `2023`, month → `2023-06`, day → `2023-06-01`.
//!   It used to be `%Y-%m-%d` throughout, which printed a year-precision fact as 1 January: the very problem the
//!   comment on `facts.valid_from_precision` describes, filling in a definite value where nothing is known. Write as many digits as the precision has.
//! - Moments without a precision (`at`, `as_of`, `recorded_at`, `doc_time`, and the bounds that anchors bring up
//!   (the two ends of derived rows, and the start of a fact with no start, read out per 0022) are written as full RFC3339, with fractional seconds.
//!   Within one second on the recording axis, an entry can be made before it is corrected (#351); truncating to day or second would fold the two beliefs back together.
//! - "Ended, date unknown" is written as `ended by <anchor>`, never `now`.
//!
//! The `web/src/time.ts::fmtTime` in the UI and the exported `rdf::world_time` are the other two copies of this same rule;
//! if the three diverge, what people see, what the model sees and what an auditor gets will not be the same date.
use chrono::{DateTime, SecondsFormat, Utc};

/// A moment, written out in full.
pub fn instant(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// One end of the world axis, written at its precision. Below hour precision, use the abbreviated ISO 8601 form (`2026-06-01T14:32Z`),
/// so the same string parses back to the same value and precision. Without a precision it is a moment (an anchor, a derived bound), written in full.
pub fn world(t: DateTime<Utc>, precision: Option<&str>) -> String {
    match precision {
        Some("year") => t.format("%Y").to_string(),
        Some("month") => t.format("%Y-%m").to_string(),
        Some("day") => t.format("%Y-%m-%d").to_string(),
        Some("hour") => t.format("%Y-%m-%dT%HZ").to_string(),
        Some("minute") => t.format("%Y-%m-%dT%H:%MZ").to_string(),
        Some("second") => t.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        _ => instant(t),
    }
}

/// The two ends of a fact: what the source states (`valid_*` and precision) and what is read out (`holds_*`, 0022), and where the date
/// comes from (#970): the end is what the timeline inferred, and the interval is what a person edited. Neither is in the source sentence
/// that the line's `[n]` opens.
#[derive(Debug, Clone, Copy, Default)]
pub struct Span<'a> {
    pub valid_from: Option<DateTime<Utc>>,
    pub from_precision: Option<&'a str>,
    pub valid_to: Option<DateTime<Utc>>,
    pub to_precision: Option<&'a str>,
    pub holds_from: Option<DateTime<Utc>>,
    pub holds_to: Option<DateTime<Utc>>,
    /// The end is closed by the timeline when the next fact starts (`facts.end_derived`)
    pub end_derived: bool,
    /// Which end a person corrected: `start` / `end` / `both` (`facts.corrected_ends`); None means never corrected
    pub corrected: Option<&'a str>,
    /// The line that closes it (#970 step 2): the end that succeeds it, and the number of its evidence (` [n]` in chat, empty in MCP)
    pub closed_by: Option<(&'a str, &'a str)>,
    /// A note written when a person edited the interval
    pub correction_note: Option<&'a str>,
}

/// A note on the line is at most this many characters: any longer and it is a paragraph, not a caption
const NOTE_CHARS: usize = 120;

/// Folds the note into one line, strips the outer whitespace, and truncates if too long. An empty one means nothing was written
fn note_text(note: &str) -> Option<String> {
    // Replace the square brackets: the note is words a person wrote, and a `[2]` inside it would land on the line looking like a citation number
    let flat = note
        .replace('[', "(")
        .replace(']', ")")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flat.is_empty() {
        return None;
    }
    if flat.chars().count() > NOTE_CHARS {
        return Some(flat.chars().take(NOTE_CHARS).collect::<String>() + "…");
    }
    Some(flat)
}

/// `from → to`, shown to the model.
///
/// Start: if the source gave one, written at its precision; if not but there is an anchor, write `attested <moment>`, so the model knows
/// this fact is only documented from that evidence onward. End: if the source gave one, written at its precision; "ended, date unknown" is written
/// `ended by <anchor>` (or `ended, date unknown` with no anchor); otherwise `now`.
/// If neither end can be written, return an empty string, and the caller leaves out the brackets.
///
/// A date that is not what the source states is said after the interval (#970): an end a person corrected is written `start corrected` or
/// `end corrected`, and `corrected` when both ends were changed; an end that was not changed is still what the source says. If a note was written, include it
/// (`start corrected: …`; a person's words are not a source paragraph, so no number); an end the timeline inferred is written
/// `superseded by X [m]`, where `[m]` opens the source sentence that the successor fact reads out, which states this
/// date. If the successor's line cannot be found, write `end derived`. These are said only when there is an end. The `[n]` at the line's end is still
/// the source sentence this fact reads out itself
pub fn span(s: Span<'_>) -> String {
    let from = match (s.valid_from, s.holds_from) {
        (Some(t), _) => Some(world(t, s.from_precision)),
        // A start that was not stated is said not to be stated. It used to write `attested <document date>`, and the model read that date
        // aloud as the day the event happened ("OpenAI was confirmed as … on 6 September 2026"); the anchor is for filtering,
        // not for people to read
        (None, Some(_)) => Some("undated".to_string()),
        (None, None) => None,
    };
    let ended_unknown =
        s.valid_to.is_none() && s.to_precision == Some(utopia_store::graph::ENDED_UNKNOWN);
    let to = match (s.valid_to, ended_unknown, s.holds_to) {
        (Some(t), _, _) => Some(world(t, s.to_precision)),
        // "Ended, date unknown" is stated as is; the date of the document that states it is likewise not given
        (None, true, Some(_)) => Some("ended, date unknown".to_string()),
        (None, true, None) => Some("ended, date unknown".to_string()),
        (None, false, _) => None,
    };
    let derived = s.end_derived && to.is_some();
    let range = match (from, to) {
        (None, None) => String::new(),
        (Some(f), None) => format!("{f} → now"),
        (None, Some(t)) => format!("→ {t}"),
        (Some(f), Some(t)) => format!("{f} → {t}"),
    };
    let mut marks: Vec<String> = Vec::new();
    if let Some(ends) = s.corrected {
        let what = match ends {
            "start" => "start corrected",
            "end" => "end corrected",
            _ => "corrected",
        };
        marks.push(match s.correction_note.and_then(note_text) {
            Some(note) => format!("{what}: {note}"),
            None => what.to_string(),
        });
    }
    if derived {
        marks.push(match s.closed_by {
            Some((who, mark)) => format!("superseded by {who}{mark}"),
            None => "end derived".to_string(),
        });
    }
    match (range.is_empty(), marks.is_empty()) {
        (_, true) => range,
        (true, false) => marks.join(", "),
        (false, false) => format!("{range}, {}", marks.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn a_world_bound_shows_exactly_its_precision() {
        let at = t("2023-06-15T00:00:00Z");
        assert_eq!(world(at, Some("year")), "2023");
        assert_eq!(world(at, Some("month")), "2023-06");
        assert_eq!(world(at, Some("day")), "2023-06-15");
        let clock = t("2026-06-01T14:32:07.382Z");
        assert_eq!(world(clock, Some("hour")), "2026-06-01T14Z");
        assert_eq!(world(clock, Some("minute")), "2026-06-01T14:32Z");
        assert_eq!(world(clock, Some("second")), "2026-06-01T14:32:07Z");
        // What is written can be read back, at the same precision
        for (p, s) in [
            ("hour", "2026-06-01T14Z"),
            ("minute", "2026-06-01T14:32Z"),
            ("second", "2026-06-01T14:32:07Z"),
        ] {
            let (back, bp) = utopia_extract::parse_time(s).unwrap();
            assert_eq!((bp, world(back, Some(p))), (p, s.to_string()));
        }
        // No precision = one moment (an anchor, a derived bound): write it in full, and do not pass it off as a particular day
        assert_eq!(world(at, None), "2023-06-15T00:00:00Z");
    }

    #[test]
    fn an_instant_keeps_its_fraction_and_reads_back() {
        for s in [
            "2026-09-05T02:43:53Z",
            "2026-09-05T02:43:53.382Z",
            "2026-09-05T02:43:53.382001Z",
        ] {
            assert_eq!(instant(t(s)), s);
        }
    }

    #[test]
    fn a_span_reads_each_end_by_its_own_rule() {
        let day = |s: &str| Some(t(s));
        // The source gave both ends
        assert_eq!(
            span(Span {
                valid_from: day("2023-01-01T00:00:00Z"),
                from_precision: Some("year"),
                valid_to: day("2024-07-01T00:00:00Z"),
                to_precision: Some("month"),
                holds_from: day("2023-01-01T00:00:00Z"),
                holds_to: day("2024-07-01T00:00:00Z"),
                ..Span::default()
            }),
            "2023 → 2024-07"
        );
        // No start: from the evidence on; still ongoing
        assert_eq!(
            span(Span {
                holds_from: day("2024-02-20T00:00:00Z"),
                ..Span::default()
            }),
            "undated → now"
        );
        // Ended, date unknown: up to the document that states it, never now
        assert_eq!(
            span(Span {
                valid_from: day("2023-06-01T00:00:00Z"),
                from_precision: Some("day"),
                valid_to: None,
                to_precision: Some("unknown"),
                holds_from: day("2023-06-01T00:00:00Z"),
                holds_to: day("2025-10-15T00:00:00Z"),
                ..Span::default()
            }),
            "2023-06-01 → ended, date unknown"
        );
        // No anchor is available for a recording-axis event
        assert_eq!(
            span(Span {
                to_precision: Some("unknown"),
                ..Span::default()
            }),
            "→ ended, date unknown"
        );
        assert_eq!(span(Span::default()), "");
    }

    /// A date that is not what the source states is said after the interval (#970): an end closed by the timeline, or an interval a person corrected.
    /// With no end there is no "derived end"; an empty interval that was corrected is only said to be corrected
    #[test]
    fn a_span_says_when_a_date_is_not_the_passages() {
        let day = |s: &str| Some(t(s));
        let closed = Span {
            valid_from: day("2024-07-05T00:00:00Z"),
            from_precision: Some("day"),
            valid_to: day("2025-09-01T00:00:00Z"),
            to_precision: Some("day"),
            ..Span::default()
        };
        assert_eq!(
            span(Span {
                end_derived: true,
                ..closed
            }),
            "2024-07-05 → 2025-09-01, end derived"
        );
        assert_eq!(
            span(Span {
                corrected: Some("both"),
                ..closed
            }),
            "2024-07-05 → 2025-09-01, corrected"
        );
        assert_eq!(
            span(Span {
                corrected: Some("both"),
                end_derived: true,
                ..closed
            }),
            "2024-07-05 → 2025-09-01, corrected, end derived"
        );
        // The timeline closed it on a successor that states no start: ended, date unknown, and that is also derived
        assert_eq!(
            span(Span {
                valid_to: None,
                to_precision: Some("unknown"),
                end_derived: true,
                ..closed
            }),
            "2024-07-05 → ended, date unknown, end derived"
        );
        // An open row has no end to derive
        assert_eq!(
            span(Span {
                valid_to: None,
                to_precision: None,
                end_derived: true,
                ..closed
            }),
            "2024-07-05 → now"
        );
        assert_eq!(
            span(Span {
                corrected: Some("both"),
                ..Span::default()
            }),
            "corrected"
        );
    }
    /// When the line that closes it can be found, the derived end names who succeeds and carries that line's number (#970 step 2);
    /// MCP has no number. An interval a person corrected carries a note: folded into one line, truncated if too long, and empty means not written
    #[test]
    fn a_derived_end_names_what_closed_it_and_a_correction_its_note() {
        let day = |s: &str| Some(t(s));
        let closed = Span {
            valid_from: day("2024-07-05T00:00:00Z"),
            from_precision: Some("day"),
            valid_to: day("2025-09-01T00:00:00Z"),
            to_precision: Some("day"),
            end_derived: true,
            ..Span::default()
        };
        assert_eq!(
            span(Span {
                closed_by: Some(("Zhou Qi", " [3]")),
                ..closed
            }),
            "2024-07-05 → 2025-09-01, superseded by Zhou Qi [3]"
        );
        assert_eq!(
            span(Span {
                closed_by: Some(("Zhou Qi", "")),
                ..closed
            }),
            "2024-07-05 → 2025-09-01, superseded by Zhou Qi"
        );
        // An open row has no end, so there is no successor to name
        assert_eq!(
            span(Span {
                valid_to: None,
                to_precision: None,
                closed_by: Some(("Zhou Qi", " [3]")),
                ..closed
            }),
            "2024-07-05 → now"
        );
        let corrected = Span {
            corrected: Some("both"),
            end_derived: false,
            ..closed
        };
        assert_eq!(
            span(Span {
                correction_note: Some("  The charter date\n was the approval date "),
                ..corrected
            }),
            "2024-07-05 → 2025-09-01, corrected: The charter date was the approval date"
        );
        assert_eq!(
            span(Span {
                correction_note: Some("   "),
                ..corrected
            }),
            "2024-07-05 → 2025-09-01, corrected"
        );
        let long = "x".repeat(200);
        assert_eq!(
            span(Span {
                correction_note: Some(&long),
                ..corrected
            }),
            format!("2024-07-05 → 2025-09-01, corrected: {}…", "x".repeat(120))
        );
        assert_eq!(
            span(Span {
                corrected: Some("both"),
                correction_note: Some("approval date"),
                closed_by: Some(("Zhou Qi", " [3]")),
                ..closed
            }),
            "2024-07-05 → 2025-09-01, corrected: approval date, superseded by Zhou Qi [3]"
        );
    }

    /// If a person corrected only one end, only that end is named (#976 review): the uncorrected end is still what the source says, and the other end can still be
    /// derived by the timeline
    #[test]
    fn a_correction_names_the_end_a_person_changed() {
        let day = |s: &str| Some(t(s));
        let row = Span {
            valid_from: day("2023-02-01T00:00:00Z"),
            from_precision: Some("day"),
            valid_to: day("2024-07-05T00:00:00Z"),
            to_precision: Some("day"),
            ..Span::default()
        };
        assert_eq!(
            span(Span {
                corrected: Some("start"),
                correction_note: Some("The charter date was the approval date"),
                ..row
            }),
            "2023-02-01 → 2024-07-05, start corrected: The charter date was the approval date"
        );
        assert_eq!(
            span(Span {
                corrected: Some("end"),
                ..row
            }),
            "2023-02-01 → 2024-07-05, end corrected"
        );
        assert_eq!(
            span(Span {
                corrected: Some("start"),
                end_derived: true,
                closed_by: Some(("Li Si", " [2]")),
                ..row
            }),
            "2023-02-01 → 2024-07-05, start corrected, superseded by Li Si [2]"
        );
    }
}
