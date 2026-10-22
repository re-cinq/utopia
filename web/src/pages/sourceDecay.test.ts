import { describe, expect, it } from "vitest";
import {
  DEFAULT_DECAY,
  decayChange,
  decayFormValue,
  decayIssue,
} from "./sourceDecay";

describe("decayFormValue", () => {
  it("fills every missing field with Lore's value when the saved object is {}", () => {
    expect(decayFormValue({})).toEqual(DEFAULT_DECAY);
  });

  it("keeps a saved field and fills the rest", () => {
    expect(decayFormValue({ max_items: 3 })).toEqual({ ...DEFAULT_DECAY, max_items: 3 });
  });

  it("is the defaults when the source does not decay", () => {
    expect(decayFormValue(null)).toEqual(DEFAULT_DECAY);
  });
});

describe("decayIssue", () => {
  it("is null for the defaults", () => {
    expect(decayIssue(DEFAULT_DECAY)).toBeNull();
  });

  it.each([
    [{ max_items: 0 }, "max_items"],
    [{ max_items: 2.5 }, "max_items"],
    [{ half_life_days: 0 }, "half_life_days"],
    [{ half_life_days: 366 }, "half_life_days"],
    [{ stale_after_days: 0 }, "stale_after_days"],
    [{ min_age_days: -1 }, "min_age_days"],
  ])("names the field when %j is out of range", (change, field) => {
    expect(decayIssue({ ...DEFAULT_DECAY, ...change })).toBe(field);
  });

  it("accepts 0 days for min_age_days and 365 for the half-life", () => {
    expect(decayIssue({ ...DEFAULT_DECAY, min_age_days: 0, half_life_days: 365 })).toBeNull();
  });
});

describe("decayChange", () => {
  it("sends nothing for a source that does not decay and stays off", () => {
    expect(decayChange(null, false, DEFAULT_DECAY)).toEqual({});
  });

  it("sends null when a decaying source is switched off", () => {
    expect(decayChange({ max_items: 3 }, false, DEFAULT_DECAY)).toEqual({ decay: null });
  });

  it("sends the whole object when decay is switched on", () => {
    expect(decayChange(null, true, DEFAULT_DECAY)).toEqual({ decay: DEFAULT_DECAY });
  });

  it("sends the whole object when one field changed", () => {
    const form = { ...DEFAULT_DECAY, max_items: 10 };
    expect(decayChange({ max_items: 3 }, true, form)).toEqual({ decay: form });
  });

  it("sends nothing when the form still equals what is saved, even a hand-written {}", () => {
    expect(decayChange({}, true, DEFAULT_DECAY)).toEqual({});
    expect(decayChange(DEFAULT_DECAY, true, DEFAULT_DECAY)).toEqual({});
  });
});
