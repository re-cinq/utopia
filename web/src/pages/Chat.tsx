/* Chat: agentic conversation (retrieval and graph tools, plus the remember memory).
   Session persistence: a session list in the left column; the server assembles the context, and the frontend only sends conversation_id plus the new message;
   the action trace (steps) and citations (sources) are saved with the messages, and history replay and the live stream share one renderer. */
import { memo, useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore } from "react";
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate, useParams } from "@tanstack/react-router";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import remend from "remend";
import {
  ArrowUp,
  BookOpen,
  ChevronDown,
  ChevronRight,
  Database,
  GitCompareArrows,
  History,
  Layers,
  MoreHorizontal,
  RotateCcw,
  Search,
  Search as SearchIcon,
  Square,
  SquarePen,
  Waypoints,
  Wrench,
} from "lucide-react";
import {
  ApiError,
  conversationsApi,
  reattachChat,
  streamChat,
  type ChatStep,
  type ConversationRow,
  type ConversationMessage,
  type Source,
} from "../api";
import { S } from "../i18n";
import { rehypeCitations } from "../citations";
import { followsBottom } from "../chatScroll";
import { copyAndSay } from "../clipboard";
import { chatMarkdown, SourceList, SourcesProvider } from "./chatCitations";
import { CopyButton } from "./chatCopy";
import { toast } from "../toast";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useKb, useKbId } from "../kb";
import {
  Button,
  cn,
  ConvMark,
  DangerConfirm,
  IconButton,
  Input,
  LinkButton,
  RAIL_CLS,
  REVEAL,
  Row,
  Textarea,
} from "../ui";
import { convMarks, useLive, useUnread } from "../unread";
import {
  answeredWithoutSources,
  citedSources,
  liveAnswer,
  type LiveHandle,
  type Turn,
} from "../liveAnswer";
import { NodCard } from "./PendingFacts";
import { stepDetail, stepLabel } from "../steps";
import { NextStep, nextStep, useReadiness } from "./NextStep";
import { KbMenuList } from "./KbMenuList";
import { DRAFT_KEY, fetchMe, lastKey } from "../authSession";

/* `Turn` is defined in liveAnswer: the run in progress is also a series of Turns,
   and it has to outlive this component (see the note at the top of that file). */

/** The turn that has no sources yet shares this one empty array: creating a new one would make the context change on every render,
 *  and every citation marker in the body would be redrawn. */
const NO_SOURCES: Source[] = [];

type ViewRequest = { kbId: string; id: string | null };
const historyTurns = (messages: ConversationMessage[]): Turn[] => messages.map((m) => ({
  role: m.role,
  id: m.id,
  content: m.content,
  steps: m.steps.length ? m.steps : undefined,
  sources: m.sources.length ? m.sources : undefined,
  stopped: m.stopped,
}));
const viewKey = (kbId: string, id: string | null) => `${kbId}/${id ?? ""}`;

/** The position on screen of the question that can be re-answered (#936), or -1 if there is none. Only the last question counts: what follows it is either nothing
 *  (as when a session is reopened) or a single failed answer (it failed halfway through).
 *  The server likewise only answers the last message of a session; earlier ones that were answered, and ones someone asked after, are not answered. */
function retryableQuestion(shown: Turn[]): number {
  const last = shown.at(-1);
  const at =
    last?.role === "assistant" && last.error
      ? shown.length - 2
      : last?.role === "user"
        ? shown.length - 1
        : -1;
  return at >= 0 && shown[at].role === "user" && shown[at].id ? at : -1;
}

export function Chat() {
  const kbId = useKbId();
  const { kb, kbs, setKb } = useKb();
  const me = useQuery({ queryKey: ["me"], queryFn: fetchMe });
  /* **Only the model tier is gated.** An empty knowledge base can still be chatted with: the mounted database can still be queried and memory can still be read;
     only with no chat model is nothing at all answerable. The greeting used to show as usual, so the user only hit the wall
     after typing the first sentence (#313). */
  const readiness = useReadiness(kbId);
  const modelStep = readiness.data?.has_chat_model
    ? null
    : nextStep(readiness.data, {
        kbId,
        isAdmin: !!me.data?.is_admin,
        canUpload: kb?.my_role !== "viewer",
      });
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  // The session is the route: /chat/$conversationId. The URL is the single source of truth for the current session (refresh and back work naturally).
  const { conversationId: routeConvId } = useParams({ strict: false }) as {
    conversationId?: string;
  };
  const [activeId, setActiveId] = useState<string | null>(null);
  // The route-sync effect's criterion: state commits later than the re-render that navigate triggers,
  // so only a synchronous write into a ref makes the "after a streaming create, only the URL changes" guard hit reliably.
  const activeIdRef = useRef<string | null>(null);
  // Whether the reader is at the bottom of the conversation (see chatScroll.ts). Switching session and sending a new message both return to "follow":
  // at that moment the person wants to see the bottom.
  const following = useRef(true);
  // Turns that have already ended are read from the database. **The one in progress is NOT here**: see below.
  const [turns, setTurns] = useState<Turn[]>([]);
  const [loadedKey, setLoadedKey] = useState<string | null>(null);
  const [idleHistoryKey, setIdleHistoryKey] = useState<string | null>(null);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [loadingHistory, setLoadingHistory] = useState(false);
  // Object identity is the viewing epoch: A → B → A creates three owners.
  // Generation handles live separately and continue after this view leaves.
  const viewRequest = useRef<ViewRequest>({ kbId, id: routeConvId ?? null });
  const claimView = (id: string | null): ViewRequest => {
    const request = { kbId, id };
    viewRequest.current = request;
    return request;
  };
  const ownsView = (request: ViewRequest) => viewRequest.current === request;
  const previousRoute = useRef(viewKey(kbId, routeConvId ?? null));
  useLayoutEffect(() => {
    const key = viewKey(kbId, routeConvId ?? null);
    if (previousRoute.current === key) return;
    previousRoute.current = key;
    // Syncing the URL for a new session also goes through here: the old send owner is invalidated and activeIdRef is cleared,
    // so route-sync calls loadConversation. onConversation has already identified the generation handle,
    // so loadConversation must check liveAnswer.entry first and claim it directly, so a database read mid-stream does not overwrite it.
    claimView(routeConvId ?? null);
    activeIdRef.current = null;
    following.current = true;
    setActiveId(routeConvId ?? null);
    setTurns([]);
    setLoadedKey(null);
    setHistoryError(null);
    setLoadingHistory(false);
  }, [kbId, routeConvId]);
  useLayoutEffect(() => () => {
    viewRequest.current = { ...viewRequest.current };
    activeIdRef.current = null; // StrictMode's next setup must issue its own read.
  }, []);
  const [input, setInput] = useState(() => sessionStorage.getItem(DRAFT_KEY) ?? "");
  /* **Claim by URL, not by state.** The top of this file says "the URL is the single source of truth for the current session",
     but here `activeId` was used once. It is state, and when you switch away and back it updates later than the first render,
     so that frame cannot recognize its own run and the screen is empty. Using the id in the address bar has no timing to speak of.
     When a new session has not yet got an id, both are empty, which also matches. */
  const currentId = routeConvId ?? activeId;
  // Runs still in progress all live outside the component (see liveAnswer.ts); here we only claim "the run being viewed"
  // from among them. Any change in another run leaves this run's snapshot reference alone. "An answer being generated elsewhere
  // should not change anything here" now also holds literally in the render layer: React skips re-renders by reference equality,
  // so another run growing character by character no longer disturbs the current session.
  const liveHere = useSyncExternalStore(
    liveAnswer.subscribe,
    () => liveAnswer.entry(kbId || null, currentId),
  );
  /* **It is "this run" that is streaming, not "a run".**
     If it were global, then while another run is generating, this run's input would also turn into a stop button and be unable to send,
     and the last turn would count as still streaming, so its citations would be hidden (that criterion is in TurnView).
     An answer being generated elsewhere should not change anything here. */
  const streaming = liveHere?.streaming ?? false;
  const shown = liveHere ? liveHere.turns : loadedKey === viewKey(kbId, currentId) ? turns : [];
  const retryAt = retryableQuestion(shown);
  const [scopeOpen, setScopeOpen] = useState(false);
  const [pendingDelete, setPendingDelete] = useState<ConversationRow | null>(null);
  // Session search. **It searches titles and the body too: people remember the question they asked, not the title.**
  const [convSearch, setConvSearch] = useState("");
  // Which row's three-dot menu is open. Only one at a time.
  // Whether the "Recent" group is collapsed. Expanded by default: the left column exists precisely to show these sessions.
  const [recentOpen, setRecentOpen] = useState(true);
  /* The marker on each session in the left column (being written / finished but not yet seen). Both tables live outside the component:
     "finished writing" often happens after this component has already unmounted (see the top of unread.ts). */
  const liveIds = useLive();
  const unreadIds = useUnread();
  const scopeRef = useRef<HTMLDivElement>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  /* Opening a run is the same as having seen it: the marker is cleared, and unread is told "this is the one being looked at":
     a run being watched should not become unread when it finishes writing. Passing null (a new conversation, or the current run was deleted)
     is equally meaningful: any run that finishes writing after that counts as unread. */
  useEffect(() => {
    convMarks.view(activeId);
  }, [activeId]);

  // Scope popover: click outside or Esc closes it (same convention as ui/Dropdown).
  useEffect(() => {
    if (!scopeOpen) return;
    const onDoc = (e: MouseEvent) => {
      const target = e.target as Element;
      // The group's submenu is attached to body, not inside scopeRef, so clicking it does not count as a click outside (KbMenuList).
      if (!scopeRef.current?.contains(target) && !target.closest?.("[data-kb-submenu]"))
        setScopeOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setScopeOpen(false);
    };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
    };
  }, [scopeOpen]);

  const convs = useInfiniteQuery({
    queryKey: ["conversations", kbId, convSearch],
    queryFn: ({ pageParam }) => conversationsApi.list(kbId, convSearch, 30, pageParam),
    initialPageParam: 0,
    getNextPageParam: (last, pages) => {
      const loaded = pages.reduce((count, page) => count + page.conversations.length, 0);
      return last.conversations.length > 0 && loaded < last.total ? loaded : undefined;
    },
    enabled: !!kbId && kb?.id === kbId,
    // When the search term changes, keep the previous screen, so it does not clear and reappear on every keystroke (Graph and Library do the same).
    // Switching base does not keep it: a session from another base should not appear here for even a moment.
    placeholderData: (prev, prevQuery) =>
      prevQuery?.queryKey[1] === kbId ? prev : undefined,
  });
  // Updated conversations can move between offset pages. Deduplicate by identity;
  // invalidation refetches the loaded page range rather than appending stale offsets.
  const conversations = [...new Map(
    (convs.data?.pages.flatMap((page) => page.conversations) ?? []).map((c) => [c.id, c]),
  ).values()];
  // Renaming: **edit in place**, no dialog; renaming something is not worth interrupting the whole page.
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  const rename = useMutation({
    mutationFn: (v: { id: string; title: string }) =>
      conversationsApi.rename(kb!.id, v.id, v.title),
    onSuccess: () => {
      setRenamingId(null);
      queryClient.invalidateQueries({ queryKey: ["conversations", kb?.id] });
    },
    onError: (e: Error) => toast.error(e.message),
  });

  // Jump straight to the bottom (instant): smooth scrolling would crawl along behind the streaming appends. **Only scroll when the person is at the bottom:**
  // while generating, `shown` changes every 33ms, and someone who scrolled up to read earlier text should not be pulled back by each new batch.
  useEffect(() => {
    if (following.current) bottomRef.current?.scrollIntoView({ behavior: "instant" });
  }, [shown]);

  // Route to session loading: a bare /chat restores the base's last session (switching back to the page is still on the same conversation).
  useEffect(() => {
    if (!kb || kb.id !== kbId) return;
    if (!routeConvId) {
      const last = sessionStorage.getItem(lastKey(kb.id));
      if (last) {
        navigate({
          to: "/kb/$kbId/chat/$conversationId",
          params: { kbId, conversationId: last },
          replace: true,
        });
      }
      return;
    }
    if (routeConvId === activeIdRef.current) return; // After a streaming create, only the URL syncs; do not reload.
    loadConversation(routeConvId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [kb?.id, kbId, routeConvId]);

  // Restored draft grows the input box (the height is normally maintained by onChange).
  useEffect(() => {
    const el = inputRef.current;
    if (el && el.value) {
      el.style.height = "auto";
      el.style.height = `${Math.min(el.scrollHeight, 192)}px`;
    }
  }, []);

  const invalidateList = () =>
    queryClient.invalidateQueries({ queryKey: ["conversations", kb?.id] });

  /** A list click only changes the URL; loading is handled by the route-sync effect. */
  const openConversation = (id: string) => {
    if (id === currentId) return;
    claimView(id);
    navigate({
      to: "/kb/$kbId/chat/$conversationId",
      params: { kbId, conversationId: id },
    });
  };

  /** Reattaches to an answer that is being generated. If nothing is running, the server replies `idle` and the saved history is read once more. */
  const attachIfRunning = (id: string, history: Turn[], owner: ViewRequest) => {
    let abort = () => {};
    let handle: LiveHandle | null = null;
    let checkedIdle = false;
    const stop = reattachChat(owner.kbId, id, {
      onConversation: () => {},
      /* **The turn is only created once the snapshot arrives.** If an empty slot were placed first and then waited for the answer,
         a session with nothing running would briefly flash an empty assistant bubble, and that is the vast majority of cases.
         The snapshot is an overwrite: it is the full picture of the answer at this moment, not an increment. */
      onSnapshot: (s) => {
        if (!ownsView(owner)) { abort(); return; }
        if (handle) return;
        handle = liveAnswer.begin(
          owner.kbId,
          id,
          [
            ...history,
            {
              role: "assistant",
              content: s.content,
              steps: s.steps.length ? s.steps : undefined,
              sources: s.sources.length ? s.sources : undefined,
            },
          ],
          abort,
        );
        handle.identify(id, undefined, s.generation_id);
      },
      onSources: (sources) => handle?.patchLast((t) => ({ ...t, sources })),
      onStep: (step) =>
        handle?.patchLast((t) => ({ ...t, steps: [...(t.steps ?? []), step] })),
      onDelta: (text) => handle?.patchLast((t) => ({ ...t, content: t.content + text })),
      onDone: (stopped) => {
        handle?.finish(stopped);
        invalidateList();
      },
      onError: (message) => {
        if (!handle && ownsView(owner)) setHistoryError(message);
        handle?.patchLast((t) => ({ ...t, error: message }));
        handle?.finish();
      },
      onIdle: async () => {
        if (checkedIdle || handle || !ownsView(owner)) return;
        checkedIdle = true;
        try {
          // The answer may have committed between the history read and attach.
          // One read closes that handoff; never re-POST or recursively attach.
          const { messages } = await conversationsApi.detail(owner.kbId, id);
          if (!ownsView(owner)) return;
          const refreshed = historyTurns(messages);
          setTurns(refreshed);
          setLoadedKey(viewKey(owner.kbId, id));
          setIdleHistoryKey(refreshed.at(-1)?.role === "user" ? viewKey(owner.kbId, id) : null);
        } catch (error) {
          if (ownsView(owner)) setHistoryError(error instanceof Error ? error.message : String(error));
        }
      },
    });
    abort = stop;
  };

  const loadConversation = async (id: string) => {
    const owner = claimView(id);
    setIdleHistoryKey(null);
    setHistoryError(null);
    setLoadingHistory(false);
    // Returning to the run that is still being written: claim it directly, do not read from the database, since the row only exists once it has finished.
    if (liveAnswer.entry(kb!.id, id)) {
      activeIdRef.current = id;
      setActiveId(id);
      return;
    }
    activeIdRef.current = id;
    setActiveId(id);
    setTurns([]);
    setLoadedKey(null);
    setLoadingHistory(true);
    try {
      const { messages } = await conversationsApi.detail(owner.kbId, id);
      if (!ownsView(owner)) return;
      sessionStorage.setItem(lastKey(owner.kbId), id);
      const history = historyTurns(messages);
      setTurns(history);
      setLoadedKey(viewKey(owner.kbId, id));
      /* **Reattach after a refresh.** The store above lives only in this one page; a refresh,
         a new tab, or another machine cannot get it, while on the server the generation is still running. Ask
         "is there one running in this session?": "no" is the most common answer, and the cost is one
         request that immediately returns `idle`.
         Only ask when the last message is the user's: that is exactly the shape of "asked but not yet answered". */
      if (history[history.length - 1]?.role === "user") {
        attachIfRunning(id, history, owner);
      }
    } catch (error) {
      if (!ownsView(owner)) return;
      if (!(error instanceof ApiError && [401, 403, 404].includes(error.status))) {
        setHistoryError(error instanceof Error ? error.message : String(error));
        return;
      }
      // Dead link (the session was deleted or belongs to another base): quietly return to a new conversation.
      sessionStorage.removeItem(lastKey(kb!.id));
      activeIdRef.current = null;
      setActiveId(null);
      setTurns([]);
      navigate({ to: "/kb/$kbId/chat", params: { kbId: owner.kbId }, replace: true });
    } finally {
      if (ownsView(owner)) setLoadingHistory(false);
    }
  };

  const newChat = () => {
    claimView(null);
    setHistoryError(null);
    setLoadingHistory(false);
    setLoadedKey(null);
    // Likewise no abort: starting a new run is not giving up on the previous one.
    if (kb) sessionStorage.removeItem(lastKey(kb.id));
    activeIdRef.current = null;
    following.current = true;
    setActiveId(null);
    setTurns([]);
    navigate({ to: "/kb/$kbId/chat", params: { kbId } });
    inputRef.current?.focus();
  };

  const removeConversation = async (id: string) => {
    const owner = viewRequest.current;
    try {
      await conversationsApi.remove(kb!.id, id);
    } catch (e) {
      // The confirm dialog has already closed: a delete that fails silently leaves this row quietly in the list.
      toast.error(e instanceof Error ? e.message : String(e));
      return;
    }
    // The marker follows the session; otherwise this id would stay in the browser's table forever.
    convMarks.forget(id);
    if (sessionStorage.getItem(lastKey(kb!.id)) === id) {
      sessionStorage.removeItem(lastKey(kb!.id));
    }
    invalidateList();
    if (ownsView(owner) && id === activeIdRef.current) newChat();
  };

  const send = () => {
    const q = input.trim();
    if (!q || !kb || kb.id !== kbId || loadingHistory || historyError) return;
    if (liveAnswer.entry(kb.id, activeId)?.streaming) return;
    const owner = claimView(activeId);
    following.current = true;
    setIdleHistoryKey(null);
    setInput("");
    sessionStorage.removeItem(DRAFT_KEY);
    if (inputRef.current) inputRef.current.style.height = "auto";

    answer(
      owner,
      kb.id,
      // Continue from the turns currently shown on screen, not from component state: after the stream ends, the content only lands
      // in the store, while state still holds the database history from when the conversation was last loaded, and using it would make
      // the last answer disappear from the screen.
      [...(liveHere?.turns ?? turns), { role: "user", content: q }, { role: "assistant", content: "" }],
      { conversation_id: activeId ?? undefined, message: q },
    );
  };

  /** Re-answers the last question that has no answer (#936). The question is already in the database and is not saved again. If the screen
   *  shows a failed answer after it, that is replaced with an empty one, and a stream is reopened with the id it was saved under. */
  const retry = () => {
    const at = retryableQuestion(shown);
    if (at < 0 || !kb || kb.id !== kbId || !activeId || loadingHistory || historyError) return;
    if (liveAnswer.entry(kb.id, activeId)?.streaming) return;
    const question = shown[at];
    const owner = claimView(activeId);
    following.current = true;
    setIdleHistoryKey(null);
    answer(owner, kb.id, [...shown.slice(0, at + 1), { role: "assistant", content: "" }], {
      conversation_id: activeId,
      message: question.content,
      retry_message_id: question.id,
    });
  };

  /* **The result stays in the store and is not handed back to component state.**
     Handing it back requires a `setTurns`, and the component may have unmounted long before the stream ends:
     that call is a no-op, and the content vanishes (switching back shows a blank screen, not even the question bubble).
     Keeping it in the store lets whoever mounts claim it. This run has a name from the moment it starts: the entry is created first,
     and the stream opened after, with callbacks writing only to their own run through the handle. New questions and re-answers go through the same path. */
  const answer = (
    owner: ViewRequest,
    kbNow: string,
    start: Turn[],
    body: Parameters<typeof streamChat>[1],
  ) => {
    const handle = liveAnswer.begin(kbNow, activeId, start, () => {});
    const abort = streamChat(kbNow, body, {
      onConversation: (id, questionId, generationId) => {
        handle.identify(id, questionId, generationId);
        invalidateList();
        if (!ownsView(owner)) return;
        // Identify the generation handle first, then swap the URL; after the layout effect resets the view, loadConversation claims that handle.
        activeIdRef.current = id;
        setActiveId(id);
        sessionStorage.setItem(lastKey(kbNow), id);
        navigate({
          to: "/kb/$kbId/chat/$conversationId",
          params: { kbId, conversationId: id },
          replace: true,
        });
      },
      onSources: (sources) => handle.patchLast((t) => ({ ...t, sources })),
      onStep: (step) =>
        handle.patchLast((t) => ({ ...t, steps: [...(t.steps ?? []), step] })),
      onDelta: (text) =>
        handle.patchLast((t) => ({ ...t, content: t.content + text })),
      onDone: (stopped) => {
        handle.finish(stopped);
        invalidateList();
      },
      onError: (message, error) => {
        if (error?.status === 409 && error.code === "answer_running" && body.conversation_id) {
          handle.discard();
          if (!ownsView(owner)) return;
          if (!body.retry_message_id) {
            const draft = [body.message, inputRef.current?.value ?? sessionStorage.getItem(DRAFT_KEY)].filter(Boolean).join("\n");
            sessionStorage.setItem(DRAFT_KEY, draft);
            setInput(draft);
          }
          void loadConversation(body.conversation_id);
          return;
        }
        handle.patchLast((t) => ({ ...t, error: message }));
        handle.finish();
      },
    });
    // streamChat's abort only exists once it returns; until the real abort is in hand, the handle holds a no-op in its place.
    handle.setAbort(abort);
  };

  /* Composer card: centred on the first screen of a new conversation, docked at the bottom once in a conversation (the same JSX is reused in both places). */
  const composerCard = (
    <div className="u-composer px-4 pt-3 pb-2">
      <Textarea
        bare
        ref={inputRef}
        rows={1}
        className="w-full resize-none text-body leading-relaxed max-h-48"
        placeholder={S.ask.placeholder}
        value={input}
        onChange={(e) => {
          setInput(e.target.value);
          sessionStorage.setItem(DRAFT_KEY, e.target.value);
          const el = e.currentTarget;
          el.style.height = "auto";
          el.style.height = `${Math.min(el.scrollHeight, 192)}px`;
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
            e.preventDefault();
            send();
          }
        }}
      />
      {liveHere?.stopError && (
        <div role="alert" className="text-small text-danger">{liveHere.stopError}</div>
      )}
      <div className="flex items-center justify-between gap-3 pt-1">
        <div className="flex items-center gap-3 min-w-0">
          {/* Scope chip: the prompt point shows "which base is being asked", and switching base keeps the existing semantics (starts a new session). */}
          <div ref={scopeRef} className="relative shrink-0">
            {/* The left edge of the icon has to land on the start of the placeholder above (it is the same icon as the top-bar switcher,
                since both mean "which base"). **Done by indenting the whole button by one step on the left, not by removing its padding**:
                removing the padding would stick the icon to the button's edge; when the pointer rests there, the background would hug the icon tightly
                with no room on the left. Indenting out keeps the padding, so the icon sits in the same place and the background is even on all sides.
                Give as much padding as you indent (both are 8). */}
            <Button
              variant="ghost"
              size="sm"
              className="max-w-52 border-0 px-2 -ml-2"
              title={S.ask.scopeLabel}
              onClick={() => setScopeOpen((v) => !v)}
            >
              <Layers size={12} className="shrink-0 text-ink-2" />
              <span className="truncate">{kb?.name ?? "…"}</span>
              <ChevronDown
                size={11}
                className={cn("u-turn shrink-0 text-ink-2", scopeOpen && "rotate-180")}
              />
            </Button>
            {scopeOpen && (
              <div className="u-menu-glass u-pop-up absolute bottom-full mb-2 left-0 z-50 w-56 rounded-overlay u-lift-strong overflow-hidden">
                <div className="border-b border-line px-4 py-3 text-body font-medium text-ink">
                  {S.ask.scopeLabel}
                </div>
                <div className="u-scroll max-h-60 overflow-y-auto">
                  <KbMenuList
                    kbs={kbs}
                    currentId={kb?.id}
                    onPick={(id) => {
                      setScopeOpen(false);
                      if (id !== kb?.id) { claimView(null); setKb(id); }
                    }}
                  />
                </div>
              </div>
            )}
          </div>
          <span className="text-fine text-ink-2 truncate">{S.ask.composerHint}</span>
        </div>
        {streaming ? (
          <IconButton
            onClick={() => {
              // **Stops only the run being viewed.** Switching page, session, or base does not interrupt it (liveAnswer.ts);
              // other runs keep writing their own entries as usual.
              if (kb) liveAnswer.stop(kb.id, currentId);
            }}
            label={liveHere?.stopping ? S.ask.stopping : S.ask.stop}
            disabled={liveHere?.stopping}
            variant="secondary"
            className="shrink-0"
          >
            {/* **Size goes on the class, not on the size prop:** the button's skin has
                `[&_svg:not([class*="size-"])]:size-4`, which overrides lucide's size attribute,
                so a solid square would be drawn at 16, leaving half a grid of white in a 32 button, which looks heavier than the thin-line
                arrow next to it. 10 is the weight a solid marker should carry on a button of this size. */}
            <Square className="size-2.5" fill="currentColor" strokeWidth={0} />
          </IconButton>
        ) : (
          <IconButton
            variant={input.trim() ? "primary" : "secondary"}
            className="shrink-0"
            label={S.ask.send}
            disabled={!input.trim() || loadingHistory || !!historyError}
            onClick={send}
          >
            <ArrowUp size={15} strokeWidth={2.4} />
          </IconButton>
        )}
      </div>
    </div>
  );

  return (
    <div className="h-full flex">
      {/* Session column */}
      <aside className={`${RAIL_CLS} flex flex-col`}>
        {/* Search is at the top, with the same build and the same corner as the graph page's search box and the ontology page's filter box
            (12px in from the top left, medium size with a magnifier): the box stays put when switching tabs.
            **A heavy title is the norm** (asking the same question twice makes it heavy), while the sentence in the body is what people
            remember, so the server searches both. */}
        <div className="px-2 pt-3 pb-1">
          <Input
            icon={<Search size={12} />}
            placeholder={S.ask.searchConversations}
            value={convSearch}
            onChange={(e) => setConvSearch(e.target.value)}
            onKeyDown={(e) => e.key === "Escape" && setConvSearch("")}
          />
        </div>
        {/* The rhythm of the left column (the left-column baseline of DESIGN rule 2): **box 8 / icon 20 / text 42**.
            The column's horizontal padding is 8, the row's is 12, the icon is 14, and icon to text is 8, so in this column
            every row's text starts at 42, and so does the search box's placeholder (its icon slot is exactly left-3
            plus pl-[34px]). Rows without an icon **keep that cell** (Row fills it in automatically), so the text lands
            on the same vertical line; `flush` is for columns where the whole group has no icons, not for this one. */}
        <div className="px-2 pb-1">
          <Row density="nav" icon={<SquarePen size={14} />} onClick={newChat}>
            {S.ask.newChat}
          </Row>
        </div>
        {/* "Recent" is the name of this group, not a session: the same row build, the same text colour tier
            (text colour has only two tiers, see styles.css), and the triangle at the right end shows the group can be collapsed
            (pointing right = collapsed, pointing down = open). */}
        <div className="px-2">
          <Row
            density="nav"
            aria-expanded={recentOpen}
            onClick={() => setRecentOpen((v) => !v)}
            trailing={
              <ChevronRight
                size={12}
                className={cn("u-turn", recentOpen && "rotate-90")}
              />
            }
          >
            {S.ask.recent}
          </Row>
        </div>
        {recentOpen && (
        <div className="u-rail-list u-scroll flex-1 overflow-y-auto px-2 pb-3">
          {conversations.map((c: ConversationRow) => (
            <div
              key={c.id}
              className="group relative"
            >
              {/* Single-line title; the delete key appears on hover (it asks for confirmation rather than deleting outright) */}
              {renamingId === c.id ? (
                /* Edit in place: Enter saves, Esc cancels. Renaming something is not worth a dialog. */
                <Input size="sm" className="w-full"
                  autoFocus
                  value={renameDraft}
                  onChange={(e) => setRenameDraft(e.target.value)}
                  onBlur={() => setRenamingId(null)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && renameDraft.trim())
                      rename.mutate({ id: c.id, title: renameDraft });
                    if (e.key === "Escape") setRenamingId(null);
                  }}
                />
              ) : (
                <Row
                  density="nav"
                  icon={
                    <ConvMark
                      state={
                        liveIds.has(c.id)
                          ? "live"
                          : unreadIds.has(c.id)
                            ? "unread"
                            : "rest"
                      }
                    />
                  }
                  active={c.id === activeId}
                  className="pr-8"
                  onClick={() => openConversation(c.id)}
                >
                  <span
                    className={`block truncate pr-6 text-body ${
                      c.id === activeId ? "text-ink" : "text-ink-2"
                    }`}
                  >
                    {c.title || S.ask.untitled}
                  </span>
                </Row>
              )}
              {/* Three-dot menu: **one entry point holds every action** (previously the delete was right there on the right,
                  and delete is the last thing that should be one step away). It uses shadcn's
                  DropdownMenu: the popup style is uniform across the site, and the trigger gets
                  `aria-haspopup` from Radix automatically. The button's "sink 1px when pressed" rule specifically excludes the menu
                  trigger, and a hand-rolled entry has no such marker, which is why it used to jump on a click. */}
              {renamingId !== c.id && (
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <IconButton
                      size="sm"
                      label={S.ask.moreActions}
                      className={cn(REVEAL, "absolute right-1 top-1/2 -translate-y-1/2")}
                      onClick={(e) => e.stopPropagation()}
                    >
                      <MoreHorizontal size={14} />
                    </IconButton>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent align="end" className="w-40">
                    <DropdownMenuItem
                      onSelect={() => {
                        setRenameDraft(c.title || "");
                        setRenamingId(c.id);
                      }}
                    >
                      {S.ask.rename}
                    </DropdownMenuItem>
                    <DropdownMenuItem
                      onSelect={() => copyAndSay(c.title || "", S.ask.copied)}
                    >
                      {S.ask.copyTitle}
                    </DropdownMenuItem>
                    <DropdownMenuItem
                      variant="destructive"
                      onSelect={() => setPendingDelete(c)}
                    >
                      {S.ask.deleteConversation}
                    </DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              )}
            </div>
          ))}
          {convs.isError && (
            <div role="alert" className="px-3 py-2 text-small text-ink-2">
              <p>{S.ask.conversationsLoadFailed}</p>
              <Button size="sm" variant="ghost" onClick={() => convs.isFetchNextPageError ? convs.fetchNextPage() : convs.refetch()}>{S.ask.retryConversations}</Button>
            </div>
          )}
          {convs.hasNextPage && !convs.isFetchNextPageError && (
            <Button size="sm" variant="ghost" disabled={convs.isFetching} onClick={() => convs.fetchNextPage()}>
              {S.ask.loadEarlierConversations}
            </Button>
          )}
          {/* Text starts at 20: the column's px-2 (8) plus the row's own px-3 (12), on the same line as "Recent" and
              the title of each session above. Writing px-2 would land it at 16, a difference of 4px that is obvious at a glance. */}
          {convs.isSuccess && conversations.length === 0 && (
            /* 34 = row padding 12 + icon 14 + gap 8: this sentence sits on the same vertical line
               as the title of each session above, rather than starting a column of its own. */
            <p className="py-2 pl-[34px] pr-3 text-small text-ink-2">
              {S.ask.noConversations}
            </p>
          )}
        </div>
        )}
      </aside>

      {/* Conversation area: on a new conversation the first screen is a greeting + a centred composer (the ChatGPT/Claude convention);
          once there are messages, the composer docks at the bottom. */}
      <div className="flex-1 min-w-0 flex flex-col">
        {idleHistoryKey === loadedKey && idleHistoryKey === viewKey(kbId, currentId) && !streaming && (
          <div role="status" className="px-4 pt-4 flex flex-wrap items-center gap-2 text-body text-ink-2">
            <span>{S.ask.noActiveAnswer}</span>
            {retryAt === shown.length - 1 && (
              <Button variant="secondary" size="sm" icon={<RotateCcw size={12} />} onClick={retry}>
                {S.ask.retryQuestion}
              </Button>
            )}
          </div>
        )}
        {historyError ? (
          <div role="alert" className="p-6 text-body">
            <p>{S.ask.historyLoadFailed}</p>
            <p className="text-ink-2">{historyError}</p>
            <Button onClick={() => currentId && loadConversation(currentId)}>{S.ask.retryHistory}</Button>
          </div>
        ) : loadingHistory && !liveHere ? (
          <div role="status" className="p-6 text-body text-ink-2">{S.ask.loadingHistory}</div>
        ) : shown.length === 0 ? (
          /* Anchored to the upper third rather than vertically centred: centring looks like it sinks in a tall window.
             22vh + top chrome (~100px) puts the greeting at about 37% of the height and the composer's centre at about 49%. */
          <div className="flex-1 px-4 pt-[22vh]">
            <div className="w-full max-w-3xl mx-auto">
              <h1
                className="text-center text-[26px] text-ink mb-8"
                style={{ fontFamily: "var(--font-brand)", letterSpacing: "0.03em" }}
              >
                {S.ask.greeting}
              </h1>
              {modelStep && (
                <div className="mb-6 grid place-items-center">
                  <NextStep {...modelStep} />
                </div>
              )}
              {composerCard}
            </div>
          </div>
        ) : (
          <>
            <div
              className="flex-1 overflow-y-auto u-scroll u-chat-fade px-4 pt-6 pb-12"
              onScroll={(e) => {
                following.current = followsBottom(e.currentTarget);
              }}
            >
              <div className="max-w-3xl mx-auto space-y-4">
                {shown.map((t, i) => (
                  <TurnView
                    key={i}
                    turn={t}
                    live={streaming && i === shown.length - 1}
                    onRetry={
                      !streaming && i === shown.length - 1 && t.role === "assistant" && retryAt >= 0
                        ? retry
                        : undefined
                    }
                  />
                ))}
                <div ref={bottomRef} />
              </div>
            </div>
            <div className="px-4 pb-4 pt-2">
              <div className="max-w-3xl mx-auto">{composerCard}</div>
            </div>
          </>
        )}
      </div>

      {pendingDelete && (
        <DangerConfirm
          title={S.ask.deleteTitle}
          hint={S.ask.deleteHint(pendingDelete.title || S.ask.untitled)}
          confirmLabel={S.ask.deleteBtn}
          cancelLabel={S.ask.cancel}
          onConfirm={() => {
            removeConversation(pendingDelete.id);
            setPendingDelete(null);
          }}
          onCancel={() => setPendingDelete(null)}
        />
      )}
    </div>
  );
}

/** A run of body text, or a group of calls that happen at the same time. */
type Segment =
  | { kind: "text"; text: string; last: boolean }
  | { kind: "steps"; steps: ChatStep[] };

/** Splits a turn's reply into segments in the order they happened.
 *
 *  The split point is `step.at`, which is how long the body already was when that step happened. **Messages saved before this migration
 *  have no `at`**: the ordering information was genuinely never stored then, so it cannot and should not be invented.
 *  They fall back to the old layout, with the whole trace at the front. */
function segments(turn: Turn): Segment[] {
  const steps = turn.steps ?? [];
  const text = turn.content ?? "";
  if (steps.length === 0) {
    return text ? [{ kind: "text", text, last: true }] : [];
  }
  if (steps.some((s) => s.at === undefined)) {
    return [
      { kind: "steps", steps },
      ...(text ? [{ kind: "text" as const, text, last: true }] : []),
    ];
  }
  const out: Segment[] = [];
  let cursor = 0;
  for (let i = 0; i < steps.length; ) {
    const at = steps[i].at!;
    // Calls at the same position are grouped together: between the several calls in one turn there is no body text, so they are one fan-out.
    let j = i;
    while (j < steps.length && steps[j].at === at) j++;
    const before = text.slice(cursor, at);
    if (before) out.push({ kind: "text", text: before, last: false });
    out.push({ kind: "steps", steps: steps.slice(i, j) });
    cursor = at;
    i = j;
  }
  const tail = text.slice(cursor);
  if (tail) out.push({ kind: "text", text: tail, last: true });
  return out;
}

function stepIcon(kind: ChatStep["kind"]) {
  if (kind === "search") return <SearchIcon size={11} />;
  if (kind === "docs") return <BookOpen size={11} />;
  if (kind === "entity" || kind === "neighbors" || kind === "path")
    return <Waypoints size={11} />;
  if (kind === "facts" || kind === "timeline") return <History size={11} />;
  // facts reads the world axis and changes reads the cognition axis; the two graph tools get different icons,
  // so the user can tell which axis was asked about when looking at the step bar.
  if (kind === "changes") return <GitCompareArrows size={11} />;
  if (kind === "query") return <Database size={11} />;
  return <Wrench size={11} />;
}

/** One line in the trace. Phrased in the reader's language (`stepDetail`, #942); a step that failed is shown in the danger colour.
 *
 *  The data-query step can expand the SQL it ran (#936): the numbers in the answer come from that statement,
 *  so a reader should be able to check them, rather than only seeing "looked it up". */
function StepRow({ step, kbId }: { step: ChatStep; kbId: string }) {
  const [sqlOpen, setSqlOpen] = useState(false);
  return (
    <div>
      <div className="flex items-center gap-2 text-small">
        <span className="text-ink-2">{stepIcon(step.kind)}</span>
        <span className="text-ink-2 truncate">{stepLabel(step)}</span>
        <span
          className={cn(
            "shrink-0",
            step.status === "failed" ? "text-danger" : "text-ink-2",
          )}
        >
          · {stepDetail(step)}
        </span>
        {step.sql && (
          <LinkButton
            className="shrink-0"
            aria-expanded={sqlOpen}
            onClick={() => setSqlOpen((open) => !open)}
          >
            {S.ask.step.sql}
          </LinkButton>
        )}
      </div>
      {sqlOpen && step.sql && (
        <pre className="mt-1 rounded-panel bg-well px-3 py-2 font-mono text-small text-ink-2 whitespace-pre-wrap break-words">
          {step.sql}
        </pre>
      )}
      {/* After the remember step comes a confirmation card (0015): the facts extracted from this sentence wait for a person to nod first.
          Extraction is asynchronous, so the card grows only once the task completes; on replay it is redrawn from the same chunk. */}
      {step.chunk_id && <NodCard kbId={kbId} chunkId={step.chunk_id} />}
    </div>
  );
}

/** Tool step → orb state: the thinking orb speaks the language of the current action. */
/** A markdown segment. **Memoized by text:** during a generation every token that arrives re-renders the whole message,
 *  and react-markdown reparses that segment from scratch each time, so the longer the answer, the costlier each token gets, and it reads
 *  as if it is stuttering more and more. A paragraph that has finished no longer changes text, so `memo` means it is never reparsed;
 *  the segment still growing is left as it is, since it is supposed to be reparsed. */
const Segment = memo(function Segment({ text }: { text: string }) {
  return (
    <div className="u-chat-prose">
      {/* rehypeCitations turns the `[n]` in the body into superscript markers (see citations.ts).
          It runs at the rehype layer, so it can see whether a square bracket sits inside a link or inside code;
          a string replacement on the body cannot, and would also change the anchor text of a markdown link. */}
      <Markdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeHighlight, rehypeCitations]}
        components={chatMarkdown}
      >
        {text}
      </Markdown>
    </div>
  );
});

/** Thinking indicator: **the sentence being worked on glows by itself**.
 *
 * This has been swapped twice. The first was the `thinking-orbs` orb, which did not fit this interface: the chrome is
 * neutral grey with no colour cast, leaving the decoration to the canvas, while that orb carries its own glowing dot matrix, and `theme` is hard-coded to dark.
 * Swapping in the spinner in the room only solved half the problem: what is being done is already said by that line of text
 * ("searching documents · 3 files"), and a spinning part next to it says the same thing twice, and the spinner only says
 * "something is busy", which has nothing to do with this sentence on screen.
 *
 * Now there is no part at all: a bright band sweeps across the line of text. It is both "still moving" and "the thing moving is this sentence".
 * When there are no steps to show yet, the band sweeps "Thinking…", which is also a sentence, not an icon. */
function Thinking({ step }: { step?: ChatStep }) {
  return (
    <span className="u-thinking text-small truncate">
      {step ? `${stepLabel(step)} · ${stepDetail(step)}` : S.ask.thinking}
    </span>
  );
}

function TurnView({
  turn,
  live,
  onRetry,
}: {
  turn: Turn;
  live?: boolean;
  /** This is the last turn, and it errored, and its question has an id stored: only then is there a re-answer action for that question (#936). */
  onRetry?: () => void;
}) {
  const kbId = useKbId();
  if (turn.role === "user") {
    return (
      <div className="flex justify-end">
        {/* **The conversation uses the title tier** (16/24, not the body's 14/22). This page is the only
            long text in the product that is meant to be read; everything else is tables, lists, and fields in panels,
            where 14px is right, but reading a passage at that size is too small. It stays within the five tiers, no new font size. */}
        <div className="u-bubble-user max-w-[85%] rounded-panel px-4 py-2 text-title whitespace-pre-wrap text-ink">
          {turn.content}
        </div>
      </div>
    );
  }

  const thinking = live && !turn.content && !turn.error;
  const cited = citedSources(turn);
  const lastStep = turn.steps?.[turn.steps.length - 1];

  return (
    <SourcesProvider value={turn.sources ?? NO_SOURCES}>
    <div className="max-w-[95%]">
      {/* The agent's reply has no bubble: the body lands directly on the canvas (the user's message keeps its bubble to tell the roles apart) */}
      <div className="py-1 text-title text-ink leading-relaxed">
        {/* **The trace is threaded through the body in the order it happened.**
            The model looks things up as it talks: it says something, makes a call, and says something more. Moving all the calls to the front
            would make it read as "looked up seven things, then said it all at once": that is not what it did, and between two adjacent calls
            the line "let me check this one first" would lose the thing it explains.
            Several calls in the same turn share one position, so they naturally merge into one group, and a group is a turn. */}
        {segments(turn).map((seg, i) =>
          seg.kind === "steps" ? (
            <div
              key={i}
              className="my-3 space-y-1 border-l border-line-strong pl-3"
            >
              {seg.steps.map((s, j) => (
                <StepRow key={j} step={s} kbId={kbId} />
              ))}
            </div>
          ) : (
            /* react-markdown does the rendering (all the skin belongs to the u-chat-prose design system),
               during streaming remend patches unclosed syntax (bold, fences, links),
               and rehype-highlight does the code highlighting: a mature assembly of parts, so the look stands on its own.
               **Only the segment still growing needs remend**: earlier paragraphs have already finished. */
            <Segment
              key={i}
              text={live && seg.last ? remend(seg.text) : seg.text}
            />
          ),
        )}
        {thinking && <Thinking step={lastStep} />}
        {turn.stopped && <div className="text-small text-ink-2">{S.ask.stopped}</div>}
        {turn.error && <div className="text-danger">{turn.error}</div>}
      </div>
      {/* **Sources come out only once the answer has finished.**
          `sources` arrives incrementally with each retrieval, and if rendered as it goes, a list still growing
          would sit under a sentence not yet written, pushing the body upward as it grows. It is the answer's sign-off,
          not part of the process; the process is already explained by the trace above. */}
      {/* One panel holds several rows: citations are a uniform group, with hover tied to each row.
          Only the ones the body actually cites are listed (see citedSources): retrieved does not mean used.
          Clicking a row opens a preview first rather than jumping away; see chatCitations. */}
      {!live && cited.length > 0 && <SourceList sources={cited} />}
      {/* When there are no citations, the citation cell is replaced by one line, "no sources cited" (#547):
          an absence nobody can read, so it has to be written out. The criterion is in answeredWithoutSources. */}
      {answeredWithoutSources(turn, !!live) && (
        <div className="mt-2 text-small text-ink-2">{S.ask.noSources}</div>
      )}
      {/* Actions on the answer (#936), likewise shown only once it has finished. Copy takes the saved Markdown,
          and the `[n]` markers are kept as they are: tables and code blocks pasted elsewhere stay as they are. A turn that failed halfway through
          gets one extra retry: the question is already in the database, so a re-answer is not saved again. */}
      {!live && (turn.content || onRetry) && (
        <div className="mt-2 flex items-center gap-2">
          {turn.content && <CopyButton label={S.ask.copyAnswer} text={() => turn.content} />}
          {onRetry && (
            <Button variant="secondary" size="sm" icon={<RotateCcw size={12} />} onClick={onRetry}>
              {S.ask.retryQuestion}
            </Button>
          )}
        </div>
      )}
    </div>
    </SourcesProvider>
  );
}
