import { useEffect } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Link,
  Outlet,
  useNavigate,
  useRouterState,
} from "@tanstack/react-router";
import {
  Database,
  Library as LibraryIcon,
  ListChecks,
  MessagesSquare,
  Search as SearchIcon,
  Settings as SettingsIcon,
  Shapes,
  Waypoints,
} from "lucide-react";
import { api, ApiError } from "../api";
import { S } from "../i18n";
import { useKb, useKbId } from "../kb";
import { Wordmark } from "../ui";
import { KbSwitcher } from "./KbSwitcher";
import { HeaderActions } from "./HeaderActions";
import { ServerDown } from "./ServerDown";
import { useAlertEvents } from "../useAlertEvents";
import { useKbEvents } from "../useKbEvents";
import { usePageTitle } from "../useTitle";
import { fetchMe } from "../authSession";

const TABS = [
  // The graph is the front door, so it comes first; the two ways of querying (Search / Ask) follow
  { to: "/kb/$kbId/graph", label: S.nav.graph, Icon: Waypoints },
  { to: "/kb/$kbId/search", label: S.nav.search, Icon: SearchIcon },
  { to: "/kb/$kbId/chat", label: S.nav.ask, Icon: MessagesSquare },
  { to: "/kb/$kbId/library", label: S.nav.library, Icon: LibraryIcon },
  { to: "/kb/$kbId/review", label: S.review.title, Icon: ListChecks },
  { to: "/kb/$kbId/ontology", label: S.ontology.title, Icon: Shapes },
  // The ontology says "what exists in the world", data mapping says "how this number is computed in the library": they sit next to each other
  { to: "/kb/$kbId/mappings", label: S.mapping.title, Icon: Database },
  // Library settings, like the other tabs, are "the current knowledge base scope", and sit alongside the content navigation
  { to: "/kb/$kbId/settings", label: S.nav.settings, Icon: SettingsIcon },
] as const;

export function Shell() {
  const navigate = useNavigate();
  const kbId = useKbId();

  const me = useQuery({ queryKey: ["me"], queryFn: fetchMe });
  const health = useQuery({
    queryKey: ["health"],
    queryFn: api.health,
  });
  const { kb, kbs, setKb, settled } = useKb();
  // The title follows the current tab: `Graph · Utopia`; the document viewer page belongs to Library
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const tabLabel =
    TABS.find((t) => pathname.startsWith(t.to))?.label ??
    (pathname.startsWith("/doc/") ? S.nav.library : undefined);
  usePageTitle(S.app.name, tabLabel);
  // The one global KB event-stream connection: document and review state refresh in real time (in place of polling). Alerts also come from it
  useKbEvents(kb?.id);
  // Only open a separate alert stream when no knowledge base is open: a page takes up just one connection for notifications (#1028).
  // Wait until the list has come back before judging, otherwise every load connects first and then disconnects once the KB arrives
  useAlertEvents(settled && !kb);

  // Not logged in: go to the login page. **Side effects belong in an effect**; the reason is in the 401 branch below
  const unauthorized =
    me.isError && me.error instanceof ApiError && me.error.status === 401;
  useEffect(() => {
    if (unauthorized) navigate({ to: "/login" });
  }, [unauthorized, navigate]);

  if (me.isPending) {
    return (
      <div className="min-h-screen flex items-center justify-center text-ink-2 text-body">
        {S.nav.loading}
      </div>
    );
  }

  if (me.isError) {
    // **The navigation is done in an effect, not during render.** Calling `navigate` while rendering changes the router's state in the middle of rendering
    // another component, and React keeps a standing warning "Cannot update a component
    // while rendering a different component". It does not fail today, but it has the smell of "depends on render order": when the layout changes,
    // this is the most likely place for it to become a real bug
    if (me.error instanceof ApiError && me.error.status === 401) {
      return null;
    }
    return <ServerDown />;
  }

  return (
    <div className="h-screen flex flex-col overflow-hidden u-arrive">
      {/* Top bar: brand + workspace + user (Vercel style) */}
      {/* z-40: backdrop-filter makes the top bar and the tab strip each their own stacking context;
          without the elevation, the latter would cover the popover panels inside the top bar by DOM order */}
      {/* Left inner padding 32px: the left edge of the wordmark lines up with the icon of the first tab below (nav px-4 + the tab's left 16).
          The gap between wordmark and switcher is gap-6 (the switcher itself only has px-2): the switcher's icon lands exactly on the icon of the second
          tab (132; a coincidence under the English UI, which would need recalculating if the wordmark or tabs changed size),
          so both rows keep the same rhythm */}
      <header className="glass-strong relative z-40 border-x-0 border-t-0 h-12 shrink-0 flex items-center gap-6 px-2">
        {/* Wordmark: letters fade in one by one; on hover an ↗ rises; clicking goes to the website */}
        <Wordmark className="u-wordmark-top pl-3" />
        {/* The knowledge base switcher sits right after the wordmark, with no slash between them: it is not a second level of breadcrumb, it is
            "which KB am I in right now". Workspace has been folded out of the concept layer into a deployment-level invisible pipe
            (settings/members still go through it to the API, as organizations do for a single tenant).
            The icon on the left is the same as the "Knowledge bases" item in the left column of the account page, which says this string
            is a KB name; the size is medium, the same as the labels below; the arrow hugs the name and does not reach a fixed-width
            right edge.
            Pure switcher: creating a KB is an admin action, and its entry is under System settings › Knowledge bases */}
        <KbSwitcher kb={kb} kbs={kbs} onChange={setKb} />
        {/* The group of three on the right, shared by the top bars (HeaderActions): it must not move when the page changes */}
        <HeaderActions
          link={{ to: "/docs", label: S.nav.docs }}
          version={health.data?.version}
          user={me.data}
        />
      </header>

      {/* Tab navigation strip: icon + text, and the selected item is a filled pill.
          **One line down the left: the box starts at 8, the content starts at 20**, exactly as in the left column
          (column px-2 = 8, row px-3 = 12, so the icon lands at 20 and the text at 42).
          The wordmark in the top bar follows this line too: header pl-2 + wordmark pl-3 = 20.
          The three left edges are now one vertical line, so nothing looks misaligned going down.
          py-1 rather than py-2: the pill is already 32 high, and padding 8 more above and below would make the whole strip 48, pressing on the body below;
          padding 4 gives 40, which together with the top bar's 48 takes up not too much of the screen */}
      <nav className="glass-strong border-x-0 border-t-0 shrink-0 flex items-center gap-1 px-2 py-1">
        {TABS.map(({ to, label, Icon }) => (
          <Link
            key={to}
            to={to}
            params={{ kbId }}
            className="u-tab"
            activeProps={{ className: "u-tab is-active" }}
          >
            <Icon size={15} strokeWidth={1.8} />
            {label}
          </Link>
        ))}
      </nav>

      <main className="flex-1 min-h-0">
        <Outlet />
      </main>
    </div>
  );
}
