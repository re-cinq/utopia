import { describe, expect, it } from "vitest";
import { versionLabel } from "./versionLabel";

describe("versionLabel", () => {
  it("labels re.32 as re.32", () => {
    expect(versionLabel("re.32")).toEqual("re.32");
  });

  it("labels 0.1.0 as v0.1.0", () => {
    expect(versionLabel("0.1.0")).toEqual("v0.1.0");
  });
});
