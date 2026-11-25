//! One proposal call of the ontology agent (0061 decision 2).
//!
//! Input is the shape the open graph has that the ontology cannot hold: signatures the aligner judged "none" or "undecided" (phrase × the two ends' classes,
//! with example sentences and quotes), and kind words that bind to no class (with example names), plus the current ontology (class and attribute definitions, domains, ranges)
//! and the library's capability questions. The question is: **which classes and which attributes, once added, let these shapes be held and these questions be answered**.
//! Each proposal carries a definition written for the aligner, the shapes it will bind, and the questions it serves; shapes that attributes already in the ontology can hold
//! are recorded in `existing` and not proposed again. The agent only proposes; it does not write the ontology (0012: the ontology is a contract signed by people).
//!
//! Replies are compact JSON, parsed as in phrase_align: a bad item is counted and does not ruin the batch; an illegal key or a shape id not in the batch
//! both count as bad.

use crate::align::parse_value;
use serde_json::Value;
use std::collections::HashSet;
use utopia_llm::ChatMessage;

/// A signature the alignment cannot hold
#[derive(Debug, Clone)]
pub struct OpenSignature<'a> {
    pub id: i64,
    pub phrase: &'a str,
    pub subject_class: Option<&'a str>,
    pub object_class: Option<&'a str>,
    pub object_is_value: bool,
    pub statement_count: i64,
    pub examples: &'a [String],
    pub quotes: &'a [String],
}

/// A kind word that binds to no class
#[derive(Debug, Clone)]
pub struct OpenKindWord<'a> {
    pub id: i64,
    pub kind_word: &'a str,
    pub count: i64,
    pub examples: &'a [String],
    pub phrases: &'a [String],
}

/// Current ontology: classes and attributes, in the library's language
#[derive(Debug, Clone, Default)]
pub struct Glossary<'a> {
    /// (key, label, description)
    pub classes: Vec<(&'a str, &'a str, &'a str)>,
    /// (key, label, kind, domains, ranges, description)
    pub properties: Vec<GlossaryProperty<'a>>,
    /// Attributes in this batch that are not used, only named: (key, label). The model needs to know they exist so it does not propose them again;
    /// definitions are given only for those that match the structure or are nearest by vector (same shortlist as alignment, 0044 decision 3's cost lesson)
    pub others: Vec<(&'a str, &'a str)>,
}

/// An attribute in the vocabulary: (key, label, kind, domains, ranges, description)
pub type GlossaryProperty<'a> = (
    &'a str,
    &'a str,
    &'a str,
    Vec<&'a str>,
    Vec<&'a str>,
    &'a str,
);

/// A capability question
#[derive(Debug, Clone)]
pub struct QuestionItem<'a> {
    pub id: i64,
    pub text: &'a str,
}

/// An element proposed by the agent
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// class | property
    pub kind: String,
    pub key: String,
    pub label: String,
    pub definition: String,
    /// Attribute: is the object a value (attribute) or an entity (relation)
    pub value: bool,
    /// Data type of a value attribute: number | date | text | bool
    pub datatype: Option<String>,
    /// Domain / range of an attribute, or a class's parent: a class key, which may be in the ontology or newly proposed in this batch
    pub domains: Vec<String>,
    pub ranges: Vec<String>,
    pub parents: Vec<String>,
    /// The signature ids and kind-word ids it will bind (within this batch)
    pub signatures: Vec<i64>,
    pub kind_words: Vec<i64>,
    /// The question id it serves
    pub questions: Vec<i64>,
}

/// "Already in the ontology": the existing attribute or class key, and the shapes it can hold
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Existing {
    pub key: String,
    /// forward | reverse: whether the shape's subject is the attribute's subject. A kind word to a class has no direction
    pub direction: Option<String>,
    pub signatures: Vec<i64>,
    pub kind_words: Vec<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProposalReply {
    pub proposals: Vec<Proposal>,
    pub existing: Vec<Existing>,
    /// Count of unreadable items
    pub malformed: usize,
    /// Why each bad item is bad: the bench counts them, and they are visible in the logs
    pub reasons: Vec<String>,
}

impl ProposalReply {
    fn bad(&mut self, why: String) {
        self.malformed += 1;
        self.reasons.push(why);
    }
}

const SYSTEM: &str = "\
You are the ontology agent of a knowledge base. The base extracts statements from documents as \
they are worded; an aligner then binds each statement shape (a relation phrase between a subject \
class and an object class, or a kind word the documents use for a thing) to a property or class of \
the ontology. The shapes below are the ones the ontology cannot hold: no property or class fits \
them. The competency questions are what the base exists to answer.\n\
Propose the smallest set of new classes and properties that would let the base hold these shapes \
AND serve the questions. Rules:\n\
- A proposal binds shapes: list the shape ids it would bind (s for signatures, k for kind words). \
Do not propose an element that binds nothing.\n\
- Prefer serving a question. An element that serves no question must bind at least two statements' \
worth of shapes, or be a class that several kind words name.\n\
- Write every label and definition in {LANGUAGE}, even where the shapes, the ontology or these \
instructions use another; keys stay snake_case ASCII. The definition is for the aligner: one or two \
sentences saying what the property asserts, what its subject and object are, and what it does NOT \
cover.\n\
- A property whose object is a value (a number, a date, text) is an attribute: value=true with a \
datatype (number | date | text | bool) and no ranges. Otherwise value=false with domains and ranges \
as class keys, existing or proposed in this same answer.\n\
- Keys are snake_case ASCII. Reuse an existing key only under existing, never as a proposal.\n\
- If an existing property or class already holds a shape, list it under existing with the shape ids \
instead of proposing, with dir: forward when the shape's subject is the property's subject, reverse \
when the shape reads the property backwards (\"X wrote Y\" is author read in reverse).\n\
- Do not propose the inverse of an existing property: list the shape under that property with dir \
reverse.\n\
- An existing entry holds shapes, not questions: every entry under existing lists at least one \
shape id in s or k. Do not add an entry only to say that a property serves a question.\n\
Answer with one JSON object and nothing else:\n\
{\"p\":[{\"kind\":\"property\",\"key\":\"...\",\"label\":\"...\",\"definition\":\"...\",\"value\":false,\
\"datatype\":null,\"domains\":[\"class_key\"],\"ranges\":[\"class_key\"],\"s\":[ids],\"k\":[ids],\"q\":[ids]},\
{\"kind\":\"class\",\"key\":\"...\",\"label\":\"...\",\"definition\":\"...\",\"parents\":[\"class_key\"],\
\"s\":[],\"k\":[ids],\"q\":[ids]}],\"existing\":[{\"key\":\"existing_key\",\"dir\":\"forward\",\"s\":[ids],\"k\":[ids]}]}";

pub fn build_proposal_messages(
    signatures: &[OpenSignature<'_>],
    kind_words: &[OpenKindWord<'_>],
    glossary: &Glossary<'_>,
    questions: &[QuestionItem<'_>],
    ontology_lang: &str,
) -> Vec<ChatMessage> {
    // Labels and definitions are written in the language set by the library's ontology_lang (0004 decisions 3, 5), not following the document, the ontology or this English instruction:
    // a zh library reads English documents and installs English packs, and still writes Chinese. The model is shown "Chinese", which it understands; it may not understand "zh"
    let language = if ontology_lang == "zh" {
        "Chinese"
    } else {
        "English"
    };
    let mut user = String::new();
    user.push_str("Ontology classes:\n");
    if glossary.classes.is_empty() {
        user.push_str("  (none)\n");
    }
    for (key, label, description) in &glossary.classes {
        user.push_str(&format!("- {key} · {label}"));
        if !description.trim().is_empty() {
            user.push_str(&format!(" · {}", description.trim()));
        }
        user.push('\n');
    }
    user.push_str("\nOntology properties:\n");
    if glossary.properties.is_empty() {
        user.push_str("  (none)\n");
    }
    for (key, label, kind, domains, ranges, description) in &glossary.properties {
        user.push_str(&format!("- {key} · {label} · {kind}"));
        if !domains.is_empty() {
            user.push_str(&format!(" · domain: {}", domains.join(", ")));
        }
        if !ranges.is_empty() {
            user.push_str(&format!(" · range: {}", ranges.join(", ")));
        }
        if !description.trim().is_empty() {
            user.push_str(&format!(" · {}", description.trim()));
        }
        user.push('\n');
    }
    if !glossary.others.is_empty() {
        user.push_str(
            "Other existing properties (key · label only; none of them fits these shapes by \
             structure or meaning, but do not propose a key already here):\n",
        );
        for (key, label) in &glossary.others {
            user.push_str(&format!("- {key} · {label}\n"));
        }
    }
    user.push_str("\nCompetency questions:\n");
    if questions.is_empty() {
        user.push_str(
            "  (none written yet: propose for the shapes that bind the most statements)\n",
        );
    }
    for q in questions {
        user.push_str(&format!("q{}: {}\n", q.id, q.text.trim()));
    }
    user.push_str("\nShapes the ontology cannot hold:\n");
    for s in signatures {
        let subject = s.subject_class.unwrap_or("(untyped)");
        let object = if s.object_is_value {
            "(a value)".to_string()
        } else {
            s.object_class.unwrap_or("(untyped)").to_string()
        };
        user.push_str(&format!(
            "s{}: phrase \"{}\" · subject class: {} · object: {} · {} statements\n",
            s.id, s.phrase, subject, object, s.statement_count
        ));
        for (i, ex) in s.examples.iter().enumerate() {
            let quote = s.quotes.get(i).map(String::as_str).unwrap_or("").trim();
            if quote.is_empty() {
                user.push_str(&format!("  · {ex}\n"));
            } else {
                user.push_str(&format!("  · {ex}\n    \"{quote}\"\n"));
            }
        }
    }
    for k in kind_words {
        user.push_str(&format!(
            "k{}: kind word \"{}\" · {} things",
            k.id, k.kind_word, k.count
        ));
        if !k.examples.is_empty() {
            user.push_str(&format!(" · e.g. {}", k.examples.join(", ")));
        }
        if !k.phrases.is_empty() {
            user.push_str(&format!(
                " · they are subjects of: {}",
                k.phrases.join(", ")
            ));
        }
        user.push('\n');
    }
    vec![
        ChatMessage {
            role: "system".into(),
            content: SYSTEM.replace("{LANGUAGE}", language),
        },
        ChatMessage {
            role: "user".into(),
            content: user,
        },
    ]
}

fn ids(v: Option<&Value>, prefix: char, known: &HashSet<i64>) -> Result<Vec<i64>, ()> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let Some(arr) = v.as_array() else {
        return Err(());
    };
    let mut out = Vec::new();
    for x in arr {
        let id = match x {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().trim_start_matches(prefix).parse::<i64>().ok(),
            _ => None,
        };
        match id {
            Some(id) if known.contains(&id) => out.push(id),
            _ => return Err(()),
        }
    }
    Ok(out)
}

fn strings(v: Option<&Value>) -> Result<Vec<String>, ()> {
    let Some(v) = v else { return Ok(Vec::new()) };
    match v {
        Value::Null => Ok(Vec::new()),
        Value::String(s) => Ok(vec![s.trim().to_string()]),
        Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().map(|s| s.trim().to_string()).ok_or(()))
            .collect(),
        _ => Err(()),
    }
}

fn valid_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 64
        && k.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && k.chars().next().is_some_and(|c| c.is_ascii_lowercase())
}

/// Read the reply. `signature_ids` / `kind_word_ids` / `question_ids` are the legal ids in this batch: an answer pointing elsewhere counts as bad
pub fn parse_proposal_response(
    raw: &str,
    signature_ids: &HashSet<i64>,
    kind_word_ids: &HashSet<i64>,
    question_ids: &HashSet<i64>,
) -> anyhow::Result<ProposalReply> {
    let v = parse_value(raw)?;
    let mut reply = ProposalReply::default();
    let items: Vec<&Value> = match v.get("p") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(Value::Object(m)) => m.values().collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(_) => {
            reply.bad("`p` is not a list".to_string());
            Vec::new()
        }
    };
    let mut seen: HashSet<String> = HashSet::new();
    for it in items {
        let Some(obj) = it.as_object() else {
            reply.bad("a proposal is not an object".to_string());
            continue;
        };
        let kind = obj
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("property")
            .trim()
            .to_lowercase();
        let key = obj
            .get("key")
            .and_then(Value::as_str)
            .map(|k| k.trim().to_lowercase())
            .unwrap_or_default();
        if !matches!(kind.as_str(), "class" | "property")
            || !valid_key(&key)
            || !seen.insert(key.clone())
        {
            reply.bad(format!("bad kind, bad key or repeated key: {kind} {key:?}"));
            continue;
        }
        let label = obj
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&key)
            .to_string();
        let definition = obj
            .get("definition")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        let value = obj.get("value").and_then(Value::as_bool).unwrap_or(false);
        let datatype = obj
            .get("datatype")
            .and_then(Value::as_str)
            .map(|d| d.trim().to_lowercase())
            .filter(|d| matches!(d.as_str(), "number" | "date" | "text" | "bool"));
        let (Ok(domains), Ok(ranges), Ok(parents)) = (
            strings(obj.get("domains")),
            strings(obj.get("ranges")),
            strings(obj.get("parents")),
        ) else {
            reply.bad(format!(
                "domains, ranges or parents are not lists of strings: {key}"
            ));
            continue;
        };
        let (Ok(signatures), Ok(kind_words), Ok(questions)) = (
            ids(obj.get("s"), 's', signature_ids),
            ids(obj.get("k"), 'k', kind_word_ids),
            ids(obj.get("q"), 'q', question_ids),
        ) else {
            reply.bad(format!("a shape or question id outside the batch: {key}"));
            continue;
        };
        // A proposal that binds no shape did not grow out of the graph: not accepted
        if signatures.is_empty() && kind_words.is_empty() {
            reply.bad(format!("binds no shape: {key}"));
            continue;
        }
        reply.proposals.push(Proposal {
            kind,
            key,
            label,
            definition,
            value,
            datatype: if value { datatype } else { None },
            domains: domains.into_iter().filter(|d| !d.is_empty()).collect(),
            ranges: if value {
                Vec::new()
            } else {
                ranges.into_iter().filter(|r| !r.is_empty()).collect()
            },
            parents: parents.into_iter().filter(|p| !p.is_empty()).collect(),
            signatures,
            kind_words,
            questions,
        });
    }
    if let Some(Value::Array(ex)) = v.get("existing") {
        for it in ex {
            let Some(obj) = it.as_object() else {
                reply.bad("an existing entry is not an object".to_string());
                continue;
            };
            let key = obj
                .get("key")
                .and_then(Value::as_str)
                .map(|k| k.trim().to_string())
                .unwrap_or_default();
            let (Ok(signatures), Ok(kind_words)) = (
                ids(obj.get("s"), 's', signature_ids),
                ids(obj.get("k"), 'k', kind_word_ids),
            ) else {
                reply.bad(format!("existing: a shape id outside the batch: {key}"));
                continue;
            };
            if key.is_empty() || (signatures.is_empty() && kind_words.is_empty()) {
                reply.bad(format!("existing: no key or no shapes: {key:?}"));
                continue;
            }
            let direction = match obj.get("dir").and_then(Value::as_str).map(str::trim) {
                Some("reverse") => Some("reverse".to_string()),
                Some("forward") | None | Some("") => Some("forward".to_string()),
                Some(_) => {
                    reply.bad(format!("existing: unknown direction: {key}"));
                    continue;
                }
            };
            reply.existing.push(Existing {
                key,
                direction,
                signatures,
                kind_words,
            });
        }
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids_of(xs: &[i64]) -> HashSet<i64> {
        xs.iter().copied().collect()
    }

    #[test]
    fn a_well_formed_reply_yields_proposals_and_existing() {
        let raw = r#"{"p":[{"kind":"property","key":"supplies","label":"supplies","definition":"The subject organisation delivers goods to the object organisation. Not for one-off sales.","value":false,"datatype":null,"domains":["organization"],"ranges":["organization"],"s":[0,2],"k":[],"q":[1]},{"kind":"class","key":"supplier","label":"Supplier","definition":"An organisation that supplies others.","parents":["organization"],"s":[],"k":[0],"q":[]}],"existing":[{"key":"located_in","dir":"reverse","s":[1],"k":[]}]}"#;
        let r = parse_proposal_response(raw, &ids_of(&[0, 1, 2]), &ids_of(&[0]), &ids_of(&[1]))
            .unwrap();
        assert_eq!(r.malformed, 0);
        assert_eq!(r.proposals.len(), 2);
        assert_eq!(r.proposals[0].key, "supplies");
        assert_eq!(r.proposals[0].signatures, vec![0, 2]);
        assert_eq!(r.proposals[0].questions, vec![1]);
        assert_eq!(r.proposals[0].domains, vec!["organization"]);
        assert_eq!(r.proposals[1].kind, "class");
        assert_eq!(r.proposals[1].parents, vec!["organization"]);
        assert_eq!(
            r.existing,
            vec![Existing {
                key: "located_in".into(),
                direction: Some("reverse".into()),
                signatures: vec![1],
                kind_words: vec![]
            }]
        );
    }

    #[test]
    fn a_proposal_that_binds_nothing_or_points_outside_the_batch_is_malformed() {
        let raw = r#"{"p":[{"kind":"property","key":"orphan","label":"o","definition":"","s":[],"k":[],"q":[]},{"kind":"property","key":"far","label":"f","definition":"","s":[9],"k":[],"q":[]},{"kind":"property","key":"Bad Key","label":"b","definition":"","s":[0],"k":[],"q":[]},{"kind":"property","key":"ok","label":"ok","definition":"d","value":true,"datatype":"number","s":["s0"],"k":[],"q":["q1"]}]}"#;
        let r = parse_proposal_response(raw, &ids_of(&[0]), &ids_of(&[]), &ids_of(&[1])).unwrap();
        assert_eq!(r.malformed, 3);
        assert_eq!(r.proposals.len(), 1);
        assert_eq!(r.proposals[0].key, "ok");
        assert!(r.proposals[0].value);
        assert_eq!(r.proposals[0].datatype.as_deref(), Some("number"));
        assert_eq!(
            r.proposals[0].signatures,
            vec![0],
            "an id written as s0 still counts"
        );
        assert_eq!(r.proposals[0].questions, vec![1]);
    }

    #[test]
    fn the_prompt_names_every_input_once() {
        let examples = vec!["W-1 —supplies→ F-2".to_string()];
        let quotes = vec!["W-1 supplies F-2 with parts.".to_string()];
        let sigs = vec![OpenSignature {
            id: 0,
            phrase: "supplies",
            subject_class: Some("organization"),
            object_class: Some("organization"),
            object_is_value: false,
            statement_count: 7,
            examples: &examples,
            quotes: &quotes,
        }];
        let names = vec!["Acme Supply".to_string()];
        let phrases = vec!["supplies".to_string()];
        let kws = vec![OpenKindWord {
            id: 0,
            kind_word: "supplier",
            count: 3,
            examples: &names,
            phrases: &phrases,
        }];
        let glossary = Glossary {
            classes: vec![("organization", "Organization", "a company or institution")],
            properties: vec![(
                "located_in",
                "located in",
                "relation",
                vec!["organization"],
                vec!["location"],
                "where a thing is",
            )],
            others: vec![("p999", "somethingElse")],
        };
        let qs = vec![QuestionItem {
            id: 1,
            text: "Who supplies whom?",
        }];
        let m = build_proposal_messages(&sigs, &kws, &glossary, &qs, "zh");
        assert_eq!(m.len(), 2);
        assert!(m[0].content.contains("definition in Chinese"));
        let user = &m[1].content;
        assert!(user.contains("Other existing properties"), "{user}");
        assert!(user.contains("- p999 · somethingElse"), "{user}");
        for needle in [
            "s0: phrase \"supplies\"",
            "k0: kind word \"supplier\"",
            "q1: Who supplies whom?",
            "- located_in · located in · relation · domain: organization · range: location",
            "\"W-1 supplies F-2 with parts.\"",
        ] {
            assert!(user.contains(needle), "missing {needle:?} in\n{user}");
        }
    }
}
