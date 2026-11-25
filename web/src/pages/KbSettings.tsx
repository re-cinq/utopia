/* Library settings: left-column sections (General / Members / Danger zone), building the skeleton for future settings items
   (extraction settings, retention policy, library-level tokens...). Access control is enforced by the API (library admin to start).
   Rendering the sections mutually exclusive also fixes the stacking bug where dropdown popovers were covered by later glass cards (backdrop-filter forms its own stacking
   context). */
import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useParams, useNavigate } from "@tanstack/react-router";
import {
  History as HistoryIcon,
  Download,
  Lock,
  Plus,
  Search,
  Settings2,
  TriangleAlert,
  Users,
} from "lucide-react";
import { api, type AuditEvent } from "../api";
import { LANG_NAMES, S } from "../i18n";
import { toast } from "../toast";
import {
  Button,
  Checkbox,
  DangerConfirm,
  Dropdown,
  Input,
  LinkButton,
  Loading,
  localDateTime,
  Pager,
  RAIL_CLS,
  type RowTone,
  Row,
  SearchSelect,
  Segmented,
  SettingsCard,
  PageHeader,
  Dialog,
  Field,
  Table,
  TBody,
  Td,
  Th,
  THead,
  Tr,
} from "../ui";

const KB_ROLES = [
  { value: "viewer", label: S.kbset.roles.viewer },
  { value: "editor", label: S.kbset.roles.editor },
  { value: "admin", label: S.kbset.roles.admin },
];

/**
 * The roles this library can grant.
 *
 * **open  * **An open library has no viewer to grant**: `access::kb_role` gives every person in the deployment Viewer directly on an open library,
 * Viewer * so writing a row with `role=viewer` adds nothing: a no-op record that
 * still takes up a row in the member list and makes people think it did something. Listing it here only makes sense for granting write access.
 *
 * Historical data may hold viewer rows for open libraries, but those rows are already hidden in the list
 *(see `listed`), so there is no need to fall back for 'current value not among the options' here.
 */
function rolesFor(isOpen: boolean) {
  return isOpen ? KB_ROLES.filter((r) => r.value !== "viewer") : KB_ROLES;
}

type Section = "general" | "members" | "activity" | "danger";

/** Saving one settings card: **send only its own fields**. PATCH is already a partial update; sending the whole form
    along would let a rename overwrite someone else's just-changed toggle. Each of the four cards has its own
    mutation, so 'saving' and 'saved' are also per card. */
function useKbPatch(kbId: string, onError: (m: string | null) => void) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (patch: Record<string, unknown>) => api.updateKb(kbId, patch),
    onSuccess: () => {
      onError(null);
      queryClient.invalidateQueries({ queryKey: ["kbOne", kbId] });
      queryClient.invalidateQueries({ queryKey: ["kbs"] });
    },
    onError: (e) => onError((e as Error).message),
  });
}

export function KbSettings() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  /* The library id comes from the path. **It used to be `?kb=`**: that was the only place the library traveled before this routing refactor;
     now the whole area sits under /kb/$kbId, so it no longer needs its own slot */
  const { kbId } = useParams({ from: "/app/kb/$kbId/settings" });

  // Failed task count and re-queue (#216). The query key carries the library id; after re-queuing, invalidate and refetch
  const failedJobs = useQuery({
    queryKey: ["jobs", "failed", kbId],
    queryFn: () => api.failedJobs(kbId!),
    enabled: !!kbId,
  });
  const requeue = useMutation({
    mutationFn: () => api.requeueJobs(kbId!),
    onSuccess: (r) => {
      toast.success(S.kbset.requeued(r.requeued));
      queryClient.invalidateQueries({ queryKey: ["jobs", "failed", kbId] });
    },
    onError: (e) => toast.error(String(e)),
  });
  const kb = useQuery({
    queryKey: ["kbOne", kbId],
    queryFn: () => api.kbDetail(kbId!),
    enabled: !!kbId,
  });

  const [section, setSection] = useState<Section>("general");
  const [name, setName] = useState("");
  const [desc, setDesc] = useState("");
  const [visibility, setVisibility] = useState<"open" | "restricted">("open");
  // **Off by default**, the opposite of the one above: inference writes facts into the ledger, and the declaration may be wrong
  const [materialize, setMaterialize] = useState(false);
  // Automatic type resolution (0016 C2): only the tier refined within the subtree is applied automatically
  const [autoResolve, setAutoResolve] = useState(false);
  // Governance (0025): **off by default**. It merges entities and has not been measured on any library yet; turning it on starts it, turning it off stops it
  const [governance, setGovernance] = useState(false);
  const [inferMins, setInferMins] = useState(60);
  const [ontoLang, setOntoLang] = useState<"en" | "zh">("en");
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Each card follows only its own server fields; when saving another card triggers a refetch, keep this card's draft.
  useEffect(() => {
    if (kb.data) {
      setName(kb.data.name);
      setDesc(kb.data.description ?? "");
    }
  }, [kbId, kb.data?.name, kb.data?.description]);

  useEffect(() => {
    if (kb.data) {
      setVisibility(kb.data.visibility);
    }
  }, [kbId, kb.data?.visibility]);

  useEffect(() => {
    if (kb.data) {
      setMaterialize(kb.data.materialize_inferences);
      setAutoResolve(kb.data.auto_type_resolution);
      setGovernance(kb.data.governance);
      setInferMins(kb.data.inference_interval_minutes);
    }
  }, [
    kbId,
    kb.data?.materialize_inferences,
    kb.data?.auto_type_resolution,
    kb.data?.governance,
    kb.data?.inference_interval_minutes,
  ]);

  useEffect(() => {
    if (kb.data) {
      setOntoLang(kb.data.ontology_lang);
    }
  }, [kbId, kb.data?.ontology_lang]);

  // Four cards, four saves: renaming should not send the toggles below along with it
  const saveIdentity = useKbPatch(kbId!, setError);
  const saveVisibility = useKbPatch(kbId!, setError);
  const saveAutomation = useKbPatch(kbId!, setError);
  const saveLang = useKbPatch(kbId!, setError);

  const removeKb = useMutation({
    mutationFn: () => api.deleteKb(kbId!),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["kbs"] });
      navigate({ to: "/kb/$kbId/library", params: { kbId } });
    },
    onError: (e) => setError((e as Error).message),
  });

  if (!kbId || kb.isPending) return <Loading>{S.nav.loading}</Loading>;
  if (kb.isError)
    return (
      <div className="p-8 text-body text-danger">
        {(kb.error as Error).message}
      </div>
    );

  const lbl = "block text-small font-medium text-ink-2 mb-1";

  /* Each card's own 'changed or not': whether the button lights up says whether something in this card moved,
     so compare local state item by item against the latest server copy, rather than keeping one global dirty flag */
  const identityDirty =
    name.trim() !== kb.data.name ||
    desc.trim() !== (kb.data.description ?? "");
  const visibilityDirty = visibility !== kb.data.visibility;
  const automationDirty =
    materialize !== kb.data.materialize_inferences ||
    autoResolve !== kb.data.auto_type_resolution ||
    governance !== kb.data.governance ||
    inferMins !== kb.data.inference_interval_minutes;
  const langDirty = ontoLang !== kb.data.ontology_lang;

  const isDefault = kb.data.is_default;
  const sections: {
    key: Section;
    label: string;
    Icon: typeof Settings2;
    tone?: RowTone;
  }[] = [
    { key: "general", label: S.kbset.general, Icon: Settings2 },
    { key: "members", label: S.kbset.members, Icon: Users },
    { key: "activity", label: S.kbset.activity, Icon: HistoryIcon },
    // The default library cannot be deleted, so the danger section does not appear at all. The entry only tints the icon with the warning color; the text matches
    // the other sections: this one just leads to the danger zone, and only the button that actually deletes the library uses the danger color
    ...(isDefault
      ? []
      : [
          {
            key: "danger" as Section,
            label: S.kbset.danger,
            Icon: TriangleAlert,
            tone: "warn" as const,
          },
        ]),
  ];

  return (
    <div className="h-full flex">
      {/* Section navigation: future extraction settings / retention policy / tokens etc. extend here */}
      <aside className={`${RAIL_CLS} u-rail-list px-2 py-3`}>
        {sections.map(({ key, label, Icon, tone }) => (
          <Row
            key={key}
            density="nav"
            active={section === key}
            icon={<Icon size={14} className={tone === "warn" ? "text-warn" : undefined} />}
            onClick={() => setSection(key)}
          >
            {label}
          </Row>
        ))}
      </aside>

      <main className="flex-1 min-w-0 overflow-y-auto u-scroll px-8 py-6">
        {/* Settings are a column of fields, not a single table: content centered with a width limit, line length does not stretch with the window */}
        <div className="mx-auto w-full max-w-4xl">
          {/* No library name suffix: the top bar switcher already shows the current library */}
          <PageHeader title={S.kbset.title} />

          {section === "general" && (
            /* Vercel's settings page layout: one card is one save unit. Previously this section was one
               long table with a single save at the end: changing a name meant sending four toggles along with it, and
               "'saved' did not say which thing was saved */
            <div className="space-y-4">
              <SettingsCard
                title={S.kbset.cardIdentity}
                note={S.kbset.cardIdentityNote}
                action={
                  <>
                    {saveIdentity.isSuccess && !identityDirty && (
                      <span className="text-small text-ink-2">{S.kbset.saved}</span>
                    )}
                    <Button variant="secondary" size="sm"
                      disabled={!name.trim() || !identityDirty || saveIdentity.isPending}
                      onClick={() =>
                        saveIdentity.mutate({
                          name: name.trim(),
                          // An empty string is what 'clear the description' means: null is COALESCE on the server, meaning 'leave this unchanged',
                          // "// so deleting the description entirely would quietly do nothing
                          description: desc.trim(),
                        })
                      }
                    >
                      {S.kbset.save}
                    </Button>
                  </>
                }
              >
                <div className="space-y-3">
                  <div>
                    <label className={lbl}>{S.settings.kbs.name}</label>
                    <Input className="w-full"
                      value={name}
                      onChange={(e) => setName(e.target.value)}
                    />
                  </div>
                  <div>
                    <label className={lbl}>{S.settings.kbs.description}</label>
                    <Input className="w-full"
                      value={desc}
                      onChange={(e) => setDesc(e.target.value)}
                    />
                  </div>
                </div>
              </SettingsCard>

              {/* The default library's visibility is locked: the card stays (this library does have a visibility setting),
                  but the footer has no save: no button when there is nothing to change */}
              <SettingsCard
                title={S.settings.kbs.visibility}
                hint={isDefault ? S.kbset.defaultOpenNote : S.kbset.cardVisibilityNote}
                note={isDefault ? undefined : S.kbset.cardVisibilityFoot}
                action={
                  isDefault ? undefined : (
                    <>
                      {saveVisibility.isSuccess && !visibilityDirty && (
                        <span className="text-small text-ink-2">{S.kbset.saved}</span>
                      )}
                      <Button variant="secondary" size="sm"
                        disabled={!visibilityDirty || saveVisibility.isPending}
                        onClick={() => saveVisibility.mutate({ visibility })}
                      >
                        {S.kbset.save}
                      </Button>
                    </>
                  )
                }
              >
                {isDefault ? (
                  <div className="flex w-fit items-center gap-2 rounded-control border border-line px-3 py-2 text-fine text-ink-2">
                    <Lock size={11} className="shrink-0 text-ink-2" />
                    {S.kbset.defaultOpenLabel}
                  </div>
                ) : (
                  <Segmented
                    size="sm"
                    className="w-fit"
                    value={visibility}
                    onChange={setVisibility}
                    options={(
                      [
                        ["open", "Open"],
                        ["restricted", S.settings.kbs.visRestricted],
                      ] as const
                    ).map(([v, label]) => ({ value: v, label, title: label }))}
                  />
                )}
              </SettingsCard>

              <SettingsCard
                title={S.kbset.cardAutomation}
                note={S.kbset.cardAutomationNote}
                action={
                  <>
                    {saveAutomation.isSuccess && !automationDirty && (
                      <span className="text-small text-ink-2">{S.kbset.saved}</span>
                    )}
                    <Button variant="secondary" size="sm"
                      disabled={!automationDirty || saveAutomation.isPending}
                      onClick={() =>
                        saveAutomation.mutate({
                          materialize_inferences: materialize,
                          auto_type_resolution: autoResolve,
                          governance,
                          inference_interval_minutes: inferMins,
                        })
                      }
                    >
                      {S.kbset.save}
                    </Button>
                  </>
                }
              >
                <div className="space-y-3">
                  {/* Materialized inference: **off by default**. It acts on the ledger: it writes facts into the graph from axioms, and the declarations may be wrong */}
                  <Checkbox
                    checked={materialize}
                    onChange={(v) => setMaterialize(v)}
                    label={S.kbset.materialize}
                    hint={S.kbset.materializeNote}
                  />
                  {/* Re-inference interval. **Shown only when turned on**: when off it affects nothing,
                      so showing it would only make people think setting it would trigger inference */}
                  {materialize && (
                    <div className="pl-6 flex flex-wrap items-center gap-2">
                      <label className="text-small text-ink-2">
                        {S.kbset.inferEvery}
                      </label>
                      <Input size="sm" className="w-24 u-num"
                        type="number"
                        min={5}
                        max={10080}
                        value={inferMins}
                        onChange={(e) => setInferMins(Number(e.target.value))}
                      />
                      <span className="text-small text-ink-2">{S.kbset.minutes}</span>
                      {kb.data.last_inference_at && (
                        <span className="text-fine text-ink-2">
                          {S.kbset.lastInference(
                            localDateTime(kb.data.last_inference_at),
                          )}
                        </span>
                      )}
                    </div>
                  )}
                  {/* Automatic type resolution: after extraction, queue a pass; it only auto-applies the tier refined within a subtree, cross-axis ones still go to a person */}
                  <Checkbox
                    checked={autoResolve}
                    onChange={(v) => setAutoResolve(v)}
                    label={S.kbset.autoResolveTypes}
                    hint={S.kbset.autoResolveTypesNote}
                  />
                  {/* Governance (0025): the agent works through queued people-waiting duplicate pairs first-in-first-out, reading people's precedents in the ledger before deciding.
                      The description must make three things clear: it reads this library's people's decisions, a merge needs precedent behind it,
                      and turning off the queue stops it */}
                  <Checkbox
                    checked={governance}
                    onChange={(v) => setGovernance(v)}
                    label={S.kbset.governance}
                    hint={S.kbset.governanceNote}
                  />
                </div>
              </SettingsCard>

              {/* Corpus language. **Not the interface language**: class descriptions go verbatim into the extraction prompt,
                  and the reader is the model reading these documents, so it follows the documents, not the reader */}
              <SettingsCard
                title={S.kbset.ontologyLang}
                hint={S.kbset.ontologyLangNote}
                action={
                  <>
                    {saveLang.isSuccess && !langDirty && (
                      <span className="text-small text-ink-2">{S.kbset.saved}</span>
                    )}
                    <Button variant="secondary" size="sm"
                      disabled={!langDirty || saveLang.isPending}
                      onClick={() => saveLang.mutate({ ontology_lang: ontoLang })}
                    >
                      {S.kbset.save}
                    </Button>
                  </>
                }
              >
                <Segmented
                  size="sm"
                  className="w-fit"
                  value={ontoLang}
                  onChange={setOntoLang}
                  options={(["en", "zh"] as const).map((l) => ({
                    value: l,
                    label: LANG_NAMES[l],
                  }))}
                />
              </SettingsCard>

              {/* Failed tasks (#216): shown only when there are some. This card has no 'save': it is not a setting,
                  it is an action: put all failed items in this library back into the queue */}
              {failedJobs.data && failedJobs.data.failed > 0 && (
                <SettingsCard
                  title={S.kbset.cardJobs}
                  note={S.kbset.failedJobs(failedJobs.data.failed)}
                  action={
                    <Button variant="secondary" size="sm"
                      disabled={requeue.isPending}
                      onClick={() => requeue.mutate()}
                    >
                      {S.kbset.requeue}
                    </Button>
                  }
                />
              )}

              <SettingsCard
                title={S.kbset.exportKb}
                hint={S.kbset.exportKbNote}
                action={
                  <Button variant="secondary" size="sm"
                    onClick={() => window.open(`/api/v1/kbs/${kbId}/export?format=turtle`)}
                  >
                    <Download size={14} />
                    {S.kbset.exportKbAction}
                  </Button>
                }
              />
            </div>
          )}

          {section === "members" && (
            <KbMembers kbId={kbId} isOpen={kb.data.visibility === "open"} />
          )}

          {section === "activity" && <KbActivity kbId={kbId} />}

          {section === "danger" && (
            <div className="glass rounded-panel px-6 py-4 flex items-center justify-between gap-4">
              <div className="min-w-0">
                <div className="text-body font-medium text-ink">
                  {S.kbset.deleteRowTitle}
                </div>
                <div className="mt-1 text-small text-ink-2">
                  {S.kbset.deleteRowHint}
                </div>
              </div>
              <Button variant="secondary" size="sm" className="shrink-0"
                style={{
                  background: "var(--u-danger-solid)",
                  color: "var(--u-on-danger)",
                }}
                onClick={() => setConfirmingDelete(true)}
              >
                {S.kbset.deleteRowBtn}
              </Button>
            </div>
          )}

          {error && <p className="text-body text-danger">{error}</p>}

          {confirmingDelete && (
            <DangerConfirm
              title={S.kbset.deleteKb}
              hint={S.kbset.deleteHint(kb.data.name)}
              requireText={kb.data.name}
              confirmLabel={S.kbset.deleteBtn}
              cancelLabel={S.library.cancel}
              busy={removeKb.isPending}
              onConfirm={() => removeKb.mutate()}
              onCancel={() => setConfirmingDelete(false)}
            />
          )}
        </div>
      </main>
    </div>
  );
}

/** Pick a human-readable name from detail (varies by action semantics; falls back key by key) */
function auditDetailName(e: AuditEvent): string {
  const d = e.detail;
  const cand = [d.label, d.name, d.filename, d.key, d.role];
  const hit = cand.find((v) => typeof v === "string" && v);
  return typeof hit === "string" ? hit : "";
}

const AUDIT_PAGE = 50;

/** The two date boxes mean days on the reader's calendar: the start day from midnight, the end day through to its end.
 *  The server's range includes the start and excludes the end, comparing by instant, so the end date must send midnight of the next day, with midnight also converted to an instant in the local timezone */
export function auditBounds(since: string, until: string) {
  const midnight = (day: string, later: number) => {
    const [y, m, d] = day.split("-").map(Number);
    return new Date(y, m - 1, d + later).toISOString();
  };
  return {
    since: since ? midnight(since, 0) : undefined,
    until: until ? midnight(until, 1) : undefined,
  };
}

export function KbActivity({ kbId }: { kbId: string }) {
  // Filters follow real query patterns: query one action class, one person, one time range.
  // Action prefix matching: `entity.` pulls retyped / renamed together as one family
  const [action, setAction] = useState("");
  const [since, setSince] = useState("");
  const [until, setUntil] = useState("");
  const [page, setPage] = useState(0);
  const audit = useQuery({
    queryKey: ["kbAudit", kbId, action, since, until, page],
    queryFn: () =>
      api.kbAudit(kbId, {
        action: action || undefined,
        ...auditBounds(since, until),
        limit: AUDIT_PAGE,
        offset: page * AUDIT_PAGE,
      }),
    placeholderData: (prev) => prev,
  });
  const result = audit.isSuccess && !audit.isPlaceholderData ? audit.data : undefined;
  // The dropdown is filled with the actions this library has actually had, not a hard-coded list
  const actions = audit.data?.actions ?? (action ? [action] : []);
  const filtered = !!(action || since || until);

  const reset = (fn: () => void) => {
    fn();
    setPage(0);
  };

  return (
    <div className="space-y-3">
      {/* The controls that filter this ledger sit outside the card (DESIGN.md 6): they are not contents of the ledger,
          and when a filter empties everything the card must be able to turn into an empty state, not take the only way to change the filter along with it */}
      <div className="flex flex-wrap items-center gap-2">
        <Dropdown
          size="sm"
          className="w-48"
          value={action}
          onChange={(v) => reset(() => setAction(v))}
          options={[
            { value: "", label: S.kbset.auditAllActions },
            ...actions.map((a) => ({ value: a, label: a })),
          ]}
        />
        {/* Date boxes **must state their own width**: `Input` without a width is w-full; two in a row would each take the full width,
            so each wraps on its own; the dropdown in the same row needs w-48 to avoid that. */}
        <Input size="sm" className="u-num w-36"
          type="date"
          value={since}
          title={S.kbset.auditSince}
          onChange={(e) => reset(() => setSince(e.target.value))}
        />
        <span className="text-small text-ink-2">→</span>
        <Input size="sm" className="u-num w-36"
          type="date"
          value={until}
          title={S.kbset.auditUntil}
          onChange={(e) => reset(() => setUntil(e.target.value))}
        />
        {filtered && (
          <Button variant="secondary" size="sm"
            onClick={() =>
              reset(() => {
                setAction("");
                setSince("");
                setUntil("");
              })
            }
          >
            {S.kbset.auditClear}
          </Button>
        )}
        {result && (
          <span className="ml-auto u-num text-fine text-ink-2">
            {S.kbset.auditTotal(result.total)}
          </span>
        )}
      </div>
      <SettingsCard title={S.kbset.activity} hint={S.kbset.activityHint}>
        {audit.isError ? (
          <div className="space-y-2">
            <div role="alert" className="text-small text-danger">
              <p>{S.kbset.activityLoadError}</p>
              {audit.error.message && <p>{audit.error.message}</p>}
            </div>
            <Button
              variant="secondary"
              size="sm"
              disabled={audit.isFetching}
              onClick={() => { void audit.refetch(); }}
            >
              {S.kbset.activityRetry}
            </Button>
          </div>
        ) : !result ? (
          <p role="status" className="text-small text-ink-2">{S.nav.loading}</p>
        ) : result.events.length === 0 ? (
          <p className="text-small text-ink-2">{S.kbset.activityEmpty}</p>
        ) : (
          <div className="divide-y divide-line">
            {result.events.map((e) => (
              <div
                key={e.id}
                className="flex items-baseline gap-3 py-2 text-body first:pt-0"
              >
                <span className="u-num shrink-0 text-fine text-ink-2">
                  {localDateTime(e.created_at)}
                </span>
                <span className="min-w-0 truncate">
                  <span className="text-ink">
                    {e.actor_name ??
                      (e.actor_id
                        ? S.kbset.deletedUser
                        : e.action.startsWith("review.")
                          ? S.kbset.adjudicator
                          : S.kbset.engine)}
                  </span>{" "}
                  <span className="text-ink-2">
                    {S.kbset.auditActions[e.action] ?? e.action}
                  </span>
                  {auditDetailName(e) && (
                    <span className="text-ink-2">
                      {" "}
                      “{auditDetailName(e)}”
                    </span>
                  )}
                </span>
              </div>
            ))}
          </div>
        )}
        {result && (
          <Pager total={result.total} pageSize={AUDIT_PAGE} page={page} onPage={setPage} />
        )}
      </SettingsCard>
    </div>
  );
}

function KbMembers({ kbId, isOpen }: { kbId: string; isOpen: boolean }) {
  const queryClient = useQueryClient();
  const members = useQuery({
    queryKey: ["kbMembers", kbId],
    queryFn: () => api.kbMembers(kbId),
  });
  const orgUsers = useQuery({ queryKey: ["orgUsers"], queryFn: api.orgUsers });
  const [adding, setAdding] = useState(false);
  const [filter, setFilter] = useState("");
  const [role, setRole] = useState("all");
  const [editingRole, setEditingRole] = useState<string | null>(null);
  const [addUserId, setAddUserId] = useState("");
  // open // Open library: not even the viewer option exists, so the default must follow
  const [addRole, setAddRole] = useState(isOpen ? "editor" : "viewer");

  const invalidate = () =>
    queryClient.invalidateQueries({ queryKey: ["kbMembers", kbId] });

  const setMember = useMutation({
    mutationFn: ({ userId, role }: { userId: string; role: string }) =>
      api.setKbMember(kbId, userId, role),
    onSuccess: () => {
      setAddUserId("");
      invalidate();
    },
  });
  const remove = useMutation({
    mutationFn: (userId: string) => api.removeKbMember(kbId, userId),
    onSuccess: invalidate,
  });

  // On an open library, a `role=viewer` row is **equivalent to having no row**: read access is already available to everyone,
  // and the record grants nothing. So the list keeps only people who actually have write access.
  //
  // **Leaving them out of memberIds is half of the pairing**: they must not just be hidden. If they stayed in,
  // that person would disappear from the add-picker, and could then never be granted editor,
  // a record that should be meaningless would instead lock the person out
  const listed = (members.data?.members ?? []).filter(
    (m) => !isOpen || m.role !== "viewer",
  );
  const memberIds = new Set(listed.map((m) => m.user_id));
  const addable = orgUsers.data?.filter((u) => !memberIds.has(u.id)) ?? [];
  const q = filter.trim().toLowerCase();
  const shown = listed
    .filter((m) => role === "all" || m.role === role)
    .filter(
      (m) =>
        !q ||
        m.display_name.toLowerCase().includes(q) ||
        m.email.toLowerCase().includes(q),
    );

  if (members.isError) return null;

  return (
    <div className="space-y-3">
      <p className="text-body text-ink-2">
        {isOpen ? S.kbset.membersHintOpen : S.kbset.membersHintRestricted}
      </p>

      {/* The filter for the list and the add-person button both sit **outside** the table (DESIGN.md 6): they are not contents of the list,
          and when a filter empties everything the table must be able to become an empty state, not take the only way to change the filter and add people along with it.
          This row is exactly like user management on the deployment side */}
      <div className="flex flex-wrap items-center gap-2">
        <Input
          icon={<Search size={13} />}
          className="w-64"
          placeholder={S.settings.searchUsers}
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <Dropdown
          className="w-32"
          value={role}
          onChange={setRole}
          options={[
            { value: "all", label: S.members.filterAllRoles },
            ...rolesFor(isOpen),
          ]}
        />
        <div className="ml-auto">
          <Button variant="primary" size="sm" onClick={() => setAdding(true)}>
            <Plus size={12} />
            {S.kbset.addMemberTitle}
          </Button>
        </div>
      </div>

      {/* One table, each column's width fixed: role and action each sit in their own column, not drifting with name length */}
      <div className="glass overflow-hidden rounded-panel">
        <Table>
          <THead>
            <Tr>
              <Th>{S.members.userLabel}</Th>
              <Th>{S.members.roleLabel}</Th>
              <Th />
            </Tr>
          </THead>
          <TBody>
            {shown.map((m) => (
              <Tr key={m.user_id}>
                <Td>
                  <div className="truncate text-body text-ink">{m.display_name}</div>
                  <div className="truncate text-small text-ink-2">{m.email}</div>
                </Td>
                <Td>
                  {/* Static text, becomes a dropdown only on click: a column of dropdowns would make a read-only list look like a table to be filled,
                      while changing roles is occasional */}
                  {editingRole === m.user_id ? (
                    <Dropdown
                      size="sm"
                      className="w-24"
                      value={m.role}
                      onChange={(r) => {
                        setEditingRole(null);
                        if (r !== m.role) setMember.mutate({ userId: m.user_id, role: r });
                      }}
                      options={rolesFor(isOpen)}
                    />
                  ) : (
                    <LinkButton onClick={() => setEditingRole(m.user_id)}>
                      {S.kbset.roles[m.role as keyof typeof S.kbset.roles] ?? m.role}
                    </LinkButton>
                  )}
                </Td>
                <Td className="whitespace-nowrap text-right">
                  <LinkButton tone="danger" onClick={() => remove.mutate(m.user_id)}>
                    {S.kbset.remove}
                  </LinkButton>
                </Td>
              </Tr>
            ))}
          </TBody>
        </Table>
        {/* No people at all is different from 'filtered away': the former should go add people, the latter should change the filter */}
        {shown.length === 0 && (
          <p className="px-4 py-6 text-body text-ink-2">
            {listed.length === 0
              ? isOpen
                ? S.kbset.noWriters
                : S.kbset.noMembers
              : S.ui.noMatches}
          </p>
        )}
      </div>

    {/* Add an existing account to this library. **The picker also stays resident in the dialog**: when no one can be added,
        it says so itself (SearchSelect has a noMatches empty state); the control disappearing would read as 'broken' */}
    <Dialog
      open={adding}
      onOpenChange={setAdding}
      title={S.kbset.addMemberTitle}
      closeLabel={S.ui.close}
      footer={
        <>
          <Button variant="secondary" size="sm" onClick={() => setAdding(false)}>
            {S.members.cancel}
          </Button>
          <Button variant="primary" size="sm"
            disabled={!addUserId || setMember.isPending}
            onClick={() => {
              setMember.mutate({ userId: addUserId, role: addRole });
              setAdding(false);
            }}
          >
            {S.members.add}
          </Button>
        </>
      }
    >
      <div className="grid grid-cols-2 gap-3">
        <Field label={S.members.userLabel} className="mb-0">
          <SearchSelect
            className="w-full"
            value={addUserId}
            onChange={setAddUserId}
            placeholder={S.kbset.addMember}
            options={addable.map((u) => ({
              value: u.id,
              label: u.display_name,
              hint: u.email,
            }))}
          />
        </Field>
        <Field label={S.members.roleLabel} className="mb-0">
          <Dropdown
            className="w-full"
            value={addRole}
            onChange={setAddRole}
            options={rolesFor(isOpen)}
          />
        </Field>
      </div>
    </Dialog>
    </div>
  );
}
