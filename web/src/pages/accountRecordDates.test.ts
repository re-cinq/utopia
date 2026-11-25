import { createElement, type FunctionComponent } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { beforeAll, describe, expect, it, vi } from "vitest";
import type { Kb, MyKb, TokenView } from "../api";
vi.mock("../kb", () => ({
  useKb: () => ({
    kbs: [],
    workspace: { id: "ws", org_id: "org", name: "Team", created_at: "2026-01-01T00:00:00Z" },
    setKb: () => {},
  }),
}));
vi.mock("@tanstack/react-router", async (original) => ({
  ...(await original<typeof import("@tanstack/react-router")>()),
  useNavigate: () => () => {},
  useSearch: () => ({}),
}));
import { MyKbs } from "./MyKbs";
import { Tokens } from "./Tokens";

// Before 8 a.m. in Shanghai, UTC is still the previous day: slicing the day by the record's UTC instant makes the viewer see the previous day
beforeAll(() => {
  process.env.TZ = "Asia/Shanghai";
});

function render(page: FunctionComponent, data: [string[], unknown][]) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  for (const [key, value] of data) client.setQueryData(key, value);
  return renderToStaticMarkup(createElement(QueryClientProvider, { client, children: createElement(page) }));
}

const days = (html: string) => html.match(/\d{4}-\d{2}-\d{2}/g);

describe("record moments on the account pages", () => {
  it("dates tokens by the reader's calendar, not by UTC", () => {
    // 10 // A 30-day token issued at 07:30 on October 16 expires on November 15 at 07:30; it was used at 07:45
    const laptop: TokenView = {
      id: "t1", name: "laptop", token_prefix: "utp_pat_ab12", scope: "read", kb_ids: null,
      created_at: "2026-10-15T23:30:00Z", expires_at: "2026-11-14T23:30:00Z",
      last_used_at: "2026-10-15T23:45:00Z", revoked_at: null,
    };
    // 10 // An old token issued at 04:00 on October 1 was revoked at 06:10 on the 16th
    const old: TokenView = {
      ...laptop, id: "t2", name: "old", token_prefix: "utp_pat_cd34",
      created_at: "2026-09-30T20:00:00Z", expires_at: null, last_used_at: null,
      revoked_at: "2026-10-15T22:10:00Z",
    };
    const html = render(Tokens, [[["tokens"], { tokens: [laptop, old] }]]);
    // Line by line: used, expires, issued; the token that was revoked has only issued and revoked
    expect(days(html)).toEqual(["2026-10-16", "2026-11-15", "2026-10-16", "2026-10-01", "2026-10-16"]);
  });

  it("dates a membership by the reader's calendar, not by UTC", () => {
    const kb: Kb = {
      id: "kb1", workspace_id: "ws", name: "Research", kind: "knowledge", description: null,
      visibility: "restricted", is_default: false, auto_extend_ontology: true,
      materialize_inferences: false, auto_type_resolution: false, governance: false,
      governance_since: null, inference_interval_minutes: 60, last_inference_at: null,
      ontology_lang: "en",
    };
    // Alice added me at 07:30 in Shanghai in the morning; the other library I joined myself at exactly midnight
    const research: MyKb = {
      kb, my_role: "editor", joined_at: "2026-10-15T23:30:00Z", added_by_name: "Alice",
      doc_count: 3, member_count: 2,
    };
    const notes: MyKb = {
      ...research, kb: { ...kb, id: "kb2", name: "Notes" },
      joined_at: "2026-10-15T16:00:00Z", added_by_name: null,
    };
    const html = render(MyKbs, [[["myKbs", "ws"], { kbs: [research, notes] }]]);
    expect(days(html)).toEqual(["2026-10-16", "2026-10-16"]);
  });
});
