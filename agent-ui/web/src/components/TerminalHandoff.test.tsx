// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { ContinueInTerminal, HandoffCommandCard, handoffBlockedReason } from "./TerminalHandoff";

// See EmptyTab.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);

type Overrides = Partial<Parameters<typeof ContinueInTerminal>[0]>;

function renderControl(overrides: Overrides = {}) {
  const onHandoff = vi.fn();
  const { container } = render(
    <ContinueInTerminal
      providerSessionId="1857dcd5-973b-46a2"
      turnInProgress={false}
      canResume={false}
      handingOff={false}
      onHandoff={onHandoff}
      {...overrides}
    />,
  );
  const button = (label: string) =>
    Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes(label));
  return { container, button, onHandoff };
}

/* The rule, mirrored from `shell/src/terminal_handoff.rs`'s `prepare_handoff`. Two implementations
   of one rule is deliberate and is the same arrangement `reducer.ts` has with the Rust projection:
   Rust is the enforcement (it rejects the command outright), this is the affordance (it stops the
   control being offered). Each side is tested on its own; neither is tested only through the
   other. */
describe("handoffBlockedReason", () => {
  it("blocks a conversation that has never taken a turn, and says the first turn is what issues the id", () => {
    const reason = handoffBlockedReason(null, false);
    expect(reason).not.toBeNull();
    expect(reason).toContain("first turn");
  });

  it("blocks a turn that is still running, and points at Stop rather than just refusing", () => {
    const reason = handoffBlockedReason("1857dcd5", true);
    expect(reason).not.toBeNull();
    expect(reason).toContain("Stop");
  });

  /* An empty string is what a provider session id that crossed proto3 unset looks like -- an absent
     string arrives as "" there, not as null. Treating it as a real id would build
     `claude --resume ` with nothing to resume. */
  it("treats an empty id as no id at all", () => {
    expect(handoffBlockedReason("", false)).not.toBeNull();
  });

  it("allows it once an id exists and no turn is running", () => {
    expect(handoffBlockedReason("1857dcd5", false)).toBeNull();
  });
});

describe("ContinueInTerminal before it can be used", () => {
  it("is disabled and says why in visible text, not only in a tooltip", () => {
    const { container, button } = renderControl({ providerSessionId: null });
    expect(button("Continue in a terminal")!.disabled).toBe(true);
    // Visible text: a disabled control with no explanation reads as broken, and a `title` is not
    // readable at all without a pointer hovering it.
    expect(container.querySelector(".handoff-blocked")!.textContent).toContain("first turn");
  });

  it("is disabled while a turn is in flight", () => {
    const { container, button } = renderControl({ turnInProgress: true });
    expect(button("Continue in a terminal")!.disabled).toBe(true);
    expect(container.querySelector(".handoff-blocked")!.textContent).toContain("Stop");
  });

  it("cannot hand off by clicking while disabled", () => {
    const { button, onHandoff } = renderControl({ providerSessionId: null });
    fireEvent.click(button("Continue in a terminal")!);
    expect(onHandoff).not.toHaveBeenCalled();
  });
});

describe("ContinueInTerminal confirmation", () => {
  /* Clicking must not hand off immediately: the action closes the conversation, and on a backend
     with no resume that is not undoable. The consequence is stated before it happens, not after. */
  it("explains what will happen instead of acting on the first click", () => {
    const { container, button, onHandoff } = renderControl();
    fireEvent.click(button("Continue in a terminal")!);
    expect(onHandoff).not.toHaveBeenCalled();
    expect(container.querySelector(".handoff-confirm")!.textContent).toContain("close");
  });

  it("hands off once, on the confirming click", () => {
    const { button, onHandoff } = renderControl();
    fireEvent.click(button("Continue in a terminal")!);
    fireEvent.click(button("Close it and show me the command")!);
    expect(onHandoff).toHaveBeenCalledTimes(1);
  });

  it("can be backed out of without closing anything", () => {
    const { container, button, onHandoff } = renderControl();
    fireEvent.click(button("Continue in a terminal")!);
    fireEvent.click(button("Cancel")!);
    expect(onHandoff).not.toHaveBeenCalled();
    expect(container.querySelector(".handoff-confirm")).toBeNull();
  });

  /* Gated on the advertised capability, never on the backend's name -- the same rule Composer's
     Stop control follows. The default backend has no resume, and saying so is the difference
     between an informed choice and a surprise. */
  it("warns that the conversation cannot be reopened here when the provider cannot resume", () => {
    const { container, button } = renderControl({ canResume: false });
    fireEvent.click(button("Continue in a terminal")!);
    expect(container.querySelector(".handoff-confirm")!.textContent).toContain("cannot be reopened");
  });

  it("does not claim it is unrecoverable when the provider can resume", () => {
    const { container, button } = renderControl({ canResume: true });
    fireEvent.click(button("Continue in a terminal")!);
    expect(container.querySelector(".handoff-confirm")!.textContent).not.toContain("cannot be reopened");
  });
});

describe("ContinueInTerminal once a handoff is in flight", () => {
  /* Rust refuses a second handoff with a real message, and that message reaches a console.warn and
     nothing else. A control that is enabled and does nothing is worse than a disabled one that says
     why, which is the argument this component's own doc makes about the blocked states. */
  it("is disabled, with the reason visible, while the close is running", () => {
    const { container, button } = renderControl({ handingOff: true });
    expect(button("Continue in a terminal")!.disabled).toBe(true);
    expect(container.querySelector(".handoff-blocked")!.textContent).toContain("being closed");
  });

  it("cannot be confirmed a second time after the first confirmation", () => {
    const { button, onHandoff } = renderControl();
    fireEvent.click(button("Continue in a terminal")!);
    fireEvent.click(button("Close it and show me the command")!);
    expect(onHandoff).toHaveBeenCalledTimes(1);
    // The host sets `handingOff` synchronously when it posts the command, so the very next render
    // is already blocked.
    cleanup();
    const second = renderControl({ handingOff: true });
    fireEvent.click(second.button("Continue in a terminal")!);
    expect(second.container.querySelector(".handoff-confirm")).toBeNull();
    expect(second.onHandoff).not.toHaveBeenCalled();
  });
});

describe("ContinueInTerminal confirmation and a changing world", () => {
  /* A confirmation dialog must never appear without a deliberate click. It could: `confirming`
     stayed true while a block was showing, so a turn starting while the block was open replaced it
     with the disabled button and then brought the block BACK by itself when the turn finished. */
  it("does not reappear by itself once whatever blocked it goes away", () => {
    const onHandoff = vi.fn();
    const props = { providerSessionId: "1857dcd5", canResume: false, handingOff: false, onHandoff };
    const { container, rerender } = render(<ContinueInTerminal {...props} turnInProgress={false} />);
    const button = (label: string) =>
      Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes(label));

    fireEvent.click(button("Continue in a terminal")!);
    expect(container.querySelector(".handoff-confirm")).not.toBeNull();

    // A turn starts while the confirmation is open.
    rerender(<ContinueInTerminal {...props} turnInProgress />);
    expect(container.querySelector(".handoff-confirm")).toBeNull();

    // ...and finishes. The confirmation must NOT come back on its own.
    rerender(<ContinueInTerminal {...props} turnInProgress={false} />);
    expect(container.querySelector(".handoff-confirm")).toBeNull();
    expect(button("Continue in a terminal")!.disabled).toBe(false);
    expect(onHandoff).not.toHaveBeenCalled();
  });
});

describe("HandoffCommandCard", () => {
  const HANDOFF = {
    command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
    cwd: "/home/user/project",
    providerSessionId: "1857dcd5-973b-46a2",
  };

  it("shows the exact command, verbatim and selectable", () => {
    const { container } = render(<HandoffCommandCard handoff={HANDOFF} />);
    // <pre>, not a paragraph: this is a line to copy character for character.
    expect(container.querySelector("pre")!.textContent).toBe(HANDOFF.command);
  });

  /* The condition design doc §8.3 attaches to this path: handing the user a command to run is not
     the strongly-exclusive handoff, and must be shown as the raw/manual path with a concurrency
     warning. Nothing here may read as a guarantee -- §8.5 and §17.7 reject one even where a lease
     IS held, and this path holds none. */
  it("says plainly that nothing is holding the session", () => {
    const { container } = render(<HandoffCommandCard handoff={HANDOFF} />);
    const text = container.textContent ?? "";
    expect(text).toContain("no lock");
    expect(text).toContain("same transcript");
    expect(text).not.toContain("exclusive");
  });

  /* The resumed session is an ordinary `claude`, with none of the flags `agent` spawns its own
     with -- notably not the `PreToolUse` gate that produces this panel's permission cards. */
  it("says the terminal session uses the user's own settings rather than Neovibe's gate", () => {
    const { container } = render(<HandoffCommandCard handoff={HANDOFF} />);
    expect(container.textContent).toContain("your own Claude Code settings");
  });
});
