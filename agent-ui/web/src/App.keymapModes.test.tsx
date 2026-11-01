// @vitest-environment jsdom
/**
 * The ? overlay's Shift+Tab row in the whole panel: it names the modes the active tab cycles through, so an
 * ACP tab, which has no manual mode, is not told about one. `tabs.test.ts` covers the filter and
 * `KeymapOverlay.test.tsx` covers the prop; this is the wiring between them, at both places `App` draws the
 * overlay (over a conversation, and over a tab whose session has not started).
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { Hello } from "./types";

afterEach(cleanup);

beforeEach(() => {
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: () => undefined } },
  };
  Element.prototype.scrollIntoView = vi.fn();
});

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

const HELLO: Hello = {
  backend: "sidecar",
  projectDir: "/home/user/project",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  account: "team",
};

const tab = (agent: string, state: "live" | "not_started") => ({
  id: 1, number: 1, label: "1 new", name: null, state, mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, title: null, agent,
});

type Layout = "conversation" | "start screen";

/** Renders `App` with one tab of `agent` in `layout` and opens the overlay; the overlay's Shift+Tab row text. */
function cycleRow(agent: string, layout: Layout): string {
  const { container } = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  if (layout === "conversation") {
    dispatch({ kind: "tabs", active: 1, tabs: [tab(agent, "live")] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: { ...initialState(), status: { kind: "running" } } });
  } else {
    dispatch({ kind: "tabs", active: 1, tabs: [tab(agent, "not_started")] });
  }
  if (layout === "conversation") {
    fireEvent.keyDown(container.querySelector<HTMLElement>(".agent-ui-conversation")!, { key: "?", shiftKey: true });
  } else {
    // The start screen has no `?` binding of its own; Rust's `open_keymap` envelope is what opens it there.
    dispatch({ kind: "open_keymap" });
  }
  const overlay = container.querySelector(".keymap-overlay");
  expect(overlay, `the overlay opens over the ${layout}`).not.toBeNull();
  const row = Array.from(overlay!.querySelectorAll("tr, li, div")).find(
    (el) => el.children.length <= 3 && /Cycle /.test(el.textContent ?? "") && /Shift\+Tab/.test(el.textContent ?? ""),
  );
  expect(row, "the overlay has a Shift+Tab row").toBeDefined();
  return row!.textContent ?? "";
}

describe.each<Layout>(["conversation", "start screen"])("the ? overlay over a %s", (layout) => {
  it("does not mention manual on an ACP tab: it cycles auto → bypass", () => {
    const overlay = cycleRow("acp:x", layout);
    expect(overlay).toContain("Cycle auto → bypass");
    expect(overlay).not.toContain("manual");
  });

  it("names manual on a Claude tab, so the ACP result is the filter and not a missing row", () => {
    expect(cycleRow("claude", layout)).toContain("Cycle manual → auto → bypass");
  });
});
