//! Scenario packs (0073): a small set of vocabulary preset for a business scenario, together with the questions it must answer.
//!
//! The standard ontology packs (`ontology_packs`, 0008) give general types, not the predicates the corpus needs; scenario packs are the reverse:
//! a few classes and ten or so properties and relations, whose definitions are written for the aligner to read, with units and datatypes already chosen, plus the
//! capability questions this kind of corpus must answer (0061). The file is embedded in the binary, so offline deployments have it too.
//!
//! **Goes through the editor's path, not OWL import.** Classes and properties are created with the same calls the ontology page makes; they have no IRI, and are this KB's
//! own elements (0012): editable, deletable, and when exported they are the KB's own vocabulary. Questions are recorded as raised by a person (`origin = person`, where
//! the person who created the KB is the author): ticking a scenario pack means this person's requirements for the KB. Installed only at KB creation: a new KB has no facts, so there is nothing to reconcile.
//!
//! A key that collides with an existing element (a standard pack installed earlier, or another scenario pack) is skipped and reported, not merged or overwritten; domain and range
//! only accept the pack's own class keys or keys already in the KB; a declaration that cannot be resolved has that one declaration dropped and the rest installed.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Deserialize;
use utopia_core::models::RelationAxioms;
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

use crate::state::AppState;

/// A bilingual text: the label is data, taken by the KB's `ontology_lang` (0004)
#[derive(Debug, Clone, Deserialize)]
pub struct Text {
    pub zh: String,
    pub en: String,
}

impl Text {
    pub fn get(&self, lang: &str) -> &str {
        if lang == "zh" {
            &self.zh
        } else {
            &self.en
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Class {
    pub key: String,
    pub label: Text,
    pub description: Text,
    /// The key of the parent class; it must come earlier in this pack or already be in the KB
    #[serde(default)]
    pub parents: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Attribute {
    pub key: String,
    /// Which class it attaches to (the domain)
    pub host: String,
    pub label: Text,
    pub description: Text,
    /// text / number / date / bool
    pub datatype: String,
    /// The unit of a number, in the form this corpus uses; a value that cannot be read is recorded as `attr_datatype` and dropped
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default = "yes")]
    pub functional: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Relation {
    pub key: String,
    pub label: Text,
    pub description: Text,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub ranges: Vec<String>,
    /// state / event / eternal (0031)
    #[serde(default = "state")]
    pub temporal: String,
    #[serde(default)]
    pub functional: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Question {
    pub question: Text,
    #[serde(default)]
    pub expected_answer: Option<Text>,
    /// The classes and properties an answer must pass through (the "shape" of 0061 decision 1), stored as they are
    #[serde(default)]
    pub needs: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioPack {
    pub id: String,
    pub name: Text,
    pub summary: Text,
    #[serde(default)]
    pub classes: Vec<Class>,
    #[serde(default)]
    pub attributes: Vec<Attribute>,
    #[serde(default)]
    pub relations: Vec<Relation>,
    #[serde(default)]
    pub questions: Vec<Question>,
}

fn yes() -> bool {
    true
}
fn state() -> String {
    "state".into()
}

const FILES: &[(&str, &str)] = &[
    (
        "enterprise-profile",
        include_str!("../packs/scenarios/enterprise-profile.json"),
    ),
    (
        "contract-chain",
        include_str!("../packs/scenarios/contract-chain.json"),
    ),
    (
        "personnel-records",
        include_str!("../packs/scenarios/personnel-records.json"),
    ),
];

/// All the scenario packs embedded. A bad file is a build problem, not a runtime one: this panics directly, and the test
/// [`tests::every_shipped_pack_parses_and_resolves`] hits it in CI before any deployment
pub static PACKS: LazyLock<Vec<ScenarioPack>> = LazyLock::new(|| {
    FILES
        .iter()
        .map(|(id, text)| {
            let pack: ScenarioPack = serde_json::from_str(text)
                .unwrap_or_else(|e| panic!("scenario pack {id} does not parse: {e}"));
            assert_eq!(&pack.id, id, "scenario pack file {id} carries another id");
            pack
        })
        .collect()
});

pub fn get(id: &str) -> Option<&'static ScenarioPack> {
    PACKS.iter().find(|p| p.id == id)
}

/// What was installed and what was skipped. `skipped` is a sentence for people to read: which key collided with whom, and which declaration could not be resolved
#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub classes: usize,
    pub properties: usize,
    pub questions: usize,
    pub skipped: Vec<String>,
}

/// Install several scenario packs into a new KB. `lang` is the KB's `ontology_lang`: labels and definitions are taken by it.
/// Once something is created, queue one `embed_ontology` pass, since elements need vectors before the first alignment (same as the editor and import)
pub async fn install(
    state: &AppState,
    kb_id: Uuid,
    actor: Uuid,
    lang: &str,
    ids: &[String],
) -> AppResult<Report> {
    let mut report = Report::default();
    if ids.is_empty() {
        return Ok(report);
    }
    let packs: Vec<&ScenarioPack> = ids
        .iter()
        .map(|id| {
            get(id).ok_or_else(|| AppError::invalid("unknown_pack", format!("未知的场景包：{id}")))
        })
        .collect::<AppResult<_>>()?;
    let pool = &state.pool;
    // KB keys that already exist (a standard pack installed earlier, the previous scenario pack): a colliding one is skipped, and declarations may point at them
    let mut classes: HashMap<String, Uuid> =
        sqlx::query_as::<_, (String, Uuid)>("SELECT key, id FROM entity_types WHERE kb_id = $1")
            .bind(kb_id)
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect();
    let mut properties: HashMap<String, Uuid> =
        sqlx::query_as::<_, (String, Uuid)>("SELECT key, id FROM relation_types WHERE kb_id = $1")
            .bind(kb_id)
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect();
    for pack in &packs {
        for c in &pack.classes {
            if classes.contains_key(&c.key) {
                report
                    .skipped
                    .push(format!("{}: class {} already exists", pack.id, c.key));
                continue;
            }
            let mut parents = Vec::new();
            for p in &c.parents {
                match classes.get(p) {
                    Some(id) => parents.push(*id),
                    None => report
                        .skipped
                        .push(format!("{}: parent {p} of {} not found", pack.id, c.key)),
                }
            }
            let id = utopia_store::ontology::create_entity_type(
                pool,
                kb_id,
                &c.key,
                c.label.get(lang),
                "#64748b",
                "circle",
                &parents,
                c.description.get(lang),
            )
            .await?;
            classes.insert(c.key.clone(), id);
            report.classes += 1;
        }
        for a in &pack.attributes {
            if properties.contains_key(&a.key) {
                report
                    .skipped
                    .push(format!("{}: property {} already exists", pack.id, a.key));
                continue;
            }
            let Some(host) = classes.get(&a.host) else {
                report.skipped.push(format!(
                    "{}: host {} of {} not found",
                    pack.id, a.host, a.key
                ));
                continue;
            };
            let id = utopia_store::ontology::create_relation_type(
                pool,
                kb_id,
                &a.key,
                a.label.get(lang),
                "state",
                RelationAxioms {
                    functional: a.functional,
                    ..RelationAxioms::default()
                },
                a.description.get(lang),
                "attribute",
                &[*host],
                &[],
                Some(a.datatype.as_str()),
                a.unit.as_deref(),
            )
            .await?;
            properties.insert(a.key.clone(), id);
            report.properties += 1;
        }
        for r in &pack.relations {
            if properties.contains_key(&r.key) {
                report
                    .skipped
                    .push(format!("{}: property {} already exists", pack.id, r.key));
                continue;
            }
            let mut resolve = |keys: &[String], side: &str| -> Vec<Uuid> {
                keys.iter()
                    .filter_map(|k| {
                        let id = classes.get(k).copied();
                        if id.is_none() {
                            report
                                .skipped
                                .push(format!("{}: {side} {k} of {} not found", pack.id, r.key));
                        }
                        id
                    })
                    .collect()
            };
            let domains = resolve(&r.domains, "domain");
            let ranges = resolve(&r.ranges, "range");
            let id = utopia_store::ontology::create_relation_type(
                pool,
                kb_id,
                &r.key,
                r.label.get(lang),
                &r.temporal,
                RelationAxioms {
                    functional: r.functional,
                    ..RelationAxioms::default()
                },
                r.description.get(lang),
                "relation",
                &domains,
                &ranges,
                None,
                None,
            )
            .await?;
            properties.insert(r.key.clone(), id);
            report.properties += 1;
        }
        for q in &pack.questions {
            let text = q.question.get(lang);
            if utopia_store::competency_questions::exists_text(pool, kb_id, text).await? {
                report
                    .skipped
                    .push(format!("{}: question already exists: {text}", pack.id));
                continue;
            }
            utopia_store::competency_questions::create(
                pool,
                kb_id,
                utopia_store::competency_questions::NewQuestion {
                    question: text,
                    expected_answer: q.expected_answer.as_ref().map(|t| t.get(lang)),
                    needs: q.needs.as_ref(),
                    origin: "person",
                    status: "accepted",
                    created_by: Some(actor),
                },
            )
            .await?;
            report.questions += 1;
        }
        let _ = utopia_store::audit::record(
            pool,
            Some(kb_id),
            actor,
            "scenario_pack.installed",
            "knowledge_base",
            Some(kb_id),
            serde_json::json!({ "pack": pack.id, "lang": lang }),
        )
        .await;
    }
    if report.classes + report.properties > 0 {
        // New elements need vectors, or the alignment shortlist cannot see them (same as `reindex_then_align`)
        utopia_store::jobs::enqueue_unless_queued_after(
            pool,
            "embed_ontology",
            serde_json::json!({ "kb_id": kb_id }),
            std::time::Duration::from_secs(2),
        )
        .await?;
    }
    if !report.skipped.is_empty() {
        tracing::info!(%kb_id, skipped = ?report.skipped, "scenario pack has skipped elements");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::{Text, PACKS};
    use std::collections::HashSet;

    /// The file is hand-written JSON: it must parse, keys must not repeat, and every declaration must point at a class earlier in this pack, with a valid datatype
    #[test]
    fn every_shipped_pack_parses_and_resolves() {
        assert_eq!(PACKS.len(), 3);
        for pack in PACKS.iter() {
            let mut classes: HashSet<&str> = HashSet::new();
            for c in &pack.classes {
                for p in &c.parents {
                    assert!(
                        classes.contains(p.as_str()),
                        "{}: parent {p} of {} comes later",
                        pack.id,
                        c.key
                    );
                }
                assert!(classes.insert(&c.key), "{}: class {} twice", pack.id, c.key);
            }
            let mut properties: HashSet<&str> = HashSet::new();
            for a in &pack.attributes {
                assert!(
                    classes.contains(a.host.as_str()),
                    "{}: host {} of {}",
                    pack.id,
                    a.host,
                    a.key
                );
                assert!(
                    matches!(a.datatype.as_str(), "text" | "number" | "date" | "bool"),
                    "{}: {}",
                    pack.id,
                    a.key
                );
                assert!(
                    properties.insert(&a.key),
                    "{}: property {} twice",
                    pack.id,
                    a.key
                );
            }
            for r in &pack.relations {
                for k in r.domains.iter().chain(r.ranges.iter()) {
                    assert!(
                        classes.contains(k.as_str()),
                        "{}: {k} of {}",
                        pack.id,
                        r.key
                    );
                }
                assert!(
                    matches!(r.temporal.as_str(), "state" | "event" | "eternal"),
                    "{}: {}",
                    pack.id,
                    r.key
                );
                assert!(
                    properties.insert(&r.key),
                    "{}: property {} twice",
                    pack.id,
                    r.key
                );
            }
            assert!(
                !pack.questions.is_empty(),
                "{}: a pack ships with its questions",
                pack.id
            );
            for t in pack
                .classes
                .iter()
                .map(|c| &c.description)
                .chain(pack.attributes.iter().map(|a| &a.description))
                .chain(pack.relations.iter().map(|r| &r.description))
            {
                assert!(
                    !t.zh.trim().is_empty() && !t.en.trim().is_empty(),
                    "{}: a definition in both languages",
                    pack.id
                );
            }
        }
    }

    #[test]
    fn a_label_follows_the_base_language() {
        let t = Text {
            zh: "企业".into(),
            en: "Company".into(),
        };
        assert_eq!(t.get("zh"), "企业");
        assert_eq!(t.get("en"), "Company");
        assert_eq!(t.get("fr"), "Company");
    }
}
