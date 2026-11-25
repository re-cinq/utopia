//! Long conversations (#964): real handler and real library; the endpoint is a fake model that replies from a script, with no cost and no tokenising.
//!
//! Each test looks at the requests sent to the model: how much history was trimmed, whether the question is still there, whether the round-trip was dropped, and how many times it was resent.
use super::*;

const REFUSAL: &str = r#"{"error":{"code":"context_length_exceeded","message":"maximum context length is 4096 tokens; request has 12000 tokens"}}"#;
const GATE: Reply = Reply::Tool(
    "no_evidence_needed",
    r#"{"reason":"restating the conversation"}"#,
);

async fn seed(f: &Fx, count: usize, chars: usize) -> anyhow::Result<Uuid> {
    let id = utopia_store::conversations::create(&f.pool, f.kb, f.user.id, "context test").await?;
    for i in 0..count {
        for (role, text) in [
            ("user", format!("question {i}")),
            ("assistant", format!("answer {i}: {}", "界".repeat(chars))),
        ] {
            utopia_store::conversations::append_message(
                &f.pool,
                id,
                role,
                &text,
                &utopia_store::conversations::TurnRecord::empty(),
            )
            .await?;
        }
    }
    Ok(id)
}

fn history_chars(request: &serde_json::Value, current: &str) -> usize {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] != "system" && m["content"] != current)
        .filter_map(|m| m["content"].as_str())
        .map(|s| s.chars().count())
        .sum()
}

fn preserves_question(request: &serde_json::Value, current: &str) {
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["role"] == "user" && m["content"] == current)
            .count(),
        1
    );
    assert_eq!(messages[0]["role"], "system");
    let first = messages.iter().find(|m| m["role"] != "system").unwrap();
    assert_eq!(
        first["role"], "user",
        "no orphan answer at the history boundary"
    );
}

#[tokio::test]
async fn long_history_is_bounded_before_the_first_request() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![GATE, Reply::Text("Recovered.")])).await? else {
        return Ok(());
    };
    let id = seed(&f, 10, 6000).await?;
    let sse = history_tests::ask_in(&f, id, "current question").await?;
    assert!(sse.contains("event: done"), "{sse}");
    let requests = f.requests();
    assert_eq!(requests.len(), 2);
    assert!(history_chars(&requests[0], "current question") <= 32_000);
    assert!(!requests[0].to_string().contains("question 0"));
    assert!(requests[0].to_string().contains("question 9"));
    preserves_question(&requests[0], "current question");
    // What gets trimmed is what was sent this time; the stored conversation loses nothing
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM conversation_messages WHERE conversation_id=$1")
            .bind(id)
            .fetch_one(&f.pool)
            .await?;
    assert_eq!(count, 22);
    f.cleanup().await
}

#[tokio::test]
async fn overflow_trims_once_and_the_next_turn_remembers_the_window() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![
        Reply::HttpError(400, REFUSAL),
        GATE,
        Reply::Text("Recovered."),
        GATE,
        Reply::Text("Next answer."),
    ]))
    .await?
    else {
        return Ok(());
    };
    let id = seed(&f, 6, 1500).await?;
    let sse = history_tests::ask_in(&f, id, "current question").await?;
    assert!(sse.contains("event: done"), "{sse}");
    let requests = f.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        history_chars(&requests[1], "current question")
            <= history_chars(&requests[0], "current question") / 2
    );
    assert_eq!(requests[0]["tools"], requests[1]["tools"]);
    assert_eq!(requests[0]["tool_choice"], requests[1]["tool_choice"]);
    assert_eq!(requests[0]["messages"][0], requests[1]["messages"][0]);
    for r in &requests {
        preserves_question(r, "current question");
    }
    let sse = history_tests::ask_in(&f, id, "next question").await?;
    assert!(sse.contains("event: done"), "{sse}");
    let requests = f.requests();
    assert_eq!(requests.len(), 5);
    // The endpoint says the window is 4096 tokens: the budget is half of it, 2048 characters. The latest question-and-answer pair plus "question 5"
    // fits, but adding the "question 4" pair would exceed it
    assert!(history_chars(&requests[3], "next question") <= 2048);
    let sent = requests[3].to_string();
    assert!(
        sent.contains("question 5") && !sent.contains("question 4"),
        "{sent}"
    );
    preserves_question(&requests[3], "next question");
    // Switching models or switching workspaces must not inherit this rejection
    let ws: Uuid = sqlx::query_scalar("SELECT workspace_id FROM knowledge_bases WHERE id=$1")
        .bind(f.kb)
        .fetch_one(&f.pool)
        .await?;
    let mut settings = utopia_store::settings::get(&f.pool, ws).await?.unwrap();
    assert_eq!(
        f.state
            .chat_clients
            .get(ws, &settings)
            .unwrap()
            .history_char_budget(),
        2048
    );
    assert_eq!(
        f.state
            .chat_clients
            .get(Uuid::now_v7(), &settings)
            .unwrap()
            .history_char_budget(),
        32_000
    );
    settings.chat_model = Some("changed-model".into());
    assert_eq!(
        f.state
            .chat_clients
            .get(ws, &settings)
            .unwrap()
            .history_char_budget(),
        32_000
    );
    f.cleanup().await
}

#[tokio::test]
async fn a_second_refusal_has_its_own_code_and_never_falls_back() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![
        Reply::HttpError(400, REFUSAL),
        Reply::HttpError(422, REFUSAL),
    ]))
    .await?
    else {
        return Ok(());
    };
    let id = seed(&f, 4, 2000).await?;
    let sse = history_tests::ask_in(&f, id, "current question").await?;
    assert!(sse.contains("event: error"), "{sse}");
    assert!(sse.contains("context_too_long"), "{sse}");
    assert!(!sse.contains("event: done"));
    let requests = f.requests();
    assert_eq!(requests.len(), 2);
    for r in requests {
        assert!(r.get("tools").is_some());
        preserves_question(&r, "current question");
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_messages WHERE conversation_id=$1 AND role='assistant'",
    )
    .bind(id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(count, 4, "no failed answer is stored as success");
    f.cleanup().await
}

#[tokio::test]
async fn overflow_after_a_tool_keeps_its_result_without_executing_it_again() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![
        GATE,
        Reply::HttpError(400, REFUSAL),
        Reply::Text("Recovered after the tool."),
    ]))
    .await?
    else {
        return Ok(());
    };
    let id = seed(&f, 4, 2000).await?;
    let sse = history_tests::ask_in(&f, id, "current question").await?;
    assert!(sse.contains("event: done"), "{sse}");
    let requests = f.requests();
    assert_eq!(requests.len(), 3);
    let tools = |r: &serde_json::Value| {
        r["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool" || m.get("tool_calls").is_some())
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(tools(&requests[1]), tools(&requests[2]));
    assert_eq!(tools(&requests[2]).len(), 2);
    preserves_question(&requests[2], "current question");
    f.cleanup().await
}

#[tokio::test]
async fn rag_uses_the_same_bound_and_can_recover_without_changing_its_system() -> anyhow::Result<()>
{
    let Some(f) = fixture(Scripted::new(vec![
        Reply::Http(400),
        Reply::Http(400),
        Reply::HttpError(400, REFUSAL),
        Reply::Text("Fallback recovered."),
    ]))
    .await?
    else {
        return Ok(());
    };
    let id = seed(&f, 10, 6000).await?;
    let sse = history_tests::ask_in(&f, id, "current question").await?;
    assert!(sse.contains("event: done"), "{sse}");
    let requests = f.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[2].get("tools").is_none());
    assert!(history_chars(&requests[2], "current question") <= 32_000);
    assert!(history_chars(&requests[3], "current question") <= 2048);
    assert_eq!(requests[2]["messages"][0], requests[3]["messages"][0]);
    for r in &requests {
        preserves_question(r, "current question");
    }
    f.cleanup().await
}

#[tokio::test]
async fn even_an_oversized_current_question_is_never_cut() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![Reply::HttpError(400, REFUSAL)])).await? else {
        return Ok(());
    };
    let question = "新问题😀".repeat(10_000);
    let sse = f.ask(&question).await?;
    assert!(sse.contains("context_too_long"), "{sse}");
    let requests = f.requests();
    // There is no history to trim, so the same request is not sent a second time
    assert_eq!(requests.len(), 1);
    preserves_question(&requests[0], &question);
    f.cleanup().await
}

/// The round-trip of the previous turn: two documents were read, each `chars` characters
fn two_documents(chars: usize) -> serde_json::Value {
    serde_json::json!([
        {"role":"assistant","content":null,"tool_calls":[
            {"id":"old-1","type":"function","function":{"name":"get_document","arguments":"{\"document_id\":\"a\"}"}},
            {"id":"old-2","type":"function","function":{"name":"get_document","arguments":"{\"document_id\":\"b\"}"}}]},
        {"role":"tool","tool_call_id":"old-1","content":"界".repeat(chars)},
        {"role":"tool","tool_call_id":"old-2","content":"界".repeat(chars)}
    ])
}

/// A stored question-and-answer pair; the answer carries the round-trip of `two_documents`
async fn seed_turn(f: &Fx, chars: usize) -> anyhow::Result<Uuid> {
    let id = utopia_store::conversations::create(&f.pool, f.kb, f.user.id, "context test").await?;
    utopia_store::conversations::append_message(
        &f.pool,
        id,
        "user",
        "which documents mention Aurora?",
        &utopia_store::conversations::TurnRecord::empty(),
    )
    .await?;
    utopia_store::conversations::append_message(
        &f.pool,
        id,
        "assistant",
        "Two documents mention Aurora [1][2].",
        &utopia_store::conversations::TurnRecord {
            tool_exchange: two_documents(chars),
            ..utopia_store::conversations::TurnRecord::empty()
        },
    )
    .await?;
    Ok(id)
}

/// The previous turn read two documents with a 48,000-character round-trip. On the follow-up "say it shorter" the round-trip is dropped and the earlier Q&A pairs all remain;
/// when the round-trip fits, it is still sent as before
#[tokio::test]
async fn an_evidence_heavy_turn_still_leaves_its_question_and_answer_for_the_follow_up(
) -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![GATE, Reply::Text("Shorter.")])).await? else {
        return Ok(());
    };
    let id = seed_turn(&f, 24_000).await?;
    let sse = history_tests::ask_in(&f, id, "make it shorter").await?;
    assert!(sse.contains("event: done"), "{sse}");
    let requests = f.requests();
    let messages = requests[0]["messages"].as_array().unwrap();
    assert!(messages
        .iter()
        .any(|m| m["role"] == "user" && m["content"] == "which documents mention Aurora?"));
    assert!(messages.iter().any(
        |m| m["role"] == "assistant" && m["content"] == "Two documents mention Aurora [1][2]."
    ));
    assert!(
        !messages
            .iter()
            .any(|m| m["role"] == "tool" || m.get("tool_calls").is_some()),
        "the exchange is dropped before the question and answer:\n{}",
        requests[0]
    );
    preserves_question(&requests[0], "make it shorter");
    f.cleanup().await?;

    let Some(g) = fixture(Scripted::new(vec![GATE, Reply::Text("Shorter.")])).await? else {
        return Ok(());
    };
    let id = seed_turn(&g, 100).await?;
    let sse = history_tests::ask_in(&g, id, "make it shorter").await?;
    assert!(sse.contains("event: done"), "{sse}");
    let requests = g.requests();
    let messages = requests[0]["messages"].as_array().unwrap();
    assert_eq!(
        messages.iter().filter(|m| m["role"] == "tool").count(),
        2,
        "a small exchange is still sent:\n{}",
        requests[0]
    );
    g.cleanup().await
}
