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
// The which-conversation controls are radios, not buttons: they select rather than activate, and an
// explicit `role="radio"` replaces a <button>'s implicit role in the accessibility tree. Querying
// them as buttons is what a screen reader would also fail to do.
const choice = (name: RegExp) => screen.getByRole("radio", { name });
const maybeChoice = (name: RegExp) => screen.queryByRole("radio", { name });
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
    expect(maybeChoice(/Continue previous session/)).toBeNull();
  });

  it("offers it, labelled by the Claude session id, when the workspace has one", () => {
    render(
      <ModeSelector
        hello={hello({ resumableSession: { provider: "claude", providerSessionId: "1857dcd5-973b-46a2", updatedAt: "0" } })}
        connecting={false}
        onStart={() => {}}
      />,
    );
    expect(choice(/Continue previous session/)).toBeTruthy();
    // The Claude identity is what is shown, because it is the one that survives a resume.
    expect(choice(/Claude 1857dcd5/)).toBeTruthy();
  });

  /* Two axes, deliberately: picking the conversation does not start anything, and the permission
     mode does. An earlier revision made "Continue previous session" a start button that carried a
     mode chosen for the user, which was invisible-but-harmless while only one mode was offered and
     became a silent decision the moment two were -- on a screen that promises the mode is a choice. */
  it("choosing the previous session starts nothing by itself", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({ resumableSession: { provider: "claude", providerSessionId: "claude-abc", updatedAt: "0" } })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(choice(/Continue previous session/));
    expect(onStart).not.toHaveBeenCalled();
  });

  it("passes the Claude provider session id through with the mode the user picked, never the Verdandi id", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({
          permissionModes: ["auto", "bypass"],
          resumableSession: { provider: "claude", providerSessionId: "claude-abc", updatedAt: "0" },
        })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(choice(/Continue previous session/));
    fireEvent.click(button(/^Auto/));
    expect(onStart).toHaveBeenCalledWith("auto", "claude-abc");
  });

  it("resuming under the other policy is equally reachable -- neither mode is chosen for the user", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({
          permissionModes: ["auto", "bypass"],
          resumableSession: { provider: "claude", providerSessionId: "claude-abc", updatedAt: "0" },
        })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(choice(/Continue previous session/));
    fireEvent.click(button(/^Bypass/));
    expect(onStart).toHaveBeenCalledWith("bypass", "claude-abc");
  });

  it("switching back to New session drops the resume id", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({
          permissionModes: ["auto", "bypass"],
          resumableSession: { provider: "claude", providerSessionId: "claude-abc", updatedAt: "0" },
        })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(choice(/Continue previous session/));
    fireEvent.click(choice(/New session/));
    fireEvent.click(button(/^Auto/));
    // Not `("auto", undefined)` by accident: a leftover id here would silently continue a
    // conversation the user just said they did not want.
    expect(onStart).toHaveBeenCalledWith("auto", undefined);
  });

  it("a fresh-start click carries no resume id at all, with New session selected by default", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({ resumableSession: { provider: "claude", providerSessionId: "claude-abc", updatedAt: "0" } })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(button(/^Bypass/));
    // Explicitly `undefined`, not an omitted argument: "no session to continue" is a decision the
    // call makes, not something it forgot to mention.
    expect(onStart).toHaveBeenCalledWith("bypass", undefined);
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
    expect(maybeChoice(/Continue previous session/)).toBeNull();
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
    expect(maybeChoice(/Continue previous session/)).toBeNull();
    expect(screen.getByText(/Starting the agent backend/)).toBeTruthy();
  });
});
