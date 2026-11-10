// @vitest-environment jsdom
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentUiState } from "./types";

const TAB = { id: 1, number: 1, label: "one", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: true, failure: null, title: null };
const TWO = [TAB, { ...TAB, id: 2, number: 2, label: "two" }];
const state = (overrides: Partial<AgentUiState> = {}): AgentUiState => ({ ...initialState(), status: { kind: "running" },
  userPrompts: [{ seq: 0, text: "start" }, { seq: 4, text: "another prompt" }],
  transcript: [{ seq: 1, text: "apple above" }, { seq: 2, text: "banana" }, { seq: 3, text: "apple below" }, { seq: 5, text: "last" }], ...overrides });
function dispatch(payload: unknown) { act(() => window.__eitriDispatch!(JSON.stringify(payload))); }
let posted: Record<string, unknown>[];
beforeEach(() => {
  vi.useFakeTimers();
  posted = [];
  window.webkit = { messageHandlers: { eitriAgent: { postMessage: (message) => posted.push(JSON.parse(message)) } } };
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.useRealTimers(); window.getSelection()?.removeAllRanges(); });
function setup(snapshot = state()) {
  const view = render(<App />);
  dispatch({ kind: "hello", backend: "legacy", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: TWO });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 10, state: snapshot });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50));
  const root = view.container.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const key = (key: string, props = {}, target: Element = root) => {
    act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50));
    fireEvent.keyDown(target, { key, ...props });
  };
  const current = () => view.container.querySelector<HTMLElement>(".row-current");
  const prompt = () => view.container.querySelector<HTMLInputElement>('[aria-label="Search the conversation"]')!;
  return { ...view, root, key, current, prompt };
}

function stubSelectionModify(): () => void {
  const proto = Selection.prototype as unknown as { modify?: (alter: string, direction: string, granularity: string) => void };
  const original = proto.modify;
  proto.modify = function (this: Selection, alter: string, direction: string, granularity: string) {
    const node = this.focusNode;
    if (node === null) return;
    const text = node.textContent ?? "";
    let offset = this.focusOffset;
    const startAnchorNode = this.anchorNode;
    const startAnchorOffset = this.anchorOffset;
    if (granularity === "character") {
      offset = direction === "forward" ? Math.min(text.length, offset + 1) : Math.max(0, offset - 1);
      // The line-end caret quirk: at a line end WebKit draws the line-break box, which
      // reads like vim's cursor past the end. A real engine still shows a one-character block caret
      // at the very last position, which needs real layout jsdom does not have. A block caret
      // (anchor === focus, both already at `text.length`) extending forward has nowhere left to move
      // within this text node -- pull the ANCHOR back by one instead, so the stub still produces the
      // one-character selection the caret needs, without claiming jsdom modelled WebKit's own rendering.
      if (
        alter === "extend" &&
        direction === "forward" &&
        offset === this.focusOffset &&
        startAnchorNode === node &&
        startAnchorOffset === this.focusOffset &&
        offset > 0
      ) {
        this.setBaseAndExtent(node, offset - 1, node, offset);
        return;
      }
    } else if (granularity === "word") {
      if (direction === "forward") {
        let i = offset;
        while (i < text.length && /\S/.test(text[i]!)) i++;
        while (i < text.length && /\s/.test(text[i]!)) i++;
        offset = i;
      } else {
        let i = offset;
        while (i > 0 && /\s/.test(text[i - 1]!)) i--;
        while (i > 0 && /\S/.test(text[i - 1]!)) i--;
        offset = i;
      }
    } else {
      // "line" / "paragraphboundary": this stub's rows are each one text node, so both collapse to
      // that node's own start/end -- the same unit `0`/`$` and V-LINE share.
      offset = direction === "forward" ? text.length : 0;
    }
    if (alter === "move") this.collapse(node, offset);
    else this.setBaseAndExtent(this.anchorNode ?? node, this.anchorOffset, node, offset);
  };
  return () => {
    if (original === undefined) delete proto.modify;
    else proto.modify = original;
  };
}

describe("directional transcript search", () => {
  it("narrows backward, wraps, accepts, repeats backward with n and reverses with N", () => {
    const { key, prompt, current, container } = setup();
    key("?", { shiftKey: true });
    expect(container.querySelector(".keymap-overlay")).toBeNull();
    expect(prompt().parentElement?.firstChild?.textContent).toBe("?");
    fireEvent.change(prompt(), { target: { value: "apple" } });
    expect(current()?.textContent).toContain("apple below");
    fireEvent.change(prompt(), { target: { value: "apple a" } });
    expect(current()?.textContent).toContain("apple above");
    fireEvent.change(prompt(), { target: { value: "apple" } });
    key("Enter", {}, prompt());
    expect(prompt()).toBeNull();
    key("n"); expect(current()?.textContent).toContain("apple above");
    key("n"); expect(current()?.textContent).toContain("apple below");
    key("N", { shiftKey: true }); expect(current()?.textContent).toContain("apple above");
    key("/");
    expect(prompt().parentElement?.firstChild?.textContent).toBe("/");
    fireEvent.change(prompt(), { target: { value: "apple" } });
    key("Enter", {}, prompt());
    key("n"); expect(current()?.textContent).toContain("apple below");
  });

  it("a hit on a tall row brings the matched line on screen below the pinned header, for / and for n", () => {
    const { key, prompt, current, container } = setup(state({
      transcript: [{ seq: 1, text: "needle-alpha heads a tall reply" }, { seq: 2, text: "banana" }, { seq: 3, text: "needle-alpha again" }],
    }));
    const list = container.querySelector<HTMLElement>(".message-list")!;
    const view = { top: 100, bottom: 400 };
    list.getBoundingClientRect = () => ({ ...view, left: 0, right: 500, width: 500, height: 300, x: 0, y: 100, toJSON() {} }) as DOMRect;
    // Each match is drawn 50 px into the conversation's content, the list scrolled far past it.
    const proto = Range.prototype as unknown as { getBoundingClientRect?: () => DOMRect };
    const had = Object.getOwnPropertyDescriptor(proto, "getBoundingClientRect");
    Object.defineProperty(proto, "getBoundingClientRect", {
      configurable: true,
      value: function () {
        const top = view.top + 50 - list.scrollTop;
        return { top, bottom: top + 18, left: 10, right: 90, width: 80, height: 18, x: 10, y: top, toJSON() {} } as DOMRect;
      },
    });
    try {
      list.scrollTop = 2000;
      key("/");
      fireEvent.change(prompt(), { target: { value: "needle" } });
      key("Enter", {}, prompt());
      expect(current()?.textContent).toContain("heads a tall reply");
      // The match's top now sits at the view's top (no header is drawn here): scrolled up by the 1950 px it was above.
      expect(list.scrollTop).toBe(50);
      list.scrollTop = 2000;
      key("n");
      expect(current()?.textContent).toContain("again");
      expect(list.scrollTop).toBe(50);
    } finally {
      if (had) Object.defineProperty(proto, "getBoundingClientRect", had);
      else delete proto.getBoundingClientRect;
    }
  });

  it("an accepted search records its origin once, despite many incremental matches", () => {
    const { key, prompt, current } = setup();
    key("?");
    for (const value of ["a", "ap", "apple", "apple a"]) fireEvent.change(prompt(), { target: { value } });
    key("Enter", {}, prompt());
    expect(current()?.textContent).toContain("apple above");
    key("o", { ctrlKey: true }); expect(current()?.textContent).toContain("last");
    key("i", { ctrlKey: true }); expect(current()?.textContent).toContain("apple above");
    key("o", { ctrlKey: true }); key("o", { ctrlKey: true });
    expect(current()?.textContent).toContain("last");
  });

  it("cancelling a search restores the origin without recording a jump", () => {
    const { key, prompt, current } = setup();
    key("g"); key("g");
    key("j");
    expect(current()?.textContent).toContain("apple above");
    key("?"); fireEvent.change(prompt(), { target: { value: "apple below" } });
    expect(current()?.textContent).toContain("apple below");
    key("Escape", {}, prompt());
    expect(current()?.textContent).toContain("apple above");
    key("j");
    expect(current()?.textContent).toContain("banana");
    key("o", { ctrlKey: true }); expect(current()?.textContent).toContain("last");
    key("i", { ctrlKey: true }); expect(current()?.textContent).toContain("banana");
  });

  it("the caret's row is the backward search origin, and the region ends", () => {
    const restore = stubSelectionModify();
    const { key, prompt, current, container } = setup();
    key("v");
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("caret");
    key("g"); key("g");
    key("?");
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    expect(window.getSelection()?.isCollapsed).toBe(true);
    expect(prompt().parentElement?.firstChild?.textContent).toBe("?");
    fireEvent.change(prompt(), { target: { value: "apple" } });
    expect(current()?.textContent).toContain("apple below");
    key("Escape", {}, prompt());
    expect(current()?.textContent).toContain("start");
    restore();
  });
});
