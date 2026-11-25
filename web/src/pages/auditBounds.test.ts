import { beforeAll, expect, it } from "vitest";
import { auditBounds } from "./KbSettings";

// October 24 in Shanghai is from 16:00 UTC on the 23rd to 16:00 UTC on the 24th
beforeAll(() => {
  process.env.TZ = "Asia/Shanghai";
});

it("filters the activity log by the days the reader picked, end day included", () => {
  expect(auditBounds("2026-10-24", "2026-10-24")).toEqual({
    since: "2026-10-23T16:00:00.000Z",
    until: "2026-10-24T16:00:00.000Z",
  });
  // The cutoff is month-end: the next day is the 1st of the next month
  expect(auditBounds("", "2026-10-31").until).toBe("2026-10-31T16:00:00.000Z");
  expect(auditBounds("", "")).toEqual({ since: undefined, until: undefined });
});
