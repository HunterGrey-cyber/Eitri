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
const allChoices = () => screen.queryAllByRole("radio");
import { ModeSelector } from "./ModeSelector";
import type { Hello, ResumableSession } from "../types";

function hello(overrides: Partial<Hello> = {}): Hello {
  return {
    backend: "sidecar",
    projectDir: "/tmp/project",
    permissionModes: ["bypass"],
    resumableSessions: [],
    expectedVerdandiRevision: "2fd30fb",
    ...overrides,
  };
}

/** Both stamps default to the same value, which is what a session that was started once and never
 *  resumed really looks like on disk. */
function session(providerSessionId: string, updatedAt = "1757700000000", createdAt = updatedAt): ResumableSession {
  return { provider: "claude", providerSessionId, createdAt, updatedAt };
}

describe("ModeSelector conversation picker", () => {
  it("offers nothing to continue when the workspace has no resumable session", () => {
    render(<ModeSelector hello={hello()} connecting={false} onStart={() => {}} />);
    expect(allChoices()).toHaveLength(0);
    // Not even the "New session" radio: with nothing to choose between, a radiogroup of one is a
    // control that cannot do anything.
    expect(maybeChoice(/New session/)).toBeNull();
  });

  it("offers one row per session, in the order Rust ranked them", () => {
    render(
      <ModeSelector
        hello={hello({
          resumableSessions: [session("1857dcd5-973b-46a2"), session("99b2b206-0000-4000")],
        })}
        connecting={false}
        onStart={() => {}}
      />,
    );
    // The provider's own session identity is what is shown: it is the one that survives a resume.
    expect(choice(/claude 1857dcd5/)).toBeTruthy();
    expect(choice(/claude 99b2b206/)).toBeTruthy();
    // New session + one row each. Order is the array's, because the component sorts nothing --
    // ranking belongs to the side that can read the timestamps as numbers.
    const names = allChoices().map((el) => el.textContent ?? "");
    expect(names).toHaveLength(3);
    expect(names[0]).toContain("New session");
    expect(names[1]).toContain("claude 1857dcd5");
    expect(names[2]).toContain("claude 99b2b206");
  });

  /** A row says only what a record knows. This is the test that fails if anyone later "improves"
   *  the picker with a summary, a subject line, or a first-prompt preview: none of those exist
   *  anywhere in the payload, so producing one would mean inventing it. */
  it("labels a row with an id and a time, and nothing it does not have", () => {
    render(
      <ModeSelector
        hello={hello({ resumableSessions: [session("1857dcd5-973b-46a2", "1757700000000")] })}
        connecting={false}
        onStart={() => {}}
      />,
    );
    const row = choice(/claude 1857dcd5/);
    const when = new Date(1757700000000).toLocaleString();
    expect(row.textContent).toBe(`claude 1857dcd5last opened ${when}`);
  });

  /** The provider name on a row is READ from the record, not written into the component.
   *
   *  `provider` was plumbed Rust -> wire -> TS and then not rendered: the row said "Claude" as a
   *  literal, which is right only for as long as `PROVIDER_NAME` is the only provider there is. The
   *  fixture below deliberately uses a provider no part of this codebase mentions, so a hardcoded
   *  label cannot pass. */
  it("names the provider the record names, not a hardcoded one", () => {
    render(
      <ModeSelector
        hello={hello({
          resumableSessions: [{ ...session("abcdef12-0000"), provider: "someprovider" }],
        })}
        connecting={false}
        onStart={() => {}}
      />,
    );
    const row = choice(/someprovider abcdef12/);
    expect(row.textContent).toContain("someprovider abcdef12");
    expect(row.textContent).not.toContain("Claude");
  });

  /** "last opened", never "last active": nothing rewrites a record during a conversation, so the
   *  stamp marks when the session was last STARTED OR RESUMED. An hour of work and a window opened
   *  and abandoned produce the same value. */
  it("does not claim the timestamp is when the session was last active", () => {
    render(
      <ModeSelector
        hello={hello({ resumableSessions: [session("claude-abc")] })}
        connecting={false}
        onStart={() => {}}
      />,
    );
    expect(choice(/claude claude-a/).textContent).not.toMatch(/last (active|used)/);
  });

  /** `createdAt` earns its place only when it says something `updatedAt` does not -- i.e. when the
   *  session has actually been resumed at least once. */
  it("shows when a session started only once that differs from when it was last opened", () => {
    render(
      <ModeSelector
        hello={hello({
          resumableSessions: [
            session("resumed-1", "1757700000000", "1757100000000"),
            session("fresh-1", "1757700000000"),
          ],
        })}
        connecting={false}
        onStart={() => {}}
      />,
    );
    expect(choice(/claude resumed-/).textContent).toContain(`started ${new Date(1757100000000).toLocaleString()}`);
    expect(choice(/claude fresh-1/).textContent).not.toContain("started");
  });

  /* Two axes, deliberately: picking the conversation does not start anything, and the permission
     mode does. An earlier revision made "Continue previous session" a start button that carried a
     mode chosen for the user, which was invisible-but-harmless while only one mode was offered and
     became a silent decision the moment two were -- on a screen that promises the mode is a choice. */
  it("choosing a previous session starts nothing by itself", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({ resumableSessions: [session("claude-abc")] })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(choice(/claude claude-a/));
    expect(onStart).not.toHaveBeenCalled();
  });

  it("passes the Claude provider session id through with the mode the user picked, never the Verdandi id", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({ permissionModes: ["auto", "bypass"], resumableSessions: [session("claude-abc")] })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(choice(/claude claude-a/));
    fireEvent.click(button(/^Auto/));
    expect(onStart).toHaveBeenCalledWith("auto", "claude-abc");
  });

  /** The whole point of a picker: the session that starts is the one the user pointed at, not
   *  whichever one happened to be first. */
  it("starts the session the user selected, not the most recent one", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({
          permissionModes: ["auto", "bypass"],
          resumableSessions: [session("newest-1"), session("older-1"), session("oldest-1")],
        })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(choice(/claude oldest-1/));
    fireEvent.click(button(/^Bypass/));
    expect(onStart).toHaveBeenCalledWith("bypass", "oldest-1");
  });

  it("re-picking replaces the selection rather than adding to it", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({ permissionModes: ["auto"], resumableSessions: [session("first-1"), session("second-1")] })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(choice(/claude first-1/));
    fireEvent.click(choice(/claude second-1/));
    expect(choice(/claude first-1/).getAttribute("aria-checked")).toBe("false");
    expect(choice(/claude second-1/).getAttribute("aria-checked")).toBe("true");
    fireEvent.click(button(/^Auto/));
    expect(onStart).toHaveBeenCalledWith("auto", "second-1");
  });

  it("resuming under the other policy is equally reachable -- neither mode is chosen for the user", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({ permissionModes: ["auto", "bypass"], resumableSessions: [session("claude-abc")] })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(choice(/claude claude-a/));
    fireEvent.click(button(/^Bypass/));
    expect(onStart).toHaveBeenCalledWith("bypass", "claude-abc");
  });

  it("switching back to New session drops the resume id", () => {
    const onStart = vi.fn();
    render(
      <ModeSelector
        hello={hello({ permissionModes: ["auto", "bypass"], resumableSessions: [session("claude-abc")] })}
        connecting={false}
        onStart={onStart}
      />,
    );
    fireEvent.click(choice(/claude claude-a/));
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
        hello={hello({ resumableSessions: [session("claude-abc")] })}
        connecting={false}
        onStart={onStart}
      />,
    );
    expect(choice(/New session/).getAttribute("aria-checked")).toBe("true");
    fireEvent.click(button(/^Bypass/));
    // Explicitly `undefined`, not an omitted argument: "no session to continue" is a decision the
    // call makes, not something it forgot to mention.
    expect(onStart).toHaveBeenCalledWith("bypass", undefined);
  });

  it("the legacy backend never offers it, even if records somehow reached the frontend", () => {
    // Defence in depth: the Rust side already sends an empty list for legacy, but a resume attempt
    // on that backend is a hard failure, so the control must not be reachable from two directions.
    render(
      <ModeSelector
        hello={hello({ backend: "legacy", permissionModes: ["auto", "bypass"], resumableSessions: [] })}
        connecting={false}
        onStart={() => {}}
      />,
    );
    expect(allChoices()).toHaveLength(0);
    expect(button(/^Auto/)).toBeTruthy();
  });

  it("shows nothing at all while a backend is connecting", () => {
    render(
      <ModeSelector
        hello={hello({ resumableSessions: [session("claude-abc")] })}
        connecting={true}
        onStart={() => {}}
      />,
    );
    expect(allChoices()).toHaveLength(0);
    expect(screen.getByText(/Starting the agent backend/)).toBeTruthy();
  });
  /** The start screen's own text promises the permission choice cannot be changed afterwards, so a
   *  mode description that overstates what the mode does is the worst place in the product to be
   *  wrong. Bypass denies the editing tools -- `agent::disallowed_tools_for` -- because nothing
   *  gates them there, and the copy said "Every tool call proceeds immediately" for a day after
   *  that stopped being true.
   *
   *  This asserts the CLAIM, not the wording: the Rust list and this string are two statements of
   *  one policy with nothing making the compiler compare them, so what is pinned here is that the
   *  screen does not promise unconditional execution. */
  it("does not promise Bypass runs every tool, because it does not", () => {
    render(<ModeSelector hello={hello(["auto", "bypass"])} connecting={false} onStart={() => {}} />);
    const bypass = screen.getByText("Bypass").closest("button")!;
    expect(bypass.textContent).not.toMatch(/every tool call proceeds/i);
    expect(bypass.textContent).toMatch(/cannot edit files/i);
  });

});
