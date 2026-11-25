//! Errata (0044 decision 7, the sixth cut): what the agent that re-reviews the typed graph per document after extraction leaves behind in the database.
//!
//! Structural report first ([`candidates`]): subject or object names that sit outside the class the property is declared on and cannot be found in the document, and date properties
//! with no date. These can be reported without asking the model, so look at them first and sample the rest -- the order from 0044 §7.
//!
//! Each fact that gets reviewed is logged once (keep is logged too): "never reviewed" is simply having no row, so the review does not repeat; retract, revise and add
//! are logged as actions, carrying the document's own words. Actions go through the 0027 gate: a retracted fact that something derives from, a subject the answer relies on,
//! or a written fact that would give a single-valued predicate two values -- these are left for a human; the agent does not act.
//!
//! A retraction has to hold up: materialization would otherwise re-derive the live statement as the same typed row. So actions record (statement, property),
//! and materialization and entailment both skip the pair that an erratum retracted (see `materialize`). Other documents that say the same thing still count --
//! errata look at this document, and retract the one row this document produced.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

use crate::execution_gate::{self, Impact};
use crate::graph::{FactObject, Validity};

/// The queued task name: one per library, reviewing documents that have typed rows not yet reviewed
pub const JOB_KIND: &str = "errata_review";

/// The four reasons the structure check can report, the same list as the CHECK in the migration
pub const FLAGS: &[&str] = &["domain", "range", "name_absent", "no_date"];

/// One typed fact sent for review: the names and classes at both ends, the property, the source statement, the reason the structure reported, and the quote from this document
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Candidate {
    pub fact_id: Uuid,
    pub statement_id: Option<Uuid>,
    pub predicate_id: Uuid,
    pub subject_id: Uuid,
    pub subject: String,
    pub subject_class: Option<String>,
    pub property: String,
    pub property_label: String,
    pub object_id: Option<Uuid>,
    pub object: String,
    pub object_class: Option<String>,
    /// Empty = structure reported nothing, so sample it
    pub flag: Option<String>,
    pub quote: Option<String>,
}

/// The class ancestor closure is computed in SQL: the property is declared on legal_entity, and organization, a subclass of it, is in scope.
/// A side with no class is not reported -- "not known what it is" is not "outside the class"
const CANDIDATES_SQL: &str = r#"
WITH RECURSIVE up AS (
    SELECT id AS child, id AS anc FROM entity_types WHERE kb_id = $1
    UNION
    SELECT up.child, p.parent_id FROM up JOIN entity_type_parents p ON p.child_id = up.anc
),
closure AS (SELECT child, array_agg(anc) AS ancs FROM up GROUP BY child),
doc AS (
    SELECT lower(string_agg(text, ' ' ORDER BY seq)) AS text
      FROM chunks WHERE document_id = $2 AND superseded_at IS NULL
),
sources AS (
    SELECT src.fact_id, src.statement_id FROM typed_fact_sources src
      JOIN fact_evidence fe ON fe.fact_id = src.statement_id AND fe.document_id = $2
    UNION
    SELECT i.fact_id, i.statement_id FROM implied_fact_sources i
      JOIN fact_evidence fe ON fe.fact_id = i.statement_id AND fe.document_id = $2
     WHERE i.statement_id IS NOT NULL
),
typed AS (SELECT DISTINCT ON (fact_id) fact_id, statement_id FROM sources ORDER BY fact_id, statement_id)
SELECT * FROM (
SELECT t.id AS fact_id, ty.statement_id, t.predicate_id, t.subject_id, t.recorded_at,
       s.canonical_name AS subject, st.key AS subject_class,
       r.key AS property, r.label AS property_label,
       t.object_id,
       COALESCE(o.canonical_name, t.object_value #>> '{value}', t.object_value::text, '') AS object,
       ot.key AS object_class,
       CASE
         WHEN s.type_id IS NOT NULL
          AND EXISTS (SELECT 1 FROM relation_type_domains d WHERE d.relation_type_id = r.id)
          AND NOT EXISTS (SELECT 1 FROM relation_type_domains d
                           WHERE d.relation_type_id = r.id AND d.entity_type_id = ANY(sc.ancs))
           THEN 'domain'
         WHEN o.type_id IS NOT NULL
          AND EXISTS (SELECT 1 FROM relation_type_ranges g WHERE g.relation_type_id = r.id)
          AND NOT EXISTS (SELECT 1 FROM relation_type_ranges g
                           WHERE g.relation_type_id = r.id AND g.entity_type_id = ANY(oc.ancs))
           THEN 'range'
         WHEN r.datatype = 'date' AND t.object_id IS NULL
          AND NOT COALESCE((t.object_value #>> '{value}') ~ '^\d{4}(-\d{2}(-\d{2})?)?', false)
           THEN 'no_date'
         WHEN position(lower(s.canonical_name) IN doc.text) = 0
           OR (o.id IS NOT NULL AND position(lower(o.canonical_name) IN doc.text) = 0)
           THEN 'name_absent'
       END AS flag,
       (SELECT fe.quote FROM fact_evidence fe
         WHERE fe.fact_id = t.id AND fe.document_id = $2 AND fe.quote IS NOT NULL
         ORDER BY fe.chunk_id LIMIT 1) AS quote
  FROM typed ty
  JOIN facts t ON t.id = ty.fact_id
  JOIN entities s ON s.id = t.subject_id
  LEFT JOIN entity_types st ON st.id = s.type_id
  LEFT JOIN closure sc ON sc.child = s.type_id
  JOIN relation_types r ON r.id = t.predicate_id
  LEFT JOIN entities o ON o.id = t.object_id
  LEFT JOIN entity_types ot ON ot.id = o.type_id
  LEFT JOIN closure oc ON oc.child = o.type_id
  CROSS JOIN doc
 WHERE t.kb_id = $1 AND t.layer = 'typed' AND t.invalidated_at IS NULL
   AND NOT EXISTS (SELECT 1 FROM errata_actions ea WHERE ea.fact_id = t.id)
) c
ORDER BY (c.flag IS NULL), c.recorded_at, c.fact_id
"#;

/// The live, not-yet-reviewed typed facts this document produced, with the structurally reported ones first
pub async fn candidates(
    pool: &PgPool,
    kb_id: Uuid,
    document_id: Uuid,
) -> AppResult<Vec<Candidate>> {
    Ok(sqlx::query_as(CANDIDATES_SQL)
        .bind(kb_id)
        .bind(document_id)
        .fetch_all(pool)
        .await?)
}

/// The body of the document's current version, joined in chunk order
pub async fn document_text(pool: &PgPool, document_id: Uuid) -> AppResult<String> {
    let text: Option<String> = sqlx::query_scalar(
        "SELECT string_agg(text, ' ' ORDER BY seq) FROM chunks
          WHERE document_id = $1 AND superseded_at IS NULL",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await?;
    Ok(text.unwrap_or_default())
}

/// Documents to look at: those that still have live typed rows not yet reviewed, and those that were extracted but never reviewed at all (it may have
/// no typed rows -- alignment did not bind -- in which case the agent can only add, and adding is the half that proved most valuable in the first real run).
/// Earliest ingested first.
///
/// Pushed documents (`statements` source) are not reviewed (RC-001): the errata check rows against the document's own words, and pushed statements
/// have no own words (0054's `quote` must be empty), so there is nothing to match against
pub async fn documents_due(pool: &PgPool, kb_id: Uuid, limit: i64) -> AppResult<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT d.id FROM documents d
          LEFT JOIN sources s ON s.id = d.source_id
          WHERE d.kb_id = $1 AND d.deleted_at IS NULL
            AND s.kind IS DISTINCT FROM 'statements'
            AND (EXISTS (
              SELECT 1 FROM fact_evidence fe
                JOIN (SELECT fact_id, statement_id FROM typed_fact_sources
                      UNION ALL
                      SELECT fact_id, statement_id FROM implied_fact_sources
                       WHERE statement_id IS NOT NULL) src ON src.statement_id = fe.fact_id
                JOIN facts t ON t.id = src.fact_id
               WHERE fe.document_id = d.id AND t.invalidated_at IS NULL
                 AND NOT EXISTS (SELECT 1 FROM errata_actions ea WHERE ea.fact_id = t.id))
              OR (d.graph_status = 'done'
                  AND NOT EXISTS (SELECT 1 FROM errata_runs r WHERE r.document_id = d.id)))
          ORDER BY d.created_at, d.id
          LIMIT $2",
    )
    .bind(kb_id)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// How many times this document has been reviewed
pub async fn runs_of(pool: &PgPool, document_id: Uuid) -> AppResult<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM errata_runs WHERE document_id = $1")
            .bind(document_id)
            .fetch_one(pool)
            .await?,
    )
}

/// Opens the books for one review
pub async fn start_run(
    pool: &PgPool,
    kb_id: Uuid,
    document_id: Uuid,
    flagged: i32,
    sampled: i32,
) -> AppResult<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO errata_runs (id, kb_id, document_id, flagged, sampled) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(kb_id)
    .bind(document_id)
    .bind(flagged)
    .bind(sampled)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Closing the books: how many calls were made and how much usage the endpoint reported (left empty if not reported)
pub async fn finish_run(
    pool: &PgPool,
    run_id: Uuid,
    requests: i32,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE errata_runs SET requests = $2, prompt_tokens = $3, completion_tokens = $4,
                finished_at = now() WHERE id = $1",
    )
    .bind(run_id)
    .bind(requests)
    .bind(prompt_tokens)
    .bind(completion_tokens)
    .execute(pool)
    .await?;
    Ok(())
}

/// The agent's claim about a fact, or a fact it wants to add
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proposed {
    Keep,
    Retract,
    /// Change the property (key) or the object (name or value), at least one of them
    Revise {
        property: Option<String>,
        object: Option<String>,
    },
    Add {
        subject: String,
        property: String,
        object: String,
    },
}

impl Proposed {
    pub fn action(&self) -> &'static str {
        match self {
            Proposed::Keep => "keep",
            Proposed::Retract => "retract",
            Proposed::Revise { .. } => "revise",
            Proposed::Add { .. } => "add",
        }
    }
}

pub struct ActionInput<'a> {
    pub run_id: Uuid,
    pub document_id: Uuid,
    /// The fact that keep / retract / revise look at; add has none
    pub candidate: Option<&'a Candidate>,
    pub proposed: Proposed,
    pub reason: &'a str,
    pub quote: Option<&'a str>,
    /// The document body; quotes are checked against it
    pub document_text: &'a str,
}

/// A recorded action: applied, left for a human, or failed verification
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recorded {
    pub id: Uuid,
    pub status: &'static str,
    /// The reason it was left for a human, or the reason it failed verification
    pub detail: Option<String>,
    pub new_fact_id: Option<Uuid>,
}

/// Is the quote the document's own words: a contiguous run after folding case and whitespace. An empty quote does not count
pub fn quote_in(document: &str, quote: &str) -> bool {
    let q = squash(quote);
    !q.is_empty() && squash(document).contains(&q)
}

fn squash(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The fact an action points at: the name for the queue card, the id for execution
struct Target {
    subject_id: Uuid,
    predicate_id: Uuid,
    object_id: Option<Uuid>,
    object_value: Option<Value>,
    subject: String,
    property: String,
    object: String,
}

impl Target {
    fn json(&self) -> Value {
        json!({
            "subject": self.subject, "property": self.property, "object": self.object,
            "subject_id": self.subject_id, "predicate_id": self.predicate_id,
            "object_id": self.object_id, "object_value": self.object_value,
        })
    }
    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            subject_id: v.get("subject_id")?.as_str()?.parse().ok()?,
            predicate_id: v.get("predicate_id")?.as_str()?.parse().ok()?,
            object_id: v
                .get("object_id")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse().ok()),
            object_value: v.get("object_value").filter(|x| !x.is_null()).cloned(),
            subject: v["subject"].as_str().unwrap_or("").to_string(),
            property: v["property"].as_str().unwrap_or("").to_string(),
            object: v["object"].as_str().unwrap_or("").to_string(),
        })
    }
}

/// The row in this library for a property key: its id, and whether it is a relation or an attribute
async fn property_by_key(
    pool: &PgPool,
    kb_id: Uuid,
    key: &str,
) -> AppResult<Option<(Uuid, String)>> {
    Ok(
        sqlx::query_as("SELECT id, kind FROM relation_types WHERE kb_id = $1 AND key = $2")
            .bind(kb_id)
            .bind(key.trim())
            .fetch_optional(pool)
            .await?,
    )
}

/// Resolve a name to something in the library: find an existing one by name; if none exists and the name is in the document's own words, create one per 0041
/// an entity carrying the name as a fact -- the document introduced something new, and an erratum adding a fact should not give up just because extraction did not treat it as an entity. Names not in the document
/// are not created: those were made up by the agent
async fn thing_named(
    pool: &PgPool,
    kb_id: Uuid,
    name: &str,
    document_text: &str,
) -> AppResult<Result<Uuid, String>> {
    if let Some(id) = crate::resolution::existing_by_name(pool, kb_id, name).await? {
        return Ok(Ok(id));
    }
    if quote_in(document_text, name) {
        return Ok(Ok(crate::implication_rules::resolve_or_create_named(
            pool, kb_id, name,
        )
        .await?));
    }
    Ok(Err(format!(
        "no thing named \"{name}\" and the document does not name it"
    )))
}

/// Resolve the object's text according to the property kind: a relation needs an actual thing (see `thing_named`), an attribute is stored as a value
async fn object_of(
    pool: &PgPool,
    kb_id: Uuid,
    kind: &str,
    text: &str,
    document_text: &str,
) -> AppResult<Result<(Option<Uuid>, Option<Value>), String>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Err("empty object".into()));
    }
    if kind == "relation" {
        Ok(thing_named(pool, kb_id, text, document_text)
            .await?
            .map(|id| (Some(id), None)))
    } else {
        Ok(Ok((None, Some(json!({ "value": text })))))
    }
}

/// Log an entry and, if possible, apply it. keep is only logged; retract, revise and add check the quote first, then pass the gate, and only then touch the graph.
/// Entries that fail verification are still logged (refused): the fact counts as reviewed, the agent's claim is kept for people to see, but it is not asked again
pub async fn record(pool: &PgPool, kb_id: Uuid, input: ActionInput<'_>) -> AppResult<Recorded> {
    let id = Uuid::now_v7();
    let action = input.proposed.action();
    let c = input.candidate;
    if !matches!(input.proposed, Proposed::Add { .. }) && c.is_none() {
        return Err(AppError::Validation(format!(
            "{action} needs the fact it is about"
        )));
    }
    let (fact_id, statement_id, flag) = match c {
        Some(c) => (Some(c.fact_id), c.statement_id, c.flag.clone()),
        None => (None, None, None),
    };
    // The fact the action points at, and the verification result
    let resolved: Result<Option<Target>, String> = match &input.proposed {
        Proposed::Keep => Ok(None),
        Proposed::Retract => {
            let c = c.unwrap();
            Ok(Some(Target {
                subject_id: c.subject_id,
                predicate_id: c.predicate_id,
                object_id: c.object_id,
                object_value: None,
                subject: c.subject.clone(),
                property: c.property.clone(),
                object: c.object.clone(),
            }))
        }
        Proposed::Revise { property, object } => {
            let c = c.unwrap();
            let prop = match property.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                None => Ok((c.predicate_id, None)),
                Some(key) => match property_by_key(pool, kb_id, key).await? {
                    Some((id, kind)) => Ok((id, Some(kind))),
                    None => Err(format!("no property with key \"{key}\"")),
                },
            };
            match prop {
                Err(e) => Err(e),
                Ok((predicate_id, kind)) => {
                    let kind = match kind {
                        Some(k) => k,
                        None => {
                            sqlx::query_scalar("SELECT kind FROM relation_types WHERE id = $1")
                                .bind(predicate_id)
                                .fetch_one(pool)
                                .await?
                        }
                    };
                    let old: (Option<Uuid>, Option<Value>) =
                        sqlx::query_as("SELECT object_id, object_value FROM facts WHERE id = $1")
                            .bind(c.fact_id)
                            .fetch_one(pool)
                            .await?;
                    let obj = match object.as_deref() {
                        Some(text) => {
                            object_of(pool, kb_id, &kind, text, input.document_text).await?
                        }
                        None => Ok(old.clone()),
                    };
                    let no_moment = empty_state_revision(pool, c.fact_id, predicate_id).await?;
                    match obj {
                        Err(e) => Err(e),
                        Ok(_) if no_moment => {
                            Err("a state cannot take a time whose start and end are equal".into())
                        }
                        Ok((object_id, object_value))
                            if predicate_id == c.predicate_id
                                && object_id == old.0
                                && object_value == old.1 =>
                        {
                            Err("the revision changes nothing".into())
                        }
                        Ok((object_id, object_value)) => {
                            let property = match property {
                                Some(k) => k.trim().to_string(),
                                None => c.property.clone(),
                            };
                            Ok(Some(Target {
                                subject_id: c.subject_id,
                                predicate_id,
                                object_id,
                                object_value,
                                subject: c.subject.clone(),
                                property,
                                object: object.clone().unwrap_or_else(|| c.object.clone()),
                            }))
                        }
                    }
                }
            }
        }
        Proposed::Add {
            subject,
            property,
            object,
        } => {
            let subject_id = thing_named(pool, kb_id, subject.trim(), input.document_text).await?;
            match (subject_id, property_by_key(pool, kb_id, property).await?) {
                (Err(e), _) => Err(e),
                (_, None) => Err(format!("no property with key \"{}\"", property.trim())),
                (Ok(subject_id), Some((predicate_id, kind))) => {
                    match object_of(pool, kb_id, &kind, object, input.document_text).await? {
                        Err(e) => Err(e),
                        Ok((object_id, object_value)) => Ok(Some(Target {
                            subject_id,
                            predicate_id,
                            object_id,
                            object_value,
                            subject: subject.trim().to_string(),
                            property: property.trim().to_string(),
                            object: object.trim().to_string(),
                        })),
                    }
                }
            }
        }
    };
    // retract, revise and add all need the quote to be the document's own words; a claim that is not an exact quote is recorded and not executed
    let resolved = match resolved {
        Ok(t) => {
            if input.proposed != Proposed::Keep
                && !input
                    .quote
                    .is_some_and(|q| quote_in(input.document_text, q))
            {
                Err("the quote is not in the document".to_string())
            } else {
                Ok(t)
            }
        }
        e => e,
    };
    let target_json = match &resolved {
        Ok(Some(t)) => Some(t.json()),
        _ => None,
    };
    // 0027 gate: what depends on the fact being retracted, and whether the fact being written would immediately open a violation
    let hold = match &resolved {
        Ok(Some(t)) => {
            let mut impact = Impact::default();
            if let Some(fact) = fact_id.filter(|_| input.proposed != Proposed::Keep) {
                impact = execution_gate::impact_of_fact(pool, kb_id, fact).await?;
            }
            if !matches!(input.proposed, Proposed::Retract) {
                let replacing =
                    fact_id.filter(|_| matches!(input.proposed, Proposed::Revise { .. }));
                let w = execution_gate::impact_of_write(
                    pool,
                    kb_id,
                    t.subject_id,
                    t.predicate_id,
                    t.object_id,
                    replacing,
                )
                .await?;
                impact.contradictions.extend(w.contradictions);
            }
            execution_gate::hold(&impact)
        }
        _ => None,
    };
    // The agent alone wants to retract or change a fact that the structure check did not report: leave it for a human. In the first real run, nine in ten retracted rows were in fact stated in the source text
    // (bench README), while the few the structure check reported are the ones it should act on; what sampling turned up, act on only after a human confirms
    let unflagged = c.is_some_and(|c| c.flag.is_none())
        && matches!(input.proposed, Proposed::Retract | Proposed::Revise { .. });
    let (status, detail): (&'static str, Option<String>) = match (&resolved, &hold) {
        (Err(e), _) => ("refused", Some(e.clone())),
        (Ok(_), Some(h)) => ("held", Some(h.to_string())),
        (Ok(_), None) if unflagged => ("held", Some("unflagged".into())),
        (Ok(_), None) => ("applied", None),
    };
    sqlx::query(
        "INSERT INTO errata_actions
             (id, kb_id, run_id, document_id, fact_id, statement_id, predicate_id, flag,
              action, reason, quote, proposed, status, detail)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(id)
    .bind(kb_id)
    .bind(input.run_id)
    .bind(input.document_id)
    .bind(fact_id)
    .bind(statement_id)
    .bind(c.map(|c| c.predicate_id))
    .bind(&flag)
    .bind(action)
    .bind(input.reason)
    .bind(input.quote)
    .bind(&target_json)
    .bind(status)
    .bind(&detail)
    .execute(pool)
    .await?;
    let mut new_fact_id = None;
    if status == "applied" {
        if let Ok(Some(t)) = &resolved {
            new_fact_id = apply(
                pool,
                kb_id,
                action,
                fact_id,
                input.document_id,
                t,
                input.quote,
            )
            .await?;
            if new_fact_id.is_some() {
                sqlx::query("UPDATE errata_actions SET new_fact_id = $2 WHERE id = $1")
                    .bind(id)
                    .bind(new_fact_id)
                    .execute(pool)
                    .await?;
            }
        }
    }
    Ok(Recorded {
        id,
        status,
        detail,
        new_fact_id,
    })
}

/// Applying to the graph: retract is a void (`reject_fact`, evidence untouched, ledger kept); revise is retract-old-then-write-new, the new row inheriting the old row's time;
/// add writes a row anchored on the document's date. The new row's evidence is the chunk containing the quote.
/// A row that is already gone (retracted elsewhere first) is not an error -- the desired result is already in place
async fn apply(
    pool: &PgPool,
    kb_id: Uuid,
    action: &str,
    fact_id: Option<Uuid>,
    document_id: Uuid,
    target: &Target,
    quote: Option<&str>,
) -> AppResult<Option<Uuid>> {
    let mut validity_row: Option<ValidityRow> = None;
    if matches!(action, "retract" | "revise") {
        let fact = fact_id.ok_or_else(|| AppError::Validation("no fact to retract".into()))?;
        validity_row = sqlx::query_as(
            "SELECT valid_from, valid_from_precision, valid_from_grade, valid_to, valid_to_precision,
                    end_after_period, attested_from FROM facts WHERE id = $1",
        )
        .bind(fact)
        .fetch_optional(pool)
        .await?;
        if action == "revise" && empty_state_revision(pool, fact, target.predicate_id).await? {
            return Err(crate::graph::empty_state_span());
        }
        match crate::graph::reject_fact(pool, kb_id, fact).await {
            Ok(()) | Err(AppError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    if action == "retract" {
        return Ok(None);
    }
    let doc_time: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT doc_time FROM documents WHERE id = $1")
            .bind(document_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    let v = validity_row.unwrap_or_default();
    let validity = Validity {
        from: v.valid_from,
        from_precision: v.valid_from_precision.as_deref(),
        from_grade: v.valid_from_grade.as_deref(),
        to: v.valid_to,
        to_precision: v.valid_to_precision.as_deref(),
        attested_at: v.attested_from.or(doc_time),
        // The new row inherits the end point of the old row and the period it says it covers (#994); for one that changes an event or a constant attribute, the write path drops it
        end_after_period: v.end_after_period,
    };
    let object = match (target.object_id, &target.object_value) {
        (Some(o), _) => FactObject::Entity(o),
        (None, Some(val)) => FactObject::Value(val),
        _ => return Err(AppError::Validation("a fact needs an object".into())),
    };
    let mut conn = pool.acquire().await?;
    let (new, _) = crate::graph::insert_fact_on(
        &mut conn,
        kb_id,
        target.subject_id,
        Some(target.predicate_id),
        object,
        validity,
        0.9,
    )
    .await?;
    // evidence: the chunk containing the quote; if the quote spans chunks, fall back to the document's first chunk
    let placed = sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, doc_version)
         SELECT $1, c.id, $3, c.document_id, c.doc_version FROM chunks c
          WHERE c.document_id = $2 AND c.superseded_at IS NULL
            AND position(lower($3) IN lower(c.text)) > 0
          ORDER BY c.seq LIMIT 1
         ON CONFLICT DO NOTHING",
    )
    .bind(new)
    .bind(document_id)
    .bind(quote.unwrap_or(""))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if placed == 0 {
        sqlx::query(
            "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, doc_version)
             SELECT $1, c.id, $3, c.document_id, c.doc_version FROM chunks c
              WHERE c.document_id = $2 AND c.superseded_at IS NULL
              ORDER BY c.seq LIMIT 1
             ON CONFLICT DO NOTHING",
        )
        .bind(new)
        .bind(document_id)
        .bind(quote.unwrap_or(""))
        .execute(&mut *conn)
        .await?;
    }
    Ok(Some(new))
}

/// revise is retract-old-then-write-new, the new row inheriting the old row's time. When both ends of the old row are equal (an instant event, or a state written before #966),
/// and the new row's property is declared as a state, the carried-over span holds at no instant, so `Validity::under` does not write it:
/// refuse at verification, and at execution check first, then retract -- so we never retract the old row only to find the new one cannot be written
async fn empty_state_revision(pool: &PgPool, fact_id: Uuid, predicate_id: Uuid) -> AppResult<bool> {
    let point: Option<bool> = sqlx::query_scalar(
        "SELECT valid_from IS NOT NULL AND valid_to IS NOT DISTINCT FROM valid_from
           FROM facts WHERE id = $1",
    )
    .bind(fact_id)
    .fetch_optional(pool)
    .await?;
    Ok(point == Some(true)
        && crate::graph::predicate_temporal(pool, Some(predicate_id)).await?
            == crate::graph::Temporal::State)
}

#[derive(Debug, Default, sqlx::FromRow)]
struct ValidityRow {
    valid_from: Option<DateTime<Utc>>,
    valid_from_precision: Option<String>,
    valid_from_grade: Option<String>,
    valid_to: Option<DateTime<Utc>>,
    valid_to_precision: Option<String>,
    end_after_period: bool,
    attested_from: Option<DateTime<Utc>>,
}

/// A gate entry left for a human: which fact from which document, what the agent wants to do, which sentence it cites, and why it was left
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct HeldItem {
    pub id: Uuid,
    pub document_id: Uuid,
    pub document: String,
    pub action: String,
    pub flag: Option<String>,
    pub fact_id: Option<Uuid>,
    pub proposed: Option<Value>,
    pub reason: String,
    pub quote: Option<String>,
    pub detail: Option<String>,
    pub created_at: DateTime<Utc>,
}

pub async fn held(pool: &PgPool, kb_id: Uuid, limit: i64, offset: i64) -> AppResult<Vec<HeldItem>> {
    Ok(sqlx::query_as(
        "SELECT ea.id, ea.document_id, d.filename AS document, ea.action, ea.flag, ea.fact_id,
                ea.proposed, ea.reason, ea.quote, ea.detail, ea.created_at
           FROM errata_actions ea JOIN documents d ON d.id = ea.document_id
          WHERE ea.kb_id = $1 AND ea.status = 'held'
          ORDER BY ea.created_at, ea.id
          LIMIT $2 OFFSET $3",
    )
    .bind(kb_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

/// Several entries waiting on a human, with the time the oldest began waiting
pub async fn waiting(pool: &PgPool, kb_id: Uuid) -> AppResult<(i64, Option<DateTime<Utc>>)> {
    Ok(sqlx::query_as(
        "SELECT count(*), min(created_at) FROM errata_actions WHERE kb_id = $1 AND status = 'held'",
    )
    .bind(kb_id)
    .fetch_one(pool)
    .await?)
}

#[derive(sqlx::FromRow)]
struct HeldRow {
    action: String,
    fact_id: Option<Uuid>,
    document_id: Uuid,
    proposed: Option<Value>,
    quote: Option<String>,
}

/// A human's answer to a gate entry: approved means it runs, rejected means it is logged as rejected. Those not held (already answered, or in another library) return false
pub async fn decide_held(
    pool: &PgPool,
    kb_id: Uuid,
    action_id: Uuid,
    approve: bool,
    actor: Uuid,
) -> AppResult<bool> {
    let row: Option<HeldRow> = sqlx::query_as(
        "SELECT action, fact_id, document_id, proposed, quote FROM errata_actions
          WHERE id = $1 AND kb_id = $2 AND status = 'held'",
    )
    .bind(action_id)
    .bind(kb_id)
    .fetch_optional(pool)
    .await?;
    let Some(HeldRow {
        action,
        fact_id,
        document_id,
        proposed,
        quote,
    }) = row
    else {
        return Ok(false);
    };
    let mut new_fact_id = None;
    if approve {
        let target = proposed
            .as_ref()
            .and_then(Target::from_json)
            .ok_or_else(|| AppError::Validation("the held action has no target".into()))?;
        new_fact_id = apply(
            pool,
            kb_id,
            &action,
            fact_id,
            document_id,
            &target,
            quote.as_deref(),
        )
        .await?;
    }
    sqlx::query(
        "UPDATE errata_actions SET status = $3, new_fact_id = $4, decided_by = $5, decided_at = now()
          WHERE id = $1 AND kb_id = $2 AND status = 'held'",
    )
    .bind(action_id)
    .bind(kb_id)
    .bind(if approve { "applied" } else { "rejected" })
    .bind(new_fact_id)
    .bind(actor)
    .execute(pool)
    .await?;
    Ok(true)
}

#[cfg(test)]
mod quote_tests {
    use super::quote_in;

    #[test]
    fn a_quote_is_the_documents_words_up_to_case_and_spacing() {
        let doc = "Acme is based in London.\n  Jane Roe runs Acme.";
        assert!(quote_in(doc, "based in London"));
        assert!(quote_in(doc, "jane roe   runs acme"));
        assert!(!quote_in(doc, "based in Paris"));
        assert!(!quote_in(doc, ""));
        assert!(!quote_in(doc, "   "));
    }
}

#[cfg(test)]
#[path = "errata_tests.rs"]
mod tests;
