import { describe, expect, it } from "vitest";
import { themeNames, themeVars } from "./themes";

const rgb = (hex: string): [number, number, number] =>
  [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16)) as [number, number, number];
function luminance(hex: string): number {
  const [r, g, b] = rgb(hex).map((v) => {
    const s = v / 255;
    return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}
function contrast(a: string, b: string): number {
  const [x, y] = [luminance(a), luminance(b)];
  return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
}

describe("preview themes", () => {
  it("sends warn and error held to 3:1 against both bg and chrome, as the panel is sent them", () => {
    for (const name of themeNames) {
      const v = themeVars(name);
      for (const signal of ["--nv-warn", "--nv-error"]) {
        expect(contrast(v[signal], v["--nv-bg"]), `${name} ${signal} on bg`).toBeGreaterThanOrEqual(3);
        expect(contrast(v[signal], v["--nv-chrome"]), `${name} ${signal} on chrome`).toBeGreaterThanOrEqual(3);
      }
    }
  });

  it("moves gruvbox-dark's error off the raw #fb4934, which is 2.56:1 on its chrome", () => {
    expect(contrast("#fb4934", "#504945")).toBeLessThan(3);
    const v = themeVars("gruvbox-dark");
    expect(v["--nv-error"]).not.toBe("#fb4934");
    expect(contrast(v["--nv-error"], v["--nv-chrome"])).toBeGreaterThanOrEqual(3);
  });

  it("leaves a colour that already passes both backgrounds as it was", () => {
    const v = themeVars("tokyonight-night");
    expect(v["--nv-warn"]).toBe("#e0af68");
    expect(v["--nv-error"]).toBe("#f7768e");
  });

  it("derives the warn and error tints from the guarded colours", () => {
    const raw = themeVars("tokyonight-night");
    const guarded = themeVars("gruvbox-dark");
    // gruvbox-dark's error tint is the bg mixed 9% toward the guarded error, not toward #fb4934.
    const mix = (a: string, b: string, t: number): string =>
      "#" + rgb(a).map((x, i) => Math.round(x + (rgb(b)[i] - x) * t).toString(16).padStart(2, "0")).join("");
    expect(guarded["--nv-error-tint"]).toBe(mix(guarded["--nv-bg"], guarded["--nv-error"], 0.09));
    expect(guarded["--nv-error-tint"]).not.toBe(mix(guarded["--nv-bg"], "#fb4934", 0.09));
    expect(raw["--nv-error-tint"]).toBe(mix(raw["--nv-bg"], "#f7768e", 0.09));
  });
});
