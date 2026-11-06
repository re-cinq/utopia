import type { EntityFact, GraphNode } from "../../api";
import { parseHop, type Hints, type Hop } from "./hints";

export interface Loaded {
  entity: GraphNode;
  facts: EntityFact[];
}

export interface ChainCard {
  id: string;
  name: string;
  /** absent while this entity's own facts are not fetched */
  loaded?: Loaded;
}

export interface ChainColumn {
  hop: Hop;
  label: string;
  cards: ChainCard[];
  state: "ok" | "none" | "unloaded";
}

export function nextHop(hop: Hop, hints: Hints): Hop | null {
  const next = hints.relations.get(hop.key)?.hints.chain_next;
  return next ? parseHop(next) : null;
}

/** The hops a chain takes from a type: its `chain_first`, then each relation's `chain_next` */
export function hopsFrom(typeKey: string | null, hints: Hints, max = 6): Hop[] {
  const first = typeKey ? hints.types.get(typeKey)?.chain_first : undefined;
  const hops: Hop[] = [];
  const seen = new Set<string>();
  for (let hop = first ? parseHop(first) : null; hop && hops.length < max; hop = nextHop(hop, hints)) {
    const id = `${hop.dir}:${hop.key}`;
    if (seen.has(id)) break;
    seen.add(id);
    hops.push(hop);
  }
  return hops;
}

/** Fills each column from the cards of the one before it, over what is fetched so far */
export function chainColumns(
  startId: string,
  hops: Hop[],
  fetched: Map<string, Loaded>,
  hints: Hints,
): ChainColumn[] {
  const columns: ChainColumn[] = [];
  let previous: ChainCard[] = [{ id: startId, name: startId, loaded: fetched.get(startId) }];
  let previousUnloaded = false;
  for (const hop of hops) {
    const cards = new Map<string, ChainCard>();
    for (const card of previous) {
      for (const f of card.loaded?.facts ?? []) {
        if (f.direction !== hop.dir || f.predicate_key !== hop.key || !f.other_id) continue;
        if (cards.has(f.other_id)) continue;
        cards.set(f.other_id, { id: f.other_id, name: f.other_name ?? f.other_id, loaded: fetched.get(f.other_id) });
      }
    }
    const waiting: boolean = previousUnloaded || previous.some((c) => !c.loaded);
    const state: ChainColumn["state"] = cards.size > 0 ? "ok" : waiting ? "unloaded" : "none";
    columns.push({ hop, label: hints.relations.get(hop.key)?.label ?? hop.key, cards: [...cards.values()], state });
    previous = [...cards.values()];
    previousUnloaded = state === "unloaded";
  }
  return columns;
}

export function frontierIds(columns: ChainColumn[]): string[] {
  return columns.flatMap((c) => c.cards.filter((card) => !card.loaded).map((card) => card.id));
}
