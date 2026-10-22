import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, it, vi } from "vitest";
import { SourceDecayFields } from "./SourceDecayFields";
import { DEFAULT_DECAY } from "./sourceDecay";

const render = (enabled: boolean, value = DEFAULT_DECAY) =>
  renderToStaticMarkup(
    createElement(SourceDecayFields, {
      enabled,
      onEnabled: vi.fn(),
      value,
      onChange: vi.fn(),
    }),
  );

it("shows only the switch when decay is off", () => {
  const html = render(false);
  expect(html).toContain("Let documents decay");
  expect(html).not.toContain('type="number"');
});

it("shows the four numbers with their saved values when decay is on", () => {
  const html = render(true, { ...DEFAULT_DECAY, max_items: 3 });
  expect(html.match(/type="number"/g)).toHaveLength(4);
  expect(html).toContain('value="3"');
  expect(html).toContain("Keep at most (documents)");
});

it("marks the label of the field that is out of range", () => {
  const html = render(true, { ...DEFAULT_DECAY, half_life_days: 400 });
  expect(html).toMatch(/text-danger[^>]*>Score halves every/);
  expect(html).not.toMatch(/text-danger[^>]*>Keep at most/);
});
