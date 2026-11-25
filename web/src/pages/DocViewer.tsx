import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { api, type ChunkFact, type Doc } from "../api";
import { S } from "../i18n";
import { useKbId } from "../kb";
import { originHint, originLabel } from "../origin";
import { fmtTime } from "../time";
import { GroupLabel, LinkButton, PageHeader, Pager, pageSlice } from "../ui";
import { AddOntologyRegressionDialog } from "./AddOntologyRegressionDialog";
import { QuotedText } from "../ui/citation";
import { SourcesRail } from "./SourcesRail";

const DOC_PAGE = 12;

const ym = (iso: string | null) => (iso ? iso.slice(0, 7) : null);

function factRange(f: ChunkFact): string | null {
  if (!f.valid_from && !f.valid_to) return null;
  return `${ym(f.valid_from) ?? "…"} → ${ym(f.valid_to) ?? S.doc.ongoing}`;
}

export function DocViewer() {
  const kbId = useKbId();
  const [caseStatement, setCaseStatement] = useState<ChunkFact | null>(null);
  const { docId } = useParams({ from: "/app/kb/$kbId/doc/$docId" });
  const { chunk, quote } = useSearch({ from: "/app/kb/$kbId/doc/$docId" });
  const navigate = useNavigate();

  const detail = useQuery({
    queryKey: ["docDetail", docId],
    queryFn: () => api.documentDetail(docId),
  });
  // The role is only returned by the detail endpoint; reuse the KbScope key, and write permission cannot be judged from the workspace KB list.
  const roleKbId = detail.data?.document.kb_id;
  const role = useQuery({
    queryKey: ["kbOne", roleKbId],
    queryFn: () => api.kbDetail(roleKbId!),
    enabled: !!roleKbId,
  });
  // Reverse evidence chain: the facts extracted from each chunk (the whole document fetched at once, grouped by chunk)
  const extractions = useQuery({
    queryKey: ["docExtractions", docId],
    queryFn: () => api.documentExtractions(docId),
  });
  const factsByChunk = useMemo(() => {
    const map = new Map<string, ChunkFact[]>();
    for (const f of extractions.data?.facts ?? []) {
      if (!map.has(f.chunk_id)) map.set(f.chunk_id, []);
      map.get(f.chunk_id)!.push(f);
    }
    return map;
  }, [extractions.data]);

  // null = auto-locate: when a citation jumps, flip to the page containing the target chunk
  const [page, setPage] = useState<number | null>(null);
  useEffect(() => setPage(null), [chunk, docId]);
  useEffect(() => setCaseStatement(null), [docId]);

  const highlightRef = useRef<HTMLDivElement>(null);
  // Citation jump: scroll to the target chunk and light it up briefly; it fades out after a moment and returns to normal (no persistent highlight)
  const [flash, setFlash] = useState(false);
  useEffect(() => {
    if (detail.data && highlightRef.current) {
      highlightRef.current.scrollIntoView({ behavior: "smooth", block: "center" });
      setFlash(true);
      // The smooth scroll itself takes a few hundred milliseconds, so the lit window must leave enough visible time after the scroll
      const t = setTimeout(() => setFlash(false), 2600);
      return () => clearTimeout(t);
    }
  }, [detail.data, chunk]);

  if (detail.isPending)
    return <div className="p-8 text-body text-ink-2">{S.doc.loading}</div>;
  if (detail.isError)
    return (
      <div className="p-8 text-body text-danger">
        {(detail.error as Error).message}
      </div>
    );

  const { document: doc, chunks } = detail.data;
  const targetIdx = chunk ? chunks.findIndex((c) => c.id === chunk) : -1;
  const curPage = page ?? (targetIdx >= 0 ? Math.floor(targetIdx / DOC_PAGE) : 0);
  const { rows: pagedChunks, safe: safePage } = pageSlice(chunks, curPage, DOC_PAGE);

  return (
    <div className="h-full flex">
      {/* The source bar is always present: the folder the current document belongs to is highlighted; clicking another folder jumps back to the matching Library view */}
      <SourcesRail
        kbId={doc.kb_id}
        active={doc.source_id ?? "uploads"}
        onSelect={(sel) => navigate({ to: "/kb/$kbId/library", params: { kbId }, search: { src: sel } })}
      />
      <div className="flex-1 min-w-0 overflow-y-auto u-scroll">
        {/* Left-aligned, with the same inset as the library list (px-8 py-6), spanning the full width to the right of the bar:
            the chunk column and the extraction column on the right are laid out together */}
        <div className="px-8 py-6">
        <PageHeader
          title={doc.filename}
          sub={<DocMeta doc={doc} chunks={chunks.length} />}
          actions={
            <Link to="/kb/$kbId/library" params={{ kbId }} className="u-link shrink-0 text-body">
              {S.doc.backToLibrary}
            </Link>
          }
        />

        <div className="space-y-3">
          {pagedChunks.map((c) => {
            const hit = c.id === chunk;
            const facts = factsByChunk.get(c.id) ?? [];
            return (
              <div key={c.id} ref={hit ? highlightRef : undefined} className="flex gap-3">
                <div
                  className={`u-chunk flex-1 min-w-0 rounded-panel border p-4 text-body leading-relaxed whitespace-pre-wrap border-line ${
                    hit && flash ? "u-flash" : "bg-surface"
                  }`}
                >
                  <div className="mb-2 text-small text-ink-2">
                    {S.doc.section} {c.seq + 1}
                    {/* A source note for a wordmark that was recognised and transcribed: it may have misread a number or misheard a name */}
                    {originLabel(c.origin, c.anchor) && (
                      <span title={originHint(c.origin, c.origin_model)}>
                        {" · "}
                        {originLabel(c.origin, c.anchor)}
                      </span>
                    )}
                    {hit && (
                      <span
                        className={`u-fade-slow ml-2 text-accent ${
                          flash ? "opacity-100" : "opacity-0"
                        }`}
                      >
                        {S.doc.citedHere}
                      </span>
                    )}
                  </div>
                  {/* When jumping from the number of a graph fact, mark the sentence that stated it (a follow-up to #968) */}
                  {hit && quote ? <QuotedText text={c.text} quotes={[quote]} /> : c.text}
                </div>

                {/* Extraction comparison bar: which facts this chunk produced (entities can jump to the graph).
                    **Width follows the screen**: each one is a triple, and in a narrow column one would wrap onto three lines,
                    and readers go back and forth between the source text and it, so the more it wraps, the harder it is to match up. At 384, most
                    triples take one or two lines */}
                {facts.length > 0 && (
                  <aside className="w-64 shrink-0 rounded-panel border border-line bg-surface p-3 xl:w-80 2xl:w-96">
                    <GroupLabel className="mb-2" count={facts.length}>
                      {S.doc.extracted}
                    </GroupLabel>
                    {/* **Draw a line between rows**: one triple wraps to three lines and the next to two; an 8px gap alone cannot separate them, and the column reads as a blur of text.
                        A rule saves more room than a bigger gap */}
                    <div className="divide-y divide-line">
                      {facts.map((f) => {
                        const range = factRange(f);
                        return (
                          <div
                            key={f.fact_id}
                            className="py-2 text-small leading-snug first:pt-0 last:pb-0"
                          >
                            <div>
                              <Link
                                to="/kb/$kbId/graph"
                                params={{ kbId }}
                                search={{ entity: f.subject_id }}
                                className="u-inline-link text-ink"
                              >
                                {f.subject}
                              </Link>
                              <span
                                className={
                                  f.predicate === null
                                    ? "italic text-ink-2"
                                    : "text-ink-2"
                                }
                                title={
                                  f.predicate && f.inferred
                                    ? S.graph.inferredPredicate
                                    : undefined
                                }
                              >
                                {" "}
                                {f.predicate ?? S.graph.unknownPredicate}{" "}
                              </span>
                              {f.object_id ? (
                                <Link
                                  to="/kb/$kbId/graph"
                                  params={{ kbId }}
                                  search={{ entity: f.object_id }}
                                  className="u-inline-link text-ink"
                                >
                                  {f.object}
                                </Link>
                              ) : (
                                /* A literal's object is **as deep** as an entity's object: one tier lighter and it would
                                   share the colour of the phrase in the middle, and the triple could not be read as where the relation ends
                                   and where the value begins */
                                <span className="text-ink">{f.object ?? ""}</span>
                              )}
                            </div>
                            {range && <div className="u-num text-fine text-ink-2">{range}</div>}
                            {f.is_open_statement &&
                              ["editor", "admin", "owner"].includes(role.data?.my_role ?? "") && (
                              <LinkButton className="mt-1 text-fine" onClick={() => setCaseStatement(f)}>
                                {S.ontology.caseMake}
                              </LinkButton>
                            )}
                          </div>
                        );
                      })}
                    </div>
                  </aside>
                )}
              </div>
            );
          })}
        </div>
          <Pager total={chunks.length} pageSize={DOC_PAGE} page={safePage} onPage={setPage} />
        </div>
      </div>
      {caseStatement && (
        <AddOntologyRegressionDialog
          key={caseStatement.fact_id}
          kbId={doc.kb_id}
          statementId={caseStatement.fact_id}
          statement={`${caseStatement.subject} ${caseStatement.predicate ?? ""} ${caseStatement.object ?? ""}`}
          isValue={!caseStatement.object_id}
          onClose={() => setCaseStatement(null)}
        />
      )}
    </div>
  );
}

/** The metadata bar at the top of the document: segment count · size · document date · upload time.
 *
 *  The document date (`doc_time`) and the upload time (`created_at`) are shown separately (#610): merging them into one cell would make readers take
 *  the upload time for a date written on the document. They are also read differently: the document date is a **calendar date**, stored as that day's
 *  UTC midnight, and is read in UTC (`fmtTime`), since reading it in local time would show someone at UTC-5 the previous day; the upload time is a
 *  real instant, read in the time zone of the viewer. */
function DocMeta({ doc, chunks }: { doc: Doc; chunks: number }) {
  const docDate = fmtTime(doc.doc_time, "day");
  return (
    <>
      {chunks} {S.doc.sections} · {(doc.size_bytes / 1024).toFixed(0)} KB ·{" "}
      {docDate && (
        <>
          <span title={S.doc.docDateFromContent}>
            {S.doc.docDate}: {docDate}
          </span>
          {" · "}
        </>
      )}
      {S.doc.uploadedAt} {new Date(doc.created_at).toLocaleDateString()}
    </>
  );
}
