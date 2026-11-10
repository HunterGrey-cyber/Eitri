// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { resumeFollowing } from "./follow";
import * as jumplist from "./jumplist";
import type { AgentUiState } from "./types";

let posted: Record<string, unknown>[];
let writeText: ReturnType<typeof vi.fn>;
let resize: { callback: ResizeObserverCallback; targets: Element[] }[];
beforeEach(() => {
  vi.useFakeTimers();
  Element.prototype.scrollIntoView = vi.fn();
  posted = [];
  resize = [];
  vi.stubGlobal("ResizeObserver", class {
    record: typeof resize[number];
    constructor(callback: ResizeObserverCallback) { this.record = { callback, targets: [] }; resize.push(this.record); }
    observe(el: Element) { this.record.targets.push(el); }
    unobserve() {}
    disconnect() {}
  });
  (window as unknown as { webkit: unknown }).webkit = { messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } } };
  writeText = vi.fn();
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
});
afterEach(() => {
  cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals();
  delete (window as unknown as { webkit?: unknown }).webkit;
  delete (navigator as unknown as Record<string, unknown>).clipboard;
});
const dispatch = (payload: unknown) => act(() => window.__eitriDispatch!(JSON.stringify(payload)));
const tab = { id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null, title: null };
function started(state: Partial<AgentUiState> = {}) {
  const view = render(<App />);
  dispatch({ kind: "hello", backend: "legacy", projectDir: "/p", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [tab] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: { ...initialState(), status: { kind: "running" }, ...state } });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  const root = view.container.querySelector<HTMLElement>(".agent-ui-conversation")!;
  act(() => {
    for (const { callback, targets } of resize) for (const target of targets) {
      if (target.matches(".status-band, .band-measure")) callback([{ target, contentRect: { width: target.matches(".band-measure") ? 7.2 : 1400 } } as ResizeObserverEntry], {} as ResizeObserver);
    }
  });
  const key = (key: string, extra = {}) => fireEvent.keyDown(root, { key, shiftKey: /^[A-Z{}]$/.test(key), ...extra });
  const keys = (text: string) => { for (const ch of text) key(ch); };
  return { ...view, root, key, keys };
}
const bash: AgentUiState["toolCalls"][number] = { seq: 1, toolUseId: "b", name: "Bash", input: { command: "echo hello" }, result: { content: "hello", isError: false } };
const edit: AgentUiState["toolCalls"][number] = { ...bash, toolUseId: "e", name: "Edit", input: { file_path: "a.rs", old_string: "a\nx\nb", new_string: "A\nx\nB" } };


const prompt = (seq: number, text: string) => ({ seq, text });
const read = (seq: number) => ({ ...bash, seq, toolUseId: `read-${seq}`, name: "Read", input: { file_path: `file-${seq}.rs` } });
const current = (container: HTMLElement) => container.querySelector<HTMLElement>(".row-current")!;
const flash = (container: HTMLElement) => container.querySelector(".band-message")?.textContent;

it("]t lands on a collapsed run and names its first call, trimmed to 60 characters", () => {
  const path = "x".repeat(100);
  const { container, keys } = started({ userPrompts: [prompt(1, "start"), prompt(4, "end")],
    toolCalls: [{ ...read(2), input: { file_path: path } }, read(3)] });
  keys("gg]t");
  expect(current(container).dataset.rowKey).toBe("r-2");
  expect(flash(container)).toBe(`first call here: ${`Read: ${path}`.slice(0, 60)}`);
  keys("[t"); expect(flash(container)).toBe("no more tool calls above");
});

it("3]t counts a collapsed run once, and [t travels back", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start"), prompt(4, "middle"), prompt(6, "middle two")],
    toolCalls: [read(2), read(3), read(5), read(7)], transcript: [{ seq: 8, text: "done" }] });
  keys("gg3]t"); expect(current(container).dataset.rowKey).toBe("t-7");
  keys("2[t"); expect(current(container).dataset.rowKey).toBe("r-2");
});

it("]d lands on a denied call without answering anything", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start")],
    toolCalls: [{ ...bash, seq: 2, result: null, denied: { reasonType: "classifier", reason: "refused" } }] });
  keys("gg]d"); expect(current(container).dataset.rowKey).toBe("t-2");
  expect(posted.some((msg) => msg.type === "permission_response")).toBe(false);
  keys("]d"); expect(flash(container)).toBe("no more errors below");
});

it("]m then ]c opens the diff and updates yih's current hunk", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start")], toolCalls: [{ ...edit, seq: 2 }] });
  keys("gg]m]c");
  expect(current(container).dataset.rowKey).toBe("t-2");
  expect(current(container).querySelector('[data-hunk="1"]')).not.toBeNull();
  keys("yih"); expect(writeText).toHaveBeenLastCalledWith("-a\n+A");
  keys("]cyih"); expect(writeText).toHaveBeenLastCalledWith("-b\n+B");
  keys("[cyih"); expect(writeText).toHaveBeenLastCalledWith("-a\n+A");
});

it("]c crosses the last hunk into the next edit, and [c enters the previous row's last hunk", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start")], toolCalls: [
    { ...edit, seq: 2 }, { ...edit, seq: 3, toolUseId: "second", input: { file_path: "b.rs", old_string: "old", new_string: "new" } },
  ] });
  keys("gg3]cyih"); expect(current(container).dataset.rowKey).toBe("t-3");
  expect(writeText).toHaveBeenLastCalledWith("-old\n+new");
  keys("]c"); expect(flash(container)).toBe("no more hunks below");
  keys("[cyih"); expect(current(container).dataset.rowKey).toBe("t-2");
  expect(writeText).toHaveBeenLastCalledWith("-b\n+B");
  keys("2[c"); expect(flash(container)).toBe("no more hunks above");
  keys("yih"); expect(writeText).toHaveBeenLastCalledWith("-a\n+A");
});

it.each([["t", "tool calls"], ["d", "errors"], ["m", "edits"], ["c", "hunks"]])(
  "empty %s searches flash at both ends without moving", (letter, name) => {
    const { container, keys } = started({ userPrompts: [prompt(1, "only row")] });
    keys(`]${letter}`); expect(flash(container)).toBe(`no more ${name} below`);
    keys(`[${letter}`); expect(flash(container)).toBe(`no more ${name} above`);
    expect(current(container).dataset.rowKey).toBe("u-1");
  },
);

it("an overlarge count stops on the last available tool and flashes", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start")], toolCalls: [read(2)] });
  keys("gg3]t"); expect(current(container).dataset.rowKey).toBe("t-2");
  expect(flash(container)).toBe("no more tool calls below");
});

it("Ctrl+o returns to the departure after ]t", () => {
  const { container, keys, key } = started({ userPrompts: [prompt(1, "start")], toolCalls: [read(2)] });
  keys("gg]t"); expect(current(container).dataset.rowKey).toBe("t-2");
  key("o", { ctrlKey: true }); expect(current(container).dataset.rowKey).toBe("u-1");
});

it("{ and } skip long rows and count rows without adding jumps", () => {
  const { container, keys, key } = started({ userPrompts: [prompt(1, "start"), prompt(3, "third"), prompt(4, "fourth")],
    transcript: [{ seq: 2, text: "long line\n\n".repeat(200) }] });
  keys("gg}2}"); expect(current(container).dataset.rowKey).toBe("u-4");
  keys("2{"); expect(current(container).dataset.rowKey).toBe("m-2");
  key("o", { ctrlKey: true }); expect(current(container).dataset.rowKey).toBe("u-4");
  key("o", { ctrlKey: true }); expect(flash(container)).toBe("no older jump");
});


/** A planted layout whose rects follow the list's scroll, including lines rendered by expansion. */
function layout(container: HTMLElement, heights: number[], hunkOffset = 100) {
  const list = container.querySelector<HTMLElement>(".message-list")!;
  const rows = Array.from(list.querySelectorAll<HTMLElement>("[data-row-key]"));
  expect(rows).toHaveLength(heights.length);
  Object.defineProperty(list, "clientHeight", { value: 300, configurable: true });
  Object.defineProperty(list, "scrollHeight", { get: () => heights.reduce((a, b) => a + b, 0), configurable: true });
  let scrollTop = 0;
  Object.defineProperty(list, "scrollTop", { configurable: true, get: () => scrollTop,
    set: (top: number) => { scrollTop = Math.max(0, Math.min(top, list.scrollHeight - list.clientHeight)); } });
  const rect = (top: number, height: number) => ({ top, bottom: top + height, left: 0, right: 200, width: 200, height }) as DOMRect;
  const nativeRect = HTMLElement.prototype.getBoundingClientRect;
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (this: HTMLElement) {
    if (this === list) return rect(0, 300);
    if (this.matches(".prompt-header")) return rect(0, 24);
    const row = this.closest<HTMLElement>("[data-row-key]");
    const index = row === null ? -1 : rows.indexOf(row);
    if (index < 0) return nativeRect.call(this);
    const top = heights.slice(0, index).reduce((a, b) => a + b, 0) - list.scrollTop;
    if (this.matches("[data-hunk]")) {
      const hunk = Array.from(row!.querySelectorAll("[data-hunk]")).indexOf(this);
      return rect(top + hunkOffset + hunk * 160, 20);
    }
    return rect(top, heights[index]);
  });
  return list;
}

it("hunk landings reveal their first line below the pinned prompt, even on the same row", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start")], toolCalls: [{ ...edit, seq: 2 }],
    transcript: [{ seq: 3, text: "tail" }] });
  keys("gg]m");
  const list = layout(container, [100, 600, 400]);
  const header = document.createElement("div");
  header.className = "prompt-header";
  list.parentElement!.append(header);
  list.scrollTop = 220;
  keys("]c");
  const first = current(container).querySelector<HTMLElement>('[data-hunk="0"]')!;
  expect(first.getBoundingClientRect().top).toBeGreaterThanOrEqual(24);
  expect(first.getBoundingClientRect().bottom).toBeLessThanOrEqual(300);
  keys("]c");
  const second = current(container).querySelector<HTMLElement>('[data-hunk="1"]')!;
  expect(second.getBoundingClientRect().top).toBeGreaterThanOrEqual(24);
  expect(second.getBoundingClientRect().bottom).toBeLessThanOrEqual(300);
  keys("yih"); expect(writeText).toHaveBeenLastCalledWith("-b\n+B");
  header.remove();
});

it("{ enters a tall previous row at its head and } leaves it without scrolling inside", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start"), prompt(3, "tail")],
    transcript: [{ seq: 2, text: "tall\n\n".repeat(100) }] });
  const list = layout(container, [100, 1000, 100]);
  keys("G{");
  expect(current(container).dataset.rowKey).toBe("m-2");
  expect(current(container).getBoundingClientRect().top).toBeGreaterThanOrEqual(0);
  expect(current(container).getBoundingClientRect().top).toBeLessThan(100);
  expect(list.scrollTop).toBeLessThan(100);
  keys("}"); expect(current(container).dataset.rowKey).toBe("u-3");
});

it("hunk jumps skip unsuccessful and hunkless edits", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start")], toolCalls: [
    { ...edit, seq: 2, result: { content: "failed", isError: true } },
    { ...edit, seq: 3, result: null },
    { ...edit, seq: 4, name: "MultiEdit" },
    { ...edit, seq: 5 },
  ] });
  keys("gg]cyih"); expect(current(container).dataset.rowKey).toBe("t-5");
  expect(writeText).toHaveBeenLastCalledWith("-a\n+A");
});

it("hunk counts and copying retain Codex file boundaries", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start")], toolCalls: [{
    ...edit, seq: 2, name: "fileChange", result: { content: "applied", isError: false }, input: { changes: [
      { path: "first.rs", kind: "update", diff: "@@ -1 +1 @@\n-a\n+A\n" },
      { path: "second.rs", kind: "update", diff: "@@ -1 +1 @@\n-b\n+B\n" },
    ] },
  }] });
  keys("gg2]c"); expect(current(container).querySelectorAll("[data-hunk]")).toHaveLength(2);
  keys("yah"); expect(writeText).toHaveBeenLastCalledWith("--- a/second.rs\n+++ b/second.rs\n-b\n+B");
});

it("]t reveals hidden input, collapses whitespace and cuts by code point", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start"), prompt(4, "end")],
    toolCalls: [{ ...read(2), input: { file_path: "a\u202Eb\n\t" + "😀".repeat(70) } }, read(3)] });
  keys("gg]t");
  expect(flash(container)).toBe(`first call here: Read: a⟨U+202E⟩b ${"😀".repeat(43)}`);
});

it("hunk choices belong to their tab even when edit row keys collide", () => {
  const state = { ...initialState(), status: { kind: "running" as const }, userPrompts: [prompt(1, "start")], toolCalls: [{ ...edit, seq: 2 }] };
  const { container, keys } = started(state);
  keys("gg2]c");
  const tabs = [tab, { ...tab, id: 2, number: 2, label: "2 new" }];
  dispatch({ kind: "tabs", active: 2, tabs });
  dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: { ...state,
    toolCalls: [{ ...edit, seq: 2, input: { file_path: "other.rs", old_string: "c\nx\nd", new_string: "C\nx\nD" } }] } });
  keys("yih");
  expect(writeText).not.toHaveBeenCalled();
  expect(container.querySelector(".path-pick")?.textContent).toContain("hunk 2");
  keys("a"); expect(writeText).toHaveBeenLastCalledWith("-c\n+C");
  keys("]cyih"); expect(writeText).toHaveBeenLastCalledWith("-c\n+C");
  dispatch({ kind: "tabs", active: 1, tabs });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state });
  keys("yih"); expect(writeText).toHaveBeenLastCalledWith("-b\n+B");
});

it("replacing a session within the same tab clears its hunk choices", () => {
  const state = { ...initialState(), sessionId: "old", status: { kind: "running" as const },
    userPrompts: [prompt(1, "start")], toolCalls: [{ ...edit, seq: 2 }] };
  const { container, keys } = started(state);
  keys("gg2]cyih");
  expect(writeText).toHaveBeenLastCalledWith("-b\n+B");
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 2, state });
  keys("yih");
  expect(writeText).toHaveBeenLastCalledWith("-b\n+B");
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: { ...state, sessionId: "new",
    toolCalls: [{ ...edit, seq: 2, input: { file_path: "other.rs", old_string: "c\nx\nd", new_string: "C\nx\nD" } }] } });
  writeText.mockClear();
  keys("yih");
  expect(writeText).not.toHaveBeenCalled();
  expect(container.querySelector(".path-pick")?.textContent).toContain("hunk 2");
  keys("a");
  expect(writeText).toHaveBeenLastCalledWith("-c\n+C");
});

it.each([1, 2])("Ctrl+o returns after %i cross-row/same-row hunk jumps", (jumps) => {
  const { container, keys, key } = started({ userPrompts: [prompt(1, "start")], toolCalls: [{ ...edit, seq: 2 }] });
  keys("gg");
  const record = vi.spyOn(jumplist, "recordJump");
  keys("]c".repeat(jumps));
  expect(current(container).dataset.rowKey).toBe("t-2");
  expect(record).toHaveBeenCalledTimes(jumps);
  expect(record.mock.calls.at(-1)?.[1].key).toBe(jumps === 2 ? "t-2" : "u-1");
  // The jumplist stores rows and deduplicates the current row when returning.
  key("o", { ctrlKey: true }); expect(current(container).dataset.rowKey).toBe("u-1");
});

it("a cross-row hunk landing keeps the anchor below the effective top", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start")], toolCalls: [{ ...edit, seq: 2 }] });
  keys("gg");
  const list = layout(container, [600, 900]);
  const header = document.createElement("div");
  header.className = "prompt-header";
  list.parentElement!.append(header);
  keys("]c");
  expect(current(container).dataset.rowKey).toBe("t-2");
  const anchor = current(container).querySelector<HTMLElement>('[data-hunk="0"]')!;
  expect(anchor.getBoundingClientRect().top).toBeGreaterThanOrEqual(24);
  expect(anchor.getBoundingClientRect().bottom).toBeLessThanOrEqual(300);
  header.remove();
});

it("a hunk landing from the following tail survives the list's resize observer", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start")], toolCalls: [{ ...edit, seq: 2 }] });
  const heights = [100, 200];
  const list = layout(container, heights, 450);
  keys("G");
  act(() => resumeFollowing(list));
  expect(list.scrollTop).toBe(list.scrollHeight - list.clientHeight);
  heights[1] = 1000;
  keys("]c");
  const anchor = current(container).querySelector<HTMLElement>('[data-hunk="0"]')!;
  const observer = resize.find(({ targets }) => targets.includes(list))!;
  expect(observer).toBeDefined();
  act(() => observer.callback([{ target: list, contentRect: list.getBoundingClientRect(),
    borderBoxSize: [], contentBoxSize: [], devicePixelContentBoxSize: [] }], {} as ResizeObserver));
  expect(anchor.getBoundingClientRect().top).toBeGreaterThanOrEqual(24);
  expect(anchor.getBoundingClientRect().bottom).toBeLessThanOrEqual(300);
});

it("]c skips a waiting Edit card's preview and lands on the applied edit", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start")],
    pendingPermissions: [{ seq: 2, permissionId: "p", toolUseId: null, toolName: "Edit", input: edit.input }],
    toolCalls: [{ ...edit, seq: 3 }] });
  expect(container.querySelector('[data-row-key="p-2"] [data-hunk]')).not.toBeNull();
  keys("gg]cyih");
  expect(current(container).dataset.rowKey).toBe("t-3");
  expect(writeText).toHaveBeenLastCalledWith("-a\n+A");
  expect(posted.some((msg) => msg.type === "permission_response")).toBe(false);
});

it("ACP hunk jumps and copies follow rendered file and hunk order", () => {
  const { container, keys } = started({ userPrompts: [prompt(1, "start")], toolCalls: [{ ...edit, seq: 2, name: "acp:edit",
    input: { title: "change files", diffs: [
      { path: "first.rs", oldText: "a\nx\nb", newText: "A\nx\nB" },
      { path: "second.rs", oldText: null, newText: "new" },
    ] } }] });
  keys("gg3]c");
  expect(current(container).querySelectorAll("[data-hunk]")).toHaveLength(3);
  keys("yah"); expect(writeText).toHaveBeenLastCalledWith("--- a/second.rs\n+++ b/second.rs\n+new");
  keys("[cyih"); expect(writeText).toHaveBeenLastCalledWith("-b\n+B");
});
