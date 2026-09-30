// @vitest-environment jsdom
/**
 * Owner decision #39 (2026-09-30, "input 直接ctrl y统一吧，不用两次"): in INPUT, a single `Ctrl+y`
 * approves the ACTIVE tab's OLDEST waiting card without leaving INPUT. An ADDED key under the v1
 * freeze -- `Ctrl+y` did nothing in INPUT before -- and BROWSE's own `Ctrl+y` (vim's one line up,
 * v1 trial item 5) is untouched. There is no deny key in INPUT: the owner kept deny where a reason
 * can be typed.
 *
 * Each rule of the handoff (`the private review notes`) has a case
 * here: the target (0/1/2 cards, two tabs), the exact chord (Shift/Alt/AltGraph/Meta/Super/Hyper and
 * an input method composing all refuse), the S1 typing guard (readline's Ctrl+u / Ctrl+w then Ctrl+y
 * yank never approves), the answer by the card's own permission id, the band segment naming the
 * card, the `?` overlay's row, the composer left exactly as it was, and BROWSE's Ctrl+y still a
 * scroll that answers nothing.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { AgentDomainEvent, AgentUiState, Hello } from "./types";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

let posted: Array<Record<string, unknown>>;

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { neovibeAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
});

function dispatch(payload: unknown) {
  act(() => {
    window.__neovibeDispatch!(JSON.stringify(payload));
  });
}

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["auto", "bypass"],
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

/** Tab 1 live with `cards` cards, the pane focused, the keys arrived (a card landing, R11, when one
 *  waits), the band widened. Stays in BROWSE. */
function started(cards: 0 | 1 | 2, tabs: readonly Record<string, unknown>[] = [TAB1]) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState() });
  const list = cardEvents(cards);
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

/** Ctrl+y on the composer, as a real keyboard delivers the `y` (the bare Control keydown before it
 *  is a modifier, which the guard ignores). Returns whether its default survived. */
const ctrlY = (box: HTMLElement, init: Record<string, unknown> = {}) =>
  fireEvent.keyDown(box, { key: "y", ctrlKey: true, ...init });

const TYPING_FLASH = "Ctrl+y approves only on its own — pause, then press it again";

describe("#39: INPUT Ctrl+y approves the active tab's oldest waiting card", () => {
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
      expect(container.querySelector('[data-testid="mode-block"]')!.textContent).toBe("INPUT");
    });

    it("two cards: the OLDEST, even with the row cursor left on the newer one; the next Ctrl+y the other", () => {
      const rendered = started(2);
      const { container } = rendered;
      // The landing took the oldest card (R11); `G` moves the row cursor off it, onto the last row.
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

  describe("rule 3: the S1 typing guard", () => {
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

  /* Fix round 1 (2026-09-30): the Opus and Codex reviews of 4795d76e. Each case below was seen
     failing on 4795d76e first (`/scratch/input-ctrl-y/fix1-failing-first.txt`). */
  describe("fix round 1", () => {
    const TARGET_FLASH = "Ctrl+y: the waiting card just changed — read it, then press again";
    const YANK_FLASH = "Ctrl+y right after Ctrl+u, Ctrl+w or Ctrl+k is a yank, not an answer — press it again";

    /** Opus B-1 (A1): the band moves on to the next card the moment the first is approved; a
     *  second Ctrl+y 10 ms after that must not approve a card that has been the target for 10 ms. */
    it("B-1/A1: a second Ctrl+y 260 ms after the first approves only the first card", () => {
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

    /** Opus B-1 (B2): the card the band named is withdrawn and another takes its place. */
    it("B-1/B2: a card replaced 30 ms before Ctrl+y is not approved", () => {
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

    /** Opus B-1 (B3): no card; one arrives 30 ms before a Ctrl+y meant as something else. */
    it("B-1/B3: a card that arrived 30 ms before Ctrl+y is not approved", () => {
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

    /** Codex blocking (= Opus I-2): the Ctrl+r search stops its own Enter/Tab/Escape, so the typing
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

    /** Codex important: focus can move to the search field with a click, which is no key. */
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

    /** Opus I-3 (= Codex minor): the band names a Ctrl+y that does nothing while the search has the keys. */
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

    /** Opus I-1: "yank never approves" must not depend on typing fast. */
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

    /** Opus I-4: under Caps Lock GTK/WebKitGTK deliver Ctrl+y as key "Y" with Shift not held. */
    it("Caps Lock: Ctrl+y arriving as key Y, no Shift, approves", () => {
      const { box } = inInput(1);
      ctrlY(box, { key: "Y" });
      wait(TYPING_GUARD_MS);
      expect(answered().map((m) => m.permission_id)).toEqual(["perm-1"]);
    });

    /** Opus M-1: the fire-time re-checks, pinned. */
    it("A7: the session closing inside the window approves nothing", () => {
      const { box } = inInput(1);
      ctrlY(box);
      wait(100);
      dispatch({ kind: "events", tab: 1, fromRevision: 5, throughRevision: 6, events: [{ type: "session_closed", reason: "x" }] });
      wait(1000);
      expect(answered()).toEqual([]);
    });

    it("A8: the card resolved by the provider and a new one arriving inside the window approves nothing", () => {
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
