//! The second layer of governance (0025, second cut): batch the pairs that cannot be decided, and look at each one again, one at a time, with tools.
//!
//! What the model can see: all facts on one side, source excerpts from one side, the ledger's human decisions on a name,
//! other entities in the library with similar names, and what a merge would affect (0028: contradictions that consistency checks would raise, derivations on one side,
//! answers to questions already asked, and whether the two sides' types belong to one broad class). It ends in one of two ways: `decide` (same / different + confidence +
//! one sentence of reasoning) or `defer` (leave a specific question to a person). Tool definitions, the prompt and turn parsing
//! live here; running the loop and querying the database happens in the server's governance task. Nothing here touches the database or the model.

use serde_json::{json, Value};

use crate::{AdjudicationPair, IDENTITY_RULES};

/// Each pair gets at most this many queries before a verdict. The model follows a menu: facts on both sides, sources on both sides, the ledger for both names,
/// namesakes, and what a merge would touch. Eight. It used to be six; on the identity bench a third of second looks
/// "looked but never finished": the trace shows it spent its last two queries on rejected namesakes and consequence queries, and never gave a verdict in a turn
pub const MAX_STEPS: usize = 8;

/// Reply when it still wants to query after the limit: no results, only a reminder to finish
pub const LIMIT_REACHED: &str = "Lookup limit reached: you have seen what can be seen. \
     Answer now with decide (same or different, with your confidence) or defer (with the \
     question a person should answer).";

/// What the first look said: carried into the second look so the model knows why it did not decide last time
pub struct EarlierLook<'a> {
    /// same | different | unsure
    pub verdict: &'a str,
    pub confidence: f32,
    pub why: Option<&'a str>,
}

/// What the model asks for in one turn
#[derive(Debug, PartialEq)]
pub enum Step {
    /// Look up one thing: facts / quotes / ledger / namesakes / consequences
    Lookup {
        tool: String,
        args: Value,
    },
    Decide {
        same: bool,
        confidence: f32,
        why: String,
    },
    Defer {
        question: String,
    },
    /// Unknown tool name or missing argument: tell the model, do not guess
    Unknown(String),
}

/// Function definition in the OpenAI protocol
pub fn tools() -> Value {
    let side = json!({
        "type": "object",
        "properties": { "side": { "type": "string", "enum": ["A", "B"] } },
        "required": ["side"]
    });
    let query = json!({
        "type": "object",
        "properties": { "query": { "type": "string" } },
        "required": ["query"]
    });
    json!([
        { "type": "function", "function": {
            "name": "facts",
            "description": "Every recorded fact of one side (A or B): its relations to other entities, with time ranges where known.",
            "parameters": side }},
        { "type": "function", "function": {
            "name": "quotes",
            "description": "Source passages that mention one side: the sentences its facts were extracted from, each with the document it comes from.",
            "parameters": side }},
        { "type": "function", "function": {
            "name": "ledger",
            "description": "What people in this base decided before about a name: merges, keeps and reverted merges whose names contain the query.",
            "parameters": query }},
        { "type": "function", "function": {
            "name": "namesakes",
            "description": "Other entities in this base whose name contains the query, with their type and how many facts they carry.",
            "parameters": query }},
        { "type": "function", "function": {
            "name": "consequences",
            "description": "What merging A and B would touch: relations that allow one value where the two sides hold different ones, derived facts resting on either side, chat answers that named either side, and whether the two types belong to one family. A merge that would touch any of these is held for a person whatever your confidence.",
            "parameters": { "type": "object", "properties": {} } }},
        { "type": "function", "function": {
            "name": "decide",
            "description": "Give the verdict for this pair.",
            "parameters": {
                "type": "object",
                "properties": {
                    "verdict": { "type": "string", "enum": ["same", "different"] },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
                    "why": { "type": "string", "description": "One sentence naming the fact, passage or precedent that settled it." }
                },
                "required": ["verdict", "confidence", "why"]
            }}},
        { "type": "function", "function": {
            "name": "defer",
            "description": "Leave the pair for a person, when no lookup can settle it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The one question a person could answer at a glance, naming the fact or document that would settle it." }
                },
                "required": ["question"]
            }}}
    ])
}

/// Start of the conversation: system prompt + this pair + the conclusion of the last look
pub fn messages(pair: &AdjudicationPair, earlier: &EarlierLook) -> Vec<Value> {
    let system = format!(
        "You are an entity-resolution adjudicator for a knowledge graph, looking at ONE pair of \
         records that an earlier, quicker look could not settle. Decide whether the two records \
         refer to the SAME real-world entity or are namesakes.\n\
         \n\
         {IDENTITY_RULES}\n\
         \n\
         You may look things up before answering, at most {MAX_STEPS} lookups: the facts of a \
         side, the source passages that mention a side, what people in this base decided about \
         a name, other entities with a similar name, and what merging the two would touch (a \
         relation that allows one value where the sides hold different ones is a contradiction, \
         and a merge that would touch anything outside the graph is held for a person whatever \
         your confidence: prefer to defer with the question that would settle it). People's \
         earlier decisions are how the \
         owners of this base want such cases judged; follow them unless the facts of this pair \
         clearly differ, and never let one override a contradiction in the facts. Look only for \
         what would change your answer.\n\
         \n\
         Finish with exactly one tool call: decide (same or different, a confidence in 0~1, and \
         one sentence naming the fact, passage or precedent that settled it) or defer (when no \
         lookup can settle it: leave the ONE question a person could answer at a glance, naming \
         the fact or document that would settle it). A wrong merge is far more damaging than \
         leaving two records separate: when the evidence is thin, defer."
    );
    let side = |label: &str, s: &crate::AdjudicationSide| {
        let facts = if s.facts.is_empty() {
            "  (no recorded facts)".to_string()
        } else {
            s.facts
                .iter()
                .map(|f| format!("  - {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        format!("Record {label}: \"{}\" ({})\n{facts}", s.name, s.type_label)
    };
    let precedents = if pair.precedents.is_empty() {
        String::new()
    } else {
        let lines = pair
            .precedents
            .iter()
            .map(|l| format!("  - {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Precedents (decided by people in this base):\n{lines}\n")
    };
    let why = earlier.why.map(|w| format!(" — {w}")).unwrap_or_default();
    let why_paired = pair
        .proposed_because
        .as_deref()
        .map(|w| format!("Why paired: {w}\n"))
        .unwrap_or_default();
    let user = format!(
        "{}\n{}\n{why_paired}{precedents}The earlier look said: {} ({:.2}){why}.",
        side("A", &pair.left),
        side("B", &pair.right),
        earlier.verdict,
        earlier.confidence,
    );
    vec![
        json!({ "role": "system", "content": system }),
        json!({ "role": "user", "content": user }),
    ]
}

/// Reminder when the model lets the tool loop look for a conclusion
pub const NUDGE: &str = "Finish with the decide or defer tool.";

/// One tool call counts as one step. Arguments are the protocol's JSON string as is
pub fn read_step(name: &str, arguments: &str) -> Step {
    let args: Value = serde_json::from_str(arguments).unwrap_or(json!({}));
    match name {
        "facts" | "quotes" => match args["side"].as_str() {
            Some("A") | Some("B") => Step::Lookup {
                tool: name.into(),
                args,
            },
            _ => Step::Unknown(format!("{name} needs side A or B")),
        },
        "ledger" | "namesakes" => match args["query"].as_str() {
            Some(q) if !q.trim().is_empty() => Step::Lookup {
                tool: name.into(),
                args,
            },
            _ => Step::Unknown(format!("{name} needs a query")),
        },
        "consequences" => Step::Lookup {
            tool: name.into(),
            args: json!({}),
        },
        "decide" => {
            let same = match args["verdict"].as_str() {
                Some("same") => true,
                Some("different") => false,
                _ => return Step::Unknown("decide needs verdict same or different".into()),
            };
            let confidence = args["confidence"]
                .as_f64()
                .map(|c| c as f32)
                .unwrap_or(0.5)
                .clamp(0.0, 1.0);
            let why = args["why"].as_str().unwrap_or("").trim().to_string();
            Step::Decide {
                same,
                confidence,
                why,
            }
        }
        "defer" => match args["question"].as_str().map(str::trim) {
            Some(q) if !q.is_empty() => Step::Defer {
                question: q.to_string(),
            },
            _ => Step::Unknown("defer needs a question".into()),
        },
        other => Step::Unknown(format!("no tool named {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AdjudicationSide;

    #[test]
    fn a_lookup_needs_its_argument() {
        assert_eq!(
            read_step("facts", r#"{"side":"A"}"#),
            Step::Lookup {
                tool: "facts".into(),
                args: json!({ "side": "A" })
            }
        );
        assert!(matches!(
            read_step("facts", r#"{"side":"C"}"#),
            Step::Unknown(_)
        ));
        assert!(matches!(read_step("ledger", r#"{}"#), Step::Unknown(_)));
        assert!(matches!(read_step("ledger", "not json"), Step::Unknown(_)));
        assert!(matches!(
            read_step("namesakes", r#"{"query":"Apple"}"#),
            Step::Lookup { .. }
        ));
    }

    #[test]
    fn a_decision_is_read_and_clamped() {
        assert_eq!(
            read_step(
                "decide",
                r#"{"verdict":"same","confidence":1.7,"why":"one CEO"}"#
            ),
            Step::Decide {
                same: true,
                confidence: 1.0,
                why: "one CEO".into()
            }
        );
        assert!(matches!(
            read_step("decide", r#"{"verdict":"maybe"}"#),
            Step::Unknown(_)
        ));
        assert_eq!(
            read_step(
                "defer",
                r#"{"question":" Is the 2024 CFO the same person? "}"#
            ),
            Step::Defer {
                question: "Is the 2024 CFO the same person?".into()
            }
        );
        assert!(matches!(
            read_step("defer", r#"{"question":""}"#),
            Step::Unknown(_)
        ));
        assert!(matches!(read_step("smash", "{}"), Step::Unknown(_)));
    }

    #[test]
    fn the_opening_carries_the_pair_and_the_earlier_look() {
        let pair = AdjudicationPair {
            left: AdjudicationSide {
                name: "Zhang Wei".into(),
                type_label: "Person".into(),
                facts: vec!["works at → Nebula".into()],
            },
            right: AdjudicationSide {
                name: "Zhang Wei".into(),
                type_label: "Person".into(),
                facts: vec![],
            },
            precedents: vec!["this same pair was kept apart by a person on 2026-09-01".into()],
            proposed_because: None,
        };
        let m = messages(
            &pair,
            &EarlierLook {
                verdict: "unsure",
                confidence: 0.5,
                why: Some("no facts on B"),
            },
        );
        assert_eq!(m.len(), 2);
        let user = m[1]["content"].as_str().unwrap();
        assert!(user.contains("Record A: \"Zhang Wei\" (Person)"));
        assert!(user.contains("(no recorded facts)"));
        assert!(user.contains("kept apart by a person"));
        assert!(user.contains("The earlier look said: unsure (0.50) — no facts on B."));
        let tools = tools();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "facts",
                "quotes",
                "ledger",
                "namesakes",
                "consequences",
                "decide",
                "defer"
            ]
        );
    }
}
