/** Display of literal object values: attribute {value,unit} / analytics map {summary} / other JSON fallback.
 *
 *  One implementation, used by three pages (graph panel, pending-review list, history). Previously each wrote its own,
 *  so the way units were attached diverged.
 *
 *  **A value that already carries its unit is not given one again.** Unadopted literal values are kept as written (`"75.0%"`,
 *  `"$2.22"`), the unit is stored in its own cell so it can be converted on adoption; a display that ignores the value itself turns it into
 *  `75.0%%`  *  and `$2.22 $`. Adopted attribute values are numbers (`75`, `5e9`), and the unit is what should be attached. */
export function fmtObjectValue(v: Record<string, unknown> | null): string | null {
  if (!v) return null;
  if (v.value !== undefined) {
    if (typeof v.value === "boolean") return v.value ? "✓" : "✗";
    const unit = typeof v.unit === "string" && v.unit.trim() ? v.unit.trim() : "";
    /* Large numbers collapse to `$5B`. An amount is stored **multiplied out** (`$5 billion` → 5000000000),
       because values must be comparable to count as non-nodes; read aloud verbatim they become '5000000000 $',
       much harder to read than the original '$5 billion'. The symbol goes first and the number is compacted; both are needed */
    if (typeof v.value === "number" && unit && unit !== "%") {
      /* Two decimal places only for the compact form: values are read in their declared unit, so '350 ten-thousand yuan' on an attribute in 100-million-yuan units is
         0.035, rounding it to two places shows 0.04, and '3 ten-thousand yuan' became 0. The plain form keeps fifteen significant digits,
         and does not show rule arithmetic's 0.1 + 0.2 as 0.30000000000000004 */
      const compact = Math.abs(v.value) >= 10000;
      const n = new Intl.NumberFormat(undefined, {
        notation: compact ? "compact" : "standard",
        ...(compact ? { maximumFractionDigits: 2 } : { maximumSignificantDigits: 15 }),
      }).format(v.value);
      // Currency symbols precede an amount; physical units follow a quantity.
      return /\p{Sc}/u.test(unit) ? `${unit}${n}` : `${n} ${unit}`;
    }
    const val = String(v.value);
    if (!unit) return val;
    if (carriesUnit(val, unit)) return val;
    // The percent sign sits right against the number, other units get a space
    return unit === "%" ? `${val}%` : `${val} ${unit}`;
  }
  if (typeof v.summary === "string") return v.summary;
  return JSON.stringify(v);
}

/** The unit is already part of the source text: `75.0%`, `$2.22`, `€30 million`, `500 MW` */
function carriesUnit(val: string, unit: string): boolean {
  const t = val.trim();
  return t.startsWith(unit) || t.endsWith(unit) || t.includes(` ${unit} `) || t.includes(`${unit} `);
}
