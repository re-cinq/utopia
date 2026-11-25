// The landing page of the review desk (#377): the three questions a reviewer has on arrival: how many are waiting, how long they have waited,
// whether the queue is shrinking or growing. All data comes from GET /review/summary; here it is only drawn, not computed. The seven tiers' counts
// use the same rules as the left column (shared WHERE on the server), so the page no longer counts them separately.
import type { ReviewSummary } from "../api";
import { S } from "../i18n";
import { Chip, GroupLabel, LinkButton } from "../ui";

/** The seven tiers you can click into from the overview */
export type WaitingQueue = keyof ReviewSummary["waiting"];

/** The order of the seven queue tiers in the overview matches the left column */
const WAITING: { key: WaitingQueue; label: string }[] = [
  { key: "pending", label: S.review.railPending },
  { key: "duplicates", label: S.review.railDuplicates },
  { key: "conflicts", label: S.review.railConflicts },
  { key: "unconfirmed", label: S.review.railUnconfirmed },
  { key: "lowconf", label: S.review.railLowConfidence },
  { key: "violations", label: S.review.railViolations },
  { key: "defects", label: S.review.railDefects },
  { key: "alignment", label: S.review.railAlignment },
];

/** Whole days from a moment until now; less than a full day counts as 0 */
function daysSince(iso: string): number {
  return Math.max(0, Math.floor((Date.now() - new Date(iso).getTime()) / 86_400_000));
}

function SectionHead({ children }: { children: string }) {
  return <GroupLabel className="mb-3">{children}</GroupLabel>;
}

/** One stat cell: big number on top, explanation below. The card itself is not a control: to enter that tier, click the
 *  sentence below; the number is only a number */
function Stat({
  value,
  label,
  note,
  action,
}: {
  value: number | string;
  label: string;
  note?: string | null;
  action?: { label: string; onClick: () => void };
}) {
  return (
    <div className="glass flex flex-col gap-1 rounded-panel p-4">
      <div className="u-num text-display text-ink">{value}</div>
      <div className="text-body text-ink-2">{label}</div>
      {note && <div className="text-fine text-ink-2">{note}</div>}
      {action && (
        <div className="mt-2">
          <LinkButton onClick={action.onClick}>{action.label}</LinkButton>
        </div>
      )}
    </div>
  );
}

/** One bar per day over the past 14 days. **Equidistant, zeros included**: the server has already padded days with no decisions as 0, so here
 *  there is no more interpolation. Height is normalized to the maximum of these 14 days; the shortest keeps a 2px baseline so that an all-zero
 *  case still shows something */
function DailyBars({ days }: { days: ReviewSummary["decided"]["daily"] }) {
  const max = Math.max(1, ...days.map((d) => d.count));
  return (
    <div>
      {/* One cell per day with a bar centered in it, **the bar itself has a maximum width**. It used to be `flex-1`,
          filling the cell: this card can be over a thousand pixels wide, and fourteen bars became fourteen boards,
          so a day with data looked like a wall rather than a bar */}
      <div className="flex h-16 items-end gap-1">
        {days.map((d) => (
          <div key={d.day} className="flex h-full flex-1 items-end justify-center">
            <div
              className="w-full max-w-[18px] rounded-none bg-bar"
              style={{
                height: `${Math.max(2, Math.round((d.count / max) * 64))}px`,
                opacity: d.count === 0 ? 0.25 : 1,
              }}
              title={`${d.day} · ${d.count}`}
            />
          </div>
        ))}
      </div>
      <div className="mt-1 flex justify-between text-fine text-ink-2">
        <span>{days[0]?.day}</span>
        <span>{days[days.length - 1]?.day}</span>
      </div>
    </div>
  );
}

export function ReviewOverview({
  summary,
  governance,
  onPick,
  onSettings,
}: {
  summary: ReviewSummary;
  /** The governance switch for this library (0025): when off, the Agent section's explanation says where to turn it on */
  governance: boolean;
  /** The 'handle it' link on a tier: lands on that tier in the left column */
  onPick: (queue: WaitingQueue | "agent") => void;
  onSettings: () => void;
}) {
  const { waiting, decided, health, agent } = summary;
  const waitingTotal = WAITING.reduce((n, w) => n + waiting[w.key].count, 0);
  const share = (n: number) =>
    health.facts === 0 ? "—" : `${Math.round((n / health.facts) * 1000) / 10}%`;

  return (
    <div className="space-y-8">
      {/* Waiting to be handled */}
      <section>
        <SectionHead>{S.review.overviewWaiting}</SectionHead>
        {waitingTotal === 0 ? (
          <div className="glass rounded-panel p-8 text-center text-body text-ink-2">
            {S.review.overviewAllClear}
          </div>
        ) : (
          <div className="grid grid-cols-2 gap-3 md:grid-cols-3 lg:grid-cols-4">
            {WAITING.filter((w) => waiting[w.key].count > 0).map((w) => {
              const q = waiting[w.key];
              return (
                <Stat
                  key={w.key}
                  value={q.count}
                  label={w.label}
                  note={q.oldest_at ? S.review.overviewOldest(daysSince(q.oldest_at)) : null}
                  action={{ label: S.review.overviewOpen, onClick: () => onPick(w.key) }}
                />
              );
            })}
          </div>
        )}
      </section>

      {/* Already handled */}
      <section>
        <SectionHead>{S.review.overviewDecided}</SectionHead>
        <div className="grid grid-cols-2 gap-3">
          <Stat
            value={decided.last_7d.total}
            label={S.review.overviewLast7}
            note={S.review.overviewAutomatic(decided.last_7d.automatic)}
          />
          <Stat
            value={decided.last_30d.total}
            label={S.review.overviewLast30}
            note={S.review.overviewAutomatic(decided.last_30d.automatic)}
          />
        </div>
        <div className="glass mt-3 rounded-panel p-4">
          <div className="mb-3 text-fine text-ink-2">{S.review.overviewDaily}</div>
          <DailyBars days={decided.daily} />
        </div>
        {decided.last_30d.total === 0 ? (
          <p className="mt-3 text-small text-ink-2">{S.review.overviewNoDecisions}</p>
        ) : (
          <div className="mt-3 grid gap-3 md:grid-cols-2">
            <div className="glass rounded-panel p-4">
              <div className="mb-2 text-fine text-ink-2">{S.review.overviewByAction}</div>
              <div className="flex flex-wrap gap-2">
                {decided.last_30d.by_action.map((a) => (
                  <Chip key={a.action}>
                    {S.review.decisionActions[a.action] ?? a.action} · {a.count}
                  </Chip>
                ))}
              </div>
            </div>
            <div className="glass rounded-panel p-4">
              <div className="mb-2 text-fine text-ink-2">{S.review.overviewByActor}</div>
              <div className="space-y-1">
                {decided.last_30d.by_actor.map((a) => (
                  <div
                    key={a.actor_id ?? "auto"}
                    className="flex items-center justify-between text-body"
                  >
                    <span className="truncate text-ink-2">
                      {a.actor_id === null ? S.review.aiActor : (a.label ?? a.actor_id)}
                    </span>
                    <span className="u-num text-ink-2">{a.count}</span>
                  </div>
                ))}
              </div>
            </div>
          </div>
        )}
      </section>

      {/* agent {/* Done by the agent (0025): open suggestions, auto-decided ones still standing, and those a person accepted / overturned / withdrew.
          When the switch is off and nothing has happened, say only one sentence about where to turn it on, rather than showing four zeros */}
      <section>
        <SectionHead>{S.review.overviewAgent}</SectionHead>
        {!governance && (
          <p className="mb-3 text-small text-ink-2">
            {S.review.overviewAgentOff}{" "}
            <LinkButton onClick={onSettings}>{S.review.overviewAgentSettings}</LinkButton>
          </p>
        )}
        {/* Running now: a pulsing dot plus how many pairs are left; not running but backlogged: how many pairs are waiting for it */}
        {agent.running ? (
          <p className="mb-3 flex items-center text-small text-ink-2">
            <span className="mr-2 inline-block h-2 w-2 rounded-full bg-warn animate-pulse" />
            {S.review.overviewAgentRunning(agent.queue)}
          </p>
        ) : governance && agent.queue > 0 ? (
          <p className="mb-3 text-small text-ink-2">{S.review.overviewAgentQueue(agent.queue)}</p>
        ) : null}
        {(governance || agent.open > 0 || agent.last_30d.applied > 0) && (
          <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
            <Stat
              value={agent.open}
              label={S.review.overviewAgentOpen}
              action={
                agent.open > 0
                  ? { label: S.review.overviewOpen, onClick: () => onPick("agent") }
                  : undefined
              }
            />
            <Stat
              value={agent.last_7d.applied}
              label={S.review.overviewAgentApplied}
              note={S.review.overviewLast7}
            />
            <Stat
              value={agent.last_30d.accepted}
              label={S.review.overviewAgentAccepted}
              note={S.review.overviewAgentOverridden(agent.last_30d.overridden)}
            />
            <Stat
              value={agent.last_30d.reverted}
              label={S.review.overviewAgentReverted}
              note={S.review.overviewLast30}
            />
          </div>
        )}
      </section>

      {/* The library's health */}
      <section>
        <SectionHead>{S.review.overviewHealth}</SectionHead>
        <p className="mb-3 text-small text-ink-2">{S.review.overviewFacts(health.facts)}</p>
        <div className="grid grid-cols-3 gap-3">
          <Stat
            value={health.low_confidence}
            label={S.review.railLowConfidence}
            note={share(health.low_confidence)}
          />
          <Stat
            value={health.unconfirmed}
            label={S.review.railUnconfirmed}
            note={share(health.unconfirmed)}
          />
          <Stat
            value={health.contested}
            label={S.review.overviewContested}
            note={share(health.contested)}
          />
        </div>
      </section>
    </div>
  );
}
