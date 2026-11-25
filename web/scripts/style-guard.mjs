// Style guard: the five rules in web/DESIGN.md become checks here.
//
// It scans src/**/*.tsx; any hit on the rules below fails. The pages listed in `style-guard.baseline.json`
// have not been migrated yet, so they are exempt for now. Each time a page is migrated, delete its line from the list; when the list is empty,
// this file goes away. **New files are never exempt**: the list can only shrink, never grow.
//
// `src/ui/` is the component library itself, so it may use native elements, hover and transitions; that is its job,
// but the color and font-size rules apply to it just the same.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const SRC = path.join(ROOT, "src");
const BASELINE = path.join(ROOT, "style-guard.baseline.json");

const PALETTE =
  "neutral|zinc|gray|slate|stone|rose|amber|red|green|blue|sky|emerald|violet|indigo|purple|pink|orange|yellow|cyan|teal|lime";

// The words allowed for `text-*` are read from styles.css: the five font-size tiers (--text-*) and the color tokens (--color-*).
// Add a token and this picks it up automatically; the guard does not need changing. What remains are the Tailwind
// text- utilities that are neither font sizes nor colors (alignment, wrapping, truncation) and a few color keywords
const STYLES = fs.readFileSync(path.join(SRC, "styles.css"), "utf8");
const TOKENS = [
  ...new Set([
    ...[...STYLES.matchAll(/--text-([a-z]+)(?![a-z-])/g)].map((m) => m[1]),
    ...[...STYLES.matchAll(/--color-([a-z0-9-]+)/g)].map((m) => m[1]),
  ]),
];
const TEXT_UTILITIES =
  "left|center|right|justify|start|end|ellipsis|clip|wrap|nowrap|balance|pretty|white|black|transparent|current|inherit";
/* `white` / `black` stay in this table so that `unknown-text` lets them through;
   they are handled by the `raw-white` rule below, and reporting both would state the same mistake twice */
const KNOWN_TEXT = [...TOKENS, ...TEXT_UTILITIES.split("|")]
  .sort((a, b) => b.length - a.length)
  .join("|");

/** Each rule: a regex, a one-line description, and whether it also covers the components directory */
const RULES = [
  {
    id: "type-scale",
    re: /\btext-(xs|sm|base|lg|\d*xl|\[[0-9.]+(px|rem)\])\b/g,
    why: "字号只用五档：text-fine / small / body / title / display（规矩 1）",
    ui: true,
  },
  {
    id: "unknown-text",
    // text-<word> that is not one of the five font sizes, a color token, or an alignment/wrapping utility is most likely made up
    // (text-caption, text-muted). It is undefined, so it renders as the inherited font size or color, and nothing shows that.
    // Tailwind's default font sizes are reported by the rule above; the model name text-embedding-* is not a class
    re: new RegExp(
      `\\btext-(?!(?:${KNOWN_TEXT}|xs|sm|base|lg|\\d*xl)(?![a-z0-9-])|embedding|\\[)[a-z][a-z0-9-]*\\b`,
      "g",
    ),
    why: "text-* 只能是五档字号、颜色令牌，或对齐/换行工具；别的没有定义（规矩 1、4）",
    ui: true,
  },
  {
    id: "raw-palette",
    re: new RegExp(
      `\\b(text|bg|border|ring|outline|from|to|via|decoration|divide|placeholder)-(${PALETTE})-[0-9]+\\b`,
      "g",
    ),
    why: "颜色只用令牌：ink / ink-2 / line / surface / ok / warn / danger…（规矩 4）",
    ui: true,
  },
  {
    id: "raw-white",
    /* White and black are not color tokens, **with or without opacity**. This rule used to catch only
       the slashed form such as `text-white/10`, so a bare `text-white` passed, and `white` was counted as a legal
       `unknown-text` utility (it sits alongside text-left and text-nowrap).
       Both gates were open, so the top bar's wordmark was hard-coded white: invisible on the dark background, and still white
       after switching to the paper theme, so invisible again. The name should be `text-ink`, which follows the theme.
       If you really want "always light text on a dark block" (text on primary and danger buttons), there is a separate token:
       `--u-on-accent` / `--u-on-danger`, which are also correct in the light set */
    re: /\b(bg|border|text|ring|outline|divide|fill|stroke|placeholder|decoration|from|via|to)-(white|black)\b(\/\[?[0-9.]+\]?)?/g,
    why: "白与黑不是令牌：文字用 ink / ink-2，面用 surface / surface-2 / surface-3，线用 line；深色块上的字用 on-accent / on-danger（规矩 4）",
    ui: true,
  },
  {
    id: "token-by-hand",
    re: /\[var\(--u-[a-z0-9-]+\)\]/g,
    why: "令牌已经是 Tailwind 颜色：写 text-danger，不写 text-[var(--u-danger)]（规矩 4）",
    ui: true,
  },
  {
    id: "radius",
    // rounded-none is not a tier; it means "this one goes to the edge" (full-width rows in a dropdown), so it is allowed
    re: /\brounded(-(sm|md|lg|xl|2xl|3xl|\[[^\]]+\]))?(?=[\s"'`}])/g,
    why:
      "圆角按角色分四档：rounded-cell / control / panel / overlay；" +
      "rounded-full 只给真圆的（规矩 3）",
    ui: true,
  },
  {
    id: "spacing",
    // 12 and above is layout (room for overlays, footer clearance), not rhythm, so it is allowed
    re: /\b-?(p|px|py|pt|pb|pl|pr|m|mx|my|mt|mb|ml|mr|gap|gap-x|gap-y|space-x|space-y)-(0\.5|1\.5|2\.5|3\.5|5|7|9|10|11|\[[^\]]+\])\b/g,
    why: "间距六档：1 2 3 4 6 8；12 以上只给版面净空（规矩 2）",
    ui: false,
  },
  {
    id: "raw-control",
    // A hidden file input is not a control (it has no look), so it is allowed
    re: /<(button|textarea|select)\b|<input\b(?![^>]*type="file")/g,
    why: "控件从 ui/ 来：Button / IconButton / Input / Textarea / Dropdown / SearchSelect（规矩 5）",
    ui: false,
  },
  {
    id: "state-in-page",
    re: /\b(hover|focus|focus-visible|active|disabled):[a-z0-9\[\]()/.-]+|\btransition(-[a-z]+)?\b|\bduration-[0-9a-z()-]+/g,
    why: "hover / focus / disabled / 动效在组件里定一次，页面不写（规矩 5）",
    ui: false,
  },
  {
    id: "raw-colour",
    // Color values themselves never appear in pages or components: tokens are defined once in styles.css, one set each for light and dark,
    // and a hard-coded #ffffff means hard-coding the "dark background". There are two exception files, and they are **readers of the tokens**:
    // graphVisuals.ts reads the colors the canvas needs out of the CSS variables (canvas does not accept var()),
    // and palette.ts holds the data colors for entity types (rule 4: color belongs only to data), with both dark and light sets in it.
    // `rgba(0,0,0,0)` is transparent, not a color, so it is allowed
    re: /#[0-9a-fA-F]{6}(?![0-9a-fA-F])|(?<![a-zA-Z_])rgba?\(/g,
    why: "颜色只用令牌（规矩 4）；画布从 graphVisuals 的调色板读，数据色在 palette.ts",
    ui: true,
    skip: (rel) => /src\/(pages\/graphVisuals\.ts|palette\.ts)$/.test(rel) || /\.test\.tsx?$/.test(rel),
    allow: /rgba\(0,\s*0,\s*0,\s*0\)/g,
  },
  {
    id: "raw-shadow",
    /* Tailwind's built-in shadows are black shadows tuned for light interfaces; they are not the same thing as `--u-shadow`,
       and on a dark background they are nearly invisible, which is why they could hide in pages unnoticed. There are only two elevation tiers:
       controls that sit against the canvas use `u-lift`, and floating layers over the page use `u-lift-strong`.
       `shadow-none` means "remove it", not a depth level, so it is allowed */
    re: /\bshadow-(sm|md|lg|xl|2xl)\b/g,
    why: "浮起两档：u-lift（贴着画布的控件）/ u-lift-strong（浮层）；深浅归 --u-shadow（规矩 4）",
    ui: true,
  },
  {
    id: "native-confirm",
    re: /\bwindow\.(confirm|alert)\(|(?<![.\w])(confirm|alert)\(/g,
    why: "确认走 DangerConfirm / Dialog，不用 window.confirm（规矩 5）",
    ui: false,
  },
];

/** The layer generated by the shadcn CLI is not checked: it is the **supply layer**, written in Tailwind's native scale
 *  (text-sm / rounded-md / bg-primary), not the same vocabulary as these rules. To change it, either edit the
 *  registry, or run `shadcn add` again to overwrite it; hand-written rules neither reach it nor should.
 *  The rules still govern pages: how pages use these components, and whether they assemble their own controls, is this file's business. */
const VENDOR = path.join("src", "components", "ui");

function walk(dir, out = []) {
  if (dir.includes(VENDOR)) return out;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(full, out);
    else if (entry.name.endsWith(".tsx") || entry.name.endsWith(".ts")) out.push(full);
  }
  return out;
}

const baseline = new Set(
  fs.existsSync(BASELINE) ? JSON.parse(fs.readFileSync(BASELINE, "utf8")) : [],
);
const files = walk(SRC).map((f) => path.relative(ROOT, f).replaceAll("\\", "/"));

let failures = 0;
const staleBaseline = [...baseline].filter((f) => !files.includes(f));
for (const rel of files) {
  if (baseline.has(rel)) continue;
  const inUi = rel.startsWith("src/ui/");
  let text = fs.readFileSync(path.join(ROOT, rel), "utf8");
  // The hidden file input can span several lines, so blank out the whole block first (keeping the newlines, so line numbers stay the same)
  // The timeline range is the same: its skin lives in .scrubber-range in styles.css
  text = text.replace(/<input\b[^>]*type="(file|range)"[^>]*>/g, (m) =>
    m.replace(/[^\n]/g, " "),
  );
  const lines = text.split("\n");
  // Lines inside block comments don't count either: /* … */ can span lines and a line can't tell that it is inside a comment, so record a state here
  let inBlock = false;
  const commentFree = lines.map((line) => {
    let out = "";
    let i = 0;
    while (i < line.length) {
      if (inBlock) {
        const end = line.indexOf("*/", i);
        if (end < 0) return out;
        inBlock = false;
        i = end + 2;
      } else {
        const start = line.indexOf("/*", i);
        if (start < 0) {
          out += line.slice(i);
          break;
        }
        out += line.slice(i, start);
        inBlock = true;
        i = start + 2;
      }
    }
    return out;
  });
  for (const rule of RULES) {
    if (inUi && !rule.ui) continue;
    if (rule.skip && rule.skip(rel)) continue;
    commentFree.forEach((line, i) => {
      // Mentioning the old form inside a comment doesn't count (the rule has to be citable inside comments)
      let code = line.replace(/\/\/.*$/, "");
      if (rule.allow) code = code.replace(rule.allow, "");
      const hits = code.match(rule.re);
      if (!hits) return;
      failures += 1;
      console.log(`${rel}:${i + 1}  [${rule.id}] ${hits.join(" ")}\n    ${rule.why}`);
    });
  }
}

if (staleBaseline.length) {
  console.log(`baseline 里有已经不存在的文件，删掉它们：\n  ${staleBaseline.join("\n  ")}`);
  failures += 1;
}

if (failures) {
  console.log(`\n${failures} 处不合规矩。规矩在 web/DESIGN.md。`);
  process.exit(1);
}
console.log(`style guard: ${files.length - baseline.size} 个文件合规，${baseline.size} 个在迁移名单上。`);
