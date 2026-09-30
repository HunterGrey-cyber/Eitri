import { describe, expect, it } from "vitest";
import { acceptsEnvelope, activeTabInfo, countedTabTarget, forgetClosed, markerGlyph, modePill, saveView, showTabBar, takeView, withoutHandoff } from "./tabs";
import type { TabViewState } from "./tabs";
import type { TabInfo, TabsEnvelope } from "./types";

const tab = (id: number, over: Partial<TabInfo> = {}): TabInfo => ({
  id, number: id, label: `${id} new`, name: null, state: "not_started", mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, title: null, ...over,
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
  it("treats the queue, the draft, the rule offers and the scratch state as the active tab's own", () => {
    for (const kind of ["queue", "draft", "queue_taken", "rule_offers", "scratch"]) {
      expect(acceptsEnvelope({ kind, tab: 1 }, 2), kind).toBe(false);
      expect(acceptsEnvelope({ kind, tab: 2 }, 2), kind).toBe(true);
    }
    for (const kind of ["history", "editor_context", "notice"]) expect(acceptsEnvelope({ kind }, 2), kind).toBe(true);
  });
});

describe("tab helpers", () => {
  it("names the mode the way the pill reads, with the cycle hint only when it is offered", () => {
    expect(modePill("auto", true)).toBe("⏵⏵ auto on (shift+tab to toggle)");
    expect(modePill("bypass", false)).toBe("⏵⏵ bypass on");
  });
  it("drops the hint and the word 'on' in short form, for the band (panel round 2 plan, Task 10)", () => {
    expect(modePill("auto", true, true)).toBe("⏵⏵ auto");
  });
  it("spells out what bypass means in short form too (v1, D8)", () => {
    expect(modePill("bypass", false, true)).toBe("⏵⏵ bypass permissions on");
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
    expect(activeTabInfo({ active: 2, tabs: [tab(1), tab(2, { name: "docs" })], defaultMode: "auto" })?.name).toBe("docs");
    expect(activeTabInfo(null)).toBeNull();
  });
});

describe("the per-tab view store", () => {
  const view = (cursor: number, unseenAfterSeq: number | null = null): TabViewState => ({
    cursor,
    mode: "browse",
    expanded: { k: true },
    scrollTop: 40 * cursor,
    atBottom: false,
    detailed: false,
    unseenAfterSeq,
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
  // Wave 3, Task 3: the unread pill's threshold is a `seq`, saved and handed back across a switch so
  // `MessageList`'s `unseenSeed` can seed it in -- round-tripped here the same way every other field
  // already is above; `null` (a tab left following, or never parked) round-trips too.
  it("round-trips the unread threshold (unseenAfterSeq)", () => {
    const store = new Map<number, TabViewState>();
    saveView(store, 1, view(3, 42));
    saveView(store, 2, view(7, null));
    expect(takeView(store, 1)?.unseenAfterSeq).toBe(42);
    expect(takeView(store, 2)?.unseenAfterSeq).toBeNull();
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

/* Task 3 (v1 picks, R2): vim's `{N}gt` / `{N}gT` (`:help gt`). The ids are deliberately not the
   numbers -- tab 5 is the bar's third -- so a target that returned the number would fail here. */
describe("countedTabTarget (vim {N}gt / {N}gT)", () => {
  const env: TabsEnvelope = { active: 2, tabs: [tab(1), tab(2), tab(5, { number: 3 })], defaultMode: "auto" };
  it("gt goes to the tab numbered N, or nowhere", () => {
    expect(countedTabTarget(env, "tab.next", 3)).toBe(5);
    expect(countedTabTarget(env, "tab.next", 9)).toBeNull();
  });
  it("gT goes N back from the active tab, wrapping", () => {
    expect(countedTabTarget(env, "tab.prev", 1)).toBe(1);
    expect(countedTabTarget(env, "tab.prev", 2)).toBe(5);
  });
  it("gT with a count past the tab count keeps wrapping round the bar", () => {
    // Three tabs, active the second: 4 back is one back, 7 back is one back too.
    expect(countedTabTarget(env, "tab.prev", 4)).toBe(1);
    expect(countedTabTarget(env, "tab.prev", 7)).toBe(1);
    expect(countedTabTarget(env, "tab.prev", 3)).toBe(2);
  });
  it("gT has no target when the active tab is not in the list", () => {
    expect(countedTabTarget({ ...env, active: 99 }, "tab.prev", 1)).toBeNull();
  });
});
