import { createElement, type ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import type { RelationTypeView } from "../api";
import { PropertyDialog } from "./ontologyDialogs";

// Radix's Portal renders nothing in server-side rendering, so the dialog shell is replaced with the body rendered directly
vi.mock("../ui", async (original) => ({
  ...(await original<typeof import("../ui")>()),
  FormDialog: ({ children }: { children: ReactNode }) => children,
}));

// Only fills in the fields the dialog uses; the unfilled ones take their defaults (`existing?.x ?? …`)
const def = (id: string, kind: RelationTypeView["kind"], label: string) =>
  ({ id, key: id, label, kind, usage: 0 }) as RelationTypeView;

describe("PropertyDialog", () => {
  it("offers the knowledge base's attributes as edge attributes", () => {
    // Selected items are shown as tags; the tag name is taken from the candidates: if the attribute is not among the candidates, the tag cannot appear
    const relation = { ...def("moves_to", "relation", "Moves to"), qualifiers: ["reason"] };
    const html = renderToStaticMarkup(createElement(QueryClientProvider, {
      client: new QueryClient(),
      children: createElement(PropertyDialog, {
        kbId: "kb", existing: relation, allTypes: [], allRelations: [relation],
        allAttributes: [def("reason", "attribute", "Reason")], initialDomain: null,
        onClose: vi.fn(), onSaved: vi.fn(), onDeleted: vi.fn(), onError: vi.fn(),
      }),
    }));
    expect(html).toContain("Reason");
  });
});
