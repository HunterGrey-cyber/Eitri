// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";

// See ModeSelector.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);
import { Winbar } from "./Winbar";
import { initialState } from "../reducer";
import type { AgentUiState } from "../types";

function stateWith(overrides: Partial<AgentUiState>): AgentUiState {
  return { ...initialState(), ...overrides };
}

describe("Winbar model", () => {
  it("shows the model once the provider has named one", () => {
    render(<Winbar state={stateWith({ model: "claude-opus-5[1m]" })} />);
    expect(screen.getByText("claude-opus-5[1m]")).toBeTruthy();
  });

  it("says no model yet before the provider has named one", () => {
    render(<Winbar state={stateWith({ model: null })} />);
    expect(screen.getByText("no model yet")).toBeTruthy();
  });
});

// Moved verbatim from SessionHeader.test.tsx's "SessionHeader identities" block (panel-as-document
// task 6) -- the collapsing logic these pin did not change, only which component renders it.
describe("Winbar identities", () => {
  it("collapses the two ids on legacy, where they are one value", () => {
    render(<Winbar state={stateWith({ sessionId: "abc12345", providerSessionId: "abc12345" })} />);
    expect(screen.getByTestId("identities").textContent).toContain("session abc12345");
    expect(screen.getByTestId("identities").textContent).not.toContain("verdandi");
  });

  it("shows verdandi and claude separately when they are genuinely different", () => {
    render(
      <Winbar
        state={stateWith({ backend: "sidecar", conversationId: "9dd752d2aaaa", sessionId: "4cf44237bbbb", providerSessionId: "fa81daebcccc" })}
      />,
    );
    const text = screen.getByTestId("identities").textContent ?? "";
    expect(text).toContain("conv 9dd752d2");
    expect(text).toContain("verdandi 4cf44237");
    expect(text).toContain("claude fa81daeb");
  });

  it("says neither id is assigned yet, rather than naming a sidecar/claude split, before the first turn", () => {
    // A session that has started but has taken no turn has BOTH ids null. `null === null` is
    // true, so this must not fall into the "they differ" branch -- which is exactly the bug: it
    // rendered "verdandi -- . claude --", naming a backend that was never involved, on the
    // default legacy backend where the two ids are always a single value.
    render(
      <Winbar state={stateWith({ backend: "legacy", conversationId: null, sessionId: null, providerSessionId: null })} />,
    );
    const text = screen.getByTestId("identities").textContent ?? "";
    expect(text).not.toContain("verdandi");
    expect(text).not.toContain("claude");
    expect(text).not.toContain("conv ");
  });
});

describe("Winbar Verdandi skew warning", () => {
  it("says nothing about a skew when the provider reports none", () => {
    const { container } = render(
      <Winbar
        state={stateWith({
          provider: {
            sidecarVersion: "1.0.0",
            claudeAgentSdkVersion: "1.0.0",
            claudeCodeVersion: "2.1.272",
            protocol: "3",
            buildDescription: null,
            startupDiagnostics: [],
          },
        })}
      />,
    );
    const badge = container.querySelector(".provider");
    expect(badge?.textContent).toBe("CLI 2.1.272");
    expect(badge?.className).not.toContain("provider-warn");
  });

  it("marks the CLI badge when the provider reports a startup diagnostic, existing text unchanged", () => {
    const { container } = render(
      <Winbar
        state={stateWith({
          provider: {
            sidecarVersion: "1.0.0",
            claudeAgentSdkVersion: "1.0.0",
            claudeCodeVersion: "2.1.272",
            protocol: "3",
            buildDescription: null,
            startupDiagnostics: ["untested CLI version"],
          },
        })}
      />,
    );
    const badge = container.querySelector(".provider");
    // Existing text unchanged: same "CLI <version> ⚠" the old SessionHeader printed.
    expect(badge?.textContent).toBe("CLI 2.1.272 ⚠");
    expect(badge?.className).toContain("provider-warn");
  });
});

describe("Winbar permission mode", () => {
  it("shows the mode the session was actually started with", () => {
    render(<Winbar state={stateWith({})} permissionMode="bypass" />);
    expect(screen.getByText("bypass")).toBeTruthy();
  });

  it("shows nothing when this page never started the session itself", () => {
    // e.g. a panel reload's restored snapshot -- App.tsx only ever remembers a mode it asked for.
    // Absence here must stay absence, never a guessed default.
    const { container } = render(<Winbar state={stateWith({})} />);
    expect(container.querySelector(".permission-mode")).toBeNull();
  });
});
