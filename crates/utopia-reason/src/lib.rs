//! Consistency check: measure the facts already persisted against the axioms the ontology declares (see `docs/decisions/0002` R0).
//!
//! **Does not write the `facts` table, and does not touch the database.** This layer only judges: input is edges and axioms, output is
//! "which facts contradict each other". Reading and persisting happen in `utopia-store` / `utopia-server`.
//!
//! This split is not fussiness. The ADR says R0's value lies in "the engine's hard parts—rule representation, evaluation, termination—
//! all built and verified, with zero risk surface", and those hard parts are pure logic: without a database you can run hundreds of
//! cases, including shapes the real corpus may not easily produce (a ring of eleven nodes, a self-loop inside a ring,
//! the same pair of nodes joined by two different predicates).
//!
//! **No axiom, no basis.** Each of the four check kinds is decided by one boolean in the ontology as to whether it runs,
//! and whatever is not declared is not checked: guessing an axiom is less safe than reporting no contradiction. So a library without an ontology package
//! reports zero, which is the truth rather than a fault.

pub mod derive;
pub mod expressions;
pub mod ontology;
pub mod rules;

use derive::TimedEdge;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// One fact taking part in the check: who, which relation, and pointing to whom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub fact: Uuid,
    pub predicate: Uuid,
    pub subject: Uuid,
    pub object: Uuid,
}

/// The axioms a predicate declares. A predicate with all four flags false is not checked at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Axioms {
    pub transitive: bool,
    pub symmetric: bool,
    pub asymmetric: bool,
    pub irreflexive: bool,
    pub functional: bool,
    pub inverse_functional: bool,
    /// `p⁻¹ = q`: the inverse of this predicate. **Cross-predicate rules come from here**—
    /// `A p B` yields `B q A`, and R0's checks also look at it (being its own inverse equals symmetric)
    pub inverse_of: Option<Uuid>,
    /// `p ⊑ q`: asserted the specific, and the general holds too. Chains must be guarded against forming cycles; R0 checks that
    pub sub_property_of: Option<Uuid>,
}

impl Axioms {
    /// A predicate with none of the bits declared need not be checked: **this is about performance, and also about semantics**:
    /// with no axioms there are no criteria, so scanning it would be wasted work.
    fn says_nothing(&self) -> bool {
        *self == Axioms::default()
    }
}

/// A contradiction found by the check. Stored in `axiom_violations`—**not in `fact_conflicts`**:
/// that table asks "which pair", whereas axiom violations ask "is the error in the data or in the definition",
/// and the way out of the latter may be to change the ontology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub kind: Kind,
    /// The facts involved. **A reflexive violation has only one**—it contradicts itself and needs no second fact.
    /// For a cycle, take the first and last two facts (the ones in between are in `path`); the three mutually exclusive kinds (asymmetry, functional,
    /// inverse_functional) take the two facts with the smallest and largest id in the group.
    pub left: Uuid,
    pub right: Uuid,
    /// The full path of a cycle, arranged by fact; the mutually exclusive kinds are the whole set of facts, sorted by id; empty for a self-loop.
    /// Kept because "A→B→C→A" is far more useful than "A and C contradict"—a person has to trace it through once to know
    /// which one to retract.
    pub path: Vec<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `A p A`, and p declares irreflexive
    SelfLoop,
    /// `A p B` and `B p A`, and p declares asymmetric
    Asymmetry,
    /// `A p B p … p A`, and p declares transitive: the closure infers `A p A`
    Cycle,
    /// The same subject and predicate point to two objects in the same time span, and p declares functional
    Functional,
    /// The same object is pointed to by two subjects in the same time span, and p declares inverse_functional.
    ///
    /// Recorded apart from `Functional` (#634): the two directions used to share one kind, so a project with two leads at once
    /// was written as "should have only one value, but has two", and people looked on the subject side, which is the wrong end
    InverseFunctional,
    /// `A p B`, but A is not in p's declared domain, or B is not in range (#190 / #196).
    ///
    /// **This class is not computed by this crate**: it needs entity types and the closure of domain / range, which is
    /// something the library provides: `utopia-store::reasoning::signature_breaks` measures it with SQL. Listed here
    /// so that it goes through the same persistence, clean-up and adjudication path as the other four kinds—left and right are the same fact,
    /// the same as the reflexive kind.
    Signature,
    /// A derivation collided with an assertion (0017): the inferred `A p B` and an asserted triple in the ledger cannot coexist under p's axioms.
    /// The derivation is not persisted; this row puts it in front of a person. `left` is the asserted fact collided with,
    /// `right` is the derivation's last premise, `path` is all premises; the inferred triple itself is in
    /// `axiom_violations.detail`—it is not persisted, so it has no id to point to.
    DerivedContradiction,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::SelfLoop => "self_loop",
            Kind::Asymmetry => "asymmetry",
            Kind::Cycle => "cycle",
            Kind::Functional => "functional",
            Kind::InverseFunctional => "inverse_functional",
            Kind::Signature => "signature",
            Kind::DerivedContradiction => "derived_contradiction",
        }
    }
}

/// Depth limit for cycle detection.
///
/// **Required, not defensive programming.** 0002 measured on a real corpus: the transitive closure of `part_of`
/// grew from 185 to 828 edges and **did not converge**—the depth distribution `1:185 2:181 3:141 4:45
/// 5:52 6:40 7:52 8:40 9:52 10:40`, oscillating from layer 5 onward instead of decaying, which is the shape of a cycle.
/// Without a limit, one cycle could make evaluation never terminate.
pub const MAX_DEPTH: usize = 12;

/// Maximum number of cycles reported per predicate (#642).
///
/// Thousands of cycles are not thousands of things: they pile up in one tight cluster of mutually reachable nodes, and the root is most likely that this predicate should not
/// declare transitive, or one or two edges have their direction written backwards. Queuing them one by one into Review would only flood the queue, the same reasoning as
/// `MAX_CLASHES_PER_PREDICATE`.
pub const MAX_CYCLES_PER_PREDICATE: usize = 1000;

/// Maximum number of steps taken per predicate when searching for cycles (one step per edge examined, #642).
///
/// **The cycle count cap does not bound the work.** In a dense strongly connected cluster, the simple paths of depth within 12 are
/// exponential in number, and most never return to their start—with no step limit, one check could hang a request and consume
/// all available memory. Measured: 500 nodes, 2000 edges, 20% random back-edges; previously it did not finish within 60 seconds.
pub const MAX_CYCLE_STEPS: usize = 1_000_000;

/// The complete result of one check: the violations, and which predicates' cycles were not fully searched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checked {
    pub violations: Vec<Violation>,
    /// Predicates whose cycle search hit `MAX_CYCLES_PER_PREDICATE` or `MAX_CYCLE_STEPS` without finishing, sorted.
    /// **Must be returned to the caller**: "not fully searched" must not be reported as "none found"; persistence uses this to avoid clearing old cycles on those predicates,
    /// and the UI uses it to say "this predicate has too many cycles, look at the axioms first"
    pub cycles_capped: Vec<Uuid>,
}

/// Check this batch of edges against the axioms.
///
/// Each predicate is checked on its own: axioms hang off predicates, and edges across predicates have no basis for comparison
/// (`A part_of B` and `B produces A` holding together is not a contradiction).
///
/// **Mutually exclusive kinds require the two facts to hold at the same time** (#634). Functional, inverse_functional and
/// asymmetric all speak of "the same moment": Lin Zhao's salary changed from 28000 to
/// 32000 on 2024-02-20; the two spans abut end to end, and that is a raise. Previously this took only `Edge`, so the interval was dropped at the caller,
/// so every succession went into Review. The criterion matches `derive::contradictions`—the derivation side
/// has always judged by interval overlap, and the assertion side now aligns with it.
///
/// **A cycle requires the whole path to hold at the same moment** (#636). A is part of B in 2019–2021, from 2022 B
/// is part of A; at no moment does the graph have this cycle. Pairwise overlap is not enough: three edges can overlap pairwise yet have no common
/// moment. So the intersection is taken along the path, the same thing `derive::validity` does for premises.
///
/// Intervals are read as in `TimedEdge`: half-open `[from, to)`, with `None` meaning that side is unbounded (for constant predicates both ends
/// are `None`, so it falls back to looking only at shape). Self-loops ignore time: `A p A` is wrong at any moment.
pub fn check(edges: &[TimedEdge], axioms: &HashMap<Uuid, Axioms>) -> Vec<Violation> {
    check_all(edges, axioms).violations
}

/// Same as [`check`], additionally reporting which predicates' cycles were not fully searched (#642).
pub fn check_all(edges: &[TimedEdge], axioms: &HashMap<Uuid, Axioms>) -> Checked {
    let mut out = Vec::new();
    let mut cycles_capped = Vec::new();
    let mut by_pred: HashMap<Uuid, Vec<TimedEdge>> = HashMap::new();
    for t in edges {
        let Some(ax) = axioms.get(&t.edge.predicate) else {
            continue;
        };
        if ax.says_nothing() {
            continue;
        }
        by_pred.entry(t.edge.predicate).or_default().push(*t);
    }
    for (pred, group) in by_pred {
        let ax = axioms[&pred];
        let shapes: Vec<Edge> = group.iter().map(|t| t.edge).collect();
        if ax.irreflexive {
            out.extend(self_loops(&shapes));
        }
        if ax.asymmetric {
            out.extend(asymmetries(&group));
        }
        if ax.transitive {
            let (found, capped) = cycles(&group);
            out.extend(found);
            if capped {
                cycles_capped.push(pred);
            }
        }
        if ax.functional {
            out.extend(clashes(
                &group,
                Kind::Functional,
                |e| (e.subject, Uuid::nil()),
                |a, b| a.object != b.object,
            ));
        }
        if ax.inverse_functional {
            out.extend(clashes(
                &group,
                Kind::InverseFunctional,
                |e| (e.object, Uuid::nil()),
                |a, b| a.subject != b.subject,
            ));
        }
    }
    cycles_capped.sort();
    Checked {
        violations: out,
        cycles_capped,
    }
}

fn self_loops(edges: &[Edge]) -> Vec<Violation> {
    edges
        .iter()
        .filter(|e| e.subject == e.object)
        .map(|e| Violation {
            kind: Kind::SelfLoop,
            // The two columns are filled with the same one: it contradicts itself, and there is no second fact to point to
            left: e.fact,
            right: e.fact,
            path: Vec::new(),
        })
        .collect()
}

/// Antisymmetry: the same pair of nodes is asserted in both directions within the same time span.
///
/// The key is the unordered pair of nodes; "different" means opposite direction—the same shape as functional, so it goes through
/// the same function.
fn asymmetries(edges: &[TimedEdge]) -> Vec<Violation> {
    // Self-loops are reported by the irreflexive tier; reporting them again here in antisymmetry would be a duplicate
    let two_way: Vec<TimedEdge> = edges
        .iter()
        .filter(|t| t.edge.subject != t.edge.object)
        .copied()
        .collect();
    clashes(
        &two_way,
        Kind::Asymmetry,
        |e| (e.subject.min(e.object), e.subject.max(e.object)),
        |a, b| a.subject != b.subject,
    )
}

/// Finding cycles. **Each cycle is reported only once, and only in one shape**: the reported path is rotated so the smallest fact
/// comes first (see the comment in `walk`).
///
/// Uses depth-first search rather than semi-naive closure evaluation: both can find cycles, but closure only tells you "A derives
/// A", whereas a person wants the **path**—following `A→B→C→A` once is how you know which one to retract. Closure drops
/// exactly this.
///
/// **Searches only within strongly connected components, and each cycle is found once, starting from its smallest node** (#642). Previously every node was a start and all
/// walking all paths up to depth 12 from every node, then deduplicating by fact set. Cycles can only lie in one strongly connected component,
/// and real `part_of` is mostly a hierarchy with a few back-edges: the components are small, and most paths run on down
/// into the DAG and never return. Measured: a hierarchy 4 wide and 14 layers with one back-edge (209 edges, 256 cycles)
/// took 10 seconds before; a 20,000-node hierarchy took 8 seconds. First compute the components and walk only the edges within them; these paths never
/// take a single step. Then require that a cycle starts only from its smallest-numbered node and passes only through nodes larger than the start, so each cycle is
/// visited exactly once, and the dedup set is saved too.
///
/// In truly dense large components the cycles themselves are exponential, and these two measures cannot help: there,
/// `MAX_CYCLES_PER_PREDICATE` / `MAX_CYCLE_STEPS` stops the search, and the second return value is true. The start and each
/// node's out-edges are sorted, so where it stops depends only on the data: however many times the same data is run, or in whatever order
/// it is read in, the same batch of cycles is reported.
///
/// R1 materialized derivation needs the closure itself and builds it then; R0 needs "which edges make up the cycle".
fn cycles(edges: &[TimedEdge]) -> (Vec<Violation>, bool) {
    // Self-loops are not this function's concern (the irreflexive tier handles them), and cannot lie on a cycle of more than two nodes
    let mut next: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for t in edges.iter().filter(|t| t.edge.subject != t.edge.object) {
        next.entry(t.edge.subject).or_default().push(t.edge.object);
    }
    let mut nodes: Vec<Uuid> = next.keys().copied().collect();
    nodes.sort_unstable();
    let component = components(&nodes, &next);

    // Only edges within a component can lie on a cycle; out-edges are sorted by (object, fact), so traversal order depends only on the data
    let mut adj: HashMap<Uuid, Vec<&TimedEdge>> = HashMap::new();
    for t in edges {
        let (s, o) = (t.edge.subject, t.edge.object);
        let same = matches!((component.get(&s), component.get(&o)), (Some(a), Some(b)) if a == b);
        if s != o && same {
            adj.entry(s).or_default().push(t);
        }
    }
    for out in adj.values_mut() {
        out.sort_unstable_by_key(|t| (t.edge.object, t.edge.fact));
    }
    let mut starts: Vec<Uuid> = adj.keys().copied().collect();
    starts.sort_unstable();

    let mut search = Search {
        adj: &adj,
        path: Vec::new(),
        on_path: HashSet::new(),
        out: Vec::new(),
        steps: 0,
        capped: false,
    };
    for start in starts {
        search.walk(start, start, (None, None));
        if search.capped {
            break;
        }
    }
    (search.out, search.capped)
}

/// Strongly connected components: the component index of each node.
///
/// Tarjan, **iterative version**: the recursive version's call depth equals the longest chain, so for a hierarchy of tens of thousands of nodes
/// it would be tens of thousands of recursive frames and the stack would blow up. Here the call stack `(node, next successor to visit)` is kept manually.
fn components(nodes: &[Uuid], next: &HashMap<Uuid, Vec<Uuid>>) -> HashMap<Uuid, usize> {
    let mut index: HashMap<Uuid, usize> = HashMap::new();
    let mut low: HashMap<Uuid, usize> = HashMap::new();
    let mut on_stack: HashSet<Uuid> = HashSet::new();
    let mut stack: Vec<Uuid> = Vec::new();
    let mut component: HashMap<Uuid, usize> = HashMap::new();
    let (mut counter, mut count) = (0usize, 0usize);
    for &root in nodes {
        let Entry::Vacant(slot) = index.entry(root) else {
            continue;
        };
        slot.insert(counter);
        low.insert(root, counter);
        counter += 1;
        stack.push(root);
        on_stack.insert(root);
        let mut calls: Vec<(Uuid, usize)> = vec![(root, 0)];
        while let Some(&(v, i)) = calls.last() {
            let succ = next.get(&v).map(Vec::as_slice).unwrap_or(&[]);
            if let Some(&w) = succ.get(i) {
                if let Some(frame) = calls.last_mut() {
                    frame.1 += 1;
                }
                if let Entry::Vacant(slot) = index.entry(w) {
                    slot.insert(counter);
                    low.insert(w, counter);
                    counter += 1;
                    stack.push(w);
                    on_stack.insert(w);
                    calls.push((w, 0));
                } else if on_stack.contains(&w) {
                    let lw = index[&w];
                    if let Some(lv) = low.get_mut(&v) {
                        *lv = (*lv).min(lw);
                    }
                }
                continue;
            }
            calls.pop();
            let lv = low[&v];
            if let Some(&(parent, _)) = calls.last() {
                if let Some(lp) = low.get_mut(&parent) {
                    *lp = (*lp).min(lv);
                }
            }
            if lv == index[&v] {
                while let Some(w) = stack.pop() {
                    on_stack.remove(&w);
                    component.insert(w, count);
                    if w == v {
                        break;
                    }
                }
                count += 1;
            }
        }
    }
    component
}

/// Cycle-search state: the current path, the cycles found, how many steps were taken, and whether the limit was hit.
struct Search<'a> {
    adj: &'a HashMap<Uuid, Vec<&'a TimedEdge>>,
    path: Vec<&'a TimedEdge>,
    on_path: HashSet<Uuid>,
    out: Vec<Violation>,
    steps: usize,
    capped: bool,
}

impl<'a> Search<'a> {
    /// Walk down from `at`. `span` is the intersection of intervals of edges already traversed on the path; the starting point is the full time
    fn walk(&mut self, start: Uuid, at: Uuid, span: (Option<i64>, Option<i64>)) {
        if self.path.len() >= MAX_DEPTH || self.capped {
            return;
        }
        let adj = self.adj;
        let Some(next) = adj.get(&at) else { return };
        for &t in next {
            self.steps += 1;
            if self.steps > MAX_CYCLE_STEPS || self.out.len() >= MAX_CYCLES_PER_PREDICATE {
                self.capped = true;
                return;
            }
            let e = &t.edge;
            // Only passes through nodes larger than the start: each cycle is reached only when starting from its smallest node, and exactly once.
            // Out-edges are sorted by object; here one could binary-search to skip, but out-edges usually number only a few
            if e.object < start {
                continue;
            }
            // Attaching this edge means the path never holds in full at any moment: whether to close a cycle or go further down, there is no need.
            // An event with no date has an empty interval (0031), which is naturally pruned here
            let Some(span) = derive::overlap(span, (t.from, t.to)) else {
                continue;
            };
            if e.object == start {
                if self.path.is_empty() {
                    continue;
                }
                let mut facts: Vec<Uuid> = self.path.iter().map(|x| x.edge.fact).collect();
                facts.push(e.fact);
                // **Rotate the cycle to its canonical position before reporting.** A cycle is a loop, and reading it from any edge gives the same cycle;
                // but `axiom_violations` keys a cycle uniquely by its whole `path` (0054): a different rotation
                // is a different row. The consequence is not "one extra row" but **a person's adjudication silently stops working**: the recompute deletes
                // open rows, the adjudicated row stays, the same cycle is inserted as open under a new key, and the re-open branch fails to match
                // so it does not trigger (#618). The start is the smallest node, but not necessarily the smallest fact—rotate to start from
                // the smallest fact id, so the key becomes a function of the cycle itself
                let at = facts
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, f)| **f)
                    .map(|(i, _)| i)
                    .unwrap();
                facts.rotate_left(at);
                self.out.push(Violation {
                    kind: Kind::Cycle,
                    left: facts[0],
                    right: *facts.last().unwrap(),
                    path: facts,
                });
                continue;
            }
            if self.on_path.contains(&e.object) {
                continue;
            }
            self.on_path.insert(e.object);
            self.path.push(t);
            self.walk(start, e.object, span);
            self.path.pop();
            self.on_path.remove(&e.object);
            if self.capped {
                return;
            }
        }
    }
}

/// Mutually exclusive: under the same key, two "different" facts hold at once.
///
/// functional (key is the subject, "different" means different objects), inverse_functional (key is the object, "different" means
/// different subjects), asymmetric (key is the unordered pair of nodes, different means opposite direction) are all this shape,
/// the caller supplies the key and the "different" test.
///
/// **One report per group.** The same subject pointing to five objects within one time span is one matter; reporting every pair (ten) would only
/// say the same thing ten times. A group is a connected block of the conflict relation: a and b hold together, b and c hold together, the three
/// form one group even if a and c abut end to end. Two conflicts that occur one after another without overlap are two groups.
///
/// **Identity depends only on which facts the group contains** (#624). `axiom_violations` is unique by
/// `(kind, left_fact, right_fact)`; previously left/right took the first two encountered, and the encounter order
/// came from a HashMap and the query order—the same conflict in a different order became another row, bypassing the row a person had adjudicated,
/// the same disease as the cycle in #618. Now `path` is the sorted whole group, and left/right are its first and last.
fn clashes(
    edges: &[TimedEdge],
    kind: Kind,
    key: fn(&Edge) -> (Uuid, Uuid),
    differ: fn(&Edge, &Edge) -> bool,
) -> Vec<Violation> {
    let mut by_key: HashMap<(Uuid, Uuid), Vec<&TimedEdge>> = HashMap::new();
    for t in edges {
        // An empty interval (an event with no date, 0031) holds at no moment and conflicts with no one
        if matches!((t.from, t.to), (Some(f), Some(to)) if f >= to) {
            continue;
        }
        by_key.entry(key(&t.edge)).or_default().push(t);
    }
    let mut out = Vec::new();
    for mut group in by_key.into_values() {
        if group.len() < 2 {
            continue;
        }
        // Scan by start, keeping in hand those not yet ended: they are exactly the ones holding at the same time as this one. A long run of back-to-back
        // historical values (monthly salary) only ever keeps one or two in hand, and they are never compared pairwise.
        group.sort_by_key(|t| (t.from.unwrap_or(i64::MIN), t.edge.fact));
        let mut parent: Vec<usize> = (0..group.len()).collect();
        let mut clashed = vec![false; group.len()];
        let mut open: Vec<usize> = Vec::new();
        for i in 0..group.len() {
            let start = group[i].from.unwrap_or(i64::MIN);
            // Half-open interval: an endpoint that exactly matches this one's start has already ended—back-to-back is a succession
            open.retain(|&j| group[j].to.is_none_or(|to| to > start));
            for &j in &open {
                if differ(&group[i].edge, &group[j].edge) {
                    clashed[i] = true;
                    clashed[j] = true;
                    let (a, b) = (root(&mut parent, i), root(&mut parent, j));
                    parent[a] = b;
                }
            }
            open.push(i);
        }
        let mut blocks: HashMap<usize, Vec<Uuid>> = HashMap::new();
        for i in 0..group.len() {
            if clashed[i] {
                let r = root(&mut parent, i);
                blocks.entry(r).or_default().push(group[i].edge.fact);
            }
        }
        for mut facts in blocks.into_values() {
            facts.sort_unstable();
            facts.dedup();
            out.push(Violation {
                kind,
                left: facts[0],
                right: facts[facts.len() - 1],
                path: facts,
            });
        }
    }
    out
}

/// Union-find root lookup, compressing the path along the way
fn root(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand for building an edge. Nodes are encoded as small integers into stable uuids, so you can tell who is who when reading assertions.
    fn n(i: u8) -> Uuid {
        Uuid::from_bytes([i; 16])
    }
    fn f(i: u8) -> Uuid {
        // Segment node ids from fact ids: nodes use the low bits, facts use the high bits. `u8` holds 0..=255,
        // but a test built a 30-edge chain, and using `200 + i` would overflow
        Uuid::from_bytes([i; 16].map(|b| b ^ 0xF0))
    }
    fn e(fact: u8, s: u8, o: u8) -> Edge {
        Edge {
            fact: f(fact),
            predicate: n(99),
            subject: n(s),
            object: n(o),
        }
    }
    fn with(ax: Axioms) -> HashMap<Uuid, Axioms> {
        HashMap::from([(n(99), ax)])
    }
    /// Query without time: both ends unbounded, so any two edges hold simultaneously and only shape is checked. Most cases ask
    /// about shape; cases that ask about time build edges with `at` and call `super::check` directly
    fn check(edges: &[Edge], axioms: &HashMap<Uuid, Axioms>) -> Vec<Violation> {
        let timed: Vec<TimedEdge> = edges
            .iter()
            .map(|&edge| TimedEdge {
                edge,
                from: None,
                to: None,
            })
            .collect();
        super::check(&timed, axioms)
    }
    /// Edge with an interval, `[from, to)`
    fn at(fact: u8, s: u8, o: u8, from: Option<i64>, to: Option<i64>) -> TimedEdge {
        TimedEdge {
            edge: e(fact, s, o),
            from,
            to,
        }
    }
    /// All permutations of the input. Order is the source of this whole class of bug, so every order must be run.
    fn permutations<T: Copy>(items: &[T]) -> Vec<Vec<T>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        let mut out = Vec::new();
        for i in 0..items.len() {
            let mut rest = items.to_vec();
            let head = rest.remove(i);
            for mut tail in permutations(&rest) {
                tail.insert(0, head);
                out.push(tail);
            }
        }
        out
    }
    /// The result of one check, arranged in an order-independent shape so it can be compared
    fn shape(v: Vec<Violation>) -> Vec<(&'static str, Uuid, Uuid, Vec<Uuid>)> {
        let mut out: Vec<_> = v
            .into_iter()
            .map(|x| (x.kind.as_str(), x.left, x.right, x.path))
            .collect();
        out.sort();
        out
    }
    fn kinds(v: &[Violation]) -> Vec<Kind> {
        let mut k: Vec<Kind> = v.iter().map(|x| x.kind).collect();
        k.sort_by_key(|x| x.as_str());
        k
    }

    /// **The same cycle is always the same row.** A cycle's `left`/`right` previously took the first and last of this traversal,
    /// while the start came from the HashMap iteration order—running the same input twice could give different rotations, and
    /// `axiom_violations` is unique by `(kind, left_fact, right_fact)`, so a different rotation is a different row,
    /// bypassing the row a person had adjudicated (#618).
    ///
    /// Run a hundred times rather than once: the order is random, so one run cannot surface the problem.
    #[test]
    fn a_cycle_is_reported_the_same_way_every_time() {
        let edges = [e(1, 1, 2), e(2, 2, 3), e(3, 3, 1)];
        let ax = with(Axioms {
            transitive: true,
            ..Default::default()
        });
        let shapes: std::collections::HashSet<(Uuid, Uuid, Vec<Uuid>)> = (0..100)
            .map(|_| {
                let v: Vec<Violation> = check(&edges, &ax)
                    .into_iter()
                    .filter(|v| v.kind == Kind::Cycle)
                    .collect();
                assert_eq!(v.len(), 1, "a cycle is reported only once");
                (v[0].left, v[0].right, v[0].path.clone())
            })
            .collect();
        assert_eq!(
            shapes.len(),
            1,
            "the same cycle gave more than one shape: {shapes:?}"
        );
        // The canonical shape starts from the smallest fact; the path is still one loop
        let (left, right, path) = shapes.into_iter().next().unwrap();
        assert_eq!(left, *path.iter().min().unwrap());
        assert_eq!(left, path[0]);
        assert_eq!(right, *path.last().unwrap());
        assert_eq!(path.len(), 3);
    }

    /// **Two cycles sharing their first and last edges are two cycles** (#641). A→B starting, X→A ending, with the middle going through C in one and
    /// through D in the other: both cycles have left A→B and right X→A. Previously the library keyed on these two columns, so one
    /// overwrote the other; here it's pinned that the checker gives two, with different paths—persistence keys on the whole path
    #[test]
    fn two_cycles_sharing_their_ends_are_two_cycles() {
        let edges = [
            e(1, 1, 2),
            e(2, 2, 3),
            e(3, 2, 4),
            e(4, 3, 5),
            e(5, 4, 5),
            e(6, 5, 1),
        ];
        let v: Vec<Violation> = check(
            &edges,
            &with(Axioms {
                transitive: true,
                ..Default::default()
            }),
        )
        .into_iter()
        .filter(|v| v.kind == Kind::Cycle)
        .collect();
        let paths: HashSet<Vec<Uuid>> = v.iter().map(|c| c.path.clone()).collect();
        assert_eq!(
            paths,
            HashSet::from([vec![f(1), f(2), f(4), f(6)], vec![f(1), f(3), f(5), f(6)]])
        );
        assert!(v.iter().all(|c| (c.left, c.right) == (f(1), f(6))));
    }

    /// **A back-edge within a hierarchy does not let cycle search walk the whole hierarchy** (#642). A hierarchy DAG 4 wide and 14 layers,
    /// with one edge from layer 5 back to layer 0: there are only 256 cycles (the point at layer 0 reaches the point at layer 5 through four
    /// layers, choosing one of four each time), all in the first six layers; previously every node was a start and paths within depth 12 were walked, so one start alone had
    /// 4^12 paths, taking 10 seconds. Searching only within strongly connected components avoids those tens of thousands of paths; what is asserted here
    /// is that the result is complete and did not hit the step limit: if component pruning failed, this graph would necessarily hit the limit.
    #[test]
    fn a_back_edge_in_a_hierarchy_does_not_walk_the_hierarchy() {
        let (width, layers) = (4u16, 14u16);
        let node = |l: u16, x: u16| Uuid::from_u64_pair(0, (l * width + x) as u64);
        let mut edges = Vec::new();
        let mut fact = 0u64;
        let mut push = |s: Uuid, o: Uuid, edges: &mut Vec<TimedEdge>| {
            fact += 1;
            edges.push(TimedEdge {
                edge: Edge {
                    fact: Uuid::from_u64_pair(1, fact),
                    predicate: n(99),
                    subject: s,
                    object: o,
                },
                from: None,
                to: None,
            });
        };
        for l in 0..layers - 1 {
            for x in 0..width {
                for y in 0..width {
                    push(node(l, x), node(l + 1, y), &mut edges);
                }
            }
        }
        push(node(5, 0), node(0, 0), &mut edges);
        let checked = super::check_all(
            &edges,
            &with(Axioms {
                transitive: true,
                ..Default::default()
            }),
        );
        assert!(checked.cycles_capped.is_empty(), "should not hit the cap");
        let cycles = checked
            .violations
            .iter()
            .filter(|v| v.kind == Kind::Cycle)
            .count();
        assert_eq!(
            cycles,
            4usize.pow(4),
            "第 0 层那个点经四层、每层四选一到第 5 层那个点"
        );
    }

    /// **In a dense component, hitting the limit stops and says so** (#642); the batch of cycles reported at the stop depends only on
    /// the data, not the order it was read in, otherwise each run on the same data would show a different batch of cycles in Review.
    #[test]
    fn a_dense_loop_stops_at_the_cap_and_says_so() {
        // Twelve nodes all pointing at each other: simple cycles within depth 12 are astronomical
        let mut edges = Vec::new();
        let mut fact = 0u8;
        for s in 1..=12u8 {
            for o in 1..=12u8 {
                if s != o {
                    fact += 1;
                    edges.push(TimedEdge {
                        edge: e(fact, s, o),
                        from: None,
                        to: None,
                    });
                }
            }
        }
        let ax = with(Axioms {
            transitive: true,
            ..Default::default()
        });
        let first = super::check_all(&edges, &ax);
        assert_eq!(first.cycles_capped, vec![n(99)]);
        let cycles = first
            .violations
            .iter()
            .filter(|v| v.kind == Kind::Cycle)
            .count();
        assert!(cycles <= MAX_CYCLES_PER_PREDICATE, "reported {cycles}");
        assert!(
            cycles > 0,
            "cycles found before hitting the cap are still reported"
        );

        let mut rng = Rng(0x0642);
        for _ in 0..5 {
            let again = super::check_all(&shuffled(&mut rng, &edges), &ax);
            assert_eq!(again.cycles_capped, first.cycles_capped);
            assert_eq!(
                shape(again.violations),
                shape(first.violations.clone()),
                "撞上上限时报出的那一批环随输入顺序变了"
            );
        }
    }

    /// **Predicates with no declared axioms are never checked at all.** This is the foundation of the whole check: with no basis, no contradiction is reported.
    ///
    /// The converse holds too: a library without an ontology package loaded returns zero, which is the truth, not a fault.
    #[test]
    fn a_predicate_that_declares_nothing_is_never_checked() {
        let edges = [e(1, 1, 1), e(2, 1, 2), e(3, 2, 1)];
        assert!(check(&edges, &with(Axioms::default())).is_empty());
        // Even when the predicate is absent from axioms, it is not checked (the ontology has no such row)
        assert!(check(&edges, &HashMap::new()).is_empty());
    }

    #[test]
    fn a_self_loop_needs_irreflexive_to_be_a_problem() {
        let edges = [e(1, 1, 1)];
        assert!(check(
            &edges,
            &with(Axioms {
                transitive: true,
                ..Default::default()
            })
        )
        .is_empty());
        let v = check(
            &edges,
            &with(Axioms {
                irreflexive: true,
                ..Default::default()
            }),
        );
        assert_eq!(kinds(&v), vec![Kind::SelfLoop]);
        // Only one fact: the two columns are filled with the same id
        assert_eq!(v[0].left, v[0].right);
    }

    /// The antisymmetry tier **does not report self-loops again**. `A p A` literally also satisfies "has a reverse edge";
    /// if not blocked, a self-loop would be reported once in each tier, and a person would see two items to handle when it is really one.
    #[test]
    fn a_self_loop_is_reported_once_not_twice() {
        let edges = [e(1, 1, 1)];
        let v = check(
            &edges,
            &with(Axioms {
                irreflexive: true,
                asymmetric: true,
                ..Default::default()
            }),
        );
        assert_eq!(kinds(&v), vec![Kind::SelfLoop]);
    }

    #[test]
    fn a_pair_pointing_both_ways_needs_asymmetric() {
        let edges = [e(1, 1, 2), e(2, 2, 1)];
        assert!(check(
            &edges,
            &with(Axioms {
                transitive: true,
                ..Default::default()
            })
        )
        .iter()
        .all(|v| v.kind != Kind::Asymmetry));
        let v = check(
            &edges,
            &with(Axioms {
                asymmetric: true,
                ..Default::default()
            }),
        );
        assert_eq!(kinds(&v), vec![Kind::Asymmetry]);
        assert_eq!((v[0].left, v[0].right), (f(1), f(2)));
    }

    /// A long cycle must report the **path**, not just "the ends contradict"—a person has to trace it through to know which one to retract.
    #[test]
    fn a_long_cycle_reports_the_whole_path() {
        let edges = [e(1, 1, 2), e(2, 2, 3), e(3, 3, 4), e(4, 4, 1)];
        let v = check(
            &edges,
            &with(Axioms {
                transitive: true,
                ..Default::default()
            }),
        );
        assert_eq!(v.len(), 1, "a cycle is reported only once");
        assert_eq!(v[0].path.len(), 4, "all four edges should be in the path");
    }

    /// **The same cycle is reached n times, starting from different nodes.** Deduplication is half the reason this function exists:
    /// reporting it four times would make a person look at the same thing four times.
    #[test]
    fn one_cycle_is_one_finding_however_many_ways_in() {
        let edges = [e(1, 1, 2), e(2, 2, 3), e(3, 3, 1)];
        let v = check(
            &edges,
            &with(Axioms {
                transitive: true,
                ..Default::default()
            }),
        );
        assert_eq!(v.len(), 1);
    }

    /// Two unrelated cycles are each reported once.
    #[test]
    fn separate_cycles_stay_separate() {
        let edges = [e(1, 1, 2), e(2, 2, 1), e(3, 5, 6), e(4, 6, 5)];
        let v = check(
            &edges,
            &with(Axioms {
                transitive: true,
                ..Default::default()
            }),
        );
        assert_eq!(v.len(), 2);
    }

    /// **The depth limit is what prevents non-convergence.** 0002 measured on a real corpus that the depth of the `part_of` closure reached 10
    /// and was still oscillating; without a limit, one long chain plus one cycle could make evaluation never terminate.
    ///
    /// Build a chain longer than the limit here and then close it: it should not make the check hang. Whether the cycle is reported is
    /// secondary; **not hanging is the priority**.
    #[test]
    fn a_chain_longer_than_the_limit_still_terminates() {
        let mut edges: Vec<Edge> = (0..30).map(|i| e(i, i, i + 1)).collect();
        edges.push(e(60, 30, 0));
        let v = check(
            &edges,
            &with(Axioms {
                transitive: true,
                ..Default::default()
            }),
        );
        // Asserts that it "finished"; the length is not required
        assert!(v.len() <= 1);
    }

    #[test]
    fn functional_and_its_inverse_are_two_directions_of_one_check() {
        // The same subject points to two objects
        let out = [e(1, 1, 2), e(2, 1, 3)];
        assert_eq!(
            kinds(&check(
                &out,
                &with(Axioms {
                    functional: true,
                    ..Default::default()
                })
            )),
            vec![Kind::Functional]
        );
        assert!(check(
            &out,
            &with(Axioms {
                inverse_functional: true,
                ..Default::default()
            })
        )
        .is_empty());

        // The same object pointed to by two subjects: recorded as its own kind, not passed off as functional (#634)
        let inn = [e(1, 2, 1), e(2, 3, 1)];
        assert_eq!(
            kinds(&check(
                &inn,
                &with(Axioms {
                    inverse_functional: true,
                    ..Default::default()
                })
            )),
            vec![Kind::InverseFunctional]
        );
        assert!(check(
            &inn,
            &with(Axioms {
                functional: true,
                ..Default::default()
            })
        )
        .is_empty());
    }

    /// One subject pointing to five objects is reported once. Reporting ten (pairwise combinations) would say the same thing ten times;
    /// this one report carries all five, so a person has a button for whichever to retract.
    #[test]
    fn one_finding_per_conflicting_key_not_one_per_pair() {
        let edges = [e(1, 1, 2), e(2, 1, 3), e(3, 1, 4), e(4, 1, 5), e(5, 1, 6)];
        let v = check(
            &edges,
            &with(Axioms {
                functional: true,
                ..Default::default()
            }),
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, vec![f(1), f(2), f(3), f(4), f(5)]);
        assert_eq!((v[0].left, v[0].right), (f(1), f(5)));
    }

    /// **One conflict, however it is read in, is the same row** (#624). Previously left/right took
    /// the first two encountered: the six orders of three values gave six keys, and the two orders of antisymmetry gave two keys—and
    /// `axiom_violations` is unique by key, so a different order bypassed the row a person had adjudicated.
    ///
    /// Run all permutations rather than a hundred runs: order is the only variable here, and every one must be covered.
    #[test]
    fn the_same_clash_is_the_same_row_whatever_the_order() {
        let functional = with(Axioms {
            functional: true,
            ..Default::default()
        });
        let inverse = with(Axioms {
            inverse_functional: true,
            ..Default::default()
        });
        let asymmetric = with(Axioms {
            asymmetric: true,
            ..Default::default()
        });
        let cases: [(&[Edge], &HashMap<Uuid, Axioms>); 3] = [
            (&[e(1, 1, 2), e(2, 1, 3), e(3, 1, 4)], &functional),
            (&[e(1, 2, 1), e(2, 3, 1), e(3, 4, 1)], &inverse),
            (&[e(1, 1, 2), e(2, 2, 1), e(3, 1, 2)], &asymmetric),
        ];
        for (edges, ax) in cases {
            let shapes: HashSet<_> = permutations(edges)
                .iter()
                .map(|p| shape(check(p, ax)))
                .collect();
            assert_eq!(
                shapes.len(),
                1,
                "reordering the input produced different rows: {shapes:?}"
            );
            let only = shapes.into_iter().next().unwrap();
            assert_eq!(only.len(), 1);
            let (_, left, right, path) = &only[0];
            assert_eq!(
                path,
                &vec![f(1), f(2), f(3)],
                "the whole group, sorted by id"
            );
            assert_eq!(
                (*left, *right),
                (f(1), f(3)),
                "the head and tail are the minimum and maximum"
            );
        }
    }

    /// **A cycle must hold over its whole path at the same moment** (#636). A is part of B in 2019–2021, from 2022 B
    /// is part of A: at no moment does the graph have this cycle. Three-edge cycles are easy to misread: three edges can overlap pairwise
    /// yet have no common moment; comparing pair by pair then gives false positives, and only intersecting along the path is correct.
    #[test]
    fn a_cycle_must_hold_at_one_moment() {
        let transitive = with(Axioms {
            transitive: true,
            ..Default::default()
        });
        let cycles = |edges: &[TimedEdge]| -> Vec<Violation> {
            super::check(edges, &transitive)
                .into_iter()
                .filter(|v| v.kind == Kind::Cycle)
                .collect()
        };

        // Two edges, one after the other
        let swapped = [
            at(1, 1, 2, Some(0), Some(100)),
            at(2, 2, 1, Some(100), None),
        ];
        assert!(
            cycles(&swapped).is_empty(),
            "two segments joined end to end cannot form a cycle"
        );

        // Three edges overlap pairwise, but not all three: [0,100) ∩ [50,150) ∩ [100,200) = ∅
        let pairwise = [
            at(1, 1, 2, Some(0), Some(100)),
            at(2, 2, 3, Some(50), Some(150)),
            at(3, 3, 1, Some(100), Some(200)),
        ];
        assert!(
            cycles(&pairwise).is_empty(),
            "pairwise overlap does not mean they hold at the same time"
        );

        // All three share the span [90,100): it is a cycle, and it is still reported only once, in canonical shape
        let together = [
            at(1, 1, 2, Some(0), Some(100)),
            at(2, 2, 3, Some(50), Some(150)),
            at(3, 3, 1, Some(90), Some(200)),
        ];
        let v = cycles(&together);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, vec![f(1), f(2), f(3)]);

        // A timeless event holds at no moment, so it belongs to no cycle
        let undated = [at(1, 1, 2, Some(50), Some(50)), at(2, 2, 1, None, None)];
        assert!(cycles(&undated).is_empty());
    }

    // ---------- Differential test: on random small graphs, compare one by one against brute force ----------

    /// Deterministic pseudo-random number generator (xorshift64*). Differential tests must be reproducible: a failing graph can be
    /// recreated exactly from the same seed; no dependency is introduced for this
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A random small graph: few nodes, many edges; parallel edges, self-loops and reverse edges are all common. Endpoints fall into ten cells over 0..=60,
    /// so abutting ends, exact coincidence, empty intervals (start not earlier than end) and unbounded ones all occur often—
    /// these are exactly the boundaries
    fn random_graph(rng: &mut Rng, nodes: u64, max_edges: u64) -> Vec<TimedEdge> {
        let count = rng.below(max_edges + 1) as u8;
        let bound = |rng: &mut Rng| {
            if rng.below(4) == 0 {
                None
            } else {
                Some(rng.below(7) as i64 * 10)
            }
        };
        (1..=count)
            .map(|i| {
                let (s, o) = (rng.below(nodes) as u8 + 1, rng.below(nodes) as u8 + 1);
                let (from, to) = (bound(rng), bound(rng));
                at(i, s, o, from, to)
            })
            .collect()
    }

    fn shuffled(rng: &mut Rng, edges: &[TimedEdge]) -> Vec<TimedEdge> {
        let mut v = edges.to_vec();
        for i in (1..v.len()).rev() {
            v.swap(i, rng.below(i as u64 + 1) as usize);
        }
        v
    }

    /// Brute-force cycle finding: from every node, enumerate all circuits with no repeated node and at least two edges; a path counts only if the
    /// interval intersection along the whole path is non-empty. No pruning, no regard for traversal order; the result is collected by fact set.
    fn cycles_by_brute_force(edges: &[TimedEdge]) -> HashSet<Vec<Uuid>> {
        fn extend(
            edges: &[TimedEdge],
            start: Uuid,
            at: Uuid,
            path: &mut Vec<usize>,
            seen: &mut Vec<Uuid>,
            out: &mut HashSet<Vec<Uuid>>,
        ) {
            for (i, t) in edges.iter().enumerate() {
                if t.edge.subject != at {
                    continue;
                }
                if t.edge.object == start {
                    if path.is_empty() {
                        continue;
                    }
                    let mut all = path.clone();
                    all.push(i);
                    let holds = all.iter().try_fold((None, None), |acc, &j| {
                        derive::overlap(acc, (edges[j].from, edges[j].to))
                    });
                    if holds.is_some() {
                        let mut facts: Vec<Uuid> =
                            all.iter().map(|&j| edges[j].edge.fact).collect();
                        facts.sort();
                        out.insert(facts);
                    }
                    continue;
                }
                if seen.contains(&t.edge.object) {
                    continue;
                }
                seen.push(t.edge.object);
                path.push(i);
                extend(edges, start, t.edge.object, path, seen, out);
                path.pop();
                seen.pop();
            }
        }
        let mut out = HashSet::new();
        let starts: HashSet<Uuid> = edges.iter().map(|t| t.edge.subject).collect();
        for start in starts {
            extend(
                edges,
                start,
                start,
                &mut Vec::new(),
                &mut vec![start],
                &mut out,
            );
        }
        out
    }

    /// Brute-force grouping: compare pairwise, connect any pair with the same key, "different", and overlapping intervals, then take connected blocks
    fn groups_by_brute_force(
        edges: &[TimedEdge],
        conflict: impl Fn(&Edge, &Edge) -> bool,
    ) -> HashSet<Vec<Uuid>> {
        let n = edges.len();
        let mut parent: Vec<usize> = (0..n).collect();
        let mut clashed = vec![false; n];
        for i in 0..n {
            for j in i + 1..n {
                let (a, b) = (&edges[i], &edges[j]);
                if conflict(&a.edge, &b.edge)
                    && derive::overlap((a.from, a.to), (b.from, b.to)).is_some()
                {
                    clashed[i] = true;
                    clashed[j] = true;
                    let (x, y) = (root(&mut parent, i), root(&mut parent, j));
                    parent[x] = y;
                }
            }
        }
        let mut blocks: HashMap<usize, Vec<Uuid>> = HashMap::new();
        for i in 0..n {
            if clashed[i] {
                let r = root(&mut parent, i);
                blocks.entry(r).or_default().push(edges[i].edge.fact);
            }
        }
        blocks
            .into_values()
            .map(|mut v| {
                v.sort();
                v
            })
            .collect()
    }

    fn found(v: &[Violation], kind: Kind) -> Vec<Vec<Uuid>> {
        v.iter()
            .filter(|x| x.kind == kind)
            .map(|x| {
                let mut facts = x.path.clone();
                facts.sort();
                facts
            })
            .collect()
    }

    /// **Cycles: on thousands of random small graphs, one-by-one agreement with brute-force enumeration** (#636). Pruning is an "only once proven safe" optimization:
    /// intersections only narrow as they are computed, so a pruned branch cannot make up a real cycle. Here we do not trust the proof; we trust differential testing:
    /// the set of cycles reported by pruned depth-first search must be exactly identical to brute-force enumeration without pruning that intersects the whole path,
    /// not one more and not one fewer; each cycle is reported once, and its shape is canonical (starting from the smallest fact, first and last
    /// matching, the path really connected end to end); shuffle the input and the output stays byte-for-byte identical.
    #[test]
    fn cycles_agree_with_brute_force_on_random_graphs() {
        let transitive = with(Axioms {
            transitive: true,
            ..Default::default()
        });
        let mut rng = Rng(0x0636_C0FF_EE00_0001);
        let (mut with_cycles, mut filtered_by_time) = (0, 0);
        for round in 0..6000 {
            let (nodes, max_edges) = if round % 2 == 0 { (3, 9) } else { (4, 12) };
            let edges = random_graph(&mut rng, nodes, max_edges);
            let v = super::check(&edges, &transitive);
            let got = found(&v, Kind::Cycle);
            let unique: HashSet<Vec<Uuid>> = got.iter().cloned().collect();
            assert_eq!(
                got.len(),
                unique.len(),
                "the same cycle was reported twice: {edges:?}"
            );
            assert_eq!(
                unique,
                cycles_by_brute_force(&edges),
                "与暴力枚举不一致: {edges:?}"
            );
            if !unique.is_empty() {
                with_cycles += 1;
            }
            // Erase time from the same graph and brute-force again: there are more shape-only cycles than time-bearing ones, which shows on this graph
            // time really played a part: pruning and intersection were exercised
            let timeless: Vec<TimedEdge> = edges
                .iter()
                .map(|t| TimedEdge {
                    from: None,
                    to: None,
                    ..*t
                })
                .collect();
            if cycles_by_brute_force(&timeless).len() > unique.len() {
                filtered_by_time += 1;
            }

            let by_fact: HashMap<Uuid, &TimedEdge> =
                edges.iter().map(|t| (t.edge.fact, t)).collect();
            for c in v.iter().filter(|x| x.kind == Kind::Cycle) {
                assert_eq!(
                    c.left,
                    *c.path.iter().min().unwrap(),
                    "start from the smallest fact"
                );
                assert_eq!((c.left, c.right), (c.path[0], *c.path.last().unwrap()));
                for w in 0..c.path.len() {
                    let here = by_fact[&c.path[w]].edge;
                    let next = by_fact[&c.path[(w + 1) % c.path.len()]].edge;
                    assert_eq!(
                        here.object, next.subject,
                        "the path must really connect head to tail: {c:?}"
                    );
                }
            }

            assert_eq!(
                shape(v),
                shape(super::check(&shuffled(&mut rng, &edges), &transitive)),
                "打乱输入换了输出: {edges:?}"
            );
        }
        // The differential test only counts if it really hits cycles: if random graphs almost never form a cycle, this test proves nothing
        eprintln!("{with_cycles} 张图有环，{filtered_by_time} 张图上时间筛掉过环");
        assert!(
            with_cycles > 1000,
            "only {with_cycles} graphs have cycles, the sample is too sparse"
        );
        assert!(
            filtered_by_time > 1000,
            "只有 {filtered_by_time} 张图上时间筛掉过环，剪枝没怎么走到"
        );
    }

    /// **Mutually exclusive kinds: on thousands of random small graphs, one-by-one agreement with brute-force grouping by pairwise comparison** (#634, #624).
    /// A sweep by start point keeps in hand only what has not ended yet—this sweep avoids pairwise comparison, and is also the easiest to get wrong at the boundaries:
    /// back-to-back, unbounded starts, empty intervals, several starting at the same moment. The brute-force approach compares every pair
    /// once and then takes connected blocks; the two must produce exactly identical groups; shuffle the input and the output stays byte-for-byte identical.
    #[test]
    fn clashes_agree_with_brute_force_on_random_graphs() {
        let all = with(Axioms {
            asymmetric: true,
            functional: true,
            inverse_functional: true,
            ..Default::default()
        });
        let mut rng = Rng(0x0634_BEEF_0000_0002);
        let mut seen = [0usize; 3];
        for round in 0..4000 {
            let (nodes, max_edges) = if round % 2 == 0 { (2, 8) } else { (4, 12) };
            let edges = random_graph(&mut rng, nodes, max_edges);
            let v = super::check(&edges, &all);
            // A closure that captures nothing is a function pointer
            type Conflict = fn(&Edge, &Edge) -> bool;
            let cases: [(Kind, Conflict); 3] = [
                (Kind::Functional, |a, b| {
                    a.subject == b.subject && a.object != b.object
                }),
                (Kind::InverseFunctional, |a, b| {
                    a.object == b.object && a.subject != b.subject
                }),
                (Kind::Asymmetry, |a, b| {
                    a.subject != a.object && a.subject == b.object && a.object == b.subject
                }),
            ];
            for (k, (kind, conflict)) in cases.into_iter().enumerate() {
                let got = found(&v, kind);
                let unique: HashSet<Vec<Uuid>> = got.iter().cloned().collect();
                assert_eq!(
                    got.len(),
                    unique.len(),
                    "{kind:?} 同一组报了两次: {edges:?}"
                );
                assert_eq!(
                    unique,
                    groups_by_brute_force(&edges, conflict),
                    "{kind:?} 与暴力分组不一致: {edges:?}"
                );
                seen[k] += unique.len();
            }
            for x in v.iter().filter(|x| x.kind != Kind::Cycle) {
                let mut sorted = x.path.clone();
                sorted.sort();
                assert_eq!(x.path, sorted, "the group is sorted by id");
                assert_eq!((x.left, x.right), (x.path[0], *x.path.last().unwrap()));
            }
            assert_eq!(
                shape(v),
                shape(super::check(&shuffled(&mut rng, &edges), &all)),
                "打乱输入换了输出: {edges:?}"
            );
        }
        assert!(
            seen.iter().all(|&n| n > 500),
            "one kind is almost never hit: {seen:?}"
        );
    }

    /// **Pruning cannot lose a real cycle.** Between the same pair of nodes there are two edges: one whose interval makes the path non-intersecting, and another
    /// that does not: after the first is pruned, the second still walks out the cycle. Every order of the input must be covered—
    /// pruning happens during traversal, so the order decides which one is hit first.
    #[test]
    fn pruning_a_dead_branch_keeps_the_live_one() {
        let transitive = with(Axioms {
            transitive: true,
            ..Default::default()
        });
        let edges = [
            at(1, 1, 2, Some(0), Some(100)),
            // Does not intersect 1: the path through it is pruned
            at(2, 2, 3, Some(200), Some(300)),
            // Intersects 1: the cycle goes through this one
            at(3, 2, 3, Some(50), Some(150)),
            at(4, 3, 1, Some(60), None),
        ];
        for order in permutations(&edges) {
            let v: Vec<Violation> = super::check(&order, &transitive)
                .into_iter()
                .filter(|v| v.kind == Kind::Cycle)
                .collect();
            assert_eq!(v.len(), 1, "{order:?}");
            assert_eq!(v[0].path, vec![f(1), f(3), f(4)]);
        }
    }

    /// **Back-to-back is a succession, not a contradiction** (#634). Lin Zhao's salary `[2023-06, 2024-02)`
    /// is 28000 and `[2024-02, now)` is 32000: one person has only one salary at any moment. Previously
    /// the check ignored time, so every raise went into Review. An overlap of even one second is what counts as having two values at once.
    #[test]
    fn a_succession_is_not_a_contradiction() {
        let functional = with(Axioms {
            functional: true,
            ..Default::default()
        });
        let (jun23, feb24) = (1_685_577_600, 1_708_387_200);

        let raise = [
            at(1, 1, 2, Some(jun23), Some(feb24)),
            at(2, 1, 3, Some(feb24), None),
        ];
        assert!(
            super::check(&raise, &functional).is_empty(),
            "head and tail connected"
        );

        let overlap = [
            at(1, 1, 2, Some(jun23), Some(feb24 + 1)),
            at(2, 1, 3, Some(feb24), None),
        ];
        assert_eq!(
            kinds(&super::check(&overlap, &functional)),
            vec![Kind::Functional]
        );

        // The object side is the same: a project changed its lead, with a one-year gap in between
        let handover = [
            at(1, 2, 9, Some(jun23), Some(feb24)),
            at(2, 3, 9, Some(feb24 + 31_536_000), None),
        ];
        let inverse = with(Axioms {
            inverse_functional: true,
            ..Default::default()
        });
        assert!(super::check(&handover, &inverse).is_empty());

        // Likewise for antisymmetry: first A manages B, later B manages A
        let reversal = [
            at(1, 1, 2, Some(jun23), Some(feb24)),
            at(2, 2, 1, Some(feb24), None),
        ];
        let asymmetric = with(Axioms {
            asymmetric: true,
            ..Default::default()
        });
        assert!(super::check(&reversal, &asymmetric).is_empty());
    }

    /// An unbounded side overlaps with everything: a fact with no known start that always holds holds at the same time as any span
    /// it overlaps. An event with no date has an empty interval (0031), holds at no moment, and conflicts with no one.
    #[test]
    fn an_unbounded_span_meets_everything_and_an_empty_one_meets_nothing() {
        let functional = with(Axioms {
            functional: true,
            ..Default::default()
        });
        let always = [at(1, 1, 2, None, None), at(2, 1, 3, Some(100), Some(200))];
        assert_eq!(
            kinds(&super::check(&always, &functional)),
            vec![Kind::Functional]
        );

        let undated_event = [
            at(1, 1, 2, Some(150), Some(150)),
            at(2, 1, 3, Some(100), Some(200)),
        ];
        assert!(super::check(&undated_event, &functional).is_empty());
    }

    /// **A group is a connected block of conflicts.** a and b overlap, b and c overlap, a and c abut: the three form one group
    /// — removing b makes both conflicts vanish together, and reporting them separately would make a person think they are two matters. Two conflicts separated from each other are two groups.
    #[test]
    fn a_clash_is_a_connected_group_and_separate_clashes_stay_separate() {
        let functional = with(Axioms {
            functional: true,
            ..Default::default()
        });
        let chained = [
            at(1, 1, 2, Some(0), Some(100)),
            at(2, 1, 3, Some(50), Some(150)),
            at(3, 1, 4, Some(100), Some(200)),
        ];
        let v = super::check(&chained, &functional);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, vec![f(1), f(2), f(3)]);

        let apart = [
            at(1, 1, 2, Some(0), Some(100)),
            at(2, 1, 3, Some(50), Some(100)),
            at(3, 1, 4, Some(500), Some(600)),
            at(4, 1, 5, Some(550), Some(600)),
            // An item that overlaps with no one is not in any group
            at(5, 1, 6, Some(300), Some(400)),
        ];
        let v = shape(super::check(&apart, &functional));
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].3, vec![f(1), f(2)]);
        assert_eq!(v[1].3, vec![f(3), f(4)]);
    }

    /// The same subject pointing **to the same** object twice is not a conflict—just a repeated assertion.
    #[test]
    fn saying_the_same_thing_twice_is_not_a_contradiction() {
        let edges = [e(1, 1, 2), e(2, 1, 2)];
        assert!(check(
            &edges,
            &with(Axioms {
                functional: true,
                ..Default::default()
            })
        )
        .is_empty());
    }

    /// **Axioms hang off predicates, and edges across predicates have no basis for comparison.**
    /// `A p B` and `B q A` holding simultaneously is not a contradiction, even if p declares antisymmetry.
    #[test]
    fn axioms_do_not_leak_across_predicates() {
        let a = Edge {
            fact: f(1),
            predicate: n(90),
            subject: n(1),
            object: n(2),
        };
        let b = Edge {
            fact: f(2),
            predicate: n(91),
            subject: n(2),
            object: n(1),
        };
        let ax = HashMap::from([
            (
                n(90),
                Axioms {
                    asymmetric: true,
                    ..Default::default()
                },
            ),
            (
                n(91),
                Axioms {
                    asymmetric: true,
                    ..Default::default()
                },
            ),
        ]);
        assert!(check(&[a, b], &ax).is_empty());
    }
}
