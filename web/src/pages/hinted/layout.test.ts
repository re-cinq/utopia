import { expect, it } from "vitest";
import { attr, link, node, sampleHints } from "./fixtures";
import { crumbTrail, groupByPredicate, layoutFor } from "./layout";

it("uses the title attribute when its role is title and the name otherwise", () => {
  expect(layoutFor(node("x"), [attr("a", "name", "Big box")], sampleHints).title).toBe("Big box");
  expect(layoutFor(node("x"), [], sampleHints).title).toBe("x");
});

it("picks the status tone from the tones hint", () => {
  expect(layoutFor(node("x"), [attr("a", "state", "closed")], sampleHints).status).toEqual({
    value: "closed",
    tone: "danger",
  });
});

it("puts membership in-facts in the crumbs", () => {
  const facts = [link("1", "inside", "shelf", "in"), link("2", "near", "z", "in")];
  expect(layoutFor(node("x"), facts, sampleHints).crumbs).toEqual([{ id: "shelf", name: "shelf" }]);
});

it("groups relation facts by predicate and direction", () => {
  const facts = [link("1", "near", "a"), link("2", "near", "b"), link("3", "near", "c", "in")];
  expect(groupByPredicate(facts).map((g) => [g.key, g.facts.map((f) => f.id)])).toEqual([
    ["out:near", ["1", "2"]],
    ["in:near", ["3"]],
  ]);
});

it("leaves attributes consumed by roles out of the groups", () => {
  const facts = [attr("a", "state", "open"), attr("b", "name", "Big"), attr("c", "colour", "red")];
  expect(layoutFor(node("x"), facts, sampleHints).groups.map((g) => g.key)).toEqual(["out:colour"]);
});

it("marks a group whose relation has a range qualifier", () => {
  const extent = { qualifier_type_id: "extent", key: "extent", label: "extent", value: { value: "1-4" }, entity_id: null, entity_name: null };
  const facts = [link("1", "near", "a", "out", { qualifiers: [extent] }), link("2", "near", "b", "in")];
  expect(layoutFor(node("x"), facts, sampleHints).groups.map((g) => [g.key, g.ranged])).toEqual([
    ["out:near", true],
    ["in:near", false],
  ]);
});

it("walks membership upward through what is loaded, outermost first", () => {
  const loaded = new Map([
    ["shelf", [link("1", "inside", "room", "in")]],
    ["room", [link("2", "inside", "house", "in")]],
  ]);
  expect(crumbTrail({ id: "shelf", name: "shelf" }, loaded, sampleHints).map((c) => c.id)).toEqual([
    "house",
    "room",
    "shelf",
  ]);
});
