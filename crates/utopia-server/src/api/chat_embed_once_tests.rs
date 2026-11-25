//! Within one turn, the same sentence is embedded only once.
//!
//! Candidates for same-named entities are ranked by the question (`rank_by_question`): the question is embedded into a vector and compared with each candidate's context profile.
//! Each by-name entity lookup within a turn goes there with the same sentence; previously it re-embedded every time, reading the model
//! setting and calling the embedding service again. Alignment when predicates do not match (`aligned_predicates`) works the same way.
//!
//! The fake embedding service records the text of each request, and every one returns the same direction. Of two entities both named Acme, the one with the close profile
//! has fewer facts: without a question (MCP has none), ranking by fact count puts the distant one first.
//!
//! - Tool layer (the same sink means the same turn): find_entities, entity_facts by name and
//!   neighbors all pick the close one; the question is embedded once; a mismatched predicate asked twice embeds only that word
//!   once; with a different sink (the next turn), the question is embedded again
//! - Whole conversation: the model first calls search_chunks (its query is that very question), then find_entities, then by name
//!   entity_facts; the embedding service receives only one request
//! - Mapping selection (`mapping_index::relevant`), chunk retrieval (`retrieval::hybrid`) and by-name entity lookup
//!   share this turn's cache: the question is embedded once; callers without a cache each embed on their own (follow-up to #971)
//!
//! Skipped rather than failing without `UTOPIA_DATABASE_URL`. It creates and drops its own database, never touching an existing one.
use super::*;
use crate::api::tools::{dispatch, ToolCtx, ToolSink};

const QUESTION: &str = "Where is the Acme lab?";

/// Fake embedding service: records the input of each request and always returns (1, 0, 0)
#[derive(Clone, Default)]
struct Embedder {
    inputs: Arc<Mutex<Vec<Vec<String>>>>,
}

async fn embeddings(
    State(e): State<Embedder>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let texts: Vec<String> = body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(str::to_string))
        .collect();
    let data: Vec<serde_json::Value> = (0..texts.len())
        .map(|i| json!({"index": i, "embedding": [1.0, 0.0, 0.0]}))
        .collect();
    e.inputs.lock().unwrap().push(texts);
    Json(json!({"data": data}))
}

impl Embedder {
    /// One line per request; each line is the text embedded that time
    fn requests(&self) -> Vec<Vec<String>> {
        self.inputs.lock().unwrap().clone()
    }
}

struct Acme {
    near: Uuid,
    far: Uuid,
}

/// Start the fake embedding service and point the embedding model of this database's workspace at it; seed two Acmes:
/// the close one has a profile in the same direction as the fake vector, with one fact; the distant one has an orthogonal profile with two facts
async fn seed(f: &Fx) -> anyhow::Result<(Embedder, tokio::task::JoinHandle<()>, Acme)> {
    let embedder = Embedder::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let app = axum::Router::new()
        .route("/embeddings", axum::routing::post(embeddings))
        .with_state(embedder.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    sqlx::query(
        "UPDATE llm_settings SET embed_base_url = $1, embed_model = 'fake-embedding'
          WHERE workspace_id = (SELECT workspace_id FROM knowledge_bases WHERE id = $2)",
    )
    .bind(&base)
    .bind(f.kb)
    .execute(&f.pool)
    .await?;

    let id = Uuid::now_v7;
    let (org, place, located_in) = (id(), id(), id());
    let (near, far, harbor, port) = (id(), id(), id(), id());
    // Only locally generated UUIDs are inserted
    sqlx::raw_sql(&format!(
        r#"
        INSERT INTO entity_types(id,kb_id,key,label) VALUES
            ('{org}','{kb}','organization','Organization'), ('{place}','{kb}','place','Place');
        INSERT INTO relation_types(id,kb_id,key,label,temporal) VALUES
            ('{located_in}','{kb}','located_in','located in','state');
        INSERT INTO entities(id,kb_id,type_id,canonical_name,profile_embedding) VALUES
            ('{near}','{kb}','{org}','Acme','[1,0,0]'),
            ('{far}','{kb}','{org}','Acme','[0,1,0]'),
            ('{harbor}','{kb}','{place}','Harbor',NULL),
            ('{port}','{kb}','{place}','Port',NULL);
        INSERT INTO facts(id,kb_id,subject_id,predicate_id,object_id,confidence) VALUES
            ('{f1}','{kb}','{near}','{located_in}','{harbor}',0.9),
            ('{f2}','{kb}','{far}','{located_in}','{harbor}',0.9),
            ('{f3}','{kb}','{far}','{located_in}','{port}',0.9);
        "#,
        kb = f.kb,
        f1 = id(),
        f2 = id(),
        f3 = id(),
    ))
    .execute(&f.pool)
    .await?;
    Ok((embedder, server, Acme { near, far }))
}

/// Turn-level tool context; `question` None means a call with no question, like MCP's.
/// `embed` reads the model settings per workspace, so use the workspace this database actually lives in
async fn ctx<'f>(f: &'f Fx, question: Option<&'f str>) -> anyhow::Result<ToolCtx<'f>> {
    let workspace_id: Uuid =
        sqlx::query_scalar("SELECT workspace_id FROM knowledge_bases WHERE id = $1")
            .bind(f.kb)
            .fetch_one(&f.pool)
            .await?;
    Ok(ToolCtx {
        state: &f.state,
        kb_id: f.kb,
        workspace_id,
        mounted_sources: &[],
        can_write: false,
        actor: Some(f.user.id),
        via_token: None,
        question,
    })
}

#[tokio::test]
async fn a_turn_embeds_its_question_and_each_word_once() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![])).await? else {
        return Ok(());
    };
    let (embedder, server, acme) = seed(&f).await?;
    let run = async {
        // No question (as with MCP): nothing is embedded; picks by fact count, and the distant one comes first
        let plain = dispatch(
            &ctx(&f, None).await?,
            &mut ToolSink::default(),
            "find_entities",
            &json!({"name": "Acme"}),
        )
        .await;
        assert!(
            plain
                .text
                .lines()
                .nth(1)
                .is_some_and(|l| l.starts_with(&acme.far.to_string())),
            "{}",
            plain.text
        );
        assert!(embedder.requests().is_empty());

        let ctx = ctx(&f, Some(QUESTION)).await?;
        let mut turn = ToolSink::default();

        // Three lookups of Acme by name: all pick the close-profile one; the question is embedded only once
        let found = dispatch(&ctx, &mut turn, "find_entities", &json!({"name": "Acme"})).await;
        assert!(
            found
                .text
                .starts_with(&format!("Best match: {} |", acme.near)),
            "{}",
            found.text
        );
        let facts = dispatch(
            &ctx,
            &mut turn,
            "entity_facts",
            &json!({"entity_id": "Acme"}),
        )
        .await;
        assert!(
            facts.text.contains("closest to the question") && !facts.text.contains("Port"),
            "the near Acme (one fact, Harbor): {}",
            facts.text
        );
        let near = dispatch(&ctx, &mut turn, "neighbors", &json!({"entity": "Acme"})).await;
        assert!(
            near.text.contains("closest to the question") && !near.text.contains("Port"),
            "{}",
            near.text
        );
        assert_eq!(embedder.requests(), vec![vec![QUESTION.to_string()]]);

        // The mismatched predicate asked twice: that word is embedded only once; the question is still not re-embedded
        for _ in 0..2 {
            dispatch(
                &ctx,
                &mut turn,
                "entity_facts",
                &json!({"entity_id": "Acme", "predicate": "headquarters"}),
            )
            .await;
        }
        assert_eq!(
            embedder.requests(),
            vec![vec![QUESTION.to_string()], vec!["headquarters".to_string()]]
        );

        // The next turn is another sink: the question is embedded again, and the pick is still the close one
        let mut next = ToolSink::default();
        let again = dispatch(&ctx, &mut next, "find_entities", &json!({"name": "Acme"})).await;
        assert!(
            again
                .text
                .starts_with(&format!("Best match: {} |", acme.near)),
            "{}",
            again.text
        );
        assert_eq!(embedder.requests().len(), 3);
        anyhow::Ok(())
    }
    .await;
    server.abort();
    f.cleanup().await?;
    run
}

#[tokio::test]
async fn a_chat_turn_embeds_its_question_once() -> anyhow::Result<()> {
    // The model first uses that question to retrieve chunks: the vector path embeds it; the later by-name entity lookup reuses the same vector
    let Some(f) = fixture(Scripted::new(vec![
        Reply::Tool("search_chunks", r#"{"query":"Where is the Acme lab?"}"#),
        Reply::Tool("find_entities", r#"{"name":"Acme"}"#),
        Reply::Tool("entity_facts", r#"{"entity_id":"Acme"}"#),
        Reply::Text("The Acme lab is in Harbor."),
    ]))
    .await?
    else {
        return Ok(());
    };
    let (embedder, server, _) = seed(&f).await?;
    let run = async {
        let sse = f.ask(QUESTION).await?;
        assert!(sse.contains("event: done"), "{sse}");
        assert_eq!(sse.matches("event: step").count(), 3, "{sse}");
        assert_eq!(embedder.requests(), vec![vec![QUESTION.to_string()]]);
        anyhow::Ok(())
    }
    .await;
    server.abort();
    f.cleanup().await?;
    run
}

/// Mapping selection, chunk retrieval and by-name entity lookup all use the same vector; a different search query is embedded again;
/// callers without a cache (HTTP retrieval, the mapping page) each embed on their own
#[tokio::test]
async fn mappings_search_and_the_graph_tools_share_one_embedding() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![])).await? else {
        return Ok(());
    };
    let (embedder, server, acme) = seed(&f).await?;
    let run = async {
        let ws: Uuid = sqlx::query_scalar("SELECT workspace_id FROM knowledge_bases WHERE id = $1")
            .bind(f.kb)
            .fetch_one(&f.pool)
            .await?;
        let mut cache = crate::llm_util::EmbedCache::default();

        // This database has no confirmed mappings at all: both mapping paths come back empty, but the question was already embedded once
        let mappings =
            crate::mapping_index::relevant(&f.state, f.kb, ws, QUESTION, 8, Some(&mut cache))
                .await?;
        assert!(mappings.is_empty());
        assert_eq!(embedder.requests(), vec![vec![QUESTION.to_string()]]);
        assert!(cache.contains_key(QUESTION));

        // Chunk retrieval reuses the same vector: the index has no chunks, the vector path still runs, it just does not embed again
        let chunks =
            crate::retrieval::hybrid(&f.state, f.kb, ws, QUESTION, 8, None, Some(&mut cache))
                .await?;
        assert!(chunks.is_empty());
        assert_eq!(embedder.requests().len(), 1, "search reuses the vector");

        // Lookup by name: still the close-profile one
        let ctx = ctx(&f, Some(QUESTION)).await?;
        let mut turn = ToolSink {
            embeddings: cache,
            ..ToolSink::default()
        };
        let found = dispatch(&ctx, &mut turn, "find_entities", &json!({"name": "Acme"})).await;
        assert!(
            found
                .text
                .starts_with(&format!("Best match: {} |", acme.near)),
            "{}",
            found.text
        );
        assert_eq!(
            embedder.requests().len(),
            1,
            "the graph lookup reuses it too"
        );

        // The model changed the query: another sentence, embedded once and recorded in the same cache
        let _ = crate::retrieval::hybrid(
            &f.state,
            f.kb,
            ws,
            "Acme lab address",
            8,
            None,
            Some(&mut turn.embeddings),
        )
        .await?;
        assert_eq!(
            embedder.requests().last(),
            Some(&vec!["Acme lab address".to_string()])
        );
        assert!(turn.embeddings.contains_key("Acme lab address"));

        // Callers without this turn's cache each embed on their own
        let _ = crate::retrieval::hybrid(&f.state, f.kb, ws, QUESTION, 8, None, None).await?;
        let _ = crate::mapping_index::relevant(&f.state, f.kb, ws, QUESTION, 8, None).await?;
        assert_eq!(embedder.requests().len(), 4);
        anyhow::Ok(())
    }
    .await;
    server.abort();
    f.cleanup().await?;
    run
}
