import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useSearch } from "@tanstack/react-router";
import {
  BookOpen,
  History as HistoryIcon,
  KeyRound,
  RefreshCw,
  Search,
  Settings as SettingsIcon,
  Upload,
  Waypoints,
  X,
} from "lucide-react";
import { api, type Doc, type ExtractionDrop, type SourceView } from "../api";
import { S } from "../i18n";
import {
  CREATABLE_SOURCE_KINDS,
  hasSyncFields,
  type CreatableSourceKind,
} from "../sourceKinds";
import { useKb, useKbId } from "../kb";
import { toast } from "../toast";
import { copyAndSay } from "../clipboard";
import { pushEndpoint } from "../pushEndpoint";
import { githubRepoConfig, globList } from "./githubRepoConfig";
import { SourceDecayFields } from "./SourceDecayFields";
import { decayChange, decayFormValue, decayIssue } from "./sourceDecay";
import {
  Button,
  Checkbox,
  cn,
  DangerConfirm,
  Dialog,
  Dropdown,
  IconButton,
  Input,
  LinkButton,
  Loading,
  Pager,
  Segmented,
  StatusCell,
  Textarea,
  PageHeader,
  buttonLike,
  localDateTime,} from "../ui";
import {
  KIND_ICON,
  SOURCE_ICONS,
  sourceIcon,
  SourcesRail,
  SYNC_DOT,
  SYNCING_KINDS,
  type LibrarySelection,
} from "./SourcesRail";
import { GoogleDrivePanel } from "./GoogleDrivePanel";

const PAGE_SIZE = 15;


/** The value the scheduler produces: interval and cron are mutually exclusive. */
interface ScheduleValue {
  sync_interval_minutes: number | null;
  sync_cron: string | null;
}

type RssContentMode = "feed" | "full_new_items";

const pad2 = (n: number | string) => String(n).padStart(2, "0");

/** Human-readable display of the sync schedule (an advanced custom expression is shown as is). */
function scheduleLabel(s: SourceView): string {
  if (s.sync_cron) {
    const daily = s.sync_cron.match(/^(\d{1,2}) (\d{1,2}) \* \* \*$/);
    if (daily) return S.library.schedule.dailyAt(`${pad2(daily[2])}:${pad2(daily[1])}`);
    const weekly = s.sync_cron.match(/^(\d{1,2}) (\d{1,2}) \* \* ([A-Za-z,-]+)$/);
    if (weekly) return `${weekly[3]} ${pad2(weekly[2])}:${pad2(weekly[1])}`;
    return s.sync_cron;
  }
  if (s.sync_interval_minutes) return S.library.intervalEvery(s.sync_interval_minutes);
  return S.library.intervalManual;
}

type ScheduleMode = "manual" | "interval" | "daily" | "weekly" | "advanced";

/** Saved schedule → the picker's initial state (echoed in edit mode; a cron it cannot recognize falls back to Advanced). */
const CRON_WEEKDAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"] as const;

export function scheduleToPickerState(initial?: ScheduleValue) {
  const base = {
    mode: "manual" as ScheduleMode,
    every: 30,
    unit: "minutes" as "minutes" | "hours",
    time: "09:00",
    days: new Set([0]),
    cron: "",
  };
  if (!initial) return base;
  if (initial.sync_cron) {
    const daily = initial.sync_cron.match(/^(\d{1,2}) (\d{1,2}) \* \* \*$/);
    if (daily)
      return { ...base, mode: "daily" as ScheduleMode, time: `${pad2(daily[2])}:${pad2(daily[1])}` };
    const weekly = initial.sync_cron.match(/^(\d{1,2}) (\d{1,2}) \* \* ([A-Za-z,]+)$/);
    if (weekly) {
      const idx = weekly[3]
        .split(",")
        .map((n) => (CRON_WEEKDAYS as readonly string[]).indexOf(n))
        .filter((i) => i >= 0);
      if (idx.length)
        return {
          ...base,
          mode: "weekly" as ScheduleMode,
          time: `${pad2(weekly[2])}:${pad2(weekly[1])}`,
          days: new Set(idx),
        };
    }
    return { ...base, mode: "advanced" as ScheduleMode, cron: initial.sync_cron };
  }
  if (initial.sync_interval_minutes) {
    const m = initial.sync_interval_minutes;
    return m % 60 === 0
      ? { ...base, mode: "interval" as ScheduleMode, every: m / 60, unit: "hours" as const }
      : { ...base, mode: "interval" as ScheduleMode, every: m, unit: "minutes" as const };
  }
  return base;
}

/** Visual sync-schedule picker: built from Manual / Interval / Daily / Weekly; only Advanced exposes cron. */
export function SchedulePicker({
  onChange,
  initial,
}: {
  onChange: (v: ScheduleValue) => void;
  /** The existing schedule in edit mode; a new one starts from Manual by default */
  initial?: ScheduleValue;
}) {
  type Mode = ScheduleMode;
  const [init] = useState(() => scheduleToPickerState(initial));
  const [mode, setMode] = useState<Mode>(init.mode);
  const [every, setEvery] = useState(init.every);
  const [unit, setUnit] = useState<"minutes" | "hours">(init.unit);
  const [time, setTime] = useState(init.time);
  const [days, setDays] = useState<Set<number>>(init.days);
  const [cron, setCron] = useState(init.cron);

  const emit = (
    m: Mode,
    v: { every?: number; unit?: string; time?: string; days?: Set<number>; cron?: string },
  ) => {
    const t = v.time ?? time;
    const [hh, mm] = t.split(":").map(Number);
    // While the time format is not yet formed (mid-typing), do not update the schedule; keep the last valid value
    if ((m === "daily" || m === "weekly") && !/^([01]?\d|2[0-3]):[0-5]\d$/.test(t)) return;
    switch (m) {
      case "manual":
        return onChange({ sync_interval_minutes: null, sync_cron: null });
      case "interval": {
        const n = Math.max(1, v.every ?? every);
        const u = v.unit ?? unit;
        return onChange({
          sync_interval_minutes: u === "hours" ? n * 60 : n,
          sync_cron: null,
        });
      }
      case "daily":
        return onChange({ sync_interval_minutes: null, sync_cron: `${mm} ${hh} * * *` });
      case "weekly": {
        const ds = [...(v.days ?? days)].sort();
        const names = ds.map((i) => CRON_WEEKDAYS[i]).join(",");
        return onChange({
          sync_interval_minutes: null,
          sync_cron: ds.length ? `${mm} ${hh} * * ${names}` : null,
        });
      }
      case "advanced":
        return onChange({
          sync_interval_minutes: null,
          sync_cron: (v.cron ?? cron).trim() || null,
        });
    }
  };

  const modes: { key: Mode; label: string }[] = [
    { key: "manual", label: S.library.schedule.manual },
    { key: "interval", label: S.library.schedule.interval },
    { key: "daily", label: S.library.schedule.daily },
    { key: "weekly", label: S.library.schedule.weekly },
    { key: "advanced", label: S.library.schedule.advanced },
  ];

  return (
    <div>
      <Segmented
        fill
        size="sm"
        className="mb-2"
        value={mode}
        onChange={(k) => {
          setMode(k);
          emit(k, {});
        }}
        options={modes.map(({ key, label }) => ({ value: key, label }))}
      />

      {mode === "interval" && (
        <div className="flex items-center gap-2 text-body text-ink-2">
          {S.library.schedule.every}
          <Input size="sm" className="u-input-plain w-16 u-num text-center"
            type="number"
            min={1}
            value={every}
            onChange={(e) => {
              const n = Number(e.target.value) || 1;
              setEvery(n);
              emit("interval", { every: n });
            }}
          />
          {/* Two options do not get a dropdown: a segmented switch, using the same recipe as the mode bar above */}
          <Segmented
            size="sm"
            value={unit}
            onChange={(u) => {
              setUnit(u);
              emit("interval", { unit: u });
            }}
            options={(["minutes", "hours"] as const).map((u) => ({
              value: u,
              label: S.library.schedule[u],
            }))}
          />
        </div>
      )}

      {(mode === "daily" || mode === "weekly") && (
        <div className="space-y-2">
          {mode === "weekly" && (
            <div className="flex gap-1">
              {S.library.schedule.daysShort.map((d, i) => (
                <Button
                  variant={days.has(i) ? "primary" : "secondary"}
                  size="sm"
                  key={d}
                  className="flex-1"
                  onClick={() => {
                    const next = new Set(days);
                    if (next.has(i)) next.delete(i);
                    else next.add(i);
                    setDays(next);
                    emit("weekly", { days: next });
                  }}
                >
                  {d}
                </Button>
              ))}
            </div>
          )}
          <div className="flex items-center gap-2 text-body text-ink-2">
            {S.library.schedule.at}
            {/* 24h plain-text input: the native time control has a large block header and follows the system language (e.g. "AM 09:00") */}
            <Input
              size="sm"
              className={cn(
                "u-input-plain u-num w-20 text-center",
                !/^([01]?\d|2[0-3]):[0-5]\d$/.test(time) && "!border-danger",
              )}
              placeholder="09:00"
              value={time}
              onChange={(e) => {
                const t = e.target.value;
                setTime(t);
                if (/^([01]?\d|2[0-3]):[0-5]\d$/.test(t)) emit(mode, { time: t });
              }}
            />
          </div>
        </div>
      )}

      {mode === "advanced" && (
        <div>
          {/* The expression uses mono; the placeholder falls back to the default font (u-placeholder-sans) */}
          <Input className="w-full font-mono u-placeholder-sans"
            placeholder={S.library.schedule.cronPlaceholder}
            value={cron}
            onChange={(e) => {
              setCron(e.target.value);
              emit("advanced", { cron: e.target.value });
            }}
          />
          <a
            href={S.library.schedule.cronDocsUrl}
            target="_blank"
            rel="noreferrer"
            className="u-link mt-2 inline-block text-fine"
          >
            {S.library.schedule.whatIsCron}
          </a>
        </div>
      )}
    </div>
  );
}

export function Library() {
  const { kb } = useKb();
  const queryClient = useQueryClient();
  const fileInput = useRef<HTMLInputElement>(null);
  // Coming back from the source bar on the document viewer page carries ?src= to locate the matching folder
  const { src } = useSearch({ from: "/app/kb/$kbId/library" });
  const [dragging, setDragging] = useState(false);
  const [selection, setSelection] = useState<LibrarySelection>(src ?? "all");
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState(false);
  // API source key dialog (can be viewed or rotated at any time)
  const [tokenReveal, setTokenReveal] = useState<{ sourceId: string; kind: string } | null>(null);
  const [cleaning, setCleaning] = useState(false);
  const [reExtracting, setReExtracting] = useState(false);
  const [rebuilding, setRebuilding] = useState(false);
  // Failure detail dialog: {file name, which pipeline, original text}
  const [errorView, setErrorView] = useState<{
    file: string;
    kind: string;
    text: string;
  } | null>(null);
  // Discarded-detail dialog: the facts this document extracted but that never landed
  const [dropsView, setDropsView] = useState<{ file: string; rows: ExtractionDrop[] } | null>(
    null,
  );
  const [showHistory, setShowHistory] = useState(false);
  const [page, setPage] = useState(0);
  const [filter, setFilter] = useState("");
  // Filter by extraction status. **Empty = no filter**: five failed items mixed into twenty-seven would previously have had to be read page by page
  const [graphFilter, setGraphFilter] = useState("");


  // Switching folder returns to page one, clears the filter, and exits the history view
  useEffect(() => {
    setPage(0);
    setFilter("");
    setShowHistory(false);
  }, [selection]);
  // A change in the filter term returns to page one
  useEffect(() => setPage(0), [filter]);

  // Status changes are pushed through the SSE event stream (useKbEvents is mounted in Shell), so no polling is needed
  const docs = useQuery({
    // **Server-side filtering and paging**: scope, name, extraction status and page number all go into the queryKey,
    // so changing any one of them refetches a page. Previously the whole library was fetched at once and sliced on the client
    queryKey: ["documents", kb?.id, selection, filter, graphFilter, page],
    queryFn: () =>
      api.documents(kb!.id, {
        source:
          selection === "all" || selection === "deleted"
            ? undefined
            : selection === "uploads"
              ? "none"
              : selection,
        state: selection === "deleted" ? "deleted" : undefined,
        q: filter.trim() || undefined,
        graph: graphFilter || undefined,
        limit: PAGE_SIZE,
        offset: page * PAGE_SIZE,
      }),
    enabled: !!kb,
    placeholderData: (prev) => prev,
  });
  const sources = useQuery({
    queryKey: ["sources", kb?.id],
    queryFn: () => api.sources(kb!.id),
    enabled: !!kb,
  });
  // After fetching the whole library once, group by document: the discarded rows from extraction are few, better than one request per row
  const drops = useQuery({
    queryKey: ["extraction-drops", kb?.id],
    queryFn: () => api.extractionDrops(kb!.id),
    enabled: !!kb,
  });
  const dropsByDoc = useMemo(() => {
    const m = new Map<string, ExtractionDrop[]>();
    for (const d of drops.data?.drops ?? []) {
      const list = m.get(d.document_id);
      if (list) list.push(d);
      else m.set(d.document_id, [d]);
    }
    return m;
  }, [drops.data]);

  const invalidate = () => {
    queryClient.invalidateQueries({ queryKey: ["documents", kb?.id] });
    queryClient.invalidateQueries({ queryKey: ["docCount", kb?.id] });
    queryClient.invalidateQueries({ queryKey: ["sources", kb?.id] });
    // Re-extraction rewrites this document's discard signals, so they are invalidated together with the document
    queryClient.invalidateQueries({ queryKey: ["extraction-drops", kb?.id] });
  };

  // One-click retry of everything that failed in this scope. **Enqueuing item by item is done on the server**: queuing carries
  // other actions (cancelling a running task, clearing incremental markers); going around it would leave half-finished state
  const retryFailed = useMutation({
    mutationFn: () =>
      api.retryFailedDocs(
        kb!.id,
        selection === "all"
          ? undefined
          : selection === "uploads"
            ? "none"
            : selection,
      ),
    onSuccess: (r) => {
      toast.success(S.library.retryQueued(r.queued));
      invalidate();
    },
    onError: (e: Error) => toast.error(e.message),
  });

  const upload = useMutation({
    // When a folder source is selected, uploads go straight into that folder
    mutationFn: (files: FileList | File[]) => {
      const folder = sources.data?.sources.find(
        (s) => s.id === selection && s.kind === "folder",
      );
      return api.upload(kb!.id, Array.from(files), folder?.id);
    },
    onSuccess: invalidate,
  });
  // Deletion is a tombstone (#268): the prompt offers an "Undo", and clicking it brings everything back the same way
  const restore = useMutation({
    mutationFn: (id: string) => api.restoreDocument(id),
    onSuccess: () => {
      toast.success(S.library.restored);
      invalidate();
    },
    onError: (e) => toast.error(String(e)),
  });
  const remove = useMutation({
    mutationFn: (id: string) => api.deleteDocument(id),
    onSuccess: (r, id) => {
      toast.info(S.library.deletedWithFacts(r.invalidated_facts), {
        label: S.library.undo,
        onClick: () => restore.mutate(id),
      });
      invalidate();
    },
  });
  // Hard delete (#268, second half): a typed confirmation; for a knowledge base admin; cannot be undone
  const [purging, setPurging] = useState<Doc | null>(null);
  const purge = useMutation({
    mutationFn: (id: string) => api.purgeDocument(id),
    onSuccess: () => {
      setPurging(null);
      toast.success(S.library.purged);
      invalidate();
    },
    onError: (e) => toast.error(String(e)),
  });
  const extract = useMutation({ mutationFn: (id: string) => api.extractDocument(id), onSuccess: invalidate });
  const reprocess = useMutation({
    mutationFn: (id: string) => api.reprocessDocument(id),
    onSuccess: invalidate,
  });
  // The role in this KB: gates the "rebuild graph" entry (starts at KB admin, consistent with the API side)
  const kbDetail = useQuery({
    queryKey: ["kbOne", kb?.id],
    queryFn: () => api.kbDetail(kb!.id),
    enabled: !!kb,
  });
  const myRole = kbDetail.data?.my_role ?? "";
  const canEdit = ["editor", "admin", "owner"].includes(myRole);
  const canRebuild = ["admin", "owner"].includes(myRole);

  const reExtractSource = useMutation({
    mutationFn: (sourceId: string) => api.reExtractSource(kb!.id, sourceId),
    onSuccess: (r) => {
      toast.success(S.library.queuedDocs(r.queued));
      setReExtracting(false);
      invalidate();
    },
    onError: (e) => toast.error((e as Error).message),
  });
  const rebuildGraph = useMutation({
    mutationFn: () => api.rebuildGraph(kb!.id),
    onSuccess: (r) => {
      toast.success(S.library.rebuildDone(r.entities_removed, r.facts_removed, r.queued));
      setRebuilding(false);
      invalidate();
    },
    onError: (e) => toast.error((e as Error).message),
  });
  const syncNow = useMutation({
    mutationFn: (sourceId: string) => api.syncSource(kb!.id, sourceId),
    onSuccess: invalidate,
  });
  const removeSource = useMutation({
    mutationFn: (sourceId: string) => api.deleteSource(kb!.id, sourceId),
    onSuccess: () => {
      setSelection("all");
      invalidate();
    },
  });
  const cleanupMissing = useMutation({
    mutationFn: (sourceId: string) => api.cleanupMissing(kb!.id, sourceId),
    onSuccess: () => {
      setCleaning(false);
      invalidate();
    },
  });

  // Only manual-semantics destinations accept uploads: All / Uploads / folder. Pull-type sources (url/rss/api/custom)
  // are filled by sync; manually stuffing files in would make them orphans "not in the source" at the next sync
  const canUpload =
    selection === "all" ||
    selection === "uploads" ||
    sources.data?.sources.find((s) => s.id === selection)?.kind === "folder";

  const onDrop = useCallback(
    (e: React.DragEvent) => {
      e.preventDefault();
      setDragging(false);
      if (!canUpload) return;
      if (e.dataTransfer.files.length && kb) upload.mutate(e.dataTransfer.files);
    },
    [kb, upload, canUpload],
  );

  // How many documents can be extracted across the whole library. **Rebuilding is a library-level action**, so it should not show the current source's number
  const kbStats = useQuery({
    queryKey: ["docCount", kb?.id],
    queryFn: () => api.documents(kb!.id, { limit: 1, offset: 0 }),
    enabled: !!kb,
  });

  // **Early return can only come after the last hook.** This line used to sit before kbStats: on a full page
  // refresh or a direct open of the link, kb had not arrived at the first render, the early return skipped the hooks below,
  // and the next render had one more hook, so React threw "Rendered more hooks". Navigating within the site
  // kb is already in the cache, so only deep links and refreshes hit it
  if (!kb) return <Loading>{S.nav.loading}</Loading>;

  // The server has already cut this page: filter, scope and page number are all in the request
  const pagedDocs = docs.data?.docs ?? [];
  const totalDocs = docs.data?.total ?? 0;
  const kbReady = kbStats.data?.ready ?? 0;
  const sourceList = sources.data?.sources ?? [];
  const selectedSource = sourceList.find((s) => s.id === selection);
  const safePage = page;
  const query = filter.trim().toLowerCase();

  return (
    <div className="h-full flex">
      <SourcesRail
        kbId={kb.id}
        active={selection}
        onSelect={setSelection}
        onAdd={() => setAdding(true)}
      />

      {/* Document area (scrollbar-gutter is permanent: when switching views the scrollbar appearing or vanishing no longer causes horizontal jitter) */}
      <div
        className="flex-1 min-w-0 overflow-y-auto u-scroll px-8 py-6 [scrollbar-gutter:stable]"
        onDragOver={(e) => {
          e.preventDefault();
          if (canUpload) setDragging(true);
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={onDrop}
      >
        {/* The work page fills the whole width to the right of the column: same rule as the Review / Ontology / settings pages, so switching pages does not jump */}
        <div>
          <PageHeader
            title={
              selectedSource?.name ??
              (selection === "uploads"
                ? S.library.uploads
                : selection === "deleted"
                  ? S.library.deleted
                  : S.library.title)
            }
            actions={
              <>
              {/* In the history view the filter box is only hidden, not removed (invisible keeps the placeholder); the title row's height neither collapses nor jitters */}
              <div className={`relative ${showHistory ? "invisible" : ""}`}>
                {/* A medium size with a magnifier, the same shape as the filter bars on the graph, ontology and members pages.
                    Previously it was small here, so next to the others it looked one tier shorter */}
                <Search
                  size={13}
                  className="absolute left-3 top-1/2 -translate-y-1/2 text-ink-2 pointer-events-none"
                />
                <Input className="w-52 pl-[34px] pr-8"
                  placeholder={S.library.filterPlaceholder}
                  value={filter}
                  onChange={(e) => setFilter(e.target.value)}
                  onKeyDown={(e) => e.key === "Escape" && setFilter("")}
                />
                {filter && (
                  <IconButton size="sm" label={S.library.close} className="absolute right-2 top-1/2 -translate-y-1/2"
                    onClick={() => setFilter("")}
                  >
                    <X size={12} />
                  </IconButton>
                )}
              </div>
              {/* Filter by extraction status. **The options are hard-coded to those five**, not filled from what the library actually has —
                  it is a fixed set of pipeline states, and "nothing is failing in this library right now" is exactly
                  what a user wants to confirm by filtering */}
              {selection !== "deleted" && (
                <Dropdown
                  size="sm"
                  className="w-44 shrink-0"
                  value={graphFilter}
                  onChange={(v) => {
                    setGraphFilter(v);
                    setPage(0);
                  }}
                  options={[
                    { value: "", label: S.library.anyStatus },
                    { value: "failed", label: S.library.statusFailed },
                    { value: "done", label: S.library.statusDone },
                    { value: "queued", label: S.library.statusQueued },
                    { value: "extracting", label: S.library.statusExtracting },
                    { value: "none", label: S.library.statusNone },
                  ]}
                />
              )}
              {/* One-click retry. **Shown only when there really are failures**: a library with no failures should not show a
                  button that does nothing when clicked. The number is written on the button, so you know how many will be touched before clicking */}
              {(docs.data?.failed ?? 0) > 0 && canUpload && (
                <Button variant="secondary" size="sm" className="flex items-center gap-2 shrink-0"
                  onClick={() => retryFailed.mutate()}
                  disabled={retryFailed.isPending}
                >
                  <RefreshCw size={12} />
                  {S.library.retryFailed(docs.data!.failed)}
                </Button>
              )}
              {/* Rebuild the whole library: liquidation semantics, KB admin only; placed in the All documents view.
                  **The button itself is in the ordinary colour**: it only opens the confirmation dialog, and the danger colour is kept for the
                  button in the dialog that actually acts; a permanent red block on the toolbar stops looking red after a while */}
              {selection === "all" && canRebuild && (
                <Button
                  variant="secondary"
                  size="sm"
                  className="shrink-0"
                  icon={<RefreshCw size={12} />}
                  onClick={() => setRebuilding(true)}
                >
                  {S.library.rebuild}
                </Button>
              )}
              {canUpload && (
                <Button variant="secondary" size="sm" className="flex items-center gap-2 shrink-0"
                  onClick={() => fileInput.current?.click()}
                >
                  <Upload size={12} />
                  {S.library.upload}
                </Button>
              )}
              </>
            }
          />
            <input
              ref={fileInput}
              type="file"
              multiple
              hidden
              accept=".pdf,.docx,.xlsx,.xls,.ods,.pptx,.md,.txt,.html,.htm,.csv,.tsv,.json,.yaml,.yml,.xml,.log,.png,.jpg,.jpeg,.webp,.tif,.tiff,.bmp,.gif,.mp3,.wav,.m4a,.flac,.ogg,.opus,.aac,.amr"
              onChange={(e) => e.target.files?.length && upload.mutate(e.target.files)}
            />

          {selectedSource && (
            <SourceBar
              kbId={kb.id}
              source={selectedSource}
              syncing={syncNow.isPending}
              historyOpen={showHistory}
              onToggleHistory={() => setShowHistory((v) => !v)}
              onSync={() => syncNow.mutate(selectedSource.id)}
              onEdit={() => setEditing(true)}
              onCleanup={() => setCleaning(true)}
              onReExtract={canEdit ? () => setReExtracting(true) : undefined}
              onToken={() => setTokenReveal({ sourceId: selectedSource.id, kind: selectedSource.kind })}
            />
          )}

          {/* Extraction progress. **The numbers come from the server**, computed by the source scope — previously they counted the current page,
              so the progress bar jumped when paging. Both numerator and denominator go only by `graph_status`: previously the numerator was
              `status='ready'` (ingestion complete), so a document whose "ingestion is complete, graph still being extracted" was
              counted once in each dimension, and two documents could show as 2 / 4 */}
          {(() => {
            const pending = docs.data?.extracting ?? 0;
            if (pending === 0) return null;
            const done = docs.data?.done ?? 0;
            const total = done + pending;
            return (
              <div className="mb-3 glass rounded-panel px-4 py-3">
                <div className="flex items-center justify-between text-small text-ink-2 mb-2">
                  <span>{S.library.extractProgress(done, total)}</span>
                  <span className="u-num text-ink-2">
                    {Math.round((done / Math.max(total, 1)) * 100)}%
                  </span>
                </div>
                <div className="h-1 rounded-full bg-surface-2 overflow-hidden">
                  <div
                    className="u-progress-fill h-full bg-warn"
                    style={{ width: `${(done / Math.max(total, 1)) * 100}%` }}
                  />
                </div>
              </div>
            );
          })()}

          {upload.isPending && (
            <div className="mb-3 text-body text-warn">{S.library.uploading}</div>
          )}
          {upload.isError && (
            <div className="mb-3 text-body text-danger">
              {S.library.uploadFailed}: {String((upload.error as Error).message)}
            </div>
          )}

          {showHistory && selectedSource ? (
            <RunsPanel kbId={kb.id} sourceId={selectedSource.id} />
          ) : (
          <>
          {/* This block is a drop zone, not a clickable object: the real feedback is u-highlight while dragging.
              In the normal state, a border lighting up under the pointer would be an interaction hint for a slot that does not accept clicks */}
          <div className={`glass rounded-panel ${dragging ? "u-highlight" : ""}`}>
            {selection === "deleted" ? (
              <DeletedTable
                docs={pagedDocs}
                sources={sourceList}
                canPurge={canRebuild}
                onRestore={(id) => restore.mutate(id)}
                onPurge={setPurging}
              />
            ) : pagedDocs.length ? (
              <table className="w-full text-body">
                <thead>
                  <tr className="text-left text-small text-ink-2 border-b border-line">
                    <th className="px-4 py-3 font-medium">{S.library.colFile}</th>
                    {selection === "all" && (
                      <th className="px-4 py-3 font-medium">{S.library.colSource}</th>
                    )}
                    <th className="px-4 py-3 font-medium">{S.library.colStatus}</th>
                    <th className="px-4 py-3 font-medium">{S.library.colGraph}</th>
                    <th className="px-4 py-3 font-medium">{S.library.colChunks}</th>
                    <th className="px-4 py-3 font-medium">{S.library.colSize}</th>
                    {/* Neither action column has a header: the column names say "what this cell is",
                        and these two cells are "what can be done", so a title would be an extra row of noise */}
                    <th className="px-4 py-3"></th>
                    <th className="px-4 py-3"></th>
                  </tr>
                </thead>
                <tbody>
                  {pagedDocs.map((d) => (
                    <DocRow
                      key={d.id}
                      doc={d}
                      source={
                        selection === "all"
                          ? (sourceList.find((s) => s.id === d.source_id) ?? null)
                          : undefined
                      }
                      onDelete={() => remove.mutate(d.id)}
                      onExtract={() => extract.mutate(d.id)}
                      onReprocess={() => reprocess.mutate(d.id)}
                      onShowError={(kind, text) =>
                        setErrorView({ file: d.filename, kind, text })
                      }
                      drops={dropsByDoc.get(d.id)}
                      onShowDrops={(rows) => setDropsView({ file: d.filename, rows })}
                    />
                  ))}
                </tbody>
              </table>
            ) : query || graphFilter ? (
              <div className="py-20 text-center text-body text-ink-2">
                {S.library.filterNoMatch}
              </div>
            ) : (
              <div className="py-20 text-center text-body text-ink-2">
                {canUpload ? (
                  <>
                    {S.library.dropHint}
                    <div className="mt-2 text-small text-ink-2">{S.library.formats}</div>
                  </>
                ) : (
                  S.library.emptyPull
                )}
              </div>
            )}
          </div>

          <Pager
            total={totalDocs}
            pageSize={PAGE_SIZE}
            page={safePage}
            onPage={setPage}
          />
          </>
          )}
        </div>
      </div>

      {adding && (
        <SourceModal
          kbId={kb.id}
          onDone={(id, pushKind) => {
            setAdding(false);
            if (id) setSelection(id);
            // Google Drive opens its settings straight after creation: connecting the account and choosing folders are both done there
            if (id && pushKind === "google_drive") setEditing(true);
            // A push source opens its key dialog straight after creation (onboarding: endpoint + key, both collected in one step)
            else if (id && pushKind) setTokenReveal({ sourceId: id, kind: pushKind });
            invalidate();
          }}
        />
      )}
      {tokenReveal && (
        <TokenModal
          kbId={kb.id}
          sourceId={tokenReveal.sourceId}
          kind={tokenReveal.kind}
          onClose={() => setTokenReveal(null)}
        />
      )}
      {editing && selectedSource && (
        <SourceEditModal
          kbId={kb.id}
          source={selectedSource}
          onDone={() => {
            setEditing(false);
            invalidate();
          }}
          onDelete={() => {
            setEditing(false);
            removeSource.mutate(selectedSource.id);
          }}
        />
      )}
      {cleaning && selectedSource && (
        <DangerConfirm
          title={S.library.cleanupTitle}
          hint={S.library.cleanupHint(selectedSource.missing_count, selectedSource.name)}
          confirmLabel={S.library.cleanupConfirm}
          cancelLabel={S.library.cancel}
          busy={cleanupMissing.isPending}
          onConfirm={() => cleanupMissing.mutate(selectedSource.id)}
          onCancel={() => setCleaning(false)}
        />
      )}
      {/* Re-extracting a source: a light confirmation (it destroys no data, it just costs time and money), no typed unlock needed */}
      {reExtracting && selectedSource && (
        <DangerConfirm
          title={S.library.reExtractTitle}
          hint={S.library.reExtractHint(
            docs.data?.ready ?? 0,
            selectedSource.name,
          )}
          confirmLabel={S.library.reExtractConfirm}
          cancelLabel={S.library.cancel}
          busy={reExtractSource.isPending}
          onConfirm={() => reExtractSource.mutate(selectedSource.id)}
          onCancel={() => setReExtracting(false)}
        />
      )}
      {errorView && (
        <ErrorModal
          file={errorView.file}
          kind={errorView.kind}
          text={errorView.text}
          onClose={() => setErrorView(null)}
        />
      )}
      {dropsView && (
        <DropsModal
          file={dropsView.file}
          rows={dropsView.rows}
          onClose={() => setDropsView(null)}
        />
      )}
      {/* Hard delete: a typed confirmation (the content is gone and cannot come back) */}
      {purging && (
        <DangerConfirm
          title={S.library.purgeTitle}
          hint={S.library.purgeHint(purging.filename)}
          requireText={purging.filename}
          confirmLabel={S.library.purgeConfirm}
          cancelLabel={S.library.cancel}
          busy={purge.isPending}
          onConfirm={() => purge.mutate(purging.id)}
          onCancel={() => setPurging(null)}
        />
      )}
      {/* Rebuild the whole library: a typed confirmation (clearing the graph layer is irreversible) */}
      {rebuilding && (
        <DangerConfirm
          title={S.library.rebuildTitle}
          hint={S.library.rebuildHint(
            kbReady,
            kb.name,
          )}
          requireText={kb.name}
          confirmLabel={S.library.rebuildConfirm}
          cancelLabel={S.library.cancel}
          busy={rebuildGraph.isPending}
          onConfirm={() => rebuildGraph.mutate()}
          onCancel={() => setRebuilding(false)}
        />
      )}
    </div>
  );
}

/** The status bar for the selected source: sync status / last sync / History toggle / Sync now / settings. */
function SourceBar({
  kbId,
  source,
  syncing,
  historyOpen,
  onToggleHistory,
  onSync,
  onEdit,
  onCleanup,
  onReExtract,
  onToken,
}: {
  kbId: string;
  source: SourceView;
  syncing: boolean;
  historyOpen: boolean;
  onToggleHistory: () => void;
  onSync: () => void;
  onEdit: () => void;
  onCleanup: () => void;
  /** Default = no edit permission, so the re-extract entry is not rendered */
  onReExtract?: () => void;
  onToken: () => void;
}) {
  const isPull = SYNCING_KINDS.has(source.kind);
  // Push-type sources: api pushes documents, statements pushes statements (0054); the same status, token and guide on the UI
  const isApi = source.kind === "api" || source.kind === "statements";
  const busy = source.last_sync_status === "running" || source.last_sync_status === "queued";
  // The config of historical data may be a jsonb null (caused by a default Value::Null being stored): defensive fallback
  const cfg = source.config ?? {};
  const configSummary =
    cfg.feed_url ??
    cfg.endpoint ??
    cfg.repo ??
    (cfg.urls ? `${cfg.urls.length} URLs` : "");
  const rssMode: RssContentMode =
    cfg.content_mode === "full_new_items" ? "full_new_items" : "feed";
  // Completion progress only has something to say while this tier is on
  const hydration =
    source.rss_full_content?.state === "disabled" ? null : source.rss_full_content;

  return (
    <div className="glass rounded-panel mb-3">
      <div className="px-4 py-3 flex items-center gap-3 text-small">
        {/* api uses the same status vocabulary as the pull-type sources: dot + status + time + output/error;
            the endpoint is one-off integration info, so it goes in the Token dialog and does not take up the permanent bar */}
        {(isPull || isApi) && (
          <>
            <span
              className={`h-1.5 w-1.5 rounded-full shrink-0 ${SYNC_DOT[source.last_sync_status]}`}
            />
            <span className="text-ink-2 whitespace-nowrap shrink-0">
              {isApi
                ? S.library.pushStatus[source.last_sync_status]
                : S.library.syncStatus[source.last_sync_status]}
            </span>
            {source.last_sync_at && (
              <span className="text-ink-2 u-num whitespace-nowrap shrink-0">
                {source.last_sync_at.slice(0, 16).replace("T", " ")}
              </span>
            )}
            {source.last_sync_status === "ok" && source.last_sync_added > 0 && (
              <span className="text-ok">
                {S.library.lastSyncAdded(source.last_sync_added)}
              </span>
            )}
            {source.last_sync_error && (
              <span className="text-danger truncate min-w-0" title={source.last_sync_error}>
                {source.last_sync_error}
              </span>
            )}
            {isPull && (
              <>
                <span className="text-ink-2 truncate min-w-0">{configSummary}</span>
                <span className="text-ink-2 shrink-0 u-num">{scheduleLabel(source)}</span>
                {source.kind === "rss" && (
                  <>
                    <span className="text-ink-2 shrink-0" title={S.library.rssContentModeHint}>
                      {rssMode === "full_new_items"
                        ? S.library.rssModeFullShort
                        : S.library.rssModeFeedShort}
                    </span>
                    {/* **Whether this tier is on is said by the server's `state`, not re-derived by the frontend.**
                        Previously this was `rssMode === "full_new_items"`, so the server's CASE was recomputed on the frontend,
                        so the same rule existed in two copies.
                        The block is present for every RSS source (0033 decision 2), so what is checked is the state:
                        a `disabled` source has no completion queue, and those five zeros are not "the queue is empty";
                        they mean "there is no queue here", so they should not be shown. */}
                    {hydration && (
                      <span className="text-ink-2 shrink-0 u-num">
                        {S.library.rssHydrationCounts(hydration)}
                      </span>
                    )}
                  </>
                )}
              </>
            )}
          </>
        )}
        {!isPull && !isApi && (
          <span className="text-ink-2 truncate min-w-0">
            {S.library.sourceKindHints[source.kind as "folder"] ?? ""}
          </span>
        )}
        <div className="ml-auto flex items-center gap-2 shrink-0">
          {/* Integration-type sources (custom pull / api push): the interface docs are within easy reach */}
          {(source.kind === "custom" || isApi) && (
            <Link
              to="/docs/$slug"
              params={{ slug: "ingest" }}
              target="_blank"
              title={S.library.ingestGuideTitle}
              className={buttonLike("ghost", "sm")}
            >
              <BookOpen size={12} />
            </Link>
          )}
          {source.missing_count > 0 && (
            <Button variant="secondary" size="sm"
              onClick={onCleanup}
            >
              {S.library.cleanupMissing(source.missing_count)}
            </Button>
          )}
          {/* History is open for both pull-type and api push-type: a failed push (bad format, etc.) is also recorded as a run */}
          {(isPull || isApi) && (
            /* The active state uses inverted colours (the same vocabulary as the dialog type tabs and the selected icon), recognizable at a glance */
            <Button variant="secondary" size="sm"
              onClick={onToggleHistory}
              className={buttonLike(historyOpen ? "primary" : "ghost", "sm")}
            >
              <HistoryIcon size={11} />
              {S.library.syncHistory}
            </Button>
          )}
          {isPull && (
            <Button variant="secondary" size="sm" className="flex items-center gap-2"
              onClick={onSync}
              disabled={busy || syncing}
            >
              <RefreshCw size={11} className={busy ? "animate-spin" : ""} />
              {S.library.syncNow}
            </Button>
          )}
          {isApi && (
            <Button variant="secondary" size="sm" className="flex items-center gap-2"
              onClick={onToken}
            >
              <KeyRound size={11} />
              {S.library.viewToken}
            </Button>
          )}
          {/* Full re-extraction of this source: offered for every type (as long as there are documents to re-extract), except the kind that says it is not extracted
              (schema documents): the backend would reject it, so the button should not appear */}
          {onReExtract && source.doc_count > 0 && source.config?.extract !== false && (
            <Button variant="secondary" size="sm" className="flex items-center gap-2"
              onClick={onReExtract}
            >
              <Waypoints size={11} />
              {S.library.reExtractSource}
            </Button>
          )}
          <Button variant="secondary" size="sm"
            onClick={onEdit}
            title={S.library.sourceSettings}
          >
            <SettingsIcon size={12} />
          </Button>
        </div>
      </div>
    </div>
  );
}

/** Failure detail: the full original text, copyable. A tooltip cannot hold it or let it be copied, so a dialog is needed. */
/** Extraction discard detail: extracted, but blocked; here it says clearly what blocked it and how much was lost. */
function DropsModal({
  file,
  rows,
  onClose,
}: {
  file: string;
  rows: ExtractionDrop[];
  onClose: () => void;
}) {
  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={S.library.dropsTitle}
      description={
        <>
          <span className="block truncate" title={file}>
            {file}
          </span>
          <span className="mt-1 block">{S.library.dropsNote}</span>
        </>
      }
      closeLabel={S.library.close}
    >
      <div className="u-scroll max-h-80 overflow-y-auto">
          {rows.map((r) => (
            <div
              key={`${r.reason}:${r.detail}`}
              className="border-b border-line py-3 last:border-0"
            >
              <div className="flex items-baseline gap-2">
                <span className="text-body text-ink">
                  {S.library.dropReason[r.reason] ?? r.reason}
                </span>
                <span className="u-num ml-auto text-small text-ink-2">×{r.count}</span>
              </div>
              <div className="mt-1 font-mono text-fine text-ink-2 break-all">
                {r.detail}
              </div>
              {r.example && (
                <div className="mt-1 text-fine text-ink-2 break-all">
                  {S.library.dropsExample} {r.example}
                </div>
              )}
            </div>
          ))}
      </div>
    </Dialog>
  );
}

function ErrorModal({
  file,
  kind,
  text,
  onClose,
}: {
  file: string;
  kind: string;
  text: string;
  onClose: () => void;
}) {
  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={S.library.errorTitle}
      description={
        <span className="block truncate" title={file}>
          {file} · {kind}
        </span>
      }
      closeLabel={S.library.close}
      footer={
        <>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => copyAndSay(text, S.library.errorCopied)}
          >
            {S.library.copyError}
          </Button>
          <Button variant="primary" size="sm" onClick={onClose}>
            {S.library.close}
          </Button>
        </>
      }
    >
      <pre className="u-scroll max-h-72 overflow-auto rounded-panel border border-line bg-surface p-3 text-small leading-relaxed text-ink-2 whitespace-pre-wrap break-words">
        {text}
      </pre>
    </Dialog>
  );
}

/** API source key: viewable at any time (an Editor-only endpoint), with rotation built in. */
function TokenModal({
  kbId,
  sourceId,
  kind,
  onClose,
}: {
  kbId: string;
  sourceId: string;
  kind: string;
  onClose: () => void;
}) {
  const queryClient = useQueryClient();
  const tokenQuery = useQuery({
    queryKey: ["sourceToken", kbId, sourceId],
    queryFn: () => api.sourceToken(kbId, sourceId),
  });
  const rotate = useMutation({
    mutationFn: () => api.rotateSourceToken(kbId, sourceId),
    onSuccess: (r) => {
      queryClient.setQueryData(["sourceToken", kbId, sourceId], { ingest_token: r.ingest_token });
      toast.success(S.toast.saved);
    },
    onError: (e) => toast.error((e as Error).message),
  });
  const token = tokenQuery.data?.ingest_token ?? null;

  const endpoint = pushEndpoint(location.origin, sourceId, kind);
  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={
        <span className="flex items-center gap-2">
          <KeyRound size={14} className="text-ink-2" />
          {S.library.tokenTitle}
        </span>
      }
      closeLabel={S.library.close}
      footer={
        <>
          <Button
            variant="secondary"
            size="sm"
            className="flex items-center gap-2"
            disabled={rotate.isPending || tokenQuery.isPending}
            onClick={() => rotate.mutate()}
          >
            <RefreshCw size={11} className={rotate.isPending ? "animate-spin" : ""} />
            {token ? S.library.rotateToken : S.library.generateToken}
          </Button>
          <Button variant="primary" size="sm" onClick={onClose}>
            {S.library.close}
          </Button>
        </>
      }
    >
      <div className="space-y-3">
          {tokenQuery.isPending ? (
            <p className="text-body text-ink-2">{S.nav.loading}</p>
          ) : token ? (
            <>
              <Button
                variant="secondary"
                className="h-auto w-full justify-start whitespace-normal break-all py-3 text-left font-mono"
                title={S.library.copyEndpoint}
                onClick={() => copyAndSay(token, S.library.tokenCopied)}
              >
                {token}
              </Button>
              <p className="text-fine leading-relaxed text-ink-2">
                {S.library.tokenWarning}
              </p>
              <div>
                <p className="mb-1 text-fine text-ink-2">{S.library.tokenUsage}</p>
                <Button
                  variant="secondary"
                  className="h-auto w-full justify-start whitespace-pre-wrap break-all py-2 text-left font-mono text-ink-2"
                  title={S.library.copyEndpoint}
                  onClick={() => copyAndSay(endpoint, S.library.endpointCopied)}
                >
                  POST {endpoint}
                  {"\n"}Authorization: Bearer &lt;token&gt;
                </Button>
              </div>
            </>
          ) : (
            <p className="text-body text-ink-2">{S.library.noToken}</p>
          )}
      </div>
    </Dialog>
  );
}

/** History view: the sync-run records that take the place of the file list (click History again to switch back). */
function RunsPanel({ kbId, sourceId }: { kbId: string; sourceId: string }) {
  const runs = useQuery({
    queryKey: ["sourceRuns", kbId, sourceId],
    queryFn: () => api.sourceRuns(kbId, sourceId),
  });
  const list = runs.data?.runs ?? [];

  return (
    <div className="glass rounded-panel">
      {runs.isLoading ? (
        <div className="py-20 text-center text-body text-ink-2">{S.nav.loading}</div>
      ) : list.length === 0 ? (
        <div className="py-20 text-center text-body text-ink-2">{S.library.noRuns}</div>
      ) : (
        <div className="px-4 py-2">
          {list.map((r) => (
            <div
              key={r.id}
              className="flex items-center gap-3 py-2 text-small border-b border-line last:border-0"
            >
              <span
                className={`h-1.5 w-1.5 rounded-full shrink-0 ${
                  r.status === "ok"
                    ? "bg-ok"
                    : r.status === "failed"
                      ? "bg-danger"
                      : "bg-warn animate-pulse"
                }`}
              />
              <span className="u-num text-ink-2 whitespace-nowrap shrink-0">
                {r.started_at.slice(0, 16).replace("T", " ")}
              </span>
              <span className="text-ink-2 whitespace-nowrap shrink-0">
                {r.created_docs > 0 && S.library.runNew(r.created_docs)}
                {r.created_docs > 0 && r.updated_docs > 0 && " · "}
                {r.updated_docs > 0 && S.library.runUpdated(r.updated_docs)}
                {r.status === "ok" && r.created_docs === 0 && r.updated_docs === 0 && (
                  <span className="text-ink-2">{S.library.runNothing}</span>
                )}
              </span>
              {r.error && (
                <span className="text-danger truncate min-w-0" title={r.error}>
                  {r.error}
                </span>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** New-source dialog: type → name/icon → type-specific config → sync cycle. */
function SourceModal({
  kbId,
  onDone,
}: {
  kbId: string;
  /** When isApi=true, the parent opens the key dialog right after */
  /** The second argument: when creating a push source, its kind (`api` / `statements`), or `google_drive`
   *  (it still needs an account connected and a folder chosen after creation); nothing else is given */
  onDone: (id?: string, pushKind?: string) => void;
}) {
  const [kind, setKind] = useState<CreatableSourceKind>("folder");
  const [name, setName] = useState("");
  const [icon, setIcon] = useState<string | null>(null);
  const [urls, setUrls] = useState("");
  const [feedUrl, setFeedUrl] = useState("");
  const [rssContentMode, setRssContentMode] = useState<RssContentMode>("feed");
  const [endpoint, setEndpoint] = useState("");
  const [authHeader, setAuthHeader] = useState("");
  const [repo, setRepo] = useState("");
  const [jiraUrl, setJiraUrl] = useState("");
  const [jiraProject, setJiraProject] = useState("");
  const [s3Bucket, setS3Bucket] = useState("");
  const [s3Prefix, setS3Prefix] = useState("");
  const [s3Endpoint, setS3Endpoint] = useState("");
  const [s3Region, setS3Region] = useState("");
  const [s3Key, setS3Key] = useState("");
  const [s3Secret, setS3Secret] = useState("");
  const [azAccount, setAzAccount] = useState("");
  const [azKey, setAzKey] = useState("");
  const [gcsKey, setGcsKey] = useState("");
  const [davUrl, setDavUrl] = useState("");
  const [davPath, setDavPath] = useState("");
  const [davUser, setDavUser] = useState("");
  const [davPass, setDavPass] = useState("");
  const [notionToken, setNotionToken] = useState("");
  const [notionQuery, setNotionQuery] = useState("");
  // A PR is also an issue in GitHub's model. Not collected by default: asking for "issues" means issues;
  // but some repos actually record decisions in PR descriptions, so a toggle is provided
  const [includePrs, setIncludePrs] = useState(false);
  const [includeGlobs, setIncludeGlobs] = useState("");
  const [excludeGlobs, setExcludeGlobs] = useState("");
  const [schedule, setSchedule] = useState<ScheduleValue>({
    sync_interval_minutes: null,
    sync_cron: null,
  });
  // folder = a pure container, api = push type: neither has a sync schedule
  const syncing =
    kind === "url" ||
    kind === "rss" ||
    kind === "custom" ||
    kind === "github_issues" ||
    kind === "github_repo" ||
    kind === "gitlab_repo" ||
    kind === "gitlab_issues" ||
    kind === "jira_issues" ||
    kind === "s3" ||
    kind === "azure_blob" ||
    kind === "gcs" ||
    kind === "webdav" ||
    kind === "notion" ||
    kind === "google_drive";
  // If the deployment has no OAuth client configured, creation still cannot connect, so do not allow it
  const driveStatus = useQuery({
    queryKey: ["google-drive-status"],
    queryFn: api.googleDriveStatus,
    enabled: kind === "google_drive",
  });

  const create = useMutation({
    mutationFn: () => {
      const config =
        kind === "url"
          ? { urls: urls.split("\n").map((u) => u.trim()).filter(Boolean) }
          : kind === "rss"
            ? { feed_url: feedUrl.trim(), content_mode: rssContentMode }
            : kind === "custom"
              ? {
                  endpoint: endpoint.trim(),
                  ...(authHeader.trim() ? { auth_header: authHeader.trim() } : {}),
                }
              : kind === "github_issues" || kind === "github_repo"
                ? {
                    repo: repo.trim(),
                    ...(authHeader.trim() ? { auth_header: authHeader.trim() } : {}),
                    ...(kind === "github_issues"
                      ? includePrs ? { include_pull_requests: true } : {}
                      : { include: globList(includeGlobs), exclude: globList(excludeGlobs) }),
                  }
                : kind === "gitlab_repo"
                  ? {
                      project: repo.trim(),
                      ...(authHeader.trim() ? { auth_header: authHeader.trim() } : {}),
                      include: globList(includeGlobs),
                      exclude: globList(excludeGlobs),
                    }
                : kind === "gitlab_issues"
                  ? {
                      project: repo.trim(),
                      ...(authHeader.trim() ? { auth_header: authHeader.trim() } : {}),
                      ...(includePrs ? { include_merge_requests: true } : {}),
                    }
                : kind === "jira_issues"
                  ? {
                      base_url: jiraUrl.trim(),
                      project: jiraProject.trim(),
                      ...(authHeader.trim() ? { auth_header: authHeader.trim() } : {}),
                    }
                  : kind === "s3"
                    ? {
                        bucket: s3Bucket.trim(),
                        ...(s3Prefix.trim() ? { prefix: s3Prefix.trim() } : {}),
                        // An empty endpoint = public-cloud S3; if filled, it is self-hosted, and the backend uses path-style accordingly
                        ...(s3Endpoint.trim() ? { endpoint: s3Endpoint.trim() } : {}),
                        ...(s3Region.trim() ? { region: s3Region.trim() } : {}),
                        ...(s3Key.trim() ? { access_key_id: s3Key.trim() } : {}),
                        ...(s3Secret.trim() ? { secret_access_key: s3Secret.trim() } : {}),
                      }
                    : kind === "azure_blob"
                      ? {
                          bucket: s3Bucket.trim(),
                          ...(s3Prefix.trim() ? { prefix: s3Prefix.trim() } : {}),
                          ...(s3Endpoint.trim() ? { endpoint: s3Endpoint.trim() } : {}),
                          ...(azAccount.trim() ? { account_name: azAccount.trim() } : {}),
                          ...(azKey.trim() ? { account_key: azKey.trim() } : {}),
                        }
                      : kind === "gcs"
                        ? {
                            bucket: s3Bucket.trim(),
                            ...(s3Prefix.trim() ? { prefix: s3Prefix.trim() } : {}),
                            ...(s3Endpoint.trim() ? { endpoint: s3Endpoint.trim() } : {}),
                            ...(gcsKey.trim()
                              ? { service_account_key: gcsKey.trim() }
                              : {}),
                          }
                        : kind === "webdav"
                          ? {
                              base_url: davUrl.trim(),
                              ...(davPath.trim() ? { path: davPath.trim() } : {}),
                              ...(davUser.trim() ? { username: davUser.trim() } : {}),
                              ...(davPass ? { password: davPass } : {}),
                            }
                          : kind === "notion"
                            ? {
                                token: notionToken.trim(),
                                ...(notionQuery.trim()
                                  ? { query: notionQuery.trim() }
                                  : {}),
                              }
                            : {};
      return api.createSource(kbId, {
        kind,
        name: name.trim(),
        config,
        // Built-in type icons are fixed; only custom allows choosing one
        icon: kind === "custom" ? icon : null,
        // folder/api have no sync semantics: the schedule is forced to be empty
        ...(syncing ? schedule : { sync_interval_minutes: null, sync_cron: null }),
      });
    },
    onSuccess: (data) =>
      onDone(
        data.source.id,
        kind === "api" || kind === "statements" || kind === "google_drive" ? kind : undefined,
      ),
  });

  const valid =
    name.trim() &&
    (kind === "url"
      ? urls.trim()
      : kind === "rss"
        ? feedUrl.trim()
        : kind === "custom"
          ? endpoint.trim()
          : kind === "notion"
            ? notionToken.trim()
            : kind === "google_drive"
              ? driveStatus.data?.enabled
            : kind === "webdav"
              ? davUrl.trim()
            : kind === "s3" || kind === "azure_blob" || kind === "gcs"
            ? s3Bucket.trim()
            : hasSyncFields(kind, { repo, jiraUrl, jiraProject, token: authHeader }));

  // Note: use div rather than label, because label forwards :hover/click to the first markable control inside
  //(a button counts too), so while hovering the icon grid or schedule picker, the first button would stay lit
  const field = (label: string, node: React.ReactNode) => (
    <div className="mb-3">
      <div className="mb-1 text-fine font-medium text-ink-2">{label}</div>
      {node}
    </div>
  );

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onDone()}
      title={S.library.newSourceTitle}
      closeLabel={S.library.close}
      footer={
        <>
          <Button variant="secondary" size="sm" onClick={() => onDone()}>
            {S.library.cancel}
          </Button>
          <Button
            variant="primary"
            size="sm"
            disabled={!valid || create.isPending}
            onClick={() => create.mutate()}
          >
            {S.library.createSource}
          </Button>
        </>
      }
    >
      <div>
          {/* Type: twelve kinds, held in one dropdown — laid out as buttons they would take four rows, and the dialog would be full
              before the name is even typed. The icon only appears in the option label: the trigger shows the label of the selected item,
              so giving the trigger an icon as well would draw it twice */}
          {field(
            S.library.sourceType,
            <Dropdown
              className="w-full"
              value={kind}
              onChange={(v) => setKind(v as CreatableSourceKind)}
              options={CREATABLE_SOURCE_KINDS.map((k) => {
                const Icon = KIND_ICON[k];
                return {
                  value: k,
                  label: (
                    <span className="flex items-center gap-2">
                      <Icon size={12} className="shrink-0 text-ink-2" />
                      {S.library.sourceKinds[k]}
                    </span>
                  ),
                };
              })}
            />,
          )}
          {/* The type explains itself: one line of description; interface details move into the built-in docs, and the dialog keeps only a link */}
          <p className="mb-4 text-fine leading-relaxed text-ink-2">
            {S.library.sourceKindHints[kind]}
            {kind === "custom" && (
              <>
                {" "}
                <Link
                  to="/docs/$slug"
                  params={{ slug: "ingest" }}
                  target="_blank"
                  className="u-link"
                >
                  {S.library.ingestGuide}
                </Link>
              </>
            )}
          </p>

          {field(
            S.library.sourceName,
            <Input className="w-full"
              value={name}
              onChange={(e) => setName(e.target.value)}
              autoFocus
            />,
          )}

          {/* Built-in type icons are fixed; only custom opens icon selection */}
          {kind === "custom" &&
            field(
              S.library.iconLabel,
              <div className="grid grid-cols-10 gap-1">
                {Object.entries(SOURCE_ICONS).map(([key, Icon]) => (
                  <IconButton
                    key={key}
                    variant={icon === key ? "primary" : "ghost"}
                    label={key}
                    onClick={() => setIcon(icon === key ? null : key)}
                  >
                    <Icon size={14} />
                  </IconButton>
                ))}
              </div>,
            )}

          {kind === "custom" && (
            <>
              {field(
                S.library.endpointField,
                <Input className="w-full font-mono"
                  placeholder={S.library.feedPlaceholder}
                  value={endpoint}
                  onChange={(e) => setEndpoint(e.target.value)}
                />,
              )}
              {field(
                S.library.authHeaderField,
                <Input className="w-full font-mono"
                  type="password"
                  placeholder="Bearer sk-…"
                  value={authHeader}
                  onChange={(e) => setAuthHeader(e.target.value)}
                />,
              )}
            </>
          )}

          {kind === "url" &&
            field(
              S.library.urlsField,
              <Textarea className="w-full font-mono resize-y"
                value={urls}
                onChange={(e) => setUrls(e.target.value)}
              />,
            )}
          {kind === "rss" && (
            <>
              {field(
                S.library.feedUrl,
                <Input
                  className="w-full font-mono"
                  value={feedUrl}
                  onChange={(e) => setFeedUrl(e.target.value)}
                />,
              )}
              {field(
                S.library.rssContentMode,
                <Dropdown
                  className="w-full"
                  value={rssContentMode}
                  onChange={(v) => setRssContentMode(v as RssContentMode)}
                  options={[
                    { value: "full_new_items", label: S.library.rssModeFull },
                    { value: "feed", label: S.library.rssModeFeed },
                  ]}
                />,
              )}
              <p className="-mt-1 mb-3 text-fine leading-relaxed text-ink-2">
                {rssContentMode === "full_new_items"
                  ? S.library.rssFullModeHint
                  : S.library.rssFeedModeHint}
              </p>
            </>
          )}
          {kind === "jira_issues" && (
            <>
              {field(
                S.library.jiraUrlField,
                <Input className="w-full font-mono"
                  placeholder="https://jira.example.com"
                  value={jiraUrl}
                  onChange={(e) => setJiraUrl(e.target.value)}
                />,
              )}
              {field(
                S.library.jiraProjectField,
                <Input className="w-full font-mono"
                  placeholder="KAFKA"
                  value={jiraProject}
                  onChange={(e) => setJiraProject(e.target.value)}
                />,
              )}
              {field(
                S.library.tokenField,
                <Input className="w-full font-mono"
                  type="password"
                  placeholder="Basic dXNlcjp0b2tlbg=="
                  value={authHeader}
                  onChange={(e) => setAuthHeader(e.target.value)}
                />,
              )}
            </>
          )}
          {(kind === "s3" || kind === "azure_blob" || kind === "gcs") && (
            <>
              {field(
                S.library.s3BucketField,
                <Input className="w-full font-mono"
                  placeholder="documents"
                  value={s3Bucket}
                  onChange={(e) => setS3Bucket(e.target.value)}
                />,
              )}
              {field(
                S.library.s3PrefixField,
                <Input className="w-full font-mono"
                  placeholder="reports/2026/"
                  value={s3Prefix}
                  onChange={(e) => setS3Prefix(e.target.value)}
                />,
              )}
              {field(
                S.library.s3EndpointField,
                <Input className="w-full font-mono"
                  placeholder="http://minio.internal:9000"
                  value={s3Endpoint}
                  onChange={(e) => setS3Endpoint(e.target.value)}
                />,
              )}
              {field(
                S.library.s3RegionField,
                <Input className="w-full font-mono"
                  placeholder="us-east-1"
                  value={s3Region}
                  onChange={(e) => setS3Region(e.target.value)}
                />,
              )}
              {kind === "s3" && (
                <>
                  {field(
                    S.library.s3KeyField,
                    <Input className="w-full font-mono"
                      value={s3Key}
                      onChange={(e) => setS3Key(e.target.value)}
                    />,
                  )}
                  {field(
                    S.library.s3SecretField,
                    <Input className="w-full font-mono"
                      type="password"
                      value={s3Secret}
                      onChange={(e) => setS3Secret(e.target.value)}
                    />,
                  )}
                </>
              )}
              {kind === "azure_blob" && (
                <>
                  {field(
                    S.library.azAccountField,
                    <Input className="w-full font-mono"
                      value={azAccount}
                      onChange={(e) => setAzAccount(e.target.value)}
                    />,
                  )}
                  {field(
                    S.library.azKeyField,
                    <Input className="w-full font-mono"
                      type="password"
                      value={azKey}
                      onChange={(e) => setAzKey(e.target.value)}
                    />,
                  )}
                </>
              )}
              {kind === "gcs" &&
                field(
                  S.library.gcsKeyField,
                  <Textarea className="w-full font-mono"
                    placeholder='{"type":"service_account",...}'
                    value={gcsKey}
                    onChange={(e) => setGcsKey(e.target.value)}
                  />,
                )}
            </>
          )}
          {kind === "webdav" && (
            <>
              {field(
                S.library.davUrlField,
                <Input className="w-full font-mono"
                  placeholder="https://cloud.example.com/remote.php/dav/files/alice"
                  value={davUrl}
                  onChange={(e) => setDavUrl(e.target.value)}
                />,
              )}
              {field(
                S.library.davPathField,
                <Input className="w-full font-mono"
                  placeholder="/Documents"
                  value={davPath}
                  onChange={(e) => setDavPath(e.target.value)}
                />,
              )}
              {field(
                S.library.davUserField,
                <Input className="w-full font-mono"
                  value={davUser}
                  onChange={(e) => setDavUser(e.target.value)}
                />,
              )}
              {field(
                S.library.davPassField,
                <Input className="w-full font-mono"
                  type="password"
                  value={davPass}
                  onChange={(e) => setDavPass(e.target.value)}
                />,
              )}
            </>
          )}
          {kind === "google_drive" && (
            <p className="mb-3 text-fine leading-relaxed text-ink-2">
              {driveStatus.data && !driveStatus.data.enabled
                ? S.library.googleDrive.notConfigured
                : S.library.googleDrive.createHint}
            </p>
          )}
          {kind === "notion" && (
            <>
              {field(
                S.library.notionTokenField,
                <Input className="w-full font-mono"
                  type="password"
                  placeholder="ntn_..."
                  value={notionToken}
                  onChange={(e) => setNotionToken(e.target.value)}
                />,
              )}
              {field(
                S.library.notionQueryField,
                <Input className="w-full font-mono"
                  value={notionQuery}
                  onChange={(e) => setNotionQuery(e.target.value)}
                />,
              )}
            </>
          )}
          {(kind === "github_issues" || kind === "github_repo" || kind.startsWith("gitlab_")) && (
            <>
              {field(
                kind.startsWith("gitlab_") ? S.library.gitlabProjectField : S.library.repoField,
                <Input className="w-full font-mono"
                  placeholder={kind.startsWith("gitlab_") ? "group/name" : "owner/name"}
                  value={repo}
                  onChange={(e) => setRepo(e.target.value)}
                />,
              )}
              {field(
                kind === "gitlab_issues"
                  ? S.library.gitlabIssuesTokenField
                  : kind === "gitlab_repo" ? S.library.gitlabTokenField : S.library.tokenField,
                <Input className="w-full font-mono"
                  type="password"
                  placeholder={kind.startsWith("gitlab_") ? "Bearer glpat-…" : "Bearer ghp_…"}
                  value={authHeader}
                  onChange={(e) => setAuthHeader(e.target.value)}
                />,
              )}
              {kind === "github_issues" || kind === "gitlab_issues" ? (
                <Checkbox
                  className="mb-4"
                  checked={includePrs}
                  onChange={(v) => setIncludePrs(v)}
                  label={
                    kind === "gitlab_issues"
                      ? S.library.includeMergeRequests
                      : S.library.includePullRequests
                  }
                />
              ) : (
                <>
                  {field(
                    S.library.includeGlobsField,
                    <Input className="w-full font-mono"
                      placeholder="**/*.md, **/*.markdown, **/*.mdx"
                      value={includeGlobs}
                      onChange={(e) => setIncludeGlobs(e.target.value)}
                    />,
                  )}
                  {field(
                    S.library.excludeGlobsField,
                    <Input className="w-full font-mono"
                      placeholder="**/.*/**, **/node_modules/**, **/vendor/**"
                      value={excludeGlobs}
                      onChange={(e) => setExcludeGlobs(e.target.value)}
                    />,
                  )}
                </>
              )}
            </>
          )}

          {syncing && field(S.library.interval, <SchedulePicker onChange={setSchedule} />)}

          {create.isError && (
            <p className="text-small text-danger mb-2">{(create.error as Error).message}</p>
          )}
      </div>
    </Dialog>
  );
}

/** Source settings dialog: rename / icon (custom only) / ingestion config / schedule; the type cannot be changed; danger zone at the bottom. */
function SourceEditModal({
  kbId,
  source,
  onDone,
  onDelete,
}: {
  kbId: string;
  source: SourceView;
  onDone: () => void;
  onDelete: () => void;
}) {
  const kind = source.kind;
  const cfg = source.config ?? {};
  const [name, setName] = useState(source.name);
  const [icon, setIcon] = useState<string | null>(source.icon);
  const [urls, setUrls] = useState((cfg.urls ?? []).join("\n"));
  const [feedUrl, setFeedUrl] = useState(cfg.feed_url ?? "");
  const [rssContentMode, setRssContentMode] = useState<RssContentMode>(
    cfg.content_mode === "full_new_items" ? "full_new_items" : "feed",
  );
  const [endpoint, setEndpoint] = useState(cfg.endpoint ?? "");
  const [authHeader, setAuthHeader] = useState("");
  // GitLab's project path is stored in `project` (same name as Jira's project key but a different meaning), and this form shares the one field
  const [repo, setRepo] = useState((kind.startsWith("gitlab_") ? cfg.project : cfg.repo) ?? "");
  const [includePrs, setIncludePrs] = useState(
    Boolean(kind === "gitlab_issues" ? cfg.include_merge_requests : cfg.include_pull_requests),
  );
  const [includeGlobs, setIncludeGlobs] = useState((cfg.include ?? []).join(", "));
  const [excludeGlobs, setExcludeGlobs] = useState((cfg.exclude ?? []).join(", "));
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [schedule, setSchedule] = useState<ScheduleValue>({
    sync_interval_minutes: source.sync_interval_minutes,
    sync_cron: source.sync_cron,
  });
  const [decayOn, setDecayOn] = useState(source.decay != null);
  const [decay, setDecay] = useState(() => decayFormValue(source.decay));
  const syncing = SYNCING_KINDS.has(kind);
  const KindIcon = KIND_ICON[kind as keyof typeof KIND_ICON] ?? Upload;

  // Whether the ingestion config has changed — if so, show the "what happens to old documents" explanation
  const ingestChanged =
    (kind === "url" && urls.trim() !== (cfg.urls ?? []).join("\n").trim()) ||
    (kind === "rss" &&
      (feedUrl.trim() !== (cfg.feed_url ?? "") ||
        rssContentMode !== (cfg.content_mode === "full_new_items" ? "full_new_items" : "feed"))) ||
    (kind === "custom" && endpoint.trim() !== (cfg.endpoint ?? "")) ||
    // Changing the repo or toggling whether PRs are collected both replace the set of documents in this source
    (kind === "github_issues" &&
      (repo.trim() !== (cfg.repo ?? "") ||
        includePrs !== Boolean(cfg.include_pull_requests))) ||
    (kind === "gitlab_issues" &&
      (repo.trim() !== (cfg.project ?? "") ||
        includePrs !== Boolean(cfg.include_merge_requests))) ||
    ((kind === "github_repo" || kind === "gitlab_repo") &&
      (repo.trim() !== ((kind === "gitlab_repo" ? cfg.project : cfg.repo) ?? "") ||
        String(globList(includeGlobs) ?? "") !== String(cfg.include ?? "") ||
        String(globList(excludeGlobs) ?? "") !== String(cfg.exclude ?? "")));

  const save = useMutation({
    mutationFn: () => {
      const config =
        kind === "url"
          ? { urls: urls.split("\n").map((u) => u.trim()).filter(Boolean) }
          : kind === "rss"
            ? { feed_url: feedUrl.trim(), content_mode: rssContentMode }
            : kind === "custom"
              ? {
                  endpoint: endpoint.trim(),
                  // Empty = the backend keeps the value already in the library (credentials go in but never come out)
                  ...(authHeader.trim() ? { auth_header: authHeader.trim() } : {}),
                }
              : kind === "github_issues"
                ? {
                    repo: repo.trim(),
                    // Empty = the backend keeps the value already in the library (credentials go in but never come out)
                    ...(authHeader.trim() ? { auth_header: authHeader.trim() } : {}),
                    include_pull_requests: includePrs,
                  }
                : kind === "gitlab_issues"
                  ? {
                      project: repo.trim(),
                      ...(authHeader.trim() ? { auth_header: authHeader.trim() } : {}),
                      // Empty = the backend keeps the value already in the library (credentials go in but never come out)
                      include_merge_requests: includePrs,
                    }
                : kind === "github_repo" || kind === "gitlab_repo"
                  ? githubRepoConfig(
                      cfg,
                      { repo, authHeader, include: includeGlobs, exclude: excludeGlobs },
                      kind === "gitlab_repo" ? "project" : "repo",
                    )
                  : undefined;
      return api.updateSource(kbId, source.id, {
        name: name.trim(),
        ...(kind === "custom" && icon ? { icon } : {}),
        ...(config ? { config } : {}),
        ...(syncing ? { schedule } : {}),
        ...decayChange(source.decay, decayOn, decay),
      });
    },
    onSuccess: () => onDone(),
  });

  const valid =
    name.trim() &&
    !(decayOn && decayIssue(decay)) &&
    (kind === "url"
      ? urls.trim()
      : kind === "rss"
        ? feedUrl.trim()
        : kind === "custom"
          ? endpoint.trim()
          : // Jira's configuration cannot be changed here: blocking it would only leave people unable to even change the name
            kind === "jira_issues" || hasSyncFields(kind, { repo }));

  // div rather than label: label forwards :hover/click to the first markable control
  const field = (label: string, node: React.ReactNode) => (
    <div className="mb-3">
      <div className="mb-1 text-fine font-medium text-ink-2">{label}</div>
      {node}
    </div>
  );

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onDone()}
      title={S.library.editSourceTitle}
      closeLabel={S.library.close}
      footer={
        <>
          <Button variant="secondary" size="sm" onClick={onDone}>
            {S.library.cancel}
          </Button>
          <Button
            variant="primary"
            size="sm"
            disabled={!valid || save.isPending}
            onClick={() => save.mutate()}
          >
            {S.library.saveChanges}
          </Button>
        </>
      }
    >
      <div>
          {/* The type is read-only: changing the type means changing the identity, so a new source should be created */}
          <div className="mb-4 flex items-center gap-2 text-small text-ink-2">
            <KindIcon size={13} className="text-ink-2" />
            {S.library.sourceKinds[kind as keyof typeof S.library.sourceKinds] ?? kind}
          </div>

          {field(
            S.library.sourceName,
            <Input className="w-full"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />,
          )}

          {kind === "custom" &&
            field(
              S.library.iconLabel,
              <div className="grid grid-cols-10 gap-1">
                {Object.entries(SOURCE_ICONS).map(([key, Icon]) => (
                  <IconButton
                    key={key}
                    variant={icon === key ? "primary" : "ghost"}
                    label={key}
                    onClick={() => setIcon(icon === key ? null : key)}
                  >
                    <Icon size={14} />
                  </IconButton>
                ))}
              </div>,
            )}

          {kind === "url" &&
            field(
              S.library.urlsField,
              <Textarea className="w-full font-mono resize-y"
                value={urls}
                onChange={(e) => setUrls(e.target.value)}
              />,
            )}
          {kind === "rss" && (
            <>
              {field(
                S.library.feedUrl,
                <Input
                  className="w-full font-mono"
                  value={feedUrl}
                  onChange={(e) => setFeedUrl(e.target.value)}
                />,
              )}
              {field(
                S.library.rssContentMode,
                <Dropdown
                  className="w-full"
                  value={rssContentMode}
                  onChange={(v) => setRssContentMode(v as RssContentMode)}
                  options={[
                    { value: "full_new_items", label: S.library.rssModeFull },
                    { value: "feed", label: S.library.rssModeFeed },
                  ]}
                />,
              )}
              <p className="-mt-1 mb-3 text-fine leading-relaxed text-ink-2">
                {rssContentMode === "full_new_items"
                  ? S.library.rssFullModeHint
                  : S.library.rssFeedModeHint}
              </p>
            </>
          )}
          {(kind === "github_issues" || kind === "github_repo" || kind.startsWith("gitlab_")) && (
            <>
              {field(
                kind.startsWith("gitlab_") ? S.library.gitlabProjectField : S.library.repoField,
                <Input className="w-full font-mono"
                  placeholder={kind.startsWith("gitlab_") ? "group/name" : "owner/name"}
                  value={repo}
                  onChange={(e) => setRepo(e.target.value)}
                />,
              )}
              {field(
                kind === "gitlab_issues"
                  ? S.library.gitlabIssuesTokenField
                  : kind === "gitlab_repo" ? S.library.gitlabTokenField : S.library.tokenField,
                <Input className="w-full font-mono"
                  type="password"
                  placeholder={kind.startsWith("gitlab_") ? "Bearer glpat-…" : "Bearer ghp_…"}
                  value={authHeader}
                  onChange={(e) => setAuthHeader(e.target.value)}
                />,
              )}
              {kind === "github_issues" || kind === "gitlab_issues" ? (
                <Checkbox
                  className="mb-4"
                  checked={includePrs}
                  onChange={(v) => setIncludePrs(v)}
                  label={
                    kind === "gitlab_issues"
                      ? S.library.includeMergeRequests
                      : S.library.includePullRequests
                  }
                />
              ) : (
                <>
                  {field(
                    S.library.includeGlobsField,
                    <Input className="w-full font-mono"
                      placeholder="**/*.md, **/*.markdown, **/*.mdx"
                      value={includeGlobs}
                      onChange={(e) => setIncludeGlobs(e.target.value)}
                    />,
                  )}
                  {field(
                    S.library.excludeGlobsField,
                    <Input className="w-full font-mono"
                      placeholder="**/.*/**, **/node_modules/**, **/vendor/**"
                      value={excludeGlobs}
                      onChange={(e) => setExcludeGlobs(e.target.value)}
                    />,
                  )}
                </>
              )}
            </>
          )}
          {kind === "custom" && (
            <>
              {field(
                S.library.endpointField,
                <Input className="w-full font-mono"
                  value={endpoint}
                  onChange={(e) => setEndpoint(e.target.value)}
                />,
              )}
              {field(
                S.library.authHeaderEditField,
                <Input className="w-full font-mono"
                  type="password"
                  placeholder={S.library.authKeepHint}
                  value={authHeader}
                  onChange={(e) => setAuthHeader(e.target.value)}
                />,
              )}
            </>
          )}

          {kind === "google_drive" && <GoogleDrivePanel kbId={kbId} source={source} />}

          {ingestChanged && (
            <p className="mb-3 text-fine leading-relaxed text-ink-2">
              {S.library.editKeepNote}
            </p>
          )}

          {syncing &&
            field(
              S.library.interval,
              <SchedulePicker
                initial={{
                  sync_interval_minutes: source.sync_interval_minutes,
                  sync_cron: source.sync_cron,
                }}
                onChange={setSchedule}
              />,
            )}

          <SourceDecayFields
            enabled={decayOn}
            onEnabled={setDecayOn}
            value={decay}
            onChange={setDecay}
          />

          {save.isError && (
            <p className="text-small text-danger mb-2">{(save.error as Error).message}</p>
          )}

          {/* Danger zone: delete the source (documents are kept and fall back to Uploads) */}
          <div className="mt-4 pt-3 border-t border-line">
            <div className="mb-1 text-fine font-medium text-ink-2">
              {S.library.dangerZone}
            </div>
            <div className="flex items-center justify-between gap-3">
              <span className="text-fine text-ink-2">{S.library.deleteSourceHint}</span>
              {/* The same principle: this only opens the confirmation dialog; the red colour is inside the dialog */}
              <Button variant="secondary" size="sm" className="shrink-0"
                onClick={() => setConfirmingDelete(true)}
              >
                {S.library.deleteSource}
              </Button>
            </div>
          </div>
      </div>

      {/* The danger confirmation is layered over the settings dialog: Radix's stacking only makes the topmost layer respond to Esc and the overlay */}
      {confirmingDelete && (
        <DangerConfirm
          title={S.library.deleteSourceTitle}
          hint={S.library.deleteSourceBody(source.name)}
          requireText={source.name}
          confirmLabel={S.library.deleteSource}
          cancelLabel={S.library.cancel}
          onConfirm={onDelete}
          onCancel={() => setConfirmingDelete(false)}
        />
      )}
    </Dialog>
  );
}

/** The "Deleted" view (#268): tombstones wait here to be restored or purged. Only a library admin can press purge */
function DeletedTable({
  docs,
  sources,
  canPurge,
  onRestore,
  onPurge,
}: {
  docs: Doc[];
  sources: SourceView[];
  canPurge: boolean;
  onRestore: (id: string) => void;
  onPurge: (doc: Doc) => void;
}) {
  if (!docs.length) {
    return (
      <div className="py-20 text-center text-body text-ink-2">{S.library.deletedEmpty}</div>
    );
  }
  return (
    <table className="w-full text-body">
      <thead>
        <tr className="text-left text-small text-ink-2 border-b border-line">
          <th className="px-4 py-3 font-medium">{S.library.colFile}</th>
          <th className="px-4 py-3 font-medium">{S.library.colSource}</th>
          <th className="px-4 py-3 font-medium">{S.library.colDeleted}</th>
          <th className="px-4 py-3"></th>
        </tr>
      </thead>
      <tbody>
        {docs.map((d) => {
          const src = sources.find((s) => s.id === d.source_id);
          return (
            <tr key={d.id} className="border-b border-line last:border-0">
              <td className="px-4 py-3 text-ink-2">{d.filename}</td>
              <td className="px-4 py-3 text-ink-2">{src?.name ?? S.library.uploads}</td>
              <td className="px-4 py-3 u-num text-ink-2">
                {d.deleted_at ? localDateTime(d.deleted_at) : ""}
              </td>
              <td className="px-4 py-3 text-right whitespace-nowrap">
                <LinkButton onClick={() => onRestore(d.id)}>
                  {S.library.restore}
                </LinkButton>
                {canPurge && (
                  <LinkButton tone="danger" className="ml-4" onClick={() => onPurge(d)}>
                    {S.library.purge}
                  </LinkButton>
                )}
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

function DocRow({
  doc,
  source,
  onDelete,
  onExtract,
  onReprocess,
  onShowError,
  drops,
  onShowDrops,
}: {
  doc: Doc;
  /** undefined = do not render the source column; null = Uploads (source_id is empty) */
  source?: SourceView | null;
  onDelete: () => void;
  onExtract: () => void;
  onReprocess: () => void;
  onShowError: (kind: string, text: string) => void;
  /** Facts this document extracted but that never landed; undefined = none at all */
  drops?: ExtractionDrop[];
  onShowDrops: (rows: ExtractionDrop[]) => void;
}) {
  const kbId = useKbId();
  const dropTotal = drops?.reduce((n, d) => n + d.count, 0) ?? 0;
  const statusText =
    S.library.status[doc.status as keyof typeof S.library.status] ?? doc.status;
  const graphText =
    S.library.graphStatus[doc.graph_status as keyof typeof S.library.graphStatus] ??
    doc.graph_status;
  const SrcIcon = source ? sourceIcon(source) : Upload;
  return (
    <tr className="border-b border-line">
      <td className="px-4 py-3 max-w-xs truncate" title={doc.filename}>
        <Link
          to="/kb/$kbId/doc/$docId"
          params={{ kbId, docId: doc.id }}
          search={{}}
          className="u-inline-link text-ink"
        >
          {doc.filename}
        </Link>
      </td>
      {source !== undefined && (
        <td className="px-4 py-3">
          <span className="flex items-center gap-2 text-small text-ink-2">
            <SrcIcon size={12} className="shrink-0 text-ink-2" />
            <span className="truncate max-w-28">{source?.name ?? S.library.uploads}</span>
          </span>
        </td>
      )}
      <td className="px-4 py-3">
        {/* A failure can be clicked open to see the original text: a tooltip would truncate it and it could not be copied */}
        <StatusCell
          danger={doc.status === "failed"}
          onClick={
            doc.status === "failed" && doc.error
              ? () => onShowError(S.library.errorParse, doc.error!)
              : undefined
          }
        >
          {statusText}
        </StatusCell>
        {doc.missing_since && (
          <span
            className="ml-3 text-small text-ink-2"
            title={doc.missing_since.slice(0, 16).replace("T", " ")}
          >
            {S.library.notInSource}
          </span>
        )}
      </td>
      <td className="px-4 py-3">
        <StatusCell
          danger={doc.graph_status === "failed"}
          onClick={
            doc.graph_status === "failed" && doc.graph_error
              ? () => onShowError(S.library.errorGraph, doc.graph_error!)
              : undefined
          }
        >
          {graphText}
        </StatusCell>
        {/* Facts extracted but never landed. A successful extraction does not mean everything is whole, so this sits alongside graph_status
            rather than replacing it: "done" and "3 dropped" are both true at once.
            **No warning colour**: a missed fact is not a fault, it is what a narrow ontology is designed to do;
            amber next to red would make a person think two things were broken in one row. It is only a number worth opening to look at */}
        {dropTotal > 0 && drops && (
          /* Underlined but not brightened: it can be opened to look at, but it should not be louder than the status next to it */
          <LinkButton
            underline
            className="ml-3 text-ink-2"
            onClick={() => onShowDrops(drops)}
          >
            {S.library.dropsChip(dropTotal)}
          </LinkButton>
        )}
      </td>
      <td className="px-4 py-3 text-ink-2">{doc.chunk_count || "—"}</td>
      <td className="px-4 py-3 text-ink-2">{formatSize(doc.size_bytes)}</td>
      {/* The actions get a column of their own. The badge says what state the row is in now; the actions say what can be done with it;
          if both were squeezed into one cell, the reader would first have to work out which word is clickable.
          This cell has at most one action: re-running parsing needs status=failed, re-extraction needs status=ready,
          and the two are mutually exclusive, so there is no need to order which comes first */}
      {/* **One icon, not three kinds of words.** This column runs for dozens of rows; it used to read "re-extract",
          "extract", "re-parse" — three texts of different lengths stacked in a column, and the reader had to recognize line by line
          which word was clickable, while they all say the same thing: run this document through again. The words moved into the
          tooltip, where they are stated precisely, and this column only needs to say "there is a run-again button here" */}
      <td className="px-4 py-3">
        {doc.status === "failed" ? (
          /* Parse pipeline failed: re-run parse → index → embed (for parser upgrades / transient failure retries) */
          <IconButton label={S.library.reprocess} size="sm" onClick={onReprocess}>
            <RefreshCw size={12} />
          </IconButton>
        ) : doc.status === "ready" &&
          ["none", "failed", "done"].includes(doc.graph_status) ? (
          /* done can also be re-extracted: after an ontology adjustment (descriptions / new classes), a forced full re-extraction is routine */
          <IconButton
            label={doc.graph_status === "done" ? S.library.reExtract : S.library.extract}
            size="sm"
            onClick={onExtract}
          >
            <RefreshCw size={12} />
          </IconButton>
        ) : null}
      </td>
      <td className="px-4 py-3 text-right">
        <LinkButton tone="danger" onClick={onDelete}>
          {S.library.delete}
        </LinkButton>
      </td>
    </tr>
  );
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
