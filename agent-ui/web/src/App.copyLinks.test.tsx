// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";

let posted: Array<Record<string, unknown>>;
let writeText: ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.useFakeTimers();
  Element.prototype.scrollIntoView = vi.fn();
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  writeText = vi.fn();
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.restoreAllMocks();
  delete (window as unknown as { webkit?: unknown }).webkit;
  delete (navigator as unknown as Record<string, unknown>).clipboard;
});

function dispatch(payload: unknown) {
  act(() => window.__eitriDispatch!(JSON.stringify(payload)));
}

function started(text?: string) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", backend: "legacy", projectDir: "/p", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [{ id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null, title: null }] });
  const state = initialState();
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: { ...state, status: { kind: "running" }, ...(text === undefined ? {} : { transcript: [{ seq: 1, text }] }) } });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  const box = (el: Element) => {
    el.getBoundingClientRect = () => ({ top: 0, bottom: 100, left: 0, right: 100, width: 100, height: 100, x: 0, y: 0 }) as DOMRect;
    Object.defineProperties(el, {
      clientWidth: { configurable: true, value: 100 },
      clientHeight: { configurable: true, value: 100 },
    });
  };
  box(rendered.container.querySelector(".agent-ui-conversation")!);
  box(rendered.container.querySelector(".message-list")!);
  rendered.container.querySelectorAll(".row a[href]").forEach(box);
  return rendered;
}

it("copies a detail row to the clipboard and primary selection", () => {
  const { container } = started();
  fireEvent.click(container.querySelector(".band-open")!);
  dispatch({ kind: "tab_detail", tab: 1, rows: [{ label: "cwd", value: "/p" }] });
  fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "y" });
  expect(writeText).toHaveBeenCalledWith("/p");
  expect(posted).toContainEqual({ type: "copy_primary", text: "/p" });
});

it("copies nothing on the detail popover's trailing action", () => {
  const { container } = started();
  fireEvent.click(container.querySelector(".band-open")!);
  dispatch({ kind: "tab_detail", tab: 1, rows: [] });
  fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "y" });
  expect(writeText).not.toHaveBeenCalled();
  expect(posted.filter((msg) => msg.type === "copy_primary")).toEqual([]);
});

/** jsdom lays nothing out: a band 900 px wide at 7.2 px a character, every range a box on screen and the band's flash
 *  on top of it, so the band's look measurement (`lookOnScreen`) finds the address shown, as a real engine would. */
function stubLaidOut(): () => void {
  const width = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "clientWidth");
  const box = HTMLElement.prototype.getBoundingClientRect;
  const proto = Range.prototype as unknown as { getClientRects?: () => DOMRect[] };
  const rects = proto.getClientRects;
  const doc = document as unknown as { elementFromPoint?: unknown };
  const hit = doc.elementFromPoint;
  Object.defineProperty(HTMLElement.prototype, "clientWidth", { configurable: true, get: () => 900 });
  // The band's ruler is one 7.2 px character; every other element keeps jsdom's empty box.
  HTMLElement.prototype.getBoundingClientRect = function (this: HTMLElement) {
    return this.classList.contains("band-measure")
      ? ({ left: 0, top: 0, right: 7.2, bottom: 10, width: 7.2, height: 10, x: 0, y: 0 } as DOMRect)
      : box.call(this);
  };
  proto.getClientRects = () => [{ left: 0, top: 0, right: 10, bottom: 10, width: 10, height: 10, x: 0, y: 0 } as DOMRect];
  doc.elementFromPoint = () => document.querySelector(".band-message");
  return () => {
    if (width) Object.defineProperty(HTMLElement.prototype, "clientWidth", width);
    else delete (HTMLElement.prototype as unknown as { clientWidth?: number }).clientWidth;
    HTMLElement.prototype.getBoundingClientRect = box;
    proto.getClientRects = rects;
    doc.elementFromPoint = hit;
  };
}

it("holds a disguised mailto click and lets its second click through once the address is on screen", () => {
  const unstub = stubLaidOut();
  try {
    const { container } = started("[support@example.org](mailto:attacker@example.net?subject=hi)");
    const anchor = container.querySelector(".row a[href]")!;
    vi.setSystemTime(10000);
    expect(fireEvent.click(anchor)).toBe(false);
    expect(container.querySelector(".band-message")?.textContent).toContain("attacker@example.net");
    vi.setSystemTime(10100);
    expect(fireEvent.click(anchor)).toBe(false);
    vi.setSystemTime(11000);
    expect(fireEvent.click(anchor)).toBe(true);
  } finally {
    unstub();
  }
});

it("never lets a disguised mailto click through while the band has not shown its address", () => {
  // No layout at all: the band has no width yet, so it draws only its mode and pill and the address is nowhere.
  const { container } = started("[support@example.org](mailto:attacker@example.net?subject=hi)");
  const anchor = container.querySelector(".row a[href]")!;
  vi.setSystemTime(10000);
  expect(fireEvent.click(anchor)).toBe(false);
  vi.setSystemTime(11000);
  expect(fireEvent.click(anchor)).toBe(false);
});

it("lets a mailto click through when its text shows the address", () => {
  const { container } = started("[a@example.com](mailto:a@example.com)");
  expect(fireEvent.click(container.querySelector(".row a[href]")!)).toBe(true);
});
