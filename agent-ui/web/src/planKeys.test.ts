import { afterEach, describe, expect, it } from "vitest";
import { noteKey, reset } from "./heldSuper";
import { planKey, type PlanKeyZone } from "./planKeys";

const key = (name: string, extra: Partial<KeyboardEvent> & { getModifierState?: (k: string) => boolean } = {}, zone: PlanKeyZone = "actions") =>
  planKey({ key: name, keyCode: 0, repeat: false, metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, isComposing: false, ...extra }, zone);

afterEach(() => reset());

describe("plan dock keys", () => {
  it("Super, Hyper and AltGraph never activate an action", () => {
    for (const modifier of ["Super", "Hyper", "AltGraph"]) {
      const held = { getModifierState: (k: string) => k === modifier };
      expect(key("Enter", held)).toEqual({ type: "none" });
      expect(key("Enter", held, "feedback")).toEqual({ type: "none" });
      expect(key("Enter", { ...held, ctrlKey: true }, "feedback")).toEqual({ type: "none" });
    }
    expect(key("Enter", { metaKey: true })).toEqual({ type: "none" });
    expect(key("Enter", { altKey: true })).toEqual({ type: "none" });
    // The Super key's own keydown, which WebKitGTK reports where the chord's own event does not.
    noteKey("keydown", { key: "Super", code: "SuperLeft" });
    expect(key("Enter")).toEqual({ type: "none" });
  });
  it("a held key from before arrival does not activate", () => {
    expect(key("Enter", { repeat: true })).toEqual({ type: "swallow" });
    expect(key("Tab", { repeat: true })).toEqual({ type: "swallow" });
    expect(key("Enter", { repeat: true, ctrlKey: true }, "feedback")).toEqual({ type: "none" });
    // Scrolling may repeat.
    expect(key("j", { repeat: true })).toEqual({ type: "scroll", by: "line", step: 1 });
  });
  it("an input method composing decides nothing", () => {
    expect(key("Enter", { isComposing: true })).toEqual({ type: "none" });
    expect(key("Enter", { keyCode: 229 })).toEqual({ type: "none" });
  });
  it("no digit or letter is an action", () => {
    for (const name of ["1", "2", "3", "4", "y", "a", "n", "d", "D", " ", "p"]) expect(key(name)).toEqual({ type: "swallow" });
  });
  it("Tab, Shift+Tab, h and l move the focus", () => {
    expect(key("Tab")).toEqual({ type: "focus", step: 1 });
    expect(key("Tab", { shiftKey: true })).toEqual({ type: "focus", step: -1 });
    expect(key("Unidentified", { shiftKey: true, code: "Tab" })).toEqual({ type: "focus", step: -1 });
    expect(key("l")).toEqual({ type: "focus", step: 1 });
    expect(key("ArrowRight")).toEqual({ type: "focus", step: 1 });
    expect(key("h")).toEqual({ type: "focus", step: -1 });
    expect(key("ArrowLeft")).toEqual({ type: "focus", step: -1 });
  });
  it("j, k, Ctrl-d, Ctrl-u, gg and G scroll the plan", () => {
    expect(key("j")).toEqual({ type: "scroll", by: "line", step: 1 });
    expect(key("k")).toEqual({ type: "scroll", by: "line", step: -1 });
    expect(key("d", { ctrlKey: true })).toEqual({ type: "scroll", by: "half", step: 1 });
    expect(key("u", { ctrlKey: true })).toEqual({ type: "scroll", by: "half", step: -1 });
    expect(key("g")).toEqual({ type: "g" });
    expect(key("G", { shiftKey: true })).toEqual({ type: "scroll", by: "edge", step: 1 });
  });
  it("Ctrl+y and other chords are not the dock's", () => {
    expect(key("y", { ctrlKey: true })).toEqual({ type: "none" });
    expect(key("b", { ctrlKey: true })).toEqual({ type: "none" });
  });
  it("Enter activates, Esc leaves; in the field Ctrl+Enter sends and Esc goes back", () => {
    expect(key("Enter")).toEqual({ type: "activate" });
    expect(key("Enter", { shiftKey: true })).toEqual({ type: "swallow" });
    expect(key("Escape")).toEqual({ type: "browse" });
    expect(key("Enter", { ctrlKey: true }, "feedback")).toEqual({ type: "send" });
    expect(key("Enter", {}, "feedback")).toEqual({ type: "none" });
    expect(key("Escape", {}, "feedback")).toEqual({ type: "back" });
    expect(key("j", {}, "feedback")).toEqual({ type: "none" });
  });
});
