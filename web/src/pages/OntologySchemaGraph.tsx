// The schema graph of the ontology: classes are nodes; subClassOf, relations and disjointness are the three kinds of edge.
// It draws the TBox (the ontology's own structure), not the ABox (instances and facts) that /graph draws:
// the two share the graphology/sigma toolchain and visual vocabulary (see graphVisuals.ts),
// but not the semantics or the queries: the ontology data exists once, fetched by useQuery in Ontology.tsx,
// and this component only takes data + callbacks.
//
// **Selection is controlled**: this component only handles the canvas itself (nodes/edges/search/legend/zoom),
// without knowing what the edit form looks like. After selecting a class or relation, the actual ClassForm /
// PropertyForm is rendered by Ontology.tsx, docked on the right of the canvas. This file used to embed a copy of it
// (selected was internal state, and the inspector also lived in this file), so the existing 'click a class name in the left column → open form'
// path on the ontology page and this one were each drawn separately, and looked different. Now both paths
// land on the same sel state and the same form component, and this file is left with only 'drawing' and 'what is selected'.
//
// **It does not draw everything.** Imported packages easily contain hundreds or thousands of classes (schema.org alone has over nine hundred),
// and spreading them all out makes a hairball. A large ontology draws only the classes the library uses (those with instances and their ancestors); classes
// clicked in the left column are added to the canvas on demand. There is no 'show all': nobody can read structure from nine hundred spread-out classes. The one thing that picture
// only tells is how big the package is, and the number on the pill says the whole sentence. The framing rules live in
// schemaScope, separate from the projection (buildSchemaGraph); both are pure functions.
//
// **The canvas has no search box of its own.** To find a class or relation, use the filter box in the left column: two search boxes on one page
// would leave it unclear who owns search. Whatever is selected in the left column is brought into view on the canvas (bringIntoView).
import { useEffect, useMemo, useRef, useState } from "react";
import Graphology from "graphology";
import { circular } from "graphology-layout";
import forceAtlas2 from "graphology-layout-forceatlas2";
import Sigma from "sigma";
import { onThemeChange } from "../theme";
import { EdgeArrowProgram, EdgeLineProgram } from "sigma/rendering";
import EdgeCurveProgram, { EdgeCurvedArrowProgram } from "@sigma/edge-curve";
import {
  drawWorldGrid,
  mix,
  NODE_BORDER_BASE,
  NODE_CORE_BASE,
  NODE_CORE_MIX,
  NODE_SHELL_BASE,
  NODE_TINT_MIX,
  RING_SELECT_MIX,
  TRANSPARENT,
  EDGE_SUBCLASS,
  EDGE_SUBCLASS_FOCUS,
  EDGE_RELATION,
  EDGE_RELATION_FOCUS,
  EDGE_DISJOINT,
  EDGE_DISJOINT_FOCUS,
  EDGE_RULE,
  EDGE_RULE_FOCUS,
  EDGE_SCHEMA_DIM as EDGE_DIM,
  LEGEND_SUBCLASS,
  LEGEND_RELATION,
  LEGEND_DISJOINT,
  LEGEND_RULE,
  INK,
  refreshPalette,
  CANVAS_TEXT,
  CANVAS_TEXT_2,
} from "./graphVisuals";
// The canvas machinery is shared by both pages (#496): construction options, state table, camera and dragging all live there,
// and this file only projects the ontology into a graph and decides what color each node is
import {
  attachDrag,
  focusNode,
  syncGraph,
  deferToHoverLayer,
  hoveredNode,
  mutedNode,
  neighborNode,
  nodeInView,
  NODE_TYPE_SHELL,
  drawLast,
  NODE_TYPE_SQUARE,
  ownColorOf,
  selectedNode,
  sigmaOptions,
  withTopLayer,
} from "./graphCanvas";
import { Maximize2, ZoomIn, ZoomOut } from "lucide-react";
import type { BusinessRule, EntityTypeView, RelationTypeView } from "../api";
import { S } from "../i18n";
import {
  Button,
  CanvasLoading,
  cn,
  Pill,
  Row,
  ToolButton,
  ToolDivider,
  ToolTower,
  Tooltip,
} from "../ui";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";

/* ============ The three edge semantics and their mapping to the three visual vocabularies ============
   Relation edges use the same gray as /graph's edges (that one was changed to pure gray on request, so this one does not start
   its own set). Several dozen relations often fan out side by side between two large classes; any chromatic color stacked there becomes
   a blob, while grays just get a bit darker when stacked. Inheritance edges are the reverse: bright and thin. They are the skeleton, and there are few of them
   (one per class), so they must be bright enough to show through the gray web of relations, and thin enough not to steal the show.
   Pink is --u-danger, the color ClassForm uses for the disjointness and parent-conflict hint text.

   The RGB of the dark state must be pushed toward the background color by hand; lowering alpha alone is not enough: sigma's edge shader under
   premultiplied blending (ONE, ONE_MINUS_SRC_ALPHA) does not premultiply RGB, so lowering alpha alone does not make the edge look darker
   (the same note appears at EDGE_DIM in Graph.tsx). The edges here need no animated fade in or out, so there is no need to carry a lerp/parseRgba set;
   each state can simply write its own color literal
   and that is enough. */

/** The meaning of each of the three edge kinds: drives color, dimming and clickability; managed separately from 'which sigma program draws it' */
const SUBCLASS_KIND = "subclass";
const RELATION_KIND = "relation";
const DISJOINT_KIND = "disjoint";
const RULE_KIND = "rule";

/** sigma's render dispatch key. **Deliberately separate from the semantics above**: relation edges can be straight or curved
 *  (a single one is straight; curved only when parallel), but both are 'relation semantics'. Earlier these two things were
 *  welded into one field, so straight relation edges all fell back to the default type and became invisible:
 *  this was the root cause of 'some lines are invisible' in the first version of the schema graph */
const EDGE_TYPE_ARROW = "arrow"; // straight + arrow (EdgeArrowProgram)
const EDGE_TYPE_CURVED_ARROW = "curvedArrow"; // curved + arrow (EdgeCurvedArrowProgram)
const EDGE_TYPE_LINE = "line"; // straight, no arrow (EdgeLineProgram): reserved for disjointness
const EDGE_TYPE_CURVED_LINE = "curvedLine"; // curved, no arrow (EdgeCurveProgram): disjointness shares it with other edges for a pair


/** Structure edges are one step thinner than relation edges. 'Semantic relations are more prominent than structural information' cannot be conveyed by color alone,
 *  so the thickness must also differ by one step. This thickness is also the hit band for click selection: sigma's edge picking
 *  uses the geometry as rendered (the WebGL picking buffer, not a separate 'click tolerance'),
 *  and at the default 1.7px minEdgeThickness, relation edges are nearly impossible to click with a real mouse.
 *  Thickening relation edges is one change that serves both visual prominence and 'clickable'. */
const SUBCLASS_EDGE_SIZE = 0.9;
const DISJOINT_EDGE_SIZE = 0.8;
/** Same thickness as relation edges (2). **Thickness is the click hit band**: sigma picks using the geometry as rendered,
 *  and thin lines are nearly impossible to click with a real mouse (this is why relation edges were thickened).
 *  Measured: at 1.6, that arc could not be clicked repeatedly */
const RULE_EDGE_SIZE = 2;
const RELATION_EDGE_SIZE = 2;
/** Beyond this many relations in the same direction between the same pair of classes, they merge into one edge with a count. In schema.org
 *  Person→Organization has over twenty (worksFor, memberOf, affiliation...);
 *  twenty-odd arcs fanning out is that hairball, and none of them can be clicked; one edge labelled '23 relations'
 *  says the same thing. Clicking it selects the domain class, and the panel's properties page lists these relations one by one */
const BUNDLE_ABOVE = 3;
/** The minimum sigma edge rendering thickness (pixels), default 1.7: the same reason, a global floor,
 *  so that no edge becomes hard to hit when zoomed out to some level.
 *
 *  **Lowered from 3 to 2** (#497): 3 flattened the thickness difference deliberately drawn on this graph. Inheritance edges
 *  are 0.9 and relation edges are 2; with the floor at 3, both were drawn at 3, so the distinction 'inheritance thin and bright, relations
 *  thick and gray' did not exist on screen at all, and the whole graph had only one weight. And once the schema graph became
 *  one level of the graph page, the switch between the two levels is visible: same canvas, edges should not change weight.
 *  2  *  2 aligns with relation edges' own size; inheritance edges can get thin again, and still be clickable */
const MIN_EDGE_THICKNESS = 2;
/** Node size follows hierarchy depth: the root is largest, each level down is one step smaller, and it stops shrinking at the bottom. The range
 *  The range matches /graph's nodes (5–13), so the two graphs side by side are drawn by the same engine. Previously it followed connection count,
 *  so Thing and each of its subclasses hit the same cap, and the hierarchy could not be read from the graph */
const NODE_SIZE_ROOT = 13;
const NODE_SIZE_STEP = 2;
const NODE_SIZE_MIN = 6;

/** Step of the side-by-side offset; when more than one relation edge exists between the same pair of classes, they fan out using it */
const RELATION_CURVATURE_STEP = 0.22;
/** A self-loop (domain === range, e.g. married_to: Person→Person) has no 'offset' to speak of:
 *  give it a fixed starting curvature, or it degenerates into an invisible point */
const SELF_LOOP_BASE_CURVATURE = 1;

/** Up to this many classes, draw them all: in a hand-built ontology of a few dozen classes, hiding some only makes people unable to find
 *  the class they just created. Beyond it (imported packages easily run to hundreds or thousands), framing follows 'what the library uses' */
const FULL_VIEW_MAX_CLASSES = 60;

/** Optional tiers for how many classes to draw, isomorphic with `NODE_BUDGETS` on the instance graph (that one is 150/300/600/1000).
 *
 *  **'Classes in use' is not enough.** In a library with schema.org installed, 121 classes have instances, and of those
 *  over a hundred have only one or two: putting them all up would make Person (172) and an ImageObject that appears only once
 *  ImageObject  *  equally big and equally prominent, and the reader would see a hairball rather than the library's shape.
 *  The default of 30 already covers the tier of 'five or more instances'; the long tail folds into '+N classes',
 *  and to see a specific one, use search: it adds that class to the canvas along with its ancestors. */
export const SCHEMA_BUDGETS: number[] = [30, 60, 120, 240];

export interface SchemaScope {
  /** Classes drawn onto the canvas; null means draw all */
  drawn: ReadonlySet<string> | null;
  /** Framing basis: all = draw everything; in-use = classes with instances and their ancestors; top = none has an instance,
   *  draw the top two levels of the hierarchy */
  basis: "all" | "in-use" | "top";
  /** Number of classes not drawn */
  hidden: number;
}

/** Default framing. A small ontology draws everything; a large one draws only the classes the library uses: those with instances, together with ancestors so
 *  the inheritance chain is complete; if none has an instance (a freshly created library), fall back to the top two levels of the hierarchy: Thing and its
 *  dozen-odd direct subclasses, the map you would draw by hand.
 *  'revealed' is the class the user named in the left column, also added with its ancestors. */
export function schemaScope(
  entityTypes: EntityTypeView[],
  revealed: ReadonlySet<string>,
  budget: number = SCHEMA_BUDGETS[0],
): SchemaScope {
  if (entityTypes.length <= FULL_VIEW_MAX_CLASSES) {
    return { drawn: null, basis: "all", hidden: 0 };
  }
  const byId = new Map(entityTypes.map((t) => [t.id, t]));
  const drawn = new Set<string>();
  // Already in the set: do not walk further up, so parents pointing at each other in a cycle cannot loop forever
  const addWithAncestors = (id: string) => {
    const t = byId.get(id);
    if (!t || drawn.has(id)) return;
    drawn.add(id);
    for (const parentId of t.parents) addWithAncestors(parentId);
  };
  /* The most-used ones, taken by tier; **ancestors do not count against the quota**: they are there to keep the inheritance chain complete,
     not to appear themselves. So the number drawn may be a little above the tier, and '+N classes' reports the actual count */
  const inUse = entityTypes
    .filter((t) => t.usage > 0)
    .sort((a, b) => b.usage - a.usage || a.label.localeCompare(b.label))
    .slice(0, budget);
  for (const t of inUse) addWithAncestors(t.id);
  const basis: SchemaScope["basis"] = drawn.size > 0 ? "in-use" : "top";
  if (basis === "top") {
    // Root: no parent points to a class that really exists (broken references and self-references do not count as parents,
    // the same test buildSchemaGraph uses when drawing inheritance edges)
    const rootIds = new Set(
      entityTypes
        .filter((t) => !t.parents.some((p) => p !== t.id && byId.has(p)))
        .map((t) => t.id),
    );
    for (const t of entityTypes) {
      if (rootIds.has(t.id) || t.parents.some((p) => rootIds.has(p)))
        drawn.add(t.id);
    }
  }
  for (const id of revealed) addWithAncestors(id);
  return { drawn, basis, hidden: entityTypes.length - drawn.size };
}

export interface SchemaGraphResult {
  graph: Graphology;
  /** Relations whose domains or ranges are empty: the ontology does not restrict them to any class, so no edge can be drawn,
   *  but they must not be pretended away */
  unscoped: RelationTypeView[];
}

/** The pure-function projection from ontology to schema graph. It does not touch React/Sigma, so the semantics can be verified on their own:
 *  do all multiple inheritance edges make it into the graph, is disjointness not duplicated by symmetric declarations, do parallel relations not collapse into one,
 *  are unrestricted relations not wrongly drawn as fully connected, and does a bad reference (a parent/domain/range pointing at a nonexistent
 *  id *  id) not crash it. */
/** Each class's depth in the hierarchy: root 0, +1 per level going down; with multiple inheritance take the shallowest path.
 *  Computed over the whole ontology, not just the part on the canvas: ancestors outside the framing still count as levels, so a class
 *  is as small as it should be when opened in the left column. When parents point at each other in a cycle, cut at the point already walked */
function classDepths(entityTypes: EntityTypeView[]): Map<string, number> {
  const byId = new Map(entityTypes.map((t) => [t.id, t]));
  const depths = new Map<string, number>();
  const walking = new Set<string>();
  const depthOf = (id: string): number => {
    const known = depths.get(id);
    if (known !== undefined) return known;
    const t = byId.get(id);
    if (!t || walking.has(id)) return 0;
    walking.add(id);
    let depth = 0;
    for (const parentId of t.parents) {
      if (parentId === id || !byId.has(parentId)) continue;
      const d = depthOf(parentId) + 1;
      if (depth === 0 || d < depth) depth = d;
    }
    walking.delete(id);
    depths.set(id, depth);
    return depth;
  };
  for (const t of entityTypes) depthOf(t.id);
  return depths;
}

export function buildSchemaGraph(
  entityTypes: EntityTypeView[],
  relationTypes: RelationTypeView[],
  /** Framing (see schemaScope): only these classes go onto the canvas; null draws all */
  drawn: ReadonlySet<string> | null = null,
  /** Business rules: **a rule that derives a class is one edge between two classes**: the main class → the conclusion class,
   *  with the conditions written in the rule. A rule that derives an attribute value has no target node and is not drawn (it lives in the rule table) */
  rules: BusinessRule[] = [],
): SchemaGraphResult {
  // Shell color and edge color are baked into graph attributes: before building the graph, read the palette as the current theme's values (0038)
  refreshPalette();
  const graph = new Graphology({ multi: true });
  const byId = new Map(entityTypes.map((t) => [t.id, t]));
  const depths = classDepths(entityTypes);

  for (const t of entityTypes) {
    if (drawn && !drawn.has(t.id)) continue;
    graph.addNode(t.id, {
      label: t.label,
      size: Math.max(
        NODE_SIZE_MIN,
        NODE_SIZE_ROOT - NODE_SIZE_STEP * (depths.get(t.id) ?? 0),
      ),
      // Same four-layer shell recipe as /graph (dark shell + 14% type tint, core 50% tint,
      // slightly tinted steel-gray outline): the schema graph and the instance graph look like the same engine drew them
      color: mix(NODE_CORE_BASE, t.color, NODE_CORE_MIX),
      shellColor: mix(NODE_SHELL_BASE, t.color, NODE_TINT_MIX),
      borderColor: mix(NODE_BORDER_BASE, t.color, 0.3),
      ringColor: TRANSPARENT,
      typeColor: t.color,
      typeLabel: t.key,
      type: t.shape === "square" ? NODE_TYPE_SQUARE : NODE_TYPE_SHELL,
      key: t.key,
    });
  }

  // subClassOf: child → parent. The direction is not reversed just because 'the parent drawn above looks nicer':
  // that is a layout problem, not a reason for an edge to lie. All parents, not just
  // primary_parent: multiple inheritance must be visible on the graph; primary_parent only governs the left-column tree.
  // Classes outside the framing are not in the graph, so their edges are not drawn either: hasNode also blocks bad references
  for (const t of entityTypes) {
    if (!graph.hasNode(t.id)) continue;
    for (const parentId of t.parents) {
      if (parentId === t.id || !graph.hasNode(parentId)) continue;
      graph.addEdgeWithKey(`sub:${t.id}:${parentId}`, t.id, parentId, {
        kind: SUBCLASS_KIND,
        type: EDGE_TYPE_ARROW,
        size: SUBCLASS_EDGE_SIZE,
      });
    }
  }

  // Disjointness: an undirected declaration, with both sides often writing it in both directions. Dedupe by the sorted id pair, drawing only one
  const disjointSeen = new Set<string>();
  for (const t of entityTypes) {
    if (!graph.hasNode(t.id)) continue;
    for (const otherId of t.disjoint) {
      if (otherId === t.id || !graph.hasNode(otherId)) continue;
      const pairKey = [t.id, otherId].sort().join("|");
      if (disjointSeen.has(pairKey)) continue;
      disjointSeen.add(pairKey);
      graph.addEdgeWithKey(`dis:${pairKey}`, t.id, otherId, {
        kind: DISJOINT_KIND,
        type: EDGE_TYPE_LINE,
        size: DISJOINT_EDGE_SIZE,
      });
    }
  }

  // Relations: an attribute's object is a literal value, not a class; here we only handle kind === "relation"
  const unscoped: RelationTypeView[] = [];
  const pairs = new Map<
    string,
    { d: string; rg: string; rels: RelationTypeView[] }
  >();
  for (const r of relationTypes) {
    if (r.kind !== "relation") continue;
    const domains = [...new Set(r.domains)].filter((id) => byId.has(id));
    const ranges = [...new Set(r.ranges)].filter((id) => byId.has(id));
    // domains/ranges empty = the ontology does not restrict this relation to some class; it does not mean 'holds for all classes'.
    // Drawing it as fully connected would assert something the ontology never said. When every referenced class is invalid
    // (bad data), it degrades to the same 'cannot be drawn' and goes into this bucket too, without swallowing it
    if (
      r.domains.length === 0 ||
      r.ranges.length === 0 ||
      domains.length === 0 ||
      ranges.length === 0
    ) {
      unscoped.push(r);
      continue;
    }
    // The part of an endpoint outside the framing is not drawn, and does not count as 'unrestricted': it is what the ontology said, it just
    // is not laid out on the canvas right now; the left-column properties page still lists it, and the number on the pill says the picture is incomplete
    const drawnDomains = domains.filter((id) => graph.hasNode(id));
    const drawnRanges = ranges.filter((id) => graph.hasNode(id));
    for (const d of drawnDomains) {
      for (const rg of drawnRanges) {
        const key = `${d}|${rg}`;
        const pair = pairs.get(key);
        if (pair) pair.rels.push(r);
        else pairs.set(key, { d, rg, rels: [r] });
      }
    }
  }

  /* Rule edges. **Disabled rules are also drawn**, just one level dimmer: 'this inference is currently paused' is itself something
     a reader of the graph needs to know; if it vanished from the graph, people would think the rule never existed */
  for (const r of rules) {
    if (r.conclusion !== "typing" || !r.conclude_type_id) continue;
    if (!graph.hasNode(r.subject_type_id) || !graph.hasNode(r.conclude_type_id))
      continue;
    if (r.subject_type_id === r.conclude_type_id) continue;
    graph.addEdgeWithKey(`rule:${r.id}`, r.subject_type_id, r.conclude_type_id, {
      kind: RULE_KIND,
      ruleId: r.id,
      type: EDGE_TYPE_CURVED_ARROW,
      curvature: 0.35,
      label: r.name,
      size: RULE_EDGE_SIZE,
      enabled: r.enabled,
    });
  }

  // First bucket by directed class pair, then draw: the few are drawn individually, the many are merged into one edge with a count (see BUNDLE_ABOVE).
  // relationIds are carried by both kinds of edge: when a relation is selected, the edge containing it should light up
  for (const { d, rg, rels } of pairs.values()) {
    if (rels.length <= BUNDLE_ABOVE) {
      for (const r of rels) {
        graph.addEdgeWithKey(`rel:${r.id}:${d}:${rg}`, d, rg, {
          kind: RELATION_KIND,
          // type is decided by layOutParallelRelations according to the final curvature (straight or arc)
          relationIds: [r.id],
          label: r.label,
          size: RELATION_EDGE_SIZE,
        });
      }
      continue;
    }
    graph.addEdgeWithKey(`bundle:${d}:${rg}`, d, rg, {
      kind: RELATION_KIND,
      relationIds: rels.map((r) => r.id),
      label: S.ontology.schemaBundle(rels.length),
      // The more merged, the thicker it gets, but with a cap: thickness is a peripheral hint of 'how many', not a bar chart
      size: RELATION_EDGE_SIZE + Math.min(2, Math.log2(rels.length) * 0.6),
    });
  }

  layOutParallelEdges(graph);
  return { graph, unscoped };
}

/** The several edges between the same pair of classes each fan out onto their own separate arc, rather than stacking into one line no one can click.
 *  **Not separated by kind**: inheritance, disjointness and relation edges can all land on the same pair of classes (Unit is both
 *  a subclass of Organization and disjoint from it; a relation whose domain and range happen to be parent and child). If only relation edges fanned out,
 *  the other two kinds would stay stacked on the straight line, and their labels would stack too. The algorithm follows the same idea as
 *  layOutParallelEdges in Graph.tsx (grouped by undirected pair, spread symmetrically around the straight line); here the edges
 *  do not need to merge inverse relations first: inverse_of is explained only in the relation inspector in the ontology, and is not folded into the canvas.
 *
 *  Also decides each edge's render program as a side effect: **a lone edge stays straight**, and it switches to the arc program only when truly parallel or a self-loop;
 *  edges with arrows (inheritance, relation) use arrowed arcs, and disjointness uses unarrowed arcs */
function layOutParallelEdges(graph: Graphology): void {
  const groups = new Map<string, string[]>();
  graph.forEachEdge((edge, _attrs, source, target) => {
    const key =
      source === target ? `loop:${source}` : [source, target].sort().join("|");
    const list = groups.get(key);
    if (list) list.push(edge);
    else groups.set(key, [edge]);
  });
  const typeFor = (kind: string, curved: boolean) =>
    kind === DISJOINT_KIND
      ? curved
        ? EDGE_TYPE_CURVED_LINE
        : EDGE_TYPE_LINE
      : curved
        ? EDGE_TYPE_CURVED_ARROW
        : EDGE_TYPE_ARROW;
  for (const edges of groups.values()) {
    const n = edges.length;
    edges.forEach((edge, i) => {
      const [source, target] = graph.extremities(edge);
      const kind = graph.getEdgeAttribute(edge, "kind") as string;
      if (source === target) {
        graph.mergeEdgeAttributes(edge, {
          curvature: SELF_LOOP_BASE_CURVATURE + i * RELATION_CURVATURE_STEP,
          type: typeFor(kind, true),
        });
        return;
      }
      const offset = n === 1 ? 0 : i - (n - 1) / 2;
      // sigma's curvature is relative to this edge's own source→target, so the sign must be normalized by which end is smaller,
      // otherwise two edges in opposite directions would each think they are alone, fan to the same side, and stack back together
      const sign = source < target ? 1 : -1;
      const curvature = offset === 0 ? 0 : sign * offset * RELATION_CURVATURE_STEP;
      graph.mergeEdgeAttributes(edge, {
        curvature,
        type: typeFor(kind, curvature !== 0),
      });
    });
  }
}

type Point = { x: number; y: number };

/** Golden angle: several new nodes that land one after another beside the same anchor are spread out rotating by it, so they do not stack */
const GOLDEN_ANGLE = 2.399963;
const SEED_RADIUS = 40;

/** Deterministic initial layout + a one-shot synchronous ForceAtlas2 that converges. **No animated worker**:
 *  the class count of an ontology is far smaller than the instance graph's (dozens to a few hundred, not thousands), so running the steps once
 *  is simpler than maintaining a worker's lifecycle, and it avoids being wrongly restarted when the user just clicked to select something
 *  (the dependency array only holds entityTypes/relationTypes and the framing; selection state lives elsewhere).
 *  circular's starting order comes from node insertion order (that is, the entityTypes array order),
 *  so when the ontology does not change the order does not change, and this layout is repeatable. It is not stable to the pixel,
 *  but two renders of the same ontology do not look wildly different.
 *
 *  **With old coordinates, do it incrementally.** Most nodes were already drawn last time (a search revealed a class, an edit added
 *  a class): they start from their original positions, only new nodes find a landing spot next to already-placed neighbors, and then a few dozen more
 *  force steps tighten things up: one extra dot beside the picture, not a reshuffle of the whole graph. Nodes that were moved by hand
 *  are pinned at this point (FA2 honors the fixed attribute). If new nodes make up most of the graph (say a package was just imported),
 *  start from scratch. */
function layoutSchemaGraph(
  graph: Graphology,
  known: ReadonlyMap<string, Point>,
  pinned: ReadonlySet<string>,
): void {
  if (graph.order === 0) return;
  let placed = 0;
  graph.forEachNode((node) => {
    const pos = known.get(node);
    if (!pos) return;
    graph.mergeNodeAttributes(node, { x: pos.x, y: pos.y });
    placed += 1;
  });
  const incremental = placed * 2 >= graph.order;
  if (!incremental) {
    // The starting circle's radius is the same number as on the instance graph (300 in Graph.tsx): with the same
    // engine, the same forces and the same starting point, the remaining differences all come from the data itself
    circular.assign(graph, { scale: 300 });
  } else if (seedFreshNodes(graph, known) === 0) {
    return; // No new nodes: the old coordinates are the final state, no more forces are run
  }
  if (graph.size === 0) return; // Only isolated nodes: placed means final, there are no forces to run
  /* The same set of forces as the instance graph (the set in Graph.tsx was tuned on a real graph).
     Previously this was gravity 0.55 / 200 steps: the pull toward the center was 60% higher, and FA2 expands progressively,
     so with a fixed number of steps it stopped at the moment it had not yet spread out; the two together made the same
     engine draw two graphs, one spread out and one clumped */
  const settings = {
    ...forceAtlas2.inferSettings(graph),
    gravity: 0.35,
    scalingRatio: 22,
    outboundAttractionDistribution: true,
  };
  const fixed = incremental
    ? [...pinned].filter((id) => graph.hasNode(id))
    : [];
  for (const id of fixed) graph.setNodeAttribute(id, "fixed", true);
  // Steps are also increased: 200 steps still did not spread out a few hundred classes. Running 600 steps synchronously at this scale
  // takes a few dozen milliseconds, so the complexity of a worker is not worth it
  forceAtlas2.assign(graph, { iterations: incremental ? 150 : 600, settings });
  for (const id of fixed) graph.removeNodeAttribute(id, "fixed");
}

/** In incremental layout, finding landing spots for nodes without old coordinates: next to an already-placed neighbor. Subclasses usually
 *  only connect to their parent, so they land beside the parent; if the neighbor is also new, wait for the next round until no one is reliably placed;
 *  anything truly isolated lands near the origin. Returns the number of new nodes. */
function seedFreshNodes(
  graph: Graphology,
  known: ReadonlyMap<string, Point>,
): number {
  const pending = new Set(graph.filterNodes((node) => !known.has(node)));
  const total = pending.size;
  let seeded = 0;
  const drop = (node: string, ax: number, ay: number) => {
    const angle = seeded * GOLDEN_ANGLE;
    seeded += 1;
    graph.mergeNodeAttributes(node, {
      x: ax + SEED_RADIUS * Math.cos(angle),
      y: ay + SEED_RADIUS * Math.sin(angle),
    });
    pending.delete(node);
  };
  let progressed = true;
  while (progressed && pending.size > 0) {
    progressed = false;
    for (const node of [...pending]) {
      const anchor = graph.findNeighbor(node, (nb) => !pending.has(nb));
      if (anchor === undefined) continue;
      drop(
        node,
        graph.getNodeAttribute(anchor, "x") as number,
        graph.getNodeAttribute(anchor, "y") as number,
      );
      progressed = true;
    }
  }
  for (const node of [...pending]) drop(node, 0, 0);
  return total;
}


export type SchemaSelection =
  | { kind: "class"; id: string }
  | { kind: "relation"; id: string }
  | { kind: "rule"; id: string }
  | null;

export function OntologySchemaGraph({
  entityTypes,
  relationTypes,
  rules = [],
  selected,
  onSelect,
  loading = false,
  onEmptyAction,
}: {
  /** The button under the sentence shown when there are no classes at all: takes people to the docs for what they say, and what the ontology still lacks */
  onEmptyAction?: () => void;
  entityTypes: EntityTypeView[];
  /** Business rule: drawn as a purple arc from the main class → the conclusion class; clicking it opens the rule page */
  rules?: BusinessRule[];
  /** All relations (including attributes): the graph draws only kind === "relation"; attributes here are simply ignored,
   *  since their objects are literal values, not classes, so they do not enter the class graph, and do not need to be separated out in this file;
   *  showing attributes is the job of the docked panel in Ontology.tsx */
  relationTypes: RelationTypeView[];
  /** Controlled selection state: shares the same `sel` as the Ontology.tsx left column. Clicking a node/edge on the canvas
   *  and clicking a class name in the left column go through the same state, so the form docked on the right is naturally the same one */
  selected: SchemaSelection;
  onSelect: (sel: SchemaSelection) => void;
  /** The ontology has not arrived yet. The grid, zoom tower and static legend are drawn as usual, with a spinner in the middle:
   *  **Separate from 'there really are no classes'**: that sentence is a conclusion, this circle is a process */
  loading?: boolean;
}) {
  const entityById = useMemo(
    () => new Map(entityTypes.map((t) => [t.id, t])),
    [entityTypes],
  );
  const relationById = useMemo(
    () => new Map(relationTypes.map((r) => [r.id, r])),
    [relationTypes],
  );
  const objectRelations = useMemo(
    () => relationTypes.filter((r) => r.kind === "relation"),
    [relationTypes],
  );

  // Framing: for a large ontology, draw only the most-used classes; classes clicked in the left column are added (see schemaScope)
  // When the theme changes, the schema graph must be rebuilt (the shell colors are baked into attributes)
  const [themeTick, setThemeTick] = useState(0);
  const [revealed, setRevealed] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const scope = useMemo(
    () => schemaScope(entityTypes, revealed),
    [entityTypes, revealed],
  );
  const scopeRef = useRef(scope);
  scopeRef.current = scope;
  /** Pull these classes (together with ancestors) onto the canvas. Ones already on the canvas do not count; when nothing is added, do not change
   *  state, to avoid rebuilding the graph for nothing */
  const reveal = (ids: string[]) => {
    const drawn = scopeRef.current.drawn;
    if (!drawn) return;
    const missing = ids.filter((id) => entityById.has(id) && !drawn.has(id));
    if (missing.length === 0) return;
    setRevealed((prev) =>
      missing.every((id) => prev.has(id)) ? prev : new Set([...prev, ...missing]),
    );
  };

  // If the ontology has not changed, do not rebuild the graph: the dependency array only watches the references of entityTypes/relationTypes and
  // the framing; selection and hover are other state and do not trigger this here
  const schema = useMemo(
    () => buildSchemaGraph(entityTypes, objectRelations, scope.drawn, rules),
    [entityTypes, objectRelations, scope.drawn, rules, themeTick]);

  /** Bring a class into view: if it is still outside the framing, reveal it first; after rebuilding, focus the camera. If it is drawn but outside the viewport,
   *  push the camera over to it; if it is already in the viewport, only highlight, and the picture does not move */
  const bringIntoView = (id: string) => {
    if (!entityById.has(id)) return;
    const g = graphRef.current;
    const sigma = sigmaRef.current;
    if (g && sigma && g.hasNode(id)) {
      if (!nodeInView(sigma, id)) focusNode(sigma, id);
      return;
    }
    reveal([id]);
    pendingFocusRef.current = id;
  };

  const selectedRef = useRef<SchemaSelection>(null);
  const hoverRef = useRef<string | null>(null);
  const hoverEdgeRef = useRef<string | null>(null);
  /** Selected, but that class is not yet drawn into the graph. **The picture holds still first**, and switches only once it arrives.
   *
   * Without this, there would be one intermediate frame of 'nothing selected': the guard in the reducer is
   * `sel.kind === "class" && g.hasNode(sel.id)`, and while the class has not been revealed, `hasNode` is false,
   * so 'selected but not yet drawn' is downgraded to 'not selected', and the whole graph **lights up for one frame** and then dims back.
   * The panel opening at the same time squeezes the canvas narrower and triggers a redraw, which brings that one frame right to the eye, looking like
   * a flicker. */
  const pendingSelectRef = useRef<SchemaSelection>(null);

  /** Whether this relation **has a place on the graph**.
   *
   * Properties with neither subject nor object (that batch of 'Unscoped properties' in the legend; a schema.org
   * library has over a hundred and eighty) connect to no class, so no edge can be drawn. When selected, both reducers would each take
   * the 'dim everything unrelated to the selection' path, and the result is the whole graph dims with not one bright point left,
   * reading as 'selected but broken'.
   *
   * When there is nothing on the graph to point at, **the picture should not move**: the details are in the right-hand panel, which says
   * Subject / Object are both Any type, which already makes it clear. */
  const relationHasFootingRef = useRef<(id: string) => boolean>(() => true);
  relationHasFootingRef.current = (id: string) => {
    const rel = relationById.get(id);
    if (!rel) return false;
    return rel.domains.length > 0 || rel.ranges.length > 0;
  };

  useEffect(() => {
    const live = graphRef.current;
    const notDrawnYet =
      selected?.kind === "class" && !!live && !live.hasNode(selected.id);
    if (notDrawnYet) pendingSelectRef.current = selected;
    else {
      pendingSelectRef.current = null;
      selectedRef.current = selected;
    }
    // Whatever is selected in the left column is brought into view on the canvas; selecting a relation also adds its two ends,
    // and the camera looks at its first domain
    if (selected?.kind === "class") bringIntoView(selected.id);
    else if (selected?.kind === "relation") {
      const rel = relationById.get(selected.id);
      if (rel) {
        reveal([...rel.domains, ...rel.ranges]);
        if (rel.domains[0]) bringIntoView(rel.domains[0]);
      }
    }
    sigmaRef.current?.refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected]);

  const containerRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLCanvasElement>(null);
  const sigmaRef = useRef<Sigma | null>(null);
  const graphRef = useRef<Graphology | null>(null);
  // Node positions manually dragged, stored by id: **survive rebuilds**. This ref is not in the [schema]
  // dependency, so editing the ontology (creating/changing a class or relation makes entityTypes/relationTypes
  // change reference and the graph rebuilds entirely) does not clear it. Without this, a layout arranged by hand would be quietly overwritten by auto layout after the next save,
  // and 'moving nodes to make it clear' would be wasted. It lives only in this session,
  // and is not written back to the backend: this is rendering-layer feel, not ontology data, for the same reason /graph's drag
  // is not persisted either
  const draggedPositionsRef = useRef<Map<string, Point>>(new Map());
  /** The coordinates of each node at the last draw (including dragged ones). On rebuild, they are first put back in place, and only
   *  new nodes find landing spots: see the incremental note in layoutSchemaGraph */
  const positionsRef = useRef<Map<string, Point>>(new Map());
  /** The class clicked in the left column that has not been drawn yet: focus it once it enters the canvas */
  const pendingFocusRef = useRef<string | null>(null);
  /* The renderer is built only once (see the second effect below), so the closure only captures values once. These two things change,
     so they are put in a ref and refreshed on every render; not doing this would force the effect to depend on them, which goes back to
     "'rebuild on every change'. */
  const onSelectRef = useRef(onSelect);
  onSelectRef.current = onSelect;
  const relationByIdRef = useRef(relationById);
  relationByIdRef.current = relationById;

  /* When the ontology or framing changes: move the newly computed graph **into the one being rendered**, without replacing the renderer.
     Previously this was `sigmaRef.current?.kill()` plus `new Sigma`: clicking a class not yet drawn in the left column would go through it again: GPU buffers,
     camera, hover and selection state all lost, and the first frame was redone from scratch. */
  useEffect(() => {
    const g = schema.graph;
    layoutSchemaGraph(
      g,
      positionsRef.current,
      new Set(draggedPositionsRef.current.keys()),
    );
    // Put dragged nodes back: a layout computed from scratch does not know the user has already moved things, so after recalculating positions,
    // the recorded coordinates are copied back as they were (in incremental layout they are pinned, so copying back is harmless)
    for (const [id, pos] of draggedPositionsRef.current) {
      if (!g.hasNode(id)) continue;
      g.setNodeAttribute(id, "x", pos.x);
      g.setNodeAttribute(id, "y", pos.y);
    }
    const positions = new Map<string, Point>();
    g.forEachNode((node, attrs) =>
      positions.set(node, { x: attrs.x as number, y: attrs.y as number }),
    );
    positionsRef.current = positions;
    /** The graph has **only one instance**: it is recorded the first time, and afterwards the new one is always moved into it */
    const live = graphRef.current;
    if (live) syncGraph(live, g);
    else graphRef.current = g;
    // The selected thing may no longer exist in the new graph (for example, the currently selected class was deleted) —
    // leave it to the existence check at render time, and do not clear it here: in most cases (after an edit is saved
    // and refreshed) the selected thing is still there, and clearing would only make the panel flash closed and reopen for no reason

    const existing = sigmaRef.current;
    if (existing) {
      // The class that was not drawn yet when clicked in the left column is now here: focus it. The node's in-box coordinates only come after
      // sigma finishes a round, so this hangs off the render after the next one.
      // **Hook up the hook first, then refresh**: the other way round, this frame may already have finished rendering,
      // and once the hook is attached it would never get the render it is waiting for
      const pending = pendingFocusRef.current;
      if (pending && graphRef.current?.hasNode(pending)) {
        pendingFocusRef.current = null;
        existing.once("afterRender", () => focusNode(existing, pending));
      }
      /* The class it was waiting for has entered the graph; only now hand the selection state to the canvas: **the switch completes in the same frame**,
         with no pass through 'nothing selected' in between */
      const held = pendingSelectRef.current;
      if (held?.kind === "class" && graphRef.current?.hasNode(held.id)) {
        pendingSelectRef.current = null;
        selectedRef.current = held;
      }
      existing.refresh();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [schema]);

  /* The renderer is **built only once**. It is the same instance; content changes are moved in by the effect above,
     so there are no changing dependencies here: the camera, hover, selection and GPU buffers all stay alive. */
  useEffect(() => {
    const g = graphRef.current;
    if (!containerRef.current || !g) return;
    // Canvas colors are read from tokens (0038): read once before the instance is built, and read again after a theme switch, then redrawn
    refreshPalette();
    const sigma = new Sigma(g, containerRef.current, {
      ...sigmaOptions({
        defaultEdgeType: EDGE_TYPE_ARROW,
        // Each program gets a 'topmost' set (see `withTopLayer`): highlighted edges are drawn last, as a batch
        edgeProgramClasses: withTopLayer({
          [EDGE_TYPE_ARROW]: EdgeArrowProgram,
          [EDGE_TYPE_LINE]: EdgeLineProgram,
          [EDGE_TYPE_CURVED_ARROW]: EdgeCurvedArrowProgram,
          [EDGE_TYPE_CURVED_LINE]: EdgeCurveProgram,
        }),
        minEdgeThickness: MIN_EDGE_THICKNESS,
        // With dozens or hundreds of classes, zooming to 0.05 still shows the whole picture; instance graphs often run to thousands, which zoom out further
        minCameraRatio: 0.05,
        maxCameraRatio: 6,
      }),
      nodeReducer: (node, attrs) => {
        const res = { ...attrs };
        const base = attrs.size as number;
        const sel = selectedRef.current;
        const hov = hoverRef.current;
        const selClass =
          sel?.kind === "class" && g.hasNode(sel.id) ? sel.id : null;
        /* **Selection overrides hover**, the same rule as the instance graph: when the pointer lands on the class that is selected, what is read
           should still be the selected appearance (inverted base card + bold name). Hovering over other classes shows hover as usual */
        if (selClass === node) {
          const picked = selectedNode(res, attrs, base);
          // The one selected is also hovered at the same time: the name is now drawn by the highlight layer, and the label layer steps aside
          return hov === node ? deferToHoverLayer(picked) : picked;
        }
        if (hov === node) return hoveredNode(res, attrs, base);
        if (selClass) {
          // Neighbors are not shrunk: in a graph of dozens of classes, shrinking them looks collapsed (the instance graph shrinks them to 0.76)
          if (g.areNeighbors(selClass, node)) return neighborNode(res, base);
          return mutedNode(res, base);
        }
        if (sel?.kind === "relation") {
          if (!relationHasFootingRef.current(sel.id)) return res;
          const rel = relationByIdRef.current.get(sel.id);
          if (rel) {
            // When a relation is selected, what lights up is its two ends: between classes there is no 'neighbor' to speak of,
            // the relation's domain and range are what it connects
            const endpoints = new Set([...rel.domains, ...rel.ranges]);
            if (endpoints.has(node)) {
              res.ringColor = mix(ownColorOf(attrs), INK, RING_SELECT_MIX);
              return neighborNode(res, base);
            }
            return mutedNode(res, base);
          }
        }
        return res;
      },
      edgeReducer: (edge, attrs) => {
        const res = { ...attrs };
        const kind = attrs.kind as string;
        const relIds = attrs.relationIds as string[] | undefined;
        const base =
          kind === SUBCLASS_KIND
            ? EDGE_SUBCLASS
            : kind === DISJOINT_KIND
              ? EDGE_DISJOINT
              : kind === RULE_KIND
                ? EDGE_RULE
                : EDGE_RELATION;
        const focus =
          kind === SUBCLASS_KIND
            ? EDGE_SUBCLASS_FOCUS
            : kind === DISJOINT_KIND
              ? EDGE_DISJOINT_FOCUS
              : kind === RULE_KIND
                ? EDGE_RULE_FOCUS
                : EDGE_RELATION_FOCUS;
        res.color = base;
        // Disabled rules: drawn, but dimmed one level — 'this inference is paused' must be visible,
        // since vanishing from the graph would make people think the rule never existed
        if (kind === RULE_KIND && attrs.enabled === false) res.color = EDGE_DIM;
        // Structural edges (inheritance/disjointness) carry no label; relation edges do — but on a large graph, all always shown
        // would become a wall of text that cannot be read, so renderEdgeLabels leaves them to zoom (see below,
        // updateEdgeLabels). The selected one is spelled out clearly in the inspector, so the canvas need not guarantee it
        // Rule edges carry their name: that is the only place they can say what they are (conditions are in the rule table)
        if (kind !== RELATION_KIND && kind !== RULE_KIND) res.label = "";

        const [s, t] = g.extremities(edge);
        const hov = hoverRef.current;
        const hoverHit = hov !== null && (hov === s || hov === t);
        const hoverEdgeHit = hoverEdgeRef.current === edge;
        const sel = selectedRef.current;
        const selHit =
          sel?.kind === "class"
            ? sel.id === s || sel.id === t
            : sel?.kind === "relation"
              ? (relIds?.includes(sel.id) ?? false)
              : sel?.kind === "rule"
                ? attrs.ruleId === sel.id
                : false;

        if (selHit || hoverHit || hoverEdgeHit) {
          res.color = focus;
          res.size = Math.max((attrs.size as number) ?? 1, 1) * 1.5;
          res.zIndex = 3;
          // Move that batch to be drawn last: a zIndex alone cannot hold back edges in other programs
          res.type = drawLast(String(res.type ?? EDGE_TYPE_ARROW));
          return res;
        }
        if (
          sel &&
          !(
            sel.kind === "relation" &&
            !relationHasFootingRef.current(sel.id)
          )
        ) {
          // Something is selected but this edge has nothing to do with it: push it toward the background color
          res.color = EDGE_DIM;
          res.label = "";
          return res;
        }
        return res;
      },
    });

    sigma.on("clickNode", ({ node }) =>
      onSelectRef.current({ kind: "class", id: node }),
    );
    sigma.on("clickEdge", ({ edge }) => {
      const ruleId = g.getEdgeAttribute(edge, "ruleId") as string | undefined;
      if (ruleId) {
        onSelectRef.current({ kind: "rule", id: ruleId });
        return;
      }
      const ids = g.getEdgeAttribute(edge, "relationIds") as string[] | undefined;
      if (!ids?.length) return;
      // Single one: select it; a bundle: select the domain class, and read them one by one in the properties page
      if (ids.length === 1)
        onSelectRef.current({ kind: "relation", id: ids[0] });
      else onSelectRef.current({ kind: "class", id: g.source(edge) });
    });
    sigma.on("clickStage", () => onSelectRef.current(null));
    sigma.on("enterNode", ({ node }) => {
      hoverRef.current = node;
      sigma.refresh();
    });
    sigma.on("leaveNode", () => {
      hoverRef.current = null;
      sigma.refresh();
    });
    sigma.on("enterEdge", ({ edge }) => {
      hoverEdgeRef.current = edge;
      sigma.refresh();
    });
    sigma.on("leaveEdge", () => {
      hoverEdgeRef.current = null;
      sigma.refresh();
    });

    // Drag nodes. **There is no live force simulation to feed**: unlike /graph, the layout here is computed once and then fixed,
    // so wherever a drag ends is where it stays, not pulled back by a force simulation; this is exactly the effect 'arrange by hand to make it clear' wants.
    // The threshold, bounding box freezing and cursor handling are all in attachDrag
    attachDrag(sigma, {
      container: () => containerRef.current,
      hovering: () => !!hoverRef.current,
      // Record while dragging: even if something goes wrong midway (component unmount, switching ontology), this move is not lost
      onMove: (node, pos) => {
        draggedPositionsRef.current.set(node, pos);
        positionsRef.current.set(node, pos);
      },
    });

    // Relation labels appear only once zoomed in: when the ontology gets large (imported packages often have dozens or hundreds of classes),
    // always showing them all is exactly the wall of text that item 11 is meant to cure
    const updateEdgeLabels = () =>
      sigma.setSetting("renderEdgeLabels", sigma.getCamera().ratio < 1.1);
    sigma.getCamera().on("updated", updateEdgeLabels);
    updateEdgeLabels();

    // World-coordinate grid: redrawn when the camera or container size changes; same background as /graph
    const renderGrid = () => {
      if (gridRef.current) drawWorldGrid(gridRef.current, sigma);
    };
    sigma.getCamera().on("updated", renderGrid);
    sigma.on("resize", renderGrid);
    renderGrid();

    sigmaRef.current = sigma;
    if (import.meta.env.DEV) {
      // Debug handle (dev only): same as /graph's __g/__sigma,
      // so the density and reducer output of the two graphs can be compared directly in a headless environment
      (window as unknown as Record<string, unknown>).__sg = g;
      (window as unknown as Record<string, unknown>).__ssigma = sigma;
    }
    if (import.meta.env.DEV) {
      // Debug handle (dev only), same convention as Graph.tsx
      (window as unknown as Record<string, unknown>).__schemaGraph = g;
      (window as unknown as Record<string, unknown>).__schemaSigma = sigma;
    }
    const offTheme = onThemeChange(() => {
      refreshPalette();
      // Label colors are fixed from the palette at the time of creation (sigmaOptions); if the instance is not rebuilt, settings must be changed
      sigma.setSetting("labelColor", { color: CANVAS_TEXT });
      sigma.setSetting("edgeLabelColor", { color: CANVAS_TEXT_2 });
      setThemeTick((t) => t + 1);
      sigma.refresh();
      // The world grid is redrawn only when the camera moves: add one stroke here, or it stays on the previous set of ink colors
      if (gridRef.current) drawWorldGrid(gridRef.current, sigma);
    });
    return () => {
      offTheme();

      sigma.kill();
      sigmaRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const [unscopedOpen, setUnscopedOpen] = useState(false);
  const empty = entityTypes.length === 0;

  return (
    <div className="h-full relative">
      {/* Legend + framing + entry for unscoped relations. No search box: find things through the left column */}
      {/* This row of the legend is **top-aligned** with the docked panel on the right (both start from top-3): previously the panel overlapped top-14,
          top-14leaving a 56px band on the right half that looked misaligned.
          When the panel is open, the right edge of this row yields to the panel's column (see .u-canvas-chrome-docked),
          otherwise in a narrow window the legend would slide under the panel */}
      <div
        className={cn(
          "absolute left-3 right-3 top-3 z-10 flex items-start gap-2 pointer-events-none",
          (selected?.kind === "class" || selected?.kind === "relation") &&
            "u-canvas-chrome-docked",
        )}
      >
        <div className="pointer-events-auto flex flex-wrap gap-2">
          {/* Static legend: what each kind of edge is called. It is not a switchable filter: an ontology has far fewer edges than the instance graph,
              so the space saved by hiding one kind of edge is not worth an extra layer of interaction.
              The rule entry appears only when there really are rules: a library without rules should not see a legend
              for something that does not exist */}
          {(
            [
              [S.ontology.schemaLegendInheritance, LEGEND_SUBCLASS],
              [S.ontology.schemaLegendRelation, LEGEND_RELATION],
              [S.ontology.schemaLegendDisjoint, LEGEND_DISJOINT],
              ...(rules.length
                ? ([[S.ontology.schemaLegendRule, LEGEND_RULE]] as const)
                : []),
            ] as const
          ).map(([label, color]) => (
            <span
              key={label}
              /* **Just the one class pill.** Same height, same inner padding, and same
                 corner radius as the clickable Pill beside it: the convention is 4 for chips and 6 for controls, but in this row the box height, padding
                 and font size are all the same, only 'can it be clicked' differs; a row of same-sized boxes with two corner radii
                 reads as misaligned, not as meaningful. `is-static` drops the hover brightening:
                 giving hover feedback to something that cannot be clicked is lying to the hand */
              className="u-pill is-static gap-2"
            >
              <span className="h-0.5 w-3 rounded-full" style={{ background: color }} />
              {label}
            </span>
          ))}

          {/* How many were not drawn: just an explanation, same look as the legend, not clickable. To see a particular class,
              click it in the left column */}
          {scope.hidden > 0 && (
            <Tooltip
              content={
                scope.basis === "top"
                  ? S.ontology.schemaScopeTopHint
                  : S.ontology.schemaScopeInUseHint
              }
            >
              <span className="u-pill is-static">
                {S.ontology.schemaMoreClasses(scope.hidden)}
              </span>
            </Tooltip>
          )}

          {schema.unscoped.length > 0 && (
            <Popover open={unscopedOpen} onOpenChange={setUnscopedOpen}>
              <PopoverTrigger asChild>
                <Pill active={unscopedOpen}>
                  {S.ontology.schemaUnscoped(schema.unscoped.length)}
                </Pill>
              </PopoverTrigger>
              <PopoverContent align="start" className="w-72 overflow-hidden p-0">
                  {/* The title row only says what this is; closing is handled by Esc, outside clicks and the pill itself */}
                  <div className="flex items-center gap-3 border-b border-line px-4 py-3">
                    <span className="min-w-0 flex-1 truncate text-body font-medium text-ink">
                      {S.ontology.schemaUnscoped(schema.unscoped.length)}
                    </span>
                  </div>
                  <p className="border-b border-line px-4 py-2 text-fine leading-relaxed text-ink-2">
                    {S.ontology.schemaUnscopedHint}
                  </p>
                  <div className="u-scroll flex max-h-64 flex-col overflow-y-auto p-2">
                    {schema.unscoped.map((r) => (
                      <Row
                        key={r.id}
                        className="text-small"
                        onClick={() => {
                          onSelect({ kind: "relation", id: r.id });
                          setUnscopedOpen(false);
                        }}
                      >
                        {r.label}
                      </Row>
                    ))}
                  </div>
              </PopoverContent>
            </Popover>
          )}
        </div>
      </div>

      {/* Canvas: the world-coordinate grid layer (moves with the camera) sits under the sigma WebGL layer, same as /graph */}
      <div className="absolute inset-0">
        <canvas ref={gridRef} className="absolute inset-0 h-full w-full" />
        <div ref={containerRef} className="absolute inset-0" />
      </div>

      {/* Bottom left: zoom + reset to home. No layout switching: the schema graph has only one deterministic layout,
          unlike the instance graph, which needs clustering by type or spreading into a ring */}
      <div className="absolute bottom-4 left-3 z-10 flex flex-col items-start gap-2">
        <ToolTower>
          <ToolButton
            label={S.ontology.schemaZoomIn}
            icon={<ZoomIn size={15} />}
            onClick={() => sigmaRef.current?.getCamera().animatedZoom({ duration: 220 })}
          />
          <ToolButton
            label={S.ontology.schemaZoomOut}
            icon={<ZoomOut size={15} />}
            onClick={() => sigmaRef.current?.getCamera().animatedUnzoom({ duration: 220 })}
          />
          <ToolDivider />
          <ToolButton
            label={S.ontology.schemaFitView}
            icon={<Maximize2 size={15} />}
            onClick={() => sigmaRef.current?.getCamera().animatedReset({ duration: 300 })}
          />
        </ToolTower>
      </div>

      {/* The ontology is still on its way: **most of this area can already be drawn**. The world-coordinate grid, the bottom-left zoom tower,
          and the three static legends are all independent of whether the ontology has been fetched; only the nodes are missing. So the spinner sits in the middle of the canvas,
          instead of replacing the whole area with a spinner: the latter would hide what is already ready.
          It is separate from 'there really are no classes': that sentence is a conclusion, this circle is a process, and looking alike would be misread */}
      {loading ? (
        <CanvasLoading />
      ) : empty ? (
        <div className="absolute inset-0 grid place-items-center pointer-events-none">
          <div className="text-center text-body text-ink-2 max-w-sm">
            {S.ontology.schemaEmpty}
            {onEmptyAction && (
              <div className="mt-3 pointer-events-auto">
                <Button size="sm" variant="secondary" onClick={onEmptyAction}>
                  {S.ontology.schemaEmptyAction}
                </Button>
              </div>
            )}
          </div>
        </div>
      ) : null}
    </div>
  );
}
