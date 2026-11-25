import { ExpressionDraftLab } from "./pages/ExpressionDraftLab";
import { docSearch } from "./quotes";
import {
  createRootRoute,
  createRoute,
  createRouter,
  redirect,
} from "@tanstack/react-router";
import { Account } from "./pages/Account";
import { AccountShell } from "./pages/AccountShell";
import { Tokens } from "./pages/Tokens";
import { Chat } from "./pages/Chat";
import { DocViewer } from "./pages/DocViewer";
import { DocsPage } from "./pages/Docs";
import { Graph } from "./pages/Graph";
import { Chain } from "./pages/Chain";
import { Library } from "./pages/Library";
import { Login } from "./pages/Login";
import { Privacy, Terms } from "./pages/Legal";
import { KbRedirect, KbScope } from "./pages/KbScope";
import { KbSettings } from "./pages/KbSettings";
import { MyKbs } from "./pages/MyKbs";
import { NotFound } from "./pages/ServerDown";
import { Ontology } from "./pages/Ontology";
import { Mappings } from "./pages/Mappings";
import { Review } from "./pages/Review";
import { Search } from "./pages/Search";
import { Settings } from "./pages/Settings";
import { Shell } from "./pages/Shell";

const rootRoute = createRootRoute();

const loginRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/login",
  component: Login,
});

// Public legal pages: reachable before login
const privacyRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/privacy",
  component: Privacy,
});

const termsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/terms",
  component: Terms,
});

const appRoute = createRoute({
  getParentRoute: () => rootRoute,
  id: "app",
  component: Shell,
});

const indexRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/",
  beforeLoad: () => {
    // Home page = the graph: the product's differentiating front door
    throw redirect({ to: "/graph" });
  },
});

/* Knowledge base scope. **The KB is a container, not a filter**: all the pages below belong to one KB,
   the path expresses the containment, and the router therefore catches errors like "forgot to include the KB" for us:
   `/kb/$kbId/search` cannot even be constructed without an id. The reasoning is in pages/KbScope.tsx */
const kbRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/kb/$kbId",
  component: KbScope,
});

const chatRoute = createRoute({
  getParentRoute: () => kbRoute,
  path: "chat",
  component: Chat,
});

// The session is the route: /chat/$conversationId carries only the URL (refresh and share return to the same conversation),
// while rendering is still the job of the parent Chat: the parent stays mounted between /chat and /chat/$id, so streaming is not interrupted
const chatConversationRoute = createRoute({
  getParentRoute: () => chatRoute,
  path: "$conversationId",
  component: () => null,
});

const searchRoute = createRoute({
  getParentRoute: () => kbRoute,
  path: "search",
  component: Search,
  // The search term is "what you are looking at", not "how you look at it": refresh, back and share all rebuild from it
  // (the same chunk for a doc, the same queue for review). Paging stays local, like the tier of the graph
  validateSearch: (search: Record<string, unknown>): { q?: string } => ({
    q: typeof search.q === "string" ? search.q : undefined,
  }),
});

const graphRoute = createRoute({
  getParentRoute: () => kbRoute,
  path: "graph",
  /* The shareable state of the graph page. **All three are "what you are looking at", not "how you are looking at it"**
     so the tier (how many to draw) is deliberately not in the URL: it is a local viewing preference, and a different machine should not follow it.

     - entity: which one is selected
     - focus: whether it is in an entity's neighbourhood (a different picture from "selected in the whole graph")
     - at: where the timeline is stopped. **This one matters most**: the product's selling point is
       "seeing the world at a moment in time", and a link without the moment loses the most interesting part */
  validateSearch: (
    search: Record<string, unknown>,
  ): { entity?: string; focus?: string; at?: string } => ({
    entity: typeof search.entity === "string" ? search.entity : undefined,
    focus: typeof search.focus === "string" ? search.focus : undefined,
    at: typeof search.at === "string" ? search.at : undefined,
  }),
  component: Graph,
});

const chainRoute = createRoute({
  getParentRoute: () => kbRoute,
  path: "chain",
  /* RC-008: the entity whose type's `chain_first` the page follows. Reached from the
     graph panel's "Open chain", not a tab of its own */
  validateSearch: (search: Record<string, unknown>): { entity?: string } => ({
    entity: typeof search.entity === "string" ? search.entity : undefined,
  }),
  component: Chain,
});

const docRoute = createRoute({
  getParentRoute: () => kbRoute,
  path: "doc/$docId",
  validateSearch: docSearch,
  component: DocViewer,
});

const libraryRoute = createRoute({
  getParentRoute: () => kbRoute,
  path: "library",
  validateSearch: (search: Record<string, unknown>): { src?: string } => ({
    src: typeof search.src === "string" ? search.src : undefined,
  }),
  component: Library,
});

// Unlisted opt-in exploration: authenticated reads, local drafts, no persistence.
const expressionDraftRoute = createRoute({
  getParentRoute: () => kbRoute,
  path: "expression-draft",
  component: ExpressionDraftLab,
});

const ontologyRoute = createRoute({
  getParentRoute: () => kbRoute,
  path: "ontology",
  component: Ontology,
});

const mappingsRoute = createRoute({
  getParentRoute: () => kbRoute,
  path: "mappings",
  component: Mappings,
});

const reviewRoute = createRoute({
  getParentRoute: () => kbRoute,
  path: "review",
  // Jumped to from the dispute chip on the entity panel: lands on the matching tier and lights up that card (0017 §3)
  validateSearch: (
    search: Record<string, unknown>,
  ): { queue?: string; item?: string } => ({
    queue: typeof search.queue === "string" ? search.queue : undefined,
    item: typeof search.item === "string" ? search.item : undefined,
  }),
  component: Review,
});

// Built-in docs: public route (readable before login; available offline on private deployments)
const docsIndexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/docs",
  beforeLoad: () => {
    throw redirect({ to: "/docs/$slug", params: { slug: "ingest" } });
  },
});

const docsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/docs/$slug",
  component: DocsPage,
});

// Account layer (Profile / Administration): unrelated to KBs, uses a separate shell with no tab navigation
const accountShellRoute = createRoute({
  getParentRoute: () => rootRoute,
  id: "account",
  component: AccountShell,
});

const accountRoute = createRoute({
  getParentRoute: () => accountShellRoute,
  path: "/account",
  component: Account,
});

const myKbsRoute = createRoute({
  getParentRoute: () => accountShellRoute,
  path: "/account/kbs",
  // The entry for creating a KB lives elsewhere (the last line of the top-bar KB switcher); with this parameter, landing opens the form directly
  validateSearch: (search: Record<string, unknown>): { create?: true } => ({
    create: search.create === true || search.create === "true" ? true : undefined,
  }),
  component: MyKbs,
});

// Personal tokens (0014): the key for agents belongs to a person, so it sits in the account layer, not inside a KB
const tokensRoute = createRoute({
  getParentRoute: () => accountShellRoute,
  path: "/account/tokens",
  component: Tokens,
});

const adminRoute = createRoute({
  getParentRoute: () => accountShellRoute,
  path: "/admin",
  // A deep link names the tab to open (e.g. "register a new connection" in the KB data section goes straight to Data sources)
  validateSearch: (
    search: Record<string, unknown>,
  ): { tab?: "models" | "members" | "datasources" | "sso" | "deployment" } => ({
    tab:
      search.tab === "models" ||
      search.tab === "members" ||
      search.tab === "datasources" ||
      search.tab === "sso" ||
      search.tab === "deployment"
        ? search.tab
        : undefined,
  }),
  component: Settings,
});

// Old path compatibility: /settings → /admin
const settingsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/settings",
  beforeLoad: () => {
    throw redirect({ to: "/admin" });
  },
});

const kbSettingsRoute = createRoute({
  getParentRoute: () => kbRoute,
  path: "settings",
  component: KbSettings,
});

/* Old path compatibility: addresses without a KB, such as `/graph`, still work: work out which KB to go to, then jump there.
   **Not made into a beforeLoad redirect**: at that point the KB list has not been fetched, and localStorage
   may also hold nothing (a new device, or the cache was cleared), so it must wait for useKb to resolve it */
/* **Written out one by one, no factory function**: a factory's path is a string, which the type system cannot tell apart
   from a literal, so `redirect({ to: "/graph" })` elsewhere would fail to type-check. The verbosity buys type safety */
const legacyGraphRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/graph",
  component: () => <KbRedirect page="graph" />,
});
const legacySearchRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/search",
  component: () => <KbRedirect page="search" />,
});
const legacyChatRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/chat",
  component: () => <KbRedirect page="chat" />,
});
const legacyLibraryRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/library",
  component: () => <KbRedirect page="library" />,
});
const legacyOntologyRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/ontology",
  component: () => <KbRedirect page="ontology" />,
});
const legacyMappingsRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/mappings",
  component: () => <KbRedirect page="mappings" />,
});
const legacyReviewRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/review",
  component: () => <KbRedirect page="review" />,
});
const legacyKbSettingsRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/kb-settings",
  component: () => <KbRedirect page="settings" />,
});

const routeTree = rootRoute.addChildren([
  loginRoute,
  privacyRoute,
  termsRoute,
  settingsRoute,
  docsIndexRoute,
  docsRoute,
  accountShellRoute.addChildren([accountRoute, myKbsRoute, tokensRoute, adminRoute]),
  appRoute.addChildren([
    indexRoute,
    legacyGraphRoute,
    legacySearchRoute,
    legacyChatRoute,
    legacyLibraryRoute,
    legacyOntologyRoute,
    legacyMappingsRoute,
    legacyReviewRoute,
    legacyKbSettingsRoute,
    kbRoute.addChildren([
      chatRoute.addChildren([chatConversationRoute]),
      searchRoute,
      graphRoute,
      chainRoute,
      docRoute,
      libraryRoute,
      reviewRoute,
      ontologyRoute,
      expressionDraftRoute,
      mappingsRoute,
      kbSettingsRoute,
    ]),
  ]),
]);

export const router = createRouter({
  routeTree,
  // The lost city: the punishment page for unknown paths
  defaultNotFoundComponent: NotFound,
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
