//! Agentic exploration for semantic mapping of questions: reads the mounted source's schema plus the KB's existing concepts, and has the LLM propose
//! mappings of the form "business concept (Metric/Dimension entity) -> data asset definition".
//!
//! Proposals are written to `concept_mappings` (status = proposed) -> the Review page gets its own tier;
//! after Confirm / Reject the status becomes confirmed, and question answering reads only the confirmed ones.
//!
//! **Previously it was a 0.6-confidence `mapped_to` fact**, surfacing through the "low-confidence fact" tier.
//! The reason for moving it out is in 0011: it is not an assertion about the world, it is configuration -- and the "confirm" action then
//! was `UPDATE facts SET confidence = 1.0`, editing in place a table that must never be edited in place.
//! The agent only proposes; the authority to make a mapping take effect rests with a person -- the same philosophy as resolution's "split rather than merge when in doubt".

use crate::llm_util;
use crate::state::AppState;
use utopia_store::exploration_runs::drop_reason;
use uuid::Uuid;

const MAX_SCHEMA_CHARS: usize = 12_000;

/// The legend line at the start of the schema text: what the square brackets in column rows mean
const COLUMN_MARKERS: &str =
    "Column markers: [PK] single-column primary key; [FK→schema.table] foreign key to that table.
";

/// One column in the schema text: `  name type [PK, FK→schema.table] -- comment`.
///
/// Key markers go after the type and before the comment, in square brackets: `-- ` already separates the comment, so writing `-- PK`
/// would be indistinguishable from a comment whose content happens to be "PK". A primary key that is also a foreign key (a one-to-one extension table) gets both marked.
/// Nullability is not written in: on a wide table almost every column is NOT NULL, and an extra dozen-plus characters per column would make
/// fewer columns fit in `MAX_SCHEMA_CHARS`, while neither prompt uses it (#502)
fn column_line(c: &crate::query_engine::SchemaColumn) -> String {
    let mut marks = Vec::new();
    if c.is_primary_key {
        marks.push("PK".to_string());
    }
    if let Some(target) = &c.references_table {
        marks.push(format!("FK→{target}"));
    }
    let marks = if marks.is_empty() {
        String::new()
    } else {
        format!(" [{}]", marks.join(", "))
    };
    let comment = c
        .comment
        .as_deref()
        .map(|x| format!(" -- {x}"))
        .unwrap_or_default();
    format!(
        "  {} {}{marks}{comment}
",
        c.column, c.data_type
    )
}

/// Maximum number of mappings proposed per round.
///
/// **The cap follows the size of the schema.** A hardcoded 12 is ample for a small database of three tables,
/// but for a wide table of eighty columns it is a ceiling on coverage: measured on TPC-H's eight tables and twenty-four mappings,
/// under a cap of twelve coverage was 25%, and what was missed included the core revenue
/// mapping that appears in seven benchmark queries (#501).
///
/// Three per table is an estimate, not a law -- a fact table deserves far more than three mappings, and a
/// lookup table deserves none. The floor only needs to beat a constant: **a large schema is not capped from the start**.
/// The floor stays 12, so a small schema does not shrink because of it; the ceiling of 60 keeps prompt size and one round's human review
/// manageable.
fn proposal_cap(tables: i32) -> i32 {
    (tables * 3).clamp(12, 60)
}

/// The JSON the model returns is often wrapped in a code fence; strip it.
fn json_body(reply: &str) -> &str {
    reply
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
}

/// At most this many questions are kept per round of descriptions. More and nobody answers them; eight already fills a page
const MAX_DATA_QUESTIONS: usize = 8;

/// The model returns `{"description": …, "questions": […]}`. If the description is empty, drop the whole thing --
/// an empty description overwriting last round's good description is worse than not writing one
fn parse_description(reply: &str) -> Option<(String, Vec<String>)> {
    let v: serde_json::Value = serde_json::from_str(json_body(reply)).ok()?;
    let description = v["description"].as_str()?.trim().to_string();
    if description.is_empty() {
        return None;
    }
    let questions = v["questions"]
        .as_array()
        .map(|qs| {
            qs.iter()
                .filter_map(|q| q.as_str())
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .map(str::to_string)
                .take(MAX_DATA_QUESTIONS)
                .collect()
        })
        .unwrap_or_default();
    Some((description, questions))
}

/// The data description written by exploration, along with the points it is unsure about (#570).
///
/// **Two halves, two sources.** What the schema and comments state -- what a row is, keys, units, code values, the time axis,
/// which two columns look alike -- exploration can read, and it is written here; what the schema does not say -- test orders do not count, valid orders are
/// 2/3/4, GMV uses amount paid not pre-discount -- exploration cannot generate, **and it must not guess**: a guessed convention goes into
/// the prompt, and question answering would compute by it, which is worse than having none. So what is unsure is written as a question, a person answers, and the answer goes into
/// `data_conventions`; this does not touch that column.
///
/// The language follows the ontology's language (0004: generated text follows the corpus, not the interface).
async fn describe_data(
    state: &AppState,
    client: &utopia_llm::LlmClient,
    settings: &utopia_core::models::LlmSettings,
    kb: &utopia_core::models::KnowledgeBase,
    schema_txt: &str,
) -> anyhow::Result<()> {
    let lang = if kb.ontology_lang == "zh" {
        "Chinese"
    } else {
        "English"
    };
    let prompt = format!(
        "You are documenting a database for analysts who will ask questions about it in plain \
         language. Reply with ONLY a JSON object {{\"description\": \"...\", \"questions\": [\"...\"]}}.\n\
         description ({lang}, 10 to 20 short lines): what each table is a table of and what one \
         row is; which columns are keys; the unit of money and quantity columns ONLY where a \
         comment states it; what status or code values mean ONLY where a comment states it; \
         which columns are the time axis; which columns look alike and how they differ (gross vs \
         net, list vs paid). State only what the schema and its comments say. Do not invent \
         business rules, thresholds, or which rows count.\n\
         questions ({lang}, at most {MAX_DATA_QUESTIONS}): the conventions you would need to \
         compute business figures correctly but the schema does not state, each phrased so the \
         owner of the data can answer in one line — which status values count, whether flagged \
         rows (test, internal, gift) are excluded, which of two similar amount columns is the \
         figure, units where no comment states them, whether net figures subtract refunds. Ask \
         only what the schema leaves open.\n\
         Schemas:\n{schema_txt}"
    );
    let _permit = llm_util::acquire_chat(state, settings).await;
    let reply = client
        .chat(&[utopia_llm::ChatMessage {
            role: "user".into(),
            content: prompt,
        }])
        .await?;
    let Some((description, questions)) = parse_description(&reply) else {
        anyhow::bail!(
            "description reply did not parse: {}",
            reply.chars().take(120).collect::<String>()
        );
    };
    utopia_store::kbs::set_data_description(&state.pool, kb.id, &description, &questions).await?;
    tracing::info!(kb_id = %kb.id, lines = description.lines().count(), questions = questions.len(), "数据描述已写");
    Ok(())
}

/// Exploration turns the schema's measures and dimensions into Metric / Dimension entities, but these two classes are in no
/// built-in ontology package -- since 0009, a new database no longer ships with classes. Without them, the `type_id` lookup below finds nothing,
/// and every proposal is swallowed by `continue`, leaving the page saying only "queued" with nothing after it (#223).
/// So before exploring, backfill the two classes: builtin, with descriptions for the extraction prompt, which the ontology page can edit
pub(crate) async fn ensure_concept_types(pool: &sqlx::PgPool, kb_id: Uuid) -> anyhow::Result<()> {
    for (key, label, description) in [
        (
            "metric",
            "Metric",
            "An aggregatable business quantity (revenue, order count, average ticket) that              maps to a definition in a mounted database.",
        ),
        (
            "dimension",
            "Dimension",
            "A group-by attribute (region, month, product line) that maps to a column in a              mounted database.",
        ),
    ] {
        sqlx::query(
            "INSERT INTO entity_types (id, kb_id, key, label, builtin, description)
             SELECT $1, $2, $3, $4, TRUE, $5
             WHERE NOT EXISTS (SELECT 1 FROM entity_types WHERE kb_id = $2 AND key = $3)",
        )
        .bind(Uuid::now_v7())
        .bind(kb_id)
        .bind(key)
        .bind(label)
        .bind(description)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// One round of exploration opens its ledger entry, does the work, and closes the ledger (#503).
///
/// **Open the row before doing the work**: a round that crashed still leaves a row, because "failed" and "ran but proposed nothing at all"
/// used to look the same on the page ("no new proposals"), while the action to take is entirely different.
pub async fn explore_mappings(state: &AppState, kb_id: Uuid) -> anyhow::Result<()> {
    let run = utopia_store::exploration_runs::start(&state.pool, kb_id).await?;
    match explore(state, kb_id, run).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // A failure on the error path must not drown out the thing it is reporting (same reasoning as `source_name`)
            let _ = utopia_store::exploration_runs::fail(&state.pool, run, &e.to_string()).await;
            Err(e)
        }
    }
}

async fn explore(state: &AppState, kb_id: Uuid, run: Uuid) -> anyhow::Result<()> {
    let kb = utopia_store::kbs::get(&state.pool, kb_id).await?;
    let settings = utopia_store::settings::get(&state.pool, kb.workspace_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Chat model not configured"))?;
    let client = llm_util::chat_client(&settings)
        .ok_or_else(|| anyhow::anyhow!("Chat model not configured"))?;

    let sources = utopia_store::datasources::mounted(&state.pool, kb_id).await?;
    if sources.is_empty() {
        anyhow::bail!("No data sources mounted");
    }
    ensure_concept_types(&state.pool, kb_id).await?;

    // Each source's schema (read directly by the engine, so it is fresh; capped to prevent a prompt blow-up)
    //
    // **The cap is one total across sources.** Previously that `break` only exited the current source's column loop,
    // and the next source kept appending to the same string -- `MAX_SCHEMA_CHARS` reads like a ceiling,
    // but in practice it is a floor that each source exceeds separately.
    let mut schema_txt = String::from(COLUMN_MARKERS);
    let mut tables_scanned = 0i32;
    let mut columns_scanned = 0i32;
    let mut truncated = false;
    for ds in &sources {
        if truncated {
            break;
        }
        let (engine, conn) = utopia_store::datasources::engine_and_conn(&state.pool, ds.id).await?;
        let cols = crate::query_engine::engine_for(&engine, &conn)?
            .fetch_schema()
            .await?;
        schema_txt.push_str(&format!("\n=== source: {} ===\n", ds.name));
        let mut current = String::new();
        for c in cols {
            let key = format!("{}.{}", c.schema, c.table);
            if key != current {
                current = key.clone();
                tables_scanned += 1;
                schema_txt.push_str(&format!("table {key}:\n"));
            }
            columns_scanned += 1;
            schema_txt.push_str(&column_line(&c));
            if schema_txt.len() > MAX_SCHEMA_CHARS {
                schema_txt.push_str("(truncated)\n");
                truncated = true;
                break;
            }
        }
    }

    let cap = proposal_cap(tables_scanned);
    let source_list: Vec<String> = sources.iter().map(|d| d.name.clone()).collect();
    let _ = utopia_store::exploration_runs::scanned(
        &state.pool,
        run,
        &source_list,
        tables_scanned,
        columns_scanned,
        truncated,
        cap,
    )
    .await;

    // Write the data description and the uncertain questions first, then propose mappings. **The description does not depend on the proposals succeeding**:
    // if the proposal step fails to parse, the whole round errors after the description has already landed; conversely, a failure to write the description
    // should not sink the proposals -- it is a side task, so warn and continue
    if let Err(e) = describe_data(state, &client, &settings, &kb, &schema_txt).await {
        tracing::warn!(%kb_id, error = %e, "数据描述没写成，提议照常");
    }

    // Existing concepts (for merge reuse, avoiding duplicate names)
    let existing: Vec<(String,)> = sqlx::query_as(
        "SELECT e.canonical_name FROM entities e
         JOIN entity_types t ON t.id = e.type_id
         WHERE e.kb_id = $1 AND e.merged_into IS NULL AND t.key IN ('metric','dimension')
         ORDER BY e.canonical_name LIMIT 100",
    )
    .bind(kb_id)
    .fetch_all(&state.pool)
    .await?;
    let existing_names: Vec<String> = existing.into_iter().map(|(n,)| n).collect();

    // Conventions written by a person go into the proposal prompt. The schema has no sentence saying "test orders do not count", and exploration on a wide table scores
    // 0/18 because of it; with this sentence, a proposal can come out as FILTER (WHERE is_test = 0) (#570)
    let conventions = kb
        .data_conventions
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("(none stated)");
    let prompt = format!(
        "You are building the semantic layer of a BI system. Given database schemas, propose \
         business concepts a user would ask about, each mapped to a concrete definition.\n\
         Existing concepts (reuse these names when the meaning matches): {}\n\
         Conventions stated by the owner of this base — apply them in every definition, as \
         filters, unit conversions, or the choice of column: {conventions}\n\
         Schemas:\n{}\n\
         Reply with ONLY a JSON array, each item:\n\
         {{\"name\": \"business concept name\", \"kind\": \"metric\"|\"dimension\", \
         \"source\": \"data source name\", \
         \"definition\": {{\"table\": \"schema.table\", \"expr\": \"SQL expression\", \
         \"sql\": \"full SELECT if joins are needed (optional)\", \"unit\": \"optional\"}}, \
         \"summary\": \"one line: source + expression, shown to reviewers\", \
         \"rationale\": \"why this mapping, citing column comments\"}}\n\
         Metrics are aggregatable quantities (use sum/count/avg in expr); dimensions are \
         group-by columns. Propose at most {cap}, only well-grounded ones.",
        if existing_names.is_empty() {
            "(none)".into()
        } else {
            existing_names.join(", ")
        },
        schema_txt
    );

    let _permit = llm_util::acquire_chat(state, &settings).await;
    let reply = client
        .chat(&[utopia_llm::ChatMessage {
            role: "user".into(),
            content: prompt,
        }])
        .await?;
    let json_str = reply
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let proposals: Vec<serde_json::Value> = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("Mapping proposal parse error: {e}"))?;

    let source_names: Vec<&str> = sources.iter().map(|d| d.name.as_str()).collect();
    let mut accepted = 0usize;
    // Dropped entries are counted, and one example is also kept -- **counts alone are a diagnosis that changes nothing**: "twelve source names did not match"
    // only tells you what to change once paired with "the model said tpch, but the mounted one is tpch-2026-09-08"
    let mut drops: std::collections::BTreeMap<&'static str, (i64, String)> = Default::default();
    let mut note = |reason: &'static str, example: String| {
        let e = drops.entry(reason).or_insert((0, example));
        e.0 += 1;
    };
    let mut covered: std::collections::BTreeSet<String> = Default::default();
    for p in proposals.iter().take(cap as usize) {
        let name = p["name"].as_str().map(str::trim).unwrap_or("");
        let kind = p["kind"].as_str().unwrap_or("");
        let said = p["source"].as_str().map(str::trim).unwrap_or("");
        if name.is_empty() || !matches!(kind, "metric" | "dimension") {
            note(drop_reason::KIND, format!("name={name:?} kind={kind:?}"));
            continue;
        }
        // **With only one source mounted, whatever source the model names counts as it.** There is no ambiguity, and the cost of a mismatch is
        // that the whole proposal disappears: measured, the source was named `tpch-2026-09-08-12-30`, the model replied `tpch` following the schema,
        // and all twelve were swallowed without exception while the task still said done (hit during the first round of #501)
        let source = if sources.len() == 1 {
            source_names[0]
        } else {
            match source_names
                .iter()
                .find(|s| s.eq_ignore_ascii_case(said))
                .copied()
            {
                Some(s) => s,
                None => {
                    note(
                        drop_reason::SOURCE,
                        format!("model said {said:?}, mounted: {}", source_names.join(", ")),
                    );
                    continue;
                }
            }
        };
        let type_id: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM entity_types WHERE kb_id = $1 AND key = $2")
                .bind(kb_id)
                .bind(kind)
                .fetch_optional(&state.pool)
                .await?;
        let Some((type_id,)) = type_id else {
            note(
                drop_reason::TYPE,
                format!("no entity type {kind:?} in this base"),
            );
            continue;
        };

        // Concept entities: go through resolution (same-name merge; without a vector context, merge as in v1).
        // There is no chunk text to give -- these names come from exploring the data source's schema, not from sentences in a document
        let resolved = utopia_store::resolution::resolve_mention(
            &state.pool,
            kb_id,
            Some(type_id),
            name,
            None,
            None,
            None,
            &[],
        )
        .await?;

        // The definition is split into columns written into concept_mappings (0011). Previously it was a blob stuffed into
        // `object_value` JSON, with the object hung on a relation called mapped_to --
        // and that relation was a row in the ontology, alongside works_at. **It is not an assertion about the world,
        // it is configuration**, so it moved to its own table
        let def = &p["definition"];
        if !def.is_object() {
            note(
                drop_reason::DEFINITION,
                format!("{name:?} has no definition object"),
            );
            continue;
        }
        let s = |k: &str| {
            def[k]
                .as_str()
                .filter(|x| !x.is_empty())
                .map(str::to_string)
        };
        // The numerator of coverage. The denominator is the number of tables scanned this round -- **eleven proposals against a wide table of eighty columns
        // and eleven that exactly cover a small database look identical in concept_mappings**
        if let Some(t) = s("table") {
            covered.insert(t);
        }
        let (_, written) = utopia_store::mappings::propose(
            &state.pool,
            kb_id,
            resolved.entity_id,
            source,
            s("table").as_deref(),
            s("expr").as_deref(),
            s("sql").as_deref(),
            s("unit").as_deref(),
            // summary is the sentence for people: the Review list and the question-answering prompt both rely on it
            p["summary"].as_str().or(def["summary"].as_str()),
            def["derived"].as_bool().unwrap_or(false),
        )
        .await?;
        if written {
            accepted += 1;
        } else {
            note(
                drop_reason::DECIDED,
                format!("{name:?} on {source}: already confirmed or rejected"),
            );
        }
    }
    // The ones beyond the cap are not looked at, but they must still be recorded: otherwise returned does not match accepted + dropped,
    // and the ledger's whole purpose is to let a person see "the model returned sixty, we only looked at thirty-six"
    if proposals.len() > cap as usize {
        drops.insert(
            drop_reason::CAP,
            (
                (proposals.len() - cap as usize) as i64,
                format!("model returned {}, cap {cap}", proposals.len()),
            ),
        );
    }

    let dropped = serde_json::Value::Object(
        drops
            .iter()
            .map(|(k, (n, ex))| {
                (
                    (*k).to_string(),
                    serde_json::json!({ "n": n, "example": ex }),
                )
            })
            .collect(),
    );
    let covered: Vec<String> = covered.into_iter().collect();
    if let Err(e) = utopia_store::exploration_runs::finish(
        &state.pool,
        run,
        proposals.len() as i32,
        accepted as i32,
        dropped,
        &covered,
    )
    .await
    {
        // Failing to record the ledger should not make the proposals run in vain, but it also must not pretend it was recorded: leave it in the log
        tracing::warn!(%kb_id, run = %run, error = %e, "探索账没记上");
    }
    tracing::info!(
        %kb_id,
        proposals = accepted,
        returned = proposals.len(),
        tables = tables_scanned,
        covered = covered.len(),
        truncated,
        "映射探索完成，提议已入审核队列"
    );
    // When nothing at all was proposed, the page does not change -- Pending is still 0, and the "queued" message
    // is long gone. Raise it through the alert center so a person knows to refresh the structure or add column comments.
    //
    // **The alert must say how it came to be empty.** Previously it only said "0 mappings, these sources", but "the model returned none at all"
    // and "it returned twelve, all blocked by source-name mismatch" are two different things, with different actions -- the former means add column comments,
    // the latter means check source names. `dropped` is that distinction, and it is in this alert now
    if accepted == 0 {
        if let Err(e) = utopia_store::alerts::raise(
            &state.pool,
            utopia_store::alerts::NewAlert {
                kb_id: Some(kb_id),
                severity: "info",
                kind: utopia_store::alerts::kind::MAPPING_EXPLORATION_EMPTY,
                min_role: utopia_core::models::Role::Editor,
                subject_type: None,
                subject_id: None,
                detail: serde_json::json!({
                    "proposals": 0,
                    "sources": source_names,
                    "returned": proposals.len(),
                    "dropped": drops.iter().map(|(k, (n, _))| (*k, *n))
                        .collect::<std::collections::BTreeMap<_, _>>(),
                    "tables_scanned": tables_scanned,
                }),
            },
        )
        .await
        {
            tracing::warn!(%kb_id, error = %e, "映射探索空结果的告警没写进去");
        }
        state.emit_alert();
    }
    state.emit_review(kb_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{column_line, parse_description, proposal_cap};
    use crate::query_engine::SchemaColumn;

    fn column(pk: bool, fk: Option<&str>, comment: Option<&str>) -> SchemaColumn {
        SchemaColumn {
            schema: "a".into(),
            table: "t".into(),
            column: "id".into(),
            data_type: "integer".into(),
            comment: comment.map(str::to_string),
            is_primary_key: pk,
            references_table: fk.map(str::to_string),
        }
    }

    #[test]
    fn a_column_line_marks_its_keys_before_the_comment() {
        assert_eq!(
            column_line(&column(false, None, None)),
            "  id integer
"
        );
        assert_eq!(
            column_line(&column(true, None, Some("Order id"))),
            "  id integer [PK] -- Order id
"
        );
        assert_eq!(
            column_line(&column(false, Some("a.orders"), None)),
            "  id integer [FK→a.orders]
"
        );
        // One-to-one extension table: the primary key is also a foreign key, so mark both
        assert_eq!(
            column_line(&column(true, Some("a.p1"), None)),
            "  id integer [PK, FK→a.p1]
"
        );
        // An ordinary column whose comment says PK is kept distinct from the marker
        assert_eq!(
            column_line(&column(false, None, Some("PK"))),
            "  id integer -- PK
"
        );
    }

    #[test]
    fn a_description_is_kept_only_when_it_says_something() {
        let (d, qs) = parse_description(
            "```json\n{\"description\": \"dw.dwd_ord_dtl: one row per order line.\", \
             \"questions\": [\" Which ord_st values count? \", \"\", \"Exclude is_test = 1?\"]}\n```",
        )
        .unwrap();
        assert_eq!(d, "dw.dwd_ord_dtl: one row per order line.");
        // Drop empty questions, strip leading and trailing whitespace, keep the order
        assert_eq!(
            qs,
            vec!["Which ord_st values count?", "Exclude is_test = 1?"]
        );
        // An empty description is dropped entirely: an empty description overwriting last round's good one is worse than not writing one
        assert!(parse_description("{\"description\": \"  \", \"questions\": []}").is_none());
        assert!(parse_description("not json").is_none());
        // Questions may be absent, and the description is still accepted
        assert_eq!(
            parse_description("{\"description\": \"x\"}").unwrap().1,
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_bigger_schema_gets_a_bigger_cap() {
        // A small database does not shrink: the hardcoded 12 was correct at this end
        assert_eq!(proposal_cap(1), 12);
        assert_eq!(proposal_cap(4), 12);
        // TPC-H with eight tables: the old cap of 12 covered only 6 of the 24 true mappings (#501)
        assert_eq!(proposal_cap(8), 24);
        // TPC-DS with twenty-four tables
        assert_eq!(proposal_cap(24), 60);
        // Anything larger still stops here: the prompt and one round's human review each have their own ceiling
        assert_eq!(proposal_cap(300), 60);
        // Even if no table was scanned at all (source unreachable, schema empty), it should not be 0 --
        // **a zero cap would turn "unreachable" into "the model proposed nothing at all", and the two things would be mixed up again**
        assert_eq!(proposal_cap(0), 12);
    }
}
