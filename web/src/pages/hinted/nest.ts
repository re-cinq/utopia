import type { EntityFact } from "../../api";

export interface NestNode {
  id: string;
  name: string;
  /** "unloaded" = this entity's own facts are not fetched yet */
  children: NestNode[] | "unloaded";
}

/** The objects of `childrenKey`, nested by following the same relation on every loaded child */
export function nestBy(
  rootFacts: EntityFact[],
  childrenKey: string,
  loaded: Map<string, EntityFact[]>,
  seen: Set<string> = new Set(),
): NestNode[] {
  return rootFacts
    .filter((f) => f.direction === "out" && f.predicate_key === childrenKey && f.other_id)
    .filter((f) => !seen.has(f.other_id!))
    .map((f) => {
      const id = f.other_id!;
      const own = loaded.get(id);
      const path = new Set(seen).add(id);
      return {
        id,
        name: f.other_name ?? id,
        children: own ? nestBy(own, childrenKey, loaded, path) : "unloaded",
      };
    });
}
