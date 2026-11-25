// How fresh server data stays in the cache (#518).
//
// Previously the default options were only retry and refetchOnWindowFocus, with staleTime 0: every query was stale as soon as it landed,
// and every navigation back to a page refetched it. Freshness is already pushed rather than pulled: `useKbEvents` subscribes to the current KB's
// event stream, and it invalidates the keys that need refreshing, regardless of staleTime. So the refetch on mount is a duplicate for keys the stream covers,
// and mostly waste for the rest: of 79 useQuery calls only 4 set their own staleTime, `me` is queried in 12 places for the same person,
// and six `placeholderData: (prev) => prev` are hiding the flicker that this refetch causes downstream.
//
// The keys are split into three tiers, written here in one place:
//
// - **Unchanging within one session**: me, health, deployment. Infinity. A change of person does not rely on expiry; instead the whole cache is cleared at login
//   (on a successful `Login`) and at logout (`UserMenu`). After the session expires, logging in to another account in the same tab is the only path where stale data leaks,
//   so that clearing at login must not be skipped. deployment is invalidated by its own mutation on the settings page.
// - **Covered by the event stream**: documents, graph, review, pending, sources, mappings. Current KB, immediately;
//   the stream only subscribes to the **current** KB, so it is not Infinity: after more than 30 seconds, mounting again refetches,
//   `setKb` actively invalidates the target KB by `STREAM_KEYS` when switching KBs, and an open after a dropped connection also refreshes the current KB.
//   staleTime only decides freshness; it does not poll automatically or trigger a refetch.
// - **The rest**: a global default of 15 seconds. While fresh, navigating back and forth does not refetch; once stale, mounting again refetches.
//   Keys that need to be shorter (readiness, 10 seconds) are written in their own place; only ones shorter than this are worth setting separately.
//
// `setQueryDefaults` matches by key prefix: `["graph"]` covers `["graph", kbId, …]`.
import type { QueryClient } from "@tanstack/react-query";

/** Default freshness window for the other keys */
export const DEFAULT_STALE_MS = 15_000;
/** Keys covered by the event stream: after this window, mounting again refetches; switching KB and reconnecting have their own active invalidation */
export const STREAM_STALE_MS = 30_000;

/** Key heads that the event stream invalidates (see `useKbEvents`); switching KB also invalidates the target KB by this table */
export const STREAM_KEYS = [
  "documents",
  "graph",
  "review",
  "pending",
  "sources",
  "mappings",
  "ontologyRegressions",
] as const;
/** Key heads that stay unchanged within one session; a change of person is handled by clearing the cache */
export const SESSION_KEYS = ["me", "health", "deployment"] as const;

const STALE_BY_HEAD: ReadonlyMap<string, number> = new Map<string, number>([
  ...SESSION_KEYS.map((head): [string, number] => [head, Infinity]),
  ...STREAM_KEYS.map((head): [string, number] => [head, STREAM_STALE_MS]),
]);

/** The freshness a key should use. Tests use it against the table; at runtime `applyQueryDefaults` installs it into the QueryClient */
export function staleTimeFor(queryKey: readonly unknown[]): number {
  const head = queryKey[0];
  if (typeof head !== "string") return DEFAULT_STALE_MS;
  return STALE_BY_HEAD.get(head) ?? DEFAULT_STALE_MS;
}

/** Installs the table above into the QueryClient; the global default is given when `main.tsx` creates the client */
export function applyQueryDefaults(client: QueryClient): void {
  for (const [head, staleTime] of STALE_BY_HEAD) {
    client.setQueryDefaults([head], { staleTime });
  }
}
