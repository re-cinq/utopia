/** Reading and writing world-axis dates: one rule, shared across the site.
 *
 *  The server's `time_text` (for the model) and the exported `rdf::world_time` (for auditors) are the same rule's
 *  two other copies. If the three diverge, the date the person sees, the model sees and the auditor gets are not the same */

/** A world-axis endpoint is shown at **its own precision**: year → 2023, month → 2023-06, day → 2023-06-01.
 *  Show as many digits as the precision: previously everything went to the day, so a year-precision fact showed as January 1, which fills in a definite value
 *  a place where ignorance is filled with a definite value. A date with no precision (a bound from evidence on a derived row, 0022) is shown by day.
 *
 *  Take year, month and day in UTC rather than local: `valid_from` / `valid_to` come from statements in the document
 * ("took office 2 May 2019"), a **calendar date, not an instant**, so it has no timezone to begin with; it stores that day
 *  the UTC midnight of that day. Rendering in local time would make a UTC-5 reader see 2019-05-01: a day out of nowhere, and the direction of the error
 *  also changes with where the reader is. Record time (when we came to believe it) is a different thing, and that one should be local,
 *  see the `ymd` comment in EntityHistory. */
export function fmtTime(
  iso: string | null,
  precision: string | null,
): string | null {
  if (!iso) return null;
  const d = new Date(iso);
  const y = d.getUTCFullYear();
  const m = String(d.getUTCMonth() + 1).padStart(2, "0");
  const day = String(d.getUTCDate()).padStart(2, "0");
  if (precision === "year") return `${y}`;
  if (precision === "month") return `${y}-${m}`;
  // Below the hour (0024): ISO's short form with a Z; a clock time without a timezone is not an instant
  const hh = String(d.getUTCHours()).padStart(2, "0");
  const mi = String(d.getUTCMinutes()).padStart(2, "0");
  const ss = String(d.getUTCSeconds()).padStart(2, "0");
  if (precision === "hour") return `${y}-${m}-${day}T${hh}Z`;
  if (precision === "minute") return `${y}-${m}-${day}T${hh}:${mi}Z`;
  if (precision === "second") return `${y}-${m}-${day}T${hh}:${mi}:${ss}Z`;
  return `${y}-${m}-${day}`;
}

/** A date in the input box → timestamp + precision. **The number of digits written is the precision**, the same convention as the extraction side
 *  `parse_time` uses the same convention: 2023 means 'that year' and 2023-06 means 'that month'.
 *
 *  This is also the reason for not using a date picker: a picker forces a day, while 'the document only says the first half'
 *  is exactly the kind of error this form must fix. The read-back comparison exists because `new Date("2023-13")` does not throw. */
export function parseDateInput(
  s: string,
): { iso: string; precision: string } | null {
  const t = s.trim();
  const shape =
    /^(\d{4})(?:-(\d{2}))?(?:-(\d{2}))?(?:T(\d{2})(?::(\d{2}))?(?::(\d{2}))?(Z|[+-]\d{2}:\d{2})?)?$/.exec(
      t,
    );
  if (!shape) return null;
  const [, y, m, d, hh, mi, ss, zone] = shape;
  const iso = `${y}-${m ?? "01"}-${d ?? "01"}T00:00:00.000Z`;
  const calendar = new Date(iso);
  if (Number.isNaN(calendar.getTime())) return null;
  // Validate the written calendar date before applying its offset: Date silently
  // rolls February 30 into March, while a valid offset can legitimately change the day.
  if (calendar.toISOString().slice(0, 10) !== iso.slice(0, 10)) return null;
  if (hh === undefined) {
    return { iso, precision: d ? "day" : m ? "month" : "year" };
  }
  // Clock time (0024): without a timezone it is not an instant; do not guess, make the person add Z or +08:00;
  // with a timezone it is converted to UTC, and the value is truncated to the digit written
  if (!d || !zone) return null;
  const parsed = new Date(`${y}-${m}-${d}T${hh}:${mi ?? "00"}:${ss ?? "00"}${zone}`);
  if (Number.isNaN(parsed.getTime())) return null;
  return {
    iso: parsed.toISOString(),
    precision: ss !== undefined ? "second" : mi !== undefined ? "minute" : "hour",
  };
}
