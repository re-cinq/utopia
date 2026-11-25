import type { ReactNode } from "react";
import { fmtObjectValue } from "../objectValue";
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MutableRefObject,
} from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import Graphology from "graphology";
import { circular, circlepack } from "graphology-layout";
import forceAtlas2 from "graphology-layout-forceatlas2";
import FA2Layout from "graphology-layout-forceatlas2/worker";
import Sigma from "sigma";
import EdgeCurveProgram from "@sigma/edge-curve";
import { EdgeRectangleProgram } from "sigma/rendering";
import { onThemeChange } from "../theme";
import {
  EDGE,
  EDGE_CONTEST,
  EDGE_DERIVED,
  EDGE_DERIVED_DIM,
  EDGE_DIM,
  EDGE_FOCUS,
  EDGE_FOCUS_CONTEST,
  EDGE_FOCUS_DERIVED,
  EDGE_INFERRED,
  HOVER_MUTE,
  MUTED_SHELL,
  NODE_BORDER_BASE,
  NODE_CORE_BASE,
  NODE_CORE_MIX,
  NODE_SHELL_BASE,
  NODE_TINT_MIX,
  SCRUB_FUTURE,
  SCRUB_PAST,
  SCRUB_PLAY,
  TRANSPARENT,
  drawWorldGrid,
  lerpColor,
  mix,
  refreshPalette,
} from "./graphVisuals";
// The canvas machine is shared by both pages (#496): construction options, state table, camera, and dragging all live there;
// this file only projects instances and facts into one graph and explains what colour each node is.
import {
  attachDrag,
  deferToHoverLayer,
  hoveredNode,
  mutedNode,
  neighborNode,
  NODE_TYPE_SHELL,
  NODE_TYPE_SQUARE,
  selectedNode,
  sigmaOptions,
  withTopLayer,
  drawLast,
  softMutedNode,
} from "./graphCanvas";
import { EntityHistory } from "./EntityHistory";
import { HintedPanel } from "./hinted/HintedPanel";
import { hasHints, hintsOf } from "./hinted/hints";
import { EntityDialog, FactTimeDialog } from "./graphDialogs";
import { fmtTime } from "../time";
import { NextStep, nextStep, useReadiness } from "./NextStep";
import {
  ArrowLeft,
  ArrowRight,
  ChevronRight,
  CircleDashed,
  ExternalLink,
  Grape,
  Loader2,
  Maximize2,
  Orbit,
  Pause,
  Pencil,
  Play,
  Search,
  Tag,
  Waypoints,
  X,
  ZoomIn,
  ZoomOut,
} from "lucide-react";
import {
  api,
  type DerivedFact,
  type EntityFact,
  type Evidence,
  type GraphEdge,
  type GraphNode,
  type NameView,
  type BlockedDerivation,
  type ProofStep,
} from "../api";
import { originHint, originLabel } from "../origin";
import { S } from "../i18n";
import { predicateSentence } from "../predicateText";
import {
  Button,
  CanvasLoading,
  ExpandCard,
  HOVER_ROW,
  POINT_WORD,
  IconButton,
  Input,
  LinkButton,
  Pill,
  REVEAL,
  ROW_VALUE,
  Row,
  Segmented,
  ToolButton,
  ToolDivider,
  ToolTower,
  cn,
  GroupLabel,
  chipLike,
  Chip,} from "../ui";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { useKb, useKbId } from "../kb";
import { toast } from "../toast";
import { fetchMe } from "../authSession";

const EDGE_CURVATURE_STEP = 0.18;

/** Which arc an edge is drawn on; `curvature === 0` means straight. */
interface PlacedEdge {
  edge: GraphEdge;
  curvature: number;
  /** Other wordings merged into this edge (inverse relations), shown together on hover */
  alsoLabels: string[];
}

/** Edges between the same pair of nodes are drawn on their own arcs; those inferred from inverse relations are merged away first.
 *
 *  **Two steps, and the order matters: merge first, then spread.**
 *
 *  1. `A works_at B` and the `B employs A` it implies are **two wordings of the same fact**,
 *  not two pieces of knowledge. Drawing them as two arcs only makes the redundancy look nicer. So an edge inferred from an inverse relation
 *  is merged into its source edge, and the wording hangs on that edge. `sub_property` (`ceo_of ⊑ works_at`)
 *  is not merged: those are two facts at different granularities, each valid on its own.
 *
 *  2. The rest are grouped by **undirected pair** and fanned out. Undirected is the point: an edge and its reverse have opposite endpoints,
 *  so grouping by directed pair would put each in its own group, each thinking it is alone, and they would stack back onto one straight line.
 *  Grouping uses min/max, and when placed on an arc the sign is flipped to follow the edge's own direction: sigma's curvature is relative to
 *  source→target, so without the flip a reversed arc would bulge to the same side. */
function layOutParallelEdges(edges: GraphEdge[]): {
  edges: PlacedEdge[];
  folded: number;
} {
  const pairKey = (a: string, b: string) => (a < b ? `${a} ${b}` : `${b} ${a}`);
  const push = (m: Map<string, GraphEdge[]>, k: string, e: GraphEdge) => {
    const list = m.get(k);
    if (list) list.push(e);
    else m.set(k, [e]);
  };

  // ---- 1. Merge away the edges inferred from inverse relations
  const survivors: GraphEdge[] = [];
  const inverses: GraphEdge[] = [];
  for (const e of edges) {
    if (e.derived && e.rule === "inverse") inverses.push(e);
    else survivors.push(e);
  }
  /* **Find the source edge by its premise, not by the node pair.**
     It was once written as "take the first edge on that pair of nodes", so `contains` got attached to an `allied_with` edge that also joined
     those two points, while `contains` belongs to `part_of`. Once attached wrongly,
     the interface looks perfectly normal, which is the hardest kind of bug to spot. The premise is computed by the server, so use it. */
  const onScreen = new Map<string, GraphEdge>();
  for (const e of survivors) onScreen.set(e.id, e);

  const also = new Map<string, string[]>();
  let folded = 0;
  for (const e of inverses) {
    const host = (e.premises ?? []).map((p) => onScreen.get(p)).find(Boolean);
    if (!host) {
      // The source edge is not on this screen (the timeline filtered it out, or it is itself inferred and was filtered out).
      // **Then keep the edge**: merging into an edge that does not exist is the same as deleting that piece of knowledge.
      survivors.push(e);
      continue;
    }
    const list = also.get(host.id) ?? [];
    // De-duplication: several transitively inferred `part_of` edges each have their own inverse, and their premise chains all lead back to the same edge,
    // so the same wording would be attached three times. **A wording is a name, not a count.**
    const name = e.label ?? e.predicate ?? "";
    if (!list.includes(name)) list.push(name);
    also.set(host.id, list);
    folded++;
  }

  // ---- 2. Fan out the rest by undirected pair
  const groups = new Map<string, GraphEdge[]>();
  for (const e of survivors) push(groups, pairKey(e.source, e.target), e);

  const placed: PlacedEdge[] = [];
  for (const group of groups.values()) {
    const n = group.length;
    group.forEach((e, i) => {
      // Spread symmetrically around the straight line: n=1 → [0]; n=2 → [-0.5, 0.5]; n=3 → [-1, 0, 1]
      const offset = n === 1 ? 0 : i - (n - 1) / 2;
      const sign = e.source < e.target ? 1 : -1;
      placed.push({
        edge: e,
        curvature: offset === 0 ? 0 : sign * offset * EDGE_CURVATURE_STEP,
        alsoLabels: also.get(e.id) ?? [],
      });
    });
  }
  return { edges: placed, folded };
}
// breathing period. The animation is not there for looks: a single colour difference across hundreds of edges goes unnoticed
const DERIVED_PULSE_MS = 2200;
// above this number only colour is applied, no animation. **Write it out rather than degrade silently:** recomputing the colour of thousands of edges every frame
// buys a graph that cannot be dragged, and at that point what the user wants is to be able to drag it
const DERIVED_ANIMATE_MAX = 400;
// duration of the on/off fade. **Slightly longer than FADE_MS (320):** playback fade-in brings edges in one at a time,
// while this is a whole batch of edges entering and leaving at once, so a slower pace is needed to see that "that batch of gold lines exits together"
const DERIVED_TOGGLE_MS = 420;
/* Inferred edges **arrive a little after the facts** and then fade in as a whole.

   I tried staging the derivation: the premises light up in order, and the conclusion last. Two versions were both unreadable:
   in the first, the premises flashed and went out, so by the time the conclusion appeared they had already dimmed; in the second, each group lit up and settled together,
   but dozens of groups still rose and fell across the graph, and it was impossible to tell which belonged to which.
   **A graph of several hundred edges is not the place to explain a chain of causes**: the derived panel in the sidebar
   lists them one by one, which is far clearer. Here we only need to convey one thing: these edges came later and are not the same kind of thing as what people wrote.
   Arriving a little late, plus their own colour, already says it all. */
const DERIVE_SETTLE_MS = 500; // how long after the facts settle before the inferred ones get their turn
const DERIVE_FADE_MS = 620; // duration of the overall fade-in; slower than the toggle tier, since it is an entrance, not a switch
// The legend shows at most this many pills; the rest are folded into "+N classes". **This row is laid out horizontally:
// with many classes it wraps and pushes the canvas down**; and a dozen identical pills in a row
// make it impossible to tell which one matters. The folded ones can still be found by searching "+N".
const LEGEND_MAX = 6;
/* The selectable tiers for how many nodes to draw. **Give tiers, not an input box:** the number has no meaning as "exact";
   it only affects whether the graph stays clear or stays draggable. Users want "more/fewer dots", not the number 237.
   The maximum is aligned with the backend GRAPH_NODE_CAP_MAX; beyond that it is dragging that fails first, not clarity */
const NODE_BUDGETS: number[] = [150, 300, 600, 1000];
/* Edge colours all come from the palette (graphVisuals, which reads the current theme's tokens, 0038).
   Ghost edges (derived ones not yet landed) are mixed toward EDGE_DIM in the same hue: sigma's edge shader cannot darken an edge with alpha under premultiplied blending,
   so the darkness has to be encoded in RGB, which is why it is a function that is recomputed after a theme switch */
const edgeGhost = () => lerpColor(EDGE_FOCUS_CONTEST, EDGE_DIM, 0.55);
const edgeGhostFocus = () => lerpColor(EDGE_FOCUS_CONTEST, EDGE_DIM, 0.2);

/** Which edge the panel is pointing at right now: a row under the pointer takes priority, otherwise the pinned one; but a pin only holds while the entity that pinned it
 *  is still selected, and it gives way while the canvas or panel is pointing at a node, so "who it connects to" stays readable */
function panelFocus(
  pointed: string | null,
  pinned: { entity: string; fact: string } | null,
  hovered: string | null,
  selected: string | null,
): string | null {
  if (pointed) return pointed;
  if (!pinned || hovered || pinned.entity !== selected) return null;
  return pinned.fact;
}
const DAY_MS = 24 * 3600 * 1000;

/* Play fade-in: parse hex / rgb / rgba (including alpha) and interpolate linearly */
/** Fade-in duration for newly appearing elements during playback */
const FADE_MS = 320;

export function Graph() {
  const kbId = useKbId();
  const { kb } = useKb();
  /* The address bar and the picture stay **two-way** in sync.
     Previously only the "in" half existed: `?entity=` was read once on mount and never touched again,
     so a link someone else gave you worked, but what you were looking at could not be shared, because the address bar stayed stuck on
     the bare /graph. */
  const search = useSearch({ from: "/app/kb/$kbId/graph" });
  const navigate = useNavigate();
  const entityParam = search.entity;
  // When the theme changes the graph must be rebuilt (colours are baked into attributes): see the graph-building effect
  const [themeTick, setThemeTick] = useState(0);
  const [focusEntity, setFocusEntity] = useState<string | null>(
    search.focus ?? entityParam ?? null,
  );
  const [selected, setSelected] = useState<string | null>(entityParam ?? null);
  const [searchInput, setSearchInput] = useState("");
  const [searchQ, setSearchQ] = useState("");
  const [hiddenTypes, setHiddenTypes] = useState<Set<string>>(new Set());
  // Whether inferred edges are shown. Shown by default: inference is off by default, so the presence of derived edges means
  const [showDerived, setShowDerived] = useState(true);
  // The info window is collapsed by default: it answers "when was this inferred", a question asked only occasionally
  /* The two floating layers on the canvas (Inference, and "+N classes") match the three panels in the top bar:
     shadcn's Popover. They used to be hand-written inline expansions, and the panel's first row had to redraw the trigger pill
     as a close button; unified, closing is handled by Esc, an outside click, and the trigger itself */
  const [derivedOpen, setDerivedOpen] = useState(false);
  const [legendOpen, setLegendOpen] = useState(false);
  const [legendQ, setLegendQ] = useState("");
  /* Entities that are exiting. **The panel cannot unmount the moment it is deselected**, or it would vanish instantly.
     It first stays in place to finish the exit, and only then is it actually removed. Read the current value from selectedRef rather than
     writing setState with a side-effecting updater: that style runs twice under StrictMode */
  const [exiting, setExiting] = useState<string | null>(null);
  // Which level to stop at and which row to expand when the panel opens; used when a ghost edge is clicked from elsewhere
  const panelIntentRef = useRef<{ view: "derived"; open: string } | null>(null);
  const deselect = useCallback(() => {
    const cur = selectedRef.current;
    if (!cur) return;
    setExiting(cur);
    setSelected(null);
    window.setTimeout(() => setExiting(null), 170);
  }, []);
  /** null = all time; a number = the as-of moment (ms).
      By default as-of is today: the temporal platform's graph presents "the world as it is now", and
      closed facts should not sit side by side with current facts indistinguishably (All time is an explicit choice) */
  /* Timeline. If the URL carries it, use it: `all` = all time, otherwise parse as YYYY-MM-DD
     (matches the day-level precision of the data, and is easier to read than a string of milliseconds) */
  const [timeT, setTimeT] = useState<number | null>(() => {
    if (search.at === "all") return null;
    if (search.at) {
      const t = Date.parse(search.at);
      if (!Number.isNaN(t)) return t;
    }
    return Date.now();
  });
  const [activeCount, setActiveCount] = useState(0);
  const [stabilizing, setStabilizing] = useState(false);
  /* Playback state is lifted to this layer: the reducer must tell "playback advancing" (fade) from "manual dragging" (instant switch) */
  const [playing, setPlaying] = useState(false);

  /* Picture → address bar. **replace, not push:** clicking a node is browsing, not navigating;
     pushing it into history would turn "back" into undoing clicks one by one.
     Skipped entirely during playback: writing the URL every frame would be a disaster */
  useEffect(() => {
    if (playing) return;
    const at =
      timeT === null
        ? "all"
        : // When sitting at "now", do not write. Otherwise every open would drag a string of today's dates into the address bar,
          // which is the default value anyway
          Math.abs(timeT - Date.now()) < DAY_MS
          ? undefined
          : new Date(timeT).toISOString().slice(0, 10);
    const next = {
      entity: selected ?? undefined,
      // **Do not write it when it equals entity:** clicking a search result sets both at once,
      // and written out as-is the address bar would show the same UUID twice.
      // It only carries information when it is "focused on A's neighbourhood but B is selected"
      focus:
        focusEntity && focusEntity !== selected ? focusEntity : undefined,
      at,
    };
    if (
      next.entity === search.entity &&
      next.focus === search.focus &&
      next.at === search.at
    )
      return;
    navigate({
      to: "/kb/$kbId/graph",
      params: { kbId },
      search: next,
      replace: true,
    });
  }, [
    selected,
    focusEntity,
    timeT,
    playing,
    search.entity,
    search.focus,
    search.at,
    navigate,
  ]);

  /* Address bar → picture. **This half is for back/forward:** without it, the browser's back button would
     only change the address and not the picture, which looks like back is broken. Both directions compare before acting, so they never fight */
  useEffect(() => {
    const e = search.entity ?? null;
    const f = search.focus ?? null;
    setSelected((cur) => (cur === e ? cur : e));
    setFocusEntity((cur) => (cur === f ? cur : f));
  }, [search.entity, search.focus]);
  /* Layout mode: force = FA2 repulsion; circular = ring; pack = circle packing clusters by type */
  type LayoutMode = "force" | "circular" | "pack";
  const [layoutMode, setLayoutMode] = useState<LayoutMode>("force");
  const layoutModeRef = useRef<LayoutMode>("force");
  const layoutCtlRef = useRef<{ apply: (m: LayoutMode) => void } | null>(null);

  /* How many to draw. **It goes into the queryKey**: if it did not, changing the tier would not refetch,
     and the interface would look changed while still showing the old data */
  const [nodeBudget, setNodeBudget] = useState<number>(NODE_BUDGETS[0]);

  // Who the empty state is shown to: admins can configure the model themselves, everyone else has to go find an admin. Shares the same cache as Shell
  const me = useQuery({ queryKey: ["me"], queryFn: fetchMe });
  // Which sentence the empty state says depends on how far this base has got (#313)
  const readiness = useReadiness(kbId);
  const step = nextStep(readiness.data, {
    kbId,
    isAdmin: !!me.data?.is_admin,
    canUpload: kb?.my_role !== "viewer",
  });
  const data = useQuery({
    queryKey: focusEntity
      ? ["graph", kb?.id, "neighborhood", focusEntity]
      : ["graph", kb?.id, "overview", nodeBudget],
    queryFn: () =>
      focusEntity
        ? api.graphNeighborhood(kb!.id, focusEntity)
        : api.graphOverview(kb!.id, nodeBudget),
    enabled: !!kb,
  });

  // Whole-graph mode searches all entities in the base; subgraph mode only filters client-side within the loaded subgraph
  const inSubgraph = !!focusEntity;
  // Upper limit on the number of results. **"Load more" rather than pagination:** this is a dropdown suggestion box,
  // and the user is looking for one specific entity; paging would make them lose the few they have just scanned
  const [searchLimit, setSearchLimit] = useState(10);
  useEffect(() => setSearchLimit(10), [searchQ]);
  const candidates = useQuery({
    queryKey: ["entitySearch", kb?.id, searchQ, searchLimit],
    queryFn: () => api.searchEntities(kb!.id, searchQ, searchLimit),
    enabled: !!kb && searchQ.length > 0 && !inSubgraph,
    placeholderData: (prev) => prev,
  });
  const subgraphHits = useMemo(() => {
    if (!inSubgraph || !searchQ || !data.data) return [];
    const q = searchQ.toLowerCase();
    return data.data.nodes
      .filter(
        (n) =>
          n.name.toLowerCase().includes(q) ||
          n.disambiguator?.toLowerCase().includes(q),
      )
      .slice(0, 10);
  }, [inSubgraph, searchQ, data.data]);
  const searchHits = inSubgraph
    ? subgraphHits
    : (candidates.data?.entities ?? []);

  const containerRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLCanvasElement>(null);
  const sigmaRef = useRef<Sigma | null>(null);
  /* Focus: hover takes priority over selection; styling is handled uniformly in the reducer */
  const selectedRef = useRef<string | null>(null);
  const hoverRef = useRef<string | null>(null);
  /** The edge the mouse is resting on, used to highlight the inverse-relation wordings merged into it */
  const hoverEdgeRef = useRef<string | null>(null);
  /** **Which edge the panel is pointing at:** the pointer rests on a fact row (pointing), or that row was clicked (pinned).
   *  Previously the panel and the canvas each said different things: in the list, `partner CoreWeave`, while the dozens of edges on the canvas
   *  connected to this entity all lit up alike, so you could not tell which one it was. The pointed-at one takes priority over the pinned one; both use the fact id as the edge key */
  const panelEdgeRef = useRef<string | null>(null);
  /** A pin remembers which entity's panel it was pinned in: once you switch entity it no longer holds, so no extra effect is needed to clear it */
  const panelPinRef = useRef<{ entity: string; fact: string } | null>(null);
  const [pin, setPin] = useState<{ entity: string; fact: string } | null>(null);
  const pinnedFact = pin && pin.entity === selected ? pin.fact : null;
  /** How close counts as "looking up close": at that point every edge gets its label (see updateEdgeLabels) */
  const deepZoomRef = useRef(false);
  const filterRef = useRef<{
    hiddenTypes: Set<string>;
    activeNodes: Set<string> | null;
    activeEdges: Set<string> | null;
    /** Whether inferred edges are shown. **Shown by default**: inference is off by default, so the presence of derived edges means
     *  the user has turned the switch on deliberately; but it must be possible to hide them with one click, to see what the "only what people have said" graph looks like */
    showDerived: boolean;
  }>({
    hiddenTypes: new Set(),
    activeNodes: null,
    activeEdges: null,
    showDerived: true,
  });
  const playingRef = useRef(false);
  /* Playback fade-in table: ids of nodes/edges newly activated this round → activation time (driven by an rAF loop until they settle) */
  const fadeRef = useRef<Map<string, number>>(new Map());
  const fadeRafRef = useRef(0);

  const kickFade = useCallback(() => {
    if (fadeRafRef.current) return;
    const step = () => {
      const now = performance.now();
      for (const [id, start] of fadeRef.current)
        if (now - start >= FADE_MS) fadeRef.current.delete(id);
      sigmaRef.current?.refresh();
      fadeRafRef.current = fadeRef.current.size
        ? requestAnimationFrame(step)
        : 0;
    };
    fadeRafRef.current = requestAnimationFrame(step);
  }, []);

  useEffect(() => {
    playingRef.current = playing;
    if (!playing) {
      // Stop playback: unfinished fade-ins go straight to their final state
      fadeRef.current.clear();
      sigmaRef.current?.refresh();
    }
  }, [playing]);

  useEffect(() => () => cancelAnimationFrame(fadeRafRef.current), []);

  const types = useMemo(() => {
    const map = new Map<
      string,
      { label: string; color: string; shape: string; count: number }
    >();
    for (const n of data.data?.nodes ?? []) {
      // Types that could not be determined fall into the empty-key tier (0009). Real keys are derived from the IRI and can never be empty,
      // so this cannot collide with any class; labels go through i18n, so do not draw null onto the legend
      const key = n.type_key ?? "";
      const cur = map.get(key);
      if (cur) cur.count++;
      else
        map.set(key, {
          label: n.type_label ?? S.graph.untyped,
          color: n.color,
          shape: n.shape,
          count: 1,
        });
    }
    // **Sorted by occurrence count, not by first encounter.** The legend only has room for a few, and those slots should go to
    // the most common classes on screen; previously it was node arrival order, which was effectively random. Ties are broken by label,
    // otherwise the same data would reshuffle on every refresh
    return [...map.entries()].sort(
      (a, b) => b[1].count - a[1].count || a[1].label.localeCompare(b[1].label),
    );
  }, [data.data]);

  /* Those that fit / those folded away. The folded ones can still be found and switched via "+N" */
  const legendShown = types.slice(0, LEGEND_MAX);
  const legendRest = types.slice(LEGEND_MAX);
  // Whether any folded class is being hidden. **Without this marker it is a silent filter**:
  // you switch a class off in the panel and collapse the panel, and nothing on the interface says it was switched off any more
  const hiddenInRest = legendRest.filter(([k]) => hiddenTypes.has(k)).length;

  // How many inferred edges there are. **When zero, the switch does not appear at all**: a base with inference not turned on should not
  // show a button that never changes anything
  const derivedCount = useMemo(
    () => (data.data?.edges ?? []).filter((e) => e.derived).length,
    [data.data],
  );

  /* Time filter: compute the set of active edges and nodes at time T */
  const recomputeActive = useCallback(
    (t: number | null) => {
      const d = data.data;
      if (!d) return;
      if (t === null) {
        filterRef.current.activeNodes = null;
        filterRef.current.activeEdges = null;
        setActiveCount(d.edges.length);
      } else {
        const prevNodes = filterRef.current.activeNodes;
        const prevEdges = filterRef.current.activeEdges;
        const edges = new Set<string>();
        const nodes = new Set<string>();
        const touched = new Set<string>();
        for (const e of d.edges) {
          // Filter by the **read** interval (0022): an edge with no start only lights up from the earliest evidence on, and once it ends
          // it goes out on the day the document saying so is read. Only a NULL end is open-ended. Here we no longer read
          // "no start" as "always there"
          const hf = e.holds_from ? Date.parse(e.holds_from) : null;
          const ht = e.holds_to ? Date.parse(e.holds_to) : null;
          touched.add(e.source);
          touched.add(e.target);
          const active = (hf === null || hf <= t) && (ht === null || ht > t);
          if (active) {
            edges.add(e.id);
            nodes.add(e.source);
            nodes.add(e.target);
          }
        }
        // Isolated nodes with no edges stay visible
        for (const n of d.nodes) if (!touched.has(n.id)) nodes.add(n.id);
        // Elements appearing during playback fade in; manual dragging switches instantly
        if (playingRef.current) {
          const now = performance.now();
          for (const id of edges)
            if (prevEdges && !prevEdges.has(id)) fadeRef.current.set(id, now);
          for (const id of nodes)
            if (prevNodes && !prevNodes.has(id)) fadeRef.current.set(id, now);
          if (fadeRef.current.size) kickFade();
        }
        filterRef.current.activeNodes = nodes;
        filterRef.current.activeEdges = edges;
        setActiveCount(edges.size);
      }
      sigmaRef.current?.refresh();
    },
    [data.data, kickFade],
  );

  useEffect(() => {
    filterRef.current.hiddenTypes = hiddenTypes;
    filterRef.current.showDerived = showDerived;
    sigmaRef.current?.refresh();
  }, [hiddenTypes, showDerived]);

  const deriveRafRef = useRef(0);
  /* Derived edges do not appear until the animation has run. **The switch is "whether to show", this one is "has the animation reached it"**:
     two things; merging them would mean that turning it off and on again plays one less animation */
  const [derivedRevealed, setDerivedRevealed] = useState(false);
  /* The reducer is a closure run every frame; reading state would give stale values, so it trusts only the ref */
  const derivedRevealedRef = useRef(false);
  useEffect(() => {
    derivedRevealedRef.current = derivedRevealed;
    sigmaRef.current?.refresh();
  }, [derivedRevealed]);

  const revealDerived = useCallback(() => {
    setDerivedRevealed(true);
    // Reuses the switch's fade-in: direction is "on", brightening from near the background colour to normal
    derivedToggleRef.current = { at: performance.now(), on: true };
    const step = () => {
      const tr = derivedToggleRef.current;
      const done = !tr || performance.now() - tr.at >= DERIVE_FADE_MS;
      if (done) derivedToggleRef.current = null;
      sigmaRef.current?.refresh();
      deriveRafRef.current = done ? 0 : requestAnimationFrame(step);
    };
    cancelAnimationFrame(deriveRafRef.current);
    deriveRafRef.current = requestAnimationFrame(step);
  }, []);

  useEffect(() => () => cancelAnimationFrame(deriveRafRef.current), []);

  /* On/off fade in and out: { start time, which direction }; null = no transition in flight */
  const derivedToggleRef = useRef<{ at: number; on: boolean } | null>(null);
  const derivedRafRef = useRef(0);
  /* The previous switch value. **Only this can tell whether it really switched**: the effect's dependencies
     also include derivedCount, and "Run now" producing new edges changes count without touching the switch;
     looking only at effect triggers would fade once, an animation nobody asked for */
  const prevShowDerived = useRef(showDerived);

  // Switching runs a gradient rather than vanishing instantly. **Must drive the redraw itself**: when turned off, the breathing timer below
  // stops turning, nobody pushes sigma to redraw, and the fade-out would stall on the first frame
  useEffect(() => {
    const changed = prevShowDerived.current !== showDerived;
    prevShowDerived.current = showDerived;
    // First mount and "only count changed" are not switches:
    // on entering the page, and when the inference run finishes and the count refreshes, you should not see a meaningless fade-in
    if (!changed) return;
    // When there are too many, do not fade: same line as breathing; recomputing the colour of thousands of edges every frame buys stutter.
    // **Write it out rather than degrade silently**
    if (derivedCount > DERIVED_ANIMATE_MAX) return;

    const now = performance.now();
    const prev = derivedToggleRef.current;
    // Reversed midway (the user double-clicked): continue from the current progress rather than from the start,
    // otherwise a brightness jump would be visible
    const at =
      prev && prev.on !== showDerived
        ? now - Math.max(0, DERIVED_TOGGLE_MS - (now - prev.at))
        : now;
    derivedToggleRef.current = { at, on: showDerived };

    const step = () => {
      const tr = derivedToggleRef.current;
      const done = !tr || performance.now() - tr.at >= DERIVED_TOGGLE_MS;
      if (done) derivedToggleRef.current = null;
      sigmaRef.current?.refresh();
      derivedRafRef.current = done ? 0 : requestAnimationFrame(step);
    };
    cancelAnimationFrame(derivedRafRef.current);
    derivedRafRef.current = requestAnimationFrame(step);
    // **Do not attach cleanup here:** cleanup would also run when dependencies change, and derivedCount is among them
    // (inference happens to finish in the middle of these 420ms, so the animation would be cut off halfway, and the picture would stay at half brightness
    // until some other redraw resets it). The loop terminates by itself; cancellation should only happen on unmount
  }, [showDerived, derivedCount]);

  // Clean up any frame still in flight on unmount
  useEffect(() => () => cancelAnimationFrame(derivedRafRef.current), []);

  /* When to enter. Both entry points share one delay: entering the page, and manually turning the switch on.
     **Does not wait for the layout to settle**: settling takes 2.5 seconds, by which time the person has already looked elsewhere.

     **The order itself is content:** the entries written by people land first, and only then the inferred ones.
     Appearing together would make it impossible to tell which came first */
  useEffect(() => {
    if (!showDerived || !data.data) {
      if (!showDerived) setDerivedRevealed(false);
      return;
    }
    if (derivedRevealed) return;
    const t = window.setTimeout(revealDerived, DERIVE_SETTLE_MS);
    return () => window.clearTimeout(t);
  }, [showDerived, data.data, derivedRevealed, revealDerived]);

  // Breathing of derived edges. **Only animates when there are derived edges, the display is on, and there are not too many:**
  // a base with inference off should not redraw itself every two seconds for this
  useEffect(() => {
    const n = derivedCount;
    if (!showDerived || n === 0 || n > DERIVED_ANIMATE_MAX) return;
    // Matching sigma's redraw rate is enough, not every frame: breathing is slow motion, and 30 fps shows no difference
    const timer = setInterval(() => sigmaRef.current?.refresh(), 1000 / 30);
    return () => clearInterval(timer);
  }, [showDerived, derivedCount]);

  useEffect(() => {
    selectedRef.current = selected;
    panelEdgeRef.current = null;
    sigmaRef.current?.refresh();
  }, [selected]);

  /** The panel points at a fact: that edge on the canvas lights up. One not on the canvas (filtered out by the timeline, or outside the neighbourhood) lights nothing,
   *  **and it is not replaced by another edge between the same pair of nodes**: that would light up an edge that is not the fact */
  const pointFact = useCallback((factId: string | null) => {
    const sigma = sigmaRef.current;
    if (!sigma) return;
    const next = factId && sigma.getGraph().hasEdge(factId) ? factId : null;
    if (panelEdgeRef.current === next) return;
    panelEdgeRef.current = next;
    sigma.refresh();
  }, []);

  /** The panel points at an entity (the word in the object position): on the canvas it is drawn as "pointed at" */
  const pointEntity = useCallback((entityId: string | null) => {
    const sigma = sigmaRef.current;
    if (!sigma) return;
    hoverRef.current = entityId && sigma.getGraph().hasNode(entityId) ? entityId : null;
    sigma.refresh();
  }, []);

  /** A fact was clicked: pin that edge, and move the camera so it sits between the two ends, zoomed out so both ends are in frame.
   *  If the edge is not on the canvas, fall back to jumping to the object: at least bring the person to the thing it connects to */
  const focusFact = useCallback((factId: string, otherId: string | null) => {
    const sigma = sigmaRef.current;
    const g = sigma?.getGraph();
    const entity = selectedRef.current;
    if (!sigma || !g || !entity || !g.hasEdge(factId)) {
      if (otherId) {
        setFocusEntity(otherId);
        setSelected(otherId);
      }
      return;
    }
    // Clicking the same row again: release
    const current = panelPinRef.current;
    if (current && current.entity === entity && current.fact === factId) {
      panelPinRef.current = null;
      setPin(null);
      sigma.refresh();
      return;
    }
    const next = { entity, fact: factId };
    panelPinRef.current = next;
    setPin(next);
    const [s, t] = g.extremities(factId);
    const a = sigma.getNodeDisplayData(s);
    const b = sigma.getNodeDisplayData(t);
    if (a && b) {
      // Camera coordinates and node display coordinates share a normalised frame (ratio 1 = the whole graph): the two ends are span apart,
      // with a 1.3× margin. The upper limit 0.6 is there to get past the line "edge labels are written only below zoom 0.7"
      const span = Math.hypot(a.x - b.x, a.y - b.y);
      sigma.getCamera().animate(
        {
          x: (a.x + b.x) / 2,
          y: (a.y + b.y) / 2,
          ratio: Math.min(0.6, Math.max(0.12, span * 1.3)),
        },
        { duration: 400 },
      );
    }
    sigma.refresh();
  }, []);

  useEffect(() => {
    recomputeActive(timeT);
  }, [timeT, recomputeActive]);

  useEffect(() => {
    // Node shell colour and edge colour are **baked into the graph attributes**, not fetched at render time, so before building, read the palette
    // into the current theme's values, and on theme switch use themeTick to make this run again (0038)
    refreshPalette();
    if (!containerRef.current || !data.data) return;
    const g = new Graphology({ multi: true });
    for (const n of data.data.nodes) {
      if (!g.hasNode(n.id)) {
        g.addNode(n.id, {
          label: n.name,
          // Semantica recipe: deep shell + 14% type tint, core 50% tint, steel-grey outline with a slight tint
          color: mix(NODE_CORE_BASE, n.color, NODE_CORE_MIX),
          shellColor: mix(NODE_SHELL_BASE, n.color, NODE_TINT_MIX),
          borderColor: mix(NODE_BORDER_BASE, n.color, 0.3),
          ringColor: TRANSPARENT,
          typeColor: n.color,
          typeLabel: n.type_label ?? S.graph.untyped,
          typeKey: n.type_key ?? "",
          type: n.shape === "square" ? NODE_TYPE_SQUARE : NODE_TYPE_SHELL,
          size: 5 + Math.min(8, Math.sqrt(Number(n.degree)) * 1.6),
        });
      }
    }
    const placed = layOutParallelEdges(
      data.data.edges.filter((e) => g.hasNode(e.source) && g.hasNode(e.target)),
    );
    for (const { edge: e, curvature, alsoLabels } of placed.edges) {
      g.addEdgeWithKey(e.id, e.source, e.target, {
        /* Disputed edge labels get a ⚠ prefix: an extra mark that does not rely on colour vision, on top of the colour itself.
           **The predicate is shown as written in the ontology**, no longer capitalised: nowhere on the interface is there capitalised, letter-spaced text,
           and the ontology page lists "part of" exactly as is */
        label: (e.contested ? "⚠ " : "") + (e.label ?? ""),
        size: e.blocked ? 0.7 : 1,
        color: e.blocked
          ? edgeGhost()
          : e.contested
            ? EDGE_CONTEST
            : e.derived
              ? EDGE_DERIVED
              : e.inferred
                ? EDGE_INFERRED
                : EDGE,
        contested: e.contested,
        blocked: e.blocked,
        // A single edge goes straight: curves exist to separate overlaps, and with no overlap there is no need to bend
        type: curvature === 0 ? "line" : "curved",
        curvature,
        // Merged inverse-relation wordings are shown alongside the main name on hover
        alsoLabels,
        // The reducer reads it every frame: decides whether to hide it and whether to breathe
        derived: e.derived,
      });
    }
    // Layout: first spread out statically, then let the worker animate for ~2.5s to settle (Semantica-style stabilising)
    let fa2: InstanceType<typeof FA2Layout> | null = null;
    let stabilizeTimer: ReturnType<typeof setTimeout> | null = null;
    // Drag state is declared before fa2: the outputReducer closure references them
    let dragged: string | null = null;
    let dragPos: { x: number; y: number } | null = null;
    let fa2Settings: ReturnType<typeof forceAtlas2.inferSettings> | null = null;
    if (g.order > 0) {
      circular.assign(g, { scale: 300 });
      /* Tried scaling by graph size (gravity 0.12–0.22 / scalingRatio 11–16 + heavier damping),
         and rejected it on a real graph: it spread out, but the tension of "being pushed apart" was lost,
         and the whole graph looked limp. **This set is the existing, deliberately larger one**: what we want is nodes pushing against each other,
         not the least-effort layout */
      const settings = {
        ...forceAtlas2.inferSettings(g),
        gravity: 0.35,
        scalingRatio: 22,
        outboundAttractionDistribution: true,
      };
      fa2Settings = settings;
      forceAtlas2.assign(g, { iterations: 60, settings });
      fa2 = new FA2Layout(g, {
        settings,
        // Key: on write-back, pin the dragged node to the cursor (no flicker); and once an outputReducer is supplied,
        // the supervisor calls readGraphPositions every frame, so the cursor position keeps feeding into the force simulation
        outputReducer: (node, attr) => {
          if (dragged && node === dragged && dragPos) {
            attr.x = dragPos.x;
            attr.y = dragPos.y;
          }
          return attr;
        },
      });
      fa2.start();
      setStabilizing(true);
      stabilizeTimer = setTimeout(() => {
        fa2?.stop();
        setStabilizing(false);
      }, 2500);
    }

    // After a data rebuild the layout returns to force (the world regrows)
    setLayoutMode("force");
    layoutModeRef.current = "force";

    // Scale any layout result uniformly to the FA2 world of the same magnitude (±target), so the camera reset looks consistent
    const rescaleWorld = (target = 300) => {
      let minX = Infinity,
        maxX = -Infinity,
        minY = Infinity,
        maxY = -Infinity;
      g.forEachNode((_n, a) => {
        minX = Math.min(minX, a.x as number);
        maxX = Math.max(maxX, a.x as number);
        minY = Math.min(minY, a.y as number);
        maxY = Math.max(maxY, a.y as number);
      });
      const span = Math.max(maxX - minX, maxY - minY) || 1;
      const k = (target * 2) / span;
      const cx = (minX + maxX) / 2;
      const cy = (minY + maxY) / 2;
      g.updateEachNodeAttributes((_n, a) => ({
        ...a,
        x: (a.x - cx) * k,
        y: (a.y - cy) * k,
      }));
    };

    // Layout switch control (attached to a ref for the component layer's buttons to call; the closure holds g / fa2 directly)
    layoutCtlRef.current = {
      apply: (mode) => {
        if (g.order === 0) return;
        if (stabilizeTimer) clearTimeout(stabilizeTimer);
        fa2?.stop();
        setStabilizing(false);
        if (mode === "force") {
          forceAtlas2.assign(g, {
            iterations: 60,
            settings: fa2Settings ?? undefined,
          });
          fa2?.start();
          setStabilizing(true);
          stabilizeTimer = setTimeout(() => {
            fa2?.stop();
            setStabilizing(false);
          }, 2500);
        } else if (mode === "circular") {
          circular.assign(g, { scale: 300 });
        } else {
          // Cluster by entity type: the same type is squeezed into the same circle
          circlepack.assign(g, { hierarchyAttributes: ["typeKey"] });
          rescaleWorld(300);
        }
        sigma.setCustomBBox(null);
        sigma.refresh();
        sigma.getCamera().animatedReset({ duration: 300 });
      },
    };

    /* **Is it still connected right now.**
     *
     * `areNeighbors` only asks whether that edge exists on the graph, not whether it still holds at the moment the timeline stops;
     * the edge side asks (see `liveNow`). When the two disagree, the picture shows "node lit, line
     * not": hover Google DeepMind and Arthur Mensch lights up, yet its
     * former_employee_of ended long ago, so the line is pushed back to the background colour, which looks like a node lit up from nowhere.
     * The same goes for derived edges when switched off: that whole edge is not drawn, so the other end should not still light up as a neighbour.
     *
     * **Cached per focus node:** this check asks every node once per frame, and hubs can have
     * hundreds of edges, so scanning degrees per node is too costly. Each change on the timeline makes a new `activeEdges`
     * Set (see `recomputeActive`), so its identity is enough as the key, plus the edge count to cover the case where the graph
     * itself has been swapped out */
    const liveNbr = {
      focus: "",
      edges: null as Set<string> | null,
      derived: true,
      size: -1,
      set: new Set<string>(),
    };
    const isLiveNeighbor = (focus: string, node: string) => {
      const { activeEdges, showDerived } = filterRef.current;
      if (
        liveNbr.focus !== focus ||
        liveNbr.edges !== activeEdges ||
        liveNbr.derived !== showDerived ||
        liveNbr.size !== g.size
      ) {
        const set = new Set<string>();
        g.forEachEdge(focus, (e, attrs, src, tgt) => {
          if (activeEdges && !activeEdges.has(e)) return;
          if (!showDerived && attrs.derived === true) return;
          set.add(src === focus ? tgt : src);
        });
        liveNbr.focus = focus;
        liveNbr.edges = activeEdges;
        liveNbr.derived = showDerived;
        liveNbr.size = g.size;
        liveNbr.set = set;
      }
      return liveNbr.set.has(node);
    };

    sigmaRef.current?.kill();
    // Canvas colours are read from tokens (0038): read once before creating the instance, and read again after a theme switch and redrawn
    refreshPalette();
    const sigma = new Sigma(g, containerRef.current, {
      ...sigmaOptions({
        defaultEdgeType: "line",
        /* Parallel edges are fanned out into arcs (see `layOutParallelEdges`). The straight-line version drew every edge between the same pair
           of nodes on the same segment, so the labels stacked up character by character into garbage. Measured:
           up to six edges piled up on one pair of nodes */
        // Each program gets a "topmost" copy (see `withTopLayer`): highlighted edges are drawn last, as one batch
        edgeProgramClasses: withTopLayer({
          line: EdgeRectangleProgram,
          curved: EdgeCurveProgram,
        }),
        // Edge labels carry the predicate; at close range every drawn edge gets one (see updateEdgeLabels)
        renderEdgeLabels: true,
        // With thousands of nodes it has to zoom out further than the ontology page before the whole picture is visible
        minCameraRatio: 0.04,
        maxCameraRatio: 8,
      }),
      nodeReducer: (node, attrs) => {
        const f = filterRef.current;
        const res = { ...attrs };
        const base = attrs.size as number;
        if (f.hiddenTypes.has(attrs.typeKey as string)) {
          res.hidden = true;
          return res;
        }
        const hov = hoverRef.current;
        // The selected entity may not be on the current canvas (a gap during a sidebar jump or neighbourhood reload): if so, skip the focus-dimming logic
        const sel =
          selectedRef.current && g.hasNode(selectedRef.current)
            ? selectedRef.current
            : null;
        /* **Selection overrides hover.** These two sentences used to be reversed: when the pointer landed on the node you had just selected,
           it switched to the hover appearance, and the selection marker (inverted base card, bold name) vanished on the spot,
           even though the reason a person moves the pointer there is often that it is the one they chose.
           Hovering over other nodes still shows hover as usual */
        if (sel === node) {
          const picked = selectedNode(res, attrs, base);
          // The selected one is also being pointed at: its name is drawn by the highlight layer now, and the label layer gives way
          return hov === node ? deferToHoverLayer(picked) : picked;
        }
        /* The panel points at an edge: both ends are drawn as "pointed at", and the rest drop half a tier; what needs to be read is who this one connects */
        const pe = panelFocus(
          panelEdgeRef.current,
          panelPinRef.current,
          hoverRef.current,
          selectedRef.current,
        );
        if (pe && g.hasEdge(pe)) {
          const [ps, pt] = g.extremities(pe);
          if (node === ps || node === pt) return hoveredNode(res, attrs, base);
          if (hov !== node) return softMutedNode(res, attrs, base);
        }
        if (hov === node) return hoveredNode(res, attrs, base);
        if (sel) {
          // Neighbours get 0.76: with thousands of nodes, make room for the selected path
          if (isLiveNeighbor(sel, node)) neighborNode(res, base, 0.76);
          /* **Whoever is pointed at, keep its neighbours too.** On the edge side hover overrides the selection
             (`boost()` runs first), while the node side did not follow, so after selecting, pointing elsewhere produced
             a ring of lit edges leading to a ring of dark dots. When you point there, you want to read "who it connects to",
             and that question could not be answered at all */
          else if (hov && isLiveNeighbor(hov, node))
            neighborNode(res, base, 0.76);
          else return mutedNode(res, base);
        } else if (hov && hov !== node && !isLiveNeighbor(hov, node)) {
          // **Hover also dims the rest**, just one tier lighter than selection (see HOVER_MUTE).
          // Neighbours stay: hover is precisely meant to answer "who it connects to".
          // **A node that does not yet exist at this moment is dimmed all the way:** this branch returns early,
          // bypassing the time filter below; if only half were dimmed it would be brighter than when not hovered
          if (f.activeNodes && !f.activeNodes.has(node))
            return mutedNode(res, base);
          return softMutedNode(res, attrs, base);
        } else {
          // default {×0.7}
          res.size = base * 0.7;
        }
        if (f.activeNodes && !f.activeNodes.has(node))
          return mutedNode(res, base);
        // Play fade-in: from the muted form gradually to the normal form computed for this frame
        const fs = fadeRef.current.get(node);
        if (fs !== undefined) {
          const t = Math.min(1, (performance.now() - fs) / FADE_MS);
          res.size = (res.size as number) * (0.55 + 0.45 * t);
          res.color = lerpColor(
            MUTED_SHELL,
            String(res.color ?? NODE_CORE_BASE),
            t,
          );
          res.shellColor = lerpColor(
            MUTED_SHELL,
            String(res.shellColor ?? NODE_SHELL_BASE),
            t,
          );
          res.borderColor = lerpColor(
            "rgba(0,0,0,0)",
            String(res.borderColor ?? NODE_BORDER_BASE),
            t,
          );
          if (t < 0.7) res.label = "";
        }
        return res;
      },
      edgeReducer: (edge, attrs) => {
        const f = filterRef.current;
        const res = { ...attrs };
        const [s, t] = g.extremities(edge);
        // At close range every drawn edge gets its label (see updateEdgeLabels)
        if (deepZoomRef.current) res.forceLabel = true;
        /* The inverse-relation wordings merged into this edge are appended after the main name: `PART OF ⁻¹ CONTAINS`.
           **Shown only when it is the focus of attention**: always on would double the label length, and overly long labels
           are exactly the problem this is meant to cure.
           Two trigger points, because **an edge is only one pixel wide, so precise hovering is hard even for people**:
           the mouse rests on this edge, or on either of its two end nodes. The second is the one actually used;
           the first is kept because sometimes a person really does want to point at that one.
           Placed before the hide/dim logic: wordings are a matter of display, not of visibility */
        const also = attrs.alsoLabels as string[] | undefined;
        if (also && also.length > 0) {
          const focused =
            edge === hoverEdgeRef.current ||
            hoverRef.current === s ||
            hoverRef.current === t ||
            selectedRef.current === s ||
            selectedRef.current === t;
          if (focused) {
            res.label = `${attrs.label} ⁻¹ ${also.join(" / ")}`;
          }
        }
        const sk = g.getNodeAttribute(s, "typeKey") as string;
        const tk = g.getNodeAttribute(t, "typeKey") as string;
        if (f.hiddenTypes.has(sk) || f.hiddenTypes.has(tk)) {
          res.hidden = true;
          return res;
        }
        // Derived edges: first check whether to hide them, then decide how far to breathe.
        // **Placed first**: an edge that is hidden need not compute the brightening and dimming that follow
        const isDerived = attrs.derived === true;

        if (isDerived) {
          // Its turn to enter has not come yet, so do not draw it for now. **Facts land first, inferred ones arrive later**
          if (!derivedRevealedRef.current) {
            res.hidden = true;
            return res;
          }
          const tr = derivedToggleRef.current;
          const k = tr
            ? Math.min(1, (performance.now() - tr.at) / DERIVED_TOGGLE_MS)
            : 1;
          // Switched off: only the case "fade-out not yet finished" still keeps it from being hidden
          if (!f.showDerived) {
            if (!tr || tr.on || k >= 1) {
              res.hidden = true;
              return res;
            }
            /* Fades from the current colour down to near the background colour. **Darkness must be encoded in RGB**
               (see the comment at EDGE_DIM: under premultiplied blending alpha cannot darken an edge),
               so it blends toward EDGE_DIM rather than lowering alpha.

               **The starting point cannot be hard-coded to full-brightness gold everywhere:** this branch returns early, before the dimming logic for hover/selection,
               so an unrelated derived edge that had been dimmed would first jump back to full brightness and then fade out.
               That jump is the "unrelated edges flash when derived edges are switched off" bug.
               The starting point must take the edge's current appearance */
            const selNow =
              selectedRef.current && g.hasNode(selectedRef.current)
                ? selectedRef.current
                : null;
            const hovNow = hoverRef.current;
            const focused = selNow ?? hovNow;
            const ghost = attrs.blocked === true;
            const from = !focused
              ? ghost
                ? edgeGhost()
                : EDGE_DERIVED
              : s === focused || t === focused
                ? ghost
                  ? edgeGhostFocus()
                  : EDGE_FOCUS_DERIVED
                : EDGE_DIM;
            res.color = lerpColor(from, EDGE_DIM, k);
            res.label = "";
            return res;
          }
          // Ghost edges do not breathe: they are not knowledge, they are a path that never went through
          const pulse = attrs.blocked === true
            ? edgeGhost()
            : lerpColor(
            EDGE_DERIVED_DIM,
            EDGE_DERIVED,
            // Triangle wave rather than sine: pauses briefly at each end, so it reads as "breathing" rather than "flashing"
            Math.abs(
              ((performance.now() % DERIVED_PULSE_MS) / DERIVED_PULSE_MS) * 2 -
                1,
            ),
          );
          // Turned on: brightens from near the background colour, then joins the breathing
          res.color =
            tr && tr.on && k < 1 ? lerpColor(EDGE_DIM, pulse, k) : pulse;
        }
        // hover: only brighten related edges; selected: brighten related edges and dim the rest
        const hov = hoverRef.current;
        const sel =
          selectedRef.current && g.hasNode(selectedRef.current)
            ? selectedRef.current
            : null;
        const boost = () => {
          res.color =
            attrs.blocked === true
              ? edgeGhostFocus()
              : attrs.contested === true
                ? EDGE_FOCUS_CONTEST
                : isDerived
                  ? EDGE_FOCUS_DERIVED
                  : EDGE_FOCUS;
          res.size = Math.max((attrs.size as number) * 1.42, 1.85);
          res.zIndex = 5;
          // Move to the batch drawn last (see `edgeProgramClasses`): zIndex alone cannot override other programs
          res.type = drawLast(String(res.type ?? "line"));
        };
        /* **Whether this edge exists at this moment, with the timeline stopped at a point.**
           Both hover branches return early, bypassing the time filter below:
           without carrying it, on hover every "not yet grown" edge would jump from near-background colour
           to 45% of the normal colour and look lit up. Measured: that is exactly how bright it gets */
        const liveNow = !f.activeEdges || f.activeEdges.has(edge);
        /* **The one the panel points at overrides everything:** when an entity is selected, its connected edges are already fully lit,
           so the pointed-at one has to be brighter than them and labelled; the other edges of the same entity drop to half the normal colour,
           and unrelated ones stay dimmed as before */
        const pe = panelFocus(
          panelEdgeRef.current,
          panelPinRef.current,
          hoverRef.current,
          selectedRef.current,
        );
        if (pe && g.hasEdge(pe)) {
          if (edge === pe) {
            boost();
            res.size = Math.max((attrs.size as number) * 2.4, 2.8);
            res.forceLabel = true;
            return res;
          }
          const from = liveNow ? String(res.color) : EDGE_DIM;
          res.color = lerpColor(from, EDGE_DIM, HOVER_MUTE);
          res.size = (attrs.size as number) * 0.6;
          res.label = "";
          return res;
        }
        if (hov && (s === hov || t === hov) && liveNow) {
          boost();
        } else if (hov && !sel) {
          // On hover the other edges also step back, but **only by half**: the same HOVER_MUTE as for the nodes.
          // Dimming all the way is only for selection. An edge that does not exist at this moment **should be dim anyway**;
          // mixing from EDGE_DIM is the same as not moving at all
          const from = liveNow ? String(res.color) : EDGE_DIM;
          res.color = lerpColor(from, EDGE_DIM, HOVER_MUTE);
          res.size = (attrs.size as number) * (1 - 0.4 * HOVER_MUTE);
          res.label = "";
          return res;
        } else if (sel) {
          if (s === sel || t === sel) {
            boost();
          } else {
            res.color = EDGE_DIM;
            res.size = (attrs.size as number) * 0.6;
            res.label = "";
            return res;
          }
        }
        if (f.activeEdges && !f.activeEdges.has(edge)) {
          res.color = EDGE_DIM;
          /* **Once dimmed, leave the topmost batch** (see `boost()`). This tier comes after `boost()`:
             an edge next to the selected item but not yet existing on the timeline has its colour already dimmed
             back to the background, yet it still keeps the topmost programme given while highlighting, which makes it a dark line drawn above all
             the highlighted edges, cutting them off right where it sits */
          res.type = attrs.type as string;
          res.label = "";
          return res;
        }
        // Play fade-in: the edge fades from near the background colour to its regular colour (alpha interpolated in step)
        const fs = fadeRef.current.get(edge);
        if (fs !== undefined) {
          const t = Math.min(1, (performance.now() - fs) / FADE_MS);
          res.color = lerpColor(EDGE_DIM, String(res.color), t);
          if (t < 0.8) res.label = "";
        }
        return res;
      },
    });
    sigma.on("clickNode", ({ node }) => setSelected(node));
    // Clicking a ghost edge: open the panel of its subject, landing on the "inferred" tier and expanding that row (0017 §3)
    sigma.on("clickEdge", ({ edge }) => {
      if (g.getEdgeAttribute(edge, "blocked") !== true) return;
      const [s] = g.extremities(edge);
      panelIntentRef.current = { view: "derived", open: edge };
      setSelected(s);
    });
    sigma.on("doubleClickNode", ({ node, event }) => {
      event.preventSigmaDefault();
      setFocusEntity(node);
      setSelected(node);
    });
    const offTheme = onThemeChange(() => {
      refreshPalette();
      setThemeTick((t) => t + 1);
      sigma.refresh();
      if (gridRef.current) drawWorldGrid(gridRef.current, sigma);
    });
    sigma.on("clickStage", () => deselect());
    sigma.on("enterNode", ({ node }) => {
      hoverRef.current = node;
      sigma.refresh();
    });
    sigma.on("leaveNode", () => {
      hoverRef.current = null;
      sigma.refresh();
    });
    /* Hover an edge to show its merged wordings.
       An inverse-relation edge was merged away (see `layOutParallelEdges`), so drawing one fewer is correct,
       but the name should not vanish: `part_of` read in reverse as `contains` is something the ontology actually says, and people have the right to see it.
       **Shown only on hover**: always on would double the label length,
       and lengthening labels is exactly the problem this is meant to cure */
    sigma.on("enterEdge", ({ edge }) => {
      hoverEdgeRef.current = edge;
      sigma.refresh();
    });
    sigma.on("leaveEdge", () => {
      hoverEdgeRef.current = null;
      sigma.refresh();
    });
    /* Edge labels only appear once zoomed in (at the default distance they are too dense; Semantica is equally restrained).
       **One step closer switches to "write a label on every visible line"**: sigma's rule for choosing edge labels is
       "only write this edge when the node names at both ends are displayed" (`edgeLabelsToDisplayFromNodes`),
       but once zoomed in both ends are often outside the viewport, so the line in the middle of the screen has no wording at all,
       exactly when you want to see one relationship clearly. `forceLabel` is a back door left by sigma,
       and setting it bypasses that heuristic */
    const updateEdgeLabels = () => {
      const ratio = sigma.getCamera().ratio;
      // A graph with few edges (≤ 40) always writes predicates: labels are hidden to keep big graphs uncluttered,
      // but a base with a dozen edges coming in at ratio = 1 would hide them and make people think the predicates have vanished
      sigma.setSetting("renderEdgeLabels", ratio < 0.7 || g.size <= 40);
      const deep = ratio < 0.35;
      if (deep !== deepZoomRef.current) {
        deepZoomRef.current = deep;
        sigma.refresh({ skipIndexation: true });
      }
    };
    sigma.getCamera().on("updated", updateEdgeLabels);
    updateEdgeLabels();

    // World coordinate grid: redraw on camera changes or container resize
    const renderGrid = () => {
      if (gridRef.current) drawWorldGrid(gridRef.current, sigma);
    };
    sigma.getCamera().on("updated", renderGrid);
    sigma.on("resize", renderGrid);
    renderGrid();

    // Node dragging + live force-directed feedback. On press only record a candidate: promote to a drag only once the viewport displacement is >4px
    //(otherwise a plain click would also start FA2 by mistake); the dragged node is pinned to the cursor by fa2's outputReducer (see above),
    // and after release it stops once settled, ~1.2s
    let settleTimer: ReturnType<typeof setTimeout> | null = null;
    // The threshold and bounding-box freezing all live in attachDrag; here we only take the three hooks.
    // `dragged` still lives in this closure: FA2's outputReducer reads it every frame,
    // and pins the dragged one back to the cursor (see the fa2 construction above)
    attachDrag(sigma, {
      onStart: (node) => {
        dragged = node;
        if (settleTimer) clearTimeout(settleTimer);
        // Under static layouts (circular/pack) dragging does not wake the force simulation, otherwise a touch would scatter everything
        if (layoutModeRef.current === "force" && fa2 && !fa2.isRunning())
          fa2.start();
      },
      onMove: (_node, pos) => {
        dragPos = pos;
      },
      onEnd: () => {
        dragged = null;
        dragPos = null;
        settleTimer = setTimeout(() => fa2?.stop(), 1200);
      },
    });
    sigmaRef.current = sigma;
    if (import.meta.env.DEV) {
      // Debug handle (dev only): inspect the reducer output in a headless environment
      (window as unknown as Record<string, unknown>).__g = g;
      (window as unknown as Record<string, unknown>).__sigma = sigma;
      (window as unknown as Record<string, unknown>).__sel = selectedRef;
    }
    recomputeActive(timeT);
    return () => {
      offTheme();

      if (stabilizeTimer) clearTimeout(stabilizeTimer);
      if (settleTimer) clearTimeout(settleTimer);
      fa2?.kill();
      setStabilizing(false);
      sigma.kill();
      sigmaRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [data.data, themeTick]);

  if (!kb)
    return <div className="p-8 text-body text-ink-2">{S.nav.loading}</div>;

  const empty = data.isSuccess && data.data.nodes.length === 0;
  const nodeCount = data.data?.nodes.length ?? 0;
  const edgeCount = data.data?.edges.length ?? 0;
  // How many are in the base in total. **Not the same as what is drawn**: the neighbourhood view has no total (it is only a small patch by nature),
  // so the default falls back to the drawn count rather than showing "0 in total"
  const totalNodes = data.data?.total_nodes ?? nodeCount;
  const totalEdges = data.data?.total_edges ?? edgeCount;
  const capped = totalNodes > nodeCount;

  return (
    <div className="h-full relative">
      {/* Top floating bar: search, legend, status */}
      <div className="absolute top-3 left-3 right-3 z-10 flex items-start gap-2 pointer-events-none">
        <div className="relative pointer-events-auto">
          {/* Same build and same corner as the filter box in the ontology page's left column (see Ontology.tsx) */}
          <Input
            icon={<Search size={12} />}
            className="w-58 u-lift"
            placeholder={
              inSubgraph ? S.graph.searchInSubgraph : S.graph.searchEntity
            }
            value={searchInput}
            onChange={(e) => {
              setSearchInput(e.target.value);
              setSearchQ(e.target.value.trim());
            }}
          />
          {searchQ && searchHits.length > 0 && (
            <div className="glass-strong absolute mt-1 w-full rounded-overlay u-lift-strong overflow-hidden">
              {searchHits.map((c) => (
                <Row
                  key={c.id}
                  trailing={c.type_label}
                  onClick={() => {
                    // A hit within the subgraph only selects (it is already in view); a whole-graph search jumps to that entity's neighbourhood
                    if (!inSubgraph) setFocusEntity(c.id);
                    setSelected(c.id);
                    setSearchInput("");
                    setSearchQ("");
                  }}
                >
                  <span className="flex min-w-0 items-center gap-2">
                    <span
                      className="h-2.5 w-2.5 shrink-0 rounded-full"
                      style={{ background: c.color }}
                    />
                    {/* **The name is laid out last:** if both segments are truncate, flex shrinks each by its own
                        width, and a slightly longer disambiguation term squeezes the name into "Ac…".
                        The disambiguation term shrinks four times faster and takes at most forty percent: it is a supplement; the name is
                        what this row is meant to be read for */}
                    <span className="truncate">{c.name}</span>
                    {c.disambiguator && c.disambiguator !== c.type_label && (
                      <span className="min-w-0 max-w-[40%] shrink-[4] truncate text-small text-ink-2">
                        · {c.disambiguator}
                      </span>
                    )}
                  </span>
                </Row>
              ))}
              {/* There are more not shown. **Say how many are left**: previously it was fixed at ten, and when the one you wanted
                  was not among the ten, the interface offered no clue at all. Searching within the subgraph is client-side
                  filtering, so there is no such thing as "more" */}
              {!inSubgraph &&
                (candidates.data?.total ?? 0) > searchHits.length && (
                  <Button
                    variant="ghost"
                    size="sm"
                    className="w-full justify-start rounded-none border-t border-line"
                    onClick={() => setSearchLimit((n) => n + 20)}
                  >
                    {S.graph.searchMore(
                      candidates.data!.total - searchHits.length,
                    )}
                  </Button>
                )}
            </div>
          )}
        </div>
        {focusEntity && (
          <Button
            variant="secondary"
            className="glass-strong pointer-events-auto u-lift"
            onClick={() => setFocusEntity(null)}
          >
            {S.graph.backToOverview}
          </Button>
        )}

        {/* Legend (click to toggle type visibility). **Only the first LEGEND_MAX are shown; the rest fold into
            "+N classes"**: that row stretches sideways, and with many classes it wraps and pushes the canvas down; and a dozen
            identical pills in a row make it impossible to tell which matters */}
        {/* Flush with the search box: the pill and the input are both 32 high, so padding this by 4 would make it shorter */}
        <div className="pointer-events-auto flex flex-wrap gap-2">
          {legendShown.map(([key, t]) => (
            <Pill
              key={key}
              dim={hiddenTypes.has(key)}
              onClick={() =>
                setHiddenTypes((prev) => {
                  const next = new Set(prev);
                  if (next.has(key)) next.delete(key);
                  else next.add(key);
                  return next;
                })
              }
            >
              <span
                className={`h-2 w-2 ${t.shape === "square" ? "scale-90" : "rounded-full"}`}
                style={{ background: t.color }}
              />
              <span>{t.label}</span>
            </Pill>
          ))}

          {/* The number on the chip is **all the classes**, not just the folded few:
              opening it shows all of them (any one can be searched), and writing "+3" would promise something else */}

          {legendRest.length > 0 && (
            <Popover open={legendOpen} onOpenChange={setLegendOpen}>
              <PopoverTrigger asChild>
              <Pill active={legendOpen} title={S.graph.legendAllHint}>
                {S.graph.legendMore(types.length)}
                {/* A folded class that is being hidden gets a click. **Without a click it is a silent filter**:
                    you switch a class off in the panel and collapse the panel, and nothing on the interface says it was switched off any more */}
                {hiddenInRest > 0 && (
                  <span className="h-1.5 w-1.5 rounded-full bg-ink-2" />
                )}
              </Pill>
              </PopoverTrigger>
              <PopoverContent align="start" className="w-72 overflow-hidden p-0">
                  {/* The title row only says what this is; it no longer doubles as the close button */}
                  <div className="flex items-center gap-3 border-b border-line px-4 py-3">
                    <span className="min-w-0 flex-1 truncate text-body font-medium text-ink">
                      {S.graph.legendMore(types.length)}
                    </span>
                  </div>
                  {/* All on / all off. **Moved here from the separate capsule on the top bar**: it only appears while something is hidden,
                      so the width of that row jumped around whenever you clicked a class; and what it does
                      (the opposite of "narrow the picture") belongs to this list, not outside it.
                      Both stay visible and grey out when unavailable: an exit that disappears
                      leaves you having to remember where it was the second time you need it */}
                  <div className="flex items-center gap-2 border-b border-line px-4 py-2">
                    <LinkButton
                      disabled={hiddenTypes.size === 0}
                      onClick={() => setHiddenTypes(new Set())}
                    >
                      {S.graph.legendShowAll(hiddenTypes.size)}
                    </LinkButton>
                    <LinkButton
                      className="ml-auto"
                      disabled={hiddenTypes.size === types.length}
                      onClick={() =>
                        setHiddenTypes(new Set(types.map(([k]) => k)))
                      }
                    >
                      {S.graph.legendHideAll}
                    </LinkButton>
                  </div>
                  {/* Search has no box of its own (bare): it is part of the panel, not a control placed inside it,
                      the same approach as the base switcher's search */}
                  <div className="border-b border-line px-4 py-3">
                    <Input
                      bare
                      autoFocus
                      value={legendQ}
                      onChange={(e) => setLegendQ(e.target.value)}
                      placeholder={S.graph.legendSearch}
                      className="w-full text-body"
                    />
                  </div>
                  {/* **Lists all the classes, not only the folded ones**: when you look for a class,
                      nobody remembers whether it happened to land among the first few */}
                  <div className="u-scroll flex max-h-64 flex-col overflow-y-auto px-2 py-1">
                    {types
                      .filter(([, t]) =>
                        t.label.toLowerCase().includes(legendQ.toLowerCase()),
                      )
                      .map(([key, t]) => (
                        /* **Two buttons in a row, not one button cycling through three states.**
                           The cost of a single cycling key: without looking at the current state you cannot tell
                           what the next click will do, and getting from "only this" back to normal has to pass through "exclude"
                           — to clear it you first have to turn the picture into another wrong shape.
                           Split apart, each gesture has a fixed meaning */
                        <div
                          key={key}
                          className={HOVER_ROW}
                        >
                          <Button
                            variant="ghost"
                            size="sm"
                            className="min-w-0 flex-1 justify-start px-0"
                            onClick={() =>
                              setHiddenTypes((prev) => {
                                const next = new Set(prev);
                                if (next.has(key)) next.delete(key);
                                else next.add(key);
                                return next;
                              })
                            }
                          >
                            <span
                              className={`h-2 w-2 shrink-0 ${t.shape === "square" ? "scale-90" : "rounded-full"}`}
                              style={{
                                background: t.color,
                                opacity: hiddenTypes.has(key) ? 0.35 : 1,
                              }}
                            />
                            <span
                              className={cn(
                                "truncate text-small",
                                hiddenTypes.has(key) ? "text-ink-2 line-through" : "text-ink",
                              )}
                            >
                              {t.label}
                            </span>
                          </Button>
                          {/* "Only this one": the action people most want when there are many classes. **An explicit button rather than
                              a modifier key**: alt+click is not something anyone would guess, and there is room for it here */}
                          <Button
                            variant="ghost"
                            size="sm"
                            className={cn(REVEAL, "shrink-0")}
                            onClick={() =>
                              setHiddenTypes(
                                new Set(
                                  types.map(([k]) => k).filter((k) => k !== key),
                                ),
                              )
                            }
                          >
                            {S.graph.legendOnly}
                          </Button>
                          <span className="u-num shrink-0 text-fine text-ink-2">
                            {t.count}
                          </span>
                        </div>
                      ))}
                    {types.every(
                      ([, t]) =>
                        !t.label.toLowerCase().includes(legendQ.toLowerCase()),
                    ) && (
                      <div className="px-2 py-2 text-small text-ink-2">
                        {S.graph.legendNone}
                      </div>
                    )}
                  </div>
              </PopoverContent>
            </Popover>
          )}
        </div>

        {/* Top right: adjusts "how many to draw" plus the statistics. **The statistic states exactly this number**
            ("drawing 150 of 548"); putting the control beside it makes clear what is being changed.
            The shell stays neutral: this area is chrome, and colour belongs only to the data */}
        <div className="ml-auto flex flex-col items-end gap-1">
          <div className="flex items-start gap-2">
            <div className="pointer-events-auto flex items-center overflow-hidden rounded-control border border-line">
            <IconButton
              size="sm"
              label={S.graph.nodeBudgetLess}
              disabled={nodeBudget <= NODE_BUDGETS[0]}
              onClick={() =>
                setNodeBudget(
                  (b) => NODE_BUDGETS[Math.max(0, NODE_BUDGETS.indexOf(b) - 1)],
                )
              }
            >
              −
            </IconButton>
            {/* **When the drawing is full, do not offer "draw more"**: there are only this many in the base, raising it changes nothing,
                and a button that does nothing when clicked is worse than no button */}
            <IconButton
              size="sm"
              label={S.graph.nodeBudgetMore}
              disabled={
                !capped || nodeBudget >= NODE_BUDGETS[NODE_BUDGETS.length - 1]
              }
              onClick={() =>
                setNodeBudget(
                  (b) =>
                    NODE_BUDGETS[
                      Math.min(NODE_BUDGETS.length - 1, NODE_BUDGETS.indexOf(b) + 1)
                    ],
                )
              }
            >
              +
            </IconButton>
          </div>
          <div className="pointer-events-none pt-1 u-num text-fine text-ink-2">
          {/* When the cap is reached, say clearly "drawing X of Y". **This number used to pass the cap off as the size**:
              a base with ten thousand entities always showed 150 in the top-right corner */}
          {/* Until the graph is ready, write nothing at all. **"0 entities · 0 facts" is a conclusion**,
              whereas right now we simply do not know yet: the spinner is turning beside it, and two such statements together would contradict each other */}
          {data.isPending ? null : capped ? (
            <span title={S.graph.cappedHint(nodeCount, totalNodes)}>
              {/* **Facts also use the "drawn / total" basis**: previously this showed the base total,
                  while entities showed "how many drawn / how many total": two bases in one sentence,
                  so when the tier changed, the entity count moved while the fact count stayed put, which looked broken.
                  With no time filter, active always equals the drawn count, so there is nothing to say */}
              {S.graph.statsCapped(
                nodeCount,
                totalNodes,
                edgeCount,
                totalEdges,
                timeT === null ? null : activeCount,
              )}
            </span>
          ) : (
            S.graph.stats(
              nodeCount,
              edgeCount,
              timeT === null ? null : activeCount,
            )
          )}
            </div>
          </div>
          {/* **On its own line, with no statistic-text prefix.**
              As a prefix it widened the whole block as soon as it appeared, and this block is right-aligned,
              so on every relayout the tier buttons on the left jostled.
              Taking its own line means the first row's width no longer changes with it */}
          {stabilizing && (
            <div className="flex items-center gap-2 text-fine text-ink-2">
              <Loader2 size={11} className="animate-spin" />
              {S.graph.stabilizing}
            </div>
          )}
        </div>
      </div>

      {/* Canvas: the world-coordinate grid layer (it moves with the camera) sits under the sigma WebGL layer (full bleed, the time island floats above it) */}
      <div className="absolute inset-0">
        <canvas ref={gridRef} className="absolute inset-0 h-full w-full" />
        <div ref={containerRef} className="absolute inset-0" />
      </div>

      {/* Bottom-left control tower: inferred edges + layout switch + camera (bottom right belongs to the entity sidebar, bottom centre to the time island) */}
      {/* **items-start**: items in a column default to stretch, so expanding one group would pull the others
          to the same width; those groups' text stays collapsed, so they look like meaningless empty bars.
          Each group sizing to its own content is what makes "expand one group at a time without affecting the others" */}
      <div className="absolute bottom-4 left-3 z-10 flex flex-col items-start gap-2">
        {/* Inferred edges: **a group of their own, and not in the type legend.**
            The legend answers "which classes are shown", a row entirely of ontology classes; this one answers
            "whether inferred edges are shown", which is a different question. When zero, the whole group does not appear.

            **Putting it on this tower is a way around a conflict**: beside the top-bar legend it would look
            like a 10th class; trying to tell it apart by colour would run into this file's stated principle
            — "chrome has no colour cast, colour belongs only to the data" (see the palette comment).
            A solid high-saturation gold block in the frame would be the only coloured block in the whole interface, eye-catching and unsystematic.

            This tower is already the territory of "how the view is looked at" (layout, zoom),
            and "whether inferred edges are shown" is the same family of question. The shell stays neutral,
            and gold appears only on the icon itself, the same approach as the colour dot on the class pills. */}
        {derivedCount > 0 && (
          /* The panel now uses a Popover (portalled to body) and is no longer a child of the tower,
             so this outer ring only separates it from the next tower */
          <div className="relative">
            <ToolTower>
              <ToolButton
                role="switch"
                aria-checked={showDerived}
                active={showDerived}
                label={S.graph.viewDerived}
                title={`${S.graph.derivedEdges(derivedCount)} · ${S.graph.derivedHint}`}
                icon={<Waypoints size={15} />}
                style={
                  showDerived ? { color: EDGE_FOCUS_DERIVED } : undefined
                }
                onClick={() => setShowDerived((v) => !v)}
              />
              <ToolDivider />
              {/* Expands into a small window: when this batch of edges was inferred, whether it is still inferring, and running it again manually.
                  **Kept as a separate button from the switch**: "hide them" is clicked every day, while "when were they inferred"
                  is asked only occasionally; merging them would add a step to the common action */}
              <Popover open={derivedOpen} onOpenChange={setDerivedOpen}>
                <PopoverTrigger asChild>
                  <ToolButton
                    active={derivedOpen}
                    label={S.graph.derivedPanel}
                    icon={
                      <span className="grid h-4 w-4 shrink-0 place-items-center leading-none">
                        ⋯
                      </span>
                    }
                  />
                </PopoverTrigger>
                {kb && (
                  <PopoverContent side="right" align="end" className="w-72 p-0">
                    <DerivedPanel kbId={kb.id} count={derivedCount} />
                  </PopoverContent>
                )}
              </Popover>
            </ToolTower>
          </div>
        )}
        <ToolTower>
          {(
            [
              { key: "force", Icon: Orbit, label: S.graph.layoutForce },
              {
                key: "circular",
                Icon: CircleDashed,
                label: S.graph.layoutCircular,
              },
              { key: "pack", Icon: Grape, label: S.graph.layoutPack },
            ] as const
          ).map(({ key, Icon, label }) => (
            <ToolButton
              key={key}
              active={layoutMode === key}
              label={label}
              icon={<Icon size={15} />}
              onClick={() => {
                setLayoutMode(key);
                layoutModeRef.current = key;
                layoutCtlRef.current?.apply(key);
              }}
            />
          ))}
        </ToolTower>
        <ToolTower>
          <ToolButton
            label={S.graph.zoomIn}
            icon={<ZoomIn size={15} />}
            onClick={() =>
              sigmaRef.current?.getCamera().animatedZoom({ duration: 220 })
            }
          />
          <ToolButton
            label={S.graph.zoomOut}
            icon={<ZoomOut size={15} />}
            onClick={() =>
              sigmaRef.current?.getCamera().animatedUnzoom({ duration: 220 })
            }
          />
          <ToolDivider />
          <ToolButton
            label={S.graph.fitView}
            icon={<Maximize2 size={15} />}
            onClick={() =>
              sigmaRef.current?.getCamera().animatedReset({ duration: 300 })
            }
          />
        </ToolTower>
      </div>

      {/* The graph is not here yet. **The tower of controls at bottom left and the row on top are already in place**; only the thing in the middle of the canvas is missing,
          so this layer is a spinner laid over it, not a replacement of the whole block.
          Kept apart from the empty state: the empty state is a conclusion saying "what to do next", while this ring is a process,
          and if the two looked alike they would be read as the same thing */}
      {data.isPending && <CanvasLoading />}

      {/* pb lifts this block 40px above the geometric centre: the visual centre of mass sits a bit higher than the geometric centre,
          so a short text block centred exactly always looks low */}
      {empty && (
        <div className="absolute inset-0 grid place-items-center pb-20 pointer-events-none">
          {/* No title block: the page is the graph page itself, and the tab bar already says so;
              writing "graph" a third time carries no information. The empty state should say what to do next,
              and "next" differs by person: admins can configure the model directly, others can only go find an admin (#267).
              This step is now decided uniformly by readiness (#313): while documents are being extracted, people should not still be sent to configure the model */}
          <div className="pointer-events-auto">
            <NextStep
              {...(step ?? {
                // The model is there, the documents have come in, extraction has finished, and the graph is still empty
                line: S.steps.nothingExtracted,
                action: {
                  label: S.steps.openLibrary,
                  to: "/kb/$kbId/library",
                  params: { kbId },
                },
              })}
            />
          </div>
        </div>
      )}

      {/* Bottom-centre floating time island */}
      {edgeCount > 0 && (
        <TimeScrubber
          edges={data.data!.edges}
          value={timeT}
          onChange={setTimeT}
          playing={playing}
          onPlayingChange={setPlaying}
        />
      )}

      {/* Entity sidebar. **After deselecting, it keeps showing for another 170ms**: during that time it is playing its exit */}
      {(selected || exiting) && kb && (
        <EntityPanel
          kbId={kb.id}
          entityId={(selected ?? exiting)!}
          exiting={!selected}
          intent={panelIntentRef}
          onClose={deselect}
          pinnedFact={pinnedFact}
          onPointFact={pointFact}
          onFocusFact={focusFact}
          onPointEntity={pointEntity}
          onNavigate={(id) => {
            // The jump target may not be on the current canvas: also refocus the graph on its neighbourhood (consistent with a search selection)
            setFocusEntity(id);
            setSelected(id);
          }}
        />
      )}
    </div>
  );
}

/* ============ Timeline (bottom-centre floating island: play + density band + drag) ============ */

/** Track clientX → the time value, snapped to day steps (the data precision is the day; dragging refines, while playback still advances by month for rhythm). */
/** The margin at the two ends of the track, **equal to the track's own corner radius** (`rounded-control`).
 *  The bars and the playhead are indented by this much: without it, the outermost bars would fall right into the curve of the corner
 *  and look cut off; when the playhead reaches the end it would also stick to the arc. The position conversion shrinks along with it,
 *  otherwise a click at the far left of the track would give a value slightly off from where it appears. */
const SCRUB_INSET = 10;

function scrubValueAt(
  clientX: number,
  track: HTMLDivElement | null,
  minTs: number,
  maxTs: number,
): number {
  if (!track) return maxTs;
  const rect = track.getBoundingClientRect();
  // Avoid division by zero producing NaN when the layout has not formed yet (width 0)
  if (rect.width < 1) return maxTs;
  const span = rect.width - SCRUB_INSET * 2;
  if (span < 1) return maxTs;
  const frac = Math.min(
    1,
    Math.max(0, (clientX - rect.left - SCRUB_INSET) / span),
  );
  const raw = minTs + frac * (maxTs - minTs);
  return Math.min(maxTs, minTs + Math.round((raw - minTs) / DAY_MS) * DAY_MS);
}

/** The step size for playback and for the bars. **These two should simply be the same unit**: previously the bars were by year
 *  and playback by day, and nowhere on the interface could you say how long one cell was. */
type ScrubUnit = "year" | "month" | "day";

/** The maximum number of bars to draw. Beyond that, adjacent buckets are merged for drawing. **This only affects drawing, not
 *  the playback step**: at day units 15 years has over five thousand buckets, which cannot be drawn at one pixel each,
 *  but playback still advances one day per step. When buckets are merged, the hint says so rather than hiding it */
const SCRUB_MAX_BARS = 220;
/** The target duration to walk the whole track. **Independent of the unit**: the unit changes granularity and density,
 *  and should not also change "how long to wait"; at day units, one day per beat would take twenty minutes for 15 years */
const SCRUB_PLAY_MS = 18000;

function bucketStart(ts: number, unit: ScrubUnit): number {
  const d = new Date(ts);
  if (unit === "year") return Date.UTC(d.getUTCFullYear(), 0, 1);
  if (unit === "month")
    return Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), 1);
  return Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate());
}
function bucketNext(ts: number, unit: ScrubUnit): number {
  const d = new Date(ts);
  if (unit === "year") return Date.UTC(d.getUTCFullYear() + 1, 0, 1);
  if (unit === "month")
    return Date.UTC(d.getUTCFullYear(), d.getUTCMonth() + 1, 1);
  return ts + DAY_MS;
}

function TimeScrubber({
  edges,
  value,
  onChange,
  playing,
  onPlayingChange,
}: {
  edges: GraphEdge[];
  value: number | null;
  onChange: (v: number | null) => void;
  /* Playback state is held by Graph: the rendering layer must tell playback advancing apart from manual dragging */
  playing: boolean;
  onPlayingChange: (v: boolean) => void;
}) {
  const setPlaying = onPlayingChange;
  /* Default year: **most bases span years**, so on entry give the level that can be taken in at a glance */
  const [unit, setUnit] = useState<ScrubUnit>("year");
  /* The number of full traversals. **Used as the key**: re-triggering the same animation on the same element does not replay it;
     changing the key so it remounts is what makes it replay */
  const [sweep, setSweep] = useState(0);
  /* While the pointer is over the track, the part already passed is brightened. **It answers "how far have I got"**:
     when not playing, the whole track is one grey, so you cannot see where progress stopped, and that is exactly what a person
     wants to know when they move the pointer over it */
  const [trackHover, setTrackHover] = useState(false);
  const trackRef = useRef<HTMLDivElement>(null);
  const draggingRef = useRef(false);
  /* The drag landing point. **The playback loop has its own floating-point accumulator** and does not read value, or rounding error
     would build up frame after frame. So changing value alone does nothing: the next frame overwrites it back.
     Dragging puts the landing point here, and the loop takes over from the new position on the next frame */
  const seekRef = useRef<number | null>(null);
  const seek = (v: number) => {
    seekRef.current = v;
    onChange(v);
  };

  const { minTs, maxTs, bars, merged, trackW } = useMemo(() => {
    const now = Date.now();
    // Bars are binned by the moment each edge **starts to hold** (0022): an edge with no start is counted from its earliest evidence, so the
    // slider's range covers the moment it lights up, rather than excluding it from the histogram
    const froms = edges
      .map((e) => (e.holds_from ? Date.parse(e.holds_from) : NaN))
      .filter((t) => !Number.isNaN(t));
    const min = froms.length
      ? Math.min(...froms)
      : now - 5 * 365 * 24 * 3600 * 1000;
    // Align the start to a unit boundary, otherwise the first bar is half a cell and reads as if a piece of data were missing
    const start = bucketStart(min, unit);

    const counts = new Map<number, number>();
    for (const t of froms) {
      const k = bucketStart(t, unit);
      counts.set(k, (counts.get(k) ?? 0) + 1);
    }
    const raw: { ts: number; n: number }[] = [];
    for (let t = start; t <= now; t = bucketNext(t, unit))
      raw.push({ ts: t, n: counts.get(t) ?? 0 });

    // If they will not fit, merge buckets. **What is merged is the drawing, not the step**
    const group = Math.max(1, Math.ceil(raw.length / SCRUB_MAX_BARS));
    const cells: { ts: number; n: number }[] = [];
    for (let i = 0; i < raw.length; i += group) {
      const slice = raw.slice(i, i + group);
      cells.push({
        ts: slice[0].ts,
        n: slice.reduce((a, b) => a + b.n, 0),
      });
    }
    const peak = Math.max(1, ...cells.map((c) => c.n));

    // Larger unit → fewer buckets → shorter island; smaller → longer. **But the floor must be raised high enough**:
    // the fixed controls in the island (play button + unit selector + two years + date + All time/Now)
    // themselves need over four hundred pixels; at 320 the flex-1 track is squashed to 0,
    // and in practice no bar is visible at all; the whole thing looks empty.
    //
    // Once raised, the unit mainly changes **the thickness of each bar**: on the same track,
    // years are a dozen-odd chunky blocks, days are two hundred-plus thin lines. That explains the point better than stretching the whole track
    const w = Math.min(780, Math.max(660, 380 + cells.length * 2));

    return {
      minTs: start,
      maxTs: now,
      bars: cells.map((c) => ({ ts: c.ts, h: c.n / peak, n: c.n })),
      merged: group,
      trackW: w,
    };
  }, [edges, unit]);

  // Playback advances by day (the data is day precision), so dates flip past quickly; the overall rhythm is still ≈ one month per 260ms.
  // Time-driven by rAF: frame-rate independent, internal floating-point accumulation avoids rounding drift, and the value is only pushed when the day changes
  useEffect(() => {
    if (!playing) return;
    // The whole track takes about SCRUB_PLAY_MS regardless of unit; the unit only decides which cell the landing point rounds to
    const SPEED = (maxTs - minTs) / SCRUB_PLAY_MS;
    let raf = 0;
    let last = performance.now();
    let acc = value ?? minTs;
    let lastPushed = 0;
    const step = (now: number) => {
      // Someone has dragged: continue from the landing point rather than along the original path
      if (seekRef.current !== null) {
        acc = seekRef.current;
        seekRef.current = null;
      }
      acc += (now - last) * SPEED;
      last = now;
      if (acc >= maxTs) {
        setPlaying(false);
        onChange(null);
        // Reached the end: sweep a light across. **This is a wrap-up**: playback stops and the time jumps back to all time;
        // without some acknowledgement it looks as if it broke off midway; a light sweeping across says "this run has finished"
        setSweep((n) => n + 1);
        return;
      }
      // **Advances continuously, does not jump between buckets.** Previously it was rounded to `bucketStart` before being pushed, so at year units
      // one step was a whole year: the playhead jumped cell by cell, looking like stutter rather than movement.
      // The unit now only governs **display** (label precision, bar span), not the step of advancement.
      //
      // The cost is that pushes become dense (once per frame), and each push recomputes the active edges for the whole graph,
      // so it is capped at ~30fps: the eye cannot tell it from 60fps, and the recomputation is halved
      if (now - lastPushed >= 33) {
        lastPushed = now;
        onChange(Math.round(acc));
      }
      raf = requestAnimationFrame(step);
    };
    raf = requestAnimationFrame(step);
    return () => cancelAnimationFrame(raf);
    // Restarts only when the playback switch changes: acc is kept inside the loop, and value changing frame by frame should not rebuild the loop
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [playing, minTs, maxTs, unit]);

  // Shown to the day: aligned with the data's day-level valid_precision
  const label = (() => {
    if (value === null) return S.graph.allTime;
    const d = new Date(value);
    const mm = String(d.getUTCMonth() + 1).padStart(2, "0");
    const dd = String(d.getUTCDate()).padStart(2, "0");
    // Precision follows the unit: at year units, writing "2019-01-01" would be false precision
    if (unit === "year") return `${d.getUTCFullYear()}`;
    if (unit === "month") return `${d.getUTCFullYear()}-${mm}`;
    return `${d.getUTCFullYear()}-${mm}-${dd}`;
  })();

  const minYear = bars.length
    ? new Date(bars[0].ts).getUTCFullYear()
    : undefined;
  const maxYear = bars.length
    ? new Date(bars[bars.length - 1].ts).getUTCFullYear()
    : undefined;

  return (
    /* Width changes with the unit: larger unit → fewer buckets → shorter; smaller unit → more buckets → longer and denser.
       Still clamped within the viewport (the calc term), so a narrow screen does not push it out.
       Measured widths: year 320 / month 648 / day 760. */
    <div
      className={`glass-strong absolute bottom-4 left-1/2 -translate-x-1/2 z-10 rounded-overlay px-3 py-2 flex items-center gap-3 u-scrub-island${playing ? " u-solid" : ""}`}
      style={{ width: `min(${trackW}px, calc(100vw - 4rem))` }}
    >
      <IconButton
        variant="secondary"
        className="shrink-0"
        label={playing ? S.graph.pause : S.graph.play}
        onClick={() => {
          // Already at the far end (`Now`): playback has to start from the beginning. **Otherwise the first press does nothing:**
          // the accumulator's start is the end point, so on the first frame the loop decides playback is over and only clears the position to All time
          if (
            !playing &&
            (value === null || value >= maxTs - (maxTs - minTs) * 0.02)
          )
            onChange(minTs);
          setPlaying(!playing);
        }}
      >
        {/* Solid: the play/pause pair is the common glyph for media keys, while a hollow triangle looks like
            a collapse handle of the "expand" kind. lucide's icons are outline-only by default, so the fill has to be given explicitly */}
        {playing ? (
          <Pause size={13} fill="currentColor" stroke="none" />
        ) : (
          <Play size={13} fill="currentColor" stroke="none" />
        )}
      </IconButton>

      {/* Step size. **Playback and the bars share it**: previously the bars were by year and playback by day,
          and nowhere on the interface could you say how long one cell was */}
      <Segmented
        size="sm"
        className="shrink-0"
        value={unit}
        onChange={setUnit}
        options={(["year", "month", "day"] as const).map((u) => ({
          value: u,
          title: S.graph.scrubUnitHint,
          label:
            u === "year"
              ? S.graph.scrubUnitYear
              : u === "month"
                ? S.graph.scrubUnitMonth
                : S.graph.scrubUnitDay,
        }))}
      />

      <span className="shrink-0 u-num text-fine text-ink-2">
        {minYear}
      </span>

      {/* Density band track: an inset light well + a bar per year of fact volume */}
      <div
        ref={trackRef}
        onMouseEnter={() => setTrackHover(true)}
        onMouseLeave={() => setTrackHover(false)}
        className="relative h-9 min-w-[150px] flex-1 overflow-hidden rounded-control bg-surface"
      >
        {/* Unmounted by **React** when finished; **do not call `remove()` yourself**.
            It used to be `onAnimationEnd={(e) => e.currentTarget.remove()}`:
            pulling a React-managed node out of the DOM without React knowing. On the next sweep the
            key changed, React tried to remove the "old node", which was no longer in its parent,
            so removeChild threw NotFoundError, and the uncaught error unmounted and remounted the whole tree:
            the symptom was that **after two playback rounds in a row the interface looked as if it had refreshed** */}
        {sweep > 0 && (
          <span
            key={sweep}
            className="u-sweep"
            onAnimationEnd={() => setSweep(0)}
          />
        )}
        {/* **The gaps must shrink with density**: with a fixed 2px, at day units 216 bars have 215 gaps
            ≈ 430px, while the inner track is only ~455px wide, so the bars are squeezed to 0.1px and the whole thing looks empty.
            That is exactly how it was lost in practice. When bars are sparse, 2px keeps them countable; when dense, they merge into a density band to read */}
        <div
          className="absolute top-1.5 bottom-1.5 flex items-end"
          style={{
            left: SCRUB_INSET,
            right: SCRUB_INSET,
            gap: bars.length > 120 ? 0 : bars.length > 40 ? 1 : 2,
          }}
        >
          {bars.map((b) => {
            // Lit on entry (the bucket start is the criterion): the bar under the playhead is already covered; the common semantics of a progress bar
            const past = value !== null && b.ts <= value;
            const d = new Date(b.ts);
            const stamp =
              unit === "year"
                ? `${d.getUTCFullYear()}`
                : unit === "month"
                  ? `${d.getUTCFullYear()}-${String(d.getUTCMonth() + 1).padStart(2, "0")}`
                  : d.toISOString().slice(0, 10);
            return (
              <div
                key={b.ts}
                className="flex-1 flex items-end h-full"
                title={`${stamp} · ${b.n}${merged > 1 ? ` · ${S.graph.scrubBarMerged(merged)}` : ""}`}
              >
                <div
                  className="u-bar w-full"
                  style={{
                    height: `${Math.max(10, b.h * 100)}%`,
                    // Parts already swept stay brightened during playback, and return to normal brightness when stopped.
                    // **Parts not yet reached are pushed almost to invisibility**: they were 0.09,
                    // which is still visible on this background colour, so the right side of the playhead looked "lit" just like the left,
                    // and you could not tell how far it had got. Keep a little rather than zero:
                    // zero would pretend that stretch had no data, when it has simply not been reached yet
                    background:
                      value !== null && past && (playing || trackHover)
                        ? SCRUB_PLAY
                        : value === null || past
                          ? SCRUB_PAST
                          : SCRUB_FUTURE,
                  }}
                />
              </div>
            );
          })}
        </div>
        <input
          type="range"
          className="scrubber-range"
          /* The width:100% rule in the CSS has to yield, or the whole thing overflows once inset on both sides */
          style={{ left: SCRUB_INSET, right: SCRUB_INSET, width: "auto" }}
          min={minTs}
          max={maxTs}
          step={DAY_MS}
          value={value ?? maxTs}
          /* **Dragging keeps playing:** a drag means "I want to see that stretch", not "I want to stop":
             after release it should keep going from the new position to the end.
             (The `All time` / `Now` buttons still stop: those are explicit jumps, not scrubbing) */
          onChange={(e) => seek(Number(e.target.value))}
          // Native range drag gestures are disturbed by page-level mouse listeners (such as dragging a node on the canvas):
          // drive the drag with pointer capture ourselves, so clicks and drags go through the same calculation path
          onPointerDown={(e) => {
            draggingRef.current = true;
            try {
              e.currentTarget.setPointerCapture(e.pointerId);
            } catch {
              /* A synthetic event's pointerId may be invalid; ignore it */
            }
            seek(scrubValueAt(e.clientX, trackRef.current, minTs, maxTs));
          }}
          onPointerMove={(e) => {
            if (draggingRef.current)
              seek(scrubValueAt(e.clientX, trackRef.current, minTs, maxTs));
          }}
          onPointerUp={() => {
            draggingRef.current = false;
          }}
          onPointerCancel={() => {
            draggingRef.current = false;
          }}
        />
      </div>

      <span className="shrink-0 u-num text-fine text-ink-2">
        {maxYear}
      </span>

      <div className="w-[5.6rem] shrink-0 text-center u-num text-small text-ink">
        {label}
      </div>

      <div className="h-5 w-px shrink-0 bg-surface-3" />

      {/* Two-anchor segments: the anchor you are at is highlighted, and clicking jumps; while dragging to a day in between, neither lights up */}
      <Segmented<"all" | "now" | "none">
        size="sm"
        className="shrink-0"
        value={value === null ? "all" : maxTs - value < DAY_MS ? "now" : "none"}
        onChange={(k) => {
          setPlaying(false);
          onChange(k === "all" ? null : maxTs);
        }}
        options={[
          { value: "all", label: S.graph.allTime },
          { value: "now", label: S.graph.nowBtn },
        ]}
      />
    </div>
  );
}

/* ============ Entity sidebar ============ */

/* Extracted so vitest can test it; the UI section (`EntityPanel`) uses the same name in its inner closure */
/** The sentence itself gives no time, but the text of the section it sits in says which day it is about (0064): "as of that day".
 *
 * This is not a start: "the market is global" in the report says that was the case on that day, not from that day on, so it is not drawn as
 * an interval; a separate sentence says so, along with what the date is called in the document. A constant fact has no time dimension, so nothing is written */
export function asOf(f: EntityFact): string {
  if (f.temporal === "eternal" || f.valid_from || f.valid_to || !f.holds_from) return "";
  const date = fmtTime(f.holds_from, "day");
  return date ? S.graph.asOf(date, f.attested_by ?? null) : "";
}

export function fmtInterval(f: EntityFact): string {
  // Temporal = eternal: no interval is drawn.
  // The write side (`Validity::under(Eternal)`) wipes both from and to to null, so it is empty at render time,
  // which is the same as "this has no time dimension".
  if (f.temporal === "eternal") return "";
  // Temporal = event: a single point. `Validity::under(Event)` sets both from and to to the same moment
  // (the 0022 / #486 write-side contract). `from ~ from` reads like a mis-drawn interval, but an event is just one moment.
  // If from is null, fall back to to; if neither exists, it is empty (extraction failed, and the UI degrades to not drawing it)
  if (f.temporal === "event") {
    const moment = fmtTime(f.valid_from, f.valid_from_precision);
    if (moment) return moment;
    const at = fmtTime(f.valid_to, f.valid_to_precision);
    return at ?? "";
  }
  const from = fmtTime(f.valid_from, f.valid_from_precision);
  const to = fmtTime(f.valid_to, f.valid_to_precision);
  // **"Ended, but no one knows when" must never be shown as "to date".** This is the face this change is meant to fix:
  // the source explicitly says "former CEO of Weta Digital", yet the interface tells the reader he is still in office
  const endedUnknown = !f.valid_to && f.valid_to_precision === "unknown";
  if (!from && !to && !endedUnknown) return "";
  const end = to ?? (endedUnknown ? S.graph.endedUnknown : S.graph.ongoing);
  return from ? `${from} ~ ${end}` : `~ ${end}`;
}

/** An inferred fact, **its proof laid out below**.
 *
 * It is not collapsed: the whole reason this tier exists is that "this edge is not what someone said; this is how it came about",
 * and hiding the premises behind a click would hide the reason. The chain is at most twelve steps, and laid out it is not long. */
/** The small window beside the inference switch: **when and by what this batch of edges was inferred, and whether they are still accurate now**.
 *
 * It exists because freshness is invisible. Inference re-runs every hour, while facts change every time a document comes in:
 * a derived edge looks exactly as it did when it was inferred, yet the premises it rests on may have been retracted three minutes ago.
 * The switch alone cannot answer "as of when is the conclusion I am seeing".
 *
 * The manual button lives here rather than elsewhere: the person who wants to re-run is exactly the one who has just read these three lines and thinks the numbers are too old.
 */
function DerivedPanel({ kbId, count }: { kbId: string; count: number }) {
  const kb = useQuery({
    queryKey: ["kbOne", kbId],
    queryFn: () => api.kbDetail(kbId),
  });
  /* Re-running asks for confirmation, but **the second press of confirmation must land on a different button**.
     This product's gesture convention is "clicking the same control twice = take it back": the switch, ⋯ and legend pills
     all work this way. Putting "click again to execute" on the same button would make the same gesture here unexpectedly mean "execute",
     whereas everywhere else it means "cancel".
     So a click only **asks a question**, with two targets under the question: Cancel / Run.

     Nor does it use the site-wide DangerConfirm: that is the danger tier, with a red heading and possibly requiring the exact text typed,
     reserved for irreversible operations like deleting a base. Re-running inference is heavy but repeatable, and does not reach that tier */
  const qc = useQueryClient();
  const [armed, setArmed] = useState(false);
  const run = useMutation({
    mutationFn: () => api.runInference(kbId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["graph"] });
      qc.invalidateQueries({ queryKey: ["kbOne", kbId] });
    },
    onError: (e: Error) => toast.error(e.message),
  });

  const on = kb.data?.materialize_inferences ?? false;
  const last = kb.data?.last_inference_at;
  // "How long ago" is easier to read than a timestamp: the question is "is it fresh", not "what time is it"
  const age = last
    ? Math.round((Date.now() - new Date(last).getTime()) / 60000)
    : null;

  /* Only the content: positioning, closing and outside clicks all belong to the Popover (the same set as the three panels in the top bar).
     It used to open in place, growing up and to the right, and had to carry a close cross as well */
  return (
    <div className="px-3 pb-3 pt-3">
      {/* items-center rather than baseline: a button and a close key sit beside the title,
          and baseline alignment would make those two look as if they float upward */}
      <div className="flex items-center gap-2">
        <span className="text-body text-ink">
          {S.graph.derivedPanel}
        </span>
        {!armed && (
          <Button
            variant="secondary"
            size="sm"
            className="ml-auto"
            disabled={!on || run.isPending}
            title={on ? undefined : S.err.inference_off}
            onClick={() => setArmed(true)}
          >
            {run.isPending ? S.graph.derivedRunning : S.graph.derivedRun}
          </Button>
        )}
      </div>

      {/* The question and its two targets. **Cancel comes first**: the one you reach first when moving from "Run" is Cancel,
          so the one whose mistaken press costs less should be closer */}
      {armed && (
        <div className="mt-2 rounded-panel bg-surface p-2">
          <p className="text-fine leading-relaxed text-ink-2">
            {S.graph.derivedRunAsk}
          </p>
          <div className="mt-2 flex gap-2">
            <Button variant="ghost" size="sm" onClick={() => setArmed(false)}>
              {S.graph.derivedRunCancel}
            </Button>
            <Button
              variant="primary"
              size="sm"
              disabled={run.isPending}
              onClick={() => {
                setArmed(false);
                run.mutate();
              }}
            >
              {S.graph.derivedRunGo}
            </Button>
          </div>
        </div>
      )}

      <dl className="mt-2 space-y-1 text-fine">
        <div className="flex justify-between gap-3">
          <dt className="text-ink-2">{S.graph.derivedCountLabel}</dt>
          <dd className="u-num text-ink">{count}</dd>
        </div>
        <div className="flex justify-between gap-3">
          <dt className="text-ink-2">{S.graph.derivedStateLabel}</dt>
          <dd className={on ? "text-ink" : "text-warn"}>
            {on
              ? S.graph.derivedOn(kb.data!.inference_interval_minutes)
              : S.graph.derivedOff}
          </dd>
        </div>
        <div className="flex justify-between gap-3">
          <dt className="text-ink-2">{S.graph.derivedLastLabel}</dt>
          <dd className="u-num text-ink">
            {age === null ? S.graph.derivedNever : S.graph.derivedAgo(age)}
          </dd>
        </div>
      </dl>

      {/* The result of the last manual run stays here. **Say separately how many were inferred and how many were retracted**:
          "nothing changed" and "thirty were swapped out" are very different things */}
      {run.data && (
        <p className="mt-2 text-fine text-ink-2">
          {run.data.inserted === 0 && run.data.invalidated === 0
            ? S.graph.derivedNoChange
            : S.graph.derivedChanged(run.data.inserted, run.data.invalidated)}
          {run.data.capped > 0 &&
            ` · ${S.graph.derivedCapped(run.data.capped)}`}
        </p>
      )}

    </div>
  );
}

/** An inferred edge. **Row style aligned with FactRow**: the same rounded row, the same
 *  chevron expansion, the same role="link" jump (avoiding a button inside a button).
 *
 *  It used to be a `glass rounded-panel p-3` card that showed the proof permanently expanded. In a column of compact
 *  Relations/Timeline/History rows it looked like a different product, and a dozen-odd
 *  stacked derivations made a wall. A proof is something you look at when you ask, so tucking it into an expandable area is right. */
function DerivedRow({
  kbId,
  d,
  otherId,
  otherName,
  open,
  onToggle,
  onNavigate,
}: {
  kbId: string;
  d: DerivedFact;
  /** A literal-valued conclusion has no entity on the other end (0021)*/
  otherId: string | null;
  otherName: string;
  open: boolean;
  onToggle: () => void;
  onNavigate: (entityId: string) => void;
}) {
  return (
    <ExpandCard
      open={open}
      onToggle={onToggle}
      header={
        <div className="flex items-center gap-2">
        {/* The conclusion of a business rule is a literal (one class, one value), and there is no entity on the other end to jump to:
            so it is drawn as plain text, rather than a link that does nothing when clicked (0021) */}
        {otherId ? (
          <span
            role="link"
            tabIndex={0}
            onClick={(ev) => {
              ev.stopPropagation();
              onNavigate(otherId);
            }}
            onKeyDown={(ev) => {
              if (ev.key === "Enter") {
                ev.stopPropagation();
                onNavigate(otherId);
              }
            }}
            className="u-inline-link truncate text-body text-ink"
          >
            {otherName}
          </span>
        ) : (
          <span className="truncate text-body text-ink">{otherName}</span>
        )}
        <span className="ml-auto shrink-0 pl-2 u-num text-fine text-ink-2">
          {d.premises.length}
        </span>
        </div>
      }
    >
      {/* Proof: premises in derivation order, each expanded to its original sentence (0002 R2). In the same place and with the same indentation as EvidenceList:
          the two are the same thing in two forms, one giving the source and the other the chain of reasoning */}
      {open && <ProofChain kbId={kbId} d={d} />}
    </ExpandCard>
  );
}

/** The proof chain of a derived edge. Fetched only on expansion: a proof is something you look at when you ask.
 *
 *  Each step is an asserted premise: the triple above, its original sentence below, and the original sentence can be clicked through to the document.
 *  A premise that has been retracted is marked but not hidden: the derivation is invalidated along with it, and "what it relied on at the time" is exactly what the record axis answers.
 *  If it cannot be fetched (the derivation is no longer valid), fall back to the text lines carried over from the list rather than leaving it empty. */
function ProofChain({ kbId, d }: { kbId: string; d: DerivedFact }) {
  const proof = useQuery({
    queryKey: ["proof", d.id],
    queryFn: () => api.derivedProof(kbId, d.id),
  });
  const steps = proof.data?.proof?.steps;
  return (
    <div>
      {proof.isPending && (
        <p className="text-fine text-ink-2">{S.graph.proofLoading}</p>
      )}
      {steps && <ProofSteps kbId={kbId} steps={steps} />}
      {/* The derivation is no longer valid and the proof cannot be fetched: fall back to the text lines carried over from the list */}
      {!proof.isPending && !steps && (
        <ol className="space-y-1">
          {d.premises.map((p, i) => (
            <li key={i} className="text-fine text-ink-2">
              {p}
            </li>
          ))}
          {d.premises.length === 0 && (
            <li className="text-fine text-ink-2">{S.graph.derivedNoProof}</li>
          )}
        </ol>
      )}
    </div>
  );
}

/** Proof steps: landed and not-yet-landed derivations share them, since the premises are the same kind of thing.
 *
 * Recursive: each step's `premises` are its sub-proof, which is nested as another
 * `<ol>` inside its own `<li>`. The visual depth limit is decided by the server (the same gate as 0030 inference), and is not
 * redrawn on the client; a leaf step has empty `premises`, and the recursion stops there
 */
export function ProofSteps({
  kbId,
  steps,
  depth = 0,
}: {
  kbId: string;
  steps: ProofStep[];
  /** Recursion depth: the top level starts at 0, and each level of sub-proof adds 1. Beyond 6 there is no further indent (narrow screens) */
  depth?: number;
}) {
  return (
    <ol className="space-y-2">
      {steps.map((st) => (
        <ProofStepRow key={st.fact_id} kbId={kbId} step={st} depth={depth} />
      ))}
    </ol>
  );
}

/** One row of a proof tree once flattened: keeps the depth and the parent chain, which the unit tests assert against */
export interface WalkedRow {
  fact_id: string;
  depth: number;
  subject: string;
  predicate: string | null;
  object: string | null;
  retracted: boolean;
  has_premises: boolean;
  evidence_count: number;
}

/** Flattens the recursive `premises` tree into rows. Depth-first, with sub-proofs placed after their parent.
 *
 * This is the "skeleton" of the recursive rendering: the component walks `premises` recursively, and unit tests use this function
 * to assert "with three levels of nesting, leaves no longer expand, depth increases monotonically, and adjacent rows differ in depth by at most 1".
 * Rendering is the component's job; the shape is the walker's job
 */
export function walkProofSteps(
  steps: ProofStep[],
  depth = 0,
): WalkedRow[] {
  const out: WalkedRow[] = [];
  const visit = (nodes: ProofStep[], d: number) => {
    for (const s of nodes) {
      out.push({
        fact_id: s.fact_id,
        depth: d,
        subject: s.subject,
        predicate: s.predicate,
        object: s.object,
        retracted: s.retracted,
        has_premises: s.premises.length > 0,
        evidence_count: s.evidence.length,
      });
      if (s.premises.length > 0) {
        visit(s.premises, d + 1);
      }
    }
  };
  visit(steps, depth);
  return out;
}

/** Indent up to this level and no further. Deeper, and there is no more indent: each level of ml-4 + pl-3 is 28 px, and six levels already
 *  eat 168 px, while the panel is only 384 px wide. The server's recursion cap (0030) governs how long the chain can be;
 *  this governs what stays legible on a narrow screen */
export const PROOF_INDENT_CAP = 6;

/** The indent class for the sub-proof block.
 *
 * **Build the base class conditionally, do not stack overriding classes**: `cn` is pure concatenation (see `ui/index.tsx`), and the repo
 * has no tailwind-merge either. When `ml-4` and `ml-0` both appear, which takes effect is decided by the order in the generated stylesheet,
 * not by the order written here, so such a cap would be no cap at all */
export function proofIndentClass(depth: number): string {
  return depth >= PROOF_INDENT_CAP
    ? "mt-1"
    : "mt-1 ml-4 pl-3 border-l border-edge";
}

/** A single proof step: its "sub-proof" is rendered once more via ProofSteps in `step.premises`.
 *
 * It is split out so that the recursion is bounded: this one component draws only one level itself, and premises calls
 * the top-level component again (the same rendering logic, not a second copy), and the recursion ends naturally
 * when the `premises` array is empty. The recursion depth cap is set by the server (0030), not drawn on the client
 */
function ProofStepRow({
  kbId,
  step,
  depth,
}: {
  kbId: string;
  step: ProofStep;
  depth: number;
}) {
  return (
    <li className="text-fine">
      <div className="flex items-baseline gap-2 flex-wrap">
        <span className="u-num text-fine text-ink-2 shrink-0">
          {S.graph.proofStep(step.seq + 1)}
        </span>
        <span className={step.retracted ? "text-ink-2 line-through" : "text-ink-2"}>
          {step.subject}
          <span className="text-ink-2"> — {step.predicate ?? "?"} → </span>
          {step.object ?? "?"}
        </span>
        {step.retracted && (
          <Chip tone="warn" className="text-fine">{S.graph.proofRetracted}</Chip>
        )}
      </div>
      <div className="mt-1 space-y-1 pl-2">
        {step.evidence.map((ev) => (
          <Link
            key={ev.chunk_id}
            to="/kb/$kbId/doc/$docId"
            params={{ kbId, docId: ev.document_id }}
            search={{ chunk: ev.chunk_id }}
            className="u-hover-ink block text-ink-2"
          >
            <div className="line-clamp-2 italic">
              {ev.quote ? `“${ev.quote}”` : S.graph.noQuote}
            </div>
            <div className="mt-1 text-ink-2">
              {S.graph.sectionRef(ev.filename, ev.seq + 1)}
              {ev.stale && (
                <span
                  className="ml-2 u-num text-fine text-ink-2"
                  title={S.graph.staleEvidenceHint}
                >
                  {S.graph.fromVersion(ev.doc_version)}
                </span>
              )}
              {ev.document_deleted && (
                <span
                  className="ml-2 text-fine text-contest"
                  title={S.graph.sourceDeletedHint}
                >
                  {S.graph.sourceDeleted}
                </span>
              )}
            </div>
          </Link>
        ))}
        {step.evidence.length === 0 && (
          <p className="text-ink-2">{S.graph.noEvidence}</p>
        )}
      </div>
      {step.premises.length > 0 && (
        <div className={proofIndentClass(depth)}>
          <ProofSteps kbId={kbId} steps={step.premises} depth={depth + 1} />
        </div>
      )}
    </li>
  );
}

/** An inferred edge that has not landed (0017 §3): a row like DerivedRow, with one more sentence on "what is blocking it",
 *  and expanding it shows its proof chain: here the person sees "the engine could have drawn this edge, and this is what stopped it" */
function BlockedRow({
  kbId,
  b,
  entityId,
  open,
  onToggle,
  onNavigate,
}: {
  kbId: string;
  b: BlockedDerivation;
  entityId: string;
  open: boolean;
  onToggle: () => void;
  onNavigate: (entityId: string) => void;
}) {
  const navigate = useNavigate();
  const out = b.subject_id === entityId;
  const otherId = out ? b.object_id : b.subject_id;
  const otherName = out ? b.object : b.subject;
  const proof = useQuery({
    queryKey: ["blocked-proof", b.violation_id],
    queryFn: () => api.blockedProof(kbId, b.violation_id),
    enabled: open,
  });
  return (
    <ExpandCard
      open={open}
      onToggle={onToggle}
      header={
        <div className="flex items-center gap-2">
        {out ? <ArrowRight size={10} className="shrink-0 text-ink-2" /> : <ArrowLeft size={10} className="shrink-0 text-ink-2" />}
        <span className="shrink-0 text-fine text-ink-2">{b.predicate}</span>
        <span
          role="link"
          tabIndex={0}
          onClick={(ev) => {
            ev.stopPropagation();
            onNavigate(otherId);
          }}
          onKeyDown={(ev) => {
            if (ev.key === "Enter") {
              ev.stopPropagation();
              onNavigate(otherId);
            }
          }}
          className="u-inline-link truncate text-body text-ink"
        >
          {otherName}
        </span>
        <span className="ml-auto shrink-0 pl-2 text-fine text-ink-2">
          {S.graph.ruleNames[b.rule] ?? b.rule}
        </span>
        </div>
      }
    >
      <div className="flex items-center gap-2 text-fine">
        <span className="truncate text-contest">
          {S.graph.blockedBy(b.against_text)}
        </span>
        <span
          role="link"
          tabIndex={0}
          onClick={() =>
            navigate({
              to: "/kb/$kbId/review",
              params: { kbId },
              search: { queue: "violations", item: b.violation_id },
            })
          }
          className="u-inline-link ml-auto shrink-0 text-ink-2"
        >
          {S.graph.blockedReview} →
        </span>
      </div>
      {open && (
        <div>
          {proof.isPending && (
            <p className="text-fine text-ink-2">{S.graph.proofLoading}</p>
          )}
          {proof.data?.steps && (
            <ProofSteps kbId={kbId} steps={proof.data.steps} />
          )}
        </div>
      )}
    </ExpandCard>
  );
}

/** Dispute chip (0017 §3): an open violation or conflict points at this assertion. The row is not dimmed:
 *  it is still alive. Clicking it goes to the matching tier in Review and lights up that card */
function ContestedChip({
  kbId,
  c,
}: {
  kbId: string;
  c: NonNullable<EntityFact["contested"]>;
}) {
  const navigate = useNavigate();
  const queue = c.kind === "temporal_conflict" ? "conflicts" : "violations";
  return (
    <span
      role="link"
      tabIndex={0}
      onClick={(ev) => {
        ev.stopPropagation();
        navigate({
          to: "/kb/$kbId/review",
          params: { kbId },
          search: { queue, item: c.ref_id },
        });
      }}
      onKeyDown={(ev) => {
        if (ev.key === "Enter") {
          ev.stopPropagation();
          navigate({
            to: "/kb/$kbId/review",
            params: { kbId },
            search: { queue, item: c.ref_id },
          });
        }
      }}
      className={chipLike("contest", "shrink-0 cursor-pointer text-fine")}
      title={S.graph.contestedHint(c.kind, c.derived ?? null)}
    >
      {S.graph.contestedChip}
    </span>
  );
}

function EntityPanel({
  kbId,
  entityId,
  exiting,
  intent,
  onClose,
  pinnedFact,
  onPointFact,
  onFocusFact,
  onPointEntity,
  onNavigate,
}: {
  kbId: string;
  entityId: string;
  /** Currently exiting: still attached to the DOM, but no longer accepts clicks */
  exiting: boolean;
  /** Which tier and which row to open on; cleared once read */
  intent?: MutableRefObject<{ view: "derived"; open: string } | null>;
  onClose: () => void;
  /** The fact that was clicked and is pinned on the canvas */
  pinnedFact: string | null;
  /** The pointer rests on a fact / leaves it (null) */
  onPointFact: (factId: string | null) => void;
  /** A fact was clicked: pin its edge and move the camera to it */
  onFocusFact: (factId: string, otherId: string | null) => void;
  /** The pointer rests on an entity name / leaves it */
  onPointEntity: (entityId: string | null) => void;
  onNavigate: (entityId: string) => void;
}) {
  const detail = useQuery({
    queryKey: ["entity", kbId, entityId],
    queryFn: () => api.entityDetail(kbId, entityId),
  });
  // RC-008: a type that declares how it is shown gets the hinted panel body
  const ontology = useQuery({ queryKey: ["ontology", kbId], queryFn: () => api.ontology(kbId) });
  const hints = useMemo(() => (ontology.data ? hintsOf(ontology.data) : null), [ontology.data]);
  const hinted =
    !!hints && !!detail.data && hasHints(detail.data.entity.type_key, detail.data.facts, hints);
  const navigateTo = useNavigate();
  const [openFact, setOpenFact] = useState<string | null>(null);
  // The inferred ones. **A separate key, not mixed into facts**: in one list the user cannot tell
  // "written in the document" apart from "the engine inferred it"
  const derived = detail.data?.derived ?? [];
  // Not yet landed (0017 §3): inferred, but it collided with an assertion
  const blocked = detail.data?.blocked ?? [];
  /* Grouped by "direction + predicate + rule"; the skeleton matches the groups in Relations.
     The rule hangs on the group rather than each row: it holds for the whole group, so repeating it per row is redundant,
     and that small amber caption would compete with the derived edges for hue */
  const derivedGroups = useMemo(() => {
    const map = new Map<
      string,
      {
        key: string;
        direction: "in" | "out";
        predicate: string;
        rule: string;
        rows: DerivedFact[];
      }
    >();
    for (const d of derived) {
      const direction = d.subject_id === entityId ? "out" : "in";
      // The four axioms each have a name; business rules use their own name (0021): "Gas-bearing well"
      // is far more meaningful than "business", and that name is what the person gave the rule when writing it.
      // **If it cannot be found, fall back to the raw kind string**: meaningless to the reader, but more honest than showing it as another rule
      const rule = d.rule_name ?? S.graph.ruleNames[d.rule] ?? d.rule;
      // Group keys use the rule name rather than the kind: two business rules on the same predicate are each filed under their own
      const key = `${direction}|${d.predicate}|${d.rule_name ?? d.rule}`;
      const cur = map.get(key);
      if (cur) cur.rows.push(d);
      else map.set(key, { key, direction, predicate: d.predicate, rule, rows: [d] });
    }
    return [...map.values()];
  }, [derived, entityId]);
  // Relations = grouped by relationship (look up relations); Timeline = the valid-time axis (when things held);
  // History = the record-time axis (when we believed it, and when we changed our mind)
  const [view, setView] = useState<"relations" | "history" | "derived">(
    "relations",
  );
  useEffect(() => {
    const it = intent?.current;
    if (!it) return;
    intent.current = null;
    setView(it.view);
    setOpenFact(it.open);
  }, [entityId, intent]);

  const e: GraphNode | undefined = detail.data?.entity;

  // Entity corrections (name, type) are in the dialog: the panel only shows
  const [editing, setEditing] = useState(false);
  /* **Other entities with the same name do not appear on this panel.** (There used to be a banner listing every
     same-named entity, each with a "merge into" option.) Two problems: those rows had only a type name to read, so seven same-named ones
     all said "Organization", and the interface asked "are these the same?" without giving the basis for judging; and the banner had no height cap, so
     a few more same-named entities pushed Relations / History / Derived off the screen. Merging belongs to Review, which has side-by-side comparison.
     screen. Merging belongs to Review, which has side-by-side comparison.
     This panel only answers "what is this entity I am looking at". */

  const openEdit = () => {
    if (!e) return;
    setEditing(true);
  };

  /* Relations is one table, no longer split into "current" and "chronicle" pages: two sections, **outgoing from this entity / pointing at this entity**,
     with one row per relation on each side: relation name on the left, entity name on the right, the same look as the ontology page (no longer split into
     sub-levels by predicate); sorted by relation name, then by start. What does not hold at the moment (judged by the read interval, per 0022) is folded into
     the "N past" at the end of the section. Wikidata keeps historical values in the same list, distinguished by end time: it is the same idea */
  const sections = useMemo(() => {
    const all = detail.data?.facts ?? [];
    const nowIso = new Date().toISOString();
    const current = (f: EntityFact) =>
      (!f.holds_from || f.holds_from <= nowIso) &&
      (!f.holds_to || f.holds_to > nowIso);
    const order = (a: EntityFact, b: EntityFact) =>
      // Sort by **the wording you actually see**, otherwise the order of `worksFor` and `accessTo` would not match
      // "works for" / "access to" as read on the screen
      predicateSentence(a.predicate_label ?? "￿").localeCompare(
        predicateSentence(b.predicate_label ?? "￿"),
      ) ||
      ((a.valid_from ?? "9999") < (b.valid_from ?? "9999") ? -1 : 1);
    const split = (dir: "out" | "in") => {
      const mine = all.filter((f) => f.direction === dir);
      return {
        rows: mine.filter(current).sort(order),
        past: mine.filter((f) => !current(f)).sort(order),
      };
    };
    return { out: split("out"), in: split("in") };
  }, [detail.data]);

  return (
    <div
      className={`${exiting ? "u-dock-out" : "u-dock-in"} glass-strong absolute top-14 right-3 bottom-20 w-96 z-10 rounded-overlay u-lift-strong flex flex-col`}
    >
      <div className="flex items-start justify-between px-4 py-4 border-b border-line">
        <div>
          {e && (
            <>
              <div className="flex items-center gap-2">
                <span
                  className="h-2.5 w-2.5 rounded-full shrink-0"
                  style={{
                    background: e.color,
                    boxShadow: `0 0 8px ${e.color}55`,
                  }}
                />
                <span
                  className="text-title font-semibold tracking-tight text-ink"
                  style={{ fontFamily: "var(--font-display)" }}
                >
                  {e.name}
                </span>
              </div>
              {/* When the disambiguation suffix finds no related fact, it falls back to the type label, which then repeats the type shown after it */}
              <div className="mt-1 text-small text-ink-2">
                {e.disambiguator && e.disambiguator !== e.type_label
                  ? `${e.disambiguator} · `
                  : ""}
                {e.type_label ?? S.graph.untyped} ·{" "}
                {detail.data?.facts.length ?? 0} {S.graph.facts}
              </div>
            </>
          )}
        </div>
        <div className="flex items-center gap-2 mt-1">
          {e && !editing && (
            <IconButton size="sm" label={S.graph.edit} onClick={openEdit}>
              <Pencil size={13} />
            </IconButton>
          )}
          <IconButton size="sm" label={S.graph.close} onClick={onClose}>
            <X size={15} />
          </IconButton>
        </div>
      </div>

      {editing && e && (
        <EntityDialog
          kbId={kbId}
          entityId={entityId}
          entity={e}
          onClose={() => setEditing(false)}
          onSaved={() => setEditing(false)}
        />
      )}


      {/* View switch: Relations (one table; the past is folded at the end of the group) | History (record axis) | Derived */}
      {/* **The left edge lines up with the collapse arrow in the column below** (16). They sit right above and below each other with nothing between,
          so the stretch reads as one vertical line; before, it was aligned to the title's text (pl-6, so the word "Relations"
          landed at 37 and pressed against the heading), but the heading sits in another box, separated by a divider line, and that
          alignment was not visible; what was visible was that it sat one step further right than the neatly aligned column below.
          16 = the scroll area's px-2 plus the row's own px-2, which is where each row's background starts;
          the 12px arrow is centred in the 16 icon cell, so the glyph is still 2px inside: those 2px are present on every icon in this
          product, so it is not a misalignment specific to this spot */}
      <div className="pl-4 pr-4 pt-3">
        <Segmented
          size="sm"
          value={view}
          onChange={setView}
          options={(["relations", "history", "derived"] as const)
            // The inferred tier: **does not appear when there is no derivation**. A base with inference not turned on should not see
            // an always-empty tab. Ones not yet landed count too: that is exactly what this tier is about
            .filter(
              (v) => v !== "derived" || derived.length > 0 || blocked.length > 0,
            )
            .map((v) => ({
              value: v,
              label:
                v === "relations"
                  ? S.graph.viewRelations
                  : v === "history"
                    ? S.graph.viewHistory
                    : S.graph.viewDerived,
            }))}
        />
      </div>

      <div className="u-scroll flex-1 overflow-y-auto px-2 py-2">
        {/* The name comes before the relations: first answer "what is it called", then "who is it related to". Only when the name is the sole entry
            is this section left out: the heading already says it, and listing it again is noise */}
        {view === "relations" && (detail.data?.names.length ?? 0) > 1 && (
          <NameSection kbId={kbId} entityId={entityId} names={detail.data!.names} />
        )}
        {view === "relations" && hinted && hints && detail.data && (
          <HintedPanel
            kbId={kbId}
            entity={detail.data.entity}
            facts={detail.data.facts}
            hints={hints}
            onNavigate={onNavigate}
            onOpenChain={() =>
              navigateTo({ to: "/kb/$kbId/chain", params: { kbId }, search: { entity: entityId } })
            }
            renderRow={(f) => (
              <FactRow
                key={f.id}
                kbId={kbId}
                dir={f.direction}
                fact={f}
                past={sections[f.direction].past.includes(f)}
                open={openFact === f.id}
                onToggle={() => setOpenFact(openFact === f.id ? null : f.id)}
                pinned={pinnedFact === f.id}
                onPointFact={onPointFact}
                onFocusFact={onFocusFact}
                onPointEntity={onPointEntity}
                onNavigate={onNavigate}
              />
            )}
          />
        )}
        {view === "relations" &&
          !hinted &&
          (["out", "in"] as const).map((dir) => {
            const { rows, past } = sections[dir];
            if (rows.length === 0 && past.length === 0) return null;
            const name = e?.name ?? "";
            return (
              <FactSection
                key={dir}
                dir={dir}
                title={dir === "out" ? S.graph.fromEntity(name) : S.graph.toEntity(name)}
                count={rows.length + past.length}
              >
                {rows.map((f) => (
                  <FactRow
                    key={f.id}
                    kbId={kbId}
                    dir={dir}
                    fact={f}
                    open={openFact === f.id}
                    onToggle={() => setOpenFact(openFact === f.id ? null : f.id)}
                    pinned={pinnedFact === f.id}
                    onPointFact={onPointFact}
                    onFocusFact={onFocusFact}
                    onPointEntity={onPointEntity}
                    onNavigate={onNavigate}
                  />
                ))}
                {past.length > 0 && (
                  <PastFold n={past.length}>
                    {past.map((f) => (
                      <FactRow
                        key={f.id}
                        kbId={kbId}
                        dir={dir}
                        fact={f}
                        past
                        open={openFact === f.id}
                        onToggle={() => setOpenFact(openFact === f.id ? null : f.id)}
                        pinned={pinnedFact === f.id}
                        onPointFact={onPointFact}
                        onFocusFact={onFocusFact}
                        onPointEntity={onPointEntity}
                        onNavigate={onNavigate}
                      />
                    ))}
                  </PastFold>
                )}
              </FactSection>
            );
          })}
        {view === "history" && (
          <EntityHistory kbId={kbId} entityId={entityId} />
        )}
{view === "derived" && (
          <>
            <p className="px-2 pb-2 pt-1 text-fine leading-relaxed text-ink-2">
              {S.graph.derivedHint}
            </p>
            {/* **Same skeleton as Relations**: direction arrow + predicate + a small heading with the count,
                Below are compact rows. Rules (transitive / symmetric) are merged into the heading: they hold for the whole group,
                repeating them on every row would be redundant, and that `--u-warn` amber is again a place
                competing with the derived edges for hue */}
            {derivedGroups.map((gr) => (
              <div key={gr.key} className="mb-3 last:mb-1">
                <GroupLabel
                  className="px-2 pb-1 pt-2"
                  icon={
                    gr.direction === "in" ? (
                      <ArrowLeft size={10} />
                    ) : (
                      <ArrowRight size={10} />
                    )
                  }
                  count={gr.rows.length > 1 ? gr.rows.length : undefined}
                >
                  {gr.predicate}
                  <span className="ml-2 font-normal text-ink-2">{gr.rule}</span>
                </GroupLabel>
                <div>
                  {gr.rows.map((d) => {
                    const out = d.subject_id === entityId;
                    return (
                      <DerivedRow
                        key={d.id}
                        kbId={kbId}
                        d={d}
                        otherId={out ? d.object_id : d.subject_id}
                        otherName={out ? d.object : d.subject}
                        open={openFact === d.id}
                        onToggle={() =>
                          setOpenFact(openFact === d.id ? null : d.id)
                        }
                        onNavigate={onNavigate}
                      />
                    );
                  })}
                </div>
              </div>
            ))}
            {blocked.length > 0 && (
              <div className="mb-3 last:mb-1">
                <GroupLabel className="px-2 pb-1 pt-2" tone="contest" count={blocked.length}>
                  {S.graph.blockedTitle}
                </GroupLabel>
                <p className="px-2 pb-2 text-fine leading-relaxed text-ink-2">
                  {S.graph.blockedHint}
                </p>
                {blocked.map((b) => (
                  <BlockedRow
                    key={b.violation_id}
                    kbId={kbId}
                    b={b}
                    entityId={entityId}
                    open={openFact === b.violation_id}
                    onToggle={() =>
                      setOpenFact(
                        openFact === b.violation_id ? null : b.violation_id,
                      )
                    }
                    onNavigate={onNavigate}
                  />
                ))}
              </div>
            )}
          </>
        )}
        {view !== "history" &&
          view !== "derived" &&
          detail.data?.facts.length === 0 && (
            <p className="text-body text-ink-2 p-2">{S.graph.noFacts}</p>
          )}
      </div>
    </div>
  );
}

/** A section (outgoing from this entity / pointing at this entity): collapsible. The fold handle takes the icon cell, and the body is indented by the same 24,
 *  so each row's direction arrow sits exactly beneath the section heading's arrow (the same set as the Relations group on the ontology page) */
function FactSection({
  dir,
  title,
  count,
  children,
}: {
  dir: "out" | "in";
  title: string;
  count: number;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(true);
  return (
    <div className="mb-2">
      <Row
        className="mt-1"
        icon={
          <span className="flex w-4 justify-center">
            <ChevronRight size={12} className={cn("u-turn", open && "rotate-90")} />
          </span>
        }
        onClick={() => setOpen((v) => !v)}
      >
        <span className="flex items-center gap-2 text-small font-medium">
          {/* **The same size** as the direction arrow on each fact row below (12). They are the same mark,
              and deliberately arranged in one column, so a 2px offset on the heading would only read as misaligned */}
          {dir === "out" ? <ArrowRight size={12} /> : <ArrowLeft size={12} />}
          <span className="truncate">{title}</span>
          <span className="u-num">{count}</span>
        </span>
      </Row>
      {/* **The arrow sits in one column with the arrow in the heading** (measured, both at 40.7). Once changed to pl-8 to make the
          rows "hang" beneath the heading, the two arrows were 8px apart and looked misaligned; but they are
          the same direction mark (→ outgoing / ← incoming), and the same mark should line up in a column.
          The hierarchy is shown by the fold triangle in front of it (at 18.7), with no need to say it again through indentation */}
      {open && <div className="pl-6">{children}</div>}
    </div>
  );
}

/** Name section (0041): primary name, short forms, former names, one name per row.
 *
 *  A name is a fact, so a mis-recorded name is also a fact that can be retracted. When extraction records "OpenAI's" or "ChatGPT
 *  apps" as an alias, before, nothing on the interface showed it, and you only found out once it had merged two entities by mistake.
 *  Retraction goes through the reject-fact path (audited, visible in History); the primary name cannot be retracted, and renaming is in the edit dialog */
function NameSection({
  kbId,
  entityId,
  names,
}: {
  kbId: string;
  entityId: string;
  names: NameView[];
}) {
  const [open, setOpen] = useState(true);
  /* Removing a name asks first, and **the second press lands on a different button**: the same gesture as the re-infer case on the graph page
     (on this product, clicking the same control twice has always meant "take it back"). It falls short of the DangerConfirm tier
     (red heading, typing the text exactly, reserved for deleting a base), but it must not fire on one click either: a name is a fact (0041), and removing it
     voids that `known_as` edge, **and there is no place on the interface to add the name back**; a wrong click can only be fixed by re-extracting the document */
  const [armed, setArmed] = useState<string | null>(null);
  const qc = useQueryClient();
  const remove = useMutation({
    mutationFn: (factId: string) => api.rejectFact(kbId, factId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["entity", kbId, entityId] });
      toast.success(S.graph.nameRemoved);
    },
    onError: (err: Error) => toast.error(err.message),
  });
  return (
    <div className="mb-2">
      <Row
        className="mt-1"
        icon={
          <span className="flex w-4 justify-center">
            <ChevronRight size={12} className={cn("u-turn", open && "rotate-90")} />
          </span>
        }
        onClick={() => setOpen((v) => !v)}
      >
        <span className="flex items-center gap-2 text-small font-medium">
          <Tag size={12} />
          <span className="truncate">{S.graph.names}</span>
          <span className="u-num">{names.length}</span>
        </span>
      </Row>
      {open && (
        <div className="pl-6">
          {names.map((n) => {
            const until = n.valid_to ? fmtTime(n.valid_to, n.valid_to_precision) : null;
            return (
              <div key={n.fact_id} className={cn(HOVER_ROW, "items-start", until && "opacity-55")}>
                <span className="shrink-0 pt-1 text-violet">
                  <Tag size={12} />
                </span>
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-body text-ink" title={n.name}>
                    {n.name}
                  </span>
                  <span className="flex items-center gap-2 text-fine text-ink-2">
                    {n.canonical && <span>{S.graph.shownName}</span>}
                    {until && <span className="u-num">{S.graph.nameUntil(until)}</span>}
                    {n.evidence_count > 0 && <span>{S.graph.sources(n.evidence_count)}</span>}
                    {!n.canonical &&
                      (armed === n.fact_id ? (
                        /* The question and its two targets expand in place: within one row there is no other layout to switch to, and the question
                           must share a row with the name it asks about, otherwise "which one to remove" would have to be remembered */
                        <span className="ml-auto flex items-center gap-2 text-fine">
                          <span className="text-ink-2">{S.graph.removeNameAsk}</span>
                          <LinkButton onClick={() => setArmed(null)}>
                            {S.graph.removeNameCancel}
                          </LinkButton>
                          <LinkButton
                            className="text-danger"
                            disabled={remove.isPending}
                            onClick={() => {
                              setArmed(null);
                              remove.mutate(n.fact_id);
                            }}
                          >
                            {S.graph.removeNameGo}
                          </LinkButton>
                        </span>
                      ) : (
                        <LinkButton
                          className={cn(REVEAL, "ml-auto text-fine")}
                          disabled={remove.isPending}
                          onClick={() => setArmed(n.fact_id)}
                        >
                          {S.graph.removeName}
                        </LinkButton>
                      ))}
                  </span>
                </span>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

/** Ended ones stay in the same section, folded: by default you see the current ones; expand to see their history */
function PastFold({ n, children }: { n: number; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <Row
        icon={
          <span className="flex w-4 justify-center">
            <ChevronRight size={12} className={cn("u-turn", open && "rotate-90")} />
          </span>
        }
        onClick={() => setOpen((v) => !v)}
      >
        <span className="text-small">{S.graph.past(n)}</span>
      </Row>
      {open && <div className="pl-6">{children}</div>}
    </>
  );
}

/** A fact is one row, with the same look as the rows of the Relations on the ontology page: a direction arrow in the icon cell, the relation name on the left
 *  (with markers such as disputed), the name of the entity on the right (the small interval text sits before the name); clicking the whole row
 *  jumps to that entity. Only when the row is hovered do "N sources" (spread out as evidence below the row) and a pencil (the interval correction
 *  dialog) appear. There is no fold handle at the start of the row: on this panel fold handles belong only to sections and to the "past" group */
function FactRow({
  kbId,
  dir,
  fact,
  past,
  open,
  onToggle,
  pinned,
  onPointFact,
  onFocusFact,
  onPointEntity,
  onNavigate,
}: {
  kbId: string;
  dir: "out" | "in";
  fact: EntityFact;
  /** Ones that have ended: faded */
  past?: boolean;
  open: boolean;
  onToggle: () => void;
  pinned: boolean;
  onPointFact: (factId: string | null) => void;
  onFocusFact: (factId: string, otherId: string | null) => void;
  onPointEntity: (entityId: string | null) => void;
  onNavigate: (entityId: string) => void;
}) {
  const [editing, setEditing] = useState(false);
  const interval = fmtInterval(fact);
  /* **One row, three things, each with its own destination:**
     - the whole row (with the predicate) is the fact: hovering = the edge on the canvas lights up; clicking = pin it and move the camera over;
     - the object is another entity: hovering = the node on the canvas lights up; clicking = jump over to look at it.
     Before, clicking the whole row jumped to the object, and there was no way to look at "which edge it is".
     Literal-valued facts (`amount 13,237`) have no edge on the canvas, so this row does not point at the canvas, and clicking it does nothing */
  const isEdge = !!fact.other_id;
  const focus = isEdge ? () => onFocusFact(fact.id, fact.other_id) : undefined;
  const goOther = (ev: { stopPropagation: () => void }) => {
    ev.stopPropagation();
    if (fact.other_id) onNavigate(fact.other_id);
  };

  return (
    <div
      className={cn((fact.stale || past) && "opacity-55")}
      title={fact.stale ? S.graph.staleFactHint : undefined}
    >
      {/* **Two lines: the fact above, and what we know about it below.**
          It used to be one line, with the predicate, interval, object, evidence count and reschedule pencil all crammed in.
          After group indentation only 249px were left, and the trailing group is shrink-0, so the only thing that could shrink, the predicate,
          absorbed the whole deficit: in practice, of one entity's 144 rows, the predicate width was 0 on 35 of them, and it read
          as "→ 2023-03-02 ~ now  Project Aurora": it says there is such a fact, but not which one (#500). The predicate is the main clause of this line
          and should not be the first thing squeezed out.
          The two actions that appear only on hover came down too: `u-reveal` only changes opacity, so they are invisible yet still take up space */}
      <div
        role={focus ? "button" : undefined}
        tabIndex={focus ? 0 : undefined}
        onClick={focus}
        onMouseEnter={isEdge ? () => onPointFact(fact.id) : undefined}
        onMouseLeave={isEdge ? () => onPointFact(null) : undefined}
        onFocus={isEdge ? () => onPointFact(fact.id) : undefined}
        onBlur={isEdge ? () => onPointFact(null) : undefined}
        onKeyDown={(ev) => {
          if (focus && ev.key === "Enter") focus();
        }}
        aria-pressed={focus ? pinned : undefined}
        className={cn(
          HOVER_ROW,
          "items-start",
          focus && "cursor-pointer",
          pinned && "bg-surface-2",
        )}
      >
        <span className="shrink-0 pt-1 text-violet">
          {dir === "out" ? <ArrowRight size={12} /> : <ArrowLeft size={12} />}
        </span>
        <span className="min-w-0 flex-1">
          {/* First line: predicate + object, which reads as the fact itself.
              **The object follows the predicate directly**, not pushed to the right: the subject is the entity on this panel, and this line
              is its second half of the sentence; pushing the object to the end of the line leaves a whole line of blank space in between, and the two words no longer
              read as one sentence. The predicate takes what its content needs and shrinks only when it does not fit; the rest goes to the object */}
          <span className="flex items-center gap-2">
            <span
              className={cn(
                "min-w-0 truncate text-body text-ink",
                isEdge && POINT_WORD,
                fact.predicate_label === null && "italic text-ink-2",
              )}
              // The predicate can still grow too long to fit (`publishingPrinciples`): hover gives the full name,
              // and a relation the ontology has adopted does not need to be called the source text's wording
              title={
                fact.predicate_label
                  ? fact.inferred
                    ? `${predicateSentence(fact.predicate_label)} · ${S.graph.inferredPredicate}`
                    : // A relation the ontology has adopted is computed from the source text's wording (0044 decision 1):
                      // hovering shows the original sentence; an edge left undrawn on the canvas does not mean it has been hidden
                      [
                        predicateSentence(fact.predicate_label),
                        fact.said_as ? S.graph.saidAs(fact.said_as) : null,
                      ]
                        .filter(Boolean)
                        .join(" · ")
                  : undefined
              }
            >
              {/* **This line is one sentence, so the predicate is read apart** (see predicateText.ts):
                  the store holds the form the vocabulary should have, `worksFor`; placed
                  between "Li Si — ? — Meridian Systems" it reads as "works for". The ontology pages that govern the ontology still show camelCase as before,
                  since there the word itself is what is recognised */}
              {fact.predicate_label
                ? predicateSentence(fact.predicate_label)
                : S.graph.unknownPredicate}
            </span>
            {isEdge ? (
              <span className={cn(ROW_VALUE, "flex-none")}>
                <span
                  role="link"
                  tabIndex={0}
                  className={POINT_WORD}
                  title={S.graph.openEntity(fact.other_name ?? "")}
                  onClick={goOther}
                  onMouseEnter={() => {
                    // Pointing at the object means that entity, not this edge: the edge is put down first, and picked up again once the word is out
                    onPointFact(null);
                    onPointEntity(fact.other_id);
                  }}
                  onMouseLeave={() => {
                    onPointEntity(null);
                    onPointFact(fact.id);
                  }}
                  onKeyDown={(ev) => {
                    if (ev.key === "Enter") goOther(ev);
                  }}
                >
                  {fact.other_name ?? "?"}
                </span>
              </span>
            ) : (
              <span className={ROW_VALUE}>
                {fmtObjectValue(fact.object_value) ?? "?"}
              </span>
            )}
            {/* Attributes on the edge (0037): `amount $4B`, following the object rather than on a new line.
                Who was invested in and how much is one sentence; split apart it would no longer read as a sentence */}
            {fact.qualifiers?.map((q) => (
              <span key={q.qualifier_type_id} className="shrink-0 text-small text-ink-2">
                {q.label || q.key}{" "}
                <span className="text-ink">
                  {q.entity_name ?? fmtObjectValue(q.value) ?? "?"}
                </span>
              </span>
            ))}
          </span>
          {/* Second line: when it holds, whether to be careful, and the entries for viewing evidence and rescheduling.
              **Even with no date, say so**: when empty, "the source gives no date" and
              "there is a date that I just am not showing" look exactly the same on the interface, and the whole point
              of this product is time.
              Confidence and the ⟲ for "interval closed by reconciliation" have both been removed: one is a number, the other a
              symbol no one could guess, and both only took up room on this line without saying anything clearly. They are spelled out in full sentences
              in the evidence tier (see EvidenceList) */}
          <span className="flex items-center gap-2">
            <span
              className={cn(
                "u-num text-fine",
                interval ? "text-ink-2" : "italic text-ink-2",
              )}
            >
              {interval || asOf(fact) || S.graph.undated}
            </span>
            {fact.stale && (
              <Chip tone="neutral" className="shrink-0 text-fine">
                {S.graph.staleFactChip}
              </Chip>
            )}
            {fact.contested && <ContestedChip kbId={kbId} c={fact.contested} />}
            <span className="ml-auto flex shrink-0 items-center gap-2 pl-2">
              {fact.evidence_count > 0 && (
                <LinkButton
                  className={cn(REVEAL, "text-fine", open && "is-on")}
                  onClick={(ev) => {
                    ev.stopPropagation();
                    onToggle();
                  }}
                >
                  {S.graph.sources(fact.evidence_count)}
                </LinkButton>
              )}
              {/* This tier holds only asserted facts: a derived interval is computed, and goes through the Derived path */}
              <span
                role="button"
                tabIndex={0}
                title={S.graph.editTime}
                aria-label={S.graph.editTime}
                onClick={(ev) => {
                  ev.stopPropagation();
                  setEditing(true);
                }}
                onKeyDown={(ev) => {
                  if (ev.key === "Enter" || ev.key === " ") {
                    ev.preventDefault();
                    ev.stopPropagation();
                    setEditing(true);
                  }
                }}
                className={cn(REVEAL, "cursor-pointer rounded-cell p-1 text-ink-2")}
              >
                <Pencil size={10} />
              </span>
            </span>
          </span>
        </span>
      </div>
      {open && (
        <div className="pb-2 pl-2 pr-2">
          <EvidenceList kbId={kbId} fact={fact} />
        </div>
      )}
      {editing && (
        <FactTimeDialog kbId={kbId} fact={fact} onClose={() => setEditing(false)} />
      )}
    </div>
  );
}

/** The evidence expansion area (FactRow spreads it out below the row): quote, jump to the source, version marker, confidence. */
function EvidenceList({ kbId, fact }: { kbId: string; fact: EntityFact }) {
  const evidence = useQuery({
    queryKey: ["evidence", fact.id],
    queryFn: () => api.factEvidence(kbId, fact.id),
  });
  return (
    /* Scrolls when there are several passages, rather than making the whole panel grow. A fact may have a dozen or so passages of evidence; spreading them all
       would push every other fact below it off the screen, but expanding one is for reading it, not for losing its context */
    <div className="u-scroll max-h-64 space-y-2 overflow-y-auto">
      {evidence.data?.evidence.map((ev: Evidence) => (
        /* **One card per passage; the card itself is not a link.**
           Previously the whole block was a `<Link>`: to read the source you moved your hand and jumped to the document page; to select a
           sentence and copy it, releasing the mouse also jumped away. The question that expanding answers is "why do you say so",
           and that sentence is right here; only after reading it does it make sense to decide whether to look at the context, so the jump is moved into
           the small marker at the end, which only navigates when clicked.
           The background uses the lowest tier, **and there is no hover state**: the whole card is not clickable, and giving it a highlight
           would be deceiving; the only thing that responds is the marker at the end, which has its own `u-hover-ink` */
        <div
          key={ev.chunk_id}
          className="rounded-cell bg-surface px-2 py-2 text-small text-ink-2"
        >
          {/* The predicate as written in the source is shown only when it differs from the one on the fact row. Predicates outside the ontology already show
              the source wording on the fact row (0052), so repeating it when the same is noise;
              it only has something to say when a fact has several wordings (3% of cases) */}
          {ev.proposed_predicate &&
            ev.proposed_predicate !== fact.predicate_key && (
              <div className="mb-1 text-fine text-ink-2">
                {S.graph.proposedPredicate(ev.proposed_predicate)}
              </div>
            )}
          {/* **Not truncated.** It used to be line-clamp-2, so "view source" showed only the source's
              first two lines: the only way to read the whole thing was to jump to the document page, which meant the tier had not really been expanded */}
          <div className="whitespace-pre-wrap italic text-ink">
            {ev.quote ? `“${ev.quote}”` : S.graph.noQuote}
          </div>
          <div className="mt-2 flex items-center gap-2">
            {/* Small marker: the origin plus going to the document page for context. This is the only place on the card that takes you away */}
            <Link
              to="/kb/$kbId/doc/$docId"
              params={{ kbId, docId: ev.document_id }}
              search={{ chunk: ev.chunk_id }}
              className="u-hover-ink inline-flex min-w-0 items-center gap-1 text-fine text-ink-2"
              title={S.graph.openInDoc}
            >
              <span className="truncate">
                {S.graph.sectionRef(ev.filename, ev.seq + 1)}
              </span>
              <ExternalLink size={11} className="shrink-0" />
            </Link>
            {/* When the origin is not the source text, say so in a sentence (0040): which OCR page, which part of a recording and who said it, or a model description */}
            {originLabel(ev.origin, ev.anchor) && (
              <span
                className="shrink-0 text-fine text-ink-2"
                title={originHint(ev.origin, ev.origin_model)}
              >
                {originLabel(ev.origin, ev.anchor)}
              </span>
            )}
            {ev.stale && (
              <span
                className="u-num shrink-0 text-fine text-ink-2"
                title={S.graph.staleEvidenceHint}
              >
                {S.graph.fromVersion(ev.doc_version)}
              </span>
            )}
            {ev.document_deleted && (
              <span
                className="shrink-0 text-fine text-contest"
                title={S.graph.sourceDeletedHint}
              >
                {S.graph.sourceDeleted}
              </span>
            )}
          </div>
        </div>
      ))}
      {evidence.data?.evidence.length === 0 && (
        <p className="text-small text-ink-2">{S.graph.noEvidence}</p>
      )}
      {/* This interval was not written in the source; the engine closed it by reconciliation or a manual ruling. **Moved here from the fact row**:
          on the row it was a ⟲ that nobody could guess the meaning of; this tier is already there to answer
          "why do you say so", and one sentence makes it clear */}
      {fact.corrected && (
        <p className="text-fine text-ink-2">{S.graph.correctedHint}</p>
      )}
      {/* Confidence speaks up only when it is low enough to be suspicious (consistent with Review's low-confidence basis); it is not marked normally */}
      {fact.confidence < 0.75 && (
        <p className="text-fine text-warn">
          {Math.round(fact.confidence * 100)}% {S.graph.confidence}
        </p>
      )}
    </div>
  );
}
