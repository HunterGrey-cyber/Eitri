import { expect, it } from "vitest";
import { STYLE_TEXT } from "./style";

it("the_dimmed_outline_uses_the_inactive_ring", () => {
  const rule = STYLE_TEXT.split("}").find((rule) =>
    rule.startsWith(".eitri-keys-current.eitri-keys-dimmed{"));
  expect(rule).toBeDefined();
  expect(rule).toMatch(/outline-color:var\(--nv-focus-ring-inactive, #[0-9a-f]+\)/);
});
