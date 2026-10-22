import type { DecaySettings } from "../api";

/** 后端的默认值（RC-003，Lore 的常数）：缺的字段取这些，所以库里手写的 `{}` 也读得出 */
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

/** 第一个越界的字段，没有就是 null。范围同后端 `DecaySettings::validate`；
 *  `min_age_days` 可以是 0（谁都可能被淘汰） */
export function decayIssue(form: DecaySettings): keyof DecaySettings | null {
  if (!whole(form.max_items, 1)) return "max_items";
  if (!whole(form.half_life_days, 1, 365)) return "half_life_days";
  if (!whole(form.stale_after_days, 1)) return "stale_after_days";
  if (!whole(form.min_age_days, 0)) return "min_age_days";
  return null;
}

/** 保存时要带的 `decay`：没改就一个字都不带（后端「缺省 = 不动」），关掉写 null，
 *  打开或改了就整份发过去 */
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
