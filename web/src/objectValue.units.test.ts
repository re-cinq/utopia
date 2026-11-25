import { describe, expect, it } from "vitest";
import { fmtObjectValue } from "./objectValue";

describe("adopted physical quantities", () => {
  it.each(["kg", "MW", "km", "m²"])("keeps %s after the numeric quantity", (unit) => {
    expect(fmtObjectValue({ value: 42, unit })).toBe(`42 ${unit}`);
  });
  it("keeps the digits of a small quantity read into a larger declared unit", () => {
    // An attribute read in units of 100 million yuan from "3.5 million yuan" is 0.035: with only two decimals it shows as 0.04, and "30 thousand yuan" became 0
    expect(fmtObjectValue({ value: 0.035, unit: "亿元" })).toBe("0.035 亿元");
    expect(fmtObjectValue({ value: 0.0003, unit: "亿元" })).toBe("0.0003 亿元");
  });
  it("does not expose floating-point noise from rule arithmetic", () => {
    expect(fmtObjectValue({ value: 0.1 + 0.2, unit: "kg" })).toBe("0.3 kg");
  });
  it("still places currency symbols before adopted amounts", () => {
    expect(fmtObjectValue({ value: 42, unit: "$" })).toBe("$42");
  });
});
