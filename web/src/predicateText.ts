/** How a predicate reads **inside a sentence**.
 *
 * The store keeps lowerCamel (see migration 0042): `worksFor`, `acceptedAnswer`, `accessTo`.
 * That is what the vocabulary should look like: the UIs that manage the ontology (left column, tables, canvas edge labels, detail panels)
 * show it this way as-is, because there the word itself is what you recognize, and `acceptedAnswer` is the one you can look up in the
 * schema.org docs.
 *
 * Fact rows are not vocabulary: **they are a sentence**:
 *
 *   Li Si — works for — Meridian Systems
 *   Li Si — worksFor — Meridian Systems     ← looks like code, not prose
 *
 * So only the places that read it as a sentence (fact rows, the timeline, Chat answers) split it here, once.
 *
 * **abbreviations stay uppercase as a whole**: `productID` splits into “product ID” rather than “product id”,
 * `checkoutPageURLTemplate` splits into “checkout page URL template”. The method is to treat
 * a run of capitals as one word; only when a lowercase letter follows it is the last capital the
 * start of the next word.
 */
export function predicateSentence(label: string): string {
  const words: string[] = [];
  let cur = "";
  const chars = [...label.trim()];
  for (let i = 0; i < chars.length; i++) {
    const c = chars[i];
    if (c === " " || c === "_" || c === "-") {
      if (cur) words.push(cur);
      cur = "";
      continue;
    }
    const isUpper = c !== c.toLowerCase() && c === c.toUpperCase();
    if (!isUpper) {
      cur += c;
      continue;
    }
    const prev = chars[i - 1];
    const next = chars[i + 1];
    const prevUpper = prev !== undefined && prev === prev.toUpperCase() && prev !== prev.toLowerCase();
    const nextLower = next !== undefined && next === next.toLowerCase() && next !== next.toUpperCase();
    // Two cases where a capital starts a new word: after a lowercase (the F in worksFor),
    // or after another capital but followed by a lowercase (the T in URLTemplate)
    if (cur && (!prevUpper || nextLower)) {
      words.push(cur);
      cur = "";
    }
    cur += c;
  }
  if (cur) words.push(cur);
  if (words.length === 0) return label;
  // the first word is lowercase; **all-caps words are kept as is**: they are abbreviations
  return words
    .map((w, i) => {
      const allCaps = w.length > 1 && w === w.toUpperCase() && w !== w.toLowerCase();
      if (allCaps) return w;
      return i === 0 ? w.charAt(0).toLowerCase() + w.slice(1) : w.toLowerCase();
    })
    .join(" ");
}
