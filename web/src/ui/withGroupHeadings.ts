type Grouped = { value: string; group?: string };

export type HeadedRow<T> =
  | { kind: "heading"; name: string }
  | { kind: "option"; option: T; index: number };

/** In the option list, insert a group-name row where the group changes. `index` is the option's position in the original list;
 *  keyboard up/down moves by it; group-name rows take no position and cannot be selected */
export function withGroupHeadings<T extends Grouped>(options: T[]): HeadedRow<T>[] {
  return options.flatMap((option, index) => {
    const opensGroup = !!option.group && option.group !== options[index - 1]?.group;
    const row: HeadedRow<T> = { kind: "option", option, index };
    return opensGroup ? [{ kind: "heading", name: option.group! }, row] : [row];
  });
}
