// Answers that are still being generated live outside the component.
//
// **Once you navigate away, they are gone from view.** The streaming `turns` used to be Chat's component state, and leaving
// the conversation page unmounts that component: the state is gone, but the fetch is still running, and its callbacks write into an
// already-dead component. When you come back, the component remounts and reads from the database, where the row only exists after generation ends,
// so only the question you asked is visible. Wait a while and come back and it is fine, because by then it has been saved.
//
// The server side (generation does not vanish with the connection) is a separate fix; this half fixes **whether it is visible when you come back**.
// Both are needed: the server keeps the answer, and here we keep the stream.
//
// **Keyed by session, not a singleton.** This table used to be a single slot, based on the premise "only one answer is in progress at a time".
// That premise does not hold, and Chat itself rules it out: switching base does not abort
// ("switching base should not kill an answer being written in another base"), and starting a new conversation does not abort ("starting a new run
// does not mean giving up on the previous one"), and the send guard is scoped per session (it explicitly rejects a global block on "cannot send a message").
// Those three "no abort" rules together mean two concurrent runs are a normal, reachable state, and a single slot cannot hold it: the second
// start overwrites the slot, the first run's callbacks keep writing to "the last entry in the current slot", and the two answers interleave
// character by character; whichever finishes first hides the other run's stop button early, and that run can no longer be stopped by anyone.
//
// So it became a table: whoever starts a run holds its handle, and every reader and writer is named. The `send` guard does not need to change,
// since it already asks "is this run streaming", and now that question is finally only about this run.
import { conversationsApi, type ChatStep, type Source } from "./api";
import { citedNumbers } from "./citations";

export interface Turn {
  role: "user" | "assistant";
  /** This entry's id. The question needs it to be re-answerable (#936): history from the database carries it, while the question just sent
   *  is told its id by the server's `conversation` frame */
  id?: string;
  content: string;
  steps?: ChatStep[];
  sources?: Source[];
  error?: string;
  stopped?: boolean;
}

/** The sources that the body actually cites.
 *
 *  `sources` is everything **retrieved** in this turn, not what the answer **used**: a greeting may also have run a
 *  search, so six excerpts hang under "hi" and read as if the greeting had six sources. So only the ones
 *  turned into superscript markers in the body are listed; numbering is kept as is, not renumbered, so it matches the markers in the body.
 *  `[1][2]`, `[1, 2]`, and the same with a full-width comma are all recognized; square brackets in code and links do not count, and are not drawn in the body either */
export function citedSources(turn: Turn): Source[] {
  if (!turn.sources?.length) return [];
  const cited = citedNumbers(turn.content);
  return turn.sources.filter((s) => cited.has(s.n));
}

/** Whether this answer gets the "no sources cited" marker (#547).
 *
 *  The criterion looks only at the data: once it has finished and the body cites no source at all, the marker is shown. A greeting or a refusal carrying it is harmless,
 *  but "here is what I found" with zero citations is what gives it away. If it retrieved something and cited none,
 *  it likewise counts as having no sources. A run still streaming is not marked: sources arrive incrementally, and showing then hiding the marker is worse than showing it late.
 *  An entry that only errored and said nothing is not marked either: it is not an answer, and the red error already says so */
export function answeredWithoutSources(turn: Turn, live: boolean): boolean {
  if (turn.role !== "assistant" || live) return false;
  if ((turn.error || turn.stopped) && !turn.content) return false;
  return citedSources(turn).length === 0;
}

/** Snapshot entry: plain data, for rendering. abort is not in the snapshot; rendering should not touch it along the way */
export interface Live {
  kbId: string;
  /** A new session is null until the server returns an id; kbId tells apart two new sessions that both still lack an id */
  conversationId: string | null;
  generationId: string | null;
  turns: Turn[];
  streaming: boolean;
  stopping: boolean;
  stopError?: string;
}

interface Slot {
  live: Live;
  abort: () => void;
}

const lives = new Map<string, Slot>();
const listeners = new Set<() => void>();

// Snapshot replaced whole: useSyncExternalStore skips unrelated renders by reference equality, **so no change in another run
// should alter this run's picture**; this old comment only became literally true once keying was in place.
let snapshot: readonly Live[] = [];

/* Notifications are **merged per frame**. Each token of a generation arrives one at a time; notifying on each would make React
   render once per token. Once the answer runs to thousands of characters, rendering cannot keep up with the tokens, and the text looks like it jerks out
   in bursts. 33ms per notification (30 times a second) is plenty for reading, and the render count drops by an order of magnitude.

   It uses setTimeout, not requestAnimationFrame: rAF stops when the tab is switched to the background,
   while this store explicitly supports "switch away and back again", so it would have to wait for the tab to return to the foreground before settling.

   Structural changes (start, end, claiming the real id) go through `flush` and notify immediately: they do not arrive continuously,
   and they should not wait for the next frame. */
const NOTIFY_MS = 33;
let pending: ReturnType<typeof setTimeout> | null = null;

function notify() {
  snapshot = [...lives.values()].map((s) => s.live);
  listeners.forEach((l) => l());
}

function emit() {
  if (pending) return;
  pending = setTimeout(() => {
    pending = null;
    notify();
  }, NOTIFY_MS);
}

function flush() {
  if (pending) {
    clearTimeout(pending);
    pending = null;
  }
  notify();
}

async function requestStop(slot: Slot) {
  const { kbId, conversationId, generationId } = slot.live;
  if (!conversationId || !generationId) return; // A stop intent before the first frame is re-sent by identify.
  try {
    await conversationsApi.stop(kbId, conversationId, generationId);
    // HTTP only confirms the request was received; done means generation has ended and the partial answer has been saved.
  } catch (error) {
    if (lives.get(conversationId) !== slot || !slot.live.streaming) return;
    slot.live = { ...slot.live, stopping: false, stopError: error instanceof Error ? error.message : String(error) };
    flush();
  }
}

// A new session that has not yet got an id from the server uses an internal token as a placeholder; when identify gets the real id, it remaps
let pendingSeq = 0;

export interface LiveHandle {
  /** A new session gets its id from the server: remap this entry from the placeholder token to the real id.
   *  `questionId` is the id this question was saved under, recorded on the last user message */
  identify: (conversationId: string, questionId?: string, generationId?: string) => void;
  /** Rewrites the last entry of this run (the assistant's turn). Only this one changes during generation */
  patchLast: (f: (t: Turn) => Turn) => void;
  /** Ends (normally, with an error, or when the person presses stop).
   *
   * **Not cleared.** A version did clear it, and that version had an ugly bug: on switching away the component unmounts,
   * and "hand the final result back to the component" is called on the already-dead component, which is a no-op. So
   * the store is emptied, the new component had already claimed this run earlier and will not read the database again, so switching back
   * shows the whole conversation blank, not even the question you asked.
   *
   * At that moment this is the only place still holding this content, so it is kept: only the `streaming` flag
   * is turned off. The next `begin` clears finished entries (see begin), and when switching to another session
   * the claim fails and it reads the database naturally. */
  finish: (stopped?: boolean) => void;
  /** Discards the optimistic display when the request is not accepted; it only disconnects and does not cancel server-side generation. */
  discard: () => void;
  /** streamChat's abort only exists once it returns: begin puts in a placeholder first, and swaps in the real abort later */
  setAbort: (abort: () => void) => void;
}

export const liveAnswer = {
  /** When the person changes (login, logout), the whole table is handed over, finished entries included: they all belong to the previous person.
   *  It only disconnects the reads and does not send stop: the server still finishes writing the previous person's answer and saves it. Clear the ownership first,
   *  so the callbacks fired synchronously on disconnect find no entry to claim */
  clear: () => {
    const previous = [...lives.values()];
    lives.clear();
    for (const slot of previous) slot.abort();
    flush();
  },
  /** `useSyncExternalStore` requires the same snapshot object to keep the same reference while nothing has changed */
  get: (): readonly Live[] => snapshot,
  subscribe: (l: () => void) => {
    listeners.add(l);
    return () => {
      listeners.delete(l);
    };
  },
  /** Claims "the run being viewed right now". Found by session; kbId only plays a role
      between two new sessions that both still lack an id. If not found, this run is not present, and the display falls back to the history in the database */
  entry: (kbId: string | null, conversationId: string | null): Live | null =>
    snapshot.find((e) => e.kbId === kbId && e.conversationId === conversationId) ?? null,
  /** Starts a run. A follow-up in the same session replaces the old entry with the same key, and also clears all finished entries:
   *
   * Clearing only happens when someone sends a new message; if a cleared session is opened again, its claim fails and it reads
   * the database naturally, and the content matches (the server saved it on done). Without clearing, this table would grow without bound;
   * entries still in progress are never cleared, which is the whole point of this module. */
  begin: (
    kbId: string,
    conversationId: string | null,
    turns: Turn[],
    abort: () => void,
  ): LiveHandle => {
    for (const [k, s] of lives) if (!s.live.streaming) lives.delete(k);
    let key = conversationId ?? `__pending__${++pendingSeq}`;
    const slot: Slot = {
      live: { kbId, conversationId, generationId: null, turns, streaming: true, stopping: false },
      abort,
    };
    lives.set(key, slot);
    flush();
    // A follow-up reuses the conversation key. Late callbacks from the old
    // stream must still belong to its original slot, not the replacement.
    const owned = () => (lives.get(key) === slot ? slot : undefined);
    return {
      identify: (id, questionId, generationId) => {
        const current = owned();
        if (!current) return;
        lives.delete(key);
        key = id;
        let turns = current.live.turns;
        if (questionId) {
          const at = turns.map((t) => t.role).lastIndexOf("user");
          if (at >= 0) {
            turns = [...turns];
            turns[at] = { ...turns[at], id: questionId };
          }
        }
        current.live = { ...current.live, conversationId: id, generationId: generationId ?? null, turns };
        lives.set(key, current);
        flush();
        if (current.live.stopping) void requestStop(current);
      },
      patchLast: (f) => {
        const current = owned();
        if (!current?.live.streaming || current.live.turns.length === 0) return;
        const turns = [...current.live.turns];
        turns[turns.length - 1] = f(turns[turns.length - 1]);
        current.live = { ...current.live, turns };
        emit();
      },
      finish: (stopped = false) => {
        const current = owned();
        if (!current || !current.live.streaming) return;
        const turns = stopped
          ? current.live.turns.map((turn, i, all) => i === all.length - 1 ? { ...turn, stopped: true } : turn)
          : current.live.turns;
        current.live = { ...current.live, turns, streaming: false, stopping: false, stopError: undefined };
        flush();
      },
      discard: () => {
        const current = owned();
        if (!current) return;
        current.abort();
        lives.delete(key);
        flush();
      },
      setAbort: (a) => {
        const current = owned();
        if (current) current.abort = a;
      },
    };
  },
  /** Only for the stop button: requests cancellation of this generation, keeping the connection for the server to wrap up. */
  stop: (kbId: string, conversationId: string | null) => {
    const slot = [...lives.values()].find((s) => s.live.kbId === kbId && s.live.conversationId === conversationId);
    if (!slot?.live.streaming || slot.live.stopping) return;
    slot.live = { ...slot.live, stopping: true, stopError: undefined };
    flush();
    void requestStop(slot);
  },
};
