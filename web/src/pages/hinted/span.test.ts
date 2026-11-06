import { expect, it } from "vitest";
import { attr, sampleHints } from "./fixtures";
import { fmtSpan, overlaps, parseRanges, spanOf } from "./span";

it("reads a span from the attributes the roles point at", () => {
  const attrs = [attr("a", "begin", 12), attr("b", "finish", "30"), attr("c", "colour", 5)];
  expect(spanOf(attrs, sampleHints)).toEqual({ start: 12, end: 30 });
});

it("returns null when either end is missing or not a number", () => {
  expect(spanOf([attr("a", "begin", 12)], sampleHints)).toBeNull();
  expect(spanOf([attr("a", "begin", 12), attr("b", "finish", "soon")], sampleHints)).toBeNull();
});

it("formats a one-line span without a dash", () => {
  expect([fmtSpan({ start: 12, end: 12 }), fmtSpan({ start: 12, end: 30 })]).toEqual(["L12", "L12–L30"]);
});

it("parses comma-separated ranges and skips malformed pieces", () => {
  expect(parseRanges("12-30, x-4,44-50,7")).toEqual([{ start: 12, end: 30 }, { start: 44, end: 50 }]);
});

it("two ranges overlap when they share a line", () => {
  expect(overlaps({ start: 1, end: 5 }, { start: 5, end: 9 })).toBe(true);
  expect(overlaps({ start: 1, end: 4 }, { start: 5, end: 9 })).toBe(false);
});
