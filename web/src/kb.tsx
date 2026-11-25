// Current workspace/knowledge-base context: both are switchable and remembered in localStorage; a workspace without a KB gets "General" created automatically.
import { useCallback, useSyncExternalStore } from "react";
import { useLocation, useNavigate, useParams } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { STREAM_KEYS } from "./queryDefaults";
import { api, type Kb, type Workspace } from "./api";
import { kbStore, wsStore } from "./wsStore";

/** The knowledge-base id from the current path. **Pages all sit under /kb/$kbId, so read it straight from the path:**
 *  no need to wait for the base list to load, since the link says which base it is. Outside any scope (the account page, etc.),
 *  fall back to the remembered one. */
export function useKbId(): string {
  const params = useParams({ strict: false }) as { kbId?: string };
  const { kb } = useKb();
  return params.kbId ?? kb?.id ?? "";
}

/** Switching base lands on the same page, but **does not carry the page's contents with it**.
 *
 *  It used to replace the whole kbId in the path, so `/kb/A/chat/some-session` became
 *  `/kb/B/chat/some-session`: the session belongs to A, the page asked B for it, got a 404, and fell back to
 *  a new conversation. The single-slot liveAnswer used to hide this (it claimed without asking whose session it was); once keyed by base (#259),
 *  the failed claim was correct, and the 404 showed through (#261).
 *
 *  So only the first segment after kbId is kept: chat, graph, library, and so on. Deeper session ids and
 *  document ids belong to that base, so they should be dropped on a switch. The document page is one particular document, so after a switch
 *  it lands on the new base's Library. Outside the /kb scope the original behaviour is kept. */
export function samePageInKb(pathname: string, fromKbId: string, toKbId: string): string {
  const prefix = `/kb/${fromKbId}`;
  if (!pathname.startsWith(prefix)) return pathname.replace(fromKbId, toKbId);
  const section = pathname.slice(prefix.length).split("/").filter(Boolean)[0] ?? "graph";
  return `/kb/${toKbId}/${section === "doc" ? "library" : section}`;
}

export function useKb(): {
  kb: Kb | null;
  kbs: Kb[];
  workspace: Workspace | null;
  workspaces: Workspace[];
  setWorkspace: (id: string) => void;
  setKb: (id: string) => void;
  /** The workspace and base lists have both been fetched: an empty `kb` means there really are no bases, not that it is still loading */
  settled: boolean;
} {
  const queryClient = useQueryClient();
  const selectedId = useSyncExternalStore(wsStore.subscribe, wsStore.get);
  const selectedKbId = useSyncExternalStore(kbStore.subscribe, kbStore.get);

  const workspaces = useQuery({ queryKey: ["workspaces"], queryFn: api.workspaces });
  const list = workspaces.data ?? [];
  const ws = list.find((w) => w.id === selectedId) ?? list[0] ?? null;

  const kbs = useQuery({
    queryKey: ["kbs", ws?.id],
    queryFn: async () => {
      const existing = await api.kbs(ws!.id);
      if (existing.length > 0) return existing;
      // An empty workspace gets General created automatically. Creating bases is now an admin action, and non-admins get a 403,
      // so wait silently for an admin to create it (in practice the first user is an admin, so General is always there).
      // **Start empty, with no pack** (#580). It used to install schema.org here, for fear that the first batch of extracted documents would all be
      // unclassified entities. Measured against that (#580's comparison), the ontology grows out of the documents on its own, and installing a pack bought
      // ten points of type accuracy at a cost of 18k vs 2k tokens per prompt chunk. Those who want a pack pick it in the create-base dialog.
      try {
        const created = await api.createKb(ws!.id, {
          name: "General",
          ontology_packs: [],
        });
        queryClient.invalidateQueries({ queryKey: ["kbs", ws!.id] });
        return [created];
      } catch {
        return [];
      }
    },
    enabled: !!ws,
  });

  const kbList = kbs.data ?? [];
  /* **When the URL has one, the URL wins**: the two answer different questions. The address bar says "what this link points to",
     localStorage says "what I was last looking at".
     A link someone shared must beat my own memory, otherwise what opens is my base,
     with different data but an identical interface. */
  const routeParams = useParams({ strict: false }) as { kbId?: string };
  const wantedKbId = routeParams.kbId ?? selectedKbId;
  const kb = kbList.find((k) => k.id === wantedKbId) ?? kbList[0] ?? null;

  /* **Switching base is a navigation, not just a note.**
     The "URL wins" rule above is right, but the cost is that every page in scope carries kbId in its address,
     so `selectedKbId` never gets a chance to change. If only the store is written, the value changes and the component re-renders, yet
     the computed result is still the same base, so the top-bar dropdown is **entirely dead** under `/kb/$kbId/*`:
     clicking does nothing, and it only takes effect after a refresh (the home redirect reads memory).

     So the navigation is folded into `setKb` itself, rather than requiring each call site to remember to pair it with `navigate`,
     because the two places that missed it (the top-bar dropdown and the Chat scope switcher) are exactly the ones where the three correct sites
     "jump to a specific page" and bring the base along. A convention you can forget is one that will be forgotten.

     Stay on the current page: switching base on the ontology page should show the other base's ontology, not send you back to the graph.
     With no kbId in the address (the account page, etc.), only a note is recorded, since there you should not be dragged away;
     the caller decides where to go. */
  const navigate = useNavigate();
  const pathname = useLocation({ select: (l) => l.pathname });
  const currentKbId = routeParams.kbId;
  const setKb = useCallback(
    (id: string) => {
      kbStore.set(id);
      // The event stream only subscribes to the current base, so changes in the target base made while nobody was subscribed were never invalidated. These keys are also no longer
      // refetched on mount (queryDefaults.ts), so invalidate them once when switching over. The cost is the one
      // round of requests that switching base already incurs today
      for (const head of STREAM_KEYS) {
        queryClient.invalidateQueries({ queryKey: [head, id] });
      }
      if (currentKbId && currentKbId !== id) {
        navigate({ to: samePageInKb(pathname, currentKbId, id), replace: false });
      }
    },
    [navigate, pathname, currentKbId, queryClient],
  );

  return {
    kb,
    kbs: kbList,
    workspace: ws,
    workspaces: list,
    setWorkspace: wsStore.set,
    setKb,
    settled: workspaces.isFetched && (!ws || kbs.isFetched),
  };
}
