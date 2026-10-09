// @vitest-environment jsdom
/**
 * A failed tab in the whole panel: what stays on screen, and how the failure is worded. The envelopes come from
 * `fixtures/failed-tabs.json`, which Rust writes from the tab set's own pump and tab views
 * (`tab_set.rs`, `a_failed_tab_says_whether_its_session_had_been_open_and_the_fixture_records_it`); the start
 * screen's `:` line and its naming of the agent are here too, being the same layout.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { AgentUiState, Hello } from "./types";
import fixture from "./fixtures/failed-tabs.json";

afterEach(cleanup);

let posted: Array<Record<string, unknown>>;

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
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
  permissionModes: ["auto", "bypass"],
  resumableSessions: [],
  expectedVerdandiRevision: null,
  account: "team",
};

const LIVE_TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, title: null, agent: "claude",
} as const;

function snapshotState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return { ...initialState(), status: { kind: "running" }, ...overrides };
}

/** Tab 1 live and showing `prompt` and `reply`, as a session that has run a turn would. */
function conversation(withContent: boolean) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
  const state = withContent
    ? snapshotState({
        userPrompts: [{ seq: 1, text: "please fix the parser" }],
        transcript: [{ seq: 2, text: "restored reply from the agent" }],
      } as Partial<AgentUiState>)
    : snapshotState();
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 3, state });
  return rendered;
}

describe("a tab a tripwire failed", () => {
  it("keeps the conversation on screen, with the reason in the lost-session row", () => {
    const { container } = conversation(true);
    dispatch(fixture.error);
    dispatch(fixture.tabs);
    const text = container.textContent ?? "";
    expect(container.querySelector(".empty-tab")).toBeNull();
    expect(text).toContain("restored reply from the agent");
    expect(text).toContain("please fix the parser");
    expect(text).toContain("This session was lost.");
    expect(text).toContain("the agent reported sandbox dangerFullAccess, Eitri asked for sandbox workspaceWrite");
    expect(text).not.toContain("did not start");
  });

  it("drops the cards: nothing can answer them any more", () => {
    const { container } = conversation(true);
    dispatch({
      kind: "events",
      tab: 1,
      events: [{ type: "permission_requested", permission_id: "p1", tool_use_id: null, tool_name: "Bash", input: { command: "ls" } }],
    });
    expect(container.querySelector(".permission-card")).not.toBeNull();
    dispatch(fixture.error);
    dispatch(fixture.tabs);
    expect(container.querySelector(".permission-card")).toBeNull();
    expect(container.textContent).toContain("restored reply from the agent");
  });

  it("says the session stopped, not that it did not start, when nothing had been shown", () => {
    const { container } = conversation(false);
    dispatch(fixture.error);
    dispatch(fixture.tabs);
    const text = container.textContent ?? "";
    expect(container.querySelector(".empty-tab")).not.toBeNull();
    expect(text).toContain("This tab's session stopped.");
    expect(text).not.toContain("did not start");
    expect(text).toContain("the agent reported sandbox dangerFullAccess");
  });
});

describe("a failed tab that is started again", () => {
  it("lets the lost conversation go as soon as the tab is starting, before any snapshot", () => {
    const { container } = conversation(true);
    dispatch(fixture.error);
    dispatch(fixture.tabs);
    expect(container.textContent).toContain("This session was lost.");
    dispatch({ ...fixture.tabs, tabs: [{ ...fixture.tabs.tabs[0], state: "starting", failure: null, failedAfterStart: false }] });
    expect(container.textContent).not.toContain("This session was lost.");
    expect(container.textContent).not.toContain("restored reply from the agent");
  });
});

describe("a tab whose start never happened", () => {
  it("still says it did not start", () => {
    const { container } = conversation(false);
    const never = fixture.tabs.tabs.find((t) => t.id === fixture.neverStartedTab)!;
    dispatch({ ...fixture.tabs, active: never.id });
    const text = container.textContent ?? "";
    expect(text).toContain("This tab's session did not start.");
    expect(text).toContain("failed to start session: codex not found");
  });
});

describe("the start screen", () => {
  function fresh(agent: string) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({
      kind: "tabs",
      active: 1,
      tabs: [{ ...LIVE_TAB, state: "not_started", agent, label: `1 ${agent}` }],
    });
    return rendered;
  }

  it("names the agent an empty Codex tab runs", () => {
    const { container } = fresh("codex");
    expect(container.querySelector(".dash-where")!.textContent).toContain("Codex");
    expect(container.querySelector(".dash-where")!.textContent).not.toContain("team");
    expect(container.textContent).toContain("New Codex session");
  });

  it("names the registered name of an ACP agent", () => {
    const { container } = fresh("acp:gem");
    expect(container.querySelector(".dash-where")!.textContent).toContain("gem");
    expect(container.textContent).toContain("New gem session");
  });

  it("leaves a Claude tab as it was", () => {
    const { container } = fresh("claude");
    expect(container.querySelector(".dash-where")!.textContent).toBe("~/project · team");
    expect(container.textContent).toContain("New session");
  });

  it("opens the command line on a colon, so :tabnew codex works from a fresh window", () => {
    const { container } = fresh("claude");
    const tab = container.querySelector(".empty-tab")!;
    fireEvent.keyDown(tab, { key: "Escape" });
    fireEvent.keyDown(tab, { key: ":" });
    const input = container.querySelector<HTMLInputElement>(".search-bar input");
    expect(input).not.toBeNull();
    fireEvent.change(input!, { target: { value: "tabnew codex" } });
    fireEvent.keyDown(input!, { key: "Enter" });
    expect(posted.find((m) => m.type === "tab_verb")).toMatchObject({ verb: "new", agent: "codex" });
    expect(container.querySelector(".search-bar input")).toBeNull();
  });
});
