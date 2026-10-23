// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, getByRole, render } from "@testing-library/react";
import { Composer } from "./Composer";
import { COMPOSER_CHORDS } from "../composerKeys";
import { resolveKey } from "../keymap";

// See EmptyTab.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
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
      running={false}
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
    running: false,
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

/** The composer is a rounded card (`index.css`), and the card is its mark: no shell-prompt glyph is drawn
 *  in front of the box, in either mode or after the session ends. */
describe("Composer's prompt glyph", () => {
  it("draws no glyph before the textarea in INPUT, the stand-in in BROWSE, or an ended session's line", () => {
    for (const overrides of [{}, { mode: "browse" as const }, { mode: "browse" as const, sessionEnded: true }]) {
      const { container } = renderComposer(overrides);
      expect(container.querySelector(".composer-prompt")).toBeNull();
      expect(container.textContent).not.toContain("❯");
      cleanup();
    }
  });
});

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
     backend rejects it, but the UI must not offer it. There is no Send button any more to also
     check as disabled -- Enter is the only
     path left, so it is the only one this pins. */
  it("cannot send while disabled, even via Enter", () => {
    const { textarea, onSend } = renderComposer({ disabled: true });
    expect(textarea.disabled).toBe(true);
    fireEvent.change(textarea, { target: { value: "anything" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).not.toHaveBeenCalled();
  });
});

// Send and Stop buttons are absent from the composer. Keyboard sending stays available:
// Enter still sends and Shift+Enter still inserts a newline (both pinned
// above), and Stop lives only in `ActivityLine` (formerly
// `StatusLine`), for mouse users -- see `ActivityLine.test.tsx`'s own "shows the turn's motion and
// Stop..." test for that half, gated the same way this one used to be, on the provider's
// advertised `interrupt` capability rather than the backend's name.

describe("Composer on a session that has ended", () => {
  it("says the session ended rather than inviting a turn that cannot be taken", () => {
    const { textarea } = renderComposer({ sessionEnded: true, disabled: true });
    expect(textarea.placeholder).toContain("session has ended");
    expect(textarea.disabled).toBe(true);
  });

  it("draws no grey prompt in an ordinary empty box, and opens one line tall", () => {
    const { textarea } = renderComposer();
    expect(textarea.placeholder).toBe("");
    expect(textarea.rows).toBe(1);
  });
});

/* The two halves of not eating a typed message: the box refuses while the conversation is closing
   and says so, and a send the host refused comes back. */
/* A held-back slash command is never sent, the draft stays, and a local flash
 *  names it. `slashCommands.test.ts` covers the classification itself; this only pins the wiring:
 *  Enter goes through `heldBackSlashCommand`, and the flash renders where `submit` puts it. */
describe("Composer and slash commands (P10)", () => {
  it("does not send a held-back command (/login), keeps the draft, and flashes", () => {
    const { container, textarea, onSend } = renderComposer();
    fireEvent.change(textarea, { target: { value: "/login" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).not.toHaveBeenCalled();
    expect(textarea.value).toBe("/login");
    expect(container.querySelector(".composer-slash-flash")!.textContent).toBe(
      "/login does not work in Eitri — ? lists the ones that do",
    );
  });

  it("holds back interactive-only commands with no local action", () => {
    for (const command of ["/config", "/resume yesterday"]) {
      const { textarea, onSend } = renderComposer();
      fireEvent.change(textarea, { target: { value: command } });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSend, command).not.toHaveBeenCalled();
      expect(textarea.value, command).toBe(command);
      cleanup();
    }
  });

  it("sends a leading / as typed in another agent's tab, held-back words included", () => {
    for (const command of ["/login", "/config", "/resume"]) {
      const { container, textarea, onSend } = renderComposer({ claudeSlashCommands: false });
      fireEvent.change(textarea, { target: { value: command } });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSend, command).toHaveBeenCalledWith(command);
      expect(container.querySelector(".composer-slash-flash"), command).toBeNull();
      cleanup();
    }
  });

  /* /model is no longer interactive-only even bare -- its reply
   *  parses into a picker (`App.tsx`'s own wiring; not this component's concern). */
  it("sends /model bare or with an argument alike", () => {
    for (const command of ["/model", "/model sonnet"]) {
      const { textarea, onSend } = renderComposer();
      fireEvent.change(textarea, { target: { value: command } });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSend, command).toHaveBeenCalledWith(command);
      expect(textarea.value, command).toBe("");
      cleanup();
    }
  });

  it("sends a command classed works (/compact)", () => {
    const { textarea, onSend } = renderComposer();
    fireEvent.change(textarea, { target: { value: "/compact" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("/compact");
    expect(textarea.value).toBe("");
  });

  it("sends an unknown slash command as today", () => {
    const { textarea, onSend } = renderComposer();
    fireEvent.change(textarea, { target: { value: "/unknown-thing" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("/unknown-thing");
    expect(textarea.value).toBe("");
  });

  it("never queues or sends-now a held-back command either", () => {
    const onQueue = vi.fn();
    const onSendNow = vi.fn();
    const running = renderComposer({ running: true, onQueue, onSendNow });
    fireEvent.change(running.textarea, { target: { value: "/login" } });
    fireEvent.keyDown(running.textarea, { key: "Enter" });
    expect(onQueue).not.toHaveBeenCalled();
    expect(running.textarea.value).toBe("/login");
    cleanup();

    const idle = renderComposer({ running: true, onQueue, onSendNow });
    fireEvent.change(idle.textarea, { target: { value: "/login" } });
    fireEvent.keyDown(idle.textarea, { key: "Enter", ctrlKey: true });
    expect(onSendNow).not.toHaveBeenCalled();
    expect(idle.textarea.value).toBe("/login");
  });

  it("flashes again on a second, identical refusal in a row", () => {
    const { container, textarea } = renderComposer();
    fireEvent.change(textarea, { target: { value: "/login" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    const first = container.querySelector(".composer-slash-flash")!;
    expect(first).not.toBeNull();
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(container.querySelector(".composer-slash-flash")).not.toBeNull();
  });
});

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
  it("shows an empty-looking box instead of a textarea in BROWSE, with the C1b/S3 hint after it", () => {
    const { container } = renderComposer({ mode: "browse" });
    expect(container.querySelector("textarea")).toBeNull();
    // The BROWSE box has a dim reminder of the way into INPUT, because an arrival
    // lands in BROWSE. The reminder tests below pin its appearance and behavior;
    // `i`, a click and Tab still reach INPUT from the box.
    // Since 2026-10-15 the reminder is the key alone at the box's right end (`.composer-key`).
    expect(container.querySelector(".composer-browse-hint")!.textContent).toBe("Ask the agent...i");
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
      running: false,
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

/** The BROWSE reminder: the dim `i or Ctrl+j to type` that follows the placeholder or the draft in
 *  BROWSE, restoring what 2026-09-19 removed once that removal's own premise (`Ctrl+l` opening the
 *  composer directly) ended. */
/* The box shows a reminder of the way in; since 2026-10-15 it is the key alone at the box's right end, as
   the start screen's menu shows its keys, not the sentence "i or Ctrl+j to type" after the placeholder. */
describe("Composer's BROWSE hint (C1b/S3)", () => {
  it("shows the key at the right end, with nothing typed, and no sentence", () => {
    const { container } = renderComposer({ mode: "browse" });
    expect(container.querySelector(".composer-key")!.textContent).toBe("i");
    expect(container.textContent).not.toContain("Ctrl+j to type");
  });

  it("shows the same key after an unsent draft too", () => {
    const { container } = renderComposer({ mode: "browse", restoredDraft: { text: "half a thought", seq: 1 } });
    const hint = container.querySelector(".composer-browse-hint")!;
    expect(hint.querySelector(".composer-draft")).not.toBeNull();
    expect(hint.querySelector(".composer-key")!.textContent).toBe("i");
  });

  it("names another key where the host says so", () => {
    const { container } = renderComposer({ mode: "browse", browseKey: "a" });
    expect(container.querySelector(".composer-key")!.textContent).toBe("a");
  });

  it("is not shown in INPUT -- the caret is the sign there", () => {
    const { container } = renderComposer({ mode: "input" });
    expect(container.querySelector(".composer-key")).toBeNull();
  });

  it("is not shown on a session that has ended -- that row says r instead", () => {
    const { container } = renderComposer({ mode: "browse", sessionEnded: true });
    expect(container.querySelector(".composer-key")).toBeNull();
    // The row above says "Press r…"; the composer does not repeat it.
    expect(container.querySelector(".composer-browse-hint")!.textContent).toBe("This session has ended.");
  });

  it("is not shown while a scratch round trip owns the draft (editingInNvim)", () => {
    const { container } = renderComposer({ mode: "browse", editingInNvim: true });
    expect(container.querySelector(".composer-key")).toBeNull();
  });
});

/** Where the caret goes on entering INPUT, now that `Composer`'s own
 *  `[focusRequest, mode]` effect runs `focusAtCaret` on every transition into "input" rather than
 *  only when `focusRequest` had separately been bumped past 0 (the focus bug was a
 *  fresh `autoFocus` mount of a non-empty box is not reliable on its own, in this project's own
 *  measured GUI pass). */
describe("Composer's caret on entering INPUT (C1a)", () => {
  it("i restores the caret left at 3 in 'hello world'", () => {
    const { container, rerender } = renderComposerWithRerender({ mode: "input" });
    const textarea = container.querySelector("textarea")!;
    fireEvent.change(textarea, { target: { value: "hello world" } });
    textarea.setSelectionRange(3, 3);
    fireEvent.keyUp(textarea, { key: "ArrowLeft" });
    // `i`: caret "kept" (the default `caretOnFocus`), the same round trip BROWSE/INPUT always was.
    rerender({ mode: "browse" });
    rerender({ mode: "input", focusRequest: 1 });
    expect(container.querySelector("textarea")!.selectionStart).toBe(3);
  });

  it("A puts the caret at the end of 'hello world' (11), regardless of where it was left", () => {
    const { container, rerender } = renderComposerWithRerender({ mode: "input" });
    const textarea = container.querySelector("textarea")!;
    fireEvent.change(textarea, { target: { value: "hello world" } });
    textarea.setSelectionRange(3, 3);
    fireEvent.keyUp(textarea, { key: "ArrowLeft" });
    rerender({ mode: "browse" });
    rerender({ mode: "input", focusRequest: 1, caretOnFocus: "end" });
    expect(container.querySelector("textarea")!.selectionStart).toBe(11);
  });

  /** Start-of-draft case: a box that mounts already carrying a draft (never one the user
   *  typed into THIS instance first) must not land the caret at 0. */
  it("a fresh mount with a draft puts the caret at the end, not the start", () => {
    const { textarea } = renderComposer({ mode: "input", restoredDraft: { text: "existing words", seq: 1 } });
    expect(document.activeElement).toBe(textarea);
    expect(textarea.selectionStart).toBe("existing words".length);
  });
});

describe("Composer in BROWSE (C2)", () => {
  it("shows the unsent draft, muted and cut to two lines, and how many are queued", () => {
    const { container } = renderComposer({
      mode: "browse",
      restoredDraft: { text: "line one\nline two\nline three", seq: 1 },
      queueCount: 2,
    });
    const hint = container.querySelector(".composer-browse-hint")!;
    expect(hint.querySelector(".composer-draft")!.textContent).toBe("line one\nline two…");
    expect(hint.querySelector(".composer-queued")!.textContent).toBe("+2 queued");
  });

  it("reads like the empty box it stands in for when there is nothing typed", () => {
    const { container } = renderComposer({ mode: "browse" });
    expect(container.querySelector(".composer-browse-hint")!.textContent).toBe("Ask the agent...i");
  });

  // `onShiftTab` is gone -- App.tsx's document-capture router (`modeKey.ts`) now
  // claims Shift+Tab ahead of this component entirely, in every mode,
  // as exercised in App.test.tsx's "Shift+Tab anywhere in the chat" describe block.
});

describe("Composer during a turn (C1, F1)", () => {
  it("stays typable while a turn runs and queues on Enter", () => {
    const onQueue = vi.fn();
    const { textarea, onSend } = renderComposer({ running: true, onQueue });
    expect(textarea.disabled).toBe(false);
    expect(textarea.placeholder).toBe("Enter holds it until the turn ends…");
    fireEvent.change(textarea, { target: { value: "and the tests" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onQueue).toHaveBeenCalledWith("and the tests");
    expect(onSend).not.toHaveBeenCalled();
    expect(textarea.value).toBe("");
  });

  it("sends now on Ctrl+Enter while running, and is Enter while idle", () => {
    const onSendNow = vi.fn();
    const running = renderComposer({ running: true, onSendNow });
    fireEvent.change(running.textarea, { target: { value: "stop, do this" } });
    fireEvent.keyDown(running.textarea, { key: "Enter", ctrlKey: true });
    expect(onSendNow).toHaveBeenCalledWith("stop, do this");
    cleanup();
    const idle = renderComposer({ running: false, onSendNow });
    fireEvent.change(idle.textarea, { target: { value: "hi" } });
    fireEvent.keyDown(idle.textarea, { key: "Enter", ctrlKey: true });
    expect(idle.onSend).toHaveBeenCalledWith("hi");
  });

  /** fcitx5's Enter commits the raw letters; it must not send or queue. */
  it("enter_while_composing_neither_sends_nor_queues", () => {
    const onQueue = vi.fn();
    const onSendNow = vi.fn();
    for (const running of [false, true]) {
      const { textarea, onSend } = renderComposer({ running, onQueue, onSendNow });
      fireEvent.change(textarea, { target: { value: "git" } });
      fireEvent.keyDown(textarea, { key: "Enter", isComposing: true });
      fireEvent.keyDown(textarea, { key: "Enter", keyCode: 229 });
      fireEvent.keyDown(textarea, { key: "Enter", ctrlKey: true, isComposing: true });
      expect(onSend).not.toHaveBeenCalled();
      expect(textarea.value).toBe("git");
      cleanup();
    }
    expect(onQueue).not.toHaveBeenCalled();
    expect(onSendNow).not.toHaveBeenCalled();
  });

  it("takes focus again when it becomes enabled while the mode is INPUT", () => {
    const { textarea, rerender } = renderComposerWithRerender({ disabled: true });
    expect(document.activeElement).not.toBe(textarea);
    rerender({ disabled: false });
    expect(document.activeElement).toBe(textarea);
  });
});

describe("Composer history and queue (C5)", () => {
  it("takes the queue back on ↑ from the first line, else walks history, and ↓ returns the draft", () => {
    const onTakeBackQueue = vi.fn();
    const queued = renderComposer({ queueCount: 2, onTakeBackQueue, history: ["old"] });
    fireEvent.keyDown(queued.textarea, { key: "ArrowUp" });
    expect(onTakeBackQueue).toHaveBeenCalledTimes(1);
    expect(queued.textarea.value, "the merge waits for queue_taken").toBe("");
    cleanup();

    const { textarea } = renderComposer({ history: ["first", "second"] });
    fireEvent.change(textarea, { target: { value: "half" } });
    textarea.setSelectionRange(0, 0);
    fireEvent.keyDown(textarea, { key: "ArrowUp" });
    expect(textarea.value).toBe("second");
    fireEvent.keyDown(textarea, { key: "ArrowUp" });
    expect(textarea.value).toBe("first");
    textarea.setSelectionRange(textarea.value.length, textarea.value.length);
    fireEvent.keyDown(textarea, { key: "ArrowDown" });
    fireEvent.keyDown(textarea, { key: "ArrowDown" });
    expect(textarea.value).toBe("half");
  });

  it("leaves ↑ to the textarea when the caret is not on the first line", () => {
    const { textarea } = renderComposer({ history: ["old"] });
    fireEvent.change(textarea, { target: { value: "one\ntwo" } });
    textarea.setSelectionRange(6, 6);
    expect(fireEvent.keyDown(textarea, { key: "ArrowUp" })).toBe(true);
    expect(textarea.value).toBe("one\ntwo");
  });

  it("merges a queue that came back ahead of the box's text", () => {
    const onDraftChange = vi.fn();
    const { textarea, rerender } = renderComposerWithRerender({ onDraftChange });
    fireEvent.change(textarea, { target: { value: "now" } });
    rerender({ queueTaken: { texts: ["a", "b"], seq: 1 }, onDraftChange });
    expect(textarea.value).toBe("a\n\nb\n\nnow");
    expect(onDraftChange).toHaveBeenLastCalledWith("a\n\nb\n\nnow");
  });

  it("searches history on Ctrl+r and puts the match in the box without sending it", () => {
    const { container, textarea, onSend } = renderComposer({ history: ["fix the parser", "run the tests"] });
    fireEvent.keyDown(textarea, { key: "r", ctrlKey: true });
    const search = container.querySelector<HTMLInputElement>(".history-search input")!;
    fireEvent.change(search, { target: { value: "fix" } });
    expect(container.querySelector(".history-search")!.textContent).toContain("fix the parser");
    fireEvent.keyDown(search, { key: "Enter", isComposing: true });
    expect(container.querySelector(".history-search"), "the IME's Enter").not.toBeNull();
    fireEvent.keyDown(search, { key: "Enter" });
    expect(container.querySelector(".history-search")).toBeNull();
    expect(textarea.value).toBe("fix the parser");
    expect(onSend).not.toHaveBeenCalled();
  });
});

/** A `Ctrl+r` search left open when the panel leaves INPUT another way than its own Esc (a tab switch, a
 *  trip to the editor and back, which both land BROWSE) ends with INPUT: the next `i` opens the box, not a
 *  search line nobody asked for, which would take what is typed next. */
describe("the Ctrl+r search ends with INPUT", () => {
  it("is gone after BROWSE and back, and the box has the keys", () => {
    const { container, textarea, rerender } = renderComposerWithRerender({ history: ["fix the parser"] });
    fireEvent.keyDown(textarea, { key: "r", ctrlKey: true });
    expect(container.querySelector(".history-search")).not.toBeNull();
    rerender({ history: ["fix the parser"], mode: "browse" });
    rerender({ history: ["fix the parser"], mode: "input" });
    expect(container.querySelector(".history-search")).toBeNull();
    expect(document.activeElement).toBe(container.querySelector("textarea"));
  });
});

describe("Composer keys", () => {
  it("interrupts on Ctrl+c while a turn runs, keeping the draft", () => {
    const onInterrupt = vi.fn();
    const { textarea } = renderComposer({ running: true, onInterrupt });
    fireEvent.change(textarea, { target: { value: "keep me" } });
    fireEvent.keyDown(textarea, { key: "c", ctrlKey: true });
    expect(onInterrupt).toHaveBeenCalledTimes(1);
    expect(textarea.value).toBe("keep me");
  });

  it("clears an idle draft into history on Ctrl+c, and leaves an empty box's Ctrl+c alone", () => {
    const onHistoryPush = vi.fn();
    const { textarea } = renderComposer({ onHistoryPush });
    expect(fireEvent.keyDown(textarea, { key: "c", ctrlKey: true })).toBe(true);
    fireEvent.change(textarea, { target: { value: "never mind" } });
    fireEvent.keyDown(textarea, { key: "c", ctrlKey: true });
    expect(onHistoryPush).toHaveBeenCalledWith("never mind");
    expect(textarea.value).toBe("");
  });

  it("deletes a word with Ctrl+w", () => {
    const { textarea } = renderComposer();
    fireEvent.change(textarea, { target: { value: "git push origin" } });
    textarea.setSelectionRange(15, 15);
    fireEvent.keyDown(textarea, { key: "w", ctrlKey: true });
    expect(textarea.value).toBe("git push ");
  });

  it("moves the caret to the line's end with Ctrl+e and its start with Ctrl+a, WebKitGTK having neither", () => {
    const { textarea } = renderComposer();
    fireEvent.change(textarea, { target: { value: "first line\nsecond line" } });
    textarea.setSelectionRange(14, 14);
    expect(fireEvent.keyDown(textarea, { key: "e", ctrlKey: true }), "the default is cancelled").toBe(false);
    expect([textarea.selectionStart, textarea.selectionEnd]).toEqual([22, 22]);
    fireEvent.keyDown(textarea, { key: "a", ctrlKey: true });
    expect([textarea.selectionStart, textarea.selectionEnd]).toEqual([11, 11]);
    textarea.setSelectionRange(3, 3);
    fireEvent.keyDown(textarea, { key: "e", ctrlKey: true });
    expect(textarea.selectionStart).toBe(10);
    expect(textarea.value, "the text is untouched").toBe("first line\nsecond line");
  });

  it("leaves Ctrl+Shift+e, Alt+Ctrl+e and a bare e to their own meaning", () => {
    const { textarea } = renderComposer();
    fireEvent.change(textarea, { target: { value: "abc def" } });
    textarea.setSelectionRange(1, 1);
    expect(fireEvent.keyDown(textarea, { key: "E", ctrlKey: true, shiftKey: true })).toBe(true);
    expect(fireEvent.keyDown(textarea, { key: "e", ctrlKey: true, altKey: true })).toBe(true);
    expect(fireEvent.keyDown(textarea, { key: "e" })).toBe(true);
    expect(textarea.selectionStart).toBe(1);
  });

  it("opens the keymap on ? only from an empty box", () => {
    const onOpenKeymap = vi.fn();
    const { textarea } = renderComposer({ onOpenKeymap });
    fireEvent.keyDown(textarea, { key: "?", shiftKey: true });
    expect(onOpenKeymap).toHaveBeenCalledTimes(1);
    fireEvent.change(textarea, { target: { value: "why" } });
    expect(fireEvent.keyDown(textarea, { key: "?", shiftKey: true })).toBe(true);
    expect(onOpenKeymap).toHaveBeenCalledTimes(1);
  });

  it("asks for nvim on Ctrl+g and says so while the draft is out", () => {
    const onEditInNvim = vi.fn();
    const { textarea, rerender } = renderComposerWithRerender({ onEditInNvim });
    fireEvent.change(textarea, { target: { value: "long prose" } });
    fireEvent.keyDown(textarea, { key: "g", ctrlKey: true });
    expect(onEditInNvim).toHaveBeenCalledWith("long prose");
    rerender({ editingInNvim: true, onEditInNvim });
    expect(textarea.readOnly).toBe(true);
    expect(textarea.placeholder).toBe("editing in nvim — :wq to return");
  });

  /** A landing put the caret before the existing text. A box the
   *  user never placed the caret in takes it at the end; one they left mid-text gets it back (with a saved caret). */
  it("puts the caret at the end of existing text, or back where the user left it", () => {
    const text = { text: "existing words", seq: 1 };
    const { container, rerender } = renderComposerWithRerender({ mode: "browse", restoredDraft: text });
    rerender({ mode: "input", focusRequest: 1, restoredDraft: text });
    const textarea = container.querySelector("textarea")!;
    expect(document.activeElement).toBe(textarea);
    expect(textarea.selectionStart).toBe("existing words".length);
    textarea.setSelectionRange(3, 3);
    fireEvent.keyUp(textarea, { key: "ArrowLeft" });
    rerender({ mode: "browse", focusRequest: 1, restoredDraft: text });
    rerender({ mode: "input", focusRequest: 2, restoredDraft: text });
    expect(container.querySelector("textarea")!.selectionStart).toBe(3);
  });

  it("grows with its content up to 40% of the panel", () => {
    const { textarea } = renderComposer();
    Object.defineProperty(textarea, "scrollHeight", { value: 900, configurable: true });
    Object.defineProperty(window, "innerHeight", { value: 1000, configurable: true });
    fireEvent.change(textarea, { target: { value: "x\n".repeat(50) } });
    expect(textarea.style.height).toBe("400px");
  });
});

/* `composerKeys.test.ts` tied `INPUT_KEYS` only to `COMPOSER_CHORDS`, a
   hand-kept list nothing derived from the handler, so deleting `Ctrl+w` from `onKeyDown` left it
   green. This table exercises every `COMPOSER_CHORDS` entry against the real `Composer` (or, for the
   two chords `Composer` leaves to bubble, against `resolveKey("input", …)`, which is what `App`'s
   INPUT branch runs), and its keys must equal `COMPOSER_CHORDS` -- so the chain is
   `INPUT_KEYS` = `COMPOSER_CHORDS` = the chords that really do something. */
describe("every chord COMPOSER_CHORDS names does what INPUT_KEYS says", () => {
  const typed = (textarea: HTMLTextAreaElement, value: string) => {
    fireEvent.change(textarea, { target: { value } });
    textarea.setSelectionRange(value.length, value.length);
  };

  const behaviours: Record<string, () => void> = {
    Enter: () => {
      const { textarea, onSend } = renderComposer();
      typed(textarea, "hi");
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSend).toHaveBeenCalledWith("hi");
    },
    "Ctrl+Enter": () => {
      const onSendNow = vi.fn();
      const { textarea } = renderComposer({ running: true, onSendNow });
      typed(textarea, "now");
      fireEvent.keyDown(textarea, { key: "Enter", ctrlKey: true });
      expect(onSendNow).toHaveBeenCalledWith("now");
    },
    "Shift+Enter": () => {
      const onQueue = vi.fn();
      const { textarea, onSend } = renderComposer({ onQueue });
      typed(textarea, "line");
      // Left to the textarea's own newline: not claimed, nothing sent.
      expect(fireEvent.keyDown(textarea, { key: "Enter", shiftKey: true })).toBe(true);
      expect(onSend).not.toHaveBeenCalled();
      expect(onQueue).not.toHaveBeenCalled();
    },
    "Alt+Enter / \\ Enter": () => {
      // Claude Code's own two newline keys. Nothing is sent.
      const { textarea, onSend } = renderComposer();
      typed(textarea, "line");
      expect(fireEvent.keyDown(textarea, { key: "Enter", altKey: true })).toBe(false);
      expect(textarea.value).toBe("line\n");
      typed(textarea, "line\ntwo\\");
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(textarea.value).toBe("line\ntwo\n");
      expect(onSend).not.toHaveBeenCalled();
    },
    "↑ / ↓": () => {
      const onTakeBackQueue = vi.fn();
      const { textarea } = renderComposer({ history: ["older prompt"] });
      fireEvent.keyDown(textarea, { key: "ArrowUp" });
      expect(textarea.value).toBe("older prompt");
      fireEvent.keyDown(textarea, { key: "ArrowDown" });
      expect(textarea.value).toBe("");
      cleanup();
      const queued = renderComposer({ queueCount: 2, onTakeBackQueue, running: true });
      fireEvent.keyDown(queued.textarea, { key: "ArrowUp" });
      expect(onTakeBackQueue).toHaveBeenCalledTimes(1);
    },
    "Ctrl+r": () => {
      const { container, textarea } = renderComposer({ history: ["a"] });
      fireEvent.keyDown(textarea, { key: "r", ctrlKey: true });
      expect(container.querySelector(".history-search")).not.toBeNull();
    },
    "/ command": () => {
      // The menu opens on a leading slash given a command source, and not without one.
      const commands = [{ name: "compact", description: "Free up context" }];
      const { container, textarea } = renderComposer({ slashCommands: commands });
      typed(textarea, "/co");
      expect(container.querySelector(".slash-menu")).not.toBeNull();
      cleanup();
      const bare = renderComposer();
      typed(bare.textarea, "/co");
      expect(bare.container.querySelector(".slash-menu")).toBeNull();
    },
    "Ctrl+n / Ctrl+p": () => {
      const commands = [{ name: "compact", description: "" }, { name: "cost", description: "" }];
      const { container, textarea } = renderComposer({ slashCommands: commands });
      typed(textarea, "/co");
      fireEvent.keyDown(textarea, { key: "n", ctrlKey: true });
      expect(container.querySelector(".slash-menu-row.current")!.textContent).toContain("/cost");
      fireEvent.keyDown(textarea, { key: "p", ctrlKey: true });
      expect(container.querySelector(".slash-menu-row.current")!.textContent).toContain("/compact");
    },
    Tab: () => {
      const { textarea } = renderComposer({ slashCommands: [{ name: "compact", description: "" }] });
      typed(textarea, "/co");
      fireEvent.keyDown(textarea, { key: "Tab" });
      expect(textarea.value).toBe("/compact ");
    },
    "Ctrl+w / Ctrl+u": () => {
      const { textarea } = renderComposer();
      typed(textarea, "git log");
      fireEvent.keyDown(textarea, { key: "w", ctrlKey: true });
      expect(textarea.value).toBe("git ");
      textarea.setSelectionRange(4, 4);
      fireEvent.keyDown(textarea, { key: "u", ctrlKey: true });
      expect(textarea.value).toBe("");
    },
    "Ctrl+a / Ctrl+e": () => {
      const { textarea } = renderComposer();
      typed(textarea, "one\ntwo");
      textarea.setSelectionRange(5, 5);
      fireEvent.keyDown(textarea, { key: "e", ctrlKey: true });
      expect(textarea.selectionStart).toBe(7);
      fireEvent.keyDown(textarea, { key: "a", ctrlKey: true });
      expect(textarea.selectionStart).toBe(4);
    },
    "Ctrl+c": () => {
      const onInterrupt = vi.fn();
      const running = renderComposer({ running: true, onInterrupt });
      fireEvent.keyDown(running.textarea, { key: "c", ctrlKey: true });
      expect(onInterrupt).toHaveBeenCalledTimes(1);
      cleanup();
      const onHistoryPush = vi.fn();
      const idle = renderComposer({ onHistoryPush });
      typed(idle.textarea, "keep me");
      fireEvent.keyDown(idle.textarea, { key: "c", ctrlKey: true });
      expect(onHistoryPush).toHaveBeenCalledWith("keep me");
      expect(idle.textarea.value).toBe("");
    },
    "Ctrl+g": () => {
      const onEditInNvim = vi.fn();
      const { textarea } = renderComposer({ onEditInNvim });
      typed(textarea, "draft");
      fireEvent.keyDown(textarea, { key: "g", ctrlKey: true });
      expect(onEditInNvim).toHaveBeenCalledWith("draft");
    },
    "Ctrl+o": () => {
      // `Composer` must leave it to bubble, and `App`'s INPUT branch must act on it.
      const { textarea } = renderComposer();
      expect(fireEvent.keyDown(textarea, { key: "o", ctrlKey: true })).toBe(true);
      expect(resolveKey("input", keyLike("o", { ctrlKey: true }), { sessionEnded: false })).toEqual({
        kind: "detailed",
      });
    },
    "Shift+Tab": () => {
      // `Composer` must leave it to bubble -- App.tsx's document-capture router
      // (`modeKey.ts`) now claims it ahead of every React handler, in every mode, not just here
      // (see App.test.tsx's "Shift+Tab anywhere in the chat" describe block).
      const { textarea } = renderComposer();
      expect(fireEvent.keyDown(textarea, { key: "Tab", shiftKey: true })).toBe(true);
    },
    Esc: () => {
      const { textarea } = renderComposer();
      expect(fireEvent.keyDown(textarea, { key: "Escape" })).toBe(true);
      expect(resolveKey("input", keyLike("Escape"), { sessionEnded: false })).toEqual({ kind: "mode", to: "browse" });
    },
    "?": () => {
      const onOpenKeymap = vi.fn();
      const { textarea } = renderComposer({ onOpenKeymap });
      fireEvent.keyDown(textarea, { key: "?", shiftKey: true });
      expect(onOpenKeymap).toHaveBeenCalledTimes(1);
    },
    "Ctrl+y": () => {
      // `Composer` must leave it to bubble (its text untouched), and `App`'s
      // INPUT branch approves the oldest waiting card on it (`App.inputCtrlY.test.tsx`).
      const { textarea } = renderComposer();
      typed(textarea, "keep");
      expect(fireEvent.keyDown(textarea, { key: "y", ctrlKey: true })).toBe(true);
      expect(textarea.value).toBe("keep");
      expect(resolveKey("input", keyLike("y", { ctrlKey: true }), { sessionEnded: false })).toEqual({
        kind: "approve-oldest",
      });
    },
  };

  it("covers exactly COMPOSER_CHORDS", () => {
    expect(new Set(Object.keys(behaviours))).toEqual(new Set(COMPOSER_CHORDS));
  });

  for (const chord of COMPOSER_CHORDS) {
    it(`${chord} does something`, () => {
      const behaviour = behaviours[chord];
      expect(behaviour, `no behaviour test for ${chord}`).toBeDefined();
      behaviour();
    });
  }
});

function keyLike(key: string, over: Partial<{ ctrlKey: boolean; shiftKey: boolean }> = {}) {
  return { key, ctrlKey: false, shiftKey: false, isComposing: false, ...over };
}

/* `Alt+Enter` and a trailing backslash then Enter send
   the half-written draft today; Claude Code documents both as a newline, so both insert one (ruled a
   bug fix). Plain Enter still sends, Shift+Enter is the textarea's own newline, Ctrl+Enter still sends
   now. */
describe("Alt+Enter and backslash+Enter insert a newline (#27, K14)", () => {
  const place = (textarea: HTMLTextAreaElement, value: string, caret: number = value.length) => {
    fireEvent.change(textarea, { target: { value } });
    textarea.setSelectionRange(caret, caret);
  };

  it("Alt+Enter inserts a newline at the caret and sends nothing", () => {
    const { textarea, onSend } = renderComposer();
    place(textarea, "abcd", 2);
    const notClaimed = fireEvent.keyDown(textarea, { key: "Enter", altKey: true });
    expect(notClaimed, "the key is claimed, so the browser adds nothing of its own").toBe(false);
    expect(textarea.value).toBe("ab\ncd");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("Alt+Enter replaces a selection with the newline", () => {
    const { textarea, onSend } = renderComposer();
    place(textarea, "abcd", 1);
    textarea.setSelectionRange(1, 3);
    fireEvent.keyDown(textarea, { key: "Enter", altKey: true });
    expect(textarea.value).toBe("a\nd");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("Alt+Enter does not queue or send-now while a turn runs either", () => {
    const onQueue = vi.fn();
    const onSendNow = vi.fn();
    const { textarea, onSend } = renderComposer({ running: true, onQueue, onSendNow });
    place(textarea, "follow up");
    fireEvent.keyDown(textarea, { key: "Enter", altKey: true });
    expect(textarea.value).toBe("follow up\n");
    expect([onSend, onQueue, onSendNow].map((f) => f.mock.calls.length)).toEqual([0, 0, 0]);
  });

  it("a backslash right before the caret, then Enter: the backslash goes, a newline takes its place", () => {
    const { textarea, onSend } = renderComposer();
    place(textarea, "first line\\");
    const notClaimed = fireEvent.keyDown(textarea, { key: "Enter" });
    expect(notClaimed).toBe(false);
    expect(textarea.value).toBe("first line\n");
    expect(onSend).not.toHaveBeenCalled();
    // ...with the caret after the newline, so the next line is typed where it belongs.
    expect(textarea.selectionStart).toBe("first line\n".length);
  });

  it("the same in the middle of the text: only the backslash before the caret is taken", () => {
    const { textarea, onSend } = renderComposer();
    place(textarea, "ab\\cd", 3);
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(textarea.value).toBe("ab\ncd");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("plain Enter still sends: no backslash before the caret, or one further back", () => {
    const { textarea, onSend } = renderComposer();
    place(textarea, "hello");
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenLastCalledWith("hello");
    place(textarea, "a\\b");
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenLastCalledWith("a\\b");
  });

  it("Ctrl+Enter after a backslash still sends now, and Shift+Enter stays the textarea's own newline", () => {
    const onSendNow = vi.fn();
    const { textarea, onSend } = renderComposer({ running: true, onSendNow });
    place(textarea, "now\\");
    fireEvent.keyDown(textarea, { key: "Enter", ctrlKey: true });
    expect(onSendNow).toHaveBeenCalledWith("now\\");
    place(textarea, "x\\");
    expect(fireEvent.keyDown(textarea, { key: "Enter", shiftKey: true })).toBe(true);
    expect(textarea.value).toBe("x\\");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("a backslash before a selection is not 'before the caret': Enter sends", () => {
    const { textarea, onSend } = renderComposer();
    place(textarea, "a\\bc", 2);
    textarea.setSelectionRange(2, 3);
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("a\\bc");
  });

  // `readOnly` stops typing but not a keydown handler, so both newline
  // keys edited a draft nvim owned for the length of a scratch round trip -- text nvim's own write would
  // then replace, losing the newline.
  it("while nvim owns the draft (editingInNvim), neither newline key touches it and nothing is sent", () => {
    const { textarea, onSend } = renderComposer({ editingInNvim: true });
    place(textarea, "abcd", 2);
    fireEvent.keyDown(textarea, { key: "Enter", altKey: true });
    expect(textarea.value).toBe("abcd");
    place(textarea, "x\\");
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(textarea.value).toBe("x\\");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("an input method's own Alt+Enter or Enter is left alone", () => {
    const { textarea, onSend } = renderComposer();
    place(textarea, "x\\");
    fireEvent.keyDown(textarea, { key: "Enter", isComposing: true });
    fireEvent.keyDown(textarea, { key: "Enter", altKey: true, keyCode: 229 });
    expect(textarea.value).toBe("x\\");
    expect(onSend).not.toHaveBeenCalled();
  });
});

/** The box's placeholder is empty while it holds the keys, and a control named only by its placeholder has no
 *  name then: a screen reader announced just "edit text". The name is fixed, whatever the placeholder says. */
describe("Composer: the accessible name", () => {
  it("is the same fixed name whether the placeholder is empty, a hint or a notice", () => {
    for (const overrides of [{}, { idleHint: "Ctrl+l to start typing" }, { sessionEnded: true }, { running: true }] as Overrides[]) {
      const { textarea } = renderComposer(overrides);
      expect(textarea.getAttribute("aria-label"), JSON.stringify(overrides)).toBe("Message the agent");
      cleanup();
    }
    const { container } = renderComposer();
    expect(container.querySelector("textarea")!.hasAttribute("placeholder")).toBe(false);
    expect(getByRole(container, "textbox", { name: "Message the agent" })).toBe(container.querySelector("textarea"));
  });
});

describe("Composer local slash actions", () => {
  it("a_mapped_word_runs_and_clears_the_draft", () => {
    for (const [word, action] of [["help", { kind: "keymap" }], ["status", { kind: "tab-info" }], ["resume", { kind: "sessions" }], ["memory", { kind: "open-path", path: "CLAUDE.md" }]] as const) {
      const onSlashAction = vi.fn();
      const onQueue = vi.fn();
      const onSendNow = vi.fn();
      const onHistoryPush = vi.fn();
      const { textarea, onSend } = renderComposer({ onSlashAction, running: true, onQueue, onSendNow, onHistoryPush });
      fireEvent.change(textarea, { target: { value: `/${word}` } });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSlashAction).toHaveBeenCalledWith(action);
      expect(onSend).not.toHaveBeenCalled();
      expect(onQueue).not.toHaveBeenCalled();
      expect(onSendNow).not.toHaveBeenCalled();
      expect(onHistoryPush).not.toHaveBeenCalled();
      expect(textarea.value).toBe("");
      cleanup();
    }
  });

  it("rewind_sends_nothing_and_keeps_the_draft", () => {
    const onSlashAction = vi.fn();
    const { textarea, container, onSend } = renderComposer({ onSlashAction });
    fireEvent.change(textarea, { target: { value: "/rewind" } });
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).not.toHaveBeenCalled();
    expect(textarea.value).toBe("/rewind");
    expect(onSlashAction).toHaveBeenCalledWith({ kind: "pointer", text: "/rewind is not in Eitri — c reviews and reverts this turn's edits" });
    // The band shows the pointer; the composer's own flash stays empty so the text appears once.
    expect(container.querySelector(".composer-slash-flash")).toBeNull();
  });

  it("on_another_agents_tab_the_words_are_sent_as_text", () => {
    for (const word of ["help", "status", "resume", "memory", "rewind"]) {
      const onSlashAction = vi.fn();
      const { textarea, onSend } = renderComposer({ claudeSlashCommands: false, onSlashAction });
      fireEvent.change(textarea, { target: { value: `/${word}` } });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSend).toHaveBeenCalledWith(`/${word}`);
      expect(onSlashAction).not.toHaveBeenCalled();
      cleanup();
    }
  });
});
