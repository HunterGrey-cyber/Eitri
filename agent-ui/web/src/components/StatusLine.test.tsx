// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";

// See ModeSelector.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);
import { StatusLine } from "./StatusLine";
import { applyEvent, initialState } from "../reducer";
import type { AgentDomainEvent, AgentUiState } from "../types";

function stateWith(overrides: Partial<AgentUiState>): AgentUiState {
  return { ...initialState(), ...overrides };
}

describe("StatusLine", () => {
  it("names the mode, the position and nothing it cannot know", () => {
    render(
      <StatusLine
        mode="browse"
        state={stateWith({ status: { kind: "running" }, activeTurnId: null })}
        position={{ index: 2, total: 7 }}
        canInterrupt={false}
        onInterrupt={() => {}}
      />,
    );
    expect(screen.getByTestId("mode-block").textContent).toBe("BROWSE");
    expect(screen.getByTestId("position").textContent).toBe("3/7");
    expect(screen.queryByRole("button", { name: /stop/i })).toBeNull();
  });

  it("claims focus only when told it has it", () => {
    const props = {
      mode: "browse" as const,
      state: stateWith({ status: { kind: "running" }, activeTurnId: null }),
      position: { index: 0, total: 1 },
      canInterrupt: false,
      onInterrupt: () => {},
    };
    const { rerender } = render(<StatusLine {...props} />);
    // Omitted means unknown, and unknown draws dim: it must never default to a bright claim.
    expect(screen.getByTestId("mode-block").dataset.focused).toBe("false");
    rerender(<StatusLine {...props} paneFocused />);
    expect(screen.getByTestId("mode-block").dataset.focused).toBe("true");
    expect(screen.getByTestId("mode-block").title).toBe("");
    rerender(<StatusLine {...props} paneFocused={false} />);
    expect(screen.getByTestId("mode-block").dataset.focused).toBe("false");
    expect(screen.getByTestId("mode-block").title).toMatch(/does not have keyboard focus/);
  });

  it("labels INPUT mode too", () => {
    render(
      <StatusLine
        mode="input"
        state={stateWith({ status: { kind: "running" }, activeTurnId: null })}
        position={{ index: 0, total: 1 }}
        canInterrupt={false}
        onInterrupt={() => {}}
      />,
    );
    expect(screen.getByTestId("mode-block").textContent).toBe("INPUT");
  });

  it("says 'working' only while a turn is genuinely running, matching SessionHeader's old rule", () => {
    const { rerender } = render(
      <StatusLine
        mode="browse"
        state={stateWith({ status: { kind: "running" }, activeTurnId: "t1" })}
        position={{ index: 0, total: 1 }}
        canInterrupt={false}
        onInterrupt={() => {}}
      />,
    );
    expect(screen.getByText("working")).toBeTruthy();

    // A terminal status wins over activeTurnId -- a session that died mid-turn must not read
    // "working" forever just because nothing ever clears activeTurnId for it.
    rerender(
      <StatusLine
        mode="browse"
        state={stateWith({ status: { kind: "closed", reason: "closed_by_host" }, activeTurnId: "t1" })}
        position={{ index: 0, total: 1 }}
        canInterrupt={false}
        onInterrupt={() => {}}
      />,
    );
    expect(screen.getByText("closed")).toBeTruthy();
  });

  it("shows an empty position as a dash rather than 1/0", () => {
    render(
      <StatusLine
        mode="browse"
        state={stateWith({ status: { kind: "running" }, activeTurnId: null })}
        position={{ index: 0, total: 0 }}
        canInterrupt={false}
        onInterrupt={() => {}}
      />,
    );
    expect(screen.getByTestId("position").textContent).toBe("—");
  });

  it("offers a clickable Stop only when the capability allows it AND a turn is genuinely working", () => {
    const onInterrupt = vi.fn();
    const { rerender } = render(
      <StatusLine
        mode="browse"
        state={stateWith({ status: { kind: "running" }, activeTurnId: "t1" })}
        position={{ index: 0, total: 1 }}
        canInterrupt={true}
        onInterrupt={onInterrupt}
      />,
    );
    const stop = screen.getByRole("button", { name: /stop/i });
    fireEvent.click(stop);
    expect(onInterrupt).toHaveBeenCalledTimes(1);

    // Capability withdrawn: gone even though a turn is running.
    rerender(
      <StatusLine
        mode="browse"
        state={stateWith({ status: { kind: "running" }, activeTurnId: "t1" })}
        position={{ index: 0, total: 1 }}
        canInterrupt={false}
        onInterrupt={onInterrupt}
      />,
    );
    expect(screen.queryByRole("button", { name: /stop/i })).toBeNull();
  });
});

/* The in-flight motion indicator (2026-09-20-in-flight-motion-design.md). These are the acceptance
   tests design doc §10.1 names: mount/unmount by DOM assertion, never by watching motion (jsdom
   runs no animations at all -- the stylesheet's own half of the contract is in indexCss.test.ts). */
describe("the in-flight motion indicator", () => {
  const opened: AgentDomainEvent = { type: "session_opened", session_id: "s", provider_session_id: "p", model: "m", cwd: "/tmp" };

  function baseProps(state: AgentUiState) {
    return {
      mode: "browse" as const,
      state,
      position: { index: 0, total: 1 },
      canInterrupt: false,
      onInterrupt: () => {},
    };
  }

  it("mounts iff a turn is genuinely working -- the SAME predicate the status word already uses, table-driven over every status kind", () => {
    const cases: Array<{ status: AgentUiState["status"]; activeTurnId: string | null; working: boolean }> = [
      { status: { kind: "starting" }, activeTurnId: null, working: false },
      { status: { kind: "running" }, activeTurnId: null, working: false },
      { status: { kind: "running" }, activeTurnId: "t1", working: true },
      { status: { kind: "unavailable", reason: "x" }, activeTurnId: "t1", working: false },
      { status: { kind: "closed", reason: "x" }, activeTurnId: "t1", working: false },
    ];
    for (const { status, activeTurnId, working } of cases) {
      const { container, unmount } = render(<StatusLine {...baseProps(stateWith({ status, activeTurnId }))} />);
      expect(container.querySelector(".turn-activity") !== null).toBe(working);
      expect(container.querySelector(".meter-fill") !== null).toBe(working);
      unmount();
    }
  });

  it("disappears after turn_completed, session_unavailable, session_closed, and a non-attaching resume_outcome", () => {
    const terminators: AgentDomainEvent[] = [
      { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: "done", stop_reason: null, usage: null },
      { type: "session_unavailable", reason: "provider crashed" },
      { type: "session_closed", reason: "provider exited" },
      {
        type: "resume_outcome",
        requested_provider_session_id: "a",
        status: "rejected",
        attached_provider_session_id: null,
        forked: false,
        detail: null,
      },
    ];
    for (const terminator of terminators) {
      let state = applyEvent(initialState(), opened);
      state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
      const before = render(<StatusLine {...baseProps(state)} />);
      expect(before.container.querySelector(".meter-fill")).not.toBeNull();
      before.unmount();

      state = applyEvent(state, terminator);
      const after = render(<StatusLine {...baseProps(state)} />);
      expect(after.container.querySelector(".meter-fill")).toBeNull();
      after.unmount();
    }
  });

  it("disappears while a permission is pending, but the word ('waiting for you') still shows -- motion is a claim the agent is working, and a card up makes that false", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    state = applyEvent(state, { type: "permission_requested", permission_id: "p1", tool_use_id: "tu1", tool_name: "Bash", input: {} });
    const { container } = render(<StatusLine {...baseProps(state)} />);
    expect(container.querySelector(".meter-fill")).toBeNull();
    expect(container.querySelector(".turn-activity")).not.toBeNull();
    expect(container.querySelector(".turn-state")?.textContent).toBe("waiting for you");
  });

  it("names the tool a running call is waiting on, and keeps the meter moving for it", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "NotebookEdit", input: {} });
    const { container } = render(<StatusLine {...baseProps(state)} />);
    expect(container.querySelector(".turn-state")?.textContent).toBe("running NotebookEdit");
    expect(container.querySelector(".meter-fill")).not.toBeNull();
  });

  it("shows sent immediately -- before any text has come back, the panel is otherwise indistinguishable from a hung one", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "user_prompt_submitted", text: "hi" });
    const { container } = render(<StatusLine {...baseProps(state)} />);
    expect(container.querySelector(".turn-state")?.textContent).toBe("sent");
  });

  it("renders the elapsed clock from the turnClock prop, honestly marking an inexact one with a trailing +", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    const exact = render(<StatusLine {...baseProps(state)} turnClock={{ turnId: "t1", since: Date.now() - 5000, exact: true }} />);
    expect(exact.container.querySelector(".turn-elapsed")?.textContent).toBe("5s");
    exact.unmount();

    const inexact = render(<StatusLine {...baseProps(state)} turnClock={{ turnId: "t1", since: Date.now() - 5000, exact: false }} />);
    expect(inexact.container.querySelector(".turn-elapsed")?.textContent).toBe("5s+");
  });

  it("shows no elapsed clock at all when none was supplied, rather than guessing at one", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    const { container } = render(<StatusLine {...baseProps(state)} />);
    expect(container.querySelector(".turn-activity")).not.toBeNull();
    expect(container.querySelector(".turn-elapsed")).toBeNull();
  });
});
