//! Segments returned by the transcription service → one body text + provenance (0040, third slice).
//!
//! The request goes to OpenAI's `/audio/transcriptions` and asks for `diarized_json`: each sentence in `segments` carries
//! `speaker`, `text`, `start`, and `end` (in seconds).
//!
//! **Speakers are written into the body text.** Extraction only reads text and cannot see the speaker in the anchor; who said "we delivered in Q3" has to be
//! on that line of text. Consecutive sentences from the same person are merged into one paragraph (one speaking turn); a change of speaker starts a new one, and each paragraph opens with
//! `Speaker A:`. The prefix is structure, the label is what the service gave, and no names are guessed.
//!
//! The whole recording is one provenance segment, and chunks are cut to a size budget; each chunk's anchor takes the earliest start, the latest end and the
//! speakers who appear (in order of appearance) across the sentences it covers. A sentence's time is exact to itself; a chunk's time is those sentences joined together.

use crate::provenance::{Origin, Provenance, Segment};
use crate::reading::{Place, Reading, Region};
use crate::{NoSpeakers, Unreadable};
use serde_json::{json, Value};

/// Reads the transcription result. If any sentence has text but no speaker, the whole result is rejected
pub fn reading(response: &Value, model: &str) -> anyhow::Result<Reading> {
    let segments = response["segments"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut text = String::new();
    let mut regions: Vec<Region> = Vec::new();
    let mut last_speaker: Option<String> = None;
    for seg in segments {
        let said = crate::mineru::without_nul(seg["text"].as_str().unwrap_or_default());
        let said = said.split_whitespace().collect::<Vec<_>>().join(" ");
        if said.is_empty() {
            continue;
        }
        let speaker = match &seg["speaker"] {
            Value::String(s) if !s.trim().is_empty() => s.trim().to_string(),
            Value::Number(n) => n.to_string(),
            _ => return Err(NoSpeakers.into()),
        };
        let start = text.len();
        if last_speaker.as_deref() == Some(speaker.as_str()) {
            text.push(' ');
        } else {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("Speaker {speaker}: "));
        }
        text.push_str(&said);
        regions.push(Region {
            range: start..text.len(),
            place: Place::Time {
                start_ms: millis(&seg["start"]),
                end_ms: millis(&seg["end"]),
                speaker: speaker.clone(),
            },
        });
        last_speaker = Some(speaker);
    }
    if text.is_empty() {
        // The whole thing has no sentences; a single undivided block of text also cannot tell who is speaking
        if response["text"]
            .as_str()
            .is_some_and(|t| !t.trim().is_empty())
        {
            return Err(NoSpeakers.into());
        }
        return Err(Unreadable("No speech was found in this recording".into()).into());
    }
    let (first, last) = (&regions[0], &regions[regions.len() - 1]);
    let whole = |p: &Place| match p {
        Place::Time {
            start_ms, end_ms, ..
        } => (*start_ms, *end_ms),
        Place::Page { .. } => (0, 0),
    };
    // The segment's anchor first covers the whole recording; after chunking, it is rewritten from the sentences each chunk covers
    let anchor = json!({
        "start_ms": whole(&first.place).0,
        "end_ms": whole(&last.place).1,
        "speaker": [],
    });
    Ok(Reading {
        segments: vec![Segment {
            range: 0..text.len(),
            provenance: Provenance {
                origin: Origin::Transcribed,
                model: Some(model.to_string()),
                anchor: Some(anchor),
            },
        }],
        text,
        regions,
    })
}

fn millis(v: &Value) -> u64 {
    (v.as_f64().unwrap_or(0.0).max(0.0) * 1000.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meeting() -> Value {
        json!({
            "text": "…",
            "segments": [
                { "id": "seg_0", "type": "transcript.text.segment", "speaker": "A", "start": 0.0, "end": 2.4, "text": "Let's review the Beta Robotics deal." },
                { "id": "seg_1", "type": "transcript.text.segment", "speaker": "A", "start": 2.4, "end": 5.1, "text": " Who owns delivery?" },
                { "id": "seg_2", "type": "transcript.text.segment", "speaker": "B", "start": 5.3, "end": 9.05, "text": "I will deliver the prototype in Q3." },
                { "id": "seg_3", "type": "transcript.text.segment", "speaker": "A", "start": 9.2, "end": 10.0, "text": "   " }
            ]
        })
    }

    #[test]
    fn a_turn_says_who_spoke_it() {
        let r = reading(&meeting(), "gpt-4o-transcribe-diarize").unwrap();
        assert_eq!(
            r.text,
            "Speaker A: Let's review the Beta Robotics deal. Who owns delivery?\n\nSpeaker B: I will deliver the prototype in Q3."
        );
        assert_eq!(r.segments.len(), 1);
        assert_eq!(r.segments[0].range, 0..r.text.len());
    }

    #[test]
    fn a_chunk_carries_its_times_and_speakers() {
        let r = reading(&meeting(), "gpt-4o-transcribe-diarize").unwrap();
        let pieces = r.chunk(300);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].provenance.origin, Origin::Transcribed);
        assert_eq!(
            pieces[0].provenance.anchor,
            Some(json!({ "start_ms": 0, "end_ms": 9050, "speaker": ["A", "B"] }))
        );
        // Keep chunks a little smaller: each chunk records only the times and speakers of its own sentences
        let small = r.chunk(12);
        assert!(small.len() > 1, "{small:#?}");
        let last = small.last().unwrap();
        assert_eq!(
            last.provenance.anchor.as_ref().unwrap()["speaker"],
            json!(["B"])
        );
        assert_eq!(
            last.provenance.anchor.as_ref().unwrap()["end_ms"],
            json!(9050)
        );
        assert!(
            last.provenance.anchor.as_ref().unwrap()["start_ms"]
                .as_u64()
                .unwrap()
                >= 5300
        );
    }

    #[test]
    fn a_transcript_without_speakers_is_refused() {
        let unlabelled =
            json!({ "segments": [{ "start": 0.0, "end": 1.0, "text": "We ship in Q3." }] });
        let err = reading(&unlabelled, "whisper-1").unwrap_err();
        assert!(err.downcast_ref::<NoSpeakers>().is_some());
        let plain = json!({ "text": "We ship in Q3." });
        assert!(reading(&plain, "whisper-1")
            .unwrap_err()
            .downcast_ref::<NoSpeakers>()
            .is_some());
        let silent = json!({ "text": "", "segments": [] });
        assert!(reading(&silent, "m")
            .unwrap_err()
            .downcast_ref::<Unreadable>()
            .is_some());
    }
}
