//! Storage and retrieval of business rules (0021 / #277). Evaluation lives in `utopia-reason::rules`, and hooking it into materialization is in
//! `reasoning::materialize`; this module only handles how the rules people write down get into the KB and come back out.
//!
//! **The criterion is that people write them.** This module has no entry point where a model generates rules — the same line it takes on
//! axioms in 0002.

use serde_json::json;
use sqlx::PgPool;
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// The built-in `is_a` predicate: derived classification conclusions land on it (0021 decision 2).
///
/// **Created on demand, not laid down when the KB is created.** A KB that never writes a rule should not gain a predicate it does not understand
/// — consistent with the choice #231 made for `metric` / `dimension`.
pub async fn ensure_is_a(pool: &PgPool, kb_id: Uuid) -> AppResult<Uuid> {
    if let Some((id,)) =
        sqlx::query_as::<_, (Uuid,)>("SELECT id FROM relation_types WHERE kb_id = $1 AND key = $2")
            .bind(kb_id)
            .bind(IS_A)
            .fetch_optional(pool)
            .await?
    {
        return Ok(id);
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind, datatype, builtin, description)
         VALUES ($1, $2, $3, 'is a', 'attribute', 'text', TRUE, $4)
         ON CONFLICT (kb_id, key) DO NOTHING",
    )
    .bind(id)
    .bind(kb_id)
    .bind(IS_A)
    .bind("A class a rule concluded for this entity. Derived, and it never replaces the asserted type.")
    .execute(pool)
    .await?;
    // An ON CONFLICT hit means a concurrent creation already happened; fetch that one
    let (id,): (Uuid,) =
        sqlx::query_as("SELECT id FROM relation_types WHERE kb_id = $1 AND key = $2")
            .bind(kb_id)
            .bind(IS_A)
            .fetch_one(pool)
            .await?;
    Ok(id)
}

pub const IS_A: &str = "is_a";

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: Uuid,
    /// Which version the current definition is (0060). The migration backfilled version 1 for every existing rule, so there is always
    version: Option<i32>,
    name: String,
    description: String,
    subject_type_id: Uuid,
    subject_label: Option<String>,
    conclusion: String,
    conclude_type_id: Option<Uuid>,
    conclude_type_label: Option<String>,
    conclude_predicate_id: Option<Uuid>,
    conclude_predicate_label: Option<String>,
    conclude_value: Option<serde_json::Value>,
    conclude_expr: Option<serde_json::Value>,
    enabled: bool,
    join_predicate_id: Option<Uuid>,
    join_predicate_label: Option<String>,
    derived_count: i64,
    capped: i32,
}

/// One row returned by the condition query: the rule, group number, attribute predicate and its label, comparison method, operand
type ConditionRow = (
    Uuid,
    i32,
    Uuid,
    Option<String>,
    String,
    Option<serde_json::Value>,
    String,
);

/// A condition, the shape shared by the UI and the API.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConditionInput {
    /// Group number: AND within a group, OR between groups (0029). **Default 0** — a caller that does not send groups
    /// sends a single conjunction, exactly as before
    #[serde(default)]
    pub group: i32,
    pub predicate_id: Uuid,
    pub op: String,
    #[serde(default)]
    pub operand: Option<serde_json::Value>,
    /// Which side of a joined pair this condition reads: `x` is the rule
    /// subject and `y` is the entity reached by the one declared join edge.
    #[serde(default = "default_condition_side")]
    pub side: String,
}

fn default_condition_side() -> String {
    "x".to_string()
}

fn validate_name(name: &str) -> AppResult<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(AppError::invalid(
            "bad_rule_name",
            "A rule needs a name of 1-80 characters.",
        ));
    }
    Ok(name)
}

/// Creates a rule. **Validation is done here**: the condition's op and operand shape, the predicate must be an attribute,
/// and each of the two conclusion shapes must be complete — the CHECK in the database is the last line, but its error message is for people.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &PgPool,
    kb_id: Uuid,
    name: &str,
    description: &str,
    subject_type_id: Uuid,
    conclusion: &str,
    conclude_type_id: Option<Uuid>,
    conclude_predicate_id: Option<Uuid>,
    conclude_value: Option<serde_json::Value>,
    // The computed conclusion tree (0032)
    conclude_expr: Option<serde_json::Value>,
    join_predicate_id: Option<Uuid>,
    conditions: &[ConditionInput],
) -> AppResult<Uuid> {
    let name = validate_name(name)?;
    if conditions.is_empty() {
        // An empty conjunction is always true and would sweep the whole class in — blocking it at the entrance is earlier than silently deriving nothing in the evaluator
        return Err(AppError::invalid(
            "no_conditions",
            "A rule needs at least one condition; without one it would conclude for every entity of the class.",
        ));
    }
    validate_conditions(pool, kb_id, conditions, conclusion == "relation").await?;

    validate_conclusion(
        pool,
        kb_id,
        &ConclusionInput {
            kind: conclusion.to_string(),
            type_id: conclude_type_id,
            predicate_id: conclude_predicate_id,
            value: conclude_value.clone(),
            expr: conclude_expr.clone(),
            join_predicate_id,
        },
    )
    .await?;
    validate_join_shape(conclusion, join_predicate_id)?;
    exists(
        pool,
        kb_id,
        "entity_types",
        subject_type_id,
        "unknown_class",
        "That class is not in this base.",
    )
    .await?;

    let id = Uuid::now_v7();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO attribute_rules
             (id, kb_id, name, description, subject_type_id, conclusion,
              conclude_type_id, conclude_predicate_id, conclude_value, conclude_expr,
              join_predicate_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(id)
    .bind(kb_id)
    .bind(name)
    .bind(description.trim())
    .bind(subject_type_id)
    .bind(conclusion)
    .bind(conclude_type_id)
    .bind(conclude_predicate_id)
    .bind(&conclude_value)
    .bind(&conclude_expr)
    .bind(join_predicate_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref d) if d.is_unique_violation() => AppError::invalid(
            "duplicate_rule",
            "A rule with that name already exists here.",
        ),
        other => AppError::Db(other),
    })?;
    insert_conditions(&mut tx, id, conditions).await?;
    record_version(&mut tx, kb_id, id).await?;
    tx.commit().await?;
    Ok(id)
}

/// Edit a rule: replace the whole condition group.
///
/// **No per-condition add/update/delete** — the conditions are one conjunction as a whole; replacing reads better than patching against seq,
/// and there is no half-edited intermediate state.
/// One rewrite of the conclusion. **Both shapes are given in full**, not "only change the class, leave the rest" —
/// the three cells of the conclusion define each other, and a partial update would leave a half state with a typing but a value.
pub struct ConclusionInput {
    /// typing | attribute | computed
    pub kind: String,
    pub type_id: Option<Uuid>,
    pub predicate_id: Option<Uuid>,
    pub value: Option<serde_json::Value>,
    /// The computed conclusion tree (0032). Required when `computed`; the other two branches must be empty
    pub expr: Option<serde_json::Value>,
    /// The X --join--> Y edge; non-empty only for a relation conclusion
    pub join_predicate_id: Option<Uuid>,
}

#[allow(clippy::too_many_arguments)]
pub async fn update(
    pool: &PgPool,
    kb_id: Uuid,
    rule_id: Uuid,
    name: Option<&str>,
    description: Option<&str>,
    enabled: Option<bool>,
    conditions: Option<&[ConditionInput]>,
    conclusion: Option<&ConclusionInput>,
) -> AppResult<()> {
    let name = name.map(validate_name).transpose()?;
    if let Some(cs) = conditions {
        if cs.is_empty() {
            return Err(AppError::invalid(
                "no_conditions",
                "A rule needs at least one condition.",
            ));
        }
        let (old_conclusion, old_join) = sqlx::query_as::<_, (String, Option<Uuid>)>(
            "SELECT conclusion, join_predicate_id
               FROM attribute_rules
              WHERE id = $2 AND kb_id = $1",
        )
        .bind(kb_id)
        .bind(rule_id)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let joined = conclusion
            .map(|c| c.kind.as_str() == "relation")
            .unwrap_or(old_conclusion == "relation");
        validate_conditions(pool, kb_id, cs, joined).await?;
        let join = if joined {
            conclusion.and_then(|c| c.join_predicate_id).or(old_join)
        } else {
            None
        };
        validate_join_shape(if joined { "relation" } else { "other" }, join)?;
    }
    if let Some(c) = conclusion {
        validate_conclusion(pool, kb_id, c).await?;
        validate_join_shape(&c.kind, c.join_predicate_id)?;
    }
    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        // If the conclusion is given, the whole group is replaced (all three cells together); if not, no cell changes — COALESCE is not enough here:
        // changing typing to attribute requires setting conclude_type_id back to NULL,
        // and COALESCE is exactly what cannot do an explicit clear
        "UPDATE attribute_rules
            SET name = COALESCE($3, name),
                description = COALESCE($4, description),
                enabled = COALESCE($5, enabled),
                conclusion = COALESCE($6, conclusion),
                conclude_type_id      = CASE WHEN $6 IS NULL THEN conclude_type_id      ELSE $7 END,
                conclude_predicate_id = CASE WHEN $6 IS NULL THEN conclude_predicate_id ELSE $8 END,
                conclude_value        = CASE WHEN $6 IS NULL THEN conclude_value        ELSE $9 END,
                conclude_expr         = CASE WHEN $6 IS NULL THEN conclude_expr         ELSE $10 END,
                join_predicate_id     = CASE WHEN $6 IS NULL THEN join_predicate_id     ELSE $11 END,
                updated_at = now()
          WHERE id = $2 AND kb_id = $1",
    )
    .bind(kb_id)
    .bind(rule_id)
    .bind(name)
    .bind(description.map(str::trim))
    .bind(enabled)
    .bind(conclusion.map(|c| c.kind.as_str()))
    .bind(conclusion.and_then(|c| c.type_id))
    .bind(conclusion.and_then(|c| c.predicate_id))
    .bind(conclusion.and_then(|c| c.value.clone()))
    .bind(conclusion.and_then(|c| c.expr.clone()))
    .bind(conclusion.and_then(|c| c.join_predicate_id))
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    if let Some(cs) = conditions {
        sqlx::query("DELETE FROM attribute_rule_conditions WHERE rule_id = $1")
            .bind(rule_id)
            .execute(&mut *tx)
            .await?;
        insert_conditions(&mut tx, rule_id, cs).await?;
    }
    if conditions.is_some() || conclusion.is_some() {
        // Validate the final conditions inside the transaction that holds the rule's row lock; neither kept conditions nor concurrent updates may bypass it.
        let reads_y_without_join: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM attribute_rules r
                 JOIN attribute_rule_conditions c ON c.rule_id = r.id
                 WHERE r.kb_id = $1 AND r.id = $2
                   AND r.join_predicate_id IS NULL AND c.subject_side = 'y'
             )",
        )
        .bind(kb_id)
        .bind(rule_id)
        .fetch_one(&mut *tx)
        .await?;
        if reads_y_without_join {
            return Err(AppError::invalid(
                "condition_side_without_join",
                "Only a joined rule can read the other side of an edge.",
            ));
        }
    }
    // Open a new version only when the definition changed; renames, description edits, and toggles do not count (0060)
    record_version(&mut tx, kb_id, rule_id).await?;
    tx.commit().await?;
    Ok(())
}

/// Snapshot of the definition (0060): what the rule says — the subject class, the conclusion cells, the connecting predicate, the conditions. Name,
/// description and toggle are not in it: they are labels and switches; changing them does not mean the rule says something different.
///
/// **Must be word-for-word identical to the expression that backfills version 1 in migration 0076**: whether a version changed is determined by comparing this JSON;
/// if the two shapes differ, unchanged rules would also be opened as new versions
const DEFINITION_SQL: &str = "jsonb_build_object(
           'subject_type_id', r.subject_type_id,
           'conclusion', r.conclusion,
           'conclude_type_id', r.conclude_type_id,
           'conclude_predicate_id', r.conclude_predicate_id,
           'conclude_value', r.conclude_value,
           'conclude_expr', r.conclude_expr,
           'join_predicate_id', r.join_predicate_id,
           'conditions', COALESCE((SELECT jsonb_agg(jsonb_build_object(
                   'group', c.group_seq, 'seq', c.seq, 'side', c.subject_side,
                   'predicate_id', c.predicate_id, 'op', c.op, 'operand', c.operand)
                   ORDER BY c.group_seq, c.seq)
               FROM attribute_rule_conditions c WHERE c.rule_id = r.id), '[]'::jsonb))";

/// If the definition changed, open a new version: close the current one and increment the sequence number. If nothing changed, do nothing. Returns the current version's id.
///
/// Runs inside the same transaction that writes the rule: the definition and its version either both land or both do not
async fn record_version(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kb_id: Uuid,
    rule_id: Uuid,
) -> AppResult<Uuid> {
    let now: serde_json::Value = sqlx::query_scalar(&format!(
        "SELECT {DEFINITION_SQL} FROM attribute_rules r WHERE r.id = $1"
    ))
    .bind(rule_id)
    .fetch_one(&mut **tx)
    .await?;
    let current: Option<(Uuid, i32, serde_json::Value)> = sqlx::query_as(
        "SELECT id, seq, definition FROM attribute_rule_versions
          WHERE rule_id = $1 AND superseded_at IS NULL",
    )
    .bind(rule_id)
    .fetch_optional(&mut **tx)
    .await?;
    let seq = match current {
        Some((id, _, definition)) if definition == now => return Ok(id),
        Some((id, seq, _)) => {
            sqlx::query("UPDATE attribute_rule_versions SET superseded_at = now() WHERE id = $1")
                .bind(id)
                .execute(&mut **tx)
                .await?;
            seq + 1
        }
        None => 1,
    };
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO attribute_rule_versions (id, kb_id, rule_id, seq, definition)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(kb_id)
    .bind(rule_id)
    .bind(seq)
    .bind(&now)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

/// One version: id, sequence number, definition, recording start and end times, and the count of conclusions that hold on it right now
type VersionRow = (
    Uuid,
    i32,
    serde_json::Value,
    chrono::DateTime<chrono::Utc>,
    Option<chrono::DateTime<chrono::Utc>>,
    i64,
);

/// A rule's definition history, newest first (0060). Each version carries its recording start and end times and the count of conclusions
/// holding on it right now, plus the labels of the classes and predicates the definition mentions — an id in the history may have been renamed or even deleted,
/// so labels are resolved as of now; where one cannot be resolved, the UI shows the id
pub async fn versions(
    pool: &PgPool,
    kb_id: Uuid,
    rule_id: Uuid,
) -> AppResult<Vec<serde_json::Value>> {
    let rows: Vec<VersionRow> = sqlx::query_as(
        "SELECT v.id, v.seq, v.definition, v.recorded_at, v.superseded_at,
                (SELECT count(*) FROM derived_facts d
                  WHERE d.attribute_rule_version_id = v.id AND d.invalidated_at IS NULL)
           FROM attribute_rule_versions v
          WHERE v.kb_id = $1 AND v.rule_id = $2
          ORDER BY v.seq DESC",
    )
    .bind(kb_id)
    .bind(rule_id)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        // The rule does not exist, or is not in this KB: both are a 404 to the caller
        let known: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM attribute_rules WHERE id = $2 AND kb_id = $1")
                .bind(kb_id)
                .bind(rule_id)
                .fetch_optional(pool)
                .await?;
        if known.is_none() {
            return Err(AppError::NotFound);
        }
    }
    let uuid_at =
        |v: &serde_json::Value, k: &str| -> Option<Uuid> { v.get(k)?.as_str()?.parse().ok() };
    let mut classes: Vec<Uuid> = Vec::new();
    let mut predicates: Vec<Uuid> = Vec::new();
    for (_, _, d, _, _, _) in &rows {
        classes.extend(uuid_at(d, "subject_type_id"));
        classes.extend(uuid_at(d, "conclude_type_id"));
        predicates.extend(uuid_at(d, "conclude_predicate_id"));
        predicates.extend(uuid_at(d, "join_predicate_id"));
        for c in d
            .get("conditions")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
        {
            predicates.extend(uuid_at(c, "predicate_id"));
        }
    }
    let mut labels: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    for (table, ids) in [("entity_types", classes), ("relation_types", predicates)] {
        if ids.is_empty() {
            continue;
        }
        let found: Vec<(Uuid, String)> =
            sqlx::query_as(&format!("SELECT id, label FROM {table} WHERE id = ANY($1)"))
                .bind(&ids)
                .fetch_all(pool)
                .await?;
        for (id, label) in found {
            labels.insert(id.to_string(), serde_json::Value::String(label));
        }
    }
    Ok(rows
        .into_iter()
        .map(
            |(id, seq, definition, recorded_at, superseded_at, derived_count)| {
                json!({
                    "id": id,
                    "seq": seq,
                    "definition": definition,
                    "recorded_at": recorded_at,
                    "superseded_at": superseded_at,
                    "derived_count": derived_count,
                    "labels": labels,
                })
            },
        )
        .collect())
}

/// Delete a rule. The derived rows it inferred go with it via `ON DELETE CASCADE` — **once the rule is gone,
/// the conclusions derived from it have no basis, and keeping them would leave them unexplainable**.
pub async fn delete(pool: &PgPool, kb_id: Uuid, rule_id: Uuid) -> AppResult<()> {
    let res = sqlx::query("DELETE FROM attribute_rules WHERE id = $2 AND kb_id = $1")
        .bind(kb_id)
        .bind(rule_id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// List rules, together with their conditions and how many conclusions each one currently infers.
pub async fn list(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<serde_json::Value>> {
    let rules: Vec<RuleRow> = sqlx::query_as(
        "SELECT r.id, r.name, r.description, r.subject_type_id,
                st.label AS subject_label,
                r.conclusion, r.conclude_type_id, ct.label AS conclude_type_label,
                r.conclude_predicate_id, cp.label AS conclude_predicate_label,
                r.conclude_value, r.conclude_expr, r.enabled,
                r.join_predicate_id, jp.label AS join_predicate_label,
                (SELECT v.seq FROM attribute_rule_versions v
                  WHERE v.rule_id = r.id AND v.superseded_at IS NULL) AS version,
                (SELECT count(*) FROM derived_facts d
                  WHERE d.attribute_rule_id = r.id AND d.invalidated_at IS NULL)
                    AS derived_count,
                r.capped_at_last_run AS capped
           FROM attribute_rules r
           JOIN entity_types st ON st.id = r.subject_type_id
           LEFT JOIN entity_types ct ON ct.id = r.conclude_type_id
           LEFT JOIN relation_types cp ON cp.id = r.conclude_predicate_id
           LEFT JOIN relation_types jp ON jp.id = r.join_predicate_id
          WHERE r.kb_id = $1
          ORDER BY r.created_at",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    if rules.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = rules.iter().map(|r| r.id).collect();
    let conds: Vec<ConditionRow> = sqlx::query_as(
        "SELECT c.rule_id, c.group_seq, c.predicate_id, p.label, c.op, c.operand,
                c.subject_side
               FROM attribute_rule_conditions c
               JOIN relation_types p ON p.id = c.predicate_id
              WHERE c.rule_id = ANY($1)
              ORDER BY c.rule_id, c.group_seq, c.seq",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;

    Ok(rules
        .into_iter()
        .map(|r| {
            let conditions: Vec<serde_json::Value> = conds
                .iter()
                .filter(|c| c.0 == r.id)
                .map(|(_, group, pid, plabel, op, operand, side)| {
                    json!({
                        "group": group,
                        "side": side,
                        "predicate_id": pid,
                        "predicate_label": plabel,
                        "op": op,
                        "operand": operand,
                    })
                })
                .collect();
            json!({
                "id": r.id,
                "name": r.name,
                "description": r.description,
                "subject_type_id": r.subject_type_id,
                "subject_label": r.subject_label,
                "conclusion": r.conclusion,
                "conclude_type_id": r.conclude_type_id,
                "conclude_type_label": r.conclude_type_label,
                "conclude_predicate_id": r.conclude_predicate_id,
                "conclude_predicate_label": r.conclude_predicate_label,
                "conclude_value": r.conclude_value,
                "conclude_expr": r.conclude_expr,
                "join_predicate_id": r.join_predicate_id,
                "join_predicate_label": r.join_predicate_label,
                "enabled": r.enabled,
                "version": r.version.unwrap_or(1),
                "derived_count": r.derived_count,
                "capped": r.capped,
                "conditions": conditions,
            })
        })
        .collect())
}

/// Read-only descriptions of stored computation trees. Reuse the write-side
/// shape/depth check, then resolve every leaf in one query scoped to this base.
/// Invalid trees or unavailable attributes are reported as unavailable, not null.
pub async fn describe_expressions(
    pool: &PgPool,
    kb_id: Uuid,
    expressions: &[&serde_json::Value],
) -> AppResult<Vec<Option<String>>> {
    let reads: Vec<_> = expressions.iter().map(|e| validate_expr(e).ok()).collect();
    let mut ids: Vec<Uuid> = reads.iter().flatten().flatten().copied().collect();
    ids.sort();
    ids.dedup();
    let mut names = std::collections::HashMap::new();
    if !ids.is_empty() {
        let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
            "SELECT id, key, label FROM relation_types WHERE kb_id=$1 AND id=ANY($2)",
        )
        .bind(kb_id)
        .bind(&ids)
        .fetch_all(pool)
        .await?;
        for (id, key, label) in rows {
            // Keys keep distinct attributes identifiable even when labels coincide.
            let name = if label.is_empty() || label == key {
                key
            } else {
                format!("{label} [{key}]")
            };
            names.insert(id, name);
        }
    }
    Ok(expressions
        .iter()
        .zip(reads)
        .map(|(e, valid)| valid.and_then(|_| expression_text(e, &names)))
        .collect())
}

// Only called after validate_expr has accepted the tree and bounded its depth.
fn expression_text(
    raw: &serde_json::Value,
    names: &std::collections::HashMap<Uuid, String>,
) -> Option<String> {
    use utopia_reason::rules::Arith;
    if let Some(attr) = raw.get("attr") {
        return names.get(&attr.as_str()?.parse::<Uuid>().ok()?).cloned();
    }
    if let Some(value) = raw.get("const") {
        return Some(
            value
                .as_str()
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| value.to_string()),
        );
    }
    let op = match Arith::parse(raw.get("op")?.as_str()?)? {
        Arith::Add => "+",
        Arith::Sub => "-",
        Arith::Mul => "*",
        Arith::Div => "/",
    };
    Some(format!(
        "({} {op} {})",
        expression_text(raw.get("l")?, names)?,
        expression_text(raw.get("r")?, names)?
    ))
}

/// One row returned by the hit query: derived id, entity id and name, conclusion, both ends of the interval, and the readable form of the premises
type MatchRow = (
    Uuid,
    Uuid,
    String,
    Option<Uuid>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
    Option<String>,
    Vec<String>,
);

/// Which entities this rule currently tags.
///
/// **The number on the rule card has to be clickable**: twenty entities can still be opened one by one, but with two hundred you can only
/// rely on this list. The premises are returned too — "which readings it was derived from" is half the answer, just like the conclusion itself.
pub async fn matches(
    pool: &PgPool,
    kb_id: Uuid,
    rule_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<serde_json::Value>, i64)> {
    let total: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM derived_facts d
          WHERE d.kb_id = $1 AND d.attribute_rule_id = $2 AND d.invalidated_at IS NULL",
    )
    .bind(kb_id)
    .bind(rule_id)
    .fetch_one(pool)
    .await?;

    let rows: Vec<MatchRow> = sqlx::query_as(
        // The name read out as the conclusion must be the human-facing one. What is stored in the KB is the class key/IRI (classification) or
        // the literal value (attribute); neither should be sent out as-is
        "SELECT d.id, e.id, e.canonical_name,
                o.id, o.canonical_name,
                COALESCE(ct.label,
                         d.object_value #>> '{value}',
                         d.object_value ->> 'class'),
                rp.label,
                d.valid_from, d.valid_to, d.valid_from_precision, d.valid_to_precision,
                COALESCE(
                    (SELECT array_agg(
                                COALESCE(pr.label, '?') || ' = '
                                || COALESCE(fd.object_value #>> '{value}',
                                            fd.object_value #>> '{class}', '?')
                                ORDER BY fd.seq)
                       FROM derivation_premises fd
                       LEFT JOIN relation_types pr ON pr.id = fd.predicate_id
                      WHERE fd.derived_fact_id = d.id),
                    ARRAY[]::text[]
                )
           FROM derived_facts d
           JOIN entities e ON e.id = d.subject_id
           LEFT JOIN entities o ON o.id = d.object_id
           JOIN attribute_rules ar ON ar.id = d.attribute_rule_id
           LEFT JOIN entity_types ct ON ct.id = ar.conclude_type_id
           LEFT JOIN relation_types rp ON rp.id = d.predicate_id
          WHERE d.kb_id = $1 AND d.attribute_rule_id = $2 AND d.invalidated_at IS NULL
          ORDER BY e.canonical_name, d.valid_from, d.id
          LIMIT $3 OFFSET $4",
    )
    .bind(kb_id)
    .bind(rule_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok((
        rows.into_iter()
            .map(
                |(
                    id,
                    entity_id,
                    name,
                    object_id,
                    object_name,
                    concluded,
                    relation_label,
                    from,
                    to,
                    fp,
                    tp,
                    premises,
                )| {
                    json!({
                        "derived_id": id,
                        "entity_id": entity_id,
                        "entity": name,
                        "object_id": object_id,
                        "object_entity": object_name,
                        "concluded": concluded,
                        "relation_predicate": relation_label,
                        "valid_from": from,
                        "valid_to": to,
                        "valid_from_precision": fp,
                        "valid_to_precision": tp,
                        "premises": premises,
                    })
                },
            )
            .collect(),
        total.0,
    ))
}

async fn insert_conditions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    rule_id: Uuid,
    conditions: &[ConditionInput],
) -> AppResult<()> {
    // seq is numbered **within a group**: (rule, group, seq) is the position of a condition; each of the two groups starts from 0
    let mut next: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    for c in conditions {
        let seq = next.entry(c.group).or_insert(0);
        sqlx::query(
            "INSERT INTO attribute_rule_conditions
                (id, rule_id, group_seq, seq, predicate_id, op, operand, subject_side)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(Uuid::now_v7())
        .bind(rule_id)
        .bind(c.group)
        .bind(*seq)
        .bind(c.predicate_id)
        .bind(&c.op)
        .bind(&c.operand)
        .bind(&c.side)
        .execute(&mut **tx)
        .await?;
        *seq += 1;
    }
    Ok(())
}

/// Validation of the conclusion. **Create and edit share this one copy** — written in two places, changing it once could bypass the create-time check,
/// while the CHECK in the database only returns a constraint name, leaving the reader no way to know which cell is wrong.
async fn validate_conclusion(pool: &PgPool, kb_id: Uuid, c: &ConclusionInput) -> AppResult<()> {
    match c.kind.as_str() {
        "typing" => {
            let t = c.type_id.ok_or_else(|| {
                AppError::invalid("no_class", "A typing rule needs the class it concludes.")
            })?;
            exists(
                pool,
                kb_id,
                "entity_types",
                t,
                "unknown_class",
                "That class is not in this base.",
            )
            .await?;
            // A classification conclusion lands on the built-in is_a; create it the first time this kind of rule is written
            ensure_is_a(pool, kb_id).await?;
            Ok(())
        }
        "attribute" => {
            let p = c.predicate_id.ok_or_else(|| {
                AppError::invalid(
                    "no_predicate",
                    "An attribute rule needs the attribute it sets.",
                )
            })?;
            if c.value.is_none() {
                return Err(AppError::invalid(
                    "no_value",
                    "An attribute rule needs the value it sets.",
                ));
            }
            attribute_predicate(pool, kb_id, p).await
        }
        // Computed attribute (0032): the predicate still has to exist, and the value becomes an expression tree
        "computed" => {
            let p = c.predicate_id.ok_or_else(|| {
                AppError::invalid(
                    "no_predicate",
                    "A computed rule needs the attribute it sets.",
                )
            })?;
            let Some(expr) = c.expr.as_ref() else {
                return Err(AppError::invalid(
                    "no_expression",
                    "A computed rule needs the expression it computes.",
                ));
            };
            // **These are blocked at the entrance, not left to the evaluator.** A tree it cannot read only results in
            // "this rule derives nothing" at evaluation time, and the person who wrote it sees no reason at all (that is how `not_in`
            // was lost in that case, see #494)
            let reads = validate_expr(expr)?;
            if reads.is_empty() {
                return Err(AppError::invalid(
                    "constant_expression",
                    "A computed rule that reads no attribute is a constant; set the value directly.",
                ));
            }
            for r in reads {
                attribute_predicate(pool, kb_id, r).await?;
            }
            attribute_predicate(pool, kb_id, p).await
        }
        // A relation conclusion names the edge itself, so both ends are
        // entities. The join edge and the conclusion may be different declared
        // predicates: supplies supplies(Y, Z) can conclude upstream_of(X, Z).
        "relation" => {
            let p = c.predicate_id.ok_or_else(|| {
                AppError::invalid(
                    "no_predicate",
                    "A relation rule needs the relation it concludes.",
                )
            })?;
            relation_predicate(pool, kb_id, p).await
        }
        _ => Err(AppError::invalid(
            "bad_conclusion",
            "A conclusion is a typing, an attribute, a computed attribute or a relation.",
        )),
    }
}

/// Validation of an expression tree, returning the predicate it reads.
///
/// **When the shape is wrong, say exactly where.** The CHECK in the database only enforces "computed needs a tree";
/// whether the tree itself is well-formed has to be judged here — otherwise a badly written tree only shows up at the next materialization as
/// "this rule derives nothing", and at that point nothing anywhere can say why.
fn validate_expr(raw: &serde_json::Value) -> AppResult<Vec<Uuid>> {
    let expr = utopia_reason::rules::Expr::from_json(raw)
        .map_err(|e| AppError::invalid(e.code(), e.message()))?;
    let mut reads = Vec::new();
    expr.predicates(&mut reads);
    Ok(reads)
}

/// Validation of conditions. **Errors must be in plain words**: the CHECK in the database only returns a constraint name.
async fn validate_conditions(
    pool: &PgPool,
    kb_id: Uuid,
    conditions: &[ConditionInput],
    joined: bool,
) -> AppResult<()> {
    for c in conditions {
        let op = utopia_reason::rules::Op::parse(&c.op).ok_or_else(|| {
            AppError::invalid(
                "bad_op",
                "A condition compares with >, >=, <, <=, a range, a set (in or not in), or presence.",
            )
        })?;
        let side = utopia_reason::rules::Side::parse(&c.side).ok_or_else(|| {
            AppError::invalid(
                "bad_condition_side",
                "A condition reads x (the rule's subject) or y (the joined entity).",
            )
        })?;
        if !joined && side != utopia_reason::rules::Side::X {
            return Err(AppError::invalid(
                "condition_side_without_join",
                "Only a joined rule can read the other side of an edge.",
            ));
        }
        attribute_predicate(pool, kb_id, c.predicate_id).await?;
        // ADR 0032 already permits expression thresholds. Validate the same AST
        // and same-base attribute references as computed conclusions; sets,
        // ranges and presence retain their separate operand contracts.
        if matches!(
            op,
            utopia_reason::rules::Op::Gt
                | utopia_reason::rules::Op::Gte
                | utopia_reason::rules::Op::Lt
                | utopia_reason::rules::Op::Lte
        ) {
            if let Some(expr) = c.operand.as_ref().filter(|v| v.is_object()) {
                for predicate in validate_expr(expr)? {
                    attribute_predicate(pool, kb_id, predicate).await?;
                }
                continue;
            }
        }
        let shaped = match op {
            utopia_reason::rules::Op::Present => c.operand.is_none(),
            utopia_reason::rules::Op::NotIn => c
                .operand
                .as_ref()
                .and_then(|v| v.as_array())
                .is_some_and(|a| !a.is_empty()),
            utopia_reason::rules::Op::Between => c
                .operand
                .as_ref()
                .and_then(|v| v.as_array())
                .is_some_and(|a| a.len() == 2 && a.iter().all(|x| x.as_f64().is_some())),
            utopia_reason::rules::Op::In => c
                .operand
                .as_ref()
                .and_then(|v| v.as_array())
                .is_some_and(|a| !a.is_empty()),
            _ => c.operand.as_ref().is_some_and(|v| {
                v.as_f64().is_some()
                    || v.as_str()
                        .and_then(|s| s.trim().parse::<f64>().ok())
                        .is_some()
            }),
        };
        if !shaped {
            return Err(AppError::invalid(
                "bad_operand",
                "That comparison does not match the value it was given: a threshold needs a number, a range needs two, a set needs at least one entry, and presence takes none.",
            ));
        }
    }
    Ok(())
}

/// Conditions and attribute conclusions may only land on predicates with kind='attribute' — the rule reads this entity's
/// own literal values; relations (entity to entity) do not take part (0021 decision 3).
async fn attribute_predicate(pool: &PgPool, kb_id: Uuid, id: Uuid) -> AppResult<()> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT kind FROM relation_types WHERE id = $2 AND kb_id = $1")
            .bind(kb_id)
            .bind(id)
            .fetch_optional(pool)
            .await?;
    match row.as_ref().map(|(k,)| k.as_str()) {
        Some("attribute") => Ok(()),
        Some(_) => Err(AppError::invalid(
            "not_an_attribute",
            "A rule reads this entity's own attributes; a relation to another entity cannot be a condition.",
        )),
        None => Err(AppError::invalid(
            "unknown_predicate",
            "That attribute is not in this base.",
        )),
    }
}

async fn relation_predicate(pool: &PgPool, kb_id: Uuid, id: Uuid) -> AppResult<()> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT kind FROM relation_types WHERE id = $2 AND kb_id = $1")
            .bind(kb_id)
            .bind(id)
            .fetch_optional(pool)
            .await?;
    match row.as_ref().map(|(k,)| k.as_str()) {
        Some("relation") => Ok(()),
        Some(_) => Err(AppError::invalid(
            "not_a_relation",
            "A join or relation conclusion names an edge between two entities.",
        )),
        None => Err(AppError::invalid(
            "unknown_predicate",
            "That relation is not in this base.",
        )),
    }
}

/// Keep the conclusion and its join edge as one replaceable shape. The database
/// CHECK is the last line of defence; these errors say which half is missing.
fn validate_join_shape(kind: &str, join_predicate_id: Option<Uuid>) -> AppResult<()> {
    match (kind == "relation", join_predicate_id.is_some()) {
        (true, true) | (false, false) => Ok(()),
        (true, false) => Err(AppError::invalid(
            "no_join_predicate",
            "A relation rule needs the relation that connects X to Y.",
        )),
        (false, true) => Err(AppError::invalid(
            "join_without_relation",
            "Only a relation conclusion can name a join predicate.",
        )),
    }
}

async fn exists(
    pool: &PgPool,
    kb_id: Uuid,
    table: &str,
    id: Uuid,
    code: &'static str,
    message: &'static str,
) -> AppResult<()> {
    let sql = format!("SELECT 1 FROM {table} WHERE id = $2 AND kb_id = $1");
    let row: Option<(i32,)> = sqlx::query_as(&sql)
        .bind(kb_id)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.map(|_| ())
        .ok_or_else(|| AppError::invalid(code, message))
}
