// The sentences quoted from the source text (a follow-up to #968).
//
// The number printed on a fact by the graph tool opens the chunk of its first valid evidence; the `quotes` on a source entry are the sentences in the chunk
// that state this fact (or these facts). The preview popover and the document page use it to mark them; the number opens the exact sentence.
//
// Each sentence only matches its first occurrence; ones not found (the quote does not match the source) are not marked; overlapping or adjacent ones are merged into one segment.
// Whitespace matches "one or more whitespace characters": a single space in the quote matches a line break in the source.

export type QuotePiece = { text: string; quoted: boolean };

const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/** The position of a quoted sentence's first occurrence in the source, `[start, end)`; null if it does not match */
function locate(text: string, quote: string): [number, number] | null {
  const words = quote.trim().split(/\s+/).filter(Boolean);
  if (words.length === 0) return null;
  const m = new RegExp(words.map(escape).join("\\s+")).exec(text);
  return m ? [m.index, m.index + m[0].length] : null;
}

/** Split the source into "plain" and "quoted" segments, in their original order */
export function quotePieces(text: string, quotes: readonly string[] | undefined): QuotePiece[] {
  const spans = (quotes ?? [])
    .map((q) => locate(text, q))
    .filter((s): s is [number, number] => s !== null)
    .sort((a, b) => a[0] - b[0]);
  const merged: [number, number][] = [];
  for (const [start, end] of spans) {
    const last = merged[merged.length - 1];
    if (last && start <= last[1]) last[1] = Math.max(last[1], end);
    else merged.push([start, end]);
  }
  const out: QuotePiece[] = [];
  let at = 0;
  for (const [start, end] of merged) {
    if (start > at) out.push({ text: text.slice(at, start), quoted: false });
    out.push({ text: text.slice(start, end), quoted: true });
    at = end;
  }
  if (at < text.length) out.push({ text: text.slice(at), quoted: false });
  return out;
}

/** Summary of the item on the footer line: if there is a quoted sentence, it is the first sentence; otherwise the start of this chunk */
export function sourceLead(source: { excerpt: string; quotes?: string[] }): string {
  return source.quotes?.[0] ?? source.excerpt;
}

/** The longest number of characters for a sentence that goes into the address bar. The address stays in the browser history and in the proxy's access log, and on refresh the whole thing is sent again:
 *  one Chinese character is about nine bytes once encoded, so a few hundred characters reach the common 8k request header limit. An over-long sentence is not carried at all:
 *  truncated, it cannot be found in the source and so cannot be marked, which is no better than not marking it */
export const QUOTE_PARAM_MAX = 300;

/** A sentence that can go into the address bar: one that is blank or over-long counts as not given */
export function quoteParam(quote: unknown): string | undefined {
  return typeof quote === "string" && quote.trim() && quote.length <= QUOTE_PARAM_MAX
    ? quote
    : undefined;
}

/** Query parameters for the document page: which chunk a citation jumps to, and which sentence to mark within the chunk */
export function docSearch(search: Record<string, unknown>): { chunk?: string; quote?: string } {
  return {
    chunk: typeof search.chunk === "string" ? search.chunk : undefined,
    quote: quoteParam(search.quote),
  };
}
