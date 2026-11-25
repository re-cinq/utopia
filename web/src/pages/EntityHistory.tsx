/* The entity's belief-change history (the record timeline).
   Orthogonal to the Timeline view on the same panel: that axis asks 'when did this hold in reality', this axis asks
   "'when did we believe this, and when did we change our mind'. The data comes from the rows in the append-only ledger that
   entity_detail filters out using invalidated_at IS NULL. */
import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { FileText, Merge, PencilLine, Tag, Undo2 } from "lucide-react";
import { api, type EntityHistoryEvent } from "../api";
import { S } from "../i18n";
import { predicateSentence } from "../predicateText";
import { useKbId } from "../kb";
import { Pager } from "../ui";

const PER = 20;

/** Event type → icon and tone (the semantic color is reserved for 'overturned'; the rest stay neutral) */
const KIND_ICON = {
  asserted: FileText,
  corrected: PencilLine,
  rejected: Undo2,
  // Merging is not a withdrawal: the content moved into another assertion without losing a word
  merged: Merge,
  /* Entity merge: the largest belief change on this axis. From now on it counts as the same thing as another entity.
     Both directions share one icon; the direction is written in the body ('←' means someone was merged into it) */
  merged_in: Merge,
  merged_away: Merge,
  merge_reverted: Undo2,
  // Changing class is not a fact change: the node on the graph got a different class, no fact moved
  retyped: Tag,
  retype_reverted: Undo2,
} as const;

/** The three entity-merge kinds: the body names the other entity, not a predicate + object */
const MERGE_KINDS = new Set(["merged_in", "merged_away", "merge_reverted"]);

const KIND_TONE: Record<string, string> = {
  asserted: "text-ink-2",
  corrected: "text-warn",
  rejected: "text-danger",
  merged: "text-ink-2",
  merged_in: "text-ink-2",
  merged_away: "text-ink-2",
  // Undo and 'class change undone' are the same tier: done, then taken back
  merge_reverted: "text-warn",
  retyped: "text-ink-2",
  retype_reverted: "text-warn",
};

/* These two functions are **deliberately different**; do not 'unify' them: they render two kinds of time.

   `ymd` gives the **recording moment** (when we came to believe it): that is a real instant,
   and should be shown in the timezone of the reader. It used to slice an ISO string too, which meant showing it in UTC:
   a revision a UTC+8 person made before 8 a.m. showed up in history as the previous day.

   `ym` gives the **world time** (when this happened): it comes from the statement in the document
   ('took office in May 2019'), a **calendar date, not an instant**, so it never had a timezone.
   Slicing the ISO string reads back the day as stored in UTC; converting it to local would make
   a reader in UTC-5 sees the previous month. */
const ymd = (iso: string) => {
  const d = new Date(iso);
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${d.getFullYear()}-${m}-${day}`;
};
const ym = (iso: string | null) => (iso ? iso.slice(0, 7) : null);

/** Object: entity name first, otherwise the summary or value of the literal (attribute fact) */
function objectText(e: EntityHistoryEvent): string {
  if (e.other_name) return e.other_name;
  const v = e.object_value as { summary?: unknown; value?: unknown } | null;
  const raw = v?.summary ?? v?.value;
  return raw === undefined || raw === null ? "—" : String(raw);
}

/** What this change did to the validity interval (an event on the record axis, changing the boundary on the validity axis).
 *
 *  Correction rows **no longer assume a correction means closing**. Previously this said 'closed at' whenever valid_to existed,
 *  and otherwise said nothing; that held when only automatic closing existed. Correction rows from manual start edits (302)
 *  have no end side, so the whole event reduced to one icon, with no way to tell what it was changed to.
 *
 *  Both branches describe the state **after** the correction and do not claim a cause: engine takeover, Review verdicts
 *  and manual edits cannot be told apart on this row, and need not be: who changed it is in the attribution on the line below. */
function intervalNote(e: EntityHistoryEvent): string | null {
  // An end side present = the interval closes here. This sentence holds for all three sources
  if (e.kind === "corrected" && e.valid_to) {
    return S.graph.historyClosedAt(ym(e.valid_to)!);
  }
  const from = ym(e.valid_from);
  if (!from) return null;
  return e.valid_to
    ? `${from} → ${ym(e.valid_to)}`
    : `${S.graph.historyFrom(from)} · ${S.graph.historyOngoing}`;
}

function EventRow({ e }: { e: EntityHistoryEvent }) {
  const kbId = useKbId();
  const Icon = KIND_ICON[e.kind] ?? FileText;
  const note = intervalNote(e);
  return (
    <div className="flex gap-2 px-2 py-2">
      <Icon size={12} className={`mt-1 shrink-0 ${KIND_TONE[e.kind] ?? ""}`} />
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-2 flex-wrap">
          <span className="text-small font-medium text-ink-2">
            {S.graph.historyKind[e.kind] ?? e.kind}
          </span>
          {note && <span className="u-num text-fine text-ink-2">{note}</span>}
        </div>
        {/* A change-class event has no predicate and no object; the body swaps in the two ends of the class.
            A missing start = changed from 'unclassified', the most common case since 0009 */}
        {e.kind === "retyped" || e.kind === "retype_reverted" ? (
          <div className="mt-1 text-body text-ink-2 truncate">
            <span className="text-small text-ink-2">
              {e.from_type_label ?? S.graph.untyped} →{" "}
            </span>
            <span className="text-ink">{e.to_type_label}</span>
          </div>
        ) : MERGE_KINDS.has(e.kind) ? (
          /* A merge event's body names only the other party. **Arrows show direction**: merged in is '←',
             merged out is '→', the same reading as the fact line above */
          <div className="mt-1 text-body text-ink-2 truncate">
            <span className="text-small text-ink-2">
              {e.kind === "merged_away" ? "→ " : "← "}
            </span>
            <span className="text-ink">{e.other_name ?? S.graph.historyGoneEntity}</span>
          </div>
        ) : (
          <div className="mt-1 text-body text-ink-2 truncate">
            <span className="text-small text-ink-2">
              {e.direction === "in" ? "← " : ""}
              <span className={e.predicate_label === null ? "italic text-ink-2" : undefined}>
                {/* The chronology is also one sentence per line, split the same way as the fact lines */}
                {e.predicate_label
                  ? predicateSentence(e.predicate_label)
                  : S.graph.unknownPredicate}
              </span>
              {e.direction === "in" ? "" : " →"}
            </span>{" "}
            <span className="text-ink">{objectText(e)}</span>
          </div>
        )}
        <div className="mt-1 flex items-center gap-2 text-fine text-ink-2">
          <span className="u-num">{ymd(e.at)}</span>
          <span>·</span>
          {/* Attribution: a person's name, or the engine (extraction write / temporal reconciliation auto-close) */}
          <span>{e.actor_name ?? S.graph.historyEngine}</span>
          {e.filename && e.document_id && (
            <>
              <span>·</span>
              <Link
                to="/kb/$kbId/doc/$docId"
                params={{ kbId, docId: e.document_id }}
                search={{}}
                className="u-hover-ink truncate"
                title={e.quote ?? e.filename}
              >
                {e.filename}
              </Link>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

export function EntityHistory({ kbId, entityId }: { kbId: string; entityId: string }) {
  const [page, setPage] = useState(0);
  useEffect(() => setPage(0), [entityId]);
  const q = useQuery({
    queryKey: ["entityHistory", kbId, entityId, page],
    queryFn: () => api.entityHistory(kbId, entityId, page, PER),
  });

  const total = q.data?.total ?? 0;
  if (q.isPending) return <p className="p-2 text-body text-ink-2">{S.nav.loading}</p>;
  // It is empty only when there are zero entries. The first assertion on the record axis is itself an event:
  // "// 'when and from which document did we learn this' is half of what this axis is meant to answer
  if (total === 0) return <p className="p-2 text-small text-ink-2">{S.graph.historyEmpty}</p>;

  return (
    <div>
      <p className="px-2 pb-2 text-fine text-ink-2">{S.graph.historyHint}</p>
      <div className="divide-y divide-line">
        {/* key {/* The key uses fact_id ?? at: a change-class event has no fact_id */}
        {(q.data?.events ?? []).map((e) => (
          <EventRow key={`${e.fact_id ?? e.at}-${e.kind}`} e={e} />
        ))}
      </div>
      <Pager total={total} pageSize={PER} page={page} onPage={setPage} />
    </div>
  );
}
