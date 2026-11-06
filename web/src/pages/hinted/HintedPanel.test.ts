import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { expect, it } from "vitest";
import { attr, node, sampleHints } from "./fixtures";
import { HintedPanel } from "./HintedPanel";

it("renders the title, a status dot with its words and a span from hinted attributes", () => {
  const facts = [
    attr("a", "name", "Big box"),
    attr("b", "state", "open"),
    attr("c", "begin", 12),
    attr("d", "finish", 30),
  ];
  const html = renderToStaticMarkup(
    createElement(
      QueryClientProvider,
      { client: new QueryClient() },
      createElement(HintedPanel, {
        kbId: "kb",
        entity: node("x", "item"),
        facts,
        hints: sampleHints,
        renderRow: () => null,
        onNavigate: () => {},
      }),
    ),
  );
  expect(html).toContain("Big box");
  expect(html).toContain("open");
  expect(html).toContain("L12–L30");
});
