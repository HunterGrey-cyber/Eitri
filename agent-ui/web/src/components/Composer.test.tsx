// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { Composer } from "./Composer";

// See ModeSelector.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);

type Overrides = Partial<Parameters<typeof Composer>[0]>;

function renderComposer(overrides: Overrides = {}) {
  const onSend = vi.fn();
  const onInterrupt = vi.fn();
  const { container } = render(
    <Composer
      disabled={false}
      turnInProgress={false}
      sessionEnded={false}
      canInterrupt
      onSend={onSend}
      onInterrupt={onInterrupt}
      {...overrides}
    />,
  );
  const textarea = container.querySelector("textarea")!;
  const button = (label: string) =>
    Array.from(container.querySelectorAll("button")).find((b) => b.textContent === label);
  return { container, textarea, button, onSend, onInterrupt };
}

describe("Composer sending", () => {
  it("sends on Enter and clears the box", () => {
    const { textarea, onSend } = renderComposer();
    fireEvent.change(textarea, { target: { value: "hello" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("hello");
    expect(textarea.value).toBe("");
  });

  /* Shift+Enter is how a multi-paragraph prompt gets written at all. Sending on it would make the
     composer unusable for anything longer than one line. */
  it("does not send on Shift+Enter, and keeps what was typed", () => {
    const { textarea, onSend } = renderComposer();
    fireEvent.change(textarea, { target: { value: "first line" } });
    fireEvent.keyDown(textarea, { key: "Enter", shiftKey: true });
    expect(onSend).not.toHaveBeenCalled();
    expect(textarea.value).toBe("first line");
  });

  it("sends on a Send click too", () => {
    const { textarea, button, onSend } = renderComposer();
    fireEvent.change(textarea, { target: { value: "via the button" } });
    fireEvent.click(button("Send")!);
    expect(onSend).toHaveBeenCalledWith("via the button");
  });

  it("refuses a whitespace-only turn rather than spending one on nothing", () => {
    const { textarea, button, onSend } = renderComposer();
    fireEvent.change(textarea, { target: { value: "   \n  " } });
    fireEvent.click(button("Send")!);
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).not.toHaveBeenCalled();
  });

  /* `disabled` is the whole guard against a second turn being sent while one is running -- the
     backend rejects it, but the UI must not offer it. */
  it("cannot send while disabled, by click or by key", () => {
    const { textarea, button, onSend } = renderComposer({ disabled: true });
    expect(textarea.disabled).toBe(true);
    expect(button("Send")!.disabled).toBe(true);
    fireEvent.change(textarea, { target: { value: "anything" } });
    fireEvent.click(button("Send")!);
    expect(onSend).not.toHaveBeenCalled();
  });
});

describe("Composer stop control", () => {
  it("offers Stop only while a turn is actually in flight", () => {
    const idle = renderComposer({ turnInProgress: false });
    expect(idle.button("Stop")!.disabled).toBe(true);
    cleanup();
    const running = renderComposer({ turnInProgress: true, disabled: true });
    expect(running.button("Stop")!.disabled).toBe(false);
    fireEvent.click(running.button("Stop")!);
    expect(running.onInterrupt).toHaveBeenCalledTimes(1);
  });

  /* Gated on the advertised capability, not on the backend's name: a provider that reports it
     cannot interrupt gets no control at all, rather than one that posts a command the server
     rejects. */
  it("hides Stop entirely when the provider cannot interrupt", () => {
    const { button } = renderComposer({ canInterrupt: false, turnInProgress: true });
    expect(button("Stop")).toBeUndefined();
  });
});

describe("Composer on a session that has ended", () => {
  it("says the session ended rather than inviting a turn that cannot be taken", () => {
    const { textarea } = renderComposer({ sessionEnded: true, disabled: true });
    expect(textarea.placeholder).toContain("session has ended");
    expect(textarea.disabled).toBe(true);
  });

  it("keeps the ordinary prompt while the session is alive", () => {
    const { textarea } = renderComposer();
    expect(textarea.placeholder).toBe("Ask the agent...");
  });
});
