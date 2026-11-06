import type { EntityFact, EntityTypeView, GraphNode, RelationTypeView } from "../../api";
import { hintsOf } from "./hints";

/** Neutral ontology and facts shared by the hinted helpers' unit checks */
export function entityType(key: string, display_hints: Record<string, string> = {}): EntityTypeView {
  return {
    id: key, key, label: key, color: "", shape: "circle", builtin: false, parents: [], disjoint: [],
    primary_parent: null, description: "", display_hints, usage: 0,
  };
}

export function relationType(
  key: string,
  display_hints: Record<string, string> = {},
  kind: RelationTypeView["kind"] = "relation",
): RelationTypeView {
  return {
    id: key, key, label: key, temporal: "", functional: false, inverse_functional: false,
    is_transitive: false, is_symmetric: false, is_asymmetric: false, is_irreflexive: false,
    inverse_of: null, sub_property_of: null, builtin: false, description: "", kind, domains: [],
    ranges: [], qualifiers: [], datatype: null, unit: null, display_hints, usage: 0,
  };
}

export const sampleHints = hintsOf({
  entity_types: [
    entityType("box", { panel: "quote", chain_first: "out:holds" }),
    entityType("item"),
  ],
  relation_types: [
    relationType("holds", { role: "children", chain_next: "in:inside" }),
    relationType("inside", { role: "membership", chain_next: "holds" }),
    relationType("near"),
    relationType("state", { role: "status", tones: "open=ok,closed=danger" }, "attribute"),
    relationType("name", { role: "title" }, "attribute"),
    relationType("begin", { role: "span_start" }, "attribute"),
    relationType("finish", { role: "span_end" }, "attribute"),
    relationType("words", { role: "quote" }, "attribute"),
    relationType("extent", { role: "range" }, "attribute"),
    relationType("colour", {}, "attribute"),
  ],
});

export function node(id: string, type_key: string | null = "box"): GraphNode {
  return { id, name: id, type_key, type_label: type_key, color: "", shape: "circle", degree: 0, disambiguator: null };
}

export function link(
  id: string,
  predicate: string,
  other: string,
  direction: "out" | "in" = "out",
  extra: Partial<EntityFact> = {},
): EntityFact {
  return {
    id, direction, predicate_key: predicate, predicate_label: predicate, said_as: null, inferred: false,
    temporal: null, other_id: other, other_name: other, object_value: null, qualifiers: [],
    statement_qualifiers: [], valid_from: null, valid_to: null, holds_from: null, holds_to: null,
    valid_from_precision: null, valid_to_precision: null, confidence: 1, evidence_count: 1,
    stale: false, corrected: false, contested: null, last_evidence_time: null, ...extra,
  };
}

export function attr(id: string, predicate: string, value: unknown): EntityFact {
  return { ...link(id, predicate, ""), other_id: null, other_name: null, object_value: { value } };
}
