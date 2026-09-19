// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";

// See ModeSelector.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);
import { StatusLine } from "./StatusLine";
import { initialState } from "../reducer";
import type { AgentUiState } from "../types";

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
