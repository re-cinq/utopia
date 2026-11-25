//! Self-consistency of the ontology itself: **does not touch facts, only looks at definitions**.
//!
//! It is a different matter from [`crate::check`]. That layer asks "do facts conflict with definitions", whereas this layer asks
//! "is the definition itself sound". Separating them is not a taxonomy quirk: an ontology that contradicts itself would make every
//! conclusion of the fact layer suspect. If a predicate declares both symmetric and asymmetric, every antisymmetry violation reported from it
//! rests on a premise that never held. So the conclusions of this layer
//! must be shown to people **first**.
//!
//! Cheapness is a reason too: the input is only a few thousand lines of ontology, so there is no need to scan the ledger.
//!
//! **Likewise, "not declared, not checked".** Every item checked here corresponds to something written down in the ontology,
//! none of it is an assumption we make on the user's behalf.

use crate::Axioms;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Depth limit for walking up the class hierarchy. Same reason as [`crate::MAX_DEPTH`]: the ontology can contain cycles
/// (the table's CHECK only blocks self-loops; `A → B → A` gets through), so without a limit it would never terminate.
pub const MAX_ANCESTRY: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Defect {
    /// The same predicate declares both symmetric and asymmetric.
    ///
    /// In OWL these two can hold together only for an **empty** property: once there is one edge `A p B`, symmetry says
    /// `B p A` must hold, while antisymmetry says it must not. So whenever this predicate has any fact,
    /// the declaration is necessarily wrong in one of the two.
    SymmetricAndAsymmetric,
    /// Transitive + functional. OWL 2 DL explicitly forbids this (a functional property must not be declared transitive),
    /// because together they take reasoning outside the decidable fragment.
    ///
    /// It makes intuitive sense too: functional says "only one value on the subject side", transitive says "keep following the chain",
    /// and the second hop along the chain yields a second value for the same subject.
    TransitiveAndFunctional,
    /// subClassOf loops back into a cycle. The table's CHECK only blocks `A → A`.
    ///
    /// A cycle means every class on it is a subclass of the others, i.e. they are in fact the same class—yet each has its own
    /// label, description and properties, and the UI draws a row for each.
    SubclassCycle,
    /// A class declares disjointness with its own ancestor → this class **can never have instances**.
    /// It inherits its ancestor's identity, yet declares itself disjoint from it.
    DisjointWithAncestor,
    /// Two ancestors of one class are mutually disjoint → likewise, unsatisfiable.
    /// This shape is not rare under multiple inheritance: each branch is reasonable on its own, but together they contradict.
    InheritsDisjoint,
    /// A predicate declares itself as its own inverse. **Equivalent to symmetric**—reasoning still runs,
    /// only the reader has to think one more step. The hint is to rewrite it as `symmetric`, which is clearer
    InverseOfItself,
    /// `p⁻¹ = q` but `q⁻¹ = r`: the two sides point to different predicates.
    ///
    /// When loading axioms only gaps are filled, human-written ones are not overwritten (**human-written takes precedence over inferred**),
    /// so this contradiction is not silently smoothed over; it is left to be reported here.
    InverseNotMutual,
    /// subPropertyOf loops back into a cycle. Same shape as [`Defect::SubclassCycle`]:
    /// every predicate on the cycle is a sub-property of the others = they are in fact the same predicate, yet each has its own label and facts.
    SubPropertyCycle,
}

impl Defect {
    pub fn as_str(self) -> &'static str {
        match self {
            Defect::SymmetricAndAsymmetric => "symmetric_and_asymmetric",
            Defect::TransitiveAndFunctional => "transitive_and_functional",
            Defect::SubclassCycle => "subclass_cycle",
            Defect::DisjointWithAncestor => "disjoint_with_ancestor",
            Defect::InheritsDisjoint => "inherits_disjoint",
            Defect::InverseOfItself => "inverse_of_itself",
            Defect::InverseNotMutual => "inverse_not_mutual",
            Defect::SubPropertyCycle => "sub_property_cycle",
        }
    }
}

/// A self-contradiction within the ontology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OntologyDefect {
    pub kind: Defect,
    /// The object at fault: a predicate (first two kinds) or a class (last three kinds)
    pub subject: Uuid,
    /// The other side: the disjoint class. The first two kinds and cycles have no second party
    pub other: Option<Uuid>,
    /// The cycle's path (arranged by class), or the path from the class to that ancestor. Others are empty
    pub path: Vec<Uuid>,
}

/// Measure the ontology against itself, once.
///
/// `parents` is a list of `(child, parent)` pairs and `disjoint` is a list of mutually exclusive pairs—**the import side has already expanded symmetry
/// into two rows**, so both directions are seen here, and deduplication relies on an ordered key.
pub fn check_ontology(
    axioms: &HashMap<Uuid, Axioms>,
    parents: &[(Uuid, Uuid)],
    disjoint: &[(Uuid, Uuid)],
) -> Vec<OntologyDefect> {
    let mut out = Vec::new();

    // ---- Two self-contradictions on predicates. **Sort before reporting**: HashMap iteration order differs each time,
    // yet two checks of the same ontology should produce the same result
    let mut preds: Vec<(&Uuid, &Axioms)> = axioms.iter().collect();
    preds.sort_by_key(|(id, _)| **id);
    for (&pred, ax) in preds {
        if ax.symmetric && ax.asymmetric {
            out.push(OntologyDefect {
                kind: Defect::SymmetricAndAsymmetric,
                subject: pred,
                other: None,
                path: Vec::new(),
            });
        }
        if ax.transitive && (ax.functional || ax.inverse_functional) {
            out.push(OntologyDefect {
                kind: Defect::TransitiveAndFunctional,
                subject: pred,
                other: None,
                path: Vec::new(),
            });
        }
        // Being its own inverse = symmetric. **Not wrong, just a detour**—reasoning still runs,
        // but the reader of the ontology has to think one step to understand it. The hint to use `symmetric` is clearer
        if ax.inverse_of == Some(pred) {
            out.push(OntologyDefect {
                kind: Defect::InverseOfItself,
                subject: pred,
                other: None,
                path: Vec::new(),
            });
        }
        // Inverse + antisymmetric: `A p B` yields `B p A` (its own inverse), and antisymmetry says this does not hold.
        // The same contradiction as symmetric+asymmetric, reached by a different spelling
        if ax.inverse_of == Some(pred) && ax.asymmetric {
            out.push(OntologyDefect {
                kind: Defect::SymmetricAndAsymmetric,
                subject: pred,
                other: None,
                path: Vec::new(),
            });
        }
        // Both sides declare an inverse, yet point to different predicates. **Not silently made consistent at load time**
        // (`reasoning::axioms` only fills gaps and does not overwrite human-written ones), so it is reported here
        if let Some(inv) = ax.inverse_of {
            if let Some(back) = axioms.get(&inv).and_then(|a| a.inverse_of) {
                if back != pred {
                    out.push(OntologyDefect {
                        kind: Defect::InverseNotMutual,
                        subject: pred,
                        other: Some(inv),
                        path: vec![pred, inv, back],
                    });
                }
            }
        }
    }

    // ---- subPropertyOf cycle. Same shape as the subClassOf cycle:
    // every predicate on the cycle is a sub-property of the others = they are in fact the same predicate, yet each has its own label and facts
    {
        let parent_of: HashMap<Uuid, Uuid> = axioms
            .iter()
            .filter_map(|(id, ax)| ax.sub_property_of.map(|p| (*id, p)))
            .collect();
        let mut starts: Vec<Uuid> = parent_of.keys().copied().collect();
        starts.sort();
        let mut reported: HashSet<Uuid> = HashSet::new();
        for start in starts {
            if reported.contains(&start) {
                continue;
            }
            let mut seen: Vec<Uuid> = Vec::new();
            let mut cur = start;
            for _ in 0..MAX_ANCESTRY {
                if seen.contains(&cur) {
                    // It walked into a cycle elsewhere, and start is not on it: leave it to the cycle's own members to report in their round
                    if cur != start {
                        break;
                    }
                    // Every member on the cycle is marked, so the whole cycle is reported once
                    for m in &seen {
                        reported.insert(*m);
                    }
                    out.push(OntologyDefect {
                        kind: Defect::SubPropertyCycle,
                        subject: start,
                        other: None,
                        path: seen.clone(),
                    });
                    break;
                }
                seen.push(cur);
                match parent_of.get(&cur) {
                    Some(&p) => cur = p,
                    None => break,
                }
            }
        }
    }

    let up = adjacency(parents);
    out.extend(subclass_cycles(&up));
    out.extend(unsatisfiable(&up, disjoint));
    out
}

/// Child → parent adjacency list. A repeated declaration of the same pair is kept only once.
fn adjacency(parents: &[(Uuid, Uuid)]) -> HashMap<Uuid, Vec<Uuid>> {
    let mut up: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for &(child, parent) in parents {
        let slot = up.entry(child).or_default();
        if !slot.contains(&parent) {
            slot.push(parent);
        }
    }
    // Sorting guarantees the same ontology yields the same path every time
    for v in up.values_mut() {
        v.sort();
    }
    up
}

/// subClassOf cycles. Each cycle is reported only once, keyed by the sorted set of classes on it.
fn subclass_cycles(up: &HashMap<Uuid, Vec<Uuid>>) -> Vec<OntologyDefect> {
    let mut reported: HashSet<Vec<Uuid>> = HashSet::new();
    let mut out = Vec::new();
    let mut starts: Vec<Uuid> = up.keys().copied().collect();
    starts.sort();
    for start in starts {
        let mut path = Vec::new();
        let mut on_path = HashSet::new();
        climb(
            start,
            start,
            up,
            &mut path,
            &mut on_path,
            &mut reported,
            &mut out,
        );
    }
    out
}

fn climb(
    start: Uuid,
    at: Uuid,
    up: &HashMap<Uuid, Vec<Uuid>>,
    path: &mut Vec<Uuid>,
    on_path: &mut HashSet<Uuid>,
    reported: &mut HashSet<Vec<Uuid>>,
    out: &mut Vec<OntologyDefect>,
) {
    if path.len() >= MAX_ANCESTRY {
        return;
    }
    let Some(ups) = up.get(&at) else {
        return;
    };
    for &parent in ups {
        if parent == start && !path.is_empty() {
            // Back to the start: this is a cycle. `path` at this point holds the classes after start
            let mut ring = vec![start];
            ring.extend(path.iter().copied());
            let mut key = ring.clone();
            key.sort();
            key.dedup();
            if reported.insert(key) {
                out.push(OntologyDefect {
                    kind: Defect::SubclassCycle,
                    subject: start,
                    other: None,
                    path: ring,
                });
            }
            continue;
        }
        if on_path.contains(&parent) || parent == start {
            // A cycle elsewhere: leave it to report in its own round; here we just do not walk into it
            continue;
        }
        path.push(parent);
        on_path.insert(parent);
        climb(start, parent, up, path, on_path, reported, out);
        on_path.remove(&parent);
        path.pop();
    }
}

/// Unsatisfiable class: a mutually exclusive pair appears in its set of ancestors.
///
/// The two shapes are reported separately, because **the message to the person differs**: being disjoint from one's own ancestor is "this disjoint
/// declaration is written backwards", whereas two ancestors being disjoint is "this class should not hang under both branches at once".
fn unsatisfiable(up: &HashMap<Uuid, Vec<Uuid>>, disjoint: &[(Uuid, Uuid)]) -> Vec<OntologyDefect> {
    let pairs: HashSet<(Uuid, Uuid)> = disjoint.iter().copied().collect();
    if pairs.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut classes: Vec<Uuid> = up.keys().copied().collect();
    classes.sort();
    for class in classes {
        let anc = ancestors(class, up);
        // 1. Disjoint from its own ancestor
        for &a in &anc {
            if pairs.contains(&(class, a)) {
                out.push(OntologyDefect {
                    kind: Defect::DisjointWithAncestor,
                    subject: class,
                    other: Some(a),
                    path: Vec::new(),
                });
            }
        }
        // 2. Two ancestors mutually disjoint. Ordered-pair deduplication, otherwise a disjoint expanded into two rows would be reported twice
        let mut sorted: Vec<Uuid> = anc.iter().copied().collect();
        sorted.sort();
        for (i, &a) in sorted.iter().enumerate() {
            for &b in &sorted[i + 1..] {
                if pairs.contains(&(a, b)) {
                    out.push(OntologyDefect {
                        kind: Defect::InheritsDisjoint,
                        subject: class,
                        other: Some(b),
                        path: vec![a],
                    });
                }
            }
        }
    }
    out
}

/// All ancestors of a class (excluding itself). Even with a cycle it cannot fail to terminate—`seen` blocks it.
fn ancestors(class: Uuid, up: &HashMap<Uuid, Vec<Uuid>>) -> HashSet<Uuid> {
    let mut seen = HashSet::new();
    let mut queue = vec![(class, 0usize)];
    while let Some((at, depth)) = queue.pop() {
        if depth >= MAX_ANCESTRY {
            continue;
        }
        let Some(ups) = up.get(&at) else {
            continue;
        };
        for &parent in ups {
            if parent != class && seen.insert(parent) {
                queue.push((parent, depth + 1));
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(i: u8) -> Uuid {
        Uuid::from_bytes([i; 16])
    }
    fn kinds(v: &[OntologyDefect]) -> Vec<Defect> {
        let mut k: Vec<Defect> = v.iter().map(|d| d.kind).collect();
        k.sort();
        k
    }
    fn ax(f: impl Fn(&mut Axioms)) -> HashMap<Uuid, Axioms> {
        let mut a = Axioms::default();
        f(&mut a);
        HashMap::from([(c(99), a)])
    }

    #[test]
    fn a_property_cannot_be_both_symmetric_and_asymmetric() {
        let a = ax(|a| {
            a.symmetric = true;
            a.asymmetric = true;
        });
        let d = check_ontology(&a, &[], &[]);
        assert_eq!(kinds(&d), vec![Defect::SymmetricAndAsymmetric]);
        assert_eq!(d[0].subject, c(99));
    }

    #[test]
    fn either_one_alone_is_fine() {
        assert!(check_ontology(&ax(|a| a.symmetric = true), &[], &[]).is_empty());
        assert!(check_ontology(&ax(|a| a.asymmetric = true), &[], &[]).is_empty());
        // Transitive + antisymmetric is **normal**—it is exactly the precondition that makes cycle detection meaningful
        let both = ax(|a| {
            a.transitive = true;
            a.asymmetric = true;
        });
        assert!(check_ontology(&both, &[], &[]).is_empty());
    }

    #[test]
    fn transitive_and_functional_is_forbidden_by_owl2() {
        let a = ax(|a| {
            a.transitive = true;
            a.functional = true;
        });
        assert_eq!(
            kinds(&check_ontology(&a, &[], &[])),
            vec![Defect::TransitiveAndFunctional]
        );
        // Inverse functionality is the same
        let b = ax(|a| {
            a.transitive = true;
            a.inverse_functional = true;
        });
        assert_eq!(
            kinds(&check_ontology(&b, &[], &[])),
            vec![Defect::TransitiveAndFunctional]
        );
    }

    #[test]
    fn subclass_of_can_form_a_ring() {
        let none = HashMap::new();
        // 1 → 2 → 3 → 1
        let ring = [(c(1), c(2)), (c(2), c(3)), (c(3), c(1))];
        let d = check_ontology(&none, &ring, &[]);
        assert_eq!(kinds(&d), vec![Defect::SubclassCycle], "三个类绕成一圈");
        assert_eq!(d.len(), 1, "同一个环只报一次，不是每个类各报一次");
        assert_eq!(d[0].path.len(), 3, "路径要带上环上全部三个类");
    }

    #[test]
    fn a_tree_is_not_a_ring() {
        let none = HashMap::new();
        // Multiple parents are not a cycle either: 4 hangs under both 2 and 3
        let tree = [(c(1), c(2)), (c(1), c(3)), (c(4), c(2)), (c(4), c(3))];
        assert!(check_ontology(&none, &tree, &[]).is_empty());
    }

    #[test]
    fn a_class_disjoint_with_its_own_ancestor_can_never_exist() {
        let none = HashMap::new();
        let parents = [(c(1), c(2)), (c(2), c(3))];
        // The import side expands disjoint's symmetry into two rows, and here too two rows are given
        let dis = [(c(1), c(3)), (c(3), c(1))];
        let d = check_ontology(&none, &parents, &dis);
        assert_eq!(kinds(&d), vec![Defect::DisjointWithAncestor]);
        assert_eq!(d[0].subject, c(1));
        assert_eq!(d[0].other, Some(c(3)), "指出跟哪个祖先互斥——人要去改那一条");
    }

    #[test]
    fn two_disjoint_ancestors_make_a_class_unsatisfiable() {
        let none = HashMap::new();
        // 1 is a subclass of both 2 and 3, and 2 and 3 are disjoint
        let parents = [(c(1), c(2)), (c(1), c(3))];
        let dis = [(c(2), c(3)), (c(3), c(2))];
        let d = check_ontology(&none, &parents, &dis);
        assert_eq!(kinds(&d), vec![Defect::InheritsDisjoint]);
        assert_eq!(d.len(), 1, "展开成两行的 disjoint 不该报两遍");
        assert_eq!(d[0].subject, c(1));
    }

    #[test]
    fn disjoint_between_unrelated_branches_is_the_point_of_disjoint() {
        let none = HashMap::new();
        // 2 and 3 are disjoint, while 1 hangs only under 2 and 4 only under 3—this is exactly the normal use of disjoint
        let parents = [(c(1), c(2)), (c(4), c(3))];
        let dis = [(c(2), c(3)), (c(3), c(2))];
        assert!(check_ontology(&none, &parents, &dis).is_empty());
    }

    #[test]
    fn a_ring_does_not_hang_the_ancestor_walk() {
        let none = HashMap::new();
        let ring = [(c(1), c(2)), (c(2), c(1))];
        let dis = [(c(1), c(2)), (c(2), c(1))];
        // Cycle + disjointness together: must report the cycle, and must not fail to terminate when walking up
        let d = check_ontology(&none, &ring, &dis);
        assert!(d.iter().any(|x| x.kind == Defect::SubclassCycle));
        assert!(d.iter().any(|x| x.kind == Defect::DisjointWithAncestor));
    }

    #[test]
    fn nothing_declared_means_nothing_reported() {
        assert!(check_ontology(&HashMap::new(), &[], &[]).is_empty());
        // Class hierarchy is complete but has no disjoint at all → no criterion
        let parents = [(c(1), c(2)), (c(2), c(3))];
        assert!(check_ontology(&HashMap::new(), &parents, &[]).is_empty());
    }
}

#[cfg(test)]
mod inverse_and_sub_property_tests {
    use super::*;

    fn c(i: u8) -> Uuid {
        Uuid::from_bytes([i; 16])
    }
    fn kinds(v: &[OntologyDefect]) -> Vec<Defect> {
        let mut k: Vec<Defect> = v.iter().map(|d| d.kind).collect();
        k.sort();
        k
    }
    fn only(id: Uuid, a: Axioms) -> HashMap<Uuid, Axioms> {
        HashMap::from([(id, a)])
    }

    /// Being its own inverse — legal but a detour; the hint is to use symmetric.
    #[test]
    fn a_predicate_that_is_its_own_inverse_should_just_say_symmetric() {
        let a = Axioms {
            inverse_of: Some(c(1)),
            ..Default::default()
        };
        let d = check_ontology(&only(c(1), a), &[], &[]);
        assert_eq!(kinds(&d), vec![Defect::InverseOfItself]);
    }

    /// Own inverse + antisymmetric = the same contradiction as symmetric+asymmetric, spelled differently.
    #[test]
    fn its_own_inverse_and_asymmetric_is_the_same_contradiction_in_disguise() {
        let a = Axioms {
            inverse_of: Some(c(1)),
            asymmetric: true,
            ..Default::default()
        };
        let d = check_ontology(&only(c(1), a), &[], &[]);
        assert!(
            kinds(&d).contains(&Defect::SymmetricAndAsymmetric),
            "**要报成同一类**：读的人不该因为写法不同就以为是两回事"
        );
    }

    /// Mutual references agree — clean, nothing should be reported.
    #[test]
    fn a_mutual_pair_is_clean() {
        let ax = HashMap::from([
            (
                c(1),
                Axioms {
                    inverse_of: Some(c(2)),
                    ..Default::default()
                },
            ),
            (
                c(2),
                Axioms {
                    inverse_of: Some(c(1)),
                    ..Default::default()
                },
            ),
        ]);
        assert!(check_ontology(&ax, &[], &[]).is_empty());
    }

    /// `p⁻¹ = q` but `q⁻¹ = r` — the two sides point to different predicates.
    ///
    /// **Loading axioms only fills gaps and does not overwrite**, so this contradiction is not silently smoothed over;
    /// it must be reported here, otherwise nowhere would mention it.
    #[test]
    fn an_inverse_that_does_not_point_back_is_reported() {
        let ax = HashMap::from([
            (
                c(1),
                Axioms {
                    inverse_of: Some(c(2)),
                    ..Default::default()
                },
            ),
            (
                c(2),
                Axioms {
                    inverse_of: Some(c(3)),
                    ..Default::default()
                },
            ),
        ]);
        let d = check_ontology(&ax, &[], &[]);
        let one = d
            .iter()
            .find(|x| x.kind == Defect::InverseNotMutual)
            .expect("该报 InverseNotMutual");
        assert_eq!(one.subject, c(1));
        assert_eq!(one.other, Some(c(2)));
        assert_eq!(one.path, vec![c(1), c(2), c(3)], "路径要说清指到哪去了");
    }

    /// subPropertyOf cycle.
    #[test]
    fn a_sub_property_ring_is_a_single_predicate_wearing_three_hats() {
        let mk = |parent: u8| Axioms {
            sub_property_of: Some(c(parent)),
            ..Default::default()
        };
        let ax = HashMap::from([(c(1), mk(2)), (c(2), mk(3)), (c(3), mk(1))]);
        let d = check_ontology(&ax, &[], &[]);
        let ring: Vec<&OntologyDefect> = d
            .iter()
            .filter(|x| x.kind == Defect::SubPropertyCycle)
            .collect();
        assert_eq!(ring.len(), 1, "**整条环只报一次**，不是每个成员报一遍");
        assert_eq!(ring[0].path.len(), 3);
    }

    /// 1 ⊑ 2 ⊑ 3 ⊑ 2: the cycle is only 2 and 3; 1 just hangs on the cycle.
    #[test]
    fn a_predicate_that_leads_into_a_ring_is_not_on_it() {
        let mk = |parent: u8| Axioms {
            sub_property_of: Some(c(parent)),
            ..Default::default()
        };
        let ax = HashMap::from([(c(1), mk(2)), (c(2), mk(3)), (c(3), mk(2))]);
        let ring: Vec<OntologyDefect> = check_ontology(&ax, &[], &[])
            .into_iter()
            .filter(|x| x.kind == Defect::SubPropertyCycle)
            .collect();
        assert_eq!(ring.len(), 1);
        assert_eq!(ring[0].subject, c(2));
        assert_eq!(ring[0].path, vec![c(2), c(3)]);
    }

    /// A chain that forms no cycle must not be falsely reported.
    #[test]
    fn a_chain_that_ends_is_not_a_ring() {
        let ax = HashMap::from([
            (
                c(1),
                Axioms {
                    sub_property_of: Some(c(2)),
                    ..Default::default()
                },
            ),
            (
                c(2),
                Axioms {
                    sub_property_of: Some(c(3)),
                    ..Default::default()
                },
            ),
        ]);
        assert!(check_ontology(&ax, &[], &[]).is_empty());
    }
}
