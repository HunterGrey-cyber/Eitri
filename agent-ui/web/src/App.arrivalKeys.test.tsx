// @vitest-environment jsdom
/**
 * The first key after a keyboard arrival must act. After `Ctrl+l` from the
 * editor the first `v` did not enter CARET and a second `v` did.
 *
 * `arrive` and `focus_permission` do their landing in an effect, which runs only after React has
 * rendered the envelope. A key the page handles in between -- the shell moves GTK focus and sends the
 * envelope in the same instant, so the first key can reach the page before that render and its
 * effects have run -- is handled against the panel as it was drawn, and the landing used to run over
 * it afterwards: `exitRegion` ended the CARET the `v` had just started and `setMode("browse")` won.
 *
 * Each "before the landing" case delivers the envelope and the key inside ONE `act`, so the page
 * handles the key before React renders the envelope -- the order WebKitGTK produces whenever the key
 * lands in that gap. The same cases with the key after the landing are here too, as the control.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { AgentDomainEvent, AgentUiState, Hello, QuestionRecord } from "./types";
import { TYPING_GUARD_MS } from "./typingGuard";
import { TABLE } from "./testFixtures";

afterEach(cleanup);

beforeAll(() => {
  // jsdom implements no layout, so MessageList's auto-scroll would throw on a missing method.
  Element.prototype.scrollIntoView = vi.fn();
});

beforeEach(() => {
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: () => {} } },
  };
});

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  account: null,
};

const LIVE_TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

function snapshotState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return { ...initialState(), status: { kind: "running" }, ...overrides };
}

/** jsdom takes each keydown's timestamp from `Date.now()`; this moves it past the typing guard's
 *  window, as a person pausing would, so two keys a test fires are not one burst. */
let clockSkew = 0;
let clockSpy: { mockRestore: () => void } | null = null;
function keyGap(ms: number = TYPING_GUARD_MS + 50) {
  if (clockSpy === null) {
    const real = Date.now.bind(Date);
    clockSpy = vi.spyOn(Date, "now").mockImplementation(() => real() + clockSkew);
  }
  clockSkew += ms;
}

/** jsdom has no `Selection.modify`, which CARET's block caret extends by one character with. This
 *  moves the focus by one character within its own text node -- enough for the region to start. */
let restoreModify: () => void = () => {};
beforeEach(() => {
  const proto = Selection.prototype as unknown as { modify?: (alter: string, direction: string, granularity: string) => void };
  const original = proto.modify;
  proto.modify = function (this: Selection, alter: string, direction: string) {
    const node = this.focusNode;
    if (node === null) return;
    const length = (node.textContent ?? "").length;
    const offset = direction === "forward" ? Math.min(length, this.focusOffset + 1) : Math.max(0, this.focusOffset - 1);
    if (alter === "move") this.collapse(node, offset);
    else this.setBaseAndExtent(this.anchorNode ?? node, this.anchorOffset, node, offset);
  };
  restoreModify = () => {
    if (original === undefined) delete proto.modify;
    else proto.modify = original;
  };
});

afterEach(() => {
  restoreModify();
  window.getSelection()?.removeAllRanges();
  clockSpy?.mockRestore();
  clockSpy = null;
  clockSkew = 0;
});

const TRANSCRIPT = [
  { seq: 1, text: "first answer" },
  { seq: 2, text: "second answer" },
  { seq: 3, text: "third answer" },
];

/** A live tab with the keys in the panel, then the keys leave for the editor (`Ctrl+h`), as they
 *  do before every `Ctrl+l` back. */
function startedThenLeft(overrides: Partial<AgentUiState> = {}) {
  const { container } = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 3, state: snapshotState({ transcript: TRANSCRIPT, ...overrides }) });
  dispatch({ kind: "pane_focus", focused: true });
  const root = container.querySelector(".agent-ui-conversation")! as HTMLElement;
  const mode = () => container.querySelector<HTMLElement>("[data-testid=mode-block]")!.dataset.mode;
  const currentRow = () => container.querySelector(".row-current")?.textContent ?? "";
  return { container, root, mode, currentRow };
}

function leave() {
  dispatch({ kind: "pane_focus", focused: false });
  keyGap();
}

/** What `shell` sends for `Ctrl+l` (`set_pane_focused(true)`, then `arrive`), and the page handling
 *  `key` before React has rendered either. */
function arriveWithKeyFirst(root: HTMLElement, envelope: unknown, key: { key: string; shiftKey?: boolean }) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify({ kind: "pane_focus", focused: true }));
    window.__eitriDispatch!(JSON.stringify(envelope));
    fireEvent.keyDown(root, key);
  });
}

describe("the first key after an arrival is not undone by the landing", () => {
  it("v handled before the arrive landing still enters CARET", () => {
    const { root, mode } = startedThenLeft();
    leave();
    arriveWithKeyFirst(root, { kind: "arrive" }, { key: "v" });
    expect(mode()).toBe("caret");
    expect(window.getSelection()?.toString()).not.toBe("");
  });

  it("V handled before the arrive landing still enters V-LINE", () => {
    const { root, mode } = startedThenLeft();
    leave();
    arriveWithKeyFirst(root, { kind: "arrive" }, { key: "V", shiftKey: true });
    expect(mode()).toBe("vline");
  });

  it("v handled before a card landing (focus_permission) still enters CARET", () => {
    const { root, mode } = startedThenLeft({
      pendingPermissions: [{ seq: 4, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "cargo fmt" } }],
    });
    leave();
    arriveWithKeyFirst(root, { kind: "focus_permission", tab: 1 }, { key: "v" });
    expect(mode()).toBe("caret");
  });

  it("j handled before the arrive landing keeps its move (a reader who had moved off the end)", () => {
    const { root, currentRow } = startedThenLeft();
    keyGap();
    fireEvent.keyDown(root, { key: "g" });
    fireEvent.keyDown(root, { key: "g" });
    expect(currentRow()).toContain("first answer");
    leave();
    arriveWithKeyFirst(root, { kind: "arrive" }, { key: "j" });
    expect(currentRow()).toContain("second answer");
  });

  it("control: with the landing rendered first, v enters CARET on the last row", () => {
    const { root, mode, currentRow } = startedThenLeft();
    leave();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    expect(mode()).toBe("browse");
    expect(currentRow()).toContain("third answer");
    keyGap();
    fireEvent.keyDown(root, { key: "v" });
    expect(mode()).toBe("caret");
  });

  it("an arrival with no key in between still lands: BROWSE on the last row from INPUT, the card when one waits", () => {
    const { container, root, mode, currentRow } = startedThenLeft();
    keyGap();
    fireEvent.keyDown(root, { key: "i" });
    expect(mode()).toBe("input");
    leave();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    expect(mode()).toBe("browse");
    expect(currentRow()).toContain("third answer");
    expect(container.querySelector("textarea")).toBeNull();
    cleanup();

    const withCard = startedThenLeft({
      pendingPermissions: [{ seq: 4, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "cargo fmt" } }],
    });
    keyGap();
    fireEvent.keyDown(withCard.root, { key: "g" });
    fireEvent.keyDown(withCard.root, { key: "g" });
    leave();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    expect(withCard.mode()).toBe("browse");
    expect(withCard.container.querySelector(".row-current")?.className).toContain("row-permission");
  });

  it("a key handled before an earlier arrival does not stop a later arrival from landing", () => {
    const { root, mode, currentRow } = startedThenLeft();
    leave();
    arriveWithKeyFirst(root, { kind: "arrive" }, { key: "v" });
    expect(mode()).toBe("caret");
    leave();
    expect(mode()).toBe("browse");
    keyGap();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    expect(mode()).toBe("browse");
    expect(currentRow()).toContain("third answer");
  });
});

/* A reveal brings a tab forward while the person may still be typing where they asked for it (a face in another
   window, `prefix a`, `prefix u`). The arrival counts as a key for every answer path, so a key typed within
   `TYPING_GUARD_MS` of it, or in an unbroken run that began inside that window, answers nothing and the card,
   question or prompt keeps waiting. Each case runs after both envelopes a reveal can send. */
describe("an arrival counts as a key for every answer path", () => {
  let posted: Array<Record<string, unknown>>;
  beforeEach(() => {
    posted = [];
    (window as unknown as { webkit: unknown }).webkit = {
      messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
    };
    // `performance` moves with `Date`: the guards read both, as a page's do.
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date", "performance"] });
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));
  const posts = (type: string) => posted.filter((m) => m.type === type);
  const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
  const here = () => (document.activeElement ?? document.body) as HTMLElement;
  /** A keydown where the keys are; Enter or Space on a button activates it unless the page claimed the key. */
  function press(key: string, init: Record<string, unknown> = {}, target: HTMLElement = here()) {
    const open = fireEvent.keyDown(target, { key, ...init });
    if (open && target instanceof HTMLButtonElement && (key === "Enter" || key === " ")) fireEvent.click(target);
    fireEvent.keyUp(target, { key, ...init });
  }

  const QUESTION: QuestionRecord = {
    seq: 9, interactionId: "q1", toolUseId: null, turnId: "t1", unsupportedReason: null, state: { kind: "waiting" },
    questions: [{
      question: "Which database?", header: "Database", multiSelect: false,
      options: [{ label: "Postgres", description: "Relational", preview: null }, { label: "SQLite", description: "One file", preview: null }],
    }],
  };
  const CARD: AgentDomainEvent[] = [
    { type: "user_prompt_submitted", text: "tidy up" },
    { type: "turn_started", turn_id: "t1" },
    { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "rm build" } },
    { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: { command: "rm build" } },
    { type: "content_delta", turn_id: "t1", kind: "text", text: "meanwhile" },
  ];

  /** A live tab with a card waiting (and a question, when asked), the panel's table loaded, the keys in the panel
   *  and settled; a `j` has been pressed, as a person uses a chat before leaving it. */
  function live({ question = false } = {}) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO, backend: "sidecar" });
    dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
    dispatch({
      kind: "snapshot", tab: 1, throughRevision: 0,
      state: snapshotState({ transcript: TRANSCRIPT, questions: question ? [QUESTION] : [] }),
    });
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: CARD.length, events: CARD });
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    wait(TYPING_GUARD_MS + 50);
    press("j");
    wait(TYPING_GUARD_MS + 50);
    return rendered;
  }

  /** The keys leave for another window, where the person asks for this tab some time later. */
  function away() {
    dispatch({ kind: "pane_focus", focused: false });
    wait(1000);
  }

  type Via = "arrive" | "focus_permission";
  const VIAS: Via[] = ["arrive", "focus_permission"];
  /** What a reveal sends: an `arrive` saying why, or a card landing. */
  const envelope = (via: Via, bare = false) =>
    via === "arrive" ? (bare ? { kind: "arrive" } : { kind: "arrive", intent: "show" }) : { kind: "focus_permission", tab: 1 };
  /** The reveal reaches the page and its landing is drawn. */
  function reveal(via: Via, bare = false) {
    dispatch({ kind: "pane_focus", focused: true });
    dispatch(envelope(via, bare));
  }
  /** The reveal reaches the page and `key` is handled before React draws it: the page acts on the panel as it was. */
  function revealWithKeyFirst(via: Via, key: () => void) {
    act(() => {
      window.__eitriDispatch!(JSON.stringify({ kind: "pane_focus", focused: true }));
      window.__eitriDispatch!(JSON.stringify(envelope(via)));
      key();
    });
  }

  const onCard = (c: HTMLElement) => c.querySelector(".row-current")?.classList.contains("row-permission") === true;
  const answeredCards = () => posts("permission_response");

  describe.each(VIAS)("after %s", (via) => {
    it("an_answer_key_within_250ms_of_an_arrival_is_refused: a", () => {
      const { container } = live();
      away();
      reveal(via);
      expect(onCard(container)).toBe(true);
      wait(100);
      press("a");
      wait(1000);
      expect(answeredCards()).toEqual([]);
    });

    it("an_answer_key_within_250ms_of_an_arrival_is_refused: Enter on a card button", () => {
      const { container } = live();
      away();
      reveal(via);
      expect(onCard(container)).toBe(true);
      press("l");
      wait(80);
      expect(here().textContent).toBe("Approve");
      press("Enter");
      wait(1000);
      expect(answeredCards()).toEqual([]);
    });

    it("an_answer_key_within_250ms_of_an_arrival_is_refused: the question dock's Enter", () => {
      const { container } = live({ question: true });
      press("i", {}, root(container));
      wait(TYPING_GUARD_MS + 50);
      const dock = container.querySelector<HTMLElement>(".question-dock")!;
      expect(dock.contains(document.activeElement)).toBe(true);
      away();
      revealWithKeyFirst(via, () => press("Enter", {}, here()));
      wait(1000);
      expect(posts("question_answer")).toEqual([]);
    });

    it("an_answer_key_within_250ms_of_an_arrival_is_refused: trust y", () => {
      const { container } = live();
      dispatch({
        kind: "trust_prompt", tab: 1, nonce: 17, agent: "Claude", root: "/home/u/p", top: "/home/u/p",
        fingerprint: "a".repeat(64), findingsDigest: "b".repeat(64), state: "untrusted", remember: "yes", rememberNote: null,
        changed: null, items: [{ what: "hook", file: ".claude/settings.json", label: "SessionStart", value: "touch m", outside: false }],
      });
      wait(TYPING_GUARD_MS + 50);
      expect(container.querySelector(".trust-prompt")).not.toBeNull();
      revealWithKeyFirst(via, () => press("y", {}, root(container)));
      wait(1000);
      expect(posts("trust_answer")).toEqual([]);
    });

    it("an_answer_key_within_250ms_of_an_arrival_is_refused: Ctrl+y", () => {
      const { container } = live();
      press("i", {}, root(container));
      wait(TYPING_GUARD_MS + 50);
      expect(here().tagName).toBe("TEXTAREA");
      away();
      revealWithKeyFirst(via, () => press("y", { ctrlKey: true, code: "KeyY" }, here()));
      wait(1000);
      expect(answeredCards()).toEqual([]);
      // A Ctrl+y on its own a moment later is taken: the arrival refused one key, it did not wedge the box.
      press("y", { ctrlKey: true, code: "KeyY" }, here());
      wait(1000);
      expect(answeredCards()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "allow" })]);
    });

    it("an_answer_key_within_250ms_of_an_arrival_is_refused: <leader>m", () => {
      live();
      away();
      reveal(via);
      wait(100);
      press(" ");
      press("m");
      wait(1000);
      expect(posts("cycle_mode")).toEqual([]);
    });

    it("an_answer_key_after_250ms_is_taken", () => {
      const { container } = live();
      away();
      reveal(via);
      wait(TYPING_GUARD_MS);
      press("a");
      wait(TYPING_GUARD_MS);
      expect(answeredCards()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "allow" })]);
      cleanup();
      posted = [];

      live();
      away();
      reveal(via);
      wait(TYPING_GUARD_MS);
      press(" ");
      press("m");
      expect(posts("cycle_mode")).toHaveLength(1);
      cleanup();
      posted = [];

      const asked = live({ question: true });
      away();
      reveal(via);
      wait(TYPING_GUARD_MS);
      press("i", {}, root(asked.container));
      wait(TYPING_GUARD_MS);
      expect(asked.container.querySelector(".question-dock")!.contains(document.activeElement)).toBe(true);
      press("Enter");
      expect(posts("question_answer")).toHaveLength(1);
      void container;
    });

    it("a_refused_key_extends_the_run", () => {
      live();
      away();
      reveal(via);
      // Refused by the arrival; the second Space comes later than the window after it, and is refused because
      // the keys refused in between still count as typing.
      wait(200);
      press(" ");
      wait(10);
      press("m");
      wait(200);
      press(" ");
      wait(10);
      press("m");
      wait(1000);
      expect(posts("cycle_mode")).toEqual([]);
      expect(answeredCards()).toEqual([]);
      press(" ");
      press("m");
      expect(posts("cycle_mode")).toHaveLength(1);
    });
  });

  it("prefix_a_arrival_is_guarded_too", () => {
    // `prefix a` lands with a bare `arrive` (or the card landing, `focus_permission`, covered above).
    const { container } = live();
    away();
    reveal("arrive", true);
    expect(onCard(container)).toBe(true);
    wait(100);
    press("a");
    wait(1000);
    expect(answeredCards()).toEqual([]);
    wait(TYPING_GUARD_MS);
    press("a");
    wait(TYPING_GUARD_MS);
    expect(answeredCards()).toHaveLength(1);
  });
});
