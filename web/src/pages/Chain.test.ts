import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, it } from "vitest";
import { link, node, sampleHints } from "./hinted/fixtures";
import { ChainColumnView } from "./Chain";

it("renders one row per card and an empty state when a column reports none", () => {
  const hop = { dir: "out" as const, key: "holds" };
  const full = renderToStaticMarkup(
    createElement(ChainColumnView, {
      column: {
        hop,
        label: "holds",
        state: "ok",
        cards: [
          { id: "a", name: "a", loaded: { entity: node("a"), facts: [link("1", "near", "z")] } },
          { id: "b", name: "b" },
        ],
      },
      hints: sampleHints,
      onOpen: () => {},
    }),
  );
  const empty = renderToStaticMarkup(
    createElement(ChainColumnView, {
      column: { hop, label: "holds", state: "none", cards: [] },
      hints: sampleHints,
      onOpen: () => {},
    }),
  );
  expect(full.match(/type="button"/g)).toHaveLength(2);
  expect(empty).toContain("Nothing along holds");
});
