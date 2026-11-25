//! Open extraction (0044 first slice, #729): the model reads a block and writes down, in **the document's own words**, what it states.
//!
//! Unlike the contract in lib.rs, which carries an ontology, this path's prompt has **no type list, no relation list,
//! and no document date**: relation phrases are kept as written in the source ("acquired", "build"), and time is copied as the words in the text
//! ("last winter"), not converted to dates or normalized. The ontology is a view applied afterwards, not a filter at extraction time.
//! The only part of the prompt that changes per block is the body, the opening and the known entities, so the system message is a constant:
//! that is what the prefix cache hits.
//!
//! The reply is compact JSON: arrays and short keys. A block's output often runs to hundreds of items, and long key names burn
//! max_tokens, and a truncation loses the second half of the block. But **statements refer to things by name, and each quote is copied on its own**,
//! and nothing is numbered. The numbered version was tested on a dense financial report, where deepseek-v4-flash shifted ids ("NVIDIA has
//! reached AI", "tokens are tokens", where the source says "AI has reached its inflection point"),
//! and two replies for the same block numbered differently. The name + per-quote prototype fabricated 0 of 333 items.
//! The extra tokens buy freedom from shifted ids.
//!
//! ```json
//! {"e": [["Harbor Bridge", "bridge", 1], ["farmland", "land", 0]],
//!  "s": [["The city council of Westbrook awarded the paving contract for the Harbor Bridge to Brightway Builders for $2 million on March 4, 2011.", "city council of Westbrook", "awarded", "paving contract", null, {"to": "Brightway Builders", "amount": "$2 million"}, "March 4, 2011", null]],
//!  "n": [["Brightway Builders", "Brightway", "Brightway Builders, known locally as Brightway, is based in Port Ellen."]]}
//! ```
//!
//! - `e`: `[name or description, kind word, named]`. Every thing a statement mentions is listed here once, using exactly
//!   the name the statement uses. named = 1 is a thing the source names (person, organization, product, place, document,
//!   law, event); 0 is a thing only described ("the northern wing", "farmland"). A name carries no quotation or
//!   qualifier ("the Harbor Treaty signed on May 3, 1998" is called "Harbor Treaty"), and no number
//!   ("about 40 hectares of farmland" is "farmland" plus an `area` = "about 40 hectares").
//!   No id.
//! - `s`: `[quote, subject name, phrase, object name or null, literal value or null, qualifiers object or null, when or null,
//!   ended or null]`. The quote goes in the first cell: the model copies the sentence first, then writes the statement from it (quote first, then restate). The subject and object are names in `e` (or the known list in the prompt), spelled exactly
//!   as written. Exactly one of object and literal value is set (the parser does not decide; both are passed to the caller). When more than two parties take part in a sentence,
//!   or the link carries an amount, title, condition or comparison, the main pair goes into subject and object, the rest into qualifiers, keyed by
//!   one or two words from the source that state their role, and valued by the names of the things listed or the source's words. when/ended are copied from the source's words,
//!   never computed. The quote is the sentence that states it, copied verbatim.
//! - `n`: `[name listed in e, another name used in this passage, quote]`. Only names the source actually writes are collected.
//!
//! Whether subject and object names match `e` (or the known list) is **not checked here**; it is left to the server, which has
//! the known entity handles and the library's names; here there are only words.
//!
//! Parsing is lenient per item: a bad record counts toward [`OpenExtraction::skipped`] and does not ruin the block; the caller
//! **must** report that count; not reporting it is a silent drop (the kind of bug in #108).

use std::collections::HashSet;

use serde_json::Value;
use utopia_llm::ChatMessage;

use crate::{
    close_brackets, json_block, json_text, opening_block, KnownEntity, KNOWN_BUDGET_CHARS,
};

#[derive(Debug, Clone)]
pub struct OpenEntity {
    /// the name the statements use for it, exactly as listed
    pub name: String,
    /// What the source says it is, in two or three words. Not validated, not added to the ontology
    pub kind: String,
    /// Whether the source names it (true) or only describes it (false)
    pub named: bool,
    /// where it stood in the reply's `e` array; a malformed or repeated neighbour is dropped
    pub raw_index: usize,
}

#[derive(Debug, Clone)]
pub struct OpenStatement {
    /// the name of a thing, as listed in `e` or in the known-things list; not resolved here
    pub subject: String,
    /// the relation phrase as written, lowercase short phrase
    pub phrase: String,
    /// entity object by name, or
    pub object: Option<String>,
    /// literal value as written (figure with units, title, status word, date words)
    pub value: Option<String>,
    /// keyed by the document's role word ("to", "amount", "title", "compared to"); the value is
    /// text, which may be the name of a listed thing. The order is serde_json's map order
    /// (alphabetical by key) and carries no meaning
    pub qualifiers: Vec<(String, String)>,
    /// the words that say when it holds, happened or began, exactly as written
    pub when: Option<String>,
    /// the words that say when it stopped, exactly as written
    pub ended: Option<String>,
    /// the sentence of the passage that states it, verbatim
    pub quote: Option<String>,
    /// where it stood in the reply's `s` array; a malformed neighbour is dropped, so this is not
    /// its position in `OpenExtraction::statements`
    pub raw_index: usize,
}

#[derive(Debug, Clone)]
pub struct OpenName {
    /// the thing's name as listed in `e` or in the known-things list
    pub entity: String,
    pub name: String,
    pub quote: Option<String>,
}

#[derive(Debug, Default)]
pub struct OpenExtraction {
    pub entities: Vec<OpenEntity>,
    pub statements: Vec<OpenStatement>,
    pub names: Vec<OpenName>,
    /// what the passage states about the document's own time (0064): dates with their names
    pub time: Vec<crate::time::TimeEntry>,
    /// items skipped because they were malformed (counted per array item; must be reported by the caller)
    pub skipped: usize,
    /// the reply was not valid JSON as a whole and was read item by item: cut off at the
    /// end, or miswritten somewhere (the caller tells which by whether the reply hit the
    /// token ceiling)
    pub truncated: bool,
}

/// System message. Rules 1–8 are wording tested on the prototype, rewritten as compact arrays; the examples are neutral and come from no
/// measured corpus. There is **no** ontology, no relation list, no document date: the whole point of this path lies in these three "no"s
const OPEN_SYSTEM: &str = "\
You read one passage of a document and write down what it states, in the passage's own words. \
Output one JSON object and nothing else, shaped like this:\n\
\n\
{\"e\": [[\"name or description\", \"kind word\", 1]],\n\
 \"s\": [[\"the sentence that states it, verbatim\", \"subject name\", \"relation phrase as written\", \"object name\", null, {\"to\": \"name of a listed thing\", \"amount\": \"$2 million\"}, \"when it holds or happened, as written\", \"when it ended, as written\"]],\n\
 \"n\": [[\"name as listed in e\", \"another name\", \"the sentence that uses it, verbatim\"]],\n\
 \"t\": [[\"now\", \"label as written\", \"the date as written\", {\"y\": 2011, \"m\": 3, \"d\": 4}, null]]}\n\
\n\
1. \"e\" lists the things the passage talks about, one entry each: [name, kind, named]. Every \
thing a statement refers to is listed here once, under exactly the name the statements use. \
named = 1 for a proper name or a fixed term: the name of one particular person, organization, \
product, place, document, law or event, or a term that means the same thing in any document (a \
disease, a drug, an industry, a product category, an indicator), written as the passage writes \
it, without citation words (\"the Harbor Treaty signed on May 3, 1998\" is named \"Harbor \
Treaty\"). named = 0 for a role or a generic phrase whose referent this passage decides, however \
particular it is here: \"the company\", \"the enterprises visited\", \"patients\", \"各部门\", \
\"the northern wing\" are described things, named by the words that say what they are. kind is \
what the thing is, in two or three words, as the passage says it. A name never carries a figure: \"about 40 hectares of farmland\" is the thing \
\"farmland\" with a statement \"area\" = \"about 40 hectares\". List each thing once.\n\
2. \"s\" lists the statements, one entry each: [quote, subject, phrase, object, value, \
qualifiers, when, ended]. quote comes first: the sentence of the passage that states it, copied \
verbatim, one sentence, and when two sentences are needed, the one that carries the link; the \
rest of the entry is written from that sentence. subject and object are names, written exactly \
as listed in \"e\" or in the list of things already recorded, with the same spelling every time. Exactly one of object \
and value is set; the other is null.\n\
   A statement with an object links two things. phrase is how the passage says the link, \
lowercase, in the passage's own words; do not translate it into any vocabulary of your own. It \
is the verb with the words that belong to it, so that subject —phrase→ object reads as a \
sentence on its own (\"acquired\", \"was designed by\", \"is the captain of\", \"went into \
partnership with\", \"细化解读\"), never a bare verb cut from a longer verb phrase (\"went\", \
\"细化\"). Several verbs sharing one object make one statement whose phrase carries them all \
(\"designed and built\"); one verb with several objects makes one statement per object. The \
object is what the verb acts on, and a described thing that is the subject or the object is \
listed like any other (\"the coating should not be used on surfaces exposed to seawater\" gives \
coating —should not be used on→ surfaces exposed to seawater, a described thing); words that say \
where, how, why, with what or for whom go in qualifiers under the passage's own role word. A \
named thing mentioned in a qualifier is still listed and the qualifier names it; a generic phrase \
that appears in a qualifier alone is words there, not a listed thing: \"各部门要走进学校、社区等\
基层单位开展宣传活动\" gives 各部门 —开展→ 宣传活动 with {\"走进\": \"学校、社区等基层单位\"}, \
and neither 学校 nor 社区 is listed.\n\
   A statement with a value is what the passage says about one thing by itself: a figure with \
its units, a percentage, an amount, a count, a title, a status. phrase is how the passage says \
what is measured or stated (\"area\", \"was completed\", \"joined\", \"占地面积\"); value is the \
literal as written. Wording that names or describes something is not a value: that something \
goes in \"e\" and is linked by a statement with an object. \"1,200 beekeepers joined\" is the \
thing \"beekeepers\" with \"joined\" = \"1,200\". Write every figure the passage states.{TABLE}\n\
   A list is one statement per member, and so is a subject or object that joins several things \
(\"the city and the county funded the bridge\" is two statements).\n\
3. Nothing in a sentence is dropped. When more than two things take part, or the link carries \
a figure, a title, a condition or a comparison, put the main pair in subject and object and the \
rest in qualifiers: an object keyed by a word or two for each part's role, whose values are the \
name of a listed thing or the words as written. \"the city council awarded the paving contract \
to Brightway Builders for $2 million\" gives city council —awarded→ paving contract with \
{\"to\": \"Brightway Builders\", \"amount\": \"$2 million\"}; \"Jane Doe is interim headmaster \
of Hillside School\" gives Jane Doe —is headmaster of→ Hillside School with {\"title\": \
\"interim headmaster\"}. A statement with a value takes qualifiers the same way: \"the plant \
cut water use by 12% compared to 2019\" gives the plant \"cut water use\" = \"12%\" with \
{\"compared to\": \"2019\"}. A statement the passage does not assert as holding but requires, \
plans, expects, forecasts or makes conditional carries a qualifier keyed \"mood\" whose value \
is the passage's own words for that (\"要\", \"should\", \"will\", \"is expected to\", \"if the \
merger closes\"); the phrase stays as written. A verb that reports (announced, said, revealed) \
is not a mood: what was announced is stated. qualifiers is null when there are none.\n\
4. When a description has another thing folded into it, also write the statement that unfolds \
it: \"hospitals accredited by the Joint Commission\" also gives Joint Commission —accredited→ \
hospitals. Do not unfold a description that only names what it belongs to or is about: \"the \
northern wing of the palace\" and \"the use of the coating\" give no statement, the palace and \
the coating are part of the description.\n\
5. when is the words that say when the statement holds, happened or began, and ended is the \
words that say when it stopped, each copied exactly as written (\"March 4, 2011\", \"去年冬天\", \
\"by the end of next season\"). Never compute, convert or normalise a date, and never write one \
the passage does not. Each is null when the passage gives none.\n\
   Words that say when go in when or ended and nowhere else, never in the phrase, the \
object, the value or a qualifier; also when they count from now or from another event \
(\"三个月后\", \"last week\", \"six months after the launch\") and when the statement is \
planned. If that leaves no value, the value is the word for what happened (\"发布\"). A \
statement with no time of its own, under a heading that names one time and nothing else \
(\"## 2025年第三季度\"), takes the heading's words as when.\n\
6. Every \"s\" and \"n\" entry carries its own quote, copied verbatim from the passage: the first \
slot of an \"s\" entry, the last slot of an \"n\" entry.\n\
7. \"n\" lists other names, one entry each: [name as listed, other name, quote] — a short form, \
a former name, a spelling in another script that this passage uses for a thing in \"e\" or a \
thing already recorded. Only names actually written in the passage; never a pronoun or a \
description.\n\
8. \"t\" lists the dates the passage states about the document itself, one entry each: \
[kind, label, words, from, to]. kind is \"now\" for the moment the text speaks from (a \
dateline, the date a report was submitted or published, a meeting's date), \"date\" for any \
other labelled date (a cut-off, an effective date, a deadline), \"period\" for a named period \
with stated bounds. label and words are copied as written; from and to transcribe the numbers \
the words state (y, m, d), to only for a period. A date inside a sentence about something that \
happened is that statement's when and is not listed. [] when there are none.\n\
9. State nothing the passage does not state, and state each thing once: with a value or with \
an object, not both. The Document line, the opening of the document and the list of things \
already recorded only say where the passage comes from; write nothing about them. If the \
passage states nothing, output {\"e\":[],\"s\":[],\"n\":[],\"t\":[]}.\n\
\n\
Example. The passage \"The city council awarded the paving contract to Brightway Builders for \
$2 million on March 4, 2011.\" gives:\n\
{\"e\": [[\"city council\", \"council\", 0], [\"paving contract\", \"contract\", 0], [\"Brightway Builders\", \"builders\", 1]],\n\
 \"s\": [[\"The city council awarded the paving contract to Brightway Builders for $2 million on March 4, 2011.\", \"city council\", \"awarded\", \"paving contract\", null, {\"to\": \"Brightway Builders\", \"amount\": \"$2 million\"}, \"March 4, 2011\", null]],\n\
 \"n\": [],\n\
 \"t\": []}";

/// The table-reading rule is included in the system message only when this passage has a table (see [`has_table`]): a passage without a table
/// does not need it, and it takes up less than a tenth of the system message
const OPEN_TABLE_RULES: &str = "\
 In a \
table, a cell is a statement about its row's thing whose phrase is the column heading and \
whose value is the cell as written; a cell is the object only when it names a thing of its own \
(a person, an organization, a place). When \
the column heading names one time (a date, a quarter, a period), it is when instead, the \
statement is about the thing the table's caption names (the company of a financial statement) \
and the phrase is the row label as written, path included (\"Operating expenses › Research and \
development\"); a heading that names a comparison between periods (a change from the quarter \
before, a change from a year ago) is not a time, it stays the phrase; a unit the caption or a \
heading gives (\"In millions\") is a qualifier on each such statement. Otherwise a row is about \
the thing its first column (or its name, title or ID column) names, not the table or the \
sheet. Leave out cells whose column heading is not in the passage.";

/// Whether this passage has a table: Markdown pipes, HTML cells, tabs, or three or more columns separated by runs of spaces
/// on one line. Better to over-detect: a false positive only adds an extra rule paragraph, as before
fn has_table(passage: &str) -> bool {
    let lower = passage.to_ascii_lowercase();
    passage.contains('|')
        || passage.contains('\t')
        || lower.contains("<td")
        || lower.contains("<th")
        || lower.contains("<tr")
        || passage.lines().any(|l| {
            l.trim()
                .split("  ")
                .filter(|c| !c.trim().is_empty())
                .count()
                >= 3
        })
}

/// System message for this passage: the rules are unchanged; only the table sentences are included or left out per passage
fn system_for(passage: &str) -> String {
    let table = if has_table(passage) {
        format!(" {OPEN_TABLE_RULES}")
    } else {
        String::new()
    };
    OPEN_SYSTEM.replace("{TABLE}", &table)
}

/// Build the two open-extraction messages: constant system message + `Document:` / opening / known entities / `Passage:`.
///
/// No document date, no type or relation list, no attributes: none of these go into the prompt. The opening and known entities
/// only say where this passage comes from (the model is told not to extract from them).
pub fn build_open_messages(
    filename: &str,
    known: &[KnownEntity],
    opening: Option<&str>,
    chunk_text: &str,
) -> Vec<ChatMessage> {
    // Known entities sit right next to the body: compliance relies on position; see the known_block comment in lib.rs for why
    let user = format!(
        "Document: {filename}\n{}{}\nPassage:\n{chunk_text}",
        opening_block(opening),
        known_block(known)
    );
    vec![
        ChatMessage {
            role: "system".into(),
            content: system_for(chunk_text),
        },
        ChatMessage {
            role: "user".into(),
            content: user,
        },
    ]
}

/// The open version of "entities this document has already recognized": `- <name> (<kind>)`; handles are not shown to the model. This path
/// statements write names, and the model writes them with the spelling shown here; the server then looks up which entity the name refers to.
/// The instructions in lib.rs's version discuss subject_ref/object_ref and "give it the same type", which are fields of the path that carries an ontology; there are none here.
/// Budget and truncation rules are the same as that version (handles are not shown, so they do not count)
fn known_block(known: &[KnownEntity]) -> String {
    if known.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    let mut used = 0usize;
    for entity in known {
        used += entity.type_key.chars().count() + entity.name.chars().count() + 6;
        if used > KNOWN_BUDGET_CHARS {
            break;
        }
        let kind = entity.type_key.trim();
        if kind.is_empty() {
            lines.push(format!("- {}", entity.name));
        } else {
            lines.push(format!("- {} ({kind})", entity.name));
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "\nAlready recorded from earlier parts of this same document:\n{}\n\
         \n\
         When the passage refers to one of these, write its name exactly as it is listed here \
         and do not list it again in \"e\"; a shortened form the passage uses for it goes in \
         \"n\". If it is a different thing, list it as the passage names it; do not force it \
         onto this list.\n",
        lines.join("\n")
    )
}

/// A truncated reply is taken from the first `{` to the **end**, and the repair step itself backs up to the last complete entry.
///
/// `json_block` takes the first `{` to the last `}`, which cuts wrongly twice for a compact reply: `}` appears only in
/// qualifier objects and at the end, and a truncated reply either has no `}` at all (it reports "no JSON", and repair
/// never gets a chance to run), or its last `}` is the qualifier of some statement midway (which cuts off the complete entries after it).
/// Code fences and thinking text are split off the same way.
fn json_tail(raw: &str) -> Option<&str> {
    let text = json_text(raw);
    text.find('{').map(|s| &text[s..])
}

/// Repair of a truncated compact reply: back up to the end of the **last complete array or object** and close the brackets.
///
/// lib.rs's `repair_truncated` accepts only `}` as the fallback point, since there every record is an object ending in `}`.
/// Here each record is an array, and a reply where all qualifiers are null may have no `}` at all; accepting only `}`
/// would void the whole block. So the fallback point is the last `]` or `}`, whichever is later; whether a bracket is inside a string
/// is decided by `close_brackets`
pub(crate) fn repair_truncated_compact(json: &str) -> Option<String> {
    let mut cut = json.len();
    for _ in 0..64 {
        let idx = json[..cut].rfind([']', '}'])?;
        if let Some(closed) = close_brackets(&json[..=idx]) {
            if serde_json::from_str::<Value>(&closed).is_ok() {
                return Some(closed);
            }
        }
        cut = idx;
    }
    None
}

/// Read an invalid reply entry by entry (only when the whole cannot be parsed). Returns `(entries by kind, count of bad entries written)`; if
/// no entry can be read, it is None.
///
/// Previously, if the whole could not be parsed, the reply was treated as truncated: back up to the last place where brackets could be closed, and drop everything after. But what the model writes wrongly
/// is usually not at the end: `e` was not closed and `s` started, or `s` is missing a `]` at its end. The reply ends normally, but the damage is
/// in the middle. Damage at one-third means the remaining two-thirds (a dozen-plus statements, aliases, the document's dates) are all lost, while
/// the block is still recorded as extracted (bench README, 2026-09-28).
///
/// Whole-reply validity is not required here: walk the text, and a `"e"`, `"s"`, `"n"` or `"t"` followed by `: [` marks the start of a
/// kind; each `[` is read on its own up to its matching bracket. An array that parses, whose first cell is not an array, and which lacks
/// a cell count after which no array appears, is one entry. An array within the cell count means one cell has the wrong type: as when the whole reads fine,
/// only that cell is void and the entry is still kept (`parse_entity` etc.). An array appearing after the cell count means this entry is missing its `]` and
/// has swallowed the following entries; it is broken, so search from its next character on, and the complete entries nested inside are still readable.
/// An array whose first cell is an array cannot be told apart as a list of entries or a broken entry, so it is read too. An eight-cell entry written in `e`
/// is a statement and is filed under `s`. For a truly truncated reply, the last entry does not balance and is naturally not kept, as before
fn salvage(text: &str) -> Option<(Value, usize)> {
    let bytes = text.as_bytes();
    // End of the string (`i` is on the opening quote): return the position after the closing quote; no end means None
    let string_end = |mut i: usize| -> Option<usize> {
        i += 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => return Some(i + 1),
                _ => i += 1,
            }
        }
        None
    };
    // Position of the bracket matching the `[` at `i`
    let balanced_end = |start: usize| -> Option<usize> {
        let (mut i, mut depth) = (start, 0usize);
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    i = string_end(i)?;
                    continue;
                }
                b'[' | b'{' => depth += 1,
                b']' | b'}' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
    };
    let mut found: [Vec<Value>; 4] = Default::default();
    let mut section: Option<usize> = None;
    let mut broken = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let Some(end) = string_end(i) else { break };
                let key = ["e", "s", "n", "t"]
                    .iter()
                    .position(|k| &text[i + 1..end - 1] == *k);
                let rest = text[end..].trim_start();
                let after_colon = rest.strip_prefix(':').map(str::trim_start);
                match (key, after_colon) {
                    (Some(k), Some(r)) if r.starts_with('[') => {
                        section = Some(k);
                        i = text.len() - r.len() + 1;
                    }
                    _ => i = end,
                }
            }
            b'[' => {
                let item = balanced_end(i).and_then(|end| {
                    let v = serde_json::from_str::<Value>(&text[i..=end]).ok()?;
                    let arr = v.as_array()?;
                    let own = section.map_or(0, |k| slots(k, arr));
                    let one_item = arr.first().is_some_and(|first| !first.is_array())
                        && arr.iter().skip(own).all(|x| !x.is_array());
                    one_item.then_some((end, v))
                });
                match (item, section) {
                    (Some((end, v)), Some(k)) => {
                        let is_statement =
                            k == 0 && v.as_array().is_some_and(|a| statement_shaped(a));
                        found[if is_statement { 1 } else { k }].push(v);
                        i = end + 1;
                    }
                    (Some((end, _)), None) => i = end + 1,
                    (None, _) => {
                        // Only counts as broken if it balances but fails to parse (or nests other entries); unbalanced means the ending was truncated
                        if section.is_some() && balanced_end(i).is_some() {
                            broken += 1;
                        }
                        i += 1;
                    }
                }
            }
            _ => i += 1,
        }
    }
    if found.iter().all(Vec::is_empty) {
        return None;
    }
    let [e, s, n, t] = found;
    Some((
        serde_json::json!({ "e": e, "s": s, "n": n, "t": t }),
        broken,
    ))
}

/// Cells per entry: `e` three (a statement written in `e`: eight), `s` eight, `n` three, `t` five.
/// Used when reading entry by entry to tell "one cell has the wrong type" apart from "this entry swallowed the following entries" (see `salvage`)
fn slots(section: usize, item: &[Value]) -> usize {
    match section {
        0 if statement_shaped(item) => 8,
        1 => 8,
        3 => 5,
        _ => 3,
    }
}

/// Statement shape: at least eight cells, the first three are strings (quote, subject, phrase)
fn statement_shaped(item: &[Value]) -> bool {
    item.len() >= 8 && item[..3].iter().all(Value::is_string)
}

/// Array under a top-level key; missing or not an array counts as empty: that is not a bad record, it means the whole section has no entries of this kind
fn items<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Non-empty string in cell i (trimmed of leading and trailing whitespace): used for cells that must not be missing
fn text_at(arr: &[Value], i: usize) -> Option<&str> {
    arr.get(i)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Optional text cell: string trimmed; null, empty string, and wrong type are None. A cell with the wrong type
/// does not make the whole record bad, only that cell
fn opt_text(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Literal value as is: string trimmed; the model sometimes writes a number as a JSON number, which is turned back into text as written.
/// Qualifier values follow the same rule
fn literal(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        v => opt_text(v),
    }
}

/// Qualifiers: those values in the object that are strings or numbers; keys trimmed; empty keys and other types are dropped silently;
/// a non-object counts as no qualifiers
fn qualifiers(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let k = k.trim();
                    (!k.is_empty()).then(|| Some((k.to_string(), literal(v)?)))?
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Dedup key: leading and trailing whitespace removed, internal whitespace collapsed to one, lowercased. "Harbor  Bridge" and "harbor bridge"
/// are the same
fn name_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// `[name, kind, named]`: name cannot be missing; a missing kind is an empty string; a missing named counts as named
fn parse_entity(arr: &[Value]) -> Option<OpenEntity> {
    let name = text_at(arr, 0)?.to_string();
    let kind = arr
        .get(1)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let named = match arr.get(2) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        _ => true,
    };
    Some(OpenEntity {
        name,
        kind,
        named,
        raw_index: 0,
    })
}

/// `[quote, subject, phrase, object, value, qualifiers, when, ended]`. All eight cells must be present: a short one
/// is usually a half entry cut from a truncation repair, with no quote, and cannot be verified, so it is not kept. Subject and phrase cannot be missing; other cells
/// with the wrong type are None for that cell only. If object and value are both present, both are kept for the caller
fn parse_statement(arr: &[Value]) -> Option<OpenStatement> {
    if arr.len() < 8 {
        return None;
    }
    let subject = text_at(arr, 1)?.to_string();
    let phrase = text_at(arr, 2)?.to_string();
    Some(OpenStatement {
        subject,
        phrase,
        object: opt_text(&arr[3]),
        value: literal(&arr[4]),
        qualifiers: qualifiers(&arr[5]),
        when: opt_text(&arr[6]),
        ended: opt_text(&arr[7]),
        quote: opt_text(&arr[0]),
        raw_index: 0,
    })
}

/// `[entity name, other name, quote]`
fn parse_name(arr: &[Value]) -> Option<OpenName> {
    let entity = text_at(arr, 0)?.to_string();
    let name = text_at(arr, 1)?.to_string();
    let quote = arr.get(2).and_then(opt_text);
    Some(OpenName {
        entity,
        name,
        quote,
    })
}

/// **One bad record must not ruin a whole block** (same as `parse_response` in lib.rs).
///
/// First take the JSON block and parse it into `Value` (if truncated, back up to the last complete entry, close the brackets, and mark `truncated`),
/// then parse item by item leniently: entries of the wrong shape are counted in `skipped`, good ones kept. Duplicate names in `e` (ignoring whitespace
/// and case) keep the first; the rest count in `skipped`. Subject and object names are not matched back to `e` here
pub fn parse_open_response(raw: &str) -> anyhow::Result<OpenExtraction> {
    // First take the block normally (from the first `{` to the last `}`): if it parses, it is a complete reply, and trailing
    // chatter after the end is not truncation. Only when it fails to parse, take from the first `{` to the end and read entry by entry (see `salvage`)
    let block = json_block(raw)
        .and_then(|b| serde_json::from_str::<Value>(&b).map_err(anyhow::Error::from));
    let mut skipped = 0usize;
    let (value, truncated) = match block {
        Ok(v) => (v, false),
        Err(e) => {
            let (v, broken) = json_tail(raw)
                .and_then(salvage)
                // Only if not a single entry can be read is it a real parse failure
                .ok_or_else(|| anyhow::anyhow!("Failed to parse open extraction JSON: {e}"))?;
            skipped = broken;
            (v, true)
        }
    };

    let mut out = OpenExtraction {
        truncated,
        skipped,
        ..Default::default()
    };
    let mut seen = HashSet::new();
    for (raw_index, item) in items(&value, "e").iter().enumerate() {
        match item.as_array().and_then(|a| parse_entity(a)) {
            Some(e) if seen.insert(name_key(&e.name)) => {
                out.entities.push(OpenEntity { raw_index, ..e })
            }
            _ => out.skipped += 1,
        }
    }
    for (raw_index, item) in items(&value, "s").iter().enumerate() {
        match item.as_array().and_then(|a| parse_statement(a)) {
            Some(s) => out.statements.push(OpenStatement { raw_index, ..s }),
            None => out.skipped += 1,
        }
    }
    for item in items(&value, "n") {
        match item.as_array().and_then(|a| parse_name(a)) {
            Some(n) => out.names.push(n),
            None => out.skipped += 1,
        }
    }
    for item in items(&value, "t") {
        match crate::time::time_entry(item) {
            Some(t) => out.time.push(t),
            None => out.skipped += 1,
        }
    }
    Ok(out)
}

/// An object written as a value is moved back to the value; returns the objects that were moved, and the caller records each one (`object_is_value`).
///
/// Rules 2 and the table rule say values go into value, but the model still writes "established on 2015-03-12", "registered capital of
/// 50 million yuan", and "pending review" from a table into the object. The signature then becomes a relation between two things, matching no
/// attribute. This only moves cells; not a single character of the source is changed. After moving, a "thing" that no statement points to
/// and that is not an alias is removed from `e`, so no entity called "2015-03-12" gets created.
///
/// Used only for the model's reply: statements pushed in are the client's contract, and what it writes as an object stays an object
pub fn values_are_not_things(x: &mut OpenExtraction) -> Vec<String> {
    let listed: std::collections::HashMap<String, (bool, String)> = x
        .entities
        .iter()
        .map(|e| (name_key(&e.name), (e.named, name_key(&e.kind))))
        .collect();
    let mut moved = Vec::new();
    for s in &mut x.statements {
        let (Some(object), None) = (s.object.as_deref(), s.value.as_deref()) else {
            continue;
        };
        let (named, kind) = listed
            .get(&name_key(object))
            .map_or((false, ""), |(n, k)| (*n, k.as_str()));
        // A cell in a table: the kind word is the column name, and it is written as an object whose phrase is the column name; the model itself says this is the value
        // of that column. A named thing counts only when the name contains a digit (number, model); a named thing seen only in table rows, in prose
        // "Model 3" remains a thing
        let a_cell = kind == name_key(&s.phrase)
            && !kind.is_empty()
            && s.quote.as_deref().is_some_and(has_table)
            && (!named || object.chars().any(|c| c.is_ascii_digit()));
        if a_cell || crate::literal_object(object, named) {
            moved.extend(s.object.take());
            s.value = moved.last().cloned();
        }
    }
    let used: HashSet<String> = x
        .statements
        .iter()
        .flat_map(|s| [Some(&s.subject), s.object.as_ref()])
        .flatten()
        .chain(x.names.iter().flat_map(|n| [&n.entity, &n.name]))
        .map(|n| name_key(n))
        .collect();
    let moved_keys: HashSet<String> = moved.iter().map(|n| name_key(n)).collect();
    x.entities.retain(|e| {
        let key = name_key(&e.name);
        used.contains(&key) || !moved_keys.contains(&key)
    });
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    const AWARD: &str =
        "The city council awarded the paving contract to Brightway Builders for $2 million on March 4, 2011.";
    const LEASE: &str = "Nebula (formerly Starlight Labs) leased the northern wing from Harbor Estates from 2010 until the end of 2019.";

    /// A complete compact reply: the names listed in this block and in the known list, two kinds of qualifiers, start and end times, aliases
    const FULL: &str = r#"{"e": [["city council", "council", 0], ["paving contract", "contract", 0],
           ["Brightway Builders", "builders", 1], ["northern wing", "wing", 0]],
     "s": [["The city council awarded the paving contract to Brightway Builders for $2 million on March 4, 2011.",
            "city council", "awarded", "paving contract", null, {"to": "Brightway Builders", "amount": "$2 million"}, "March 4, 2011", null],
           ["Nebula (formerly Starlight Labs) leased the northern wing from Harbor Estates from 2010 until the end of 2019.",
            "Nebula Technologies Inc.", "leased", "northern wing", null, {"from": "Harbor Estates", "term": "ten years"}, "from 2010", "until the end of 2019"],
           ["The city council awarded the paving contract to Brightway Builders for $2 million on March 4, 2011.",
            "paving contract", "worth", null, "$2 million", null, null, null]],
     "n": [["Nebula Technologies Inc.", "Starlight Labs", "Nebula (formerly Starlight Labs) leased the northern wing from Harbor Estates from 2010 until the end of 2019."]]}"#;

    /// serde_json objects sort keys (preserve_order is off); qualifier order carries no meaning
    fn by_key(mut q: Vec<(String, String)>) -> Vec<(String, String)> {
        q.sort_by(|a, b| a.0.cmp(&b.0));
        q
    }

    fn pairs(q: &[(&str, &str)]) -> Vec<(String, String)> {
        by_key(
            q.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn a_full_reply_parses_into_the_structs() {
        let x = parse_open_response(FULL).unwrap();
        assert!(!x.truncated);
        assert_eq!(x.skipped, 0);

        assert_eq!(x.entities.len(), 4);
        assert_eq!(x.entities[0].name, "city council");
        assert_eq!(x.entities[0].kind, "council");
        assert!(!x.entities[0].named);
        assert_eq!(x.entities[2].name, "Brightway Builders");
        assert!(x.entities[2].named);

        assert_eq!(x.statements.len(), 3);
        let s = &x.statements[0];
        assert_eq!(s.subject, "city council");
        assert_eq!(s.phrase, "awarded");
        assert_eq!(s.object.as_deref(), Some("paving contract"));
        assert_eq!(s.value, None);
        assert_eq!(
            by_key(s.qualifiers.clone()),
            pairs(&[("to", "Brightway Builders"), ("amount", "$2 million")])
        );
        assert_eq!(s.when.as_deref(), Some("March 4, 2011"));
        assert_eq!(s.ended, None);
        assert_eq!(s.quote.as_deref(), Some(AWARD));

        let s = &x.statements[1];
        assert_eq!(
            s.subject, "Nebula Technologies Inc.",
            "已知清单里的名字照字面收"
        );
        assert_eq!(s.object.as_deref(), Some("northern wing"));
        assert_eq!(
            by_key(s.qualifiers.clone()),
            pairs(&[("from", "Harbor Estates"), ("term", "ten years")])
        );
        assert_eq!(s.when.as_deref(), Some("from 2010"));
        assert_eq!(s.ended.as_deref(), Some("until the end of 2019"));
        assert_eq!(s.quote.as_deref(), Some(LEASE));

        let s = &x.statements[2];
        assert_eq!(s.object, None);
        assert_eq!(s.value.as_deref(), Some("$2 million"));
        assert!(s.qualifiers.is_empty());
        assert_eq!(s.when, None);
        assert_eq!(s.ended, None);
        assert_eq!(s.quote.as_deref(), Some(AWARD));

        assert_eq!(x.names.len(), 1);
        assert_eq!(x.names[0].entity, "Nebula Technologies Inc.");
        assert_eq!(x.names[0].name, "Starlight Labs");
        assert_eq!(x.names[0].quote.as_deref(), Some(LEASE));
    }

    /// Cut in the middle of `n`: everything before `s` is kept, `truncated` marked
    #[test]
    fn a_cut_off_reply_keeps_what_was_complete() {
        let marker = "\"n\": [[\"Nebula Technologies Inc.\", \"Starl";
        let cut = FULL.find(marker).unwrap() + marker.len();
        let x = parse_open_response(&FULL[..cut]).unwrap();
        assert!(x.truncated, "截断要标出来");
        assert_eq!(x.entities.len(), 4);
        assert_eq!(x.statements.len(), 3);
        assert_eq!(x.statements[2].quote.as_deref(), Some(AWARD));
        assert!(x.names.is_empty());
        assert_eq!(x.skipped, 0);
    }

    /// Cut after a statement's qualifiers: that half entry does not balance, and is not kept as a statement with only a quote. It is not counted as a bad entry
    /// as bad: it is missing only because the reply was truncated, which `truncated` already says
    #[test]
    fn a_half_statement_at_the_cut_is_not_kept() {
        let raw = r#"{"e": [["A", "thing", 1]],
            "s": [["A is b.", "A", "is", null, "b", null, null, null], ["A was c.", "A", "was", null, "c", {"at": "home"}, "in 20"#;
        let x = parse_open_response(raw).unwrap();
        assert!(x.truncated);
        assert_eq!(x.statements.len(), 1);
        assert_eq!(x.skipped, 0);
    }

    /// `e` not closed and `s` started (the reply ends normally, damage at one-third): before, it backed up to before the damage and everything after
    /// (statements, aliases) was lost, leaving only the one entry written into `e`. Read entry by entry, none are missing
    #[test]
    fn statements_written_after_an_unclosed_list_are_all_read() {
        let raw = r#"```json
{"e": [["Harbor Bakery", "bakery", 1], ["Hillside School", "school", 1], ["bread", "bread", 0],
["Harbor Bakery supplies Hillside School.", "Harbor Bakery", "supplies", "Hillside School", null, null, null, null],
"s": [["Harbor Bakery bakes bread [daily].", "Harbor Bakery", "bakes", "bread", null, {"how often": "daily"}, null, null],
["Harbor Bakery opened in 2005.", "Harbor Bakery", "opened", null, "opened", null, "2005", null]],
"n": [["Harbor Bakery", "the Bakery", "The Bakery opened in 2005."]],
"t": []}
```"#;
        let x = parse_open_response(raw).unwrap();
        assert!(x.truncated, "读是读出来了，回复不合法要标出来");
        assert_eq!(x.entities.len(), 3);
        assert_eq!(
            x.statements
                .iter()
                .map(|s| s.phrase.as_str())
                .collect::<Vec<_>>(),
            ["supplies", "bakes", "opened"],
            "写在 e 里的那条八格条目是陈述"
        );
        assert_eq!(
            x.statements[1].quote.as_deref(),
            Some("Harbor Bakery bakes bread [daily].")
        );
        assert_eq!(x.names.len(), 1);
        assert_eq!(x.skipped, 0);
    }

    /// `s` missing one `]` at its end: the statements all remain; before, what was lost was `n` and `t` after it, the document's dates
    /// were gone, and in this passage the time references counting "today" had nothing to count with
    #[test]
    fn what_follows_a_miswritten_bracket_is_still_read() {
        let raw = r#"{"e": [["码表", "产品", 1]],
"s": [["码表平均售价为849元。", "码表", "平均售价", null, "849元", null, "2025年第四季度", null]}
,
"n": [],
"t": [["now", "提报日期", "2026年8月28日", {"y": 2026, "m": 8, "d": 28}, null]]}"#;
        let x = parse_open_response(raw).unwrap();
        assert!(x.truncated);
        assert_eq!(x.statements.len(), 1);
        assert_eq!(x.time.len(), 1);
        assert_eq!(x.skipped, 0);
    }

    /// One broken entry (object missing `}`) loses only that entry, counted; the ones before and after are kept
    #[test]
    fn one_miswritten_item_costs_only_itself() {
        let raw = r#"{"e": [["A", "thing", 1]],
"s": [["A is b.", "A", "is", null, "b", null, null, null],
["A was c.", "A", "was", null, "c", {"at": "home"], "in 2019", null],
["A has d.", "A", "has", null, "d", null, null, null]],
"n": [], "t": []}"#;
        let x = parse_open_response(raw).unwrap();
        assert!(x.truncated);
        assert_eq!(
            x.statements
                .iter()
                .map(|s| s.phrase.as_str())
                .collect::<Vec<_>>(),
            ["is", "has"]
        );
        assert_eq!(x.skipped, 1);
    }

    /// An entry missing its closing `]` swallowed the next one: a block nesting other entries is not an entry; the one inside is kept
    #[test]
    fn an_item_that_swallowed_the_next_one_gives_the_next_one_back() {
        let raw = r#"{"e": [["A", "thing", 1]],
"s": [["A is b.", "A", "is", null, "b", null, null, null,
["A has d.", "A", "has", null, "d", null, null, null]],
"n": [], "t": []}"#;
        let x = parse_open_response(raw).unwrap();
        assert_eq!(
            x.statements
                .iter()
                .map(|s| s.phrase.as_str())
                .collect::<Vec<_>>(),
            ["has"]
        );
        assert_eq!(x.skipped, 1);
    }

    /// A cell with the wrong type (kind written as an array in an entity, qualifiers written as an array of pairs) loses only that cell, as when
    /// the whole reads fine. Entry-by-entry reading used to drop the whole entry and read the array in that cell as an entry: the kind
    /// `["company", "org"]` was read as an entity named company, and Acme disappeared
    #[test]
    fn a_slot_of_the_wrong_type_costs_only_that_slot_when_read_item_by_item() {
        // `e` missing its closing `]`: the whole does not parse, read entry by entry
        let raw = r#"{"e": [["Acme", ["company", "org"], 1], ["Brightway", "builder", 1],
"s": [["Lin joined Acme as CEO.", "Lin", "joined", "Acme", null, [["role", "CEO"]], null, null],
["Lin left Acme.", "Lin", "left", "Acme", null, null, null, null]],
"n": [], "t": []}"#;
        let x = parse_open_response(raw).unwrap();
        assert!(x.truncated);
        assert_eq!(
            x.entities
                .iter()
                .map(|e| (e.name.as_str(), e.kind.as_str()))
                .collect::<Vec<_>>(),
            [("Acme", ""), ("Brightway", "builder")],
            "no entity is read out of a slot"
        );
        assert_eq!(
            x.statements
                .iter()
                .map(|s| s.phrase.as_str())
                .collect::<Vec<_>>(),
            ["joined", "left"]
        );
        assert!(x.statements[0].qualifiers.is_empty());
        assert_eq!(x.skipped, 0);
    }

    /// The same batch of entries, one whole and valid, the other missing only the `]` that closes `e`: both read the same. The entries contain cells of the wrong
    /// type (kind, named, object, literal value, qualifiers, time, alias quotes), and also ones that should be skipped
    /// (statements with only seven cells)
    #[test]
    fn a_reply_read_item_by_item_reads_what_the_whole_reply_reads() {
        let e =
            r#"["Acme", ["company", "org"], 1], ["Lin", "person", [1]], ["Harbor", "place", 0]"#;
        let s = r#"["Lin joined Acme.", "Lin", "joined", ["Acme"], null, [["role", "CEO"]], null, null],
["Acme sells tools.", "Acme", "sells", null, ["tools", "parts"], null, ["2020"], null],
["Harbor is old.", "Harbor", "is", null, "old", null, null]"#;
        let n = r#"["Acme", "Acme Corp", ["Acme Corp is based in Harbor."]]"#;
        let whole = format!(r#"{{"e": [{e}], "s": [{s}], "n": [{n}], "t": []}}"#);
        let broken = format!(r#"{{"e": [{e}, "s": [{s}], "n": [{n}], "t": []}}"#);
        let (w, b) = (
            parse_open_response(&whole).unwrap(),
            parse_open_response(&broken).unwrap(),
        );
        assert!(!w.truncated && b.truncated);
        let read = |x: &OpenExtraction| {
            (
                format!("{:?}", x.entities),
                format!("{:?}", x.statements),
                format!("{:?}", x.names),
                x.skipped,
            )
        };
        assert_eq!(read(&b), read(&w));
        assert_eq!(
            (w.entities.len(), w.statements.len(), w.names.len()),
            (3, 2, 1)
        );
        assert_eq!(w.skipped, 1, "the seven-slot statement, in both");
    }

    /// In a reply where all qualifiers are null there is not a single `}`: a repair that accepts only `}` would void the whole block,
    /// so this backs up to the last complete `]`
    #[test]
    fn a_cut_off_reply_without_any_closing_brace_is_still_repaired() {
        let raw = r#"{"e": [["A", "thing", 1]],
            "s": [["A is b.", "A", "is", null, "b", null, null, null], ["A was c.", "A", "was", null, "c", nu"#;
        let x = parse_open_response(raw).unwrap();
        assert!(x.truncated);
        assert_eq!(x.entities.len(), 1);
        assert_eq!(x.statements.len(), 1);
        assert_eq!(x.statements[0].value.as_deref(), Some("b"));
        assert_eq!(x.skipped, 0);
    }

    /// Still report failure when there is not even one complete entry: tolerance must not turn an empty result into success
    #[test]
    fn a_reply_with_nothing_complete_still_fails() {
        assert!(parse_open_response(r#"{"e": [["a", "b"#).is_err());
    }

    /// A seven-cell statement is a bad entry: skipped and counted; eight-cell ones are kept
    #[test]
    fn a_statement_with_seven_slots_is_skipped_and_counted() {
        let raw = r#"{"e": [["A", "thing", 1]],
            "s": [["A is b.", "A", "is", null, "b", null, null],
                  ["A is b.", "A", "is", null, "b", null, null, null]]}"#;
        let x = parse_open_response(raw).unwrap();
        assert_eq!(x.statements.len(), 1);
        assert_eq!(x.statements[0].quote.as_deref(), Some("A is b."));
        assert_eq!(x.skipped, 1);
        assert!(!x.truncated);
    }

    /// A literal value written as a JSON number is also kept as text, as written
    #[test]
    fn a_number_in_the_value_slot_becomes_text() {
        let raw = r#"{"e": [["a", "k", 1]],
            "s": [["q", "a", "cut water use", null, 12, null, null, null],
                  ["q", "a", "ratio", null, 1.5, null, null, null]]}"#;
        let x = parse_open_response(raw).unwrap();
        assert_eq!(x.statements[0].value.as_deref(), Some("12"));
        assert_eq!(x.statements[1].value.as_deref(), Some("1.5"));
        assert_eq!(x.skipped, 0);
    }

    /// Duplicate names in `e` (trim leading and trailing whitespace, collapse internal whitespace to one, ignore case) keep the first; the rest count toward skipped
    #[test]
    fn a_duplicate_entity_name_keeps_the_first_and_is_counted() {
        let raw = r#"{"e": [["Harbor Bridge", "bridge", 1], [" harbor   bridge ", "span", 0],
                            ["HARBOR BRIDGE", "bridge", 1], ["Harbor Bridge Authority", "agency", 1]]}"#;
        let x = parse_open_response(raw).unwrap();
        assert_eq!(x.entities.len(), 2);
        assert_eq!(x.entities[0].name, "Harbor Bridge");
        assert_eq!(x.entities[0].kind, "bridge");
        assert!(x.entities[0].named);
        assert_eq!(x.entities[1].name, "Harbor Bridge Authority");
        assert_eq!(x.skipped, 2);
    }

    /// Bad entries are counted one by one: entities without a name, entities that are not arrays, statements without a subject, statements with an empty phrase,
    /// statements with one cell missing, statements that are not arrays, n without an alias, n with an object in the entity cell, n with an empty entity name
    #[test]
    fn malformed_items_are_counted_not_fatal() {
        let raw = r#"{"e": [["A", "thing", 1], [""], [5, "x"], "not an array", []],
            "s": [["A is b.", "A", "is", null, "b", null, null, null],
                  ["A is b.", null, "is", null, "b", null, null, null],
                  ["A is b.", "A", "  ", null, "b", null, null, null],
                  ["A is b.", "A", "is", null, "b", null, null],
                  {"subject": "A"}],
            "n": [["A", "Ay", "A is b."], ["A"], [{}, "Ay", "q"], ["", "Ay", "q"]]}"#;
        let x = parse_open_response(raw).unwrap();
        assert_eq!(x.entities.len(), 1);
        assert_eq!(x.statements.len(), 1);
        assert_eq!(x.names.len(), 1);
        assert_eq!(x.skipped, 4 + 4 + 3);
        assert!(!x.truncated);
    }

    /// A wrong-type optional cell only makes that cell None, and the whole record is kept: object given a number, time given an array, ended given an
    /// object, quote given a number, literal value given a boolean, qualifiers given a string
    #[test]
    fn a_wrong_type_in_an_optional_slot_is_null_not_malformed() {
        let raw = r#"{"e": [["A", "thing", 1]],
            "s": [[7, "A", "is", 3, true, "not an object", [2019], {"y": 2020}]]}"#;
        let x = parse_open_response(raw).unwrap();
        assert_eq!(x.statements.len(), 1);
        let s = &x.statements[0];
        assert_eq!(s.object, None);
        assert_eq!(s.value, None);
        assert!(s.qualifiers.is_empty());
        assert_eq!(s.when, None);
        assert_eq!(s.ended, None);
        assert_eq!(s.quote, None);
        assert_eq!(x.skipped, 0);
    }

    /// Qualifiers: a string is a name or the source text, a number is turned into text as written, and other types and empty keys are dropped silently
    #[test]
    fn qualifiers_take_strings_and_numbers_and_drop_the_rest() {
        let raw = r#"{"e": [["a", "k", 1]],
            "s": [["q", "a", "cut water use", null, "12%", {"compared to": "2019", "at": "Harbor Estates", "by": 3, "ratio": 1.5, "flag": true, "who": null, "list": ["x"], "": "x", "blank": "  "}, null, null]]}"#;
        let x = parse_open_response(raw).unwrap();
        assert_eq!(
            by_key(x.statements[0].qualifiers.clone()),
            pairs(&[
                ("compared to", "2019"),
                ("at", "Harbor Estates"),
                ("by", "3"),
                ("ratio", "1.5"),
            ])
        );
        assert_eq!(x.skipped, 0);
    }

    /// Every string is trimmed of leading and trailing whitespace: names, kinds, subject, phrase, object, time, quote, and qualifier keys and values
    #[test]
    fn every_string_is_trimmed() {
        let raw = r#"{"e": [["  A  ", " thing ", 1]],
            "s": [[" A is B. ", " A ", " is ", " B ", null, {" at ": " home "}, " in 2019 ", " until 2020 "]],
            "n": [[" A ", " Ay ", " A is B. "]]}"#;
        let x = parse_open_response(raw).unwrap();
        assert_eq!(x.entities[0].name, "A");
        assert_eq!(x.entities[0].kind, "thing");
        let s = &x.statements[0];
        assert_eq!(s.subject, "A");
        assert_eq!(s.phrase, "is");
        assert_eq!(s.object.as_deref(), Some("B"));
        assert_eq!(s.qualifiers, vec![("at".to_string(), "home".to_string())]);
        assert_eq!(s.when.as_deref(), Some("in 2019"));
        assert_eq!(s.ended.as_deref(), Some("until 2020"));
        assert_eq!(s.quote.as_deref(), Some("A is B."));
        assert_eq!(x.names[0].entity, "A");
        assert_eq!(x.names[0].name, "Ay");
        assert_eq!(x.names[0].quote.as_deref(), Some("A is B."));
    }

    /// Object and literal value both present: kept, both passed to the caller; not decided here
    #[test]
    fn a_statement_with_both_object_and_value_is_passed_through() {
        let raw = r#"{"e": [["A", "thing", 1], ["B", "thing", 1]],
            "s": [["A is b.", "A", "is", "B", "b", null, null, null]]}"#;
        let x = parse_open_response(raw).unwrap();
        assert_eq!(x.statements.len(), 1);
        assert_eq!(x.statements[0].object.as_deref(), Some("B"));
        assert_eq!(x.statements[0].value.as_deref(), Some("b"));
        assert_eq!(x.skipped, 0);
    }

    /// A value that was written into the object as a thing is moved back to value, so the "thing" is no longer listed. Real things (Hangzhou), named things in prose
    /// (Model 3, where the kind word happens to be the phrase), and things used as a subject (the year 2020) are not touched
    #[test]
    fn a_value_written_as_an_object_becomes_the_value() {
        let raw = r#"{"e": [["星河科技", "公司", 1], ["2015年3月12日", "日期", 1],
                            ["杭州", "城市", 1], ["Model 3", "竞品", 1], ["2020年", "年份", 1]],
            "s": [["星河科技成立于2015年3月12日。", "星河科技", "成立于", "2015年3月12日", null, null, null, null],
                  ["总部位于杭州。", "星河科技", "总部位于", "杭州", null, null, null, null],
                  ["竞品是Model 3。", "星河科技", "竞品", "Model 3", null, null, null, null],
                  ["2020年是上市之年。", "2020年", "是", null, "上市之年", null, null, null]]}"#;
        let mut x = parse_open_response(raw).unwrap();
        assert_eq!(values_are_not_things(&mut x), vec!["2015年3月12日"]);
        let objects: Vec<Option<&str>> = x.statements.iter().map(|s| s.object.as_deref()).collect();
        assert_eq!(objects, vec![None, Some("杭州"), Some("Model 3"), None]);
        assert_eq!(x.statements[0].value.as_deref(), Some("2015年3月12日"));
        let names: Vec<&str> = x.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["星河科技", "杭州", "Model 3", "2020年"]);
    }

    /// Measured requirements table (#987): a cell listed as an unnamed thing, whose kind word is the column name, written as an object with the column name as
    /// phrase. That is the value of this cell; named ones, or ones whose kind word and phrase differ, remain objects
    #[test]
    fn a_table_cell_listed_as_a_thing_becomes_the_value() {
        let row = "| 修改体重时App出现bug | 待审核 | P1 | 个人资料 | 张伟 |";
        let raw = format!(
            r#"{{"e": [["修改体重时App出现bug", "需求", 0], ["待审核", "处理状态", 0],
                      ["P1", "优先级", 0], ["个人资料", "模块", 0], ["张伟", "负责人", 1],
                      ["REQ-0019", "需求编号", 1]],
                "s": [["{row}", "修改体重时App出现bug", "处理状态", "待审核", null, null, null, null],
                      ["{row}", "修改体重时App出现bug", "优先级", "P1", null, null, null, null],
                      ["{row}", "修改体重时App出现bug", "需求编号", "REQ-0019", null, null, null, null],
                      ["{row}", "修改体重时App出现bug", "功能模块", "个人资料", null, null, null, null],
                      ["{row}", "修改体重时App出现bug", "负责人", "张伟", null, null, null, null]]}}"#
        );
        let mut x = parse_open_response(&raw).unwrap();
        values_are_not_things(&mut x);
        let shape: Vec<(Option<&str>, Option<&str>)> = x
            .statements
            .iter()
            .map(|s| (s.object.as_deref(), s.value.as_deref()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (None, Some("待审核")),
                (None, Some("P1")),
                (None, Some("REQ-0019")),
                (Some("个人资料"), None),
                (Some("张伟"), None),
            ]
        );
        // A cell moved to a value is no longer listed, so no isolated entity called REQ-0019 is created
        let names: Vec<&str> = x.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["修改体重时App出现bug", "个人资料", "张伟"]);
    }

    /// named accepts 1/0 and true/false; if missing it counts as named; a missing kind is an empty string
    #[test]
    fn named_takes_numbers_and_booleans_and_defaults_to_true() {
        let raw = r#"{"e": [["a", "k", 1], ["b", "k", 0], ["c", "k", true],
                            ["d", "k", false], ["e"]]}"#;
        let x = parse_open_response(raw).unwrap();
        let named: Vec<bool> = x.entities.iter().map(|e| e.named).collect();
        assert_eq!(named, vec![true, false, true, false, true]);
        assert_eq!(x.entities[4].kind, "");
        assert_eq!(x.skipped, 0);
    }

    /// Code fences and surrounding chatter are still tolerated (same as json_block)
    #[test]
    fn a_fenced_reply_parses() {
        let raw = "Here you go:\n```json\n{\"e\": [[\"a\", \"k\", 1]], \"s\": [], \"n\": []}\n```";
        let x = parse_open_response(raw).unwrap();
        assert_eq!(x.entities.len(), 1);
        assert!(!x.truncated);
    }

    fn known(handle: &str, name: &str, type_key: &str) -> KnownEntity {
        KnownEntity {
            handle: handle.into(),
            type_key: type_key.into(),
            name: name.into(),
        }
    }

    /// The prompt has no document date, no type or relation list, no numbering or handles; it has the file name, the opening, the
    /// known names and the body, in that order
    #[test]
    fn the_table_rules_ride_only_with_a_passage_that_has_a_table() {
        let plain = "The council met on March 4, 2011. It awarded the paving contract.";
        let system = &build_open_messages("a.md", &[], None, plain)[0].content;
        assert!(!system.contains("In a table"));
        assert!(!system.contains("{TABLE}"));
        assert!(system.contains("Write every figure the passage states.\n"));
        for table in [
            "| Item | Q3 2025 |\n|---|---|\n| Revenue | 12 |",
            "<table><tr><td>Revenue</td><td>12</td></tr></table>",
            "Revenue\t12\t14",
            "Revenue      12      14\nCosts        7       8",
        ] {
            let system = &build_open_messages("a.md", &[], None, table)[0].content;
            assert!(
                system.contains("Write every figure the passage states. In a table, a cell is"),
                "{table}"
            );
            assert!(system.contains("column heading is not in the passage.\n"));
        }
    }

    #[test]
    fn the_prompt_carries_no_ontology_and_no_document_date() {
        let msgs = build_open_messages(
            "annual-report.txt",
            &[
                known("k1", "Nebula Technologies Inc.", "organization"),
                known("k2", "Harbor Estates", ""),
            ],
            Some("Nebula Technologies Inc. Annual Report 2019"),
            "Nebula leased the northern wing.",
        );
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        let system = &msgs[0].content;
        let user = &msgs[1].content;
        for s in [system, user] {
            assert!(!s.contains("Document date"), "{s}");
            assert!(!s.contains("Resolve relative time"), "{s}");
            assert!(!s.contains("Entity types"), "{s}");
            assert!(!s.contains("Relation types"), "{s}");
            assert!(!s.contains("Attributes ("), "{s}");
            assert!(!s.contains("\"q\""), "没有引文表: {s}");
            assert!(!s.contains("k1"), "句柄不给模型看: {s}");
        }
        assert!(system.contains("Never compute, convert or normalise a date"));
        // 0064: dates the document itself states are reported by this block (`t`); time words go only into when / ended;
        // the prompt still has no document date, and the model is not asked to calculate any
        assert!(
            system.contains("\"t\" lists the dates the passage states about the document itself")
        );
        assert!(system.contains("go in when or ended and nowhere else"));
        assert!(system.contains("transcribe the numbers"));
        assert!(system.contains("do not translate it into any vocabulary of your own"));
        assert!(system.contains("written exactly as listed in \"e\""));
        assert!(system.contains("copied verbatim"));
        assert!(system.contains("write nothing about them"));
        assert!(system.contains(
            "\"s\": [[\"The city council awarded the paving contract to Brightway Builders for $2 million on March 4, 2011.\", \"city council\", \"awarded\", \"paving contract\", null, {\"to\": \"Brightway Builders\""
        ));

        let doc = user.find("Document: annual-report.txt").unwrap();
        let opening = user.find("Opening of this document").unwrap();
        assert!(user.contains("Annual Report 2019"));
        let recorded = user.find("Already recorded from earlier parts").unwrap();
        assert!(user.contains("\n- Nebula Technologies Inc. (organization)\n"));
        assert!(user.contains("\n- Harbor Estates\n"), "空类型不留括号");
        assert!(user.contains("write its name exactly as it is listed here"));
        let passage = user
            .find("Passage:\nNebula leased the northern wing.")
            .unwrap();
        assert!(doc < opening && opening < recorded && recorded < passage);
        assert!(user.ends_with("Nebula leased the northern wing."));
    }

    /// First block: no opening and no known entities, so neither section appears
    #[test]
    fn the_first_chunk_carries_neither_opening_nor_known_block() {
        let msgs = build_open_messages("a.txt", &[], None, "text");
        let user = &msgs[1].content;
        assert_eq!(user, "Document: a.txt\n\nPassage:\ntext");
        assert!(!user.contains("Opening of this document"));
        assert!(!user.contains("Already recorded"));
    }
}
