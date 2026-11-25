//! Execution gate (0027, #357): whether an automatic merge can land without a person depends on **what it can be reverted from**,
//! not only on confidence.
//!
//! A merge in the graph can be reverted: `revert_merge` moves the facts back, and since 0019 even the pre-merge graph can be read.
//! What cannot be reverted is what left the graph during the days it was live: derivations inferred from it, violations and alerts it raised,
//! triples it sent out in exports, and answers it gave with citations. Reverting restores the graph, not the world the graph was read into.
//!
//! **Whether this merge would immediately send anything out of the graph**. If so, it is left to a person, however confident the decision.
//! When a source declares that a name is an identity, a merge is also decided by a person (0071); the model may not overturn the source's declaration.
//! Only merges are handled: undoing them separately costs little, and the next merge is itself the undo.
//!
//! `hold` is a pure function, `impact_of` only fetches data; `hold_for_merge` checks the identity source first, then the impact.
//! The batch arbiter and the governance agent go through the same gate;
//! the "execution gate" on the roadmap (which checks the agent's calls) will also use this one, rather than a second one.

use sqlx::PgPool;
use utopia_core::AppResult;
use uuid::Uuid;

/// What a merge immediately affects
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Impact {
    /// Two facts on each side hold the same functional predicate and point to different entities; after the merge they cannot be ordered by time either:
    /// a `functional` violation, which the consistency check will raise on its next run — that is the first stop outside the graph.
    /// Records the predicate label; `inverse_functional` is computed symmetrically
    pub contradictions: Vec<String>,
    /// Number of still-valid derived facts involving either side: the merge changes the premise, so these derivations get rewritten
    pub derived: i64,
    /// Number of answers on either side that were recognized in a conversation: this entity is one people ask about, so a wrong merge carries into the next answer
    pub answered: i64,
}

/// Reason left for a person. `review_reason` writes the card's code, `explain` writes the sentence for the decision ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    Identity,
    Contradiction(String),
    Derived(i64),
    Answered(i64),
}

impl std::fmt::Display for Hold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Hold::Identity => write!(f, "identity"),
            Hold::Contradiction(p) => write!(f, "contradiction {p}"),
            Hold::Derived(n) => write!(f, "derived {n}"),
            Hold::Answered(n) => write!(f, "answered {n}"),
        }
    }
}

impl Hold {
    pub fn review_reason(&self) -> String {
        match self {
            Hold::Identity => "escalate_identity".into(),
            _ => format!("escalate_impact|{self}"),
        }
    }

    /// One sentence written to `agent_decisions.reason`: why the agent did not act, as seen by a person in the Agent queue
    pub fn explain(&self) -> String {
        match self {
            Hold::Identity => "a source treats one side's names as identifiers".into(),
            Hold::Contradiction(p) => {
                format!("merging would put two \"{p}\" facts on one entity")
            }
            Hold::Derived(n) => format!("{n} derived facts rest on one side"),
            Hold::Answered(n) => format!("one side was named in {n} answers"),
        }
    }
}

impl Impact {
    /// Text shown to the second-tier tool (0028): what the merge would affect, one item per line; if nothing is affected, say so too
    pub fn describe(&self) -> String {
        let mut lines = Vec::new();
        for p in &self.contradictions {
            lines.push(format!(
                "- merging would put two \"{p}\" facts on one entity; \"{p}\" allows one value"
            ));
        }
        if self.derived > 0 {
            lines.push(format!(
                "- {} derived fact(s) rest on one side and would be rewritten",
                self.derived
            ));
        }
        if self.answered > 0 {
            lines.push(format!(
                "- one side was named in {} chat answer(s)",
                self.answered
            ));
        }
        if lines.is_empty() {
            "(merging would touch nothing outside the graph: no one-value relation clashes, no derived facts, no answers named either side)".to_string()
        } else {
            lines.join(
                "
",
            )
        }
    }
}

/// Whether this merge should be left to a person. Contradictions come first — they would raise violations immediately; then derivations, then answers.
/// Nothing affected means None: if confidence is high enough, proceed automatically as before
pub fn hold(impact: &Impact) -> Option<Hold> {
    if let Some(p) = impact.contradictions.first() {
        return Some(Hold::Contradiction(p.clone()));
    }
    if impact.derived > 0 {
        return Some(Hold::Derived(impact.derived));
    }
    if impact.answered > 0 {
        return Some(Hold::Answered(impact.answered));
    }
    None
}

/// Name evidence from an identity source belongs to the entity, not only to the name it currently displays; mixed-in prose evidence does not revoke the declaration either.
/// Re-reads the source's switch every time an automatic merge is about to happen, including cached verdicts and the successor verdict for a side already merged in the same cluster.
pub async fn hold_for_merge(
    pool: &PgPool,
    kb_id: Uuid,
    a: Uuid,
    b: Uuid,
) -> AppResult<Option<Hold>> {
    let identity: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM facts f
             JOIN relation_types r ON r.id = f.predicate_id
             JOIN fact_evidence fe ON fe.fact_id = f.id
             JOIN documents d ON d.id = fe.document_id
             JOIN sources s ON s.id = d.source_id
             WHERE f.kb_id = $1 AND f.subject_id IN ($2, $3)
               AND f.invalidated_at IS NULL AND r.builtin AND r.key = $4
               AND s.kind = 'statements' AND s.config->>'names' = 'identity'
         )",
    )
    .bind(kb_id)
    .bind(a)
    .bind(b)
    .bind(crate::names::KNOWN_AS)
    .fetch_one(pool)
    .await?;
    if identity {
        return Ok(Some(Hold::Identity));
    }
    Ok(hold(&impact_of(pool, kb_id, a, b).await?))
}

/// Computes what merging a pair of entities would affect. Three queries, run only when the arbiter is confident enough to merge.
///
/// The write path's guard looks at the "current row" (`invalidated_at IS NULL`), and `resolve_mention`
/// counts a spelling as one; it does not use record-axis predicates — corrections always happen in the present (0019)
pub async fn impact_of(pool: &PgPool, kb_id: Uuid, a: Uuid, b: Uuid) -> AppResult<Impact> {
    // Same basis as the consistency check: only edges to entity objects, **judged by time** — when the two sides each hold a different value,
    // if after merging they can be ordered as successors (landlord first HPBB1, then BBHQ1), the consistency check does not report and this does not block either
    // Previously time was not considered here, so a lease whose landlord changed, with two names, could never be merged
    let contradictions = crate::temporal::merge_would_overlap(pool, kb_id, a, b).await?;

    let derived: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM derived_facts d
         WHERE d.kb_id = $1 AND d.invalidated_at IS NULL
           AND (d.subject_id IN ($2, $3) OR d.object_id IN ($2, $3))",
    )
    .bind(kb_id)
    .bind(a)
    .bind(b)
    .fetch_one(pool)
    .await?;

    // `resolved` is the entities recognized in each turn's answer (id, name, type); being recognized means having been asked about
    let answered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_messages m
         JOIN conversations c ON c.id = m.conversation_id
         WHERE c.kb_id = $1 AND m.role = 'assistant'
           AND EXISTS (SELECT 1 FROM jsonb_array_elements(m.resolved) e
                       WHERE e->>'id' IN ($2, $3))",
    )
    .bind(kb_id)
    .bind(a.to_string())
    .bind(b.to_string())
    .fetch_one(pool)
    .await?;

    Ok(Impact {
        contradictions,
        derived,
        answered,
    })
}

/// What retracting a fact would affect (the erratum agent under 0044 decision 7 goes through this same gate): derivations that rest on it and still hold;
/// answers in which its subject was recognized (an answer records what was recognized, not the fact cited — the subject was asked about,
/// so a fact about it may have entered the answer; same basis as merging). The contradiction column is left empty: retracting one does not raise violations
pub async fn impact_of_fact(pool: &PgPool, kb_id: Uuid, fact_id: Uuid) -> AppResult<Impact> {
    let derived: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT d.id) FROM fact_derivations fd
           JOIN derived_facts d ON d.id = fd.derived_fact_id
          WHERE fd.premise_fact_id = $2 AND d.kb_id = $1 AND d.invalidated_at IS NULL",
    )
    .bind(kb_id)
    .bind(fact_id)
    .fetch_one(pool)
    .await?;
    let answered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_messages m
           JOIN conversations c ON c.id = m.conversation_id
          WHERE c.kb_id = $1 AND m.role = 'assistant'
            AND EXISTS (SELECT 1 FROM jsonb_array_elements(m.resolved) e
                         WHERE e->>'id' = (SELECT subject_id::text FROM facts WHERE id = $2))",
    )
    .bind(kb_id)
    .bind(fact_id)
    .fetch_one(pool)
    .await?;
    Ok(Impact {
        contradictions: vec![],
        derived,
        answered,
    })
}

/// What writing a fact would affect: the predicate allows only one value and the subject already has another — the next consistency check run
/// raises a `functional` violation. Same basis as 0027 §5: only edges to entity objects, not time, not literal values
pub async fn impact_of_write(
    pool: &PgPool,
    kb_id: Uuid,
    subject_id: Uuid,
    predicate_id: Uuid,
    object_id: Option<Uuid>,
    // The write is replacing this row: while the old row is still live when a fact is changed, it does not count as a collision
    replacing: Option<Uuid>,
) -> AppResult<Impact> {
    let Some(object) = object_id else {
        return Ok(Impact::default());
    };
    let clash: Option<String> = sqlx::query_scalar(
        "SELECT r.label FROM relation_types r
          WHERE r.id = $2 AND r.kb_id = $1 AND r.functional
            AND EXISTS (SELECT 1 FROM facts f
                         WHERE f.kb_id = $1 AND f.subject_id = $3 AND f.predicate_id = $2
                           AND f.invalidated_at IS NULL AND f.object_id IS NOT NULL
                           AND f.object_id <> $4 AND f.id IS DISTINCT FROM $5)",
    )
    .bind(kb_id)
    .bind(predicate_id)
    .bind(subject_id)
    .bind(object)
    .bind(replacing)
    .fetch_optional(pool)
    .await?;
    Ok(Impact {
        contradictions: clash.into_iter().collect(),
        derived: 0,
        answered: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_touched_means_no_hold() {
        assert_eq!(hold(&Impact::default()), None);
    }

    #[test]
    fn a_contradiction_speaks_before_anything_else() {
        let i = Impact {
            contradictions: vec!["CEO of".into(), "headquartered in".into()],
            derived: 4,
            answered: 2,
        };
        assert_eq!(hold(&i), Some(Hold::Contradiction("CEO of".into())));
        assert_eq!(hold(&i).unwrap().to_string(), "contradiction CEO of");
    }

    #[test]
    fn describe_says_what_would_move_or_that_nothing_would() {
        assert!(Impact::default()
            .describe()
            .contains("nothing outside the graph"));
        let i = Impact {
            contradictions: vec!["CEO of".into()],
            derived: 2,
            answered: 0,
        };
        let text = i.describe();
        assert!(text.contains("two \"CEO of\" facts"));
        assert!(text.contains("2 derived fact"));
        assert!(!text.contains("chat answer"));
    }

    #[test]
    fn derived_then_answered() {
        let i = Impact {
            contradictions: vec![],
            derived: 3,
            answered: 1,
        };
        assert_eq!(hold(&i), Some(Hold::Derived(3)));
        let i = Impact { derived: 0, ..i };
        assert_eq!(hold(&i), Some(Hold::Answered(1)));
        assert_eq!(Hold::Answered(1).to_string(), "answered 1");
        assert_eq!(
            Hold::Contradiction("CEO of".into()).explain(),
            "merging would put two \"CEO of\" facts on one entity"
        );
    }
}
