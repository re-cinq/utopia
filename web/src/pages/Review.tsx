import { alignmentErrorMessage } from "./reviewErrors";
import { asksMarks, phraseStart } from "./alignmentPhrase";
import { useEffect, useState } from "react";
import { LayoutDashboard } from "lucide-react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate, useSearch } from "@tanstack/react-router";
import {
  api,
  type AgentDecision,
  type AgentPrecedent,
  type AxiomViolation,
  type ReviewQueue,
  type ReviewTypeFilter,
  type OntologyDefect,
  type AlignmentItem,
  type AlignmentMarks,
  type ErrataItem,
  type EntityTypeView,
  type RelationTypeView,
  type ConflictItem,
  type FactReviewItem,
  type MergeLog,
  type PendingFactItem,
  type ReviewHistoryEvent,
  type ReviewItem,
  type ReviewSide,
  type ViolationResolution,
} from "../api";
import { fmtTime, parseDateInput } from "../time";
import { PendingFactRow, useCanDecide } from "./PendingFacts";
import { ReviewOverview } from "./ReviewOverview";
import { S } from "../i18n";
import { useKb, useKbId } from "../kb";
import { toast } from "../toast";
import {
  Button,
  CARD_ACTIONS,
  Checkbox,
  Chip,
  GroupLabel,
  Input,
  LinkButton,
  PageHeader,
  Pager,
  RAIL_CLS,
  RailItem,
  Segmented,
  SearchSelect,
  Status,
  cn,
  localDate,
  type ChipTone,
} from "../ui";

const DUP_PAGE = 6;
const FACT_PAGE = 10;
const MERGE_PAGE = 10;
const CONFLICT_PAGE = 8;

export function DuplicateReviewPager({
  totals,
  types,
  page,
  onPage,
}: {
  totals: Record<ReviewTypeFilter, number>;
  types: ReviewTypeFilter;
  page: number;
  onPage: (page: number) => void;
}) {
  return <Pager total={totals[types]} pageSize={DUP_PAGE} page={page} onPage={onPage} />;
}

const ym = (iso: string | null) => (iso ? iso.slice(0, 7) : null);

/** `code` or `code|detail`. If it cannot be looked up, show it as-is: legacy rows still hold the old English prose */
function escalationText(reason: string): string {
  const [code, detail] = reason.split("|");
  const worded = S.review.escalated[code];
  if (!worded) return reason;
  // Left by the execution gate (0027): detail is `kind value`, worded by kind, so the bare code is not shown to people
  if (code === "escalate_impact" && detail) {
    const [kind, ...rest] = detail.split(" ");
    const said = S.review.impact[kind]?.(rest.join(" "));
    if (said) return `${worded} — ${said}`;
  }
  return detail ? S.errDetail(worded, detail) : worded;
}

function dateRange(from: string | null, to: string | null): string | null {
  if (!from && !to) return null;
  return `${ym(from) ?? "…"} → ${ym(to) ?? S.review.ongoing}`;
}

function SideCard({ side }: { side: ReviewSide }) {
  return (
    <div className="flex-1 min-w-0">
      {/* **Write only the name, without the color dot.** The color dot is a marker on the canvas: a screen there holds dozens of nodes,
          color is the only thing that tells classes apart at a glance. This card has just two entities, and the class name on the line below
          is written in plain text ('Organization · 5 facts'); saying it again would be redundant,
          and it would push the name a fair way to the right. */}
      <div className="mb-1 flex items-center gap-2">
        <span className="truncate text-body font-medium text-ink">
          {side.name}
        </span>
        {side.disambiguator && (
          <span className="text-small text-ink-2 truncate">
            · {side.disambiguator}
          </span>
        )}
      </div>
      <div className="text-small text-ink-2 mb-2">
        {side.type_label ?? S.graph.untyped} ·{" "}
        {S.review.factsCount(side.degree)}
      </div>
      {side.top_facts.length > 0 ? (
        <ul className="space-y-1">
          {side.top_facts.map((f, i) => (
            <li key={i} className="text-small text-ink-2 truncate">
              {f}
            </li>
          ))}
        </ul>
      ) : (
        <p className="text-small text-ink-2">{S.review.noFacts}</p>
      )}
    </div>
  );
}

/** Both sides have types and they differ: same name, different meaning; merging would be wrong. This is the one thing to spot at a glance when scanning a screen of duplicates,
 *  so it is promoted to a marker, not left for people to read a line of small text on each of two cards */
function typesDiffer(item: ReviewItem): boolean {
  return (
    item.left.type_label !== null &&
    item.right.type_label !== null &&
    item.left.type_label !== item.right.type_label
  );
}

function DuplicateCard({
  item,
  busy,
  locked,
  picked,
  onPick,
  onDecide,
}: {
  item: ReviewItem;
  busy: boolean;
  /** An agent is adjudicating this pair (0025): during these minutes people cannot act on it, and the API will also refuse */
  locked: boolean;
  /** Batch selection (#428): the checkbox sits at the top left of the card; once selected it follows the batch buttons above */
  picked: boolean;
  onPick: (picked: boolean) => void;
  /** The second argument is the sentence a person wrote (0026): what made you decide this. Optional */
  onDecide: (action: "merge" | "keep", rationale?: string) => void;
}) {
  const reasonCode = item.reason?.split("|", 1)[0];
  const [why, setWhy] = useState("");

  return (
    <div className={cn("glass rounded-panel p-4", picked && "u-picked")}>
      {/* **Why this pair**, written before the two sides: who is deciding, whether the types match, what the agent suggests,
          and why it says they are alike. Only after reading this line do the two columns below make sense about what to watch. */}
      <div className="mb-3 flex flex-wrap items-center gap-3">
        {locked ? (
          <Status tone="warn" pulse>
            {S.review.agentDeciding}
          </Status>
        ) : (
          <Status tone={item.stage === "human" ? "warn" : "neutral"}>
            {item.stage === "human"
              ? S.review.stageHuman
              : S.review.stageAdjudicating}
          </Status>
        )}
        {typesDiffer(item) && (
          <Chip tone="warn" title={S.review.typesDifferHint}>
            {S.review.typesDiffer(
              item.left.type_label ?? "",
              item.right.type_label ?? "",
            )}
          </Chip>
        )}
        {/* agent {/* The agent's suggestion (0025): Merge / Keep below are the answer to it */}
        {item.proposal && (
          <Chip tone="info" title={item.proposal.reason ?? undefined}>
            {S.review.agentSuggests(
              S.review.agentActions[item.proposal.action],
              Math.round(item.proposal.confidence * 100),
            )}
          </Chip>
        )}
        {reasonCode !== "namesake" && (
          <span className="text-small text-ink-2">
            {S.review.similarity(Math.round(item.score * 100))}
          </span>
        )}
        {item.reason && (
          <span className="truncate text-small text-ink-2">
            {escalationText(item.reason)}
          </span>
        )}
      </div>
      <div className="flex gap-4">
        <Checkbox
          className="shrink-0 self-start"
          checked={picked}
          disabled={busy || locked}
          onChange={(v) => onPick(v)}
          label={<span className="sr-only">{S.review.pickPair}</span>}
        />
        <SideCard side={item.left} />
        <div className="self-center text-ink-2 text-body shrink-0">≟</div>
        <SideCard side={item.right} />
      </div>
      {/* The footer holds only actions, the same as other cards (CARD_ACTIONS): bottom left, dangerous ones last.
          'Why this pair' has moved to the very top of the card: it is **what you need to know before reading this card**,
          not a footnote after the decision. When it sat in the bottom right, a person had to read both sides' facts first, then move their eyes
          diagonally to find 'why do they look alike'. */}
      <div className={cn(CARD_ACTIONS, "pt-3 border-t border-line")}>
        {/* Reason box (0026): optional; if written it goes into the ledger with the decision, and the next precedent carries it */}
        <Input
          size="sm"
          className="w-56"
          placeholder={S.review.rationalePlaceholder}
          value={why}
          disabled={busy || locked}
          onChange={(e) => setWhy(e.target.value)}
        />
        <Button variant="primary" size="sm"
          disabled={busy || locked}
          onClick={() => onDecide("merge", why)}
        >
          {S.review.merge}
        </Button>
        <Button variant="secondary" size="sm"
          disabled={busy || locked}
          onClick={() => onDecide("keep", why)}
        >
          {S.review.keep}
        </Button>
      </div>
    </div>
  );
}

function FactRow({
  fact,
  busy,
  onConfirm,
  onReject,
}: {
  fact: FactReviewItem;
  busy: boolean;
  onConfirm: () => void;
  onReject: () => void;
}) {
  const range = dateRange(fact.valid_from, fact.valid_to);
  return (
    <div className="glass rounded-panel p-4">
      <div className="flex items-center gap-2 flex-wrap">
        <span className="text-body font-medium text-ink">
          {fact.subject_name}
        </span>
        <span className="text-small text-ink-2">
          —{" "}
          <span
            className={
              fact.predicate_label === null
                ? "italic text-ink-2"
                : undefined
            }
          >
            {fact.predicate_label ?? S.graph.unknownPredicate}
          </span>{" "}
          →
        </span>
        <span className="text-body font-medium text-ink">
          {fact.object_name ?? "?"}
        </span>
        {range && <span className="text-small text-ink-2">({range})</span>}
        {/* Confidence is **one number**, not a state: the same plain numerals as the other three confidence readings on this page.
            Previously this was a filled amber pill, so the same number had two looks on the same page */}
        <span className="ml-auto u-num text-small text-ink-2 shrink-0">
          {S.review.confidence(Math.round(fact.confidence * 100))}
        </span>
      </div>
      {fact.quote && (
        <p className="mt-2 text-small text-ink-2 italic line-clamp-2">
          “{fact.quote}”
        </p>
      )}
      <div className={CARD_ACTIONS}>
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
      </div>
    </div>
  );
}

/** Temporal conflict row: old fact vs new fact, three actions (Close old / Keep both / Reject new). */
function ConflictRow({
  conflict,
  busy,
  onResolve,
}: {
  conflict: ConflictItem;
  busy: boolean;
  onResolve: (
    action: "close" | "retime_new" | "keep" | "reject_new",
    closeAt?: string,
    closeAtPrecision?: string,
  ) => void;
}) {
  const [closeAt, setCloseAt] = useState("");
  const c = conflict;
  const needsDate = !c.new_valid_from;
  // The number of digits written is the precision (time.ts): '2023-06' closes at that month, without inventing a 1st day
  const closeParsed = parseDateInput(closeAt);
  const closeAtIso = closeParsed?.iso;
  const invalidDate = closeAt.trim() !== "" && !closeParsed;

  return (
    <div className="glass rounded-panel p-4">
      <div className="flex items-center gap-2 flex-wrap">
        <span className="text-body font-medium text-ink">{c.old_subject}</span>
        <span className="text-small text-ink-2">
          — {c.predicate_label} →
        </span>
        <span className="text-body font-medium text-ink">
          {c.old_object ?? "?"}
        </span>
        {c.old_valid_from && (
          <span className="u-num text-small text-ink-2">
            ({S.review.conflictSince(fmtTime(c.old_valid_from, c.old_valid_from_precision)!)})
          </span>
        )}
        <span className="text-small text-ink-2">{S.review.conflictVs}</span>
        <span className="text-body font-medium text-ink">{c.new_subject}</span>
        <span className="text-small text-ink-2">
          — {c.predicate_label} →
        </span>
        <span className="text-body font-medium text-ink">
          {c.new_object ?? "?"}
        </span>
        {c.new_valid_from && (
          <span className="u-num text-small text-ink-2">
            ({S.review.conflictSince(fmtTime(c.new_valid_from, c.new_valid_from_precision)!)})
          </span>
        )}
        <Status tone="warn" className="ml-auto shrink-0">
          {S.review.conflictReason[c.reason] ?? c.reason}
        </Status>
      </div>
      <div className={CARD_ACTIONS}>
        <Button variant="secondary" size="sm"
          disabled={busy}
          onClick={() => onResolve("keep")}
        >
          {S.review.keepBoth}
        </Button>
        <Input size="sm" className="u-num w-28 text-center"
          aria-label={S.review.conflictDate}
          placeholder={S.review.closeAtPlaceholder}
          value={closeAt}
          onChange={(e) => setCloseAt(e.target.value)}
        />
        <Button variant="secondary" size="sm"
          disabled={busy || invalidDate || (needsDate && !closeAtIso)}
          onClick={() => onResolve("close", closeAtIso, closeParsed?.precision)}
        >
          {closeParsed
            ? S.review.closeOldAt(fmtTime(closeParsed.iso, closeParsed.precision)!)
            : c.new_valid_from
              ? S.review.closeOldAt(fmtTime(c.new_valid_from, c.new_valid_from_precision)!)
              : S.review.closeOld}
        </Button>
        <Button variant="secondary" size="sm"
          disabled={busy || !closeParsed}
          onClick={() => onResolve("retime_new", closeAtIso, closeParsed?.precision)}
        >
          {S.review.retimeNew}
        </Button>
        <Button variant="danger" size="sm"
          disabled={busy}
          onClick={() => onResolve("reject_new")}
        >
          {S.review.rejectNew}
        </Button>
      </div>
    </div>
  );
}

/** "/** A fact row that the newer document version no longer mentions: Reject (extraction error) or Close at date (it has ended). */
function UnconfirmedRow({
  fact,
  busy,
  onReject,
  onClose,
}: {
  fact: FactReviewItem;
  busy: boolean;
  onReject: () => void;
  onClose: (validTo: string, precision: string) => void;
}) {
  const [closeAt, setCloseAt] = useState("");
  const closeParsed = parseDateInput(closeAt);
  const closeAtIso = closeParsed?.iso;
  const range = dateRange(fact.valid_from, fact.valid_to);

  return (
    <div className="glass rounded-panel p-4">
      <div className="flex items-center gap-2 flex-wrap">
        <span className="text-body font-medium text-ink">
          {fact.subject_name}
        </span>
        <span className="text-small text-ink-2">
          —{" "}
          <span
            className={
              fact.predicate_label === null
                ? "italic text-ink-2"
                : undefined
            }
          >
            {fact.predicate_label ?? S.graph.unknownPredicate}
          </span>{" "}
          →
        </span>
        <span className="text-body font-medium text-ink">
          {fact.object_name ?? "?"}
        </span>
        {range && (
          <span className="u-num text-small text-ink-2">({range})</span>
        )}
      </div>
      {fact.quote && (
        <p className="mt-2 text-small text-ink-2 italic line-clamp-2">
          “{fact.quote}”
        </p>
      )}
      <div className={CARD_ACTIONS}>
        <Input size="sm" className="u-num w-28 text-center"
          placeholder={S.review.closeAtPlaceholder}
          value={closeAt}
          onChange={(e) => setCloseAt(e.target.value)}
        />
        <Button variant="secondary" size="sm"
          disabled={busy || !closeAtIso}
          onClick={() =>
            closeParsed && onClose(closeParsed.iso, closeParsed.precision)
          }
        >
          {closeAt.trim()
            ? S.review.closeFactAt(closeAt.trim())
            : S.review.closeFact}
        </Button>
        <Button variant="danger" size="sm"
          disabled={busy}
          onClick={onReject}
        >
          {S.review.reject}
        </Button>
      </div>
    </div>
  );
}

function MergeRow({
  merge,
  busy,
  onRevert,
}: {
  merge: MergeLog;
  busy: boolean;
  onRevert: () => void;
}) {
  return (
    <div className="glass rounded-panel px-4 py-3 flex items-center gap-3">
      <div className="min-w-0 flex-1">
        <div className="text-body text-ink-2 truncate">
          <span className="text-ink-2">{merge.source_name}</span>
          <span className="text-ink-2"> → </span>
          <span className="text-ink">{merge.target_name}</span>
        </div>
        <div className="text-small text-ink-2 truncate">
          {merge.merged_by_name
            ? S.review.mergedBy(merge.merged_by_name)
            : S.review.mergedByAi}
          {" · "}
          {localDate(merge.created_at)}
          {merge.reason ? ` · ${escalationText(merge.reason)}` : ""}
        </div>
      </div>
      {merge.reverted_at ? (
        <Status className="shrink-0">{S.review.reverted}</Status>
      ) : (
        <Button variant="secondary" size="sm" className="shrink-0"
          disabled={busy}
          onClick={onRevert}
        >
          {S.review.revert}
        </Button>
      )}
    </div>
  );
}

/* ---------- agent /* ---------- The agent's entry (0025) ---------- */

const AGENT_ACTION_TONE: Record<AgentDecision["action"], ChipTone> = {
  merge: "violet",
  keep: "neutral",
  unsure: "warn",
};

const AGENT_STATUS_TONE: Record<AgentDecision["status"], ChipTone> = {
  proposed: "warn",
  applied: "info",
  accepted: "success",
  overridden: "neutral",
  reverted: "danger",
  superseded: "neutral",
};

/** One precedent written as a sentence; for the same type a habit is summarized, a single sentence on its own */
function precedentText(p: AgentPrecedent): string {
  if (p.family === "type_pair")
    return S.review.agentPrecedentHabit(p.merged, p.kept, p.reverted);
  const verb =
    p.action === "merge.revert"
      ? S.review.agentPrecedentReverted
      : p.action === "review.keep"
        ? S.review.agentPrecedentKept
        : S.review.agentPrecedentMerged;
  const line = `${p.left} ≟ ${p.right} · ${verb} · ${localDate(p.at)}`;
  // The sentence the person wrote (0026) follows after: a precedent is more than just the result
  return p.why ? `${line} · “${p.why}”` : line;
}

function AgentRow({
  d,
  busy,
  onAnswer,
}: {
  d: AgentDecision;
  busy: boolean;
  onAnswer: (action: "merge" | "keep" | "revert", rationale?: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [why, setWhy] = useState("");
  const precedents = d.precedents ?? [];
  const trace = d.trace ?? [];
  const hasDetail = precedents.length > 0 || trace.length > 0;
  return (
    <div className="glass rounded-panel px-4 py-3">
      <div className="flex items-center gap-3">
        <Status tone={AGENT_ACTION_TONE[d.action]}>{S.review.agentActions[d.action]}</Status>
        <span className="text-body text-ink-2 truncate min-w-0">
          {d.left ?? "?"} ≟ {d.right ?? "?"}
        </span>
        <span className="u-num text-small text-ink-2 shrink-0">
          {Math.round(d.confidence * 100)}%
        </span>
        <Status tone={AGENT_STATUS_TONE[d.status]} className="ml-auto shrink-0">
          {S.review.agentStatus[d.status]}
        </Status>
      </div>
      {/* defer {/* The question left by defer (second slice): this is body text for people to read, not a footnote */}
      {d.question && (
        <p className="mt-1 text-body text-ink">
          <span className="text-ink-2">{S.review.agentAsks} </span>
          {d.question}
        </p>
      )}
      {(d.reason || hasDetail) && (
        <div className="mt-1 flex items-center gap-3 text-small text-ink-2">
          {d.reason && <span className="truncate min-w-0">{d.reason}</span>}
          {hasDetail && (
            <LinkButton className="shrink-0" onClick={() => setOpen((v) => !v)}>
              {[
                precedents.length > 0 ? S.review.agentPrecedents(precedents.length) : null,
                trace.length > 0 ? S.review.agentLookups(trace.length) : null,
              ]
                .filter(Boolean)
                .join(" · ")}
            </LinkButton>
          )}
        </div>
      )}
      {open && hasDetail && (
        <ul className="mt-2 space-y-1 border-t border-line pt-2 text-small text-ink-2">
          {precedents.map((p, i) => (
            <li key={`p${i}`} className="truncate">
              {precedentText(p)}
            </li>
          ))}
          {/* The trace is the explanation: what it looked up, one line per lookup */}
          {trace.map((t, i) => (
            <li key={`t${i}`} className="truncate">
              {S.review.agentLookedAt} {t.note}
            </li>
          ))}
        </ul>
      )}
      <div className="mt-2 flex items-center gap-2">
        <span className="text-fine text-ink-2">
          {d.decided_by_name
            ? S.review.agentAnsweredBy(d.decided_by_name, localDate(d.decided_at ?? d.created_at))
            : localDate(d.created_at)}
        </span>
        <div className="ml-auto flex items-center gap-2 shrink-0">
          {/* The answer agent can also carry a sentence of reasoning (0026): it goes through exactly the human decision path */}
          {(d.status === "proposed" || d.status === "applied") && (
            <Input
              size="sm"
              className="w-56"
              placeholder={S.review.rationalePlaceholder}
              value={why}
              disabled={busy}
              onChange={(e) => setWhy(e.target.value)}
            />
          )}
          {d.status === "proposed" && (
            <>
              <Button variant="secondary" size="sm" disabled={busy} onClick={() => onAnswer("keep", why)}>
                {S.review.keep}
              </Button>
              <Button variant="primary" size="sm" disabled={busy} onClick={() => onAnswer("merge", why)}>
                {S.review.merge}
              </Button>
            </>
          )}
          {d.status === "applied" && d.action === "merge" && (
            <Button variant="secondary" size="sm" disabled={busy} onClick={() => onAnswer("revert", why)}>
              {S.review.revert}
            </Button>
          )}
          {d.status === "applied" && d.action === "keep" && (
            <Button variant="secondary" size="sm" disabled={busy} onClick={() => onAnswer("merge", why)}>
              {S.review.merge}
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}

/* ---------- Decision ledger row ---------- */

const DECISION_TONE: Record<string, ChipTone> = {
  "review.merge": "violet",
  "merge.manual": "violet",
  "review.keep": "neutral",
  "fact.confirm": "success",
  "fact.reject": "danger",
  "conflict.reject_new": "danger",
  "fact.close": "info",
  "conflict.close_old": "info",
  "conflict.retime_new": "info",
  "conflict.keep_both": "neutral",
  "merge.revert": "warn",
};

function DecisionRow({ e }: { e: ReviewHistoryEvent }) {
  // detail is a self-contained snapshot at the time of decision: it does not join live data, so the ledger stays complete even after the fact is deleted
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const d = e.detail as any;
  let text: string;
  if (e.action.startsWith("review.")) text = `${d.left} ≟ ${d.right}`;
  else if (e.action.startsWith("fact."))
    text = `${d.subject} — ${d.predicate ?? "?"} → ${d.object ?? "?"}`;
  else if (e.action.startsWith("conflict."))
    text = `${d.old_subject} — ${d.predicate} → ${d.old_object ?? "?"} · vs · ${
      d.new_object ?? d.new_subject
    }`;
  else text = `${d.source} → ${d.target}`;

  return (
    <div className="glass rounded-panel px-4 py-3 flex items-center gap-3">
      <Chip tone={DECISION_TONE[e.action] ?? "neutral"}>
        {S.review.decisionActions[e.action] ?? e.action}
      </Chip>
      <span className="text-body text-ink-2 truncate min-w-0">{text}</span>
      {/* The sentence a person (or model) wrote (0026); old rows do not have one */}
      {typeof d.why === "string" && d.why && (
        <span className="text-small text-ink-2 truncate min-w-0">
          “{d.why}”
        </span>
      )}
      {typeof d.confidence === "number" && (
        <span className="u-num text-small text-ink-2 shrink-0">
          {Math.round(d.confidence * 100)}%
        </span>
      )}
      {typeof (d.new_from ?? d.close_at ?? d.valid_to) === "string" && (
        <span className="u-num text-small text-ink-2 shrink-0">
          → {fmtTime(
            d.new_from ?? d.close_at ?? d.valid_to,
            d.new_from_precision ?? d.close_at_precision ?? d.valid_to_precision,
          )}
        </span>
      )}
      <span className="ml-auto shrink-0 text-small text-ink-2">
        {e.actor_name ?? S.review.aiActor}
        {" · "}
        <span className="u-num">{localDate(e.created_at)}</span>
      </span>
    </div>
  );
}

/** One data-mapping definition awaiting a stance (0011).
 *
 * The focus is **'how this number is computed'**: SQL / expression / table name, one of them taken in this order of priority,
 * because a person is judging exactly whether it is right. The concept name and source are identity; the unit is the dimension the answer must carry. */
/** A self-contradiction within the ontology itself. **Two buttons, not three**: this tier does not look at data at all,
 *  so there is no 'the data is wrong' exit; it can only be 'I went and changed the ontology' or 'leave it for now'. */
function voteText(
  v: { property: string; direction: string; marks?: AlignmentMarks | null } | string | null | undefined,
): string {
  if (v === undefined || v === null) return S.review.alignmentNone;
  if (typeof v === "string") return v;
  const side = v.direction === "reverse" ? S.review.alignmentReverse : S.review.alignmentForward;
  const marks = v.marks ? ` · ${S.review.alignmentMarks} ${marksText(v.marks)}` : "";
  return `${v.property} · ${side}${marks}`;
}

function marksText(m: AlignmentMarks): string {
  return m === "start"
    ? S.review.alignmentMarksStart
    : m === "end"
      ? S.review.alignmentMarksEnd
      : S.review.alignmentMarksNone;
}

/** An implication rule the aligner proposed (0044 decision 3, fifth slice): this shape also implies which attribute, and where the object comes from; a person approves or rejects */
/** A note the erratum agent left for a person (0044 decision 7): which document, what it wants to do to which fact, on what quoted sentence, and why it was left */
function ErrataRow({
  item,
  busy,
  onDecide,
}: {
  item: ErrataItem;
  busy: boolean;
  onDecide: (approve: boolean) => void;
}) {
  const verb =
    item.action === "retract"
      ? S.review.errataRetract
      : item.action === "revise"
        ? S.review.errataRevise
        : S.review.errataAdd;
  const p = item.proposed;
  return (
    <div className="glass rounded-panel p-3">
      <div className="text-small text-ink-2">{item.document}</div>
      <div className="mt-1 text-body">
        <span className="font-medium">{verb}</span>{" "}
        {p ? `${p.subject} —${p.property}→ ${p.object}` : ""}
      </div>
      {item.flag && <div className="mt-1 text-small text-ink-2">{S.review.errataFlag(item.flag)}</div>}
      {item.reason && <div className="mt-1 text-small">{item.reason}</div>}
      {item.quote && (
        <div className="mt-1 text-small text-ink-2">
          {S.review.errataQuote} “{item.quote}”
        </div>
      )}
      {item.detail && <div className="mt-1 text-small text-warn">{S.review.errataHeld(item.detail)}</div>}
      <div className={CARD_ACTIONS}>
        <Button size="sm" disabled={busy} onClick={() => onDecide(true)}>
          {S.review.errataApprove}
        </Button>
        <Button size="sm" disabled={busy} onClick={() => onDecide(false)}>
          {S.review.errataReject}
        </Button>
      </div>
    </div>
  );
}

function AlignmentRuleRow({
  item,
  busy,
  onDecide,
}: {
  item: Extract<AlignmentItem, { kind: "rule" }>;
  busy: boolean;
  onDecide: (approve: boolean) => void;
}) {
  const shape =
    item.trigger === "kind_word"
      ? S.review.alignmentRuleKindWord(item.phrase)
      : `${item.subject_class ?? "?"} —${item.phrase}→ ${item.object_is_value ? "value" : (item.object_class ?? "?")}`;
  return (
    <div className="glass rounded-panel p-3">
      <div className="text-body font-medium">{shape}</div>
      <div className="mt-1 text-small">
        {S.review.alignmentRuleImplies(item.property_label || item.property)} ·{" "}
        {item.reading ? S.review.alignmentRuleReading(item.reading) : S.review.alignmentRuleObjectIsStatement}
      </div>
      {item.examples.length > 0 && (
        <div className="mt-1 space-y-1 text-small text-ink-2">
          {item.examples.map((e, i) => (
            <div key={i}>{e}</div>
          ))}
        </div>
      )}
      <div className={CARD_ACTIONS}>
        <Button size="sm" disabled={busy} onClick={() => onDecide(true)}>
          {S.review.alignmentApprove}
        </Button>
        <Button size="sm" disabled={busy} onClick={() => onDecide(false)}>
          {S.review.alignmentReject}
        </Button>
      </div>
    </div>
  );
}

/** A phrase signature: the phrase, the classes at both ends, examples, two votes; a person chooses the attribute and direction, or 'none'.
 *  When a status attribute is chosen, also say whether a single date marks the start, the end, or neither (#966) */
function AlignmentPhraseRow({
  item,
  properties,
  busy,
  onDecide,
}: {
  item: Extract<AlignmentItem, { kind: "phrase" }>;
  properties: RelationTypeView[];
  busy: boolean;
  onDecide: (
    property: string | null,
    direction: "forward" | "reverse",
    marks: AlignmentMarks | null,
  ) => void;
}) {
  const first = item.votes?.first ?? null;
  const second = item.votes?.second ?? null;
  const start = phraseStart(item);
  const [property, setProperty] = useState<string>(start.property);
  const [direction, setDirection] = useState<"forward" | "reverse">(start.direction);
  const [marks, setMarks] = useState<AlignmentMarks | null>(start.marks);
  // A signature with a literal as its object fits only attributes; between two things, only relations fit
  const fitting = properties.filter((p) =>
    item.object_is_value ? p.kind === "attribute" : p.kind === "relation",
  );
  const state = asksMarks(fitting, property);
  // Under a status attribute, what a single date marks is not preselected: it binds only once a person chooses
  const unread = state && marks === null;
  return (
    <div className="glass rounded-panel p-3">
      <div className="flex items-baseline gap-2 flex-wrap">
        <span className="text-body text-ink">“{item.phrase}”</span>
        <span className="text-small text-ink-2">
          {item.subject_class ?? "?"} → {item.object_is_value ? S.review.alignmentValue : (item.object_class ?? "?")}
        </span>
        <span className="text-small text-ink-2">{S.review.alignmentStatements(item.statement_count)}</span>
      </div>
      {item.examples.length > 0 && (
        <div className="mt-1 space-y-1 text-small text-ink-2">
          {item.examples.map((e, i) => (
            <div key={i}>{e}</div>
          ))}
        </div>
      )}
      {item.bound_to && (
        <div className="mt-1 text-small text-ink-2">
          {S.review.alignmentAwaitsMarks(item.bound_to)}
        </div>
      )}
      {/* A bound signature lists two votes only when the agent voted: they each say what a single date marks */}
      {(!item.bound_to || first || second) && (
        <div className="mt-1 text-small text-ink-2">
          {S.review.alignmentVotes(voteText(first), voteText(second))}
        </div>
      )}
      {/* Signatures with too many candidates to ask the model about (0053): say clearly that this is the reason, not two votes cast as empty */}
      {item.votes?.reason === "too_many_candidates" && (
        <div className="mt-1 text-small text-ink-2">
          {S.review.alignmentTooMany(item.votes?.candidates ?? 0)}
        </div>
      )}
      <div className={CARD_ACTIONS}>
        <SearchSelect
          size="sm"
          className="w-56"
          value={property}
          onChange={setProperty}
          options={[
            { value: "", label: S.review.alignmentNone },
            ...fitting.map((p) => ({ value: p.key, label: p.label })),
          ]}
        />
        {property && !item.object_is_value && (
          <Segmented
            size="sm"
            value={direction}
            onChange={setDirection}
            options={[
              { value: "forward", label: S.review.alignmentForward },
              { value: "reverse", label: S.review.alignmentReverse },
            ]}
          />
        )}
        {state && (
          <>
            <span className="text-small text-ink-2">{S.review.alignmentMarks}</span>
            <Segmented<AlignmentMarks | "">
              size="sm"
              value={marks ?? ""}
              onChange={(m) => setMarks(m || null)}
              options={[
                {
                  value: "start",
                  label: S.review.alignmentMarksStart,
                  title: S.review.alignmentMarksStartHint,
                },
                {
                  value: "end",
                  label: S.review.alignmentMarksEnd,
                  title: S.review.alignmentMarksEndHint,
                },
                {
                  value: "none",
                  label: S.review.alignmentMarksNone,
                  title: S.review.alignmentMarksNoneHint,
                },
              ]}
            />
          </>
        )}
        <Button
          size="sm"
          disabled={busy || unread}
          title={unread ? S.review.alignmentMarksRequired : undefined}
          onClick={() => onDecide(property || null, direction, state ? marks : null)}
        >
          {property ? S.review.alignmentBind : S.review.alignmentLeaveOpen}
        </Button>
      </div>
    </div>
  );
}

/** A category word: word, spelling, example names, the phrases they participate in, two votes; a person chooses class, or 'none' */
function AlignmentKindWordRow({
  item,
  classes,
  busy,
  onDecide,
}: {
  item: Extract<AlignmentItem, { kind: "kind_word" }>;
  classes: EntityTypeView[];
  busy: boolean;
  onDecide: (cls: string | null) => void;
}) {
  const first = item.votes?.first ?? null;
  const second = item.votes?.second ?? null;
  const [cls, setCls] = useState<string>(first ?? second ?? "");
  return (
    <div className="glass rounded-panel p-3">
      <div className="flex items-baseline gap-2 flex-wrap">
        <span className="text-body text-ink">{item.kind_word}</span>
        {item.words.length > 0 && (
          <span className="text-small text-ink-2">{item.words.join(" · ")}</span>
        )}
        <span className="text-small text-ink-2">{S.review.alignmentEntities(item.entity_count)}</span>
      </div>
      {item.examples.length > 0 && (
        <div className="mt-1 text-small text-ink-2">{item.examples.join(" · ")}</div>
      )}
      {item.phrases.length > 0 && (
        <div className="mt-1 text-small text-ink-2">{item.phrases.map((p) => `—${p}→`).join("  ")}</div>
      )}
      <div className="mt-1 text-small text-ink-2">
        {S.review.alignmentVotes(voteText(first), voteText(second))}
      </div>
      <div className={CARD_ACTIONS}>
        <SearchSelect
          size="sm"
          className="w-56"
          value={cls}
          onChange={setCls}
          options={[
            { value: "", label: S.review.alignmentNone },
            ...classes.map((c) => ({ value: c.key, label: c.label })),
          ]}
        />
        <Button size="sm" disabled={busy} onClick={() => onDecide(cls || null)}>
          {cls ? S.review.alignmentBind : S.review.alignmentLeaveOpen}
        </Button>
      </div>
    </div>
  );
}

function DefectRow({
  defect: d,
  busy,
  onDecide,
}: {
  defect: OntologyDefect;
  busy: boolean;
  onDecide: (resolution: "fixed" | "accepted") => void;
}) {
  const what = {
    symmetric_and_asymmetric: S.review.defectSymAsym,
    transitive_and_functional: S.review.defectTransFunc,
    subclass_cycle: S.review.defectCycle,
    disjoint_with_ancestor: S.review.defectDisjointAncestor,
    inherits_disjoint: S.review.defectInheritsDisjoint,
    inverse_of_itself: S.review.defectInverseSelf,
    inverse_not_mutual: S.review.defectInverseNotMutual,
    sub_property_cycle: S.review.defectSubPropertyCycle,
    rules_disagree: S.review.defectRulesDisagree,
  }[d.kind];
  const rules = d.kind === "rules_disagree" ? (d.detail.rules ?? []) : [];
  // The consequences of the latter two classes are worth writing out: an unsatisfiable class raises no error, it just stays empty forever
  const unsatisfiable =
    d.kind === "disjoint_with_ancestor" || d.kind === "inherits_disjoint";
  return (
    <div className="glass rounded-panel p-3">
      <div className="flex items-baseline gap-2 flex-wrap">
        <span className="text-body text-danger">{what}</span>
        {d.subject_label && (
          <span className="text-small text-ink-2">{d.subject_label}</span>
        )}
        {d.other_label && (
          <span className="text-small text-ink-2">↔ {d.other_label}</span>
        )}
      </div>
      {d.path_labels.length > 0 && (
        <div className="mt-1 text-small text-ink-2">
          {d.path_labels.join(" → ")} → {d.path_labels[0]}
        </div>
      )}
      {unsatisfiable && (
        <p className="mt-1 text-small text-ink-2">
          {S.review.defectNeverInstantiable}
        </p>
      )}
      {rules.length > 0 && (
        <div className="mt-1 space-y-1 text-small text-ink-2">
          <div>{S.review.rulesDisagreeCount(d.detail.count ?? 0)}</div>
          {rules.map((r, i) => (
            <div key={i}>
              <div className="text-ink-2">
                {S.review.rulesDisagreeRule(r.rule_a, r.via_a, r.rule_b, r.via_b, r.axiom)}
              </div>
              {r.examples.map(([x, y], j) => (
                <div key={j} className="pl-3 text-ink-2">
                  {x} <span className="text-ink-2">·</span> {y}
                </div>
              ))}
            </div>
          ))}
        </div>
      )}
      <div className={CARD_ACTIONS}>
        <Button variant="secondary" size="sm"
          disabled={busy}
          onClick={() => onDecide("accepted")}
        >
          {S.review.defectAccepted}
        </Button>
        <Button variant="primary" size="sm"
          disabled={busy}
          onClick={() => onDecide("fixed")}
        >
          {S.review.defectFixed}
        </Button>
      </div>
    </div>
  );
}

/** An axiom violation. **Three buttons, not two**: the third is this tier's exclusive exit:
 *  the contradiction may lie in the definition (the user-imported ontology declares some property antisymmetric, while in his corpus
 *  that relation is in fact bidirectional); then what should change is the ontology, not twenty facts. */
/** Extra parameters of the verdict: the closing date (fact_closed), which one to retract (fact_retracted) */
type DecideOpts = { closeAt?: string; closeAtPrecision?: string; factId?: string };

function ViolationRow({
  violation: v,
  busy,
  onDecide,
  onDuplicates,
  onOntology,
}: {
  violation: AxiomViolation;
  busy: boolean;
  onDecide: (resolution: ViolationResolution, opts?: DecideOpts) => void;
  onDuplicates: () => void;
  onOntology: () => void;
}) {
  const what = {
    self_loop: S.review.violationSelfLoop,
    asymmetry: S.review.violationAsymmetry,
    cycle: S.review.violationCycle,
    functional: S.review.violationFunctional,
    inverse_functional: S.review.violationInverseFunctional,
    signature: S.review.violationSignature,
    derived_contradiction: S.review.violationDerived,
  }[v.kind];
  if (v.kind === "derived_contradiction") {
    return <ContradictionRow {...{ v, what, busy, onDecide, onDuplicates, onOntology }} />;
  }
  // For reflexive kinds, the two facts are the same one: showing once is enough; showing twice looks like a bug
  const single = v.left_fact === v.right_fact;
  // 'The data is wrong' needs to retract a specific one: cycles are listed one by one, the two facts of a double fact each get a button, single facts need not ask (#202)
  const facts =
    v.path.length > 0
      ? v.path
      : single
        ? [{ id: v.left_fact, text: v.left_text }]
        : [
            { id: v.left_fact, text: v.left_text },
            { id: v.right_fact, text: v.right_text },
          ];
  return (
    <div className="glass rounded-panel p-3">
      <div className="flex items-baseline gap-2 flex-wrap">
        <span className="text-body text-warn">{what}</span>
        {v.predicate && (
          <span className="text-fine text-ink-2">
            {S.review.violationVia(v.predicate)}
          </span>
        )}
        {v.kind === "cycle" && v.path_len > 0 && (
          <span className="text-fine text-ink-2">
            {S.review.violationPath(v.path_len)}
          </span>
        )}
      </div>
      <div className="mt-2 space-y-1">
        {facts.map((f) => (
          <div key={f.id} className="flex items-center gap-2">
            <span className="text-small text-ink-2 min-w-0 flex-1">{f.text}</span>
            {!single && (
              <Button variant="secondary" size="sm" className="shrink-0"
                disabled={busy}
                title={S.review.retractThisHint}
                onClick={() => onDecide("fact_retracted", { factId: f.id })}
              >
                {S.review.retractThis}
              </Button>
            )}
          </div>
        ))}
      </div>
      <div className={CARD_ACTIONS}>
        <Button variant="secondary" size="sm"
          disabled={busy}
          onClick={() => onDecide("accepted")}
        >
          {S.review.acceptBoth(facts.length)}
        </Button>
        <Button variant="secondary" size="sm"
          disabled={busy}
          onClick={() => onDecide("axiom_relaxed")}
        >
          {S.review.relaxAxiom}
        </Button>
        {single && (
          <Button variant="primary" size="sm"
            disabled={busy}
            onClick={() => onDecide("fact_retracted", { factId: v.left_fact })}
          >
            {S.review.retractFact}
          </Button>
        )}
      </div>
    </div>
  );
}

/**
 * A derivation colliding with an assertion (0017): the card is one review, and the clue points to an upstream error: the old assertion should close,
 * two same-named entities are in fact one, and extraction was never certain. The fix is on the card; the endpoint executes it on the person's behalf.
 */
function ContradictionRow({
  v,
  what,
  busy,
  onDecide,
  onDuplicates,
  onOntology,
}: {
  v: AxiomViolation;
  what: string;
  busy: boolean;
  onDecide: (resolution: ViolationResolution, opts?: DecideOpts) => void;
  onDuplicates: () => void;
  onOntology: () => void;
}) {
  const [closeAt, setCloseAt] = useState("");
  const d = v.detail;
  const hint =
    v.hint === "stale"
      ? S.review.hintStale
      : v.hint === "duplicate"
        ? S.review.hintDuplicate
        : v.hint === "unsure"
          ? S.review.hintUnsure
          : S.review.hintReadBoth;
  return (
    <div className="glass rounded-panel p-3 border border-[color-mix(in_srgb,var(--u-contest)_35%,transparent)]">
      <div className="flex items-baseline gap-2 flex-wrap">
        <span className="text-body text-contest">{what}</span>
        {v.predicate && (
          <span className="text-fine text-ink-2">
            {S.review.violationVia(v.predicate)}
          </span>
        )}
      </div>
      <div className="mt-2 space-y-1">
        <div className="text-small text-ink-2">
          {S.review.derivedLine(d.subject ?? "?", d.predicate ?? "?", d.object ?? "?")}
          {d.rule && d.via_label && (
            <span className="ml-2 text-ink-2">
              {S.review.derivedBy(d.rule, d.via_label)}
            </span>
          )}
        </div>
        <div className="text-small text-ink-2">
          {S.review.assertedLine(v.left_text)}
        </div>
      </div>
      <p className="mt-2 text-small text-ink-2">{hint}</p>
      <div className={CARD_ACTIONS}>
        {/* Do not use a date picker: it forces a day, but 'ended that year' is the common answer here.
            the number of digits written is the precision (time.ts) */}
        <Input size="sm" className="u-num w-28 text-center"
          placeholder={S.review.closeAtPlaceholder}
          value={closeAt}
          title={S.review.closeAssertion}
          onChange={(e) => setCloseAt(e.target.value)}
        />
        <Button variant="primary" size="sm"
          disabled={busy || !parseDateInput(closeAt)}
          onClick={() => {
            const parsed = parseDateInput(closeAt);
            if (parsed) {
              onDecide("fact_closed", {
                closeAt: parsed.iso,
                closeAtPrecision: parsed.precision,
              });
            }
          }}
        >
          {S.review.closeAssertion}
        </Button>
        <Button variant="secondary" size="sm"
          disabled={busy}
          onClick={() => onDecide("fact_retracted")}
        >
          {S.review.retractAssertion}
        </Button>
        <Button variant="secondary" size="sm"
          disabled={busy}
          onClick={onDuplicates}
        >
          {S.review.seeDuplicates}
        </Button>
        <Button variant="secondary" size="sm"
          disabled={busy}
          onClick={onOntology}
        >
          {S.review.openOntology}
        </Button>
        <Button variant="secondary" size="sm"
          disabled={busy}
          onClick={() => onDecide("accepted")}
        >
          {S.review.letBothStand}
        </Button>
      </div>
    </div>
  );
}

/* ---------- Page: left-column categories + the content area of one category ---------- */

type Sel =
  // Overview (#377): the landing page. It answers 'how many are waiting, how long they have waited, is the queue shrinking or growing',
  // not any one queue tier
  | "overview"
  // Facts extracted from memory, awaiting a nod (0015). First: they are the person's own words,
  // and the things in this tier **are not yet in the graph**. Every other tier reviews things already on the graph
  | "pending"
  | "duplicates"
  | "conflicts"
  | "unconfirmed"
  | "lowconf"
  // Axiom violations (0002 R0). **Separate from conflicts**: that tier asks 'which one is right',
  // this tier may also answer 'the axiom was written wrong'. The exits differ
  | "violations"
  // The ontology's own self-contradiction. **Separate from violations**: that tier looks at facts; this tier looks only at definitions
  | "defects"
  // Signatures and category words where the two aligner votes disagree (#725, 0044 decision 3): the question is 'which property of the ontology does this wording belong to'
  | "alignment"
  // Actions of the erratum agent that the gate blocked (0044 decision 7)
  | "errata"
  // Every entry of the agent (0025): suggestions wait for a human answer, auto-decided ones can be undone. It is not one of the seven tiers:
  // the seven tiers ask 'is this knowledge right', this tier asks 'is what the machine did on your behalf right'
  | "agent"
  | "decisions"
  | "merges";

/** The tiers that page server-side (the decision ledger has its own endpoint) */
const QUEUE_FETCHED: ReviewQueue[] = [
  "pending",
  "duplicates",
  "conflicts",
  "unconfirmed",
  "lowconf",
  "violations",
  "defects",
  "alignment",
  "errata",
  "merges",
  "agent",
];

const QUEUE_ORDER: Sel[] = [
  "pending",
  "duplicates",
  "conflicts",
  "unconfirmed",
  "lowconf",
  "violations",
  "defects",
  "alignment",
  "errata",
];
/** The tiers with a content area that page through; the overview does not page */
type Paged = Exclude<Sel, "overview">;

const PAGE_SIZE: Record<Paged, number> = {
  pending: FACT_PAGE,
  duplicates: DUP_PAGE,
  conflicts: CONFLICT_PAGE,
  unconfirmed: FACT_PAGE,
  lowconf: FACT_PAGE,
  violations: FACT_PAGE,
  defects: FACT_PAGE,
  alignment: FACT_PAGE,
  errata: FACT_PAGE,
  merges: MERGE_PAGE,
  decisions: 20,
  agent: 20,
};

function RailHeader({ label }: { label: string }) {
  return (
    // Text starts at 20, on the same line as the icons in the row (box 12 + inline 8)
    <GroupLabel className="mx-2 px-3 pt-4 pb-2">{label}</GroupLabel>
  );
}


export function Review() {
  const kbId = useKbId();
  const { kb } = useKb();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  // The panel's dispute chip carries queue / item when it jumps here: land on that tier first, then light up that card
  const search = useSearch({ from: "/app/kb/$kbId/review" });
  // Deep links recognize the seven tiers and the agent's tier (0025's alerts point here); anything else lands on the overview
  const [sel, setSel] = useState<Sel | null>(
    QUEUE_ORDER.includes(search.queue as Sel) || search.queue === "agent"
      ? (search.queue as Sel)
      : null,
  );
  const [page, setPage] = useState(0);
  // Type filter and batch selection for duplicates (#428). The selected set is cleared per page: when paging, switching tiers, or switching filters,
  // anything still checked is no longer in view, and keeping it would let 'merge selected' merge things you cannot see
  const [types, setTypes] = useState<ReviewTypeFilter>("any");
  const [picked, setPicked] = useState<Set<string>>(() => new Set());
  const [batchWhy, setBatchWhy] = useState("");
  useEffect(() => setPicked(new Set()), [page, sel, types]);

  // Queue changes are pushed through the SSE event stream (useKbEvents mounted in Shell); no polling needed.
  //
  // **Fetch by tier + page number**: previously all eight queues came back in one go, 100 items each, paginated on the client,
  // so the badge in the left column was a truncated number, and things past page eleven did not exist on screen. Now the count
  // comes back every time (server-side COUNT, unaffected by how many items a page has), and content returns only one page of the current tier.
  const queueSel: ReviewQueue = QUEUE_FETCHED.includes(
    (sel ?? "duplicates") as ReviewQueue,
  )
    ? ((sel ?? "duplicates") as ReviewQueue)
    : "duplicates";
  const typesForQuery = queueSel === "duplicates" ? types : "any";
  const review = useQuery({
    queryKey: ["review", kb?.id, queueSel, page, typesForQuery],
    queryFn: () =>
      api.review(
        kb!.id,
        queueSel,
        PAGE_SIZE[queueSel as Paged],
        page * PAGE_SIZE[queueSel as Paged],
        typesForQuery,
      ),
    enabled: !!kb,
    // When paging, do not flash the previous page into a blank: the count and skeleton are still there, only the items change
    placeholderData: (prev) => prev,
  });
  // Decision ledger: server-side paging, fetched only when selected
  const history = useQuery({
    queryKey: ["reviewHistory", kb?.id, page],
    queryFn: () => api.reviewHistory(kb!.id, page),
    enabled: !!kb && sel === "decisions",
  });

  // Overview: pulled only when it is landed on; after every decision it is invalidated along with it. It counts exactly these things
  const summary = useQuery({
    queryKey: ["reviewSummary", kb?.id],
    queryFn: () => api.reviewSummary(kb!.id),
    enabled: !!kb && (sel ?? "overview") === "overview",
  });

  const invalidate = () => {
    queryClient.invalidateQueries({ queryKey: ["review", kb?.id] });
    queryClient.invalidateQueries({ queryKey: ["reviewSummary", kb?.id] });
    queryClient.invalidateQueries({ queryKey: ["reviewHistory", kb?.id] });
    queryClient.invalidateQueries({ queryKey: ["graph"] });
  };

  const decide = useMutation({
    mutationFn: ({
      id,
      action,
      rationale,
    }: {
      id: string;
      action: "merge" | "keep";
      rationale?: string;
    }) => api.decideReview(kb!.id, id, action, rationale),
    onSettled: invalidate,
  });
  // An answer from the agent (0025): goes through the human decision path, and becomes the precedent for the next round
  const agentAnswer = useMutation({
    mutationFn: ({
      id,
      action,
      rationale,
    }: {
      id: string;
      action: "merge" | "keep" | "revert";
      rationale?: string;
    }) => api.agentAnswer(kb!.id, id, action, rationale),
    onError: (e) => toast.error((e as Error).message),
    onSettled: invalidate,
  });
  // Batch decisions: one action per batch; on return, each is reported as done or not; those not done stay in the list, those done disappear
  const batch = useMutation({
    mutationFn: ({
      ids,
      action,
      rationale,
    }: {
      ids: string[];
      action: "merge" | "keep";
      rationale?: string;
    }) => api.reviewBatch(kb!.id, ids, action, rationale),
    onSuccess: (r) => {
      const failed = r.outcomes.filter((o) => o.error).length;
      if (failed > 0) toast.error(S.review.batchDone(r.decided, failed));
      else toast.success(S.review.batchDone(r.decided, 0));
    },
    onError: (e) => toast.error((e as Error).message),
    onSettled: () => {
      setPicked(new Set());
      setBatchWhy("");
      invalidate();
    },
  });
  const factAction = useMutation({
    mutationFn: ({
      id,
      action,
    }: {
      id: string;
      action: "confirm" | "reject";
    }) =>
      action === "confirm"
        ? api.confirmFact(kb!.id, id)
        : api.rejectFact(kb!.id, id),
    onSettled: invalidate,
  });
  // Facts awaiting a nod (0015): confirming lands them in the ledger; rejecting is recorded in rejected_facts
  // Nodding is an action that writes the graph: a Viewer sees the proposal but not the buttons (same threshold as the server's Editor check)
  const canDecidePending = useCanDecide(kb?.id);
  const pendingAction = useMutation({
    mutationFn: ({
      id,
      action,
    }: {
      id: string;
      action: "confirm" | "reject";
    }) => api.decidePending(kb!.id, id, action),
    onSettled: () => {
      invalidate();
      queryClient.invalidateQueries({ queryKey: ["pending", kb?.id] });
    },
  });
  const defectAction = useMutation({
    mutationFn: ({
      id,
      resolution,
    }: {
      id: string;
      resolution: "fixed" | "accepted";
    }) => api.decideDefect(kb!.id, id, resolution),
    onSettled: invalidate,
  });
  const alignmentPhraseAction = useMutation({
    mutationFn: ({
      id,
      property,
      direction,
      marks,
    }: {
      id: string;
      property: string | null;
      direction: "forward" | "reverse";
      marks: AlignmentMarks | null;
    }) => api.decideAlignmentPhrase(kb!.id, id, property, direction, marks),
    // 202// 202: the judgment was accepted; the typed graph is recomputed in the background. When done, the `review` / `graph` events refresh
    // the queue and graph; here we only tell the person 'saved', without making up a number
    onSuccess: () => toast.success(S.review.alignmentAccepted),
    onSettled: invalidate,
  });
  const alignmentRuleAction = useMutation({
    mutationFn: ({ id, approve }: { id: string; approve: boolean }) =>
      api.decideAlignmentRule(kb!.id, id, approve),
    onSuccess: () => toast.success(S.review.alignmentRuleAccepted),
    onSettled: invalidate,
  });
  const alignmentKindWordAction = useMutation({
    mutationFn: ({ kindWord, cls }: { kindWord: string; cls: string | null }) =>
      api.decideAlignmentKindWord(kb!.id, kindWord, cls),
    onError: (e) => toast.error(
      alignmentErrorMessage(e),
    ),
    onSettled: invalidate,
  });
  const errataAction = useMutation({
    mutationFn: ({ id, approve }: { id: string; approve: boolean }) =>
      api.decideErrata(kb!.id, id, approve),
    onSuccess: () => toast.success(S.review.errataDecided),
    onError: (e) => toast.error(e instanceof Error ? e.message : String(e)),
    onSettled: invalidate,
  });
  const violationAction = useMutation({
    mutationFn: ({
      id,
      resolution,
      opts,
    }: {
      id: string;
      resolution: ViolationResolution;
      opts?: DecideOpts;
    }) => api.decideViolation(kb!.id, id, resolution, opts),
    onError: (e) => toast.error(e instanceof Error ? e.message : String(e)),
    onSettled: invalidate,
  });
  // The check is a synchronous pure computation, so mutate directly without queuing. After it runs, leave the report next to the button:
  // **Zero and zero are not the same**: with no axioms, say 'nothing to judge against', not 'no contradictions found'
  const runCheck = useMutation({
    mutationFn: () => api.runConsistencyCheck(kb!.id),
    onSettled: invalidate,
  });
  const revert = useMutation({
    mutationFn: (mergeId: string) => api.revertMerge(kb!.id, mergeId),
    onSettled: invalidate,
  });
  const conflictAction = useMutation({
    mutationFn: ({
      id,
      action,
      closeAt,
      closeAtPrecision,
    }: {
      id: string;
      action: "close" | "retime_new" | "keep" | "reject_new";
      closeAt?: string;
      closeAtPrecision?: string;
    }) =>
      api.resolveConflict(kb!.id, id, {
        action,
        close_at: action === "close" ? closeAt : undefined,
        close_at_precision: action === "close" ? closeAtPrecision : undefined,
        new_from: action === "retime_new" ? closeAt : undefined,
        new_from_precision: action === "retime_new" ? closeAtPrecision : undefined,
      }),
    onError: (e) => toast.error(e instanceof Error ? e.message : String(e)),
    onSettled: invalidate,
  });

  const closeFactAction = useMutation({
    mutationFn: ({
      id,
      validTo,
      precision,
    }: {
      id: string;
      validTo: string;
      precision: string;
    }) => api.closeFact(kb!.id, id, validTo, precision),
    onError: (e) => toast.error(e instanceof Error ? e.message : String(e)),
    onSettled: invalidate,
  });

  // **The badge reads the server's COUNT, not the list length.** This is the root of the earlier 'the library has 164, the screen says
  // 100// 100': array length reflects how many items a page holds, not how many are in the library.
  const c = review.data?.counts;
  // mappings is not a tier on this page (approval is on the 'data mappings' page), but the count is still collected:
  // the inbox should say 'how many are waiting for you'
  const counts: Record<Sel | "mappings", number> = {
    overview: 0,
    pending: c?.pending ?? 0,
    duplicates: c?.duplicates ?? 0,
    conflicts: c?.conflicts ?? 0,
    unconfirmed: c?.unconfirmed ?? 0,
    lowconf: c?.lowconf ?? 0,
    mappings: c?.mappings ?? 0,
    violations: c?.violations ?? 0,
    defects: c?.defects ?? 0,
    alignment: c?.alignment ?? 0,
    errata: c?.errata ?? 0,
    merges: c?.merges ?? 0,
    decisions: history.data?.total ?? 0,
    agent: c?.agent ?? 0,
  };
  const duplicateTotals: Record<ReviewTypeFilter, number> = {
    any: counts.duplicates,
    same: c?.duplicates_same_type ?? 0,
    conflict: c?.duplicates_type_conflict ?? 0,
  };
  // The current tier's one page. **The server has already cut it**; here we only narrow the type per tier:
  // if narrowing is wrong it shows up at render time, rather than quietly displaying an empty list
  const rows = review.data?.queue === queueSel ? (review.data.items ?? []) : [];
  const asPending = () => rows as PendingFactItem[];
  const asDuplicates = () => rows as ReviewItem[];
  const asFacts = () => rows as FactReviewItem[];
  const asConflicts = () => rows as ConflictItem[];
  const asViolations = () => rows as AxiomViolation[];
  const asDefects = () => rows as OntologyDefect[];
  const asAlignment = () => rows as AlignmentItem[];
  const asErrata = () => rows as ErrataItem[];
  // The alignment card must list the ontology's classes and properties for a person to choose from; fetched only on this tier
  const ontology = useQuery({
    queryKey: ["ontology", kb?.id],
    queryFn: () => api.ontology(kb!.id),
    enabled: !!kb && queueSel === "alignment",
  });
  const asMerges = () => rows as MergeLog[];
  const asAgent = () => rows as AgentDecision[];
  // The pair an agent is adjudicating (0025): the task is running, and this pair is marked adjudicating. Skip them when batch-selecting a page
  const lockedByAgent = (item: ReviewItem) =>
    !!c?.agent_running && item.stage === "adjudicating";
  const selectable = () => asDuplicates().filter((d) => !lockedByAgent(d));
  const queueEmpty = QUEUE_ORDER.every((k) => counts[k] === 0);

  // Landing without ?queue= falls on the overview. It used to be 'the first non-empty queue', which effectively decided for the person
  // which tier to look at first; now it gives the full picture first, and the person picks which tier to handle first
  const select = (s: Sel) => {
    setSel(s);
    setPage(0);
  };

  const active: Sel = sel ?? "overview";
  const isQueueSel = QUEUE_ORDER.includes(active);

  const SECTION: Record<Sel, { title: string; hint: string | null }> = {
    overview: { title: S.review.overviewTitle, hint: S.review.overviewHint },
    pending: { title: S.review.pending, hint: S.review.pendingHint },
    duplicates: { title: S.review.duplicates, hint: S.review.duplicatesHint },
    conflicts: { title: S.review.conflicts, hint: S.review.conflictsHint },
    unconfirmed: {
      title: S.review.unconfirmed,
      hint: S.review.unconfirmedHint,
    },
    lowconf: {
      title: S.review.lowConfidence,
      hint: S.review.lowConfidenceHint,
    },
    violations: {
      title: S.review.violations,
      hint: S.review.violationsHint,
    },
    defects: { title: S.review.defects, hint: S.review.defectsHint },
    alignment: { title: S.review.alignment, hint: S.review.alignmentHint },
    errata: { title: S.review.errata, hint: S.review.errataHint },
    decisions: { title: S.review.decisionsTitle, hint: S.review.decisionsHint },
    merges: { title: S.review.mergeHistory, hint: null },
    agent: { title: S.review.agentTitle, hint: S.review.agentHint },
  };

  return (
    <div className="h-full flex">
      {/* Left column: queue categories + history, each with live counts (SSE drives the refresh) */}
      {/* `overflow-y-auto`: on a short window this column's content is taller than it, and the bottom item is the exit to 'handle elsewhere'.
          the exit to elsewhere. Without scrolling it would be cut off and unreachable. `mt-auto` only pushes it to the bottom when there is spare room;
          the two must be given together */}
      <aside className={`${RAIL_CLS} flex flex-col overflow-y-auto u-scroll`}>
        {/* The overview is at the top, with the seven queues directly below it and no separate heading: the word 'queues'
            says what they are, and what people want is how many there are */}
        <div className="u-rail-list px-2 pt-3">
          <RailItem
            active={active === "overview"}
            icon={<LayoutDashboard size={14} />}
            onClick={() => select("overview")}
          >
            {S.review.railOverview}
          </RailItem>
        </div>
        <div className="u-rail-list px-2 pt-1">
          <RailItem
            active={active === "pending"}
            count={counts.pending}
            onClick={() => select("pending")}
          >
            {S.review.railPending}
          </RailItem>
          <RailItem
            active={active === "duplicates"}
            count={counts.duplicates}
            onClick={() => select("duplicates")}
          >
            {S.review.railDuplicates}
          </RailItem>
          <RailItem
            active={active === "conflicts"}
            count={counts.conflicts}
            onClick={() => select("conflicts")}
          >
            {S.review.railConflicts}
          </RailItem>
          <RailItem
            active={active === "unconfirmed"}
            count={counts.unconfirmed}
            onClick={() => select("unconfirmed")}
          >
            {S.review.railUnconfirmed}
          </RailItem>
          <RailItem
            active={active === "lowconf"}
            count={counts.lowconf}
            onClick={() => select("lowconf")}
          >
            {S.review.railLowConfidence}
          </RailItem>
          <RailItem
            active={active === "violations"}
            count={counts.violations}
            onClick={() => select("violations")}
          >
            {S.review.railViolations}
          </RailItem>
          <RailItem
            active={active === "defects"}
            count={counts.defects}
            onClick={() => select("defects")}
          >
            {S.review.railDefects}
          </RailItem>
          <RailItem
            active={active === "alignment"}
            count={counts.alignment}
            onClick={() => select("alignment")}
          >
            {S.review.railAlignment}
          </RailItem>
          <RailItem
            active={active === "errata"}
            count={counts.errata}
            onClick={() => select("errata")}
          >
            {S.review.railErrata}
          </RailItem>
          {/* agent {/* The agent's queue (0025): the badge is the number of suggestions awaiting a human answer */}
          <RailItem
            active={active === "agent"}
            count={counts.agent}
            dot={c?.agent_running ? "bg-warn animate-pulse" : undefined}
            onClick={() => select("agent")}
          >
            {S.review.railAgent}
          </RailItem>
        </div>
        <RailHeader label={S.review.tabHistory} />
        <div className="u-rail-list px-2">
          <RailItem
            active={active === "decisions"}
                        onClick={() => select("decisions")}
          >
            {S.review.railDecisions}
          </RailItem>
          <RailItem
            active={active === "merges"}
            count={counts.merges}
            onClick={() => select("merges")}
          >
            {S.review.railMerges}
          </RailItem>
        </div>

        {/* Data mappings: **neither group belongs, so it sits alone at the bottom.**
            The seven tiers above all ask 'is this knowledge right', while mappings ask 'how is this number computed'
            (0011 already separated it out at the data layer); the two tiers below are a log of what was done on this page,
            and decisions about mappings never enter `review_history` (it picks up only review./fact./
            conflict./merge., while mappings record mapping.decided).
            **The count stays**: the inbox should say 'how many are waiting for you', but work it on the page that has the context */}
        <div className="mt-auto border-t border-line px-2 py-2">
          <RailItem
            active={false}
            count={counts.mappings}
            onClick={() =>
              navigate({ to: "/kb/$kbId/mappings", params: { kbId } })
            }
            external
          >
            {S.review.railMappings}
          </RailItem>
        </div>
      </aside>

      {/* Right side: only one tier shown at a time, single pagination */}
      <div className="flex-1 min-w-0 overflow-y-auto u-scroll px-8 py-6">
        <div>
          {review.isPending && (
            <p className="text-body text-ink-2">{S.nav.loading}</p>
          )}
          {review.isError && (
            <p className="text-body text-danger">
              {(review.error as Error).message}
            </p>
          )}

          {review.data && (
            <section>
              <PageHeader title={SECTION[active].title} sub={SECTION[active].hint} />

              {/* Empty state: whole to-do cleared vs one tier cleared. **The axiom tier is excluded**: its own sentence needs
                  to distinguish 'checked, no contradiction' from 'not yet checked', which the generic empty state cannot express */}
              {isQueueSel &&
                active !== "violations" &&
                active !== "defects" &&
                counts[active] === 0 && (
                  <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
                    {queueEmpty ? S.review.empty : S.review.categoryEmpty}
                  </div>
                )}

              {active === "overview" &&
                (summary.isPending ? (
                  <p className="text-body text-ink-2">{S.nav.loading}</p>
                ) : summary.isError ? (
                  <p className="text-body text-danger">
                    {(summary.error as Error).message}
                  </p>
                ) : summary.data ? (
                  <ReviewOverview
                    summary={summary.data}
                    governance={kb?.governance ?? false}
                    onPick={select}
                    onSettings={() =>
                      navigate({ to: "/kb/$kbId/settings", params: { kbId } })
                    }
                  />
                ) : null)}

              {active === "duplicates" && counts.duplicates > 0 && (
                <div className="space-y-3">
                  {/* Filter relations by the types at both ends (#428): the same-name-same-type tier is what people most want to
                      merge at a glance, the type-conflict tier must never be merged; each of the three tiers carries its real count */}
                  <div className="flex flex-wrap items-center gap-3">
                    <Segmented
                      size="sm"
                      value={types}
                      onChange={(v) => {
                        setTypes(v);
                        setPage(0);
                      }}
                      options={[
                        { value: "any", label: S.review.typesAny, count: duplicateTotals.any },
                        {
                          value: "same",
                          label: S.review.typesSame,
                          count: duplicateTotals.same,
                        },
                        {
                          value: "conflict",
                          label: S.review.typesConflict,
                          count: duplicateTotals.conflict,
                        },
                      ]}
                    />
                    {/* Batch: tick this page's items, one action decides a batch. The buttons appear only when something is selected */}
                    <Checkbox
                      className="ml-auto"
                      checked={
                        selectable().length > 0 &&
                        selectable().every((d) => picked.has(d.id))
                      }
                      onChange={(v) =>
                        setPicked(
                          v
                            ? new Set(selectable().map((d) => d.id))
                            : new Set(),
                        )
                      }
                      label={S.review.selectPage}
                    />
                    {picked.size > 0 && (
                      <>
                        <span className="u-num text-small text-ink-2">
                          {S.review.selected(picked.size)}
                        </span>
                        {/* One reason per batch (0026): why this batch was decided together */}
                        <Input
                          size="sm"
                          className="w-44"
                          placeholder={S.review.rationalePlaceholder}
                          value={batchWhy}
                          disabled={batch.isPending}
                          onChange={(e) => setBatchWhy(e.target.value)}
                        />
                        <Button
                          variant="secondary"
                          size="sm"
                          disabled={batch.isPending}
                          onClick={() =>
                            batch.mutate({
                              ids: [...picked],
                              action: "keep",
                              rationale: batchWhy,
                            })
                          }
                        >
                          {S.review.keepSelected}
                        </Button>
                        <Button
                          variant="primary"
                          size="sm"
                          disabled={batch.isPending}
                          onClick={() =>
                            batch.mutate({
                              ids: [...picked],
                              action: "merge",
                              rationale: batchWhy,
                            })
                          }
                        >
                          {S.review.mergeSelected}
                        </Button>
                      </>
                    )}
                  </div>
                  {asDuplicates().length === 0 && (
                    <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
                      {S.review.typesEmpty}
                    </div>
                  )}
                  {asDuplicates().map((item) => (
                    <DuplicateCard
                      key={item.id}
                      item={item}
                      locked={lockedByAgent(item)}
                      picked={picked.has(item.id)}
                      onPick={(on) =>
                        setPicked((prev) => {
                          const next = new Set(prev);
                          if (on) next.add(item.id);
                          else next.delete(item.id);
                          return next;
                        })
                      }
                      busy={
                        (decide.isPending && decide.variables?.id === item.id) ||
                        (batch.isPending && picked.has(item.id))
                      }
                      onDecide={(action, rationale) =>
                        decide.mutate({ id: item.id, action, rationale })
                      }
                    />
                  ))}
                </div>
              )}

              {active === "conflicts" && counts.conflicts > 0 && (
                <div className="space-y-3">
                  {asConflicts().map((c) => (
                    <ConflictRow
                      key={c.id}
                      conflict={c}
                      busy={
                        conflictAction.isPending &&
                        conflictAction.variables?.id === c.id
                      }
                      onResolve={(action, closeAt, closeAtPrecision) =>
                        conflictAction.mutate({
                          id: c.id,
                          action,
                          closeAt,
                          closeAtPrecision,
                        })
                      }
                    />
                  ))}
                </div>
              )}

              {active === "unconfirmed" && counts.unconfirmed > 0 && (
                <div className="space-y-3">
                  {asFacts().map((fact) => (
                    <UnconfirmedRow
                      key={fact.id}
                      fact={fact}
                      busy={
                        (factAction.isPending &&
                          factAction.variables?.id === fact.id) ||
                        (closeFactAction.isPending &&
                          closeFactAction.variables?.id === fact.id)
                      }
                      onReject={() =>
                        factAction.mutate({ id: fact.id, action: "reject" })
                      }
                      onClose={(validTo, precision) =>
                        closeFactAction.mutate({ id: fact.id, validTo, precision })
                      }
                    />
                  ))}
                </div>
              )}

              {active === "pending" && counts.pending > 0 && (
                <div className="space-y-3">
                  {asPending().map((fact) => (
                    <PendingFactRow
                      key={fact.id}
                      fact={fact}
                      canDecide={canDecidePending}
                      busy={
                        pendingAction.isPending &&
                        pendingAction.variables?.id === fact.id
                      }
                      onConfirm={() =>
                        pendingAction.mutate({ id: fact.id, action: "confirm" })
                      }
                      onReject={() =>
                        pendingAction.mutate({ id: fact.id, action: "reject" })
                      }
                    />
                  ))}
                </div>
              )}

              {active === "lowconf" && counts.lowconf > 0 && (
                <div className="space-y-3">
                  {asFacts().map((fact) => (
                    <FactRow
                      key={fact.id}
                      fact={fact}
                      busy={
                        factAction.isPending &&
                        factAction.variables?.id === fact.id
                      }
                      onConfirm={() =>
                        factAction.mutate({ id: fact.id, action: "confirm" })
                      }
                      onReject={() =>
                        factAction.mutate({ id: fact.id, action: "reject" })
                      }
                    />
                  ))}
                </div>
              )}

              {active === "defects" && (
                <div className="space-y-3">
                  {counts.defects === 0 && (
                    <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
                      {S.review.categoryEmpty}
                    </div>
                  )}
                  {asDefects().map((d) => (
                    <DefectRow
                      key={d.id}
                      defect={d}
                      busy={
                        defectAction.isPending &&
                        defectAction.variables?.id === d.id
                      }
                      onDecide={(resolution) =>
                        defectAction.mutate({ id: d.id, resolution })
                      }
                    />
                  ))}
                </div>
              )}

              {active === "errata" && (
                <div className="space-y-3">
                  {counts.errata === 0 && (
                    <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
                      {S.review.categoryEmpty}
                    </div>
                  )}
                  {asErrata().map((item) => (
                    <ErrataRow
                      key={item.id}
                      item={item}
                      busy={errataAction.isPending && errataAction.variables?.id === item.id}
                      onDecide={(approve) => errataAction.mutate({ id: item.id, approve })}
                    />
                  ))}
                </div>
              )}

              {active === "alignment" && (
                <div className="space-y-3">
                  {counts.alignment === 0 && (
                    <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
                      {S.review.categoryEmpty}
                    </div>
                  )}
                  {asAlignment().map((item) =>
                    item.kind === "phrase" ? (
                      <AlignmentPhraseRow
                        key={item.id}
                        item={item}
                        properties={ontology.data?.relation_types ?? []}
                        busy={
                          alignmentPhraseAction.isPending &&
                          alignmentPhraseAction.variables?.id === item.id
                        }
                        onDecide={(property, direction, marks) =>
                          alignmentPhraseAction.mutate({ id: item.id, property, direction, marks })
                        }
                      />
                    ) : item.kind === "rule" ? (
                      <AlignmentRuleRow
                        key={item.id}
                        item={item}
                        busy={
                          alignmentRuleAction.isPending &&
                          alignmentRuleAction.variables?.id === item.id
                        }
                        onDecide={(approve) => alignmentRuleAction.mutate({ id: item.id, approve })}
                      />
                    ) : (
                      <AlignmentKindWordRow
                        key={item.kind_word}
                        item={item}
                        classes={ontology.data?.entity_types ?? []}
                        busy={
                          alignmentKindWordAction.isPending &&
                          alignmentKindWordAction.variables?.kindWord === item.kind_word
                        }
                        onDecide={(cls) =>
                          alignmentKindWordAction.mutate({ kindWord: item.kind_word, cls })
                        }
                      />
                    ),
                  )}
                </div>
              )}

              {active === "violations" && (
                <div className="space-y-3">
                  {/* The button lives in this tier, not in the page header: only people who look at this tier think to rerun it.
                      The report stays next to the button: an empty result must say whether it is 'no contradictions' or 'no criteria' */}
                  <div className="flex items-center gap-3">
                    {/* ghost {/* Ghost rather than a solid white: this is the same kind of thing as 'probe mapping':
                        a manually triggered analysis, not the page's main action. Keep the solid white for
                        the real decision (confirm / merge) */}
                    <Button variant="secondary" size="sm"
                      disabled={runCheck.isPending}
                      onClick={() => runCheck.mutate()}
                    >
                      {runCheck.isPending
                        ? S.review.checking
                        : S.review.runCheck}
                    </Button>
                    {runCheck.data && (
                      <span className="text-small text-ink-2">
                        {/* The three outcomes say three things. **`found` is not the number to report**:
                            rerunning recomputes the already-decided ones too, so saying '3 conflicts' while
                            the list has only one entry looks like the interface dropped something */}
                        {runCheck.data.predicates_with_axioms === 0
                          ? S.review.checkNoAxioms
                          : runCheck.data.inserted > 0
                            ? S.review.checkFound(runCheck.data.inserted)
                            : runCheck.data.found > 0
                              ? S.review.checkNothingNew
                              : S.review.checkClean(runCheck.data.edges)}
                      </span>
                    )}
                  </div>
                  {counts.violations === 0 && !runCheck.data && (
                    <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
                      {S.review.checkNeverRun}
                    </div>
                  )}
                  {asViolations().map((v) => (
                    <div
                      key={v.id}
                      className={
                        v.id === search.item
                          ? "rounded-panel ring-1 ring-contest"
                          : undefined
                      }
                    >
                    <ViolationRow
                      violation={v}
                      busy={
                        violationAction.isPending &&
                        violationAction.variables?.id === v.id
                      }
                      onDecide={(resolution, opts) =>
                        violationAction.mutate({ id: v.id, resolution, opts })
                      }
                      onDuplicates={() => select("duplicates")}
                      onOntology={() => navigate({ to: "/ontology" })}
                    />
                    </div>
                  ))}
                </div>
              )}

              {active === "agent" &&
                ((c?.agent_rows ?? 0) === 0 ? (
                  <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
                    {S.review.agentEmpty}
                    {!kb?.governance && (
                      <>
                        {" "}
                        {S.review.overviewAgentOff}{" "}
                        <LinkButton
                          onClick={() =>
                            navigate({ to: "/kb/$kbId/settings", params: { kbId } })
                          }
                        >
                          {S.review.overviewAgentSettings}
                        </LinkButton>
                      </>
                    )}
                  </div>
                ) : (
                  <div className="space-y-2">
                    {asAgent().map((d) => (
                      <AgentRow
                        key={d.id}
                        d={d}
                        busy={agentAnswer.isPending && agentAnswer.variables?.id === d.id}
                        onAnswer={(action, rationale) =>
                          agentAnswer.mutate({ id: d.id, action, rationale })
                        }
                      />
                    ))}
                  </div>
                ))}

              {active === "merges" &&
                (counts.merges === 0 ? (
                  <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
                    {S.review.historyEmpty}
                  </div>
                ) : (
                  <div className="space-y-2">
                    {asMerges().map((m) => (
                      <MergeRow
                        key={m.id}
                        merge={m}
                        busy={revert.isPending && revert.variables === m.id}
                        onRevert={() => revert.mutate(m.id)}
                      />
                    ))}
                  </div>
                ))}

              {active === "decisions" &&
                (history.isPending ? (
                  <p className="text-body text-ink-2">{S.nav.loading}</p>
                ) : (history.data?.total ?? 0) === 0 ? (
                  <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
                    {S.review.decisionsEmpty}
                  </div>
                ) : (
                  <div className="space-y-2">
                    {(history.data?.events ?? []).map((e) => (
                      <DecisionRow key={e.id} e={e} />
                    ))}
                  </div>
                ))}

              {/* Single pagination: queue/merges are sliced on the client, decisions paginate server-side; the overview has no page */}
              {active === "duplicates" ? (
                <DuplicateReviewPager totals={duplicateTotals} types={types} page={page} onPage={setPage} />
              ) : active !== "decisions" && active !== "overview" && (
                <Pager
                  total={active === "agent" ? (c?.agent_rows ?? 0) : counts[active]}
                  pageSize={PAGE_SIZE[active]}
                  page={page}
                  onPage={setPage}
                />
              )}
              {active === "decisions" && (
                <Pager
                  total={history.data?.total ?? 0}
                  pageSize={PAGE_SIZE.decisions}
                  page={page}
                  onPage={setPage}
                />
              )}
            </section>
          )}
        </div>
      </div>
    </div>
  );
}
