//! Exactly one **earned** terminal per chat turn (#857).
//!
//! This is not a regression written for one particular hole. #845 / #850 / #851 / #852 each closed one place where "failed below,
//! reported success above", and each came with its own regression -- **four tests that each catch one case cannot catch a fifth**.
//! A fifth will be written the same way by the next person: a failing path added after the first byte has been sent,
//! while "the loop ran to completion" still reaches `done`.
//!
//! So what is pinned here is the rule itself. Each row of the table is one injection point, and every row passes the same three checks:
//!
//!   1. The client does not receive `event: done`
//!   2. Exactly one terminal is observed (`done` and `error` together, exactly once)
//!   3. The terminal can state its reason: the `error` data is JSON with a `code` and the English sentence (0004)
//!
//! **Adding a new failing path costs one row**; if that row cannot be added, the path itself has not been thought through
//! about how it ends. Reviewers should watch for "a new failing path was opened without adding a row".
//!
//! ## Paths not in the table
//!
//! Some paths are injected by fixture-level means and cannot go into a table scripted by reply; each is covered by a targeted test in the same directory:
//! an error midway through retrieval needs the fixture connection pool closed; an assistant INSERT that is rejected needs a fixture-scoped trigger --
//! `chat_persistence_tests.rs`; the request shape of a degraded answer -- `chat_fallback_tests.rs`;
//! a stream cut midway through the answer, or a terminal broadcast lost because it came before the registry entry was removed -- `chat_registry_tests.rs`.
//! For a new path of this kind, first check whether these three files have a place for it before considering a new file.

use super::chat_empty_reply_tests::{fixture, Reply, Scripted};

/// How this turn should end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ends {
    /// The answer holds: one `done`, no `error`
    Done,
    /// The answer does not hold: one `error`, **no** `done`
    Error,
}

/// One row in the table: one injection point.
struct Case {
    /// What went wrong -- this is the sentence printed when an assertion fails
    what: &'static str,
    /// How the fake model replies in this turn, per this script
    replies: Vec<Reply>,
    /// How this should end
    ends: Ends,
    fallback: bool,
}

/// Count how many terminals appear in this SSE stream.
///
/// Count by frame, not by substring: `event: done` may also appear inside the body of some `data:`
/// (the model is free to discuss SSE in its answer), and that is not a terminal
fn terminals(sse: &str) -> (usize, usize, Vec<String>) {
    let (mut done, mut error, mut reasons) = (0usize, 0usize, Vec::new());
    for frame in sse.split("\n\n") {
        let mut kind = "";
        let mut data = String::new();
        for line in frame.split('\n') {
            if let Some(rest) = line.strip_prefix("event:") {
                kind = rest.trim();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim());
            }
        }
        match kind {
            "done" => done += 1,
            "error" => {
                error += 1;
                reasons.push(data);
            }
            _ => {}
        }
    }
    (done, error, reasons)
}

/// Three assertions; each row passes through the same check.
fn assert_one_earned_terminal(what: &str, ends: Ends, sse: &str) {
    let (done, error, reasons) = terminals(sse);

    assert_eq!(
        done + error,
        1,
        "{what}：恰好一个终结，实际 done={done} error={error}\n{sse}"
    );

    match ends {
        Ends::Done => assert_eq!(
            done, 1,
            "{what}: when the answer holds, the terminal event should be done\n{sse}"
        ),
        Ends::Error => {
            assert_eq!(
                done, 0,
                "{what}：失败不许报成 done——这正是 #857 那一族\n{sse}"
            );
            let reason = reasons.first().map(String::as_str).unwrap_or_default();
            assert!(
                !reason.is_empty(),
                "{what}：终结得说得出理由，不能是个空 error\n{sse}"
            );
            // The reason carries a code: the UI uses it to pick wording; the English sentence is only for logs and clients that do not localize (0004)
            let body: serde_json::Value = serde_json::from_str(reason).unwrap_or_default();
            assert!(
                body["code"].as_str().is_some_and(|c| !c.is_empty())
                    && body["error"].as_str().is_some_and(|e| !e.is_empty()),
                "{what}：error 帧要带 code 和原句：{reason}"
            );
        }
    }
}

/// One tool call, so this turn runs the full evidence-gathering path before ending.
const TOOL: Reply = Reply::Tool("find_entities", r#"{"name":"Acme"}"#);

// The #845 rows test the exhausted gathering boundary, so they reach it before
// injecting a final candidate; a one-tool fixture would exercise an ordinary turn
// instead. Since #937 an ordinary turn is checked for tool-control text as well,
// in a row of its own.
fn at_budget(candidate: Reply) -> Vec<Reply> {
    let mut replies = vec![TOOL; 6];
    replies.push(candidate);
    replies
}

fn table() -> Vec<Case> {
    vec![
        // Control row: a normal answer must be done. Without it, the three assertions above could be satisfied
        // by "never emit done", and the whole table would be empty
        Case {
            what: "模型正常作答",
            replies: vec![TOOL, Reply::Text("Acme 去年第四季度换了 CFO。")],
            ends: Ends::Done,
            fallback: false,
        },
        // Fixed: still an empty body after the retry
        Case {
            what: "重试之后正文仍然为空",
            replies: vec![TOOL, Reply::Empty, Reply::Empty],
            ends: Ends::Error,
            fallback: false,
        },
        Case {
            what: "降级模型仍然返回空答案",
            replies: vec![Reply::Http(400), Reply::Http(400), Reply::Empty],
            ends: Ends::Error,
            fallback: true,
        },
        // #845: after the budget runs out, the endpoint spits tool-control text out as the body
        Case {
            what: "最后一轮吐的是裸的工具控制标记",
            replies: at_budget(Reply::Text(
                "<DSMLcalls><DSMLinvoke name=\"search\"></DSMLinvoke></DSMLcalls>",
            )),
            ends: Ends::Error,
            fallback: false,
        },
        // #845: same, but preceded by a decent narrative -- where the framing boundary falls should not affect the verdict
        Case {
            what: "叙述之后接上工具控制标记",
            replies: at_budget(Reply::Text(
                "我去核对一下证据。\n<DSMLcalls><DSMLinvoke name=\"search\"></DSMLinvoke></DSMLcalls>",
            )),
            ends: Ends::Error,
            fallback: false,
        },
        // #937: a normal turn wrote the tool call as body text, and after one retry wrote it again
        Case {
            what: "普通回合连续两次把工具调用写成正文",
            replies: vec![
                TOOL,
                Reply::Text("<DSMLcalls><DSMLinvoke name=\"search\"></DSMLinvoke></DSMLcalls>"),
                Reply::Text("<DSMLcalls><DSMLinvoke name=\"search\"></DSMLinvoke></DSMLcalls>"),
            ],
            ends: Ends::Error,
            fallback: false,
        },
    ]
}

#[tokio::test]
async fn a_turn_ends_in_exactly_one_earned_terminal() -> anyhow::Result<()> {
    let mut ran = 0usize;

    for case in table() {
        let Some(f) = fixture(Scripted::new(case.replies.clone())).await? else {
            eprintln!("没有 UTOPIA_DATABASE_URL，整张表跳过");
            return Ok(());
        };
        let sse = f.ask("Acme 去年第四季度有什么变化？").await?;
        assert_one_earned_terminal(case.what, case.ends, &sse);
        if case.fallback {
            let requests = f.requests();
            assert!(
                requests.len() > 1 && requests.last().is_some_and(|r| r.get("tools").is_none()),
                "{}：必须真的进入无工具的降级回答请求：{requests:?}",
                case.what
            );
        }
        if case.ends == Ends::Error {
            assert!(
                f.stored_answer().await?.is_none(),
                "{}：失败时不能保存助手消息",
                case.what
            );
        }
        eprintln!("verified terminal contract: {}", case.what);
        f.cleanup().await?;
        ran += 1;
    }

    assert!(
        ran > 0,
        "the whole table was skipped, so nothing was tested"
    );
    Ok(())
}

/// Terminal counting is by frame, not by substring.
///
/// Kept separate because it is the foundation of the three assertions above: if `terminals` counted the `event: done` inside the answer body,
/// the whole table would turn green or red together on the day the model discusses SSE.
#[test]
fn a_terminal_is_a_frame_not_a_substring() {
    let sse = "event: delta\ndata: {\"text\": \"SSE 里用 event: done 表示结束\"}\n\n\
               event: done\ndata: {}\n\n";
    assert_eq!(
        terminals(sse).0,
        1,
        "a done mentioned in the body does not count as terminal"
    );

    let sse = "event: error\ndata: Model returned an empty answer\n\n";
    let (done, error, reasons) = terminals(sse);
    assert_eq!((done, error), (0, 1));
    assert_eq!(reasons, vec!["Model returned an empty answer".to_string()]);
}
