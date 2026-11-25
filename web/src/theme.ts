/* Theme: dark / light / follow system. **Dark by default**: that is this interface's own color; light is for people looking at it
 * eight hours a day. Stored in the browser (`utopia.theme`), not sent to the backend: the viewer decides for themselves,
 * the same reasoning as language (0004).
 *
 * It lands on `<html data-theme="dark|light">`, and **only these two values**: 'system' is resolved here
 * to one of the two; CSS only recognizes data-theme, and no separate prefers-color-scheme block is written,
 * the same tokens written twice would diverge sooner or later. index.html has the same logic before the first frame, for the purpose of
 * avoiding a flash of the dark background on refresh. */

export type Theme = "system" | "light" | "dark";
export type Resolved = "light" | "dark";

const KEY = "utopia.theme";
const listeners = new Set<(t: Resolved) => void>();

function stored(): Theme {
  try {
    const v = localStorage.getItem(KEY);
    return v === "light" || v === "dark" || v === "system" ? v : "dark";
  } catch {
    return "dark";
  }
}

export function systemPrefers(): Resolved {
  return typeof matchMedia === "function" && matchMedia("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

export function resolve(t: Theme, system: Resolved = systemPrefers()): Resolved {
  return t === "system" ? system : t;
}

export function getTheme(): Theme {
  return stored();
}

export function resolvedTheme(): Resolved {
  return resolve(stored());
}

function apply(r: Resolved) {
  document.documentElement.dataset.theme = r;
  /* shadcn dark-variant recognizes the `.dark` class (`@custom-variant dark`),
     our own tokens and canvas recognize `data-theme`. **Both are set**: so components taken from the shadcn
     registry components follow the theme without edits, while the `data-theme` set
     (the canvas reads tokens, and the script before the first frame) is left untouched */
  document.documentElement.classList.toggle("dark", r === "dark");
  for (const fn of listeners) fn(r);
}

export function setTheme(t: Theme) {
  try {
    localStorage.setItem(KEY, t);
  } catch {
    /* Private mode: nothing is stored; it still applies for this session */
  }
  apply(resolve(t));
}

/** Things that must be redrawn once the theme settles (the canvas reads computed tokens, and CSS does not refresh them for it) are hooked here */
export function onThemeChange(fn: (t: Resolved) => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

/** Called once at startup: applies the stored choice to <html>, and follows system preference changes (only when system is selected) */
export function initTheme() {
  apply(resolvedTheme());
  if (typeof matchMedia !== "function") return;
  matchMedia("(prefers-color-scheme: light)").addEventListener("change", () => {
    if (stored() === "system") apply(systemPrefers());
  });
}
