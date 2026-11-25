// The citation markers in the body text.
//
// The model writes `[1]`, `[1][2]`, `[1, 2]`, and `[1, 2]` with a full-width comma. **Which are drawn as superscripts and which are listed
// in the footnote list must use one and the same rule**, otherwise a number you can click in the body has no matching row below.
// So there is only one shape here, and `liveAnswer.citedSources` and `rehypeCitations` share it.
// **The rule also lives in one place**: the footnotes used to run the regex over the whole raw text, so `[1, 2]` in code blocks and links
// `[1](…)` were counted too, while superscripts are drawn only on body text. `citedNumbers` uses the same parser as rendering,
// and skips the same elements.
//
// Build a new regex on each call: the `g` flag's `lastIndex` carries over from the previous call, and sharing one instance would make
// the second text start matching from the middle.

import remarkGfm from "remark-gfm";
import remarkParse from "remark-parse";
import { unified } from "unified";

/** `[1]` / `[1][2]` / `[1, 2]` / `[1, 2]` brackets holding only digits and separators count as citations */
export const citeRe = () => /\[(\d+(?:\s*[,，]\s*\d+)*)\]/g;

/** The numbers in one marker: `[1, 2]` is two */
export function citeNumbers(spec: string): number[] {
  return spec
    .split(/[,，]/)
    .map((s) => Number(s.trim()))
    .filter((n) => Number.isInteger(n) && n > 0);
}

export type CitePiece = { text: string } | { cite: number[] };

/** Split plain text into 'text' and 'a group of citation numbers'.
 *
 *  An unfinished `[1` during streaming does not match; it waits for the closing bracket, so a superscript is never drawn half-way and then reshaped. */
export function splitCitations(text: string): CitePiece[] {
  const out: CitePiece[] = [];
  const re = citeRe();
  let last = 0;
  for (let m = re.exec(text); m; m = re.exec(text)) {
    const ns = citeNumbers(m[1]);
    if (!ns.length) continue;
    if (m.index > last) out.push({ text: text.slice(last, m.index) });
    out.push({ cite: ns });
    last = m.index + m[0].length;
  }
  if (!out.length) return [{ text }];
  if (last < text.length) out.push({ text: text.slice(last) });
  return out;
}

/* ── ── Footnotes: the numbers drawn as superscripts in the body ──────────────────────────────────────── */

/** mdast fields we read (same reason as the hast types below: structural types, no extra dependency) */
type MdNode = { type: string; value?: string; children?: MdNode[] };

/** `OPAQUE` (a / code / pre) in mdast, plus raw HTML:
 *  react-markdown passes it to rehype as a raw node, and `rehypeCitations` does not draw superscripts inside it */
const MD_OPAQUE = new Set(["link", "linkReference", "inlineCode", "code", "html"]);

/** Same remark parsing as rendering (remark-gfm handles tables and autolinks) */
const markdown = unified().use(remarkParse).use(remarkGfm);

/** Memoized by body text: during a turn every token would recompute it, but a finished answer's text no longer changes */
const citedMemo = new Map<string, ReadonlySet<number>>();
const CITED_MEMO_LIMIT = 200;

/** The numbers in the body that get drawn as superscripts. */
export function citedNumbers(text: string): ReadonlySet<number> {
  const hit = citedMemo.get(text);
  if (hit) return hit;
  const cited = new Set<number>();
  collectCited(markdown.parse(text) as MdNode, cited);
  if (citedMemo.size >= CITED_MEMO_LIMIT) {
    const oldest = citedMemo.keys().next().value;
    if (oldest !== undefined) citedMemo.delete(oldest);
  }
  citedMemo.set(text, cited);
  return cited;
}

function collectCited(node: MdNode, into: Set<number>): void {
  if (node.type === "text" && typeof node.value === "string") {
    for (const piece of splitCitations(node.value)) {
      if ("cite" in piece) for (const n of piece.cite) into.add(n);
    }
    return;
  }
  if (MD_OPAQUE.has(node.type)) return;
  for (const child of node.children ?? []) collectCited(child, into);
}

/* ── rehype plugin ────────────────────────────────────────────────────────── */

/** The two node kinds we act on in hast. Structural types instead of `@types/hast`:
 *  we only read `children` / `value` / `tagName`, not worth an extra dependency */
type HastText = { type: "text"; value: string };
type HastNode = {
  type: string;
  tagName?: string;
  value?: string;
  properties?: Record<string, unknown>;
  children?: HastNode[];
};

/** Brackets inside these elements are not citations: a link's anchor text may happen to be a number,
 *  a `[0]` in a code block is a subscript */
const OPAQUE = new Set(["a", "code", "pre"]);

const CITE_PREFIX = "#cite-";

/** A `[n]` in the body becomes `<a href="#cite-n">`.
 *
 *  **Why `a` and not a custom tag**: react-markdown turns hast properties into JSX
 *  properties, and `href` is the one it already recognizes, so `components.a` gets typed
 *  props; a custom tag would have to dig through `node.properties`, which yields `unknown`.
 *
 *  **Why an anchor and not a made-up `cite:` protocol**: react-markdown only allows
 *  http/https/mailto/tel and relative addresses by default; other protocols are blanked to an empty string, so the superscript degrades into a
 *  underlined link that cannot be clicked. A leading `#` is a relative address. */
export function rehypeCitations() {
  return (tree: HastNode) => walk(tree);
}

function walk(node: HastNode): void {
  const kids = node.children;
  if (!kids?.length) return;
  let changed = false;
  const out: HastNode[] = [];
  for (const child of kids) {
    if (child.type === "text" && typeof child.value === "string") {
      const pieces = splitCitations(child.value);
      if (pieces.length === 1 && "text" in pieces[0]) {
        out.push(child);
        continue;
      }
      changed = true;
      for (const p of pieces) {
        if ("text" in p) {
          if (p.text) out.push({ type: "text", value: p.text } as HastText);
        } else {
          out.push({
            type: "element",
            tagName: "a",
            properties: { href: `${CITE_PREFIX}${p.cite.join(",")}` },
            children: [{ type: "text", value: `[${p.cite.join(", ")}]` }],
          });
        }
      }
      continue;
    }
    if (!(child.type === "element" && OPAQUE.has(child.tagName ?? ""))) {
      walk(child);
    }
    out.push(child);
  }
  if (changed) node.children = out;
}

/** `#cite-1,2` → `[1, 2]`; return null if it is not a citation link */
export function citeHref(href: string | undefined): number[] | null {
  if (!href?.startsWith(CITE_PREFIX)) return null;
  const ns = citeNumbers(href.slice(CITE_PREFIX.length));
  return ns.length ? ns : null;
}
