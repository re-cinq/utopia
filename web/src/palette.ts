/* Data colours for entity types (rule 4: colour belongs only to data). Moved here from ui/index.tsx (0038),
 * because style-guard now forbids colour values themselves in pages and components, while this set **is data** and must match
 * `crates/utopia-store/src/palette.rs` exactly. The light version temporarily reuses the same set: a data palette designed for a light background
 * has to change on the server side too, which is a separate cut. */

/* ---------- ColorPicker (curated palette with a hex fallback; entity colours deliberately do not open the full gamut) ----------
   **Changing this means changing `crates/utopia-store/src/palette.rs`**: colours picked by hand and colours chosen automatically by key
   must come from the same set, otherwise two palettes would appear in one graph. There is a test on the other side that goes red if this is missed. */
export const ENTITY_PALETTE = [
  "#7fd0ff",
  "#5fa8ff",
  "#5fd4d0",
  "#63e2b7",
  "#4cc38a",
  "#a8d878",
  "#ffd479",
  "#f2b66d",
  "#ff9d76",
  "#ff8a9e",
  "#ff9daf",
  "#e797d8",
  "#c4a5ff",
  "#9fa8ff",
  "#8ea5bd",
  "#b3b9c4",
];

/**
 * Class key → colour. **Must match `color_for_key` in `crates/utopia-store/src/palette.rs` bit for bit**:
 * when a class is created, the frontend first picks a colour from the key and shows it; if the user does not change it, that is what gets stored.
 * The import and resolution path is computed by the backend; if the two compute differently,
 * the same key would get different colours depending on "who created it".
 *
 * FNV-1a with avalanche mixing. BigInt is used because JS bitwise operations are 32-bit,
 * and this needs 64-bit multiplication: using Number would silently drop the high bits,
 * and the result would disagree with Rust, with no error.
 */
export function colorForKey(key: string): string {
  let h = 0xcbf29ce484222325n;
  const M = (1n << 64n) - 1n;
  for (const b of new TextEncoder().encode(key)) {
    h = (h ^ BigInt(b)) & M;
    h = (h * 0x100000001b3n) & M;
  }
  h = (h ^ (h >> 33n)) & M;
  h = (h * 0xff51afd7ed558ccdn) & M;
  h = (h ^ (h >> 33n)) & M;
  return ENTITY_PALETTE[Number(h % BigInt(ENTITY_PALETTE.length))];
}
