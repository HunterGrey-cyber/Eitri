// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { Composer } from "./Composer";
import { builtInCompletions } from "../slashCommands";
import type { CompletionCommand } from "../slashCommands";

afterEach(cleanup);

type Overrides = Partial<Parameters<typeof Composer>[0]>;

const COMMANDS: CompletionCommand[] = [
  { name: "compact", description: "Free up context" },
  { name: "cost", description: "Show the cost" },
  { name: "clear", description: "Start fresh" },
  { name: "recompute", description: "Contains co" },
  { name: "model", description: "Switch the model" },
];

function renderComposer(overrides: Overrides = {}) {
  const onSend = vi.fn();
  const rendered = render(
    <Composer
      disabled={false}
      sessionEnded={false}
      closing={false}
      restoredDraft={null}
      mode="input"
      onModeChange={vi.fn()}
      onSend={onSend}
      running={false}
      slashCommands={COMMANDS}
      {...overrides}
    />,
  );
  const textarea = rendered.container.querySelector("textarea")!;
  return { ...rendered, textarea, onSend };
}

/** Types `value` and leaves the caret at `caret` (the end by default), as the box reports it. */
function typed(textarea: HTMLTextAreaElement, value: string, caret = value.length) {
  fireEvent.change(textarea, { target: { value } });
  textarea.setSelectionRange(caret, caret);
  fireEvent.select(textarea);
}

const rows = (c: HTMLElement) => Array.from(c.querySelectorAll(".slash-menu-row"));
const names = (c: HTMLElement) => rows(c).map((r) => r.querySelector(".slash-menu-name")!.textContent);
const menu = (c: HTMLElement) => c.querySelector(".slash-menu");
const current = (c: HTMLElement) => c.querySelector(".slash-menu-row.current .slash-menu-name")?.textContent;

describe("opening and filtering", () => {
  it("opens when the draft is a lone slash, listing every command with its muted description", () => {
    const { container, textarea } = renderComposer();
    expect(menu(container)).toBeNull();
    typed(textarea, "/");
    expect(names(container)).toEqual(["/compact", "/cost", "/clear", "/recompute", "/model"]);
    expect(container.querySelector(".slash-menu-row .slash-menu-desc")!.textContent).toBe("Free up context");
    expect(current(container)).toBe("/compact");
    expect(container.querySelector(".slash-menu")!.getAttribute("role")).toBe("listbox");
  });

  it("filters by prefix first, then by substring, and closes when nothing matches", () => {
    const { container, textarea } = renderComposer();
    typed(textarea, "/co");
    expect(names(container)).toEqual(["/compact", "/cost", "/recompute"]);
    typed(textarea, "/cl");
    expect(names(container)).toEqual(["/clear"]);
    typed(textarea, "/zzz");
    expect(menu(container)).toBeNull();
  });

  it("is not offered for ordinary text, a second line, or a slash after the first word", () => {
    const { container, textarea } = renderComposer();
    typed(textarea, "hello /co");
    expect(menu(container)).toBeNull();
    typed(textarea, "/co\nmore");
    expect(menu(container)).toBeNull();
  });

  it("closes when a space is typed and when the caret leaves the first word, and opens again on return", () => {
    const { container, textarea } = renderComposer();
    typed(textarea, "/compact");
    expect(menu(container)).not.toBeNull();
    typed(textarea, "/compact ");
    expect(menu(container)).toBeNull();
    typed(textarea, "/compact now", 12);
    expect(menu(container)).toBeNull();
    typed(textarea, "/compact now", 4);
    expect(menu(container)).not.toBeNull();
    typed(textarea, "/compact now", 12);
    expect(menu(container)).toBeNull();
  });

  it("closes on a send and on leaving INPUT", () => {
    const { container, textarea, onSend, rerender } = renderComposer();
    typed(textarea, "/compact");
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("/compact");
    expect(menu(container)).toBeNull();
    typed(textarea, "/co");
    expect(menu(container)).not.toBeNull();
    rerender(
      <Composer
        disabled={false}
        sessionEnded={false}
        closing={false}
        restoredDraft={null}
        mode="browse"
        onModeChange={vi.fn()}
        onSend={onSend}
        running={false}
        slashCommands={COMMANDS}
      />,
    );
    expect(menu(container)).toBeNull();
  });

  it("is never drawn without a command source, for a dead session, a disabled box, the scratch editor or the history search", () => {
    for (const overrides of [{ slashCommands: null }, { disabled: true }, { editingInNvim: true }] as Overrides[]) {
      const { container, textarea, unmount } = renderComposer(overrides);
      typed(textarea, "/");
      expect(menu(container), JSON.stringify(overrides)).toBeNull();
      unmount();
    }
    const { container, textarea } = renderComposer({ history: ["old"] });
    typed(textarea, "/co");
    expect(menu(container)).not.toBeNull();
    fireEvent.keyDown(textarea, { key: "r", ctrlKey: true });
    expect(container.querySelector(".history-search")).not.toBeNull();
    expect(menu(container)).toBeNull();
  });
});

describe("at most eight rows, scrolling with the selection", () => {
  const many = Array.from({ length: 12 }, (_, i) => ({ name: `cmd${String(i).padStart(2, "0")}`, description: "" }));

  it("draws eight, and the window follows the selection down and back up", () => {
    const { container, textarea } = renderComposer({ slashCommands: many });
    typed(textarea, "/");
    expect(rows(container)).toHaveLength(8);
    expect(names(container)[0]).toBe("/cmd00");
    for (let i = 0; i < 8; i++) fireEvent.keyDown(textarea, { key: "ArrowDown" });
    expect(current(container)).toBe("/cmd08");
    expect(names(container)[0]).toBe("/cmd01");
    expect(rows(container)).toHaveLength(8);
    fireEvent.keyDown(textarea, { key: "ArrowUp" });
    expect(current(container)).toBe("/cmd07");
    expect(names(container)[0]).toBe("/cmd01");
    for (let i = 0; i < 7; i++) fireEvent.keyDown(textarea, { key: "ArrowUp" });
    expect(current(container)).toBe("/cmd00");
    expect(names(container)[0]).toBe("/cmd00");
  });

  it("wraps past either end, window and all", () => {
    const { container, textarea } = renderComposer({ slashCommands: many });
    typed(textarea, "/");
    fireEvent.keyDown(textarea, { key: "ArrowUp" });
    expect(current(container)).toBe("/cmd11");
    expect(names(container)[0]).toBe("/cmd04");
    fireEvent.keyDown(textarea, { key: "ArrowDown" });
    expect(current(container)).toBe("/cmd00");
    expect(names(container)[0]).toBe("/cmd00");
  });

  it("moves the selection with the wheel", () => {
    const { container, textarea } = renderComposer({ slashCommands: many });
    typed(textarea, "/");
    fireEvent.wheel(menu(container)!, { deltaY: 100 });
    expect(current(container)).toBe("/cmd01");
    fireEvent.wheel(menu(container)!, { deltaY: -100 });
    fireEvent.wheel(menu(container)!, { deltaY: -100 });
    expect(current(container)).toBe("/cmd00");
  });
});

describe("keys while the menu is open", () => {
  it("↓ ↑ Ctrl+n Ctrl+p move the selection and are claimed", () => {
    const { container, textarea } = renderComposer();
    typed(textarea, "/");
    expect(fireEvent.keyDown(textarea, { key: "ArrowDown" })).toBe(false);
    expect(current(container)).toBe("/cost");
    fireEvent.keyDown(textarea, { key: "n", ctrlKey: true });
    expect(current(container)).toBe("/clear");
    fireEvent.keyDown(textarea, { key: "p", ctrlKey: true });
    expect(current(container)).toBe("/cost");
    fireEvent.keyDown(textarea, { key: "ArrowUp" });
    expect(current(container)).toBe("/compact");
  });

  it("restarts the selection at the top when the filter changes", () => {
    const { container, textarea } = renderComposer();
    typed(textarea, "/");
    fireEvent.keyDown(textarea, { key: "ArrowDown" });
    fireEvent.keyDown(textarea, { key: "ArrowDown" });
    typed(textarea, "/c");
    expect(current(container)).toBe("/compact");
  });

  it("does not walk prompt history or take the queue back while open, and does when closed", () => {
    const onTakeBackQueue = vi.fn();
    const { container, textarea } = renderComposer({ history: ["an older prompt"], queueCount: 2, onTakeBackQueue, running: true });
    typed(textarea, "/co");
    fireEvent.keyDown(textarea, { key: "ArrowUp" });
    fireEvent.keyDown(textarea, { key: "ArrowUp" });
    expect(onTakeBackQueue).not.toHaveBeenCalled();
    expect(textarea.value).toBe("/co");
    // Closed (a space typed): ↑ is the queue's again.
    typed(textarea, "/co ");
    expect(menu(container)).toBeNull();
    fireEvent.keyDown(textarea, { key: "ArrowUp" });
    expect(onTakeBackQueue).toHaveBeenCalledTimes(1);
    cleanup();
    const idle = renderComposer({ history: ["an older prompt"] });
    typed(idle.textarea, "/co");
    fireEvent.keyDown(idle.textarea, { key: "ArrowUp" });
    expect(idle.textarea.value).toBe("/co");
    typed(idle.textarea, "");
    fireEvent.keyDown(idle.textarea, { key: "ArrowUp" });
    expect(idle.textarea.value).toBe("an older prompt");
  });

  it("a history walk passes over a recalled slash command in both directions, the menu staying shut", () => {
    const { container, textarea } = renderComposer({ history: ["an older prompt", "/compact"] });
    fireEvent.keyDown(textarea, { key: "ArrowUp" });
    expect(textarea.value).toBe("/compact");
    expect(menu(container), "a recalled draft does not open the menu").toBeNull();
    fireEvent.keyDown(textarea, { key: "ArrowUp" });
    expect(textarea.value).toBe("an older prompt");
    fireEvent.keyDown(textarea, { key: "ArrowDown" });
    expect(textarea.value).toBe("/compact");
    expect(menu(container)).toBeNull();
    fireEvent.keyDown(textarea, { key: "ArrowDown" });
    expect(textarea.value).toBe("");
  });

  it("typing into a recalled slash command opens the menu from then on", () => {
    const { container, textarea } = renderComposer({ history: ["/compact"] });
    fireEvent.keyDown(textarea, { key: "ArrowUp" });
    expect(menu(container)).toBeNull();
    typed(textarea, "/comp");
    expect(names(container)).toEqual(["/compact", "/recompute"]);
  });

  it("a slash command taken back from the queue, or restored after a refused send, leaves the menu shut", () => {
    const first = renderComposer({ queueTaken: null });
    first.rerender(
      <Composer
        disabled={false}
        sessionEnded={false}
        closing={false}
        restoredDraft={null}
        mode="input"
        onModeChange={vi.fn()}
        onSend={vi.fn()}
        running={false}
        slashCommands={COMMANDS}
        queueTaken={{ texts: ["/compact"], seq: 1 }}
      />,
    );
    expect(first.textarea.value).toBe("/compact");
    expect(menu(first.container)).toBeNull();
    cleanup();
    const second = renderComposer({ restoredDraft: { text: "/cost", seq: 1 } });
    expect(second.textarea.value).toBe("/cost");
    expect(menu(second.container)).toBeNull();
  });

  it("Tab completes the selected name and a space, with the caret after it, and sends nothing", () => {
    const { container, textarea, onSend } = renderComposer();
    typed(textarea, "/co");
    fireEvent.keyDown(textarea, { key: "ArrowDown" });
    expect(fireEvent.keyDown(textarea, { key: "Tab" })).toBe(false);
    expect(textarea.value).toBe("/cost ");
    expect(onSend).not.toHaveBeenCalled();
    expect(menu(container)).toBeNull();
  });

  it("Tab completes inside the first word of a longer draft, reusing the space already there", async () => {
    const { textarea } = renderComposer();
    typed(textarea, "/co now", 3);
    fireEvent.keyDown(textarea, { key: "Tab" });
    expect(textarea.value).toBe("/compact now");
    await new Promise((resolve) => requestAnimationFrame(resolve));
    expect(onCaret(textarea)).toBe("/compact ".length);
  });

  it("Enter completes a name that is not yet the whole command, without sending", () => {
    const { container, textarea, onSend } = renderComposer();
    typed(textarea, "/co");
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(textarea.value).toBe("/compact ");
    expect(onSend).not.toHaveBeenCalled();
    expect(menu(container)).toBeNull();
    // The next Enter is an ordinary send.
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("/compact ");
  });

  it("Enter sends a name that only contains the typed letters, instead of completing another command", () => {
    const { textarea, onSend } = renderComposer({
      slashCommands: [{ name: "review", description: "" }, { name: "security-review", description: "" }],
    });
    // `/view` lists `review` (it contains the letters) but is not where it starts: a user's own `/view` skill goes out as typed.
    typed(textarea, "/view");
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("/view");
  });

  it("Tab and the arrows still take a row Enter would not", () => {
    const { textarea, onSend } = renderComposer({ slashCommands: [{ name: "review", description: "" }] });
    typed(textarea, "/view");
    fireEvent.keyDown(textarea, { key: "Tab" });
    expect(textarea.value).toBe("/review ");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("Enter completes from the start of a word within a name, ignoring the separators", () => {
    const { textarea, onSend } = renderComposer({
      slashCommands: [{ name: "security-review", description: "" }, { name: "add-dir", description: "" }],
    });
    typed(textarea, "/rev");
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(textarea.value).toBe("/security-review ");
    typed(textarea, "/adddir");
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(textarea.value).toBe("/add-dir ");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("a bare slash then Enter sends the slash as typed", () => {
    const { textarea, onSend } = renderComposer();
    typed(textarea, "/");
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("/");
  });

  it("Enter sends when the draft is already exactly the selected command", () => {
    const { textarea, onSend } = renderComposer();
    typed(textarea, "/clear");
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("/clear");
  });

  it("an equal name is selected ahead of longer ones, so Enter sends what was typed in full", () => {
    const { container, textarea, onSend } = renderComposer({
      slashCommands: [{ name: "review-all", description: "" }, { name: "review", description: "" }],
    });
    typed(textarea, "/review");
    expect(current(container)).toBe("/review");
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("/review");
  });

  it("Ctrl+Enter keeps its meaning, sending now, with the menu open", () => {
    const onSendNow = vi.fn();
    const { textarea, onSend } = renderComposer({ running: true, onSendNow });
    typed(textarea, "/co");
    fireEvent.keyDown(textarea, { key: "Enter", ctrlKey: true });
    expect(onSendNow).toHaveBeenCalledWith("/co");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("Shift+Enter and Alt+Enter stay newlines and complete nothing", () => {
    const { textarea } = renderComposer();
    typed(textarea, "/co");
    expect(fireEvent.keyDown(textarea, { key: "Enter", shiftKey: true })).toBe(true);
    expect(textarea.value).toBe("/co");
    fireEvent.keyDown(textarea, { key: "Enter", altKey: true });
    expect(textarea.value).toBe("/co\n");
  });

  it("Esc closes the menu, keeps the text, and does not reach the panel; a second Esc does", () => {
    const outer = vi.fn();
    const onLineKey = vi.fn();
    // React's own bubbling: the wrapper's handler stands for the panel's root handler, which turns Esc
    // into "back to browsing".
    const { container } = render(
      <div onKeyDown={outer}>
        <Composer
          disabled={false}
          sessionEnded={false}
          closing={false}
          restoredDraft={null}
          mode="input"
          onModeChange={vi.fn()}
          onSend={vi.fn()}
          running={false}
          slashCommands={COMMANDS}
          onLineKey={onLineKey}
        />
      </div>,
    );
    const textarea = container.querySelector("textarea")!;
    typed(textarea, "/co");
    expect(menu(container)).not.toBeNull();
    fireEvent.keyDown(textarea, { key: "Escape" });
    expect(menu(container)).toBeNull();
    expect(textarea.value).toBe("/co");
    expect(outer).not.toHaveBeenCalled();
    expect(onLineKey, "the typing guard is told of the key").toHaveBeenCalledTimes(1);
    fireEvent.keyDown(textarea, { key: "Escape" });
    expect(outer).toHaveBeenCalledTimes(1);
    // Typing again brings it back.
    typed(textarea, "/com");
    expect(menu(container)).not.toBeNull();
  });

  it("a click on a row completes it, and the press does not take focus from the box", () => {
    const { container, textarea } = renderComposer();
    typed(textarea, "/");
    const down = fireEvent.mouseDown(menu(container)!);
    expect(down, "mousedown is claimed so the box keeps focus").toBe(false);
    fireEvent.click(rows(container)[4]);
    expect(textarea.value).toBe("/model ");
    expect(menu(container)).toBeNull();
  });
});

describe("an input method's keys", () => {
  it("never complete, move or close anything while composing", () => {
    const { container, textarea, onSend } = renderComposer();
    typed(textarea, "/co");
    for (const key of ["Enter", "Tab", "ArrowDown", "ArrowUp", "Escape"]) {
      fireEvent.keyDown(textarea, { key, isComposing: true });
      fireEvent.keyDown(textarea, { key, keyCode: 229 });
    }
    expect(textarea.value).toBe("/co");
    expect(current(container)).toBe("/compact");
    expect(menu(container)).not.toBeNull();
    expect(onSend).not.toHaveBeenCalled();
  });
});

describe("an agent's text", () => {
  it("is drawn as plain text, never as markup", () => {
    const hostile: CompletionCommand[] = [
      { name: "<img src=x onerror=alert(1)>", description: "<b>bold</b> **x** [l](http://evil)" },
      { name: "web", description: "&lt;script&gt;", hint: "<i>q</i>" },
    ];
    const { container, textarea } = renderComposer({ slashCommands: hostile });
    typed(textarea, "/");
    const menuEl = menu(container)!;
    expect(menuEl.querySelector("img, b, i, a, script")).toBeNull();
    expect(menuEl.textContent).toContain("<img src=x onerror=alert(1)>");
    expect(menuEl.textContent).toContain("<b>bold</b> **x** [l](http://evil)");
    expect(menuEl.textContent).toContain("&lt;script&gt;");
    expect(menuEl.textContent).toContain("<i>q</i>");
  });

  it("offers Claude's built-ins when handed them, descriptions and all", () => {
    const { container, textarea } = renderComposer({ slashCommands: builtInCompletions() });
    typed(textarea, "/com");
    expect(names(container)).toEqual(["/compact [instructions]"]);
    expect(container.querySelector(".slash-menu-hint")!.textContent).toBe(" [instructions]");
    expect(container.querySelector(".slash-menu-desc")!.textContent).toBe("Free up context by summarizing the conversation so far");
  });

  it("draws a hint in its own span and a description with invisible characters as visible escapes", () => {
    const { container, textarea } = renderComposer({
      slashCommands: [{ name: "x", description: "a\u202Eb\u200Bc", hint: "[q\u2066]" }],
    });
    typed(textarea, "/");
    const row = container.querySelector(".slash-menu-row")!;
    expect(row.querySelector(".slash-menu-name > .slash-menu-hint")!.textContent).toBe(" [q⟨U+2066⟩]");
    expect(row.querySelector(".slash-menu-desc")!.textContent).toBe("a⟨U+202E⟩b⟨U+200B⟩c");
    expect(row.querySelectorAll(".slash-menu-desc .permission-card-escape")).toHaveLength(2);
  });
});

/** Where the caret is, as the box would report it (the rAF that places it after a completion runs later). */
function onCaret(textarea: HTMLTextAreaElement): number {
  return textarea.selectionStart;
}
