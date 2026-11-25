import type { Kb } from "../api";

export type KbGroupItem = { kb: Kb; label: string };
export type KbEntry =
  | { kind: "kb"; kb: Kb }
  | { kind: "group"; name: string; items: KbGroupItem[] };

/** A library whose name contains `/` is grouped by the part before the first `/` (`re-cinq/lore` → group `re-cinq`,
 *  label `lore`). Both sides must have text after trimming whitespace, and a group needs at least two libraries to form: `/foo`,
 *  `foo/`, and the sole `CI/CD` remain full-name rows as before. A group sits where its first member originally was,
 *  and the order of the other libraries does not change */
export function kbGroups(kbs: Kb[]): KbEntry[] {
  const groups = new Map<string, KbGroupItem[]>();
  const owners = kbs.map((kb) => {
    const split = ownerAndLabel(kb.name);
    if (!split) return undefined;
    groups.set(split.owner, [...(groups.get(split.owner) ?? []), { kb, label: split.label }]);
    return split.owner;
  });
  const placed = new Set<string>();
  return kbs.flatMap((kb, index): KbEntry[] => {
    const owner = owners[index];
    const items = owner === undefined ? undefined : groups.get(owner)!;
    if (!items || items.length < 2) return [{ kind: "kb", kb }];
    if (placed.has(owner!)) return [];
    placed.add(owner!);
    return [{ kind: "group", name: owner!, items }];
  });
}

function ownerAndLabel(name: string) {
  const slash = name.indexOf("/");
  const owner = name.slice(0, slash).trim();
  const label = name.slice(slash + 1).trim();
  return slash >= 0 && owner && label ? { owner, label } : undefined;
}

/** Options for the multi-select dropdown: members of the same group sit together, `label` keeps the full name (search and the selected chips use it),
 *  and `short` is the name shown on the row below the group name */
export function kbOptions(kbs: Kb[]) {
  return kbGroups(kbs).flatMap((entry) =>
    entry.kind === "kb"
      ? [{ value: entry.kb.id, label: entry.kb.name }]
      : entry.items.map((item) => ({
          value: item.kb.id,
          label: item.kb.name,
          group: entry.name,
          short: item.label,
        })),
  );
}
