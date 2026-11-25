/* Citations in chat: the superscripts in the body, the footnote line, and the wiring for the preview popup.
   The look lives in ui/citation.tsx; this file only handles 'which number is which source' and 'what is the route to the original text'. */
import { createContext, useContext } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { ArrowUpRight, BookOpen } from "lucide-react";
import type { Components } from "react-markdown";
import { api, type Source } from "../api";
import { citeHref } from "../citations";
import { S } from "../i18n";
import { useKbId } from "../kb";
import { originLabel } from "../origin";
import { quoteParam, sourceLead } from "../quotes";
import { CiteChip, CiteMark, CiteRow, PreviewCard, QuotedText } from "../ui/citation";
import { CodeBlock } from "./chatCopy";

/** The sources for this turn's answer.
 *
 *  **Uses context rather than a prop**: `Segment` is memoized by text (during a long answer, every token re-renders it,
 *  and a finished paragraph should not be parsed again), while sources arrive one at a time. Passing it down as a prop would
 *  invalidate all paragraphs each time a source arrives; with context, only the superscripts redraw themselves. */
const SourcesCtx = createContext<Source[]>([]);
export const SourcesProvider = SourcesCtx.Provider;

/** The `components` for react-markdown. **Must be a module-level constant**: building one on each render makes
 *  a new object, react-markdown then thinks the config changed, and `Segment`'s memoization becomes pointless */
export const chatMarkdown: Components = {
  // `node` is the hast node react-markdown passes down, not a DOM attribute: if it lands on an <a>,
  // React complains once in the console
  a: ({ node: _node, href, children, ...rest }) => {
    /* remend completes a `[2` still open mid-stream into a link (`streamdown:incomplete-link`),
       so a citation being typed flashes first as a blue "2" before turning into a superscript. It is not a link to begin with,
       so draw it as plain text as-is */
    if (href?.startsWith("streamdown:")) return <>{children}</>;
    const ns = citeHref(href);
    if (!ns) {
      return (
        <a href={href} {...rest}>
          {children}
        </a>
      );
    }
    // `[1, 2]` is two sources: drawn as two superscripts, each clickable; do not let one number open two passages of original text
    return (
      <>
        {ns.map((n) => (
          <InlineCite key={n} n={n} />
        ))}
      </>
    );
  },
  // Code blocks get a copy button (#936): what people most often copy out of an answer is SQL
  pre: ({ node: _node, ...rest }) => <CodeBlock {...rest} />,
};

function InlineCite({ n }: { n: number }) {
  const sources = useContext(SourcesCtx);
  const s = sources.find((x) => x.n === n);
  // Mid-stream, sources arrive later than the body: draw them static first; when one arrives, it turns clickable on its own
  if (!s) return <CiteMark n={n} />;
  return <CiteChip n={n} preview={<SourcePreview s={s} />} />;
}

/** Footnotes: list only the entries the body cites (see citedSources), one panel holds one group */
export function SourceList({ sources }: { sources: Source[] }) {
  return (
    <div className="mt-2 glass rounded-panel divide-y divide-line">
      {sources.map((s) => (
        <CiteRow key={s.n} preview={<SourcePreview s={s} />}>
          {s.kind === "charter" ? (
            <span className="flex items-center gap-2">
              <span className="u-num text-accent">[{s.n}]</span>
              <BookOpen size={11} className="shrink-0 text-ink-2" />
              <span className="truncate">{charterTitle(s)}</span>
            </span>
          ) : (
            <span className="block truncate">
              <span className="u-num text-accent">[{s.n}]</span> {s.filename} ·{" "}
              {sourceLead(s).slice(0, 60)}…
            </span>
          )}
        </CiteRow>
      ))}
    </div>
  );
}

/** The heading of the intro section is the article's name, to avoid "X › X" */
function charterTitle(s: Source): string {
  return s.heading && s.heading !== s.filename
    ? `${s.filename} › ${s.heading}`
    : s.filename;
}

/** Preview: first show the excerpt returned by the search (160 characters, so there is something to see at once),
 *  and the full text is swapped in once the document details arrive: the same react-query key,
 *  so the page you reach after clicking 'Open original' is ready-made */
function SourcePreview({ s }: { s: Source }) {
  const kbId = useKbId();
  const doc = useQuery({
    queryKey: ["docDetail", s.document_id],
    queryFn: () => api.documentDetail(s.document_id!),
    enabled: !!s.document_id,
  });
  const chunk = doc.data?.chunks.find((c) => c.id === s.chunk_id);

  if (s.kind === "charter") {
    return (
      <PreviewCard
        title={charterTitle(s)}
        body={s.excerpt}
        action={
          <Link
            to="/docs/$slug"
            params={{ slug: s.slug! }}
            hash={s.anchor || undefined}
            className="u-card-link flex items-center gap-1 text-fine text-ink-2"
          >
            {S.ask.openOriginal}
            <ArrowUpRight size={11} />
          </Link>
        }
      />
    );
  }

  return (
    <PreviewCard
      title={s.filename}
      meta={originLabel(chunk?.origin, chunk?.anchor) ?? undefined}
      body={<QuotedText text={chunk?.text ?? s.excerpt} quotes={s.quotes} />}
      action={
        <Link
          to="/kb/$kbId/doc/$docId"
          params={{ kbId, docId: s.document_id! }}
          search={{ chunk: s.chunk_id, quote: quoteParam(s.quotes?.[0]) }}
          className="u-card-link flex items-center gap-1 text-fine text-ink-2"
        >
          {S.ask.openOriginal}
          <ArrowUpRight size={11} />
        </Link>
      }
    />
  );
}
