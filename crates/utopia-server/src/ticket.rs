//! One ticket is one document, and the three ticket sources share this one layout (0013 decision 7).
//!
//! Each source loads its own data into [`Ticket`] and fetches it itself; this file only writes it into a document. The wording (GitHub writes
//! "Opened by", Jira writes "Reported by") is composed by the source and passed in, and nothing here changes a single word: a document's bytes are its identity,
//! so changing one word gives every ticket one more version on its next sync. `ticket_golden_tests` pins this down

use chrono::{DateTime, Utc};

pub struct Ticket {
    /// The line after `# `, such as `#12 Fix it` or `KAFKA-1 Summary`
    pub heading: String,
    /// The sentences under the heading, already composed, in the source's own order
    pub sentences: Vec<String>,
    /// The body under `## Description`; a blank body writes no section
    pub description: Option<String>,
    /// `(instant, text)`, written as `- <instant> — <text>`, stably sorted by instant
    pub history: Vec<(DateTime<Utc>, String)>,
    /// Comments with an empty body are not written, but as long as there is any comment the `## Comments` section appears, as before
    pub comments: Vec<Comment>,
}

pub struct Comment {
    pub author: String,
    /// An entry with no instant is written as `?`
    pub at: Option<DateTime<Utc>>,
    pub body: String,
}

/// How an instant on a ticket looks when written into the body: to the second, with `Z` (#691, 0024 section 3)
pub fn stamp(t: DateTime<Utc>) -> String {
    crate::time_text::world(t, Some("second"))
}

pub fn render(t: &Ticket) -> String {
    let mut out = format!("# {}\n\n", t.heading);
    for s in &t.sentences {
        out.push_str(s);
        out.push('\n');
    }
    if let Some(d) = t
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        out.push_str("\n## Description\n\n");
        out.push_str(d);
        out.push('\n');
    }
    // The endpoint's order is not a contract; sort it ourselves: getting it wrong means the history is told backwards. A stable sort: several entries at the same instant
    // keep the order the source gave
    if !t.history.is_empty() {
        let mut history: Vec<&(DateTime<Utc>, String)> = t.history.iter().collect();
        history.sort_by_key(|(at, _)| *at);
        out.push_str("\n## History\n\n");
        for (at, text) in history {
            out.push_str(&format!("- {} — {}\n", stamp(*at), text));
        }
    }
    if !t.comments.is_empty() {
        out.push_str("\n## Comments\n\n");
        for c in &t.comments {
            let body = c.body.trim();
            if body.is_empty() {
                continue;
            }
            let at = c.at.map(stamp).unwrap_or_else(|| "?".into());
            out.push_str(&format!("### {} on {}\n\n{}\n\n", c.author, at, body));
        }
    }
    out
}
