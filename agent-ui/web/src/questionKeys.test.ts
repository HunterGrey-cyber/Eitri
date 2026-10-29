import { describe, expect, it } from "vitest";
import { noteKey, reset } from "./heldSuper";
import { questionKey, type QuestionKeyZone } from "./questionKeys";

const key = (key: string, extra: Partial<KeyboardEvent> = {}, zone: QuestionKeyZone = "option", preview = false) => questionKey({
  key, keyCode: 0, repeat: false, metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, isComposing: false, ...extra,
}, { zone, count: 4, preview });

describe("question keys", () => {
  it.each(["Enter", "Escape", "1", " ", "j", "Tab"])("ignores %s during IME composition and repeats", (name) => {
    expect(key(name, { isComposing: true })).toEqual({ type: "none" });
    expect(key(name, { keyCode: 229 })).toEqual({ type: "none" });
    expect(key(name, { repeat: true })).toEqual({ type: "swallow" });
  });
  it("digits focus and pick only a row in range", () => {
    for (let n = 1; n <= 4; n++) expect(key(String(n))).toEqual({ type: "number", index: n - 1 });
    expect(key("5")).toEqual({ type: "swallow" });
    expect(key("1", {}, "input")).toEqual({ type: "none" });
  });
  it.each(["j", "ArrowDown", "k", "ArrowUp"])("%s moves the highlighted row", (name) => {
    expect(key(name)).toEqual({ type: "move", step: ["j", "ArrowDown"].includes(name) ? 1 : -1 });
  });
  it("Space selects and Enter commits on keydown", () => {
    expect(key(" ")).toEqual({ type: "activate" });
    expect(key("Enter")).toEqual({ type: "commit" });
    expect(key("Enter", { shiftKey: true }, "input")).toEqual({ type: "none" });
  });
  it.each(["Tab", "l", "h", "ArrowRight", "ArrowLeft"])("%s navigates pages", (name) => {
    expect(key(name)).toEqual({ type: "question", step: ["h", "ArrowLeft"].includes(name) ? -1 : 1 });
    expect(key("Tab", { shiftKey: true })).toEqual({ type: "question", step: -1 });
  });
  it("Ctrl+Enter works in text fields and Ctrl+G only edits custom text", () => {
    expect(key("Enter", { ctrlKey: true }, "input")).toEqual({ type: "advance" });
    expect(key("g", { ctrlKey: true }, "custom")).toEqual({ type: "edit" });
    expect(key("g", { ctrlKey: true }, "input")).toEqual({ type: "edit" });
    expect(key("g", { ctrlKey: true })).toEqual({ type: "none" });
  });
  it("n opens preview notes alone", () => {
    expect(key("n", {}, "option", true)).toEqual({ type: "notes" });
    expect(key("n")).toEqual({ type: "none" });
    expect(key("n", {}, "input", true)).toEqual({ type: "none" });
  });
  it("Escape leaves a field first and then dismisses", () => {
    expect(key("Escape", {}, "input")).toEqual({ type: "leave" });
    expect(key("Escape")).toEqual({ type: "dismiss" });
  });
  it("text editing retains arrows, Space and newline, with Up leaving the field", () => {
    for (const name of ["ArrowLeft", "ArrowRight", "ArrowDown", " ", "a"]) expect(key(name, {}, "input")).toEqual({ type: "none" });
    expect(key("ArrowUp", {}, "input")).toEqual({ type: "leave" });
    expect(key("Home")).toEqual({ type: "edge", to: "first" });
    expect(key("End")).toEqual({ type: "edge", to: "last" });
    expect(key("a", {}, "custom")).toEqual({ type: "type" });
  });
  it("Super, Hyper and AltGraph never become an answer even when metaKey is false", () => {
    for (const modifier of ["Super", "Hyper", "AltGraph"]) {
      const event = { getModifierState: (name: string) => name === modifier };
      expect(key("Enter", event)).toEqual({ type: "none" });
      expect(key("Enter", { ...event, ctrlKey: true })).toEqual({ type: "none" });
    }
    noteKey("keydown", { key: "Super", code: "OSLeft" });
    expect(key("1")).toEqual({ type: "none" });
    reset();
  });

  it("window and card modifiers do not answer a question", () => {
    for (const modifiers of [{ metaKey: true }, { altKey: true }, { ctrlKey: true }]) {
      for (const name of ["1", "a", "d", "y", "c", "b"]) expect(key(name, modifiers)).toEqual({ type: "none" });
    }
  });
});
