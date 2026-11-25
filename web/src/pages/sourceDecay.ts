import type { DecaySettings } from "../api";

/** Backend defaults (RC-003, a constant from Lore): missing fields take these, so a hand-written `{}` in the library can still be read */
export const DEFAULT_DECAY: DecaySettings = {
  max_items: 500,
  half_life_days: 60,
  stale_after_days: 30,
  min_age_days: 30,
};

export const decayFormValue = (
  saved: Partial<DecaySettings> | null | undefined,
): DecaySettings => ({ ...DEFAULT_DECAY, ...saved });

const whole = (n: number, min: number, max = Infinity) =>
  Number.isInteger(n) && n >= min && n <= max;

/** The first field out of range, or null if none. The range is the same as the backend `DecaySettings::validate`;
 *  `min_age_days` may be 0 (anyone may be evicted) */
export function decayIssue(form: DecaySettings): keyof DecaySettings | null {
  if (!whole(form.max_items, 1)) return "max_items";
  if (!whole(form.half_life_days, 1, 365)) return "half_life_days";
  if (!whole(form.stale_after_days, 1)) return "stale_after_days";
  if (!whole(form.min_age_days, 0)) return "min_age_days";
  return null;
}

/** The `decay` to send on save: if unchanged, nothing at all is sent (the backend's "absent = leave as is"); when turned off, write null,
 *  and when opened or changed, send the whole object */
export function decayChange(
  saved: Partial<DecaySettings> | null,
  enabled: boolean,
  form: DecaySettings,
): { decay?: DecaySettings | null } {
  if (!enabled) return saved === null ? {} : { decay: null };
  const unchanged =
    saved !== null &&
    (Object.keys(DEFAULT_DECAY) as (keyof DecaySettings)[]).every(
      (key) => decayFormValue(saved)[key] === form[key],
    );
  return unchanged ? {} : { decay: form };
}
