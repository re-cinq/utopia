/** How a step on the trace is worded (#942).
 *
 *  The server gives each step only fields (0004): `status`, count, and the moment, plus a few of the step's own.
 *  The wording is done here in the reader's language. `label`, the analytics query and the remember `detail` are data, shown verbatim.
 *
 *  **a message without a `status` was stored earlier**, and only has the English sentence the server wrote at the time,
 *  shown as-is; the English sentence is not parsed again. */
import type { ChatStep } from "./api";
import { S, type Strings } from "./i18n";
import { fmtTime } from "./time";
import { localDateTime } from "./ui";

/** World time: a calendar day, read in UTC (see fmtTime). A clock time is written only when one is given */
function worldMoment(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const precision =
    d.getUTCSeconds() || d.getUTCMilliseconds()
      ? "second"
      : d.getUTCHours() || d.getUTCMinutes()
        ? "minute"
        : "day";
  return fmtTime(iso, precision) ?? iso;
}

/** Record time: a real instant, shown in the timezone of the viewer (see localDateTime) */
function recordMoment(iso: string): string {
  return Number.isNaN(new Date(iso).getTime()) ? iso : localDateTime(iso);
}

/** The object of this step. Only changes needs to change: the '2026-09-01 → now' the server builds has 'now' in English */
export function stepLabel(step: ChatStep, T: Strings = S): string {
  if (step.status && step.kind === "changes" && step.since) {
    return `${step.since} → ${step.until ?? T.ask.step.now}`;
  }
  return step.label;
}

/** The result of this step */
export function stepDetail(step: ChatStep, T: Strings = S): string {
  const W = T.ask.step;
  if (!step.status) return step.detail;
  // The analytics `detail` is the purpose of this query; the result follows it
  const withPurpose = (result: string) =>
    step.kind === "query" && step.detail ? `${step.detail} · ${result}` : result;
  if (step.status === "failed") return withPurpose(W.failed);
  if (step.status === "not_found") {
    if (step.kind === "document") return W.documentNotFound;
    if (step.kind === "query") return withPurpose(W.sourceNotFound);
    if (step.kind === "tool") return W.unknownTool;
    return W.entityNotFound;
  }
  if (step.status === "invalid") {
    if (!step.param) return W.unparsed;
    return step.missing ? W.missing(step.param) : W.invalid(step.param);
  }
  const n = step.count ?? 0;
  const more = step.more === true;
  switch (step.kind) {
    case "search":
      return W.sources(n);
    case "document":
    case "docs":
      return W.sections(n);
    case "entity":
      return W.matches(n, step.total);
    case "facts": {
      const record = step.before
        ? W.recordedBefore(recordMoment(step.before))
        : step.as_of
          ? W.recordedBy(recordMoment(step.as_of))
          : null;
      return W.facts(n, step.valid_at ? worldMoment(step.valid_at) : null, record);
    }
    case "neighbors":
      return W.linked(n, step.total ?? n);
    case "timeline":
      return W.dated(n, step.total ?? n);
    case "path":
      return W.paths(n, more, step.hops ?? 0);
    case "changes":
      return W.changes(n, more);
    case "query":
      return withPurpose(W.rows(n, more));
    case "tool":
      if (step.label === "list_rules") return W.rules(n);
      if (step.label === "rule_matches") return W.marked(n);
      // remember `detail` is the sentence that was recorded
      return step.detail;
    default:
      // A kind the server adds later: its `detail` is still an English sentence that can be read
      return step.detail;
  }
}
