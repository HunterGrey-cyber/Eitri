import { describe, expect, it } from "vitest";
import { BAND_BLEED_PX, bandBox } from "./MessageList";

const row = (offsetLeft: number, offsetWidth: number) =>
  ({ offsetLeft, offsetTop: 40, offsetWidth, offsetHeight: 20 }) as HTMLElement;

describe("bandBox", () => {
  it("runs the list's whole width when the margin is narrower than the bleed", () => {
    expect(bandBox(row(12, 336), 360)).toEqual({ left: 0, top: 40, width: 360, height: 20 });
  });

  it("covers the centred column and the bleed each side in a wide panel", () => {
    expect(bandBox(row(575, 770), 1920)).toEqual({
      left: 575 - BAND_BLEED_PX,
      top: 40,
      width: 770 + 2 * BAND_BLEED_PX,
      height: 20,
    });
  });

  it("never reaches past the list's right edge", () => {
    expect(bandBox(row(10, 400), 412).width).toBe(412);
  });
});
