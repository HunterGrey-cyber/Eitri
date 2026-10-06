// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { ContinueInTerminal, HandoffCommandCard, handoffBlockedReason } from "./TerminalHandoff";

// See EmptyTab.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);

type Overrides = Partial<Parameters<typeof ContinueInTerminal>[0]>;

/** Panel round 2 (plan Task 10; spec §5.4): `ContinueInTerminal` is controlled now -- `open`
 *  defaults to `true` here, since every test below is about what it draws once something (`<leader>t`,
 *  the detail popover's trailing row) has already asked to open it; the "closed" state (`open`
 *  `false`, drawing nothing) is its own small describe block below. */
function renderControl(overrides: Overrides = {}) {
  const onHandoff = vi.fn();
  const onClose = vi.fn();
  const { container } = render(
    <ContinueInTerminal
      open={true}
      onClose={onClose}
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
  return { container, button, onHandoff, onClose };
}

/* The rule, mirrored from `panel/src/terminal_handoff.rs`'s `prepare_handoff`. Two implementations
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

describe("ContinueInTerminal closed (panel round 2 plan, Task 10: open is now the only gate)", () => {
  it("draws nothing at all while open is false, blocked or not", () => {
    const { container } = renderControl({ open: false });
    expect(container.textContent).toBe("");
    const { container: blocked } = renderControl({ open: false, providerSessionId: null });
    expect(blocked.textContent).toBe("");
  });
});

describe("ContinueInTerminal before it can be used", () => {
  it("says why in visible text, not only in a tooltip", () => {
    const { container } = renderControl({ providerSessionId: null });
    // Visible text: a control with no explanation reads as broken, and a `title` is not readable
    // at all without a pointer hovering it.
    expect(container.querySelector(".handoff-blocked")!.textContent).toContain("first turn");
  });

  it("says why while a turn is in flight", () => {
    const { container } = renderControl({ turnInProgress: true });
    expect(container.querySelector(".handoff-blocked")!.textContent).toContain("Stop");
  });

  it("draws no confirm dialog, and no button to hand off from, while blocked", () => {
    const { container, onHandoff } = renderControl({ providerSessionId: null });
    expect(container.querySelector(".handoff-confirm")).toBeNull();
    expect(container.querySelector("button")).toBeNull();
    expect(onHandoff).not.toHaveBeenCalled();
  });
});

describe("ContinueInTerminal confirmation", () => {
  /* Clicking must not hand off immediately: the action closes the conversation, and on a backend
     with no resume that is not undoable. The consequence is stated before it happens, not after --
     so it shows as soon as this opens (`<leader>t`/the popover row), with no button of its own to
     click first any more. */
  it("explains what will happen before anything acts", () => {
    const { container, onHandoff } = renderControl();
    expect(onHandoff).not.toHaveBeenCalled();
    expect(container.querySelector(".handoff-confirm")!.textContent).toContain("close");
  });

  it("hands off once, on the confirming click", () => {
    const { button, onHandoff } = renderControl();
    fireEvent.click(button("Close it and show me the command")!);
    expect(onHandoff).toHaveBeenCalledTimes(1);
  });

  it("can be backed out of via Cancel, which asks the host to close it", () => {
    const { button, onHandoff, onClose } = renderControl();
    fireEvent.click(button("Cancel")!);
    expect(onHandoff).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  /* Gated on the advertised capability, never on the backend's name -- the same rule Composer's
     Stop control follows. The default backend has no resume, and saying so is the difference
     between an informed choice and a surprise. */
  it("warns that the conversation cannot be reopened here when the provider cannot resume", () => {
    const { container } = renderControl({ canResume: false });
    expect(container.querySelector(".handoff-confirm")!.textContent).toContain("cannot be reopened");
  });

  it("does not claim it is unrecoverable when the provider can resume", () => {
    const { container } = renderControl({ canResume: true });
    expect(container.querySelector(".handoff-confirm")!.textContent).not.toContain("cannot be reopened");
  });
});

describe("ContinueInTerminal once a handoff is in flight", () => {
  /* Rust refuses a second handoff with a real message, and that message reaches a console.warn and
     nothing else. A control that is enabled and does nothing is worse than one that says why, which
     is the argument this component's own doc makes about the blocked states. */
  it("shows the reason visible, with no confirm dialog or button, while the close is running", () => {
    const { container } = renderControl({ handingOff: true });
    expect(container.querySelector(".handoff-blocked")!.textContent).toContain("being closed");
    expect(container.querySelector(".handoff-confirm")).toBeNull();
    expect(container.querySelector("button")).toBeNull();
  });

  it("cannot be confirmed a second time after the first confirmation", () => {
    const { button, onHandoff } = renderControl();
    fireEvent.click(button("Close it and show me the command")!);
    expect(onHandoff).toHaveBeenCalledTimes(1);
    // The host sets `handingOff` synchronously when it posts the command, so the very next render
    // is already blocked -- checked here as a fresh render standing in for that next render, the
    // same way the old version of this test did.
    cleanup();
    const second = renderControl({ handingOff: true });
    expect(second.container.querySelector(".handoff-confirm")).toBeNull();
    expect(second.container.querySelector("button")).toBeNull();
  });
});

describe("ContinueInTerminal confirmation and a changing world", () => {
  /* Correction from the pre-Task-10 version of this test: with a `confirming` state of its own,
     this component used to remember a dismissed-by-blocking confirmation and NOT bring it back once
     the block cleared -- the bug that guarded against was a stale re-open with no new click behind
     it. Now `open` is the host's own explicit fact (only `<leader>t`/the popover row set it, and
     only `Esc`/Cancel/a finished handoff clear it) rather than this component's memory of a click,
     so while the host keeps it open across a transient block, this always shows a dialog that is
     true of the CURRENT world rather than a stale one -- and does reappear once the block clears,
     which is correct for what `open` means now: the confirmation is not a guess about whether the
     block matches what this component last saw, it is what `open && blocked === null` says right
     now. */
  it("reflects the current block on every render, rather than remembering a stale dismissal", () => {
    const onHandoff = vi.fn();
    const props = { open: true, onClose: vi.fn(), providerSessionId: "1857dcd5", canResume: false, handingOff: false, onHandoff };
    const { container, rerender } = render(<ContinueInTerminal {...props} turnInProgress={false} />);

    expect(container.querySelector(".handoff-confirm")).not.toBeNull();

    // A turn starts while the confirmation is open.
    rerender(<ContinueInTerminal {...props} turnInProgress />);
    expect(container.querySelector(".handoff-confirm")).toBeNull();
    expect(container.querySelector(".handoff-blocked")).not.toBeNull();

    // ...and finishes. `open` never changed, so the confirmation reflects the current, unblocked
    // world again -- the host is the one that decides whether it should still be open at all.
    rerender(<ContinueInTerminal {...props} turnInProgress={false} />);
    expect(container.querySelector(".handoff-confirm")).not.toBeNull();
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
  it("says the terminal session uses the user's own settings rather than Eitri's gate", () => {
    const { container } = render(<HandoffCommandCard handoff={HANDOFF} />);
    expect(container.textContent).toContain("your own Claude Code settings");
  });
});
