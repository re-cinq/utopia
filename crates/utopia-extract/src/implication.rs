//! Two model calls for implication rules (0044 decision 3, fifth slice).
//!
//! One is **proposing rules**: given a signature the aligner just judged (or a kind word), ask "for statements of this shape, besides the attribute they bind to,
//! what other attribute facts they imply, and where the object comes from". The answer is (attribute, reading or null); a null attribute = implies nothing.
//! The second is **readings**: for a word given as distinct, ask "under this reading, what does it refer to", answering a name, a value or null.
//! Readings are cached per word, so each word is asked once for life; the prompt has only the word and the reading, no document.
//!
//! Replies are all compact JSON, parsed as in phrase_align: a bad item is counted, not allowed to ruin the batch; ids not answered do not appear.

use crate::phrase_align::{candidate_line, PropertyCandidate};
use utopia_llm::ChatMessage;

/// One shape to propose a rule for: a signature or kind word, with example sentences and candidate attributes
#[derive(Debug, Clone)]
pub struct RuleItem<'a> {
    pub id: i64,
    /// phrase | kind_word
    pub trigger: &'a str,
    pub phrase: &'a str,
    pub subject_class: Option<&'a str>,
    pub object_class: Option<&'a str>,
    pub object_is_value: bool,
    /// Attribute keys the signature is already bound to (kind words have none); the proposed rule cannot be one of them
    pub bound_to: Option<&'a str>,
    pub examples: &'a [String],
    pub candidates: Vec<PropertyCandidate<'a>>,
}

/// The model's answer for one shape: Some((attribute key, reading)) or None (implies nothing)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleChoice {
    pub id: i64,
    pub implies: Option<(String, Option<String>)>,
}

const RULE_SYSTEM: &str = "\
You find facts a careful reader draws from a statement without the text stating them. \
Each numbered item is one shape: a relation phrase between a subject class and an object class \
(or a value), or a kind word the documents use for a thing, with example statements. The item \
may already be bound to a property; that binding is not the question. The question is whether \
this shape implies a fact of ANOTHER candidate property about the subject, and how the object \
of that fact is obtained:\n\
- null: the object is the statement's own object (the shape implies a second property about the same pair);\n\
- a reading: the object is read from the words of the statement's object (for a kind word, from the kind word itself). \
Readings available, by name:\n\
{READINGS}\n\
Examples of what is implied: a kind word \"British film\" implies country of origin = United Kingdom \
(reading country_of_nationality); \"located in the Piedmont region of Virginia\" implies country = \
United States (reading country_of_place); \"released in the summer of 1952\" implies publication year \
(reading year_of_phrase). Answer null when nothing beyond the binding is implied, when the implication \
would only sometimes hold, or when no candidate property fits.\n\
Answer with one JSON object and nothing else: {\"i\": [[id, \"property_key\" | null, \"reading\" | null]]}. \
Every item id appears exactly once.";

fn readings_text(readings: &[(&str, &str)]) -> String {
    readings
        .iter()
        .map(|(k, d)| format!("- {k}: {d}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn build_rule_messages(items: &[RuleItem<'_>], readings: &[(&str, &str)]) -> Vec<ChatMessage> {
    // The candidate table is written once per batch, and each item lists only keys (same reasoning as `phrase_align::build_phrase_messages`:
    // with the whole table in every item, one request ran over 50,000 tokens)
    let mut glossary: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in items.iter().flat_map(|i| i.candidates.iter()) {
        if seen.insert(c.key.trim()) {
            glossary.push(candidate_line(&PropertyCandidate {
                via: Vec::new(),
                ..c.clone()
            }));
        }
    }
    let mut user = String::new();
    if !glossary.is_empty() {
        user.push_str(&format!("Properties:\n{}\n\n", glossary.join("\n")));
    }
    for item in items {
        let shape = if item.trigger == "kind_word" {
            format!("kind word \"{}\"", item.phrase)
        } else {
            let object = if item.object_is_value {
                "value".to_string()
            } else {
                item.object_class.unwrap_or("?").to_string()
            };
            format!(
                "phrase \"{}\" · subject class: {} · object: {}",
                item.phrase,
                item.subject_class.unwrap_or("?"),
                object
            )
        };
        let mut examples = String::new();
        for ex in item.examples {
            examples.push_str(&format!("\n  · {ex}"));
        }
        if examples.is_empty() {
            examples.push_str(" (none)");
        }
        let bound = item
            .bound_to
            .map(|b| format!(" · already bound to: {b}"))
            .unwrap_or_default();
        let candidates = if item.candidates.is_empty() {
            " (none)".to_string()
        } else {
            let keys: Vec<&str> = item.candidates.iter().map(|c| c.key.trim()).collect();
            format!(" {}", keys.join(", "))
        };
        user.push_str(&format!(
            "Item {}: {shape}{bound}\nExamples:{examples}\nCandidates:{candidates}\n\n",
            item.id
        ));
    }
    vec![
        ChatMessage {
            role: "system".into(),
            content: RULE_SYSTEM.replace("{READINGS}", &readings_text(readings)),
        },
        ChatMessage {
            role: "user".into(),
            content: user.trim_end().to_string(),
        },
    ]
}

/// Parse the rule-proposal reply: `(verdicts, bad item count)`. Only the first answer per id is kept; an attribute key not among the candidates, a reading not
/// in the list, or the attribute that is already bound, all count as bad
pub fn parse_rule_response(
    raw: &str,
    items: &[RuleItem<'_>],
    readings: &[(&str, &str)],
) -> Result<(Vec<RuleChoice>, usize), String> {
    let v: serde_json::Value =
        serde_json::from_str(extract_json(raw)).map_err(|e| e.to_string())?;
    let rows = v
        .get("i")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "no \"i\" array".to_string())?;
    let mut out = Vec::new();
    let mut malformed = 0usize;
    // A bad row does not use up an id: only accepted answers count as answered, so a later good answer for the same id can still be accepted; a repeat is one that comes after an accepted answer
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let parsed = (|| {
            let arr = row.as_array()?;
            let id = arr.first()?.as_i64()?;
            let item = items.iter().find(|i| i.id == id)?;
            if seen.contains(&id) {
                return None;
            }
            let key = arr.get(1).and_then(|x| x.as_str());
            let reading = arr.get(2).and_then(|x| x.as_str());
            let implies = match key {
                None => None,
                Some(written) => {
                    let k = resolve_key(item, written)?;
                    if item.bound_to == Some(k) {
                        return None;
                    }
                    if let Some(r) = reading {
                        if !readings.iter().any(|(name, _)| *name == r) {
                            return None;
                        }
                    }
                    // A kind word has no object of its own: without a reading there is no object, and the answer is meaningless
                    if item.trigger == "kind_word" && reading.is_none() {
                        return None;
                    }
                    Some((k.to_string(), reading.map(str::to_string)))
                }
            };
            Some(RuleChoice { id, implies })
        })();
        match parsed {
            Some(choice) => {
                seen.insert(choice.id);
                out.push(choice);
            }
            None => malformed += 1,
        }
    }
    Ok((out, malformed))
}

/// A word to read
#[derive(Debug, Clone)]
pub struct ReadingItem<'a> {
    pub id: i64,
    pub reading: &'a str,
    pub phrase: &'a str,
}

/// The reading's answer: a name (an existing thing in the library), a value, or unreadable
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadingAnswer {
    pub id: i64,
    pub name: Option<String>,
    pub value: Option<String>,
}

const READING_SYSTEM: &str = "\
You read a short phrase in a stated way and answer what it names. Each numbered item gives the \
reading and the phrase. Readings:\n\
{READINGS}\n\
Answer the canonical English name of the thing (a country's common name, e.g. \"United Kingdom\", \
\"United States\"), or for year_of_phrase the four-digit year as a string, or null when the phrase \
does not determine an answer (an ambiguous demonym, a place you cannot place, no year in the words). \
Do not guess. Answer with one JSON object and nothing else: {\"r\": [[id, \"answer\" | null]]}. \
Every item id appears exactly once.";

pub fn build_reading_messages(
    items: &[ReadingItem<'_>],
    readings: &[(&str, &str)],
) -> Vec<ChatMessage> {
    let user = items
        .iter()
        .map(|i| format!("Item {}: {} · \"{}\"", i.id, i.reading, i.phrase))
        .collect::<Vec<_>>()
        .join("\n");
    vec![
        ChatMessage {
            role: "system".into(),
            content: READING_SYSTEM.replace("{READINGS}", &readings_text(readings)),
        },
        ChatMessage {
            role: "user".into(),
            content: user,
        },
    ]
}

/// Parse the reading reply. A year reading's answer becomes a value, the rest become names; an empty string or a non-four-digit year counts as bad
pub fn parse_reading_response(
    raw: &str,
    items: &[ReadingItem<'_>],
) -> Result<(Vec<ReadingAnswer>, usize), String> {
    let v: serde_json::Value =
        serde_json::from_str(extract_json(raw)).map_err(|e| e.to_string())?;
    let rows = v
        .get("r")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "no \"r\" array".to_string())?;
    let mut out = Vec::new();
    let mut malformed = 0usize;
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let parsed = (|| {
            let arr = row.as_array()?;
            let id = arr.first()?.as_i64()?;
            let item = items.iter().find(|i| i.id == id)?;
            if seen.contains(&id) {
                return None;
            }
            let answer = arr.get(1).and_then(|x| match x {
                serde_json::Value::String(s) => Some(s.trim().to_string()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            });
            match answer {
                None => Some(ReadingAnswer {
                    id,
                    name: None,
                    value: None,
                }),
                Some(a) if a.is_empty() => None,
                Some(a) if item.reading == "year_of_phrase" => {
                    if a.len() == 4 && a.chars().all(|c| c.is_ascii_digit()) {
                        Some(ReadingAnswer {
                            id,
                            name: None,
                            value: Some(a),
                        })
                    } else {
                        None
                    }
                }
                Some(a) => Some(ReadingAnswer {
                    id,
                    name: Some(a),
                    value: None,
                }),
            }
        })();
        match parsed {
            Some(answer) => {
                seen.insert(answer.id);
                out.push(answer);
            }
            None => malformed += 1,
        }
    }
    Ok((out, malformed))
}

/// The reply may be wrapped in ```json fences or surrounded by chatter: take the first { through the last }
/// The model's keys: a candidate key (case-insensitive), or a label that matches uniquely. When the key is a code like `p569` the model
/// often answers with the label (same reasoning as `phrase_align::candidate_key`)
fn resolve_key<'a>(item: &RuleItem<'a>, written: &str) -> Option<&'a str> {
    let written = written.trim();
    if let Some(c) = item.candidates.iter().find(|c| c.key.trim() == written) {
        return Some(c.key);
    }
    let lower = written.to_lowercase();
    let mut by_key = item
        .candidates
        .iter()
        .filter(|c| c.key.trim().to_lowercase() == lower);
    if let Some(c) = by_key.next() {
        return by_key.next().is_none().then_some(c.key);
    }
    let folded = crate::phrase_align::fold(written);
    let mut by_label = item
        .candidates
        .iter()
        .filter(|c| crate::phrase_align::fold(c.label) == folded);
    let c = by_label.next()?;
    by_label.next().is_none().then_some(c.key)
}

fn extract_json(raw: &str) -> &str {
    match (raw.find('{'), raw.rfind('}')) {
        (Some(a), Some(b)) if b > a => &raw[a..=b],
        _ => raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const READINGS: &[(&str, &str)] = &[("country_of_nationality", "…"), ("year_of_phrase", "…")];

    fn cand(key: &'static str) -> PropertyCandidate<'static> {
        PropertyCandidate {
            key,
            label: key,
            description: "",
            kind: "relation",
            temporal: "state",
            domains: vec![],
            ranges: vec![],
            via: vec![],
        }
    }
    fn item(id: i64, trigger: &'static str, bound: Option<&'static str>) -> RuleItem<'static> {
        RuleItem {
            id,
            trigger,
            phrase: "british film",
            subject_class: Some("film"),
            object_class: None,
            object_is_value: false,
            bound_to: bound,
            examples: &[],
            candidates: vec![cand("country_of_origin"), cand("genre")],
        }
    }

    #[test]
    fn a_rule_answer_names_a_candidate_and_a_known_reading() {
        let items = vec![item(0, "kind_word", None), item(1, "phrase", Some("genre"))];
        let (choices, bad) = parse_rule_response(
            "```json\n{\"i\":[[0,\"country_of_origin\",\"country_of_nationality\"],[1,null,null]]}\n```",
            &items,
            READINGS,
        )
        .unwrap();
        assert_eq!(bad, 0);
        assert_eq!(
            choices[0].implies,
            Some((
                "country_of_origin".into(),
                Some("country_of_nationality".into())
            ))
        );
        assert_eq!(choices[1].implies, None);
    }

    #[test]
    fn bad_rule_answers_are_counted_not_believed() {
        let items = vec![item(0, "kind_word", None), item(1, "phrase", Some("genre"))];
        let (choices, bad) = parse_rule_response(
            // Unknown attribute; kind word with no reading; already-bound attribute; unknown reading; duplicate id
            "{\"i\":[[0,\"director\",null],[0,\"country_of_origin\",null],[1,\"genre\",null],[1,\"country_of_origin\",\"made_up\"],[1,null,null],[1,null,null]]}",
            &items,
            READINGS,
        )
        .unwrap();
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].id, 1);
        assert_eq!(bad, 5);
    }

    #[test]
    fn readings_land_as_names_or_four_digit_years() {
        let items = vec![
            ReadingItem {
                id: 0,
                reading: "country_of_nationality",
                phrase: "british",
            },
            ReadingItem {
                id: 1,
                reading: "year_of_phrase",
                phrase: "the summer of 1952",
            },
            ReadingItem {
                id: 2,
                reading: "year_of_phrase",
                phrase: "last year",
            },
            ReadingItem {
                id: 3,
                reading: "country_of_nationality",
                phrase: "iberian",
            },
        ];
        let (answers, bad) = parse_reading_response(
            "{\"r\":[[0,\"United Kingdom\"],[1,1952],[2,\"recently\"],[3,null]]}",
            &items,
        )
        .unwrap();
        assert_eq!(bad, 1, "a non-year for year_of_phrase is malformed");
        assert_eq!(answers[0].name.as_deref(), Some("United Kingdom"));
        assert_eq!(answers[1].value.as_deref(), Some("1952"));
        assert_eq!(
            (answers[2].name.as_deref(), answers[2].value.as_deref()),
            (None, None)
        );
    }

    #[test]
    fn the_prompts_carry_the_readings_and_the_shape() {
        let items = vec![item(0, "phrase", Some("genre"))];
        let m = build_rule_messages(&items, READINGS);
        assert!(m[0].content.contains("- country_of_nationality"));
        assert!(m[1]
            .content
            .contains("phrase \"british film\" · subject class: film · object: ?"));
        assert!(m[1].content.contains("already bound to: genre"));
        let r = build_reading_messages(
            &[ReadingItem {
                id: 7,
                reading: "year_of_phrase",
                phrase: "in 1952",
            }],
            READINGS,
        );
        assert!(r[1]
            .content
            .contains("Item 7: year_of_phrase · \"in 1952\""));
    }
}
