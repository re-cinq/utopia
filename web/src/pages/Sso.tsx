/* Single sign-on (0056).

   **Binding is done by the person themselves**: on the account page, 'Link identity' goes to the identity provider, and on return the server checks that the person is still the one logged in before writing the binding. The admin page only lists who linked which subject, and can unlink; there is no entry for binding on someone else's behalf,
   because that would be a way to impersonate anyone at login.
   (see the module notes in crates/utopia-server/src/api/oidc_routes.rs). */
import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api";
import { S } from "../i18n";
import { toast } from "../toast";
import {
  Button,
  DangerConfirm,
  LinkButton,
  Table,
  TBody,
  Td,
  Th,
  THead,
  Tr,
} from "../ui";

/** Three rows of info, read-only: issuer / client_id / redirect_uri are set by environment variables and cannot be changed here.
 *  A two-column grid rather than one sentence joined together: the three values differ in length, so each right-hand column truncates more readably */
function ConfigSummary({
  issuer,
  clientId,
  redirectUri,
}: {
  issuer: string;
  clientId: string;
  redirectUri: string;
}) {
  const D = S.settings.sso;
  return (
    <div className="grid grid-cols-[max-content_1fr] gap-x-6 gap-y-1 text-fine text-ink-2">
      <span>{D.issuer}</span>
      <span className="truncate font-mono" title={issuer}>
        {issuer}
      </span>
      <span>{D.clientId}</span>
      <span className="truncate font-mono" title={clientId}>
        {clientId}
      </span>
      <span>{D.redirectUri}</span>
      <span className="truncate font-mono" title={redirectUri}>
        {redirectUri}
      </span>
    </div>
  );
}

export function SsoAdmin() {
  const queryClient = useQueryClient();
  const status = useQuery({ queryKey: ["oidc-status"], queryFn: api.oidcStatus });
  const identities = useQuery({
    queryKey: ["oidc-identities"],
    queryFn: api.oidcIdentities,
    enabled: !!status.data?.enabled,
  });
  const [confirming, setConfirming] = useState<{ userId: string; email: string } | null>(
    null,
  );
  const unlink = useMutation({
    mutationFn: (userId: string) => api.oidcUnlink(userId),
    onSuccess: () => {
      setConfirming(null);
      queryClient.invalidateQueries({ queryKey: ["oidc-identities"] });
    },
    onError: (e) => toast.error((e as Error).message),
  });

  if (status.isPending) return null;

  if (!status.data?.enabled) {
    return (
      <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
        {S.settings.sso.disabled}
      </div>
    );
  }

  const rows = identities.data?.identities ?? [];

  return (
    <div className="space-y-4">
      <p className="text-small text-ink-2">{S.settings.sso.hint}</p>

      {identities.data && (
        <ConfigSummary
          issuer={identities.data.issuer}
          clientId={identities.data.client_id}
          redirectUri={identities.data.redirect_uri}
        />
      )}

      {rows.length === 0 ? (
        <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
          {S.settings.sso.empty}
        </div>
      ) : (
        <div className="glass rounded-panel overflow-hidden">
          <Table>
            <THead>
              <Tr>
                <Th>{S.settings.sso.colUser}</Th>
                <Th>{S.settings.sso.colSubject}</Th>
                <Th />
              </Tr>
            </THead>
            <TBody>
              {rows.map((r) => (
                <Tr key={r.user_id}>
                  <Td className="text-ink">{r.email}</Td>
                  <Td
                    className="max-w-xs truncate font-mono text-small text-ink-2"
                    title={r.subject}
                  >
                    {r.subject}
                  </Td>
                  <Td className="whitespace-nowrap text-right">
                    <LinkButton
                      tone="danger"
                      onClick={() => setConfirming({ userId: r.user_id, email: r.email })}
                    >
                      {S.settings.sso.unlink}
                    </LinkButton>
                  </Td>
                </Tr>
              ))}
            </TBody>
          </Table>
        </div>
      )}

      {confirming && (
        <DangerConfirm
          title={S.settings.sso.unlinkTitle(confirming.email)}
          hint={S.settings.sso.unlinkHint}
          confirmLabel={S.settings.sso.unlink}
          cancelLabel={S.members.cancel}
          busy={unlink.isPending}
          onConfirm={() => unlink.mutate(confirming.userId)}
          onCancel={() => setConfirming(null)}
        />
      )}
    </div>
  );
}

/** A section on the account page: my own bindings. On deployments without SSO configured, the whole section does not appear */
export function SsoAccount() {
  const queryClient = useQueryClient();
  const me = useQuery({ queryKey: ["oidc-me"], queryFn: api.oidcMe });
  const [confirming, setConfirming] = useState(false);
  const unlink = useMutation({
    mutationFn: api.oidcUnlinkMe,
    onSuccess: () => {
      setConfirming(false);
      queryClient.invalidateQueries({ queryKey: ["oidc-me"] });
      toast.success(S.account.sso.unlinked);
    },
    onError: (e) => toast.error((e as Error).message),
  });

  // Returning from the identity provider carries a result (`?sso=linked` or `?sso_error=`): show the hint once, then clean the address,
  // so that a refresh does not show the hint again
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const linked = params.get("sso") === "linked";
    const error = params.get("sso_error");
    if (!linked && !error) return;
    if (linked) toast.success(S.account.sso.linked);
    if (error) toast.error(S.login.ssoErrors[error] ?? S.login.ssoErrorOther);
    params.delete("sso");
    params.delete("sso_error");
    const rest = params.toString();
    window.history.replaceState(null, "", window.location.pathname + (rest ? `?${rest}` : ""));
  }, []);

  if (!me.data?.enabled) return null;

  return (
    <div className="glass rounded-panel p-6 mt-4">
      <div className="max-w-xl">
        <h2 className="text-body font-medium text-ink mb-2">{S.account.sso.title}</h2>
        <p className="text-small text-ink-2 mb-4">{S.account.sso.hint}</p>
        <div className="flex items-center gap-3">
          <span className="min-w-0 flex-1 truncate text-body text-ink-2">
            {me.data.linked && me.data.subject
              ? S.account.sso.linkedAs(me.data.subject)
              : S.account.sso.notLinked}
          </span>
          {me.data.linked ? (
            <Button variant="secondary" size="sm" onClick={() => setConfirming(true)}>
              {S.account.sso.unlink}
            </Button>
          ) : (
            <Button variant="primary" size="sm"
              onClick={() => {
                window.location.href = "/api/v1/auth/oidc/start?link=1";
              }}
            >
              {S.account.sso.link}
            </Button>
          )}
        </div>
      </div>
      {confirming && (
        <DangerConfirm
          title={S.account.sso.unlinkTitle}
          hint={S.account.sso.unlinkHint}
          confirmLabel={S.account.sso.unlink}
          cancelLabel={S.account.cancel}
          busy={unlink.isPending}
          onConfirm={() => unlink.mutate()}
          onCancel={() => setConfirming(false)}
        />
      )}
    </div>
  );
}
