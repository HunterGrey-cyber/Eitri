// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { Composer } from "./Composer";

// See ModeSelector.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);

type Overrides = Partial<Parameters<typeof Composer>[0]>;

function renderComposer(overrides: Overrides = {}) {
  const onSend = vi.fn();
  const { container } = render(
    <Composer
      disabled={false}
      sessionEnded={false}
      closing={false}
      restoredDraft={null}
      // Defaulted to INPUT: every existing test in this file exercises the textarea directly, and
      // BROWSE's stand-in for it (`.composer-browse-hint`) is covered by its own describe block
      // below rather than by threading a mode override through every other test here.
      mode="input"
      onModeChange={vi.fn()}
      onSend={onSend}
      {...overrides}
    />,
  );
  const textarea = container.querySelector("textarea")!;
  return { container, textarea, onSend };
}

/** Same defaults, but able to change props afterwards -- the restored-draft path is driven entirely
 *  by a prop the host updates. */
function renderComposerWithRerender(overrides: Overrides = {}) {
  const onSend = vi.fn();
  const base = {
    disabled: false,
    sessionEnded: false,
    closing: false,
    restoredDraft: null,
    mode: "input" as const,
    onModeChange: vi.fn(),
    onSend,
    ...overrides,
  };
  const { container, rerender } = render(<Composer {...base} />);
  return {
    container,
    textarea: container.querySelector("textarea")!,
    onSend,
    rerender: (next: Overrides) => rerender(<Composer {...base} {...next} />),
  };
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

  it("refuses a whitespace-only turn rather than spending one on nothing", () => {
    const { textarea, onSend } = renderComposer();
    fireEvent.change(textarea, { target: { value: "   \n  " } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).not.toHaveBeenCalled();
  });

  /* `disabled` is the whole guard against a second turn being sent while one is running -- the
     backend rejects it, but the UI must not offer it. There is no Send button any more (spec §3.4,
     removed panel-as-document task 6 fix round 1) to also check as disabled -- Enter is the only
     path left, so it is the only one this pins. */
  it("cannot send while disabled, even via Enter", () => {
    const { textarea, onSend } = renderComposer({ disabled: true });
    expect(textarea.disabled).toBe(true);
    fireEvent.change(textarea, { target: { value: "anything" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).not.toHaveBeenCalled();
  });
});

// Send and Stop buttons were removed from the composer entirely (spec §3.4, panel-as-document
// task 6 fix round 1): Enter still sends and Shift+Enter still inserts a newline (both pinned
// above), and Stop now lives only in `StatusLine`, for mouse users -- see `StatusLine.test.tsx`'s
// own "offers a clickable Stop..." test for that half, gated the same way this one used to be, on
// the provider's advertised `interrupt` capability rather than the backend's name.

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

/* The two halves of not eating a typed message: the box refuses while the conversation is closing
   and says so, and a send the host refused comes back. */
describe("Composer while the conversation is closing", () => {
  it("refuses to send, and explains rather than just going grey", () => {
    const { container, textarea, onSend } = renderComposer({ disabled: true, closing: true });
    fireEvent.change(textarea, { target: { value: "still mine" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).not.toHaveBeenCalled();
    expect(textarea.value).toBe("still mine");
    expect(container.querySelector(".composer-closing")).not.toBeNull();
  });
});

describe("Composer restoring a refused draft", () => {
  it("puts the text back exactly as it was", () => {
    const { textarea, rerender } = renderComposerWithRerender();
    fireEvent.change(textarea, { target: { value: "the one that got away" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(textarea.value).toBe("");
    rerender({ restoredDraft: { text: "the one that got away", seq: 1 } });
    expect(textarea.value).toBe("the one that got away");
  });

  /* Two refusals of the SAME text are two distinct restores. A bare string prop would not change
     identity the second time and the box would silently stay empty. */
  it("restores identical text a second time", () => {
    const { textarea, rerender } = renderComposerWithRerender();
    rerender({ restoredDraft: { text: "same", seq: 1 } });
    expect(textarea.value).toBe("same");
    fireEvent.change(textarea, { target: { value: "" } });
    rerender({ restoredDraft: { text: "same", seq: 2 } });
    expect(textarea.value).toBe("same");
  });
});

/* BROWSE's rendering of the same control -- a line, not a box, so there is nothing for a stray
   keystroke to land in -- and the two mouse-driven paths that have to agree with the keyboard
   table in `keymap.ts` on what mode the panel is in. */
describe("Composer's BROWSE/INPUT split", () => {
  it("shows an empty-looking box instead of a textarea in BROWSE, with no instruction in it", () => {
    const { container } = renderComposer({ mode: "browse" });
    expect(container.querySelector("textarea")).toBeNull();
    // The owner asked for "按 i 开始输入" to go (2026-09-19). The stand-in reads like the textarea's
    // own placeholder, and `i`, a click and Tab still reach INPUT from it.
    expect(container.querySelector(".composer-browse-hint")!.textContent).toBe("Ask the agent...");
  });

  it("takes focus back when asked, even if it is already mounted", () => {
    // The path `setMode("input")` cannot cover: already INPUT, focus somewhere else. `onModeChange`
    // is a no-op here, so the blur cannot flip the mode out from under the test.
    const outside = document.createElement("button");
    document.body.appendChild(outside);
    const props = {
      disabled: false,
      sessionEnded: false,
      closing: false,
      restoredDraft: null,
      mode: "input" as const,
      onModeChange: vi.fn(),
      onSend: vi.fn(),
    };
    const { container, rerender } = render(<Composer {...props} focusRequest={0} />);
    outside.focus();
    expect(document.activeElement).toBe(outside);
    rerender(<Composer {...props} focusRequest={1} />);
    expect(document.activeElement).toBe(container.querySelector("textarea"));
    outside.remove();
  });

  it("reports focus on the hint line as entering INPUT", () => {
    const onModeChange = vi.fn();
    const { container } = renderComposer({ mode: "browse", onModeChange });
    fireEvent.focus(container.querySelector(".composer-browse-hint")!);
    expect(onModeChange).toHaveBeenCalledWith("input");
  });

  it("reports the textarea losing focus as leaving INPUT", () => {
    const onModeChange = vi.fn();
    const { textarea } = renderComposer({ mode: "input", onModeChange });
    fireEvent.blur(textarea);
    expect(onModeChange).toHaveBeenCalledWith("browse");
  });
});
