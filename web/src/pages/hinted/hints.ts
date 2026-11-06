import type { EntityFact, EntityTypeView, RelationTypeView } from "../../api";

/** The `utopia:*` literals one class or property carries (RC-008), key → value */
export type HintMap = Record<string, string>;

export interface RelationHint {
  kind: RelationTypeView["kind"];
  label: string;
  hints: HintMap;
}

export interface Hints {
  types: Map<string, HintMap>;
  relations: Map<string, RelationHint>;
}

export interface Hop {
  dir: "out" | "in";
  key: string;
}

export function hintsOf(ontology: {
  entity_types: EntityTypeView[];
  relation_types: RelationTypeView[];
}): Hints {
  return {
    types: new Map(ontology.entity_types.map((t) => [t.key, t.display_hints ?? {}])),
    relations: new Map(
      ontology.relation_types.map((r) => [
        r.key,
        { kind: r.kind, label: r.label, hints: r.display_hints ?? {} },
      ]),
    ),
  };
}

const isEmpty = (hints: HintMap | undefined) => !hints || Object.keys(hints).length === 0;

export function hasHints(typeKey: string | null, facts: EntityFact[], hints: Hints): boolean {
  if (typeKey && !isEmpty(hints.types.get(typeKey))) return true;
  return facts.some((f) => f.predicate_key && !isEmpty(hints.relations.get(f.predicate_key)?.hints));
}

export function parseHop(written: string): Hop {
  const [prefix, ...rest] = written.split(":");
  if (rest.length > 0 && (prefix === "in" || prefix === "out")) {
    return { dir: prefix, key: rest.join(":") };
  }
  return { dir: "out", key: written };
}

export function attrsOf(facts: EntityFact[]): EntityFact[] {
  return facts.filter((f) => f.other_id === null);
}

/** Keys of the properties whose `utopia:role` is `role`, optionally of one kind */
export function rolesOf(hints: Hints, role: string, kind?: RelationHint["kind"]): string[] {
  return [...hints.relations]
    .filter(([, r]) => r.hints.role === role && (!kind || r.kind === kind))
    .map(([key]) => key);
}

export function roleOf(hints: Hints, predicateKey: string | null): string | undefined {
  return predicateKey ? hints.relations.get(predicateKey)?.hints.role : undefined;
}

/** The literal a fact states, as text; empty when it has none */
export function valueOf(fact: EntityFact): string {
  const raw = fact.object_value?.value ?? fact.object_value?.summary;
  return raw === undefined || raw === null ? "" : String(raw);
}
