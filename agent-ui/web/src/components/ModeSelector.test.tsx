// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";

// Explicit, because this project's vitest config does not enable `globals`, so
// @testing-library/react never registers its automatic cleanup. Without it the DOM accumulates
// across tests in a file and every query starts matching elements from earlier renders -- which
// surfaces as "found multiple elements", not as a stale-state bug, so it is easy to misread as a
// component problem.
afterEach(cleanup);

// Role-based queries throughout: each button renders its label inside a <strong>, so a plain text
// query matches both the <strong> and its parent <button> and RTL rejects the ambiguity.
const button = (name: RegExp) => screen.getByRole("button", { name });
const maybeButton = (name: RegExp) => screen.queryByRole("button", { name });
import { ModeSelector } from "./ModeSelector";
import type { Hello } from "../types";

function hello(overrides: Partial<Hello> = {}): Hello {
  return {
    backend: "sidecar",
    projectDir: "/tmp/project",
    permissionModes: ["bypass"],
    resumableSession: null,
    expectedVerdandiRevision: "2fd30fb",
    ...overrides,
  };
}

describe("ModeSelector resume offer", () => {
  it("offers nothing to continue when the workspace has no resumable session", () => {
    render(<ModeSelector hello={hello()} connecting={false} onStart={() => {}} />);
    expect(maybeButton(/Continue previous session/)).toBeNull();
  });

  it("offers it, labelled by the Claude session id, when the workspace has one", () => {
    render(
      <ModeSelector
        hello={hello({ resumableSession: { provider: "claude", providerSessionId: "1857dcd5-973b-46a2", updatedAt: "0" } })}
        connecting={false}
        onStart={() => {}}
      />,
    );
    expect(button(/Continue previous session/)).toBeTruthy();
    // The Claude identity is what is shown, because it is the one that survives a resume.
    expect(button(/Claude 1857dcd5/)).toBeTruthy();
  });

  it("passes the Claude provider session id through when clicked, never the Verdandi one", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({ resumableSession: { provider: "claude", providerSessionId: "claude-abc", updatedAt: "0" } })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(button(/Continue previous session/));
    expect(onStart).toHaveBeenCalledWith("bypass", "claude-abc");
  });

  it("a fresh-start click carries no resume id at all", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({ resumableSession: { provider: "claude", providerSessionId: "claude-abc", updatedAt: "0" } })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(button(/^Bypass/));
    expect(onStart).toHaveBeenCalledWith("bypass");
  });

  it("the legacy backend never offers it, even if a record somehow reached the frontend", () => {
    // Defence in depth: the Rust side already returns null for legacy, but a resume attempt on that
    // backend is a hard failure, so the control must not be reachable from two directions.
    render(
      <ModeSelector
        hello={hello({ backend: "legacy", permissionModes: ["auto", "bypass"], resumableSession: null })}
        connecting={false}
        onStart={() => {}}
      />,
    );
    expect(maybeButton(/Continue previous session/)).toBeNull();
    expect(button(/^Auto/)).toBeTruthy();
  });

  it("shows nothing at all while a backend is connecting", () => {
    render(
      <ModeSelector
        hello={hello({ resumableSession: { provider: "claude", providerSessionId: "claude-abc", updatedAt: "0" } })}
        connecting={true}
        onStart={() => {}}
      />,
    );
    expect(maybeButton(/Continue previous session/)).toBeNull();
    expect(screen.getByText(/Starting the agent backend/)).toBeTruthy();
  });
});
