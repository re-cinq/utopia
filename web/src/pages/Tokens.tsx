/* Personal access token page (docs/decisions/0014, A2 of 0016).
   Tokens belong to a person and act as that person: effective permissions = role ∩ scope, and `kb_ids` only narrows, never grants.
   The server's three endpoints already exist (issue / list / revoke); what was missing was this page. Without it,
   MCP for a user was "there is an API, but no way to configure it".
   The plaintext appears only once, in the response that issues the token, so the centre of gravity of this page is that moment: the token is shown together with a copyable
   client config, and once the person has copied it and clicked "Done", the list keeps only the prefix. */
import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, Copy, KeyRound, Plus } from "lucide-react";
import { api, type TokenView } from "../api";
import { S } from "../i18n";
import { useKb } from "../kb";
import {
  Button,
  Chip,
  Dialog,
  Dropdown,
  Field,
  Input,
  Loading,
  MultiSearchSelect,
  PageHeader,
  RadioGroup,
  Table,
  TBody,
  Td,
  Th,
  THead,
  Tr,
  buttonLike,
  localDate,
} from "../ui";
import { toast } from "../toast";
import { copyAndSay } from "../clipboard";
import { kbOptions } from "./kbGroups";

const EXPIRY_CHOICES = [30, 90, 365, 0] as const;

/** The Streamable HTTP form used by the Claude Code / Claude Desktop family. One endpoint per base (0014:
 *  tokens are scoped to a base, and endpoints are per base too), so the snippet has to select the base */
function mcpSnippet(kbId: string, kbName: string, token: string): string {
  const slug = kbName
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "") || "kb";
  const cfg = {
    mcpServers: {
      [`utopia-${slug}`]: {
        type: "http",
        url: `${window.location.origin}/api/v1/kbs/${kbId}/mcp`,
        headers: { Authorization: `Bearer ${token}` },
      },
    },
  };
  return JSON.stringify(cfg, null, 2);
}

function CopyButton({ text, small }: { text: string; small?: boolean }) {
  const [done, setDone] = useState(false);
  return (
    <Button variant="secondary" size="sm"
      className={buttonLike("ghost", small ? "sm" : "md")}
      onClick={async () => {
        // Only switches to "copied" once the copy has succeeded. Previously the label changed first and then copying was attempted; on a page opened over http nothing was
        // copied, yet the button still said copied
        if (!(await copyAndSay(text, S.account.copied))) return;
        setDone(true);
        setTimeout(() => setDone(false), 1500);
      }}
    >
      {done ? <Check size={12} /> : <Copy size={12} />}
      {done ? S.account.copied : S.account.copy}
    </Button>
  );
}

/** The one just issued: plaintext + config snippet; once closed it can never be seen again.
    It lives in the dialog that issues the token: the second after issuing is "copy it away", and there should be no window close in between */
export function IssuedBody({
  token,
  info,
  kbs,
  preferredKbId,
}: {
  token: string;
  info: TokenView;
  kbs: { id: string; name: string }[];
  preferredKbId?: string;
}) {
  // The account page still remembers the base last opened; pick only from the bases the token allows, so it does not default to connecting to an empty General.
  const candidates = info.kb_ids?.length
    ? kbs.filter((k) => info.kb_ids!.includes(k.id))
    : kbs;
  const [pickedKbId, setPickedKbId] = useState<string>();
  const kb = candidates.find((k) => k.id === pickedKbId)
    ?? candidates.find((k) => k.id === preferredKbId)
    ?? candidates[0];
  const kbId = kb?.id ?? "";
  const snippet = kb ? mcpSnippet(kb.id, kb.name, token) : "";
  return (
    <div>
      <div className="flex items-center gap-2">
        <code className="min-w-0 flex-1 truncate rounded-cell bg-well px-3 py-2 font-mono text-small text-ink">
          {token}
        </code>
        <CopyButton text={token} />
      </div>

      <div className="mt-6 flex items-baseline justify-between gap-3 flex-wrap">
        <div className="text-body font-medium text-ink">{S.account.mcpTitle}</div>
        {candidates.length > 1 && (
          <span className="flex items-center gap-2 text-small text-ink-2">
            {S.account.mcpBase}
            <Dropdown
              size="sm"
              className="w-40"
              value={kbId}
              onChange={setPickedKbId}
              options={candidates.map((k) => ({ value: k.id, label: k.name }))}
            />
          </span>
        )}
      </div>
      <p className="mt-1 text-small text-ink-2">{S.account.mcpHint}</p>
      <div className="mt-2 relative">
        <pre className="rounded-panel bg-well px-3 py-2 font-mono text-small text-ink-2 overflow-x-auto u-scroll">
          {snippet}
        </pre>
        <div className="absolute top-1.5 right-1.5">
          <CopyButton text={snippet} small />
        </div>
      </div>
    </div>
  );
}

function TokenRow({
  t,
  kbName,
  busy,
  onRevoke,
}: {
  t: TokenView;
  kbName: (id: string) => string;
  busy: boolean;
  onRevoke: () => void;
}) {
  // Revoking cannot be undone, but the cost is only reissuing a token: light confirmation (a second click), no typing to unlock
  const [arm, setArm] = useState(false);
  const revoked = !!t.revoked_at;
  const bases = t.kb_ids?.length
    ? t.kb_ids.map(kbName).join(" · ")
    : S.account.allBases;
  return (
    <Tr className={revoked ? "opacity-55" : undefined}>
      <Td>
        <div className="flex items-center gap-2">
          <KeyRound size={13} className="shrink-0 text-ink-2" />
          <span className="truncate text-body text-ink">{t.name}</span>
          <code className="shrink-0 font-mono text-fine text-ink-2">
            {t.token_prefix}…
          </code>
        </div>
      </Td>
      <Td>
        <Chip tone={t.scope === "write" ? "warn" : "neutral"}>
          {t.scope === "write" ? S.account.scopeWrite : S.account.scopeRead}
        </Chip>
      </Td>
      <Td className="text-small text-ink-2" title={bases}>
        {t.kb_ids?.length ? S.account.nBases(t.kb_ids.length) : S.account.allBases}
      </Td>
      <Td className="u-num text-small text-ink-2">
        {t.last_used_at ? localDate(t.last_used_at) : S.account.neverUsed}
      </Td>
      <Td className="u-num text-small text-ink-2">
        {t.expires_at ? localDate(t.expires_at) : S.account.noExpiry}
      </Td>
      <Td className="u-num text-small text-ink-2">{localDate(t.created_at)}</Td>
      <Td className="text-right">
        {revoked ? (
          <Chip tone="danger">{S.account.revokedOn(localDate(t.revoked_at!))}</Chip>
        ) : arm ? (
          <Button variant="danger" size="sm"
            disabled={busy}
            onClick={onRevoke}
            onBlur={() => setArm(false)}
          >
            {S.account.revokeConfirm}
          </Button>
        ) : (
          <Button variant="secondary" size="sm"
            disabled={busy}
            onClick={() => setArm(true)}
          >
            {S.account.revoke}
          </Button>
        )}
      </Td>
    </Tr>
  );
}

export function Tokens() {
  const queryClient = useQueryClient();
  const { kb, kbs } = useKb();
  const list = useQuery({ queryKey: ["tokens"], queryFn: api.tokens });

  // Issuing a token is an action, not this page's permanent content: the form lives in the dialog, and the page only holds the list
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [scope, setScope] = useState<"read" | "write">("read");
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [days, setDays] = useState<number>(90);
  const [issued, setIssued] = useState<{ token: string; info: TokenView } | null>(null);

  const issue = useMutation({
    mutationFn: () =>
      api.issueToken({
        name: name.trim(),
        scope,
        kb_ids: picked.size ? Array.from(picked) : null,
        expires_in_days: days,
      }),
    onSuccess: (res) => {
      // The same window switches content: right after issuing it shows "copy it away", and the plaintext is only shown this once
      setIssued(res);
      setName("");
      setPicked(new Set());
      queryClient.invalidateQueries({ queryKey: ["tokens"] });
    },
    onError: (e) => toast.error((e as Error).message),
  });
  const revoke = useMutation({
    mutationFn: (id: string) => api.revokeToken(id),
    onSettled: () => queryClient.invalidateQueries({ queryKey: ["tokens"] }),
    onError: (e) => toast.error((e as Error).message),
  });

  const kbName = useMemo(() => {
    const m = new Map(kbs.map((k) => [k.id, k.name]));
    return (id: string) => m.get(id) ?? id.slice(0, 8);
  }, [kbs]);

  if (list.isPending) return <Loading>{S.nav.loading}</Loading>;
  // Live ones first, newest to oldest; revoked ones sink to the bottom but stay listed: the fact that something was revoked must itself stay visible
  const rows = [...(list.data?.tokens ?? [])].sort((a, b) => {
    if (!!a.revoked_at !== !!b.revoked_at) return a.revoked_at ? 1 : -1;
    return b.created_at.localeCompare(a.created_at);
  });

  const close = () => {
    setCreating(false);
    setIssued(null);
  };

  return (
    <div className="px-8 py-6">
      {/* Same build as the admin pages: content centred with a width limit, so line length does not stretch with the window */}
      <div className="mx-auto w-full max-w-4xl">
        <PageHeader
          title={S.account.tokensTitle}
          sub={S.account.tokensHint}
          actions={
            <Button variant="primary" size="sm" onClick={() => setCreating(true)}>
              <Plus size={12} />
              {S.account.newToken}
            </Button>
          }
        />

        {rows.length === 0 ? (
          <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
            {S.account.noTokens}
          </div>
        ) : (
          <div className="glass rounded-panel overflow-hidden">
            <Table>
              <THead>
                <Tr>
                  <Th>{S.account.tokenName}</Th>
                  <Th>{S.account.tokenScope}</Th>
                  <Th>{S.account.tokenKbs}</Th>
                  <Th>{S.account.colLastUsed}</Th>
                  <Th>{S.account.tokenExpires}</Th>
                  <Th>{S.account.colCreated}</Th>
                  <Th />
                </Tr>
              </THead>
              <TBody>
                {rows.map((t) => (
                  <TokenRow
                    key={t.id}
                    t={t}
                    kbName={kbName}
                    busy={revoke.isPending && revoke.variables === t.id}
                    onRevoke={() => revoke.mutate(t.id)}
                  />
                ))}
              </TBody>
            </Table>
          </div>
        )}

        {/* One window with two contents: first the form, and after issuing, the plaintext token. The window does not close in between:
            close it and you can never see it again */}
        <Dialog
          open={creating || !!issued}
          onOpenChange={(o) => !o && close()}
          width={issued ? "lg" : "md"}
          closeLabel={S.ui.close}
          title={issued ? S.account.issuedTitle : S.account.newToken}
          description={issued ? S.account.issuedHint : undefined}
          footer={
            issued ? (
              <Button variant="primary" size="sm" onClick={close}>
                {S.account.tokenDone}
              </Button>
            ) : (
              <>
                <Button variant="secondary" size="sm" onClick={close}>
                  {S.account.cancel}
                </Button>
                <Button variant="primary" size="sm"
                  disabled={!name.trim() || issue.isPending}
                  onClick={() => issue.mutate()}
                >
                  {S.account.issueToken}
                </Button>
              </>
            )
          }
        >
          {issued ? (
            <IssuedBody token={issued.token} info={issued.info} kbs={kbs} preferredKbId={kb?.id} />
          ) : (
            <div>
              <Field label={S.account.tokenName}>
                <Input className="w-full"
                  autoFocus
                  placeholder={S.account.tokenNamePlaceholder}
                  value={name}
                  maxLength={64}
                  onChange={(e) => setName(e.target.value)}
                />
              </Field>
              {/* Scope and expiry side by side: both are small choices settled at a glance,
                  and giving each a full row would only stretch the table */}
              <div className="grid grid-cols-2 gap-4">
                {/* Pick one of two, with both options readable: radio buttons, not a row of buttons */}
                <Field label={S.account.tokenScope} hint={S.account.scopeHint}>
                  <RadioGroup
                    name="token-scope"
                    className="flex h-8 flex-row items-center gap-4"
                    value={scope}
                    onChange={(v) => setScope(v)}
                    options={[
                      { value: "read" as const, label: S.account.scopeRead },
                      { value: "write" as const, label: S.account.scopeWrite },
                    ]}
                  />
                </Field>
                <Field label={S.account.tokenExpires}>
                  <Dropdown
                    className="w-full"
                    value={String(days)}
                    onChange={(v) => setDays(Number(v))}
                    options={EXPIRY_CHOICES.map((d) => ({
                      value: String(d),
                      label: d === 0 ? S.account.expiresNever : S.account.expiresDays(d),
                    }))}
                  />
                </Field>
              </div>
              {/* There may be many bases: search and select, with the selected ones stacked above the box. The height of a chip wall grows with the number of bases,
                  this does not. **No separate sentence "not selecting means all"**: the control itself already says "All bases",
                  and saying the same thing twice only stretches the form */}
              <Field label={S.account.tokenKbs} className="mb-0">
                <MultiSearchSelect
                  className="w-full"
                  values={Array.from(picked)}
                  options={kbOptions(kbs)}
                  placeholder={S.account.pickBases}
                  emptyHint={S.account.allBases}
                  onToggle={(id) => {
                    const next = new Set(picked);
                    if (next.has(id)) next.delete(id);
                    else next.add(id);
                    setPicked(next);
                  }}
                />
              </Field>
            </div>
          )}
        </Dialog>
      </div>
    </div>
  );
}
