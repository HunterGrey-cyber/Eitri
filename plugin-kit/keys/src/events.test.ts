// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { blockedEvent, isEscape, shouldSkip } from "./events";

afterEach(() => document.body.replaceChildren());

describe("event guard", () => {
  it("respects an event consumed by the page", () => {
    const ev = new KeyboardEvent("keydown", { key: "j", cancelable: true });
    ev.preventDefault();
    expect(shouldSkip(ev, false)).toBe(true);
  });
  it.each([{ isComposing: true }, { keyCode: 229 }])("leaves composing Esc alone: %j", (init) => {
    expect(shouldSkip(new KeyboardEvent("keydown", { key: "Escape", ...init }), false)).toBe(true);
  });
  it.each([{ ctrlKey: true }, { altKey: true }, { metaKey: true }])("skips modified plain keys: %j", (init) => {
    expect(shouldSkip(new KeyboardEvent("keydown", { key: "j", ...init }), false)).toBe(true);
  });
  it("skips a physically held Super even without modifier flags", () => {
    expect(shouldSkip(new KeyboardEvent("keydown", { key: "j" }), true)).toBe(true);
  });
  it.each(["button", "a", "summary", "input", "select", "textarea", "div[role=button]",
    "div[role=tab]", "div[role=option]", "div[role=checkbox]", "div[role=switch]"])(
    "leaves Enter and Space on %s", (selector) => {
      const [tag, role] = selector.replace("]", "").split("[role=");
      const el = document.createElement(tag);
      if (role) el.setAttribute("role", role);
      el.tabIndex = 0;
      document.body.append(el);
      el.focus();
      for (const key of ["Enter", " "]) {
        expect(shouldSkip(new KeyboardEvent("keydown", { key }), false)).toBe(true);
      }
    },
  );
  it("Space on a focused button is blocked unless spaceToKeys", () => {
    const button = document.createElement("button");
    document.body.append(button);
    button.focus();
    const space = () => new KeyboardEvent("keydown", { key: " " });
    expect(blockedEvent(space(), false)).toBe(true);
    expect(blockedEvent(space(), false, false)).toBe(true);
    expect(blockedEvent(space(), false, true)).toBe(false);
    expect(shouldSkip(space(), false, true)).toBe(false);
  });
  it("Enter on a focused button is blocked either way", () => {
    const button = document.createElement("button");
    document.body.append(button);
    button.focus();
    for (const spaceToKeys of [false, true]) {
      expect(blockedEvent(new KeyboardEvent("keydown", { key: "Enter" }), false, spaceToKeys)).toBe(true);
    }
  });
  it("isEscape accepts Escape and Ctrl+[ only", () => {
    const ev = (init: KeyboardEventInit) => new KeyboardEvent("keydown", init);
    expect(isEscape(ev({ key: "Escape" }))).toBe(true);
    expect(isEscape(ev({ key: "Escape", shiftKey: true }))).toBe(true);
    expect(isEscape(ev({ key: "[", ctrlKey: true }))).toBe(true);
    for (const init of [
      { key: "[", ctrlKey: true, shiftKey: true }, { key: "[", ctrlKey: true, altKey: true },
      { key: "[", ctrlKey: true, metaKey: true }, { key: "[" }, { key: "Escape", ctrlKey: true },
      { key: "Escape", altKey: true }, { key: "Escape", metaKey: true }, { key: "]", ctrlKey: true },
    ]) expect(isEscape(ev(init)), JSON.stringify(init)).toBe(false);
  });
  it("allows Enter on a plain focusable div", () => {
    const el = document.createElement("div");
    el.tabIndex = 0;
    document.body.append(el);
    el.focus();
    expect(shouldSkip(new KeyboardEvent("keydown", { key: "Enter" }), false)).toBe(false);
  });
});
