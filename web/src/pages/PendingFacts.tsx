import { Button, CARD_ACTIONS, Status } from "../ui";
/* Facts awaiting a person's nod (docs/decisions/0015).
   A triple extracted by a remember step first goes into the pending-confirmation queue and does not reach the graph; it is only booked into the ledger once a person nods here.
   **Source sentence on top, triple below**: listing only the triple would force the person to judge its correctness out of thin air;
   in testing, the `Acme --?--> 深圳` entry was one a person would reject at once on reading the source sentence.
   Two places share the same row component: the "pending" tier on the Review page, and the card that follows a remember step in Chat. */
import { fmtObjectValue } from "../objectValue";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type PendingFactItem } from "../api";
import { S } from "../i18n";
import { toast } from "../toast";

function ym(iso: string | null): string | null {
  return iso ? iso.slice(0, 7) : null;
}

/** When a memory is stored, the body is prefixed with `[YYYY-MM-DD HH:MM] ` (the timestamp added by `memory::append_episode`).
 *  The card wants the sentence itself; the timestamp is for indexing, so strip it */
function sentence(quote: string): string {
  return quote.replace(/^\[\d{4}-\d{2}-\d{2}(?: \d{2}:\d{2})?\]\s*/, "");
}

function objectText(f: PendingFactItem): string {
  if (f.object_name) return f.object_name;
  return fmtObjectValue(f.object_value as Record<string, unknown> | null) ?? "?";
}

/** Nodding is a graph-writing action, so the minimum is Editor, the same bar as the server's `require_kb(Role::Editor)`.
 *  A Viewer can see the proposal but not the buttons: a button that lights up but cannot be pressed would make people guess whether they have permission.
 *  The query key is shared with the Library page (`kbOne`), so it does not cost an extra request */
export function useCanDecide(kbId: string | undefined): boolean {
  const kbDetail = useQuery({
    queryKey: ["kbOne", kbId],
    queryFn: () => api.kbDetail(kbId!),
    enabled: !!kbId,
  });
  return ["editor", "admin", "owner"].includes(kbDetail.data?.my_role ?? "");
}

export function PendingFactRow({
  fact,
  busy,
  canDecide,
  onConfirm,
  onReject,
}: {
  fact: PendingFactItem;
  busy: boolean;
  canDecide: boolean;
  onConfirm: () => void;
  onReject: () => void;
}) {
  const from = ym(fact.valid_from);
  const to = ym(fact.valid_to);
  // Open statements (0044) carry the time words copied verbatim, not an interval computed from them: if there is a word, show the word
  const timeWords = (fact.time_words ?? []).map((t) => t.text).join(" · ");
  const range = timeWords
    ? timeWords
    : from || to
      ? `${from ?? "…"} → ${to ?? S.review.ongoing}`
      : null;
  const qualifiers = fact.qualifiers ?? [];
  return (
    <div className="glass rounded-panel p-4">
      {/* The source sentence comes first. It is what the person said, and it is the basis of the judgement */}
      <p className="text-small text-ink-2 italic">“{sentence(fact.quote)}”</p>
      <div className="mt-3 flex items-center gap-2 flex-wrap">
        <span className="text-body font-medium text-ink">{fact.subject_name}</span>
        <span className="text-small text-ink-2">
          —{" "}
          {fact.predicate_label ? (
            <span>{fact.predicate_label}</span>
          ) : fact.phrase ? (
            /* Open statement: the document's own words; italics mark it as a verbatim quote rather than a word from the vocabulary (0044) */
            <span className="italic text-ink-2" title={S.review.pendingOwnWords}>
              {fact.phrase}
            </span>
          ) : (
            /* The relation is not in the ontology: show the verbatim wording, in italics to mark that it is not a vocabulary term (0010) */
            <span
              className="italic text-ink-2"
              title={S.review.pendingNoPredicate}
            >
              {fact.proposed_predicate ?? S.graph.unknownPredicate}
            </span>
          )}{" "}
          →
        </span>
        <span className="text-body font-medium text-ink">{objectText(fact)}</span>
        {range && <span className="text-small text-ink-2">({range})</span>}
        {!fact.predicate_label && !fact.phrase && (
          <Status tone="warn" className="ml-auto shrink-0">
            {S.review.pendingNoPredicateChip}
          </Status>
        )}
      </div>
      {qualifiers.length > 0 && (
        /* Qualifiers hang off the document's role words (0044): amount, object, comparison basis */
        <div className="mt-2 flex flex-wrap gap-1">
          {qualifiers.map((q) => (
            <span key={q.role} className="rounded-full bg-surface-2 px-2 py-1 text-fine text-ink-2">
              {q.role}: {q.entity_name ?? String(q.value ?? "")}
            </span>
          ))}
        </div>
      )}
      {/* The same footer as the other six kinds of card on the review page (CARD_ACTIONS): actions on the left, "who said it" pushed to the right */}
      <div className={CARD_ACTIONS}>
        {canDecide && (
          <>
            <Button variant="secondary" size="sm"
              disabled={busy}
              onClick={onConfirm}
            >
              {S.review.confirm}
            </Button>
            <Button variant="danger" size="sm"
              disabled={busy}
              onClick={onReject}
            >
              {S.review.reject}
            </Button>
          </>
        )}
        {fact.proposed_by_name && (
          <span className="ml-auto truncate text-fine text-ink-2">
            {fact.proposed_token_name
              ? S.review.pendingSaidVia(
                  fact.proposed_by_name,
                  fact.proposed_token_name,
                )
              : S.review.pendingSaidBy(fact.proposed_by_name)}
          </span>
        )}
      </div>
    </div>
  );
}

/** The confirmation card that follows a remember step.
 *  Extraction is asynchronous: the card grows only when the task completes (the SSE `pending` event invalidates the query and refetches);
 *  replaying an old session also fetches by chunk, so any still awaiting a nod are shown, and once all are handled they take up no space. */
export function NodCard({ kbId, chunkId }: { kbId: string; chunkId: string }) {
  const queryClient = useQueryClient();
  const canDecide = useCanDecide(kbId);
  const q = useQuery({
    queryKey: ["pending", kbId, chunkId],
    queryFn: () => api.pendingForChunk(kbId, chunkId),
  });
  const decide = useMutation({
    mutationFn: ({ id, action }: { id: string; action: "confirm" | "reject" }) =>
      api.decidePending(kbId, id, action),
    onError: (e: Error) => toast.error(e.message),
    onSettled: () => {
      queryClient.invalidateQueries({ queryKey: ["pending", kbId] });
      queryClient.invalidateQueries({ queryKey: ["review", kbId] });
    },
  });
  const items = q.data?.items ?? [];
  if (items.length === 0) return null;
  return (
    <div className="my-2 space-y-2">
      <div className="text-small text-ink-2">{S.review.nodCardTitle(items.length)}</div>
      {items.map((f) => (
        <PendingFactRow
          key={f.id}
          fact={f}
          busy={decide.isPending && decide.variables?.id === f.id}
          canDecide={canDecide}
          onConfirm={() => decide.mutate({ id: f.id, action: "confirm" })}
          onReject={() => decide.mutate({ id: f.id, action: "reject" })}
        />
      ))}
    </div>
  );
}
