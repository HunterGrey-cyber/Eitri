// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { shouldSkip } from "./events";

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
  it("allows Enter on a plain focusable div", () => {
    const el = document.createElement("div");
    el.tabIndex = 0;
    document.body.append(el);
    el.focus();
    expect(shouldSkip(new KeyboardEvent("keydown", { key: "Enter" }), false)).toBe(false);
  });
});
