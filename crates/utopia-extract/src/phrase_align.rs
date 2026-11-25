//! Binds relation phrases to properties by signature (second slice of 0044 decision 3). Signature = phrase × subject's class × object's class
//! (a literal object is recorded as "value"). A library has far fewer distinct signatures than statements; each is judged once, and bindings are
//! cached per library and reused per signature.
//!
//! Alignment reads the source (decision 3): each signature goes into the prompt with the few statements under it and their quotes, and the model judges
//! whether **every** statement under this signature states this property. It must also say the direction: forward means the statement's subject
//! is the property's subject; reverse is the opposite ("owns" bound to subsidiary_of). A property may be broader than the phrase
//! ("opened a plant in" is located_in), but not narrower, and not merely related ("announced the
//! acquisition of" is not acquired); phrases that only report or evaluate (said, is expected to) answer null;
//! a value that is not the property's kind of quantity also answers null (share count is not revenue). Candidate properties' definitions, domains and ranges are given as written in the library,
//! and answer keys are copied verbatim.
//!
//! The reply is compact JSON: `{"b": [[id, "key" | null, "forward" | "reverse" | null, marks]]}`. The fourth cell
//! only means something for state properties (#966): a statement that states a single moment marks whether that moment is the start of the state (start),
//! its end (end), or neither (none). Events, eternal facts and non-bindings answer null, and the old three-cell form is still read (marks
//! absent). Parsing is like the kind-word side: a bad item is counted and does not ruin the batch; a key not among the candidates, an id not in the batch, a binding without
//! a direction, marks written under a state property but not recognized, or a second answer for the same id all count as bad. Events and eternal facts do not read the fourth cell,
//! and anything written there is not bad. An unanswered id means "ask again", not null.
//!
//! **The shape is lenient too** (a lesson from the kind-word side): the model (tested with DeepSeek-V3.2) does not always follow the example. It may
//! answer the whole thing as an object keyed by id (`{"0": ["headquartered_in", "forward"], "1": null}`),
//! write an item as `{"id": 0, "key": ..., "direction": ...}`, write `b` as an object, and
//! drop the outer object and give only an array, or repeat the id first in the value. These all mean the same thing, so there is one reading only; at temperature zero
//! the same prompt returns the same shape, so an unreadable reply is unreadable every round: such a reply used to parse as "zero items, zero bad", and the caller
//! silently skipped it as "one vote unanswered", with no log and no retry. Unreadable keys and
//! ids count as bad items, not as absence.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use utopia_llm::ChatMessage;

use crate::align::{item_id, parse_value};

/// A candidate property: the key is copied into the answer; the rest is what the model judges by, given in the library's language
#[derive(Debug, Clone)]
pub struct PropertyCandidate<'a> {
    pub key: &'a str,
    pub label: &'a str,
    pub description: &'a str,
    /// relation (between two things) or attribute (the object is a value)
    pub kind: &'a str,
    /// state / event / eternal (0031): only states have the question of "which end this moment marks" (#966)
    pub temporal: &'a str,
    /// Class keys of domains and ranges; empty means none declared
    pub domains: Vec<&'a str>,
    pub ranges: Vec<&'a str>,
    /// This candidate matched through inheritance: declared on an ancestor, and the signature's class is its subclass. The basis is shown to the model,
    /// otherwise for a property with domain legal_entity and an organization subject it would answer null
    /// (#807: widening candidates in code alone is not enough; the model must see the basis of inheritance)
    pub via: Vec<String>,
}

/// A signature to bind: phrase, the classes at both ends, example sentences and quotes, candidate properties
#[derive(Debug, Clone)]
pub struct PhraseItem<'a> {
    pub id: i64,
    /// the normalised phrase ("acquired")
    pub phrase: &'a str,
    /// the subject's class key; None when its kind word is bound to no class yet
    pub subject_class: Option<&'a str>,
    /// the object's class key; None when unbound, or when the object is a value
    pub object_class: Option<&'a str>,
    pub object_is_value: bool,
    pub statement_count: i64,
    /// rendered statements ("Brightway Builders —acquired→ Harbor Estates") with their quotes
    pub examples: &'a [String],
    pub quotes: &'a [String],
    pub candidates: Vec<PropertyCandidate<'a>>,
    /// A key that also matches structurally but was not in the shortlist: if the model saw it in the batch's property table and chose it, it still counts as a vote,
    /// since the shortlist is a way to save tokens, not a limit
    pub also_allowed: Vec<&'a str>,
}

/// The model's verdict on one signature: Some = (candidate key, direction); None = do not bind
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhraseChoice {
    pub id: i64,
    pub property: Option<(String, Direction)>,
    /// When bound to a state property, a statement that states only one moment marks which end that moment is (#966); no answer or not applicable is None
    pub marks: Option<Marks>,
}

/// Which end a moment marks on a state (`phrase_bindings.marks`, #966)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marks {
    /// joined, was appointed, took over: the state starts from this moment
    Start,
    /// left, resigned from: the state ends at this moment
    End,
    /// only true at that time: not a typed row
    Neither,
}

impl Marks {
    pub fn as_str(self) -> &'static str {
        match self {
            Marks::Start => "start",
            Marks::End => "end",
            Marks::Neither => "none",
        }
    }

    fn parse(written: &str) -> Option<Self> {
        match written.trim().to_lowercase().as_str() {
            "start" => Some(Marks::Start),
            "end" => Some(Marks::End),
            "none" | "neither" => Some(Marks::Neither),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Reverse,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Forward => "forward",
            Direction::Reverse => "reverse",
        }
    }
}

/// System message. The examples are neutral and come from no measured corpus; the rule 1 "every" is the criterion for the whole path.
/// Reply slots use placeholders: a local model copied the old example's 12/13/14
/// into a batch numbered 0/1/2, leaving every signature unanswered (#1321).
const PHRASE_SYSTEM: &str = "\
You bind the relation phrases documents use to the properties of a knowledge base's ontology. \
Each numbered item is one signature: a phrase as the documents wrote it, the class of the thing \
it is said of (its subject) and the class of what it points at (its object), or \"value\" when \
the object is a figure, a title or a status; a few statements with that signature, each with the \
sentence it was taken from; and the keys of its candidate properties. The properties themselves \
are listed once under \"Properties\", each with its key, its label, its kind (a relation between two \
things, or an attribute whose object is a value), whether it is a state (holds over a span), an event \
(happens at a moment) or eternal, its domain, its range and its definition. A class \
written as \"?\" means the documents' kind word for that side is bound to no class yet. A candidate \
key marked \"fits by inheritance\" declares its domain or range on an ancestor of the item's class; \
that is a fit, not a mismatch.\n\
For each item, answer with the key of the one property that every statement of this signature \
states by that property's definition, and the direction: \"forward\" when the statement's \
subject is the property's subject, \"reverse\" when the statement's object is; or null. For a \
state, also say which end of it a statement dated at a single moment marks (rule 6).\n\
\n\
Output exactly one JSON object and nothing else, one entry per item. Format (uppercase words \
and the ellipsis are placeholders, not literal output):\n\
{\"b\": [[ITEM_ID, PROPERTY_KEY_OR_NULL, DIRECTION_OR_NULL, MARKS_OR_NULL], ...]}\n\
Copy the exact integer after \"Item\" in the user message into that item's ITEM_ID slot. \
Never invent an id or renumber the items. Answer every supplied id exactly once. \
The other slots contain the chosen property's key, direction and marks, as described below; \
when no property fits, all three are null.\n\
\n\
1. Choose a property only when each of the statements under this signature states that \
property, by the definition as given. The property may be broader than the phrase: \"opened a \
plant in\" states located_in; \"is the chief executive of\" states officer_of. It is never \
narrower and never merely related: \"announced the acquisition of\" is not acquired when the \
sentence says it was announced, not completed; \"revenue grew 18%\" is a change from the prior \
period, not revenue.\n\
2. Judge by the definition and by the sentences, not by the label. Labels, definitions and \
phrases may be in any language.\n\
3. Answer null when no candidate fits; when the sentences show the phrase meaning different \
things under this signature; when the phrase only reports, introduces or evaluates (\"said\", \
\"announced\", \"is expected to\") unless a candidate is about that; or when a value is not \
what the attribute measures (a share count is not revenue, a date is not an amount). A text \
attribute may be a candidate for an item whose object is a named thing: choose it when the \
object is named only to carry that attribute's value for the subject (a legal representative's \
name, a registered address), and the object's name becomes the value.\n\
4. The direction follows the definition: for \"X —is a subsidiary of→ Y\" subsidiary_of is \
forward; for \"X —owns→ Y\", if subsidiary_of is the only fitting candidate, it is reverse.\n\
5. Never invent a key, never answer with a label, never choose for an item a key that is not \
among its candidates. One entry per item, every item answered.\n\
6. When the property you choose is a state, the fourth value says what a statement of this \
signature dated at a single moment means for that state: \"start\" when the phrase says the state \
began then (\"joined\", \"was appointed\", \"took over\"), \"end\" when it says the state ended \
then (\"left\", \"resigned from\", \"stepped down as\"), \"none\" when the date only says the \
state held at that moment (\"was chairman in 2019\"). For an event or eternal property, or no \
property, the fourth value is null.";

pub fn candidate_line(c: &PropertyCandidate<'_>) -> String {
    let mut line = format!("- {} · {} · {} · {}", c.key, c.label, c.kind, c.temporal);
    if !c.domains.is_empty() {
        line.push_str(&format!(" · domain: {}", c.domains.join(", ")));
    }
    if !c.ranges.is_empty() {
        line.push_str(&format!(" · range: {}", c.ranges.join(", ")));
    }
    if !c.via.is_empty() {
        line.push_str(&format!(" · fits by inheritance: {}", c.via.join("; ")));
    }
    line.push_str(&format!(" · {}", c.description));
    line
}

/// Build two messages: a constant system message + a per-item user message. Each item: id, phrase, classes at both ends, example sentences and quotes, candidates.
/// The candidate property table is written once per batch (Properties), and each item lists only its candidate keys; earlier each item carried the whole table
/// (up to 60 entries, each with a definition); 12 items per batch was over 30,000 tokens, and alignment took 80% of a run's usage
/// (bench README, 2026-09-24). "Matched by inheritance" is a matter between an item and a candidate, written after the item's key
pub fn build_phrase_messages(items: &[PhraseItem<'_>]) -> Vec<ChatMessage> {
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
        let object = if item.object_is_value {
            "value".to_string()
        } else {
            item.object_class.unwrap_or("?").to_string()
        };
        let mut examples = String::new();
        for (i, ex) in item.examples.iter().enumerate() {
            let quote = item.quotes.get(i).map(String::as_str).unwrap_or("");
            examples.push_str(&format!("\n  · {ex}\n    \"{}\"", quote.trim()));
        }
        if examples.is_empty() {
            examples.push_str(" (none)");
        }
        let candidates = if item.candidates.is_empty() {
            " (none)".to_string()
        } else {
            let keys: Vec<String> = item
                .candidates
                .iter()
                .map(|c| {
                    if c.via.is_empty() {
                        c.key.to_string()
                    } else {
                        format!("{} (fits by inheritance: {})", c.key, c.via.join("; "))
                    }
                })
                .collect();
            format!(" {}", keys.join(", "))
        };
        user.push_str(&format!(
            "Item {}: phrase \"{}\" · subject class: {} · object: {} · {} statements\nStatements:{examples}\nCandidates:{candidates}\n\n",
            item.id,
            item.phrase,
            item.subject_class.unwrap_or("?"),
            object,
            item.statement_count,
        ));
    }
    vec![
        ChatMessage {
            role: "system".into(),
            content: PHRASE_SYSTEM.to_string(),
        },
        ChatMessage {
            role: "user".into(),
            content: user.trim_end().to_string(),
        },
    ]
}

/// Parse the reply: `(verdicts, bad item count)`. Only the first answer per id is kept; an id not answered does not appear.
pub fn parse_phrase_response(
    raw: &str,
    items: &[PhraseItem<'_>],
) -> anyhow::Result<(Vec<PhraseChoice>, usize)> {
    let value = parse_value(raw)?;
    let by_id: HashMap<i64, &PhraseItem<'_>> = items.iter().map(|i| (i.id, i)).collect();
    // What time semantics each property described in the batch has: only states read the fourth cell
    let temporal: HashMap<&str, &str> = items
        .iter()
        .flat_map(|i| i.candidates.iter().map(|c| (c.key.trim(), c.temporal)))
        .collect();
    let mut choices = Vec::new();
    let mut malformed = 0usize;
    let mut seen = HashSet::new();
    for (id, key, direction, marks) in answers(&value) {
        match parse_answer(&id, &key, &direction, &marks, &by_id, &temporal) {
            Some(choice) if seen.insert(choice.id) => choices.push(choice),
            _ => malformed += 1,
        }
    }
    Ok((choices, malformed))
}

/// Each answer in the reply: four raw values (id, key, direction, marks), shape not yet validated. marks is in the fourth cell or the
/// `marks` field; if absent, it is null (the old three-cell form is still read).
///
/// Four forms are accepted, all meaning "this id chose this key and this direction": `{"b": [[id, key, dir]]}`
/// (the example), `{"b": {"id": [key, dir]}}`, a top-level object without `b` `{"id": [key, dir]}`,
/// a top-level array `[[id, key, dir]]`; an entry may also be written `{"id": .., "key": .., "direction": ..}`,
/// and with id as key the value may also be `{"key": .., "direction": ..}`, `null` (not bound), or `[id, key, dir]` repeating the id first.
/// If `b` is present only `b` is read; other top-level keys are the model's commentary, not answers. If `b` is missing and
/// the value is neither an object nor an array, there are no answers
fn answers(value: &Value) -> Vec<(Value, Value, Value, Value)> {
    let listed = value.get("b").unwrap_or(value);
    match listed {
        Value::Array(entries) => entries
            .iter()
            .map(|entry| match entry {
                Value::Array(list) if list.len() >= 2 => {
                    let (key, direction, marks) = key_direction_marks(&list[1..]);
                    (list[0].clone(), key, direction, marks)
                }
                Value::Object(map) => {
                    let (key, direction, marks) = fields(map);
                    (
                        map.get("id").cloned().unwrap_or(Value::Null),
                        key,
                        direction,
                        marks,
                    )
                }
                other => (other.clone(), Value::Null, Value::Null, Value::Null),
            })
            .collect(),
        Value::Object(map) => map
            .iter()
            .map(|(id, v)| {
                let (key, direction, marks) = match v {
                    Value::Array(list) => {
                        // The value repeats the id before the key and direction: `{"0": [0, "headquartered_in", "forward"]}`
                        let copied = list
                            .first()
                            .and_then(item_id)
                            .is_some_and(|first| id.trim().parse::<i64>().ok() == Some(first));
                        let list = if copied { &list[1..] } else { &list[..] };
                        key_direction_marks(list)
                    }
                    Value::Object(inner) => fields(inner),
                    other => (other.clone(), Value::Null, Value::Null),
                };
                (Value::String(id.clone()), key, direction, marks)
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Three cells `[key, dir, marks]`; with only one cell there is no direction (a null answer often gives only one cell); without a third cell
/// there are no marks. An empty array means "nothing written", and reading it as a null key would call an unanswered item "not bound": keep it as an object,
/// the caller counts it as bad
fn key_direction_marks(list: &[Value]) -> (Value, Value, Value) {
    match list {
        [] => (Value::Array(Vec::new()), Value::Null, Value::Null),
        [key] => (key.clone(), Value::Null, Value::Null),
        [key, direction] => (key.clone(), direction.clone(), Value::Null),
        [key, direction, marks, ..] => (key.clone(), direction.clone(), marks.clone()),
    }
}

/// The three fields of an entry written as an object: the key is key / property / p, the direction is direction / dir / d,
/// and which end a moment marks is marks / m
fn fields(map: &serde_json::Map<String, Value>) -> (Value, Value, Value) {
    let pick = |names: &[&str]| {
        names
            .iter()
            .find_map(|n| map.get(*n))
            .cloned()
            .unwrap_or(Value::Null)
    };
    (
        pick(&["key", "property", "p"]),
        pick(&["direction", "dir", "d"]),
        pick(&["marks", "m"]),
    )
}

/// An answer: id must be in this batch; key is null (not bound) or one of the candidates; if bound, a direction is needed, and an unrecognized direction
/// counts as bad. A key wrapped in an array (`["headquartered_in", "forward"]` placed in the second cell) is still read,
/// with direction and marks taken from the array. Marks are read only when bound to a state property: null or absent means not stated; written but unrecognized
/// counts as bad; events and eternal facts (or keys not described in the batch) do not read this cell, and whatever is written is ignored
fn parse_answer(
    id: &Value,
    key: &Value,
    direction: &Value,
    marks: &Value,
    by_id: &HashMap<i64, &PhraseItem<'_>>,
    temporal: &HashMap<&str, &str>,
) -> Option<PhraseChoice> {
    let id = item_id(id)?;
    let item = by_id.get(&id)?;
    let (key, direction, marks) = match key {
        Value::Array(list) if !list.is_empty() => {
            let (k, d, m) = key_direction_marks(list);
            let d = if d.is_null() { direction.clone() } else { d };
            let m = if m.is_null() { marks.clone() } else { m };
            (k, d, m)
        }
        other => (other.clone(), direction.clone(), marks.clone()),
    };
    let property = match &key {
        Value::Null => None,
        Value::String(written) => {
            let key = candidate_key(item, written)?;
            let direction = match direction
                .as_str()
                .map(|d| d.trim().to_lowercase())
                .as_deref()
            {
                Some("forward") => Direction::Forward,
                Some("reverse") => Direction::Reverse,
                _ => return None,
            };
            Some((key, direction))
        }
        _ => return None,
    };
    let state = property
        .as_ref()
        .is_some_and(|(key, _)| temporal.get(key.as_str()) == Some(&"state"));
    let marks = match (state, &marks) {
        (false, _) | (_, Value::Null) => None,
        (true, Value::String(written)) => Some(Marks::parse(written)?),
        (true, _) => return None,
    };
    Some(PhraseChoice {
        id,
        property,
        marks,
    })
}

/// Match the model's key to this item's candidates: as written first, then case-insensitively (only on a unique hit)
fn candidate_key(item: &PhraseItem<'_>, written: &str) -> Option<String> {
    let written = written.trim();
    if written.is_empty() {
        return None;
    }
    let keys = || {
        item.candidates
            .iter()
            .map(|c| c.key.trim())
            .chain(item.also_allowed.iter().map(|k| k.trim()))
    };
    if let Some(exact) = keys().find(|k| *k == written) {
        return Some(exact.to_string());
    }
    let lower = written.to_lowercase();
    let mut hits = keys().filter(|k| k.to_lowercase() == lower);
    if let Some(first) = hits.next() {
        return hits.next().is_none().then(|| first.to_string());
    }
    // When the key is a code like `p569`, the model very likely answers with the label ("dateOfBirth"): in the first real run half of the signatures
    // were therefore judged bad votes (bench README, 2026-09-24). If the label matches uniquely, accept its key; if it matches two, do not accept
    let folded = fold(written);
    let mut by_label = item
        .candidates
        .iter()
        .filter(|c| fold(c.label) == folded)
        .map(|c| c.key.trim());
    let first = by_label.next()?;
    by_label.next().is_none().then(|| first.to_string())
}

/// Case-folding for label comparison: case, spaces, underscores, and hyphens are all ignored
pub fn fold(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    fn candidates<'a>() -> Vec<PropertyCandidate<'a>> {
        vec![
            PropertyCandidate {
                key: "headquartered_in",
                label: "headquartered in",
                description: "The organization's principal office is at the place.",
                kind: "relation",
                temporal: "state",
                domains: vec!["organization"],
                ranges: vec!["place"],
                via: Vec::new(),
            },
            PropertyCandidate {
                key: "subsidiary_of",
                label: "subsidiary of",
                description: "The organization is owned or controlled by the other organization.",
                kind: "relation",
                temporal: "state",
                domains: vec!["organization"],
                ranges: vec!["organization"],
                via: Vec::new(),
            },
            PropertyCandidate {
                key: "revenue",
                label: "revenue",
                description: "Total income from sales for a period, as an amount of money.",
                kind: "attribute",
                temporal: "state",
                domains: vec!["organization"],
                ranges: vec![],
                via: Vec::new(),
            },
        ]
    }

    #[test]
    fn the_prompt_lists_the_signature_its_sentences_and_the_candidates() {
        let examples = strings(&["Harbor Bakery —is based in→ Port Ellen"]);
        let quotes = strings(&["Harbor Bakery is based in Port Ellen."]);
        let items = vec![PhraseItem {
            id: 3,
            phrase: "is based in",
            subject_class: Some("organization"),
            object_class: None,
            object_is_value: false,
            statement_count: 4,
            examples: &examples,
            quotes: &quotes,
            candidates: candidates(),
            also_allowed: vec![],
        }];
        let msgs = build_phrase_messages(&items);
        let user = &msgs[1].content;
        assert!(user.contains("Item 3: phrase \"is based in\" · subject class: organization · object: ? · 4 statements"), "{user}");
        assert!(user.contains("· Harbor Bakery —is based in→ Port Ellen\n    \"Harbor Bakery is based in Port Ellen.\""), "{user}");
        assert!(user.contains("- headquartered_in · headquartered in · relation · state · domain: organization · range: place · The organization's"), "{user}");
        assert!(
            user.starts_with("Properties:\n- "),
            "the glossary comes first, once: {user}"
        );
        assert!(
            user.contains("Candidates: headquartered_in"),
            "items list keys only: {user}"
        );
        assert_eq!(
            user.matches("- headquartered_in ·").count(),
            1,
            "each property is described once: {user}"
        );
        assert!(
            user.contains(
                "- revenue · revenue · attribute · state · domain: organization · Total income"
            ),
            "{user}"
        );
        assert!(msgs[0]
            .content
            .contains("every statement of this signature"));
    }

    #[test]
    fn a_key_the_shortlist_hid_still_counts_when_it_fits_structurally() {
        let examples = strings(&[]);
        let quotes = strings(&[]);
        let items = vec![PhraseItem {
            id: 0,
            phrase: "is based in",
            subject_class: Some("organization"),
            object_class: None,
            object_is_value: false,
            statement_count: 1,
            examples: &examples,
            quotes: &quotes,
            candidates: candidates(),
            also_allowed: vec!["located_in"],
        }];
        let (choices, malformed) =
            parse_phrase_response(r#"{"b":[[0,"located_in","forward"]]}"#, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices[0].property.as_ref().map(|(k, _)| k.as_str()),
            Some("located_in")
        );
        let (_, malformed) =
            parse_phrase_response(r#"{"b":[[0,"made_up","forward"]]}"#, &items).unwrap();
        assert_eq!(malformed, 1, "a key that fits nowhere is still malformed");
    }

    #[test]
    fn a_label_answer_maps_to_its_key_when_unique() {
        let examples = strings(&[]);
        let quotes = strings(&[]);
        let items = vec![PhraseItem {
            id: 0,
            phrase: "is based in",
            subject_class: Some("organization"),
            object_class: None,
            object_is_value: false,
            statement_count: 1,
            examples: &examples,
            quotes: &quotes,
            candidates: candidates(),
            also_allowed: vec![],
        }];
        let (choices, malformed) =
            parse_phrase_response(r#"{"b":[[0,"Headquartered In","forward"]]}"#, &items).unwrap();
        assert_eq!(malformed, 0, "a label, folded, names the key");
        assert_eq!(
            choices[0].property.as_ref().map(|(k, _)| k.as_str()),
            Some("headquartered_in")
        );
        assert_eq!(fold("Date of Birth"), "dateofbirth");
    }

    #[test]
    fn a_value_signature_says_value_for_its_object() {
        let examples = strings(&["Harbor Bakery —revenue→ $2 million"]);
        let quotes = strings(&["Harbor Bakery's revenue was $2 million."]);
        let items = vec![PhraseItem {
            id: 0,
            phrase: "revenue",
            subject_class: Some("organization"),
            object_class: None,
            object_is_value: true,
            statement_count: 1,
            examples: &examples,
            quotes: &quotes,
            candidates: candidates(),
            also_allowed: vec![],
        }];
        let user = &build_phrase_messages(&items)[1].content;
        assert!(user.contains("· object: value ·"), "{user}");
    }

    /// Bind with a direction; null does not bind; the key is copied back from the candidates; bound without a direction, a key not among the candidates, or an id not in the batch all count as bad
    #[test]
    fn triples_parse_and_bad_ones_are_counted() {
        let examples = strings(&[]);
        let quotes = strings(&[]);
        let mk = |id: i64, phrase: &'static str| PhraseItem {
            id,
            phrase,
            subject_class: Some("organization"),
            object_class: Some("organization"),
            object_is_value: false,
            statement_count: 1,
            examples: &examples,
            quotes: &quotes,
            candidates: candidates(),
            also_allowed: vec![],
        };
        let items = vec![
            mk(0, "owns"),
            mk(1, "said"),
            mk(2, "acquired"),
            mk(3, "employs"),
            mk(4, "x"),
        ];
        let raw = r#"{"b": [[0, "Subsidiary_Of", "reverse"], [1, null, null], [2, "acquired", "forward"], [3, "subsidiary_of"], ["4", "revenue", "forward"], [9, null, null], [0, null, null]]}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(
            choices,
            vec![
                PhraseChoice {
                    id: 0,
                    property: Some(("subsidiary_of".into(), Direction::Reverse)),
                    marks: None,
                },
                PhraseChoice {
                    id: 1,
                    property: None,
                    marks: None,
                },
                PhraseChoice {
                    id: 4,
                    property: Some(("revenue".into(), Direction::Forward)),
                    marks: None,
                },
            ]
        );
        // 2: key not among candidates; 3: bound without direction; 9: id not in batch; second answer for 0
        assert_eq!(malformed, 4);
    }

    #[test]
    fn a_truncated_reply_keeps_the_complete_triples() {
        let examples = strings(&[]);
        let quotes = strings(&[]);
        let items = vec![
            PhraseItem {
                id: 0,
                phrase: "a",
                subject_class: None,
                object_class: None,
                object_is_value: false,
                statement_count: 1,
                examples: &examples,
                quotes: &quotes,
                candidates: candidates(),
                also_allowed: vec![],
            },
            PhraseItem {
                id: 1,
                phrase: "b",
                subject_class: None,
                object_class: None,
                object_is_value: false,
                statement_count: 1,
                examples: &examples,
                quotes: &quotes,
                candidates: candidates(),
                also_allowed: vec![],
            },
        ];
        let raw = r#"{"b": [[0, "revenue", "forward"], [1, "subsid"#;
        let (choices, _) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].id, 0);
    }

    /// Three signatures: owns (should bind reversed to subsidiary_of), said (should be null), revenue (a value)
    fn three_items<'a>(examples: &'a [String], quotes: &'a [String]) -> Vec<PhraseItem<'a>> {
        let mk = |id: i64, phrase: &'static str, value: bool| PhraseItem {
            id,
            phrase,
            subject_class: Some("organization"),
            object_class: (!value).then_some("organization"),
            object_is_value: value,
            statement_count: 1,
            examples,
            quotes,
            candidates: candidates(),
            also_allowed: vec![],
        };
        vec![
            mk(0, "owns", false),
            mk(1, "said", false),
            mk(2, "revenue", true),
        ]
    }

    fn bound(id: i64, key: &str, direction: Direction) -> PhraseChoice {
        PhraseChoice {
            id,
            property: Some((key.to_string(), direction)),
            marks: None,
        }
    }

    fn unbound(id: i64) -> PhraseChoice {
        PhraseChoice {
            id,
            property: None,
            marks: None,
        }
    }

    fn marked(id: i64, key: &str, direction: Direction, marks: Marks) -> PhraseChoice {
        PhraseChoice {
            marks: Some(marks),
            ..bound(id, key, direction)
        }
    }

    /// The fourth cell says which end a moment marks on a state (#966): the four-cell array, the object's marks field, and the keyed-by-id form are all read;
    /// absent or null means not stated; not-bound ones carry none; written but unrecognized ones count as bad
    #[test]
    fn the_fourth_value_says_which_end_a_moment_marks() {
        let (examples, quotes) = (strings(&[]), strings(&[]));
        let items = three_items(&examples, &quotes);
        let raw = r#"{"b": [[0, "subsidiary_of", "reverse", "start"], [1, null, null, "end"], [2, "revenue", "forward", null]]}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![
                marked(0, "subsidiary_of", Direction::Reverse, Marks::Start),
                unbound(1),
                bound(2, "revenue", Direction::Forward),
            ]
        );
        let raw = r#"{"0": ["subsidiary_of", "reverse", "END"], "2": {"key": "revenue", "direction": "forward", "marks": "none"}}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![
                marked(0, "subsidiary_of", Direction::Reverse, Marks::End),
                marked(2, "revenue", Direction::Forward, Marks::Neither),
            ]
        );
        let raw = r#"{"b": [{"id": 0, "p": "subsidiary_of", "d": "reverse", "m": "start"}, [2, "revenue", "forward", "later"]]}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(
            choices,
            vec![marked(0, "subsidiary_of", Direction::Reverse, Marks::Start)]
        );
        assert_eq!(malformed, 1, "an unreadable marks is a bad answer");
    }

    /// Events and eternal facts do not read the fourth cell: an unrecognized value written there is not a bad item, and a recognized one is not kept. Under a state property
    /// an unrecognized one still counts as bad
    #[test]
    fn an_event_property_does_not_read_the_fourth_value() {
        let (examples, quotes) = (strings(&[]), strings(&[]));
        let mut items = three_items(&examples, &quotes);
        items[0].candidates.push(PropertyCandidate {
            key: "acquired",
            label: "acquired",
            description: "The organization bought the other organization.",
            kind: "relation",
            temporal: "event",
            domains: vec!["organization"],
            ranges: vec!["organization"],
            via: Vec::new(),
        });
        let raw =
            r#"{"b": [[0, "acquired", "forward", "later"], [2, "revenue", "forward", "later"]]}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(choices, vec![bound(0, "acquired", Direction::Forward)]);
        assert_eq!(malformed, 1, "revenue is a state: its fourth value is read");
        let raw = r#"{"b": [[0, "acquired", "forward", "start"]]}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(
            (choices, malformed),
            (vec![bound(0, "acquired", Direction::Forward)], 0)
        );
    }

    /// The prompt states whether each property is a state, and how to answer the fourth cell
    #[test]
    fn the_prompt_says_which_properties_are_states_and_asks_for_the_fourth_value() {
        let (examples, quotes) = (strings(&[]), strings(&[]));
        let msgs = build_phrase_messages(&three_items(&examples, &quotes));
        assert!(msgs[1]
            .content
            .contains("- subsidiary_of · subsidiary of · relation · state ·"));
        let system = &msgs[0].content;
        assert!(
            system.contains("\"start\" when the phrase says the state began then"),
            "{system}"
        );
        assert!(
            system.contains("\"end\" when it says the state ended then"),
            "{system}"
        );
        assert!(
            system.contains("For an event or eternal property"),
            "{system}"
        );
    }

    /// Form observed with the kind-word side, moved to phrases: the whole reply is an object keyed by id, with no `b`, and the value is
    /// `[key, dir]`. It must parse to exactly the same shape as the example
    #[test]
    fn an_id_keyed_object_with_pairs_is_read_as_the_triple_list() {
        let (examples, quotes) = (strings(&[]), strings(&[]));
        let items = three_items(&examples, &quotes);
        let raw = r#"{ "0": ["subsidiary_of", "reverse"], "1": [null, null], "2": ["revenue", "forward"] }"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![
                bound(0, "subsidiary_of", Direction::Reverse),
                unbound(1),
                bound(2, "revenue", Direction::Forward),
            ]
        );
        // Not bound is written as a bare null or with one cell; bound with only one cell means no direction, which counts as bad
        let raw = r#"{"0": ["subsidiary_of"], "1": null, "2": [null]}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(malformed, 1, "bound without a direction");
        assert_eq!(choices, vec![unbound(1), unbound(2)]);
        // Repeat the id in the value before the key and direction
        let raw = r#"{"0": [0, "subsidiary_of", "reverse"], "1": [1, null, null]}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![bound(0, "subsidiary_of", Direction::Reverse), unbound(1)]
        );
        // An empty array is not an answer
        let raw = r#"{"0": [], "1": [null]}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(malformed, 1);
        assert_eq!(choices, vec![unbound(1)]);
    }

    /// Keyed by id with an object value: `{"key": .., "direction": ..}`; several aliases of field names are accepted
    #[test]
    fn an_id_keyed_object_with_object_values_parses() {
        let (examples, quotes) = (strings(&[]), strings(&[]));
        let items = three_items(&examples, &quotes);
        let raw = r#"{"0": {"key": "subsidiary_of", "direction": "Reverse"}, "1": {"key": null, "direction": null}, "2": {"property": "revenue", "dir": "FORWARD"}}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![
                bound(0, "subsidiary_of", Direction::Reverse),
                unbound(1),
                bound(2, "revenue", Direction::Forward),
            ]
        );
    }

    /// An object under `b` rather than an array: read the same way; if `b` is present only `b` is read, other top-level keys are commentary
    #[test]
    fn b_as_an_object_parses_and_other_top_level_keys_are_ignored() {
        let (examples, quotes) = (strings(&[]), strings(&[]));
        let items = three_items(&examples, &quotes);
        let raw = r#"{"note": "done", "b": {"0": ["subsidiary_of", "reverse"], "1": null}}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![bound(0, "subsidiary_of", Direction::Reverse), unbound(1)]
        );
        let raw = r#"{"b": {"0": {"key": "subsidiary_of", "direction": "reverse"}}}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(choices, vec![bound(0, "subsidiary_of", Direction::Reverse)]);
    }

    /// An entry in an array written as an object, or the whole reply given as a bare array without an outer object: read as is
    #[test]
    fn object_triples_and_a_bare_array_parse() {
        let (examples, quotes) = (strings(&[]), strings(&[]));
        let items = three_items(&examples, &quotes);
        let raw = r#"{"b": [{"id": 0, "key": "subsidiary_of", "direction": "reverse"}, {"id": "1", "key": null, "direction": null}]}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![bound(0, "subsidiary_of", Direction::Reverse), unbound(1)]
        );
        let raw = concat!(
            "```json\n",
            r#"[[0, "subsidiary_of", "reverse"], [2, "revenue", "forward"]]"#,
            "\n```"
        );
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![
                bound(0, "subsidiary_of", Direction::Reverse),
                bound(2, "revenue", Direction::Forward)
            ]
        );
        // In the sample shape the second cell is wrapped one more level: `[id, [key, dir]]`
        let raw = r#"{"b": [[0, ["subsidiary_of", "reverse"]], [1, [null]]]}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![bound(0, "subsidiary_of", Direction::Reverse), unbound(1)]
        );
    }

    /// `b` is empty: no item answered, not a bad item. No `b` and keys that are not ids: that is an unreadable answer,
    /// counted as bad. Previously this kind of reply counted as "nothing answered" and the caller skipped it silently, leaving no trace
    #[test]
    fn an_empty_b_answers_nothing_but_an_unreadable_reply_is_malformed() {
        let (examples, quotes) = (strings(&[]), strings(&[]));
        let items = three_items(&examples, &quotes);
        for raw in [r#"{"b": []}"#, r#"{"b": {}}"#] {
            let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
            assert!(choices.is_empty(), "{raw}");
            assert_eq!(malformed, 0, "{raw}");
        }
        let raw = r#"{"answer": "subsidiary_of", "direction": "reverse"}"#;
        let (choices, malformed) = parse_phrase_response(raw, &items).unwrap();
        assert!(choices.is_empty());
        assert_eq!(malformed, 2);
        // `b` is a scalar: no entries at all
        let (choices, malformed) = parse_phrase_response(r#"{"b": 1}"#, &items).unwrap();
        assert!(choices.is_empty());
        assert_eq!(malformed, 0);
    }
}
