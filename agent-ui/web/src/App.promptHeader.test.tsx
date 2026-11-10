// @vitest-environment jsdom
/**
 * The pinned prompt header in the whole panel: a click on it lands the cursor on the prompt row it names and puts
 * that row at the top of the list, which ends the header; `zt` puts a row right under the header while it is shown.
 * jsdom lays nothing out, so the rows are stacked at planted heights in a 300px list whose rects follow `scrollTop`,
 * and the header's own rect is planted as 24px at the list's top (`MessageList.header.test.tsx` has the header's
 * own cases).
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { AgentUiState, Hello } from "./types";
import { noteUserScroll } from "./follow";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const realRect = HTMLElement.prototype.getBoundingClientRect;
beforeEach(() => {
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: () => {} } },
  };
  HTMLElement.prototype.getBoundingClientRect = function (this: HTMLElement) {
    return this.classList.contains("prompt-header") ? ({ top: 0, bottom: 24 } as DOMRect) : realRect.call(this);
  };
});
afterEach(() => {
  HTMLElement.prototype.getBoundingClientRect = realRect;
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

const LIVE_TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

// Prompt 0, two replies, prompt 3, four replies: eight rows of 100px.
function started() {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
  const state: AgentUiState = {
    ...initialState(),
    status: { kind: "running" },
    userPrompts: [
      { seq: 0, text: "first question" },
      { seq: 3, text: "second question" },
    ],
    transcript: [1, 2, 4, 5, 6, 7].map((seq) => ({ seq, text: `reply ${seq}` })),
  } as AgentUiState;
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state });
  return rendered;
}

const ROW = 100;
const VIEW = 300;

function fakeLayout(container: HTMLElement) {
  const list = container.querySelector(".message-list") as HTMLElement;
  const rows = Array.from(list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]'));
  expect(rows).toHaveLength(8);
  const total = rows.length * ROW;
  let scrollTop = total - VIEW;
  Object.defineProperty(list, "clientHeight", { value: VIEW, configurable: true });
  Object.defineProperty(list, "scrollHeight", { value: total, configurable: true });
  Object.defineProperty(list, "scrollTop", {
    configurable: true,
    get: () => scrollTop,
    set: (v: number) => {
      scrollTop = Math.max(0, Math.min(total - VIEW, v));
    },
  });
  list.getBoundingClientRect = () => ({ top: 0, bottom: VIEW }) as DOMRect;
  rows.forEach((row, i) => {
    row.getBoundingClientRect = () => ({ top: i * ROW - scrollTop, bottom: (i + 1) * ROW - scrollTop }) as DOMRect;
    row.querySelector<HTMLElement>(".row-body")!.style.lineHeight = "20px";
  });
  act(() => container.querySelector<HTMLElement>(".agent-ui-conversation")!.focus());
  fireEvent.scroll(list);
  return list;
}

/** The reader scrolls the list up to `top` (as a wheel would: the move is announced, then its scroll arrives). */
function readUp(list: HTMLElement, top: number) {
  noteUserScroll(list, "up");
  list.scrollTop = top;
  fireEvent.scroll(list);
}

const header = (c: HTMLElement) => c.querySelector<HTMLElement>(".prompt-header");
const current = (c: HTMLElement) => c.querySelector(".row-current .row-body")?.textContent;

describe("the pinned prompt header in the panel", () => {
  it("a click lands the cursor on the prompt row and puts it at the list's top, and the header goes", () => {
    const { container } = started();
    const list = fakeLayout(container);
    readUp(list, 410);
    expect(header(container)?.textContent).toContain("second question");
    expect(current(container)).not.toBe("second question");

    fireEvent.mouseDown(header(container)!);
    fireEvent.click(header(container)!);
    expect(current(container)).toBe("second question");
    expect(list.scrollTop).toBe(3 * ROW);
    fireEvent.scroll(list); // the engine's scroll event for that move
    expect(header(container)).toBeNull();
    // The view stays where the click put it: the move stopped following, so nothing snaps it to the end.
    expect(list.scrollTop).toBe(3 * ROW);
    expect(document.activeElement).toBe(container.querySelector(".agent-ui-conversation"));
  });

  it("zt puts the cursor's row right under the header, not beneath it", () => {
    const { container } = started();
    const list = fakeLayout(container);
    readUp(list, 200); // rows 2-4 show; the cursor is re-homed from the last row to row 4
    expect(header(container)?.textContent).toContain("first question");
    // zt brings the cursor's row to the top of what shows.
    const row = container.querySelector<HTMLElement>(".row-current")!;
    const index = Array.from(list.querySelectorAll('[data-nav-stop="row"]')).indexOf(row);
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "z" });
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "t" });
    expect(index).toBe(4);
    expect(list.scrollTop).toBe(index * ROW - 24);
  });
  it("zt with no header showing lands the row below the header its landing brings", () => {
    const { container } = started();
    const list = fakeLayout(container);
    readUp(list, 320); // the second prompt's row shows 80px at the top: its own row is on screen, so no header
    expect(header(container)).toBeNull();
    // 5zt: row 4, a reply under that prompt. At the list's top it would be the first row past the header's band,
    // which then names the prompt above it and covers the row's first 24px.
    for (const key of ["5", "z", "t"]) fireEvent.keyDown(document.activeElement ?? document.body, { key });
    expect(current(container)).toContain("reply 4");
    expect(list.scrollTop).toBe(4 * ROW - 24);
    fireEvent.scroll(list);
    expect(header(container)?.textContent).toContain("second question");
  });

  it("zt on a prompt row puts it at the list's top: no header will show over it", () => {
    const { container } = started();
    const list = fakeLayout(container);
    readUp(list, 200);
    expect(header(container)).not.toBeNull();
    for (const key of ["4", "z", "t"]) fireEvent.keyDown(document.activeElement ?? document.body, { key });
    expect(current(container)).toBe("second question");
    expect(list.scrollTop).toBe(3 * ROW);
  });
});
