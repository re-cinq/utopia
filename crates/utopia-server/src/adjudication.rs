//! Batched entity-resolution adjudication task: consumes gray-zone pairs in stage=adjudicating from the review queue.
//! Checks the adjudication cache first; cache misses are batched into one LLM call that adjudicates several pairs;
//! high-confidence same -> merged automatically (reversible), high-confidence different -> kept apart automatically,
//! everything else goes to a human. Without a configured model everything goes to a human; this task failing or being absent never affects extraction or querying.

use crate::governance::{look_again, Look};
use crate::llm_util;
use crate::state::AppState;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use utopia_core::models::ReviewItem;
use utopia_core::AppError;
use utopia_store::governance as gov;
use uuid::Uuid;

const BATCH_SIZE: i64 = 12;
const AUTO_CONF: f32 = 0.8;
const MAX_ROUNDS: usize = 20;
/// Cache key: type + both names + fact digest + precedents (independent of entity ids, so re-uploading a document does not pay twice).
/// **Precedents are part of the key too**: the answer changes with precedents (0025 says exactly this, which is why the governance path skips the cache altogether);
/// when a human adjudicates another one the key changes and the old answer is naturally invalidated, no cleanup needed
fn pair_key(item: &ReviewItem, precedents: &[String]) -> String {
    let side = |s: &utopia_core::models::ReviewSide| {
        format!(
            "{}|{}|{}",
            s.name.to_lowercase(),
            // A side whose type was not determined must still be cacheable (0009)
            s.type_label.as_deref().unwrap_or("untyped"),
            s.top_facts.join(";")
        )
    };
    let mut sides = [side(&item.left), side(&item.right)];
    sides.sort();
    let digest =
        Sha256::digest(format!("{}##{}", sides.join("##"), precedents.join("\n")).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
pub(crate) fn seed_measurement_cache_key(item: &ReviewItem, precedents: &[String]) -> String {
    pair_key(item, precedents)
}

/// Whether a pair the batch left undecided gets a second look with tools (0028): no verdict or confidence below the bar; pairs the hard rules already stop are not re-checked
/// (different major categories, version suffixes, one-sentence name mentions: a second look cannot change the rule); retracted pairs are not re-checked, that is the human's call
fn wants_another_look(
    item: &ReviewItem,
    p: &gov::Precedents,
    same: Option<bool>,
    conf: f32,
) -> bool {
    let unsettled = same.is_none() || conf < AUTO_CONF;
    let ruled = gov::types_conflict(
        item.left.type_label.as_deref(),
        item.right.type_label.as_deref(),
    ) || matches!(
        gov::name_shape(&item.left.name, &item.right.name),
        gov::NameShape::Version | gov::NameShape::Phrase
    );
    unsettled && !ruled && p.reverts.is_empty()
}

/// Pairs proposed by name-vector recall (0041, second cut): two **different strings** were put together because they are similar.
/// "Proposed for similarity" and "proposed for an identical name" are different evidence strengths and must not share one auto-merge line: batch adjudication only sees
/// the names and a few facts; on the measurement bench it merged "Zhang Wei" into "finance director Zhang Wei" (0.85, over the line). So for this kind of
/// pair, same always goes through a second look with tools, which can read both sides' facts and source text before deciding; different and unsure
/// stay as they are, since keeping apart is the safe direction. Do not skip on `ruled` or retraction: those two are shortcuts for "a second look cannot change it"; but here the point is
/// exactly the second look.
fn similarity_proposed(item: &ReviewItem) -> bool {
    utopia_core::review_reasons::similarity_proposed(item.reason.as_deref())
}

fn needs_second_look(
    item: &ReviewItem,
    p: &gov::Precedents,
    same: Option<bool>,
    conf: f32,
) -> bool {
    wants_another_look(item, p, same, conf) || !batch_verdict_may_apply(item, same)
}

/// Whether the batch's first-look verdict can be applied without a second look. For pairs proposed by name vectors, saying same is not enough: if the second look did not run
/// (budget used up, model error), it goes to a human: it is not merged on the batch's verdict and that verdict is not cached, since once cached
/// every later encounter of the pair would merge straight from cache and the second look would never happen (#889 review)
pub(crate) fn batch_verdict_may_apply(item: &ReviewItem, same: Option<bool>) -> bool {
    !(similarity_proposed(item) && same == Some(true))
}

/// Reason given when the second look did not run and the pair is escalated
pub(crate) const SECOND_LOOK_UNAVAILABLE: &str = "escalate_unsure|second_look_unavailable";

/// What an adjudication became once applied: the second-tier row records applied or proposed based on it
enum Outcome {
    Merged(Uuid),
    Kept,
    Escalated,
}

/// A pair the second tier reviewed gets one `agent_decisions` row (0028): trace, question and the calls spent are all in it.
/// The budget is counted from these rows and the agent queue also reads from here: whether governance is on or not, everything the machine looked at is visible
async fn record_look(
    state: &AppState,
    kb_id: Uuid,
    run_id: Uuid,
    item: &ReviewItem,
    p: &gov::Precedents,
    look: &Look,
    outcome: &Outcome,
) -> anyhow::Result<()> {
    let action = match look.same {
        Some(true) => "merge",
        Some(false) => "keep",
        None => "unsure",
    };
    let (status, merge_id) = match outcome {
        Outcome::Merged(id) => ("applied", Some(*id)),
        Outcome::Kept => ("applied", None),
        Outcome::Escalated => ("proposed", None),
    };
    gov::record(
        &state.pool,
        kb_id,
        gov::NewDecision {
            run_id,
            target_id: item.id,
            action,
            confidence: look.conf,
            reason: look.why.as_deref(),
            precedents: gov::precedents_json(p),
            status,
            merge_id,
            question: look.question.as_deref(),
            trace: serde_json::Value::Array(look.trace.clone()),
            calls: look.calls,
        },
    )
    .await?;
    Ok(())
}

/// Only one adjudication runs per knowledge base at a time (same reasoning as `ontology_index::PER_KB`).
///
/// Enqueue deduplication only blocks jobs that are still queued; when a batch of documents finishes extracting one after another, a later job is enqueued while the earlier one
/// is already `running`, so seven jobs run on the same knowledge base at once. Each of them reads `pending_adjudications`
/// and gets the **same batch** of pending items: model calls multiply by seven, and when two jobs reach a verdict on the same pair, the second one
/// hits the `agent_decisions_open_idx` unique index and fails (observed 2026-09-08: among seven jobs,
/// two failed this way).
///
/// **Wait, do not skip**: a late arrival waits for the earlier one to finish before entering, and re-queries the pending items on entry,
/// which usually leaves only the pairs added after the earlier one read them.
static PER_KB: LazyLock<Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

/// A batch's name: the two names of its first pair. Enough to tell which batch is which in logs; the full list of names would be too long
pub(crate) fn batch_name(pairs: &[utopia_extract::AdjudicationPair]) -> String {
    match pairs.first() {
        Some(p) => format!("{} / {}", p.left.name, p.right.name),
        None => "(empty)".into(),
    }
}

fn lock_for(kb_id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
    PER_KB
        .lock()
        .expect("per-kb adjudication lock table poisoned")
        .entry(kb_id)
        .or_default()
        .clone()
}

pub async fn adjudicate_entities(state: &AppState, kb_id: Uuid) -> anyhow::Result<()> {
    let lock = lock_for(kb_id);
    let _serial = lock.lock().await;
    let kb = utopia_store::kbs::get(&state.pool, kb_id).await?;
    let settings = utopia_store::settings::get(&state.pool, kb.workspace_id).await?;
    let client = settings.as_ref().and_then(llm_util::chat_client);
    let model = settings
        .as_ref()
        .and_then(|s| s.chat_model.clone())
        .unwrap_or_default();

    let Some(client) = client else {
        // No model available: everything goes to a human and the task itself finishes successfully
        let items =
            utopia_store::resolution::pending_adjudications(&state.pool, kb_id, 500).await?;
        for item in items {
            utopia_store::resolution::escalate_review(&state.pool, item.id, "escalate_no_model")
                .await?;
        }
        state.emit_review(kb_id);
        return Ok(());
    };
    // Second-tier rows all hang off this one task run
    let run_id = Uuid::now_v7();

    for _ in 0..MAX_ROUNDS {
        let items =
            utopia_store::resolution::pending_adjudications(&state.pool, kb_id, BATCH_SIZE).await?;
        if items.is_empty() {
            break;
        }

        // Tier one: adjudication cache. Precedents (what humans did in this knowledge base for these names, along with the reasons they wrote)
        // are fetched first: they go into both the prompt and the cache key
        let mut to_ask: Vec<(ReviewItem, String, gov::Precedents, Vec<String>)> = Vec::new();
        for item in items {
            let p = gov::precedents_for(&state.pool, kb_id, &item).await?;
            let precedents = gov::render_lines(&p);
            let key = pair_key(&item, &precedents);
            match utopia_store::resolution::get_verdict(&state.pool, kb_id, &key).await? {
                Some((same, conf)) => {
                    apply_verdict(state, kb_id, &item, same, conf, "cached", None).await?;
                }
                None => to_ask.push((item, key, p, precedents)),
            }
        }
        if to_ask.is_empty() {
            continue;
        }

        // Tier two: batched LLM adjudication
        let pairs: Vec<utopia_extract::AdjudicationPair> = to_ask
            .iter()
            .map(
                |(item, _, _, precedents)| utopia_extract::AdjudicationPair {
                    left: utopia_extract::AdjudicationSide {
                        name: item.left.name.clone(),
                        type_label: item
                            .left
                            .type_label
                            .clone()
                            .unwrap_or_else(|| "untyped".into()),
                        facts: item.left.top_facts.clone(),
                    },
                    right: utopia_extract::AdjudicationSide {
                        name: item.right.name.clone(),
                        type_label: item
                            .right
                            .type_label
                            .clone()
                            .unwrap_or_else(|| "untyped".into()),
                        facts: item.right.top_facts.clone(),
                    },
                    precedents: precedents.clone(),
                    proposed_because: utopia_extract::proposed_because(
                        utopia_core::review_reasons::name_vector_cosine(item.reason.as_deref()),
                    ),
                },
            )
            .collect();
        let messages = utopia_extract::build_adjudication_messages(&pairs);
        // Call/parse failure -> the job retries with backoff; once retries are exhausted the rows stay in the queue, where a human can still decide
        let reply = {
            // The second look acquires the same model's permit again; the batch call releases it when done, otherwise with a limit of 1 it would wait on itself.
            let _permit = match settings.as_ref() {
                Some(s) => llm_util::acquire_chat(state, s).await,
                None => None,
            };
            client.chat(&messages).await?
        };
        let (verdicts, repaired) = utopia_extract::parse_adjudication_repairing(&reply)?;
        if repaired {
            // The model broke a line in the middle of a quoted reason: it is read as-is after patching, but the batch is logged (same as #894/#895)
            tracing::warn!(
                %kb_id,
                pairs = pairs.len(),
                first_pair = %batch_name(&pairs),
                "裁决回复的字符串里有裸控制字符，转义后才解开"
            );
        }
        let by_i: HashMap<usize, &utopia_extract::AdjudicationVerdict> =
            verdicts.iter().map(|v| (v.i, v)).collect();

        for (idx, (item, key, p, _)) in to_ask.iter().enumerate() {
            match by_i.get(&idx) {
                Some(v) => {
                    let same = match v.verdict.as_str() {
                        "same" => Some(true),
                        "different" => Some(false),
                        _ => None,
                    };
                    let conf = v.confidence.unwrap_or(0.5).clamp(0.0, 1.0);
                    // Tier two (0028): what the batch left undecided gets a second look with tools before it is applied. If the budget runs out or
                    // the loop did not run, the batch's verdict stands
                    if needs_second_look(item, p, same, conf) {
                        let earlier = Look::from_batch(same, conf, v.why.clone());
                        if let Some(look) = look_again(
                            state,
                            kb_id,
                            &client,
                            &settings,
                            item,
                            &pairs[idx],
                            &earlier,
                        )
                        .await
                        {
                            let outcome = if look.same.is_some() {
                                utopia_store::resolution::put_verdict(
                                    &state.pool,
                                    kb_id,
                                    key,
                                    look.same,
                                    look.conf,
                                    &model,
                                )
                                .await?;
                                apply_verdict(
                                    state,
                                    kb_id,
                                    item,
                                    look.same,
                                    look.conf,
                                    "investigated",
                                    look.why.as_deref(),
                                )
                                .await?
                            } else {
                                // It asked a question, or looked and reached no conclusion: left for a human, with the suggestion and question on the card
                                utopia_store::resolution::escalate_review(
                                    &state.pool,
                                    item.id,
                                    "proposed",
                                )
                                .await?;
                                Outcome::Escalated
                            };
                            record_look(state, kb_id, run_id, item, p, &look, &outcome).await?;
                            continue;
                        }
                        // The second look did not run: a same for a similar-name pair must not merge on the batch's verdict, and it is not cached
                        if !batch_verdict_may_apply(item, same) {
                            utopia_store::resolution::escalate_review(
                                &state.pool,
                                item.id,
                                SECOND_LOOK_UNAVAILABLE,
                            )
                            .await?;
                            continue;
                        }
                    }
                    utopia_store::resolution::put_verdict(
                        &state.pool,
                        kb_id,
                        key,
                        same,
                        conf,
                        &model,
                    )
                    .await?;
                    apply_verdict(
                        state,
                        kb_id,
                        item,
                        same,
                        conf,
                        "adjudicated",
                        v.why.as_deref(),
                    )
                    .await?;
                }
                None => {
                    utopia_store::resolution::escalate_review(
                        &state.pool,
                        item.id,
                        "escalate_no_verdict",
                    )
                    .await?;
                }
            }
        }
        // This round's verdicts are all persisted; push to the frontend to refresh the review queue
        state.emit_review(kb_id);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn apply_verdict(
    state: &AppState,
    kb_id: Uuid,
    item: &ReviewItem,
    same: Option<bool>,
    conf: f32,
    via: &str,
    why: Option<&str>,
) -> anyhow::Result<Outcome> {
    // Act when confident, no longer skim 10% off for a human (0026 revision): what is left waiting for a human in the queue is only what the machine is unsure about,
    // or what the gate says would leave the graph or overturn a source's identity claim
    let outcome = match same {
        Some(true) if conf >= AUTO_CONF => {
            // Execution gate (0027): merges that would immediately leave the graph (violations, derivations, answers) are left for a human,
            // and identity sources are also left for a human (0071), however confident. A human sees the reason it was left,
            // not "the adjudicator was not confident"
            if let Some(hold) = utopia_store::execution_gate::hold_for_merge(
                &state.pool,
                kb_id,
                item.left.id,
                item.right.id,
            )
            .await?
            {
                utopia_store::resolution::escalate_review(
                    &state.pool,
                    item.id,
                    &hold.review_reason(),
                )
                .await?;
                return Ok(Outcome::Escalated);
            }
            let (target, source) =
                utopia_store::resolution::merge_direction(&state.pool, item.left.id, item.right.id)
                    .await?;
            let reason = format!("auto_merged|{via} {conf:.2}");
            match utopia_store::resolution::merge_entities(
                &state.pool,
                kb_id,
                source,
                target,
                None,
                &reason,
            )
            .await
            {
                Ok(merge_id) => {
                    utopia_store::resolution::close_review_auto(
                        &state.pool,
                        item.id,
                        "merged",
                        &reason,
                    )
                    .await?;
                    // Decision ledger: AI automatic merge (empty actor = system)
                    let _ = utopia_store::audit::record_opt(
                        &state.pool,
                        Some(kb_id),
                        None,
                        "review.merge",
                        "review",
                        Some(item.id),
                        serde_json::json!({
                            "left": item.left.name, "right": item.right.name,
                            "score": item.score, "confidence": conf, "via": via,
                            // Keep the model's one-line reason too: machine rows are not precedents (0025 decision 1),
                            // but a human looking at Decisions should be able to see what it is based on
                            "why": why,
                        }),
                    )
                    .await;
                    Outcome::Merged(merge_id)
                }
                // A chained merge within the same batch may already have absorbed one side: hand over to a human instead of failing the task
                Err(AppError::Conflict(_)) | Err(AppError::NotFound) => {
                    utopia_store::resolution::escalate_review(
                        &state.pool,
                        item.id,
                        "escalate_entity_changed",
                    )
                    .await?;
                    Outcome::Escalated
                }
                Err(e) => return Err(e.into()),
            }
        }
        Some(false) if conf >= AUTO_CONF => {
            utopia_store::resolution::close_review_auto(
                &state.pool,
                item.id,
                "kept",
                &format!("kept_apart|{via} {conf:.2}"),
            )
            .await?;
            let _ = utopia_store::audit::record_opt(
                &state.pool,
                Some(kb_id),
                None,
                "review.keep",
                "review",
                Some(item.id),
                serde_json::json!({
                    "left": item.left.name, "right": item.right.name,
                    "score": item.score, "confidence": conf, "via": via,
                    "why": why,
                }),
            )
            .await;
            Outcome::Kept
        }
        _ => {
            utopia_store::resolution::escalate_review(
                &state.pool,
                item.id,
                &format!("escalate_unsure|{via} {conf:.2}"),
            )
            .await?;
            Outcome::Escalated
        }
    };
    Ok(outcome)
}

#[cfg(test)]
#[path = "adjudication_identity_tests.rs"]
mod identity_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use utopia_core::models::{ReviewItem, ReviewSide};

    #[tokio::test]
    async fn a_second_look_releases_the_batch_model_permit() -> anyhow::Result<()> {
        use serde_json::{json, Value};
        use std::time::Duration;
        use wiremock::{
            matchers::method, matchers::path, Mock, MockServer, Request, ResponseTemplate,
        };

        let Some(url) = utopia_store::test_db::url() else {
            return Ok(());
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        utopia_store::db::migrate(&pool).await?;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(|request: &Request| {
                let body: Value = request.body_json().unwrap();
                let message = if body["tools"].is_array() {
                    json!({"role":"assistant","content":null,"tool_calls":[{
                        "id":"defer-1","type":"function","function":{
                            "name":"defer","arguments":r#"{"question":"Which source identifies this person?"}"#
                        }
                    }]})
                } else {
                    json!({"role":"assistant","content":json!({"verdicts":[{
                        "i":0,"verdict":"unsure","confidence":0.5
                    }]}).to_string()})
                };
                ResponseTemplate::new(200).set_body_json(json!({
                    "choices":[{"message":message,"finish_reason":"stop"}]
                }))
            })
            .mount(&server)
            .await;
        let (org, workspace, kb, left, right) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        let endpoint = server.uri();
        let result = async {
            sqlx::raw_sql(&format!(
                "INSERT INTO organizations(id,name) VALUES ('{org}','permit-test');
                 INSERT INTO workspaces(id,org_id,name) VALUES ('{workspace}','{org}','permit-test');
                 INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{workspace}','permit-test');
                 INSERT INTO entities(id,kb_id,canonical_name) VALUES ('{left}','{kb}','Alex Lee'),('{right}','{kb}','Alex Lee');"
            )).execute(&pool).await?;
            utopia_store::resolution::create_review(
                &pool, kb, left, right, 0.7, "ambiguous_name|0.7",
                utopia_store::resolution::ReviewStage::Adjudicating,
            ).await?;
            utopia_store::settings::upsert(
                &pool, workspace, Some(&endpoint), None, Some("permit-test"),
                None, None, None, None,
            ).await?;
            utopia_store::model_limits::set(&pool, &endpoint, "permit-test", Some(1)).await?;
            let dir = tempfile::tempdir()?;
            let cfg = utopia_core::config::AppConfig {
                data_dir: dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            };
            let search = Arc::new(utopia_search::SearchIndex::open(&dir.path().join("search"))?);
            let state = AppState::new(pool.clone(), &cfg, search, "test-only".into());
            let run = tokio::time::timeout(
                Duration::from_secs(5), adjudicate_entities(&state, kb),
            ).await;
            let requests = server.received_requests().await.unwrap();
            anyhow::ensure!(run.is_ok(), "second look blocked after {} model requests", requests.len());
            run.unwrap()?;
            assert_eq!(requests.len(), 2);
            assert!(!requests[0].body_json::<Value>()?["tools"].is_array());
            assert!(requests[1].body_json::<Value>()?["tools"].is_array());
            let decision: (String, String, String, i32) = sqlx::query_as(
                "SELECT action, status, question, calls FROM agent_decisions WHERE kb_id=$1",
            ).bind(kb).fetch_one(&pool).await?;
            assert_eq!(decision, (
                "unsure".into(), "proposed".into(),
                "Which source identifies this person?".into(), 1,
            ));
            Ok(())
        }.await;
        utopia_store::model_limits::set(&pool, &endpoint, "permit-test", None).await?;
        sqlx::query("DELETE FROM organizations WHERE id=$1")
            .bind(org)
            .execute(&pool)
            .await?;
        pool.close().await;
        result
    }

    fn item(reason: &str) -> ReviewItem {
        let side = |name: &str| ReviewSide {
            id: Uuid::now_v7(),
            name: name.into(),
            type_label: Some("person".into()),
            color: String::new(),
            disambiguator: None,
            degree: 0,
            top_facts: vec![],
        };
        ReviewItem {
            id: Uuid::now_v7(),
            score: 0.78,
            reason: Some(reason.into()),
            stage: "adjudicating".into(),
            created_at: chrono::Utc::now(),
            left: side("张伟"),
            right: side("财务部总监张伟"),
            proposal: None,
        }
    }

    /// Pairs proposed by name vectors: batch saying same still needs a second look, however confident; different / unsure stay as they are
    #[test]
    fn a_similarity_proposed_same_always_gets_the_second_look() {
        let p = gov::Precedents::default();
        let it = item("name_vector|0.78");
        assert!(needs_second_look(&it, &p, Some(true), 0.99));
        assert!(needs_second_look(&it, &p, Some(true), 0.85));
        assert!(
            !needs_second_look(&it, &p, Some(false), 0.9),
            "分开是安全方向，照旧落地"
        );
        assert!(needs_second_look(&it, &p, None, 0.5), "没定的本来就要再看");
    }

    /// When the second look did not run: a same from a similar-name proposal is not applied nor cached; the rest follow the batch's verdict
    #[test]
    fn a_similarity_proposed_same_never_applies_on_the_batch_verdict_alone() {
        assert!(!batch_verdict_may_apply(
            &item("name_vector|0.78"),
            Some(true)
        ));
        assert!(batch_verdict_may_apply(
            &item("name_vector|0.78"),
            Some(false)
        ));
        assert!(batch_verdict_may_apply(&item("name_vector|0.78"), None));
        assert!(batch_verdict_may_apply(
            &item("ambiguous_name|0.41"),
            Some(true)
        ));
        assert!(batch_verdict_may_apply(
            &item("shared_name|张伟"),
            Some(true)
        ));
    }

    /// Pairs from same-name families are unaffected: if they clear the bar they are still auto-applied as before
    #[test]
    fn a_same_name_pair_keeps_the_old_rule() {
        let p = gov::Precedents::default();
        let it = item("ambiguous_name|0.41");
        assert!(!needs_second_look(&it, &p, Some(true), 0.9));
        assert!(needs_second_look(&it, &p, Some(true), 0.6), "不到线才再看");
    }
}
