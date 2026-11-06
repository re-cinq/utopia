import { expect, it } from "vitest";
import { parseTones, toneOf } from "./tone";

it("maps ok to the success tone", () => {
  expect(toneOf("open", { tones: "open=ok,closed=danger" })).toBe("success");
});

it("a value not among the tones is neutral", () => {
  expect(toneOf("ajar", { tones: "open=ok,closed=danger" })).toBe("neutral");
});

it("whitespace around pairs is ignored", () => {
  expect([...parseTones(" open = ok , closed=warn ")]).toEqual([["open", "success"], ["closed", "warn"]]);
});

it("a malformed pair is skipped", () => {
  expect([...parseTones("open,=ok,closed=danger,a=b=c")]).toEqual([["closed", "danger"]]);
});
