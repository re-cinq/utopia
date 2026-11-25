/* Account-level "My knowledge libraries": accessible libraries + my role + join info + overview stats, **and creating libraries**.
   Previously the create entry was inside Administration, and that section is visible only to system administrators; creating a library needs
   workspace Admin+, so a workspace admin had no way to create a library in the interface at all. Both places listed the same data
   (both read `my-kbs`), so they are merged into this page. */
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { Lock, Plus, Search } from "lucide-react";
import { api, type MyKb } from "../api";
import { S, lang } from "../i18n";
import { useKb } from "../kb";
import { kbGroups } from "./kbGroups";
import {
  Button,
  Checkbox,
  Chip,
  Dialog,
  Field,
  Input,
  Loading,
  MultiSearchSelect,
  PageHeader,
  TBody,
  THead,
  Table,
  Td,
  Th,
  Tr,
  localDate,
} from "../ui";
import { fetchMe } from "../authSession";

function joinInfo(row: MyKb): string {
  if (row.my_role === "owner") return S.account.deploymentAdmin;
  if (row.joined_at) {
    return row.added_by_name
      ? S.account.addedBy(row.added_by_name, localDate(row.joined_at))
      : S.account.joinedOn(localDate(row.joined_at));
  }
  return S.account.openToEveryone;
}

export function MyKbs() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { workspace, setKb } = useKb();
  // The library switcher's last row arrives with ?create=true: open the form on landing
  const { create: deepLink } = useSearch({ from: "/account/account/kbs" });
  const [creating, setCreating] = useState(!!deepLink);
  const [filter, setFilter] = useState("");

  const mine = useQuery({
    queryKey: ["myKbs", workspace?.id],
    queryFn: () => api.myKbs(workspace!.id),
    enabled: !!workspace,
  });
  const me = useQuery({ queryKey: ["me"], queryFn: fetchMe });
  /* Who can create libraries: a system administrator, or Admin+ in this workspace (that is how api/kbs.rs create judges it).
     **Editors cannot**: they manage the contents of a library, not how many libraries exist */
  const wsRole = useQuery({
    queryKey: ["workspaceRole", workspace?.id],
    queryFn: () => api.workspaceRole(workspace!.id),
    enabled: !!workspace,
  });
  const canCreate =
    !!me.data?.is_admin ||
    wsRole.data?.role === "admin" ||
    wsRole.data?.role === "owner";

  if (!workspace || mine.isPending) return <Loading>{S.nav.loading}</Loading>;

  const q = filter.trim().toLowerCase();
  const rows = (mine.data?.kbs ?? []).filter(
    (r) =>
      !q ||
      r.kb.name.toLowerCase().includes(q) ||
      (r.kb.description ?? "").toLowerCase().includes(q),
  );
  // Without a filter, group by `owner/`: the group name takes one line, the short name below it (#1249); with a filter, list full names flat
  const rowById = new Map(rows.map((r) => [r.kb.id, r]));
  const lines: ({ heading: string } | { row: (typeof rows)[number]; label: string })[] = q
    ? rows.map((row) => ({ row, label: row.kb.name }))
    : kbGroups(rows.map((r) => r.kb)).flatMap((entry) =>
        entry.kind === "kb"
          ? [{ row: rowById.get(entry.kb.id)!, label: entry.kb.name }]
          : [
              { heading: entry.name },
              ...entry.items.map((item) => ({ row: rowById.get(item.kb.id)!, label: item.label })),
            ],
      );
  const openKb = (id: string) => {
    setKb(id);
    navigate({ to: "/kb/$kbId/graph", params: { kbId: id } });
  };

  return (
    <div className="px-8 py-6">
      {/* Same layout as the admin page: content centered with a width limit, line length does not stretch with the window */}
      <div className="mx-auto w-full max-w-4xl">
        <PageHeader
          title={S.account.kbsTitle}
          actions={
            canCreate && (
              <Button variant="primary" size="sm" onClick={() => setCreating(true)}>
                <Plus size={12} />
                {S.settings.kbs.newKb}
              </Button>
            )
          }
        />

        {/* What filters this list sits outside the panel (DESIGN.md 6) */}
        <div className="mb-4">
          <Input
            icon={<Search size={13} />}
            /* The search box on the filter bar has one width across the site: w-64 (members, rules and data scope all use it) */
            className="w-64"
            placeholder={S.account.kbsFilter}
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
        </div>

        {/* **A table, not a column of cards.** This page is a list of records: each library has the same fields,
            people come here to compare across: which has more documents, where I am admin, which is restricted.
            Previously these were piled into 'name + three pills' with a line of small text joined by dots; the same fact
            landed at different horizontal positions on different rows and could not be compared; and every other list on the site
            (members, tokens, library, rules, ontology) already used Table / Th / Td, and only this page was hand-written.
            The identity column **is one word, not a filled pill**: it sits in its own column,
            and the column header already says what it is, so there is no need to color it again. */}
        <div className="glass rounded-panel overflow-hidden">
          <Table>
            <THead>
              <Tr>
                <Th>{S.account.kbNameLabel}</Th>
                <Th>{S.account.kbRoleLabel}</Th>
                <Th className="text-right">{S.account.kbDocsLabel}</Th>
                <Th className="text-right">{S.account.kbMembersLabel}</Th>
                <Th>{S.account.kbAccessLabel}</Th>
                <Th />
              </Tr>
            </THead>
            <TBody>
              {lines.map((line) => {
                if ("heading" in line)
                  return (
                    <Tr key={`group-${line.heading}`}>
                      <th
                        scope="rowgroup"
                        colSpan={6}
                        className="px-3 pt-4 pb-2 text-left align-top text-small font-medium text-ink-2"
                      >
                        {line.heading}
                      </th>
                    </Tr>
                  );
                const { row, label } = line;
                const canManage =
                  row.my_role === "admin" || row.my_role === "owner";
                return (
                  <Tr key={row.kb.id}>
                    <Td>
                      <div className="flex items-center gap-2">
                        <span className="truncate text-body text-ink" title={row.kb.name}>
                          {label}
                        </span>
                        {/* These two are **properties of the library itself**, not a column of data: there is only one default library,
                            and few are restricted, so a column would sit mostly empty; pinned next to the name instead */}
                        {row.kb.is_default && (
                          <Chip tone="neutral">{S.settings.kbs.defaultChip}</Chip>
                        )}
                        {row.kb.visibility === "restricted" && (
                          <span
                            className="flex shrink-0 items-center gap-1 text-fine text-ink-2"
                            title={S.account.kbRestricted}
                          >
                            <Lock size={10} />
                            {S.account.kbRestricted}
                          </span>
                        )}
                      </div>
                    </Td>
                    <Td className="text-ink-2">
                      {row.my_role
                        ? (S.account.roleNames[row.my_role] ?? row.my_role)
                        : "—"}
                    </Td>
                    <Td className="u-num text-right text-ink-2">{row.doc_count}</Td>
                    <Td className="u-num text-right text-ink-2">
                      {row.member_count}
                    </Td>
                    <Td className="text-small text-ink-2">{joinInfo(row)}</Td>
                    {/* Settings first, open after: not every row has settings (admin required), so the one that is always present goes on the right end,
                        so the right edge of the button column is consistent row to row */}
                    <Td className="text-right">
                      <div className="flex items-center justify-end gap-2">
                        {canManage && (
                          <Button variant="secondary" size="sm"
                            onClick={() => {
                              setKb(row.kb.id);
                              navigate({
                                to: "/kb/$kbId/settings",
                                params: { kbId: row.kb.id },
                              });
                            }}
                          >
                            {S.account.kbSettingsBtn}
                          </Button>
                        )}
                        <Button variant="secondary" size="sm"
                          onClick={() => openKb(row.kb.id)}
                        >
                          {S.account.openKb}
                        </Button>
                      </div>
                    </Td>
                  </Tr>
                );
              })}
            </TBody>
          </Table>
          {rows.length === 0 && (
            <p className="px-4 py-6 text-body text-ink-2">{S.ui.noMatches}</p>
          )}
        </div>

        {creating && (
          <NewKbModal
            workspaceId={workspace.id}
            onDone={(id) => {
              setCreating(false);
              queryClient.invalidateQueries({ queryKey: ["myKbs", workspace.id] });
              queryClient.invalidateQueries({ queryKey: ["kbs", workspace.id] });
              // After creating, go straight to library settings: the next step is almost always inviting people or configuring
              if (id) {
                setKb(id);
                navigate({ to: "/kb/$kbId/settings", params: { kbId: id } });
              }
            }}
          />
        )}
      </div>
    </div>
  );
}

/** New knowledge library dialog: restricted by default, so it does not pollute the all-member switcher.
    **Creating a library is not exclusive to system administrators**: a workspace Admin can create one (see create in api/kbs.rs),
    createso it lives on this page that everyone visits, not in Administration */
function NewKbModal({
  workspaceId,
  onDone,
}: {
  workspaceId: string;
  onDone: (id?: string) => void;
}) {
  const [name, setName] = useState("");
  const [desc, setDesc] = useState("");
  const [restricted, setRestricted] = useState(true);
  // Nothing is ticked by default (#580). It used to pre-tick schema.org, on the reasoning that it is 'a starting point that already recognizes people,
  // organizations and products'; but after measuring, the ontology that grows out of the documents is the library's own dozen-odd classes, and a package
  // of 915 classes raised each extraction prompt from 2k to 18k tokens. Those who want a package tick it here
  const [packs, setPacks] = useState<string[]>([]);
  // Scenario packs (0073): classes, attributes and competency questions preset for business scenarios, installed after the standard packs
  const [scenarios, setScenarios] = useState<string[]>([]);

  const available = useQuery({
    queryKey: ["ontologyPacks"],
    queryFn: api.ontologyPacks,
  });
  const scenarioPacks = useQuery({
    queryKey: ["scenarioPacks"],
    queryFn: api.scenarioPacks,
  });

  // Tick order is install order: the first package's classes claim same-named seed classes,
  // and later name collisions are only found via the alignment table. So unticking and re-ticking moves it to the end: that is correct
  const toggle = (id: string) =>
    setPacks((prev) =>
      prev.includes(id) ? prev.filter((p) => p !== id) : [...prev, id],
    );

  const create = useMutation({
    mutationFn: () =>
      api.createKb(workspaceId, {
        name: name.trim(),
        description: desc.trim() || null,
        visibility: restricted ? "restricted" : "open",
        ontology_packs: packs,
        scenario_packs: scenarios,
      }),
    onSuccess: (kb) => onDone(kb.id),
  });

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onDone()}
      title={S.settings.kbs.newKb}
      closeLabel={S.ui.close}
      width="sm"
      footer={
        <>
          <Button variant="secondary" size="sm" onClick={() => onDone()}>
            {S.library.cancel}
          </Button>
          <Button
            variant="primary"
            size="sm"
            disabled={!name.trim() || create.isPending}
            onClick={() => create.mutate()}
          >
            {S.settings.kbs.create}
          </Button>
        </>
      }
    >
      <div>
        <Input className="w-full mb-2"
          autoFocus
          placeholder={S.settings.kbs.name}
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
        <Input className="w-full mb-3"
          placeholder={S.settings.kbs.description}
          value={desc}
          onChange={(e) => setDesc(e.target.value)}
        />
        <Checkbox
          className="mb-4"
          checked={restricted}
          onChange={(v) => setRestricted(v)}
          label={S.settings.kbs.visRestricted}
        />

        {/* Packs are optional, and **usually only one is installed**: five spread-out cards take up most of this dialog,
            so search-and-select instead; the secondary text for descriptions and scale moves into the dropdown, with the selected ones stacked above the box */}
        <Field label={S.settings.kbs.packsLabel} hint={S.settings.kbs.packsHint}>
          <MultiSearchSelect
            className="w-full"
            values={packs}
            options={(available.data?.packs ?? []).map((p) => ({
              value: p.id,
              label: p.name,
              hint: `${p.summary} · ${S.settings.kbs.packsCount(p.classes, p.properties)}`,
            }))}
            placeholder={S.settings.kbs.packsPick}
            emptyHint={S.settings.kbs.packsNone}
            onToggle={toggle}
          />
        </Field>
        <Field label={S.settings.kbs.scenariosLabel} hint={S.settings.kbs.scenariosHint}>
          <MultiSearchSelect
            className="w-full"
            values={scenarios}
            options={(scenarioPacks.data?.packs ?? []).map((p) => ({
              value: p.id,
              label: p.name[lang],
              hint: `${p.summary[lang]} · ${S.settings.kbs.scenariosCount(p.classes, p.properties, p.questions)}`,
            }))}
            placeholder={S.settings.kbs.scenariosPick}
            emptyHint={S.settings.kbs.scenariosNone}
            onToggle={(id) =>
              setScenarios((prev) =>
                prev.includes(id) ? prev.filter((p) => p !== id) : [...prev, id],
              )
            }
          />
        </Field>
        {create.isError && (
          <p className="text-small text-danger mb-2">
            {(create.error as Error).message}
          </p>
        )}
      </div>
    </Dialog>
  );
}

