// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { exportView, importView, parseView } from "./tabs";
import type { TabViewState } from "./tabs";
import type { AgentUiState, Hello } from "./types";
import { CursorBand } from "./cursorBand";

// `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);

beforeAll(() => {
  // jsdom implements no layout, so MessageList's auto-scroll would throw on a missing method.
  Element.prototype.scrollIntoView = vi.fn();
});

/** Everything the page posts to Rust, in order. */
let posted: Array<Record<string, unknown>>;

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
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
  expectedVerdandiRevision: null,
  account: null,
};

const TAB = {
  number: 1, name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null,
  title: null, agent: "claude",
} as const;

function tabsOf(active: number, ids: number[]) {
  return { kind: "tabs", active, tabs: ids.map((id, at) => ({ ...TAB, id, number: at + 1, label: `${at + 1}` })) };
}

function rows(n: number): AgentUiState {
  return {
    ...initialState(),
    status: { kind: "running" },
    transcript: Array.from({ length: n }, (_, i) => ({ seq: i + 1, text: `row ${i + 1}` })),
  };
}

const MOVED: TabViewState = {
  cursor: 3,
  mode: "browse",
  expanded: {},
  scrollTop: 240,
  atBottom: false,
  detailed: false,
  unseenAfterSeq: null,
};

describe("a tab's view moving between the windows", () => {
  function arrive() {
    act(() => {
      for (const payload of [
        { kind: "view_import", tab: 7, view: { ...MOVED, atBottom: true } },
        tabsOf(7, [1, 7]),
        { kind: "snapshot", tab: 7, throughRevision: 6, state: rows(6) },
        { kind: "draft", tab: 7, text: "a draft that moves with its tab" },
      ]) window.__eitriDispatch!(JSON.stringify(payload));
    });
  }

  it("an_arrived_tab_shows_its_draft_and_cursor_in_the_render_that_shows_its_rows", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch(tabsOf(1, [1]));
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 6, state: rows(6) });
    arrive();
    expect(container.querySelector(".composer-draft")?.textContent).toBe("a draft that moves with its tab");
    expect(container.querySelector(".row-current")?.textContent).toContain("row 4");
  });

  it("the_band_does_not_glide_onto_an_arrived_tab", () => {
    const place = vi.spyOn(CursorBand.prototype, "place");
    try {
      const { container } = render(<App />);
      dispatch({ kind: "hello", ...HELLO });
      dispatch(tabsOf(1, [1]));
      dispatch({ kind: "snapshot", tab: 1, throughRevision: 6, state: rows(6) });
      fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "g" });
      fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "g" });
      expect(container.querySelector(".row-current")?.textContent).toContain("row 1");
      place.mockClear();
      arrive();
      expect(place.mock.calls.length).toBeGreaterThan(0);
      expect(place.mock.calls.at(-1)?.[1]).toBe(false);
    } finally {
      place.mockRestore();
    }
  });

  it("a_plain_switch_to_a_tab_left_at_the_end_still_lands_on_its_last_row", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch(tabsOf(1, [1, 7]));
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 6, state: rows(6) });
    fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "k" });
    dispatch(tabsOf(7, [1, 7]));
    dispatch({ kind: "snapshot", tab: 7, throughRevision: 6, state: rows(6) });
    dispatch(tabsOf(1, [1, 7]));
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 8, state: rows(8) });
    expect(container.querySelector(".row-current")?.textContent).toContain("row 8");
  });

  it("the_view_blob_restores_the_browse_cursor_and_the_scroll_anchor", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch(tabsOf(1, [1]));
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 5, state: rows(5) });
    // The tab arrives from the other window with the view its page there had of it, then is switched to.
    dispatch({ kind: "view_import", tab: 7, view: MOVED });
    dispatch(tabsOf(7, [1, 7]));
    dispatch({ kind: "snapshot", tab: 7, throughRevision: 6, state: rows(6) });
    const current = container.querySelector(".row-current");
    expect(current?.textContent).toContain("row 4");
    const list = container.querySelector<HTMLElement>(".message-list")!;
    expect(list.scrollTop).toBe(240);
  });

  it("answers view_export with the live view of the tab on screen, and with a parked one for another", () => {
    render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch(tabsOf(1, [1, 2]));
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 4, state: rows(4) });
    dispatch({ kind: "view_export", request: 9, tab: 1 });
    const live = posted.find((m) => m.type === "view_exported" && m.request === 9);
    expect(live).toBeDefined();
    expect(parseView(live!.view)).not.toBeNull();
    dispatch({ kind: "view_export", request: 10, tab: 2 });
    const parked = posted.find((m) => m.type === "view_exported" && m.request === 10);
    expect(parked?.view).toBeNull();
  });

  it("sends what was typed and not mirrored yet before its answer to view_export", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch(tabsOf(1, [1]));
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 4, state: rows(4) });
    fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "i" });
    fireEvent.change(container.querySelector("textarea")!, { target: { value: "half typed" } });
    // Asked well inside the mirror's debounce, as a move right after typing is.
    dispatch({ kind: "view_export", request: 11, tab: 1 });
    const draft = posted.findIndex((m) => m.type === "draft" && m.tab === 1 && m.text === "half typed");
    const answer = posted.findIndex((m) => m.type === "view_exported" && m.request === 11);
    expect(draft).toBeGreaterThanOrEqual(0);
    expect(answer).toBeGreaterThan(draft);
  });

  it("drops a moved view of any other shape, and the tab shows from its start", () => {
    const store = new Map<number, TabViewState>();
    for (const bad of [
      null, 7, "view", [], { ...MOVED, cursor: -1 }, { ...MOVED, cursor: 1.5 }, { ...MOVED, mode: "insert" },
      { ...MOVED, scrollTop: Number.NaN }, { ...MOVED, expanded: { a: "yes" } }, { ...MOVED, atBottom: 1 },
      { ...MOVED, unseenAfterSeq: "3" },
    ]) {
      expect(importView(store, 1, bad)).toBe(false);
    }
    expect(store.size).toBe(0);
    expect(importView(store, 1, { ...MOVED, extra: "ignored" })).toBe(true);
    expect(store.get(1)).toEqual(MOVED);
    expect(exportView(store, 2, 1, () => MOVED)).toEqual(MOVED);
    expect(exportView(store, 2, 3, () => MOVED)).toBeNull();
  });
});
