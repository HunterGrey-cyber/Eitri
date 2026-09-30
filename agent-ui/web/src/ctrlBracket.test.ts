// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent } from "@testing-library/react";
import { installCtrlBracketAsEscape, isCtrlBracket } from "./ctrlBracket";

/* R9 (idiom matrix E10; kbux pass 2026-09-29, row P15): WebKitGTK 2.52.6 delivers `Ctrl+[` as
   `key "["`, `code BracketLeft`, `ctrlKey` -- and nothing in the panel read that as Esc, which vim,
   nvim and every terminal do. */
function ctrlBracket(init: KeyboardEventInit = {}): KeyboardEvent {
  return new KeyboardEvent("keydown", { key: "[", code: "BracketLeft", ctrlKey: true, ...init });
}

describe("isCtrlBracket", () => {
  it("is true for a plain Ctrl+[", () => {
    expect(isCtrlBracket(ctrlBracket())).toBe(true);
  });

  it("is true for the shape WebKitGTK sends, and for a held key's repeat", () => {
    expect(isCtrlBracket(ctrlBracket({ code: "BracketLeft", repeat: true }))).toBe(true);
  });

  it.each([
    ["without Ctrl", { ctrlKey: false }],
    ["with Shift", { shiftKey: true }],
    ["with Alt", { altKey: true }],
    ["with Meta", { metaKey: true }],
    ["while an input method is composing", { isComposing: true }],
    ["with the legacy keyCode 229 some WebKit builds still report", { keyCode: 229 }],
  ] as const)("is false %s", (_name, init) => {
    expect(isCtrlBracket(ctrlBracket(init))).toBe(false);
  });

  it("is false for any other key, Ctrl held or not", () => {
    for (const key of ["]", "Escape", "a", "Control", "{"]) {
      expect(isCtrlBracket(ctrlBracket({ key, code: "" })), key).toBe(false);
    }
  });
});

describe("installCtrlBracketAsEscape", () => {
  let cleanups: Array<() => void> = [];
  afterEach(() => {
    for (const cleanup of cleanups) cleanup();
    cleanups = [];
    document.body.innerHTML = "";
  });

  /** An element on the page, with a spy listening on it -- the target a real key would land on. */
  function target(): { el: HTMLElement; seen: KeyboardEvent[] } {
    const el = document.createElement("div");
    el.tabIndex = 0;
    document.body.appendChild(el);
    const seen: KeyboardEvent[] = [];
    el.addEventListener("keydown", (event) => seen.push(event));
    return { el, seen };
  }

  function install(): () => void {
    const remove = installCtrlBracketAsEscape(document);
    cleanups.push(remove);
    return remove;
  }

  it("claims Ctrl+[ and hands the target exactly one keydown, a plain Escape", () => {
    install();
    const { el, seen } = target();
    const notPrevented = fireEvent.keyDown(el, { key: "[", code: "BracketLeft", ctrlKey: true });
    expect(notPrevented, "the Ctrl+[ itself is claimed").toBe(false);
    expect(seen).toHaveLength(1);
    expect(seen[0].key).toBe("Escape");
    expect(seen[0].code).toBe("Escape");
    // Plain: none of the original's modifiers ride along, or a handler reading `Ctrl+Esc` would
    // see a chord nobody pressed.
    expect([seen[0].ctrlKey, seen[0].shiftKey, seen[0].altKey, seen[0].metaKey]).toEqual([false, false, false, false]);
    expect(seen[0].isComposing).toBe(false);
    expect(seen[0].target).toBe(el);
  });

  it("dispatches the Escape so it bubbles to the page and can be cancelled, like a real one", () => {
    install();
    const { el } = target();
    const bubbled: KeyboardEvent[] = [];
    const onBody = (event: KeyboardEvent) => bubbled.push(event);
    document.body.addEventListener("keydown", onBody);
    cleanups.push(() => document.body.removeEventListener("keydown", onBody));
    fireEvent.keyDown(el, { key: "[", ctrlKey: true });
    expect(bubbled).toHaveLength(1);
    expect(bubbled[0].key).toBe("Escape");
    expect(bubbled[0].bubbles).toBe(true);
    expect(bubbled[0].cancelable).toBe(true);
    expect(bubbled[0].composed).toBe(true);
  });

  it("carries a held key's repeat flag over, so an auto-repeat is still a repeat", () => {
    install();
    const { el, seen } = target();
    fireEvent.keyDown(el, { key: "[", ctrlKey: true });
    fireEvent.keyDown(el, { key: "[", ctrlKey: true, repeat: true });
    expect(seen.map((event) => [event.key, event.repeat])).toEqual([
      ["Escape", false],
      ["Escape", true],
    ]);
  });

  it("stops the original, so no later listener sees a Ctrl+[ alongside the Escape", () => {
    install();
    const later: string[] = [];
    // Registered after the install, in the same tier: the shape of `onModeKey` in App.tsx.
    const record = (event: KeyboardEvent) => later.push(`${event.ctrlKey ? "Ctrl+" : ""}${event.key}`);
    document.addEventListener("keydown", record, true);
    cleanups.push(() => document.removeEventListener("keydown", record, true));
    const { el } = target();
    fireEvent.keyDown(el, { key: "[", ctrlKey: true });
    expect(later).toEqual(["Escape"]);
  });

  it("leaves every other key alone", () => {
    install();
    const { el, seen } = target();
    expect(fireEvent.keyDown(el, { key: "a" }), "a is not claimed").toBe(true);
    expect(fireEvent.keyDown(el, { key: "[" }), "a bare [ is not claimed").toBe(true);
    expect(fireEvent.keyDown(el, { key: "]", ctrlKey: true }), "Ctrl+] is not claimed").toBe(true);
    expect(fireEvent.keyDown(el, { key: "[", ctrlKey: true, shiftKey: true }), "Ctrl+Shift+[ is not claimed").toBe(true);
    expect(fireEvent.keyDown(el, { key: "[", ctrlKey: true, altKey: true }), "Ctrl+Alt+[ is not claimed").toBe(true);
    expect(fireEvent.keyDown(el, { key: "[", ctrlKey: true, metaKey: true }), "Ctrl+Meta+[ is not claimed").toBe(true);
    expect(fireEvent.keyDown(el, { key: "Escape" }), "a real Escape is passed on as it is").toBe(true);
    expect(seen.map((event) => [event.key, event.ctrlKey])).toEqual([
      ["a", false],
      ["[", false],
      ["]", true],
      ["[", true],
      ["[", true],
      ["[", true],
      ["Escape", false],
    ]);
  });

  it("leaves an input method's own Ctrl+[ to it, as it leaves Esc", () => {
    install();
    const { el, seen } = target();
    expect(fireEvent.keyDown(el, { key: "[", ctrlKey: true, isComposing: true })).toBe(true);
    expect(fireEvent.keyDown(el, { key: "[", ctrlKey: true, keyCode: 229 })).toBe(true);
    expect(seen.map((event) => event.key)).toEqual(["[", "["]);
  });

  it("does nothing once removed", () => {
    const remove = install();
    const { el, seen } = target();
    remove();
    expect(fireEvent.keyDown(el, { key: "[", ctrlKey: true })).toBe(true);
    expect(seen.map((event) => event.key)).toEqual(["["]);
  });

  it("re-dispatches on the element the key landed on, wherever that is", () => {
    install();
    const outer = document.createElement("div");
    const input = document.createElement("input");
    outer.appendChild(input);
    document.body.appendChild(outer);
    const onOuter = vi.fn();
    outer.addEventListener("keydown", onOuter);
    const onInput = vi.fn();
    input.addEventListener("keydown", onInput);
    fireEvent.keyDown(input, { key: "[", ctrlKey: true });
    expect(onInput).toHaveBeenCalledTimes(1);
    expect(onInput.mock.calls[0][0].key).toBe("Escape");
    expect(onOuter).toHaveBeenCalledTimes(1);
    expect(onOuter.mock.calls[0][0].target).toBe(input);
  });
});
