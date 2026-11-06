import { expect, it } from "vitest";
import { attr, entityType, link, relationType, sampleHints } from "./fixtures";
import { attrsOf, hasHints, hintsOf, parseHop } from "./hints";

it("builds a type map and a relation map keyed by key", () => {
  const hints = hintsOf({
    entity_types: [entityType("box", { panel: "tree" })],
    relation_types: [relationType("state", { role: "status" }, "attribute")],
  });
  expect([...hints.types]).toEqual([["box", { panel: "tree" }]]);
  expect([...hints.relations]).toEqual([
    ["state", { kind: "attribute", label: "state", hints: { role: "status" } }],
  ]);
});

it("a hop without a prefix reads as out", () => {
  expect(parseHop("holds")).toEqual({ dir: "out", key: "holds" });
});

it("a hop with an in prefix reads as in", () => {
  expect(parseHop("in:inside")).toEqual({ dir: "in", key: "inside" });
});

it("attributes are the facts without another entity", () => {
  const colour = attr("a", "colour", "red");
  expect(attrsOf([colour, link("b", "near", "y")])).toEqual([colour]);
});

it("an entity whose type has no hints but whose facts carry a hinted predicate is hinted", () => {
  expect(hasHints("item", [attr("a", "state", "open")], sampleHints)).toBe(true);
});

it("an entity with neither is not hinted", () => {
  expect(hasHints("item", [link("a", "near", "y")], sampleHints)).toBe(false);
});
