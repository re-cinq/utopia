//! Chat session persistence: conversation/message repository. Trajectory (steps) and citations (sources) are stored with
//! the assistant message when it is saved; history replay and the live stream share the same data shape.

use sqlx::PgPool;
use utopia_core::models::{ConversationMessage, ConversationView};
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

pub async fn create(pool: &PgPool, kb_id: Uuid, user_id: Uuid, title: &str) -> AppResult<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO conversations (id, kb_id, user_id, title) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(kb_id)
        .bind(user_id)
        .bind(title.chars().take(80).collect::<String>())
        .execute(pool)
        .await?;
    Ok(id)
}

/// The current user's sessions in this store. **Searchable and paginated**: titles repeat (asking the same question twice gives the same title),
/// and sessions beyond a fixed cap of one hundred would not appear in the session UI at all.
///
/// Search covers both the title and the message body: people tend to remember "the one where I asked about Q3",
/// and that wording lives in the body, while the title may have been truncated into something else.
pub async fn list(
    pool: &PgPool,
    kb_id: Uuid,
    user_id: Uuid,
    q: Option<&str>,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<ConversationView>, i64)> {
    const WHERE: &str = "WHERE c.kb_id = $1 AND c.user_id = $2
           AND ($3::text IS NULL
                OR c.title ILIKE '%' || $3 || '%'
                OR EXISTS (SELECT 1 FROM conversation_messages m
                            WHERE m.conversation_id = c.id
                              AND m.content ILIKE '%' || $3 || '%'))";
    let rows: Vec<ConversationView> = sqlx::query_as(&format!(
        "SELECT c.id, c.title, c.created_at, c.updated_at,
                (SELECT count(*) FROM conversation_messages m
                 WHERE m.conversation_id = c.id) AS message_count
         FROM conversations c
         {WHERE}
         ORDER BY c.updated_at DESC, c.id DESC
         LIMIT $4 OFFSET $5"
    ))
    .bind(kb_id)
    .bind(user_id)
    .bind(q)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) = sqlx::query_as(&format!("SELECT count(*) FROM conversations c {WHERE}"))
        .bind(kb_id)
        .bind(user_id)
        .bind(q)
        .fetch_one(pool)
        .await?;
    Ok((rows, total))
}

/// Renames a session.
///
/// **The title was originally taken automatically from the first message**, and that message often is not what the conversation later became:
/// conversations drift as a matter of course, and renaming lets a person find it again by the name they remember.
pub async fn rename(
    pool: &PgPool,
    kb_id: Uuid,
    user_id: Uuid,
    conversation_id: Uuid,
    title: &str,
) -> AppResult<()> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 120 {
        return Err(AppError::invalid(
            "bad_title",
            "Title must be 1-120 characters",
        ));
    }
    let res = sqlx::query(
        "UPDATE conversations SET title = $4
          WHERE id = $3 AND kb_id = $1 AND user_id = $2",
    )
    .bind(kb_id)
    .bind(user_id)
    .bind(conversation_id)
    .bind(title)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Ownership check: the session must belong to this KB's own user.
pub async fn require_owned(
    pool: &PgPool,
    kb_id: Uuid,
    user_id: Uuid,
    conversation_id: Uuid,
) -> AppResult<()> {
    let found: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM conversations WHERE id = $1 AND kb_id = $2 AND user_id = $3",
    )
    .bind(conversation_id)
    .bind(kb_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    found.map(|_| ()).ok_or(AppError::NotFound)
}

pub async fn messages(pool: &PgPool, conversation_id: Uuid) -> AppResult<Vec<ConversationMessage>> {
    let rows: Vec<ConversationMessage> = sqlx::query_as(
        "SELECT id, role, content, steps, sources, stopped, created_at
         FROM conversation_messages WHERE conversation_id = $1 ORDER BY created_at",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The message a regenerate (#936) looks for: its role, body, and whether it is the last message in this conversation.
pub struct RetryTarget {
    pub role: String,
    pub content: String,
    pub last: bool,
}

/// Reads the message to regenerate; an id not in this conversation returns `None`.
///
/// There is no link between an answer and its question; order does it (`created_at`, then id for the same instant). So "not answered yet"
/// can only be judged this way: nothing follows this question. If an answer follows, it has been answered; if someone asks again after it,
/// answering now would put the answer after those turns and scramble the order. Neither case can be regenerated in place
pub async fn retry_target(
    pool: &PgPool,
    conversation_id: Uuid,
    message_id: Uuid,
) -> AppResult<Option<RetryTarget>> {
    let row: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT m.role, m.content, NOT EXISTS (
             SELECT 1 FROM conversation_messages later
              WHERE later.conversation_id = m.conversation_id
                AND (later.created_at, later.id) > (m.created_at, m.id))
           FROM conversation_messages m
          WHERE m.id = $2 AND m.conversation_id = $1",
    )
    .bind(conversation_id)
    .bind(message_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(role, content, last)| RetryTarget {
        role,
        content,
        last,
    }))
}

/// History replayed for one turn.
///
/// Three things, each answering a different question: the body (what was said), entities (who was recognized),
/// and the latest turn's tool round trips (**what was done**).
///
/// The third was added later. The original judgment was "replaying identity is enough: with the id, the next turn can call
/// entity_facts": that saves the chunk text piling up each turn, and that consideration was right. But it also dropped
/// "I already looked this up": across turns the model sees only the prose it wrote,
/// so when asked to "translate" it looked things up again, and landed on a different set of same-named entities.
///
/// Tool round trips **replay only the latest turn**: what is needed is "what I just did", not twenty turns of output.
/// The final-answer reasoning the protocol requires is replayed with each body, and is not tool evidence that can be trimmed separately.
pub struct History {
    /// Identities parallel to turns, so callers can exclude their own appended question.
    pub turn_ids: Vec<Uuid>,
    /// `(role, content)`, in time order
    pub turns: Vec<(String, String)>,
    /// Protocol reasoning for the final answer, one per `turns` entry; not mixed into the body the user sees.
    pub reasoning: Vec<Option<String>>,
    /// Entities already recognized in this conversation (deduplicated)
    pub entities: Vec<serde_json::Value>,
    /// **What the assistant did in the latest turn**: the assistant message with `tool_calls` and its matching tool results.
    ///
    /// Only the latest turn. This section lets the model know what it just did: when asked to continue with "translate"
    /// or "shorter", the evidence is right there and need not be looked up again (so it won't be re-looked-up as
    /// a different batch of same-named entities). Bringing back twenty turns of tool output is another matter; that is exactly why
    /// only the body was stored in the first place.
    pub last_tool_exchange: Vec<serde_json::Value>,
    /// Sources of the same assistant message (#943). A rewrite that does not look anything up copies that answer's `[n]`,
    /// so this turn reattaches them, so the numbers stay clickable
    pub last_sources: Vec<serde_json::Value>,
}

/// A message row read by `recent_context`
#[derive(sqlx::FromRow)]
struct ContextRow {
    id: Uuid,
    role: String,
    content: String,
    resolved: serde_json::Value,
    tool_exchange: serde_json::Value,
    sources: serde_json::Value,
    stopped: bool,
}

pub async fn recent_context(pool: &PgPool, conversation_id: Uuid, n: i64) -> AppResult<History> {
    let mut rows: Vec<ContextRow> = sqlx::query_as(
        "SELECT id, role, content, resolved, tool_exchange, sources, stopped FROM conversation_messages
         WHERE conversation_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(conversation_id)
    .bind(n)
    .fetch_all(pool)
    .await?;
    rows.reverse();
    Ok(history_from_rows(rows))
}

fn reasoning_metadata(message: &serde_json::Value) -> bool {
    message["role"] == "assistant"
        && message.get("tool_calls").is_none()
        && message.get("content").is_none()
        && message.get("reasoning_content").is_some()
}

fn history_from_rows(rows: Vec<ContextRow>) -> History {
    // Entities are deduplicated by id, keeping first-appearance order: the same entity recurring across turns is normal,
    // listing it once per turn would just say the same thing three times
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entities: Vec<serde_json::Value> = Vec::new();
    for row in &rows {
        for e in row.resolved.as_array().into_iter().flatten() {
            let Some(id) = e["id"].as_str() else { continue };
            if seen.insert(id.to_string()) {
                entities.push(e.clone());
            }
        }
    }
    // The segment of the last assistant message. **Search backwards**: the last message is usually the user message just saved
    let last_assistant = rows.iter().rev().find(|row| row.role == "assistant");
    let mut last_tool_exchange = last_assistant
        .and_then(|row| row.tool_exchange.as_array().cloned())
        .unwrap_or_default();
    last_tool_exchange.retain(|m| !reasoning_metadata(m));
    let last_sources = last_assistant
        .and_then(|row| row.sources.as_array().cloned())
        .unwrap_or_default();
    History {
        turn_ids: rows.iter().map(|row| row.id).collect(),
        reasoning: rows
            .iter()
            .map(|row| {
                if row.role != "assistant" {
                    return None;
                }
                row.tool_exchange
                    .as_array()?
                    .iter()
                    .rev()
                    .find(|m| reasoning_metadata(m))?["reasoning_content"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect(),
        turns: rows
            .into_iter()
            .map(|mut row| {
                if row.stopped {
                    row.content = format!(
                        "[This response was stopped by the user before completion.]\n\n{}",
                        row.content
                    );
                }
                (row.role, row.content)
            })
            .collect(),
        entities,
        last_tool_exchange,
        last_sources,
    }
}

/// What a turn leaves behind besides the body.
///
/// **All four are `serde_json::Value`, and passing them loose gives the compiler nothing to check**: passing them in the wrong order gives
/// a record that stores and reads back fine but has its contents mixed up. Same reason as `RelationAxioms`.
#[derive(Default)]
pub struct TurnRecord {
    /// The user explicitly stopped this turn; the body may be partial or empty.
    pub stopped: bool,
    /// Action trail: what was called and how much came back (shown in the UI)
    pub steps: serde_json::Value,
    /// Citation list
    pub sources: serde_json::Value,
    /// Entities recognized this turn (id / name / type). Replayed next turn so the model continues from them instead of searching again
    pub resolved: serde_json::Value,
    /// What this turn called and what it got back (the truncated copy). The most recent segment is replayed next turn;
    /// without it, the model after several turns does not know it has already looked, and looks again.
    /// An optional trailing final-answer protocol metadata containing only `role` and `reasoning_content`.
    pub tool_exchange: serde_json::Value,
}

impl TurnRecord {
    /// User message: all four are empty.
    pub fn empty() -> Self {
        Self {
            stopped: false,
            steps: serde_json::json!([]),
            sources: serde_json::json!([]),
            resolved: serde_json::json!([]),
            tool_exchange: serde_json::json!([]),
        }
    }
}

pub async fn append_message(
    pool: &PgPool,
    conversation_id: Uuid,
    role: &str,
    content: &str,
    rec: &TurnRecord,
) -> AppResult<Uuid> {
    let id = Uuid::now_v7();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO conversation_messages
             (id, conversation_id, role, content, steps, sources, resolved, tool_exchange, stopped)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(conversation_id)
    .bind(role)
    .bind(content)
    .bind(&rec.steps)
    .bind(&rec.sources)
    .bind(&rec.resolved)
    .bind(&rec.tool_exchange)
    .bind(rec.stopped)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE conversations SET updated_at = now() WHERE id = $1")
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn delete(
    pool: &PgPool,
    kb_id: Uuid,
    user_id: Uuid,
    conversation_id: Uuid,
) -> AppResult<()> {
    let res =
        sqlx::query("DELETE FROM conversations WHERE id = $1 AND kb_id = $2 AND user_id = $3")
            .bind(conversation_id)
            .bind(kb_id)
            .bind(user_id)
            .execute(pool)
            .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn history_separates_terminal_reasoning_from_tool_exchanges() {
        let row = |role: &str, tool_exchange| ContextRow {
            id: Uuid::now_v7(),
            role: role.into(),
            content: "same text".into(),
            resolved: json!([]),
            tool_exchange,
            sources: json!([]),
            stopped: false,
        };
        let call = json!({"role":"assistant","reasoning_content":"tool thought",
            "tool_calls":[{"id":"c"}]} );
        let result = json!({"role":"tool","tool_call_id":"c","content":"ok"});
        let history = history_from_rows(vec![
            row("user", json!([])),
            row("assistant", json!([])),
            row(
                "assistant",
                json!([{"role":"assistant","reasoning_content":"older thought"}]),
            ),
            row(
                "assistant",
                json!([{"role":"assistant","reasoning_content":7}]),
            ),
            row(
                "assistant",
                json!([call.clone(), result.clone(),
                {"role":"assistant","reasoning_content":""}]),
            ),
            row("user", json!([])),
        ]);
        assert_eq!(
            history.reasoning,
            [
                None,
                None,
                Some("older thought".into()),
                None,
                Some(String::new()),
                None
            ]
        );
        assert_eq!(history.reasoning.len(), history.turns.len());
        assert_eq!(history.turn_ids.len(), history.turns.len());
        assert_eq!(history.last_tool_exchange, [call, result]);
    }
}
