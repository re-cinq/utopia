// KB event stream subscription: on receipt, an event only does react-query invalidation and refetch (events carry no business data, so they are naturally idempotent).
// EventSource reconnects automatically after a disconnect; after a successful reconnect, refetch whatever was missed. This replaces the polling in Library and Review.
// Alerts also come in on this stream: a page holds only one connection for notifications (see `eventStream`).
//
// **Invalidation is batched.** When a document is extracted, every fact that lands emits a graph event,
// and previously each event invalidated once, so the graph page refetched the overview a dozen-odd times in those few seconds,
// always the same graph, with only the last one mattering. So events only record the key and wait for a window,
// then invalidate once: the events in the window earn one refetch, and the last one necessarily includes all the earlier changes.
// The window starts from the first event and is not reset midway: during continuous pushes it still flushes periodically, without waiting for pushes to stop.
// Idempotency is unchanged; it is just that "refresh on every one" became "refresh on the last one".
//
// Invalidation ignores staleTime: the keys invalidated here are set to 30 seconds in queryDefaults.ts, and this layer is what
// pushes the current base's changes to the page immediately.
import { useEffect } from "react";
import { partialMatchKey, useQueryClient, type QueryKey } from "@tanstack/react-query";
import { subscribeEvents } from "./eventStream";
import { STREAM_KEYS } from "./queryDefaults";

/** The merge window. Facts land much more closely spaced than this, so the delay is invisible to the eye */
export const SETTLE_MS = 300;

/** The full-graph overview has to count the entire base: recounting it every few seconds during a push is enough (#1296) */
export const OVERVIEW_SETTLE_MS = 3_000;

/**
 * Merges the invalidations within one window into one: `push` only records keys, and after `settleMs` from the first `push` hands them all to `invalidate`,
 * so the same key and its prefixes are invalidated only once; `flush` is for unmount and reconnect, flushing what has accumulated rather than dropping it.
 * It is extracted as a pure function so it can be tested; the subscription layer only maps event names to keys.
 */
export function createInvalidationCoalescer(
  invalidate: (key: QueryKey) => void,
  settleMs: number,
) {
  const pending = new Map<string, QueryKey>();
  let timer: ReturnType<typeof setTimeout> | null = null;
  const flush = () => {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    const keys = [...pending.values()];
    pending.clear();
    for (const key of keys) {
      // A normal graph event may already have queued ["graph", kbId, "neighborhood"], and a reconnect then adds ["graph", kbId]:
      // the prefix already covers the latter, so the request just started must not be cancelled and refetched.
      if (keys.some((prefix) => prefix.length < key.length && partialMatchKey(key, prefix))) continue;
      invalidate(key);
    }
  };
  const push = (...keys: QueryKey[]) => {
    for (const key of keys) pending.set(JSON.stringify(key), key);
    if (timer === null) timer = setTimeout(flush, settleMs);
  };
  return { push, flush };
}

export function useKbEvents(kbId: string | undefined) {
  const queryClient = useQueryClient();

  useEffect(() => {
    if (!kbId) return;
    const { push, flush } = createInvalidationCoalescer(
      (key) => queryClient.invalidateQueries({ queryKey: key }),
      SETTLE_MS,
    );
    // The overview is also handed to the fast layer when due: it rides along the window in flight, without flushing other keys early;
    // the ["graph", kbId] added by reconnection already covers it, so there is no extra fetch.
    const overview = createInvalidationCoalescer((key) => push(key), OVERVIEW_SETTLE_MS);
    const pushGraph = () => {
      push(["graph", kbId, "neighborhood"]);
      overview.push(["graph", kbId, "overview"]);
    };
    const flushAll = () => {
      overview.flush();
      flush();
    };

    const unsubscribe = subscribeEvents(
      `/api/v1/kbs/${kbId}/events`,
      {
        document: () => {
          push(["documents", kbId]);
          pushGraph();
        },
        graph: () => {
          push(["ontologyRegressions", kbId]);
          pushGraph();
        },
        // A mapping exploration run also emits review events: the Pending column has to refresh along with it
        review: () => push(["review", kbId], ["mappings", kbId], ["ontologyRegressions", kbId]),
        // A sentence extracted a fact awaiting a person's nod (0015): the confirmation card in the chat grows along with it
        pending: () => push(["pending", kbId], ["review", kbId]),
        source: () => push(["sources", kbId], ["documents", kbId]),
        // Alerts come in on this stream (#1028): a page with a base open no longer opens a separate alerts stream
        alert: () => push(["alerts"]),
      },
      () => {
        // Events are not replayed, and expiry of staleTime does not make a request by itself. After a disconnect, recovery must refetch;
        // the first normal connection is handled by the page's existing read, but if the first connection failed and then recovered, events may be missed too.
        push(...STREAM_KEYS.map((head) => [head, kbId]), ["alerts"]);
        flushAll();
      },
    );
    return () => {
      unsubscribe();
      // On unmount, flush what has accumulated rather than dropping it: coming back to the page must show fresh data
      flushAll();
    };
  }, [kbId, queryClient]);
}
