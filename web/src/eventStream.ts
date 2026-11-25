// Event stream for notifications: refill on reconnect after a drop, and hand the connection back after the page has been hidden for a while (#1028).
//
// Browsers allow only six HTTP/1.1 connections per origin, and an event stream takes one and holds on to it. Previously every tab
// opened two notification streams (current library events and alerts), and an answer being generated took one more: two tabs filled all six,
// Stop ordinary requests of this kind queue behind them and never go out; three idle tabs did the same. So each page now opens only one notification stream
// (the library event stream also carries alerts, see `useKbEvents`), and a page hidden for longer than `HIDDEN_RELEASE_MS` gives it up
// and reconnects when it returns to the foreground.
//
// Events are not replayed: changes during a disconnect or while released are never received, so `onRecover` runs once on reconnect to refetch.
// The first normal connection does not refetch, since the page's own reads are already fresh.

/** How long a page stays hidden before giving up the connection. A brief switch away and back keeps the stream open, so no refetch is needed */
export const HIDDEN_RELEASE_MS = 30_000;

export function subscribeEvents(
  url: string,
  handlers: Record<string, () => void>,
  onRecover: () => void,
): () => void {
  const page = typeof document === "undefined" ? null : document;
  let source: EventSource | null = null;
  let missed = false;
  let disposed = false;
  let release: ReturnType<typeof setTimeout> | null = null;

  // Late callbacks arriving after unmount are always ignored: events from the old connection must not invalidate the new page's queries
  const guarded = Object.entries(handlers).map(([type, handler]): [string, () => void] => [
    type,
    () => {
      if (!disposed) handler();
    },
  ]);
  const listeners: Record<string, () => void> = {
    ...Object.fromEntries(guarded),
    error: () => {
      if (!disposed) missed = true;
    },
    open: () => {
      if (disposed || !missed) return;
      missed = false;
      onRecover();
    },
  };
  const open = () => {
    if (disposed || source) return;
    source = new EventSource(url);
    for (const [type, listener] of Object.entries(listeners)) source.addEventListener(type, listener);
  };
  const close = () => {
    if (!source) return;
    for (const [type, listener] of Object.entries(listeners)) source.removeEventListener(type, listener);
    source.close();
    source = null;
  };
  const cancelRelease = () => {
    if (release === null) return;
    clearTimeout(release);
    release = null;
  };
  const onVisibility = () => {
    cancelRelease();
    if (page?.visibilityState !== "hidden") {
      open();
      return;
    }
    release = setTimeout(() => {
      release = null;
      if (!source) return;
      close();
      missed = true;
    }, HIDDEN_RELEASE_MS);
  };

  page?.addEventListener("visibilitychange", onVisibility);
  if (page?.visibilityState === "hidden") missed = true;
  else open();

  return () => {
    disposed = true;
    page?.removeEventListener("visibilitychange", onVisibility);
    cancelRelease();
    close();
  };
}
