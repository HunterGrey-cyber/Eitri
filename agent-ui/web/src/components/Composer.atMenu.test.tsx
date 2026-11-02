// @vitest-environment jsdom
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Composer } from "./Composer";
import { builtInCompletions } from "../slashCommands";

type Props = Parameters<typeof Composer>[0];
beforeEach(() => vi.useFakeTimers());
afterEach(() => { cleanup(); vi.useRealTimers(); });
function setup(overrides: Partial<Props> = {}) {
  const onSend = vi.fn();
  const onAtQuery = vi.fn();
  const props: Props = { disabled: false, sessionEnded: false, closing: false, restoredDraft: null,
    mode: "input", onModeChange: vi.fn(), running: false, onSend, onAtQuery,
    slashCommands: builtInCompletions(), ...overrides };
  const view = render(<Composer {...props} />);
  const textarea = view.container.querySelector("textarea")!;
  function type(value: string, caret = value.length) {
    fireEvent.change(textarea, { target: { value } });
    textarea.setSelectionRange(caret, caret);
    fireEvent.select(textarea);
  }
  function wait() { act(() => vi.advanceTimersByTime(60)); }
  function reply(items: string[], seq = onAtQuery.mock.lastCall?.[1] as number) {
    view.rerender(<Composer {...props} atPaths={{ items, seq }} />);
  }
  function query(value: string, items: string[]) { type(value); wait(); reply(items); }
  return { ...view, props, textarea, type, wait, reply, query, onAtQuery, onSend };
}
const names = (c: HTMLElement) =>
  Array.from(c.querySelectorAll('[role="option"] .slash-menu-name')).map((r) => r.textContent);
const selectedName = (c: HTMLElement) => c.querySelector('[aria-selected="true"] .slash-menu-name')?.textContent;
const menu = (c: HTMLElement) => c.querySelector('[aria-label="Files"]');
const key = (el: HTMLTextAreaElement, value: string, extra = {}) => fireEvent.keyDown(el, { key: value, ...extra });

describe("path requests and replies", () => {
  it("debounces trailing queries for 60ms and drops stale replies", () => {
    const s = setup();
    s.type("@s"); act(() => vi.advanceTimersByTime(40)); s.type("@sm");
    act(() => vi.advanceTimersByTime(59)); expect(s.onAtQuery).not.toHaveBeenCalled();
    act(() => vi.advanceTimersByTime(1)); expect(s.onAtQuery).toHaveBeenLastCalledWith("sm", 1);
    s.reply(["small.txt"]); expect(names(s.container)).toEqual(["small.txt"]);
    s.type("@lo"); expect(menu(s.container)).toBeNull(); s.wait();
    s.reply(["long.txt", "blob.bin"], 2); expect(names(s.container)).toEqual(["long.txt", "blob.bin"]);
    s.reply(["small.txt"], 1); expect(menu(s.container)).toBeNull();
    s.reply([], 2); expect(menu(s.container)).toBeNull();
  });
  it("requests a bare @ and mentions anywhere, with no email, shell or slash requests", () => {
    const s = setup();
    for (const value of ["mail me@sm", "!cat @sm", "/cmd @sm"]) { s.type(value); s.wait(); }
    expect(s.onAtQuery).not.toHaveBeenCalled();
    s.query("@", [".env", "sub/", "small.txt"]);
    expect(names(s.container)).toEqual([".env", "sub/", "small.txt", "buffers", "quickfix", "diagnostics"]);
    s.type("first line\nlook @sm"); s.wait(); expect(s.onAtQuery).toHaveBeenLastCalledWith("sm", 2);
    s.type('@"with sp'); s.wait(); expect(s.onAtQuery).toHaveBeenLastCalledWith("with sp", 3);
  });
  it.each([{ claudeSlashCommands: false }, { disabled: true }, { editingInNvim: true }, { mode: "browse" as const }])(
    "withholds completion while another mode owns the draft: %j", (overrides) => {
      const s = setup(overrides); if (s.textarea !== null) { s.type("@sm"); s.wait(); }
      expect(s.onAtQuery).not.toHaveBeenCalled(); expect(menu(s.container)).toBeNull();
    });
  it("cancels a pending query for history search and the slash menu", () => {
    const s = setup(); s.type("@sm"); key(s.textarea, "r", { ctrlKey: true }); s.wait();
    expect(s.onAtQuery).not.toHaveBeenCalled(); cleanup();
    const other = setup(); other.type("/co"); other.wait(); expect(other.onAtQuery).not.toHaveBeenCalled();
  });
});

describe("the editor's own sources", () => {
  it("lead a query they start, with what each is, and complete as a word", () => {
    const s = setup(); s.query("@d", ["docs/", "d.txt"]);
    expect(names(s.container)).toEqual(["diagnostics", "docs/", "d.txt"]);
    expect(s.container.querySelector('[role="option"]')?.textContent).toContain("language server problems");
    key(s.textarea, "Tab"); expect(s.textarea.value).toBe("@diagnostics "); expect(menu(s.container)).toBeNull();
  });
  it("are not offered for a path, and Enter sends what was typed until a row is chosen", () => {
    const s = setup(); s.query("@sub/b", ["sub/b.txt"]); expect(names(s.container)).toEqual(["sub/b.txt"]);
    s.query("@bu", []); expect(names(s.container)).toEqual(["buffers"]);
    key(s.textarea, "Enter"); expect(s.onSend).toHaveBeenCalledWith("@bu");
  });
  it("are not offered for a quoted word, which names a file", () => {
    const s = setup(); s.query('@"buf', ["buffers"]); expect(names(s.container)).toEqual(["buffers"]);
    key(s.textarea, "Tab"); expect(s.textarea.value).toBe('@"buffers" ');
    expect(s.container.querySelector(".slash-menu-desc")).toBeNull();
    s.query('@"quick', []); expect(menu(s.container)).toBeNull();
  });
  it("still ask the host about files, since a file may start with the same letters", () => {
    const s = setup(); s.type("@q"); s.wait(); expect(s.onAtQuery).toHaveBeenLastCalledWith("q", 1);
    expect(names(s.container)).toEqual(["quickfix"]);
  });
});

describe("completing and moving", () => {
  it("Tab takes the first file, reuses one space, and preserves surrounding text and suffix", () => {
    const s = setup(); s.type("look @smeared here\nand below", 8); s.wait(); s.reply(["small.txt"]);
    key(s.textarea, "Tab"); expect(s.textarea.value).toBe("look @small.txt here\nand below");
    expect(menu(s.container)).toBeNull(); expect(s.onSend).not.toHaveBeenCalled();
  });
  it.each(["sub/", "my dir/"])("keeps a completed directory open: %s", (path) => {
    const s = setup(); s.query("@", [path]); key(s.textarea, "Tab");
    expect(s.textarea.value).toBe(path === "sub/" ? "@sub/" : '@"my dir/"');
    expect(menu(s.container)).not.toBeNull();
    s.wait(); s.reply([path, `${path}deep/`, `${path}inner.txt`, `${path}deep/leaf.txt`]);
    expect(names(s.container)).toHaveLength(4); expect(s.onAtQuery).toHaveBeenLastCalledWith(path, 2);
  });
  it("quotes a file holding spaces", () => {
    const s = setup(); s.query('@"with sp', ["with space.txt"]); key(s.textarea, "Tab");
    expect(s.textarea.value).toBe('@"with space.txt" '); expect(menu(s.container)).toBeNull();
  });
  it("Enter sends the literal draft until arrows have selected a row", () => {
    const s = setup(); s.query("@lo", ["long.txt", "blob.bin"]); key(s.textarea, "Enter");
    expect(s.onSend).toHaveBeenCalledWith("@lo"); s.query("@lo", ["long.txt", "blob.bin"]);
    key(s.textarea, "ArrowDown"); key(s.textarea, "Enter"); expect(s.textarea.value).toBe("@blob.bin ");
    expect(s.onSend).toHaveBeenCalledTimes(1);
  });
  it("wraps arrows and picker keys and windows rows with the selection", () => {
    const s = setup(); const paths = Array.from({ length: 12 }, (_, i) => `file${i}`); s.query("@", paths);
    expect(names(s.container)).toHaveLength(8); key(s.textarea, "ArrowUp");
    expect(selectedName(s.container)).toBe("diagnostics");
    key(s.textarea, "n", { ctrlKey: true });
    expect(selectedName(s.container)).toBe("file0");
    key(s.textarea, "Enter"); expect(s.textarea.value).toBe("@file0 ");
  });
  it("Esc stops before the panel and stays dismissed until text changes", () => {
    const onLineKey = vi.fn(); const s = setup({ onLineKey }); s.query("@sm", ["small.txt"]);
    const bubbled = vi.fn(); document.addEventListener("keydown", bubbled); key(s.textarea, "Escape");
    expect(bubbled).not.toHaveBeenCalled(); document.removeEventListener("keydown", bubbled); expect(onLineKey).toHaveBeenCalledOnce();
    expect(s.textarea.value).toBe("@sm"); expect(menu(s.container)).toBeNull();
    s.textarea.setSelectionRange(1, 1); fireEvent.select(s.textarea); s.wait(); expect(menu(s.container)).toBeNull();
    s.type("@sma"); s.wait(); s.reply(["small.txt"]); expect(menu(s.container)).not.toBeNull();
  });
  it("draws names as revealed text and takes a clicked row without losing focus", () => {
    const s = setup(); s.query("@", ["<b>x</b>", "hidden\u202Ename"]); expect(s.container.querySelector("b")).toBeNull();
    expect(names(s.container)[1]).toContain("U+202E"); fireEvent.click(s.container.querySelector('[role="option"]')!);
    expect(s.textarea.value).toBe("@<b>x</b> "); expect(document.activeElement).toBe(s.textarea);
  });
  it("leaves IME and modified Enter on their existing routes", () => {
    const s = setup(); s.query("@sm", ["small.txt"]); key(s.textarea, "Tab", { isComposing: true, keyCode: 229 });
    expect(s.textarea.value).toBe("@sm"); key(s.textarea, "ArrowDown"); key(s.textarea, "Enter", { ctrlKey: true });
    expect(s.onSend).toHaveBeenCalledWith("@sm");
  });
});
