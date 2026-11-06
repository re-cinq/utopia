import type { ChipTone } from "../../ui";
import type { EntityFact, GraphNode } from "../../api";
import { attrsOf, roleOf, rolesOf, valueOf, type Hints } from "./hints";
import { spanOf, type LineRange } from "./span";
import { toneOf } from "./tone";

export type PanelStyle = "card" | "tree" | "quote";

export interface FactGroup {
  key: string;
  predicate: string | null;
  label: string;
  direction: "out" | "in";
  facts: EntityFact[];
  /** a qualifier on its facts has the `range` role */
  ranged: boolean;
}

export interface Crumb {
  id: string;
  name: string;
}

export interface Layout {
  panel: PanelStyle;
  title: string;
  status: { value: string; tone: ChipTone } | null;
  span: LineRange | null;
  link: string | null;
  keys: string[];
  quote: string | null;
  crumbs: Crumb[];
  childrenKey: string | null;
  groups: FactGroup[];
  chainFirst: string | null;
}

const HEADER_ROLES = new Set(["title", "status", "span_start", "span_end", "link", "key", "quote"]);
const PANELS: PanelStyle[] = ["card", "tree", "quote"];

export function groupByPredicate(facts: EntityFact[]): Omit<FactGroup, "ranged">[] {
  const groups = new Map<string, Omit<FactGroup, "ranged">>();
  for (const fact of facts) {
    const predicate = fact.predicate_key;
    const key = `${fact.direction}:${predicate ?? fact.predicate_label ?? ""}`;
    const group = groups.get(key) ?? {
      key,
      predicate,
      label: fact.predicate_label ?? predicate ?? "",
      direction: fact.direction,
      facts: [],
    };
    group.facts.push(fact);
    groups.set(key, group);
  }
  return [...groups.values()];
}

/** In-facts along a `membership` relation: the entities this one sits within */
export function crumbsOf(facts: EntityFact[], hints: Hints): Crumb[] {
  const membership = new Set(rolesOf(hints, "membership"));
  return facts
    .filter((f) => f.direction === "in" && f.other_id && membership.has(f.predicate_key ?? ""))
    .map((f) => ({ id: f.other_id!, name: f.other_name ?? f.other_id! }));
}

/** Walks membership upward from `first` through what is loaded, nearest last, at most `max` deep */
export function crumbTrail(
  first: Crumb,
  loaded: Map<string, EntityFact[]>,
  hints: Hints,
  max = 4,
): Crumb[] {
  const trail = [first];
  while (trail.length < max) {
    const facts = loaded.get(trail[0].id);
    const parent = facts ? crumbsOf(facts, hints)[0] : undefined;
    if (!parent || trail.some((c) => c.id === parent.id)) break;
    trail.unshift(parent);
  }
  return trail;
}

export function layoutFor(entity: GraphNode, facts: EntityFact[], hints: Hints): Layout {
  const typeHints = (entity.type_key && hints.types.get(entity.type_key)) || {};
  const attrs = attrsOf(facts);
  const byRole = (role: string) => attrs.find((f) => roleOf(hints, f.predicate_key) === role);
  const text = (role: string) => {
    const fact = byRole(role);
    return fact ? valueOf(fact) || null : null;
  };
  const statusFact = byRole("status");
  const statusValue = statusFact ? valueOf(statusFact) : "";
  const childrenKey =
    rolesOf(hints, "children").find((key) =>
      facts.some((f) => f.direction === "out" && f.predicate_key === key),
    ) ?? null;
  const consumed = (f: EntityFact) => {
    const role = roleOf(hints, f.predicate_key);
    if (f.other_id === null) return role !== undefined && HEADER_ROLES.has(role);
    if (role === "membership") return f.direction === "in";
    return role === "children" && f.direction === "out" && f.predicate_key === childrenKey;
  };
  const isRange = (qualifierKey: string) => hints.relations.get(qualifierKey)?.hints.role === "range";
  return {
    panel: PANELS.find((p) => p === typeHints.panel) ?? "card",
    title: text("title") ?? entity.name,
    status: statusValue
      ? { value: statusValue, tone: toneOf(statusValue, hints.relations.get(statusFact!.predicate_key!)?.hints) }
      : null,
    span: spanOf(attrs, hints),
    link: text("link"),
    keys: attrs
      .filter((f) => roleOf(hints, f.predicate_key) === "key")
      .map(valueOf)
      .filter(Boolean),
    quote: text("quote"),
    crumbs: crumbsOf(facts, hints),
    childrenKey,
    groups: groupByPredicate(facts.filter((f) => !consumed(f))).map((g) => ({
      ...g,
      ranged: g.facts.some((f) => f.qualifiers.some((q) => isRange(q.key))),
    })),
    chainFirst: typeHints.chain_first ?? null,
  };
}

/** The text of a fact's `range` qualifiers */
export function rangeTextOf(fact: EntityFact, hints: Hints): string | null {
  const q = fact.qualifiers.find((q) => hints.relations.get(q.key)?.hints.role === "range");
  const raw = q?.value?.value;
  return raw === undefined || raw === null ? null : String(raw);
}
