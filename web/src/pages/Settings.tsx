import { useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useSearch } from "@tanstack/react-router";
import { Plus, X } from "lucide-react";
import { api, type DataSourceView } from "../api";
import { LANG_NAMES, S } from "../i18n";
import { useKb } from "../kb";
import { toast } from "../toast";
import {
  Button,
  Checkbox,
  Chip,
  Dialog,
  Dropdown,
  Field,
  IconButton,
  Input,
  LinkButton,
  Pill,
  SearchSelect,
  Segmented,
  SettingsCard,
  Table,
  TBody,
  Td,
  Th,
  THead,
  Tr,
  PageHeader,
  chipLike,} from "../ui";
import { Members } from "./Members";
import { SsoAdmin } from "./Sso";
import {
  clearSubmittedDraft, savedModelFields,
  type ModelCard, type ModelDraft, type ModelDrafts,
} from "./modelSettingsDraft";

/** The notes on the two model cards say only one thing, and only the latest one (#698).
 *
 * Previously the note was 'say the test result if there is one, otherwise say saved': after a test, changing the key left the old result stuck to the card;
 * one test ran both cards together while both buttons spun 'Testing…' at once; the embedding card's error was body-text colored,
 * while the chat card's error was danger-colored. So 'Saved' did not say which action it was, nor which card this test belonged to:
 * all had to be guessed.
 *
 * Three rules. The display layer only enforces the last one; the first two are guaranteed at the call site:
 * 1. Changing any cell or pressing any save expires the old Saved and any errors; a test result expires only for the card that was changed
 *    (see `cardsOf`). Cards whose config was not touched keep their test conclusions (#1424);
 * 2. One test per card (#1424): the test, its 'Testing…' and its conclusion belong only to the card whose button was pressed;
 * 3. A newly arrived test result (including a failed one) overrides the old saved state: having just pressed 'Test', the card should talk about the test.
 *
 * Pass in plain data and get back a plain conclusion (including which tone to use); the strings are still localized (i18n) at the call site,
 * and the pure function lets vitest pin this priority table directly. */
export type ModelCardTest = { ok: boolean; message: string } | null;
/** `dirty`: the cells on this card that were changed but not yet saved */
export type ModelCardSave = { error: string | null; saved: boolean; dirty: boolean };
export type ModelCardStatus =
  | { kind: "note"; tone: "text-ok" | "text-danger"; text: string }
  | { kind: "unsaved" }
  | { kind: "saved" }
  | { kind: "idle" };

/** Which cards' config changed given the cells just edited. The field-name prefix is the card: chat_ / embed_ / ocr_ / transcribe_;
 *  a preset that fills both the chat and embedding cells at once counts both (#1424) */
export function cardsOf(fields: readonly string[]): ModelCard[] {
  return (["chat", "embed", "ocr", "transcribe"] as const).filter((c) =>
    fields.some((f) => f.startsWith(`${c}_`)),
  );
}

/** The test conclusion for the chat card: one row per request shape; a rejected row carries the endpoint's own words, so at a glance you see
 *  which parameter is not accepted (#1354). When an old server does not return `checks`, fall back to one whole sentence */
export function chatTestMessage(chat: {
  ok: boolean;
  reply?: string;
  error?: string;
  checks?: Record<"plain" | "streaming" | "tools", { ok: boolean; timed_out?: boolean; error?: string }>;
}): string {
  if (!chat.checks) return chat.ok ? S.settings.ok(chat.reply ?? "OK") : (chat.error ?? "");
  return (["plain", "streaming", "tools"] as const)
    .map((shape) => {
      const c = chat.checks![shape];
      const verdict = c.ok
        ? S.settings.chatShapeOk
        : c.timed_out
          ? S.settings.chatShapeTimedOut(c.error ?? "")
          : S.settings.chatShapeRefused(c.error ?? "");
      return `${S.settings.chatShapes[shape]}: ${verdict}`;
    })
    .join("\n");
}

export function modelCardStatus(
  test: ModelCardTest,
  testError: string | null,
  save: ModelCardSave,
): ModelCardStatus {
  /* A test always tests the **saved** config. When the card has unsaved edits, no test result describes the form's values:
     in the form. Previously, after changing the key, pressing test showed the old key's 'connected', or the cells were all filled in
     yet it said Not configured (#698's rules 2 and 3). An error from a failed save still speaks first */
  if (save.error)
    return { kind: "note", tone: "text-danger", text: save.error };
  if (save.dirty) return { kind: "unsaved" };
  if (test)
    return test.ok
      ? { kind: "note", tone: "text-ok", text: test.message }
      : { kind: "note", tone: "text-danger", text: test.message };
  if (testError) return { kind: "note", tone: "text-danger", text: testError };
  if (save.saved) return { kind: "saved" };
  return { kind: "idle" };
}

/** The six sections of the admin page. **They are the second level of the left column, not a row of tabs above the body**:
    *  they are the same kind of thing as the four items in the account bar (where you are), just one level lower; the address uses `?tab=`,
    *  so refresh, back, and shared links all land on the same section. The left column (AccountShell) and this page's title
    *  read from the same source, so the name exists in only one place. **label is a function**: the interface language can be switched at runtime,
    *  and reading it at module top would lock in the language loaded the first time */
export const ADMIN_TABS = [
  { key: "models", label: () => S.settings.tabModels },
  { key: "members", label: () => S.settings.tabMembers },
  { key: "datasources", label: () => S.settings.datasources.tab },
  { key: "sso", label: () => S.settings.tabSso },
  { key: "deployment", label: () => S.settings.tabDeployment },
] as const;

/** Which workspaces a source is authorized for (0014).
 *
 * **Authorization and mounting are two layers**: here we say 'who this source may be used by'; the KB admin then picks, among the authorized set, which to mount.
 * Previously there was no such layer: the mountable list returned every source in the deployment, so the admin of any
 * library could mount any production library into their own.
 *
 * Both layers are many-to-many: one source can be authorized to many workspaces, and one workspace can get many sources. */
function SourceGrants({ sourceId }: { sourceId: string }) {
  const queryClient = useQueryClient();
  const [picked, setPicked] = useState("");
  const [notice, setNotice] = useState<string | null>(null);

  const grants = useQuery({
    queryKey: ["dataSourceGrants", sourceId],
    queryFn: () => api.dataSourceGrants(sourceId),
  });
  const workspaces = useQuery({
    queryKey: ["workspaces"],
    queryFn: api.workspaces,
  });
  const invalidate = () =>
    queryClient.invalidateQueries({ queryKey: ["dataSourceGrants", sourceId] });

  const grant = useMutation({
    mutationFn: (wsId: string) => api.grantDataSource(sourceId, wsId),
    onSuccess: () => {
      setPicked("");
      setNotice(null);
      invalidate();
    },
    onError: (e: unknown) => toast.error((e as Error).message),
  });
  const revoke = useMutation({
    mutationFn: (wsId: string) => api.revokeDataSource(sourceId, wsId),
    // Say how many were removed: revoking authorization also cuts off mounts in use,
    // and a silent cut is worse than the cut itself
    onSuccess: (r) => {
      setNotice(S.settings.datasources.grantRevoked(r.unmounted));
      invalidate();
    },
    onError: (e: unknown) => toast.error((e as Error).message),
  });

  const granted = grants.data?.workspaces ?? [];
  const grantedIds = new Set(granted.map((w) => w.id));
  const grantable = (workspaces.data ?? []).filter(
    (w) => !grantedIds.has(w.id),
  );

  return (
    <div className="border-t border-line pt-3 space-y-2">
      <div className="flex items-baseline gap-2">
        <span className="text-fine text-ink-2">
          {S.settings.datasources.grants}
        </span>
        {granted.length === 0 ? (
          <span className="text-fine text-ink-2">
            {S.settings.datasources.grantsNone}
          </span>
        ) : (
          <div className="flex flex-wrap gap-2">
            {granted.map((w) => (
              <span
                key={w.id}
                className={chipLike("neutral", "text-fine flex items-center gap-1")}
              >
                {w.name}
                <IconButton size="sm" label={S.settings.datasources.grantRevoke}
                  disabled={revoke.isPending}
                  onClick={() => revoke.mutate(w.id)}
                >
                  <X size={10} />
                </IconButton>
              </span>
            ))}
          </div>
        )}
      </div>
      {grantable.length > 0 && (
        <div className="flex items-center gap-2">
          <SearchSelect
            className="flex-1"
            value={picked}
            options={grantable.map((w) => ({ value: w.id, label: w.name }))}
            onChange={(v) => {
              setPicked(v);
              if (v) grant.mutate(v);
            }}
            placeholder={S.settings.datasources.grantAdd}
          />
        </div>
      )}
      {notice && <p className="text-fine text-ink-2">{notice}</p>}
    </div>
  );
}

/** Deployment-level config: the registration switch + worker concurrency. */
export function DeploymentAdmin() {
  const queryClient = useQueryClient();
  const dep = useQuery({
    queryKey: ["deployment"],
    queryFn: api.adminDeployment,
  });
  const [workers, setWorkers] = useState<number | null>(null);
  const shown = workers ?? dep.data?.worker_concurrency ?? 64;
  // Per-model concurrency: a default value + an override for each model in use
  const [modelDefault, setModelDefault] = useState<number | null>(null);
  const shownDefault =
    modelDefault ?? dep.data?.default_model_concurrency ?? 10;
  const [perModel, setPerModel] = useState<Record<string, number>>({});
  const save = useMutation({
    mutationFn: (v: {
      open: boolean;
      workers?: number;
      defaultModel?: number;
      modelLimit?: {
        base_url: string;
        model: string;
        max_concurrent: number | null;
      };
      ontologyLang?: "en" | "zh";
    }) =>
      api.saveAdminDeployment(
        v.open,
        v.workers,
        v.defaultModel,
        v.modelLimit,
        v.ontologyLang,
      ),
    onSuccess: () => {
      setPerModel({});
      queryClient.invalidateQueries({ queryKey: ["deployment"] });
    },
  });
  const open = dep.data?.open_registration ?? true;

  return (
    /* One concern per card, with the same layout as library settings. Those that take effect immediately (the registration switch, the default ontology language)
       have no footer: they have no 'save' step; numbers that only count after a press have their button in the footer */
    <div className="space-y-4">
      <SettingsCard title={S.settings.cardAccounts}>
        <Checkbox
          checked={open}
          disabled={dep.isPending || save.isPending}
          onChange={(v) => save.mutate({ open: v })}
          label={S.settings.deployment.openReg}
          hint={S.settings.deployment.openRegHint}
        />
      </SettingsCard>

      {/* The ontology language for newly created libraries. **Not the interface language**: the interface language is picked by each person in the account menu,
          and never goes through the backend at all (docs/decisions/0004). The explanation must state this */}
      <SettingsCard
        title={S.settings.deployment.ontologyLang}
        hint={S.settings.deployment.ontologyLangHint}
      >
        <Segmented
          size="sm"
          className="w-fit"
          disabled={dep.isPending || save.isPending}
          value={dep.data?.default_ontology_lang ?? "en"}
          onChange={(l) => save.mutate({ open, ontologyLang: l })}
          options={(["en", "zh"] as const).map((l) => ({
            value: l,
            label: LANG_NAMES[l],
          }))}
        />
      </SettingsCard>

      <SettingsCard
        title={S.settings.deployment.workers}
        hint={S.settings.deployment.workersHint}
        action={
          <>
            <Input size="sm" className="u-input-plain w-16 u-num text-center"
              type="number"
              min={1}
              max={256}
              value={shown}
              disabled={dep.isPending}
              onChange={(e) =>
                setWorkers(Math.max(1, Math.min(256, Number(e.target.value) || 1)))
              }
            />
            <Button variant="secondary" size="sm"
              disabled={
                save.isPending ||
                workers === null ||
                workers === dep.data?.worker_concurrency
              }
              onClick={() => save.mutate({ open, workers: shown })}
            >
              {S.settings.deployment.workersApply}
            </Button>
          </>
        }
      />

      {/* Per-model concurrency is the real throttle: the constraint comes from the provider's rate limit, which is computed per model.
          The worker concurrency above is only an outer fallback, to stop tasks from piling up without bound */}
      <SettingsCard
        title={S.settings.deployment.modelConcurrency}
        hint={S.settings.deployment.modelConcurrencyHint}
        note={S.settings.deployment.modelDefault}
        action={
          <>
            <Input size="sm" className="u-input-plain w-16 u-num text-center"
              type="number"
              min={1}
              max={256}
              value={shownDefault}
              disabled={dep.isPending}
              onChange={(e) =>
                setModelDefault(
                  Math.max(1, Math.min(256, Number(e.target.value) || 1)),
                )
              }
            />
            <Button variant="secondary" size="sm"
              disabled={
                save.isPending ||
                modelDefault === null ||
                modelDefault === dep.data?.default_model_concurrency
              }
              onClick={() => save.mutate({ open, defaultModel: shownDefault })}
            >
              {S.settings.deployment.workersApply}
            </Button>
          </>
        }
      >
        {/* Each model in use gets its own row: the numbers and buttons on this row belong to this row, not to the footer */}
        {!!dep.data?.models_in_use?.length && (
          <div className="divide-y divide-line">
            {dep.data.models_in_use.map((m) => {
              const cur =
                dep.data?.model_limits?.find(
                  (l) => l.base_url === m.base_url && l.model === m.model,
                )?.max_concurrent ?? null;
              const key = `${m.base_url}|${m.model}`;
              const val = perModel[key] ?? cur ?? shownDefault;
              return (
                <div
                  key={key}
                  className="flex items-center gap-2 py-3 text-small first:pt-0"
                >
                  <span className={chipLike("neutral", "text-fine px-2 shrink-0")}>
                    {m.kind}
                  </span>
                  <span className="font-mono text-ink-2 truncate">{m.model}</span>
                  <span className="text-ink-2 truncate hidden sm:inline">
                    {m.base_url}
                  </span>
                  {/* The number box and button are grouped, **with the same geometry as the group in the footer**
                      (w-16, gap-3): both right edges reach the card's inner padding; if the spacing differs by 4px,
                      the left edges of the two boxes in the same column misalign, and it looks like the padding is not aligned */}
                  <div className="ml-auto flex shrink-0 items-center gap-3">
                    <Input size="sm" className="u-input-plain w-16 u-num text-center"
                      type="number"
                      min={1}
                      max={256}
                      value={val}
                      onChange={(e) =>
                        setPerModel({
                          ...perModel,
                          [key]: Math.max(1, Math.min(256, Number(e.target.value) || 1)),
                        })
                      }
                    />
                    <Button variant="secondary" size="sm"
                      disabled={save.isPending || perModel[key] === undefined}
                      onClick={() =>
                        save.mutate({
                          open,
                          modelLimit: {
                            base_url: m.base_url,
                            model: m.model,
                            max_concurrent: val,
                          },
                        })
                      }
                    >
                      {S.settings.deployment.workersApply}
                    </Button>
                    {cur !== null && (
                      <Button variant="secondary" size="sm"
                        disabled={save.isPending}
                        title={S.settings.deployment.modelResetHint}
                        onClick={() =>
                          save.mutate({
                            open,
                            modelLimit: {
                              base_url: m.base_url,
                              model: m.model,
                              max_concurrent: null,
                            },
                          })
                        }
                      >
                        {S.settings.deployment.modelReset}
                      </Button>
                    )}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </SettingsCard>

      {save.isError && (
        <p className="text-small text-danger">{(save.error as Error).message}</p>
      )}
    </div>
  );
}

/** Library management (deployment level): an overview of all libraries + creating new ones (creating is an admin action; the switcher only switches). */

/** System-level data source registration (for query): credentials go in but never come out; the list only shows a host:port/db summary. */
/* ---- What each engine asks for, and how those answers are assembled into one connection string ----

   **The person fills in fields, but what is stored is still a connection string.** The server has always identified the engine by scheme (see
   `query_engine::engine_from_conn`), so here we just turn 'writing a URL' into
   'filling in a few boxes': the @ and / in a password need not be escaped by hand either.

   The last tier is the verbatim connection string: forms like sslmode and extra parameters cannot be assembled, so leave a door open for experts. */
type DsField = {
  key: string;
  label: string;
  placeholder?: string;
  optional?: boolean;
  secret?: boolean;
};
type EngineSpec = {
  id: string;
  /** Engine names are proper nouns and are not translated */
  label: string;
  fields: DsField[];
  /** Assemble the connection string from the filled-in boxes.
   *
   * **What it receives is an incomplete table**: `values` holds only the boxes a person has actually filled in; right after switching to a tier
   * of engine it may be empty. So every build must tolerate missing keys:
   * the type is written as `Partial` rather than `Record<string, string>`, because the latter is a lie:
   * an index signature claims that any key yields a string, so `v.conn.trim()` compiles,
   * and selecting 'connection string' throws, replacing the whole page with an error screen (#573).
   * The other tiers did not blow up at the time, only because the template string quietly rendered `undefined` as "undefined". */
  build: (v: Partial<Record<string, string>>) => string;
};

/** Both helpers **accept an absent value**, because the form is filled in one box at a time:
 *  right after switching to a tier of engine, no box has a value. Their bodies were already written this way (`enc` treats a missing value as an empty string,
 *  `auth` uses truthiness), but the signature always said `string`, and never made this explicit.
 *  #573 fell through exactly this gap. */
const enc = (v?: string) => encodeURIComponent(v ?? "");
/** `user:pass@` segment: both parts may be empty (Trino allows no password) */
const auth = (user?: string, pass?: string) =>
  pass ? `${enc(user)}:${enc(pass)}@` : user ? `${enc(user)}@` : "";

export function dsSpecs(): EngineSpec[] {
  const D = S.settings.datasources;
  return [
    {
      id: "postgres",
      label: "Postgres",
      fields: [
        { key: "host", label: D.fHost, placeholder: "db.internal" },
        { key: "port", label: D.fPort, placeholder: "5432" },
        { key: "database", label: D.fDatabase, placeholder: "analytics" },
        { key: "user", label: D.fUser },
        { key: "password", label: D.fPassword, secret: true },
      ],
      build: (v) =>
        `postgres://${auth(v.user, v.password)}${v.host}:${v.port || "5432"}/${enc(v.database)}`,
    },
    {
      id: "mysql",
      label: "MySQL",
      fields: [
        { key: "host", label: D.fHost, placeholder: "db.internal" },
        { key: "port", label: D.fPort, placeholder: "3306" },
        { key: "database", label: D.fDatabase },
        { key: "user", label: D.fUser },
        { key: "password", label: D.fPassword, secret: true },
      ],
      build: (v) =>
        `mysql://${auth(v.user, v.password)}${v.host}:${v.port || "3306"}/${enc(v.database)}`,
    },
    {
      id: "clickhouse",
      label: "ClickHouse",
      // Both user and database may be left blank: no user means the server's default, and no database means read all non-system databases.
      // Port 8443 / 443 makes the server itself use https, so the form need not ask again
      fields: [
        { key: "host", label: D.fHost, placeholder: "ch.internal" },
        { key: "port", label: D.fPort, placeholder: "8123" },
        { key: "database", label: D.fDatabase, optional: true },
        { key: "user", label: D.fUser, optional: true, placeholder: "default" },
        { key: "password", label: D.fPassword, optional: true, secret: true },
      ],
      // A password alone is the password of the default user: with no user name, the server will not send the password out
      build: (v) =>
        `clickhouse://${auth(v.user || (v.password ? "default" : ""), v.password)}` +
        `${v.host}:${v.port || "8123"}` +
        (v.database ? `/${enc(v.database)}` : ""),
    },
    {
      id: "trino",
      label: "Trino",
      fields: [
        { key: "host", label: D.fHost },
        { key: "port", label: D.fPort, placeholder: "8080" },
        { key: "catalog", label: D.fCatalog, placeholder: "iceberg" },
        { key: "schema", label: D.fSchema, optional: true },
        { key: "user", label: D.fUser },
        { key: "password", label: D.fPassword, optional: true, secret: true },
      ],
      build: (v) =>
        `trino://${auth(v.user, v.password)}${v.host}:${v.port || "8080"}/${enc(v.catalog)}` +
        (v.schema ? `/${enc(v.schema)}` : ""),
    },
    {
      id: "databricks",
      label: "Databricks",
      fields: [
        { key: "host", label: D.fHost, placeholder: "dbc-1234.cloud.databricks.com" },
        { key: "token", label: D.fToken, secret: true },
        { key: "warehouse", label: D.fWarehouseId, placeholder: "abc123def456" },
        { key: "catalog", label: D.fCatalog, optional: true, placeholder: "main" },
      ],
      build: (v) =>
        `databricks://:${enc(v.token)}@${v.host}/sql/1.0/warehouses/${v.warehouse}` +
        (v.catalog ? `?catalog=${enc(v.catalog)}` : ""),
    },
    {
      id: "snowflake",
      label: "Snowflake",
      fields: [
        {
          key: "account",
          label: D.fAccount,
          placeholder: "acme-xy12345.snowflakecomputing.com",
        },
        { key: "token", label: D.fToken, secret: true },
        { key: "database", label: D.fDatabase },
        { key: "schema", label: D.fSchema },
        { key: "warehouse", label: D.fWarehouse },
      ],
      build: (v) =>
        `snowflake://:${enc(v.token)}@${v.account}/${enc(v.database)}/${enc(v.schema)}` +
        `?warehouse=${enc(v.warehouse)}`,
    },
    {
      id: "raw",
      label: D.engineRaw,
      fields: [{ key: "conn", label: D.connString }],
      build: (v) => (v.conn ?? "").trim(),
    },
  ];
}

/** Register a data source: pick an engine, fill in a few boxes, and **try it once before saving**. */
function NewDataSourceDialog({
  open,
  onOpenChange,
  onCreated,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated: () => void;
}) {
  const specs = dsSpecs();
  const [name, setName] = useState("");
  const [engineId, setEngineId] = useState(specs[0].id);
  const [values, setValues] = useState<Record<string, string>>({});
  const spec = specs.find((s) => s.id === engineId) ?? specs[0];
  const conn = spec.build(values);
  const filled = spec.fields.every((f) => f.optional || (values[f.key] ?? "").trim());
  const ready = !!name.trim() && filled;

  // The trial connection saves nothing, so after switching engine or editing fields the previous result no longer counts
  const probe = useMutation({
    mutationFn: () => api.adminTestConnString(conn),
  });
  const create = useMutation({
    mutationFn: () => api.adminCreateDataSource({ name: name.trim(), conn_string: conn }),
    onSuccess: () => {
      setName("");
      setValues({});
      onCreated();
    },
  });
  // The dialog shows only one result at a time: the trial and the add each have their own error; keeping both would show the same sentence twice
  // (#921). Editing a field or switching the action makes the previous result no longer count
  const set = (key: string, v: string) => {
    probe.reset();
    create.reset();
    setValues((prev) => ({ ...prev, [key]: v }));
  };

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title={S.settings.datasources.newTitle}
      closeLabel={S.ui.close}
      width="lg"
      footer={
        <>
          {/* The trial connection sits on the left: it is not 'done', it is the step before done */}
          <Button variant="secondary" size="sm" className="mr-auto"
            disabled={!filled || probe.isPending}
            onClick={() => {
              create.reset();
              probe.mutate();
            }}
          >
            {probe.isPending
              ? S.settings.datasources.testing
              : S.settings.datasources.testConn}
          </Button>
          <Button variant="secondary" size="sm" onClick={() => onOpenChange(false)}>
            {S.members.cancel}
          </Button>
          <Button variant="primary" size="sm"
            disabled={!ready || create.isPending}
            onClick={() => {
              probe.reset();
              create.mutate();
            }}
          >
            {S.settings.datasources.add}
          </Button>
        </>
      }
    >
      <div className="space-y-3">
        <div className="grid grid-cols-2 gap-3">
          <Field label={S.settings.datasources.name} className="mb-0">
            <Input className="w-full"
              autoFocus
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </Field>
          <Field label={S.settings.datasources.engine} className="mb-0">
            <Dropdown
              className="w-full"
              value={engineId}
              onChange={(v) => {
                setEngineId(v);
                setValues({});
                probe.reset();
                create.reset();
              }}
              options={specs.map((s) => ({ value: s.id, label: s.label }))}
            />
          </Field>
        </div>

        <div className="grid grid-cols-2 gap-3">
          {spec.fields.map((f) => (
            <Field
              key={f.key}
              className={`mb-0 ${f.key === "conn" ? "col-span-2" : ""}`}
              label={
                f.optional
                  ? `${f.label} ${S.settings.datasources.optional}`
                  : f.label
              }
            >
              <Input className="w-full"
                type={f.secret ? "password" : undefined}
                autoComplete={f.secret ? "new-password" : "off"}
                placeholder={f.placeholder}
                value={values[f.key] ?? ""}
                onChange={(e) => set(f.key, e.target.value)}
              />
            </Field>
          ))}
        </div>

        {/* The assembled string is shown for a glance, with the password masked. A wrong port, or an extra slash in the database name,
            is obvious here at a glance, without waiting for the trial connection */}
        {engineId !== "raw" && filled && (
          <p className="truncate font-mono text-fine text-ink-2" title={maskConn(conn)}>
            {maskConn(conn)}
          </p>
        )}

        {probe.data && (
          <p className={`text-small ${probe.data.ok ? "text-ok" : "text-danger"}`}>
            {probe.data.ok
              ? S.settings.datasources.testOk
              : (probe.data.error ?? S.settings.datasources.testFail)}
          </p>
        )}
        {probe.isError && (
          <p className="text-small text-danger">{(probe.error as Error).message}</p>
        )}
        {create.isError && (
          <p className="text-small text-danger">{(create.error as Error).message}</p>
        )}
      </div>
    </Dialog>
  );
}

/** When echoing, mask the password segment: `postgres://user:••••@host/db` */
function maskConn(conn: string): string {
  return conn.replace(/:\/\/([^:@/]*):([^@]*)@/, (_m, user: string) =>
    user ? `://${user}:••••@` : "://:••••@",
  );
}

function DataSourcesAdmin() {
  const queryClient = useQueryClient();
  const list = useQuery({
    queryKey: ["dataSources"],
    queryFn: api.adminDataSources,
  });
  const [creating, setCreating] = useState(false);
  // Authorization is per workspace; in a single-tenant deployment nobody has seen that one workspace, so it does not take a column in the table,
  // and expands only when the 'available to' cell is clicked
  const [grantsFor, setGrantsFor] = useState<DataSourceView | null>(null);
  const invalidate = () =>
    queryClient.invalidateQueries({ queryKey: ["dataSources"] });

  const remove = useMutation({
    mutationFn: (id: string) => api.adminDeleteDataSource(id),
    onSettled: invalidate,
  });
  const test = useMutation({
    mutationFn: (id: string) => api.adminTestDataSource(id),
    onSettled: invalidate,
  });

  const rows = list.data?.data_sources ?? [];

  return (
    <div className="space-y-4">
      {/* The registration sits above the list, on the right: actions come before content */}
      <div className="flex items-start gap-4">
        <p className="min-w-0 flex-1 text-small text-ink-2">
          {S.settings.datasources.hint}
        </p>
        <Button variant="primary" size="sm" className="shrink-0"
          onClick={() => setCreating(true)}
        >
          <Plus size={12} />
          {S.settings.datasources.add}
        </Button>
      </div>

      {rows.length === 0 ? (
        <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
          {S.settings.datasources.empty}
        </div>
      ) : (
        <div className="glass rounded-panel overflow-hidden">
          <Table>
            <THead>
              <Tr>
                <Th>{S.settings.datasources.name}</Th>
                <Th>{S.settings.datasources.engine}</Th>
                <Th>{S.settings.datasources.colConn}</Th>
                <Th>{S.settings.datasources.grants}</Th>
                <Th>{S.settings.datasources.colStatus}</Th>
                <Th />
              </Tr>
            </THead>
            <TBody>
              {rows.map((d) => (
                <Tr key={d.id}>
                  <Td className="text-ink">{d.name}</Td>
                  <Td className="text-small text-ink-2">{d.engine}</Td>
                  <Td
                    className="max-w-xs truncate font-mono text-small text-ink-2"
                    title={d.summary}
                  >
                    {d.summary}
                  </Td>
                  <Td>
                    <GrantsCell sourceId={d.id} onOpen={() => setGrantsFor(d)} />
                  </Td>
                  <Td>
                    <Chip
                      tone={
                        d.last_test_ok === true
                          ? "success"
                          : d.last_test_ok === false
                            ? "danger"
                            : "neutral"
                      }
                    >
                      {d.last_test_ok === true
                        ? S.settings.datasources.testOk
                        : d.last_test_ok === false
                          ? S.settings.datasources.testFail
                          : S.settings.datasources.neverTested}
                    </Chip>
                  </Td>
                  <Td className="whitespace-nowrap text-right">
                    <LinkButton
                      disabled={test.isPending}
                      onClick={() => test.mutate(d.id)}
                    >
                      {S.settings.datasources.test}
                    </LinkButton>
                    <LinkButton
                      tone="danger"
                      className="ml-3"
                      disabled={remove.isPending}
                      onClick={() => remove.mutate(d.id)}
                    >
                      {S.settings.datasources.remove}
                    </LinkButton>
                  </Td>
                </Tr>
              ))}
            </TBody>
          </Table>
        </div>
      )}

      <NewDataSourceDialog
        open={creating}
        onOpenChange={setCreating}
        onCreated={() => {
          setCreating(false);
          invalidate();
        }}
      />
      <Dialog
        open={!!grantsFor}
        onOpenChange={(o) => !o && setGrantsFor(null)}
        title={grantsFor?.name ?? ""}
        description={S.settings.datasources.grantsHint}
        closeLabel={S.ui.close}
      >
        {grantsFor && <SourceGrants sourceId={grantsFor.id} />}
      </Dialog>
    </div>
  );
}

/** The cell in the table: who it is authorized to. Clicking it expands the authorization panel. **Being unauthorized must be visible**,
    *  since such a source cannot be mounted by anyone, and this is the only place that can say so */
function GrantsCell({
  sourceId,
  onOpen,
}: {
  sourceId: string;
  onOpen: () => void;
}) {
  const grants = useQuery({
    queryKey: ["dataSourceGrants", sourceId],
    queryFn: () => api.dataSourceGrants(sourceId),
  });
  const names = (grants.data?.workspaces ?? []).map((w) => w.name);
  // Unauthorized is a serious state (no one can mount it), so it uses a warning pill; an authorized one is only a plain fact
  return names.length === 0 ? (
    <Chip tone="warn" onClick={onOpen}>
      {S.settings.datasources.grantsNoneShort}
    </Chip>
  ) : (
    <LinkButton onClick={onOpen} title={names.join(" · ")}>
      {S.settings.datasources.grantsCount(names.length)}
    </LinkButton>
  );
}

const PRESETS: Record<
  string,
  { chat: string; embed: string; chatModel: string; embedModel: string }
> = {
  DeepSeek: {
    chat: "https://api.deepseek.com/v1",
    chatModel: "deepseek-chat",
    embed: "",
    embedModel: "",
  },
  SiliconFlow: {
    chat: "https://api.siliconflow.cn/v1",
    chatModel: "deepseek-ai/DeepSeek-V3",
    embed: "https://api.siliconflow.cn/v1",
    embedModel: "BAAI/bge-m3",
  },
  "Qwen (DashScope)": {
    chat: "https://dashscope.aliyuncs.com/compatible-mode/v1",
    chatModel: "qwen-plus",
    embed: "https://dashscope.aliyuncs.com/compatible-mode/v1",
    embedModel: "text-embedding-v3",
  },
  Ollama: {
    chat: "http://localhost:11434/v1",
    chatModel: "qwen2.5:7b",
    embed: "http://localhost:11434/v1",
    embedModel: "bge-m3",
  },
  OpenAI: {
    chat: "https://api.openai.com/v1",
    chatModel: "gpt-4o-mini",
    embed: "https://api.openai.com/v1",
    embedModel: "text-embedding-3-small",
  },
};

export function Settings() {
  const { workspace } = useKb();
  /* Which section the person is in is decided by **the address**: the second level of the left column is a set of Links (see AccountShell),
     so refresh, back, and sending the link to a colleague all land on the same section. Previously a separate state was kept here,
     so the address and the page each said something different */
  const { tab: tabParam } = useSearch({ from: "/account/admin" });
  const tab = tabParam ?? "models";
  if (!workspace)
    return <div className="p-8 text-body text-ink-2">{S.nav.loading}</div>;

  return <WorkspaceSettings key={workspace.id} workspaceId={workspace.id} tab={tab} />;
}

function WorkspaceSettings({ workspaceId, tab }: { workspaceId: string; tab: string }) {
  const settings = useQuery({
    queryKey: ["settings", workspaceId],
    queryFn: () => api.settings(workspaceId),
  });
  const [drafts, setDrafts] = useState<ModelDrafts>({});
  const savedDraft = useRef<{ card: ModelCard; draft: ModelDraft | undefined } | null>(null);
  const saved = savedModelFields(settings.data);
  const form = { ...saved, ...drafts.chat, ...drafts.embed, ...drafts.ocr, ...drafts.transcribe };
  type Fields = typeof form;

  // Chat and embedding share a PUT that replaces the whole thing: neighboring card fields take the server's saved values, and the key is sent only for the current card.
  const sharedFields = {
    chat_base_url: saved.chat_base_url,
    chat_model: saved.chat_model,
    embed_base_url: saved.embed_base_url,
    embed_model: saved.embed_model,
  };
  const reloadSettings = async () => {
    const submitted = savedDraft.current;
    try {
      await settings.refetch({ throwOnError: true });
    } catch {
      if (submitted) toast.error(S.settings.savedRefreshFailed);
      return;
    }
    if (submitted && savedDraft.current === submitted) {
      savedDraft.current = null;
      setDrafts((current) => clearSubmittedDraft(current, submitted.card, submitted.draft));
    }
  };
  const useCardSave = <Body, Result>(
    card: ModelCard,
    body: Body,
    save: (workspaceId: string, body: Body) => Promise<Result>,
  ) => {
    const mutation = useMutation({
      mutationFn: ({ body }: { body: Body; draft: ModelDraft | undefined }) => save(workspaceId, body),
      onSuccess: async (_, { draft }) => {
        savedDraft.current = { card, draft };
        await reloadSettings();
      },
    });
    return { ...mutation, submit: () => mutation.mutate({ body, draft: drafts[card] }) };
  };
  const saveChat = useCardSave("chat", {
    ...sharedFields,
    chat_base_url: form.chat_base_url,
    chat_model: form.chat_model,
    chat_api_key: form.chat_api_key,
    chat_reasoning_effort: form.chat_reasoning_effort,
  }, api.saveSettings);
  const saveEmbed = useCardSave("embed", {
    ...sharedFields,
    embed_base_url: form.embed_base_url,
    embed_model: form.embed_model,
    embed_api_key: form.embed_api_key,
  }, api.saveSettings);
  const saveOcr = useCardSave("ocr", {
    provider: form.ocr_provider,
    base_url: form.ocr_base_url,
    api_key: form.ocr_api_key,
    backend: form.ocr_backend,
    model: form.ocr_model,
  }, api.saveOcrSettings);
  const saveTranscribe = useCardSave("transcribe", {
    base_url: form.transcribe_base_url,
    api_key: form.transcribe_api_key,
    model: form.transcribe_model,
  }, api.saveTranscribeSettings);
  const saves = { chat: saveChat, embed: saveEmbed, ocr: saveOcr, transcribe: saveTranscribe };
  // Only after the read-back following the save completes does the next card get the latest baseline that the PUT needs.
  const canSave = settings.isSuccess && !settings.isFetching
    && !Object.values(saves).some((save) => save.isPending);

  /* The four cards each test their own, one mutation per card (#1424): previously the four shared one test, and pressing any one tested all four together,
     and one still had to record 'which one was pressed' so that only that one spun 'Testing…' (#698). Now testing one tests only that one,
     and conclusions still valid on the other cards are unaffected */
  const useCardTest = (card: ModelCard) =>
    // eslint-disable-next-line react-hooks/rules-of-hooks
    useMutation({ mutationFn: () => api.testSettings(workspaceId, card) });
  const tests = {
    chat: useCardTest("chat"),
    embed: useCardTest("embed"),
    ocr: useCardTest("ocr"),
    transcribe: useCardTest("transcribe"),
  };
  const editCard = (card: ModelCard, fields: ModelDraft) => {
    tests[card].reset();
    for (const save of Object.values(saves)) {
      // Editing may invalidate the old note, but we must keep tracking a save that has not yet finished.
      if (!save.isPending) save.reset();
    }
    setDrafts((current) => ({ ...current, [card]: { ...current[card], ...fields } }));
  };
  const set = (card: ModelCard, field: keyof Fields) =>
    (event: React.ChangeEvent<HTMLInputElement>) => editCard(card, { [field]: event.target.value });

  const saveCard = (card: ModelCard) => {
    tests[card].reset();
    saves[card].submit();
  };
  const startTest = (card: ModelCard) => {
    tests[card].reset();
    saves[card].reset();
    tests[card].mutate();
  };

  const label = "block text-small font-medium text-ink-2 mb-1";

  /* The notes on the two cards: a newly arrived test result overrides the old saved state; a failed test (even if the connection could not be made at all) also no longer
     quietly falls back to 'Saved' or blank: previously, when the request itself failed, the card said nothing (#698) */
  const testTransportError = (card: keyof typeof tests) =>
    tests[card].error ? (tests[card].error as Error).message : null;
  const chatStatus = modelCardStatus(
    tests.chat.data?.chat
      ? {
          ok: tests.chat.data.chat.ok,
          message: chatTestMessage(tests.chat.data.chat),
        }
      : null,
    testTransportError("chat"),
    {
      error: saveChat.error ? (saveChat.error as Error).message : null,
      saved: saveChat.isSuccess,
      dirty: drafts.chat !== undefined,
    },
  );
  const embedStatus = modelCardStatus(
    tests.embed.data?.embed
      ? {
          ok: tests.embed.data.embed.ok,
          message: tests.embed.data.embed.ok
            ? S.settings.okDim(tests.embed.data.embed.dim ?? 0)
            : (tests.embed.data.embed.error ?? ""),
        }
      : null,
    testTransportError("embed"),
    {
      error: saveEmbed.error ? (saveEmbed.error as Error).message : null,
      saved: saveEmbed.isSuccess,
      dirty: drafts.embed !== undefined,
    },
  );
  const ocrStatus = modelCardStatus(
    tests.ocr.data?.ocr
      ? {
          ok: tests.ocr.data.ocr.ok,
          message: tests.ocr.data.ocr.ok
            ? S.settings.okVersion(tests.ocr.data.ocr.version ?? "?")
            : (tests.ocr.data.ocr.error ?? ""),
        }
      : null,
    testTransportError("ocr"),
    {
      error: saveOcr.error ? (saveOcr.error as Error).message : null,
      saved: saveOcr.isSuccess,
      dirty: drafts.ocr !== undefined,
    },
  );
  const transcribeStatus = modelCardStatus(
    tests.transcribe.data?.transcribe
      ? {
          ok: tests.transcribe.data.transcribe.ok,
          message: tests.transcribe.data.transcribe.ok
            ? S.settings.okReachable
            : (tests.transcribe.data.transcribe.error ?? ""),
        }
      : null,
    testTransportError("transcribe"),
    {
      error: saveTranscribe.error ? (saveTranscribe.error as Error).message : null,
      saved: saveTranscribe.isSuccess,
      dirty: drafts.transcribe !== undefined,
    },
  );
  /* The note's styling is shared by all four cards: success uses the ok color, failure uses the danger color. Previously success was neutral accent,
     and the embedding card's errors were body-text colored, inconsistent with the ok/danger on this page's data sources section (#698) */
  const cardNote = (s: ModelCardStatus) =>
    s.kind === "idle" ? undefined : s.kind === "saved" ? (
      S.settings.saved
    ) : s.kind === "unsaved" ? (
      S.settings.unsaved
    ) : (
      <span className={`${s.tone} whitespace-pre-line`}>{s.text}</span>
    );
  /* On the two model cards that read files, add one more line after saving: how many files it has started reading for the pending job */
  const readerNote = (s: ModelCardStatus, requeued: number | undefined) =>
    s.kind === "saved" && requeued ? S.settings.savedRequeued(requeued) : cardNote(s);

  return (
    <div className="h-full overflow-y-auto u-scroll px-8 py-6">
      {/* As with KbSettings: settings are a column of fields, centered with a width limit; line length does not stretch with the window */}
      <div className="mx-auto w-full max-w-4xl">
        {/* The title is the name of this section, not 'Administration': the left column already says the person is in the admin area,
            and repeating it at the top of the page would make every section's title the same */}
        <PageHeader
          title={ADMIN_TABS.find((t) => t.key === tab)?.label() ?? S.settings.title}
        />

        {tab === "members" && <Members workspaceId={workspaceId} />}
        {tab === "datasources" && <DataSourcesAdmin />}
        {tab === "sso" && <SsoAdmin />}
        {tab === "deployment" && <DeploymentAdmin />}

        {tab === "models" && (
          <div className="space-y-4">
            <p className="text-body text-ink-2">{S.settings.modelsIntro}</p>
            {settings.isError && (
              <div className="flex items-center gap-2">
                <p role="alert" className="text-small text-danger">{settings.error.message}</p>
                <Button variant="secondary" size="sm" onClick={() => void reloadSettings()}
                  disabled={settings.isFetching}>
                  {S.settings.reloadSettings}
                </Button>
              </div>
            )}

            {/* The preset that fills both cards' fields with one press: it is not a setting itself, so it sits outside the cards */}
            <div className="flex flex-wrap gap-2">
              {Object.entries(PRESETS).map(([name, p]) => (
                <Pill
                  key={name}
                  onClick={() => {
                    editCard("chat", { chat_base_url: p.chat, chat_model: p.chatModel });
                    editCard("embed", { embed_base_url: p.embed, embed_model: p.embedModel });
                  }}
                >
                  {name}
                </Pill>
              ))}
            </div>

            <SettingsCard
              title={S.settings.chatModel}
              note={cardNote(chatStatus)}
              action={
                <>
                  <Button variant="secondary" size="sm"
                    onClick={() => {
                      startTest("chat");
                    }}
                    disabled={tests.chat.isPending || saveChat.isPending || !!drafts.chat}
                  >
                    {tests.chat.isPending ? S.settings.testing : S.settings.test}
                  </Button>
                  <Button variant="secondary" size="sm"
                    onClick={() => saveCard("chat")}
                    disabled={!canSave}
                  >
                    {saveChat.isPending ? S.settings.saving : S.settings.save}
                  </Button>
                </>
              }
            >
              <div className="space-y-3">
                <div>
                  <label className={label}>{S.settings.baseUrl}</label>
                  <Input
                    className="w-full"
                    placeholder="https://api.deepseek.com/v1"
                    value={form.chat_base_url}
                    onChange={set("chat", "chat_base_url")}
                  />
                </div>
                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className={label}>{S.settings.model}</label>
                    <Input
                      className="w-full"
                      placeholder="deepseek-chat"
                      value={form.chat_model}
                      onChange={set("chat", "chat_model")}
                    />
                  </div>
                  <div>
                    <label className={label}>{S.settings.reasoningEffort}</label>
                    {/* Reasoning models answer while thinking by default; for extraction, about 90% of a call's output is thinking. Every kind of call uses this one value;
                        which value turns thinking off depends on the endpoint (some accept minimal, some accept only none).
                        A small bounded enumeration: Dropdown (web/DESIGN.md rule 5; the page has no native select) */}
                    <Dropdown
                      className="w-full"
                      value={form.chat_reasoning_effort}
                      onChange={(chat_reasoning_effort) => editCard("chat", { chat_reasoning_effort })}
                      options={[
                        { value: "", label: S.settings.reasoningDefault },
                        { value: "none", label: "none" },
                        { value: "minimal", label: "minimal" },
                        { value: "low", label: "low" },
                        { value: "medium", label: "medium" },
                        { value: "high", label: "high" },
                      ]}
                    />
                    <div className="mt-1 text-small text-ink-2">{S.settings.reasoningHint}</div>
                  </div>
                  <div>
                    <label className={label}>
                      {S.settings.apiKey}{" "}
                      {settings.data?.has_chat_key && (
                        <span className="text-accent">{S.settings.keyConfigured}</span>
                      )}
                    </label>
                    <Input
                      className="w-full"
                      type="password"
                      placeholder="sk-…"
                      value={form.chat_api_key}
                      onChange={set("chat", "chat_api_key")}
                    />
                  </div>
                </div>
              </div>
            </SettingsCard>

            <SettingsCard
              title={S.settings.embedModel}
              note={cardNote(embedStatus)}
              action={
                <>
                  <Button variant="secondary" size="sm"
                    onClick={() => {
                      startTest("embed");
                    }}
                    disabled={tests.embed.isPending || saveEmbed.isPending || !!drafts.embed}
                  >
                    {tests.embed.isPending ? S.settings.testing : S.settings.test}
                  </Button>
                  <Button variant="secondary" size="sm"
                    onClick={() => saveCard("embed")}
                    disabled={!canSave}
                  >
                    {saveEmbed.isPending ? S.settings.saving : S.settings.save}
                  </Button>
                </>
              }
            >
              <div className="space-y-3">
                <div>
                  <label className={label}>{S.settings.baseUrl}</label>
                  <Input
                    className="w-full"
                    placeholder="http://localhost:11434/v1"
                    value={form.embed_base_url}
                    onChange={set("embed", "embed_base_url")}
                  />
                </div>
                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className={label}>{S.settings.model}</label>
                    <Input
                      className="w-full"
                      placeholder="bge-m3"
                      value={form.embed_model}
                      onChange={set("embed", "embed_model")}
                    />
                  </div>
                  <div>
                    <label className={label}>
                      {S.settings.apiKey}{" "}
                      {settings.data?.has_embed_key && (
                        <span className="text-accent">{S.settings.keyConfigured}</span>
                      )}
                    </label>
                    <Input
                      className="w-full"
                      type="password"
                      value={form.embed_api_key}
                      onChange={set("embed", "embed_api_key")}
                    />
                  </div>
                </div>
              </div>
            </SettingsCard>

            {/* Read-file models: scanned documents, images, recordings (0040). Explained separately from the two cards above, because they are only used for those file types;
                if they are not configured, other things are not affected: the file waits, and the message center will say so */}
            <h2 className="pt-4 text-title text-ink">{S.settings.readersTitle}</h2>
            <p className="text-body text-ink-2">{S.settings.readersIntro}</p>

            <SettingsCard
              title={S.settings.ocrService}
              hint={form.ocr_provider === "ark" ? S.settings.ocrHintArk : S.settings.ocrHint}
              note={readerNote(ocrStatus, saveOcr.data?.requeued)}
              action={
                <>
                  <Button variant="secondary" size="sm"
                    onClick={() => {
                      startTest("ocr");
                    }}
                    disabled={tests.ocr.isPending || saveOcr.isPending || !!drafts.ocr}
                  >
                    {tests.ocr.isPending ? S.settings.testing : S.settings.test}
                  </Button>
                  <Button variant="secondary" size="sm"
                    onClick={() => saveCard("ocr")}
                    disabled={!canSave}
                  >
                    {saveOcr.isPending ? S.settings.saving : S.settings.save}
                  </Button>
                </>
              }
            >
              <div className="space-y-3">
                {/* Two protocols (0065): MinerU takes the job and then polls, Ark sends each page to a vision model. Switching to the other is switching to another provider;
                    the address, key and model do not carry over, so the whole card is cleared and refilled on switch; the server also will not carry
                    the old key over to the new provider */}
                <div>
                  <label className={label}>{S.settings.readerProvider}</label>
                  <Dropdown
                    className="w-full"
                    value={form.ocr_provider}
                    onChange={(v) => {
                      if (v === form.ocr_provider) return;
                      editCard("ocr", {
                        ocr_provider: v, ocr_base_url: "", ocr_api_key: "", ocr_backend: "", ocr_model: "",
                      });
                    }}
                    options={[
                      { value: "mineru", label: "MinerU" },
                      { value: "ark", label: S.settings.arkProvider },
                    ]}
                  />
                </div>
                <div>
                  <label className={label}>{S.settings.serviceUrl}</label>
                  <Input
                    className="w-full"
                    placeholder={form.ocr_provider === "ark" ? "https://ark.cn-beijing.volces.com/api/plan/v3" : "http://localhost:8000"}
                    value={form.ocr_base_url}
                    onChange={set("ocr", "ocr_base_url")}
                  />
                </div>
                <div className="grid grid-cols-2 gap-3">
                  {form.ocr_provider === "ark" ? (
                    <div>
                      <label className={label}>{S.settings.model}</label>
                      <Input
                        className="w-full"
                        placeholder="doubao-seed-2.1-pro"
                        value={form.ocr_model}
                        onChange={set("ocr", "ocr_model")}
                      />
                    </div>
                  ) : (
                    <div>
                      <label className={label}>{S.settings.backend}</label>
                      <Input
                        className="w-full"
                        placeholder="vlm-auto-engine"
                        value={form.ocr_backend}
                        onChange={set("ocr", "ocr_backend")}
                      />
                    </div>
                  )}
                  <div>
                    <label className={label}>
                      {S.settings.apiKey}{" "}
                      {settings.data?.has_ocr_key && settings.data.ocr_provider === form.ocr_provider && (
                        <span className="text-accent">{S.settings.keyConfigured}</span>
                      )}
                    </label>
                    <Input
                      className="w-full"
                      type="password"
                      value={form.ocr_api_key}
                      onChange={set("ocr", "ocr_api_key")}
                    />
                  </div>
                </div>
              </div>
            </SettingsCard>

            <SettingsCard
              title={S.settings.transcribeModel}
              hint={S.settings.transcribeHint}
              note={readerNote(transcribeStatus, saveTranscribe.data?.requeued)}
              action={
                <>
                  <Button variant="secondary" size="sm"
                    onClick={() => {
                      startTest("transcribe");
                    }}
                    disabled={tests.transcribe.isPending || saveTranscribe.isPending || !!drafts.transcribe}
                  >
                    {tests.transcribe.isPending ? S.settings.testing : S.settings.test}
                  </Button>
                  <Button variant="secondary" size="sm"
                    onClick={() => saveCard("transcribe")}
                    disabled={!canSave}
                  >
                    {saveTranscribe.isPending ? S.settings.saving : S.settings.save}
                  </Button>
                </>
              }
            >
              <div className="space-y-3">
                <div>
                  <label className={label}>{S.settings.baseUrl}</label>
                  <Input
                    className="w-full"
                    placeholder="https://api.openai.com/v1"
                    value={form.transcribe_base_url}
                    onChange={set("transcribe", "transcribe_base_url")}
                  />
                </div>
                <div className="grid grid-cols-2 gap-3">
                  <div>
                    <label className={label}>{S.settings.model}</label>
                    <Input
                      className="w-full"
                      placeholder="gpt-4o-transcribe-diarize"
                      value={form.transcribe_model}
                      onChange={set("transcribe", "transcribe_model")}
                    />
                  </div>
                  <div>
                    <label className={label}>
                      {S.settings.apiKey}{" "}
                      {settings.data?.has_transcribe_key && (
                        <span className="text-accent">{S.settings.keyConfigured}</span>
                      )}
                    </label>
                    <Input
                      className="w-full"
                      type="password"
                      placeholder="sk-…"
                      value={form.transcribe_api_key}
                      onChange={set("transcribe", "transcribe_api_key")}
                    />
                  </div>
                </div>
              </div>
            </SettingsCard>
          </div>
        )}
      </div>
    </div>
  );
}
