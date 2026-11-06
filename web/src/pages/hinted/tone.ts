import type { ChipTone } from "../../ui";
import type { HintMap } from "./hints";

const TONE_WORDS: Record<string, ChipTone> = {
  ok: "success",
  danger: "danger",
  warn: "warn",
};

/** `"v1=ok,v2=danger"` → value → tone; a pair without both sides is skipped */
export function parseTones(written: string): Map<string, ChipTone> {
  const tones = new Map<string, ChipTone>();
  for (const pair of written.split(",")) {
    const [value, word, ...extra] = pair.split("=").map((s) => s.trim());
    if (!value || !word || extra.length > 0) continue;
    tones.set(value, TONE_WORDS[word] ?? "neutral");
  }
  return tones;
}

export function toneOf(value: string, hints: HintMap | undefined): ChipTone {
  return parseTones(hints?.tones ?? "").get(value) ?? "neutral";
}
