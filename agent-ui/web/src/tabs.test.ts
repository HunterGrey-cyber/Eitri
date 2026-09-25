import { describe, expect, it } from "vitest";
import { acceptsEnvelope, activeTabInfo, forgetClosed, markerGlyph, modePill, saveView, showTabBar, takeView, withoutHandoff } from "./tabs";
import type { TabViewState } from "./tabs";
import type { TabInfo } from "./types";

const tab = (id: number, over: Partial<TabInfo> = {}): TabInfo => ({
  id, number: id, label: `${id} new`, name: null, state: "not_started", mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, ...over,
});

describe("acceptsEnvelope", () => {
  it("drops a session envelope for any tab but the active one", () => {
    expect(acceptsEnvelope({ kind: "events", tab: 2 }, 2)).toBe(true);
    expect(acceptsEnvelope({ kind: "events", tab: 1 }, 2)).toBe(false);
    expect(acceptsEnvelope({ kind: "snapshot", tab: 1 }, 2)).toBe(false);
    expect(acceptsEnvelope({ kind: "focus_permission", tab: 1 }, 2)).toBe(false);
    expect(acceptsEnvelope({ kind: "error", tab: 3 }, 2)).toBe(false);
  });
  it("drops a session envelope with no tab, and one before any tabs envelope arrived", () => {
    expect(acceptsEnvelope({ kind: "snapshot" }, 2)).toBe(false);
    expect(acceptsEnvelope({ kind: "snapshot", tab: 1 }, null)).toBe(false);
  });
  it("always accepts window envelopes", () => {
    for (const kind of ["hello", "tabs", "theme", "pane_focus", "keymap", "chooser", "command_result", "hint_show"]) {
      expect(acceptsEnvelope({ kind }, null)).toBe(true);
    }
  });
});

describe("tab helpers", () => {
  it("names the mode the way the pill reads, with the cycle hint before a start", () => {
    expect(modePill("auto", false)).toBe("⏵⏵ auto on (shift+tab to cycle)");
    expect(modePill("bypass", true)).toBe("⏵⏵ bypass on");
  });
  it("draws one glyph per marker", () => {
    expect(markerGlyph("needs_input", 1)).toBe("⚑");
    expect(markerGlyph("needs_input", 3)).toBe("⚑3");
    expect(markerGlyph("ended", 0)).toBe("✕");
    expect(markerGlyph("unread", 0)).toBe("•");
    // working is TurnActivity's motion, not a glyph
    expect(markerGlyph("working", 0)).toBe("");
    expect(markerGlyph(null, 0)).toBe("");
  });
  it("shows the tab bar with two or more tabs, or while a rename is open", () => {
    expect(showTabBar(1, false)).toBe(false);
    expect(showTabBar(2, false)).toBe(true);
    expect(showTabBar(1, true)).toBe(true);
  });
  it("finds the active tab", () => {
    expect(activeTabInfo({ active: 2, tabs: [tab(1), tab(2, { name: "docs" })] })?.name).toBe("docs");
    expect(activeTabInfo(null)).toBeNull();
  });
});

describe("the per-tab view store", () => {
  const view = (cursor: number): TabViewState => ({
    cursor,
    mode: "browse",
    expanded: { k: true },
    scrollTop: 40 * cursor,
    atBottom: false,
    draft: `draft ${cursor}`,
  });
  it("gives each tab back what it left, and forgets closed tabs", () => {
    const store = new Map<number, TabViewState>();
    saveView(store, 1, view(3));
    saveView(store, 2, view(7));
    expect(takeView(store, 1)).toEqual(view(3));
    expect(takeView(store, 3)).toBeUndefined();
    forgetClosed(store, [2]);
    expect(takeView(store, 1)).toBeUndefined();
    expect(takeView(store, 2)?.cursor).toBe(7);
  });
});

describe("withoutHandoff", () => {
  it("ends only the named tab's handoff, and only for its own request when one is named", () => {
    const both: ReadonlyMap<number, string> = new Map([[1, "r1"], [2, "r2"]]);
    expect([...withoutHandoff(both, 1).entries()]).toEqual([[2, "r2"]]);
    expect([...withoutHandoff(both, 2, "r2").entries()]).toEqual([[1, "r1"]]);
    expect(withoutHandoff(both, 2, "an older request")).toBe(both);
    expect(withoutHandoff(both, 3)).toBe(both);
  });
});
