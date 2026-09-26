import { describe, expect, it } from "vitest";
import { PILL_HIDE_PX, PILL_SHOW_PX, pillLabel, pillShown } from "./pill";

describe("the R2 pill", () => {
  it("shows past 200px, hides within 60px, and never while following", () => {
    expect(pillShown(false, false, PILL_SHOW_PX)).toBe(false);
    expect(pillShown(false, false, PILL_SHOW_PX + 1)).toBe(true);
    expect(pillShown(true, false, 100), "hysteresis: still out beyond 60px").toBe(true);
    expect(pillShown(true, false, PILL_HIDE_PX)).toBe(false);
    expect(pillShown(true, true, 5000)).toBe(false);
  });

  it("names new rows, a card above all, and Claude Code's fallback", () => {
    expect(pillLabel(3, false)).toBe("↓ 3 new");
    expect(pillLabel(3, true)).toBe("↓ ⚑ approval");
    expect(pillLabel(0, false)).toBe("↓ Jump to bottom");
  });
});
