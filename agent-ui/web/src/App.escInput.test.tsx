// @vitest-environment jsdom
/**
 * vim's `Esc i`, typed fast. `i`/`o`/`A` enter INPUT only on a key that stands alone, so the `i` of a word typed
 * into BROWSE does not open the box halfway through it; but no prose holds an Esc, and the `i` right after the Esc
 * that just left INPUT is a command. It used to be swallowed (the band said "i or Ctrl+j to type") whenever it came
 * within the guard window of that Esc. Both pages that have the two modes are covered: a conversation and the empty
 * tab. The guard's own rules, arrivals included, are `typingGuard.test.ts`'s.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { Hello } from "./types";
import { TYPING_GUARD_MS } from "./typingGuard";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

beforeEach(() => {
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: () => {} } },
  };
});

/** jsdom stamps each keydown with `Date.now()`; this moves that clock on as a person pausing would. */
let clockSkew = 0;
let clockSpy: { mockRestore: () => void } | null = null;
function keyGap(ms: number = TYPING_GUARD_MS + 50) {
  if (clockSpy === null) {
    const real = Date.now.bind(Date);
    clockSpy = vi.spyOn(Date, "now").mockImplementation(() => real() + clockSkew);
  }
  clockSkew += ms;
}
afterEach(() => {
  clockSpy?.mockRestore();
  clockSpy = null;
  clockSkew = 0;
});

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  account: null,
};

const TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

function conversation() {
  const { container } = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [TAB] });
  dispatch({
    kind: "snapshot", tab: 1, throughRevision: 1,
    state: { ...initialState(), status: { kind: "running" }, transcript: [{ seq: 1, text: "an answer" }] },
  });
  dispatch({ kind: "pane_focus", focused: true });
  const root = container.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const mode = () => container.querySelector<HTMLElement>("[data-testid=mode-block]")!.dataset.mode;
  return { container, root, mode };
}

describe("Esc then i at once", () => {
  it("in a conversation: the i right after the Esc that left INPUT goes back into the box", () => {
    const { container, root, mode } = conversation();
    keyGap();
    fireEvent.keyDown(root, { key: "i" });
    expect(mode()).toBe("input");
    const box = container.querySelector("textarea")!;
    fireEvent.keyDown(box, { key: "x" });
    fireEvent.keyDown(box, { key: "Escape" });
    expect(mode()).toBe("browse");
    // No gap: a person's `Esc i`.
    fireEvent.keyDown(root, { key: "i" });
    expect(mode()).toBe("input");
  });

  it("in a conversation: A right after the Esc does the same", () => {
    const { container, root, mode } = conversation();
    keyGap();
    fireEvent.keyDown(root, { key: "i" });
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "Escape" });
    fireEvent.keyDown(root, { key: "A", shiftKey: true });
    expect(mode()).toBe("input");
  });

  it("in a conversation: the i of a word typed into BROWSE is still swallowed", () => {
    const { root, mode } = conversation();
    keyGap();
    fireEvent.keyDown(root, { key: "h" });
    fireEvent.keyDown(root, { key: "i" });
    expect(mode()).toBe("browse");
  });

  it("in a conversation: an arrival between the Esc and the i refuses the i", () => {
    const { container, root, mode } = conversation();
    keyGap();
    fireEvent.keyDown(root, { key: "i" });
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "Escape" });
    dispatch({ kind: "arrive" });
    fireEvent.keyDown(root, { key: "i" });
    expect(mode()).toBe("browse");
  });

  it("in a conversation: Ctrl+[ (Esc) then i at once goes back into the box too", () => {
    const { container, root, mode } = conversation();
    keyGap();
    fireEvent.keyDown(root, { key: "i" });
    fireEvent.keyDown(container.querySelector("textarea")!, { key: "[", code: "BracketLeft", ctrlKey: true });
    expect(mode()).toBe("browse");
    fireEvent.keyDown(root, { key: "i" });
    expect(mode()).toBe("input");
  });

  it("in a conversation: an Esc pressed in BROWSE is no exception, even on its own", () => {
    const { root, mode } = conversation();
    keyGap();
    fireEvent.keyDown(root, { key: "Escape" });
    fireEvent.keyDown(root, { key: "i" });
    expect(mode()).toBe("browse");
  });

  it("on the empty tab: the i right after the Esc that left the box goes back into it", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, state: "not_started" }] });
    dispatch({ kind: "pane_focus", focused: true });
    const screen = container.querySelector<HTMLElement>(".empty-tab")!;
    if (container.querySelector("textarea") === null) {
      keyGap();
      fireEvent.keyDown(screen, { key: "i" });
    }
    const box = container.querySelector("textarea");
    expect(box, "the empty tab's box is open").not.toBeNull();
    fireEvent.keyDown(box!, { key: "x" });
    fireEvent.keyDown(box!, { key: "Escape" });
    expect(container.querySelector("textarea"), "Esc left the box").toBeNull();
    fireEvent.keyDown(container.querySelector<HTMLElement>(".empty-tab")!, { key: "i" });
    expect(container.querySelector("textarea"), "the i went back into it").not.toBeNull();
  });
});
