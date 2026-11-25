import type { AlignmentItem, AlignmentMarks, RelationTypeView } from "../api";

type PhraseItem = Extract<AlignmentItem, { kind: "phrase" }>;

/** What is preselected when a phrase signature opens. If it is already bound and only the marks are left, follow the current binding; otherwise follow the first vote, then the second vote.
 *  Which mark a single date carries is **not preselected**: if bindings that came in in bulk after the upgrade preselected a reading, one click would write it for the person
 *  (#975 review). Under a state attribute the person must choose one themselves before binding */
export function phraseStart(item: PhraseItem): {
  property: string;
  direction: "forward" | "reverse";
  marks: AlignmentMarks | null;
} {
  const first = item.votes?.first ?? null;
  const second = item.votes?.second ?? null;
  return {
    property: item.bound_to ?? first?.property ?? second?.property ?? "",
    direction: item.direction ?? first?.direction ?? second?.direction ?? "forward",
    marks: null,
  };
}

/** Only ask which mark a single date carries when the selected attribute is a state: events and constants have no such question, and the server does not accept it either */
export function asksMarks(properties: RelationTypeView[], key: string): boolean {
  return properties.find((p) => p.key === key)?.temporal === "state";
}
