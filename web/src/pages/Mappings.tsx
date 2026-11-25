// Data mapping: what each business concept corresponds to in the database, and how it is computed.
//
// **This page did not exist before**, yet what it manages has always been there: definitions were proposed by the profiling task, confirmed in
// "Review", and then sank into the question-answering system prompt, where people could never see or change them.
// `mappings::revise` and its audit table have been called zero times since they were created.
//
// Approval stays on the same endpoint (`review/mappings/{id}`, which already writes the audit trail).
// What moves is the interface, not the logic: judging whether a definition is right requires seeing the table structure, and that lives on this page.
import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Database, Plug, Plus, Search } from "lucide-react";
import { useNavigate } from "@tanstack/react-router";
import { api, type ConceptMapping } from "../api";
import { S } from "../i18n";
import { useKb } from "../kb";
import { toast } from "../toast";
import {
  Button,
  Checkbox,
  Chip,
  Dropdown,
  EmptyState,
  ErrorText,
  Input,
  Loading,
  PageHeader,
  Pager,
  RAIL_CLS,
  Row,
  SearchSelect,
  Status,
  cn,
  type ChipTone,
} from "../ui";
import { fetchMe } from "../authSession";

const PAGE = 25;

type StatusFilter = "all" | "proposed" | "confirmed" | "rejected";
type MappingDecision = Extract<
  ConceptMapping["status"],
  "confirmed" | "rejected"
>;
const NO_PENDING_MAPPING_IDS: ReadonlySet<string> = new Set();

export const decideMappings = async (
  ids: string[],
  decide: (id: string) => Promise<unknown>,
) => {
  const outcomes = await Promise.allSettled(ids.map((id) => decide(id)));
  const failure = outcomes.find((outcome) => outcome.status === "rejected");
  if (failure?.status === "rejected") throw failure.reason;
};

export const selectedMappingIds = (
  mappings: Pick<ConceptMapping, "id" | "status">[],
  picked: ReadonlySet<string>,
  pending: ReadonlySet<string> = NO_PENDING_MAPPING_IDS,
) =>
  mappings
    .filter(
      (mapping) =>
        mapping.status === "proposed" &&
        picked.has(mapping.id) &&
        !pending.has(mapping.id),
    )
    .map((mapping) => mapping.id);

const TONE: Record<string, ChipTone> = {
  proposed: "warn",
  confirmed: "success",
  rejected: "neutral",
};

/** "How is this number computed". Take one of SQL / expression / table name, in that priority order:
 *  all three answer the same question, and SQL is the most specific while the table name is the coarsest */
const howComputed = (m: ConceptMapping) =>
  m.sql ?? m.expr ?? m.table_name ?? null;

const statusLabel = (s: string) =>
  s === "proposed"
    ? S.mapping.filterProposed
    : s === "confirmed"
      ? S.mapping.filterConfirmed
      : S.mapping.filterRejected;

export function Mappings() {
  const { kb } = useKb();
  const queryClient = useQueryClient();
  const [tab, setTab] = useState<"definitions" | "sources">("definitions");
  const [status, setStatus] = useState<StatusFilter>("all");
  const [q, setQ] = useState("");
  const [page, setPage] = useState(0);
  const [picked, setPicked] = useState<Set<string>>(() => new Set());
  const [deciding, setDeciding] = useState<Set<string>>(() => new Set());
  useEffect(() => setPicked(new Set()), [kb?.id, page, q, status, tab]);

  const data = useQuery({
    queryKey: ["mappings", kb?.id, status, q, page],
    queryFn: () =>
      api.mappings(kb!.id, {
        status: status === "all" ? undefined : status,
        q: q || undefined,
        limit: PAGE,
        offset: page * PAGE,
      }),
    enabled: !!kb,
  });

  const refresh = () =>
    queryClient.invalidateQueries({ queryKey: ["mappings", kb?.id] });

  const batch = useMutation({
    mutationFn: ({ ids, status }: { ids: string[]; status: MappingDecision }) =>
      decideMappings(ids, (id) => api.decideMapping(kb!.id, id, status)),
    onSuccess: () => setPicked(new Set()),
    onError: (e: unknown) => toast.error((e as Error).message),
    onSettled: refresh,
  });

  if (!kb) return <Loading>{S.nav.loading}</Loading>;

  const counts = data.data?.counts;
  const selectable =
    data.data?.items.filter(
      (m) => m.status === "proposed" && !deciding.has(m.id),
    ) ?? [];
  const selectedIds = selectedMappingIds(
    data.data?.items ?? [],
    picked,
    deciding,
  );
  const individualPending = deciding.size > 0;
  const FILTERS: { key: StatusFilter; label: string; n?: number }[] = [
    { key: "all", label: S.mapping.filterAll },
    { key: "proposed", label: S.mapping.filterProposed, n: counts?.proposed },
    {
      key: "confirmed",
      label: S.mapping.filterConfirmed,
      n: counts?.confirmed,
    },
    { key: "rejected", label: S.mapping.filterRejected, n: counts?.rejected },
  ];

  return (
    /* **The left column switches features, not tabs:** definitions and data sources are two different things (one is judgement,
       the other is registering connections). Everywhere on the site, such switches live in the left column (the library, review and ontology all do).
       Before, this was a row of buttons acting as tabs, with the selected one a solid primary button; in this vocabulary,
       solid means "the most important press on this screen", and four tabs would be four calls to action.
       The content area also changed from the settings page's centred, width-limited layout (max-w-4xl) to the full-bleed content-page layout (px-8 py-6). */
    <div className="flex h-full">
      <aside className={`${RAIL_CLS} u-rail-list px-2 py-3`}>
        {(["definitions", "sources"] as const).map((t) => (
          <Row
            key={t}
            density="nav"
            active={tab === t}
            icon={t === "definitions" ? <Database size={14} /> : <Plug size={14} />}
            onClick={() => setTab(t)}
          >
            {t === "definitions" ? S.mapping.tabDefinitions : S.mapping.tabSources}
          </Row>
        ))}
      </aside>

      <div className="u-scroll flex-1 min-w-0 overflow-y-auto px-8 py-6">
        <PageHeader
          title={S.mapping.title}
          sub={S.mapping.hint}
          className="mb-4"
        />

      {tab === "sources" ? (
        <DataSources kbId={kb.id} onExplored={refresh} />
      ) : (
        <div className="space-y-4">
          {/* The filter is a dropdown, not a row of buttons: the four status tiers are "pick one to look at", the same control (Dropdown)
              as the filters on the other pages of the site, with the count following the label */}
          <div className="flex items-center gap-2 flex-wrap">
            {/* The filter bar has the same build as Members, the library and the graph: **the w-64 input with a magnifier comes first**,
                with the dropdown following. Before, the dropdown came first and the input was flex-1: a person moving from one page to
                another saw the same toolbar row with the controls in a different order, and the search box spanning the whole screen. */}
            <Input
              icon={<Search size={13} />}
              className="w-64"
              placeholder={S.mapping.searchPlaceholder}
              value={q}
              onChange={(e) => {
                setQ(e.target.value);
                setPage(0);
              }}
            />
            <Dropdown
              className="w-40"
              value={status}
              onChange={(v) => {
                setStatus(v as StatusFilter);
                setPage(0);
              }}
              /* The count goes inside the brackets, not hung on after with an interpunct. "Pending · 0" reads as
                 two items side by side (the `·` used elsewhere on this page is exactly that usage: definition · source ·
                 table), whereas the 0 here is not a second field; it is "how many are Pending":
                 the brackets say exactly that subordinate relationship. */
              options={FILTERS.map((f) => ({
                value: f.key,
                label: f.n != null ? `${f.label} (${f.n})` : f.label,
              }))}
            />
            {/* **One toolbar row**: filter, search, select all and batch actions sit together.
                Before, each took its own row, three rows of controls stacked on top of the content, and you had to scroll past them to reach the list */}
            {selectable.length > 0 && (
              <Checkbox
                checked={selectable.every((m) => picked.has(m.id))}
                disabled={batch.isPending || individualPending}
                onChange={(v) =>
                  setPicked(
                    v
                      ? new Set(selectable.map((m) => m.id))
                      : new Set(),
                  )
                }
                label={S.mapping.selectPage}
                className="shrink-0"
              />
            )}
            {selectedIds.length > 0 && (
              <>
                  <span className="u-num text-small text-ink-2">
                    {S.mapping.selected(selectedIds.length)}
                  </span>
                  <Button
                    variant="secondary"
                    size="sm"
                    disabled={batch.isPending || individualPending}
                    onClick={() =>
                      batch.mutate({
                        ids: selectedIds,
                        status: "confirmed",
                      })
                    }
                  >
                    {S.mapping.approve}
                  </Button>
                  <Button
                    variant="ghost"
                    size="sm"
                    className="text-danger"
                    disabled={batch.isPending || individualPending}
                    onClick={() =>
                      batch.mutate({
                        ids: selectedIds,
                        status: "rejected",
                      })
                    }
                  >
                    {S.mapping.reject}
                  </Button>
              </>
            )}
          </div>

          {status === "rejected" && (
            <p className="text-small text-ink-2">{S.mapping.rejectedHint}</p>
          )}
          {/* The ledger of the most recent exploration round (#503). **The list alone cannot answer "how much was missed"**: twelve
              proposals look the same against an eighty-column wide table as they do when they exactly cover a small base. Posting this lets people
              see the coverage among the "equal number of proposals". Filtering it out of view would be worse: while searching,
              people most easily forget how deep the last run went */}
          {data.data?.last_run ? (
            <p className="text-small text-ink-2">
              {S.mapping.lastRun({
                tables: data.data.last_run.tables_scanned,
                columns: data.data.last_run.columns_scanned,
                returned: data.data.last_run.returned,
                accepted: data.data.last_run.accepted,
                truncated: data.data.last_run.schema_truncated,
              })}
            </p>
          ) : (
            <p className="text-small text-ink-2">{S.mapping.lastRunMissing}</p>
          )}

          {data.isPending ? (
            <Loading>{S.nav.loading}</Loading>
          ) : data.data && data.data.items.length === 0 ? (
            <div className="py-12">
              <EmptyState icon={<Database size={20} />}>
                {q || status !== "all"
                  ? S.mapping.emptyFiltered
                  : S.mapping.empty}
              </EmptyState>
            </div>
          ) : (
            /* A panel holds several rows, not one card per row (DESIGN.md 6): this page is
               a team of definitions awaiting a stance, a uniform group, with the same look as the library, search and members */
            <div className="glass rounded-panel divide-y divide-line">
              {data.data?.items.map((m) => (
                <MappingRow
                  key={m.id}
                  kbId={kb.id}
                  mapping={m}
                  picked={selectedIds.includes(m.id)}
                  batchPending={batch.isPending}
                  onDecisionPending={(pending) =>
                    setDeciding((prev) => {
                      const next = new Set(prev);
                      if (pending) next.add(m.id);
                      else next.delete(m.id);
                      return next;
                    })
                  }
                  onPick={(on) =>
                    setPicked((prev) => {
                      const next = new Set(prev);
                      if (on) next.add(m.id);
                      else next.delete(m.id);
                      return next;
                    })
                  }
                  onChanged={refresh}
                />
              ))}
            </div>
          )}
          <Pager
            total={data.data?.total ?? 0}
            pageSize={PAGE}
            page={page}
            onPage={setPage}
          />
        </div>
      )}
      </div>
    </div>
  );
}

/** A definition. **Only the ones with no stance yet get the confirm / reject buttons**; ones already ruled on get "Edit",
 *  because revising a definition and making the first ruling are two different things: the former leaves a trail (revisions), the latter does not. */
function MappingRow({
  kbId,
  mapping: m,
  picked,
  batchPending,
  onDecisionPending,
  onPick,
  onChanged,
}: {
  kbId: string;
  mapping: ConceptMapping;
  picked: boolean;
  batchPending: boolean;
  onDecisionPending: (pending: boolean) => void;
  onPick: (picked: boolean) => void;
  onChanged: () => void;
}) {
  const [editing, setEditing] = useState(false);
  const [showHistory, setShowHistory] = useState(false);

  const decide = useMutation({
    mutationFn: (s: "confirmed" | "rejected") =>
      api.decideMapping(kbId, m.id, s),
    onMutate: () => onDecisionPending(true),
    onSuccess: onChanged,
    onError: (e: unknown) => toast.error((e as Error).message),
    onSettled: () => onDecisionPending(false),
  });

  const how = howComputed(m);
  return (
    <div className={cn("px-4 py-3", picked && "u-picked")}>
      <div className="flex items-baseline gap-2 flex-wrap">
        {m.status === "proposed" && (
          <Checkbox
            className="shrink-0 self-center"
            checked={picked}
            disabled={decide.isPending || batchPending}
            onChange={(v) => onPick(v)}
            label={
              <span className="sr-only">
                {S.mapping.selectMapping(m.concept_name, m.source)}
              </span>
            }
          />
        )}
        <span className="text-body text-ink">{m.concept_name}</span>
        <span className="text-fine text-ink-2">{m.source}</span>
        {m.unit && (
          <span className="text-fine text-ink-2">[{m.unit}]</span>
        )}
        {m.derived && (
          <Chip tone="warn" className="text-fine">
            {S.mapping.derivedBadge}
          </Chip>
        )}
        <span className="flex-1" />
        <Status tone={TONE[m.status]} className="text-fine">
          {statusLabel(m.status)}
        </Status>
      </div>

      <div
        className={cn(
          "mt-1 u-num text-small break-all",
          how ? "text-ink-2" : "text-ink-2",
        )}
      >
        {how ?? S.mapping.noDefinition}
      </div>
      {m.summary && (
        <p className="mt-1 text-small text-ink-2">{m.summary}</p>
      )}

      {editing ? (
        <EditForm
          kbId={kbId}
          mapping={m}
          onDone={() => {
            setEditing(false);
            onChanged();
          }}
          onCancel={() => setEditing(false)}
        />
      ) : (
        /* Actions in the row: **the ruling press is outlined; the rest are ghost, none is solid.** Eleven rows
           of eleven solid buttons would paint a whole page as actions, and a screen should have at most one primary.
           But "not solid" does not mean "looks like a sentence". Before, these three were `LinkButton`s:
           no padding, no hover background, and the colour was the body text's grey, so they looked like the explanation line above them
           and did not look clickable. Ghost restores the two things a control needs: a ring of padding,
           and a background on pointer hover. At rest it is still light.
           Reject **should be red even at rest**: it is the press that takes effect on click, and a person should be able to tell before pressing
           that it is not the same kind as "Edit", rather than discovering it on hover. */
        <div className="mt-2 -ml-2 flex items-center gap-1 flex-wrap">
          {m.status === "proposed" && (
            <>
              <Button variant="secondary" size="sm" className="ml-2"
                disabled={decide.isPending || batchPending || picked}
                onClick={() => decide.mutate("confirmed")}
              >
                {S.mapping.approve}
              </Button>
              <Button variant="ghost" size="sm" className="text-danger"
                disabled={batchPending || picked}
                onClick={() =>
                  !picked && !decide.isPending && decide.mutate("rejected")
                }
              >
                {S.mapping.reject}
              </Button>
            </>
          )}
          <Button variant="ghost" size="sm" onClick={() => setEditing(true)}>
            {S.mapping.edit}
          </Button>
          <Button variant="ghost" size="sm" onClick={() => setShowHistory((v) => !v)}>
            {S.mapping.history}
          </Button>
        </div>
      )}

      {showHistory && <RevisionList kbId={kbId} mappingId={m.id} />}
    </div>
  );
}

function EditForm({
  kbId,
  mapping: m,
  onDone,
  onCancel,
}: {
  kbId: string;
  mapping: ConceptMapping;
  onDone: () => void;
  onCancel: () => void;
}) {
  const [table, setTable] = useState(m.table_name ?? "");
  const [expr, setExpr] = useState(m.expr ?? "");
  const [sql, setSql] = useState(m.sql ?? "");
  const [unit, setUnit] = useState(m.unit ?? "");
  const [summary, setSummary] = useState(m.summary ?? "");
  const [derived, setDerived] = useState(m.derived);

  const save = useMutation({
    mutationFn: () =>
      api.reviseMapping(kbId, m.id, {
        table_name: table,
        expr,
        sql,
        unit,
        summary,
        derived,
      }),
    onSuccess: onDone,
    onError: (e: unknown) => toast.error((e as Error).message),
  });

  const nothing = !table.trim() && !expr.trim() && !sql.trim();
  const field = (
    label: string,
    value: string,
    set: (v: string) => void,
    mono = false,
  ) => (
    <label className="block">
      <span className="text-fine text-ink-2">{label}</span>
      <Input
        size="sm"
        className={cn("mt-1 w-full", mono && "u-num")}
        value={value}
        onChange={(e) => set(e.target.value)}
      />
    </label>
  );

  return (
    <div className="mt-3 space-y-2 border-t border-line pt-3">
      <p className="text-fine text-ink-2">{S.mapping.editTitle}</p>
      <div className="grid grid-cols-2 gap-2">
        {field(S.mapping.fieldTable, table, setTable, true)}
        {field(S.mapping.fieldUnit, unit, setUnit)}
      </div>
      {field(S.mapping.fieldExpr, expr, setExpr, true)}
      {field(S.mapping.fieldSql, sql, setSql, true)}
      {field(S.mapping.fieldSummary, summary, setSummary)}
      <Checkbox
        checked={derived}
        onChange={(v) => setDerived(v)}
        label={S.mapping.fieldDerived}
      />
      <span className="hidden">
      </span>
      {nothing && (
        <p className="text-small text-danger">{S.mapping.needOne}</p>
      )}
      <div className="flex gap-2">
        <Button variant="secondary" size="sm"
          onClick={onCancel}
        >
          {S.mapping.cancel}
        </Button>
        <Button variant="primary" size="sm"
          disabled={nothing || save.isPending}
          onClick={() => save.mutate()}
        >
          {S.mapping.save}
        </Button>
      </div>
    </div>
  );
}

/** Revision history. **The audit table has never been read since it was created**: 0006 says it is kept so that one can answer
 *  "how was this number computed last quarter", and this is where that sentence is fulfilled. */
function RevisionList({
  kbId,
  mappingId,
}: {
  kbId: string;
  mappingId: string;
}) {
  const revs = useQuery({
    queryKey: ["mappingRevisions", kbId, mappingId],
    queryFn: () => api.mappingRevisions(kbId, mappingId),
  });

  if (revs.isPending) return <Loading>{S.nav.loading}</Loading>;
  // **A failed fetch must say it failed.** `?? []` would draw a 500 as "not revised yet":
  // a record that was revised being described as never revised is much harder to debug than an error
  if (revs.isError)
    return (
      <div className="mt-3 border-t border-line pt-3">
        <ErrorText>{(revs.error as Error).message}</ErrorText>
      </div>
    );
  const list = revs.data?.revisions ?? [];
  return (
    <div className="mt-3 border-t border-line pt-3 space-y-2">
      <p className="text-fine text-ink-2">{S.mapping.historyHint}</p>
      {list.length === 0 ? (
        <p className="text-small text-ink-2">{S.mapping.historyEmpty}</p>
      ) : (
        list.map((r) => {
          const b = r.before;
          const was = (b.sql ?? b.expr ?? b.table_name) as string | null;
          return (
            <div key={r.id} className="text-small">
              <div className="text-ink-2">
                {S.mapping.historyBy(
                  r.changed_by_name ?? S.mapping.historyUnknown,
                )}
                <span className="u-num ml-2 text-ink-2">
                  {r.changed_at.slice(0, 16).replace("T", " ")}
                </span>
              </div>
              <div className="u-num text-ink-2 break-all">
                {was ?? S.mapping.noDefinition}
              </div>
            </div>
          );
        })
      )}
    </div>
  );
}

/** Mounting of data sources at the knowledge-base level. **Moved here from KB settings**: which base to mount and how the definition is decided
 *  are two halves of the same thing: without knowing which tables exist, you cannot judge whether a definition is right.
 *  Registering a new connection is still a deployment-level action: admins get a direct entry point, others are pointed to an admin. */
function DataSources({
  kbId,
  onExplored,
}: {
  kbId: string;
  onExplored: () => void;
}) {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const me = useQuery({ queryKey: ["me"], queryFn: fetchMe });
  const mounted = useQuery({
    queryKey: ["kbDataSources", kbId],
    queryFn: () => api.kbDataSources(kbId),
  });
  const available = useQuery({
    queryKey: ["kbDataSourcesAvail", kbId],
    queryFn: () => api.kbDataSourcesAvailable(kbId),
  });
  const [picked, setPicked] = useState("");
  const [notice, setNotice] = useState<string | null>(null);
  // Kept separate from the notice: one says "it succeeded", the other "it succeeded halfway", and the colours differ too
  const [warning, setWarning] = useState<string | null>(null);
  const invalidate = () =>
    queryClient.invalidateQueries({ queryKey: ["kbDataSources", kbId] });

  const mount = useMutation({
    mutationFn: (dsId: string) => api.mountDataSource(kbId, dsId),
    // **Mounted, but the schema did not come through, are two different things.** The server replies ok at this point (the source did mount),
    // so we cannot read `schema_tables: 0` as "ingested 0 tables": that would say it succeeded.
    // Say clearly which half failed; the same event also goes to the alerts centre
    onSuccess: (r) => {
      setPicked("");
      setNotice(null);
      setWarning(r.schema_error ? S.mapping.schemaFailed : null);
      if (!r.schema_error) setNotice(S.mapping.schemaSynced(r.schema_tables));
      invalidate();
    },
    onError: (e: unknown) => toast.error((e as Error).message),
  });
  const unmount = useMutation({
    mutationFn: (dsId: string) => api.unmountDataSource(kbId, dsId),
    onSettled: invalidate,
  });
  const sync = useMutation({
    mutationFn: (dsId: string) => api.syncDataSourceSchema(kbId, dsId),
    onSuccess: (r) => setNotice(S.mapping.schemaSynced(r.schema_tables)),
    onError: (e: unknown) => toast.error((e as Error).message),
  });
  const explore = useMutation({
    mutationFn: () => api.exploreMappings(kbId),
    onSuccess: () => {
      setNotice(S.mapping.exploreQueued);
      onExplored();
    },
    onError: (e: unknown) => toast.error((e as Error).message),
  });

  const mountedIds = new Set(
    (mounted.data?.data_sources ?? []).map((d) => d.id),
  );
  const mountable = (available.data?.data_sources ?? []).filter(
    (d) => !mountedIds.has(d.id),
  );
  const hasMounted = (mounted.data?.data_sources.length ?? 0) > 0;

  return (
    <div className="space-y-4">
      <p className="text-small text-ink-2">{S.mapping.sourcesHint}</p>

      <div className="glass rounded-panel divide-y divide-line">
        {(mounted.data?.data_sources ?? []).map((d) => (
          <div key={d.id} className="px-4 py-3 flex items-center gap-3">
            <div className="min-w-0 flex-1">
              <div className="text-body text-ink">{d.name}</div>
              <div className="text-small text-ink-2 u-num truncate">
                {d.summary}
              </div>
            </div>
            <Button variant="secondary" size="sm" className="shrink-0"
              disabled={sync.isPending}
              onClick={() => sync.mutate(d.id)}
            >
              {S.mapping.syncSchema}
            </Button>
            <Button
              variant="ghost"
              size="sm"
              className="shrink-0 text-danger"
              disabled={unmount.isPending}
              onClick={() => unmount.mutate(d.id)}
            >
              {S.mapping.unmount}
            </Button>
          </div>
        ))}
        {!hasMounted && (
          <p className="px-4 py-6 text-body text-ink-2">
            {S.mapping.sourcesEmpty}
          </p>
        )}
      </div>

      {mountable.length > 0 && (
        <div className="flex items-center gap-2">
          <SearchSelect
            className="flex-1"
            value={picked}
            options={mountable.map((d) => ({
              value: d.id,
              label: d.name,
              hint: d.summary,
            }))}
            onChange={setPicked}
            placeholder={S.mapping.mount + "…"}
          />
          <Button variant="primary" size="sm" className="shrink-0"
            disabled={!picked || mount.isPending}
            onClick={() => mount.mutate(picked)}
          >
            {S.mapping.mount}
          </Button>
        </div>
      )}

      {me.data?.is_admin ? (
        <Button
          variant="ghost"
          size="sm"
          className="-ml-2"
          onClick={() =>
            navigate({ to: "/admin", search: { tab: "datasources" } })
          }
        >
          <Plus size={12} />
          {S.mapping.newConn}
        </Button>
      ) : (
        mountable.length === 0 &&
        available.data &&
        !hasMounted && (
          <p className="text-small text-ink-2">
            {S.mapping.sourcesNoneAvailable}
          </p>
        )
      )}

      {hasMounted && (
        <div className="glass rounded-panel px-4 py-3 flex items-center gap-3">
          <p className="text-small text-ink-2 flex-1">
            {S.mapping.exploreHint}
          </p>
          <Button variant="secondary" size="sm" className="shrink-0"
            disabled={explore.isPending}
            onClick={() => explore.mutate()}
          >
            {S.mapping.explore}
          </Button>
        </div>
      )}
      {notice && <p className="text-small text-ok">{notice}</p>}
      {warning && <p className="text-small text-warn">{warning}</p>}
    </div>
  );
}
