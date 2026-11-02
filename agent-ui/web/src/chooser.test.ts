import { describe, expect, it } from "vitest";
import { choosable, chooserRows, pickerStep, relativeWhen, resumeMode, tabStateWord, withChooserTitles } from "./chooser";
import type { ChooserEnvelope, TabInfo } from "./types";

const TAB: TabInfo = {
  id: 1, number: 1, label: "1 fix-parser", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, title: "write the docs",
};

const ENVELOPE: ChooserEnvelope = {
  open: [
    { tab: 1, label: "1 fix-parser", marker: "needs_input", pending: 2, resumable: true },
    { tab: 2, label: "2 legacy one", marker: null, pending: 0, resumable: false },
  ],
  records: [
    { providerSessionId: "aaaa1111-x", name: "docs", title: "write the docs", createdAt: "1", updatedAt: "2", heldElsewhere: false },
    { providerSessionId: "bbbb2222-x", name: null, title: "fix the picker", createdAt: "1", updatedAt: "2", heldElsewhere: true },
    { providerSessionId: "cccc3333-x", name: null, title: null, createdAt: "1", updatedAt: "2", heldElsewhere: false },
  ],
};
const TABS: TabInfo[] = [
  { ...TAB, id: 1, number: 1, label: "1 fix-parser", marker: "needs_input", pending: 2 },
  { ...TAB, id: 2, number: 2, label: "2 legacy one", marker: null, pending: 0, resumable: false, state: "not_started" },
];

describe("chooserRows", () => {
  it("lists New session first, then open tabs, then records, in the order Rust sent", () => {
    const rows = chooserRows(ENVELOPE, TABS, "");
    expect(rows.map((r) => r.kind)).toEqual(["new", "tab", "tab", "record", "record", "record"]);
  });
  it("lists Restore last session first while tabs are on offer, and drops it while filtering", () => {
    const offer = { labels: ["api", "docs"], bypass: 0 };
    expect(chooserRows(ENVELOPE, TABS, "", offer).map((r) => r.kind)).toEqual(["restore", "new", "tab", "tab", "record", "record", "record"]);
    expect(chooserRows(ENVELOPE, TABS, "fix", offer).some((r) => r.kind === "restore")).toBe(false);
    expect(chooserRows(ENVELOPE, TABS, "", { labels: [], bypass: 0 }).some((r) => r.kind === "restore")).toBe(false);
    expect(chooserRows(ENVELOPE, TABS, "").some((r) => r.kind === "restore")).toBe(false);
  });
  it("joins each open row to its TabInfo by id", () => {
    const rows = chooserRows(ENVELOPE, TABS, "");
    const row = rows.find((r) => r.kind === "tab" && r.tab.tab === 1);
    expect(row).toMatchObject({ kind: "tab", info: { id: 1, title: "write the docs" } });
  });
  it("a tab with no matching TabInfo (the defensive race) joins to null, not a crash", () => {
    const row = chooserRows(ENVELOPE, [], "").find((r) => r.kind === "tab" && r.tab.tab === 1);
    expect(row).toMatchObject({ kind: "tab", info: null });
  });
  it("drops New session while a filter narrows the list -- it never matches typed text", () => {
    const rows = chooserRows(ENVELOPE, TABS, "fix");
    expect(rows.some((r) => r.kind === "new")).toBe(false);
  });
  it("/ filters by what a row shows, case-insensitively", () => {
    expect(chooserRows(ENVELOPE, TABS, "PICK").map((r) => (r.kind === "record" ? r.record.providerSessionId : r.kind))).toEqual(["bbbb2222-x"]);
    expect(chooserRows(ENVELOPE, TABS, "fix").map((r) => r.kind)).toEqual(["tab", "record"]);
  });
  it("/ filters a record with both name and title by either field, not just the leading name", () => {
    // `recordLead`'s `name ?? title` short-circuit meant a
    // record with both fields set was searchable only by `name`, even though the placeholder
    // ("filter by name or title") and the row itself both show the title too.
    const named: ChooserEnvelope = {
      open: [],
      records: [{ providerSessionId: "renamed-0000", name: "notes", title: "fix the parser bug", createdAt: "1", updatedAt: "2", heldElsewhere: false }],
    };
    expect(chooserRows(named, [], "parser").map((r) => r.kind)).toEqual(["record"]);
    expect(chooserRows(named, [], "notes").map((r) => r.kind)).toEqual(["record"]);
  });
});

describe("choosable", () => {
  it("New session and open tabs are always choosable; a record open in another window is not", () => {
    expect(chooserRows(ENVELOPE, TABS, "").map(choosable)).toEqual([true, true, true, true, false, true]);
  });
});

describe("tabStateWord", () => {
  it("names a tab's state from its marker", () => {
    expect(tabStateWord({ ...TAB, marker: "working" })).toBe("running");
    expect(tabStateWord({ ...TAB, marker: "unread" })).toBe("done, unread");
    expect(tabStateWord({ ...TAB, state: "failed" })).toBe("failed");
  });
  it("a stopped state wins over a stale marker, and a not_started or missing tab reads as new", () => {
    expect(tabStateWord({ ...TAB, state: "ended", marker: "working" })).toBe("ended");
    expect(tabStateWord({ ...TAB, state: "not_started", marker: "unread" })).toBe("new");
    expect(tabStateWord(null)).toBe("new");
  });
  it("a tab whose start waits for the trust answer says so rather than idle", () => {
    expect(tabStateWord({ ...TAB, state: "awaiting_trust", marker: null })).toBe("trust?");
  });
  it("a live tab with no marker reads idle, and one holding a card reads waiting", () => {
    expect(tabStateWord({ ...TAB, marker: null })).toBe("idle");
    expect(tabStateWord({ ...TAB, marker: "needs_input", pending: 2 })).toBe("waiting");
  });
});

describe("relativeWhen", () => {
  it("words a record's time the way Claude Code's picker does", () => {
    const now = Date.parse("2026-09-26T12:00:00");
    expect(relativeWhen(now, String(now - 30_000))).toBe("just now");
    expect(relativeWhen(now, String(now - 12 * 60_000))).toBe("12 min ago");
    expect(relativeWhen(now, String(now - 3 * 3_600_000))).toBe("3 h ago");
    expect(relativeWhen(now, String(Date.parse("2026-09-25T20:00:00")))).toBe("yesterday");
    expect(relativeWhen(now, String(Date.parse("2026-09-24T09:00:00")))).toBe("Sep 24");
    expect(relativeWhen(now, "garbage")).toBe("garbage");
  });
});

describe("resumeMode", () => {
  it("resumes in the empty active tab's mode, else the default", () => {
    expect(resumeMode({ ...TAB, state: "not_started", mode: "bypass" }, "auto")).toEqual({ mode: "bypass", into: "this tab" });
    expect(resumeMode({ ...TAB, state: "live", mode: "bypass" }, "auto")).toEqual({ mode: "auto", into: "a new tab" });
  });
  it("no active tab yet also lands in a new tab, in the default mode", () => {
    expect(resumeMode(null, "bypass")).toEqual({ mode: "bypass", into: "a new tab" });
  });
});

/** The one place the four list-motion keys of a picker are named, shared by the chooser
 *  (its list and its filter box) and the `/model`/`/effort` pickers. */
describe("pickerStep", () => {
  it("names ↓ and Ctrl+n as a step down, ↑ and Ctrl+p as a step up", () => {
    expect(pickerStep({ key: "ArrowDown", ctrlKey: false })).toBe(1);
    expect(pickerStep({ key: "n", ctrlKey: true })).toBe(1);
    expect(pickerStep({ key: "ArrowUp", ctrlKey: false })).toBe(-1);
    expect(pickerStep({ key: "p", ctrlKey: true })).toBe(-1);
  });
  it("is null for every other key: a bare n or p is typed text, and j, k, Ctrl+j, Ctrl+k stay the caller's own", () => {
    for (const init of [
      { key: "n", ctrlKey: false },
      { key: "p", ctrlKey: false },
      { key: "j", ctrlKey: false },
      { key: "k", ctrlKey: false },
      { key: "j", ctrlKey: true },
      { key: "k", ctrlKey: true },
      { key: "ArrowLeft", ctrlKey: false },
      { key: "ArrowRight", ctrlKey: false },
      { key: "Enter", ctrlKey: false },
    ]) {
      expect(pickerStep(init), JSON.stringify(init)).toBeNull();
    }
  });
});

describe("withChooserTitles", () => {
  const record = (id: string, title: string | null) => ({
    providerSessionId: id, name: null, title, createdAt: "1", updatedAt: "2", heldElsewhere: false,
  });
  const envelope: ChooserEnvelope = { open: [], records: [record("s-1", "old"), record("s-2", null), record("s-3", "kept")] };

  it("replaces only the titles it names, keeping the rows and their order", () => {
    const next = withChooserTitles(envelope, [{ providerSessionId: "s-2", title: "the CLI's" }, { providerSessionId: "s-1", title: "new" }]);
    expect(next.records.map((r) => [r.providerSessionId, r.title])).toEqual([["s-1", "new"], ["s-2", "the CLI's"], ["s-3", "kept"]]);
    expect(next.open).toBe(envelope.open);
    expect(next.records[2]).toBe(envelope.records[2]);
  });

  it("adds no row for a session it does not list", () => {
    const next = withChooserTitles(envelope, [{ providerSessionId: "s-9", title: "elsewhere" }]);
    expect(next.records).toEqual(envelope.records);
  });
});
