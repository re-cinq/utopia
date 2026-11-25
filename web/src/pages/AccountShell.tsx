/* The account-level shell: the host for Profile / Administration.
   Not related to a KB, so there is no KB switcher and no tab navigation: just the wordmark, back, and the user menu. */
import { useQuery } from "@tanstack/react-query";
import { Link, Outlet, useLocation, useNavigate } from "@tanstack/react-router";
import { usePageTitle } from "../useTitle";
import {
  KeyRound,
  Layers,
  ShieldCheck,
  UserRound,
} from "lucide-react";
import { api, ApiError } from "../api";
import { S } from "../i18n";
import {
  RAIL_CLS,
  rowClass,
  SectionMark,
} from "../ui";
import { ServerDown } from "./ServerDown";
import { HeaderActions } from "./HeaderActions";
import { ADMIN_TABS } from "./Settings";
import { fetchMe } from "../authSession";

export function AccountShell() {
  const navigate = useNavigate();
  const loc = useLocation();
  const onAdmin = loc.pathname === "/admin";
  const adminTab = (loc.search as { tab?: string }).tab ?? "models";
  const me = useQuery({ queryKey: ["me"], queryFn: fetchMe });
  const health = useQuery({ queryKey: ["health"], queryFn: api.health });
  // Title: `Utopia | Persona`, one name for the whole account area, not subdivided per page
  usePageTitle(S.app.name, S.account.titleTag);

  if (me.isPending) {
    return (
      <div className="min-h-screen flex items-center justify-center text-ink-2 text-body">
        {S.nav.loading}
      </div>
    );
  }
  if (me.isError) {
    if (me.error instanceof ApiError && me.error.status === 401) {
      navigate({ to: "/login" });
      return null;
    }
    return <ServerDown />;
  }

  const rail = rowClass(false, "nav");
  const railActive = rowClass(true, "nav");

  return (
    <div className="h-screen flex flex-col overflow-hidden u-arrive">
      {/* The top bar matches the Docs page: section wordmark (click to go back to the city) + back + GitHub·version + user */}
      {/* px-8 is the same inner padding as the App top bar: the group on the right must not move when the page changes */}
      <header className="glass-strong relative z-40 border-x-0 border-t-0 h-12 shrink-0 flex items-center px-2">
        <SectionMark className="pl-3" text={S.account.brand} title={S.docs.backTitle} />
        <HeaderActions
          link={{ to: "/", label: S.account.backToApp }}
          version={health.data?.version}
          user={me.data}
        />
      </header>

      <div className="flex-1 min-h-0 flex">
        {/* Account navigation bar (only two items; admins get one more) */}
        <aside className={`${RAIL_CLS} u-rail-list px-2 py-3`}>
          {/* exact: /account is a prefix of /account/kbs, so default prefix matching would light up both */}
          <Link
            to="/account"
            activeOptions={{ exact: true }}
            className={rail}
            activeProps={{ className: railActive }}
          >
            <UserRound size={14} />
            {S.account.profile}
          </Link>
          <Link to="/account/kbs" className={rail} activeProps={{ className: railActive }}>
            <Layers size={14} />
            {S.account.kbsNav}
          </Link>
          <Link to="/account/tokens" className={rail} activeProps={{ className: railActive }}>
            <KeyRound size={14} />
            {S.account.tokensNav}
          </Link>
          {me.data.is_admin && (
            <>
              {/* On the admin page the parent row is no longer highlighted: another row below is already lit, and two lit at once
                  would make it unclear where the person is */}
              <Link
                to="/admin"
                className={rail}
                activeProps={onAdmin ? {} : { className: railActive }}
              >
                <ShieldCheck size={14} />
                {S.account.administration}
              </Link>
              {onAdmin &&
                ADMIN_TABS.map(({ key, label }) => (
                  <Link
                    key={key}
                    to="/admin"
                    search={{ tab: key }}
                    className={rowClass(adminTab === key, "nav", undefined, true)}
                  >
                    {label()}
                  </Link>
                ))}
            </>
          )}
        </aside>
        <main className="flex-1 min-w-0 overflow-y-auto u-scroll">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
