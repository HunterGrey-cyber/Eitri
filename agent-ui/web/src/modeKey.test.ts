import { describe, expect, it } from "vitest";
import { bypassYesCounts, isModeCycleKey, KeyClock, modeFixedMessage, modeKeyRoute, OTHER_CLOCK_LAG_MS } from "./modeKey";

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

  it("makes a question not yet painted unanswerable", () => {
    expect(bypassYesCounts({ now: 5000, openedAt: Infinity, lastKeyAt: -Infinity })).toBe(false);
  });
});
