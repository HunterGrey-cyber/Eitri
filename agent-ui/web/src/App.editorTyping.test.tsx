// @vitest-environment jsdom
/**
 * Owner decision #37 (revised 2026-09-29), the page's half: Rust's `editor_typing` envelope slows the
 * turn meter while the user types in the editor (`typingCadence.ts`). The attribute and the meter
 * step are Rust's word alone -- nothing else in the page sets or clears them -- so a tab switch, a
 * focus change or a snapshot must leave them exactly as Rust last said, and a page that goes away
 * must not leave them behind.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { AgentUiState } from "./types";

afterEach(cleanup);
afterEach(() => {
  document.documentElement.removeAttribute("data-editor-typing");
  document.documentElement.style.removeProperty("--meter-step");
});

beforeAll(() => {
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

const root = () => document.documentElement;
const typing = () => root().hasAttribute("data-editor-typing");
const step = () => root().style.getPropertyValue("--meter-step");

const TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

function snapshot(tab: number): unknown {
  const state: AgentUiState = { ...initialState(), status: { kind: "running" } };
  return { kind: "snapshot", tab, throughRevision: 1, state };
}

describe("editor_typing", () => {
  it("slows the meter while the editor is being typed in, and lets it go when Rust says so", () => {
    render(<App />);
    dispatch({ kind: "editor_typing", typing: true, periodMs: 500 });
    expect(typing()).toBe(true);
    expect(step()).toBe("500ms");
    dispatch({ kind: "editor_typing", typing: false, periodMs: 500 });
    expect(typing()).toBe(false);
    expect(step()).toBe("");
  });

  it("is not undone by anything else the page hears: a tab switch, a snapshot, the panel losing or gaining the keys", () => {
    render(<App />);
    dispatch({ kind: "tabs", active: 1, tabs: [TAB, { ...TAB, id: 2, number: 2, label: "2 new" }] });
    dispatch(snapshot(1));
    dispatch({ kind: "editor_typing", typing: true, periodMs: 1000 });
    // A switch to tab 2, its snapshot, and the panel's own focus changing: window-level state.
    dispatch({ kind: "tabs", active: 2, tabs: [{ ...TAB }, { ...TAB, id: 2, number: 2, label: "2 new" }] });
    dispatch(snapshot(2));
    dispatch({ kind: "pane_focus", focused: false });
    dispatch({ kind: "pane_focus", focused: true });
    expect(typing()).toBe(true);
    expect(step()).toBe("1000ms");
    // ...and only Rust's own "not typing" clears it, on either tab.
    dispatch({ kind: "editor_typing", typing: false, periodMs: 1000 });
    expect(typing()).toBe(false);
  });

  it("leaves nothing set by a message it cannot use", () => {
    render(<App />);
    dispatch({ kind: "editor_typing", typing: true, periodMs: 500 });
    dispatch({ kind: "editor_typing", typing: true, periodMs: null });
    expect(typing()).toBe(false);
    expect(step()).toBe("");
  });

  it("leaves nothing behind when the page goes away", () => {
    const { unmount } = render(<App />);
    dispatch({ kind: "editor_typing", typing: true, periodMs: 500 });
    expect(typing()).toBe(true);
    unmount();
    expect(typing()).toBe(false);
    expect(step()).toBe("");
  });
});
