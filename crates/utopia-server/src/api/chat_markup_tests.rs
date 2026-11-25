//! The model wrote a tool call into the body text (#937).
//!
//! The endpoint occasionally does not use the protocol's `tool_calls`; it writes the call markers verbatim into `delta.content`:
//! DeepSeek's `<｜tool▁calls▁begin｜>…`, or DSML. Nothing was executed, and that text is not an answer.
//! Previously only the wrap-up turn after the budget ran out checked it; on an ordinary turn it was streamed to the user as usual, stored as the answer,
//! and finished with `done`. This pins down the current behaviour:
//!
//! 1. Markers are not sent out: they are absent from the stream, the stored answer, and the tool round-trip replayed to the next turn;
//! 2. A text-only turn is retried once (`MARKUP_RETRY`), and the model goes on to call a tool or answer;
//! 3. If it writes markup again, it finishes with an error and stores nothing;
//! 4. Narration still streams out, holding back only the lines that might be markers. Body text that merely starts with `<`, and examples inside fences,
//!    once told apart, are let through as they are and are not sent back for a retry.
use super::*;

/// DeepSeek's native form: one tool call written verbatim as body text
const CALL: &str = "<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>function<｜tool▁sep｜>search_chunks\n\
                    ```json\n{\"query\":\"Acme revenue\"}\n```<｜tool▁call▁end｜><｜tool▁calls▁end｜>";
const ANSWER: &str = "Acme's revenue rose 5% last quarter.";
const TOOL: Reply = Reply::Tool("find_entities", r#"{"name":"Acme"}"#);
const QUESTION: &str = "What was Acme's revenue last quarter?";

/// All the body text streamed to the user, concatenated frame by frame
fn streamed(sse: &str) -> String {
    sse.split("\n\n")
        .filter_map(|frame| frame.strip_prefix("event: delta\ndata: "))
        .map(|data| {
            serde_json::from_str::<serde_json::Value>(data).expect("delta is JSON")["text"]
                .as_str()
                .expect("delta has text")
                .to_string()
        })
        .collect()
}

/// How many requests ended with the retry sentence
fn sent_back(f: &Fx) -> usize {
    f.requests()
        .iter()
        .filter(|r| last_message(r) == agent::MARKUP_RETRY)
        .count()
}

/// The stored tool round-trip of this turn; the next turn replays it to the model
async fn stored_exchange(f: &Fx) -> anyhow::Result<serde_json::Value> {
    Ok(sqlx::query_scalar(
        "SELECT m.tool_exchange FROM conversation_messages m
           JOIN conversations c ON c.id = m.conversation_id
          WHERE c.kb_id = $1 AND m.role = 'assistant'",
    )
    .bind(f.kb)
    .fetch_one(&f.pool)
    .await?)
}

#[tokio::test]
async fn a_call_written_as_text_after_a_tool_is_sent_back_once() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![
        TOOL,
        Reply::Text(CALL),
        Reply::Text(ANSWER),
    ]))
    .await?
    else {
        return Ok(());
    };

    let sse = f.ask(QUESTION).await?;

    assert!(sse.contains("event: done"), "{sse}");
    assert!(!sse.contains("event: error"), "{sse}");
    assert_eq!(
        streamed(&sse),
        ANSWER,
        "the markup never reaches the reader"
    );
    assert_eq!(f.stored_answer().await?.as_deref(), Some(ANSWER));
    let requests = f.requests();
    assert_eq!(requests.len(), 3, "tool, markup, then one retry");
    assert_eq!(last_message(&requests[2]), agent::MARKUP_RETRY);
    assert_eq!(sent_back(&f), 1);
    // Keep the turn when retrying: the model can see what it wrote
    let messages = requests[2]["messages"].as_array().expect("messages");
    assert_eq!(messages[messages.len() - 2]["role"], "assistant");
    assert_eq!(messages[messages.len() - 2]["content"], CALL);
    f.cleanup().await
}

#[tokio::test]
async fn a_call_written_as_text_in_the_first_turn_is_left_out_of_the_answer() -> anyhow::Result<()>
{
    let Some(f) = fixture(Scripted::new(vec![
        Reply::Text(CALL),
        TOOL,
        Reply::Text(ANSWER),
    ]))
    .await?
    else {
        return Ok(());
    };

    let sse = f.ask(QUESTION).await?;

    assert!(sse.contains("event: done"), "{sse}");
    assert!(
        sse.contains("event: step"),
        "the retry made the call for real:\n{sse}"
    );
    assert_eq!(streamed(&sse), ANSWER);
    assert_eq!(f.stored_answer().await?.as_deref(), Some(ANSWER));
    let requests = f.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        last_message(&requests[1]),
        agent::MARKUP_RETRY,
        "sent back as markup, not as a turn that skipped the tools"
    );
    assert_eq!(sent_back(&f), 1);
    f.cleanup().await
}

#[tokio::test]
async fn a_call_written_as_text_twice_is_an_error_and_nothing_is_stored() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![
        TOOL,
        Reply::Text(CALL),
        Reply::Text(CALL),
        Reply::Text("Never requested"),
    ]))
    .await?
    else {
        return Ok(());
    };

    let sse = f.ask(QUESTION).await?;

    assert!(sse.contains("event: error"), "{sse}");
    assert!(!sse.contains("event: done"), "{sse}");
    assert!(sse.contains(agent::CONTROL_TEXT), "{sse}");
    assert_eq!(streamed(&sse), "", "neither turn reached the reader");
    assert!(f.stored_answer().await?.is_none());
    assert_eq!(f.requests().len(), 3, "one retry, not a loop");
    assert_eq!(sent_back(&f), 1);
    f.cleanup().await
}

#[tokio::test]
async fn narration_streams_while_a_split_marker_is_held() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![
        TOOL,
        Reply::SplitText(&[
            "I will search the filings.\n",
            "<｜tool",
            "▁calls▁begin｜><｜tool▁call▁begin｜>function<｜tool▁sep｜>search_chunks\n",
            "```json\n{\"query\":\"Acme filings\"}\n```<｜tool▁call▁end｜><｜tool▁calls▁end｜>",
        ]),
        Reply::Text(ANSWER),
    ]))
    .await?
    else {
        return Ok(());
    };

    let sse = f.ask(QUESTION).await?;

    assert!(sse.contains("event: done"), "{sse}");
    let shown = streamed(&sse);
    assert!(
        shown.starts_with("I will search the filings.\n"),
        "{shown:?}"
    );
    assert!(shown.ends_with(ANSWER), "{shown:?}");
    assert!(
        !shown.contains("<｜tool") && !shown.contains("```json"),
        "no part of the split marker went out: {shown:?}"
    );
    assert_eq!(f.stored_answer().await?.as_deref(), Some(shown.as_str()));
    assert_eq!(sent_back(&f), 1);
    f.cleanup().await
}

#[tokio::test]
async fn markup_beside_a_real_call_is_dropped_and_not_replayed() -> anyhow::Result<()> {
    const SAID: &str = "Checking the ledger.\n<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>function\
                        <｜tool▁sep｜>find_entities\n```json\n{\"name\":\"Acme\"}\n```\
                        <｜tool▁call▁end｜><｜tool▁calls▁end｜>";
    let Some(f) = fixture(Scripted::new(vec![
        Reply::TextWithTool(SAID),
        Reply::Text(ANSWER),
    ]))
    .await?
    else {
        return Ok(());
    };

    let sse = f.ask(QUESTION).await?;

    assert!(sse.contains("event: done"), "{sse}");
    let shown = streamed(&sse);
    assert!(shown.starts_with("Checking the ledger.\n"), "{shown:?}");
    assert!(shown.ends_with(ANSWER), "{shown:?}");
    assert!(!shown.contains("tool▁"), "{shown:?}");
    assert_eq!(f.stored_answer().await?.as_deref(), Some(shown.as_str()));
    assert_eq!(
        f.requests().len(),
        2,
        "the real call ran; nothing is sent back"
    );
    assert_eq!(sent_back(&f), 0);
    // This step falls after the narration and before the answer, not after the held-back marker
    let step: serde_json::Value = sse
        .split("\n\n")
        .find_map(|frame| frame.strip_prefix("event: step\ndata: "))
        .map(serde_json::from_str)
        .expect("a step")?;
    let before_answer = shown.len() - ANSWER.len();
    assert_eq!(step["at"], serde_json::json!(before_answer));
    let exchange = stored_exchange(&f).await?;
    assert_eq!(exchange[0]["content"], "Checking the ledger.\n");
    assert!(!exchange.to_string().contains("tool▁"), "{exchange}");
    f.cleanup().await
}

#[tokio::test]
async fn text_that_only_starts_like_a_marker_goes_out_whole_and_once() -> anyhow::Result<()> {
    const TEXT: &str = "<b>Acme</b> grew 5% [1].\n<DSML> is the name of that markup, not a call.";
    let Some(f) = fixture(Scripted::new(vec![
        TOOL,
        Reply::SplitText(&[
            "<",
            "b>Acme</b> grew 5% [1].",
            "\n<DS",
            "ML> is the name of that markup, not a call.",
        ]),
    ]))
    .await?
    else {
        return Ok(());
    };

    let sse = f.ask(QUESTION).await?;

    assert!(sse.contains("event: done"), "{sse}");
    assert_eq!(streamed(&sse), TEXT);
    assert_eq!(f.stored_answer().await?.as_deref(), Some(TEXT));
    assert_eq!(f.requests().len(), 2);
    assert_eq!(sent_back(&f), 0);
    f.cleanup().await
}

#[tokio::test]
async fn a_fenced_example_of_the_markup_is_released_when_the_turn_ends() -> anyhow::Result<()> {
    const EXAMPLE: &str = "It looks like this:\n~~~\n<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>\
                           function<｜tool▁sep｜>search_chunks\n~~~\nThat is the raw form of one call.";
    let Some(f) = fixture(Scripted::new(vec![
        Reply::Tool("no_evidence_needed", r#"{"reason":"about the format"}"#),
        Reply::Text(EXAMPLE),
    ]))
    .await?
    else {
        return Ok(());
    };

    let sse = f
        .ask("What does DeepSeek's raw tool-call format look like?")
        .await?;

    assert!(sse.contains("event: done"), "{sse}");
    assert_eq!(streamed(&sse), EXAMPLE);
    assert_eq!(f.stored_answer().await?.as_deref(), Some(EXAMPLE));
    assert_eq!(f.requests().len(), 2);
    assert_eq!(sent_back(&f), 0);
    f.cleanup().await
}
