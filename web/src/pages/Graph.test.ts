// `fmtInterval` // in the entity sidebar draws a fact's validity interval as 'yyyy-mm-dd ~ now' /
// "ended, date unknown" / a single-point pure function. The 2024 PR (before #669) had already
// distinguishes "endedUnknown" from "ongoing"; now the "event" (a single moment) is added:
// previously it showed "2024-03-15 ~ 2024-03-15" (the write side folds from/to into the same moment,
// and the UI still drew it as an interval); now it returns from alone.

import { describe, expect, it, vi } from "vitest";

// Same as classHierarchy.test.ts: Graph.tsx imports sigma at the top level, and sigma at startup
// tries to grab WebGL2RenderingContext, which vitest's default jsdom does not provide, so the module
// throws during evaluation. A polyfill is enough; no need to render for real
vi.hoisted(() => {
  for (const name of ["WebGLRenderingContext", "WebGL2RenderingContext"]) {
    Object.defineProperty(globalThis, name, {
      configurable: true,
      value: class WebGLRenderingContext {},
    });
  }
});

import {
  fmtInterval,
  proofIndentClass,
  PROOF_INDENT_CAP,
  walkProofSteps,
} from "./Graph";
import type { EntityFact, ProofStep } from "../api";

const base: EntityFact = {
  id: "f",
  said_as: null,
  direction: "out",
  predicate_key: null,
  predicate_label: null,
  inferred: false,
  temporal: null,
  other_id: null,
  other_name: null,
  object_value: null,
  qualifiers: [],
  statement_qualifiers: [],
  valid_from: null,
  valid_to: null,
  holds_from: null,
  holds_to: null,
  valid_from_precision: null,
  valid_to_precision: null,
  confidence: 1,
  evidence_count: 1,
  stale: false,
  corrected: false,
  contested: null,
  last_evidence_time: null,
};

const isoDay = "2024-03-15T00:00:00Z";

describe("fmtInterval", () => {
  it("eternal 不画区间", () => {
    expect(fmtInterval({ ...base, temporal: "eternal", valid_from: isoDay, valid_to: isoDay })).toBe(
      "",
    );
  });

  it("event 画一个时刻（from == to 的 day 精度）", () => {
    expect(
      fmtInterval({
        ...base,
        temporal: "event",
        valid_from: isoDay,
        valid_from_precision: "day",
        valid_to: isoDay,
        valid_to_precision: "day",
      }),
    ).toBe("2024-03-15");
  });

  it("event 单 hour 精度按 from 时刻返回", () => {
    expect(
      fmtInterval({
        ...base,
        temporal: "event",
        valid_from: "2024-03-15T10:30:00Z",
        valid_from_precision: "minute",
        valid_to: "2024-03-15T10:30:00Z",
        valid_to_precision: "minute",
      }),
    ).toBe("2024-03-15T10:30Z");
  });

  it("event 没有 from 时退到 to", () => {
    expect(
      fmtInterval({
        ...base,
        temporal: "event",
        valid_from: null,
        valid_to: "2024-03-15T00:00:00Z",
        valid_to_precision: "day",
      }),
    ).toBe("2024-03-15");
  });

  it("event from/to 都没有时返回空串（抽取失败不画）", () => {
    expect(fmtInterval({ ...base, temporal: "event" })).toBe("");
  });

  it("state 正常区间：from ~ ongoing（valid_to 为 null）", () => {
    expect(
      fmtInterval({
        ...base,
        temporal: "state",
        valid_from: "2020-01-01T00:00:00Z",
        valid_from_precision: "day",
        valid_to: null,
        valid_to_precision: null,
      }),
    ).toBe("2020-01-01 ~ now");
  });

  it("state 闭环：from ~ to", () => {
    expect(
      fmtInterval({
        ...base,
        temporal: "state",
        valid_from: "2020-01-01T00:00:00Z",
        valid_from_precision: "day",
        valid_to: "2024-01-01T00:00:00Z",
        valid_to_precision: "day",
      }),
    ).toBe("2020-01-01 ~ 2024-01-01");
  });

  it("state endedUnknown：from ~ ended, date unknown", () => {
    expect(
      fmtInterval({
        ...base,
        temporal: "state",
        valid_from: "2020-01-01T00:00:00Z",
        valid_from_precision: "day",
        valid_to: null,
        valid_to_precision: "unknown",
      }),
    ).toBe("2020-01-01 ~ ended, date unknown");
  });

  it("state 什么都没有时返回空串", () => {
    expect(fmtInterval({ ...base, temporal: "state" })).toBe("");
  });
});

// The shape of the recursive proof tree (0030): the ProofStep the server's proof() returns now carries
// premises: ProofStep[], and the frontend flattens it into WalkedRow[] for the rendering component.
// Three things are asserted here:
//   1. //   1. Depth is monotone, and only goes +1 across a has_premises boundary (a leaf's premises are empty, so it stops)
//   2. //   2. A three-level nested tree of 9 nodes yields 9 rows, depth sequence [0,1,2,3,2,3,1,2,3],
//      i.e. DFS pre-order: parent → first child → first grandchild → … → first leaf → sibling →
//   3. //   3. Leaf rows have has_premises=false; a row that is not a leaf must be true
//       A
//      / \
//     B   C
//    / \   \
//   D   E   F
//   |   |   |
//   G   H   I   ← leaf (premises empty, walker does not descend)

function step(
  id: string,
  premises: ProofStep[] = [],
  partial: Partial<ProofStep> = {},
): ProofStep {
  return {
    seq: 0,
    fact_id: id,
    subject_id: id,
    subject: id,
    predicate_id: null,
    predicate: "→",
    object_id: null,
    object: id,
    valid_from: null,
    valid_to: null,
    confidence: 1,
    retracted: false,
    evidence: [],
    premises,
    ...partial,
  };
}

describe("walkProofSteps", () => {
  it("叶子（premises 为空）只有一个深度 0 的行", () => {
    const tree = [step("leaf")];
    const rows = walkProofSteps(tree);
    expect(rows).toHaveLength(1);
    expect(rows[0].depth).toBe(0);
    expect(rows[0].has_premises).toBe(false);
    expect(rows[0].fact_id).toBe("leaf");
  });

  it("三层嵌套的树产出 9 行，深度 0..3 单调", () => {
    const tree = [
      step("A", [
        step("B", [step("D", [step("G")]), step("E", [step("H")])]),
        step("C", [step("F", [step("I")])]),
      ]),
    ];
    const rows = walkProofSteps(tree);
    expect(rows.map((r) => r.fact_id)).toEqual([
      "A", "B", "D", "G", "E", "H", "C", "F", "I",
    ]);
    expect(rows.map((r) => r.depth)).toEqual([
      0, 1, 2, 3, 2, 3, 1, 2, 3,
    ]);
    // Depth never exceeds 4 (a `premises` edge adds only +1); never below 0;
    // within one row, the depth difference across `premises` can only be +1 (just descended) or non-positive (climbed back to an ancestor or stayed on the same level)
    for (const r of rows) {
      expect(r.depth).toBeGreaterThanOrEqual(0);
      expect(r.depth).toBeLessThanOrEqual(3);
    }
    // Entering premises adds +1; exiting does not force -1 (it may return to an ancestor in one step)
    for (let i = 1; i < rows.length; i++) {
      const diff = rows[i].depth - rows[i - 1].depth;
      expect(diff).toBeLessThanOrEqual(1);
    }
  });

  it("叶子行 has_premises=false；非叶子行必为 true", () => {
    const tree = [
      step("A", [step("B", [step("leaf")])]),
    ];
    const rows = walkProofSteps(tree);
    const byId = Object.fromEntries(rows.map((r) => [r.fact_id, r]));
    expect(byId["A"].has_premises).toBe(true);
    expect(byId["B"].has_premises).toBe(true);
    expect(byId["leaf"].has_premises).toBe(false);
  });

  it("深度起点偏移：depth=2 时整棵树每个节点的深度都比直接调用多 2", () => {
    const tree = [step("A", [step("B")])];
    const at0 = walkProofSteps(tree, 0);
    const at2 = walkProofSteps(tree, 2);
    expect(at0.map((r) => r.depth)).toEqual([0, 1]);
    expect(at2.map((r) => r.depth)).toEqual([2, 3]);
  });
});

// Indentation cap: `cn` is pure concatenation and the repo has no tailwind-merge, so when both `ml-4`
// and `ml-0` are emitted, stylesheet order decides which applies, which makes such a cap a sham. What is pinned here is
// 'past a certain depth, no indent class is emitted', not 'emit another class to override it'
describe("proofIndentClass", () => {
  it("封顶以内：带缩进、内边距与那条竖线", () => {
    for (let depth = 0; depth < PROOF_INDENT_CAP; depth++) {
      const cls = proofIndentClass(depth);
      expect(cls).toContain("ml-4");
      expect(cls).toContain("pl-3");
      expect(cls).toContain("border-l");
    }
  });

  it("到了封顶就不再发缩进类", () => {
    for (const depth of [PROOF_INDENT_CAP, PROOF_INDENT_CAP + 1, 12]) {
      const cls = proofIndentClass(depth);
      expect(cls).not.toContain("ml-4");
      expect(cls).not.toContain("pl-3");
      expect(cls).not.toContain("border-l");
    }
  });

  it("任何深度都不会同时发出互相冲突的两个类", () => {
    for (let depth = 0; depth <= 12; depth++) {
      const parts = proofIndentClass(depth).split(/\s+/).filter(Boolean);
      for (const [a, b] of [
        ["ml-4", "ml-0"],
        ["pl-3", "pl-0"],
        ["border-l", "border-l-0"],
      ]) {
        expect(parts.includes(a) && parts.includes(b)).toBe(false);
      }
      // Two values should not appear in the same attribute group either
      const margins = parts.filter((c) => /^ml-/.test(c));
      expect(margins.length).toBeLessThanOrEqual(1);
    }
  });

  it("每一层都留着 mt-1，封顶只去掉横向的缩进", () => {
    for (let depth = 0; depth <= 12; depth++) {
      expect(proofIndentClass(depth)).toContain("mt-1");
    }
  });
});