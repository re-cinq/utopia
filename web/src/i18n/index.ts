// UI language: **the viewer chooses it**; it does not go through the backend (see docs/decisions/0004).
// Same kind as dark mode: a deployment setting such as the concurrency cap describes the deployment, while the UI language describes the reader.
//
// The locale is resolved only in this one file. The other 600+ places only consume `S`, and nobody reads the source.
// If we later decide whether to follow the browser or allow per-user overrides, the change happens here, not in those 600 places.
import { en, type Strings } from "./en";
import { zh } from "./zh";

export type { Strings };

export const LANGS = ["en", "zh"] as const;
export type Lang = (typeof LANGS)[number];

/** A language's own name is never translated: in the switcher, "中文" is the signpost for people who cannot read English. */
export const LANG_NAMES: Record<Lang, string> = { en: "English", zh: "中文" };

const BUNDLES: Record<Lang, Strings> = { en, zh };
const KEY = "utopia.lang";

function detect(): Lang {
  try {
    const saved = localStorage.getItem(KEY);
    if (saved && (LANGS as readonly string[]).includes(saved))
      return saved as Lang;
  } catch {
    // In private mode localStorage throws, so fall back to English and keep the first screen from hanging
  }
  // **Does not follow the browser language.** The Chinese pack still trails the English one, and guessing the wrong language costs
  // a Chinese user an unfinished half-translation instead of the complete English. Once zh has caught up, put back
  // the navigator.language line; that is a one-line change.
  // A choice the person made themselves still counts (the paragraph above), and the switcher stays in the user menu as usual
  return "en";
}

export const lang: Lang = detect();

/** The current language pack. It is fixed once when the module loads, so module-level constants (`const X = S.a.b`) are also correct */
export const S: Strings = BUNDLES[lang];

/**
 * Switching language does a full page reload, not a remount.
 *
 * 635 references already include one evaluated at module top level (ROLE_OPTIONS in `Members.tsx`),
 * and this pattern will be written again in future. A remount would leave them on the old language, and **silently** so;
 * a reload re-evaluates the whole module graph, at the cost of one refresh. Switching language happens only a few times a year.
 */
export function setLang(next: Lang) {
  if (next === lang) return;
  try {
    localStorage.setItem(KEY, next);
  } catch {
    // If it cannot be stored, it only applies to this session, which is better than doing nothing
  }
  location.reload();
}
