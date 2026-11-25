/* Built-in docs: bundled with the app (so private deployments work offline), with a version that matches the deployment.
   Public route: does not depend on a session, and is readable before login (developers wiring up an API may not have an account). */
import { useEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Link, useNavigate, useParams } from "@tanstack/react-router";
import { Command, Search } from "lucide-react";
import { api } from "../api";
import { S } from "../i18n";
import {
  Button,
  cn,
  Input,
  Row,
  rowClass,
  SectionMark,
  buttonLike,} from "../ui";
import { HeaderActions } from "./HeaderActions";
import { usePageTitle } from "../useTitle";
import ingestMd from "../docs/ingest.md?raw";
import mcpMd from "../docs/mcp.md?raw";
import { fetchMe } from "../authSession";

/** Doc list: slug → title + content (bundled into the bundle at build time) */
const DOCS: { slug: string; title: string; body: string }[] = [
  { slug: "ingest", title: "Ingest interfaces", body: ingestMd },
  { slug: "mcp", title: "Agents over MCP", body: mcpMd },
];

/** Full-text search (client-side; the docs are already in the bundle): a hit row → the doc + an excerpt centred on the matched word */
function searchDocs(q: string): { slug: string; title: string; snippet: string }[] {
  const needle = q.trim().toLowerCase();
  if (needle.length < 2) return [];
  const hits: { slug: string; title: string; snippet: string }[] = [];
  for (const d of DOCS) {
    for (const line of d.body.split("\n")) {
      const cleaned = line.replace(/[#`*|>-]/g, " ").replace(/\s+/g, " ").trim();
      const idx = cleaned.toLowerCase().indexOf(needle);
      if (idx < 0) continue;
      // 摘录窗口以命中词为中心，避免命中被 90 字截断截丢
      const start = Math.max(0, idx - 30);
      const snippet = (start > 0 ? "…" : "") + cleaned.slice(start, start + 100);
      hits.push({ slug: d.slug, title: d.title, snippet });
      if (hits.length >= 8) return hits;
    }
  }
  return hits;
}

/** 摘录内高亮命中词（不区分大小写，全部命中都标） */
function Highlighted({ text, q }: { text: string; q: string }) {
  const needle = q.trim();
  if (!needle) return <>{text}</>;
  const parts = text.split(new RegExp(`(${needle.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")})`, "ig"));
  return (
    <>
      {parts.map((p, i) =>
        p.toLowerCase() === needle.toLowerCase() ? (
          /* 命中词用警示琥珀：结果列表里一眼定位到匹配处 */
          <mark
            key={i}
            className="u-mark"
          >
            {p}
          </mark>
        ) : (
          <span key={i}>{p}</span>
        ),
      )}
    </>
  );
}

/** 标题 → 锚点 id（TOC 与正文用同一函数保证对得上） */
function slugify(s: string): string {
  return s
    .toLowerCase()
    .replace(/[^a-z0-9一-龥]+/g, "-")
    .replace(/(^-+|-+$)/g, "");
}

/** React children → 纯文本（h2/h3 里可能嵌 code/strong） */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
function textOf(children: any): string {
  if (typeof children === "string") return children;
  if (Array.isArray(children)) return children.map(textOf).join("");
  if (children && typeof children === "object" && "props" in children)
    return textOf(children.props.children);
  return "";
}

/** 从 markdown 源提取 h2/h3 目录 */
function tocOf(body: string): { id: string; text: string; level: 2 | 3 }[] {
  const out: { id: string; text: string; level: 2 | 3 }[] = [];
  for (const line of body.split("\n")) {
    const m = /^(#{2,3})\s+(.+)$/.exec(line);
    if (!m) continue;
    const text = m[2].replace(/[`*]/g, "").trim();
    out.push({ id: slugify(text), text, level: m[1].length as 2 | 3 });
  }
  return out;
}

const IS_MAC = navigator.platform.toUpperCase().includes("MAC");

export function DocsPage() {
  const { slug } = useParams({ from: "/docs/$slug" });
  const doc = DOCS.find((d) => d.slug === slug) ?? DOCS[0];
  const navigate = useNavigate();
  const [q, setQ] = useState("");
  const searchRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const results = searchDocs(q);
  const toc = tocOf(doc.body);
  const mainRef = useRef<HTMLElement>(null);
  const [activeHeading, setActiveHeading] = useState<string | null>(null);
  // Title: `Utopia | {article name}`. Charter is the storefront wordmark, and the title just gives the article
  usePageTitle(S.app.name, doc.title);
  // Public pages are also aware of login state: logged in gets the user menu, logged out gets Sign in
  const me = useQuery({ queryKey: ["me"], queryFn: fetchMe, retry: false });
  const health = useQuery({ queryKey: ["health"], queryFn: api.health });

  // Scroll-follow: the nearest heading above the top edge of the viewport is the current section;
  // scrolling to the bottom forces the last section active (a short final section can never get past the decision line)
  const onScrollSpy = () => {
    const main = mainRef.current;
    if (!main) return;
    const top = main.getBoundingClientRect().top;
    let current: string | null = null;
    for (const h of toc) {
      const el = document.getElementById(h.id);
      if (el && el.getBoundingClientRect().top - top <= 96) current = h.id;
    }
    if (main.scrollTop + main.clientHeight >= main.scrollHeight - 8 && toc.length)
      current = toc[toc.length - 1].id;
    setActiveHeading(current);
  };

  useEffect(() => {
    if (!q) return;
    const onDown = (e: MouseEvent) => {
      if (searchRef.current && !searchRef.current.contains(e.target as Node)) setQ("");
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setQ("");
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [q]);

  // Keyboard summon: ⌘K / Ctrl+K works at any time; `/` only when focus is not in an input (do not hijack typing)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const el = document.activeElement;
      const typing =
        el instanceof HTMLInputElement ||
        el instanceof HTMLTextAreaElement ||
        (el instanceof HTMLElement && el.isContentEditable);
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        inputRef.current?.focus();
        inputRef.current?.select();
      } else if (e.key === "/" && !typing) {
        e.preventDefault();
        inputRef.current?.focus();
      } else if (e.key === "Escape" && el === inputRef.current) {
        inputRef.current?.blur();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div className="h-screen flex flex-col overflow-hidden">
      <header className="glass-strong relative z-40 border-x-0 border-t-0 h-12 shrink-0 flex items-center px-2">
        <SectionMark className="pl-3" text={S.docs.brand} title={S.docs.backTitle} />
        {/* Centred search over all the docs bundled locally; width aligned to the body column (max-w-3xl without px-8) */}
        <div
          ref={searchRef}
          className="absolute left-1/2 -translate-x-1/2 w-[min(44rem,55vw)]"
        >
          <div className="relative">
            {/* Medium: same height as the search boxes elsewhere (32); small would look shorter than the ones in the top bar */}
            <Input
              className="w-full"
              icon={<Search size={13} />}
              ref={inputRef}
              placeholder={S.docs.searchPlaceholder}
              value={q}
              onChange={(e) => setQ(e.target.value)}
            />
            {/* Shortcut hint: stays out of the way while typing. ⌘ uses a lucide icon;
                Ctrl has no graphic symbol (⌘ is a Mac-only key glyph), so the Windows convention is plain text */}
            {!q && (
              <kbd className="pointer-events-none absolute right-2.5 top-1/2 -translate-y-1/2 flex items-center gap-1 rounded-cell border border-line bg-surface px-2 py-1 font-sans text-fine text-ink-2">
                {IS_MAC ? <Command size={9} /> : <span>Ctrl</span>}
                <span>K</span>
              </kbd>
            )}
          </div>
          {q.trim().length >= 2 && (
            <div className="u-menu-glass u-pop-in u-pop-in-tl absolute inset-x-0 top-full mt-2 rounded-overlay u-lift-strong overflow-hidden">
              {results.length === 0 ? (
                <p className="px-4 py-3 text-small text-ink-2">{S.docs.noResults}</p>
              ) : (
                results.map((r, i) => (
                  <Row
                    key={i}
                    density="menu"
                    className="border-b border-line px-4 py-3 last:border-0"
                    onClick={() => {
                      setQ("");
                      navigate({ to: "/docs/$slug", params: { slug: r.slug } });
                    }}
                  >
                    <div className="text-fine text-ink-2">{r.title}</div>
                    <div className="text-body text-ink truncate">
                      <Highlighted text={r.snippet} q={q} />
                    </div>
                  </Row>
                ))
              )}
            </div>
          )}
        </div>

        {/* The right side is the same as on the App and account pages, and should not change on page switch. When logged out, only a single "Log in" remains */}
        <HeaderActions
          link={{ to: "/", label: S.account.backToApp }}
          version={health.data?.version}
          user={me.data}
          signedOut={
            me.isError ? (
              <Link to="/login" className={buttonLike("ghost")}>
                {S.login.signIn}
              </Link>
            ) : null
          }
        />
      </header>

      {/* Whole-page scroll (the scrollbar hugs the window's right edge); the sidebar is pinned sticky: the standard docs-site layout */}
      <main
        ref={mainRef}
        onScroll={onScrollSpy}
        className="flex-1 min-h-0 overflow-y-auto u-scroll"
      >
        <div className="flex items-start">
          <aside className="w-64 shrink-0 sticky top-0 h-[calc(100vh-3.5rem)] glass-strong border-y-0 border-l-0 px-2 py-3 space-y-1">
            {DOCS.map((d) => (
              <Link
                key={d.slug}
                to="/docs/$slug"
                params={{ slug: d.slug }}
                className={rowClass(d.slug === doc.slug, "nav")}
              >
                {d.title}
              </Link>
            ))}
          </aside>

          {/* Generous space above and below: the heading does not sit against the top, and the last section can scroll to mid-screen, since people read the middle of the screen.
              Typography is left to the official @tailwindcss/typography (prose, 16px base);
              the only custom parts are the heading anchor ids, external links opening in a new tab, and the scroll container for wide tables */}
          {/* Typography goes through the official shadcn/typeset (src/typeset.css): inside the container,
              headings, lists, tables and code are all handled by it, and the content itself carries no class;
              font size and face are tuned in the `.typeset-docs` preset, and colours follow our tokens */}
          <article className="typeset typeset-docs [&_h1]:scroll-mt-6 [&_h2]:scroll-mt-6 [&_h3]:scroll-mt-6 flex-1 min-w-0 max-w-3xl mx-auto px-8 pt-16 pb-[40vh]">
            <ReactMarkdown
              remarkPlugins={[remarkGfm]}
              components={{
                h2: ({ children, ...p }) => (
                  <h2 id={slugify(textOf(children))} {...p}>
                    {children}
                  </h2>
                ),
                h3: ({ children, ...p }) => (
                  <h3 id={slugify(textOf(children))} {...p}>
                    {children}
                  </h3>
                ),
                a: (p) => <a target="_blank" rel="noreferrer" {...p} />,
                // Do not add u-scroll: overscroll-contain would trap the vertical wheel inside the block and the page would not scroll
                table: (p) => (
                  <div className="overflow-x-auto">
                    <table {...p} />
                  </div>
                ),
              }}
            >
              {doc.body}
            </ReactMarkdown>
          </article>

          {/* Right-hand table of contents: sticky, starting on the same horizontal line as the body headings */}
          {toc.length > 0 && (
            <aside className="hidden lg:block w-64 shrink-0 sticky top-0 pt-16 pr-8">
              <nav className="space-y-1">
                {toc.map((h) => (
                  <Button variant="ghost" size="sm"
                    key={h.id}
                    onClick={() =>
                      document.getElementById(h.id)?.scrollIntoView({ behavior: "smooth" })
                    }
                    className={cn(
                      "h-auto w-full justify-start whitespace-normal py-2 text-left leading-snug",
                      h.level === 3 && "pl-6",
                      activeHeading === h.id ? "text-ink" : "text-ink-2",
                    )}
                  >
                    {h.text}
                  </Button>
                ))}
              </nav>
            </aside>
          )}
        </div>
      </main>
    </div>
  );
}
