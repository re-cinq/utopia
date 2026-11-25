//! Reading time references (0045 decisions 2, 3, 8): **the model reads, the code computes**.
//!
//! Only structured fields come back; the model never writes a computed date:
//!
//! **Reading references** ([`build_interpretation_messages`] / [`parse_interpretation_response`]):
//! each reference gives its shape, quote and granularity. Absolute values are copied from the source as numeric parts; relative ones name an anchor (another reference,
//! the document itself, a period the document names) and an offset (number, unit, direction); periods name themselves. Calendar arithmetic,
//! fiscal years to intervals, truncation by granularity, and intervals implied by a shape are all computed on the server.
//!
//! The document's own dates are no longer asked of the model separately: each block reports the date its text states during extraction (0064).
//!
//! The code has **no time words** (decision 8): it does not recognize month names or "last year" or "end of last year"; it only parses JSON, checks
//! part ranges, and assembles prompts. The prompts are constant English; the examples are neutral (town council, bridge, bakery) and come from no
//! measured corpus.
//!
//! Part keys are short: `y` year, `m` month, `d` day, `h` hour, `min` minute, `s` second; only the parts the source actually mentions are written
//! (e.g. "2024" is `{"y": 2024}`). A finer part must come with the coarser ones (a day implies a month); this is
//! the precision ladder of 0024.
//!
//! Parsing is lenient per item: bad entries are counted and do not ruin the batch; the caller **must** report the count (the kind of bug in #108).
//! A truncated reply falls back to the last complete entry and closes the brackets, using the same repair as `open.rs`.

use std::collections::HashSet;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utopia_llm::ChatMessage;

use crate::open::repair_truncated_compact;
use crate::{json_block, json_text};

/// Date parts the source mentions; `Some` only where the source wrote them. When stored as JSONB, full-name keys are used
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateParts {
    pub year: i32,
    /// The quarter within the year as written ("2025 Q3", "Q3 2025"), 1–4; if present there is no month.
    /// Quarter-to-month is calendar arithmetic and belongs to code (0045 decision 2): the model only copies the number "third"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarter: Option<u32>,
    pub month: Option<u32>,
    pub day: Option<u32>,
    pub hour: Option<u32>,
    pub minute: Option<u32>,
    pub second: Option<u32>,
}

impl DateParts {
    /// The finest level the parts reach: a day if there is a day, a year if only a year. The server compares it with the granularity the model reports;
    /// references that disagree go to review (the last item of the "not done" list)
    pub fn granularity(&self) -> Granularity {
        if self.second.is_some() {
            Granularity::Second
        } else if self.minute.is_some() {
            Granularity::Minute
        } else if self.hour.is_some() {
            Granularity::Hour
        } else if self.day.is_some() {
            Granularity::Day
        } else if self.month.is_some() || self.quarter.is_some() {
            Granularity::Month
        } else {
            Granularity::Year
        }
    }
}

/// Formatting only, no calculation: `2011`, `2011-03`, `2011-03-04`, `2011-03-04 14:30`
impl fmt::Display for DateParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.year)?;
        if let Some(q) = self.quarter {
            write!(f, "-Q{q}")?;
        }
        if let Some(m) = self.month {
            write!(f, "-{m:02}")?;
        }
        if let Some(d) = self.day {
            write!(f, "-{d:02}")?;
        }
        if let Some(h) = self.hour {
            write!(f, " {h:02}")?;
        }
        if let Some(mi) = self.minute {
            write!(f, ":{mi:02}")?;
        }
        if let Some(s) = self.second {
            write!(f, ":{s:02}")?;
        }
        Ok(())
    }
}

/// A period the document names and its bounds ("fiscal 2019" to September 30; "2024 year" as the reporting year)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedPeriod {
    pub name: String,
    pub from: DateParts,
    pub to: DateParts,
}

/// A date the document states, with its name and what it governs (0064 decision 1). Reported with each block during extraction;
/// the server checks the words are in the block, computes the heading path it sits under, and stores it in the document's time context.
///
/// A document can have more than one date, and none outranks the others: the reporting date, the data cutoff and the effective date each govern their own things.
/// Relative time words are counted from whichever `now` governs the section they sit in ([`now_in_force`])
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeEntry {
    /// `now`: the moment the text speaks from (dateline, report submission date, meeting date);
    /// `date`: another date the text names (cutoff, effective date, deadline); `period`: a named period
    pub kind: String,
    /// the label as written ("Date of report", "Data as of"); empty for a bare dateline
    pub name: String,
    /// the date words as written
    pub words: String,
    pub from: DateParts,
    pub to: Option<DateParts>,
    /// the headings the entry was stated under, outermost first; it governs what sits under them
    #[serde(default)]
    pub scope: Vec<String>,
    /// the chunk that stated it and where in it (set by the server)
    #[serde(default)]
    pub chunk: Option<String>,
    #[serde(default)]
    pub char_start: Option<i32>,
}

/// The headings still in force before this character position in the block's body, from outermost to innermost.
///
/// The criterion is structural: a `#` run at the start of a line is the heading's level, and older headings of the same or deeper level are replaced by the new heading. The chunker makes
/// each block start with the heading it sits under, so a block's own body can say which section it is in, without depending on the order of blocks;
/// this holds even when blocks are extracted in parallel (#588)
pub fn headings_at(text: &str, char_pos: usize) -> Vec<String> {
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut seen = 0usize;
    let mut fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        if seen > char_pos {
            break;
        }
        let indentation = line.chars().take_while(|c| *c == ' ').count();
        let trimmed = line.trim();
        let marker = trimmed.chars().next();
        let markers = marker.map_or(0, |c| trimmed.chars().take_while(|x| *x == c).count());
        if let Some((kind, width)) = fence {
            if indentation <= 3
                && marker == Some(kind)
                && markers >= width
                && trimmed[markers..].trim().is_empty()
            {
                fence = None;
            }
            seen += line.chars().count();
            continue;
        }
        if indentation <= 3
            && matches!(marker, Some('`' | '~'))
            && markers >= 3
            && (marker != Some('`') || !trimmed[markers..].contains('`'))
        {
            fence = marker.map(|c| (c, markers));
            seen += line.chars().count();
            continue;
        }
        if indentation >= 4 || line.starts_with('\t') {
            seen += line.chars().count();
            continue;
        }
        let level = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&level) {
            let title = trimmed[level..].trim();
            if !title.is_empty() && trimmed[level..].starts_with(char::is_whitespace) {
                stack.retain(|(l, _)| *l < level);
                stack.push((level, title.to_string()));
            }
        }
        seen += line.chars().count();
    }
    stack.into_iter().map(|(_, t)| t).collect()
}

/// The `now` in force at this position: among those whose heading path starts within its scope, the one with the deepest scope;
/// if several are at the same level, the one stated first. If none, it is `None`: a relative time word has no anchor and waits (0045 decision 4)
pub fn now_in_force<'a>(entries: &'a [TimeEntry], path: &[String]) -> Option<&'a TimeEntry> {
    let mut best: Option<&TimeEntry> = None;
    for e in entries.iter().filter(|e| e.kind == "now") {
        if e.scope.len() <= path.len()
            && e.scope.iter().zip(path).all(|(a, b)| a == b)
            && best.is_none_or(|b| e.scope.len() > b.scope.len())
        {
            best = Some(e);
        }
    }
    best
}

/// One `t` in an extraction reply: `[kind, name, words, from, to]`. Parts are numbers copied from the text; those out of range,
/// or missing words, are bad entries
pub fn time_entry(item: &Value) -> Option<TimeEntry> {
    let a = item.as_array()?;
    let text = |i: usize| a.get(i).and_then(Value::as_str).map(str::trim);
    let kind = text(0)?.to_lowercase();
    if !matches!(kind.as_str(), "now" | "date" | "period") {
        return None;
    }
    let words = text(2).filter(|w| !w.is_empty())?;
    let from = parts(a.get(3)?)?;
    let to = match a.get(4) {
        None | Some(Value::Null) => None,
        Some(v) => Some(parts(v)?),
    };
    Some(TimeEntry {
        kind,
        name: text(1).unwrap_or("").to_string(),
        words: words.to_string(),
        from,
        to,
        scope: Vec::new(),
        chunk: None,
        char_start: None,
    })
}

/// The document's own time context (decision 3): read from the opening, stored on the document, carried by every block
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DocumentDating {
    /// Dates reported by each block during extraction (0064): when present, the opening is not asked about again
    pub entries: Vec<TimeEntry>,
    /// the document's own date as the text states it (a dateline, a filing date, a bulletin's
    /// reporting year); None when the text gives none
    pub date: Option<DateParts>,
    /// the words that state it, verbatim (the server checks they occur in the opening)
    pub date_words: Option<String>,
    /// periods the document names with bounds it states ("fiscal 2027" with its end date;
    /// "2024 year" as a reporting year)
    pub periods: Vec<NamedPeriod>,
    /// (month, day) on which the document's fiscal year ends, when stated
    pub fiscal_year_end: Option<(u32, u32)>,
    /// items the reply wrote but that were malformed (a date with month 13, a period without
    /// bounds); must be reported by the caller
    pub skipped: usize,
}

/// What the reference did to the statement it governs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    Point,
    Since,
    Until,
    Interval,
    AsOf,
    Duration,
    EndedUnknown,
}

/// How far the source's words reach on the ladder (0024), recorded separately from the quote
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    Year,
    Month,
    /// The ladder has no "week" (0024): a reference that mentions a week ("last week") falls to the day level. The model reports week as written;
    /// before, the whole interpretation was discarded as a bad entry, so "last week" never got a date
    #[serde(alias = "week")]
    Day,
    Hour,
    Minute,
    Second,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Before,
    After,
}

/// Offset as written: "two years later" = 2 years after; "this quarter" = 0 quarter
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offset {
    pub count: i64,
    pub unit: Unit,
    pub direction: Direction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Anchor {
    Mention { id: i64 },
    Document,
    Period { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reference {
    /// the words state the value; `to` only for an interval written out ("from March to May 2024")
    Absolute {
        from: DateParts,
        to: Option<DateParts>,
    },
    /// relative to an anchor, with an offset when the words give one ("two years later"); no
    /// offset = the anchor itself ("today")
    Anchored {
        anchor: Anchor,
        offset: Option<Offset>,
    },
    /// a named period from the context or one whose bounds the text states here
    Period {
        name: String,
        from: Option<DateParts>,
        to: Option<DateParts>,
    },
    None,
}

/// The interpretation of one reference; stored as JSONB, the server computes intervals from it
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interpretation {
    pub id: i64,
    pub shape: Shape,
    pub reference: Reference,
    pub granularity: Granularity,
}

/// A reference sent for interpretation: number, the words of the source, and the sentence it sits in (the model picks the anchor from the sentence)
#[derive(Debug, Clone, Copy)]
pub struct MentionInput<'a> {
    pub id: i64,
    pub text: &'a str,
    pub sentence: &'a str,
}

/// Dating context for the prompt: a borrowed view of [`DocumentDating`]
#[derive(Debug, Clone, Copy)]
pub struct TimeContext<'a> {
    pub date: Option<&'a DateParts>,
    pub date_words: Option<&'a str>,
    pub periods: &'a [NamedPeriod],
    pub fiscal_year_end: Option<(u32, u32)>,
}

/// System message for reading references. Each shape on its own line; a quote is what is said literally, not what it implies; never compute
const INTERPRETATION_SYSTEM: &str = "\
You interpret the time expressions of a document. Each mention is given with its id, its words \
exactly as written and the sentence it occurs in; before them, the document's own time context: \
its date and the words that state it, the periods it names with their bounds, and the day its \
fiscal year ends. For each mention you write what its words state — a shape, a reference and a \
granularity — and never a date you computed: the reader computes dates from what you write. \
Output one JSON object and nothing else, shaped like this:\n\
\n\
{\"m\": [[id, \"shape\", reference, \"granularity\"]]}\n\
\n\
1. shape is what the time does to the statement it dates, one of:\n\
   \"point\" — the statement happened or holds at that time;\n\
   \"since\" — it holds from that time on;\n\
   \"until\" — it holds up to that time;\n\
   \"interval\" — it holds between two bounds;\n\
   \"as_of\" — the state observed at that time;\n\
   \"duration\" — a length of time with no position;\n\
   \"ended_unknown\" — the words say it ended but not when.\n\
2. reference is what the words say, not what they imply, in one of four forms:\n\
   {\"kind\": \"absolute\", \"from\": {\"y\": 2011, \"m\": 3, \"d\": 4}} — the words state the \
value. Parts are transcribed digits: y the year, m the month (a month name becomes its number), \
d the day, and h, min, s for a clock time; write only the parts the words state. A bare year \
(\"2019\") is {\"y\": 2019} with granularity \"year\". A quarter of a stated year is written \
with q, the number of the quarter as the words give it, and no month: \"Q3 2025\" and \"2025年\
第三季度\" are {\"y\": 2025, \"q\": 3} with granularity \"month\". \"to\" is added only for an interval the \
words write out with both bounds (\"from March to May 2024\": from {\"y\": 2024, \"m\": 3}, to \
{\"y\": 2024, \"m\": 5}).\n\
   {\"kind\": \"anchored\", \"anchor\": {\"kind\": \"document\"}, \"offset\": {\"count\": 1, \
\"unit\": \"year\", \"direction\": \"before\"}} — the words point at another time. The anchor is \
the document itself, {\"kind\": \"document\"}, for \"today\", \"now\", \"this quarter\", \"last \
year\", \"the prior year\"; the mention the sentence counts from, {\"kind\": \"mention\", \
\"id\": 3}, for \"two years later\", \"three months earlier\", \"the following day\" when the \
sentence names that time, and the document when it names none; a period from the context, \
{\"kind\": \"period\", \"name\": \"fiscal 2019\"}, for \"the end of fiscal 2019\". offset is the \
count, unit and direction the words state: \"two years later\" is count 2, unit \"year\", \
direction \"after\"; \"this quarter\" and \"this year\" are count 0 with that unit; \"today\" \
and \"now\" have no offset. unit is one of year, quarter, month, week, day, hour, minute, \
second; direction is before or after.\n\
   {\"kind\": \"period\", \"name\": \"Q2 fiscal 2019\"} — the words name a period. One listed in \
the context is named exactly as listed. One whose bounds the words state here carries them: a \
heading \"Three months ended June 30, 2019\" is {\"kind\": \"period\", \"name\": \"Three months \
ended June 30, 2019\", \"to\": {\"y\": 2019, \"m\": 6, \"d\": 30}}, with \"from\" only when the \
words state it. Words that name a period only relatively (the whole year, the end of the year, \
the start of the month, this quarter) are not a period: they are anchored to the document with \
count 0 and that unit; the end of it takes shape \"as_of\" or \"until\", the start \"since\", the \
whole \"interval\". A count inside something the sentence names (a week of a trial, a month of a \
programme) anchors to that thing's dated mention when the sentence gives one, otherwise it is \
{\"kind\": \"none\"}: it is not counted from the document.\n\
   {\"kind\": \"none\"} — the words give nothing to anchor to: a duration (\"ten years\"), an \
ending without a date (\"formerly\", \"no longer\"), a vague time (\"recently\").\n\
3. granularity is the rung the words reach: \"year\", \"month\", \"day\", \"hour\", \"minute\" \
or \"second\". \"March 2024\" is month; \"two years later\" is year; \"today\" is day; a quarter \
or a season is month.\n\
4. Never compute: do not resolve \"last year\" to a year, do not add an offset to a date, do not \
turn a period into dates, do not use today's date. Write one item per mention, with the id from \
the list, and no item for an id not listed.\n\
\n\
Example. With the document dated March 4, 2011 and the mentions [id 1 \"March 4, 2011\" in \"The \
council met on March 4, 2011.\", id 2 \"since 2005\" in \"Harbor Bakery has supplied the school \
since 2005.\", id 3 \"2019\" and id 4 \"two years later\" in \"The bridge opened in 2019; two \
years later the eastern span was widened.\", id 5 \"last year\" in \"Last year the bakery opened \
a second shop.\"]:\n\
{\"m\": [[1, \"point\", {\"kind\": \"absolute\", \"from\": {\"y\": 2011, \"m\": 3, \"d\": 4}}, \"day\"],\n\
 [2, \"since\", {\"kind\": \"absolute\", \"from\": {\"y\": 2005}}, \"year\"],\n\
 [3, \"point\", {\"kind\": \"absolute\", \"from\": {\"y\": 2019}}, \"year\"],\n\
 [4, \"point\", {\"kind\": \"anchored\", \"anchor\": {\"kind\": \"mention\", \"id\": 3}, \"offset\": {\"count\": 2, \"unit\": \"year\", \"direction\": \"after\"}}, \"year\"],\n\
 [5, \"point\", {\"kind\": \"anchored\", \"anchor\": {\"kind\": \"document\"}, \"offset\": {\"count\": 1, \"unit\": \"year\", \"direction\": \"before\"}}, \"year\"]]}";

/// Two messages for reading references: constant system message + context block + list of references (number, words of the source, sentence)
pub fn build_interpretation_messages(
    context: &TimeContext<'_>,
    mentions: &[MentionInput<'_>],
) -> Vec<ChatMessage> {
    let mut user = String::from("Document time context:\n");
    match (context.date, context.date_words) {
        (Some(date), Some(words)) => {
            user.push_str(&format!(
                "- date: {date}, stated by the words \"{words}\"\n"
            ));
        }
        (Some(date), None) => user.push_str(&format!("- date: {date}\n")),
        (None, _) => user.push_str(
            "- date: not stated; still anchor to the document where the words point at it\n",
        ),
    }
    if context.periods.is_empty() {
        user.push_str("- periods: none named\n");
    } else {
        let listed: Vec<String> = context
            .periods
            .iter()
            .map(|p| format!("\"{}\" from {} to {}", p.name, p.from, p.to))
            .collect();
        user.push_str(&format!("- periods: {}\n", listed.join("; ")));
    }
    match context.fiscal_year_end {
        Some((m, d)) => user.push_str(&format!("- fiscal year end: month {m}, day {d}\n")),
        None => user.push_str("- fiscal year end: not stated\n"),
    }
    user.push_str("\nMentions:\n");
    for m in mentions {
        user.push_str(&format!(
            "- id {}, words \"{}\", in the sentence: \"{}\"\n",
            m.id, m.text, m.sentence
        ));
    }
    vec![
        ChatMessage {
            role: "system".into(),
            content: INTERPRETATION_SYSTEM.to_string(),
        },
        ChatMessage {
            role: "user".into(),
            content: user,
        },
    ]
}

/// Parse as usual first; if that fails, take from the first `{` to the end and back up to the last complete entry to close the brackets
/// (as in `open.rs`: in a compact reply the last `}` is not a reliable end)
fn reply_value(raw: &str, what: &str) -> anyhow::Result<Value> {
    let block = json_block(raw)
        .and_then(|b| serde_json::from_str::<Value>(&b).map_err(anyhow::Error::from));
    match block {
        Ok(v) => Ok(v),
        Err(e) => {
            let text = json_text(raw);
            let fixed = text
                .find('{')
                .and_then(|s| repair_truncated_compact(&text[s..]))
                .ok_or_else(|| anyhow::anyhow!("Failed to parse {what} JSON: {e}"))?;
            serde_json::from_str::<Value>(&fixed)
                .map_err(|e| anyhow::anyhow!("Failed to parse {what} JSON: {e}"))
        }
    }
}

/// An integer: a JSON number, or a number the model occasionally writes as a string
fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// An optional integer part, with range: missing or null is `Some(None)`; present but out of range or not a number is `None` (bad)
fn part(obj: &Value, key: &str, range: std::ops::RangeInclusive<i64>) -> Option<Option<u32>> {
    match obj.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(v) => {
            let n = int(v)?;
            range.contains(&n).then_some(Some(n as u32))
        }
    }
}

/// Date parts: `y` is always required; a finer part must come with the coarser (a day implies a month); month 1–12, day 1–31,
/// hour 0–23, minute and second 0–59; out of range is a bad entry
fn parts(v: &Value) -> Option<DateParts> {
    if !v.is_object() {
        return None;
    }
    let year = i32::try_from(int(v.get("y")?)?).ok()?;
    let quarter = part(v, "q", 1..=4)?;
    let month = part(v, "m", 1..=12)?;
    // Quarter and month are not written together: writing a quarter means the statement goes only as far as the quarter
    if quarter.is_some() && month.is_some() {
        return None;
    }
    let day = part(v, "d", 1..=31)?;
    let hour = part(v, "h", 0..=23)?;
    let minute = part(v, "min", 0..=59)?;
    let second = part(v, "s", 0..=59)?;
    let ladder = [
        month.is_some(),
        day.is_some(),
        hour.is_some(),
        minute.is_some(),
        second.is_some(),
    ];
    if ladder.windows(2).any(|w| w[1] && !w[0]) {
        return None;
    }
    Some(DateParts {
        year,
        quarter,
        month,
        day,
        hour,
        minute,
        second,
    })
}

/// Optional part: missing or null is `Some(None)`; present but bad is `None`
fn opt_parts(v: Option<&Value>) -> Option<Option<DateParts>> {
    match v {
        None | Some(Value::Null) => Some(None),
        Some(v) => parts(v).map(Some),
    }
}

/// Non-empty text, trimmed of leading and trailing whitespace
fn text(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Enum words: recognized by serde's snake_case names; case, spaces and hyphens are all tolerated ("As of" → as_of)
fn word<T: DeserializeOwned>(v: &Value) -> Option<T> {
    let s = v
        .as_str()?
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-'], "_");
    serde_json::from_value(Value::String(s)).ok()
}

/// Anchor: `{"kind": "mention", "id": N}` / `{"kind": "document"}` / `{"kind": "period", "name"}`;
/// a bare "document" is accepted too, when it carries no other field
fn anchor(v: &Value) -> Option<Anchor> {
    let kind = match v {
        Value::String(s) => s.clone(),
        v => v.get("kind")?.as_str()?.to_string(),
    };
    let kind = kind.trim().to_ascii_lowercase();
    match kind.as_str() {
        "mention" => Some(Anchor::Mention {
            id: int(v.get("id")?)?,
        }),
        "document" => Some(Anchor::Document),
        "period" => Some(Anchor::Period {
            name: text(v.get("name")?)?,
        }),
        _ => None,
    }
}

/// Offset: the number is not negative (direction is separate); unit and direction are enum words
fn offset(v: &Value) -> Option<Offset> {
    let count = int(v.get("count")?)?;
    if count < 0 {
        return None;
    }
    Some(Offset {
        count,
        unit: word(v.get("unit")?)?,
        direction: word(v.get("direction")?)?,
    })
}

fn opt_offset(v: Option<&Value>) -> Option<Option<Offset>> {
    match v {
        None | Some(Value::Null) => Some(None),
        Some(v) => offset(v).map(Some),
    }
}

/// Reference: four kinds by `kind`; JSON null counts as none
fn reference(v: &Value) -> Option<Reference> {
    if v.is_null() {
        return Some(Reference::None);
    }
    let kind = v.get("kind")?.as_str()?.trim().to_ascii_lowercase();
    match kind.as_str() {
        "absolute" => Some(Reference::Absolute {
            from: parts(v.get("from")?)?,
            to: opt_parts(v.get("to"))?,
        }),
        "anchored" => Some(Reference::Anchored {
            anchor: anchor(v.get("anchor")?)?,
            offset: opt_offset(v.get("offset"))?,
        }),
        "period" => Some(Reference::Period {
            name: text(v.get("name")?)?,
            from: opt_parts(v.get("from"))?,
            to: opt_parts(v.get("to"))?,
        }),
        "none" => Some(Reference::None),
        _ => None,
    }
}

/// One `[id, shape, reference, granularity]`
fn interpretation(v: &Value) -> Option<Interpretation> {
    let arr = v.as_array()?;
    Some(Interpretation {
        id: int(arr.first()?)?,
        shape: word(arr.get(1)?)?,
        reference: reference(arr.get(2)?)?,
        granularity: word(arr.get(3)?)?,
    })
}

/// Parse the interpretation reply: returns the entries parsed and the count of bad entries. An id not in `ids`, or a second appearance of the same id,
/// both count as bad; entries lost to a truncated reply's repair are not bad; the caller checks `ids` to see who did not come back
pub fn parse_interpretation_response(
    raw: &str,
    ids: &[i64],
) -> anyhow::Result<(Vec<Interpretation>, usize)> {
    let value = reply_value(raw, "time interpretation")?;
    let known: HashSet<i64> = ids.iter().copied().collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut skipped = 0;
    if let Some(items) = value.get("m").and_then(Value::as_array) {
        for item in items {
            match interpretation(item) {
                Some(i) if known.contains(&i.id) && seen.insert(i.id) => out.push(i),
                _ => skipped += 1,
            }
        }
    }
    Ok((out, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_headings_in_force_are_read_from_the_chunk_itself() {
        let text = "# 周报汇编\n\n## 第35周周报\n\n提报日期：2026年8月28日\n\n### 风险\n无。\n\n## 第36周周报\n\n上周开售。\n";
        let at = |needle: &str| {
            text.chars().count() - text[text.find(needle).unwrap()..].chars().count()
        };
        assert_eq!(
            headings_at(text, at("提报日期")),
            vec!["周报汇编", "第35周周报"]
        );
        assert_eq!(
            headings_at(text, at("无。")),
            vec!["周报汇编", "第35周周报", "风险"]
        );
        // A new heading of the same level replaces the old one, along with the deeper ones under it
        assert_eq!(
            headings_at(text, at("上周开售")),
            vec!["周报汇编", "第36周周报"]
        );
        // A `#` with no whitespace after it is not a heading (`#588`, `#tag`)
        assert_eq!(
            headings_at("#588 是一个编号\n正文", 12),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_now_in_force_is_the_one_of_the_innermost_section_that_holds_the_position() {
        let entry = |words: &str, scope: &[&str], kind: &str| TimeEntry {
            kind: kind.into(),
            name: "提报日期".into(),
            words: words.into(),
            from: DateParts {
                quarter: None,
                year: 2026,
                month: Some(9),
                day: Some(4),
                hour: None,
                minute: None,
                second: None,
            },
            to: None,
            scope: scope.iter().map(|s| s.to_string()).collect(),
            chunk: None,
            char_start: None,
        };
        let entries = vec![
            entry("2026年3月28日", &["汇编"], "now"),
            entry("2026年8月28日", &["汇编", "第35周"], "now"),
            entry("2026年9月4日", &["汇编", "第36周"], "now"),
            entry("2026年8月31日", &["汇编", "第36周"], "date"),
        ];
        let path = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            now_in_force(&entries, &path(&["汇编", "第36周"]))
                .unwrap()
                .words,
            "2026年9月4日"
        );
        assert_eq!(
            now_in_force(&entries, &path(&["汇编", "第35周", "风险"]))
                .unwrap()
                .words,
            "2026年8月28日"
        );
        // Those in other sections are not our concern here; fall back to the one for the whole document
        assert_eq!(
            now_in_force(&entries, &path(&["汇编", "附录"]))
                .unwrap()
                .words,
            "2026年3月28日"
        );
        assert!(now_in_force(&entries[1..], &path(&["别的文档"])).is_none());
    }

    #[test]
    fn a_quarter_of_a_stated_year_is_transcribed_not_computed() {
        let q = parts(&serde_json::json!({"y": 2025, "q": 3})).unwrap();
        assert_eq!((q.year, q.quarter, q.month), (2025, Some(3), None));
        assert_eq!(q.granularity(), Granularity::Month);
        assert_eq!(q.to_string(), "2025-Q3");
        // Quarter and month are not written together; there is no fifth quarter
        assert!(parts(&serde_json::json!({"y": 2025, "q": 3, "m": 7})).is_none());
        assert!(parts(&serde_json::json!({"y": 2025, "q": 5})).is_none());
        // Old data has no such cell: read back as None, and written out without this cell
        let old: DateParts = serde_json::from_value(serde_json::json!({
            "year": 2024, "month": 3, "day": null, "hour": null, "minute": null, "second": null
        }))
        .unwrap();
        assert_eq!(old.quarter, None);
        assert!(serde_json::to_value(&old).unwrap().get("quarter").is_none());
    }

    #[test]
    fn a_context_entry_is_read_with_its_parts_and_a_bad_one_is_refused() {
        let ok = serde_json::json!(["now", "提报日期", "2026年9月4日", {"y": 2026, "m": 9, "d": 4}, null]);
        let e = time_entry(&ok).unwrap();
        assert_eq!(
            (e.kind.as_str(), e.name.as_str(), e.words.as_str()),
            ("now", "提报日期", "2026年9月4日")
        );
        assert_eq!(e.from.to_string(), "2026-09-04");
        let period = serde_json::json!(["period", "报告期", "2025年1月1日至2025年12月31日", {"y": 2025, "m": 1, "d": 1}, {"y": 2025, "m": 12, "d": 31}]);
        assert_eq!(
            time_entry(&period).unwrap().to.unwrap().to_string(),
            "2025-12-31"
        );
        for bad in [
            serde_json::json!(["today", "x", "2026", {"y": 2026}, null]),
            serde_json::json!(["now", "x", "", {"y": 2026}, null]),
            serde_json::json!(["now", "x", "13月", {"y": 2026, "m": 13}, null]),
            serde_json::json!(["now", "x", "2026"]),
        ] {
            assert!(time_entry(&bad).is_none(), "{bad}");
        }
    }

    fn ymd(y: i32, m: u32, d: u32) -> DateParts {
        DateParts {
            quarter: None,
            year: y,
            month: Some(m),
            day: Some(d),
            hour: None,
            minute: None,
            second: None,
        }
    }

    fn year(y: i32) -> DateParts {
        DateParts {
            quarter: None,
            year: y,
            month: None,
            day: None,
            hour: None,
            minute: None,
            second: None,
        }
    }

    #[test]
    fn a_clock_time_parses_and_prints_on_the_ladder() {
        let date =
            parts(&serde_json::json!({"y": 2011, "m": 3, "d": 4, "h": 14, "min": 30, "s": 5}))
                .unwrap();
        assert_eq!(date.granularity(), Granularity::Second);
        assert_eq!(date.to_string(), "2011-03-04 14:30:05");
        assert_eq!(year(2024).to_string(), "2024");
        assert_eq!(year(2024).granularity(), Granularity::Year);
        assert_eq!(ymd(2019, 9, 30).granularity(), Granularity::Day);
    }

    /// Four kinds of reference and seven shapes all appear in this one reply
    const INTERPRETED: &str = r#"{"m": [
        [1, "point", {"kind": "absolute", "from": {"y": 2011, "m": 3, "d": 4}}, "day"],
        [2, "since", {"kind": "anchored", "anchor": {"kind": "document"}, "offset": {"count": 2, "unit": "year", "direction": "before"}}, "year"],
        [3, "interval", {"kind": "period", "name": "fiscal 2019"}, "day"],
        [4, "until", {"kind": "none"}, "day"],
        [5, "as_of", {"kind": "anchored", "anchor": {"kind": "document"}}, "day"],
        [6, "duration", {"kind": "none"}, "year"],
        [7, "ended_unknown", null, "day"],
        [8, "point", {"kind": "anchored", "anchor": {"kind": "mention", "id": 1}, "offset": {"count": 3, "unit": "month", "direction": "after"}}, "month"],
        [9, "interval", {"kind": "absolute", "from": {"y": 2024, "m": 3}, "to": {"y": 2024, "m": 5}}, "month"],
        [10, "interval", {"kind": "period", "name": "Three months ended June 30, 2019", "to": {"y": 2019, "m": 6, "d": 30}}, "day"],
        [11, "point", {"kind": "anchored", "anchor": {"kind": "period", "name": "fiscal 2019"}, "offset": {"count": 0, "unit": "quarter", "direction": "after"}}, "month"]
    ]}"#;

    #[test]
    fn an_interpretation_reply_parses_every_reference_kind_and_shape() {
        let ids: Vec<i64> = (1..=11).collect();
        let (items, skipped) = parse_interpretation_response(INTERPRETED, &ids).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(items.len(), 11);

        assert_eq!(
            items[0],
            Interpretation {
                id: 1,
                shape: Shape::Point,
                reference: Reference::Absolute {
                    from: ymd(2011, 3, 4),
                    to: None
                },
                granularity: Granularity::Day,
            }
        );
        assert_eq!(
            items[1].reference,
            Reference::Anchored {
                anchor: Anchor::Document,
                offset: Some(Offset {
                    count: 2,
                    unit: Unit::Year,
                    direction: Direction::Before
                }),
            }
        );
        assert_eq!(items[1].shape, Shape::Since);
        assert_eq!(items[1].granularity, Granularity::Year);
        assert_eq!(
            items[2].reference,
            Reference::Period {
                name: "fiscal 2019".into(),
                from: None,
                to: None
            }
        );
        assert_eq!(items[2].shape, Shape::Interval);
        assert_eq!(items[3].reference, Reference::None);
        assert_eq!(items[3].shape, Shape::Until);
        assert_eq!(
            items[4].reference,
            Reference::Anchored {
                anchor: Anchor::Document,
                offset: None
            }
        );
        assert_eq!(items[4].shape, Shape::AsOf);
        assert_eq!(items[5].shape, Shape::Duration);
        assert_eq!(items[6].shape, Shape::EndedUnknown);
        assert_eq!(
            items[6].reference,
            Reference::None,
            "JSON null reads as none"
        );
        assert_eq!(
            items[7].reference,
            Reference::Anchored {
                anchor: Anchor::Mention { id: 1 },
                offset: Some(Offset {
                    count: 3,
                    unit: Unit::Month,
                    direction: Direction::After
                }),
            }
        );
        assert_eq!(
            items[8].reference,
            Reference::Absolute {
                from: DateParts {
                    quarter: None,
                    year: 2024,
                    month: Some(3),
                    day: None,
                    hour: None,
                    minute: None,
                    second: None
                },
                to: Some(DateParts {
                    quarter: None,
                    year: 2024,
                    month: Some(5),
                    day: None,
                    hour: None,
                    minute: None,
                    second: None
                }),
            }
        );
        assert_eq!(
            items[9].reference,
            Reference::Period {
                name: "Three months ended June 30, 2019".into(),
                from: None,
                to: Some(ymd(2019, 6, 30)),
            }
        );
        assert_eq!(
            items[10].reference,
            Reference::Anchored {
                anchor: Anchor::Period {
                    name: "fiscal 2019".into()
                },
                offset: Some(Offset {
                    count: 0,
                    unit: Unit::Quarter,
                    direction: Direction::After
                }),
            }
        );
    }

    /// Bad shape, month 13, an id not in the list, a duplicate id, a bad unit each count once; good ones are kept
    #[test]
    fn malformed_interpretation_items_are_counted_not_fatal() {
        let raw = r#"{"m": [
            [1, "point", {"kind": "absolute", "from": {"y": 2011, "m": 3, "d": 4}}, "day"],
            [2, "sometime", {"kind": "none"}, "day"],
            [3, "point", {"kind": "absolute", "from": {"y": 2011, "m": 13}}, "month"],
            [99, "point", {"kind": "none"}, "day"],
            [1, "since", {"kind": "none"}, "day"],
            [4, "point", {"kind": "anchored", "anchor": {"kind": "document"}, "offset": {"count": 2, "unit": "fortnight", "direction": "after"}}, "day"],
            [5, "point", {"kind": "elsewhere"}, "day"],
            [6, "point", {"kind": "none"}, "fortnight"],
            [7, "AS OF", {"kind": "anchored", "anchor": "document"}, "Day"]
        ]}"#;
        let (items, skipped) = parse_interpretation_response(raw, &[1, 2, 3, 4, 5, 6, 7]).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, 1);
        assert_eq!(items[1].id, 7, "case and spacing of the words are forgiven");
        assert_eq!(items[1].shape, Shape::AsOf);
        assert_eq!(items[1].granularity, Granularity::Day);
        assert_eq!(
            items[1].reference,
            Reference::Anchored {
                anchor: Anchor::Document,
                offset: None
            }
        );
        assert_eq!(skipped, 7);
    }

    /// Cut in the middle of the third entry: the first two stay, and no error is raised
    #[test]
    fn a_cut_off_interpretation_reply_keeps_the_complete_items() {
        let marker = r#"[3, "interval", {"kind": "period", "name": "fis"#;
        let cut = INTERPRETED.find(marker).unwrap() + marker.len();
        let ids: Vec<i64> = (1..=11).collect();
        let (items, skipped) = parse_interpretation_response(&INTERPRETED[..cut], &ids).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].id, 2);
    }

    #[test]
    fn a_reply_without_json_is_an_error() {
        assert!(parse_interpretation_response("no", &[1]).is_err());
    }

    /// Corpus wording: none of it may appear in the prompt (the examples are town council, bridge, bakery)
    const CORPUS_MARKS: &[&str] = &[
        "NVIDIA",
        "Food and Drug",
        "FDA",
        "January 25, 2027",
        "January 26, 2026",
        "July 26, 2026",
        "fiscal 2027",
        "上年末",
        "统计公报",
        "10-Q",
        "10-K",
    ];

    #[test]
    fn the_interpretation_prompt_carries_the_context_and_every_mention() {
        let date = ymd(2019, 11, 12);
        let periods = vec![
            NamedPeriod {
                name: "fiscal 2019".into(),
                from: ymd(2018, 10, 1),
                to: ymd(2019, 9, 30),
            },
            NamedPeriod {
                name: "the fourth quarter".into(),
                from: ymd(2019, 7, 1),
                to: ymd(2019, 9, 30),
            },
        ];
        let context = TimeContext {
            date: Some(&date),
            date_words: Some("November 12, 2019"),
            periods: &periods,
            fiscal_year_end: Some((9, 30)),
        };
        let mentions = [
            MentionInput {
                id: 7,
                text: "two years later",
                sentence:
                    "The bridge opened in 2019; two years later the eastern span was widened.",
            },
            MentionInput {
                id: 8,
                text: "this quarter",
                sentence: "Sales this quarter rose at the bakery.",
            },
        ];
        let msgs = build_interpretation_messages(&context, &mentions);
        assert_eq!(msgs.len(), 2);
        let system = &msgs[0].content;
        assert!(system.contains("Never compute"));
        for shape in [
            "\"point\"",
            "\"since\"",
            "\"until\"",
            "\"interval\"",
            "\"as_of\"",
            "\"duration\"",
            "\"ended_unknown\"",
        ] {
            assert!(system.contains(shape), "shape {shape} is not explained");
        }
        for mark in CORPUS_MARKS {
            assert!(!system.contains(mark), "prompt carries {mark}");
        }
        let user = &msgs[1].content;
        assert!(user.contains("2019-11-12"));
        assert!(user.contains("\"November 12, 2019\""));
        assert!(user.contains("\"fiscal 2019\" from 2018-10-01 to 2019-09-30"));
        assert!(user.contains("\"the fourth quarter\""));
        assert!(user.contains("fiscal year end: month 9, day 30"));
        assert!(user.contains("id 7, words \"two years later\""));
        assert!(user.contains(mentions[0].sentence));
        assert!(user.contains("id 8, words \"this quarter\""));
        assert!(user.contains(mentions[1].sentence));
    }

    #[test]
    fn an_undated_document_says_so_in_the_context() {
        let context = TimeContext {
            date: None,
            date_words: None,
            periods: &[],
            fiscal_year_end: None,
        };
        let msgs = build_interpretation_messages(&context, &[]);
        let user = &msgs[1].content;
        assert!(user.contains("date: not stated"));
        assert!(user.contains("periods: none named"));
        assert!(user.contains("fiscal year end: not stated"));
    }

    /// Stored as JSONB and read back, byte for byte
    #[test]
    fn interpretations_round_trip_through_serde() {
        let ids: Vec<i64> = (1..=11).collect();
        let (items, _) = parse_interpretation_response(INTERPRETED, &ids).unwrap();
        for item in &items {
            let json = serde_json::to_string(item).unwrap();
            let back: Interpretation = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, item);
        }
        let json = serde_json::to_value(&items[1]).unwrap();
        assert_eq!(json["shape"], "since");
        assert_eq!(json["reference"]["kind"], "anchored");
        assert_eq!(json["reference"]["anchor"]["kind"], "document");
        assert_eq!(json["reference"]["offset"]["unit"], "year");
        assert_eq!(json["granularity"], "year");
    }
}

#[cfg(test)]
mod heading_code_regressions {
    use super::headings_at;
    #[test]
    fn code_examples_do_not_replace_document_time_scopes() {
        for code in [
            "```markdown\n## Sample\n```",
            "~~~\n## Sample\n~~~",
            "    ## Sample",
        ] {
            let text = format!("# Report\n{code}\nActual date here");
            assert_eq!(
                headings_at(&text, text.chars().count()),
                vec!["Report"],
                "{code}"
            );
        }
    }
    #[test]
    fn closing_fence_requires_the_same_marker_and_no_nonspace_tail() {
        let text =
            "# Report\n````\n```\n## Still code\n```` trailing\n## Also code\n````\n## Real\n";
        assert_eq!(
            headings_at(text, text.chars().count()),
            vec!["Report", "Real"]
        );
    }
    #[test]
    fn unicode_offsets_and_nested_real_headings_remain_correct() {
        let text = "# 周报\n## 数据\n日期";
        assert_eq!(
            headings_at(text, text.chars().count()),
            vec!["周报", "数据"]
        );
    }
}
