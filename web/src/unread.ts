// Which state each conversation in the left column is in right now: being written / finished but unread by you / read.
//
// **Why a separate module rather than in Chat.** Answers do not stop when you leave (see the start of liveAnswer's
// start): the person asks something and switches to the graph page, the Chat component unmounts, and the stream keeps running. The 'done' moment
// therefore often happens when no component is mounted: putting the check in a component would mean missing the very kind of event it
// is meant to record. This subscribes to liveAnswer when the module loads, and keeps recording even when no one is looking.
//
// **"unread" is client-side and not stored in the database.** It means 'this person on this machine has not seen this one yet',
// recomputing on another machine is actually right; putting it in the conversations table would make it a cross-device state,
// which is a different thing (it would need read_at and an endpoint). Stored in localStorage: a refresh keeps it,
// clearing it only loses a marker and no content.
import { useSyncExternalStore } from "react";
import { liveAnswer } from "./liveAnswer";

const KEY = "utopia.unread";

function load(): string[] {
  try {
    const raw = localStorage.getItem(KEY);
    const v = raw ? JSON.parse(raw) : [];
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
  } catch {
    return [];
  }
}

const ids = new Set<string>(load());
const listeners = new Set<() => void>();

/* useSyncExternalStore skips re-renders by reference equality, so each of the two tables keeps its own snapshot,
   only swaps in a new object when it **really changed**: while an answer grows word by word, liveAnswer emits on every
   token emits, while these two tables do not move in that time, so the left column should not redraw. */
let unreadSnap: ReadonlySet<string> = new Set(ids);
let liveSnap: ReadonlySet<string> = new Set<string>();

function save() {
  try {
    localStorage.setItem(KEY, JSON.stringify([...ids]));
  } catch {
    /* Private mode cannot store: it still works within this session and starts from zero after a refresh */
  }
}

function emitUnread() {
  unreadSnap = new Set(ids);
  save();
  for (const l of listeners) l();
}

function same(a: ReadonlySet<string>, b: ReadonlySet<string>): boolean {
  if (a.size !== b.size) return false;
  for (const x of a) if (!b.has(x)) return false;
  return true;
}

/** The one being looked at right now. When it finishes it should not become 'unread': the person is watching it */
let viewing: string | null = null;

liveAnswer.subscribe(() => {
  const now = new Set<string>();
  for (const e of liveAnswer.get()) {
    if (e.streaming && e.conversationId) now.add(e.conversationId);
  }
  /* It was streaming on the last tick and is gone on this one = that one just finished (normal end, error, or the person pressed stop,
     all three are the same thing to the left column: it stopped moving, and you may not have been looking). liveAnswer's entry
     stays in the table after it ends (it only drops streaming), so this set difference is reliable. */
  let changed = false;
  for (const id of liveSnap) {
    if (now.has(id) || id === viewing) continue;
    if (!ids.has(id)) {
      ids.add(id);
      changed = true;
    }
  }
  if (!same(now, liveSnap)) {
    liveSnap = now;
    if (!changed) for (const l of listeners) l();
  }
  if (changed) emitUnread();
});

export const convMarks = {
  /** Cleared when the person changes: these are all the previous person's conversations. Call it after liveAnswer.clear, so that the
   *  entries still being written are not recorded as finished-unread by the subscription above. liveSnap is not cleared here: it follows
   *  liveAnswer does: once the answers are cleared it empties naturally; when only the shared copy is cleared, the remaining answers still need recording */
  clear() {
    ids.clear();
    viewing = null;
    unreadSnap = new Set();
    try {
      localStorage.removeItem(KEY);
    } catch {
      /* Same as save: if it cannot be stored, clear only memory */
    }
    for (const l of listeners) l();
  },
  unread: (): ReadonlySet<string> => unreadSnap,
  live: (): ReadonlySet<string> => liveSnap,
  subscribe(l: () => void) {
    listeners.add(l);
    return () => {
      listeners.delete(l);
    };
  },
  /** Opens one run (pass null = nobody opened, e.g. a new conversation). Opening it means it has been read */
  view(conversationId: string | null) {
    viewing = conversationId;
    if (conversationId && ids.delete(conversationId)) emitUnread();
  },
  /** When a conversation is deleted, its marker goes too; otherwise this id stays in storage forever */
  forget(conversationId: string) {
    if (ids.delete(conversationId)) emitUnread();
  },
};

/** Conversations with unread answers */
export function useUnread(): ReadonlySet<string> {
  return useSyncExternalStore(convMarks.subscribe, convMarks.unread);
}

/** Conversations being written right now */
export function useLive(): ReadonlySet<string> {
  return useSyncExternalStore(convMarks.subscribe, convMarks.live);
}
