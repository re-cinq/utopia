//! Business rules: read an entity's own attribute facts and derive a conclusion (see `docs/decisions/0021`).
//!
//! **A separate pass from `derive()`.** That pass walks entity–entity edges and takes the closure under the axioms;
//! this pass does "compare the literal value against a threshold". The inputs, criteria and termination conditions of the two differ,
//! and cramming them into one function would only make both harder to read.
//!
//! What it shares with that pass is the **interval semantics** (`derive::validity`) and the identity that "the conclusion is derived":
//! whatever a hit produces still goes into `derived_facts`, still carries its premises, and still becomes invalid when a premise disappears.
//!
//! This layer likewise does not touch the database. Fetching rules, fetching attribute facts and persisting all happen in `utopia-store`.

use crate::derive::validity;
pub use crate::expressions::{Expr, Scalar, MAX_EXPR_DEPTH};
use std::collections::HashMap;
use uuid::Uuid;

/// A literal value for one entity, one attribute, over one time period. The full input to the evaluator.
#[derive(Debug, Clone, PartialEq)]
pub struct AttrFact {
    pub id: Uuid,
    pub subject: Uuid,
    /// The predicate in `relation_types` with kind='attribute'
    pub predicate: Uuid,
    /// `facts.object_value -> 'value'`, already extracted from the wrapper
    pub value: serde_json::Value,
}

/// An entity–entity edge taking part in a rule's join.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleEdge {
    pub id: Uuid,
    pub predicate: Uuid,
    pub subject: Uuid,
    pub object: Uuid,
}

/// The two sides of the join edge. `X` is the rule's subject; `Y` is the other entity this edge connects it to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    X,
    Y,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::X => "x",
            Side::Y => "y",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "x" | "X" => Side::X,
            "y" | "Y" => Side::Y,
            _ => return None,
        })
    }
}

/// How a condition compares. One-to-one with the CHECK on `attribute_rule_conditions.op`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Gt,
    Gte,
    Lt,
    Lte,
    Between,
    In,
    /// The inverse In. **It still judges an existing fact**, so it still has premises and an interval;
    /// "the attribute is absent altogether" is another matter and is not covered by this (see the end of 0029)
    NotIn,
    Present,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Gt => "gt",
            Op::Gte => "gte",
            Op::Lt => "lt",
            Op::Lte => "lte",
            Op::Between => "between",
            Op::In => "in",
            Op::NotIn => "not_in",
            Op::Present => "present",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "gt" => Op::Gt,
            "gte" => Op::Gte,
            "lt" => Op::Lt,
            "lte" => Op::Lte,
            "between" => Op::Between,
            "in" => Op::In,
            "not_in" => Op::NotIn,
            "present" => Op::Present,
            _ => return None,
        })
    }
}

/// Operand. Its shape is decided by `op`; it is parsed once on read, so the evaluation hot path no longer touches JSON.
#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Num(f64),
    /// Closed interval [lo, hi]
    Range(f64, f64),
    /// Category set. **Exact string match**—the same category in another language does not count as a hit; this is the open question 0021 recorded
    /// and fuzzy matching is not quietly done here
    Set(Vec<String>),
    /// A computed number (0032): a threshold calculated from this entity's other readings, rather than a hard-coded
    Calc(Expr),
    None,
}

/// Four arithmetic operations. **Only these four**: adding another would require answering what it means for missing readings, division by zero, and units,
/// and that is the start of a language (0002 / 0032)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arith {
    Add,
    Sub,
    Mul,
    Div,
}

impl Arith {
    pub fn as_str(self) -> &'static str {
        match self {
            Arith::Add => "add",
            Arith::Sub => "sub",
            Arith::Mul => "mul",
            Arith::Div => "div",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "add" => Arith::Add,
            "sub" => Arith::Sub,
            "mul" => Arith::Mul,
            "div" => Arith::Div,
            _ => return None,
        })
    }
}

impl Expr {
    /// Comparisons still need a number, including the legacy numeric-string readings.
    pub fn eval(&self, bound: &HashMap<Uuid, &AttrFact>) -> Option<f64> {
        self.compute(bound)?.number()
    }

    fn compute(&self, bound: &HashMap<Uuid, &AttrFact>) -> Option<Scalar> {
        let value = self.evaluate(&|id| Scalar::read(&bound.get(&id)?.value))?;
        // An old bare-attribute expression meant a numeric reading. CASE and
        // date truncation introduce text results explicitly, without changing it.
        if matches!(self, Self::Attr(_)) {
            value.number().map(Scalar::Number)
        } else {
            Some(value)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    /// Conditions in the same group are joined with AND, groups are joined with OR (0029). Old rules are all in group 0
    pub group: i32,
    /// Which side of the join this condition reads. Old rules without a join always read X
    pub side: Side,
    pub predicate: Uuid,
    pub op: Op,
    pub operand: Operand,
}

/// Conclusion. The two shapes correspond to the two branches of `attribute_rules.conclusion`.
#[derive(Debug, Clone, PartialEq)]
pub enum Conclusion {
    /// Derived classification: the conclusion is a class, stored as a literal value on `is_a`
    Typing { class: String },
    /// Derived attribute: a computed number (0032). The only difference from the branch above is where the value comes from—
    /// the readings the formula reads **also go into the premises**, so the conclusion can still say "on what basis"
    Computed { predicate: Uuid, expr: Expr },
    /// Derived attribute: a literal value on some attribute predicate
    Attribute {
        predicate: Uuid,
        value: serde_json::Value,
    },
    /// Derived relation: from X to Y through the join the rule declares (0047)
    Relation { predicate: Uuid },
}

#[derive(Debug, Clone, PartialEq)]
pub struct BusinessRule {
    pub id: Uuid,
    pub conclusion: Conclusion,
    /// The predicate that connects X and Y. `None` is the old single-entity rule
    pub join_predicate: Option<Uuid>,
    /// Conditions. **AND within a group, OR between groups** (0029): a group holds only when all its conditions are satisfied,
    /// and the rule hits when any group holds. An empty condition set never hits—a rule with no criteria should
    /// infer nothing, rather than sweep in the whole class
    pub conditions: Vec<Condition>,
}

/// One hit. The premises are **the facts that actually made it hold**, not all of this entity's attributes.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleHit {
    pub rule: Uuid,
    pub subject: Uuid,
    /// The object of a relation conclusion; other conclusions have no entity object
    pub object: Option<Uuid>,
    pub premises: Vec<Uuid>,
    pub from: Option<i64>,
    pub to: Option<i64>,
    /// Each combination has its own scalar result and interval. A conversion
    /// can also produce text or an ISO calendar date; constant conclusions use None.
    pub value: Option<Scalar>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleReport {
    /// Number of rules taking part in evaluation
    pub rules: usize,
    /// Number of hits (after deduplication)
    pub hits: usize,
    /// Number of (rule, entity) pairs not fully expanded because the combination count exceeded the cap. **Must be reported**:
    /// inferring a few fewer and "this entity does not satisfy it" look identical in the result
    pub capped: usize,
}

/// How many premise combinations a single (rule, entity) pair may expand to at most.
///
/// The combination count is the product of the sizes of each condition's hit set: one well's same reading reported three times across three conditions
/// is 27 combinations. In real data each item usually has only one or two readings, so this cap only comes into play when "the same attribute is repeatedly
/// covered dozens of times"; and in those cases the extra intervals computed have long lost their reading value.
const MAX_COMBOS: usize = 64;

/// Evaluation.
///
/// `facts` are attribute facts **already restricted to this rule's main class**—choosing which entities take part is
/// the job of the data-fetching side (subclass expansion consults the ontology); this layer only judges.
///
/// Semantics: each condition picks out the set of facts that satisfy it, then over the combinations of "one from each condition"
/// the interval intersection is taken; a non-empty intersection is one hit. **So the same rule can produce, on the same entity,
/// several intervals**—a 2023 reading hits and a 2025 one does not, giving two spans each holding on its own
/// conclusions, rather than one row flipping back and forth (0021 decision 4).
///
/// **AND within a group, OR between groups** (0029): the paragraph above is the algorithm for one group; the whole rule is that run group by group
/// several times. The cap is applied per group—one group failing to expand must not drag down another; when two groups infer the same interval, deduplicate by interval
/// and keep the first-arriving group as the proof (multiple proof paths make no difference to the reader; storing them all would only make
/// the proof tree grow with the number of paths).
pub fn evaluate(
    rules: &[BusinessRule],
    facts: &[AttrFact],
    spans: &HashMap<Uuid, (Option<i64>, Option<i64>)>,
    edges: &[RuleEdge],
) -> (Vec<RuleHit>, RuleReport) {
    evaluate_with_pool(rules, facts, facts, spans, edges)
}

/// Evaluate rules with the facts available to a joined `Y` supplied separately.
///
/// `facts` stays scoped to the conclusion's subject type. A one-hop join can
/// reach another entity type, so its conditions read from `pool_facts`.
/// A joined X may have no attribute facts, so callers must also scope `edges`
/// by their subjects or add an X-side scope condition.
pub fn evaluate_with_pool(
    rules: &[BusinessRule],
    facts: &[AttrFact],
    pool_facts: &[AttrFact],
    spans: &HashMap<Uuid, (Option<i64>, Option<i64>)>,
    edges: &[RuleEdge],
) -> (Vec<RuleHit>, RuleReport) {
    let mut report = RuleReport {
        rules: rules.len(),
        ..Default::default()
    };
    let mut hits: Vec<RuleHit> = Vec::new();

    for rule in rules {
        if rule.conditions.is_empty() {
            continue;
        }
        if let Some(join_predicate) = rule.join_predicate {
            joined_evaluate(
                rule,
                join_predicate,
                facts,
                pool_facts,
                spans,
                edges,
                &mut hits,
                &mut report,
            );
            continue;
        }

        // Group by entity: this rule still concerns one entity's own attributes
        let mut by_subject: HashMap<Uuid, Vec<&AttrFact>> = HashMap::new();
        for f in facts {
            by_subject.entry(f.subject).or_default().push(f);
        }
        // Split by group, keeping group order stable: when two groups infer the same interval, what is kept is **the proof of the group that comes first**
        // rather than a random one decided by HashMap order
        let groups = group_conditions(&rule.conditions);
        for (subject, own) in &by_subject {
            // On the same entity, several groups of one rule may infer the same span. Deduplication crosses groups,
            // because they land as the same derived fact row
            // The deduplication key includes the computed value (0032): two different values on the same interval are two rows
            let mut seen: Vec<(Option<i64>, Option<i64>, Option<Scalar>)> = Vec::new();
            // Whether any group of this rule was left unexpanded on this entity because of too many combinations. **Recorded per (rule, entity)
            // once**: what is reported is "something was inferred less here", not "some groups were missing"
            let mut capped_here = false;
            for group in &groups {
                // ---- Slots: the places where this group each picks one reading.
                //
                // The first group.len() slots are the conditions' own predicates (**one slot per condition**, the same as
                // before 0032: two conditions written on the same attribute still each pick their own reading);
                // The rest are predicates read in the formula that the conditions do not cover (0032). When the formula refers to a
                // predicate, it binds to the **first** slot with the same predicate: in `thc > thc * 0.5` both sides mean
                // the same reading, not two readings of the same attribute
                let mut slots: Vec<Uuid> = group.iter().map(|c| c.predicate).collect();
                let mut extra: Vec<Uuid> = Vec::new();
                for c in group {
                    if let Operand::Calc(e) = &c.operand {
                        e.predicates(&mut extra);
                    }
                }
                if let Conclusion::Computed { expr, .. } = &rule.conclusion {
                    expr.predicates(&mut extra);
                }
                for p in extra {
                    if !slots.contains(&p) {
                        slots.push(p);
                    }
                }

                // Candidate readings for each slot. If any one is empty, this group does not hold on this entity
                let mut per_slot: Vec<Vec<Uuid>> = Vec::with_capacity(slots.len());
                let mut satisfiable = true;
                for p in &slots {
                    let matched: Vec<Uuid> = own
                        .iter()
                        .filter(|f| f.predicate == *p)
                        .map(|f| f.id)
                        .collect();
                    if matched.is_empty() {
                        satisfiable = false;
                        break;
                    }
                    per_slot.push(matched);
                }
                if !satisfiable {
                    continue;
                }

                // The cap is per group: one group failing to expand should not block the other either
                if combinations_exceed_cap(&per_slot) {
                    capped_here = true;
                    continue;
                }

                let by_id: HashMap<Uuid, &AttrFact> = own.iter().map(|f| (f.id, *f)).collect();

                // Cartesian product. **Conditions can only be judged once the combination is fixed** (0032): a computed
                // threshold reads the readings selected in this round, so the judgement no longer looks at just one fact.
                // The same interval may come from several combinations (the same reading reported twice); deduplicate by interval
                for combo in cartesian(&per_slot) {
                    // Slot → the reading selected this round. Both the formula and the conditions read through it
                    let mut bound: HashMap<Uuid, &AttrFact> = HashMap::new();
                    for (i, id) in combo.iter().enumerate() {
                        let Some(f) = by_id.get(id) else { continue };
                        bound.entry(slots[i]).or_insert(f);
                    }
                    let holds = group.iter().enumerate().all(|(i, c)| {
                        combo
                            .get(i)
                            .and_then(|id| by_id.get(id))
                            .is_some_and(|f| satisfies(c, &f.value, &bound))
                    });
                    if !holds {
                        continue;
                    }
                    // Missing readings and failed conversions do not manufacture
                    // a value. The same premises and interval cover text results.
                    let value = match &rule.conclusion {
                        Conclusion::Computed { expr, .. } => match expr.compute(&bound) {
                            Some(v) => Some(v),
                            None => continue,
                        },
                        _ => None,
                    };
                    let Some((from, to)) = validity(&combo, spans) else {
                        continue;
                    };
                    // Deduplication is by (interval, value): a computed conclusion can have different values on the same interval,
                    // which makes two rows, not one
                    let key = (from, to, value.clone());
                    // Keep the old numeric bit key while admitting text results.
                    if seen.iter().any(|(old_from, old_to, old_value)| {
                        *old_from == from
                            && *old_to == to
                            && match (old_value, &value) {
                                (Some(Scalar::Number(a)), Some(Scalar::Number(b))) => {
                                    a.to_bits() == b.to_bits()
                                }
                                _ => old_value == &value,
                            }
                    }) {
                        continue;
                    }
                    seen.push(key);
                    hits.push(RuleHit {
                        rule: rule.id,
                        subject: *subject,
                        object: None,
                        premises: combo,
                        from,
                        to,
                        value,
                    });
                }
            }
            if capped_here {
                report.capped += 1;
            }
        }
    }
    report.hits = hits.len();
    (hits, report)
}

/// Evaluate a rule spanning two entities (0047).
///
/// **A join edge is a pair `(X, Y)`, not a Cartesian join.** The ADR allows only one hop, and
/// makes each edge a premise in its own right: two same-predicate edges, even with identical ends, may each hold for a span of their own.
/// The cap is therefore reported by `(rule, X, Y)`, keeping the same meaning as `(rule, X)` for single-entity rules.
#[allow(clippy::too_many_arguments)]
fn joined_evaluate(
    rule: &BusinessRule,
    join_predicate: Uuid,
    facts: &[AttrFact],
    pool_facts: &[AttrFact],
    spans: &HashMap<Uuid, (Option<i64>, Option<i64>)>,
    edges: &[RuleEdge],
    hits: &mut Vec<RuleHit>,
    report: &mut RuleReport,
) {
    // X is already scoped to the rule's subject type. The join, however, is
    // the way the rule reaches a differently typed Y.
    let mut x_by_subject: HashMap<Uuid, Vec<&AttrFact>> = HashMap::new();
    for f in facts {
        x_by_subject.entry(f.subject).or_default().push(f);
    }
    let mut pool_by_subject: HashMap<Uuid, Vec<&AttrFact>> = HashMap::new();
    for f in pool_facts {
        pool_by_subject.entry(f.subject).or_default().push(f);
    }

    // Join edges are bucketed by X and scanned once: searching all edges for each X would cost (number of X) × (number of edges)—
    // measured at 5.9 s for 100k pairs versus 82 ms for 10k pairs; after bucketing it is linear in the number of pairs
    let mut edges_by_x: HashMap<Uuid, Vec<&RuleEdge>> = HashMap::new();
    for e in edges.iter().filter(|e| e.predicate == join_predicate) {
        edges_by_x.entry(e.subject).or_default().push(e);
    }
    let mut x_subjects: Vec<Uuid> = edges_by_x.keys().copied().collect();
    x_subjects.sort_unstable();
    let groups = group_conditions(&rule.conditions);

    for x in x_subjects {
        let x_facts = x_by_subject.get(&x).map(Vec::as_slice).unwrap_or_default();
        for edge in &edges_by_x[&x] {
            let y = edge.object;
            let y_facts = pool_by_subject
                .get(&y)
                .map(Vec::as_slice)
                .unwrap_or_default();
            // Several groups on the same pair may infer the same conclusion. The first-arriving group is kept as the proof, consistent with the single-entity
            // rule's deduplication rule
            let mut seen: Vec<(Option<i64>, Option<i64>, Option<u64>)> = Vec::new();
            let mut capped_here = false;
            for group in &groups {
                let mut slots: Vec<(Side, Uuid)> =
                    group.iter().map(|c| (c.side, c.predicate)).collect();
                let mut extra: Vec<(Side, Uuid)> = Vec::new();
                for c in group {
                    if let Operand::Calc(e) = &c.operand {
                        expr_predicates(e, c.side, &mut extra);
                    }
                }
                if let Conclusion::Computed { expr, .. } = &rule.conclusion {
                    expr_predicates(expr, Side::X, &mut extra);
                }
                for slot in extra {
                    if !slots.contains(&slot) {
                        slots.push(slot);
                    }
                }

                let mut per_slot: Vec<Vec<Uuid>> = Vec::with_capacity(slots.len());
                let mut satisfiable = true;
                for (side, predicate) in &slots {
                    let side_facts = match side {
                        Side::X => x_facts,
                        Side::Y => y_facts,
                    };
                    let matched: Vec<Uuid> = side_facts
                        .iter()
                        .filter(|f| f.predicate == *predicate)
                        .map(|f| f.id)
                        .collect();
                    if matched.is_empty() {
                        satisfiable = false;
                        break;
                    }
                    per_slot.push(matched);
                }
                if !satisfiable {
                    continue;
                }
                if combinations_exceed_cap(&per_slot) {
                    capped_here = true;
                    continue;
                }

                let x_by_id: HashMap<Uuid, &AttrFact> =
                    x_facts.iter().map(|f| (f.id, *f)).collect();
                let y_by_id: HashMap<Uuid, &AttrFact> =
                    y_facts.iter().map(|f| (f.id, *f)).collect();
                for combo in cartesian(&per_slot) {
                    let mut x_bound: HashMap<Uuid, &AttrFact> = HashMap::new();
                    let mut y_bound: HashMap<Uuid, &AttrFact> = HashMap::new();
                    for ((side, predicate), id) in slots.iter().zip(combo.iter()) {
                        let bound = match side {
                            Side::X => &mut x_bound,
                            Side::Y => &mut y_bound,
                        };
                        let facts_by_id = match side {
                            Side::X => &x_by_id,
                            Side::Y => &y_by_id,
                        };
                        if let Some(f) = facts_by_id.get(id) {
                            bound.entry(*predicate).or_insert(*f);
                        }
                    }
                    let holds = group.iter().enumerate().all(|(i, c)| {
                        let bound = match c.side {
                            Side::X => &x_bound,
                            Side::Y => &y_bound,
                        };
                        combo
                            .get(i)
                            .and_then(|id| match c.side {
                                Side::X => x_by_id.get(id),
                                Side::Y => y_by_id.get(id),
                            })
                            .is_some_and(|f| satisfies(c, &f.value, bound))
                    });
                    if !holds {
                        continue;
                    }
                    // Relation is the only joined conclusion. Store validation
                    // keeps computed conclusions on the old single-entity path,
                    // so this branch need not invent a value for an edge.
                    if !matches!(rule.conclusion, Conclusion::Relation { .. }) {
                        continue;
                    }
                    let mut premises = combo;
                    premises.push(edge.id);
                    let Some((from, to)) = validity(&premises, spans) else {
                        continue;
                    };
                    let key = (from, to, None);
                    if seen.contains(&key) {
                        continue;
                    }
                    seen.push(key);
                    hits.push(RuleHit {
                        rule: rule.id,
                        subject: x,
                        object: Some(y),
                        premises,
                        from,
                        to,
                        value: None,
                    });
                }
            }
            if capped_here {
                report.capped += 1;
            }
        }
    }
}

fn expr_predicates(expr: &Expr, side: Side, out: &mut Vec<(Side, Uuid)>) {
    let mut predicates = Vec::new();
    expr.predicates(&mut predicates);
    for predicate in predicates {
        let slot = (side, predicate);
        if !out.contains(&slot) {
            out.push(slot);
        }
    }
}

/// Split into groups by `group`, **group order ascending by group_seq**—when two groups infer the same interval,
/// the proof kept must be the stable one, and must not vary with storage order.
fn group_conditions(conditions: &[Condition]) -> Vec<Vec<&Condition>> {
    let mut keys: Vec<i32> = conditions.iter().map(|c| c.group).collect();
    keys.sort_unstable();
    keys.dedup();
    keys.into_iter()
        .map(|g| conditions.iter().filter(|c| c.group == g).collect())
        .collect()
}

/// The caller has already excluded empty slots; if any prefix exceeds the cap, the full combination necessarily exceeds it too.
fn combinations_exceed_cap(per_slot: &[Vec<Uuid>]) -> bool {
    let mut combinations = 1_usize;
    for candidates in per_slot {
        let Some(next) = combinations.checked_mul(candidates.len()) else {
            return true;
        };
        if next > MAX_COMBOS {
            return true;
        }
        combinations = next;
    }
    false
}

/// Take one from each condition and enumerate combinations exhaustively. The caller has already blocked the cap outside.
fn cartesian(sets: &[Vec<Uuid>]) -> Vec<Vec<Uuid>> {
    let mut out: Vec<Vec<Uuid>> = vec![Vec::new()];
    for set in sets {
        let mut next = Vec::with_capacity(out.len() * set.len());
        for prefix in &out {
            for id in set {
                let mut row = prefix.clone();
                row.push(*id);
                next.push(row);
            }
        }
        out = next;
    }
    out
}

/// Whether an attribute value satisfies a condition.
///
/// **A wrong type means not satisfied, not an error.** An attribute that should be a number was extracted as
/// "twelve point three", and this rule does not hold on this entity—rather than failing the whole materialization round.
fn satisfies(c: &Condition, value: &serde_json::Value, bound: &HashMap<Uuid, &AttrFact>) -> bool {
    // A computed threshold: first compute it from the readings selected this round; if it cannot be computed, the condition is not satisfied (0032)
    if let Operand::Calc(e) = &c.operand {
        let Some(n) = e.eval(bound) else { return false };
        let Some(v) = num(value) else { return false };
        return match c.op {
            Op::Gt => v > n,
            Op::Gte => v >= n,
            Op::Lt => v < n,
            Op::Lte => v <= n,
            // The formula yields a number: intervals, sets and recorded-ness are not positions it can fill
            _ => false,
        };
    }
    match (&c.op, &c.operand) {
        (Op::Present, _) => !value.is_null(),
        // A value not in the set counts as satisfied; **no value does not count**—that is "not recorded", not "not it"
        (Op::NotIn, Operand::Set(set)) => !value.is_null() && in_set(value, set) == Some(false),
        (Op::Gt, Operand::Num(n)) => num(value).is_some_and(|v| v > *n),
        (Op::Gte, Operand::Num(n)) => num(value).is_some_and(|v| v >= *n),
        (Op::Lt, Operand::Num(n)) => num(value).is_some_and(|v| v < *n),
        (Op::Lte, Operand::Num(n)) => num(value).is_some_and(|v| v <= *n),
        (Op::Between, Operand::Range(lo, hi)) => num(value).is_some_and(|v| v >= *lo && v <= *hi),
        (Op::In, Operand::Set(set)) => in_set(value, set) == Some(true),
        // op and the operand shape do not match: the library's CHECK blocks most of these, and the rest count as not satisfied
        _ => false,
    }
}

/// numbers. **Numbers inside strings also count**—extraction often stores "12.3" as a string,
/// and a threshold rule silently failing because of the quotes is the hardest kind of failure to spot.
fn num(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Set membership (0029). When both the reading and an item in the set can be read as numbers, compare as numbers: numeric attributes are stored as f64,
/// read back as `3.0`, while the set writes `3`; a set written as `[3.0]` stores "3.0". Comparing as text
/// always fails on one side. Anything else is still compared literally. When a reading has no literal at all, it is `None`
fn in_set(value: &serde_json::Value, set: &[String]) -> Option<bool> {
    let v = text(value)?;
    let n = match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|n| n.is_finite());
    Some(
        set.iter()
            .any(|x| x == &v || n.is_some_and(|n| x.trim().parse::<f64>().is_ok_and(|x| x == n))),
    )
}

/// Category. Numbers and booleans are also converted to their literal forms for set comparison, for the same reason.
fn text(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod joined_tests {
    use super::*;
    use serde_json::json;

    fn id(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn fact(fid: u8, subject: u8, pred: u8, value: serde_json::Value) -> AttrFact {
        AttrFact {
            id: id(fid),
            subject: id(subject),
            predicate: id(pred),
            value,
        }
    }

    /// A joined rule sees a second entity across exactly one declared edge.
    /// Both sides' readings, and the edge itself, are premises.
    #[test]
    fn a_rule_joins_one_entity_and_concludes_a_relation() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: Some(id(1)),
            conclusion: Conclusion::Relation { predicate: id(2) },
            conditions: vec![
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(10),
                    op: Op::Gt,
                    operand: Operand::Num(50.0),
                },
                Condition {
                    group: 0,
                    side: Side::Y,
                    predicate: id(11),
                    op: Op::Gt,
                    operand: Operand::Num(50.0),
                },
            ],
        };
        let facts = vec![fact(1, 50, 10, json!(60.0)), fact(2, 51, 11, json!(70.0))];
        let edges = vec![RuleEdge {
            id: id(9),
            predicate: id(1),
            subject: id(50),
            object: id(51),
        }];
        let spans = HashMap::from([
            (id(1), (Some(100), Some(200))),
            (id(2), (Some(150), Some(300))),
            (id(9), (Some(120), None)),
        ]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &edges);

        assert_eq!(hits.len(), 1, "one edge yields one joined conclusion");
        assert_eq!(hits[0].subject, id(50));
        assert_eq!(hits[0].object, Some(id(51)));
        assert_eq!((hits[0].from, hits[0].to), (Some(150), Some(200)));
        assert!(hits[0].premises.contains(&id(1)));
        assert!(hits[0].premises.contains(&id(2)));
        assert!(hits[0].premises.contains(&id(9)));
    }

    /// A condition on Y cannot manufacture the edge that binds it to X.
    #[test]
    fn a_joined_condition_without_the_edge_fires_nothing() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: Some(id(1)),
            conclusion: Conclusion::Relation { predicate: id(2) },
            conditions: vec![Condition {
                group: 0,
                side: Side::Y,
                predicate: id(11),
                op: Op::Gt,
                operand: Operand::Num(50.0),
            }],
        };
        let facts = vec![fact(2, 51, 11, json!(70.0))];
        let spans = HashMap::from([(id(2), (Some(100), Some(200)))]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert!(hits.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn fact(fid: u8, subject: u8, pred: u8, value: serde_json::Value) -> AttrFact {
        AttrFact {
            id: id(fid),
            subject: id(subject),
            predicate: id(pred),
            value,
        }
    }

    /// Total hydrocarbon 12.3 and interpreted as gas-logging anomaly → hit, premises are exactly those two readings
    #[test]
    fn a_conjunction_fires_and_names_the_two_readings() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing {
                class: "GasBearingWell".into(),
            },
            conditions: vec![
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(10),
                    op: Op::Gt,
                    operand: Operand::Num(8.0),
                },
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(11),
                    op: Op::In,
                    operand: Operand::Set(vec!["气测异常".into(), "气测异常后效".into()]),
                },
            ],
        };
        let facts = vec![
            fact(1, 50, 10, json!(12.3)),
            fact(2, 50, 11, json!("气测异常")),
        ];
        let spans = HashMap::from([(id(1), (Some(100), None)), (id(2), (Some(100), None))]);
        let (hits, report) = evaluate(&[rule], &facts, &spans, &[]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject, id(50));
        assert_eq!(hits[0].premises, vec![id(1), id(2)]);
        assert_eq!(hits[0].from, Some(100));
        assert_eq!(report.hits, 1);
    }

    /// Missing one item in a conjunction means it does not hold. **This is the easiest place to get backwards**: if any condition has no hit,
    /// the whole rule does not hold on this entity, rather than being "computed from the remaining conditions"
    #[test]
    fn a_missing_condition_fires_nothing() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing {
                class: "GasBearingWell".into(),
            },
            conditions: vec![
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(10),
                    op: Op::Gt,
                    operand: Operand::Num(8.0),
                },
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(11),
                    op: Op::Present,
                    operand: Operand::None,
                },
            ],
        };
        let facts = vec![fact(1, 50, 10, json!(12.3))];
        let spans = HashMap::from([(id(1), (Some(100), None))]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert!(hits.is_empty(), "第二个条件没有任何事实，不该命中");
    }

    /// Two readings of the same attribute each hold → two intervals, not one row flipped (0021 decision 4)
    #[test]
    fn two_readings_give_two_intervals() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing {
                class: "GasBearingWell".into(),
            },
            conditions: vec![Condition {
                group: 0,
                side: Side::X,
                predicate: id(10),
                op: Op::Gt,
                operand: Operand::Num(8.0),
            }],
        };
        // 2023 and 2025 readings both pass the threshold, and the intervals do not intersect
        let facts = vec![fact(1, 50, 10, json!(12.3)), fact(2, 50, 10, json!(9.9))];
        let spans = HashMap::from([
            (id(1), (Some(100), Some(200))),
            (id(2), (Some(300), Some(400))),
        ]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert_eq!(hits.len(), 2, "两次读数各自成立");
        let mut spans_out: Vec<_> = hits.iter().map(|h| (h.from, h.to)).collect();
        spans_out.sort();
        assert_eq!(
            spans_out,
            vec![(Some(100), Some(200)), (Some(300), Some(400))]
        );
    }

    /// Two premises with non-intersecting intervals cannot make up one hit: the total hydrocarbon is from 2023, the interpretation is from 2025,
    /// and the two were never true at the same time
    #[test]
    fn premises_that_never_overlapped_fire_nothing() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing {
                class: "GasBearingWell".into(),
            },
            conditions: vec![
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(10),
                    op: Op::Gt,
                    operand: Operand::Num(8.0),
                },
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(11),
                    op: Op::In,
                    operand: Operand::Set(vec!["气测异常".into()]),
                },
            ],
        };
        let facts = vec![
            fact(1, 50, 10, json!(12.3)),
            fact(2, 50, 11, json!("气测异常")),
        ];
        let spans = HashMap::from([
            (id(1), (Some(100), Some(200))),
            (id(2), (Some(300), Some(400))),
        ]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert!(hits.is_empty(), "两条前提没有同时成立的时段");
    }

    /// Two groups joined by OR: a hit if either holds. The second group's premises do **not** include the first group's reading—
    /// a derived fact carries only the ones that actually made it hold, not all this entity's attributes
    #[test]
    fn either_group_can_fire_and_carries_only_its_own_premises() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing {
                class: "GasBearingWell".into(),
            },
            conditions: vec![
                // Group 0: total hydrocarbon > 8 and interpretation ∈ {gas-logging anomaly}
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(10),
                    op: Op::Gt,
                    operand: Operand::Num(8.0),
                },
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(11),
                    op: Op::In,
                    operand: Operand::Set(vec!["气测异常".into()]),
                },
                // Group 1: comprehensive interpretation ∈ {gas layer}
                Condition {
                    side: Side::X,
                    group: 1,
                    predicate: id(12),
                    op: Op::In,
                    operand: Operand::Set(vec!["气层".into()]),
                },
            ],
        };
        // Only the second group's reading
        let facts = vec![fact(3, 50, 12, json!("气层"))];
        let spans = HashMap::from([(id(3), (Some(100), Some(200)))]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert_eq!(hits.len(), 1, "第二组独自成立");
        assert_eq!(hits[0].premises, vec![id(3)]);
        assert_eq!((hits[0].from, hits[0].to), (Some(100), Some(200)));
    }

    /// Two groups hold at once and infer the same span: **one hit, not two**—they land as
    /// the same derived fact row, and the proof kept is the one from the group that comes first
    #[test]
    fn two_groups_on_the_same_interval_are_one_hit() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing {
                class: "GasBearingWell".into(),
            },
            conditions: vec![
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(10),
                    op: Op::Gt,
                    operand: Operand::Num(8.0),
                },
                Condition {
                    side: Side::X,
                    group: 1,
                    predicate: id(11),
                    op: Op::In,
                    operand: Operand::Set(vec!["气层".into()]),
                },
            ],
        };
        let facts = vec![fact(1, 50, 10, json!(12.3)), fact(2, 50, 11, json!("气层"))];
        // The same span
        let spans = HashMap::from([
            (id(1), (Some(100), Some(200))),
            (id(2), (Some(100), Some(200))),
        ]);
        let (hits, report) = evaluate(&[rule], &facts, &spans, &[]);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].premises,
            vec![id(1)],
            "留下的是组序在前那一组的证明"
        );
        assert_eq!(report.hits, 1);
    }

    /// One group failing to expand must not drag down another: **the cap is per group**, and this (rule, entity) pair
    /// is reported as "not fully expanded" only once
    #[test]
    fn a_capped_group_does_not_stop_the_other() {
        let mut facts: Vec<AttrFact> = Vec::new();
        let mut spans = HashMap::new();
        // Group 0: 100 readings on the same attribute, combination count 100 > 64
        for i in 1..=100u8 {
            facts.push(fact(i, 50, 10, json!(12.3)));
            spans.insert(id(i), (Some(100), Some(200)));
        }
        // Group 1: one is enough
        facts.push(fact(200, 50, 11, json!("气层")));
        spans.insert(id(200), (Some(100), Some(200)));
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing {
                class: "GasBearingWell".into(),
            },
            conditions: vec![
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(10),
                    op: Op::Gt,
                    operand: Operand::Num(8.0),
                },
                Condition {
                    side: Side::X,
                    group: 1,
                    predicate: id(11),
                    op: Op::In,
                    operand: Operand::Set(vec!["气层".into()]),
                },
            ],
        };
        let (hits, report) = evaluate(&[rule], &facts, &spans, &[]);
        assert_eq!(hits.len(), 1, "第二组照样出结论");
        assert_eq!(hits[0].premises, vec![id(200)]);
        assert_eq!(report.capped, 1, "(规则, 实体) 只报一次");
    }

    /// `is not one of`: a value outside the set counts as satisfied; **no such reading does not count**—that is "not recorded",
    /// not "not it"
    #[test]
    fn not_one_of_needs_a_reading_to_be_true() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing {
                class: "NonGas".into(),
            },
            conditions: vec![Condition {
                group: 0,
                side: Side::X,
                predicate: id(11),
                op: Op::NotIn,
                operand: Operand::Set(vec!["气层".into()]),
            }],
        };
        let spans = HashMap::from([(id(1), (None, None))]);
        let (hit, _) = evaluate(
            std::slice::from_ref(&rule),
            &[fact(1, 50, 11, json!("水层"))],
            &spans,
            &[],
        );
        assert_eq!(hit.len(), 1, "读数在集合外");
        let (miss, _) = evaluate(
            std::slice::from_ref(&rule),
            &[fact(1, 50, 11, json!("气层"))],
            &spans,
            &[],
        );
        assert!(miss.is_empty(), "读数在集合里");
        let (none, _) = evaluate(&[rule], &[], &HashMap::new(), &[]);
        assert!(none.is_empty(), "没有这条读数：不成立，而不是「不是它」");
    }

    /// Numbers extracted as strings are still comparable. **This one guards against the hardest failure to spot**:
    /// the rule never errors, it just never hits
    #[test]
    fn a_number_in_quotes_still_compares() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Attribute {
                predicate: id(20),
                value: json!("good"),
            },
            conditions: vec![Condition {
                group: 0,
                side: Side::X,
                predicate: id(10),
                op: Op::Gte,
                operand: Operand::Num(12.0),
            }],
        };
        let facts = vec![fact(1, 50, 10, json!(" 12.3 "))];
        let spans = HashMap::from([(id(1), (None, None))]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert_eq!(hits.len(), 1);
    }

    /// Readings of numeric attributes are stored as f64: `3` stored comes back as `3.0`, and "belongs to {3}" must still hit
    #[test]
    fn a_stored_number_is_one_of_the_numbers_in_the_set() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing {
                class: "Grade3".into(),
            },
            conditions: vec![Condition {
                group: 0,
                side: Side::X,
                predicate: id(10),
                op: Op::In,
                operand: Operand::Set(vec!["3".into()]),
            }],
        };
        let facts = vec![fact(1, 50, 10, json!(3.0))];
        let spans = HashMap::from([(id(1), (None, None))]);
        let (hits, _) = evaluate(std::slice::from_ref(&rule), &facts, &spans, &[]);
        assert_eq!(hits.len(), 1);
        // `parse_operand` stores `[3.0]` as "3.0": the reading 3.0 is still in it, and the integer 3 is too
        let mut rule = rule;
        rule.conditions[0].operand = Operand::Set(vec!["3.0".into()]);
        let facts = vec![fact(1, 50, 10, json!(3.0))];
        let (hits, _) = evaluate(std::slice::from_ref(&rule), &facts, &spans, &[]);
        assert_eq!(hits.len(), 1);
        let facts = vec![fact(1, 50, 10, json!(3))];
        let (hits, _) = evaluate(std::slice::from_ref(&rule), &facts, &spans, &[]);
        assert_eq!(hits.len(), 1);
        // Anything that is not a number is still compared literally
        rule.conditions[0].operand = Operand::Set(vec!["3".into()]);
        let facts = vec![fact(1, 50, 10, json!("三"))];
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert!(hits.is_empty());
    }

    /// Raise the threshold above the reading and the same data stops hitting—rerunning with lowered/raised thresholds is an acceptance item of 0021
    #[test]
    fn raising_the_threshold_clears_the_hit() {
        let facts = vec![fact(1, 50, 10, json!(12.3))];
        let spans = HashMap::from([(id(1), (None, None))]);
        let mk = |threshold: f64| BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing {
                class: "GasBearingWell".into(),
            },
            conditions: vec![Condition {
                group: 0,
                side: Side::X,
                predicate: id(10),
                op: Op::Gt,
                operand: Operand::Num(threshold),
            }],
        };
        assert_eq!(evaluate(&[mk(8.0)], &facts, &spans, &[]).0.len(), 1);
        assert!(evaluate(&[mk(20.0)], &facts, &spans, &[]).0.is_empty());
    }

    /// A rule with no conditions infers nothing. An empty conjunction is logically always true, and would sweep in the whole class—
    /// that is the worst way for "the rule isn't finished yet" to fail
    #[test]
    fn a_rule_without_conditions_concludes_nothing() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing { class: "X".into() },
            conditions: vec![],
        };
        let facts = vec![fact(1, 50, 10, json!(12.3))];
        let spans = HashMap::from([(id(1), (None, None))]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert!(hits.is_empty());
    }

    /// When the combination count exceeds the cap, **report it**, rather than silently inferring less
    #[test]
    fn too_many_combinations_are_reported() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing { class: "X".into() },
            conditions: vec![
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(10),
                    op: Op::Present,
                    operand: Operand::None,
                },
                Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(11),
                    op: Op::Present,
                    operand: Operand::None,
                },
            ],
        };
        let mut facts = Vec::new();
        let mut spans = HashMap::new();
        for i in 0..10u8 {
            facts.push(fact(i, 50, 10, json!(1)));
            facts.push(fact(i + 100, 50, 11, json!(1)));
            spans.insert(id(i), (None, None));
            spans.insert(id(i + 100), (None, None));
        }
        let (hits, report) = evaluate(&[rule], &facts, &spans, &[]);
        assert_eq!(report.capped, 1, "100 种组合超过上限，要计数");
        assert!(hits.is_empty());
    }

    #[test]
    fn combination_limits_apply_before_overflow_in_both_rule_paths() {
        let facts = vec![
            fact(1, 50, 10, json!(120.0)),
            fact(2, 50, 10, json!(130.0)),
            fact(3, 50, 20, json!(true)),
        ];
        let spans = HashMap::from([
            (id(1), (Some(100), None)),
            (id(2), (Some(100), None)),
            (id(3), (Some(100), None)),
            (id(4), (Some(100), None)),
        ]);
        for joined in [false, true] {
            let edges = if joined {
                vec![RuleEdge {
                    id: id(4),
                    predicate: id(11),
                    subject: id(50),
                    object: id(51),
                }]
            } else {
                Vec::new()
            };
            // Each condition has two candidates: 6 conditions gives 64 combinations, 7 gives 128, and usize::BITS conditions would overflow.
            for (condition_count, has_other_group, expected_capped, expected_hits) in [
                (6, false, 0, 1),
                (7, false, 1, 0),
                (usize::BITS as usize, false, 1, 0),
                (usize::BITS as usize, true, 1, 1),
            ] {
                let presence_condition = Condition {
                    group: 0,
                    side: Side::X,
                    predicate: id(10),
                    op: Op::Present,
                    operand: Operand::None,
                };
                let mut conditions = vec![presence_condition.clone(); condition_count];
                if has_other_group {
                    conditions.push(Condition {
                        group: 1,
                        predicate: id(20),
                        ..presence_condition
                    });
                }
                let rule = BusinessRule {
                    id: id(90),
                    join_predicate: joined.then_some(id(11)),
                    conclusion: if joined {
                        Conclusion::Relation { predicate: id(12) }
                    } else {
                        Conclusion::Typing { class: "X".into() }
                    },
                    conditions,
                };
                let (hits, report) = evaluate(&[rule], &facts, &spans, &edges);
                let case = format!(
                    "joined={joined}, conditions={condition_count}, other_group={has_other_group}"
                );
                assert_eq!(report.capped, expected_capped, "{case}");
                assert_eq!(report.hits, expected_hits, "{case}");
                assert_eq!(hits.len(), expected_hits, "{case}");
                if has_other_group {
                    let mut expected_premises = vec![id(3)];
                    if joined {
                        expected_premises.push(id(4));
                    }
                    assert_eq!(hits[0].premises, expected_premises, "{case}");
                }
            }
        }
    }

    fn attr(p: u8) -> Expr {
        Expr::Attr(id(p))
    }
    fn arith(op: Arith, l: Expr, r: Expr) -> Expr {
        Expr::Arith {
            op,
            l: Box::new(l),
            r: Box::new(r),
        }
    }

    /// The conclusion is computed: margin = revenue − cost. **Both readings the formula reads go into the premises**,
    /// so the conclusion can say on what basis it holds, and it also retires when either reading changes (0032)
    #[test]
    fn a_computed_conclusion_carries_the_readings_it_read() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Computed {
                predicate: id(12),
                expr: arith(Arith::Sub, attr(10), attr(11)),
            },
            conditions: vec![Condition {
                group: 0,
                side: Side::X,
                predicate: id(10),
                op: Op::Present,
                operand: Operand::None,
            }],
        };
        let facts = vec![fact(1, 50, 10, json!(300.0)), fact(2, 50, 11, json!(120.0))];
        let spans = HashMap::from([(id(1), (Some(100), None)), (id(2), (Some(100), None))]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].value, Some(Scalar::Number(180.0)));
        // The condition only mentions revenue, but cost is also read—it is still a premise
        assert_eq!(hits[0].premises.len(), 2);
        assert!(hits[0].premises.contains(&id(1)) && hits[0].premises.contains(&id(2)));
    }

    /// **Each combination is computed separately.** Two revenue readings and one cost reading make two numbers, two intervals,
    /// two conclusion rows—rather than picking one and computing once (0032's "row count is the real cost")
    #[test]
    fn each_combination_of_readings_computes_its_own_value() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Computed {
                predicate: id(12),
                expr: arith(Arith::Sub, attr(10), attr(11)),
            },
            conditions: vec![Condition {
                group: 0,
                side: Side::X,
                predicate: id(10),
                op: Op::Present,
                operand: Operand::None,
            }],
        };
        let facts = vec![
            fact(1, 50, 10, json!(300.0)),
            fact(2, 50, 10, json!(400.0)),
            fact(3, 50, 11, json!(120.0)),
        ];
        let spans = HashMap::from([
            (id(1), (Some(100), Some(200))),
            (id(2), (Some(200), Some(300))),
            (id(3), (Some(100), Some(300))),
        ]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert_eq!(hits.len(), 2, "两条 revenue 各算一个 margin");
        let mut values: Vec<f64> = hits
            .iter()
            .filter_map(|h| h.value.as_ref()?.number())
            .collect();
        values.sort_by(f64::total_cmp);
        assert_eq!(values, vec![180.0, 280.0]);
    }

    /// A missing reading means no conclusion—**it is not treated as zero**. Not recorded is not zero; filling in a definite answer where there is ignorance
    /// is exactly what this ledger should least do
    #[test]
    fn a_missing_reading_computes_nothing_rather_than_zero() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Computed {
                predicate: id(12),
                expr: arith(Arith::Sub, attr(10), attr(11)),
            },
            conditions: vec![Condition {
                group: 0,
                side: Side::X,
                predicate: id(10),
                op: Op::Present,
                operand: Operand::None,
            }],
        };
        let facts = vec![fact(1, 50, 10, json!(300.0))];
        let spans = HashMap::from([(id(1), (Some(100), None))]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert!(hits.is_empty(), "cost 没记，margin 就不该有");
    }

    /// Division by zero likewise yields nothing. A conclusion that cannot be computed is the same as one that does not hold: there is no
    #[test]
    fn dividing_by_zero_computes_nothing() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Computed {
                predicate: id(12),
                expr: arith(Arith::Div, attr(10), attr(11)),
            },
            conditions: vec![Condition {
                group: 0,
                side: Side::X,
                predicate: id(10),
                op: Op::Present,
                operand: Operand::None,
            }],
        };
        let facts = vec![fact(1, 50, 10, json!(300.0)), fact(2, 50, 11, json!(0.0))];
        let spans = HashMap::from([(id(1), (Some(100), None)), (id(2), (Some(100), None))]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert!(hits.is_empty());
    }

    /// The threshold can also be computed: `revenue > cost × 1.5`. The judgement can only be made once the combination is fixed,
    /// because the threshold reads exactly the cost selected this round
    #[test]
    fn a_threshold_can_be_computed_from_another_reading() {
        let rule = |factor: f64| BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Typing {
                class: "Healthy".into(),
            },
            conditions: vec![Condition {
                group: 0,
                side: Side::X,
                predicate: id(10),
                op: Op::Gt,
                operand: Operand::Calc(arith(Arith::Mul, attr(11), Expr::Const(factor))),
            }],
        };
        let facts = vec![fact(1, 50, 10, json!(300.0)), fact(2, 50, 11, json!(120.0))];
        let spans = HashMap::from([(id(1), (Some(100), None)), (id(2), (Some(100), None))]);

        let (hits, _) = evaluate(&[rule(1.5)], &facts, &spans, &[]);
        assert_eq!(hits.len(), 1, "300 > 120 × 1.5");
        assert_eq!(hits[0].premises.len(), 2, "门槛读的那条也是前提");

        let (none, _) = evaluate(&[rule(3.0)], &facts, &spans, &[]);
        assert!(none.is_empty(), "300 不大于 120 × 3");
    }

    /// Two different values on the same interval are **two rows**, not one. The deduplication key includes the value, otherwise
    /// the one computed later would be dropped as a duplicate
    #[test]
    fn two_values_on_one_interval_are_two_hits() {
        let rule = BusinessRule {
            id: id(90),
            join_predicate: None,
            conclusion: Conclusion::Computed {
                predicate: id(12),
                expr: attr(10),
            },
            conditions: vec![Condition {
                group: 0,
                side: Side::X,
                predicate: id(10),
                op: Op::Present,
                operand: Operand::None,
            }],
        };
        let facts = vec![fact(1, 50, 10, json!(300.0)), fact(2, 50, 10, json!(400.0))];
        let spans = HashMap::from([(id(1), (Some(100), None)), (id(2), (Some(100), None))]);
        let (hits, _) = evaluate(&[rule], &facts, &spans, &[]);
        assert_eq!(hits.len(), 2, "同一段区间，两个值");
    }

    /// The formula's depth is computed correctly—the compile side relies on it to stop "someone writing a program here"
    #[test]
    fn depth_counts_the_deepest_branch() {
        assert_eq!(attr(10).depth(), 1);
        assert_eq!(arith(Arith::Add, attr(10), attr(11)).depth(), 2);
        assert_eq!(
            arith(Arith::Div, arith(Arith::Sub, attr(10), attr(11)), attr(10)).depth(),
            3
        );
    }
}
