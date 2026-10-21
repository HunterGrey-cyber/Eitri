import { describe, expect, it } from "vitest";
import { DEFAULT_SETTINGS, prefixKey, settingsOf } from "./settings";

describe("settingsOf", () => {
  it("gives the defaults for nothing", () => {
    expect(DEFAULT_SETTINGS).toEqual({
      scrollStep: 60, smooth: true, prefixWait: true, whichKeyDelay: 200, hintChars: "sadfjklewcmpgh", findCase: "smart",
    });
    for (const wire of [undefined, null, "x", 3, [], {}]) expect(settingsOf(wire)).toEqual(DEFAULT_SETTINGS);
  });
  it("rejects non-plain objects and invalid characters", () => {
    for (const wire of [new Date(), Object.assign([], { scroll_step: 40 }), Object.create({ scroll_step: 40 })]) {
      expect(settingsOf(wire)).toEqual(DEFAULT_SETTINGS);
    }
    expect(settingsOf({ hint_chars: "ab\n" }).hintChars).toBe(DEFAULT_SETTINGS.hintChars);
    expect(prefixKey("Ctrl+b\n")).toBeNull();
  });
  it("reads the host's member", () => {
    expect(settingsOf({
      scroll_step: 40, smooth: false, prefix_wait: false, which_key_delay: null, hint_chars: "asdf", find_case: "match",
    })).toEqual({ scrollStep: 40, smooth: false, prefixWait: false, whichKeyDelay: null, hintChars: "asdf", findCase: "match" });
  });
  it("keeps the default for each bad field and reads the good ones beside it", () => {
    expect(settingsOf({
      scroll_step: 5, smooth: "no", prefix_wait: 1, which_key_delay: -1, hint_chars: "aa", find_case: "loud",
    })).toEqual(DEFAULT_SETTINGS);
    expect(settingsOf({ scroll_step: 401 }).scrollStep).toBe(60);
    expect(settingsOf({ scroll_step: 12.5 }).scrollStep).toBe(60);
    expect(settingsOf({ scroll_step: 10, smooth: "no" })).toEqual({ ...DEFAULT_SETTINGS, scrollStep: 10 });
    expect(settingsOf({ which_key_delay: 0 }).whichKeyDelay).toBe(0);
    expect(settingsOf({ which_key_delay: 5001 }).whichKeyDelay).toBe(200);
    expect(settingsOf({ hint_chars: "abC" }).hintChars).toBe("sadfjklewcmpgh");
    expect(settingsOf({ hint_chars: "a" }).hintChars).toBe("sadfjklewcmpgh");
    expect(settingsOf({ hint_chars: "abcdefghijklmnopqrstuvwxyz" }).hintChars).toBe("abcdefghijklmnopqrstuvwxyz");
  });
});
describe("prefixKey", () => {
  it("spells a Ctrl+letter prefix as the key a row would hold", () => {
    expect(prefixKey("Ctrl+b")).toBe("<C-b>");
    expect(prefixKey("Ctrl+a")).toBe("<C-a>");
    expect(prefixKey("Ctrl+d")).toBe("<C-d>");
  });
  it("gives nothing for any other prefix, since no generic default could equal it", () => {
    for (const label of [undefined, null, "", "Ctrl+Space", "Alt+a", "Ctrl+Shift+a", "F12", "Ctrl+B", "b"]) {
      expect(prefixKey(label)).toBeNull();
    }
  });
});
