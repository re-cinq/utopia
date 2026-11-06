import type { EntityFact } from "../../api";
import { roleOf, valueOf, type Hints } from "./hints";

export interface LineRange {
  start: number;
  end: number;
}

const lineNumber = (raw: string | undefined) => {
  if (raw === undefined || raw.trim() === "") return null;
  const n = Number(raw);
  return Number.isFinite(n) ? n : null;
};

export function spanOf(attrs: EntityFact[], hints: Hints): LineRange | null {
  const byRole = (role: string) => {
    const fact = attrs.find((f) => roleOf(hints, f.predicate_key) === role);
    return fact ? lineNumber(valueOf(fact)) : null;
  };
  const start = byRole("span_start");
  const end = byRole("span_end");
  return start === null || end === null ? null : { start, end };
}

export function fmtSpan({ start, end }: LineRange): string {
  return start === end ? `L${start}` : `L${start}–L${end}`;
}

/** `"12-30,44-50"` → ranges; a piece that is not two numbers is skipped */
export function parseRanges(written: string): LineRange[] {
  return written.split(",").flatMap((piece) => {
    const [a, b, ...extra] = piece.split("-");
    const start = lineNumber(a);
    const end = lineNumber(b);
    return start === null || end === null || extra.length > 0 ? [] : [{ start, end }];
  });
}

export function fmtRanges(ranges: LineRange[]): string {
  return ranges.map(fmtSpan).join(", ");
}

export function overlaps(a: LineRange, b: LineRange): boolean {
  return a.start <= b.end && b.start <= a.end;
}
