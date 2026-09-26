// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
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

/** The bare shell-prompt glyph replaces the bordered box (panel round 2 plan, Task 10; spec §5.1). */
describe("Composer's prompt glyph", () => {
  /** `mock: bottom.html` B draws `❯ Ask the agent…` in BROWSE too; until the r2-gui GUI pass
   *  (2026-09-26) BROWSE kept the old bordered box with no glyph, so the bottom changed shape on
   *  every `i` and `Esc`. */
  it("draws ❯ right before the textarea in INPUT, and before the stand-in in BROWSE", () => {
    const { container } = renderComposer();
    const prompt = container.querySelector(".composer-prompt")!;
    expect(prompt.textContent).toBe("❯");
    expect(prompt.compareDocumentPosition(container.querySelector("textarea")!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    cleanup();
    const browse = renderComposer({ mode: "browse" });
    const glyph = browse.container.querySelector(".composer-prompt")!;
    expect(glyph.textContent).toBe("❯");
    expect(glyph.compareDocumentPosition(browse.container.querySelector(".composer-browse-hint")!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });
  it("draws no glyph for a session that has ended: there is no prompt to type at", () => {
    const { container } = renderComposer({ mode: "browse", sessionEnded: true });
    expect(container.querySelector(".composer-prompt")).toBeNull();
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
// above), and Stop now lives only in `ActivityLine` (V2, session tabs Task 10; formerly
// `StatusLine`), for mouse users -- see `ActivityLine.test.tsx`'s own "shows the turn's motion and
// Stop..." test for that half, gated the same way this one used to be, on the provider's
// advertised `interrupt` capability rather than the backend's name.

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
    expect(container.querySelector(".composer-browse-hint")!.textContent).toBe("Ask the agent...");
  });

  it("hands Shift+Tab to the host when asked to (D6)", () => {
    const onShiftTab = vi.fn();
    const { textarea } = renderComposer({ onShiftTab });
    fireEvent.keyDown(textarea, { key: "Tab", shiftKey: true });
    expect(onShiftTab).toHaveBeenCalledTimes(1);
  });
});

describe("Composer during a turn (C1, F1)", () => {
  it("stays typable while a turn runs and queues on Enter", () => {
    const onQueue = vi.fn();
    const { textarea, onSend } = renderComposer({ running: true, onQueue });
    expect(textarea.disabled).toBe(false);
    expect(textarea.placeholder).toBe("Queue a follow-up…");
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

  /** Review focus 3: fcitx5's Enter commits the raw letters; it must not send or queue. */
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

describe("Composer keys (C6, D1, ?)", () => {
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

  /** Defect 1 (phase 1's sandbox pass): a landing put the caret before the existing text. A box the
   *  user never placed the caret in takes it at the end; one they left mid-text gets it back (C2). */
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

/* Review finding (phase 3): `composerKeys.test.ts` tied `INPUT_KEYS` only to `COMPOSER_CHORDS`, a
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
    "Ctrl+w / Ctrl+u": () => {
      const { textarea } = renderComposer();
      typed(textarea, "git log");
      fireEvent.keyDown(textarea, { key: "w", ctrlKey: true });
      expect(textarea.value).toBe("git ");
      textarea.setSelectionRange(4, 4);
      fireEvent.keyDown(textarea, { key: "u", ctrlKey: true });
      expect(textarea.value).toBe("");
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
      const onShiftTab = vi.fn();
      const { textarea } = renderComposer({ onShiftTab });
      fireEvent.keyDown(textarea, { key: "Tab", shiftKey: true });
      expect(onShiftTab).toHaveBeenCalledTimes(1);
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
