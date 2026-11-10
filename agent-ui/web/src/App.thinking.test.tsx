// @vitest-environment jsdom
/**
 * The agent's thinking in the conversation, at each `agent.thinking` level (the `display` envelope):
 * `collapsed` (one muted row per block, its text behind the row's fold), `preview` (a block's last lines
 * in a box of fixed height while it is written), `expanded` (every block whole) and `hidden` (no rows);
 * the detailed view (`Ctrl+o`) shows every block whole whatever the level. A thinking row is a cursor
 * stop that `Enter`/`zo`/`zc` fold and `y` copies, and its text is never read as markdown or HTML.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentDomainEvent, AgentUiState, Hello, ThinkingDisplay } from "./types";
import { lastThoughtLines, MessageList, timelineRowRenderCount } from "./components/MessageList";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

let clipboard: { writeText: ReturnType<typeof vi.fn> };

beforeEach(() => {
  (window as unknown as { webkit: unknown }).webkit = { messageHandlers: { eitriAgent: { postMessage: () => {} } } };
  vi.useFakeTimers();
  clipboard = { writeText: vi.fn() };
  Object.defineProperty(navigator, "clipboard", { value: clipboard, configurable: true });
});
afterEach(() => {
  vi.useRealTimers();
  delete (navigator as unknown as Record<string, unknown>).clipboard;
});

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["auto", "bypass"],
  resumableSessions: [],
  account: null,
};
const TAB1 = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));

const THOUGHT = "The parser loops forever.\nThe bound is off by one.\nFix the bound.";
const LONG_THOUGHT = ["one", "two", "three", "four", "five", "six"].join("\n");

/** A turn that thought, read a file, thought again and is still thinking: two finished blocks and an
 *  open one, the open one last. */
function thinkingState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return {
    ...initialState(),
    status: { kind: "running" },
    activeTurnId: "t1",
    userPrompts: [{ seq: 0, text: "fix the parser" }],
    thinking: [
      { seq: 1, turnId: "t1", text: THOUGHT },
      { seq: 4, turnId: "t1", text: "One line." },
      { seq: 5, turnId: "t1", text: LONG_THOUGHT },
    ],
    thinkingOpen: true,
    transcript: [{ seq: 2, text: "Looking at it." }],
    toolCalls: [{ seq: 3, toolUseId: "a", name: "Read", input: { file_path: "a.rs" }, result: { content: "ok", isError: false }, turnId: "t1" }],
    ...overrides,
  };
}

/** Tab 1 live with `state`, at `level` (no `display` envelope at all when `null`), the keys in the chat. */
function started(state: AgentUiState, level: ThinkingDisplay | null, throughRevision = 10) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  if (level !== null) dispatch({ kind: "display", thinking: level });
  dispatch({ kind: "tabs", active: 1, tabs: [TAB1] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision, state });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  wait(TYPING_GUARD_MS + 50);
  const root = rendered.container.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const key = (k: string, extra: Record<string, unknown> = {}) => {
    fireEvent.keyDown(root, { key: k, ...extra });
  };
  return { ...rendered, root, key };
}

const rows = (c: HTMLElement) => Array.from(c.querySelectorAll<HTMLElement>(".row-thinking"));
const heads = (c: HTMLElement) => rows(c).map((r) => r.querySelector(".thinking-head")!.textContent);
const wholeTexts = (c: HTMLElement) => rows(c).map((r) => r.querySelector(".thinking-text")?.textContent ?? null);
const current = (c: HTMLElement) => c.querySelector<HTMLElement>(".row-current");

describe("thinking in the conversation", () => {
  it("collapsed by default: one muted row per block, Thinking… while written, Thought and its lines after", () => {
    const { container } = started(thinkingState(), null);
    expect(heads(container)).toEqual(["Thought · 3 lines ▸", "Thought · 1 line ▸", "Thinking… ▸"]);
    expect(wholeTexts(container)).toEqual([null, null, null]);
    expect(container.querySelector(".thinking-preview")).toBeNull();
    // Each is a row on the sign column, ordered among the others by its seq.
    const kinds = Array.from(container.querySelectorAll(".message-list > .row")).map((r) => r.classList[1]);
    expect(kinds).toEqual(["row-prompt", "row-thinking", "row-assistant", "row-tool", "row-thinking", "row-thinking"]);
    expect(rows(container).every((r) => r.dataset.navStop === "row")).toBe(true);
  });

  it("Enter opens and closes a thought's fold, and zo / zc set it, each idempotent", () => {
    const { container, key } = started(thinkingState({ thinkingOpen: false }), "collapsed");
    // The cursor lands on the last row: the six-line thought.
    expect(current(container)!.classList.contains("row-thinking")).toBe(true);
    const shown = () => current(container)!.querySelector(".thinking-text")?.textContent ?? null;
    expect(shown()).toBeNull();

    key("Enter");
    expect(shown()).toBe(LONG_THOUGHT);
    expect(current(container)!.querySelector(".fold-marker")!.textContent).toBe("▾");
    key("Enter");
    expect(shown()).toBeNull();

    const z = (second: string) => {
      key("z");
      key(second);
    };
    z("o");
    expect(shown(), "zo opens it").toBe(LONG_THOUGHT);
    z("o");
    expect(shown(), "zo again leaves it open").toBe(LONG_THOUGHT);
    z("c");
    expect(shown(), "zc closes it").toBeNull();
    z("c");
    expect(shown(), "zc again leaves it closed").toBeNull();
  });

  it("y copies the thought's own text, folded or not", () => {
    const { container, key } = started(thinkingState({ thinkingOpen: false }), "collapsed");
    key("g");
    key("g");
    key("j");
    expect(current(container)!.classList.contains("row-thinking")).toBe(true);
    key("y");
    expect(clipboard.writeText).toHaveBeenCalledWith(THOUGHT);
  });

  it("preview: the block being written shows its last four lines in a fixed box; a finished one is the collapsed row", () => {
    const { container } = started(thinkingState(), "preview");
    const previews = Array.from(container.querySelectorAll(".thinking-preview")).map((p) => p.textContent);
    expect(previews).toEqual(["three\nfour\nfive\nsix"]);
    expect(heads(container)).toEqual(["Thought · 3 lines ▸", "Thought · 1 line ▸", "Thinking… ▸"]);
    expect(wholeTexts(container)).toEqual([null, null, null]);
  });

  it("preview: once the block is over, its box goes and the collapsed row is left", () => {
    const { container } = started(thinkingState(), "preview");
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 10,
      throughRevision: 11,
      events: [{ type: "content_delta", turn_id: "t1", kind: "text", text: "Fixed." }],
    });
    expect(container.querySelector(".thinking-preview")).toBeNull();
    expect(heads(container)[2]).toBe("Thought · 6 lines ▸");
  });

  it("expanded: every block whole, with nothing to fold", () => {
    const { container } = started(thinkingState(), "expanded");
    expect(wholeTexts(container)).toEqual([THOUGHT, "One line.", LONG_THOUGHT]);
    expect(rows(container).some((r) => r.querySelector(".fold-marker") !== null)).toBe(false);
    expect(heads(container)).toEqual(["Thought · 3 lines", "Thought · 1 line", "Thinking…"]);
  });

  it("hidden: no thinking rows at all, and the cursor walks only the rest", () => {
    const { container, key } = started(thinkingState(), "hidden");
    expect(rows(container)).toEqual([]);
    expect(container.querySelectorAll(".message-list > .row")).toHaveLength(3);
    key("g");
    key("g");
    key("j");
    expect(current(container)!.classList.contains("row-assistant")).toBe(true);
  });

  it("the detailed view shows every block whole, under hidden too, and zi again puts the level back", () => {
    for (const level of ["hidden", "collapsed", "preview"] as const) {
      const { container, key, unmount } = started(thinkingState(), level);
      key("z"); key("i");
      expect(wholeTexts(container), level).toEqual([THOUGHT, "One line.", LONG_THOUGHT]);
      expect(container.querySelector(".thinking-preview"), level).toBeNull();
      key("z"); key("i");
      expect(wholeTexts(container), level).toEqual(level === "hidden" ? [] : [null, null, null]);
      unmount();
    }
  });

  it("never reads a thought as markdown or HTML: it is drawn as the characters that came", () => {
    const hostile = "**bold** _em_ `code`\n<img src=x onerror=\"window.__pwned = 1\"><b>tag</b>\n[link](https://example.com)";
    const { container } = started(
      thinkingState({ thinking: [{ seq: 1, turnId: "t1", text: hostile }], thinkingOpen: false }),
      "expanded",
    );
    const row = rows(container)[0];
    expect(row.querySelector(".thinking-text")!.textContent).toBe(hostile);
    expect(row.querySelectorAll("strong, em, code, img, b, a")).toHaveLength(0);
    expect((window as unknown as { __pwned?: number }).__pwned).toBeUndefined();
  });

  it("a live thought streams into one row and is finished by the reply after it", () => {
    const { container } = started(
      { ...initialState(), status: { kind: "running" }, activeTurnId: "t1", userPrompts: [{ seq: 0, text: "go" }] },
      "expanded",
      1,
    );
    const thought = (text: string): AgentDomainEvent => ({ type: "content_delta", turn_id: "t1", kind: "thinking", text });
    dispatch({ kind: "events", tab: 1, fromRevision: 1, throughRevision: 3, events: [thought("first "), thought("half")] });
    expect(heads(container)).toEqual(["Thinking…"]);
    expect(wholeTexts(container)).toEqual(["first half"]);
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 3,
      throughRevision: 5,
      events: [thought(", second half"), { type: "content_delta", turn_id: "t1", kind: "text", text: "Done." }],
    });
    expect(heads(container)).toEqual(["Thought · 1 line"]);
    expect(wholeTexts(container)).toEqual(["first half, second half"]);
    expect(container.querySelector(".row-assistant")!.textContent).toContain("Done.");
  });

  it("a finished thought between two calls is folded into their run, and opening the run brings it back", () => {
    const done = { content: "ok", isError: false };
    const state: AgentUiState = {
      ...initialState(),
      status: { kind: "running" },
      userPrompts: [{ seq: 0, text: "read both" }],
      toolCalls: [
        { seq: 1, toolUseId: "a", name: "Read", input: { file_path: "a.rs" }, result: done, turnId: "t1" },
        { seq: 3, toolUseId: "b", name: "Read", input: { file_path: "b.rs" }, result: done, turnId: "t1" },
      ],
      thinking: [{ seq: 2, turnId: "t1", text: "a.rs is fine, now b.rs" }],
      transcript: [{ seq: 4, text: "Both are fine." }],
    };
    const { container, key } = started(state, "collapsed");
    expect(container.querySelector(".row-tool-run")!.textContent).toContain("Read 2 files");
    expect(rows(container)).toEqual([]);
    key("g");
    key("g");
    key("j");
    key("Enter");
    expect(container.querySelector(".row-tool-run")).toBeNull();
    expect(heads(container)).toEqual(["Thought · 1 line ▸"]);
    // `zc` on the thought, whose own fold is closed, closes the run it was unfolded from.
    key("j");
    expect(current(container)!.classList.contains("row-thinking")).toBe(true);
    key("z");
    key("c");
    expect(container.querySelector(".row-tool-run")).not.toBeNull();
    expect(current(container)!.classList.contains("row-tool-run")).toBe(true);
    cleanup();

    // Under `expanded` nothing is folded away: the thought splits the two calls.
    const shown = started(state, "expanded");
    expect(shown.container.querySelector(".row-tool-run")).toBeNull();
    expect(wholeTexts(shown.container)).toEqual(["a.rs is fine, now b.rs"]);
    cleanup();

    // Under `hidden` the run folds exactly as if there were no thought.
    const hidden = started(state, "hidden");
    expect(hidden.container.querySelector(".row-tool-run")!.textContent).toContain("Read 2 files");
  });

  it("a finished thought of only blank lines says just Thought, with no count", () => {
    const { container } = started(
      { ...initialState(), thinking: [{ seq: 1, turnId: "t0", text: "  \n\n \n" }, { seq: 2, turnId: "t0", text: "one\n" }] },
      null,
    );
    expect(heads(container)).toEqual(["Thought ▸", "Thought · 1 line ▸"]);
  });

  /* A batch of streamed deltas draws again only the row whose record it changed: rows are memoized on
     their timeline item, and an item stays the same object while its record does. The four hundred
     rows of two hundred replies, each followed by a finished call, are not drawn again. */
  for (const level of ["collapsed", "preview", "expanded", "hidden"] as const) {
    it(`a batch of thinking deltas draws only the open thought's row again (${level})`, () => {
      const N = 200;
      const transcript = Array.from({ length: N }, (_, i) => ({ seq: i * 2 + 1, text: `reply **${i}**` }));
      const toolCalls = Array.from({ length: N }, (_, i) => ({
        seq: i * 2 + 2,
        toolUseId: `tu${i}`,
        name: "Bash",
        input: { command: `echo ${i}` },
        result: { content: "ok", isError: false },
        turnId: "t0",
      }));
      let rev = N * 2 + 2;
      started({ ...initialState(), status: { kind: "running" }, activeTurnId: "t1", transcript, toolCalls }, level, rev);
      const deliver = (events: AgentDomainEvent[]) => {
        dispatch({ kind: "events", tab: 1, fromRevision: rev, throughRevision: rev + events.length, events });
        rev += events.length;
      };
      const delta = (kind: "thinking" | "text"): AgentDomainEvent => ({ type: "content_delta", turn_id: "t1", kind, text: "more " });
      deliver([{ type: "content_delta", turn_id: "t1", kind: "thinking", text: "start " }]);
      const before = timelineRowRenderCount();
      for (let k = 0; k < 10; k++) deliver(Array.from({ length: 20 }, () => delta("thinking")));
      expect((timelineRowRenderCount() - before) / 10).toBe(level === "hidden" ? 0 : 1);

      // A reply streaming after it: the reply's row, and the thought's once, as it closes.
      deliver([delta("text")]);
      const afterReply = timelineRowRenderCount();
      for (let k = 0; k < 10; k++) deliver(Array.from({ length: 20 }, () => delta("text")));
      expect((timelineRowRenderCount() - afterReply) / 10).toBe(1);
    });
  }

  /* The same with folded runs: a run, and the finished thought folded into it, is the same item while
     its calls and thought are, so a thought streamed after a hundred runs draws one row a batch. */
  it("a batch of thinking deltas does not draw the folded runs again", () => {
    const N = 100;
    const transcript = Array.from({ length: N }, (_, i) => ({ seq: i * 4 + 1, text: `reply ${i}` }));
    const done = (seq: number, i: number) => ({ seq, toolUseId: `tu${seq}`, name: "Read", input: { file_path: `/f${i}` }, result: { content: "ok", isError: false }, turnId: "t0" });
    const toolCalls = Array.from({ length: N }, (_, i) => [done(i * 4 + 2, i), done(i * 4 + 4, i)]).flat();
    const thinking = Array.from({ length: N }, (_, i) => ({ seq: i * 4 + 3, turnId: "t0", text: `between ${i}` }));
    let rev = N * 4 + 4;
    const { container } = started(
      { ...initialState(), status: { kind: "running" }, activeTurnId: "t1", transcript, toolCalls, thinking, thinkingOpen: false },
      "collapsed",
      rev,
    );
    const deliver = (events: AgentDomainEvent[]) => {
      dispatch({ kind: "events", tab: 1, fromRevision: rev, throughRevision: rev + events.length, events });
      rev += events.length;
    };
    const delta: AgentDomainEvent = { type: "content_delta", turn_id: "t1", kind: "thinking", text: "more " };
    deliver([{ ...delta, text: "start " }]);
    // The last pair is the running turn's tail (only thinking follows it), so it stays unfolded.
    expect(container.querySelectorAll(".row-tool-run").length).toBe(N - 1);
    const before = timelineRowRenderCount();
    for (let k = 0; k < 10; k++) deliver(Array.from({ length: 20 }, () => delta));
    expect((timelineRowRenderCount() - before) / 10).toBe(1);
    expect(container.querySelectorAll(".row-tool-run").length).toBe(N - 1);
  });

  it("a display envelope naming an unknown level keeps the level the panel has", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { container } = started(thinkingState(), "expanded");
    dispatch({ kind: "display", thinking: "verbose" });
    expect(wholeTexts(container)).toEqual([THOUGHT, "One line.", LONG_THOUGHT]);
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });
});

describe("subagent navigation and folds", () => {
  const delegation = (background = false): AgentUiState => ({
    ...initialState(), status: { kind: "running" }, activeTurnId: "t1",
    transcript: [{ seq: 3, text: "Main reply" }],
    toolCalls: [{ seq: 1, toolUseId: "agent", turnId: "t1", name: "Agent",
      input: { subagent_type: "Explore", description: "inspect code", run_in_background: background }, result: null,
      children: { toolCalls: [{ seq: 2, toolUseId: "read", turnId: "t1", name: "Read", input: { file_path: "file.rs" }, result: null }],
        transcript: [], thinking: [], assistantMessageOpen: false, thinkingOpen: false } }],
  });

  it("Enter toggles children, zo opens, zc closes and j/k visit the indented row", () => {
    const { container, key } = started(delegation(), null);
    key("g"); key("g");
    const child = () => container.querySelector(".subagent-child");
    expect(child()).toBeNull();
    key("Enter");
    expect(child()).not.toBeNull();
    key("Enter");
    expect(child()).toBeNull();
    key("z"); key("o");
    expect(child()).not.toBeNull();
    key("z"); key("o");
    expect(container.querySelectorAll(".subagent-child")).toHaveLength(1);
    key("j");
    expect(current(container)?.classList.contains("row-tool")).toBe(true);
    key("k");
    expect(current(container)?.classList.contains("row-subagent")).toBe(true);
    key("j");
    key("z"); key("c");
    expect(child()).toBeNull();
    expect(current(container)?.classList.contains("row-subagent")).toBe(true);
    key("z"); key("o");
    expect(child()).not.toBeNull();
    key("z"); key("c");
    expect(child()).toBeNull();
  });

  it("Enter, zo and the detailed view never open a background agent in the transcript", () => {
    const { container, key } = started(delegation(true), null);
    key("g"); key("g");
    key("Enter");
    key("z"); key("o");
    key("z"); key("i");
    expect(container.querySelector(".subagent-child")).toBeNull();
    expect(container.querySelector(".row-subagent .fold-marker")).toBeNull();
  });
});

describe("thinking text work", () => {
  it("gets the same preview as splitting, including trailing newlines and CRLF, without splitting", () => {
    for (const text of ["", "\n\n", "\na", "\n\na", "one", "a\nb\nc\nd\ne\n\n", "a\r\nb\r\n\r\n", "\r\n", "a\nb\n", "x\r"]) {
      for (const count of [1, 2, 4, 10]) {
        const expected = text.replace(/\n+$/, "").split("\n").slice(-count).join("\n");
        const split = vi.spyOn(String.prototype, "split");
        const result = lastThoughtLines(text, count);
        const calls = split.mock.calls.length;
        split.mockRestore();
        expect(result).toBe(expected);
        expect(calls).toBe(0);
      }
    }
  });

  it("counts and checks blankness only after closure, and keeps the result across fold changes", () => {
    const text = "a long thought\n".repeat(500);
    const original = String.prototype.trim;
    let trims = 0;
    const trim = vi.spyOn(String.prototype, "trim").mockImplementation(function (this: string) {
      if (String(this) === text) trims++;
      return original.call(this);
    });
    const current = thinkingState({ thinking: [{ seq: 1, turnId: "t1", text }], thinkingOpen: true });
    const props = { state: current, sessionEnded: false, expanded: {}, cursor: -1, onAnswerPermission: vi.fn() };
    try {
      const view = render(<MessageList {...props} />);
      expect(trims).toBe(0);
      view.rerender(<MessageList {...props} state={{ ...current, thinkingOpen: false }} />);
      expect(view.container.querySelector(".thinking-head")!.textContent).toContain("500 lines");
      expect(trims).toBe(1);
      view.rerender(<MessageList {...props} state={{ ...current, thinkingOpen: false }} expanded={{ "k-1": true }} />);
      expect(trims).toBe(1);
    } finally { trim.mockRestore(); }
  });
});
