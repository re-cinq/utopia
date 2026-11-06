import { useQueries } from "@tanstack/react-query";
import { api } from "../../api";
import type { Loaded } from "./chain";

const MAX_IDS = 60;

/** The facts of several entities, on the same cache key the entity panel reads */
export function useDetails(kbId: string, ids: string[]): Map<string, Loaded> {
  const capped = [...new Set(ids)].slice(0, MAX_IDS);
  const results = useQueries({
    queries: capped.map((id) => ({
      queryKey: ["entity", kbId, id],
      queryFn: () => api.entityDetail(kbId, id),
      staleTime: 60_000,
    })),
  });
  const loaded = new Map<string, Loaded>();
  results.forEach((r, i) => {
    if (r.data) loaded.set(capped[i], { entity: r.data.entity, facts: r.data.facts });
  });
  return loaded;
}

export function factsById(loaded: Map<string, Loaded>) {
  return new Map([...loaded].map(([id, l]) => [id, l.facts]));
}
