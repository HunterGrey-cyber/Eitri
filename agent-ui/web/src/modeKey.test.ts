import { describe, expect, it } from "vitest";
import { bypassYesCounts, isModeCycleKey, KeyClock, LAGS_AGREE_MS, modeFixedMessage, modeKeyRoute, OTHER_CLOCK_LAG_MS } from "./modeKey";

const key = (over: Partial<Parameters<typeof isModeCycleKey>[0]> = {}) => ({
  key: "Tab", shiftKey: true, ctrlKey: false, altKey: false, metaKey: false, isComposing: false, ...over,
});

describe("isModeCycleKey", () => {
  it("is Shift+Tab and nothing else", () => {
    expect(isModeCycleKey(key())).toBe(true);
    expect(isModeCycleKey(key({ shiftKey: false }))).toBe(false);
    expect(isModeCycleKey(key({ ctrlKey: true }))).toBe(false);
    expect(isModeCycleKey(key({ altKey: true }))).toBe(false);
    expect(isModeCycleKey(key({ key: "ISO_Left_Tab" }))).toBe(false);
  });
  it("matches Shift+Tab as WebKitGTK delivers it from a real keyboard (ISO_Left_Tab)", () => {
    // WebKitGTK delivers key "Unidentified", code "Tab", keyCode 9.
    expect(isModeCycleKey(key({ key: "Unidentified", code: "Tab", keyCode: 9 }))).toBe(true);
    expect(isModeCycleKey(key({ key: "Unidentified", code: "Tab", keyCode: 9, shiftKey: false }))).toBe(false);
    expect(isModeCycleKey(key({ key: "Unidentified", code: "KeyQ", keyCode: 81 }))).toBe(false);
  });
  it("leaves a composing key to the input method", () => {
    expect(isModeCycleKey(key({ isComposing: true }))).toBe(false);
    expect(isModeCycleKey(key({ keyCode: 229 }))).toBe(false);
  });
});

describe("modeKeyRoute (no canSwitch; an ended or failed tab already in bypass is the exception)", () => {
  const base = { confirmOpen: false, chooserOpen: false, tabState: "not_started" as const, tabMode: "auto" as const };

  it("always cycles a not_started, starting, awaiting_trust or live tab", () => {
    for (const tabState of ["not_started", "starting", "awaiting_trust", "live"] as const) {
      expect(modeKeyRoute({ ...base, tabState })).toBe("cycle");
    }
  });

  it("fixes an ended or failed tab in auto (entering bypass there is refused)", () => {
    for (const tabState of ["ended", "failed"] as const) {
      expect(modeKeyRoute({ ...base, tabState })).toBe("fixed");
    }
  });

  it("still cycles an ended or failed tab that is already in bypass -- leaving it always works", () => {
    for (const tabState of ["ended", "failed"] as const) {
      expect(modeKeyRoute({ ...base, tabState, tabMode: "bypass" })).toBe("cycle");
    }
  });

  it("leaves an open prompt or chooser its own key", () => {
    expect(modeKeyRoute({ ...base, confirmOpen: true })).toBe("overlay");
    expect(modeKeyRoute({ ...base, chooserOpen: true, tabState: "live" })).toBe("overlay");
  });

  it("does nothing before the first tabs envelope", () => {
    expect(modeKeyRoute({ ...base, tabState: null, tabMode: null })).toBe("none");
  });
});

it("names the way back once a session has ended", () => {
  expect(modeFixedMessage("r")).toBe("the session has ended — r to start again");
  expect(modeFixedMessage("")).toBe("the session has ended — start a new one to change the mode");
});

describe("bypassYesCounts", () => {
  it("counts once the prompt has been on screen the guard, with no key at all since it opened", () => {
    expect(bypassYesCounts({ now: 1300, openedAt: 1000, lastKeyAt: -Infinity })).toBe(true);
  });
  it("cancels when the prompt has not been on screen the guard yet, even with no other key", () => {
    // 100ms after the envelope, no key before it: the on-screen rule alone fails.
    expect(bypassYesCounts({ now: 1100, openedAt: 1000, lastKeyAt: -Infinity })).toBe(false);
  });
  it("cancels when a key landed less than the guard before this one, even once on-screen long enough", () => {
    // 300ms after the envelope (on-screen rule OK), but another key 100ms before this one.
    expect(bypassYesCounts({ now: 1300, openedAt: 1000, lastKeyAt: 1200 })).toBe(false);
  });
  it("counts only once BOTH rules clear, at exactly the guard", () => {
    expect(bypassYesCounts({ now: 1250, openedAt: 1000, lastKeyAt: 1000 })).toBe(true);
    expect(bypassYesCounts({ now: 1249, openedAt: 1000, lastKeyAt: 1000 })).toBe(false);
  });
});

describe("KeyClock", () => {
  it("is the press for a key the page handles later", () => {
    expect(new KeyClock().pressedAt({ timeStamp: 1000 }, 1800)).toBe(1000);
  });

  it("is when it was handled where the stamp cannot be this clock", () => {
    const clock = new KeyClock();
    expect(clock.pressedAt({ timeStamp: 0 }, 1800)).toBe(1800);
    expect(clock.pressedAt({ timeStamp: -5 }, 1800)).toBe(1800);
    expect(clock.pressedAt({ timeStamp: 1_790_000_000_000 }, 1800)).toBe(1800);
  });

  it("does not take a repeat's stamp, which stands at the press while a key is held", () => {
    expect(new KeyClock().pressedAt({ timeStamp: 1000, repeat: true }, 1800)).toBe(1800);
  });

  it("keeps the press of a key held up by a stall of any length", () => {
    const clock = new KeyClock();
    expect(clock.pressedAt({ timeStamp: 900 }, 905)).toBe(900);
    const pressedDuringIt = 1100;
    const handled = pressedDuringIt + 10 * OTHER_CLOCK_LAG_MS;
    const now = clock.pressedAt({ timeStamp: pressedDuringIt }, handled);
    expect(now).toBe(pressedDuringIt);
    expect(bypassYesCounts({ now, openedAt: 1000, lastKeyAt: -Infinity })).toBe(false);
  });

  it("keeps it for the first key the page ever sees, too", () => {
    const now = new KeyClock().pressedAt({ timeStamp: 1100 }, 1100 + 10 * OTHER_CLOCK_LAG_MS);
    expect(bypassYesCounts({ now, openedAt: 1000, lastKeyAt: -Infinity })).toBe(false);
  });

  it("takes off a clock's offset once two keys show one, so a question stays answerable", () => {
    const hour = 3_600_000;
    const clock = new KeyClock();
    clock.pressedAt({ timeStamp: 5_000 }, 5_003 + hour);
    const now = clock.pressedAt({ timeStamp: 9_000 }, 9_001 + hour);
    expect(now).toBe(9_001 + hour);
    expect(bypassYesCounts({ now, openedAt: 8_000 + hour, lastKeyAt: 5_003 + hour })).toBe(true);
  });

  it("takes no offset from two keys held up by one stall, whose lags differ by the time between them", () => {
    const stall = 2 * OTHER_CLOCK_LAG_MS;
    const clock = new KeyClock();
    expect(clock.pressedAt({ timeStamp: 1_000 }, 1_000 + stall)).toBe(1_000);
    const second = 1_300;
    expect(clock.pressedAt({ timeStamp: second }, 1_000 + stall + 1)).toBe(second);
  });

  it("lets lags agree to LAGS_AGREE_MS, and no further", () => {
    const hour = 3_600_000;
    const agreeing = new KeyClock();
    agreeing.pressedAt({ timeStamp: 1_000 }, 1_000 + hour);
    expect(agreeing.pressedAt({ timeStamp: 2_000 }, 2_000 + hour + LAGS_AGREE_MS)).toBe(2_000 + hour);
    const apart = new KeyClock();
    apart.pressedAt({ timeStamp: 1_000 }, 1_000 + hour);
    expect(apart.pressedAt({ timeStamp: 2_000 }, 2_000 + hour + LAGS_AGREE_MS + 1)).toBe(2_000);
  });

  it("lowers an offset when a key lags less, so a press is never put after its own handling", () => {
    const offset = 3_600_000;
    const clock = new KeyClock();
    clock.pressedAt({ timeStamp: 1_000 }, 1_000 + offset + 1_000);
    clock.pressedAt({ timeStamp: 3_000 }, 3_000 + offset + 1_000);
    const handled = 4_500 + offset + 300;
    const now = clock.pressedAt({ timeStamp: 4_500 }, handled);
    expect(now).toBeLessThanOrEqual(handled);
    expect(now).toBe(handled);
    const paintedJustBefore = handled - 100;
    expect(bypassYesCounts({ now, openedAt: paintedJustBefore, lastKeyAt: 3_000 + offset })).toBe(false);
  });

  it("finds the offset through keys that are handled late by turns", () => {
    const offset = 3_600_000;
    const clock = new KeyClock();
    clock.pressedAt({ timeStamp: 5_000 }, 5_000 + offset);
    expect(clock.pressedAt({ timeStamp: 6_000 }, 6_000 + offset + LAGS_AGREE_MS + 5)).toBe(6_000);
    expect(clock.pressedAt({ timeStamp: 7_000 }, 7_000 + offset)).toBe(7_000 + offset);
  });

  it("marks the key that turns an offset on, and only that key", () => {
    const hour = 3_600_000;
    const clock = new KeyClock();
    clock.pressedAt({ timeStamp: 1_000 }, 1_000 + hour);
    expect(clock.turned).toBe(false);
    clock.pressedAt({ timeStamp: 2_000 }, 2_000 + hour);
    expect(clock.turned).toBe(true);
    clock.pressedAt({ timeStamp: 3_000 }, 3_000 + hour);
    expect(clock.turned).toBe(false);
    clock.pressedAt({ timeStamp: 4_000, repeat: true }, 4_000 + hour);
    expect(clock.turned).toBe(false);
  });

  it("takes no offset when the least lag is within the line, though this key's is past it", () => {
    const clock = new KeyClock();
    clock.pressedAt({ timeStamp: 1_000 }, 1_000 + OTHER_CLOCK_LAG_MS - 5);
    expect(clock.pressedAt({ timeStamp: 2_000 }, 2_000 + OTHER_CLOCK_LAG_MS + 5)).toBe(2_000);
    expect(clock.turned).toBe(false);
  });

  it("drops an offset for good once a key shows the clocks agree", () => {
    const hour = 3_600_000;
    const clock = new KeyClock();
    clock.pressedAt({ timeStamp: 1_000 }, 1_000 + hour);
    clock.pressedAt({ timeStamp: 2_000 }, 2_000 + hour);
    expect(clock.pressedAt({ timeStamp: 5_000 + hour }, 5_003 + hour)).toBe(5_000 + hour);
    expect(clock.pressedAt({ timeStamp: 6_000 + hour }, 6_000 + 2 * hour)).toBe(6_000 + hour);
  });

  it("makes a question not yet painted unanswerable", () => {
    expect(bypassYesCounts({ now: 5000, openedAt: Infinity, lastKeyAt: -Infinity })).toBe(false);
  });
});
