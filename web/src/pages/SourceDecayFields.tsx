import type { DecaySettings } from "../api";
import { S } from "../i18n";
import { Checkbox, cn, Input } from "../ui";
import { decayIssue } from "./sourceDecay";

const FIELDS: { key: keyof DecaySettings; label: () => string; min: number; max?: number }[] = [
  { key: "max_items", label: () => S.library.decay.maxItems, min: 1 },
  { key: "half_life_days", label: () => S.library.decay.halfLife, min: 1, max: 365 },
  { key: "stale_after_days", label: () => S.library.decay.staleAfter, min: 1 },
  { key: "min_age_days", label: () => S.library.decay.minAge, min: 0 },
];

/** 来源的衰减开关和四个数（RC-003）。数不合规就把那一格的标签标红，保存键由调用方按
 *  `decayIssue` 关掉 */
export function SourceDecayFields({
  enabled,
  onEnabled,
  value,
  onChange,
}: {
  enabled: boolean;
  onEnabled: (on: boolean) => void;
  value: DecaySettings;
  onChange: (next: DecaySettings) => void;
}) {
  const issue = enabled ? decayIssue(value) : null;
  return (
    <div className="mb-3">
      <Checkbox checked={enabled} onChange={onEnabled} label={S.library.decay.toggle} />
      {enabled && (
        <>
          <p className="mb-2 mt-1 text-fine leading-relaxed text-ink-2">{S.library.decay.hint}</p>
          <div className="grid grid-cols-2 gap-x-3 gap-y-2">
            {FIELDS.map(({ key, label, min, max }) => (
              <div key={key}>
                <div
                  className={cn(
                    "mb-1 text-fine font-medium",
                    issue === key ? "text-danger" : "text-ink-2",
                  )}
                >
                  {label()}
                </div>
                <Input
                  className="w-full u-num"
                  type="number"
                  min={min}
                  max={max}
                  value={value[key]}
                  onChange={(e) => onChange({ ...value, [key]: Number(e.target.value) })}
                />
              </div>
            ))}
          </div>
        </>
      )}
    </div>
  );
}
