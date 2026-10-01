// @vitest-environment jsdom
/**
 * The first key after a keyboard arrival. The owner, on an installed 0.2.0: after `Ctrl+l` from the
 * editor the first `v` did not enter CARET and a second `v` did.
 *
 * `arrive` and `focus_permission` do their landing in an effect, which runs only after React has
 * rendered the envelope. A key the page handles in between -- the shell moves GTK focus and sends the
 * envelope in the same instant, so the first key can reach the page before that render and its
 * effects have run -- is handled against the panel as it was drawn, and the landing used to run over
 * it afterwards: `exitRegion` ended the CARET the `v` had just started and `setMode("browse")` won.
 *
 * Each "before the landing" case delivers the envelope and the key inside ONE `act`, so the page
 * handles the key before React renders the envelope -- the order WebKitGTK produces whenever the key
 * lands in that gap. The same cases with the key after the landing are here too, as the control.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { AgentUiState, Hello } from "./types";
import { TYPING_GUARD_MS } from "./typingGuard";

afterEach(cleanup);

beforeAll(() => {
  // jsdom implements no layout, so MessageList's auto-scroll would throw on a missing method.
  Element.prototype.scrollIntoView = vi.fn();
});

beforeEach(() => {
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: () => {} } },
  };
});

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["auto", "bypass"],
  resumableSessions: [],
  expectedVerdandiRevision: null,
  account: null,
};

const LIVE_TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

function snapshotState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return { ...initialState(), status: { kind: "running" }, ...overrides };
}

/** jsdom takes each keydown's timestamp from `Date.now()`; this moves it past the typing guard's
 *  window, as a person pausing would, so two keys a test fires are not one burst. */
let clockSkew = 0;
let clockSpy: { mockRestore: () => void } | null = null;
function keyGap(ms: number = TYPING_GUARD_MS + 50) {
  if (clockSpy === null) {
    const real = Date.now.bind(Date);
    clockSpy = vi.spyOn(Date, "now").mockImplementation(() => real() + clockSkew);
  }
  clockSkew += ms;
}

/** jsdom has no `Selection.modify`, which CARET's block caret extends by one character with. This
 *  moves the focus by one character within its own text node -- enough for the region to start. */
let restoreModify: () => void = () => {};
beforeEach(() => {
  const proto = Selection.prototype as unknown as { modify?: (alter: string, direction: string, granularity: string) => void };
  const original = proto.modify;
  proto.modify = function (this: Selection, alter: string, direction: string) {
    const node = this.focusNode;
    if (node === null) return;
    const length = (node.textContent ?? "").length;
    const offset = direction === "forward" ? Math.min(length, this.focusOffset + 1) : Math.max(0, this.focusOffset - 1);
    if (alter === "move") this.collapse(node, offset);
    else this.setBaseAndExtent(this.anchorNode ?? node, this.anchorOffset, node, offset);
  };
  restoreModify = () => {
    if (original === undefined) delete proto.modify;
    else proto.modify = original;
  };
});

afterEach(() => {
  restoreModify();
  window.getSelection()?.removeAllRanges();
  clockSpy?.mockRestore();
  clockSpy = null;
  clockSkew = 0;
});

const TRANSCRIPT = [
  { seq: 1, text: "first answer" },
  { seq: 2, text: "second answer" },
  { seq: 3, text: "third answer" },
];

/** A live tab with the keys in the panel, then the keys leave for the editor (`Ctrl+h`), as they
 *  do before every `Ctrl+l` back. */
function startedThenLeft(overrides: Partial<AgentUiState> = {}) {
  const { container } = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 3, state: snapshotState({ transcript: TRANSCRIPT, ...overrides }) });
  dispatch({ kind: "pane_focus", focused: true });
  const root = container.querySelector(".agent-ui-conversation")! as HTMLElement;
  const mode = () => container.querySelector<HTMLElement>("[data-testid=mode-block]")!.dataset.mode;
  const currentRow = () => container.querySelector(".row-current")?.textContent ?? "";
  return { container, root, mode, currentRow };
}

function leave() {
  dispatch({ kind: "pane_focus", focused: false });
  keyGap();
}

/** What `shell` sends for `Ctrl+l` (`set_pane_focused(true)`, then `arrive`), and the page handling
 *  `key` before React has rendered either. */
function arriveWithKeyFirst(root: HTMLElement, envelope: unknown, key: { key: string; shiftKey?: boolean }) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify({ kind: "pane_focus", focused: true }));
    window.__eitriDispatch!(JSON.stringify(envelope));
    fireEvent.keyDown(root, key);
  });
}

describe("the first key after an arrival is not undone by the landing", () => {
  it("v handled before the arrive landing still enters CARET", () => {
    const { root, mode } = startedThenLeft();
    leave();
    arriveWithKeyFirst(root, { kind: "arrive" }, { key: "v" });
    expect(mode()).toBe("caret");
    expect(window.getSelection()?.toString()).not.toBe("");
  });

  it("V handled before the arrive landing still enters V-LINE", () => {
    const { root, mode } = startedThenLeft();
    leave();
    arriveWithKeyFirst(root, { kind: "arrive" }, { key: "V", shiftKey: true });
    expect(mode()).toBe("vline");
  });

  it("v handled before a card landing (focus_permission) still enters CARET", () => {
    const { root, mode } = startedThenLeft({
      pendingPermissions: [{ seq: 4, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "cargo fmt" } }],
    });
    leave();
    arriveWithKeyFirst(root, { kind: "focus_permission", tab: 1 }, { key: "v" });
    expect(mode()).toBe("caret");
  });

  it("j handled before the arrive landing keeps its move (a reader who had moved off the end)", () => {
    const { root, currentRow } = startedThenLeft();
    keyGap();
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "g" });
    expect(currentRow()).toContain("first answer");
    leave();
    arriveWithKeyFirst(root, { kind: "arrive" }, { key: "j" });
    expect(currentRow()).toContain("second answer");
  });

  it("control: with the landing rendered first, v enters CARET on the last row", () => {
    const { root, mode, currentRow } = startedThenLeft();
    leave();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    expect(mode()).toBe("browse");
    expect(currentRow()).toContain("third answer");
    keyGap();
    fireEvent.keyDown(root, { key: "v" });
    expect(mode()).toBe("caret");
  });

  it("an arrival with no key in between still lands: BROWSE on the last row from INPUT, the card when one waits", () => {
    const { container, root, mode, currentRow } = startedThenLeft();
    keyGap();
    fireEvent.keyDown(root, { key: "i" });
    expect(mode()).toBe("input");
    leave();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    expect(mode()).toBe("browse");
    expect(currentRow()).toContain("third answer");
    expect(container.querySelector("textarea")).toBeNull();
    cleanup();

    const withCard = startedThenLeft({
      pendingPermissions: [{ seq: 4, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "cargo fmt" } }],
    });
    keyGap();
    fireEvent.keyDown(withCard.root, { key: "g" });
    fireEvent.keyDown(withCard.root, { key: "g" });
    leave();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    expect(withCard.mode()).toBe("browse");
    expect(withCard.container.querySelector(".row-current")?.className).toContain("row-permission");
  });

  it("a key handled before an earlier arrival does not stop a later arrival from landing", () => {
    const { root, mode, currentRow } = startedThenLeft();
    leave();
    arriveWithKeyFirst(root, { kind: "arrive" }, { key: "v" });
    expect(mode()).toBe("caret");
    leave();
    expect(mode()).toBe("browse");
    keyGap();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    expect(mode()).toBe("browse");
    expect(currentRow()).toContain("third answer");
  });
});
