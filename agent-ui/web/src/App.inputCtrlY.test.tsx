// @vitest-environment jsdom
/**
 * In INPUT, a single `Ctrl+y` approves the active tab's oldest waiting card without leaving INPUT.
 * BROWSE's `Ctrl+y` still scrolls one line up. Denying stays on the card, where a reason can be typed.
 *
 * These tests cover the target across card counts and tabs, the exact chord, composing input,
 * the typing guard, and readline yanks after Ctrl+u or Ctrl+w. They also check the permission id,
 * the band and `?` overlay, the unchanged composer, and BROWSE scrolling without answering.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { AgentDomainEvent, AgentUiState, Hello } from "./types";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { RELEASE_WAIT_MAX_MS, TYPING_GUARD_MS } from "./typingGuard";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

let posted: Array<Record<string, unknown>>;

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
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
  expectedVerdandiRevision: null,
  account: null,
};

const TAB1 = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;
const TAB2 = { ...TAB1, id: 2, number: 2, label: "2 new" } as const;

function snapshotState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return { ...initialState(), status: { kind: "running" }, ...overrides };
}

/** The same `ResizeObserver` stub `App.test.tsx`'s `stubBandWidth` uses: jsdom has no layout, so the
 *  band shows only mode and pill until something reports a real width. */
function stubBandWidth(): (container: HTMLElement) => void {
  type Rec = { callback: ResizeObserverCallback; observed: Element[] };
  const observers: Rec[] = [];
  class FakeResizeObserver {
    private record: Rec;
    constructor(callback: ResizeObserverCallback) {
      this.record = { callback, observed: [] };
      observers.push(this.record);
    }
    observe(el: Element) {
      this.record.observed.push(el);
    }
    unobserve() {}
    disconnect() {}
  }
  vi.stubGlobal("ResizeObserver", FakeResizeObserver);
  return (container: HTMLElement) => {
    const band = container.querySelector(".status-band");
    const measure = container.querySelector(".band-measure");
    for (const o of observers) {
      for (const el of o.observed) {
        if (el === band) o.callback([{ target: el, contentRect: { width: 1400 } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
        if (el === measure) o.callback([{ target: el, contentRect: { width: 7.2 } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
      }
    }
  };
}

/** A running turn in tab 1 with `cards` cards waiting: `perm-1` (Bash `rm build`, the OLDER) and
 *  `perm-2` (Bash `git push`, the newer), each gating its own tool call. */
function cardEvents(cards: 0 | 1 | 2): AgentDomainEvent[] {
  const list: AgentDomainEvent[] = [
    { type: "user_prompt_submitted", text: "tidy up" },
    { type: "turn_started", turn_id: "t1" },
  ];
  if (cards >= 1) {
    list.push(
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "rm build" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: { command: "rm build" } },
    );
  }
  if (cards >= 2) {
    list.push(
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_2", name: "Bash", input: { command: "git push" } },
      { type: "permission_requested", permission_id: "perm-2", tool_use_id: "toolu_2", tool_name: "Bash", input: { command: "git push" } },
    );
  }
  list.push({ type: "content_delta", turn_id: "t1", kind: "text", text: "meanwhile" });
  return list;
}

let widen: (container: HTMLElement) => void;

const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));
const answered = () => posted.filter((m) => m.type === "permission_response");
const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
const textarea = (c: HTMLElement) => c.querySelector<HTMLTextAreaElement>(".composer textarea");
const bandText = (c: HTMLElement) => c.querySelector(".status-band")?.textContent ?? "";
const bandMessage = (c: HTMLElement) => c.querySelector(".band-message")?.textContent ?? null;

/** Tab 1 live with `cards` cards, the pane focused, the keys arrived (a card landing when one
 *  waits), the band widened. Stays in BROWSE. */
function started(
  cards: 0 | 1 | 2,
  tabs: readonly Record<string, unknown>[] = [TAB1],
  events: AgentDomainEvent[] = cardEvents(cards),
) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState() });
  const list = events;
  dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  dispatch({
    kind: "keymap",
    prefix: "Ctrl+b",
    window: [],
    prefixKeys: [],
    panel: EMPTY_PANEL_TABLE,
    newTabChord: "Ctrl+b c",
  });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  return rendered;
}

/** `started`, then `i` into INPUT and a pause longer than the guard, so the next key stands alone. */
function inInput(cards: 0 | 1 | 2, tabs: readonly Record<string, unknown>[] = [TAB1]) {
  const rendered = started(cards, tabs);
  wait(TYPING_GUARD_MS + 50);
  fireEvent.keyDown(root(rendered.container), { key: "i" });
  wait(TYPING_GUARD_MS + 50);
  const box = textarea(rendered.container);
  expect(box).not.toBeNull();
  expect(document.activeElement).toBe(box);
  return { ...rendered, box: box! };
}

/** Ctrl+y pressed on the composer and held: only its keydown, as a real keyboard delivers the `y`
 *  (the bare Control keydown before it is a modifier, which the guard ignores). Returns whether its
 *  default survived. */
const ctrlYDown = (box: HTMLElement, init: Record<string, unknown> = {}) =>
  fireEvent.keyDown(box, { key: "y", code: "KeyY", ctrlKey: true, ...init });

/** The `y` of a Ctrl+y let go. */
const ctrlYUp = (box: HTMLElement) => fireEvent.keyUp(box, { key: "y", code: "KeyY", ctrlKey: true });

/** Ctrl+y tapped on the composer: pressed and let go at once. Returns whether the keydown's default
 *  survived. */
const ctrlY = (box: HTMLElement, init: Record<string, unknown> = {}) => {
  const kept = ctrlYDown(box, init);
  ctrlYUp(box);
  return kept;
};

const TYPING_FLASH = "Ctrl+y approves only on its own — pause, then press it again";
const HELD_FLASH = "Ctrl+y held down approves nothing — tap it to approve";

/** A key event for `y` with `Ctrl` whose `timeStamp` is `stamp`, as a page handling it late still sees
 *  the time it happened. */
function stampedCtrlY(type: "keydown" | "keyup", stamp: number): KeyboardEvent {
  const event = new KeyboardEvent(type, { key: "y", code: "KeyY", ctrlKey: true, bubbles: true, cancelable: true });
  Object.defineProperty(event, "timeStamp", { value: stamp });
  return event;
}

describe("INPUT Ctrl+y approves the active tab's oldest waiting card", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    widen = stubBandWidth();
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  describe("rule 1: the target", () => {
    it("with no card waiting, Ctrl+y does nothing and leaves the key to the box", () => {
      const { container, box } = inInput(0);
      expect(ctrlY(box)).toBe(true);
      wait(1000);
      expect(answered()).toEqual([]);
      expect(bandMessage(container)).toBeNull();
      expect(document.activeElement).toBe(box);
    });

    it("one card: approved TYPING_GUARD_MS later, by its own id, in its own tab, and INPUT stays", () => {
      const { container, box } = inInput(1);
      expect(ctrlY(box)).toBe(false);
      wait(TYPING_GUARD_MS - 1);
      expect(answered()).toEqual([]);
      wait(1);
      expect(answered()).toEqual([
        expect.objectContaining({ type: "permission_response", tab: 1, permission_id: "perm-1", decision: "allow" }),
      ]);
      expect(document.activeElement).toBe(textarea(container));
      expect(container.querySelector('[data-testid="mode-block"]')!.textContent).toBe("INSERT");
    });

    it("a held Ctrl+y approves nothing: still down when the wait ends, then its first repeat refuses it", () => {
      const { container, box } = inInput(1);
      expect(ctrlYDown(box)).toBe(false);
      // GTK's first repeat comes after its delay (600 ms on sway, 500 on GNOME), well past the wait.
      wait(600);
      expect(answered()).toEqual([]);
      ctrlYDown(box, { repeat: true });
      wait(40);
      ctrlYDown(box, { repeat: true });
      ctrlYUp(box);
      wait(1000);
      expect(answered()).toEqual([]);
      expect(bandMessage(container)).toBe(TYPING_FLASH);
    });

    it("a slow press let go after the wait, with no repeat, approves at its release", () => {
      const { box } = inInput(1);
      ctrlYDown(box);
      wait(400);
      expect(answered()).toEqual([]);
      ctrlYUp(box);
      expect(answered()).toEqual([
        expect.objectContaining({ type: "permission_response", tab: 1, permission_id: "perm-1", decision: "allow" }),
      ]);
    });

    it("with key repeat off, a Ctrl+y held past RELEASE_WAIT_MAX_MS approves nothing at its release, and says why", () => {
      const { container, box } = inInput(1);
      ctrlYDown(box);
      // No repeat ever comes (GNOME's "Repeat Keys" off): only the release.
      wait(2000);
      expect(answered()).toEqual([]);
      ctrlYUp(box);
      wait(1000);
      expect(answered()).toEqual([]);
      expect(bandMessage(container)).toBe(HELD_FLASH);
    });

    it("a quick press whose release the page handles late, after a stall, still approves: the hold is read from the events' own times", () => {
      const { box } = inInput(1);
      // The events' clock, whatever it counts from.
      const pressedAt = new Event("probe").timeStamp;
      fireEvent(box, stampedCtrlY("keydown", pressedAt));
      // The page's main thread stalls well past RELEASE_WAIT_MAX_MS; the keyup it then handles happened
      // 100 ms after the press.
      wait(RELEASE_WAIT_MAX_MS * 3);
      expect(answered()).toEqual([]);
      fireEvent(box, stampedCtrlY("keyup", pressedAt + 100));
      expect(answered()).toEqual([
        expect.objectContaining({ type: "permission_response", tab: 1, permission_id: "perm-1", decision: "allow" }),
      ]);
    });

    it("a held Ctrl+y whose release the page handles at once is refused by the events' times too", () => {
      const { container, box } = inInput(1);
      const pressedAt = new Event("probe").timeStamp;
      fireEvent(box, stampedCtrlY("keydown", pressedAt));
      wait(TYPING_GUARD_MS + 10);
      fireEvent(box, stampedCtrlY("keyup", pressedAt + RELEASE_WAIT_MAX_MS + 1));
      wait(1000);
      expect(answered()).toEqual([]);
      expect(bandMessage(container)).toBe(HELD_FLASH);
    });

    it("a press let go just inside RELEASE_WAIT_MAX_MS still approves at its release", () => {
      const { box } = inInput(1);
      ctrlYDown(box);
      wait(RELEASE_WAIT_MAX_MS - 1);
      ctrlYUp(box);
      expect(answered()).toEqual([
        expect.objectContaining({ type: "permission_response", tab: 1, permission_id: "perm-1", decision: "allow" }),
      ]);
    });

    it("two cards: the OLDEST, even with the row cursor left on the newer one; the next Ctrl+y the other", () => {
      const rendered = started(2);
      const { container } = rendered;
      // The landing took the oldest card; `G` moves the row cursor off it, onto the last row.
      wait(TYPING_GUARD_MS + 50);
      fireEvent.keyDown(root(container), { key: "G", shiftKey: true });
      expect(container.querySelector(".row-current")!.textContent).not.toContain("rm build");
      wait(TYPING_GUARD_MS + 50);
      fireEvent.keyDown(root(container), { key: "i" });
      wait(TYPING_GUARD_MS + 50);
      const box = textarea(container)!;
      ctrlY(box);
      wait(TYPING_GUARD_MS);
      expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "allow" })]);
      wait(TYPING_GUARD_MS + 50);
      ctrlY(box);
      wait(TYPING_GUARD_MS);
      expect(answered().map((m) => m.permission_id)).toEqual(["perm-1", "perm-2"]);
    });

    it("two tabs: a card waiting only in the OTHER tab is never Ctrl+y's", () => {
      const { box } = inInput(0, [TAB1, { ...TAB2, pending: 1, marker: "needs_input" }]);
      expect(ctrlY(box)).toBe(true);
      wait(1000);
      expect(answered()).toEqual([]);
    });

    it("two tabs: a switch while the approval waits cancels it; the new tab's card is not answered", () => {
      const { box } = inInput(1, [TAB1, TAB2]);
      ctrlY(box);
      wait(100);
      dispatch({ kind: "tabs", active: 2, tabs: [TAB1, TAB2] });
      const list: AgentDomainEvent[] = [
        { type: "turn_started", turn_id: "t9" },
        { type: "permission_requested", permission_id: "perm-9", tool_use_id: null, tool_name: "Bash", input: { command: "ls" } },
      ];
      dispatch({ kind: "snapshot", tab: 2, throughRevision: 0, state: snapshotState() });
      dispatch({ kind: "events", tab: 2, fromRevision: 0, throughRevision: list.length, events: list });
      wait(1000);
      expect(answered()).toEqual([]);
    });
  });

  describe("rule 2: exactly Ctrl+y", () => {
    it.each([
      ["Shift", { key: "Y", shiftKey: true }],
      ["Shift, reported as a lowercase y", { shiftKey: true }],
      ["Alt", { altKey: true }],
      ["AltGraph", { modifierAltGraph: true }],
      ["Meta", { metaKey: true }],
      ["Super", { modifierSuper: true }],
      ["Hyper", { modifierHyper: true }],
      ["an input method composing", { isComposing: true }],
      ["an input method's keyCode 229", { keyCode: 229 }],
    ])("Ctrl+y with %s approves nothing", (_name, init) => {
      const { box } = inInput(1);
      ctrlY(box, init);
      wait(1000);
      expect(answered()).toEqual([]);
    });

    it("Ctrl+y while Super is held, as WebKitGTK reports it (Super's own keydown only), approves nothing", () => {
      const { box } = inInput(1);
      fireEvent.keyDown(box, { key: "Super", code: "OSLeft" });
      wait(TYPING_GUARD_MS + 50);
      ctrlY(box);
      wait(1000);
      expect(answered()).toEqual([]);
      fireEvent.keyUp(box, { key: "Super", code: "OSLeft" });
    });
  });

  describe("only the composer's own box", () => {
    it("Ctrl+y in the Ctrl+r history search -- a query, not the draft -- approves nothing", () => {
      const { container, box } = inInput(1);
      fireEvent.keyDown(box, { key: "r", ctrlKey: true });
      const search = container.querySelector<HTMLInputElement>(".history-search input");
      expect(search).not.toBeNull();
      act(() => search!.focus());
      wait(TYPING_GUARD_MS + 50);
      fireEvent.keyDown(search!, { key: "y", ctrlKey: true });
      wait(1000);
      expect(answered()).toEqual([]);
    });
  });

  describe("the typing guard", () => {
    it.each([
      ["Ctrl+u", "u"],
      ["Ctrl+w", "w"],
    ])("%s then Ctrl+y at 100 ms -- readline's yank -- approves nothing, and the band says why", (_name, letter) => {
      const { container, box } = inInput(1);
      fireEvent.change(box, { target: { value: "some words here" } });
      box.setSelectionRange(15, 15);
      fireEvent.keyDown(box, { key: letter, ctrlKey: true });
      wait(100);
      ctrlY(box);
      wait(1000);
      expect(answered()).toEqual([]);
      expect(bandMessage(container)).toBe(TYPING_FLASH);
    });

    it("a letter typed right after Ctrl+y cancels the approval, and says so", () => {
      const { container, box } = inInput(1);
      ctrlY(box);
      wait(100);
      fireEvent.keyDown(box, { key: "x" });
      wait(1000);
      expect(answered()).toEqual([]);
      expect(bandMessage(container)).toBe(TYPING_FLASH);
    });

    it("a held Ctrl+y's repeat approves nothing", () => {
      const { box } = inInput(1);
      ctrlY(box, { repeat: true });
      wait(1000);
      expect(answered()).toEqual([]);
    });

    it("the bare Control keydown before the y is not a key: a lone Ctrl+y still approves", () => {
      const { box } = inInput(1);
      fireEvent.keyDown(box, { key: "Control", ctrlKey: true });
      ctrlY(box);
      wait(TYPING_GUARD_MS);
      expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-1", decision: "allow" })]);
    });
  });

  describe("rule 5: the band, the overlay, and the composer", () => {
    it("the band names the card Ctrl+y would approve while the box has the keys, and only then", () => {
      const rendered = started(2);
      const { container } = rendered;
      // BROWSE: `a`/`d` are the keys there; the band does not advertise Ctrl+y.
      expect(bandText(container)).not.toContain("Ctrl+y");
      wait(TYPING_GUARD_MS + 50);
      fireEvent.keyDown(root(container), { key: "i" });
      expect(bandText(container)).toContain("Ctrl+y approves Bash: rm build");
      const box = textarea(container)!;
      wait(TYPING_GUARD_MS + 50);
      ctrlY(box);
      wait(TYPING_GUARD_MS);
      // The first is answered; the band now names the next one waiting.
      expect(bandText(container)).toContain("Ctrl+y approves Bash: git push");
      // Focus leaving the pane: the box no longer has the keys.
      dispatch({ kind: "pane_focus", focused: false });
      expect(bandText(container)).not.toContain("Ctrl+y");
    });

    it("with no card waiting the band says nothing about Ctrl+y", () => {
      const { container } = inInput(0);
      expect(bandText(container)).not.toContain("Ctrl+y");
    });

    it("the ? overlay's Typing section lists Ctrl+y", () => {
      const { container, box } = inInput(1);
      fireEvent.keyDown(box, { key: "?", shiftKey: true });
      const overlay = container.querySelector<HTMLElement>(".keymap-overlay");
      expect(overlay).not.toBeNull();
      const typing = Array.from(overlay!.querySelectorAll("section")).find(
        (s) => s.querySelector("h2")?.textContent === "Typing",
      )!;
      const rows = Array.from(typing.querySelectorAll("tr")).map((tr) => tr.textContent ?? "");
      expect(rows.some((r) => r.startsWith("Ctrl+y") && /oldest/.test(r))).toBe(true);
    });

    it("an answer leaves the composer's text, caret and history exactly as they were", () => {
      const { container, box } = inInput(1);
      fireEvent.change(box, { target: { value: "draft text" } });
      box.setSelectionRange(3, 3);
      wait(TYPING_GUARD_MS + 50);
      ctrlY(box);
      wait(TYPING_GUARD_MS);
      expect(answered()).toHaveLength(1);
      const after = textarea(container)!;
      expect(after.value).toBe("draft text");
      expect(after.selectionStart).toBe(3);
      expect(after.selectionEnd).toBe(3);
      expect(posted.filter((m) => m.type === "history_push")).toEqual([]);
    });
  });

  describe("BROWSE's Ctrl+y is untouched", () => {
    it("on the card itself, BROWSE Ctrl+y answers nothing", () => {
      const { container } = started(1);
      expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
      wait(TYPING_GUARD_MS + 50);
      expect(fireEvent.keyDown(root(container), { key: "y", ctrlKey: true })).toBe(false);
      wait(1000);
      expect(answered()).toEqual([]);
      expect(container.querySelector('[data-testid="mode-block"]')!.textContent).toBe("BROWSE");
    });
  });

  /* Cases where a delayed approval must refuse a changed target or preserve a yank. */
  describe("the answer window and the yank guard", () => {
    const TARGET_FLASH = "Ctrl+y: the waiting card just changed — read it, then press again";
    const YANK_FLASH = "Ctrl+y right after Ctrl+u, Ctrl+w or Ctrl+k is a yank, not an answer — press it again";

    /** The band moves on to the next card the moment the first is approved; a
     *  second Ctrl+y 10 ms after that must not approve a card that has been the target for 10 ms. */
    it("a second Ctrl+y 260 ms after the first approves only the first card", () => {
      const { container, box } = inInput(2);
      ctrlY(box);
      wait(TYPING_GUARD_MS + 10);
      ctrlY(box);
      wait(1000);
      expect(answered().map((m) => m.permission_id)).toEqual(["perm-1"]);
      expect(bandMessage(container)).toBe(TARGET_FLASH);
      // Once the second card has been the target for the guard window, a lone Ctrl+y approves it.
      ctrlY(box);
      wait(TYPING_GUARD_MS);
      expect(answered().map((m) => m.permission_id)).toEqual(["perm-1", "perm-2"]);
    });

    /** The card the band named is withdrawn and another takes its place. */
    it("a card replaced 30 ms before Ctrl+y is not approved", () => {
      const { container, box } = inInput(1);
      dispatch({
        kind: "events", tab: 1, fromRevision: 5, throughRevision: 7,
        events: [
          { type: "permission_resolved", permission_id: "perm-1", outcome: "expired" },
          { type: "permission_requested", permission_id: "perm-3", tool_use_id: null, tool_name: "Bash", input: { command: "rm -rf ~/work" } },
        ],
      });
      wait(30);
      ctrlY(box);
      wait(1000);
      expect(answered()).toEqual([]);
      expect(bandMessage(container)).toBe(TARGET_FLASH);
    });

    /** No card; one arrives 30 ms before a Ctrl+y meant as something else. */
    it("a card that arrived 30 ms before Ctrl+y is not approved", () => {
      const { box } = inInput(0);
      dispatch({
        kind: "events", tab: 1, fromRevision: 3, throughRevision: 5,
        events: [
          { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_5", name: "Bash", input: { command: "git push --force" } },
          { type: "permission_requested", permission_id: "perm-5", tool_use_id: "toolu_5", tool_name: "Bash", input: { command: "git push --force" } },
        ],
      });
      wait(30);
      ctrlY(box);
      wait(1000);
      expect(answered()).toEqual([]);
    });

    /** A tab that arrives in the page -- another view's tab joined back, or a switch -- brings its
     *  waiting card with it: that is a change of target like any other, so a Ctrl+y right after the
     *  arrival is refused, and the arrived card is approved once it has been the target long enough. */
    it("a tab arriving in the page counts as a change of waiting card: Ctrl+y right after it is refused", () => {
      const { container } = inInput(0, [TAB1]);
      const TAB3 = { ...TAB1, id: 3, number: 3, label: "3 new", pending: 1, marker: "needs_input" } as const;
      dispatch({ kind: "tabs", active: 3, tabs: [TAB3] });
      dispatch({ kind: "snapshot", tab: 3, throughRevision: 0, state: snapshotState() });
      const list = cardEvents(1);
      dispatch({ kind: "events", tab: 3, fromRevision: 0, throughRevision: list.length, events: list });
      // The switch lands BROWSE; the window hands the composer the keys again, with no key pressed.
      dispatch({ kind: "enter_input" });
      wait(30);
      const box = textarea(container);
      expect(box).not.toBeNull();
      expect(document.activeElement).toBe(box);
      ctrlY(box!);
      wait(TYPING_GUARD_MS + 50);
      expect(answered()).toEqual([]);
      expect(bandMessage(container)).toBe(TARGET_FLASH);
      ctrlY(textarea(container)!);
      wait(TYPING_GUARD_MS);
      expect(answered()).toEqual([
        expect.objectContaining({ type: "permission_response", tab: 3, permission_id: "perm-1", decision: "allow" }),
      ]);
    });

    /** The Ctrl+r search stops its own Enter/Tab/Escape, so the typing
     *  guard never saw them. */
    it.each(["Enter", "Tab", "Escape"])("the Ctrl+r search's %s counts as a key: Ctrl+y 20 ms after it approves nothing", (exit) => {
      const { container, box } = inInput(1);
      dispatch({ kind: "history", entries: ["fix parser"] });
      fireEvent.keyDown(box, { key: "r", ctrlKey: true });
      const search = container.querySelector<HTMLInputElement>(".history-search input")!;
      act(() => search.focus());
      fireEvent.change(search, { target: { value: "f" } });
      wait(TYPING_GUARD_MS + 50);
      fireEvent.keyDown(search, { key: exit });
      wait(20);
      const after = textarea(container)!;
      expect(document.activeElement).toBe(after);
      ctrlY(after);
      wait(1000);
      expect(answered()).toEqual([]);
      expect(bandMessage(container)).toBe(TYPING_FLASH);
    });

    /** Focus can move to the search field with a click, which is no key. */
    it("the approval waits for a box that still has the keys: a click into the Ctrl+r search inside the window cancels it", () => {
      const { container, box } = inInput(1);
      fireEvent.keyDown(box, { key: "r", ctrlKey: true });
      const search = container.querySelector<HTMLInputElement>(".history-search input")!;
      act(() => box.focus());
      wait(TYPING_GUARD_MS + 50);
      ctrlY(box);
      wait(100);
      act(() => search.focus());
      wait(1000);
      expect(answered()).toEqual([]);
    });

    /** The band names a Ctrl+y that does nothing while the search has the keys. */
    it("the band does not advertise Ctrl+y while the Ctrl+r search has the keys", () => {
      const { container, box } = inInput(1);
      expect(bandText(container)).toContain("Ctrl+y approves");
      fireEvent.keyDown(box, { key: "r", ctrlKey: true });
      const search = container.querySelector<HTMLInputElement>(".history-search input")!;
      act(() => search.focus());
      expect(bandText(container)).not.toContain("Ctrl+y");
      act(() => box.focus());
      expect(bandText(container)).toContain("Ctrl+y approves");
    });

    /** "Yank never approves" must not depend on typing fast. */
    it.each(["u", "w", "k"])("Ctrl+%s, a 300 ms pause, then Ctrl+y is a yank: nothing approved; a second Ctrl+y approves", (letter) => {
      const { container, box } = inInput(1);
      fireEvent.change(box, { target: { value: "some words here" } });
      box.setSelectionRange(15, 15);
      fireEvent.keyDown(box, { key: letter, ctrlKey: true });
      wait(300);
      ctrlY(box);
      wait(1000);
      expect(answered()).toEqual([]);
      expect(bandMessage(container)).toBe(YANK_FLASH);
      ctrlY(box);
      wait(TYPING_GUARD_MS);
      expect(answered().map((m) => m.permission_id)).toEqual(["perm-1"]);
    });

    it("a kill, then another key, then a pause and Ctrl+y approves (only the key right before counts)", () => {
      const { box } = inInput(1);
      fireEvent.keyDown(box, { key: "u", ctrlKey: true });
      wait(300);
      fireEvent.keyDown(box, { key: "x" });
      wait(300);
      ctrlY(box);
      wait(TYPING_GUARD_MS);
      expect(answered().map((m) => m.permission_id)).toEqual(["perm-1"]);
    });

    /** Under Caps Lock GTK/WebKitGTK deliver Ctrl+y as key "Y" with Shift not held. */
    it("Caps Lock: Ctrl+y arriving as key Y, no Shift, approves", () => {
      const { box } = inInput(1);
      ctrlY(box, { key: "Y" });
      wait(TYPING_GUARD_MS);
      expect(answered().map((m) => m.permission_id)).toEqual(["perm-1"]);
    });

    /** Re-check the target and context when the deferred answer fires. */
    it("the session closing inside the window approves nothing", () => {
      const { box } = inInput(1);
      ctrlY(box);
      wait(100);
      dispatch({ kind: "events", tab: 1, fromRevision: 5, throughRevision: 6, events: [{ type: "session_closed", reason: "x" }] });
      wait(1000);
      expect(answered()).toEqual([]);
    });

    it("the card resolved by the provider and a new one arriving inside the window approves nothing", () => {
      const { box } = inInput(1);
      ctrlY(box);
      wait(100);
      dispatch({
        kind: "events", tab: 1, fromRevision: 5, throughRevision: 7,
        events: [
          { type: "permission_resolved", permission_id: "perm-1", outcome: "denied" },
          { type: "permission_requested", permission_id: "perm-3", tool_use_id: null, tool_name: "Bash", input: { command: "rm -rf /" } },
        ],
      });
      wait(1000);
      expect(answered()).toEqual([]);
    });

    it("leaving INPUT with a click inside the window approves nothing", () => {
      const { container, box } = inInput(1);
      ctrlY(box);
      wait(100);
      const row = container.querySelector<HTMLElement>(".row")!;
      fireEvent.mouseDown(row);
      act(() => root(container).focus());
      fireEvent.click(row);
      wait(1000);
      expect(answered()).toEqual([]);
    });
  });
});

/** An ACP card Approve cannot answer: the agent offered an allow-once option, but the call was too large for the
 *  card to show whole, so Rust held it back (`inputCut`) and left only the reject. */
function unapprovableAcpCard(): AgentDomainEvent[] {
  const input = {
    title: "Edit notes.txt",
    inputCut: true,
    options: [{ kind: "reject_once", name: "Reject" }],
    diffs: [],
    locations: [{ path: "/work/notes.txt" }],
  };
  return [
    { type: "user_prompt_submitted", text: "edit it" },
    { type: "turn_started", turn_id: "t1" },
    { type: "tool_call_started", turn_id: "t1", tool_use_id: "c1", name: "acp:edit", input },
    { type: "permission_requested", permission_id: "acp-1", tool_use_id: "c1", tool_name: "acp:edit", input },
  ];
}

describe("a card Approve cannot answer: neither Ctrl+y nor a sends an approval", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    widen = stubBandWidth();
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("the band does not offer Ctrl+y for it, and Ctrl+y flashes why and sends nothing", () => {
    const { container } = started(1, [TAB1], unapprovableAcpCard());
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "i" });
    wait(TYPING_GUARD_MS + 50);
    const box = textarea(container)!;
    expect(bandText(container)).not.toContain("Ctrl+y approves");
    expect(ctrlY(box)).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(bandMessage(container)).toContain("cannot be approved");
  });

  it("a on it flashes why and sends nothing; d still denies", () => {
    const { container } = started(1, [TAB1], unapprovableAcpCard());
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "a" });
    wait(1000);
    expect(answered()).toEqual([]);
    expect(bandMessage(container)).toContain("cannot be approved");
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "d" });
    wait(1000);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "acp-1", decision: "deny" })]);
  });
});
