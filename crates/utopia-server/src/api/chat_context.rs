//! One history window shared by a whole user turn, and recovery from a context overflow (#964).
//!
//! Only whole question-and-answer pairs that are already finished or failed can be dropped, along with the previous turn's tool round trips. This turn's question, system
//! prompt, and the tool results already obtained in this turn are left untouched, and tools are not rerun. The stored conversation does not change: what is trimmed is
//! what is sent to the model this time.
use super::{agent, rig_model};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use utopia_llm::LlmClient;

type Turns = Vec<(String, String)>;

/// Three places in a turn (the request with tools, the RAG fallback, and the final answer that only looks at evidence) share the same window,
/// so it is `Arc<Mutex<_>>`: whoever hits the overflow first trims, the others send the trimmed version, and the turn trims only once in total
#[derive(Clone)]
pub(super) struct Context(Arc<Mutex<Window>>);

struct Window {
    /// Stored history, oldest first; anything before `start` is no longer sent
    turns: Turns,
    /// Each final answer's own protocol reasoning, kept or trimmed along with its body by original index.
    reasoning: Vec<Option<String>>,
    /// The previous turn's tool round trips: the assistant message carrying tool_calls and the matching tool results, as protocol JSON
    exchange: Vec<Value>,
    /// Index in `turns` of the last assistant message; the round trip is inserted in front of it
    last_assistant: Option<usize>,
    start: usize,
    /// Whether to still send the round trip. **It has its own bit, and it is dropped before any question-and-answer pair**: a single get_document result can be
    /// 24,000 characters, and two of them exceed the default budget. Previously it was tied to the last answer, so dropping it meant dropping everything from the oldest
    /// to the newest pair, and a follow-up like "make it shorter" went out with no history at all (#973 review).
    /// The answer is the conversation a person has read; the round trip is only the evidence that answer digested
    exchange_kept: bool,
    /// Whether this turn has recovered, across the three places combined
    retried: bool,
}

impl Window {
    fn size(&self) -> usize {
        self.turns[self.start..]
            .iter()
            .zip(&self.reasoning[self.start..])
            .map(|((_, s), reasoning)| {
                s.chars().count() + reasoning.as_deref().map_or(0, |s| s.chars().count())
            })
            .sum::<usize>()
            + if self.keeps_exchange() {
                self.exchange
                    .iter()
                    .map(|m| m.to_string().chars().count())
                    .sum()
            } else {
                0
            }
    }

    /// A round trip goes with its answer: if the answer is trimmed, the round trip cannot stay on its own
    fn keeps_exchange(&self) -> bool {
        self.exchange_kept && self.last_assistant.is_some_and(|i| i >= self.start)
    }

    /// Advance `start` to the next user message. A history cut by count may begin in the middle of a question-and-answer pair,
    /// and a stray assistant answer is not sent even if the budget would fit it
    fn skip_orphans(&mut self) {
        while self.start < self.turns.len() && self.turns[self.start].0 != "user" {
            self.start += 1;
        }
    }

    /// Drop round trips first, then drop whole question-and-answer pairs from the oldest, until it fits the budget; a message is never cut in half
    fn trim(&mut self, budget: usize) {
        self.skip_orphans();
        if self.size() > budget && self.keeps_exchange() {
            self.exchange_kept = false;
        }
        while self.start < self.turns.len() && self.size() > budget {
            self.start += 1;
            self.skip_orphans();
        }
    }
}

/// Expand the stored history and round trips into protocol messages, on the same path as rig's `wire`, so the counted number matches
#[cfg(test)]
fn protocol(turns: &[(String, String)], exchange: &[Value]) -> Vec<Value> {
    protocol_with_reasoning(turns, exchange, &[])
}

fn protocol_with_reasoning(
    turns: &[(String, String)],
    exchange: &[Value],
    reasoning: &[Option<String>],
) -> Vec<Value> {
    let mut out = Vec::new();
    for m in agent::history_messages_with_reasoning(turns, exchange, reasoning) {
        rig_model::push_message(&mut out, &m);
    }
    out
}

impl Context {
    #[cfg(test)]
    pub fn new(turns: Turns, exchange: Vec<Value>, budget: usize) -> Self {
        Self::with_reasoning(turns, exchange, vec![], budget)
    }

    pub fn with_reasoning(
        turns: Turns,
        exchange: Vec<Value>,
        mut reasoning: Vec<Option<String>>,
        budget: usize,
    ) -> Self {
        reasoning.resize(turns.len(), None);
        let last_assistant = turns.iter().rposition(|(role, _)| role == "assistant");
        let mut window = Window {
            turns,
            reasoning,
            exchange,
            last_assistant,
            start: 0,
            exchange_kept: true,
            retried: false,
        };
        window.trim(budget);
        Self(Arc::new(Mutex::new(window)))
    }

    /// The history and round trips currently being sent
    pub fn snapshot(&self) -> (Turns, Vec<Value>) {
        let w = self.0.lock().unwrap();
        (
            w.turns[w.start..].to_vec(),
            if w.keeps_exchange() {
                w.exchange.clone()
            } else {
                vec![]
            },
        )
    }

    /// rig's message list stays the same across the turn: it starts with the full history it was given, followed by this turn's question and this turn's tool
    /// round trips. Before each send, that history section is swapped for the trimmed one. What is counted is protocol messages, not conversation turns: one stored
    /// answer can expand into several tool messages; the system message stays where it is, so the entity system message that was attached
    /// sits right before the question. The previous turn's round trip comes before the last answer, so dropping it removes it from the middle, not from the head or tail
    pub fn apply(&self, messages: &[Value]) -> Vec<Value> {
        let w = self.0.lock().unwrap();
        let full = protocol_with_reasoning(&w.turns, &w.exchange, &w.reasoning);
        // rig's list should start with this full history, with at least this turn's question still after it. If it is short, because rig did not hand back
        // the full history or returned a list that is not this turn's, replacing by count would swallow this turn's question too, silently.
        // In that case send it as-is: a too-long request at worst gets another 400, which is better than a request with no question (follow-up to #973 review)
        let sent = messages.iter().filter(|m| m["role"] != "system").count();
        if sent <= full.len() {
            if !full.is_empty() {
                tracing::warn!(
                    stored = full.len(),
                    sent,
                    "rig's message list is shorter than the stored history; sending it unchanged"
                );
            }
            return messages.to_vec();
        }
        let kept = protocol_with_reasoning(
            &w.turns[w.start..],
            if w.keeps_exchange() { &w.exchange } else { &[] },
            &w.reasoning[w.start..],
        );
        let mut out = Vec::with_capacity(messages.len());
        let mut replaced = 0;
        for m in messages {
            if m["role"] == "system" || replaced == full.len() {
                out.push(m.clone());
                continue;
            }
            replaced += 1;
            if replaced == full.len() {
                out.extend(kept.iter().cloned());
            }
        }
        out
    }

    /// The endpoint says the context is over: record the window it reported, and resend once with the history trimmed by half; this happens only once per turn.
    /// **If trimming cannot shrink it, do not resend**: when the history is already empty and the question itself is too long, resending as-is only gets
    /// the same 400 again (#973 review)
    pub fn recover(&self, client: &LlmClient, err: &anyhow::Error) -> bool {
        if utopia_llm::context_too_long(err).is_none() {
            return false;
        }
        client.remember_context_window(err);
        let mut w = self.0.lock().unwrap();
        if w.retried {
            return false;
        }
        w.retried = true;
        let before = (w.start, w.keeps_exchange());
        let budget = (w.size() / 2).min(client.history_char_budget());
        w.trim(budget);
        if (w.start, w.keeps_exchange()) == before {
            tracing::info!("Context refused and there is no older exchange to drop");
            return false;
        }
        tracing::info!(
            history_chars = w.size(),
            "Context refused; retrying once with older exchanges removed"
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn refusal() -> anyhow::Error {
        anyhow::Error::new(utopia_llm::ContextTooLong {
            status: 400,
            reason: "400 Bad Request".into(),
            detail: "Prompt is too long".into(),
            window: None,
        })
    }

    fn heavy_exchange() -> Vec<Value> {
        vec![
            json!({"role":"assistant","tool_calls":[
                {"id":"old-1","type":"function","function":{"name":"get_document","arguments":"{}"}},
                {"id":"old-2","type":"function","function":{"name":"get_document","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"old-1","content":"x".repeat(24_000)}),
            json!({"role":"tool","tool_call_id":"old-2","content":"y".repeat(24_000)}),
        ]
    }

    #[test]
    fn a_budget_drops_whole_exchanges_including_failed_and_stopped_turns() {
        let turns = vec![
            ("assistant".into(), "orphan".into()),
            ("user".into(), "failed unanswered question".into()),
            ("user".into(), "latest".into()),
            ("assistant".into(), "[stopped]界😀".into()),
        ];
        let budget = "latest[stopped]界😀".chars().count();
        let context = Context::new(turns.clone(), vec![], budget);
        assert_eq!(context.snapshot().0, turns[2..]);
        let smaller = Context::new(turns, vec![], budget - 1);
        assert!(
            smaller.snapshot().0.is_empty(),
            "never cut the answer to make it fit"
        );
    }

    #[test]
    fn the_previous_tool_exchange_goes_before_any_question_or_answer() {
        let turns: Turns = vec![
            ("user".into(), "which documents mention Aurora?".into()),
            (
                "assistant".into(),
                "Two documents mention Aurora [1][2].".into(),
            ),
        ];
        let exchange = heavy_exchange();
        let context = Context::new(turns.clone(), exchange.clone(), 32_000);
        assert_eq!(context.snapshot().0, turns, "the question and answer stay");
        assert!(context.snapshot().1.is_empty(), "the exchange goes first");

        let system = json!({"role":"system","content":"preserve system"});
        let entities = json!({"role":"system","content":"known entities"});
        let current = json!({"role":"user","content":"make it shorter"});
        let current_call = json!({"role":"assistant","tool_calls":[{"id":"new-call"}]});
        let current_result =
            json!({"role":"tool","tool_call_id":"new-call","content":"preserve evidence"});
        let mut messages = vec![system.clone()];
        messages.extend(protocol(&turns, &exchange));
        messages.extend([
            entities.clone(),
            current.clone(),
            current_call.clone(),
            current_result.clone(),
        ]);
        let mut expected = vec![system];
        expected.extend(protocol(&turns, &[]));
        expected.extend([entities, current, current_call, current_result]);
        assert_eq!(context.apply(&messages), expected);

        let smaller = Context::new(turns.clone(), exchange, 10);
        assert!(smaller.snapshot().0.is_empty(), "then the oldest exchanges");
        assert!(smaller.snapshot().1.is_empty());
    }

    /// rig did not hand back the full history (or returned a list that is not this turn's): replacing by count would swallow this turn's question,
    /// so it is sent as-is; a list carrying the full history is trimmed as usual
    #[test]
    fn a_list_shorter_than_the_history_is_sent_unchanged() {
        let turns: Turns = vec![
            ("user".into(), "old".into()),
            ("assistant".into(), "answer".into()),
        ];
        let context = Context::new(turns.clone(), vec![], 3);
        assert!(context.snapshot().0.is_empty(), "everything is trimmed");
        let system = json!({"role":"system","content":"preserve system"});
        let current = json!({"role":"user","content":"make it shorter"});
        let short = vec![system.clone(), current.clone()];
        assert_eq!(
            context.apply(&short),
            short,
            "one message where two of history were expected: the question must survive"
        );
        let mut whole = vec![system.clone()];
        whole.extend(protocol(&turns, &[]));
        whole.push(current.clone());
        assert_eq!(context.apply(&whole), vec![system, current]);
    }

    #[test]
    fn a_small_exchange_stays_with_its_answer() {
        let turns: Turns = vec![
            ("user".into(), "old".into()),
            ("assistant".into(), "answer".into()),
            ("user".into(), "failed".into()),
        ];
        let exchange = vec![
            json!({"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"find_entities","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"c","content":"small"}),
        ];
        let context = Context::new(turns.clone(), exchange.clone(), 32_000);
        assert_eq!(context.snapshot(), (turns.clone(), exchange.clone()));
        let messages = protocol(&turns, &exchange);
        assert_eq!(context.apply(&messages), messages, "nothing to remove");
    }

    #[test]
    fn reasoning_stays_with_its_answer_when_exchanges_and_older_turns_are_dropped() {
        let turns: Turns = vec![
            ("user".into(), "question".into()),
            ("assistant".into(), "same answer".into()),
            ("user".into(), "question".into()),
            ("assistant".into(), "same answer".into()),
        ];
        let reasoning = vec![
            None,
            Some("old thought".into()),
            None,
            Some("界😀new thought".into()),
        ];
        let exchange = heavy_exchange();
        let context = Context::with_reasoning(turns.clone(), exchange.clone(), reasoning, 100);
        assert!(
            context.snapshot().1.is_empty(),
            "tool evidence is dropped first"
        );
        let mut messages = protocol(&turns, &exchange);
        messages.push(json!({"role":"user","content":"current"}));
        let applied = context.apply(&messages);
        assert_eq!(applied[1]["reasoning_content"], "old thought");
        assert_eq!(applied[3]["reasoning_content"], "界😀new thought");
        assert!(applied[0].get("reasoning_content").is_none());

        let budget = "questionsame answer界😀new thought".chars().count();
        context.0.lock().unwrap().trim(budget);
        assert_eq!(context.snapshot().0, turns[2..]);
        let applied = context.apply(&messages);
        assert_eq!(applied.len(), 3);
        assert_eq!(applied[1]["reasoning_content"], "界😀new thought");
        assert_eq!(applied[2]["content"], "current");
        context.0.lock().unwrap().trim(budget - 1);
        assert!(
            context.snapshot().0.is_empty(),
            "reasoning counts toward the budget"
        );
    }

    #[test]
    fn a_refusal_halves_the_history_once_and_only_when_something_can_go() {
        let client = LlmClient::new("http://unused", None, "m");
        let turns: Turns = (0..4)
            .flat_map(|i| {
                [
                    ("user".into(), format!("question {i}")),
                    ("assistant".into(), "界".repeat(1000)),
                ]
            })
            .collect();
        let context = Context::new(turns.clone(), heavy_exchange(), 32_000);
        assert!(context.snapshot().1.is_empty());
        assert!(context.recover(&client, &refusal()));
        let (kept, _) = context.snapshot();
        assert_eq!(kept, turns[4..], "half of four exchanges, the newest kept");
        assert!(
            !context.recover(&client, &refusal()),
            "the second refusal of a turn is final"
        );

        let empty = Context::new(vec![], vec![], 32_000);
        assert!(
            !empty.recover(&client, &refusal()),
            "nothing to drop, so the same request is not sent again"
        );
        let other = anyhow::anyhow!("network down");
        assert!(!Context::new(turns, vec![], 32_000).recover(&client, &other));
    }
}
