// @vitest-environment jsdom
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";

beforeEach(() => {
  vi.useFakeTimers();
  Element.prototype.scrollIntoView = vi.fn();
  window.webkit = { messageHandlers: { eitriAgent: { postMessage: vi.fn() } } };
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); });

function dispatch(payload: unknown) {
  act(() => window.__eitriDispatch!(JSON.stringify(payload)));
}

it("takes the activity look and name from the shown tab even when capabilities are identical", () => {
  const { container } = render(<App />);
  dispatch({ kind: "hello", backend: "sidecar", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"],
    resumableSessions: [], account: null });
  const tabs = ["codex", "acp:gemini", "claude"].map((agent, i) => ({
    id: i + 1, number: i + 1, label: agent, agent, name: null, state: "live", mode: "auto",
    marker: null, pending: 0, resumable: false, failure: null, title: null,
  }));
  for (const [index, kind, name, glyph] of [
    [1, "codex", "Codex", "•"], [2, "acp", "gemini", "•"], [3, "claude", "Claude Code", "✻"],
  ] as const) {
    dispatch({ kind: "tabs", active: index, tabs });
    dispatch({ kind: "snapshot", tab: index, throughRevision: 1, state: {
      ...initialState(), status: { kind: "running" }, activeTurnId: "t1",
      turnStarts: [{ seq: 1, turnId: "t1", origin: "cli", cause: null }],
    } });
    const mark = container.querySelector<HTMLElement>(".activity-glyph")!;
    expect(mark.textContent?.trim()).toBe(glyph);
    expect(mark.dataset.look).toBe(kind);
    expect(container.querySelector(".activity-line .turn-activity")?.textContent)
      .toBe(`${name} is handling: ${name} started this turn`);
  }
});
