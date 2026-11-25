import type { QueryClient } from "@tanstack/react-query";
import { api, type User } from "./api";
import { liveAnswer } from "./liveAnswer";
import { convMarks } from "./unread";
import { kbStore, wsStore } from "./wsStore";

/** Same-tab memory: the last session (per database) and the unsent draft; restored when switching back to the page, and a new tab starts from scratch */
const LAST_PREFIX = "chat:last:";
export const lastKey = (kbId: string) => `${LAST_PREFIX}${kbId}`;
export const DRAFT_KEY = "chat:draft";

/** Who this is remembered for. Two places each keep a copy: sessionStorage holds what belongs to this tab alone (the draft,
 *  the last session, the in-memory answers), and localStorage holds what all tabs share (the unread markers, the chosen database).
 *  Remembering only in localStorage is not enough: another tab may have switched to a different person, so the shared copy now belongs to the new person, and on refresh this tab
 *  would still have to recognize that its draft belongs to the previous person. Remembering only in sessionStorage is not enough either: a newly opened tab is empty, so the
 *  same person would have to clear the unread markers and the chosen database once per tab. */
const OWNER_KEY = "utopia.account";

/** This tab alone: the answers outlive the page (liveAnswer), and the draft and last session outlive a refresh */
function clearTab() {
  liveAnswer.clear();
  sessionStorage.removeItem(DRAFT_KEY);
  // Walk backwards: deleting a key shifts the later indexes forward
  for (let i = sessionStorage.length - 1; i >= 0; i--) {
    const key = sessionStorage.key(i);
    if (key?.startsWith(LAST_PREFIX)) sessionStorage.removeItem(key);
  }
}

/** Shared by all tabs: the unread markers and the chosen database outlive closing the tab. Language and theme do not belong to any one person, so they are not in it.
 *  Call this after clearTab: drop the answers still being written, or unread will record them as "finished writing" */
function clearShared() {
  convMarks.clear();
  wsStore.clear();
  kbStore.clear();
}

/** Whoever signs in is attributed to that person; anything remembered for someone else is cleared. When the session expires and the same person logs in again,
 *  the draft and last session are still there. If nothing was remembered, treat it as a different person: after the first load following an upgrade, or after an explicit logout, the leftovers cannot be attributed to anyone,
 *  so the cost is one clear after each upgrade. A change of person can also come through a path other than this page's login form: single sign-on started from a link or a proxy
 *  is recognized when `me` is fetched next. A login in another tab is recognized when this tab next fetches `me` (`me` never expires,
 *  so in practice that is on refresh) (see fetchMe) */
export function claimFor(userId: string) {
  if (sessionStorage.getItem(OWNER_KEY) !== userId) clearTab();
  if (localStorage.getItem(OWNER_KEY) !== userId) clearShared();
  sessionStorage.setItem(OWNER_KEY, userId);
  localStorage.setItem(OWNER_KEY, userId);
}

/** Fetching `me`: every fetch checks first. Shell renders the child pages only after `me` returns,
 *  so this step is already done before Chat reads the draft */
export async function fetchMe(): Promise<User> {
  const me = await api.me();
  claimFor(me.id);
  return me;
}

/** Explicit logout: hand over everything this tab was holding for this person, and clear the query cache too */
export function clearSessionState(queryClient: QueryClient) {
  clearTab();
  clearShared();
  sessionStorage.removeItem(OWNER_KEY);
  localStorage.removeItem(OWNER_KEY);
  queryClient.clear();
}
