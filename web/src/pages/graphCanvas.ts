// The one machine shared by the two canvases (#496).
//
// `/graph` draws instances and facts, and the ontology page draws classes and relations. Their **data** is not shared, and neither is their meaning,
// but the machinery that turns data into a canvas you can see and click is one copy: the four-layer anatomy of the node program, the label
// appear/disappear timing, the hover card, the state table, the camera and dragging. Previously these were written out twice in two files, word for word the same:
// even the comment explaining why the label threshold is 5 and 0.8 was duplicated character for character. The cost is not the line count,
// it is **drift**: the dimming factor was 0.52 on one side and 0.55 on the other, and nobody knew which was right.
//
// The dividing line: this file owns "how it is drawn and how it moves", the page owns "what is drawn". The page projects its own data
// into one graphology graph, gives each node a type colour, and states which should be hidden; everything else is here.
//
// `graphVisuals.ts` holds the constants and drawing functions extracted in the previous round; this file is built on top of it.

import type Graphology from "graphology";
import Sigma from "sigma";
import type { Settings } from "sigma/settings";
import { createNodeBorderProgram } from "@sigma/node-border";

import { NodeSquareShellProgram } from "./squareShellProgram";
import {
  CANVAS_FONT,
  CANVAS_LABEL_SIZE,
  CANVAS_TEXT,
  CANVAS_TEXT_2,
  drawHoverCard,
  drawNodeLabel,
  HOVER_MUTE,
  lerpColor,
  mix,
  MUTED_SHELL,
  NODE_CORE_BASE,
  NODE_SHELL_BASE,
  RING_HOVER_MIX,
  RING_SELECT_MIX,
  TRANSPARENT,
  INK,
} from "./graphVisuals";

/* ============ Node program ============ */

/** Semantica node anatomy: state ring → outline → dark shell → lightly coloured core.
 *
 * One copy for both pages. Previously the default one was called `shell` on `/graph` and `circle` on the ontology page,
 * yet they were the same `createNodeBorderProgram`: two names for one thing */
export const NODE_TYPE_SHELL = "shell";
export const NODE_TYPE_SQUARE = "square";

/* ---------- Highlighted edges are drawn on top ---------- */

/** The type-name suffix for the "top layer" batch */
const TOP = "Top";

/** The type name an edge should use when it is highlighted.
 *
 * **`zIndex` does not work here.** sigma draws one program at a time, and `zIndex` only sorts within a batch;
 * which program goes first across programs depends on the order of the keys in `edgeProgramClasses`. So after selecting a node,
 * its bright lines are still cut by the dimmed edges of other programs: zoomed in, the white line is sliced
 * into thin black bars one by one.
 *
 * The fix is to **register the same program twice** (see `withTopLayer`): highlighted edges go in the batch of the copy registered later,
 * which is drawn last. Both pages fell into this trap: the graph page solved it first, and the ontology page fell into it again later,
 * so it lives here, not in either page. */
export function drawLast(type: string): string {
  return type.endsWith(TOP) ? type : type + TOP;
}

/** Pair an edge-program table with its "top layer" copy: the same program, with the key placed later.
 *  When highlighted, replace `res.type` with `drawLast(res.type)` to land in this batch. */
export function withTopLayer<T>(programs: Record<string, T>): Record<string, T> {
  return {
    ...programs,
    ...Object.fromEntries(
      Object.entries(programs).map(([name, program]) => [drawLast(name), program]),
    ),
  };
}

export const NODE_PROGRAMS = {
  [NODE_TYPE_SHELL]: createNodeBorderProgram({
    borders: [
      { size: { value: 0.1 }, color: { attribute: "ringColor" } },
      { size: { value: 0.07 }, color: { attribute: "borderColor" } },
      { size: { value: 0.3 }, color: { attribute: "shellColor" } },
      { size: { fill: true }, color: { attribute: "color" } },
    ],
  }),
  [NODE_TYPE_SQUARE]: NodeSquareShellProgram,
};

/* ============ Construction options ============ */

/** The few items that really differ between the two pages. Everything else is fixed in `sigmaOptions`: they decide
 *  what the canvas looks like and how it responds, and it would be a bug, not a feature, for the two pages to look different */
export interface CanvasOptions {
  /** Mapping from edge type names to programs. `/graph` only has straight lines and arcs; the ontology page also needs arrows */
  edgeProgramClasses: Settings["edgeProgramClasses"];
  defaultEdgeType: string;
  /** How far the camera can zoom out and in. The two graphs differ by several orders of magnitude in size, so their framing ranges are naturally different */
  minCameraRatio: number;
  maxCameraRatio: number;
  /** Whether edges get text. `/graph` edges are predicates and need it; the ontology page only labels relation edges */
  renderEdgeLabels?: boolean;
  minEdgeThickness?: number;
}

/** `new Sigma(g, el, { ...sigmaOptions(...), nodeReducer, edgeReducer })`.
 *
 * **The two reducers do not pass through here.** sigma's reducer types follow its own generic parameters, and passing them through a middle layer cuts off the inference;
 * and the **contents** of the reducers are each page's own business, so what is shared is the state table they call
 * (next section), not the functions themselves.
 *
 * Labels appear according to distance: far away only the shape is shown, and names appear only when close. **Tried without distance**
 * (threshold zeroed, filtering only by crowding): after zooming out, over a hundred names spread out and overprinted each other, unreadable and
 * impossible to point at accurately. Threshold 5, with 0.8 per 130px square: only half a step looser than before. **Loosening to 3 / 1.2 was tried,
 * and a screen with a hundred names spread across it was too noisy**. What is wanted here is "a few landmarks recognizable at a distance",
 * not "every point reports its name". When zoomed in, sigma lifts this cap itself by 1/ratio²
 * (see `getLabelsToDisplay`), so the closer you get, the more is revealed, with no ceiling.
 *
 * Edge text is the same tier as the nodes (fine) and the same secondary colour. Previously it was 9px/#a1a1a1: 9 is half the size of the smallest
 * text in the interface, and #a1a1a1 was the previous version's ink-2.
 *
 * Edge hover events are off by default; here they are all turned on, since both pages rely on `enterEdge` to make edges speak. */
export function sigmaOptions(o: CanvasOptions) {
  return {
    /* **Draw by level.** Both reducers have been setting `res.zIndex` (dimmed 0, neighbours 2,
       selected 3, pointed-at 4, highlighted edges 5), but sigma does not look at it by default: the drawing order is the buffer order,
       so a highlighted white edge is cut segment by segment by dark edges drawn later, looking as if it were broken by a dashed line.
       Turning it on makes those level values take effect. The cost is one extra sort per frame, which at the scale of a few hundred edges
       is negligible */
    zIndex: true,
    allowInvalidContainer: true,
    defaultNodeType: NODE_TYPE_SHELL,
    nodeProgramClasses: NODE_PROGRAMS,
    edgeProgramClasses: o.edgeProgramClasses,
    defaultEdgeType: o.defaultEdgeType,
    enableEdgeEvents: true,
    renderEdgeLabels: o.renderEdgeLabels ?? false,
    ...(o.minEdgeThickness === undefined
      ? {}
      : { minEdgeThickness: o.minEdgeThickness }),
    labelFont: CANVAS_FONT,
    labelSize: CANVAS_LABEL_SIZE,
    labelColor: { color: CANVAS_TEXT },
    labelRenderedSizeThreshold: 5,
    labelDensity: 0.8,
    labelGridCellSize: 130,
    minCameraRatio: o.minCameraRatio,
    maxCameraRatio: o.maxCameraRatio,
    edgeLabelSize: CANVAS_LABEL_SIZE,
    edgeLabelColor: { color: CANVAS_TEXT_2 },
    edgeLabelFont: CANVAS_FONT,
    defaultDrawNodeLabel: drawNodeLabel,
    defaultDrawNodeHover: drawHoverCard,
  };
}

/* ============ State table ============ */

/** The attribute bag a reducer receives and the one it hands back. sigma's types are `any` at this layer,
 *  and the two pages' attribute keys differ, so here it only recognizes "a bag of things" */
export type NodeAttrs = Record<string, unknown>;

/** The state ring takes the node's **own type colour**, not the `color` that has already been mixed with the shell colour: see the
 *  notes on RING_*_MIX in graphVisuals */
export function ownColorOf(attrs: NodeAttrs): string {
  return (attrs.typeColor as string) ?? NODE_CORE_BASE;
}

/** Pressed to the bottom: everything else exits, leaving only the shape.
 *
 * The factor used to differ between the two pages, 0.52 and 0.55, and nowhere was it explained why. The design table says 0.52,
 * and this unifies on it: that 0.55 was a slip when copying, not something anyone decided */
export const MUTE_SCALE = 0.52;

export function mutedNode(res: NodeAttrs, base: number): NodeAttrs {
  res.size = base * MUTE_SCALE;
  res.color = mix(MUTED_SHELL, NODE_CORE_BASE, 0.3);
  res.shellColor = MUTED_SHELL;
  res.borderColor = TRANSPARENT;
  res.ringColor = TRANSPARENT;
  res.label = "";
  res.zIndex = 0;
  return res;
}

/** On hover, the rest are muted one step by `HOVER_MUTE` (selection mutes them all the way down).
 *  Neighbours are not muted: hover answers "who is it connected to", and muting the neighbours too would mean not answering at all */
export function softMutedNode(
  res: NodeAttrs,
  attrs: NodeAttrs,
  base: number,
): NodeAttrs {
  res.size = base * (1 - (1 - MUTE_SCALE) * HOVER_MUTE);
  res.color = lerpColor(
    String(attrs.color ?? NODE_CORE_BASE),
    mix(MUTED_SHELL, NODE_CORE_BASE, 0.3),
    HOVER_MUTE,
  );
  res.shellColor = lerpColor(
    String(attrs.shellColor ?? NODE_SHELL_BASE),
    MUTED_SHELL,
    HOVER_MUTE,
  );
  res.borderColor = TRANSPARENT;
  res.ringColor = TRANSPARENT;
  res.label = "";
  res.zIndex = 0;
  return res;
}

/** The one being pointed at: ×1.08, minimum 10.4, a slightly white ring, so that it stands out.
 *  The backing of its name label is one step brighter and floats up, with the same shape as the selected state (selected is inverted:
 *  light background, dark text). Previously this was a floating two-line card that looked nothing like the selected state */
export function hoveredNode(
  res: NodeAttrs,
  attrs: NodeAttrs,
  base: number,
): NodeAttrs {
  res.size = Math.max(base * 1.08, 10.4);
  res.ringColor = mix(ownColorOf(attrs), INK, RING_HOVER_MIX);
  res.forceLabel = true;
  res.labelLift = true;
  res.zIndex = 4;
  return deferToHoverLayer(res);
}

/** Handed to the highlight layer to draw: the label layer skips this one (see the note in `drawNodeLabel`).
 *
 * sigma draws the name in both the label layer and the highlight layer for a node that is being pointed at. This marker exists to make way
 * for the label layer, and **it should only ever land on the one that is really being pointed at right now** — so it does not belong to `selectedNode`:
 * when the selected one is not being pointed at, the label layer is what draws it. */
export function deferToHoverLayer(res: NodeAttrs): NodeAttrs {
  res.hideBaseLabel = true;
  return res;
}

/** The selected one: **the name is inverted**.
 *
 * What is inverted is the label backing, not the node itself. The four-layer node anatomy (ring / outline / shell / core) is left untouched.
 * Tried letting the shell take the full type colour and the core fade to dark: the picture immediately became noisy, since one solid bright ring
 * pressed down all the neighbours around it, and the neighbours are exactly what should be read once something is selected. **Size is also nearly unchanged
 * (×1.02)**, since a node that gets fatter on selection changes the density of the whole graph as it looks.
 *
 * So "selected" is entirely expressed on the name: a light backing with dark text, the same shape flipped relative to the pointed-at one (dark backing with light text),
 * so you can tell "I am pointing at it" from "I selected it" at a glance, without a third marker.
 * The ring is kept, taking the tier that leans toward the node's own colour, so it is distinct from the slightly white hover ring. */
export function selectedNode(
  res: NodeAttrs,
  attrs: NodeAttrs,
  base: number,
): NodeAttrs {
  res.size = Math.max(base * 1.02, 9.2);
  res.ringColor = mix(ownColorOf(attrs), INK, RING_SELECT_MIX);
  res.forceLabel = true;
  res.labelLift = true;
  // **Inverted**: light background, dark text. The one being pointed at is a label whose backing is one step brighter; the same shape flipped —
  // so you can tell "I am pointing at it" from "I selected it" at a glance, without needing another marker
  res.labelInvert = true;
  // One step bolder. The inverted form has to be looked at to be read; the weight is still distinguishable in peripheral vision
  res.labelBold = true;
  res.zIndex = 3;
  return res;
}

/** The neighbours of the selected one.
 *
 * `scale` differs between the two pages, and this spot is a real design difference rather than drift: `/graph` has thousands of nodes,
 * and 0.76 makes room for the selected path; the ontology page has a few dozen classes, where shrinking them would look limp */
export function neighborNode(
  res: NodeAttrs,
  base: number,
  scale = 1,
): NodeAttrs {
  if (scale !== 1) res.size = Math.max(base * scale, 4);
  res.zIndex = 2;
  return res;
}

/* ============ Camera ============ */

export interface Point {
  x: number;
  y: number;
}

export function nodePosition(sigma: Sigma, id: string): Point {
  const graph = sigma.getGraph();
  return {
    x: graph.getNodeAttribute(id, "x") as number,
    y: graph.getNodeAttribute(id, "y") as number,
  };
}

/** Whether the node is inside the viewport at this moment (computed from the camera of the last render) */
export function nodeInView(sigma: Sigma, id: string, margin = 48): boolean {
  const { x, y } = sigma.graphToViewport(nodePosition(sigma, id));
  const { width, height } = sigma.getDimensions();
  return (
    x >= margin && x <= width - margin && y >= margin && y <= height - margin
  );
}

/** Push the camera onto a node. sigma's camera coordinates are "in-frame" coordinates normalized to [0,1],
 *  not graph coordinates: feeding graph coordinates directly (x values often in the hundreds) makes the camera fly tens of thousands of pixels off-screen, and the canvas
 *  goes blank. sigma has no public graph→frame conversion, so it takes a detour through the viewport: `graphToViewport` uses
 *  the matrix from the last render, and `viewportToFramedGraph` uses the same camera, so going there and back cancels the camera out,
 *  and what remains is the in-frame coordinate */
export function focusNode(sigma: Sigma, id: string, maxRatio = 0.5): void {
  if (!sigma.getGraph().hasNode(id)) return;
  const framed = sigma.viewportToFramedGraph(
    sigma.graphToViewport(nodePosition(sigma, id)),
  );
  const ratio = Math.min(sigma.getCamera().ratio, maxRatio);
  sigma
    .getCamera()
    .animate({ x: framed.x, y: framed.y, ratio }, { duration: 300 });
}

/** **Move** a freshly computed graph into the one being rendered.
 *
 * sigma is bound to the graphology instance it was given at construction, so "the graph changed" is easily written as
 * "kill and rebuild". The cost is not small: the GPU buffers, camera, hover and selection state are all lost, and the first frame must be run again.
 * The ontology page goes through this whole thing just to reveal a class in the picture, and the picture visibly stutters.
 *
 * graphology emits an event for every add, delete and change, and sigma listens, so **changing in place is what it is designed to support**.
 * What is done here is the plainest reconciliation: delete the extras, add the missing, and overwrite the properties of what remains.
 * At the scale of dozens or hundreds of classes, one full overwrite is simpler than computing an exact diff, and less error-prone.
 *
 * Deleting a node also removes its edges, so take a snapshot first, then iterate (`nodes()` returns an array). */
export function syncGraph(live: Graphology, next: Graphology): void {
  for (const node of live.nodes()) if (!next.hasNode(node)) live.dropNode(node);
  next.forEachNode((node, attrs) => {
    if (live.hasNode(node)) live.replaceNodeAttributes(node, { ...attrs });
    else live.addNode(node, { ...attrs });
  });
  for (const edge of live.edges()) if (!next.hasEdge(edge)) live.dropEdge(edge);
  next.forEachEdge((edge, attrs, source, target) => {
    if (live.hasEdge(edge)) live.replaceEdgeAttributes(edge, { ...attrs });
    else live.addEdgeWithKey(edge, source, target, { ...attrs });
  });
}

/* ============ Dragging ============ */

/** Drag a node. **On press it only records a candidate; it becomes a drag only once the displacement passes a threshold**: otherwise a plain click
 *  would also be treated as a 0-pixel drag, and the moment of mouse release would fight with click-to-select.
 *
 * At the moment it is promoted, freeze the bounding box: while dragging, when the node flies toward the edge of the screen, the camera should not
 * auto-zoom to "fit" the new bounding box, since then the whole graph would shake with every drag. On release, unfreeze it: "return to place" must show
 * the new position just moved to, and cannot still be computed from the old range from before the drag started.
 *
 * The two pages answer "what else to do during the drag" differently: `/graph` feeds the cursor position into the force simulation,
 * and the ontology page records the position in its own coordinate table. So those two things are callbacks, not here. */
export function attachDrag(
  sigma: Sigma,
  o: {
    /** The moment it is promoted to a drag. `/graph` wakes the force simulation here */
    onStart?: (node: string) => void;
    /** The new position each frame. The two writes into the graph have already been done */
    onMove?: (node: string, pos: Point) => void;
    /** On release. `/graph` schedules the force simulation to stop here */
    onEnd?: (node: string) => void;
    /** Who owns the cursor shape; if not given, the cursor is left alone */
    container?: () => HTMLElement | null;
    /** Whether hovering over a node should look "grabbable". The ontology page supplies it; `/graph` does not */
    hovering?: () => boolean;
  } = {},
): void {
  const graph = sigma.getGraph();
  let dragCandidate: string | null = null;
  let downPoint: Point | null = null;
  let dragged: string | null = null;
  const cursor = (v: string) => {
    const el = o.container?.();
    if (el) el.style.cursor = v;
  };

  sigma.on("downNode", (e) => {
    dragCandidate = e.node;
    downPoint = { x: e.event.x, y: e.event.y };
  });
  sigma.getMouseCaptor().on("mousemovebody", (e) => {
    if (!dragCandidate) return;
    if (!dragged) {
      if (!downPoint || Math.hypot(e.x - downPoint.x, e.y - downPoint.y) < 4)
        return;
      dragged = dragCandidate;
      cursor("grabbing");
      if (!sigma.getCustomBBox()) sigma.setCustomBBox(sigma.getBBox());
      o.onStart?.(dragged);
    }
    const pos = sigma.viewportToGraph(e);
    graph.setNodeAttribute(dragged, "x", pos.x);
    graph.setNodeAttribute(dragged, "y", pos.y);
    o.onMove?.(dragged, pos);
    // Stop the camera from following the pan
    e.preventSigmaDefault();
    e.original.preventDefault();
    e.original.stopPropagation();
  });
  const endDrag = () => {
    dragCandidate = null;
    downPoint = null;
    if (!dragged) return;
    const node = dragged;
    dragged = null;
    cursor(o.hovering?.() ? "grab" : "");
    sigma.setCustomBBox(null);
    o.onEnd?.(node);
  };
  sigma.getMouseCaptor().on("mouseup", endDrag);

  // Hovering over a node shows a "grabbable" hint: an interaction that has to be guessed at is as good as absent
  if (o.hovering) {
    sigma.on("enterNode", () => {
      if (!dragged) cursor("grab");
    });
    sigma.on("leaveNode", () => {
      if (!dragged) cursor("");
    });
  }
}
