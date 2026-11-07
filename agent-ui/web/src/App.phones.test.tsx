// @vitest-environment jsdom
/**
 * Remote control in the panel: the status band's phone mark and the sheet `?` opens, fed by the window's
 * `remote_connected` envelope. The mark's own layout is `band.test.ts`; this file is the wiring.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentUiState, Hello } from "./types";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

let posted: Array<Record<string, unknown>>;

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  vi.useFakeTimers();
  widen = stubBandWidth();
  Object.defineProperty(navigator, "clipboard", { value: { writeText: vi.fn() }, configurable: true });
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
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
const TAB1 = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

function snapshotState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return { ...initialState(), status: { kind: "running" }, ...overrides };
}

function stubBandWidth(): (container: HTMLElement) => void {
  type Rec = { callback: ResizeObserverCallback; observed: Element[] };
  const observers: Rec[] = [];
  class FakeResizeObserver {
    private record: Rec;
    constructor(callback: ResizeObserverCallback) {
      this.record = { callback, observed: [] };
      observers.push(this.record);
    }
    observe(el: Element) {
      this.record.observed.push(el);
    }
    unobserve() {}
    disconnect() {}
  }
  vi.stubGlobal("ResizeObserver", FakeResizeObserver);
  return (container: HTMLElement) => {
    const band = container.querySelector(".status-band");
    const measure = container.querySelector(".band-measure");
    for (const o of observers) {
      for (const el of o.observed) {
        if (el === band) o.callback([{ target: el, contentRect: { width: 1400 } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
        if (el === measure) o.callback([{ target: el, contentRect: { width: 7.2 } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
      }
    }
  };
}
let widen: (container: HTMLElement) => void;

const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));
const bandText = (c: HTMLElement) => c.querySelector(".status-band")?.textContent ?? "";


function started() {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [TAB1] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState() });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  wait(TYPING_GUARD_MS + 50);
  return rendered;
}

const PIXEL = { device: "d1", name: "Pixel", grant: "watch", host: "127.0.0.1:7341", since: 5 };
const TABLET = { device: "d2", name: "Tablet", grant: "watch+steer", host: "desk.example.org", since: 9 };

describe("phones connected to this desk", () => {
  it("a notice naming a phone shows the hidden characters of its claimed name", () => {
    const { container } = started();
    dispatch({ kind: "notice", text: "refused from Pix\u202Eel\u200B: the work has changed" });
    expect(container.querySelector(".status-band .band-message")?.textContent)
      .toBe("refused from Pix⟨U+202E⟩el⟨U+200B⟩: the work has changed");
  });

  it("the band shows nothing until a phone connects, counts them, and drops the mark when the last one goes", () => {
    const { container } = started();
    expect(bandText(container)).not.toContain("☎");
    dispatch({ kind: "remote_connected", devices: [PIXEL, TABLET] });
    expect(bandText(container)).toContain("☎2");
    dispatch({ kind: "remote_connected", devices: [TABLET] });
    expect(bandText(container)).toContain("☎1");
    dispatch({ kind: "remote_connected", devices: [] });
    expect(bandText(container)).not.toContain("☎");
  });

  it("? names each connected phone with its grant and host", () => {
    const { container } = started();
    dispatch({ kind: "remote_connected", devices: [PIXEL, TABLET] });
    dispatch({ kind: "open_keymap" });
    const sheet = container.querySelector(".keymap-overlay")!;
    const text = sheet.textContent ?? "";
    expect(text).toContain("Phones connected");
    expect(text).toContain("Pixel");
    expect(text).toContain("127.0.0.1:7341");
    expect(text).toContain("Tablet");
    expect(text).toContain("watch+steer");
  });

  it("an unstarted tab's band carries the mark too", () => {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB1, state: "not_started" }] });
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
    dispatch({ kind: "pane_focus", focused: true });
    act(() => widen(rendered.container));
    wait(TYPING_GUARD_MS + 50);
    expect(rendered.container.querySelector(".empty-stage")).not.toBeNull();
    expect(bandText(rendered.container)).not.toContain("☎");
    dispatch({ kind: "remote_connected", devices: [PIXEL] });
    expect(bandText(rendered.container)).toContain("☎1");
  });
});
