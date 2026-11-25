import { afterEach, describe, expect, it } from "vitest";
import { flattenOnLight, mix } from "./graphVisuals";

/* Both ends of a blend may be the `rgb(...)` read back from a token, not only `#rrggbb`.
   An end it cannot recognize used to silently become mid-grey, which is why the ring around a node looked dull. */
describe("mix", () => {
  it("mixes two hex colours", () => {
    expect(mix("#000000", "#ffffff", 0.5)).toBe("rgb(128,128,128)");
  });

  /* **The short form is produced by the build, not written by anyone.** The token holds `#ffffff`,
     but Lightning CSS compresses it to `#fff` when packing, and reading it back gives three digits. A parser that only recognizes six digits falls back
     to mid-grey here, and when one end of `mix(white, type colour, t)` becomes grey, every node turns into a grey lump in light mode.
     dev does not minify, so only the bundled output triggers it, which looks like a theme bug. */
  it("takes the short hex a minifier leaves behind", () => {
    expect(mix("#fff", "#000", 0.5)).toBe("rgb(128,128,128)");
    // An unrecognized value falls back to mid-grey, so white mixed with white is still 128: this is exactly what this case must catch
    expect(mix("#fff", "#fff", 0.5)).toBe("rgb(255,255,255)");
    expect(mix("#f00", "#ffffff", 0.5)).toBe("rgb(255,128,128)");
  });

  it("takes hex with alpha on either end", () => {
    expect(mix("#ffffffff", "#000000", 0.5)).toBe("rgb(128,128,128)");
    expect(mix("#fff8", "#ffffff", 0)).toBe("rgb(255,255,255)");
  });

  it("takes rgb() on either end, the way a token reads back", () => {
    // Red → white, halfway: only a recognized rgba gives 255,128,128; an unrecognized one falls back to 192,64,64
    expect(mix("#ff0000", "rgba(255,255,255,1)", 0.5)).toBe("rgb(255,128,128)");
    expect(mix("rgb(255,0,0)", "#ffffff", 0.5)).toBe("rgb(255,128,128)");
  });

  it("mixes toward ink on paper, not toward grey", () => {
    // In light mode INK is rgb(23,23,23): green moves 30% toward the ink, so green should darken rather than turn grey
    const ring = mix("#4ea172", "rgb(23,23,23)", 0.35);
    expect(ring).toBe("rgb(59,113,82)");
  });
});

/* In light mode, sigma's edge shader is **additive** blending (ONE, ONE_MINUS_SRC_ALPHA, and not premultiplied
   RGB): a semi-transparent colour cannot darken a line, it only adds to the paper. So the edge colour must, before handing it to sigma,
   be flattened against the background colour into an opaque rgb.
   This group touches the document (the theme marker + the background token), and by the house rules this test bench does not start a DOM
   (vite.config.ts: pure-logic unit tests). Two minimal stand-ins are enough: what is tested is "what it reads and what it computes",
   not the browser. */
describe("flattenOnLight", () => {
  const stub = (theme: string, ground = "250,250,250") => {
    const g = globalThis as Record<string, unknown>;
    g.document = { documentElement: { dataset: { theme } } };
    g.getComputedStyle = () => ({
      getPropertyValue: (n: string) => (n === "--u-ground-rgb" ? ground : ""),
    });
  };
  afterEach(() => {
    const g = globalThis as Record<string, unknown>;
    delete g.document;
    delete g.getComputedStyle;
  });

  it("leaves a colour alone outside the light theme", () => {
    stub("dark");
    expect(flattenOnLight("rgba(176,120,20,0.6)")).toBe("rgba(176,120,20,0.6)");
  });

  it("flattens rgba against the paper", () => {
    stub("light");
    expect(flattenOnLight("rgba(176,120,20,0.6)")).toBe("rgb(206,172,112)");
  });

  /* **The compressed form must also be recognized.** The token holds rgba(176,120,20,0.6), which the packer compresses to
     #b0781499; if only rgba(...) is recognized, it slips past entirely, and that derived edge becomes a bright yellow. */
  it("flattens the hex-with-alpha a minifier leaves behind", () => {
    stub("light");
    expect(flattenOnLight("#b0781499")).toBe("rgb(206,172,112)");
    expect(flattenOnLight("#1717175c")).not.toContain("#");
  });

  it("leaves an opaque colour as it is", () => {
    stub("light");
    expect(flattenOnLight("#e4e4e4")).toBe("#e4e4e4");
    expect(flattenOnLight("rgb(20,20,20)")).toBe("rgb(20,20,20)");
  });
});
