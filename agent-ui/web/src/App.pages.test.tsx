// @vitest-environment jsdom
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import App from "./App";
import { TYPING_GUARD_MS } from "./typingGuard";

let posted: Array<Record<string, unknown>>;
beforeEach(() => {
  vi.useFakeTimers();
  posted = [];
  Element.prototype.scrollIntoView = vi.fn();
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  delete (window as unknown as { webkit?: unknown }).webkit;
});
function dispatch(payload: unknown) {
  act(() => window.__eitriDispatch!(JSON.stringify(payload)));
}
const PAGES = [
  { id: "markdown", title: "Markdown", summary: "<b>Read files</b>" },
  { id: "agents-top", title: "Agents", summary: null },
];
function screen() {
  const rendered = render(<App />);
  dispatch({ kind: "hello", backend: "sidecar", projectDir: "/p", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, defaultMode: "auto", tabs: [{ id: 1, number: 1, label: "1 new", name: null, state: "not_started", mode: "auto", marker: null, pending: 0, resumable: true, failure: null, title: null }] });
  dispatch({ kind: "arrive" });
  dispatch({ kind: "pages", pages: PAGES });
  return { ...rendered, root: rendered.container.querySelector<HTMLElement>(".empty-tab")! };
}
function key(root: HTMLElement, value: string) {
  act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50));
  fireEvent.keyDown(root, { key: value });
}
function current(container: HTMLElement) {
  return container.querySelector('[aria-current="true"]');
}

it("draws Pages after the dashboard rows with literal summaries and no letter, and removes the group when empty", () => {
  const { container, getByText } = screen();
  const page = getByText("Markdown").closest(".dash-item")!;
  expect(page).not.toBeNull();
  expect(getByText("Pages")).toBeDefined();
  expect(page.textContent).toContain("<b>Read files</b>");
  expect(page.querySelector("b, kbd")).toBeNull();
  const rows = Array.from(container.querySelectorAll(".dash-item"));
  expect(rows.indexOf(page)).toBeGreaterThan(rows.findIndex((row) => row.textContent?.includes("Keys")));
  dispatch({ kind: "pages", pages: [] });
  expect(container.textContent).not.toContain("Pages");
  expect(container.textContent).not.toContain("Markdown");
});

it.each([["Enter", "load_page"], ["o", "page_line"]])("j/k reach Pages and %s sends %s", (pressed, type) => {
  const { root, container } = screen();
  key(root, "k");
  expect(current(container)?.textContent).toContain("Agents");
  key(root, "k");
  expect(current(container)?.textContent).toContain("Markdown");
  key(root, "k");
  expect(current(container)?.textContent).toContain("Keys");
  key(root, "j");
  expect(current(container)?.textContent).toContain("Markdown");
  key(root, pressed);
  act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50));
  expect(posted.filter((m) => m.type === type)).toEqual([{ type, id: "markdown" }]);
  expect(posted.some((m) => m.type === "send_message")).toBe(false);
  dispatch({ kind: "pages", pages: [PAGES[1]] });
  expect(container.textContent).not.toContain("Markdown");
});

it("w still opens All sessions from a page row", () => {
  const { root, container } = screen();
  key(root, "k");
  expect(current(container)?.textContent).toContain("Agents");
  key(root, "w");
  act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50));
  expect(posted.some((m) => m.type === "tab_verb" && m.verb === "choose")).toBe(true);
});

it("the prefix sheet lists Pages and their verbs without loading them", () => {
  const { container } = screen();
  dispatch({ kind: "open_keymap", opening: 1 });
  const sheet = container.querySelector('[role="dialog"][aria-label="Keys"]')!;
  expect(sheet.textContent).toContain("Pages");
  expect(sheet.textContent).toContain(":Eitri plugin load markdown");
  expect(sheet.textContent).toContain(":Eitri plugin load agents-top");
  expect(posted.some((m) => m.type === "load_page")).toBe(false);
});
