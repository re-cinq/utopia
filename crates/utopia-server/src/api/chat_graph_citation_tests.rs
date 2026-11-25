//! The answer the graph produces must let the reader open the sentence behind each citation (#935).
//!
//! The four graph tools (entity_facts, neighbors, timeline, paths_between) each print, in the conversation, every fact they show
//! with the `[n]` of the fact's first valid evidence block; that block is registered in this turn's source list and shares one numbering with retrieval results.
//! Pinned down here:
//!
//! - Each tool's number points to that fact's evidence block; with several pieces of evidence, the earliest document's block is used
//! - The same block has only one number in a turn; printed again by another tool, it keeps that number
//! - A fact whose evidence exists only in a deleted document carries no number; with `as_of` back before the deletion, it carries one again
//! - The derived-fact line carries no number; its premises each carry their own
//! - MCP text is unchanged by one character, and its source list is empty (numbers are meaningless to callers without a list)
//! - Whole conversation: the numbers printed by the tools go out with the `sources` frame and are saved with the answer; the fact lines the model reads carry the numbers
//! - A number opens the sentence itself (follow-up to #968): the entry carries the quote from the block that states this fact; a block cited several times records each sentence;
//!   when the block is replaced by a new version, the number switches to the still-valid block; with `as_of` back before the replacement it is the original block again
//! - Retrieval and graph tools share one number in a turn: a graph citation of a block retrieval already registered keeps its number and gains the sentence
//!   only the quote was added and the entry count is unchanged, so the sources frame is still sent again
//! - A date not in the original sentence is stated on the line (#970): a timeline end point inferred from the line that closes it is written out with that line's number
//!   (if that line has no valid evidence, it carries no number and adds no entry to the list); a range a person edited is written out with a correction note and no number;
//!   this line's own number is unchanged. Chat and MCP behave the same (MCP carries no numbers): rule 4 says the inferred end point cites the line that closes it, and neither
//!   date is attributed to this line's original sentence
//!
//! Skipped rather than failing without `UTOPIA_DATABASE_URL`. It creates and drops its own database, never touching an existing one.
use super::*;
use crate::api::tools::{dispatch, ToolCtx, ToolSink};

struct Seed {
    /// Two pieces of evidence that 张三 leads Aurora: the earlier document (charter) and the later one (recap)
    charter: Uuid,
    recap: Uuid,
    /// Evidence that 李四 leads Aurora
    handover: Uuid,
    /// The only evidence that 王五 advises Aurora, in a deleted document
    deleted: Uuid,
}

/// One project, three people, four documents (one deleted), plus a fact inferred by a symmetric rule
async fn seed(pool: &sqlx::PgPool, kb: Uuid) -> anyhow::Result<Seed> {
    let id = Uuid::now_v7;
    let (person, project, leads, advises, works_with, rule) = (id(), id(), id(), id(), id(), id());
    let (zhang, li, wang, aurora) = (id(), id(), id(), id());
    let (d_charter, d_recap, d_handover, d_deleted) = (id(), id(), id(), id());
    let (charter, recap, handover, deleted) = (id(), id(), id(), id());
    let (f_zhang, f_li, f_wang, f_pair, derived) = (id(), id(), id(), id(), id());
    // Only locally generated UUIDs and fixed dates are inserted
    sqlx::raw_sql(&format!(
        r#"
        INSERT INTO entity_types(id,kb_id,key,label) VALUES
            ('{person}','{kb}','person','Person'), ('{project}','{kb}','project','Project');
        INSERT INTO relation_types(id,kb_id,key,label,temporal) VALUES
            ('{leads}','{kb}','leads','leads','state'),
            ('{advises}','{kb}','advises','advises','state'),
            ('{works_with}','{kb}','works_with','works with','state');
        INSERT INTO entities(id,kb_id,type_id,canonical_name,created_at) VALUES
            ('{zhang}','{kb}','{person}','Zhang San','2026-01-01'),
            ('{li}','{kb}','{person}','Li Si','2026-01-01'),
            ('{wang}','{kb}','{person}','Wang Wu','2026-01-01'),
            ('{aurora}','{kb}','{project}','Project Aurora','2026-01-01');
        INSERT INTO documents(id,kb_id,filename,sha256,created_at,doc_time,deleted_at) VALUES
            ('{d_charter}','{kb}','charter.md',repeat('1',64),'2026-01-01','2023-01-05',NULL),
            ('{d_recap}','{kb}','recap.md',repeat('2',64),'2025-12-01','2025-01-01',NULL),
            ('{d_handover}','{kb}','handover.md',repeat('3',64),'2026-01-01','2024-07-05',NULL),
            ('{d_deleted}','{kb}','advisor-note.md',repeat('4',64),'2026-01-01','2023-03-02','2026-06-01');
        INSERT INTO chunks(id,kb_id,document_id,seq,text,created_at) VALUES
            ('{charter}','{kb}','{d_charter}',0,'Zhang San leads Project Aurora from 2023-01-10.','2026-01-01'),
            ('{recap}','{kb}','{d_recap}',0,'Zhang San led Project Aurora until the handover.','2026-01-01'),
            ('{handover}','{kb}','{d_handover}',0,'Li Si leads Project Aurora from 2024-07-05.','2026-01-01'),
            ('{deleted}','{kb}','{d_deleted}',0,'Wang Wu advises Project Aurora.','2026-01-01');
        INSERT INTO facts(id,kb_id,subject_id,predicate_id,object_id,valid_from,valid_from_precision,
                          valid_to,valid_to_precision,recorded_at,confidence) VALUES
            ('{f_zhang}','{kb}','{zhang}','{leads}','{aurora}','2023-01-10','day','2024-07-05','day','2026-01-01',0.9),
            ('{f_li}','{kb}','{li}','{leads}','{aurora}','2024-07-05','day',NULL,NULL,'2026-01-01',0.9),
            ('{f_wang}','{kb}','{wang}','{advises}','{aurora}','2023-03-02','day',NULL,NULL,'2026-01-01',0.9),
            ('{f_pair}','{kb}','{zhang}','{works_with}','{li}','2023-01-10','day',NULL,NULL,'2026-01-01',0.9);
        -- 晚的那篇先插、也先入库：取哪条证据按文档自己的日期，不按插入或入库的先后
        INSERT INTO fact_evidence(fact_id,chunk_id,document_id,doc_version,quote) VALUES
            ('{f_zhang}','{recap}','{d_recap}',1,'Zhang San led Project Aurora until the handover.'),
            ('{f_zhang}','{charter}','{d_charter}',1,'Zhang San leads Project Aurora from 2023-01-10.'),
            ('{f_li}','{handover}','{d_handover}',1,'Li Si leads Project Aurora from 2024-07-05.'),
            ('{f_wang}','{deleted}','{d_deleted}',1,'Wang Wu advises Project Aurora.'),
            ('{f_pair}','{charter}','{d_charter}',1,'Zhang San leads Project Aurora from 2023-01-10.');
        INSERT INTO rules(id,kb_id,predicate_id,kind) VALUES ('{rule}','{kb}','{works_with}','symmetric');
        INSERT INTO derived_facts(id,kb_id,subject_id,predicate_id,object_id,rule_id,derived_at,
                                  valid_from,valid_from_precision) VALUES
            ('{derived}','{kb}','{li}','{works_with}','{zhang}','{rule}','2026-01-01','2023-01-10','day');
        INSERT INTO fact_derivations(derived_fact_id,premise_fact_id,seq) VALUES ('{derived}','{f_pair}',0);
        "#
    ))
    .execute(pool)
    .await?;
    Ok(Seed {
        charter,
        recap,
        handover,
        deleted,
    })
}

fn ctx(f: &Fx, via_token: Option<Uuid>) -> ToolCtx<'_> {
    ToolCtx {
        state: &f.state,
        kb_id: f.kb,
        workspace_id: Uuid::nil(),
        mounted_sources: &[],
        can_write: false,
        actor: Some(f.user.id),
        via_token,
        question: None,
    }
}

/// A citation number in a text: `[3]` counts, while confidence `[90%]` and `[rule: …]` do not
fn marks(text: &str) -> Vec<usize> {
    text.match_indices('[')
        .filter_map(|(at, _)| {
            let rest = &text[at + 1..];
            let end = rest.find(']')?;
            rest[..end].parse().ok()
        })
        .collect()
}

/// The line containing `needle`. neighbors writes the items of a group on one line separated by ` · `; this takes that item
fn line<'t>(text: &'t str, needle: &str) -> &'t str {
    text.lines()
        .flat_map(|l| l.split(" · "))
        .find(|item| item.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{text}"))
}

/// Which block source number `n` registered in the list
fn chunk_of(sink: &ToolSink, n: usize) -> Uuid {
    sink.sources[n - 1]["chunk_id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no source {n} in {:?}", sink.sources))
}

#[tokio::test]
async fn each_graph_tool_cites_the_first_live_evidence_of_each_fact() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![])).await? else {
        return Ok(());
    };
    let run = async {
        let s = seed(&f.pool, f.kb).await?;
        let chat = ctx(&f, None);
        let mut sink = ToolSink::default();

        let facts = dispatch(
            &chat,
            &mut sink,
            "entity_facts",
            &json!({"entity": "Project Aurora"}),
        )
        .await;
        let text = &facts.text;
        let zhang = marks(line(text, "Zhang San"));
        let li = marks(line(text, "Li Si"));
        assert_eq!(zhang.len(), 1, "{text}");
        assert_eq!(
            chunk_of(&sink, zhang[0]),
            s.charter,
            "the earliest document's chunk, not the recap"
        );
        assert_eq!(li.len(), 1, "{text}");
        assert_eq!(chunk_of(&sink, li[0]), s.handover);
        assert!(
            marks(line(text, "Wang Wu")).is_empty(),
            "evidence only in a deleted document: {text}"
        );
        assert_eq!(sink.sources.len(), 2, "{:?}", sink.sources);
        assert_eq!(sink.sources[zhang[0] - 1]["filename"], "charter.md");
        assert!(
            sink.sources
                .iter()
                .all(|x| x["chunk_id"] != s.recap.to_string()),
            "one number per fact: the later evidence is not listed"
        );

        // Same block, same number: printed again by another tool, the list does not grow
        let near = dispatch(
            &chat,
            &mut sink,
            "neighbors",
            &json!({"entity": "Project Aurora"}),
        )
        .await;
        assert_eq!(marks(line(&near.text, "Zhang San")), zhang, "{}", near.text);
        assert_eq!(marks(line(&near.text, "Li Si")), li, "{}", near.text);
        let dated = dispatch(
            &chat,
            &mut sink,
            "timeline",
            &json!({"entity": "Project Aurora"}),
        )
        .await;
        assert_eq!(
            marks(line(&dated.text, "Zhang San")),
            zhang,
            "{}",
            dated.text
        );
        assert_eq!(marks(line(&dated.text, "Li Si")), li, "{}", dated.text);
        assert!(
            marks(line(&dated.text, "Wang Wu")).is_empty(),
            "{}",
            dated.text
        );
        // Each hop of the path is a fact with its own number: the evidence for 王五's hop is in the deleted document, so it carries none
        let path = dispatch(
            &chat,
            &mut sink,
            "paths_between",
            &json!({"from": "Wang Wu", "to": "Li Si"}),
        )
        .await;
        let hops = line(&path.text, "—advises→");
        assert_eq!(marks(hops), [li[0]], "{}", path.text);
        assert_eq!(
            sink.sources.len(),
            2,
            "no chunk was numbered twice: {:?}",
            sink.sources
        );

        // The derived line carries no number; its premise (张三 works with 李四) carries its own
        let li_facts = dispatch(
            &chat,
            &mut sink,
            "entity_facts",
            &json!({"entity": "Li Si"}),
        )
        .await;
        let derived = line(&li_facts.text, "[rule:");
        assert!(marks(derived).is_empty(), "{}", li_facts.text);
        let premise = marks(line(&li_facts.text, "Zhang San"));
        assert_eq!(premise.len(), 1, "{}", li_facts.text);
        assert_eq!(chunk_of(&sink, premise[0]), s.charter);

        // Back to before the deletion: that document was still there, so 王五's fact carries a number
        let mut before = ToolSink::default();
        let then = dispatch(
            &chat,
            &mut before,
            "entity_facts",
            &json!({"entity": "Project Aurora", "as_of": "2026-03-01"}),
        )
        .await;
        let wang = marks(line(&then.text, "Wang Wu"));
        assert_eq!(wang.len(), 1, "{}", then.text);
        assert_eq!(chunk_of(&before, wang[0]), s.deleted);

        // MCP: the text is identical to the conversation's once the numbers are removed, and the source list is empty
        let mcp = ctx(&f, Some(Uuid::now_v7()));
        for (tool, args) in [
            ("entity_facts", json!({"entity": "Project Aurora"})),
            ("neighbors", json!({"entity": "Project Aurora"})),
            ("timeline", json!({"entity": "Project Aurora"})),
            ("paths_between", json!({"from": "Wang Wu", "to": "Li Si"})),
        ] {
            let mut empty = ToolSink::default();
            let plain = dispatch(&mcp, &mut empty, tool, &args).await.text;
            let numbered = dispatch(&chat, &mut ToolSink::default(), tool, &args)
                .await
                .text;
            let mut stripped = numbered.clone();
            for n in 1..=2 {
                stripped = stripped.replace(&format!(" [{n}]"), "");
            }
            assert_eq!(plain, stripped, "{tool} over MCP keeps its text");
            assert!(marks(&plain).is_empty(), "{tool}: {plain}");
            assert!(empty.sources.is_empty(), "{tool}: {:?}", empty.sources);
        }
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn a_graph_answer_publishes_and_stores_its_sources() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![
        Reply::Tool("entity_facts", r#"{"entity_id":"Project Aurora"}"#),
        Reply::Text("Li Si leads Project Aurora."),
    ]))
    .await?
    else {
        return Ok(());
    };
    let run = async {
        let s = seed(&f.pool, f.kb).await?;
        let sse = f.ask("Who leads Project Aurora?").await?;
        assert!(sse.contains("event: done"), "{sse}");

        // `sources` frame: two blocks, numbered in the order printed
        let published: Vec<serde_json::Value> = sse
            .split("\n\n")
            .filter(|frame| frame.lines().any(|l| l == "event: sources"))
            .filter_map(|frame| frame.lines().find_map(|l| l.strip_prefix("data: ")))
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect();
        let last = published.last().expect("a sources frame");
        let chunks: Vec<&str> = last
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["chunk_id"].as_str())
            .collect();
        assert_eq!(chunks.len(), 2, "{last}");
        assert!(chunks.contains(&s.charter.to_string().as_str()), "{last}");
        assert!(chunks.contains(&s.handover.to_string().as_str()), "{last}");

        // The saved answer carries the same list; the numbers still open when the session is reopened
        let stored: serde_json::Value = sqlx::query_scalar(
            "SELECT m.sources FROM conversation_messages m
               JOIN conversations c ON c.id = m.conversation_id
              WHERE c.kb_id = $1 AND m.role = 'assistant'",
        )
        .bind(f.kb)
        .fetch_one(&f.pool)
        .await?;
        assert_eq!(&stored, last);

        // The fact lines the model reads carry the number: it follows the confidence
        let requests = f.requests();
        let sent = requests[1]["messages"].to_string();
        assert!(sent.contains("Li Si") && sent.contains("%] ["), "{sent}");
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// The quote carried by number `n`'s entry
fn quotes_of(sink: &ToolSink, n: usize) -> Vec<String> {
    sink.sources[n - 1]["quotes"]
        .as_array()
        .map(|q| {
            q.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Documents still in the seed that go into the full-text index: retrieval must be able to find them
async fn index_seed(f: &Fx) -> anyhow::Result<()> {
    let rows: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT c.document_id, c.id, c.text FROM chunks c JOIN documents d ON d.id = c.document_id
          WHERE c.kb_id = $1 AND d.deleted_at IS NULL ORDER BY c.document_id, c.seq",
    )
    .bind(f.kb)
    .fetch_all(&f.pool)
    .await?;
    let mut by_doc: std::collections::BTreeMap<Uuid, Vec<(String, String)>> = Default::default();
    for (doc, chunk, text) in rows {
        by_doc
            .entry(doc)
            .or_default()
            .push((chunk.to_string(), text));
    }
    for (doc, chunks) in by_doc {
        f.state
            .search
            .reindex_document(&f.kb.to_string(), &doc.to_string(), &chunks)?;
    }
    Ok(())
}

/// A number opens the sentence that states this fact: the entry carries the quote, and both sentences are there when one block holds two facts; after the block is replaced
/// it switches to the still-valid block and its sentence; with `as_of` back before the replacement it is the original again
#[tokio::test]
async fn a_graph_number_opens_the_sentence_it_was_read_from() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![])).await? else {
        return Ok(());
    };
    let run = async {
        let s = seed(&f.pool, f.kb).await?;
        let pair: Uuid = sqlx::query_scalar(
            "SELECT f.id FROM facts f JOIN relation_types r ON r.id = f.predicate_id
              WHERE f.kb_id = $1 AND r.key = 'works_with'",
        )
        .bind(f.kb)
        .fetch_one(&f.pool)
        .await?;
        // The charter block has two sentences; 张三's two facts are each read from one sentence
        sqlx::query("UPDATE chunks SET text = $2 WHERE id = $1")
            .bind(s.charter)
            .bind("Zhang San leads Project Aurora from 2023-01-10. Zhang San works with Li Si.")
            .execute(&f.pool)
            .await?;
        sqlx::query(
            "UPDATE fact_evidence SET quote = 'Zhang San works with Li Si.' WHERE fact_id = $1",
        )
        .bind(pair)
        .execute(&f.pool)
        .await?;
        let chat = ctx(&f, None);
        let mut sink = ToolSink::default();
        let text = dispatch(
            &chat,
            &mut sink,
            "entity_facts",
            &json!({"entity": "Zhang San"}),
        )
        .await
        .text;
        // 张三 works with 李四 has two lines: the asserted line carries a number, the rule-derived line (`[rule:`) does not
        let works_line = text
            .lines()
            .flat_map(|l| l.split(" · "))
            .find(|l| l.contains("Li Si") && !l.contains("[rule:"))
            .unwrap_or_else(|| panic!("no stated works-with line in:\n{text}"));
        let (leads, works) = (marks(line(&text, "Project Aurora")), marks(works_line));
        assert_eq!(leads, works, "one chunk, one number: {text}");
        assert_eq!(chunk_of(&sink, leads[0]), s.charter);
        let mut quotes = quotes_of(&sink, leads[0]);
        quotes.sort();
        assert_eq!(
            quotes,
            [
                "Zhang San leads Project Aurora from 2023-01-10.",
                "Zhang San works with Li Si."
            ],
            "both sentences the chunk was cited for"
        );

        // The charter block is replaced by a new version: 张三 leads Aurora switches to the still-valid recap block and its sentence
        sqlx::query("UPDATE chunks SET superseded_at = now() WHERE id = $1")
            .bind(s.charter)
            .execute(&f.pool)
            .await?;
        let mut now = ToolSink::default();
        let text = dispatch(
            &chat,
            &mut now,
            "entity_facts",
            &json!({"entity": "Project Aurora"}),
        )
        .await
        .text;
        let zhang = marks(line(&text, "Zhang San"));
        assert_eq!(chunk_of(&now, zhang[0]), s.recap, "{text}");
        assert_eq!(
            quotes_of(&now, zhang[0]),
            ["Zhang San led Project Aurora until the handover."]
        );
        // Back before the replacement: the charter block was still the current one then
        let mut then = ToolSink::default();
        let text = dispatch(
            &chat,
            &mut then,
            "entity_facts",
            &json!({"entity": "Project Aurora", "as_of": "2026-03-01"}),
        )
        .await
        .text;
        let zhang = marks(line(&text, "Zhang San"));
        assert_eq!(chunk_of(&then, zhang[0]), s.charter, "{text}");
        assert_eq!(
            quotes_of(&then, zhang[0]),
            ["Zhang San leads Project Aurora from 2023-01-10."]
        );
        // MCP has no source list, so there are no quotes
        let mut empty = ToolSink::default();
        dispatch(
            &ctx(&f, Some(Uuid::now_v7())),
            &mut empty,
            "entity_facts",
            &json!({"entity": "Project Aurora"}),
        )
        .await;
        assert!(empty.sources.is_empty());
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// In one turn, retrieval and graph tools share one number: retrieval registered the handover memo block first, and when the graph cites it the number is unchanged,
/// with 李四's sentence added to the entry; the reverse also holds: retrieval does not start a new number, and the quote is kept
#[tokio::test]
async fn a_search_and_a_graph_tool_share_one_number() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![])).await? else {
        return Ok(());
    };
    let run = async {
        let s = seed(&f.pool, f.kb).await?;
        index_seed(&f).await?;
        let chat = ctx(&f, None);
        let number_of = |sink: &ToolSink, chunk: Uuid| {
            sink.sources
                .iter()
                .position(|x| x["chunk_id"] == chunk.to_string())
                .map(|i| i + 1)
        };
        let li_says = "Li Si leads Project Aurora from 2024-07-05.";

        let mut sink = ToolSink::default();
        let found = dispatch(
            &chat,
            &mut sink,
            "search_chunks",
            &json!({"query": "Li Si leads Project Aurora"}),
        )
        .await
        .text;
        let n = number_of(&sink, s.handover).unwrap_or_else(|| panic!("the handover hit: {found}"));
        assert!(
            quotes_of(&sink, n).is_empty(),
            "a search hit carries no quote"
        );
        let listed = sink.sources.len();
        let text = dispatch(
            &chat,
            &mut sink,
            "entity_facts",
            &json!({"entity": "Project Aurora"}),
        )
        .await
        .text;
        assert_eq!(
            marks(line(&text, "Li Si")),
            [n],
            "the search's number: {text}"
        );
        assert_eq!(
            quotes_of(&sink, n),
            [li_says],
            "the entry gains the sentence"
        );
        assert_eq!(
            number_of(&sink, s.handover),
            Some(n),
            "still one entry for the chunk"
        );
        assert!(sink.sources.len() >= listed);

        let mut reverse = ToolSink::default();
        let text = dispatch(
            &chat,
            &mut reverse,
            "entity_facts",
            &json!({"entity": "Project Aurora"}),
        )
        .await
        .text;
        let m = marks(line(&text, "Li Si"))[0];
        let listed = reverse.sources.len();
        dispatch(
            &chat,
            &mut reverse,
            "search_chunks",
            &json!({"query": "Li Si leads Project Aurora"}),
        )
        .await;
        assert_eq!(
            number_of(&reverse, s.handover),
            Some(m),
            "the graph's number"
        );
        assert_eq!(quotes_of(&reverse, m), [li_says], "the quote stays");
        assert!(reverse.sources.len() >= listed);
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// Whole conversation: retrieval registered three blocks and the graph cites two of them again; only the quotes are added, the entry count does not change, and the sources frame is still sent again
/// once, and what is stored matches the last frame
#[tokio::test]
async fn a_quote_added_to_a_search_hit_is_published() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![
        Reply::Tool("search_chunks", r#"{"query":"Project Aurora"}"#),
        Reply::Tool("entity_facts", r#"{"entity_id":"Li Si"}"#),
        Reply::Text("Li Si leads Project Aurora."),
    ]))
    .await?
    else {
        return Ok(());
    };
    let run = async {
        seed(&f.pool, f.kb).await?;
        index_seed(&f).await?;
        let sse = f.ask("Who leads Project Aurora?").await?;
        assert!(sse.contains("event: done"), "{sse}");
        let published: Vec<serde_json::Value> = sse
            .split("\n\n")
            .filter(|frame| frame.lines().any(|l| l == "event: sources"))
            .filter_map(|frame| frame.lines().find_map(|l| l.strip_prefix("data: ")))
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect();
        assert!(published.len() >= 2, "{published:?}");
        let (first, last) = (&published[0], published.last().unwrap());
        assert_eq!(
            first.as_array().map(Vec::len),
            last.as_array().map(Vec::len),
            "the graph step added no entry, only quotes: {published:?}"
        );
        assert!(
            last.as_array()
                .unwrap()
                .iter()
                .any(|x| x["quotes"].as_array().is_some_and(|q| !q.is_empty())),
            "{last}"
        );
        let stored: serde_json::Value = sqlx::query_scalar(
            "SELECT m.sources FROM conversation_messages m
               JOIN conversations c ON c.id = m.conversation_id
              WHERE c.kb_id = $1 AND m.role = 'assistant'",
        )
        .bind(f.kb)
        .fetch_one(&f.pool)
        .await?;
        assert_eq!(&stored, last);
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}

/// Two parts of #970: 周七 took over on 2025-09-01, and the timeline closes 李四's segment on that day; a person changed 张三's start from the charter's
/// 2023-01-10 to 2023-02-01. The numbers of both lines still open the sentences they were read from, and those sentences do not contain these two dates,
/// so the line states where the dates came from. After 周七's document is deleted, the line that closes 李四's segment has no valid evidence: the line still
/// states that he took over, with no number
#[tokio::test]
async fn a_fact_line_says_when_a_date_is_not_the_passages() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![])).await? else {
        return Ok(());
    };
    let run = async {
        let s = seed(&f.pool, f.kb).await?;
        let (leads, person, aurora): (Uuid, Uuid, Uuid) = sqlx::query_as(
            "SELECT r.id, t.id, e.id FROM relation_types r, entity_types t, entities e
              WHERE r.kb_id = $1 AND r.key = 'leads' AND t.kb_id = $1 AND t.key = 'person'
                AND e.kb_id = $1 AND e.canonical_name = 'Project Aurora'",
        )
        .bind(f.kb)
        .fetch_one(&f.pool)
        .await?;
        let row_of = |name: &'static str| {
            let pool = f.pool.clone();
            let kb = f.kb;
            async move {
                sqlx::query_scalar::<_, Uuid>(
                    "SELECT f.id FROM facts f JOIN entities e ON e.id = f.subject_id
                      WHERE f.kb_id = $1 AND e.canonical_name = $2 AND f.predicate_id = $3
                        AND f.invalidated_at IS NULL",
                )
                .bind(kb)
                .bind(name)
                .bind(leads)
                .fetch_one(&pool)
                .await
            }
        };
        let (zhang, li) = (row_of("Zhang San").await?, row_of("Li Si").await?);
        let (zhou, d_update, update, zhou_fact) =
            (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        // Only locally generated UUIDs and fixed dates are inserted
        sqlx::raw_sql(&format!(
            r#"
            INSERT INTO entities(id,kb_id,type_id,canonical_name,created_at) VALUES
                ('{zhou}','{kb}','{person}','Zhou Qi','2026-01-01');
            INSERT INTO documents(id,kb_id,filename,sha256,created_at,doc_time) VALUES
                ('{d_update}','{kb}','leadership-update.md',repeat('5',64),'2026-01-01','2025-09-01');
            INSERT INTO chunks(id,kb_id,document_id,seq,text,created_at) VALUES
                ('{update}','{kb}','{d_update}',0,'Zhou Qi leads Project Aurora from 2025-09-01.','2026-01-01');
            INSERT INTO facts(id,kb_id,subject_id,predicate_id,object_id,valid_from,valid_from_precision,
                              recorded_at,confidence) VALUES
                ('{zhou_fact}','{kb}','{zhou}','{leads}','{aurora}','2025-09-01','day','2026-01-01',0.9);
            INSERT INTO fact_evidence(fact_id,chunk_id,document_id,doc_version,quote) VALUES
                ('{zhou_fact}','{update}','{d_update}',1,'Zhou Qi leads Project Aurora from 2025-09-01.');
            UPDATE relation_types SET inverse_functional = TRUE WHERE id = '{leads}';
            "#,
            kb = f.kb
        ))
        .execute(&f.pool)
        .await?;
        // A project has only one lead at a time: reconciliation closes 李四's segment on the day 周七 took over
        utopia_store::temporal::reconcile_moved_facts(&f.pool, f.kb, &[li, zhou_fact]).await?;
        // A person changes 张三's start; the end stays; the audit is recorded on the changed line (the same path as PATCH /facts/{id})
        let day = |s: &str| s.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
        utopia_store::temporal::correct_interval(
            &f.pool,
            zhang,
            utopia_store::graph::Validity {
                from: Some(day("2023-02-01T00:00:00Z")),
                from_precision: Some("day"),
                to: Some(day("2024-07-05T00:00:00Z")),
                to_precision: Some("day"),
                attested_at: None,
                from_grade: None,
                end_after_period: false,
            },
        )
        .await?
        .expect("the row was live");
        utopia_store::audit::record(
            &f.pool,
            Some(f.kb),
            f.user.id,
            "fact.time_corrected",
            "fact",
            Some(zhang),
            json!({ "note": "The charter date was the approval date" }),
        )
        .await?;

        let chat = ctx(&f, None);
        let mut sink = ToolSink::default();
        for tool in ["entity_facts", "neighbors", "timeline"] {
            let text = dispatch(&chat, &mut sink, tool, &json!({"entity": "Project Aurora"}))
                .await
                .text;
            let zhang_line = line(&text, "Zhang San");
            assert!(
                zhang_line.contains(
                    "2023-02-01 → 2024-07-05, start corrected: The charter date was the approval date)"
                ),
                "{tool}: {text}"
            );
            let li_line = line(&text, "Li Si");
            assert!(
                li_line.contains("2024-07-05 → 2025-09-01, superseded by Zhou Qi ["),
                "{tool}: {text}"
            );
            // 李四's line also says 周七 (he closed it), so it is found by its own range within 周七's line
            let zhou_line = line(&text, "2025-09-01 → now)");
            assert!(zhou_line.contains("Zhou Qi"), "{tool}: {text}");
            // This line's number is unchanged: it still opens the sentence this fact was read from. The number that closes 李四's segment opens
            // the block where 2025-09-01 is written; 周七's own line is that block too, so the block has only one number. The note is not added to the list
            let (z, l, q) = (marks(zhang_line), marks(li_line), marks(zhou_line));
            assert_eq!((z.len(), l.len(), q.len()), (1, 2, 1), "{tool}: {text}");
            assert_eq!(chunk_of(&sink, z[0]), s.charter, "{tool}");
            assert_eq!(chunk_of(&sink, l[0]), update, "{tool}: the closing fact's passage");
            // That entry carries 周七's original words (follow-up to #968): the number opens the sentence with 2025-09-01
            assert_eq!(
                quotes_of(&sink, l[0]),
                ["Zhou Qi leads Project Aurora from 2025-09-01."],
                "{tool}: the closer's entry carries the sentence that states the date"
            );
            assert_eq!(chunk_of(&sink, l[1]), s.handover, "{tool}: the line's own passage");
            assert_eq!(q[0], l[0], "{tool}: one number for one chunk");
            assert!(
                sink.sources
                    .iter()
                    .all(|x| !x.to_string().contains("approval date")),
                "the note is not a source"
            );
        }
        let path = dispatch(
            &chat,
            &mut sink,
            "paths_between",
            &json!({"from": "Wang Wu", "to": "Li Si"}),
        )
        .await
        .text;
        assert!(
            path.contains("2024-07-05 → 2025-09-01, superseded by Zhou Qi ["),
            "{path}"
        );
        // The edge on the path that a person changed (review of #976): again only the changed end is stated
        let path = dispatch(
            &chat,
            &mut sink,
            "paths_between",
            &json!({"from": "Wang Wu", "to": "Zhang San"}),
        )
        .await
        .text;
        assert!(
            path.contains(
                "2023-02-01 → 2024-07-05, start corrected: The charter date was the approval date"
            ),
            "{path}"
        );

        // MCP text carries the same markers: they are this line's facts, not citations
        let mcp = ctx(&f, Some(Uuid::now_v7()));
        let plain = dispatch(
            &mcp,
            &mut ToolSink::default(),
            "entity_facts",
            &json!({"entity": "Project Aurora"}),
        )
        .await
        .text;
        assert!(
            plain.contains(", start corrected: The charter date was the approval date)")
                && plain.contains(", superseded by Zhou Qi)"),
            "{plain}"
        );
        assert!(marks(&plain).is_empty(), "{plain}");

        // Rule 4: an inferred end point cites the line that closes it; neither the inferred date nor the person-edited date is attributed to this line's original sentence, and the untagged
        // date is still the original sentence's date
        assert!(
            SYSTEM_PROMPT.contains("An end written `superseded by X [m]`")
                && SYSTEM_PROMPT.contains("cite [m] for that end, never the line's own [n]")
                && SYSTEM_PROMPT.contains("A line marked `start corrected` or `end corrected`")
                && SYSTEM_PROMPT.contains("a date the line does not mark still is"),
            "{SYSTEM_PROMPT}"
        );

        // 张三 is changed again, this time the end (review of #976): both ends are person-edited, and the note takes the most recent change
        let zhang_now = row_of("Zhang San").await?;
        utopia_store::temporal::correct_interval(
            &f.pool,
            zhang_now,
            utopia_store::graph::Validity {
                from: Some(day("2023-02-01T00:00:00Z")),
                from_precision: Some("day"),
                to: Some(day("2024-07-01T00:00:00Z")),
                to_precision: Some("day"),
                attested_at: None,
                from_grade: None,
                end_after_period: false,
            },
        )
        .await?
        .expect("the row was live");
        utopia_store::audit::record(
            &f.pool,
            Some(f.kb),
            f.user.id,
            "fact.time_corrected",
            "fact",
            Some(zhang_now),
            json!({ "note": "The handover was on 1 July" }),
        )
        .await?;
        let text = dispatch(
            &chat,
            &mut ToolSink::default(),
            "entity_facts",
            &json!({"entity": "Project Aurora"}),
        )
        .await
        .text;
        assert!(
            line(&text, "Zhang San")
                .contains("2023-02-01 → 2024-07-01, corrected: The handover was on 1 July)"),
            "{text}"
        );

        // The line that closes 李四's segment no longer has valid evidence (周七's document was deleted, review of #986): the line still states who took over,
        // but carries no number and adds nothing to the list; this line's number still opens the handover document
        sqlx::query("UPDATE documents SET deleted_at = now() WHERE id = $1")
            .bind(d_update)
            .execute(&f.pool)
            .await?;
        let mut sink = ToolSink::default();
        for tool in ["entity_facts", "neighbors", "timeline"] {
            let text = dispatch(&chat, &mut sink, tool, &json!({"entity": "Project Aurora"}))
                .await
                .text;
            let li_line = line(&text, "Li Si");
            assert!(
                li_line.contains("2024-07-05 → 2025-09-01, superseded by Zhou Qi)"),
                "{tool}: {text}"
            );
            let l = marks(li_line);
            assert_eq!(l.len(), 1, "{tool}: {text}");
            assert_eq!(chunk_of(&sink, l[0]), s.handover, "{tool}: the line's own passage");
        }
        let path = dispatch(
            &chat,
            &mut sink,
            "paths_between",
            &json!({"from": "Wang Wu", "to": "Li Si"}),
        )
        .await
        .text;
        assert!(
            path.contains("2024-07-05 → 2025-09-01, superseded by Zhou Qi)"),
            "{path}"
        );
        assert!(
            !sink.source_ids.contains(&update.to_string()),
            "a closer without live evidence adds no entry: {:?}",
            sink.sources
        );
        anyhow::Ok(())
    }
    .await;
    f.cleanup().await?;
    run
}
