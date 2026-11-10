// @vitest-environment jsdom
/**
 * INPUT's `Ctrl+y` answers nothing. Cards are answered in card mode; the chord is still claimed, so the box never
 * acts on it (the composer has no kill ring for it to yank from), and the band says where cards are. Every way a
 * `Ctrl+y` could still reach a card has a case here: each card kind (a provider card, an ACP card, a card of
 * Eitri's own tool broker) and none, from the composer and from the question dock, a lone press after a pause,
 * one right after a kill, one held down, and two in a row. BROWSE's `Ctrl+y` is still the one-line scroll and
 * answers nothing either.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { applyEvent, initialState } from "./reducer";
import type { AgentDomainEvent, AgentUiState, EditorCardWire, Hello, QuestionRecord } from "./types";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";
import { CARD_ARM_MS } from "./cardMode";
import { CTRL_Y_GONE_FLASH } from "./app/flashes";

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
  brokerCards: readonly EditorCardWire[] = [],
  snapshot: Partial<AgentUiState> = {},
) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState(snapshot) });
  const list = events;
  dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  if (brokerCards.length > 0) dispatch({ kind: "editor_cards", tab: 1, cards: brokerCards });
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
function inInput(
  cards: 0 | 1 | 2,
  tabs: readonly Record<string, unknown>[] = [TAB1],
  brokerCards: readonly EditorCardWire[] = [],
) {
  const rendered = started(cards, tabs, cardEvents(cards), brokerCards);
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
const yDown = (box: HTMLElement, init: Record<string, unknown> = {}) =>
  fireEvent.keyDown(box, { key: "y", code: "KeyY", ctrlKey: true, ...init });

/** The `y` of a Ctrl+y let go. */
const yUp = (box: HTMLElement) => fireEvent.keyUp(box, { key: "y", code: "KeyY", ctrlKey: true });

/** Ctrl+y tapped on the composer: pressed and let go at once. Returns whether the keydown's default
 *  survived. */
const tapY = (box: HTMLElement, init: Record<string, unknown> = {}) => {
  const kept = yDown(box, init);
  yUp(box);
  return kept;
};

const brokerAnswers = () => posted.filter((m) => m.type === "editor_card_answer");
const questionAnswers = () => posted.filter((m) => m.type === "question_answer" || m.type === "question_decline");
const modeOf = (c: HTMLElement) => c.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode") ?? null;

/** A card Eitri's own tool broker raised, as Rust serialises it. `seq` is where it sits among the conversation's rows. */
function brokerCard(id: string, seq: number, toolUseId: string | null = null): EditorCardWire {
  return {
    id, seq, tool: "open_file", toolUseId, title: "Show /etc/hosts in your editor?",
    body: [{ kind: "code", text: "/etc/hosts" }],
  };
}

/** An ACP agent's card, `allowed` saying whether it offered an allow-once option (without one Approve is off). */
function acpCard(allowed: boolean): AgentDomainEvent[] {
  const input = {
    title: "Edit notes.txt",
    options: allowed ? [{ kind: "allow_once", name: "Allow" }, { kind: "reject_once", name: "Reject" }] : [{ kind: "reject_once", name: "Reject" }],
    diffs: [],
    locations: [{ path: "/work/notes.txt" }],
    ...(allowed ? {} : { inputCut: true }),
  };
  return [
    { type: "user_prompt_submitted", text: "edit it" },
    { type: "turn_started", turn_id: "t1" },
    { type: "tool_call_started", turn_id: "t1", tool_use_id: "c1", name: "acp:edit", input },
    { type: "permission_requested", permission_id: "acp-1", tool_use_id: "c1", tool_name: "acp:edit", input },
  ];
}

const question: QuestionRecord = { seq: 1, interactionId: "q1", toolUseId: null, turnId: "t1", unsupportedReason: null, state: { kind: "waiting" }, questions: [{
  question: "Which database?", header: "Database", multiSelect: false,
  options: [{ label: "Postgres", description: "Relational", preview: null }, { label: "SQLite", description: "One file", preview: null }],
}] };

type Waiting = "a provider card" | "an ACP card" | "a broker card" | "no card";
type From = "the composer" | "the question dock";

/** Tab 1 with `waiting`, `i` into INPUT -- the composer, or the question dock when a question waits too -- and a
 *  pause past the guard. Returns the element that has the keys. */
function placed(waiting: Waiting, from: From) {
  const snapshot: Partial<AgentUiState> = from === "the question dock" ? { questions: [question] } : {};
  const rendered =
    waiting === "a provider card" ? started(1, [TAB1], cardEvents(1), [], snapshot)
    : waiting === "an ACP card" ? started(0, [TAB1], acpCard(true), [], snapshot)
    : waiting === "a broker card" ? started(0, [TAB1], cardEvents(0), [brokerCard("ec-0123456789abcdef-1", 1)], snapshot)
    : started(0, [TAB1], cardEvents(0), [], snapshot);
  const { container } = rendered;
  wait(TYPING_GUARD_MS + 50);
  fireEvent.keyDown(root(container), { key: "i" });
  wait(TYPING_GUARD_MS + 50);
  const target = document.activeElement as HTMLElement | null;
  expect(target).not.toBeNull();
  if (from === "the composer") {
    expect(target).toBe(textarea(container));
    const box = target as HTMLTextAreaElement;
    fireEvent.change(box, { target: { value: "draft text" } });
    box.setSelectionRange(3, 3);
    wait(TYPING_GUARD_MS + 50);
  } else {
    expect(target!.closest(".question-dock")).not.toBeNull();
  }
  return { ...rendered, target: target! };
}

/** What a key must leave as it was: the composer's text and caret (hidden behind the dock or not), and where the
 *  keys are. */
function draftOf(container: HTMLElement) {
  const box = textarea(container);
  return { text: box?.value ?? null, caret: box ? [box.selectionStart, box.selectionEnd] : null, focus: document.activeElement };
}

/** How `Ctrl+y` is pressed: what comes before it (judged as part of the draft it must leave alone), and the presses,
 *  returning whether any keydown's default survived. */
const PRESSES: Array<[string, (target: HTMLElement) => void, (target: HTMLElement) => boolean]> = [
  ["a lone Ctrl+y after a pause", () => {}, (target) => tapY(target)],
  // The kill acts on the draft first; the Ctrl+y right after it must change nothing more.
  ["Ctrl+y right after Ctrl+u", (target) => {
    fireEvent.keyDown(target, { key: "u", ctrlKey: true });
    wait(100);
  }, (target) => tapY(target)],
  ["a Ctrl+y held down, repeating", () => {}, (target) => {
    let kept = yDown(target);
    for (let i = 0; i < 3; i++) {
      wait(40);
      kept = yDown(target, { repeat: true }) || kept;
    }
    wait(600);
    yUp(target);
    return kept;
  }],
  ["two Ctrl+y in a row", () => {}, (target) => {
    const first = tapY(target);
    wait(TYPING_GUARD_MS + 50);
    return tapY(target) || first;
  }],
];

describe("INPUT Ctrl+y answers nothing", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    widen = stubBandWidth();
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  const waitings: Waiting[] = ["a provider card", "an ACP card", "a broker card", "no card"];
  const froms: From[] = ["the composer", "the question dock"];
  for (const waiting of waitings) {
    for (const from of froms) {
      for (const [how, before, press] of PRESSES) {
        it(`with ${waiting} waiting, from ${from}: ${how} is claimed, answers nothing and says cards are on Alt-n`, () => {
          const { container, target } = placed(waiting, from);
          before(target);
          const draft = draftOf(container);
          expect(press(target), "claimed").toBe(false);
          wait(1000);
          expect(draftOf(container)).toEqual(draft);
          expect(answered()).toEqual([]);
          expect(brokerAnswers()).toEqual([]);
          expect(questionAnswers()).toEqual([]);
          expect(bandMessage(container)).toBe(CTRL_Y_GONE_FLASH);
          expect(modeOf(container)).toBe("input");
        });
      }
    }
  }

  /* The dock's own text field ("Type something") is a text field outside the composer: the page claims Ctrl+y there
     too, so the field never acts on it, and nothing is answered. */
  it.each([[1], [0]] as const)("with %i cards waiting, Ctrl+y in the question's Type something field is claimed and answers nothing", (cards) => {
    // The question's own draft already holds typed text, so the dock draws its field.
    const draft = { q1: { page: 0, focus: 2, answers: [{ options: [], other: true, text: "pg" }] } };
    const { container } = started(cards, [{ ...TAB1, questionDraft: draft }], cardEvents(cards), [], { questions: [question] });
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "i" });
    wait(TYPING_GUARD_MS + 50);
    const field = container.querySelector<HTMLTextAreaElement>('.question-dock textarea[aria-label="Type something"]');
    expect(field).not.toBeNull();
    act(() => field!.focus());
    expect(field!.value).toBe("pg");
    wait(TYPING_GUARD_MS + 50);
    expect(tapY(field!), "claimed").toBe(false);
    wait(1000);
    expect(field!.value).toBe("pg");
    expect(document.activeElement).toBe(field);
    expect(answered()).toEqual([]);
    expect(brokerAnswers()).toEqual([]);
    expect(questionAnswers()).toEqual([]);
    expect(bandMessage(container)).toBe(CTRL_Y_GONE_FLASH);
  });

  it("Caps Lock's Ctrl+y, arriving as key Y with no Shift, is the same chord: claimed, nothing answered", () => {
    const { container, box } = inInput(1);
    expect(tapY(box, { key: "Y" })).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(bandMessage(container)).toBe(CTRL_Y_GONE_FLASH);
  });

  it("a card Approve cannot answer gets the same flash, not its own", () => {
    const { container } = started(0, [TAB1], acpCard(false));
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "i" });
    wait(TYPING_GUARD_MS + 50);
    expect(tapY(textarea(container)!)).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(bandMessage(container)).toBe(CTRL_Y_GONE_FLASH);
  });

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
  ])("Ctrl+y with %s is another chord: not claimed by the panel, and nothing answered", (_name, init) => {
    const { container, box } = inInput(1);
    tapY(box, init);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(bandMessage(container)).not.toBe(CTRL_Y_GONE_FLASH);
  });

  it("Ctrl+y in the Ctrl+r history search is claimed the same way and answers nothing", () => {
    const { container, box } = inInput(1);
    fireEvent.keyDown(box, { key: "r", ctrlKey: true });
    const search = container.querySelector<HTMLInputElement>(".history-search input");
    expect(search).not.toBeNull();
    act(() => search!.focus());
    wait(TYPING_GUARD_MS + 50);
    expect(fireEvent.keyDown(search!, { key: "y", ctrlKey: true })).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(bandMessage(container)).toBe(CTRL_Y_GONE_FLASH);
  });

  it("the band names no Ctrl+y while cards wait in INPUT, and none in BROWSE", () => {
    const { container } = started(2);
    expect(bandText(container)).not.toContain("Ctrl+y");
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "i" });
    expect(bandText(container)).not.toContain("Ctrl+y");
  });

  it("the ? overlay's Typing section says Ctrl+y does nothing and where cards are", () => {
    const { container, box } = inInput(1);
    fireEvent.keyDown(box, { key: "?", shiftKey: true });
    const overlay = container.querySelector<HTMLElement>(".keymap-overlay");
    expect(overlay).not.toBeNull();
    const typing = Array.from(overlay!.querySelectorAll("section")).find(
      (s) => s.querySelector("h2")?.textContent === "Typing",
    )!;
    const rows = Array.from(typing.querySelectorAll("tr")).map((tr) => tr.textContent ?? "");
    expect(rows.some((r) => r.startsWith("Ctrl+y") && r.includes("nothing") && r.includes("Alt-n"))).toBe(true);
    expect(rows.some((r) => /oldest/.test(r))).toBe(false);
  });

  it("on the card itself, BROWSE Ctrl+y is still the scroll and answers nothing", () => {
    const { container } = started(1);
    expect(container.querySelector(".row-current")!.classList.contains("row-permission")).toBe(true);
    wait(TYPING_GUARD_MS + 50);
    expect(fireEvent.keyDown(root(container), { key: "y", ctrlKey: true })).toBe(false);
    wait(1000);
    expect(answered()).toEqual([]);
    expect(bandMessage(container)).not.toBe(CTRL_Y_GONE_FLASH);
    expect(container.querySelector('[data-testid="mode-block"]')!.textContent).toBe("BROWSE");
  });

  it("keeps an in-flight card inert across a tab switch and restores it on failure", () => {
    const { container } = started(1, [TAB1, TAB2]);
    fireEvent.click(container.querySelector<HTMLButtonElement>('[data-nav-action="allow"]')!, { detail: 1 });
    const request = answered()[0];
    expect(request).toBeDefined();
    dispatch({ kind: "tabs", active: 2, tabs: [TAB1, TAB2] });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 0, state: snapshotState() });
    dispatch({ kind: "tabs", active: 1, tabs: [TAB1, TAB2] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: cardEvents(1).reduce(applyEvent, snapshotState()) });
    expect(container.textContent).toContain("Sending answer…");
    expect(container.querySelector<HTMLButtonElement>('[data-nav-action="allow"]')!.disabled).toBe(true);
    dispatch({ kind: "command_result", requestId: request.request_id, ok: false, error: "answer failed" });
    expect(container.textContent).not.toContain("Sending answer…");
    expect(container.querySelector<HTMLButtonElement>('[data-nav-action="allow"]')!.disabled).toBe(false);
  });
});

/** A card Approve cannot answer, in card mode: its 1 says why and sends nothing; 2 still denies. */
describe("a card Approve cannot answer", () => {
  beforeEach(() => {
    vi.useFakeTimers({
      toFake: [
        "setTimeout", "clearTimeout", "setInterval", "clearInterval", "setImmediate", "clearImmediate", "Date", "performance",
        "requestAnimationFrame", "cancelAnimationFrame",
      ],
    });
    widen = stubBandWidth();
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("1 on it in card mode flashes why and sends nothing; 2 still denies; a answers nothing", () => {
    const { container } = started(1, [TAB1], acpCard(false));
    wait(TYPING_GUARD_MS + 50);
    fireEvent.keyDown(root(container), { key: "a" });
    wait(1000);
    expect(answered()).toEqual([]);
    fireEvent.keyDown(root(container), { key: "Enter" });
    expect(container.querySelector(".permission-card-mode")).not.toBeNull();
    wait(CARD_ARM_MS + 100);
    fireEvent.keyDown(root(container), { key: "1" });
    expect(bandMessage(container)).toContain("cannot be approved");
    wait(1000);
    expect(answered()).toEqual([]);
    fireEvent.keyDown(root(container), { key: "2" });
    wait(1000);
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "acp-1", decision: "deny" })]);
  });
});
