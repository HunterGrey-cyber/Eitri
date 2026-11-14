// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { initialState } from "./reducer";
import type { ProviderInfo } from "./types";

afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
beforeAll(() => { Element.prototype.scrollIntoView = vi.fn(); });
let posted: Array<Record<string, unknown>>;
let measureBand: () => void;
beforeEach(() => {
  posted = [];
  const measurements: Array<() => void> = [];
  vi.stubGlobal("ResizeObserver", class {
    constructor(private callback: ResizeObserverCallback) {}
    observe(target: Element) {
      measurements.push(() => this.callback([{ target, contentRect: {
        width: target.classList.contains("band-measure") ? 7.2 : 2000,
      } } as unknown as ResizeObserverEntry], this as unknown as ResizeObserver));
    }
    unobserve() {}
    disconnect() {}
  });
  measureBand = () => act(() => { measurements.forEach(measure => measure()); });
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
});
function dispatch(payload: unknown) {
  act(() => { window.__eitriDispatch!(JSON.stringify(payload)); });
}
const provider: ProviderInfo = {
  host: "example-host", accountName: "work", sidecarVersion: "test", claudeAgentSdkVersion: "test",
  claudeCodeVersion: "test", protocol: "1.0", buildDescription: null, startupDiagnostics: [],
};
const tab = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "manual",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
};
function started() {
  const view = render(<App />);
  dispatch({ kind: "hello", backend: "sidecar", projectDir: "/srv/project", permissionModes: ["manual"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [tab] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: {
    ...initialState(), status: { kind: "running" }, provider,
    linkState: { kind: "disconnected", sinceUnixMs: 10 },
  } });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  measureBand();
  return view;
}
it("restores a link from the snapshot and updates without conversation events", () => {
  const { container } = started();
  expect(container.querySelector(".status-band")?.textContent).toContain("example-host · account work");
  const line = () => container.querySelector(".remote-link-state")?.textContent;
  expect(line()).toBe("disconnected from example-host; the agent may still be working");
  dispatch({ kind: "link_state", tab: 2, linkState: { kind: "catching_up", done: 1, total: 2 } });
  expect(line()).toBe("disconnected from example-host; the agent may still be working");
  dispatch({ kind: "link_state", tab: 1, linkState: { kind: "reconnecting", attempt: 2, nextInMs: 10 } });
  expect(line()).toBe("reconnecting to example-host… (attempt 2)");
  dispatch({ kind: "link_state", tab: 1, linkState: { kind: "catching_up", done: 12, total: 30 } });
  expect(line()).toBe("catching up 12 / 30");
  dispatch({ kind: "link_state", tab: 1, linkState: { kind: "live" } });
  expect(line()).toBeUndefined();
  dispatch({ kind: "tabs", active: 2, tabs: [tab, { ...tab, id: 2, number: 2 }] });
  dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: { ...initialState(), status: { kind: "running" } } });
  expect(line()).toBeUndefined();
});
it("explains why remote turn review is unavailable without sending a request", () => {
  const { container } = started();
  fireEvent.keyDown(container.querySelector(".agent-ui-conversation")!, { key: "c" });
  expect(container.textContent).toContain("turn review is not available on remote tabs yet");
  expect(posted.some(p => p.type === "review_request")).toBe(false);
});
