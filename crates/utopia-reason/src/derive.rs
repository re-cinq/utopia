//! R1: Materialized derivation. **This layer adds facts to the graph**, so every one of its constraints is necessary.
//!
//! The dividing line from R0: R0 only points out problems, so its risk surface is zero; R1 writes facts. ADR 0002 gives this step
//! its own tier and sets three hard rules, each of which is enforced in the code below.
//!
//! **1. Rules are compiled only from ontology axioms.** There is no user-defined DSL; that would be a different product.
//! Today only `TransitiveProperty` and `SymmetricProperty` can be compiled: `inverseOf`
//! and `subPropertyOf` projections are not yet persisted, so there are none here. **Having one fewer rule is not
//! a defect; it is the same rule as "not declared, not inferred".**
//!
//! **2. Assertions take precedence over derivations, strictly.** A triple that has already been asserted is not derived again; not to
//! save lines, but so that "who said this" has exactly one answer.
//!
//! **3. A depth limit plus cycle detection, required by measurement.** The ADR measured on a real corpus: the transitive closure of `part_of`
//! grew from 185 to 828 edges and **did not converge**; the depth distribution oscillated from layer 5 onward instead of
//! decaying, which is the shape of a cycle. So here we neither infer self-loops (`A → A` is a contradiction, not knowledge, and is left to
//! R0 to report), and we also cap the number of rounds.
//!
//! There is one more ADR open question that must be answered here: **valid time is the intersection**.
//! Premise A `[2020,2023)`, premise B `[2022,∞)` → derived `[2022,2023)`. An empty intersection
//! is not inferred: when the two spans do not overlap, the chain itself does not hold at any moment.

use crate::{Axioms, Edge, Kind, MAX_DEPTH};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Upper bound on the number of facts derived for a single predicate.
///
/// **Not defensive programming; it is measured**: a 4.5x blow-up appeared on a predicate with 185 edges,
/// and the blow-up is superlinear. How many facts get cut off after capping must be **reported** (see [`Derivation::capped`])
/// — silent truncation would make "fully derived" look identical to "partially derived".
pub const MAX_DERIVED_PER_PREDICATE: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rule {
    /// `A p B` ∧ `B p C` ⟹ `A p C`
    Transitive,
    /// `A p B` ⟹ `B p A`
    Symmetric,
    /// `A p B` ∧ `p⁻¹ = q` ⟹ `B q A`. **Subject and object are swapped and the predicate is changed**—
    /// both happen together; doing only one is where this rule is most easily gotten wrong
    Inverse,
    /// `A p B` ∧ `p ⊑ q` ⟹ `A q B`. Subject and object stay put; only the predicate is lifted
    SubProperty,
    /// A relation edge derived from a business rule (0047). Not an axiom: `derive()` never produces it,
    /// it only enters the contradiction check as a **candidate**, so that when it collides with an assertion or another derivation it can be reported just like an axiom-derived one.
    /// The rule row that triggers it lives in `attribute_rules`, not in the axiom rule table
    Business,
}

impl Rule {
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::Transitive => "transitive",
            Rule::Symmetric => "symmetric",
            Rule::Inverse => "inverse",
            Rule::SubProperty => "sub_property",
            Rule::Business => "business_rule",
        }
    }
}

/// A derived fact to be persisted, together with its proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    pub predicate: Uuid,
    /// **Which predicate's declaration triggered it.**
    ///
    /// Transitive and symmetric rules do not change the predicate, so `via == predicate`; whereas `inverseOf` and
    /// `subPropertyOf` do change it: the fact inferred from `ceo_of ⊑ works_at` has predicate `works_at`,
    /// but the declaration is written on `ceo_of`.
    ///
    /// When persisting, the rule row is looked up by `via`. **This bit us once**: it used to look up by `predicate`,
    /// which was correct for the first two rules (the two are the same), but after adding the two cross-predicate rules the lookup found no rule,
    /// so `continue` silently dropped them: inferred but never stored, the hardest kind to debug.
    pub via: Uuid,
    pub subject: Uuid,
    pub object: Uuid,
    pub rule: Rule,
    /// The premises used, in derivation order. **This is one layer of the proof tree**: R2 follows it when expanding explanations,
    /// and when a premise is invalidated it is also how we know which derivations must be invalidated along with it
    pub premises: Vec<Uuid>,
}

/// The output of one derivation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Derivation {
    pub facts: Vec<Derived>,
    /// Predicates that hit the limit and were not fully derived. **Must be returned to the caller**: the UI has to be able to say
    /// "this predicate is too dense; only 20,000 facts were derived", rather than letting people think it was fully derived
    pub capped: Vec<Uuid>,
}

/// An edge taking part in derivation, carrying a validity period on top of [`Edge`].
///
/// A separate type rather than adding a field to `Edge`: cycles and self-loops don't use time, but R0's three mutually exclusive kinds need to check
/// whether two edges hold at the same time (#634), and R1 computes an intersection at every step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedEdge {
    pub edge: Edge,
    /// Half-open interval `[from, to)`. Either end may be empty = unknown / always
    pub from: Option<i64>,
    pub to: Option<i64>,
}

/// Intersection. `None` means that side is unbounded.
pub fn overlap(
    a: (Option<i64>, Option<i64>),
    b: (Option<i64>, Option<i64>),
) -> Option<(Option<i64>, Option<i64>)> {
    let from = match (a.0, b.0) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    };
    let to = match (a.1, b.1) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    };
    // Empty intersections are not inferred. When the two spans do not overlap, the chain does not hold at any moment; what would be inferred is a
    // fact that is never true, which is worse than not inferring it
    if let (Some(f), Some(t)) = (from, to) {
        if f >= t {
            return None;
        }
    }
    Some((from, to))
}

/// An edge in the adjacency list: (other end, start, end, list of supporting fact ids).
///
/// The fourth item is a **list**, not a single id: this item also ends up in derived edges, and a derivation's proof can
/// have several premises. If only the first were kept, then when transitivity chains onto a derived edge the proof would come out short, with one premise missing from the chain
/// and `validity` would intersect only the surviving ones, so the conclusion's validity would be wider than it really is.
type Hop = (Uuid, Option<i64>, Option<i64>, Vec<Uuid>);

/// Half-open interval `[from, to)`, with either end possibly empty.
type Span = (Option<i64>, Option<i64>);

/// Derivation intermediate state: how a (subject, object) pair came to be.
#[derive(Clone)]
struct Reached {
    from: Option<i64>,
    to: Option<i64>,
    premises: Vec<Uuid>,
}

/// The identity of a triple: (predicate, subject, object).
///
/// **The predicate is now part of the key; this is the most important change in this version.** Previously derivation grouped by predicate, each group self-contained,
/// because transitive and symmetric rules never change the predicate; but `inverseOf` and `subPropertyOf` inherently cross predicates:
/// `A works_at B` yields `B employs A`, which lands on a different predicate. Once grouping is applied, these two rules
/// would have nowhere to go.
type Triple = (Uuid, Uuid, Uuid);

// The same triple can have several validity spans that do not contain one another; a span fully covered by a wider one is not kept as a separate copy.

/// Run the axioms over this batch of edges to derive new ones.
///
/// **A global fixed point; no longer grouped by predicate.** The three rules chain together:
///
/// ```text
/// A ceo_of B  --(subPropertyOf)-->  A works_at B  --(inverseOf)-->  B employs A
/// ```
///
/// If each predicate were computed separately, the chain would break at the first step. So it is now semi-naive evaluation over the full set: each round takes
/// the new edges from the previous round (the frontier) and derives again; it stops when nothing new is added.
///
/// The three one-hop rules (symmetric / inverse / sub-property) and transitivity are processed in the same round, because each is an input to the others—
/// an edge produced by inversion may let some transitive chain connect, and vice versa.
pub fn derive(edges: &[TimedEdge], axioms: &HashMap<Uuid, Axioms>) -> Derivation {
    let mut out = Derivation::default();

    // Asserted temporal triples. **A derivation that collides with one yields**: asserted > derived is a hard rule;
    // the check is interval coverage, not just equality, so a timeless or wider span can also block a narrower derivation.
    let mut asserted: HashMap<Triple, Vec<Span>> = HashMap::new();
    for e in edges {
        asserted
            .entry((e.edge.predicate, e.edge.subject, e.edge.object))
            .or_default()
            .push((e.from, e.to));
    }

    // Already-derived → how it was derived. When continuing expansion, both the interval and the proof length are considered: a short proof covering the same span
    // can still take a few more steps before the depth limit, and must not be blocked by a long proof that arrived first.
    let mut reached: HashMap<Triple, Vec<(Span, Reached)>> = HashMap::new();
    // **The cap is still per predicate**: the constant's meaning has not changed (one predicate derives at most 20,000 facts),
    // and `Derivation::capped` also returns a list of predicates. If it were changed to one global number after going cross-predicate,
    // the UI could no longer answer "which predicate is too dense"
    let mut per_pred: HashMap<Uuid, usize> = HashMap::new();
    let mut capped: HashSet<Uuid> = HashSet::new();

    // Edges reachable from (predicate, subject), used for transitivity. Derived edges go in here too, because they are already
    // our assertions, and a chain should not break just because of "different provenance"
    let mut adj: HashMap<(Uuid, Uuid), Vec<Hop>> = HashMap::new();
    // A new edge may also be the right half of a chain: the left half has already left the frontier and must not wait to be expanded again.
    // Index incoming edges by object, so that a late-arriving B→C can attach to an already-seen A→B.
    let mut incoming: HashMap<(Uuid, Uuid), Vec<Hop>> = HashMap::new();
    for e in edges {
        adj.entry((e.edge.predicate, e.edge.subject))
            .or_default()
            .push((e.edge.object, e.from, e.to, vec![e.edge.fact]));
        incoming
            .entry((e.edge.predicate, e.edge.object))
            .or_default()
            .push((e.edge.subject, e.from, e.to, vec![e.edge.fact]));
    }

    let mut frontier: Vec<(Triple, Reached)> = edges
        .iter()
        .map(|e| {
            (
                (e.edge.predicate, e.edge.subject, e.edge.object),
                Reached {
                    from: e.from,
                    to: e.to,
                    premises: vec![e.edge.fact],
                },
            )
        })
        .collect();
    // The starting frontier's order follows the input arguments, and their order is not guaranteed—sort once by full fact identity,
    // so that two derivations over the same library produce the same result
    frontier.sort_by_key(|(t, acc)| (*t, acc.from, acc.to));

    // The round limit is a **fallback**; the real bound is the `premises.len() >= MAX_DEPTH` check below:
    // the constant means "paths are at most 12 long", which only matches if counted by number of premises. The round count only guards against pathological input
    for _ in 0..MAX_DEPTH {
        if frontier.is_empty() {
            break;
        }
        let mut next: Vec<(Triple, Reached)> = Vec::new();

        for (triple, acc) in frontier.drain(..) {
            let (pred, subj, obj) = triple;
            let Some(ax) = axioms.get(&pred) else {
                continue;
            };
            // Chaining one more would exceed the limit: this one is not extended further, but it has already produced output
            if acc.premises.len() >= MAX_DEPTH {
                continue;
            }

            // ---- The three one-hop rules: swap subject and object (symmetric), change predicate (inverse / sub-property)
            let mut hops: Vec<(Triple, Rule)> = Vec::new();
            if ax.symmetric {
                hops.push(((pred, obj, subj), Rule::Symmetric));
            }
            if let Some(inv) = ax.inverse_of {
                // `A p B ⟹ B p⁻¹ A`. Subject and object are swapped **and** the predicate is changed—both happen together,
                // doing only one is where this rule is most easily gotten wrong
                hops.push(((inv, obj, subj), Rule::Inverse));
            }
            if let Some(sup) = ax.sub_property_of {
                // `A p B ∧ p ⊑ q ⟹ A q B`. Subject and object stay put; only the predicate is lifted
                hops.push(((sup, subj, obj), Rule::SubProperty));
            }
            for (t, rule) in hops {
                if emit(
                    t,
                    pred,
                    rule,
                    &acc,
                    acc.from,
                    acc.to,
                    None,
                    &asserted,
                    &mut reached,
                    &mut per_pred,
                    &mut capped,
                    &mut out,
                    &mut next,
                    &mut adj,
                    &mut incoming,
                ) {
                    continue;
                }
            }

            // ---- Transitivity: a new edge can serve as either the left or the right half
            if ax.transitive {
                let outs = adj.get(&(pred, obj)).cloned().unwrap_or_default();
                for (c, from, to, hop_premises) in outs {
                    // **Self-loops are not inferred.** `A p A` on a transitive + antisymmetric predicate is a contradiction
                    // rather than knowledge; the R0 side reports this cycle along with its path
                    if subj == c {
                        continue;
                    }
                    let Some((nf, nt)) = overlap((acc.from, acc.to), (from, to)) else {
                        continue;
                    };
                    emit(
                        (pred, subj, c),
                        pred,
                        Rule::Transitive,
                        &acc,
                        nf,
                        nt,
                        Some(&hop_premises),
                        &asserted,
                        &mut reached,
                        &mut per_pred,
                        &mut capped,
                        &mut out,
                        &mut next,
                        &mut adj,
                        &mut incoming,
                    );
                }
                let ins = incoming.get(&(pred, subj)).cloned().unwrap_or_default();
                for (a, from, to, premises) in ins {
                    if a == obj {
                        continue;
                    }
                    let Some((nf, nt)) = overlap((from, to), (acc.from, acc.to)) else {
                        continue;
                    };
                    // emit concatenates the proof in left-then-right order; the late arrival is the right side, and the whole path must not be reversed.
                    let prefix = Reached { from, to, premises };
                    emit(
                        (pred, a, obj),
                        pred,
                        Rule::Transitive,
                        &prefix,
                        nf,
                        nt,
                        Some(&acc.premises),
                        &asserted,
                        &mut reached,
                        &mut per_pred,
                        &mut capped,
                        &mut out,
                        &mut next,
                        &mut adj,
                        &mut incoming,
                    );
                }
            }
        }
        next.sort_by_key(|(t, acc)| (*t, acc.from, acc.to));
        frontier = next;
    }

    // A narrow interval may be emitted first and a wide one arrive later. Output is cleaned up after propagation finishes: a narrower but shorter proof
    // may still be a necessary intermediate state for finishing the rest of the chain within MAX_DEPTH, so it must not be removed during propagation based on the interval alone.
    let spans = edges
        .iter()
        .map(|edge| (edge.edge.fact, (edge.from, edge.to)))
        .collect();
    retain_widest_outputs(&mut out.facts, &spans);

    let mut capped: Vec<Uuid> = capped.into_iter().collect();
    capped.sort();
    out.capped = capped;
    out
}

/// Whether `outer` covers `inner`. Both ends `None` means unbounded, so it covers any interval in the same direction.
fn span_contains(outer: Span, inner: Span) -> bool {
    outer
        .0
        .is_none_or(|outer| inner.0.is_some_and(|inner| outer <= inner))
        && outer
            .1
            .is_none_or(|outer| inner.1.is_some_and(|inner| outer >= inner))
}

fn retain_widest_outputs(facts: &mut Vec<Derived>, spans: &HashMap<Uuid, Span>) {
    let mut grouped: HashMap<Triple, Vec<(usize, Span)>> = HashMap::new();
    for (index, fact) in facts.iter().enumerate() {
        // Outputs whose validity cannot be recomputed keep the caller's existing handling; cleanup does not introduce new failure semantics.
        let Some(span) = validity(&fact.premises, spans) else {
            continue;
        };
        grouped
            .entry((fact.predicate, fact.subject, fact.object))
            .or_default()
            .push((index, span));
    }

    let mut keep = vec![true; facts.len()];
    for outputs in grouped.values() {
        for &(index, span) in outputs {
            keep[index] = !outputs.iter().any(|&(other, outer)| {
                // For an identical interval only the first-emitted copy is kept, so that mutual overwriting does not cause everything to be deleted.
                other != index && span_contains(outer, span) && (outer != span || other < index)
            });
        }
    }
    // The original order and the whole Derived are kept; the proof, via and rule are not reassembled.
    let mut index = 0;
    facts.retain(|_| {
        let retained = keep[index];
        index += 1;
        retained
    });
}

/// Record one derivation and link it into the adjacency list for later transitive use. Returns true = this predicate hit the cap.
///
/// The parameter list is ugly, but extracting it is necessary: the four rules' persistence actions are identical
/// (check assertions, check already-derived, compute the interval, record the proof, enter the frontier, enter the adjacency list), and the previous version
/// had symmetry and transitivity each written out separately, so their "skip conditions" gradually diverged.
#[allow(clippy::too_many_arguments)]
fn emit(
    t: Triple,
    // The declaration of the predicate that triggered it. All four rules use "the predicate of the edge currently being expanded"
    via: Uuid,
    rule: Rule,
    acc: &Reached,
    from: Option<i64>,
    to: Option<i64>,
    // All premises the extra transitive edge depends on; one-hop rules have none
    extra_premises: Option<&[Uuid]>,
    asserted: &HashMap<Triple, Vec<Span>>,
    reached: &mut HashMap<Triple, Vec<(Span, Reached)>>,
    per_pred: &mut HashMap<Uuid, usize>,
    capped: &mut HashSet<Uuid>,
    out: &mut Derivation,
    next: &mut Vec<(Triple, Reached)>,
    adj: &mut HashMap<(Uuid, Uuid), Vec<Hop>>,
    incoming: &mut HashMap<(Uuid, Uuid), Vec<Hop>>,
) -> bool {
    let (pred, subj, obj) = t;
    // Self-loops are not inferred, for every rule alike: `A p A` is a contradiction, not knowledge
    if subj == obj {
        return false;
    }
    let span = (from, to);
    // Assertions take precedence; a timeless assertion also covers a dated derivation.
    if asserted
        .get(&t)
        .into_iter()
        .flatten()
        .copied()
        .any(|outer| span_contains(outer, span))
    {
        return false;
    }
    let mut premises = acc.premises.clone();
    if let Some(extra) = extra_premises {
        premises.extend_from_slice(extra);
    }
    // The depth limit is checked again here. The `premises.len() >= MAX_DEPTH` check when expanding the frontier
    // only sees the current copy; the edge that transitivity attaches may carry several premises of its own, which can push the length over the limit in one step
    // so the limit can only be judged **after concatenation**
    if premises.len() > MAX_DEPTH {
        return false;
    }

    let previous = reached.entry(t).or_default();
    // A long proof for the same span can only block duplicate display; it must not block a short proof from continuing to expand: otherwise, having taken the detour first,
    // it could stop at 12 premises even though a shortcut finishing within 12 premises existed.
    // Only drop it when both the interval and the length are covered; mutually inverse pairs still converge at proofs of equal length.
    let covered = previous
        .iter()
        .any(|(outer, _)| span_contains(*outer, span));
    if previous
        .iter()
        .any(|(outer, proof)| span_contains(*outer, span) && proof.premises.len() <= premises.len())
    {
        return false;
    }
    if !covered {
        let n = per_pred.entry(pred).or_insert(0);
        if *n >= MAX_DERIVED_PER_PREDICATE {
            capped.insert(pred);
            return true;
        }
        *n += 1;
    }

    let r = Reached {
        from,
        to,
        premises: premises.clone(),
    };
    previous.retain(|(old_span, proof)| {
        !(span_contains(span, *old_span) && premises.len() <= proof.premises.len())
    });
    previous.push((span, r.clone()));
    // Derived edges can also be chained by later transitivity. **The whole proof must be carried**: this entry in the adjacency list
    // will later be concatenated as a premise into the next derivation; keeping only the first would make the proofs along the chain shorter and shorter
    if !premises.is_empty() {
        for (index, key, other) in [(adj, (pred, subj), obj), (incoming, (pred, obj), subj)] {
            let hops = index.entry(key).or_default();
            hops.retain(|(end, f, t, proof)| {
                *end != other || !(span_contains(span, (*f, *t)) && premises.len() <= proof.len())
            });
            hops.push((other, from, to, premises.clone()));
        }
    }
    if !covered {
        out.facts.push(Derived {
            predicate: pred,
            via,
            subject: subj,
            object: obj,
            rule,
            premises,
        });
    }
    next.push((t, r));
    false
}

/// The validity period of a derived fact, for the persistence side to use.
///
/// Kept separate from [`derive`] because the intersection was already computed during derivation, while the caller's
/// [`Derived`] carries only the premises. Recomputing is simpler than stuffing the interval into the result, and harder to get wrong:
/// the premises are those facts, and the intersection is a function of them.
pub fn validity(
    premises: &[Uuid],
    by_fact: &HashMap<Uuid, (Option<i64>, Option<i64>)>,
) -> Option<(Option<i64>, Option<i64>)> {
    let mut acc = (None, None);
    for p in premises {
        let span = *by_fact.get(p)?;
        acc = overlap(acc, span)?;
    }
    Some(acc)
}

// ===================== Contradictions: what a derivation collided with (0017) =====================

/// A derivation collided with an assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clash {
    /// Index into `Derivation::facts`
    pub derived: usize,
    /// Which axiom it collided with: `Functional`, `InverseFunctional`, `Asymmetry`, `SelfLoop`
    pub axiom: Kind,
    /// The asserted fact it collided with. A self-loop has no other side, so take the derivation's last premise
    pub against: Uuid,
}

/// Two rules together produced mutually contradictory derivations.
///
/// **Aggregated by rule pair, not reported pair by pair**: `ceo_of ⊑ works_at` plus `works_at` functional, every organization with
/// two CEOs collides once per pair. The root cause is those two declarations; queueing pair by pair would only flood Review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleClash {
    /// (predicate where the declaration lives, rule kind); the two are sorted by (predicate, kind), with a ≤ b
    pub a: (Uuid, Rule),
    pub b: (Uuid, Rule),
    pub axiom: Kind,
    /// Pairs of mutually colliding derivations, by index into `Derivation::facts`
    pub pairs: Vec<(usize, usize)>,
}

/// (predicate, one end) → edges to the other end: (other end, fact, interval). Functional has one entry per direction
type ByEnd = HashMap<(Uuid, Uuid), Vec<(Uuid, Uuid, Span)>>;
/// The identity of a rule: the predicate where its declaration lives + the rule kind
type RuleSide = (Uuid, Rule);
/// Colliding derivation pairs, grouped by (rule a, rule b, which axiom they collided on)
type Grouped = HashMap<(RuleSide, RuleSide, Kind), Vec<(usize, usize)>>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contradictions {
    pub with_assertions: Vec<Clash>,
    pub between_derivations: Vec<RuleClash>,
}

impl Contradictions {
    /// Indices of derivations that must not be persisted: those that collided with an assertion, and those that collided with another derivation. **When writing to the graph, err on the side of fewer** (0002)
    pub fn blocked(&self) -> HashSet<usize> {
        let mut out: HashSet<usize> = self.with_assertions.iter().map(|c| c.derived).collect();
        for rc in &self.between_derivations {
            for (i, j) in &rc.pairs {
                out.insert(*i);
                out.insert(*j);
            }
        }
        out
    }
}

/// Measure the derivations against the axioms once: collisions with assertions are listed one by one, collisions among derivations are aggregated by rule pair.
///
/// Only four kinds are checked — `functional` (including inverse), `asymmetric`, `irreflexive` — because only they
/// can establish a contradiction from **two edges**; transitive cycles require walking the closure, and the derivations are themselves part of the closure, so R0 checking the assertions
/// is enough. Both functional and asymmetric require **overlapping validity intervals**: Mira leaving and Devin taking over
/// means the two `ceo_of` intervals do not intersect; that is a succession, not a contradiction.
///
/// A derivation that collides with an assertion is **never persisted** (asserted > derived, hard rule); this step makes the act of "yielding"
/// visible instead of silent — the row in the 0002 table that was written but never implemented.
pub fn contradictions(
    derivation: &Derivation,
    edges: &[TimedEdge],
    axioms: &HashMap<Uuid, Axioms>,
    spans: &HashMap<Uuid, (Option<i64>, Option<i64>)>,
) -> Contradictions {
    // Three indexes over assertions: (predicate, subject) → object; (predicate, object) → subject; (predicate, subject, object) → edge
    let mut by_ps: ByEnd = HashMap::new();
    let mut by_po: ByEnd = HashMap::new();
    let mut by_spo: HashMap<(Uuid, Uuid, Uuid), Vec<(Uuid, Span)>> = HashMap::new();
    for e in edges {
        let span = (e.from, e.to);
        let x = e.edge;
        by_ps
            .entry((x.predicate, x.subject))
            .or_default()
            .push((x.object, x.fact, span));
        by_po
            .entry((x.predicate, x.object))
            .or_default()
            .push((x.subject, x.fact, span));
        by_spo
            .entry((x.predicate, x.subject, x.object))
            .or_default()
            .push((x.fact, span));
    }

    let mut out = Contradictions::default();
    // Derivation interval: computed by the same function as on the persistence side; anything that cannot be computed (premise intervals do not intersect) would never be persisted anyway
    let derived_spans: Vec<Option<Span>> = derivation
        .facts
        .iter()
        .map(|d| validity(&d.premises, spans))
        .collect();

    for (i, d) in derivation.facts.iter().enumerate() {
        let Some(span) = derived_spans[i] else {
            continue;
        };
        let Some(ax) = axioms.get(&d.predicate) else {
            continue;
        };
        let Some(&last) = d.premises.last() else {
            continue;
        };
        if ax.irreflexive && d.subject == d.object {
            out.with_assertions.push(Clash {
                derived: i,
                axiom: Kind::SelfLoop,
                against: last,
            });
        }
        if ax.asymmetric {
            if let Some(v) = by_spo.get(&(d.predicate, d.object, d.subject)) {
                for (fact, sp) in v {
                    if overlap(span, *sp).is_some() {
                        out.with_assertions.push(Clash {
                            derived: i,
                            axiom: Kind::Asymmetry,
                            against: *fact,
                        });
                    }
                }
            }
        }
        if ax.functional {
            if let Some(v) = by_ps.get(&(d.predicate, d.subject)) {
                for (obj, fact, sp) in v {
                    if *obj != d.object && overlap(span, *sp).is_some() {
                        out.with_assertions.push(Clash {
                            derived: i,
                            axiom: Kind::Functional,
                            against: *fact,
                        });
                    }
                }
            }
        }
        if ax.inverse_functional {
            if let Some(v) = by_po.get(&(d.predicate, d.object)) {
                for (subj, fact, sp) in v {
                    if *subj != d.subject && overlap(span, *sp).is_some() {
                        out.with_assertions.push(Clash {
                            derived: i,
                            axiom: Kind::InverseFunctional,
                            against: *fact,
                        });
                    }
                }
            }
        }
    }

    // Among derivations: the same three indexes, except keyed by index
    let mut d_ps: HashMap<(Uuid, Uuid), Vec<usize>> = HashMap::new();
    let mut d_po: HashMap<(Uuid, Uuid), Vec<usize>> = HashMap::new();
    let mut d_spo: HashMap<(Uuid, Uuid, Uuid), Vec<usize>> = HashMap::new();
    for (i, d) in derivation.facts.iter().enumerate() {
        if derived_spans[i].is_none() {
            continue;
        }
        d_ps.entry((d.predicate, d.subject)).or_default().push(i);
        d_po.entry((d.predicate, d.object)).or_default().push(i);
        d_spo
            .entry((d.predicate, d.subject, d.object))
            .or_default()
            .push(i);
    }
    let mut grouped: Grouped = HashMap::new();
    let mut note = |i: usize, j: usize, axiom: Kind| {
        let (i, j) = if i < j { (i, j) } else { (j, i) };
        let ri = (derivation.facts[i].via, derivation.facts[i].rule);
        let rj = (derivation.facts[j].via, derivation.facts[j].rule);
        let (a, b) = if (ri.0, ri.1.as_str()) <= (rj.0, rj.1.as_str()) {
            (ri, rj)
        } else {
            (rj, ri)
        };
        grouped.entry((a, b, axiom)).or_default().push((i, j));
    };
    for (i, d) in derivation.facts.iter().enumerate() {
        let Some(span) = derived_spans[i] else {
            continue;
        };
        let Some(ax) = axioms.get(&d.predicate) else {
            continue;
        };
        let overlapping = |j: usize| derived_spans[j].is_some_and(|s| overlap(span, s).is_some());
        if ax.asymmetric {
            if let Some(v) = d_spo.get(&(d.predicate, d.object, d.subject)) {
                for &j in v {
                    if j > i && overlapping(j) {
                        note(i, j, Kind::Asymmetry);
                    }
                }
            }
        }
        if ax.functional {
            if let Some(v) = d_ps.get(&(d.predicate, d.subject)) {
                for &j in v {
                    if j > i && derivation.facts[j].object != d.object && overlapping(j) {
                        note(i, j, Kind::Functional);
                    }
                }
            }
        }
        if ax.inverse_functional {
            if let Some(v) = d_po.get(&(d.predicate, d.object)) {
                for &j in v {
                    if j > i && derivation.facts[j].subject != d.subject && overlapping(j) {
                        note(i, j, Kind::InverseFunctional);
                    }
                }
            }
        }
    }
    let mut rule_clashes: Vec<RuleClash> = grouped
        .into_iter()
        .map(|((a, b, axiom), mut pairs)| {
            pairs.sort_unstable();
            pairs.dedup();
            RuleClash { a, b, axiom, pairs }
        })
        .collect();
    // Output is sorted — half the value of this path lies in determinism
    rule_clashes.sort_by(|x, y| {
        (x.a.0, x.a.1.as_str(), x.b.0, x.b.1.as_str()).cmp(&(
            y.a.0,
            y.a.1.as_str(),
            y.b.0,
            y.b.1.as_str(),
        ))
    });
    out.between_derivations = rule_clashes;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(i: u8) -> Uuid {
        Uuid::from_bytes([i; 16])
    }
    fn f(i: u8) -> Uuid {
        Uuid::from_bytes([i; 16].map(|b| b ^ 0xF0))
    }
    /// A timeless edge
    fn e(fact: u8, s: u8, o: u8) -> TimedEdge {
        TimedEdge {
            edge: Edge {
                fact: f(fact),
                predicate: n(99),
                subject: n(s),
                object: n(o),
            },
            from: None,
            to: None,
        }
    }
    /// An edge with an interval
    fn te(fact: u8, s: u8, o: u8, from: Option<i64>, to: Option<i64>) -> TimedEdge {
        TimedEdge {
            from,
            to,
            ..e(fact, s, o)
        }
    }
    fn with(ax: Axioms) -> HashMap<Uuid, Axioms> {
        HashMap::from([(n(99), ax)])
    }
    fn transitive() -> HashMap<Uuid, Axioms> {
        with(Axioms {
            transitive: true,
            ..Default::default()
        })
    }
    fn pairs(d: &Derivation) -> Vec<(u8, u8)> {
        let mut p: Vec<(u8, u8)> = d
            .facts
            .iter()
            .map(|x| (x.subject.as_bytes()[0], x.object.as_bytes()[0]))
            .collect();
        p.sort();
        p
    }

    #[test]
    fn nothing_declared_derives_nothing() {
        let edges = [e(1, 1, 2), e(2, 2, 3)];
        assert!(derive(&edges, &HashMap::new()).facts.is_empty());
        // The same goes for declaring other axioms — only transitive / symmetric can be compiled into rules
        let irr = with(Axioms {
            irreflexive: true,
            ..Default::default()
        });
        assert!(derive(&edges, &irr).facts.is_empty());
    }

    #[test]
    fn a_chain_closes() {
        // 1→2→3→4; transitivity should derive 1→3, 1→4, 2→4
        let edges = [e(1, 1, 2), e(2, 2, 3), e(3, 3, 4)];
        let d = derive(&edges, &transitive());
        assert_eq!(pairs(&d), vec![(1, 3), (1, 4), (2, 4)]);
        assert!(d.capped.is_empty());
    }

    #[test]
    fn the_proof_is_the_premises_in_order() {
        let edges = [e(1, 1, 2), e(2, 2, 3), e(3, 3, 4)];
        let d = derive(&edges, &transitive());
        let long = d
            .facts
            .iter()
            .find(|x| x.subject == n(1) && x.object == n(4))
            .unwrap();
        assert_eq!(
            long.premises,
            vec![f(1), f(2), f(3)],
            "证明要按推导顺序带上三条前提"
        );
        assert_eq!(long.rule, Rule::Transitive);
    }

    #[test]
    fn a_chained_derivation_keeps_every_premise() {
        // 4→1→2 yields 4→2, and 4→2 is already a proof; when transitivity then chains 2→3, that derived edge
        // carries two premises that must both enter the proof. If either is missing, `validity` would intersect only the remaining premises
        // and the conclusion would be wider than it really is
        let edges = [
            te(1, 1, 2, Some(0), Some(100)),
            te(2, 2, 3, Some(40), Some(50)),
            te(3, 4, 1, Some(0), Some(100)),
        ];
        let d = derive(&edges, &transitive());
        let to3 = d
            .facts
            .iter()
            .find(|x| x.subject == n(4) && x.object == n(3))
            .expect("4 to 3 should be derived from 4 to 1 to 2 to 3");
        assert_eq!(
            to3.premises,
            vec![f(3), f(1), f(2)],
            "证明要带上链上三条前提，中间那条派生边的两条都在内"
        );
        let by_fact: HashMap<Uuid, _> = edges
            .iter()
            .map(|x| (x.edge.fact, (x.from, x.to)))
            .collect();
        assert_eq!(
            validity(&to3.premises, &by_fact),
            Some((Some(40), Some(50))),
            "有效期应是三条前提的交集，而不是漏掉中间那条的宽区间"
        );
    }

    #[test]
    fn asserted_beats_derived() {
        // 1→2 and 2→3 already derive 1→3, and 1→3 was also asserted → not derived again
        let edges = [e(1, 1, 2), e(2, 2, 3), e(3, 1, 3)];
        let d = derive(&edges, &transitive());
        assert!(
            d.facts.is_empty(),
            "asserted triples must not be derived again"
        );
    }

    #[test]
    fn a_ring_does_not_derive_self_loops_and_does_not_hang() {
        // 1→2→3→1: a cycle. The transitive closure would infer 1→1, which is a contradiction, not knowledge
        let edges = [e(1, 1, 2), e(2, 2, 3), e(3, 3, 1)];
        let d = derive(&edges, &transitive());
        assert!(
            d.facts.iter().all(|x| x.subject != x.object),
            "自环不该被推出来——R0 会把这个环连路径一起报"
        );
        // But the remaining derivations on the cycle hold: 1→3, 2→1, 3→2
        assert_eq!(pairs(&d), vec![(1, 3), (2, 1), (3, 2)]);
    }

    #[test]
    fn symmetric_derives_the_other_direction_once() {
        let sym = with(Axioms {
            symmetric: true,
            ..Default::default()
        });
        let edges = [e(1, 1, 2)];
        let d = derive(&edges, &sym);
        assert_eq!(pairs(&d), vec![(2, 1)]);
        assert_eq!(d.facts[0].rule, Rule::Symmetric);
        assert_eq!(d.facts[0].premises, vec![f(1)]);
        // Both directions asserted → nothing to derive
        let both = [e(1, 1, 2), e(2, 2, 1)];
        assert!(derive(&both, &sym).facts.is_empty());
    }

    #[test]
    fn validity_is_the_intersection() {
        // 1→2 holds on [10,30), 2→3 holds on [20,∞) ⟹ 1→3 holds on [20,30)
        let edges = [te(1, 1, 2, Some(10), Some(30)), te(2, 2, 3, Some(20), None)];
        let d = derive(&edges, &transitive());
        assert_eq!(pairs(&d), vec![(1, 3)]);
        let by_fact = HashMap::from([(f(1), (Some(10), Some(30))), (f(2), (Some(20), None))]);
        assert_eq!(
            validity(&d.facts[0].premises, &by_fact),
            Some((Some(20), Some(30)))
        );
    }

    #[test]
    fn no_overlap_derives_nothing() {
        // 1→2 holds only on [10,20), 2→3 only on [30,40) — this chain does not hold at any moment
        let edges = [
            te(1, 1, 2, Some(10), Some(20)),
            te(2, 2, 3, Some(30), Some(40)),
        ];
        let d = derive(&edges, &transitive());
        assert!(
            d.facts.is_empty(),
            "两段不重叠时推出来的是一条从不为真的事实"
        );
    }

    #[test]
    fn a_touching_boundary_is_not_an_overlap() {
        // [10,20) and [20,30): half-open intervals; touching endpoints do not count as overlap
        let edges = [
            te(1, 1, 2, Some(10), Some(20)),
            te(2, 2, 3, Some(20), Some(30)),
        ];
        assert!(derive(&edges, &transitive()).facts.is_empty());
    }

    #[test]
    fn depth_is_bounded() {
        // A 40-hop chain; the depth limit is 12
        let edges: Vec<TimedEdge> = (1..=40).map(|i| e(i, i, i + 1)).collect();
        let d = derive(&edges, &transitive());
        let longest = d.facts.iter().map(|x| x.premises.len()).max().unwrap();
        assert!(
            longest <= MAX_DEPTH,
            "证明长度不该超过深度上限，实际 {longest}"
        );
        assert!(
            !d.facts.is_empty(),
            "having a limit does not mean derive nothing"
        );
    }

    fn edges_with_shortcuts() -> Vec<TimedEdge> {
        // 21→8→16 and 14→1→7→12 both have shortcuts; reaching a detour proof first should not
        // cost the later short proof its chance to keep deriving. The shortest path 11→20 has exactly 12 premises.
        [
            (12, 0),
            (7, 12),
            (15, 10),
            (1, 7),
            (2, 5),
            (8, 16),
            (13, 21),
            (5, 13),
            (6, 14),
            (14, 12),
            (21, 8),
            (0, 20),
            (10, 2),
            (21, 16),
            (14, 1),
            (11, 15),
            (16, 6),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (s, o))| e(i as u8, s, o))
        .collect()
    }

    #[test]
    fn a_shorter_proof_keeps_a_chain_within_the_depth_limit() {
        let edges = edges_with_shortcuts();
        let d = derive(&edges, &transitive());
        let chain = d
            .facts
            .iter()
            .find(|x| x.subject == n(11) && x.object == n(20))
            .expect("a 12-premise chain must not be blocked by an earlier long proof");
        assert_eq!(chain.premises.len(), MAX_DEPTH);
        assert!(d.facts.iter().all(|x| x.premises.len() <= MAX_DEPTH));
        let distinct: HashSet<_> = d.facts.iter().map(|x| (x.subject, x.object)).collect();
        assert_eq!(
            distinct.len(),
            d.facts.len(),
            "a short proof no longer materializes the same facts"
        );
        assert!(d.capped.is_empty());
    }

    #[test]
    fn shorter_proofs_do_not_spend_the_output_cap_twice() {
        let component = edges_with_shortcuts();
        // Several disconnected copies share one predicate; each contains short proofs that need to be expanded again.
        // The cap counts facts actually produced; these substitute proofs must not be used to offset the quota.
        let edges: Vec<_> = (0..300u128)
            .flat_map(|copy| {
                component.iter().enumerate().map(move |(i, e)| TimedEdge {
                    edge: Edge {
                        fact: Uuid::from_u128(100_000 + copy * 32 + i as u128),
                        predicate: e.edge.predicate,
                        subject: Uuid::from_u128(copy * 32 + e.edge.subject.as_bytes()[0] as u128),
                        object: Uuid::from_u128(copy * 32 + e.edge.object.as_bytes()[0] as u128),
                    },
                    ..*e
                })
            })
            .collect();
        let d = derive(&edges, &transitive());
        assert_eq!(d.facts.len(), MAX_DERIVED_PER_PREDICATE);
        assert_eq!(d.capped, vec![n(99)]);
        let distinct: HashSet<_> = d.facts.iter().map(|x| (x.subject, x.object)).collect();
        assert_eq!(distinct.len(), d.facts.len());
        assert!(d.facts.iter().all(|x| x.premises.len() <= MAX_DEPTH));
    }

    #[test]
    fn symmetric_feeds_the_transitive_chain() {
        // Declares both symmetric and transitive: 1→2 and 3→2 are asserted; symmetry yields 2→3,
        // so transitivity can connect 1→3
        let both = with(Axioms {
            symmetric: true,
            transitive: true,
            ..Default::default()
        });
        let edges = [e(1, 1, 2), e(2, 3, 2)];
        let d = derive(&edges, &both);
        let got = pairs(&d);
        assert!(
            got.contains(&(2, 1)) && got.contains(&(2, 3)),
            "two symmetric edges"
        );
        assert!(
            got.contains(&(1, 3)),
            "an edge derived by symmetry must still take part in transitivity"
        );
    }

    #[test]
    fn each_predicate_is_closed_on_its_own() {
        // 99 is transitive, 98 is not: cross-predicate edges must not be chained
        let mut other = e(2, 2, 3);
        other.edge.predicate = n(98);
        let edges = [e(1, 1, 2), other];
        let d = derive(&edges, &transitive());
        assert!(d.facts.is_empty(), "1 (99) 2 (98) 3 derives nothing");
    }

    // ---- The two cross-predicate rules (inverseOf / subPropertyOf) ----
    //
    // The ones above all use a single predicate `n(99)`; these two rules inherently need three predicates to be expressed,
    // so they get their own set of constants and constructors

    const P: Uuid = Uuid::from_bytes([1; 16]);
    const Q: Uuid = Uuid::from_bytes([2; 16]);
    const R: Uuid = Uuid::from_bytes([3; 16]);

    /// A timeless edge on the given predicate
    fn ep(pred: Uuid, fact: u8, s: u8, o: u8) -> TimedEdge {
        TimedEdge {
            edge: Edge {
                fact: f(fact),
                predicate: pred,
                subject: n(s),
                object: n(o),
            },
            from: None,
            to: None,
        }
    }
    /// On the given predicate, with an interval
    fn tep(pred: Uuid, fact: u8, s: u8, o: u8, from: Option<i64>, to: Option<i64>) -> TimedEdge {
        TimedEdge {
            from,
            to,
            ..ep(pred, fact, s, o)
        }
    }
    /// `p⁻¹ = q` and `q⁻¹ = p` — mutual reference; convergence is measured by this
    fn inverse_pair() -> HashMap<Uuid, Axioms> {
        HashMap::from([
            (
                P,
                Axioms {
                    inverse_of: Some(Q),
                    ..Default::default()
                },
            ),
            (
                Q,
                Axioms {
                    inverse_of: Some(P),
                    ..Default::default()
                },
            ),
        ])
    }
    /// `p ⊑ q`
    fn sub_property() -> HashMap<Uuid, Axioms> {
        HashMap::from([(
            P,
            Axioms {
                sub_property_of: Some(Q),
                ..Default::default()
            },
        )])
    }

    /// `A works_at B` ⟹ `B employs A`: subject and object swapped **and** the predicate changed.
    /// Doing only one is the most common way to get this rule wrong, so both are asserted.
    #[test]
    fn the_inverse_swaps_the_ends_and_the_predicate() {
        let d = derive(&[ep(P, 1, 1, 2)], &inverse_pair());
        assert_eq!(d.facts.len(), 1, "an edge derives only one inverse");
        let got = &d.facts[0];
        assert_eq!(got.predicate, Q, "**predicate changed**");
        assert_eq!(
            (got.subject, got.object),
            (n(2), n(1)),
            "**subject and object also swapped**"
        );
        assert_eq!(got.rule, Rule::Inverse);
        assert_eq!(got.via, P, "declared on P, the output lands on Q");
        assert_eq!(got.premises, vec![f(1)], "the proof is that original edge");
    }

    /// `p⁻¹ = q` and `q⁻¹ = p` — mutual reference. The one inferred back has already been asserted,
    /// and must converge rather than oscillate back and forth.
    #[test]
    fn a_mutual_inverse_settles_instead_of_bouncing() {
        let d = derive(&[ep(P, 1, 1, 2), ep(Q, 2, 2, 1)], &inverse_pair());
        assert!(
            d.facts.is_empty(),
            "两个方向都已经断言过，一条都不该推——**断言优先**"
        );
    }

    /// `p ⊑ q`: the specific is asserted, and the general one holds too. Subject and object stay put.
    #[test]
    fn a_sub_property_lifts_the_predicate_and_keeps_the_ends() {
        let d = derive(&[ep(P, 1, 1, 2)], &sub_property());
        assert_eq!(d.facts.len(), 1);
        let got = &d.facts[0];
        assert_eq!(got.predicate, Q, "raised to the parent property");
        assert_eq!(
            (got.subject, got.object),
            (n(1), n(2)),
            "subject and object unchanged"
        );
        assert_eq!(got.rule, Rule::SubProperty);
        assert_eq!(
            got.via, P,
            "**via 是声明公理的那个谓词**，不是推出来的那个——落库按它找规则行"
        );
    }

    /// **For the two rules that do not change the predicate, `via` must equal `predicate`.**
    ///
    /// This looks like a truism, yet it is exactly why the bug could hide: persistence used to look up by `predicate`
    /// to find the rule row, which was always right for transitive and symmetric, so nobody noticed the wrong key. Once the two cross-predicate rules
    /// were added, facts inferred from `ceo_of ⊑ works_at` could not find their rule and were silently dropped.
    #[test]
    fn for_the_same_predicate_rules_via_is_the_predicate() {
        let d = derive(&[e(1, 1, 2), e(2, 2, 3)], &transitive());
        assert!(!d.facts.is_empty());
        for f in &d.facts {
            assert_eq!(
                f.via, f.predicate,
                "transitivity does not change the predicate"
            );
        }
    }

    /// **This is the reason for the whole rework**: the three rules chained together.
    ///
    /// `A ceo_of B` ∧ `ceo_of ⊑ works_at` ∧ `works_at⁻¹ = employs`
    ///   ⟹ `A works_at B` ⟹ `B employs A`
    ///
    /// The old structure that grouped by predicate broke at the first step.
    #[test]
    fn a_sub_property_feeds_the_inverse() {
        let mut ax = HashMap::new();
        // ceo_of ⊑ works_at
        ax.insert(
            P,
            Axioms {
                sub_property_of: Some(Q),
                ..Default::default()
            },
        );
        // works_at⁻¹ = employs
        ax.insert(
            Q,
            Axioms {
                inverse_of: Some(R),
                ..Default::default()
            },
        );
        let d = derive(&[ep(P, 1, 1, 2)], &ax);
        let mut got: Vec<(Uuid, u8, u8)> = d
            .facts
            .iter()
            .map(|x| (x.predicate, x.subject.as_bytes()[0], x.object.as_bytes()[0]))
            .collect();
        got.sort();
        assert!(got.contains(&(Q, 1, 2)), "first raised to works_at");
        assert!(
            got.contains(&(R, 2, 1)),
            "**再转成 employs 的反方向**——跨了两个谓词，旧结构做不到"
        );
        assert_eq!(got.len(), 2);
        // The proof has to grow along: the second hop uses the first of the two premises
        let employs = d.facts.iter().find(|x| x.predicate == R).unwrap();
        assert_eq!(
            employs.premises,
            vec![f(1)],
            "the root is still the original assertion"
        );
    }

    /// An edge produced by inversion must be connectable by transitivity: `p` is transitive, `q` is its inverse,
    /// `B q A` ∧ `C q B` should yield `C q A` (if q is also transitive).
    #[test]
    fn what_the_inverse_produces_can_still_be_chained() {
        let mut ax = HashMap::new();
        ax.insert(
            P,
            Axioms {
                inverse_of: Some(Q),
                ..Default::default()
            },
        );
        ax.insert(
            Q,
            Axioms {
                transitive: true,
                ..Default::default()
            },
        );
        // A p B, B p C  ⟹  B q A, C q B  ⟹ (q transitive) ⟹ C q A
        let d = derive(&[ep(P, 1, 1, 2), ep(P, 2, 2, 3)], &ax);
        let got: Vec<(Uuid, u8, u8)> = d
            .facts
            .iter()
            .map(|x| (x.predicate, x.subject.as_bytes()[0], x.object.as_bytes()[0]))
            .collect();
        assert!(got.contains(&(Q, 2, 1)));
        assert!(got.contains(&(Q, 3, 2)));
        assert!(
            got.contains(&(Q, 3, 1)),
            "**逆产出的边要进邻接表**，否则传递接不上它"
        );
    }

    #[test]
    fn a_late_right_hand_edge_completes_an_existing_chain() {
        for (source, target) in [(Q, P), (P, Q)] {
            for inverse in [false, true] {
                let ax = HashMap::from([
                    (
                        source,
                        Axioms {
                            inverse_of: inverse.then_some(target),
                            sub_property_of: (!inverse).then_some(target),
                            ..Default::default()
                        },
                    ),
                    (
                        target,
                        Axioms {
                            transitive: true,
                            ..Default::default()
                        },
                    ),
                ]);
                let left = ep(target, 1, 1, 2);
                let right = if inverse {
                    ep(source, 2, 3, 2)
                } else {
                    ep(source, 2, 2, 3)
                };
                for edges in [[left, right], [right, left]] {
                    let d = derive(&edges, &ax);
                    let chain = d
                        .facts
                        .iter()
                        .find(|x| x.predicate == target && x.subject == n(1) && x.object == n(3))
                        .expect("right side derived one step later must not leave the already-expanded left unable to connect to it");
                    assert_eq!(chain.rule, Rule::Transitive);
                    assert_eq!(chain.via, target);
                    assert_eq!(chain.premises, vec![f(1), f(2)]);
                    assert!(d.capped.is_empty());
                }
            }
        }
    }

    #[test]
    fn a_late_right_hand_edge_keeps_the_whole_prefix_and_its_span() {
        let ax = HashMap::from([
            (
                P,
                Axioms {
                    transitive: true,
                    ..Default::default()
                },
            ),
            (
                Q,
                Axioms {
                    sub_property_of: Some(P),
                    ..Default::default()
                },
            ),
            (
                R,
                Axioms {
                    sub_property_of: Some(Q),
                    ..Default::default()
                },
            ),
        ]);
        let edges = [
            tep(P, 1, 1, 2, Some(0), Some(30)),
            tep(P, 2, 2, 3, Some(10), Some(40)),
            tep(R, 3, 3, 4, Some(20), Some(50)),
            tep(R, 4, 3, 5, Some(30), Some(50)),
        ];
        let spans = edges
            .iter()
            .map(|e| (e.edge.fact, (e.from, e.to)))
            .collect();
        let d = derive(&edges, &ax);
        let chain = d
            .facts
            .iter()
            .find(|x| x.predicate == P && x.subject == n(1) && x.object == n(4))
            .expect("when the right side arrives across two rounds, the derived left must still connect");
        assert_eq!(chain.premises, vec![f(1), f(2), f(3)]);
        assert_eq!(
            validity(&chain.premises, &spans),
            Some((Some(20), Some(30)))
        );
        assert!(
            !d.facts
                .iter()
                .any(|x| x.predicate == P && x.subject == n(1) && x.object == n(5)),
            "前缀止于 30，右边始于 30，半开区间没有交集"
        );
    }

    /// The same triple can hold over several time spans. The second span must not be blocked from transitivity by the first,
    /// and the time closure must be the same regardless of input order.
    #[test]
    fn distinct_intervals_for_one_triple_survive_axiom_steps_in_any_order() {
        let ax = HashMap::from([
            (
                P,
                Axioms {
                    inverse_of: Some(Q),
                    ..Default::default()
                },
            ),
            (
                Q,
                Axioms {
                    transitive: true,
                    ..Default::default()
                },
            ),
        ]);
        let early = tep(P, 1, 1, 2, Some(0), Some(10));
        let late = tep(P, 2, 1, 2, Some(20), Some(30));
        let cb = tep(Q, 3, 3, 2, Some(20), Some(30));
        let spans = HashMap::from([
            (f(1), (Some(0), Some(10))),
            (f(2), (Some(20), Some(30))),
            (f(3), (Some(20), Some(30))),
        ]);
        let closure = |edges: [TimedEdge; 3]| {
            let d = derive(&edges, &ax);
            let mut closure: Vec<_> = d
                .facts
                .iter()
                .map(|x| {
                    let (from, to) = validity(&x.premises, &spans)
                        .expect("derived interval should be computable");
                    (x.predicate, x.subject, x.object, from, to)
                })
                .collect();
            closure.sort();
            closure
        };

        let early_first = closure([early, late, cb]);
        let late_first = closure([late, early, cb]);

        assert_eq!(
            early_first, late_first,
            "closure must not change with fact input order"
        );
        assert!(
            early_first.contains(&(Q, n(2), n(1), Some(0), Some(10))),
            "第一段 B q A 也要保留"
        );
        assert!(
            early_first.contains(&(Q, n(2), n(1), Some(20), Some(30))),
            "第二段 B q A 要保留"
        );
        assert!(
            early_first.contains(&(Q, n(3), n(1), Some(20), Some(30))),
            "第二段 B q A 必须还能和 C q B 接成 C q A"
        );
    }

    /// If an asserted span covers a derived narrower span, no second fact with the same triple is kept.
    #[test]
    fn an_asserted_interval_covers_a_narrower_derivation() {
        let edges = [
            te(1, 1, 2, Some(10), Some(20)),
            te(2, 2, 3, Some(10), Some(20)),
            te(3, 1, 3, Some(0), Some(100)),
        ];
        let d = derive(&edges, &transitive());
        assert!(
            d.facts.is_empty(),
            "断言 [0,100) 已覆盖推出来的 [10,20)，不该再派生一份"
        );
    }

    /// A timeless assertion means it always holds, and also covers a dated derivation of the same triple.
    #[test]
    fn an_undated_assertion_covers_a_dated_derivation() {
        let edges = [
            te(1, 1, 2, Some(10), Some(20)),
            te(2, 2, 3, Some(10), Some(20)),
            te(3, 1, 3, None, None),
        ];
        let d = derive(&edges, &transitive());
        assert!(
            d.facts.is_empty(),
            "无时间断言覆盖 [10,20)，不该推出一条说得更少的同 triple 事实"
        );
    }

    /// Two paths yield [0,100) and [10,20) respectively: the wide one is kept, the narrow one dropped; the same holds when input order is reversed.
    #[test]
    fn a_wider_route_suppresses_a_narrower_derivation() {
        let edges = [
            te(1, 1, 2, Some(0), Some(100)),
            te(2, 2, 4, Some(0), Some(100)),
            te(3, 1, 3, Some(10), Some(20)),
            te(4, 3, 4, Some(10), Some(20)),
        ];
        let by_fact: HashMap<_, _> = edges
            .iter()
            .map(|e| (e.edge.fact, (e.from, e.to)))
            .collect();
        let spans = |edges: [TimedEdge; 4]| {
            let d = derive(&edges, &transitive());
            let mut spans: Vec<_> = d
                .facts
                .iter()
                .filter(|x| x.subject == n(1) && x.object == n(4))
                .map(|x| {
                    validity(&x.premises, &by_fact).expect("derived interval should be computable")
                })
                .collect();
            spans.sort();
            spans
        };

        let broad = (Some(0), Some(100));
        assert_eq!(spans(edges), vec![broad], "only the wider [0,100) is kept");
        assert_eq!(
            spans([edges[3], edges[2], edges[1], edges[0]]),
            vec![broad],
            "输入反序不能把被覆盖的窄区间又放回来"
        );
    }

    fn competing_routes(
        p: Uuid,
        q: Uuid,
        nodes: [Uuid; 4],
        narrow: Span,
        broad: Span,
    ) -> ([TimedEdge; 4], HashMap<Uuid, Axioms>) {
        let [a, b, c, d] = nodes;
        let edges = [
            (101, p, a, d, narrow),
            (102, q, a, b, broad),
            (103, q, b, c, broad),
            (104, q, c, d, broad),
        ]
        .map(|(fact, predicate, subject, object, (from, to))| TimedEdge {
            edge: Edge {
                fact: Uuid::from_u128(fact),
                predicate,
                subject,
                object,
            },
            from,
            to,
        });
        let axioms = HashMap::from([
            (
                p,
                Axioms {
                    sub_property_of: Some(q),
                    ..Default::default()
                },
            ),
            (
                q,
                Axioms {
                    transitive: true,
                    ..Default::default()
                },
            ),
        ]);
        (edges, axioms)
    }

    /// Follow-up left by #1034 / #1040: emit the narrow sub-property interval first, then the wide transitive interval.
    #[test]
    fn a_later_wider_derivation_replaces_the_narrow_output_with_its_whole_proof() {
        let p = Uuid::from_u128(1);
        let q = Uuid::from_u128(2);
        let [a, b, c, d] = [10, 20, 30, 40].map(Uuid::from_u128);
        let (edges, axioms) = competing_routes(
            p,
            q,
            [a, b, c, d],
            (Some(10), Some(20)),
            (Some(0), Some(100)),
        );
        let result = derive(&edges, &axioms);
        let spans = spans_of(&edges);
        let outputs: Vec<_> = result
            .facts
            .iter()
            .filter(|fact| (fact.predicate, fact.subject, fact.object) == (q, a, d))
            .collect();
        assert_eq!(
            outputs
                .iter()
                .map(|fact| validity(&fact.premises, &spans).unwrap())
                .collect::<Vec<_>>(),
            vec![(Some(0), Some(100))],
            "后到的宽区间应移除先到的窄区间输出"
        );
        assert_eq!(
            outputs[0].premises,
            [102, 103, 104].map(Uuid::from_u128).to_vec()
        );
        assert_eq!(outputs[0].rule, Rule::Transitive);
        assert_eq!(outputs[0].via, q);
        assert!(result.capped.is_empty());
    }

    #[test]
    fn final_intervals_do_not_depend_on_input_or_uuid_order() {
        for (p, q) in [(1, 2), (2, 1)].map(|(p, q)| (Uuid::from_u128(p), Uuid::from_u128(q))) {
            for nodes in [[10, 20, 30, 40], [40, 30, 20, 10], [20, 40, 10, 30]] {
                let [a, b, c, d] = nodes.map(Uuid::from_u128);
                let (edges, axioms) = competing_routes(
                    p,
                    q,
                    [a, b, c, d],
                    (Some(10), Some(20)),
                    (Some(0), Some(100)),
                );
                let spans = spans_of(&edges);
                let mut expected = vec![
                    (q, a, c, Some(0), Some(100)),
                    (q, a, d, Some(0), Some(100)),
                    (q, b, d, Some(0), Some(100)),
                ];
                expected.sort();
                // All 24 permutations of four facts; when predicate ordering is swapped, a wide interval can arrive first.
                for i in 0..4 {
                    for j in 0..4 {
                        for k in 0..4 {
                            if i == j || i == k || j == k {
                                continue;
                            }
                            let last = (0..4).find(|x| *x != i && *x != j && *x != k).unwrap();
                            let result =
                                derive(&[edges[i], edges[j], edges[k], edges[last]], &axioms);
                            let mut actual: Vec<_> = result
                                .facts
                                .iter()
                                .map(|fact| {
                                    let (from, to) = validity(&fact.premises, &spans).unwrap();
                                    (fact.predicate, fact.subject, fact.object, from, to)
                                })
                                .collect();
                            actual.sort();
                            assert_eq!(actual, expected);
                            let wide = result
                                .facts
                                .iter()
                                .find(|fact| fact.object == d && fact.subject == a)
                                .unwrap();
                            assert_eq!(
                                wide.premises,
                                [102, 103, 104].map(Uuid::from_u128).to_vec()
                            );
                            assert_eq!((wide.via, wide.rule), (q, Rule::Transitive));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn final_outputs_keep_incomparable_spans_and_one_equal_span() {
        let narrow = (Some(10), Some(20));
        let cases = [
            (narrow, (None, Some(100)), vec![(None, Some(100))]),
            (narrow, (Some(0), None), vec![(Some(0), None)]),
            (narrow, (None, None), vec![(None, None)]),
            ((None, Some(20)), (None, None), vec![(None, None)]),
            ((Some(10), None), (None, None), vec![(None, None)]),
            (narrow, narrow, vec![narrow]),
            ((None, None), (None, None), vec![(None, None)]),
            (
                narrow,
                (Some(15), Some(30)),
                vec![narrow, (Some(15), Some(30))],
            ),
            (
                narrow,
                (Some(20), Some(30)),
                vec![narrow, (Some(20), Some(30))],
            ),
            (
                narrow,
                (Some(30), Some(40)),
                vec![narrow, (Some(30), Some(40))],
            ),
            (
                (None, Some(20)),
                (Some(10), None),
                vec![(None, Some(20)), (Some(10), None)],
            ),
        ];
        for (narrow, broad, mut expected) in cases {
            let (edges, axioms) = competing_routes(P, Q, [n(1), n(2), n(3), n(4)], narrow, broad);
            let result = derive(&edges, &axioms);
            let spans = spans_of(&edges);
            let mut actual: Vec<_> = result
                .facts
                .iter()
                .filter(|fact| (fact.predicate, fact.subject, fact.object) == (Q, n(1), n(4)))
                .map(|fact| validity(&fact.premises, &spans).unwrap())
                .collect();
            actual.sort();
            expected.sort();
            assert_eq!(actual, expected, "interval {narrow:?} vs {broad:?}");
        }
    }

    #[test]
    fn a_union_of_outputs_does_not_cover_another_output() {
        let target = n(100);
        let intervals = [
            (Some(0), Some(60)),
            (Some(40), Some(100)),
            (Some(20), Some(80)),
        ];
        let edges = [P, Q, R]
            .into_iter()
            .zip(intervals)
            .enumerate()
            .map(|(i, (source, (from, to)))| tep(source, i as u8, 1, 2, from, to))
            .collect::<Vec<_>>();
        let axioms = [P, Q, R]
            .map(|source| {
                (
                    source,
                    Axioms {
                        sub_property_of: Some(target),
                        ..Default::default()
                    },
                )
            })
            .into_iter()
            .collect();
        let result = derive(&edges, &axioms);
        let spans = spans_of(&edges);
        let mut actual: Vec<_> = result
            .facts
            .iter()
            .map(|fact| {
                assert_eq!(
                    (fact.predicate, fact.subject, fact.object),
                    (target, n(1), n(2))
                );
                assert_eq!(fact.premises.len(), 1);
                let source = edges
                    .iter()
                    .find(|edge| edge.edge.fact == fact.premises[0])
                    .unwrap();
                assert_eq!(
                    (fact.via, fact.rule),
                    (source.edge.predicate, Rule::SubProperty)
                );
                validity(&fact.premises, &spans).unwrap()
            })
            .collect();
        actual.sort();
        let mut expected = intervals.to_vec();
        expected.sort();
        assert_eq!(
            actual, expected,
            "must be covered by a single interval, not replaced by the union of two segments"
        );
    }

    #[test]
    fn final_cleanup_keeps_one_equal_output_and_its_original_proof() {
        let first = Derived {
            predicate: Q,
            subject: n(1),
            object: n(2),
            via: P,
            rule: Rule::SubProperty,
            premises: vec![f(1)],
        };
        let second = Derived {
            via: Q,
            rule: Rule::Transitive,
            premises: vec![f(2), f(3)],
            ..first.clone()
        };
        let other_triple = Derived {
            predicate: R,
            ..first.clone()
        };
        let spans = HashMap::from([
            (f(1), (Some(10), Some(20))),
            (f(2), (Some(0), Some(20))),
            (f(3), (Some(10), Some(30))),
        ]);
        let mut outputs = vec![first.clone(), other_triple.clone(), second];
        retain_widest_outputs(&mut outputs, &spans);
        assert_eq!(outputs, vec![first, other_triple]);
    }

    #[test]
    fn final_cleanup_does_not_change_outputs_with_an_empty_validity() {
        // Single-hop rules originally allowed this input to return output, with the consumer rejecting it via validity; cleanup must not be changed to panic.
        for to in [10, 20] {
            let edges = [tep(P, 1, 1, 2, Some(20), Some(to))];
            let result = derive(&edges, &sub_property());
            assert_eq!(result.facts.len(), 1);
            assert_eq!(result.facts[0].premises, vec![f(1)]);
            assert_eq!(validity(&result.facts[0].premises, &spans_of(&edges)), None);
        }
    }

    #[test]
    fn a_narrower_shorter_proof_still_finishes_within_the_depth_limit() {
        let mut edges = edges_with_shortcuts();
        let shortcut = edges
            .iter_mut()
            .find(|edge| edge.edge.subject == n(21) && edge.edge.object == n(16))
            .unwrap();
        shortcut.from = Some(10);
        shortcut.to = Some(20);
        let shortcut_fact = shortcut.edge.fact;
        let result = derive(&edges, &transitive());
        let spans = spans_of(&edges);
        let chain = result
            .facts
            .iter()
            .find(|fact| fact.subject == n(11) && fact.object == n(20))
            .expect("a narrower shortcut proof must also finish within 12 premises");
        assert_eq!(chain.premises.len(), MAX_DEPTH);
        assert!(chain.premises.contains(&shortcut_fact));
        assert_eq!(
            validity(&chain.premises, &spans),
            Some((Some(10), Some(20)))
        );
        let prefix = result
            .facts
            .iter()
            .filter(|fact| fact.subject == n(11) && fact.object == n(16))
            .collect::<Vec<_>>();
        assert_eq!(
            prefix.len(),
            1,
            "窄的中间证明参与传播，但最终只展示宽的前缀"
        );
        assert_eq!(validity(&prefix[0].premises, &spans), Some((None, None)));
    }

    /// The interval still takes the intersection, cross-predicate included.
    #[test]
    fn the_inverse_carries_the_same_span() {
        let d = derive(&[tep(P, 1, 1, 2, Some(10), Some(20))], &inverse_pair());
        assert_eq!(d.facts.len(), 1);
        let v = validity(
            &d.facts[0].premises,
            &HashMap::from([(f(1), (Some(10), Some(20)))]),
        );
        assert_eq!(
            v,
            Some((Some(10), Some(20))),
            "an inverse does not change the validity period"
        );
    }

    /// Being its own inverse = symmetric, but it should not infer a self-loop.
    #[test]
    fn a_predicate_that_is_its_own_inverse_still_refuses_self_loops() {
        let ax = HashMap::from([(
            P,
            Axioms {
                inverse_of: Some(P),
                ..Default::default()
            },
        )]);
        let d = derive(&[ep(P, 1, 1, 1)], &ax);
        assert!(
            d.facts.is_empty(),
            "the inverse of `A p A` is still `A p A`, a self loop derives nothing"
        );
    }

    // ---------- Contradictions (0017) ----------

    /// An edge with an interval on the given predicate
    fn et(pred: Uuid, fact: u8, s: u8, o: u8, from: Option<i64>, to: Option<i64>) -> TimedEdge {
        TimedEdge {
            edge: Edge {
                fact: f(fact),
                predicate: pred,
                subject: n(s),
                object: n(o),
            },
            from,
            to,
        }
    }

    fn spans_of(edges: &[TimedEdge]) -> HashMap<Uuid, (Option<i64>, Option<i64>)> {
        edges
            .iter()
            .map(|e| (e.edge.fact, (e.from, e.to)))
            .collect()
    }

    /// `ceo_of ⊑ works_at`, works_at functional: Mira's ceo_of yields works_at Acme,
    /// while the ledger says she works_at Globex — the derivation collides with an assertion, named explicitly
    #[test]
    fn a_derivation_that_breaks_functional_names_the_assertion_it_hit() {
        let ax = HashMap::from([
            (
                P,
                Axioms {
                    sub_property_of: Some(Q),
                    ..Default::default()
                },
            ),
            (
                Q,
                Axioms {
                    functional: true,
                    ..Default::default()
                },
            ),
        ]);
        let edges = [ep(P, 1, 1, 2), ep(Q, 2, 1, 3)];
        let d = derive(&edges, &ax);
        assert_eq!(d.facts.len(), 1);
        let c = contradictions(&d, &edges, &ax, &spans_of(&edges));
        assert_eq!(
            c.with_assertions,
            vec![Clash {
                derived: 0,
                axiom: Kind::Functional,
                against: f(2)
            }]
        );
        assert!(c.between_derivations.is_empty());
        assert_eq!(c.blocked(), HashSet::from([0]));
    }

    /// Non-overlapping intervals are not a contradiction: predecessor and successor
    #[test]
    fn disjoint_intervals_are_succession_and_stay_silent() {
        let ax = HashMap::from([
            (
                P,
                Axioms {
                    sub_property_of: Some(Q),
                    ..Default::default()
                },
            ),
            (
                Q,
                Axioms {
                    functional: true,
                    ..Default::default()
                },
            ),
        ]);
        let edges = [
            et(P, 1, 1, 2, Some(10), Some(20)),
            et(Q, 2, 1, 3, Some(30), None),
        ];
        let d = derive(&edges, &ax);
        let c = contradictions(&d, &edges, &ax, &spans_of(&edges));
        assert!(c.with_assertions.is_empty(), "{c:?}");
    }

    /// Symmetry vs asymmetry: `A p B` symmetrically yields `B p A`, yet p also declares asymmetric —
    /// every assertion collides with its own mirror
    #[test]
    fn a_symmetric_derivation_hits_the_asymmetric_assertion() {
        let ax = HashMap::from([(
            P,
            Axioms {
                symmetric: true,
                asymmetric: true,
                ..Default::default()
            },
        )]);
        let edges = [ep(P, 1, 1, 2)];
        let d = derive(&edges, &ax);
        let c = contradictions(&d, &edges, &ax, &spans_of(&edges));
        assert_eq!(c.with_assertions.len(), 1);
        assert_eq!(c.with_assertions[0].axiom, Kind::Asymmetry);
        assert_eq!(c.with_assertions[0].against, f(1));
    }

    /// When two derivations collide, they are aggregated by rule pair, and neither is persisted
    #[test]
    fn derivations_that_disagree_are_grouped_by_the_rules_that_made_them() {
        let ax = HashMap::from([
            (
                P,
                Axioms {
                    sub_property_of: Some(Q),
                    ..Default::default()
                },
            ),
            (
                Q,
                Axioms {
                    functional: true,
                    ..Default::default()
                },
            ),
        ]);
        // 1 ceo_of 2 and 1 ceo_of 3: two works_at facts derived from the same rule, mutually exclusive
        let edges = [ep(P, 1, 1, 2), ep(P, 2, 1, 3), ep(P, 3, 4, 5)];
        let d = derive(&edges, &ax);
        assert_eq!(d.facts.len(), 3);
        let c = contradictions(&d, &edges, &ax, &spans_of(&edges));
        assert!(c.with_assertions.is_empty());
        assert_eq!(c.between_derivations.len(), 1);
        let rc = &c.between_derivations[0];
        assert_eq!(rc.a, (P, Rule::SubProperty));
        assert_eq!(rc.b, (P, Rule::SubProperty));
        assert_eq!(rc.axiom, Kind::Functional);
        assert_eq!(rc.pairs.len(), 1);
        // The third (4 works_at 5) collides with nothing, so it is persisted as usual
        assert_eq!(c.blocked().len(), 2);
        assert!(!c.blocked().contains(&2));
    }

    /// Without axioms on a predicate, there is no contradiction to speak of
    #[test]
    fn a_predicate_without_axioms_cannot_contradict() {
        let ax = HashMap::from([(
            P,
            Axioms {
                sub_property_of: Some(Q),
                ..Default::default()
            },
        )]);
        let edges = [ep(P, 1, 1, 2), ep(Q, 2, 1, 3)];
        let d = derive(&edges, &ax);
        let c = contradictions(&d, &edges, &ax, &spans_of(&edges));
        assert_eq!(c, Contradictions::default());
    }
}
