import { describe, expect, it } from "vitest";
import { isModeCycleKey, modeFixedMessage, modeKeyRoute } from "./modeKey";

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
    // Measured in the wave-4 GUI pass: key "Unidentified", code "Tab", keyCode 9.
    expect(isModeCycleKey(key({ key: "Unidentified", code: "Tab", keyCode: 9 }))).toBe(true);
    expect(isModeCycleKey(key({ key: "Unidentified", code: "Tab", keyCode: 9, shiftKey: false }))).toBe(false);
    expect(isModeCycleKey(key({ key: "Unidentified", code: "KeyQ", keyCode: 81 }))).toBe(false);
  });
  it("leaves a composing key to the input method", () => {
    expect(isModeCycleKey(key({ isComposing: true }))).toBe(false);
    expect(isModeCycleKey(key({ keyCode: 229 }))).toBe(false);
  });
});

describe("modeKeyRoute", () => {
  const base = { confirmOpen: false, chooserOpen: false, tabState: "not_started" as const, canSwitch: false };
  it("cycles an empty tab", () => expect(modeKeyRoute(base)).toBe("cycle"));
  it("says the mode is fixed once a session exists, without the switch capability", () => {
    for (const tabState of ["live", "ended", "failed"] as const) {
      expect(modeKeyRoute({ ...base, tabState })).toBe("fixed");
    }
  });
  // Wave 5: a starting tab always says so (W5) -- the fixed-mode text would be wrong on a
  // switch-capable sidecar, so it is never shown here regardless of `canSwitch`.
  it("says a starting session is starting, even without the switch capability", () =>
    expect(modeKeyRoute({ ...base, tabState: "starting" })).toBe("starting"));
  it("leaves an open prompt or chooser its own key", () => {
    expect(modeKeyRoute({ ...base, confirmOpen: true })).toBe("overlay");
    expect(modeKeyRoute({ ...base, chooserOpen: true, tabState: "live" })).toBe("overlay");
  });
  it("does nothing before the first tabs envelope", () => {
    expect(modeKeyRoute({ ...base, tabState: null })).toBe("none");
  });
});

describe("modeKeyRoute with a switch-capable session (wave 5)", () => {
  const base = { confirmOpen: false, chooserOpen: false, tabState: "live" as const, canSwitch: true };
  it("cycles a live tab whose sidecar can switch", () => expect(modeKeyRoute(base)).toBe("cycle"));
  it("keeps the fixed flash without the capability", () =>
    expect(modeKeyRoute({ ...base, canSwitch: false })).toBe("fixed"));
  it("cannot switch an ended or failed session", () => {
    for (const tabState of ["ended", "failed"] as const) expect(modeKeyRoute({ ...base, tabState })).toBe("fixed");
  });
  it("says a starting session is starting", () =>
    expect(modeKeyRoute({ ...base, tabState: "starting" })).toBe("starting"));
  it("still leaves an overlay its own key", () =>
    expect(modeKeyRoute({ ...base, chooserOpen: true })).toBe("overlay"));
});

it("names the way to a different mode", () => {
  expect(modeFixedMessage("C-b c")).toBe("mode is fixed for this session — C-b c for a new tab");
  expect(modeFixedMessage("")).toBe("mode is fixed for this session — open a new tab to choose");
});
