// Node visuals: colour maths, the four-layer shell colouring, capsule labels and hover cards, and the world-coordinate grid.
// Extracted from Graph.tsx: these are the parts that decide how to draw a node with a type colour. They carry no instance-graph
// semantics (no derived / contested / temporal concepts), so the ontology-mode graph reuses them unchanged,
// and the graph and the schema graph look as if drawn by the same engine, rather than each having its own visual identity.
//
// **Edge drawing is not extracted here**: the /graph edges carry state that only the instance graph has (derived / contested / temporal),
// while the schema-graph edges carry ontology semantics (subClassOf / relation / disjoint). What an edge means
// is simply not the same thing in the two, and sharing the code would weld two sets of semantics together.
import type Sigma from "sigma";

/* Canvas palette. The structure is taken from Semantica's GraphWorkspace source; the base colours have been neutralised:
   Semantica's original is a steel-blue family (#0B1320/#5A7A9E/#7A92AE), replaced under the settled principle "chrome has no colour cast,
   colour belongs only to the data" with pure greys of the same lightness, while the type-colour mixing ratios stay the same */
export let NODE_SHELL_BASE = "#121212"; // dark base of the node shell (neutralised from #0B1320)
export let NODE_CORE_BASE = "#767676"; // grey of the node core (neutralised from #5A7A9E)
export let NODE_BORDER_BASE = "#909090"; // node outline (neutralised from #7A92AE)
/* The **ratios are shared by both themes**; only the three base colours above change with the theme. A node is always
   "shell + coloured core": the shell follows the paper (dark #121212, light #ffffff), and the type colour seeps into the shell at 14%,
   while the core takes 50%, so the middle point in both themes is the class's own colour;
   only the backdrop it sits on, black or white, differs. */
export const NODE_TINT_MIX = 0.14; // ratio of the type colour mixed into the shell
export const NODE_CORE_MIX = 0.5; // ratio of the type colour mixed into the core
/* The state ring takes **the node's own type colour**, not a fixed hue. It is mixed toward white rather than used as is:
   the ring is drawn on the node itself, and with the same colour and lightness it would not read as a ring. **On hover it is mixed whiter, on selection less so**:
   on hover the rest of the graph is not dimmed, so the ring must jump out at once from a tangle of lines; on selection the rest are all dimmed,
   and the node is already isolated, so the ring should say "who it is", hence it stays closer to its own
   colour. */
export const RING_HOVER_MIX = 0.7; // hover: whiter, to make it jump out
export const RING_SELECT_MIX = 0.35; // selected: closer to the base colour, so it is recognisable
export const TRANSPARENT = "rgba(0,0,0,0)";
export let MUTED_SHELL = "#151515";
/* How much the rest is dimmed on hover. **Lighter than on selection** (selection dims all the way): hover follows the mouse,
   changing every time it crosses a node, so dimming fully would make the whole canvas flicker constantly */
export const HOVER_MUTE = 0.78;
export let PILL_BG = "rgba(12,12,12,0.9)";
/** The backdrop of the spot being pointed at. The resting one is `PILL_BG`; the selected one is inverted; see `drawNodeLabel` */
export let PILL_BG_HOVER = "rgba(42,42,42,0.96)";
/** The backdrop of the selected tier (inverted). **Not the same as `PILL_TEXT`**: on the paper backdrop the ink is near black */
export let PILL_INVERT = "#ededed";
export let PILL_BORDER = "rgba(255,255,255,0.14)"; // --u-line-strong
export let PILL_TEXT = "#ededed"; // --u-text
/* Halo for bare text: an outline in the canvas colour (--u-ground), only so that lines running under the text are
   held back. Not a shadow: a shadow would smear into a dirty blob amid thin lines */
export let LABEL_HALO = "rgba(10,10,10,0.92)";
/** The surface of floating layers, borrowed from `.u-pop` (the near-solid tier used by tooltip / toast) */
export let POP_BG = "rgba(16,16,16,0.98)";

/* Canvas text uses the same scale as the interface. **The canvas cannot read CSS variables**, so a copy of the values in
   `styles.css` is mirrored here: it is the source, so remember to come back and change this when that changes.
   These numbers used to be grown ad hoc (node 11, edge 9, type row 10, text colour #e5e5e5 /
   #a1a1a1): after the whole font scale moved up one tier, the canvas became the only place still on the old scale,
   and 9px is half the size of the smallest text in the interface */
export const CANVAS_FONT = '"Geist", "Inter", "Noto Sans SC", sans-serif';
export let CANVAS_TEXT = "#ededed"; // --u-text
export let CANVAS_TEXT_2 = "#a8a8a8"; // --u-text-2
/* Text drawn between nodes and lines is one tier smaller than the interface base (11). **The canvas is not the interface**:
   this text sits on a field of lines and dots, and its neighbours are 5–13px nodes, not body text on a page;
   12 here looks heavier than the thing it labels. Floating cards above the canvas do not count: those are tooltips
   and follow the interface scale (see CANVAS_TITLE_SIZE / CANVAS_META_SIZE) */
export const CANVAS_LABEL_SIZE = 11;
export const CANVAS_TITLE_SIZE = 14; // --text-body
export const CANVAS_META_SIZE = 12; // --text-fine

/** Accepts `#rgb` / `#rgba` / `#rrggbb` / `#rrggbbaa`.
 *
 * **Accepting the short forms is required, not a nicety**: these values are read back from the CSS tokens, and at build time
 * Lightning CSS compresses `#ffffff` to `#fff`. Previously only six-digit forms were recognised, so the light-theme
 * white tokens all fell through to the fallback below and quietly became mid-grey 128,128,128. What showed up on the canvas
 * was "every node a grey lump in light mode": the node shell's base should have been paper white but read as mid-grey,
 * and one of the four layers of the recipe collapsed. In dev the CSS is not compressed, so only the built bundle triggered it. */
export function hexToRgb(hex: string): [number, number, number] {
  const [r, g, b] = hexToRgba(hex);
  return [r, g, b];
}

/** Same as above, alpha included. The last two digits of the eight-digit form are the alpha; if absent it is 1 */
export function hexToRgba(hex: string): [number, number, number, number] {
  const m = /^#?([0-9a-f]{3,8})$/i.exec(hex.trim());
  if (!m) return [128, 128, 128, 1];
  let h = m[1];
  // Each digit of the short form is doubled: #1a2 → #11aa22, #1a2f → #11aa22ff
  if (h.length === 3 || h.length === 4) h = [...h].map((c) => c + c).join("");
  if (h.length !== 6 && h.length !== 8) return [128, 128, 128, 1];
  const v = parseInt(h.slice(0, 6), 16);
  const a = h.length === 8 ? parseInt(h.slice(6), 16) / 255 : 1;
  return [(v >> 16) & 255, (v >> 8) & 255, v & 255, a];
}

/** Mixes c1 toward c2 in proportion t */
/** Mixes two colours in proportion. **Both ends go through `parseRgba`, not `hexToRgb`**: the values read back from tokens
 *  are written as `rgb(23,23,23)`, and a parser that only accepts `#rrggbb` would quietly fall back to mid-grey
 *  (`hexToRgb` returns 128,128,128 when it does not recognise the input), so `mix(type colour, INK, t)` mixes
 *  not ink but a grey, and the ring for selection and hover therefore looks dull in both themes.
 *  Before, `INK` was the literal "#ffffff" and the problem did not show; once 0038 changed it to be read from the tokens, it did. */
export function mix(c1: string, c2: string, t: number): string {
  const [r1, g1, b1] = parseRgba(c1);
  const [r2, g2, b2] = parseRgba(c2);
  const f = (a: number, b: number) => Math.round(a + (b - a) * t);
  return `rgb(${f(r1, r2)},${f(g1, g2)},${f(b1, b2)})`;
}

/** Accepts rgb / rgba / #hex, fading from `from` to `to` by t, **with alpha fading too**.
 *
 * Division of labour with `mix`: that one takes only hex and only adjusts the type colour toward the shell colour in proportion (the node's recipe);
 * this one handles the edge's `rgba(...)` and the fade-in and fade-out, so both ends must parse and alpha must not be lost */
function parseRgba(c: string): [number, number, number, number] {
  if (c.startsWith("#")) return hexToRgba(c);
  const m = c.match(
    /rgba?\(\s*([\d.]+)[,\s]+([\d.]+)[,\s]+([\d.]+)(?:[,\s/]+([\d.]+))?/,
  );
  if (!m) return [128, 128, 128, 1];
  return [+m[1], +m[2], +m[3], m[4] !== undefined ? +m[4] : 1];
}

export function lerpColor(from: string, to: string, t: number): string {
  const a = parseRgba(from);
  const b = parseRgba(to);
  const f = (i: number) => a[i] + (b[i] - a[i]) * t;
  return `rgba(${Math.round(f(0))},${Math.round(f(1))},${Math.round(f(2))},${f(3).toFixed(3)})`;
}

/* Node label: **normally a line of bare text; only the one pointed at or selected gets a backdrop.**
   Previously it was always a capsule (dark background with a 14% white outline ring), and in the interface's vocabulary that look
   means "status": Ready, 3 dropped, System admin. A node's name is not a status; it is the thing itself, yet it wears status clothes
   and is brighter than any other chip around it.
   Now the canvas stays quiet, and attention lands where it belongs: the same logic as "hover dims everything else,
   and selection leaves only one path". */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export function drawNodeLabel(
  ctx: CanvasRenderingContext2D,
  data: any,
  _settings: any,
): void {
  if (!data.label) return;
  /* The one being pointed at is not drawn on this layer. sigma makes two passes per frame: `renderLabels` lays the label layer,
     and `renderHighlightedNodes` hands hoveredNode to drawHoverCard on the highlight layer above;
     **and the former does not exclude the latter**. Both passes draw, so the same backdrop is drawn on two stacked canvases.
     With a solid fill this does not show, but with a shadow it does: `shadowBlur` is applied twice, so the black halo doubles in depth;
     when unselected the backdrop is `rgba(12,12,12,0.9)`, which after stacking comes to nearly 0.99, more opaque than the token defines.
     The one that yields is this layer, because the highlight layer is drawn on top */
  if (data.hideBaseLabel) return;
  const size = CANVAS_LABEL_SIZE;
  /* The selected name is **one weight heavier**: 500 → 600, the same tier as other emphasis in the interface,
     not a separate font weight. The reverse-colour backdrop has to face it to be read, and the weight is told apart even in peripheral vision:
     only one name on a screen is heavier.
     **It must be set before measuring the width**: the backdrop is sized by `measureText`; if you measure and then change the weight,
     the bolder text overflows outside the backdrop */
  ctx.font = `${data.labelBold ? 600 : 500} ${size}px ${CANVAS_FONT}`;
  ctx.textBaseline = "middle";
  const padX = 8;
  const padY = 3;
  const w = ctx.measureText(data.label).width;
  /* By default a name hangs at the upper right of the node. **Those that touch the canvas edge flip over**: after zooming in, nodes often
     lean against the edge of the viewport, and if the name were drawn as usual the whole thing would land outside the canvas, looking as if the text vanished on zoom.
     If there is no room, flip it to the left or below: the node is on screen, so the name is on screen too */
  const dx = Math.max(data.size * 0.7, 12);
  const dy = Math.max(data.size * 0.9, 10) + size / 2 + padY;
  const dpr = window.devicePixelRatio || 1;
  const vw = ctx.canvas.width / dpr;
  const x = data.x + dx + w + padX > vw ? data.x - dx - w : data.x + dx;
  const y = data.y - dy - size / 2 < 0 ? data.y + dy : data.y - dy;
  ctx.save();
  /* **A name is always a plate**: one backdrop plus a fine border, rounded cell (4). The three tiers change only the colour;
     the shape and position never move:
       resting: the `--u-pill-bg` backdrop with a `--u-line-strong` border
       pointed at: `--u-pill-bg-hover`, backdrop one tier brighter, and it floats up (shadow)
       selected: inverted (light backdrop, dark text), one weight heavier, and it also floats up
     Previously the resting tier was bare text with a ring of canvas-coloured outline, and only the pointed-at or selected ones had a backdrop. Bare text in a field of
     lines and nodes reads with effort: the outline holds back the lines but not the uneven shades behind them,
     whereas a plate carries its own stable backdrop. The cost is a busier picture, so the plate's border is one fine line and the resting tier has no shadow:
     with dozens of plates on screen, if each floated they would blur into one mess. */
  const h = size + padY * 2;
  if (data.labelLift) {
    ctx.shadowColor = token("--u-shadow", "rgba(0,0,0,0.6)");
    ctx.shadowBlur = 12;
  }
  ctx.beginPath();
  ctx.roundRect(x - padX, y - h / 2, w + padX * 2, h, 4);
  ctx.fillStyle = data.labelInvert
    ? PILL_INVERT
    : data.labelLift
      ? PILL_BG_HOVER
      : PILL_BG;
  ctx.fill();
  // The shadow follows the backdrop only: stroking again would double the black outline's depth
  ctx.shadowBlur = 0;
  /* The inverted tier has no outline: a light backdrop already separates itself from the canvas, and stroking it again looks like two layers of skin */
  if (!data.labelInvert) {
    ctx.lineWidth = 1;
    ctx.strokeStyle = PILL_BORDER;
    ctx.stroke();
  }
  ctx.fillStyle = data.labelInvert ? LABEL_HALO : PILL_TEXT;
  ctx.fillText(data.label, x, y);
  ctx.restore();
}

/* What to draw when a node is pointed at. **It is that label plate** (`drawNodeLabel`), not another card.
   Previously this was a floating two-line card, with the name on one line and the type on the other, positioned at the node's upper right,
   so the two neighbouring states "pointed at" and "selected" looked like two completely different things: a float, and a plate stuck to the node.
   The type line's information is clearer in the right-hand panel, and the cost of every node passed over is one more card on the picture.
   (The name is kept so that the type line is clear in the panel.)
   The function name is kept because sigma's `defaultDrawNodeHover` recognises it */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export function drawHoverCard(
  ctx: CanvasRenderingContext2D,
  data: any,
  settings: any,
): void {
  // This layer does not honour `hideBaseLabel`: that gate says exactly "this one is handed to the highlight layer",
  // and if the highlight layer then also steps aside, nobody draws it
  drawNodeLabel(
    ctx,
    data.hideBaseLabel ? { ...data, hideBaseLabel: false } : data,
    settings,
  );
}

/* World-coordinate grid: fades in and out in tiers as the camera zooms */
const GRID_BASE_WORLD = 24; // base world cell spacing (matches the layout at roughly 300 scale)
const GRID_FADE_IN_PX = 13;
const GRID_FULL_PX = 52;
const GRID_MAX_LEVEL_PX = 480;
const GRID_MAX_ALPHA = 0.055;

export function drawWorldGrid(canvas: HTMLCanvasElement, sigma: Sigma): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  const { width, height } = sigma.getDimensions();
  const dpr = window.devicePixelRatio || 1;
  const pw = Math.round(width * dpr);
  const ph = Math.round(height * dpr);
  if (canvas.width !== pw || canvas.height !== ph) {
    canvas.width = pw;
    canvas.height = ph;
  }
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, width, height);
  if (width <= 0 || height <= 0) return;

  // World → screen: two probe points give the pixels per world unit and the origin position (scene without camera rotation)
  const p0 = sigma.graphToViewport({ x: 0, y: 0 });
  const p1 = sigma.graphToViewport({ x: 1, y: 0 });
  const ppw = p1.x - p0.x;
  if (!Number.isFinite(ppw) || ppw <= 0) return;

  // Finest visible tier: the smallest power-of-four cell spacing whose screen spacing is at least the fade-in threshold
  let spacing = GRID_BASE_WORLD;
  while (spacing * ppw < GRID_FADE_IN_PX) spacing *= 4;
  while (spacing * ppw >= GRID_FADE_IN_PX * 4) spacing /= 4;

  for (let sp = spacing; sp * ppw < GRID_MAX_LEVEL_PX; sp *= 4) {
    const ss = sp * ppw;
    const t = Math.min(
      1,
      (ss - GRID_FADE_IN_PX) / (GRID_FULL_PX - GRID_FADE_IN_PX),
    );
    if (t <= 0) continue;
    // The grid is ink-coloured, not white: white on a dark backdrop, near black on a paper backdrop (0038). Opacity is unchanged
    ctx.strokeStyle = `rgba(${INK_RGB},${(GRID_MAX_ALPHA * t).toFixed(4)})`;
    ctx.lineWidth = 1;
    ctx.beginPath();
    const startX = ((p0.x % ss) + ss) % ss;
    for (let x = startX; x <= width; x += ss) {
      const px = Math.round(x) + 0.5;
      ctx.moveTo(px, 0);
      ctx.lineTo(px, height);
    }
    const startY = ((p0.y % ss) + ss) % ss;
    for (let y = startY; y <= height; y += ss) {
      const py = Math.round(y) + 0.5;
      ctx.moveTo(0, py);
      ctx.lineTo(width, py);
    }
    ctx.stroke();
  }
}

/* ---------- Readers of the tokens (0038 light edition) ----------
 * The canvas cannot read var(), so every colour the canvas uses is read from the tokens computed on <html>.
 * The initial values of the `let`s above are the dark theme, and only apply before the first frame and in tests with no DOM;
 * `refreshPalette()` replaces all of them with the current theme's values at startup and on every theme switch.
 * An ES module export is a live binding: assign here, and the importing side reads the new value. */
export let EDGE = "rgba(163,163,163,0.2)";
export let EDGE_INFERRED = "rgba(163,163,163,0.1)";
export let EDGE_DIM = "#141414";
export let EDGE_FOCUS = "rgba(255,255,255,0.55)";
export let EDGE_DERIVED = "rgba(231,197,124,0.42)";
export let EDGE_DERIVED_DIM = "rgba(231,197,124,0.14)";
export let EDGE_FOCUS_DERIVED = "rgba(255,214,140,0.95)";
export let EDGE_CONTEST = "rgba(255,106,61,0.55)";
export let EDGE_FOCUS_CONTEST = "rgba(255,106,61,1)";
export let EDGE_SUBCLASS = "rgba(235,235,235,0.55)";
export let EDGE_SUBCLASS_FOCUS = "rgba(255,255,255,0.95)";
export let EDGE_RELATION = "rgba(128,128,128,0.3)";
export let EDGE_RELATION_FOCUS = "rgba(255,255,255,0.6)";
export let EDGE_DISJOINT = "rgba(255,157,175,0.45)";
export let EDGE_DISJOINT_FOCUS = "rgba(255,157,175,0.9)";
export let EDGE_RULE = "rgba(196,165,255,0.5)";
export let EDGE_RULE_FOCUS = "rgba(196,165,255,0.95)";
export let EDGE_SCHEMA_DIM = "rgba(48,48,48,0.4)";
export let LEGEND_SUBCLASS = "#ebebeb";
export let LEGEND_RELATION = "#8c8c8c";
export let LEGEND_DISJOINT = "#ff9daf";
export let LEGEND_RULE = "#c4a5ff";
export let SCRUB_PAST = "rgba(255,255,255,0.32)";
export let SCRUB_PLAY = "rgba(255,255,255,0.62)";
export let SCRUB_FUTURE = "rgba(255,255,255,0.04)";
/** The brightest tier of ink (white on dark, near black on light): the ring mixes toward it, and the login scene's particles use it */
export let INK = "#ffffff";
/** The ink triple, for places that need to adjust opacity themselves */
export let INK_RGB = "255,255,255";

function token(name: string, fallback: string): string {
  if (typeof document === "undefined") return fallback;
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}
function rgbOf(triplet: string, alpha: number): string {
  return `rgba(${triplet.replace(/\s+/g, "")},${alpha})`;
}

/** Ink by opacity: the login scene's particles, edges and pulses all use it; the page no longer assembles rgba on its own */
export function inkAt(alpha: number): string {
  return rgbOf(INK_RGB, alpha);
}

/** On a light theme, flattens an rgba edge colour onto the backdrop colour into an opaque rgb.
 *
 * sigma's edge shader uses premultiplied blending (ONE, ONE_MINUS_SRC_ALPHA) but does not premultiply the RGB,
 * so alpha cannot darken an edge; it only **adds** it onto the background: a little grey on black is a faint line,
 * while the same rgba(90,90,90,0.25) added on paper overflows to white, and that is where "all lines are white when nothing is selected" comes from.
 * Darkness has to be encoded in RGB (the same thing the comment on EDGE_DIM in Graph.tsx says).
 * The dark set was tuned under additive blending and is left alone; on light, alpha is flattened away here. */
export function flattenOnLight(color: string): string {
  if (typeof document === "undefined" || document.documentElement.dataset.theme !== "light") {
    return color;
  }
  /* **Both notations must be recognised.** The token is written as `rgba(176,120,20,0.6)`, and at build time Lightning CSS
     compresses it to `#b0781499`: the same colour in hex with alpha. Previously there was only a regex for `rgba(...)`, so the compressed form
     slipped through entirely, and the semi-transparent value went to sigma unflattened, so that derived edge was **added** on paper as a bright yellow streak.
     (It did not blow up earlier because another bug covered it: `hexToRgb`, which only recognised six digits, read `#b0781499` as an opaque mid-grey,
     so the line was drawn grey.
     Two errors combining into a passable result; fixing one exposed the other.)
     Going through `parseRgba` means the notation does not matter; an alpha that is already 1 is returned as is, saving a pointless rewrite. */
  const [r, g, b, a] = parseRgba(color);
  if (a >= 1) return color;
  const ground = token("--u-ground-rgb", "250,250,250")
    .split(",")
    .map((v) => Number(v.trim()));
  const ch = (v: number, i: number) => Math.round(v * a + ground[i] * (1 - a));
  return `rgb(${ch(r, 0)},${ch(g, 1)},${ch(b, 2)})`;
}

/** Called once at startup and after a theme switch, then `sigma.refresh()`. */
export function refreshPalette() {
  NODE_SHELL_BASE = token("--u-node-shell", NODE_SHELL_BASE);
  NODE_CORE_BASE = token("--u-node-core", NODE_CORE_BASE);
  NODE_BORDER_BASE = token("--u-node-border", NODE_BORDER_BASE);
  MUTED_SHELL = token("--u-node-muted", MUTED_SHELL);
  PILL_BG = token("--u-pill-bg", PILL_BG);
  PILL_BG_HOVER = token("--u-pill-bg-hover", PILL_BG_HOVER);
  PILL_INVERT = token("--u-pill-invert", PILL_INVERT);
  PILL_BORDER = token("--u-line-strong", PILL_BORDER);
  PILL_TEXT = token("--u-text", PILL_TEXT);
  LABEL_HALO = token("--u-halo", LABEL_HALO);
  POP_BG = token("--u-pop-surface", POP_BG);
  CANVAS_TEXT = token("--u-text", CANVAS_TEXT);
  CANVAS_TEXT_2 = token("--u-text-2", CANVAS_TEXT_2);
  EDGE = token("--u-edge", EDGE);
  EDGE_INFERRED = token("--u-edge-inferred", EDGE_INFERRED);
  EDGE_DIM = token("--u-edge-dim", EDGE_DIM);
  EDGE_FOCUS = token("--u-edge-focus", EDGE_FOCUS);
  EDGE_DERIVED = token("--u-edge-derived", EDGE_DERIVED);
  EDGE_DERIVED_DIM = token("--u-edge-derived-dim", EDGE_DERIVED_DIM);
  EDGE_FOCUS_DERIVED = token("--u-edge-derived-focus", EDGE_FOCUS_DERIVED);
  EDGE_SUBCLASS = token("--u-edge-subclass", EDGE_SUBCLASS);
  EDGE_SUBCLASS_FOCUS = token("--u-edge-subclass-focus", EDGE_SUBCLASS_FOCUS);
  EDGE_RELATION = token("--u-edge-relation", EDGE_RELATION);
  EDGE_RELATION_FOCUS = token("--u-edge-relation-focus", EDGE_RELATION_FOCUS);
  EDGE_SCHEMA_DIM = token("--u-edge-schema-dim", EDGE_SCHEMA_DIM);
  SCRUB_PAST = token("--u-scrub-past", SCRUB_PAST);
  SCRUB_PLAY = token("--u-scrub-play", SCRUB_PLAY);
  SCRUB_FUTURE = token("--u-scrub-future", SCRUB_FUTURE);
  INK_RGB = token("--u-ink-rgb", INK_RGB);
  INK = rgbOf(INK_RGB, 1);
  // Semantic colours take opacity by triple: contested edges, disjoint edges, rule edges
  const contest = token("--u-contest-rgb", "255,106,61");
  const danger = token("--u-danger-rgb", "255,157,175");
  const violet = token("--u-violet-rgb", "196,165,255");
  EDGE_CONTEST = rgbOf(contest, 0.55);
  EDGE_FOCUS_CONTEST = rgbOf(contest, 1);
  EDGE_DISJOINT = rgbOf(danger, 0.45);
  EDGE_DISJOINT_FOCUS = rgbOf(danger, 0.9);
  EDGE_RULE = rgbOf(violet, 0.5);
  EDGE_RULE_FOCUS = rgbOf(violet, 0.95);
  // Canvas edges: on a light theme, flattened onto the backdrop colour (see flattenOnLight)
  EDGE = flattenOnLight(EDGE);
  EDGE_INFERRED = flattenOnLight(EDGE_INFERRED);
  EDGE_FOCUS = flattenOnLight(EDGE_FOCUS);
  EDGE_DERIVED = flattenOnLight(EDGE_DERIVED);
  EDGE_DERIVED_DIM = flattenOnLight(EDGE_DERIVED_DIM);
  EDGE_FOCUS_DERIVED = flattenOnLight(EDGE_FOCUS_DERIVED);
  EDGE_CONTEST = flattenOnLight(EDGE_CONTEST);
  EDGE_FOCUS_CONTEST = flattenOnLight(EDGE_FOCUS_CONTEST);
  EDGE_SUBCLASS = flattenOnLight(EDGE_SUBCLASS);
  EDGE_SUBCLASS_FOCUS = flattenOnLight(EDGE_SUBCLASS_FOCUS);
  EDGE_RELATION = flattenOnLight(EDGE_RELATION);
  EDGE_RELATION_FOCUS = flattenOnLight(EDGE_RELATION_FOCUS);
  EDGE_DISJOINT = flattenOnLight(EDGE_DISJOINT);
  EDGE_DISJOINT_FOCUS = flattenOnLight(EDGE_DISJOINT_FOCUS);
  EDGE_RULE = flattenOnLight(EDGE_RULE);
  EDGE_RULE_FOCUS = flattenOnLight(EDGE_RULE_FOCUS);
  EDGE_SCHEMA_DIM = flattenOnLight(EDGE_SCHEMA_DIM);
  LEGEND_SUBCLASS = token("--u-text", LEGEND_SUBCLASS);
  LEGEND_RELATION = token("--u-text-2", LEGEND_RELATION);
  LEGEND_DISJOINT = token("--u-danger", LEGEND_DISJOINT);
  LEGEND_RULE = token("--u-violet", LEGEND_RULE);
}
