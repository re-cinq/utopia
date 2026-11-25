//! utopia-extract: prompts and parsing for extraction.
//!
//! Open extraction ([`open`]) is the only extraction contract (0044 decision 2): the model writes statements in the document's own words,
//! without reading the ontology or computing dates. What lives here are the parts it borrows (handles of known entities, the file-opening budget,
//! JSON block extraction and bracket closing), the batched entity-resolution verdicts, and the reading of quantities and times: adoption of ontology attributes,
//! tool arguments and connectors all use them.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use utopia_llm::ChatMessage;

pub mod align;
pub mod errata;
pub mod governor;
pub mod implication;
pub mod ontology_agent;
pub mod open;
pub mod phrase_align;
pub mod question_agent;
pub mod time;

/// Response-scoped reference to a persistent entity; database UUIDs must never enter prompts.
pub struct KnownEntity {
    pub handle: String,
    pub type_key: String,
    pub name: String,
}

/// The character budget for the start of a file going into the prompt. A supplementary protocol's title, effective date, parties and "which protocol it amends"
/// are usually within the first thousand characters; a press release's dateline and lead are too
pub(crate) const OPENING_BUDGET_CHARS: usize = 1500;

/// Format the start of a file as a paragraph of the prompt. An empty start (or only whitespace) returns an empty string;
/// "this block is the opening itself" is decided by the caller, which then passes `None`.
///
/// Truncates by character: never cuts a character, but can cut mid-word, so an English file's last word may be left as half a word
pub(crate) fn opening_block(opening: Option<&str>) -> String {
    let Some(text) = opening.map(str::trim).filter(|t| !t.is_empty()) else {
        return String::new();
    };
    let cut: String = text.chars().take(OPENING_BUDGET_CHARS).collect();
    let more = if cut.chars().count() < text.chars().count() {
        " …"
    } else {
        ""
    };
    format!(
        "\nOpening of this document, for context only (do not extract facts from it; they are \
         extracted from that part separately). Use it to know what the text below belongs to — \
         which agreement, company or event it concerns, who the parties are, and the date it \
         takes effect — so that facts in the text below attach to the right entity and carry \
         the right dates:\n\"\"\"\n{cut}{more}\n\"\"\"\n"
    )
}

/// Entities already mentioned in this document, put into the prompt's character budget.
///
/// Truncates when over budget (keeping those that appeared first). Chinese business text gives the full name first and the protagonist first, so
/// **first-appearance order naturally favors names that will later be shortened**.
pub(crate) const KNOWN_BUDGET_CHARS: usize = 1200;

/// The part of the reply's text that may be JSON: strip thinking and code fences. Chatter before and after is left to the caller to locate by brackets:
/// bracket positioning: [`json_block`] takes the first `{` through the last `}`; open extraction's truncation repair instead takes from
/// the first `{` to the end, since there the records are arrays and the last `}` is not a reliable end
pub(crate) fn json_text(raw: &str) -> &str {
    let text = raw.trim();
    // Reasoning models' thinking (#690): `LlmClient::chat` strips it first, but this block-extraction layer
    // must recognize the markers itself to be the final safeguard, because the `chat_tools` path does not go through `chat`.
    // Without stripping, braces inside the thinking would move the start of "the first `{`" earlier,
    // and the truncation-repair logic cannot recognize the chatter sitting in between, so the whole block is voided.
    let text = match text.rfind("</think>") {
        Some(pos) => text[pos + "</think>".len()..].trim(),
        None => text,
    };
    text.strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .map(|s| s.trim_end_matches("```"))
        .unwrap_or(text)
}

/// Robustly extract the JSON block from an LLM reply (tolerating code fences and surrounding chatter).
pub fn json_block(raw: &str) -> anyhow::Result<String> {
    let cleaned = json_text(raw);
    let start = cleaned.find('{');
    let end = cleaned.rfind('}');
    match (start, end) {
        (Some(s), Some(e)) if e > s => Ok(cleaned[s..=e].to_string()),
        _ => anyhow::bail!("No JSON found in LLM reply"),
    }
}

/// Append the missing brackets after head. Brackets inside string literals do not count: `"a[b"` is not an open bracket.
///
/// Returns None = the structure itself is wrong (e.g., brackets already too many), not "not finished".
pub(crate) fn close_brackets(head: &str) -> Option<String> {
    let mut stack: Vec<char> = Vec::new();
    let (mut in_str, mut esc) = (false, false);
    for c in head.chars() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '[' | '{' => stack.push(c),
            // Not written as two guarded branches: that would hide the side effect of stack.pop() inside the guard,
            // which happens to be right, but a reader would not expect a guard to change state
            ']' | '}' => {
                let want = if c == ']' { '[' } else { '{' };
                if stack.pop() != Some(want) {
                    return None;
                }
            }
            _ => {}
        }
    }
    if in_str {
        return None; // cut mid-string, this piece is unusable
    }
    let mut out = String::from(head);
    for c in stack.iter().rev() {
        out.push(if *c == '[' { ']' } else { '}' });
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Entity-resolution verdicts (batched: one call judges several pairs; the LLM only handles the gray zone that embeddings cannot separate)
// ---------------------------------------------------------------------------

/// One side awaiting a verdict: name + type + fact summary line.
#[derive(Debug, Clone)]
pub struct AdjudicationSide {
    pub name: String,
    pub type_label: String,
    pub facts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AdjudicationPair {
    pub left: AdjudicationSide,
    pub right: AdjudicationSide,
    /// What people in this library have done on this pair, this name, or this type pairing (0025). Empty means not mentioned
    pub precedents: Vec<String>,
    /// How the pair was proposed: written only when the proposal basis is not "same name": name-vector recall (0041, second cut)
    /// It proposes two **different strings**; if the adjudicator does not know this, it would treat "Zhang Wei" as "finance director Zhang Wei",
    /// the same person once the qualifier is removed. That wrong merge on the measurement bench came from exactly this. Empty means not mentioned; same-name pairs stay as before
    pub proposed_because: Option<String>,
}

/// A pair proposed by name-vector recall, written as a proposal basis sentence the adjudicator can read; `cosine` is the cosine text recorded by recall
/// (taken from the review pair's reason in `utopia_core::review_reasons::name_vector_cosine`). Other reasons
/// (same-name gray zone, same-name tie, containment) all belong to the "same string" family, for which the adjudicator's rules were written,
/// pass None
pub fn proposed_because(cosine: Option<&str>) -> Option<String> {
    let cosine = cosine?;
    Some(format!(
        "the names are similar but NOT the same string (name-vector cosine {cosine}): this may be a \
         short form, another script, or a different thing with a similar name; a dropped qualifier is \
         not evidence here, the facts are"
    ))
}

#[derive(Debug, Deserialize)]
pub struct AdjudicationVerdict {
    pub i: usize,
    pub verdict: String,
    #[serde(default)]
    pub confidence: Option<f32>,
    /// one sentence of reasoning; a verdict with precedent must say which precedent it relied on
    #[serde(default)]
    pub why: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdjudicationReply {
    #[serde(default)]
    verdicts: Vec<AdjudicationVerdict>,
}

/// Build the batched adjudication prompt. Conservative bias: insufficient evidence answers unsure (split rather than merge; merging needs evidence).
/// Identity rules (0025 first iteration, 2026-09-07): shared by the batched and tool-loop prompts.
///
/// Four kinds of error measured against the ai-timeline ground truth: versions merged into the series, "X's Y" merged into X, lists merged into members,
/// subsidiaries merged into the parent company; and same names split by "different type label" or "different partner". Each is written as
/// one rule plus an example; the examples come from that corpus.
pub const IDENTITY_RULES: &str = "\
A record is one specific thing in the world: a person, an organization, a legal entity, a \
product, one version of a product, a document, an event, a place, or a concept. Two records \
are the SAME only when they denote exactly the same thing.\n\
\n\
Names:\n\
- The same proper name, compatible kinds, and no real contradiction: the same thing. Type \
  labels were assigned by an extractor and are noisy: Organization, Corporation, \
  ResearchOrganization, NGO and Store can all be one company; SoftwareApplication, \
  CreativeWork, Intangible, Product, Service, Offer and ComputerLanguage can all be one \
  product; Place and State can be one state. Only incompatible kinds count as a difference: \
  a person against a company, a place against a product, an event against an organization. \
  For identical proper names answer \"different\" only when you can name the contradiction \
  in one sentence; a type label, a missing fact, fewer facts, or different partners, \
  products, roles or events is not one. Never treat the absence of facts as a difference.\n\
- A surname or a first name alone against a full name that contains it, in the same \
  documents, is the same person unless another person with that name appears: \"Pachocki\" \
  is \"Jakub Pachocki\", \"Kwon\" is \"Jason Kwon\", \"Nadella\" is \"Satya Nadella\". A person \
  who moved between two organizations is still one person.\n\
- A parenthetical acronym, an expanded acronym or a fuller product designation is the same \
  thing: \"reinforcement learning (RL)\" is \"reinforcement learning\", \"US Federal Trade \
  Commission (FTC)\" is \"Federal Trade Commission\", \"MI450\" is the \"AMD Instinct MI450\".\n\
- A name that is the other name with a qualifier removed from the FRONT is usually the same \
  thing abbreviated: \"Google DeepMind\" and \"DeepMind\", \"Adam D'Angelo\" and \"D'Angelo\", \
  \"Meta Platforms\" and \"Meta\", \"Altimeter Capital\" and \"Altimeter\", \"The New York \
  Times\" and \"New York Times\", \"Apple Inc.\" and \"Apple\". Documents drop the qualifier \
  after first mention.\n\
- A name that is the other name with something ADDED AT THE END is a different, more \
  specific thing: a version or edition (\"Claude 4 Opus\" is not \"Claude\", \"AlphaFold2\" is \
  not \"AlphaFold\", \"Genie 2\" is not \"Genie\", \"GPT-4.5\" is not \"GPT-4\", \"2024 \
  International Mathematical Olympiad\" is not \"International Mathematical Olympiad\"), a \
  variant or tier (\"Gemini Robotics-ER\" is not \"Gemini Robotics\", \"Claude 3 Haiku\" is \
  not \"Haiku\"), a division, subsidiary or legal entity (\"DeepMind Health\" is not \
  \"DeepMind\", \"OpenAI Ireland Ltd\" is not \"OpenAI\", \"Microsoft AI\" is not \
  \"Microsoft\"), a project, programme, team, app or component. Never merge a version into \
  its family or a part into its whole.\n\
- A document, agreement or filing is cited in many ways and stays one thing through its \
  amendments: \"the Lease\", \"Lease Agreement\", \"Lease Agreement dated May 16, 2016\" and \
  \"Lease Agreement dated May 16, 2016, as amended\" are one agreement when their parties and \
  subject do not contradict each other. When it was signed and that it was amended describe \
  the agreement; they do not make a second one. Each amendment is a document of its own, and \
  an agreement for other premises, another phase or other parties (\"Phase 2 Lease\") is a \
  different agreement.\n\
- A phrase that merely contains a name is not that name: \"Sam Altman's efforts\", \
  \"psychological abuse from Sam Altman\", \"share sale led by Thrive Capital\", \"leaked \
  letter from the National Data Guardian\", \"ChatGPT played a role in the campaign\", \"a \
  consistent pattern of lying\". These describe something about the thing; they are not the \
  thing.\n\
- A list of names (\"MuZero, AlphaStar, AlphaGeometry\") is not any of its members.\n\
- A common noun or generic phrase (\"employees\", \"users\", \"lawsuit\", \"investors\", \
  \"event\", \"safety\") is not a proper name. Two such records are the same only when their \
  facts show they are one specific instance; usually they are different.\n\
- When the facts of either record describe ownership, control, a subsidiary, a holding or \
  a parent relation between the two names, or show one of them as one legal entity among \
  several in a group (\"OpenAI, Inc. controls the for-profit company\", \"OpenAI GP LLC \
  controls OpenAI LP\"), they are two entities even if one name is the other plus a \
  corporate suffix. A group and its legal entities are different records.\n\
\n\
Facts:\n\
- Different facts are not contradictory facts. One company has many partnerships, investors, \
  lawsuits and contracts; a person changes jobs; one product is praised in one document and \
  criticised in another. A contradiction is two facts that cannot both hold of one thing at \
  once: two different founders, two headquarters at the same time, two different birth dates, \
  or affiliations that overlap in time and exclude each other. Only a contradiction is \
  evidence of difference.\n\
- A record with no facts contributes nothing; the name and the other record decide.";

pub fn build_adjudication_messages(pairs: &[AdjudicationPair]) -> Vec<ChatMessage> {
    let system = format!(
        "You are an entity-resolution adjudicator for a knowledge graph. For each numbered \
         pair, decide whether the two records refer to the SAME real-world thing or are two \
         things that share a name.\n\
         \n\
         {IDENTITY_RULES}\n\
         \n\
         Some pairs carry precedents: decisions people made in this same knowledge base on \
         these names, or on pairs of the same two types. Treat them as how the owners of this \
         base want such cases judged. Follow a precedent on the same pair unless the facts of \
         this pair clearly differ from it; when precedents disagree with each other, answer \
         \"unsure\". A precedent never overrides a contradiction in the facts. Some precedents quote what the person wrote when deciding: weigh that stated ground, not only the outcome; a decision made for a reason that does not hold here is not a precedent for this pair.\n\
         \n\
         Output exactly one JSON object and nothing else:\n\
         {{\"verdicts\":[{{\"i\":0,\"verdict\":\"same|different|unsure\",\"confidence\":0.9,\
         \"why\":\"one sentence\"}}]}}\n\
         \n\
         Rules:\n\
         1. One verdict per pair, using the pair's number as \"i\".\n\
         2. confidence in 0~1.\n\
         3. \"why\" is one short sentence; when a precedent decided it, say which.\n\
         4. A wrong merge is far more damaging than leaving two records separate: answer \
            \"same\" only when the rules above make it so; when a rule says two things, say \
            \"different\" with confidence, not \"unsure\"; \"unsure\" is for evidence that \
            genuinely points both ways."
    );

    let mut user = String::new();
    for (i, p) in pairs.iter().enumerate() {
        let fmt = |s: &AdjudicationSide| {
            let facts = if s.facts.is_empty() {
                "  (no recorded facts)".to_string()
            } else {
                s.facts
                    .iter()
                    .map(|f| format!("  - {f}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            format!("\"{}\" ({})\n{}", s.name, s.type_label, facts)
        };
        let precedents = if p.precedents.is_empty() {
            String::new()
        } else {
            let lines = p
                .precedents
                .iter()
                .map(|l| format!("  - {l}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("Precedents (decided by people in this base):\n{lines}\n")
        };
        let why_paired = p
            .proposed_because
            .as_deref()
            .map(|w| format!("Why paired: {w}\n"))
            .unwrap_or_default();
        user.push_str(&format!(
            "Pair {i}:\nRecord A: {}\nRecord B: {}\n{why_paired}{precedents}\n",
            fmt(&p.left),
            fmt(&p.right)
        ));
    }

    vec![
        ChatMessage {
            role: "system".into(),
            content: system,
        },
        ChatMessage {
            role: "user".into(),
            content: user,
        },
    ]
}

pub fn parse_adjudication(raw: &str) -> anyhow::Result<Vec<AdjudicationVerdict>> {
    parse_adjudication_repairing(raw).map(|(verdicts, _)| verdicts)
}

/// Same as `parse_adjudication`, but also returns whether it was repaired.
///
/// The model writes a line break directly inside the quoted reason: JSON forbids bare control characters in strings, and serde
/// reports `control character (\u0000-\u001F) found while parsing a string`. A batch
/// of dozens of pairs is voided entirely by one line break; retrying the same reply three times voids the whole batch again, and the pairs in it
/// stay queued forever (on 100 Re-DocRED documents, 2903 were stuck). Parse as-is first; if that fails, escape the control characters inside
/// string literals as `\n`, `\t`, `\u00XX` and parse again. Line breaks outside strings
/// are legal whitespace and left alone. The repair flag is only a bool, and the verdict is the same: a well-formed reply
/// takes the first path, as before. A caller that gets true logs a WARN saying which batch it was.
pub fn parse_adjudication_repairing(raw: &str) -> anyhow::Result<(Vec<AdjudicationVerdict>, bool)> {
    let json_str = json_block(raw)?;
    match serde_json::from_str::<AdjudicationReply>(&json_str) {
        Ok(reply) => Ok((reply.verdicts, false)),
        Err(first) => {
            let escaped = escape_control_in_strings(&json_str);
            if escaped == json_str {
                anyhow::bail!("Failed to parse adjudication JSON: {first}");
            }
            let reply: AdjudicationReply = serde_json::from_str(&escaped)
                .map_err(|e| anyhow::anyhow!("Failed to parse adjudication JSON: {e}"))?;
            Ok((reply.verdicts, true))
        }
    }
}

/// Escape bare control characters (U+0000–U+001F) inside string literals as JSON escapes; outside literals they are kept as is.
/// Quote and backslash tracking is the same as `close_brackets`: `\"` does not end the string
pub(crate) fn escape_control_in_strings(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut in_str, mut esc) = (false, false);
    for c in text.chars() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            } else if c.is_control() && (c as u32) < 0x20 {
                match c {
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    other => out.push_str(&format!("\\u{:04x}", other as u32)),
                }
                continue;
            }
        } else if c == '"' {
            in_str = true;
        }
        out.push(c);
    }
    out
}

/// A string that is **entirely one quantity** → (value, unit).
///
/// Strict criteria: optional currency symbol + number + optional magnitude word + optional percent sign, and **no other word** is allowed.
/// A word still attached at the tail makes the meaning no longer just that number:
///
/// ```text
/// "$5 billion"                        → (5e9, Some("$"))
/// "52%"                               → (52.0, Some("%"))
/// "3.5 million"                       → (3.5e6, None)
/// "35,000"                            → (35000.0, None)
/// "900 million weekly active users"   → None   a content word follows
/// "2025 Atlantic hurricane season"    → None   that is an event, not 2025
/// "8GW data center"                   → None
/// "3M"                                → None   that is a company
/// ```
///
/// **Magnitude words are recognized only in full.** Single-letter suffixes (`3M`, `5k`, `2B`) look convenient, but they read names like 3M,
/// K2, B1 as numbers: a real entity gets read as a quantity, which makes the fact's shape wrong,
/// and the wrong direction is irreversible: the node is never created and the name is not kept.
///
/// **Units are copied as the symbol; currency is not guessed.** `$` may be USD, CAD or AUD, and the yen sign may be JPY or
/// CNY. A guessed "USD" is an assertion nobody is responsible for, while the `$` written in the source is the fact.
pub fn parse_quantity(s: &str) -> Option<(f64, Option<String>)> {
    scan_quantity(s, true).map(|q| (q.n, q.unit))
}

/// Starts with a quantity and has words after it → that quantity. `"1,250 people"` → (1250, "people").
///
/// **This is for places that already know what they want**, and is not the same strictness as `parse_quantity`.
/// `parse_quantity` must judge "is this string one thing", and a wrong judgement swallows a real entity,
/// so any content word at the tail is rejected. The callers here already have an attribute declaring
/// `datatype = number`; the question is no longer "is it a number" but "what is that number",
/// and a wrong judgement only costs a wrong value, with magnitudes off by several orders.
pub fn parse_leading_quantity(s: &str) -> Option<(f64, Option<String>)> {
    scan_quantity(s, false).map(|q| (q.n, q.unit))
}

/// Currency: symbol, ISO code, English and Chinese words, all unified to a symbol. **Only this table is accepted**; anything unrecognized is not guessed.
pub(crate) fn currency_unit(tok: &str) -> Option<&'static str> {
    Some(
        match tok.trim_matches(|c: char| c == ',' || c == '.' || c == ';') {
            "$" | "USD" | "usd" | "US$" | "dollar" | "dollars" | "美元" => "$",
            "€" | "EUR" | "eur" | "euro" | "euros" | "欧元" => "€",
            "£" | "GBP" | "gbp" | "pound" | "pounds" | "英镑" => "£",
            // The full-width yen sign (U+FFE5) used in Chinese typesetting is the same symbol
            "¥" | "￥" | "JPY" | "jpy" | "yen" | "日元" => "¥",
            "CNY" | "cny" | "RMB" | "rmb" | "yuan" | "人民币" | "元" | "元人民币" | "人民币元" => {
                "¥"
            }
            "HKD" | "hkd" | "HK$" | "港元" | "港币" => "HK$",
            "₩" | "KRW" | "won" | "韩元" => "₩",
            "₹" | "INR" | "rupee" | "rupees" | "卢比" => "₹",
            _ => return None,
        },
    )
}

/// Decimal exponents of magnitude words: English spelled out; Chinese thousand, ten-thousand and hundred-million. **Single letters are not recognized** (`3M` is a company).
fn magnitude(tok: &str) -> Option<u8> {
    Some(match tok {
        "thousand" | "千" => 3,
        "万" => 4,
        "million" | "百万" => 6,
        "千万" => 7,
        "亿" => 8,
        "billion" | "十亿" => 9,
        "trillion" | "万亿" => 12,
        _ => return None,
    })
}

/// Splits forms like `200 million USD`, `1.5 billion RMB`, `€30 million`, `30 million euros`, `USD 30m` (m not recognized)
/// into [prefix currency] number [magnitude] [suffix currency/unit] [rest].
/// `strict` = the whole must be exactly one quantity: if the rest is not empty it is not recognized.
fn scan_quantity(s: &str, strict: bool) -> Option<Quantity> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let negated = if strict { None } else { accounting_negative(s) };
    let s = negated.as_deref().unwrap_or(s);
    let (body, percent) = match s.strip_suffix('%') {
        Some(b) => (b.trim_end(), true),
        None => (s, false),
    };
    // 1. Prefix currency: the symbol touches the number, or an ISO code/word followed by a space
    let mut rest = body;
    let mut currency: Option<&'static str> = None;
    if let Some(c) = rest.chars().next() {
        if let Some(u) = currency_unit(&c.to_string()) {
            currency = Some(u);
            rest = rest[c.len_utf8()..].trim_start();
        }
    }
    if currency.is_none() {
        if let Some((head, tail)) = rest.split_once(char::is_whitespace) {
            if let Some(u) = currency_unit(head) {
                currency = Some(u);
                rest = tail.trim_start();
            }
        }
    }
    // 2. Number: leading [-+0-9.,_]; the minus sign may also be a typesetting U+2212
    let num_end = rest
        .char_indices()
        .find(|(_, c)| !matches!(c, '0'..='9' | '.' | ',' | '_' | '-' | '+' | '−'))
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    let (num, after) = rest.split_at(num_end);
    let cleaned: String = num
        .chars()
        .filter(|c| !matches!(c, ',' | '_'))
        .map(|c| if c == '−' { '-' } else { c })
        .collect();
    let mut n: f64 = cleaned.parse().ok()?;
    // 3. After the number: magnitude and currency words, attached or space-separated, are consumed one by one; whatever cannot be consumed is the "rest"
    let mut tail = after.trim_start();
    let mut unit: Option<String> = None;
    let mut ate_magnitude = false;
    loop {
        if tail.is_empty() {
            break;
        }
        // Take the next token: Chinese by character (magnitude/currency words are at most two or three characters long), others split on whitespace
        let (tok, next) = next_token(tail);
        if !ate_magnitude {
            // In headings written as "$5 Billion": magnitude words are case-insensitive, otherwise 5 billion would read as 5
            if let Some(m) = magnitude(&tok.to_ascii_lowercase()) {
                // Parse the written decimal with its scale in one conversion. Multiplying
                // an already rounded f64 needs an epsilon that can erase real fractions.
                // The suffix is at most three bytes (e12), so allocation stays O(num.len()).
                n = format!("{cleaned}e{m}").parse().ok()?;
                ate_magnitude = true;
                tail = next.trim_start();
                continue;
            }
        }
        if unit.is_none() && currency.is_none() {
            if let Some(u) = currency_unit(tok) {
                unit = Some(u.to_string());
                tail = next.trim_start();
                continue;
            }
        }
        break;
    }
    if percent && (currency.is_some() || unit.is_some()) {
        return None;
    }
    if !n.is_finite() {
        return None;
    }
    let unit = if percent {
        Some("%".to_string())
    } else {
        currency.map(str::to_string).or(unit)
    };
    if strict && !tail.is_empty() {
        return None;
    }
    // Loose: the first word of the rest is the unit (`1,250 people` → people); used only when no currency is present
    let unit = unit.or_else(|| (!tail.is_empty()).then(|| next_token(tail).0.to_string()));
    Some(Quantity {
        n,
        unit,
        scaled: ate_magnitude,
    })
}

/// Accounting notation: a number in parentheses is negative. `(1,234)` → `-1,234`, `$(1.2) billion` → `$-1.2 billion`,
/// `(12.5%)` → `-12.5%`. Before the parenthesis only one currency is allowed, inside only a number, after only a magnitude word, currency or `%`:
/// "(1) 1,200 employees" and "(2024) 500" have parentheses that are numbering and years, not a minus sign
fn accounting_negative(s: &str) -> Option<String> {
    let (head, rest) = s.split_once('(')?;
    let (inner, after) = rest.split_once(')')?;
    if !head.trim().is_empty() && currency_unit(head.trim()).is_none() {
        return None;
    }
    let mut tail = after.trim();
    tail = tail.strip_suffix('%').unwrap_or(tail).trim_end();
    while !tail.is_empty() {
        let (tok, next) = next_token(tail);
        if magnitude(&tok.to_ascii_lowercase()).is_none() && currency_unit(tok).is_none() {
            return None;
        }
        tail = next.trim_start();
    }
    let digits = inner.trim().strip_suffix('%').unwrap_or(inner.trim());
    if !digits.starts_with(|c: char| c.is_ascii_digit())
        || !digits
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ',' | '.'))
    {
        return None;
    }
    Some(format!("{head}-{}{after}", inner.trim()))
}

/// A quantity read out. `n` has already been multiplied by the magnitude word; `scaled` records whether the source wrote a magnitude word. When converting to the attribute's
/// declared unit we need to know whether `5000` is written that way in the source, or was expanded out of a form like `5000 ten-thousand`
struct Quantity {
    n: f64,
    unit: Option<String>,
    scaled: bool,
}

/// Words like "about" and "around" that say the number is only approximate. Removing them leaves the same number; "over" and "more than" are not here:
/// they state a bound, and treating the bound as a value is an assertion the source never made
fn strip_approx(s: &str) -> &str {
    let mut s = s.trim();
    for p in [
        "大约",
        "约合",
        "将近",
        "接近",
        "约",
        "近",
        "about ",
        "approximately ",
        "around ",
        "roughly ",
        "nearly ",
        "~",
        "≈",
    ] {
        // An approximation word at the start of a sentence, such as "About 350" or "Approximately 1,200": English approximation words are case-insensitive
        if s.get(..p.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(p))
        {
            s = s[p.len()..].trim_start();
            break;
        }
    }
    s.strip_suffix("左右").map_or(s, str::trim_end)
}

/// Counting classifiers: `1200 people`, `350 members`, `32 companies`. A number followed by just this word is still one number as a whole.
const COUNTERS: &[&str] = &[
    "人",
    "名",
    "位",
    "个",
    "家",
    "台",
    "件",
    "项",
    "岁",
    "次",
    "座",
    "所",
    "户",
    "辆",
    "种",
    " people",
    " employees",
    " staff",
];

/// Whether an object's name is actually a value: a quantity (with a magnitude word, currency, percent sign or counting classifier) or a
/// date with a clear format. In open extraction the model sometimes lists values such as "established on 2015-03-12" or "registered capital of
/// 50 million yuan" as things in the object, so the signature becomes a relation between two things and never matches an attribute.
///
/// **A bare number counts as a value only when it is not a named thing**: `1200` is a number, while named items like
/// `1984` or `360` are most likely a book or a company. Swallowing a real entity is irreversible; leaving a number
/// in the object only fails to bind an attribute
pub fn literal_object(name: &str, named: bool) -> bool {
    let t = strip_approx(name);
    if t.is_empty() {
        return false;
    }
    if written_date(t).is_some() || t.contains(['-', '/', '.']) && parse_time(t).is_some() {
        return true;
    }
    match scan_quantity(t, true) {
        Some(q) if q.unit.is_some() || q.scaled => true,
        Some(_) => !named,
        None => COUNTERS.iter().any(|c| {
            t.strip_suffix(c)
                .and_then(|r| scan_quantity(r.trim_end(), true))
                .is_some_and(|q| q.unit.is_none())
        }),
    }
}

/// Next token: ASCII is split on whitespace; for CJK try the longest three-, two-, then one-character magnitude/currency word that can be recognized,
/// and if none can be recognized, take up to the next whitespace
fn next_token(s: &str) -> (&str, &str) {
    let first = s.chars().next().unwrap_or(' ');
    if first.is_ascii() {
        let end = s.find(char::is_whitespace).unwrap_or(s.len());
        return (&s[..end], &s[end..]);
    }
    let idx: Vec<usize> = s
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(s.len()))
        .collect();
    for len in [4usize, 3, 2, 1] {
        if idx.len() > len {
            let cand = &s[..idx[len]];
            if magnitude(cand).is_some() || currency_unit(cand).is_some() {
                return (cand, &s[idx[len]..]);
            }
        }
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    (&s[..end], &s[end..])
}

/// Normalize an attribute value by its datatype. Failure returns None — prefer missing to dirty; the caller skips it and logs.
/// number tolerates thousands separators and spaces; date accepts rule 3's formats (YYYY[-MM[-DD]], a time with timezone, kept as is),
/// and also accepts dates whose format makes the day clear ([`written_date`]), converting them to rule 3's form; bool accepts yes/no loosely.
pub fn normalize_attr_value(datatype: &str, raw: &serde_json::Value) -> Option<serde_json::Value> {
    match datatype {
        "number" => match raw {
            // A JSON number from the model also goes through f64: `65` and "65%" parse to the same number `65.0`,
            // whereas serde_json treats integers and floats as two kinds of value; on one edge 65 collided with 65.0 and was recorded as a conflict
            serde_json::Value::Number(n) => n
                .as_f64()
                .filter(|f| f.is_finite())
                .and_then(serde_json::Number::from_f64)
                .map(serde_json::Value::Number),
            serde_json::Value::String(s) => {
                // "about 350 members": the approximation word stands before the number, which the loose path also cannot handle
                let s = strip_approx(s);
                let cleaned: String = s
                    .chars()
                    .filter(|c| !matches!(c, ',' | ' ' | '_'))
                    .collect();
                cleaned
                    .parse::<f64>()
                    .ok()
                    // Values cleaning cannot handle are parsed as quantities: `$5 billion` and `52%` are whole numbers,
                    // only carrying a sign and a magnitude word. The unit is not decided here; it travels with the fact
                    // (see `parse_quantity`); this tier only turns the value into a comparable number
                    // Whatever cleaning cannot handle is parsed as a quantity. **This tier has already declared datatype = number**,
                    // so the question is not "is it a number" but "what number is it", so the broad path is used:
                    // `$5 billion` → 5e9, `1,250 people` → 1250,
                    // `42% from customers in Europe` → 42
                    .or_else(|| parse_leading_quantity(s).map(|(n, _)| n))
                    .filter(|f| f.is_finite())
                    .and_then(serde_json::Number::from_f64)
                    .map(serde_json::Value::Number)
            }
            _ => None,
        },
        // Dates written in rule 3's form are kept as is (precision follows how many digits are written, and time-of-day with timezone too); dates written in another form that can still be read
        // (#688) are converted to rule 3's form, so the same day has only one form and comparison and dedup can match
        "date" => {
            let s = raw.as_str()?.trim();
            match written_date(s) {
                Some((date, precision)) => Some(serde_json::Value::String(match precision {
                    "year" => date.format("%Y").to_string(),
                    "month" => date.format("%Y-%m").to_string(),
                    _ => date.format("%Y-%m-%d").to_string(),
                })),
                None => parse_time(s).map(|_| serde_json::Value::String(s.to_string())),
            }
        }
        "bool" => match raw {
            serde_json::Value::Bool(b) => Some(serde_json::Value::Bool(*b)),
            serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "是" => Some(serde_json::Value::Bool(true)),
                "false" | "no" | "否" => Some(serde_json::Value::Bool(false)),
                _ => None,
            },
            _ => None,
        },
        _ => {
            let s = match raw {
                serde_json::Value::String(s) => s.trim().to_string(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => return None,
            };
            (!s.is_empty()).then(|| serde_json::Value::String(s.chars().take(500).collect()))
        }
    }
}

/// Put a string value of an open statement onto an attribute: normalize by the attribute's datatype, and convert the number to the attribute's declared unit.
///
/// Units only rescale magnitude: the attribute is in "ten-thousand yuan" and the source writes "2.3 hundred-million yuan", store 23000; a source writing a bare number such as "5000"
/// is taken as already in the declared unit. If the source currency differs from the declared one ("50 million USD" landing on "ten-thousand yuan"),
/// it returns None: a currency conversion is an assertion nobody is responsible for. Unreadable values are also None: prefer missing to dirty,
/// the statement stays in the open graph, and a string like "about 350 members" is not written into the typed graph as a number
pub fn attribute_value(
    datatype: &str,
    unit: Option<&str>,
    raw: &serde_json::Value,
) -> Option<serde_json::Value> {
    let declared = unit.map(str::trim).filter(|u| !u.is_empty());
    let read = match (raw, datatype, declared) {
        (serde_json::Value::String(s), "number", Some(d)) => {
            scan_quantity(strip_approx(s), false).map(|q| (q, d))
        }
        _ => None,
    };
    let Some((q, declared)) = read else {
        return normalize_attr_value(datatype, raw);
    };
    // Split the declared unit into magnitude and the rest: "ten-thousand yuan" → (4, yuan), "million dollars" → (6, dollars)
    // Currency written first, as in "USD million" or "€ million": the magnitude is at the end
    let (tok, rest) = next_token(declared);
    let trailing = declared
        .rsplit_once(char::is_whitespace)
        .and_then(|(head, last)| Some((magnitude(&last.to_ascii_lowercase())?, head.trim())));
    let (exponent, base) = match magnitude(&tok.to_ascii_lowercase()) {
        Some(m) => (m, rest.trim()),
        None => trailing.unwrap_or((0, declared)),
    };
    let written = q.unit.as_deref().and_then(currency_unit);
    let wanted = currency_unit(base);
    if written.is_some() && wanted.is_some() && written != wanted {
        return None;
    }
    // Either the source wrote a magnitude word, or it wrote the same unit as declared ("50000000 yuan" against "ten-thousand yuan", "12,000 people" against
    // "ten-thousand people"), or wrote a counting classifier, then n is an absolute quantity and must be divided; a bare number is already in the declared unit.
    // Division is done by one decimal parse, for the same reason `scan_quantity` multiplies magnitude words: computing in f64 and then dividing leaves
    // 23000.000000000004
    let counted = q
        .unit
        .as_deref()
        .is_some_and(|u| u == base || COUNTERS.iter().any(|c| c.trim() == u));
    let n = if q.scaled || counted || (written.is_some() && written == wanted) {
        format!("{}e-{exponent}", q.n).parse().ok()?
    } else {
        q.n
    };
    serde_json::Number::from_f64(n).map(serde_json::Value::Number)
}

/// Parse a time string → (UTC time, precision).
///
/// Date: YYYY / YYYY-MM / YYYY-MM-DD, with precision following how many digits are written. A time with timezone (0024):
/// `YYYY-MM-DDTHH[:MM[:SS]]` followed by `Z` or `±HH:MM`, precision to hour / minute / second,
/// with the value truncated to that place. **A clock time without a timezone is not a moment** (nobody knows which place "14:32" refers to),
/// so only the date half is taken, by day; the clock time stays in the quote. Sub-second parts are always dropped: the ledger stops at seconds.
///
/// This is rule 3's contract format; tool arguments and times shown in the UI accept only it. Reading a model reply uses [`read_time`]
pub fn parse_time(s: &str) -> Option<(DateTime<Utc>, &'static str)> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("null") {
        return None;
    }
    if let Some((date, clock)) = s.split_once(['T', ' ']) {
        let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
        let Some((clock, offset)) = split_zone(clock) else {
            return Some((Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?), "day"));
        };
        let parts: Vec<&str> = clock.split(':').collect();
        let (h, m, sec, precision) = match parts.as_slice() {
            [h] => (*h, "0", "0", "hour"),
            [h, m] => (*h, *m, "0", "minute"),
            [h, m, sec] => (*h, *m, sec.split('.').next().unwrap_or(sec), "second"),
            _ => return None,
        };
        let time =
            chrono::NaiveTime::from_hms_opt(h.parse().ok()?, m.parse().ok()?, sec.parse().ok()?)?;
        let utc = d.and_time(time) - offset;
        return Some((Utc.from_utc_datetime(&utc), precision));
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some((Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?), "day"));
    }
    if let Ok(d) = NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d") {
        return Some((Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?), "month"));
    }
    if s.len() == 4 {
        if let Ok(year) = s.parse::<i32>() {
            let d = NaiveDate::from_ymd_opt(year, 1, 1)?;
            return Some((Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?), "year"));
        }
    }
    None
}

/// Times in a model reply: first rule 3 ([`parse_time`]); a date not written in that form but whose format makes the day clear
/// is also accepted ([`written_date`], #688). Range endpoints, date attributes, and date attributes on edges are all read from here.
pub fn read_time(s: &str) -> Option<(DateTime<Utc>, &'static str)> {
    parse_time(s).or_else(|| {
        let (date, precision) = written_date(s)?;
        Some((
            Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?),
            precision,
        ))
    })
}

/// Day of month in "17th", "2nd", "23rd". The suffix must match the number: 11, 12, 13 take th, not 11st.
fn ordinal_day(w: &str) -> Option<&str> {
    let (digits, suffix) = w.split_at_checked(w.len().checked_sub(2)?)?;
    if digits.is_empty() || digits.len() > 2 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    let expected = match n % 100 {
        11..=13 => "th",
        _ => match n % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };
    suffix.eq_ignore_ascii_case(expected).then_some(digits)
}

fn day_digits(w: &str) -> Option<&str> {
    if w.chars().all(|c| c.is_ascii_digit()) {
        Some(w)
    } else {
        ordinal_day(w)
    }
}

/// A date not written per rule 3 but whose day (or month) is clear (#688).
///
/// Dates in contracts and notices are often written this way, and the model often copies them; before, these values were all dropped as "not a date".
/// Two kinds are accepted, with precision following the digits written: a date written only to the month is a month, and no day is filled in:
/// - Month written as a name: `March 17, 2020`, `17 March 2020`, `Mar. 17 2020`, `March 2020`.
///   Names are recognized by chrono's `%B` / `%b` (the full name or the three-letter abbreviation, case-insensitive)
/// - Year-first numbers: `2020/03/17`, `2020.3.17`, and Chinese-style year/month/day forms (`2020 year 3 month 17 day`, `2020 year 3 month`)
///
/// **Dates where day and month are both numbers and the year is not first are not accepted**: `03/04/2020` is March 4 or April 3, and the form itself cannot say which;
/// guessing wrong once gives a wrong deadline.
pub fn written_date(s: &str) -> Option<(NaiveDate, &'static str)> {
    let s = s.trim();
    if let Some(found) = year_first_date(s) {
        return found;
    }
    // Month written as a name: the period (after an abbreviation) and commas are just punctuation
    let words: Vec<&str> = s
        .split(|c: char| c.is_whitespace() || c == ',' || c == '.')
        .filter(|w| !w.is_empty())
        .collect();
    let is_number = |w: &str| w.chars().all(|c| c.is_ascii_digit());
    // The day may carry an ordinal suffix (March 17th, 2nd March); the month name does not
    let (day, month, year) = match words.as_slice() {
        [m, d, y] if !is_number(m) && day_digits(d).is_some() && is_number(y) => {
            (day_digits(d), *m, *y)
        }
        [d, m, y] if day_digits(d).is_some() && !is_number(m) && is_number(y) => {
            (day_digits(d), *m, *y)
        }
        [m, y] if !is_number(m) && is_number(y) => (None, *m, *y),
        _ => return None,
    };
    if year.len() != 4 || day.is_some_and(|d| d.len() > 2) {
        return None;
    }
    // The AP style shortens September to the four letters "Sept."; chrono's `%b` recognizes only three letters
    let month = if month.eq_ignore_ascii_case("sept") {
        "Sep"
    } else {
        month
    };
    let month = ["%B", "%b"].iter().find_map(|f| {
        NaiveDate::parse_from_str(&format!("1 {month} 2000"), &format!("%d {f} %Y"))
            .ok()
            .map(|d| chrono::Datelike::month(&d))
    })?;
    let year = year.parse().ok()?;
    match day {
        Some(d) => NaiveDate::from_ymd_opt(year, month, d.parse().ok()?).map(|date| (date, "day")),
        None => NaiveDate::from_ymd_opt(year, month, 1).map(|date| (date, "month")),
    }
}

/// Year-first numeric dates. Outer `None` = not this form, pass to the next form; `Some(None)` = this form,
/// but not a real day
fn year_first_date(s: &str) -> Option<Option<(NaiveDate, &'static str)>> {
    let marked = s.contains('年');
    let separated = s.contains(['/', '.']) && !s.contains(char::is_whitespace);
    if !marked && !separated {
        return None;
    }
    let parts: Vec<&str> = s
        .trim_end_matches('日')
        .split(['/', '.', '年', '月'])
        .filter(|p| !p.is_empty())
        .collect();
    if !parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    if parts.first().is_none_or(|y| y.len() != 4) {
        // Year not first: the day-month order cannot be determined, so do not hand it to another form to guess
        return Some(None);
    }
    let number = |p: &str| p.parse::<u32>().ok();
    let year = parts[0].parse::<i32>().ok()?;
    Some(match parts[1..] {
        [m, d] if m.len() <= 2 && d.len() <= 2 => {
            NaiveDate::from_ymd_opt(year, number(m)?, number(d)?).map(|date| (date, "day"))
        }
        // Only to the month: accepted only when "year" and "month" are written (`2020/03` looks too much like other things)
        [m] if marked && s.ends_with('月') && m.len() <= 2 => {
            NaiveDate::from_ymd_opt(year, number(m)?, 1).map(|date| (date, "month"))
        }
        // Only to the year: "1982 year". Counts only with the year marker; a bare `1982` goes to parse_time
        [] if marked && s.ends_with('年') => {
            NaiveDate::from_ymd_opt(year, 1, 1).map(|date| (date, "year"))
        }
        _ => None,
    })
}

/// The timezone after the clock time: `Z` or `±HH[:]MM` / `±HH`. Returns (clock time, offset from UTC);
/// no timezone returns None — the caller then records only the day
fn split_zone(clock: &str) -> Option<(&str, chrono::Duration)> {
    if let Some(c) = clock.strip_suffix(['Z', 'z']) {
        return Some((c, chrono::Duration::zero()));
    }
    let i = clock.rfind(['+', '-'])?;
    let (c, zone) = clock.split_at(i);
    let sign: i64 = if zone.starts_with('-') { -1 } else { 1 };
    let digits: String = zone[1..].chars().filter(|ch| ch.is_ascii_digit()).collect();
    let (hh, mm) = match digits.len() {
        2 => (&digits[..2], "0"),
        4 => (&digits[..2], &digits[2..]),
        _ => return None,
    };
    let (h, m): (i64, i64) = (hh.parse().ok()?, mm.parse().ok()?);
    Some((c, chrono::Duration::minutes(sign * (h * 60 + m))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #690: braces inside thinking text must not skew the start and end of the JSON. Without stripping the markers, the start of "the first `{`"
    /// would move earlier into the thinking; code fences and surrounding chatter likewise do not count
    #[test]
    fn json_block_ignores_a_think_block_and_a_fence_before_the_json() {
        let raw = "先想想 {\"a\": 1，再回答。\n</think>{\"entities\":[{\"name\":\"张三\"}]}";
        assert_eq!(
            json_block(raw).unwrap(),
            "{\"entities\":[{\"name\":\"张三\"}]}"
        );
        let fenced = "好的，结果如下：\n```json\n{\"entities\":[]}\n```";
        assert_eq!(json_block(fenced).unwrap(), "{\"entities\":[]}");
        assert!(json_block("no json here").is_err());
    }

    /// Brackets inside strings are not structure: `"a[b"` is not an open bracket; a piece cut mid-string is unusable
    #[test]
    fn brackets_inside_strings_are_not_structure() {
        assert_eq!(
            close_brackets(r#"{"s": [{"a": "a[b{c"}"#).as_deref(),
            Some(r#"{"s": [{"a": "a[b{c"}]}"#)
        );
        assert!(close_brackets(r#"{"s": "cut in the mid"#).is_none());
        assert!(close_brackets("]").is_none());
    }

    #[test]
    fn a_long_opening_is_cut_on_a_character_boundary() {
        let long = "租".repeat(OPENING_BUDGET_CHARS + 10);
        let block = opening_block(Some(&long));
        assert_eq!(block.matches('租').count(), OPENING_BUDGET_CHARS);
        assert!(block.contains(" …"));
        assert_eq!(opening_block(Some("   ")), "");
    }

    #[test]
    fn written_magnitudes_preserve_fractional_values() {
        for (input, expected) in [
            ("1.00000025 million", 1000000.25),
            ("100.000025万", 1000000.25),
            ("-1.00000025 million", -1000000.25),
            ("+100.000025万", 1000000.25),
            ("9.2亿", 920000000.0),
            ("0.00000000025 thousand", 0.00000025),
            ("0 million", 0.0),
            ("1,000.00025 thousand", 1000000.25),
        ] {
            assert_eq!(parse_quantity(input), Some((expected, None)), "{input}");
            assert_eq!(
                parse_leading_quantity(input),
                Some((expected, None)),
                "{input}"
            );
            assert_eq!(
                normalize_attr_value("number", &serde_json::json!(input)),
                Some(serde_json::json!(expected)),
                "{input}"
            );
        }
        assert_eq!(
            parse_quantity("$1.00000025 million"),
            Some((1000000.25, Some("$".into())))
        );
        assert_eq!(
            parse_leading_quantity("1.00000025 million people worldwide"),
            Some((1000000.25, Some("people".into())))
        );
        for rejected in [
            "3M",
            "5k",
            "1e3 million",
            "1.2.3 million",
            "--1 million",
            "1 million people",
        ] {
            assert_eq!(parse_quantity(rejected), None, "{rejected}");
        }
        assert_eq!(
            parse_quantity(&format!("{} trillion", "9".repeat(400))),
            None
        );
        assert_eq!(
            parse_quantity(&format!("{}1.00000025 million", "0".repeat(20_000))),
            Some((1000000.25, None))
        );
    }

    #[test]
    fn written_magnitudes_match_integer_decimal_oracles() {
        // The oracle shifts exact u128 integers, then parses an ordinary decimal.
        // No float multiplication or epsilon can erase a meaningful remainder.
        for (word, exponent) in [
            ("thousand", 3),
            ("万", 4),
            ("million", 6),
            ("千万", 7),
            ("亿", 8),
            ("billion", 9),
            ("trillion", 12),
        ] {
            for coefficient in [0u128, 1, 25, 100000025, 9200000000, 9007199254740991] {
                for places in 0..=15u32 {
                    let divisor = 10u128.pow(places);
                    let expanded = coefficient * 10u128.pow(exponent);
                    let decimal = |n: u128| {
                        if places == 0 {
                            n.to_string()
                        } else {
                            format!(
                                "{}.{:0width$}",
                                n / divisor,
                                n % divisor,
                                width = places as usize
                            )
                        }
                    };
                    for sign in ["", "-"] {
                        let input = format!("{sign}{} {word}", decimal(coefficient));
                        let expected: f64 = format!("{sign}{}", decimal(expanded)).parse().unwrap();
                        assert_eq!(parse_quantity(&input), Some((expected, None)), "{input}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_quantity_is_the_whole_string_or_nothing() {
        // The whole is one quantity: sign, magnitude word, thousands separators all readable
        assert_eq!(parse_quantity("$5 billion"), Some((5e9, Some("$".into()))));
        assert_eq!(
            parse_quantity("€1.5 million"),
            Some((1.5e6, Some("€".into())))
        );
        assert_eq!(parse_quantity("52%"), Some((52.0, Some("%".into()))));
        assert_eq!(parse_quantity("3.5 million"), Some((3.5e6, None)));
        assert_eq!(parse_quantity("35,000"), Some((35000.0, None)));
        assert_eq!(parse_quantity("  42 "), Some((42.0, None)));
        // Currency: symbol, ISO code, English and Chinese words, unified to a symbol; magnitude: English spelled out and Chinese thousand, ten-thousand, hundred-million
        assert_eq!(
            parse_quantity("EUR 30 million"),
            Some((3e7, Some("€".into())))
        );
        assert_eq!(
            parse_quantity("30 million euros"),
            Some((3e7, Some("€".into())))
        );
        assert_eq!(
            parse_quantity("USD 5 billion"),
            Some((5e9, Some("$".into())))
        );
        assert_eq!(parse_quantity("2亿美元"), Some((2e8, Some("$".into()))));
        assert_eq!(
            parse_quantity("15亿元人民币"),
            Some((1.5e9, Some("¥".into())))
        );
        assert_eq!(parse_quantity("3000万元"), Some((3e7, Some("¥".into()))));
        assert_eq!(parse_quantity("1.5亿"), Some((1.5e8, None)));
        // A number multiplied by a magnitude is collapsed to an integer: 9.2 hundred-million is not 919999999.9999999
        assert_eq!(
            parse_quantity("9.2亿元"),
            Some((920000000.0, Some("¥".into())))
        );
        assert_eq!(
            parse_quantity("$2.5 billion"),
            Some((2500000000.0, Some("$".into())))
        );

        // A content word still at the tail: the meaning is no longer just that number, so better a thing than a quantity
        assert_eq!(parse_quantity("900 million weekly active users"), None);
        assert_eq!(parse_quantity("2025 Atlantic hurricane season"), None);
        assert_eq!(parse_quantity("$10 billion investment"), None);
        assert_eq!(parse_quantity("8GW data center"), None);
        // Single-letter suffixes not recognized: 3M is a company, and reading it as three million would swallow a real entity
        assert_eq!(parse_quantity("3M"), None);
        assert_eq!(parse_quantity("5k"), None);
        // Two tokens colliding is not a quantity
        assert_eq!(parse_quantity("$5%"), None);
        assert_eq!(parse_quantity(""), None);
        assert_eq!(parse_quantity("杭州"), None);
    }

    #[test]
    fn a_declared_number_reads_past_the_unit() {
        // The attribute already declares datatype = number, and the question is "what is that number".
        // The two cases that once got stuck are both here
        assert_eq!(
            parse_leading_quantity("1,250 people"),
            Some((1250.0, Some("people".into())))
        );
        assert_eq!(
            parse_leading_quantity("42% from customers in Europe"),
            Some((42.0, Some("%".into())))
        );
        assert_eq!(
            parse_leading_quantity("3,400 people worldwide"),
            Some((3400.0, Some("people".into())))
        );
        assert_eq!(
            parse_leading_quantity("900 million weekly active users"),
            Some((9e8, Some("weekly".into())))
        );
        // A whole that is a quantity still takes the strict path: the unit is `$`, not `billion`
        assert_eq!(
            parse_leading_quantity("$5 billion"),
            Some((5e9, Some("$".into())))
        );
        // If the start is not a number, it is still not recognized
        // Currency is also accepted at the tail; only words that cannot be recognized fall through to "the unit is the first word"
        assert_eq!(
            parse_leading_quantity("30 million euros in cash"),
            Some((3e7, Some("€".into())))
        );
        assert_eq!(
            parse_leading_quantity("15亿元人民币的投资"),
            Some((1.5e9, Some("¥".into())))
        );
        assert_eq!(
            parse_leading_quantity("30 million francs"),
            Some((3e7, Some("francs".into())))
        );
        assert_eq!(parse_leading_quantity("about ten"), None);
        assert_eq!(parse_leading_quantity(""), None);

        // **The strict path is not loosened at all**: it must judge "is this one thing", and a wrong judgement would swallow a real entity
        assert_eq!(parse_quantity("1,250 people"), None);
        assert_eq!(parse_quantity("2025 Atlantic hurricane season"), None);
    }

    #[test]
    fn a_number_attribute_takes_a_written_quantity() {
        use serde_json::json;
        // When adopting an attribute, convert by datatype; the quantity must also convert, otherwise `$5 billion`
        // would keep failing to convert, and the fact would never get its predicate
        assert_eq!(
            normalize_attr_value("number", &json!("$5 billion")),
            Some(json!(5e9))
        );
        assert_eq!(
            normalize_attr_value("number", &json!("$2.5 Billion")),
            Some(json!(2.5e9))
        );
        assert_eq!(
            normalize_attr_value("number", &json!("52%")),
            Some(json!(52.0))
        );
        // A typeset minus sign is U+2212 (Wikipedia, typeset PDFs all write it this way); read the same as `-`
        assert_eq!(
            normalize_attr_value("number", &json!("−3.2%")),
            Some(json!(-3.2))
        );
        assert_eq!(
            normalize_attr_value("number", &json!("−1,250")),
            Some(json!(-1250.0))
        );
        assert_eq!(
            attribute_value("number", Some("万元"), &json!("−2.3亿元")),
            Some(json!(-23000.0))
        );
        // The two forms already recognized before are unaffected
        assert_eq!(
            normalize_attr_value("number", &json!("35,000")),
            Some(json!(35000.0))
        );
        assert_eq!(normalize_attr_value("number", &json!("about ten")), None);
    }

    #[test]
    fn parse_time_precisions() {
        assert_eq!(parse_time("2024").unwrap().1, "year");
        assert_eq!(parse_time("2024-07").unwrap().1, "month");
        assert_eq!(parse_time("2024-07-15").unwrap().1, "day");
        // A clock time with timezone: to the minute, to the hour, to the second; the value is truncated to that place, and the offset is converted back to UTC
        let (t, p) = parse_time("2026-06-01T14:32Z").unwrap();
        assert_eq!(
            (t.to_rfc3339(), p),
            ("2026-06-01T14:32:00+00:00".to_string(), "minute")
        );
        assert_eq!(parse_time("2026-06-01T14Z").unwrap().1, "hour");
        let (t, p) = parse_time("2026-06-01T14:32:07.382+08:00").unwrap();
        assert_eq!(
            (t.to_rfc3339(), p),
            ("2026-06-01T06:32:07+00:00".to_string(), "second")
        );
        // A clock time without a timezone is not a moment: only the day is recorded
        let (t, p) = parse_time("2026-06-01T14:32").unwrap();
        assert_eq!(
            (t.to_rfc3339(), p),
            ("2026-06-01T00:00:00+00:00".to_string(), "day")
        );
        assert!(parse_time("null").is_none());
        assert!(parse_time("").is_none());
        assert!(parse_time("下个月").is_none());
        // Forms outside the contract format are not its job: "August 2024" in tool arguments is not guessed
        assert!(parse_time("June 23, 2020").is_none());
        // Reading the one model-reply form that writes a date (#688), with precision following how many digits were written
        let (t, p) = read_time("June 23, 2020").unwrap();
        assert_eq!(
            (t.to_rfc3339(), p),
            ("2020-06-23T00:00:00+00:00".to_string(), "day")
        );
        assert_eq!(read_time("March 2020").unwrap().1, "month");
        assert_eq!(read_time("2024-07").unwrap().1, "month");
        assert!(read_time("03/04/2020").is_none());
    }

    /// Date forms in contracts and notices (#688): those whose day is clear are converted to rule 3's form; those that cannot be determined are not guessed
    #[test]
    fn a_written_date_is_read_only_when_its_form_says_which_day() {
        use serde_json::json;
        let day = |s: &str| normalize_attr_value("date", &json!(s));
        for written in [
            "March 17th, 2020",
            "March 17, 2020",
            "March 17 2020",
            "march 17, 2020",
            "MARCH 17, 2020",
            "Mar 17, 2020",
            "Mar. 17, 2020",
            "17 March 2020",
            "17 Mar. 2020",
            "17 March, 2020",
            "  March 17, 2020 ",
            "2020/03/17",
            "2020.3.17",
            "2020年3月17日",
        ] {
            assert_eq!(day(written), Some(json!("2020-03-17")), "{written}");
        }
        for (written, iso) in [
            ("2nd March 2020", "2020-03-02"),
            ("March 11th, 2020", "2020-03-11"),
            ("March 21st, 2020", "2020-03-21"),
            ("March 22nd, 2020", "2020-03-22"),
            ("March 23rd, 2020", "2020-03-23"),
        ] {
            assert_eq!(day(written), Some(json!(iso)), "{written}");
        }
        // A date written only to the month stays a month; no day is filled in
        for written in ["March 2020", "Mar. 2020", "2020年3月"] {
            assert_eq!(day(written), Some(json!("2020-03")), "{written}");
        }
        // Already in rule 3's form: keep as is
        assert_eq!(day("2020-03-17"), Some(json!("2020-03-17")));
        assert_eq!(day("2020"), Some(json!("2020")));
        // Day and month both numbers, year not first: cannot tell which month and day
        for ambiguous in ["03/04/2020", "3.4.2020", "04-03-2020", "3/4/20"] {
            assert_eq!(day(ambiguous), None, "{ambiguous}");
        }
        // Not a date, or not a real day
        for not_a_date in [
            "45 days after the Trigger Date",
            "Q3 2020",
            "Sometime 2020",
            "February 30, 2020",
            "March 17, 20",
            "March 123, 2020",
            "2020/13/01",
            "2020/03",
            "next March",
        ] {
            assert_eq!(day(not_a_date), None, "{not_a_date}");
        }
        // Range endpoints are read by the same parser
        assert_eq!(
            read_time("17 Mar 2020").map(|(t, p)| (t.date_naive().to_string(), p)),
            Some(("2020-03-17".to_string(), "day"))
        );
    }

    #[test]
    fn september_abbreviated_as_sept_is_read() {
        use serde_json::json;
        let day = |s: &str| normalize_attr_value("date", &json!(s));
        for written in [
            "Sept. 17, 2020",
            "Sept 17, 2020",
            "17 Sept. 2020",
            "SEPT. 17, 2020",
        ] {
            assert_eq!(day(written), Some(json!("2020-09-17")), "{written}");
        }
        assert_eq!(day("Sept. 2020"), Some(json!("2020-09")));
        assert_eq!(
            read_time("Sept. 30, 2025").map(|(t, p)| (t.date_naive().to_string(), p)),
            Some(("2025-09-30".to_string(), "day"))
        );
    }

    #[test]
    fn a_similarity_proposed_pair_says_so_in_the_prompt() {
        let side = |name: &str| AdjudicationSide {
            name: name.into(),
            type_label: "person".into(),
            facts: vec![],
        };
        let pairs = vec![
            AdjudicationPair {
                left: side("张伟"),
                right: side("财务部总监张伟"),
                precedents: vec![],
                proposed_because: proposed_because(Some("0.78")),
            },
            AdjudicationPair {
                left: side("张伟"),
                right: side("张伟"),
                precedents: vec![],
                proposed_because: proposed_because(None),
            },
        ];
        let user = &build_adjudication_messages(&pairs)[1].content;
        let first = &user[..user.find("Pair 1:").unwrap()];
        let second = &user[user.find("Pair 1:").unwrap()..];
        assert!(
            first.contains("Why paired:") && first.contains("cosine 0.78"),
            "{first}"
        );
        assert!(
            !second.contains("Why paired:"),
            "同名对不该带这一行：{second}"
        );
    }

    #[test]
    fn a_proposal_note_needs_a_cosine() {
        assert!(proposed_because(Some("0.62")).is_some());
        assert!(proposed_because(None).is_none());
    }

    #[test]
    fn parse_adjudication_reply() {
        let raw = "```json\n{\"verdicts\":[{\"i\":0,\"verdict\":\"same\",\"confidence\":0.92},{\"i\":1,\"verdict\":\"unsure\"}]}\n```";
        let v = parse_adjudication(raw).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].verdict, "same");
        assert_eq!(v[1].confidence, None);
    }

    /// A well-formed reply is not counted as repaired: the verdict is identical to `parse_adjudication`
    #[test]
    fn a_well_formed_reply_is_not_repaired() {
        let raw = "{\"verdicts\":[{\"i\":0,\"verdict\":\"same\",\"confidence\":0.92,\"why\":\"one line\"}]}";
        let (v, repaired) = parse_adjudication_repairing(raw).unwrap();
        assert!(!repaired);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].why.as_deref(), Some("one line"));
        assert_eq!(v[0].confidence, Some(0.92));
        let plain = parse_adjudication(raw).unwrap();
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].why, v[0].why);
    }

    /// The model writes a line break directly inside the quoted reason (gemini-3.5-flash through the gateway writes it this way):
    /// serde reports a control character; after repair the batch is read as usual, the line break stays in the reason, and it is marked as repaired
    #[test]
    fn a_raw_newline_inside_a_reason_string_is_repaired() {
        let raw = "```json\n{\"verdicts\":[\n  {\"i\":0,\"verdict\":\"different\",\"confidence\":0.9,\n   \"reason\":\"Rule 2: a version is not the series.\nRecord A is Claude 3, Record B is the family.\",\n   \"why\":\"a version\nis not the series\"},\n  {\"i\":1,\"verdict\":\"same\",\"confidence\":0.8,\"why\":\"tab\there\"}\n]}\n```";
        assert!(parse_adjudication_repairing(
            "{\"verdicts\":[{\"i\":0,\"verdict\":\"same\",\"why\":\"a\""
        )
        .is_err());
        let (v, repaired) = parse_adjudication_repairing(raw).unwrap();
        assert!(repaired);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].verdict, "different");
        assert_eq!(v[0].why.as_deref(), Some("a version\nis not the series"));
        assert_eq!(v[1].why.as_deref(), Some("tab\there"));
        // Calling `parse_adjudication` directly reads it the same way
        assert_eq!(parse_adjudication(raw).unwrap().len(), 2);
    }

    /// Escaping touches only what is inside strings: a line break outside quotes is legal whitespace; `"` does not end a string; an already-escaped `\n` is left alone
    #[test]
    fn only_control_characters_inside_strings_are_escaped() {
        let text =
            "{\n  \"a\": \"x\ny\",\n  \"b\": \"q\\\"\nz\",\n  \"c\": \"already\\nescaped\"\n}";
        let fixed = escape_control_in_strings(text);
        assert_eq!(
            fixed,
            "{\n  \"a\": \"x\\ny\",\n  \"b\": \"q\\\"\\nz\",\n  \"c\": \"already\\nescaped\"\n}"
        );
        let v: serde_json::Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(v["a"], "x\ny");
        assert_eq!(v["b"], "q\"\nz");
        assert_eq!(v["c"], "already\nescaped");
        let clean = "{\"a\": \"fine\"}";
        assert_eq!(escape_control_in_strings(clean), clean);
    }

    #[test]
    fn normalize_attr_values() {
        use serde_json::json;
        assert_eq!(
            normalize_attr_value("number", &json!("35,000")),
            Some(json!(35000.0))
        );
        // Integers in JSON also become the same kind of number: `42` and "42" parse to the same value
        assert_eq!(
            normalize_attr_value("number", &json!(42)),
            Some(json!(42.0))
        );
        assert_eq!(normalize_attr_value("number", &json!("about ten")), None);
        assert_eq!(
            normalize_attr_value("date", &json!("2024-07")),
            Some(json!("2024-07"))
        );
        assert_eq!(normalize_attr_value("date", &json!("下个月")), None);
        assert_eq!(
            normalize_attr_value("bool", &json!("yes")),
            Some(json!(true))
        );
        assert_eq!(
            normalize_attr_value("text", &json!(" CTO ")),
            Some(json!("CTO"))
        );
        assert_eq!(normalize_attr_value("text", &json!([1])), None);
    }

    /// In financial tables a number in parentheses is negative: before, the whole cell was unreadable and the value was lost
    #[test]
    fn an_accounting_negative_is_read_as_negative() {
        use serde_json::json;
        let n = |s: &str| normalize_attr_value("number", &json!(s));
        assert_eq!(n("(1,234)"), Some(json!(-1234.0)));
        assert_eq!(n("$(1.2) billion"), Some(json!(-1200000000.0)));
        assert_eq!(n("(12.5%)"), Some(json!(-12.5)));
        assert_eq!(
            attribute_value("number", Some("万元"), &json!("(350)万元")),
            Some(json!(-350.0))
        );
        // Parenthesized text that is not a number, or not preceded by a currency, is still unreadable
        assert_eq!(n("(see note 4)"), None);
        assert_eq!(n("Q3 (12)"), None);
        // A number following in parentheses: the parentheses are numbering or a year, not a minus sign
        assert_eq!(n("(1) 1,200 employees"), None);
        assert_eq!(n("(2024) 500"), None);
        // Object judgement does not change with this: a named "(1984)" is still a thing
        assert!(!literal_object("(1984)", true));
    }
}

#[cfg(test)]
mod a_number_is_one_number {
    use super::normalize_attr_value;
    use serde_json::json;

    /// Whether the model writes `65` or "65%", both come out as the same number; otherwise one edge would record a conflict
    #[test]
    fn a_number_is_one_number_however_it_is_written() {
        let a = normalize_attr_value("number", &json!(65)).unwrap();
        let b = normalize_attr_value("number", &json!("65%")).unwrap();
        let c = normalize_attr_value("number", &json!("65")).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a.as_f64(), Some(65.0));
    }
}

#[cfg(test)]
mod an_attribute_value_is_read_in_its_declared_unit {
    use super::{attribute_value, literal_object};
    use serde_json::json;

    fn value(datatype: &str, unit: Option<&str>, raw: &str) -> Option<serde_json::Value> {
        attribute_value(datatype, unit, &json!(raw))
    }

    /// The most ordinary sentences in company profiles: the value is kept as written, and on the attribute it should be that number and that day
    #[test]
    fn plain_company_figures_are_read() {
        assert_eq!(
            value("number", Some("万元"), "5000万元"),
            Some(json!(5000.0))
        );
        assert_eq!(
            value("number", Some("万元"), "2.3亿元"),
            Some(json!(23000.0))
        );
        assert_eq!(value("number", Some("亿元"), "8.6亿元"), Some(json!(8.6)));
        assert_eq!(
            value("number", Some("万元"), "50000000元"),
            Some(json!(5000.0))
        );
        // A bare number is a number in the declared unit
        assert_eq!(value("number", Some("万元"), "5000"), Some(json!(5000.0)));
        assert_eq!(value("number", Some("人"), "约350名"), Some(json!(350.0)));
        assert_eq!(
            value("number", Some("人"), "Approximately 1,200"),
            Some(json!(1200.0))
        );
        assert_eq!(
            value("number", Some("people"), "About 350 people"),
            Some(json!(350.0))
        );
        // A date written only to the year is by year
        assert_eq!(value("date", None, "1982年"), Some(json!("1982")));
    }

    /// Financial-report units often put the currency before the magnitude: "USD million" or "€ million", same as "million dollars"
    #[test]
    fn a_unit_with_its_currency_first_is_still_read_in_millions() {
        assert_eq!(
            value("number", Some("USD million"), "$5 billion"),
            Some(json!(5000.0))
        );
        assert_eq!(
            value("number", Some("€ million"), "€30 million"),
            Some(json!(30.0))
        );
        assert_eq!(value("number", Some("USD million"), "€30 million"), None);
        assert_eq!(
            value("number", Some("million dollars"), "$5 billion"),
            Some(json!(5000.0))
        );
    }

    /// A number followed by a word for people is a count of people, so on "ten-thousand people" it must be divided down
    #[test]
    fn a_counted_figure_is_read_into_a_larger_unit() {
        assert_eq!(value("number", Some("万人"), "12,000人"), Some(json!(1.2)));
        assert_eq!(value("number", Some("万人"), "约3500名"), Some(json!(0.35)));
        assert_eq!(
            value("number", Some("thousand people"), "1,250 employees"),
            Some(json!(1.25))
        );
        assert_eq!(
            value("number", Some("万人"), "1400万人"),
            Some(json!(1400.0))
        );
        assert_eq!(value("number", Some("万人"), "1.2"), Some(json!(1.2)));
    }

    /// Those that cannot be converted are not converted: currency mismatch, or a number that cannot be read; prefer missing to dirty
    #[test]
    fn what_cannot_be_read_is_not_written() {
        assert_eq!(value("number", Some("万元"), "5000万美元"), None);
        assert_eq!(value("number", Some("人"), "超过一千人"), None);
        assert_eq!(value("date", None, "2023年底"), None);
        assert_eq!(value("bool", None, "尚未上市"), None);
    }

    /// Amounts in Chinese financial text often use the full-width yen sign (U+FFE5), the same symbol as "¥"
    #[test]
    fn a_full_width_yen_sign_is_read_as_the_currency() {
        assert_eq!(value("number", None, "￥5,000万"), Some(json!(5e7)));
        assert_eq!(
            value("number", Some("万元"), "￥5,000万"),
            Some(json!(5000.0))
        );
        assert_eq!(value("number", Some("元"), "￥1,280"), Some(json!(1280.0)));
        assert_eq!(value("number", Some("万美元"), "￥5,000万"), None);
        assert!(literal_object("￥5000万", true));
    }

    #[test]
    fn a_figure_or_a_date_is_not_a_thing() {
        for v in [
            "5000万元",
            "8.6亿元",
            "1200人",
            "约350名",
            "2015年3月12日",
            "1978年5月",
            "2020年",
            "2015-03-12",
            "$5 billion",
            "52%",
        ] {
            assert!(literal_object(v, true), "{v}");
        }
        // A bare number: an unnamed one is a value; a named one is most likely a thing ("1984" the novel, 360)
        assert!(literal_object("1200", false));
        assert!(!literal_object("1984", true));
        for name in ["3M", "7天", "2023年底", "星河科技", "董事长兼总经理", "K2"] {
            assert!(!literal_object(name, false), "{name}");
        }
    }
}
