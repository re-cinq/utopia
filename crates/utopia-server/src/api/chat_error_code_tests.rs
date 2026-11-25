//! Failures in the chat stream carry a code (0004).
//!
//! The server does not send display text: the `error` frame and a rejected request share one envelope `{error, code}`, and the UI looks up the wording by code in the `err.*` table.
//! The English sentence is kept for logs, MCP and clients that do not localize. Before this, the frame held only
//! one English sentence, so on a Chinese UI, billing, rate limits, empty answers and storage failures all showed in English.
//!
//! This test checks, for each failure a user can hit, which code it maps to; the fact that every `error` frame carries a code
//! is checked across the whole table by the terminal contract (`chat_terminal_tests.rs`).
use super::*;

const TOOL: Reply = Reply::Tool("find_entities", r#"{"name":"Acme"}"#);

/// The code of the `error` frame in this SSE stream, and the English sentence
fn failure(sse: &str) -> (String, String) {
    let data = sse
        .split("\n\n")
        .find_map(|frame| frame.strip_prefix("event: error\ndata: "))
        .unwrap_or_else(|| panic!("no error frame:\n{sse}"));
    let body: serde_json::Value = serde_json::from_str(data)
        .unwrap_or_else(|e| panic!("the error frame is not JSON ({e}): {data}"));
    (
        body["code"].as_str().unwrap_or_default().to_string(),
        body["error"].as_str().unwrap_or_default().to_string(),
    )
}

#[tokio::test]
async fn each_failure_a_user_can_meet_carries_its_code() -> anyhow::Result<()> {
    let mut budget = vec![Reply::NarratedTool; 6];
    budget.extend([Reply::Text(DSML), Reply::Text(DSML)]);
    for (what, replies, code) in [
        (
            "empty after a retry",
            vec![TOOL, Reply::Empty, Reply::Empty],
            "answer_empty",
        ),
        (
            "tool-control text at the budget",
            budget,
            "answer_tool_text",
        ),
        (
            "a tool call written as text twice (#946)",
            vec![TOOL, Reply::Text(DSML), Reply::Text(DSML)],
            "answer_tool_text",
        ),
        (
            "out of credit",
            vec![Reply::Http(402)],
            "model_out_of_credit",
        ),
        ("rate limited", vec![Reply::Http(429)], "model_rate_limited"),
        ("unavailable", vec![Reply::Http(503)], "model_unavailable"),
        ("key rejected", vec![Reply::Http(401)], "model_rejected"),
    ] {
        let Some(f) = fixture(Scripted::new(replies)).await? else {
            return Ok(());
        };
        let sse = f.ask("What changed at Acme?").await?;
        let (got, message) = failure(&sse);
        assert_eq!(got, code, "{what}: {message}");
        assert!(
            !message.is_empty(),
            "{what}: the English sentence stays for logs and MCP"
        );
        f.cleanup().await?;
    }
    Ok(())
}

#[tokio::test]
async fn an_answer_that_cannot_be_saved_says_so_with_its_code() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![TOOL, Reply::Text("Accepted answer.")])).await? else {
        return Ok(());
    };
    f.reject_answer_saves().await?;
    let sse = f.ask("What happened?").await?;
    assert_eq!(failure(&sse).0, "answer_not_saved");
    f.cleanup().await
}

/// The reason from the finalization gate travels as text through `chat_finalization`, so it is matched by its original wording. This test takes the output of
/// `finalization_error` directly: changing the wording fails here, rather than quietly turning into "answer failed"
#[test]
fn the_finalization_reasons_keep_their_codes() {
    for (text, calls, code) in [
        ("", false, "answer_empty"),
        ("Answer.", true, "answer_tool_call"),
        (DSML, false, "answer_tool_text"),
    ] {
        let reason = agent::finalization_error(text, calls, "What changed?").expect("a reason");
        assert_eq!(unpublishable_code(reason), Some(code), "{reason}");
        // When `chat_finalization` still fails after one repair, the reason comes back wrapped in two layers of text
        let wrapped = format!(
            "Model could not produce a final answer: Model could not produce a final answer \
             after one recovery: {reason}"
        );
        assert_eq!(unpublishable_code(&wrapped), Some(code), "{wrapped}");
    }
    assert_eq!(
        unpublishable_code("Model final answer exceeded the size limit"),
        Some("answer_too_long")
    );
    assert_eq!(unpublishable_code("Final answer timed out"), None);
}
