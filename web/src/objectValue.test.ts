import { describe, expect, it } from "vitest";
import { fmtObjectValue } from "./objectValue";

describe("fmtObjectValue", () => {
  it("does not stick a unit onto a value that already carries it", () => {
    // Unadopted literal values are written down as they are, with the unit recorded in a separate cell: display must not turn them into 75.0%% / $2.22 $
    expect(fmtObjectValue({ value: "75.0%", unit: "%" })).toBe("75.0%");
    expect(fmtObjectValue({ value: "$2.22", unit: "$" })).toBe("$2.22");
    expect(fmtObjectValue({ value: "€30 million", unit: "€" })).toBe("€30 million");
    expect(fmtObjectValue({ value: "500 MW", unit: "MW" })).toBe("500 MW");
  });

  it("adds the unit when the value is bare", () => {
    expect(fmtObjectValue({ value: "75.0", unit: "%" })).toBe("75.0%");
    expect(fmtObjectValue({ value: "500", unit: "MW" })).toBe("500 MW");
    expect(fmtObjectValue({ value: 75, unit: "%" })).toBe("75%");
  });

  it("formats an adopted amount compactly with its symbol in front", () => {
    // Compact notation follows the runtime locale (en gives 5B, zh gives 50亿): only the symbol's position is pinned, and the number is shortened
    const big = fmtObjectValue({ value: 5_000_000_000, unit: "$" })!;
    expect(big.startsWith("$")).toBe(true);
    expect(big).not.toContain("5000000000");
    expect(big.length).toBeLessThan(8);
    expect(fmtObjectValue({ value: 42, unit: "$" })).toBe("$42");
  });

  it("falls back the way it always did", () => {
    expect(fmtObjectValue(null)).toBeNull();
    expect(fmtObjectValue({ value: true })).toBe("✓");
    expect(fmtObjectValue({ summary: "Revenue = sum(...)" })).toBe("Revenue = sum(...)");
    expect(fmtObjectValue({ odd: 1 })).toBe('{"odd":1}');
  });
});
