// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { SessionHeader } from "./SessionHeader";
import { initialState } from "../reducer";
import type { AgentUiState } from "../types";

function stateWith(overrides: Partial<AgentUiState>): AgentUiState {
  return { ...initialState(), ...overrides };
}

describe("SessionHeader status", () => {
  it("says working only while the session is actually running", () => {
    const { container } = render(
      <SessionHeader state={stateWith({ status: { kind: "running" }, activeTurnId: "t1" })} />,
    );
    expect(container.querySelector(".status")?.textContent).toBe("working");
  });

  it("a session that dies mid-turn reports its terminal status, not 'working' forever", () => {
    // Nothing clears activeTurnId when a session dies -- correctly, since no provider event says
    // "that turn is over". Without the guard this renders the word "working" indefinitely, in the
    // red styling written for the status text it was hiding.
    for (const status of [
      { kind: "unavailable", reason: "provider process exited unexpectedly" },
      { kind: "closed", reason: "closed_by_host" },
    ] as const) {
      const { container } = render(<SessionHeader state={stateWith({ status, activeTurnId: "t1" })} />);
      expect(container.querySelector(".status")?.textContent).toBe(status.kind);
      expect(container.querySelector(".status")?.className).toContain(`status-${status.kind}`);
    }
  });
});

describe("SessionHeader identities", () => {
  it("shows verdandi and claude separately when they are genuinely different", () => {
    const { container } = render(
      <SessionHeader
        state={stateWith({ backend: "sidecar", conversationId: "9dd752d2aaaa", sessionId: "4cf44237bbbb", providerSessionId: "fa81daebcccc" })}
      />,
    );
    const text = container.querySelector(".identities")?.textContent ?? "";
    expect(text).toContain("conv 9dd752d2");
    expect(text).toContain("verdandi 4cf44237");
    expect(text).toContain("claude fa81daeb");
  });

  it("collapses them into one when the backend never separated them", () => {
    // The legacy CLI has a single session id, so the projection reports the SAME value for both.
    // Printing two segments there would suggest two identities that happen to coincide.
    const { container } = render(
      <SessionHeader state={stateWith({ backend: "legacy", conversationId: null, sessionId: "af3785b8dddd", providerSessionId: "af3785b8dddd" })} />,
    );
    const text = container.querySelector(".identities")?.textContent ?? "";
    expect(text).toContain("session af3785b8");
    expect(text).not.toContain("verdandi");
    expect(text).not.toContain("conv ");
  });
});
