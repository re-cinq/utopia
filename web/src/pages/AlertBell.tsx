// Top-bar alerts (0005): the bell + the unread badge + the dropdown panel.
//
// **The popover is not a page**: alerts are something you glance at in passing, not a place you go browsing.
// Making it a page would force people to leave what they are doing, and the cost of leaving is that nobody looks.
//
// One alert = one failure, frozen once written; there is no "resolved".
// "Read" is per person: one person having read it does not mean it should vanish from everyone else's unread list.
import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Bell } from "lucide-react";

import { api, type AlertGroup } from "../api";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { S } from "../i18n";
import { toast } from "../toast";
import {
  Button,
  Chip,
  type ChipTone,
  cn,
  IconButton,
  Input,
  LinkButton,
  Pager,
  localDateTime,} from "../ui";

const PAGE = 8;

/** The line shown to people in the detail: object name — the original error text, or the truncated range */
function line(d: AlertGroup["lines"][number]): string | null {
  const partial = d.warnings?.flatMap((warning) => {
    const anchor = warning.detail;
    if (
      warning.kind === "spreadsheet.rows_truncated" &&
      typeof anchor?.sheet === "string" &&
      typeof anchor.rows_read === "number" &&
      typeof anchor.rows_total === "number"
    ) {
      return [S.alerts.partialRows(anchor.sheet, anchor.rows_read, anchor.rows_total)];
    }
    if (
      warning.kind === "csv.records_truncated" &&
      typeof anchor?.records_read === "number" &&
      typeof anchor.records_total === "number"
    ) {
      return [S.alerts.partialRecords(anchor.records_read, anchor.records_total)];
    }
    return [];
  });
  const parts = [d.name ?? d.job, d.error, ...(partial ?? [])].filter(Boolean);
  return parts.length ? parts.join(" — ") : null;
}

/** Which alerts carry "run again": the kinds where the task does not come back by itself once the fault is fixed (top-up, changed endpoint) */
const REQUEUE_KINDS = new Set(["llm.out_of_credit", "llm.unreachable"]);

/* Severity is set by the server (`severity` in `utopia-store/src/alerts.rs`); a group takes the most severe tier in it.
   * Not all alerts are faults: running out of credit and a source sync failure are error; rate limiting, a schema not ingested,
   * and a governance trip are warning, while "mapping exploration did not propose a single convention" is info: it is "the thing you were waiting for has no result",
   * not broken. Previously this column ignored severity entirely, and the seven kinds of alert all looked the same. */
const SEVERITY_TONE: Record<string, ChipTone> = {
  error: "danger",
  warning: "warn",
  info: "info",
};

function AlertRow({
  g,
  onRead,
  onRequeue,
  requeuing,
}: {
  g: AlertGroup;
  onRead: (g: AlertGroup) => void;
  onRequeue: (g: AlertGroup) => void;
  requeuing: boolean;
}) {
  // A kind we have not seen must still be displayable: when a new alert source goes live, the frontend may not have caught up,
  // and "there is an alert but I do not recognize it" is far better than "nothing is shown"
  const worded = S.alerts.kinds[g.kind];
  const lines = g.lines.map(line).filter((l): l is string => !!l);
  /* The same error sentence repeated five times: a reader stops reading after the second time, yet it still makes the panel a fair bit taller:
     identical rows are merged into one, with a count on the right. **What is merged is the display, not the count**:
     the "N more" below still uses the original number of entries */
  const tally = new Map<string, number>();
  for (const l of lines) tally.set(l, (tally.get(l) ?? 0) + 1);
  // count counts the whole group; lines only carries back the first few, and the difference is the "N more"
  const rest = g.count - lines.length;
  return (
    // div rather than button: the row has an action button inside it, and a button inside a button is invalid HTML
    <div
      role="button"
      tabIndex={0}
      // **Only a click counts as read**, not hovering over it. The mouse passing over a column of alerts does not mean they were seen,
      // and once marked read it never comes back by itself. One click marks the whole group
      onClick={() => {
        if (g.unread > 0) onRead(g);
      }}
      onKeyDown={(e) => {
        if (e.key === "Enter" && g.unread > 0) onRead(g);
      }}
      className={cn(
        "u-row-shell relative w-full cursor-pointer border-b border-line px-4 py-3 text-left last:border-b-0",
        // A read item drops a level: title, database name, time and detail all dim together, so a glance shows
        // which ones have not been looked at yet. Hover works as usual
        g.unread === 0 && "opacity-65",
      )}
    >
      {/* **Read or not is shown by the brightness of the whole row, not by a dot.** Previously unread was a coloured dot on the left, which could only
          be squeezed into the inner padding (if it sat in the text column, the body would be indented 18px from the panel title and search box,
          so one panel would have three left edges), and the result was hugging the left border, looking as if it had fallen outside.
          Now an unread title is body colour at medium weight, and a read row drops to the secondary colour: a glance down the list shows
          what is lit is what has not been seen yet. Severity is said by the colour of the count chip on the right, no separate dot is needed. */}
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <span
            className={cn(
              "min-w-0 flex-1 text-body",
              g.unread > 0 ? "font-medium text-ink" : "text-ink-2",
            )}
          >
            {worded?.title ?? S.alerts.unknownKind(g.kind)}
          </span>
          {/* The count is "how many times this happened", not a side note: neutral grey would read it as a
              label, while it says how heavy this alert is */}
          {g.count > 1 && (
            <Chip tone={SEVERITY_TONE[g.severity] ?? "neutral"}>{g.count}</Chip>
          )}
        </div>
        {/* Which database, and when: the footer sits on its own line. If it followed the title, a long title would push
            them onto the next line, and the first two lines of each alert would look different */}
        <div className="mt-1 flex items-center gap-2">
          <Chip tone={g.kb_name ? "neutral" : "violet"}>
            {g.kb_name ?? S.alerts.system}
          </Chip>
          {/* The latest of the occurrences in the group */}
          <span className="u-num ml-auto shrink-0 text-fine text-ink-2">
            {localDateTime(g.latest_at)}
          </span>
        </div>
        {worded && (
          <p className="mt-1 text-small text-ink-2">{worded.hint}</p>
        )}
        {lines.length > 0 && (
          <ul className="mt-1 space-y-1">
            {[...tally].map(([l, n]) => (
              <li key={l} className="text-fine text-ink-2 break-words">
                {l}
                {n > 1 && <span className="u-num text-ink-2"> ×{n}</span>}
              </li>
            ))}
            {rest > 0 && (
              <li className="text-fine text-ink-2">
                {S.alerts.andMore(rest)}
              </li>
            )}
          </ul>
        )}
        {/* Continue running after the fix: put back into the queue the tasks that failed during this fault window (#216).
            Running out of credit is the only failure where "once a person has done one concrete thing, they want the work to continue",
            so the action sits on the alert, the loop closes here, and there is no need for a separate queue page */}
        {REQUEUE_KINDS.has(g.kind) && (
          <Button variant="secondary" size="sm" className="mt-2"
            type="button"
            disabled={requeuing}
            onClick={(e) => {
              e.stopPropagation();
              onRequeue(g);
            }}
          >
            {S.alerts.runAgain}
          </Button>
        )}
      </div>
    </div>
  );
}

function Panel() {
  const [q, setQ] = useState("");
  const [page, setPage] = useState(0);
  const qc = useQueryClient();

  // After searching, go back to the first page: if you stay on page 4 of a result that has only 2 pages,
  // the panel shows blank, and the person reads it as "no alerts"
  useEffect(() => {
    setPage(0);
  }, [q]);

  const list = useQuery({
    queryKey: ["alerts", "list", q, page],
    queryFn: () => api.alerts({ q, limit: PAGE, offset: page * PAGE }),
    // Keep the previous page while paging, so the panel height does not collapse and then spring back
    placeholderData: (prev) => prev,
  });

  const read = useMutation({
    mutationFn: (g: AlertGroup) =>
      api.alertReadGroup({
        kb_id: g.kb_id,
        kind: g.kind,
        from: g.earliest_at,
        to: g.latest_at,
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["alerts"] }),
  });
  const readAll = useMutation({
    mutationFn: () => api.alertsReadAll(),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["alerts"] }),
  });
  // The time window starts from the earliest failure in this group: failures before that are not part of this one
  const requeue = useMutation({
    mutationFn: (g: AlertGroup) =>
      api.requeueJobs(g.kb_id, { failed_since: g.earliest_at }),
    onSuccess: (r) => {
      toast.success(S.alerts.requeued(r.requeued));
      qc.invalidateQueries({ queryKey: ["jobs"] });
    },
    onError: (e) => toast.error(String(e)),
  });

  const groups = list.data?.items ?? [];
  const total = list.data?.total ?? 0;

  return (
    <>
      {/* Title row. **No longer doubles as the close button**: previously the panel grew out of the bell in place, and the first row sat over
          the bell, so clicking it folded back; now that it is a standard popover, closing is handled by Esc, clicking outside and the bell itself,
          and this row only says what this is */}
      <div className="flex items-center gap-3 border-b border-line px-4 py-3">
        <Bell size={15} strokeWidth={1.8} className="shrink-0 text-ink-2" />
        <span className="min-w-0 flex-1 truncate text-body font-medium text-ink">
          {S.alerts.title}
        </span>
      </div>

      {/* Search looks the same as the library switcher: no box of its own (bare), it is a section of the panel,
          not a control placed inside it; Esc and outside clicks are handled uniformly by Popover */}
      <div className="border-b border-line px-4 py-3">
        <Input
          bare
          className="w-full text-body"
          placeholder={S.alerts.searchPlaceholder}
          value={q}
          onChange={(e) => setQ(e.target.value)}
        />
      </div>

      <div className="max-h-[420px] overflow-y-auto">
        {groups.length === 0 ? (
          <div className="px-4 py-8 text-center">
            <p className="text-body text-ink-2">
              {q ? S.alerts.noMatch : S.alerts.empty}
            </p>
            {!q && (
              <p className="mt-1 text-small text-ink-2">
                {S.alerts.emptyHint}
              </p>
            )}
          </div>
        ) : (
          groups.map((g) => (
            <AlertRow
              key={`${g.kb_id ?? "system"}|${g.kind}|${g.latest_at}`}
              g={g}
              onRead={(x) => read.mutate(x)}
              onRequeue={(x) => requeue.mutate(x)}
              requeuing={requeue.isPending}
            />
          ))
        )}
      </div>

      {/* Bottom bar: list-wide actions sit together with paging, furthest from the cursor */}
      {groups.length > 0 && (
        <div className="flex items-center gap-3 px-4 py-2 border-t border-line">
          {groups.some((g) => g.unread > 0) && (
            <LinkButton onClick={() => readAll.mutate()}>
              {S.alerts.markAllRead}
            </LinkButton>
          )}
          {/* Shown even with one page: this bottom bar is fixed, and if the pager hid it, it would become an empty border */}
          <Pager
            always
            className="ml-auto"
            total={total}
            pageSize={PAGE}
            page={page}
            onPage={setPage}
          />
        </div>
      )}
    </>
  );
}

export function AlertBell() {
  const [open, setOpen] = useState(false);
  const unread = useQuery({
    queryKey: ["alerts", "unread"],
    queryFn: () => api.alertsUnread(),
    // Push is the main path; this is only the fallback when the stream drops
    refetchInterval: 120_000,
  });
  const n = unread.data?.unread ?? 0;

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <IconButton size="md" label={S.alerts.badgeLabel} className="relative">
        <Bell size={15} />
        {/* The badge is also a dot, not a number: "something unread" is binary; open it to see how many. A number
            would keep jumping up with retries, and at three digits it would deform the bell */}
        {n > 0 && (
          <span className="absolute top-1 right-1 h-1.5 w-1.5 rounded-full bg-danger" />
        )}
        </IconButton>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-[420px] overflow-hidden p-0">
        <Panel />
      </PopoverContent>
    </Popover>
  );
}
