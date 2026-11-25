//! Kind words bind to classes (first step of 0044 decisions 3–4): the typed graph is derived from the open graph by signature binding,
//! and a kind word (what open extraction writes as "what it is") is the simplest signature: one word to one class, or
//! no match. Bindings are cached per library and reused per word, so the cost grows with distinct phrasings, not with documents.
//!
//! Alignment reads the source (decision 3): each word goes into the prompt with the spellings seen in documents, examples of the class (names), and its
//! relation phrases; the model judges whether **every** such thing is an instance of the class. A class may
//! be broader than the word (a bakery is an organization), but not narrower, and not merely related (a contract is not a "file", if the class definition
//! says a file is a `file`). Role words, descriptive words and words with two senses answer null. Candidate class definitions are given as written in the library
//! (Chinese or English both work), and answer keys are copied verbatim.
//!
//! The reply is compact JSON: `{"b": [[id, "key" | null]]}`. Parsing is lenient per item: a bad pair is counted, not allowed to ruin
//! the whole batch; a key not among that item's candidates, an id not in the batch, or a second answer for the same id all count as bad; a truncated reply falls back to
//! the last complete pair. An id that was not answered is simply unanswered: the caller reconciles by id, and absence means "ask again",
//! not null.
//!
//! **The shape is lenient too**: the model (tested with DeepSeek-V3.2) does not always follow the example. It may answer the whole thing as an object keyed by id
//! (`{"0": null, "1": "organization"}`), wrap the key in a single-element array
//! (`{"0": ["organization"], "4": [null]}`), write `b` as an object, or write a pair as
//! `{"id": 0, "key": ...}`, or drop the outer object and give just an array. These all mean the same thing, so there is one reading
//! only; at temperature zero the same prompt returns the same shape, so an unreadable shape is unreadable every round,
//! which is how one library went twelve documents without binding a single word. Unreadable keys and ids count as bad items, not as absence.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use utopia_llm::ChatMessage;

use crate::{json_block, json_text, open::repair_truncated_compact};

/// A candidate class: the key is copied into the answer; the label and definition are what the model judges by, given in the library's language
#[derive(Debug, Clone, Copy)]
pub struct ClassCandidate<'a> {
    pub key: &'a str,
    pub label: &'a str,
    pub description: &'a str,
}

/// A kind-word signature to bind: the word, how it looks in documents, examples and phrases of the class, and the candidate classes
#[derive(Debug, Clone)]
pub struct KindWordItem<'a> {
    pub id: i64,
    /// the normalised kind word ("company")
    pub kind_word: &'a str,
    /// spellings seen in documents ("Company", "company", "Companies")
    pub spellings: &'a [String],
    /// example entity names of this kind ("Brightway Builders", "Harbor Estates")
    pub examples: &'a [String],
    /// relation phrases these things are subjects of ("is based in", "was founded by")
    pub phrases: &'a [String],
    pub candidates: Vec<ClassCandidate<'a>>,
}

/// The model's verdict on one item: Some = that candidate's key (copied from the candidates, not written by the model); None = do not bind
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindWordChoice {
    pub id: i64,
    pub key: Option<String>,
}

/// System message. The examples are neutral (bakery, bridge, town council) and come from no measured corpus; the rule 1
/// "every such thing" criterion is the test for the whole path: broader is fine, narrower is not, merely related is not
const KIND_WORD_SYSTEM: &str = "\
You bind the words documents use for kinds of things to the classes of a knowledge base's \
ontology. Each numbered item is one kind word as the documents wrote it: the word, the \
spellings seen, some things of this kind by name, the phrases those things take part in, and \
the keys of its candidate classes. The classes themselves are listed once under \"Classes\", \
each with its key, its label and its definition. For each item, answer with the key of the one \
candidate class that every thing of this kind is an instance of, or null.\n\
\n\
Output exactly one JSON object and nothing else, one pair per item. Format (uppercase words \
and the ellipsis are placeholders, not literal output):\n\
{\"b\": [[ITEM_ID, CLASS_KEY_OR_NULL], ...]}\n\
Copy the exact integer after \"Item\" in the user message into that item's ITEM_ID slot. \
Never invent an id or renumber the items. Answer every supplied id exactly once. \
The second element is the chosen key, copied exactly as listed under that item, or null.\n\
\n\
1. Choose a candidate only when every thing of this kind is an instance of that class by the \
class's definition. The class may be broader than the word: a bakery is an organization, a \
bridge is a structure. It is never narrower and never merely related: a contract is not a \
document when the definition says a document is a file; a town council is not a town; a \
bakery is not a building because bakeries are in buildings.\n\
2. Judge by the definition as given, not by the label or the key alone. Labels and \
definitions may be written in any language, and so may the kind word; a word in another \
script or language is the same word when it names the same kind.\n\
3. Answer null when no candidate fits; when the word names a role, a part or a description \
rather than a kind of thing (\"participants\", \"results\", \"the northern wing\"); or when the \
examples and phrases show two kinds of thing under one word (a \"bank\" that is a lender in one \
document and a riverbank in another).\n\
4. The spellings, examples and phrases are the evidence of what the word means in these \
documents: things that \"were founded by\" and \"are based in\" are organizations; things that \
\"were signed on\" and \"expire\" are agreements. Read them before the label.\n\
5. Never invent a key, never answer with a label or a word of your own, and never choose for \
an item a key that is not among its candidates. One pair per item, every item answered.";

/// Build two messages: a constant system message plus a user message. The class definition table is written once per batch, in order of first appearance;
/// each item: id, word, spellings, examples, phrases, and candidate keys (in the order given, so a second vote with reversed order is listed reversed).
/// An empty list is written as `(none)`, so the model knows the field exists but has no evidence this time.
///
/// Earlier, each item listed the candidate definitions again: a batch of twenty words with ten candidates each wrote the same table twenty times,
/// and 120 of the 140-odd tokens per word were that table (bench README, 2026-09-29). Same
/// reasoning as `phrase_align::build_phrase_messages`
pub fn build_kind_word_messages(items: &[KindWordItem<'_>]) -> Vec<ChatMessage> {
    let mut glossary: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in items.iter().flat_map(|i| i.candidates.iter()) {
        if seen.insert(c.key.trim()) {
            glossary.push(candidate_line(c));
        }
    }
    let mut user = String::new();
    if !glossary.is_empty() {
        user.push_str(&format!("Classes:\n{}\n\n", glossary.join("\n")));
    }
    for item in items {
        let candidates = if item.candidates.is_empty() {
            " (none)".to_string()
        } else {
            let keys: Vec<&str> = item.candidates.iter().map(|c| c.key.trim()).collect();
            format!(" {}", keys.join(", "))
        };
        user.push_str(&format!(
            "Item {}: \"{}\"\nSpellings: {}\nExamples: {}\nPhrases: {}\nCandidates:{candidates}\n\n",
            item.id,
            item.kind_word,
            quoted_list(item.spellings),
            quoted_list(item.examples),
            quoted_list(item.phrases),
        ));
    }
    vec![
        ChatMessage {
            role: "system".into(),
            content: KIND_WORD_SYSTEM.to_string(),
        },
        ChatMessage {
            role: "user".into(),
            content: user.trim_end().to_string(),
        },
    ]
}

/// `"a", "b", "c"`; an empty list is `(none)`. Quoted because names may contain commas
fn quoted_list(values: &[String]) -> String {
    if values.is_empty() {
        return "(none)".to_string();
    }
    values
        .iter()
        .map(|v| format!("\"{}\"", v.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `- key: label — description`. Omit the label if it equals the key; omit the dash if the definition is empty
fn candidate_line(c: &ClassCandidate<'_>) -> String {
    let key = c.key.trim();
    let mut line = format!("- {key}");
    let label = c.label.trim();
    if !label.is_empty() && label != key {
        line.push_str(&format!(": {label}"));
    }
    let description = c.description.trim();
    if !description.is_empty() {
        line.push_str(&format!(" — {description}"));
    }
    line
}

/// Parse the reply leniently, item by item. Returns (verdicts, bad item count).
///
/// A pair is bad if it is not an array, has fewer than two cells, has a non-integer id, an id not in this batch, or a key that is neither null nor
/// a string, a key not among that item's candidates, or an id already answered (the first is kept). The key is first matched to the candidates as written,
/// then matched case-insensitively; when it matches, the candidate's own key is written back, not the one the model wrote.
/// A truncated reply first falls back to the last complete pair; it is an error only if there is no complete pair at all
pub fn parse_kind_word_response(
    raw: &str,
    items: &[KindWordItem<'_>],
) -> anyhow::Result<(Vec<KindWordChoice>, usize)> {
    let value = parse_value(raw)?;
    let by_id: HashMap<i64, &KindWordItem<'_>> = items.iter().map(|i| (i.id, i)).collect();
    let mut choices = Vec::new();
    let mut malformed = 0usize;
    let mut seen = HashSet::new();
    for (id, key) in answers(&value) {
        match parse_pair(&id, &key, &by_id) {
            Some(choice) if seen.insert(choice.id) => choices.push(choice),
            _ => malformed += 1,
        }
    }
    Ok((choices, malformed))
}

/// First take the block normally (from the first `{` to the last `}`); only if that fails to parse, take from the first `{` to the end and repair it.
/// The split between block extraction and repair matches `open.rs`: in a compact reply `}` appears only at the end; a truncated reply either has no `}`,
/// or its last `}` is not the end. A reply with no outer object that gives an array directly takes the first `[` through the last
/// `]`, but only when the text itself starts with `[`; do not read a pair inside an object as the whole reply
pub(crate) fn parse_value(raw: &str) -> anyhow::Result<Value> {
    let text = json_text(raw).trim();
    if text.starts_with('[') {
        if let Some(end) = text.rfind(']') {
            if let Ok(v) = serde_json::from_str::<Value>(&text[..=end]) {
                return Ok(v);
            }
        }
    }
    let block = json_block(raw)
        .and_then(|b| serde_json::from_str::<Value>(&b).map_err(anyhow::Error::from));
    match block {
        Ok(v) => Ok(v),
        Err(e) => {
            let fixed = text
                .find('{')
                .map(|s| &text[s..])
                .and_then(repair_truncated_compact)
                .ok_or_else(|| anyhow::anyhow!("Failed to parse kind-word binding JSON: {e}"))?;
            serde_json::from_str::<Value>(&fixed)
                .map_err(|e| anyhow::anyhow!("Failed to parse kind-word binding JSON: {e}"))
        }
    }
}

/// Each answer in the reply: two raw values (id, key), shape not yet validated.
///
/// Four forms are accepted, all meaning "this id chose this key": `{"b": [[id, key]]}` (the example),
/// `{"b": {"id": key}}`, a top-level object without `b` `{"id": key}`, a top-level array `[[id, key]]`;
/// an entry in an array may also be `{"id": .., "key": ..}`. If `b` is present only `b` is read; other top-level keys are
/// the model's commentary, not answers. If `b` is missing and the value is neither an object nor an array, there are no answers
fn answers(value: &Value) -> Vec<(Value, Value)> {
    let listed = value.get("b").unwrap_or(value);
    match listed {
        Value::Array(entries) => entries
            .iter()
            .map(|entry| match entry {
                Value::Array(pair) if pair.len() >= 2 => (pair[0].clone(), pair[1].clone()),
                Value::Object(map) => (
                    map.get("id").cloned().unwrap_or(Value::Null),
                    map.get("key")
                        .or_else(|| map.get("class"))
                        .or_else(|| map.get("b"))
                        .cloned()
                        .unwrap_or(Value::Null),
                ),
                other => (other.clone(), Value::Null),
            })
            .collect(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| (Value::String(k.clone()), v.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// An answer: the id must be in this batch, and the key must be null or one of the candidates (wrapped in an array also counts,
/// `["organization"]`, `[null]`, `["organization", "Organization — definition"]`, these are other ways the model writes it,
/// not a different answer)
fn parse_pair(
    id: &Value,
    key: &Value,
    by_id: &HashMap<i64, &KindWordItem<'_>>,
) -> Option<KindWordChoice> {
    let id = item_id(id)?;
    let item = by_id.get(&id)?;
    let key = match unwrapped(item, id, key)? {
        Value::Null => None,
        Value::String(written) => Some(candidate_key(item, written)?),
        _ => return None,
    };
    Some(KindWordChoice { id, key })
}

/// The first value in the array: the model wraps the key in an array, sometimes followed by that candidate's label and definition
/// (`["organization", "Organization — research institutes, ..."]`), and sometimes repeats the id first
/// (`{"0": [0, "organization"]}`); a repeated id is skipped. If what follows is **another candidate's key**,
/// that is two answers, not one: None, and the caller counts it as bad. An empty array is also None; a non-array is returned as is
fn unwrapped<'v>(item: &KindWordItem<'_>, id: i64, v: &'v Value) -> Option<&'v Value> {
    match v {
        Value::Array(list) => {
            let list = match list.first() {
                Some(first) if item_id(first) == Some(id) => &list[1..],
                _ => &list[..],
            };
            let (first, rest) = list.split_first()?;
            let another_key = rest
                .iter()
                .filter_map(Value::as_str)
                .any(|s| candidate_key(item, s).is_some());
            (!another_key).then_some(first)
        }
        other => Some(other),
    }
}

/// The id is a JSON integer; the model occasionally writes it as a string, which is read as a number
pub(crate) fn item_id(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Match the model's key to this item's candidates: first as written, then case-insensitively (only on a unique hit); returns
/// the candidate's own key. An empty string or a key not among the candidates is None, which the caller counts as bad
fn candidate_key(item: &KindWordItem<'_>, written: &str) -> Option<String> {
    let written = written.trim();
    if written.is_empty() {
        return None;
    }
    let keys = || item.candidates.iter().map(|c| c.key.trim());
    if let Some(exact) = keys().find(|k| *k == written) {
        return Some(exact.to_string());
    }
    let lower = written.to_lowercase();
    let mut hits = keys().filter(|k| k.to_lowercase() == lower);
    let first = hits.next()?;
    hits.next().is_none().then(|| first.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    /// Three items: company (should bind to organization), participant (a role word, should be null), bridge (the candidates include a structure)
    struct Fixture {
        company_spellings: Vec<String>,
        company_examples: Vec<String>,
        company_phrases: Vec<String>,
        participant_spellings: Vec<String>,
        bridge_spellings: Vec<String>,
        bridge_examples: Vec<String>,
        bridge_phrases: Vec<String>,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                company_spellings: strings(&["Company", "company", "公司"]),
                company_examples: strings(&["Brightway Builders", "Harbor Estates"]),
                company_phrases: strings(&["is based in", "was founded by"]),
                participant_spellings: strings(&["participants"]),
                bridge_spellings: strings(&["bridge", "Bridge"]),
                bridge_examples: strings(&["Harbor Bridge"]),
                bridge_phrases: strings(&["spans", "was completed"]),
            }
        }

        fn items(&self) -> Vec<KindWordItem<'_>> {
            let organization = ClassCandidate {
                key: "organization",
                label: "Organization",
                description: "An organized group of people with a shared purpose, such as a company, a council or a club.",
            };
            let person = ClassCandidate {
                key: "person",
                label: "Person",
                description: "A human being.",
            };
            let structure = ClassCandidate {
                key: "structure",
                label: "建筑物",
                description: "人建造的固定物：桥、楼、水坝。",
            };
            vec![
                KindWordItem {
                    id: 12,
                    kind_word: "company",
                    spellings: &self.company_spellings,
                    examples: &self.company_examples,
                    phrases: &self.company_phrases,
                    candidates: vec![organization, person],
                },
                KindWordItem {
                    id: 13,
                    kind_word: "participant",
                    spellings: &self.participant_spellings,
                    examples: &[],
                    phrases: &[],
                    candidates: vec![person, organization],
                },
                KindWordItem {
                    id: 14,
                    kind_word: "bridge",
                    spellings: &self.bridge_spellings,
                    examples: &self.bridge_examples,
                    phrases: &self.bridge_phrases,
                    candidates: vec![structure],
                },
            ]
        }
    }

    const FULL: &str = r#"{"b": [[12, "organization"], [13, null], [14, "structure"]]}"#;

    fn choice(id: i64, key: Option<&str>) -> KindWordChoice {
        KindWordChoice {
            id,
            key: key.map(str::to_string),
        }
    }

    #[test]
    fn a_full_reply_parses_with_keys_and_nulls() {
        let f = Fixture::new();
        let items = f.items();
        let (choices, malformed) = parse_kind_word_response(FULL, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![
                choice(12, Some("organization")),
                choice(13, None),
                choice(14, Some("structure")),
            ]
        );
    }

    /// Code fences and surrounding chatter are still tolerated (same json_block path)
    #[test]
    fn a_fenced_reply_parses() {
        let f = Fixture::new();
        let items = f.items();
        let raw = format!("Here you go:\n```json\n{FULL}\n```\nDone.");
        let (choices, malformed) = parse_kind_word_response(&raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(choices.len(), 3);
    }

    /// A key not among that item's candidates is a bad item; another item having that candidate does not count
    #[test]
    fn a_key_not_among_the_items_candidates_is_malformed() {
        let f = Fixture::new();
        let items = f.items();
        let raw = r#"{"b": [[12, "place"], [14, "organization"], [13, "person"]]}"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 2);
        assert_eq!(choices, vec![choice(13, Some("person"))]);
    }

    #[test]
    fn an_unknown_id_is_malformed() {
        let f = Fixture::new();
        let items = f.items();
        let raw = r#"{"b": [[99, "organization"], [12, "organization"]]}"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 1);
        assert_eq!(choices, vec![choice(12, Some("organization"))]);
    }

    /// Cut in the middle of the third pair's key: the first two pairs stay, the third item is absent (not null, and not bad)
    #[test]
    fn a_truncated_reply_is_repaired_to_the_last_complete_pair() {
        let f = Fixture::new();
        let items = f.items();
        let cut = FULL.find("\"struc").unwrap();
        let (choices, malformed) = parse_kind_word_response(&FULL[..cut], &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![choice(12, Some("organization")), choice(13, None)]
        );
    }

    /// Cut after a pair's id (`[14`): fall back to the previous complete `]`
    #[test]
    fn a_reply_cut_inside_a_pair_keeps_the_pairs_before_it() {
        let f = Fixture::new();
        let items = f.items();
        let raw = r#"{"b": [[12, "organization"], [13, null], [14"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(choices.len(), 2);
    }

    /// Still report failure when there is no pair at all: tolerance must not turn an empty result into success
    #[test]
    fn a_reply_with_nothing_complete_still_fails() {
        let f = Fixture::new();
        let items = f.items();
        assert!(parse_kind_word_response(r#"{"b": [[12, "org"#, &items).is_err());
        assert!(parse_kind_word_response("no json here", &items).is_err());
    }

    /// `b` is empty: no item answered, not a bad item. No `b` and keys that are not ids: that is an unreadable answer,
    /// counted as bad. Previously this kind of reply counted as "nothing answered", and a library left no trace over a dozen rounds
    #[test]
    fn a_reply_without_pairs_gives_no_choices() {
        let f = Fixture::new();
        let items = f.items();
        let (choices, malformed) = parse_kind_word_response(r#"{"b": {}}"#, &items).unwrap();
        assert!(choices.is_empty());
        assert_eq!(malformed, 0);
        let (choices, malformed) = parse_kind_word_response(r#"{"b": []}"#, &items).unwrap();
        assert!(choices.is_empty());
        assert_eq!(malformed, 0);
        let (choices, malformed) = parse_kind_word_response(r#"{"x": 1}"#, &items).unwrap();
        assert!(choices.is_empty());
        assert_eq!(malformed, 1);
    }

    /// Form observed with DeepSeek-V3.2: the whole reply is an object keyed by id, with no `b`. This is the bench
    /// reply where "12 documents, all type_ids empty" came from; it must parse to exactly the same shape as the example
    #[test]
    fn an_id_keyed_object_is_read_as_the_pair_list() {
        let f = Fixture::new();
        let items = f.items();
        let raw = r#"{ "12": "organization", "13": null, "14": "structure" }"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![
                choice(12, Some("organization")),
                choice(13, None),
                choice(14, Some("structure")),
            ]
        );
        // `b` holds an object rather than an array: read it the same way
        let raw = r#"{"b": {"12": "organization", "13": null}}"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(choices.len(), 2);
    }

    /// Another form from the same model: the key wrapped in a single-element array. An empty array and two keys are not one answer
    #[test]
    fn a_key_wrapped_in_a_one_element_array_is_unwrapped() {
        let f = Fixture::new();
        let items = f.items();
        let raw = r#"{ "12": ["organization"], "13": [null], "14": [] }"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 1, "an empty array is not an answer");
        assert_eq!(
            choices,
            vec![choice(12, Some("organization")), choice(13, None)]
        );
        let raw = r#"{"b": [[12, ["organization", "person"]]]}"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 1, "two keys is not an answer");
        assert!(choices.is_empty());
        // The value repeats the id before the key: `{"0": [0, "organization"]}`
        let raw = r#"{"12": [12, "organization"], "13": [13, null]}"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![choice(12, Some("organization")), choice(13, None)]
        );
        // The key is followed by a candidate line (label and definition): that is a repeated candidate, not a second answer
        let raw = r#"{"12": ["organization", "Organization — An organized group of people"], "13": [null, "no candidate fits"]}"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![choice(12, Some("organization")), choice(13, None)]
        );
    }

    /// A pair written as an object, or the whole reply given as a bare array without an outer object: read as is
    #[test]
    fn object_pairs_and_a_bare_array_parse() {
        let f = Fixture::new();
        let items = f.items();
        let raw = r#"{"b": [{"id": 12, "key": "organization"}, {"id": 13, "key": null}]}"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![choice(12, Some("organization")), choice(13, None)]
        );
        let raw = "```json\n[[12, \"organization\"], [14, \"structure\"]]\n```";
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![
                choice(12, Some("organization")),
                choice(14, Some("structure"))
            ]
        );
    }

    /// The same id answered twice: keep the first, count the second as bad
    #[test]
    fn a_duplicate_id_keeps_the_first() {
        let f = Fixture::new();
        let items = f.items();
        let raw = r#"{"b": [[12, "organization"], [12, null], [12, "person"]]}"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(choices, vec![choice(12, Some("organization"))]);
        assert_eq!(malformed, 2);
    }

    /// Bad items are counted one by one: not an array, only one cell, key is a number, key is an empty string, id is an object;
    /// an id written as a string is read as a number
    #[test]
    fn malformed_pairs_are_counted_not_fatal() {
        let f = Fixture::new();
        let items = f.items();
        let raw = r#"{"b": ["not a pair", [12], [13, 7], [14, ""], [{"id": 12}, null], ["12", "organization"]]}"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 5);
        assert_eq!(choices, vec![choice(12, Some("organization"))]);
    }

    /// Key with wrong case: matched back to the candidate's own key; leading and trailing whitespace is also trimmed
    #[test]
    fn a_key_in_the_wrong_case_is_read_back_as_the_candidates_key() {
        let f = Fixture::new();
        let items = f.items();
        let raw = r#"{"b": [[12, "Organization"], [14, " structure "]]}"#;
        let (choices, malformed) = parse_kind_word_response(raw, &items).unwrap();
        assert_eq!(malformed, 0);
        assert_eq!(
            choices,
            vec![
                choice(12, Some("organization")),
                choice(14, Some("structure"))
            ]
        );
    }

    /// The prompt lists each item's id, word, spellings, examples, phrases, candidate keys and definitions, and carries the "every
    /// such thing" criterion; an empty list is written as `(none)`
    #[test]
    fn the_prompt_lists_every_item_and_carries_the_rule() {
        let f = Fixture::new();
        let items = f.items();
        let msgs = build_kind_word_messages(&items);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        let system = &msgs[0].content;
        let user = &msgs[1].content;

        assert!(system.contains("every thing of this kind is an instance of that class"));
        assert!(system.contains("{\"b\": [[ITEM_ID, CLASS_KEY_OR_NULL], ...]}"));
        assert!(system.contains("never merely related"));
        assert!(system.contains("Never invent a key"));

        assert!(user.contains("Item 12: \"company\"\n"));
        assert!(user.contains("Spellings: \"Company\", \"company\", \"公司\"\n"));
        assert!(user.contains("Examples: \"Brightway Builders\", \"Harbor Estates\"\n"));
        assert!(user.contains("Phrases: \"is based in\", \"was founded by\"\n"));
        assert!(user.starts_with("Classes:\n- "));
        assert_eq!(
            user.matches("- organization: Organization — An organized group of people")
                .count(),
            1,
            "一条定义一批只写一遍"
        );
        assert_eq!(user.matches("- person: Person — A human being.").count(), 1);
        assert!(
            user.find("- structure: ").unwrap() < user.find("Item 12").unwrap(),
            "定义表在各项之前"
        );

        assert!(user.contains("Item 13: \"participant\"\n"));
        assert!(user.contains("Spellings: \"participants\"\nExamples: (none)\nPhrases: (none)\n"));

        assert!(user.contains("Item 14: \"bridge\"\n"));
        assert!(user.contains("Examples: \"Harbor Bridge\"\n"));
        assert!(user.contains("Phrases: \"spans\", \"was completed\"\n"));
        assert!(
            user.contains("- structure: 建筑物 — 人建造的固定物：桥、楼、水坝。"),
            "候选的标签与定义照库里的语言给: {user}"
        );

        let a = user.find("Item 12").unwrap();
        let b = user.find("Item 13").unwrap();
        let c = user.find("Item 14").unwrap();
        assert!(a < b && b < c);
        assert!(!user.ends_with('\n'));
    }

    /// Label equal to key is not repeated; empty definition has no dash; no candidates is written as `(none)`
    #[test]
    fn a_candidate_line_omits_what_is_empty() {
        let f = Fixture::new();
        let spellings = strings(&["council"]);
        let mut items = f.items();
        items.truncate(1);
        items[0].candidates = vec![
            ClassCandidate {
                key: "organization",
                label: "organization",
                description: "",
            },
            ClassCandidate {
                key: "place",
                label: "",
                description: "A location.",
            },
        ];
        items.push(KindWordItem {
            id: 15,
            kind_word: "council",
            spellings: &spellings,
            examples: &[],
            phrases: &[],
            candidates: vec![],
        });
        let user = &build_kind_word_messages(&items)[1].content;
        assert!(user.starts_with("Classes:\n- organization\n- place — A location.\n\n"));
        assert!(user.contains("Candidates: organization, place\n"));
        assert!(user.contains("Item 15: \"council\"\n"));
        assert!(user.ends_with("Candidates: (none)"));
    }

    /// The prompt's examples are neutral: no names from any measured corpus appear
    #[test]
    fn the_prompt_names_no_benchmark_corpus() {
        let f = Fixture::new();
        let items = f.items();
        let msgs = build_kind_word_messages(&items);
        for m in &msgs {
            for name in [
                "NVIDIA",
                "Nvidia",
                "EDGAR",
                "CUAD",
                "DocRED",
                "Holmes",
                "Wikidata",
                "ai-timeline",
                "OpenAI",
                "Pfizer",
                "State of the Union",
            ] {
                assert!(!m.content.contains(name), "{name} in prompt: {}", m.content);
            }
        }
    }
}
