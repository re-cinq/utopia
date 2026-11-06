import { expect, it } from "vitest";
import { link } from "./fixtures";
import { nestBy } from "./nest";

it("lists the direct children of the root", () => {
  const facts = [link("1", "holds", "a"), link("2", "near", "z"), link("3", "holds", "b")];
  expect(nestBy(facts, "holds", new Map())).toEqual([
    { id: "a", name: "a", children: "unloaded" },
    { id: "b", name: "b", children: "unloaded" },
  ]);
});

it("nests a loaded child's own children", () => {
  const loaded = new Map([["a", [link("2", "holds", "a1")]]]);
  expect(nestBy([link("1", "holds", "a")], "holds", loaded)).toEqual([
    { id: "a", name: "a", children: [{ id: "a1", name: "a1", children: "unloaded" }] },
  ]);
});

it("marks an unloaded child so the view can fetch it", () => {
  expect(nestBy([link("1", "holds", "a")], "holds", new Map())[0].children).toBe("unloaded");
});

it("a cycle does not recurse forever", () => {
  const loaded = new Map([
    ["a", [link("2", "holds", "b")]],
    ["b", [link("3", "holds", "a")]],
  ]);
  expect(nestBy([link("1", "holds", "a")], "holds", loaded)).toEqual([
    { id: "a", name: "a", children: [{ id: "b", name: "b", children: [] }] },
  ]);
});
