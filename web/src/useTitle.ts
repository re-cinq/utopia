/* Page title system: `{domain} | {page}`, with the brand first (a product decision; the cost is that after tabs are truncated they share a prefix).
   Domains: Utopia (main app) / Utopia Charter (docs) / Utopia Persona (account). */
import { useEffect } from "react";

export function usePageTitle(...parts: (string | null | undefined)[]) {
  const title = parts.filter(Boolean).join(" | ");
  useEffect(() => {
    if (title) document.title = title;
  }, [title]);
}
