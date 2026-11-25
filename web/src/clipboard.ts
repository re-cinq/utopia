import { S } from "./i18n";
import { toast } from "./toast";

/** Puts a piece of text on the clipboard. Returns false if that did not work.
 *
 *  `navigator.clipboard` exists only in secure contexts: https, or localhost. On a deployment opened over http on the LAN (the most common way self-hosted instances are opened),
 *  it is undefined, and calling it directly
 *  does nothing when the button is pressed and throws no error. There, fall back to `execCommand("copy")`: select an invisible
 *  textarea and copy it. This API is no longer recommended, but browsers still support it, and it only runs when the clipboard API
 *  is unavailable. If neither works, return false and let the caller say so, rather than failing silently. */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // Permission denied, or the page does not have focus: use the old method below
  }
  const focused = document.activeElement as HTMLElement | null;
  const area = document.createElement("textarea");
  area.value = text;
  // Read-only: on phones the keyboard does not pop up; fixed in the viewport and transparent, so it does not scroll the page away and cannot be seen
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.top = "0";
  area.style.left = "0";
  area.style.opacity = "0";
  document.body.appendChild(area);
  try {
    area.focus();
    area.select();
    return document.execCommand("copy");
  } catch {
    return false;
  } finally {
    area.remove();
    focused?.focus?.();
  }
}

/** Copies, then states the result with a toast: says `copied` on success, says it cannot copy on failure.
 *  Each page's copy button goes through here; the chat ones swap in their own icon and only show a message on failure */
export async function copyAndSay(text: string, copied: string): Promise<boolean> {
  const ok = await copyText(text);
  if (ok) toast.success(copied);
  else toast.error(S.toast.copyFailed);
  return ok;
}
