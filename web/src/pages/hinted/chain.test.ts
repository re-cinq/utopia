import { expect, it } from "vitest";
import { entityType, link, node, relationType, sampleHints } from "./fixtures";
import { chainColumns, frontierIds, hopsFrom, type Loaded } from "./chain";
import { hintsOf, parseHop } from "./hints";

const loaded = (id: string, facts: Loaded["facts"]): [string, Loaded] => [id, { entity: node(id), facts }];

it("reads the first hop from the class and the next hops from the relations", () => {
  expect(hopsFrom("box", sampleHints, 2)).toEqual([
    { dir: "out", key: "holds" },
    { dir: "in", key: "inside" },
  ]);
});

it("stops at a relation with no next hop", () => {
  const hints = hintsOf({
    entity_types: [entityType("box", { chain_first: "near" })],
    relation_types: [relationType("near")],
  });
  expect(hopsFrom("box", hints)).toEqual([{ dir: "out", key: "near" }]);
});

it("does not loop when relations point back at each other", () => {
  expect(hopsFrom("box", sampleHints)).toEqual([
    { dir: "out", key: "holds" },
    { dir: "in", key: "inside" },
  ]);
});

it("fills a column from the previous column's loaded cards", () => {
  const fetched = new Map([loaded("s", [link("1", "holds", "a"), link("2", "near", "z")])]);
  const [column] = chainColumns("s", [parseHop("holds")], fetched, sampleHints);
  expect(column).toMatchObject({ state: "ok", cards: [{ id: "a", name: "a" }] });
});

it("a column whose previous cards are not loaded reports unloaded", () => {
  const fetched = new Map([loaded("s", [link("1", "holds", "a")])]);
  const columns = chainColumns("s", [parseHop("holds"), parseHop("near")], fetched, sampleHints);
  expect(columns.map((c) => c.state)).toEqual(["ok", "unloaded"]);
});

it("a column with loaded cards but no matches reports none", () => {
  const fetched = new Map([loaded("s", [link("1", "near", "z")])]);
  expect(chainColumns("s", [parseHop("holds")], fetched, sampleHints)[0].state).toBe("none");
});

it("the same entity reached twice appears once", () => {
  const fetched = new Map([
    loaded("s", [link("1", "holds", "a"), link("2", "holds", "b")]),
    loaded("a", [link("3", "near", "z")]),
    loaded("b", [link("4", "near", "z")]),
  ]);
  const columns = chainColumns("s", [parseHop("holds"), parseHop("near")], fetched, sampleHints);
  expect(columns[1].cards.map((c) => c.id)).toEqual(["z"]);
});

it("frontier lists only cards without facts", () => {
  const fetched = new Map([loaded("s", [link("1", "holds", "a"), link("2", "holds", "b")]), loaded("a", [])]);
  expect(frontierIds(chainColumns("s", [parseHop("holds")], fetched, sampleHints))).toEqual(["b"]);
});
