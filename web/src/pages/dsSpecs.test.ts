import { describe, expect, it } from "vitest";

import { dsSpecs } from "./Settings";

/** Connection-string assembly for each engine tier.
 *
 * This pins down only one property: **it must not throw when no field has been filled in**.
 * #573 reported exactly this: right after choosing the "connection string" tier and before typing anything,
 * `build` called `.trim()` on `undefined`, and the whole page was replaced by an error screen.
 * The other tiers did not blow up back then, only because they interpolated `undefined` quietly through template strings.
 * So the assertion runs across all tiers together, and a newly added engine falls into this net automatically. */
describe("dsSpecs", () => {
  it("空表单不抛", () => {
    for (const spec of dsSpecs()) {
      expect(() => spec.build({}), `${spec.id} 在空表单上抛了`).not.toThrow();
    }
  });

  it("空表单只填了一部分也不抛", () => {
    for (const spec of dsSpecs()) {
      const half = Object.fromEntries(
        spec.fields.slice(0, 1).map((f) => [f.key, "x"]),
      );
      expect(() => spec.build(half), `${spec.id} 在半填表单上抛了`).not.toThrow();
    }
  });

  it("ClickHouse 的用户与库可以不填，端口默认 8123", () => {
    const ch = dsSpecs().find((s) => s.id === "clickhouse");
    expect(ch).toBeDefined();
    expect(ch!.build({ host: "ch.internal" })).toBe("clickhouse://ch.internal:8123");
    expect(ch!.build({ host: "ch.internal", password: "pw" })).toBe(
      "clickhouse://default:pw@ch.internal:8123",
    );
    expect(
      ch!.build({ host: "ch.internal", port: "8443", database: "sales", user: "ro", password: "p@ss" }),
    ).toBe("clickhouse://ro:p%40ss@ch.internal:8443/sales");
  });

  it("连接串那一档原样返回去掉首尾空白的输入", () => {
    const raw = dsSpecs().find((s) => s.id === "raw");
    expect(raw).toBeDefined();
    expect(raw!.build({ conn: "  postgres://u@h/db  " })).toBe("postgres://u@h/db");
  });
});
