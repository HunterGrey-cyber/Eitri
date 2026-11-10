// @vitest-environment jsdom
/**
 * Card mode in the page: how a card takes it (`Alt+n`/`Alt+p` from BROWSE, the box and CARET, a lone Enter on the
 * card's row or the tool row it gates), what its keys do (an armed, fresh digit or Enter answers, through the
 * after-half; the reason field; leaving), and what ends it (an overlay, a tab switch, the card going). Every card
 * kind: a provider's card without and with a saved-rule offer, a broker card, an ACP card and a Codex card. Card mode
 * is the only keyboard way to answer a card: the old keys (`a`, `d`, `D`, `]p`, `[p`, Enter on a card's button) answer
 * nothing, and their cases are in `App.test.tsx`'s "typing never answers a card".
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, onTestFinished, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { hintTargets } from "./nav";
import { TABLE } from "./testFixtures";
import { TYPING_GUARD_MS } from "./typingGuard";
import { CARD_ARM_MS } from "./cardMode";
import {
  ANSWER_CANCELLED_FLASH,
  BROKER_NO_REASON_FLASH,
  CARD_WITHDRAWN_FLASH,
  COUNT_DROPPED_FLASH,
  ENTER_ALONE_FLASH,
  NO_CARD_WAITING_FLASH,
  NO_OLDER_JUMP_FLASH,
  REPLAYED_KEY_FLASH,
  SHIFT_TAB_IN_CARD_FLASH,
  cardArmingFlash,
  heldAnswerFlash,
  noButtonPickedFlash,
  notApprovableFlash,
  agentNoReasonFlash,
  moreWaitingFlash,
  pendingDroppedFlash,
} from "./app/flashes";
import type { AgentDomainEvent, AgentUiState, EditorCardWire, Hello } from "./types";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

let posted: Array<Record<string, unknown>>;
let widen: (container: HTMLElement) => void;

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  // The paint that arms card mode comes from animation frames; the keys' stamps and the guard's clock from Date and
  // performance. All of them move together here.
  vi.useFakeTimers({
    toFake: ["Date", "performance", "setTimeout", "clearTimeout", "setInterval", "clearInterval", "requestAnimationFrame", "cancelAnimationFrame"],
  });
  widen = stubBandWidth();
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

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

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  account: null,
};

const TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

type Kind = "provider" | "provider with a rule" | "broker" | "ACP" | "Codex";
const KINDS: Kind[] = ["provider", "provider with a rule", "broker", "ACP", "Codex"];

/** The card's answer list as card mode reads it: a rule offer adds Always allow, second, before Deny. */
const answersOf = (kind: Kind) => (kind === "provider with a rule" ? 3 : 2);
/** The digit that denies: the card's last, 3 beside Always allow and 2 elsewhere. */
const denyDigitOf = (kind: Kind) => String(answersOf(kind));
/** Whether the card has a tool row it gates (a broker card here names no tool call, a Codex card no tool row). */
const gatesARow = (kind: Kind) => kind === "provider" || kind === "provider with a rule" || kind === "ACP";

function toolCard(id: string, toolUseId: string, name: string, input: unknown): AgentDomainEvent[] {
  return [
    { type: "tool_call_started", turn_id: "t1", tool_use_id: toolUseId, name, input },
    { type: "permission_requested", permission_id: id, tool_use_id: toolUseId, tool_name: name, input },
  ];
}

function brokerCard(id: string, seq: number): EditorCardWire {
  return { id, seq, tool: "open_file", toolUseId: null, title: "Show /etc/hosts in your editor?", body: [{ kind: "code", text: "/etc/hosts" }] };
}

const ACP_INPUT = {
  title: "Edit notes.txt",
  options: [{ kind: "allow_once", name: "Allow" }, { kind: "reject_once", name: "Reject" }],
  diffs: [],
  locations: [{ path: "/work/notes.txt" }],
};

/** The id of the (first) card each kind waits with. */
const cardId = (kind: Kind) =>
  kind === "broker" ? "ec-0123456789abcdef-1" : kind === "ACP" ? "acp-1" : kind === "Codex" ? "perm-command" : "perm-1";

type Setup = { kind?: Kind; second?: boolean; turnRunning?: boolean };

/** Tab 1 live, a turn running with a reply under way, one card of `kind` waiting (`second`: a second provider card
 *  after it), the pane focused, the keys arrived (BROWSE on the card), the band wide, and a pause past the guard. */
function started({ kind = "provider", second = false }: Setup = {}) {
  const tab = kind === "ACP" ? { ...TAB, agent: "acp:goose" } : kind === "Codex" ? { ...TAB, agent: "codex" } : TAB;
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [tab] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: { ...initialState(), status: { kind: "running" } } as AgentUiState });
  const events: AgentDomainEvent[] = [
    { type: "user_prompt_submitted", text: "tidy up" },
    { type: "turn_started", turn_id: "t1" },
    { type: "content_delta", turn_id: "t1", kind: "text", text: "looking" },
  ];
  if (kind === "provider" || kind === "provider with a rule") events.push(...toolCard("perm-1", "toolu_1", "Bash", { command: "rm build" }));
  if (kind === "ACP") events.push(...toolCard("acp-1", "c1", "acp:edit", ACP_INPUT));
  if (kind === "Codex") {
    events.push({
      type: "permission_requested", permission_id: "perm-command", tool_use_id: "item-command", tool_name: "commandExecution",
      input: { command: "/bin/bash -lc 'git push'", cwd: "/project", escalation: "", network: null, reason: "" },
    });
  }
  if (second) events.push(...toolCard("perm-2", "toolu_2", "Bash", { command: "git push" }));
  dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: events.length, events });
  if (kind === "broker") dispatch({ kind: "editor_cards", tab: 1, cards: [brokerCard(cardId("broker"), 3)] });
  if (kind === "provider with a rule") dispatch({ kind: "rule_offers", tab: 1, offers: { "perm-1": "Bash(rm *)" } });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  wait(TYPING_GUARD_MS + 50);
  return rendered;
}

const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));
/** Past the paint that draws card mode and its arming. */
const arm = () => wait(CARD_ARM_MS + 100);
/** Past an answer's after-half. */
const afterHalf = () => wait(TYPING_GUARD_MS + 20);
const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
const inCardMode = (c: HTMLElement) => c.querySelector(".permission-card-mode");
const highlighted = (c: HTMLElement) => c.querySelectorAll(".permission-card-mode .card-highlight").length;
const band = (c: HTMLElement) => c.querySelector(".band-message")?.textContent ?? null;
const modeOf = (c: HTMLElement) => c.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode") ?? null;
const answers = () => posted.filter((m) => m.type === "permission_response" || m.type === "editor_card_answer");
const currentRowKind = (c: HTMLElement) => c.querySelector(".row-current")?.className ?? "";
/** A key on whatever has the keys now. Returns whether its default survived. */
const press = (key: string, init: Record<string, unknown> = {}) =>
  fireEvent.keyDown(document.activeElement ?? document.body, { key, ...init });
const altN = () => press("n", { altKey: true, code: "KeyN" });
const altP = () => press("p", { altKey: true, code: "KeyP" });

/** What an answer of `kind`'s card posted: its decision, `remember`, and its reason. */
function answerOf(kind: Kind) {
  const [m] = answers();
  if (m === undefined) return null;
  if (kind === "broker") return { decision: m.decision, origin: m.origin };
  return { decision: m.decision, remember: m.remember ?? false, reason: m.reason ?? null };
}

/** `started`, then card mode by `Alt+n` from BROWSE, armed. */
function armedCard(setup: Setup = {}) {
  const rendered = started(setup);
  altN();
  expect(inCardMode(rendered.container)).not.toBeNull();
  arm();
  return rendered;
}

describe.each(KINDS)("card mode on a %s card", (kind) => {
  it("Alt+n and Alt+p from BROWSE enter it, with nothing highlighted", () => {
    const { container } = started({ kind });
    expect(altN()).toBe(false);
    expect(inCardMode(container)).not.toBeNull();
    expect(highlighted(container)).toBe(0);
    expect(document.activeElement).toBe(root(container));
    press("Escape");
    expect(inCardMode(container)).toBeNull();
    wait(TYPING_GUARD_MS + 50);
    expect(altP()).toBe(false);
    expect(inCardMode(container)).not.toBeNull();
    expect(highlighted(container)).toBe(0);
    expect(answers()).toEqual([]);
  });

  it("Alt+n from the box enters it, and Esc goes back to the box with the caret where it was", () => {
    const { container } = started({ kind });
    press("i");
    wait(TYPING_GUARD_MS + 50);
    const box = container.querySelector<HTMLTextAreaElement>(".composer textarea")!;
    expect(document.activeElement).toBe(box);
    fireEvent.change(box, { target: { value: "draft text" } });
    box.setSelectionRange(3, 3);
    fireEvent.keyUp(box, { key: "ArrowLeft" });
    wait(TYPING_GUARD_MS + 50);
    expect(altN()).toBe(false);
    expect(inCardMode(container)).not.toBeNull();
    expect(highlighted(container)).toBe(0);
    expect(document.activeElement).toBe(root(container));
    arm();
    press("Escape");
    wait(50);
    expect(inCardMode(container)).toBeNull();
    expect(modeOf(container)).toBe("input");
    const back = container.querySelector<HTMLTextAreaElement>(".composer textarea")!;
    expect(document.activeElement).toBe(back);
    expect(back.value).toBe("draft text");
    expect([back.selectionStart, back.selectionEnd]).toEqual([3, 3]);
    expect(answers()).toEqual([]);
  });

  it("g then Alt+n and 3 then Alt+n enter it and say what they dropped", () => {
    const { container } = started({ kind });
    press("g");
    altN();
    expect(inCardMode(container)).not.toBeNull();
    expect(band(container)).toBe(pendingDroppedFlash("g"));
    press("Escape");
    wait(TYPING_GUARD_MS + 50);
    press("3");
    altN();
    expect(inCardMode(container)).not.toBeNull();
    expect(band(container)).toBe(COUNT_DROPPED_FLASH);
  });

  it("a lone Enter on the card row enters it; an Enter right after another key does not", () => {
    const { container } = started({ kind });
    expect(currentRowKind(container)).toContain("row-permission");
    press("Enter");
    expect(inCardMode(container)).not.toBeNull();
    expect(highlighted(container)).toBe(0);
    press("Escape");
    wait(TYPING_GUARD_MS + 50);
    press("x");
    press("Enter");
    expect(inCardMode(container)).toBeNull();
    expect(band(container)).toBe(ENTER_ALONE_FLASH);
    expect(answers()).toEqual([]);
  });

  it("Enter on one of its buttons holding DOM focus (a mouse press dragged off it) enters it, nothing highlighted", () => {
    const { container } = started({ kind });
    const approve = container.querySelector<HTMLButtonElement>('.permission-card [data-nav-action="allow"]')!;
    act(() => approve.focus());
    wait(TYPING_GUARD_MS + 50);
    expect(fireEvent.keyDown(approve, { key: "Enter" })).toBe(false);
    expect(inCardMode(container)).not.toBeNull();
    expect(highlighted(container)).toBe(0);
    expect(document.activeElement).toBe(root(container));
    // Space there does nothing, card mode or not.
    press("Escape");
    wait(TYPING_GUARD_MS + 50);
    act(() => approve.focus());
    expect(fireEvent.keyDown(approve, { key: " " })).toBe(false);
    expect(inCardMode(container)).toBeNull();
    wait(1000);
    expect(answers()).toEqual([]);
  });

  it("a HINT label on one of its buttons enters it, nothing highlighted, the keys on the root", () => {
    const { container } = started({ kind });
    // jsdom lays nothing out: every stop, control and code block gets a 10px box of its own.
    const box = (el: Element, top: number, height = 10) => {
      (el as HTMLElement).getBoundingClientRect = () =>
        ({ top, bottom: top + height, left: 0, right: 100, width: 100, height, x: 0, y: top }) as DOMRect;
    };
    box(container.querySelector(".agent-ui-root") ?? root(container), 0, 1000);
    box(root(container), 0, 1000);
    box(container.querySelector(".message-list")!, 0, 1000);
    let y = 0;
    for (const el of container.querySelectorAll("[data-nav-stop], button, input, pre.code-block, .row-sign")) {
      box(el, y);
      y += 10;
    }
    const deny = container.querySelector<HTMLButtonElement>('.permission-card [data-nav-action="deny"]')!;
    const index = hintTargets(root(container)).findIndex((t) => t.el === deny);
    expect(index).toBeGreaterThanOrEqual(0);
    dispatch({ kind: "hint_collect", sessionId: 1 });
    dispatch({ kind: "hint_land", sessionId: 1, index });
    expect(inCardMode(container)?.contains(deny)).toBe(true);
    expect(highlighted(container)).toBe(0);
    expect(document.activeElement).toBe(root(container));
    arm();
    press(denyDigitOf(kind));
    afterHalf();
    expect(answerOf(kind)).toMatchObject({ decision: "deny" });
  });

  if (gatesARow(kind)) {
    it("a lone Enter on the tool row the card gates enters it", () => {
      const { container } = started({ kind });
      press("k");
      wait(TYPING_GUARD_MS + 50);
      expect(currentRowKind(container)).toContain("row-tool");
      press("Enter");
      expect(inCardMode(container)).not.toBeNull();
      expect(container.querySelector(".tasks-overlay")).toBeNull();
    });
  }

  it("Alt+n from CARET ends the region and enters it", () => {
    // jsdom has no `Selection.modify`, which CARET's block caret extends by one character with: one character within
    // the focus's own text node is enough for the region to start.
    const proto = Selection.prototype as unknown as { modify?: (alter: string, direction: string) => void };
    const original = proto.modify;
    proto.modify = function (this: Selection, alter: string, direction: string) {
      const node = this.focusNode;
      if (node === null) return;
      const length = (node.textContent ?? "").length;
      const offset = direction === "forward" ? Math.min(length, this.focusOffset + 1) : Math.max(0, this.focusOffset - 1);
      if (alter === "move") this.collapse(node, offset);
      else this.setBaseAndExtent(this.anchorNode ?? node, this.anchorOffset, node, offset);
    };
    onTestFinished(() => {
      if (original === undefined) delete proto.modify;
      else proto.modify = original;
    });
    const { container } = started({ kind });
    press("k");
    wait(TYPING_GUARD_MS + 50);
    press("v");
    wait(TYPING_GUARD_MS + 50);
    expect(modeOf(container)).toBe("caret");
    altN();
    expect(inCardMode(container)).not.toBeNull();
    expect(modeOf(container)).toBe("browse");
  });

  it("an armed 1 approves and the card's last digit denies (3 beside Always allow, 2 elsewhere), each after its after-half", () => {
    const { container } = armedCard({ kind });
    expect(container.querySelectorAll(".card-digit.armed").length).toBe(answersOf(kind));
    press("1");
    expect(answers()).toEqual([]);
    afterHalf();
    expect(answerOf(kind)).toMatchObject({ decision: "allow" });
    if (kind === "broker") expect(answerOf(kind)).toMatchObject({ origin: "key" });
    expect(inCardMode(container)).toBeNull();
    cleanup();
    posted = [];
    const again = armedCard({ kind });
    const deny = again.container.querySelector('.permission-card-mode [data-nav-action="deny"]')!;
    expect(deny.querySelector(".card-digit")!.textContent).toBe(denyDigitOf(kind));
    press(denyDigitOf(kind));
    afterHalf();
    expect(answerOf(kind)).toMatchObject({ decision: "deny" });
    expect(inCardMode(again.container)).toBeNull();
  });

  it("2 is Always allow where the card offers a rule (Claude Code's order); elsewhere 2 denies and there is no 3", () => {
    const { container } = armedCard({ kind });
    const digits = Array.from(container.querySelectorAll(".permission-card-mode .permission-card-buttons button")).map(
      (b) => b.textContent,
    );
    if (kind === "provider with a rule") {
      expect(digits.map((t) => t?.slice(0, 1))).toEqual(["1", "2", "3"]);
      expect(digits[1]).toContain("Always allow");
      expect(digits[2]).toContain("Deny");
      press("2");
      afterHalf();
      expect(answerOf(kind)).toEqual({ decision: "allow", remember: true, reason: null });
    } else {
      expect(digits.map((t) => t?.slice(0, 1))).toEqual(["1", "2"]);
      expect(digits[1]).toContain("Deny");
      press("3");
      afterHalf();
      expect(answers()).toEqual([]);
      expect(band(container)).toBe(`3: this card has 2 answers`);
      expect(inCardMode(container)).not.toBeNull();
    }
  });

  it("l, a pause, then Enter approves; Enter with nothing highlighted refuses", () => {
    const { container } = armedCard({ kind });
    press("Enter");
    afterHalf();
    expect(answers()).toEqual([]);
    expect(band(container)).toBe(noButtonPickedFlash(answersOf(kind)));
    press("l");
    expect(highlighted(container)).toBe(1);
    expect(container.querySelector(".permission-card-mode .card-highlight")!.textContent).toContain("Approve");
    arm();
    press("Enter");
    afterHalf();
    expect(answerOf(kind)).toMatchObject({ decision: "allow" });
  });

  if (kind === "provider" || kind === "provider with a rule") {
    it("Tab opens the reason field: typed text is the field's, Enter at once refuses, Enter after a pause denies with it", () => {
      const { container } = armedCard({ kind });
      expect(press("Tab")).toBe(false);
      const field = container.querySelector<HTMLInputElement>(".permission-card-mode input")!;
      expect(document.activeElement).toBe(field);
      for (const letter of "because") expect(fireEvent.keyDown(field, { key: letter }), letter).toBe(true);
      fireEvent.change(field, { target: { value: "because" } });
      expect(press("Enter")).toBe(false);
      afterHalf();
      expect(answers()).toEqual([]);
      expect(band(container)).toBe(cardArmingFlash("Enter"));
      arm();
      press("Enter");
      expect(answers()).toEqual([]);
      afterHalf();
      expect(answerOf(kind)).toEqual({ decision: "deny", remember: false, reason: "because" });
    });

    it("Esc closes the reason field with card mode and the text kept; an input method's Escape keeps it open", () => {
      const { container } = armedCard({ kind });
      press("Tab");
      const field = container.querySelector<HTMLInputElement>(".permission-card-mode input")!;
      fireEvent.change(field, { target: { value: "not this" } });
      expect(fireEvent.keyDown(field, { key: "Escape", keyCode: 229 })).toBe(true);
      expect(document.activeElement).toBe(field);
      expect(fireEvent.keyDown(field, { key: "Escape", isComposing: true })).toBe(true);
      expect(document.activeElement).toBe(field);
      press("Escape");
      expect(document.activeElement).toBe(root(container));
      expect(inCardMode(container)).not.toBeNull();
      expect(field.value).toBe("not this");
      // Ctrl+c typed into the field is the field's: no interrupt, card mode stays.
      press("Tab");
      expect(document.activeElement).toBe(field);
      expect(fireEvent.keyDown(field, { key: "c", ctrlKey: true })).toBe(true);
      expect(posted.filter((m) => m.type === "interrupt")).toEqual([]);
      expect(inCardMode(container)).not.toBeNull();
      expect(answers()).toEqual([]);
    });
  } else {
    it("Tab is refused: this card's deny takes no reason", () => {
      const { container } = armedCard({ kind });
      expect(press("Tab")).toBe(false);
      expect(band(container)).toBe(
        kind === "broker" ? BROKER_NO_REASON_FLASH : agentNoReasonFlash(kind === "ACP" ? "goose" : "Codex", 2),
      );
      expect(container.querySelector(".permission-card-mode input")).toBeNull();
      expect(inCardMode(container)).not.toBeNull();
    });
  }
});

describe("card mode answers only an armed, fresh press with nothing after it", () => {
  it("a digit pressed before card mode is armed answers nothing and says so", () => {
    const { container } = started();
    altN();
    wait(100);
    press("1");
    afterHalf();
    arm();
    expect(answers()).toEqual([]);
    expect(band(container)).toBe(cardArmingFlash("1"));
    expect(inCardMode(container)).not.toBeNull();
  });

  it("a held digit answers nothing", () => {
    const { container } = armedCard();
    press("1", { repeat: true });
    afterHalf();
    expect(answers()).toEqual([]);
    expect(band(container)).toBe(heldAnswerFlash("1"));
  });

  it("a digit queued in a stall (pressed before the paint, handled after it) answers nothing", () => {
    const { container } = started();
    altN();
    const pressed = Date.now();
    arm();
    const event = new KeyboardEvent("keydown", { key: "1", bubbles: true, cancelable: true });
    Object.defineProperty(event, "timeStamp", { value: pressed + 5 });
    act(() => {
      root(container).dispatchEvent(event);
    });
    afterHalf();
    expect(answers()).toEqual([]);
    expect(band(container)).toBe(cardArmingFlash("1"));
  });

  it("a digit followed within the after-half by any key answers nothing", () => {
    const { container } = armedCard();
    press("1");
    wait(100);
    press(".");
    afterHalf();
    expect(answers()).toEqual([]);
    expect(band(container)).toBe(ANSWER_CANCELLED_FLASH);
    // "1. do x" typed straight on answers nothing either.
    arm();
    for (const k of ["1", ".", " ", "d", "o", " ", "x"]) {
      press(k);
      wait(60);
    }
    afterHalf();
    expect(answers()).toEqual([]);
  });

  it("a card changed between the press and the end of its after-half is not answered", () => {
    // The cursor is on the older card, so Alt+n takes the one after it, perm-2.
    const { container } = armedCard({ second: true });
    press("1");
    wait(100);
    altN();
    afterHalf();
    expect(answers()).toEqual([]);
    expect(inCardMode(container)).not.toBeNull();
    // The next card round the ring holds it now, perm-1; a fresh armed press answers that one, never perm-2.
    arm();
    press("2");
    afterHalf();
    expect(answers()).toHaveLength(1);
    expect(answers()[0]).toMatchObject({ permission_id: "perm-1", decision: "deny" });
  });

  it("a card withdrawn during the after-half is not answered, and card mode ends saying so", () => {
    const { container } = armedCard();
    press("1");
    wait(100);
    dispatch({ kind: "events", tab: 1, fromRevision: 10, throughRevision: 11, events: [{ type: "permission_resolved", permission_id: "perm-1", outcome: "cancelled_by_interrupt" }] });
    afterHalf();
    expect(answers()).toEqual([]);
    expect(inCardMode(container)).toBeNull();
    expect(band(container)).toBe(CARD_WITHDRAWN_FLASH);
  });

  it("an arrival during the after-half voids the answer; card mode stays and arms again from it", () => {
    const { container } = armedCard();
    press("1");
    wait(100);
    dispatch({ kind: "arrive" });
    afterHalf();
    expect(answers()).toEqual([]);
    expect(inCardMode(container)).not.toBeNull();
    press("1");
    afterHalf();
    expect(answers()).toEqual([]);
    expect(band(container)).toBe(cardArmingFlash("1"));
    arm();
    press("1");
    afterHalf();
    expect(answers()).toHaveLength(1);
  });

  it("a digit replayed on the root after a focus loss refuses, and a fresh one then answers", () => {
    const { container } = armedCard();
    act(() => (document.activeElement as HTMLElement).blur());
    expect(document.activeElement).toBe(document.body);
    fireEvent.keyDown(document.body, { key: "1", code: "Digit1" });
    afterHalf();
    expect(answers()).toEqual([]);
    expect(band(container)).toBe(REPLAYED_KEY_FLASH);
    expect(document.activeElement).toBe(root(container));
    arm();
    press("1");
    afterHalf();
    expect(answers()).toHaveLength(1);
  });

  it("an ACP card that cannot be approved refuses 1 and still denies with 2", () => {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, agent: "acp:goose" }] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: { ...initialState(), status: { kind: "running" } } as AgentUiState });
    const input = { ...ACP_INPUT, options: [{ kind: "reject_once", name: "Reject" }], inputCut: true };
    const events: AgentDomainEvent[] = [{ type: "turn_started", turn_id: "t1" }, ...toolCard("acp-1", "c1", "acp:edit", input)];
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: events.length, events });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    act(() => widen(rendered.container));
    wait(TYPING_GUARD_MS + 50);
    altN();
    arm();
    press("1");
    afterHalf();
    expect(answers()).toEqual([]);
    expect(band(rendered.container)).toBe(notApprovableFlash("1", 2));
    arm();
    press("2");
    afterHalf();
    expect(answers()[0]).toMatchObject({ permission_id: "acp-1", decision: "deny" });
  });
});

describe("card mode ends when something else takes the keys", () => {
  it("the chooser opening ends it", () => {
    const { container } = armedCard();
    dispatch({ kind: "chooser", open: [], records: [] });
    expect(inCardMode(container)).toBeNull();
  });

  it("a tab switch ends it", () => {
    const { container } = armedCard();
    dispatch({ kind: "tabs", active: 2, tabs: [TAB, { ...TAB, id: 2, number: 2, label: "2 new" }] });
    expect(inCardMode(container)).toBeNull();
  });

  it("an arrival of another tab ends it; one of the same tab keeps it", () => {
    const { container } = armedCard();
    dispatch({ kind: "arrive" });
    expect(inCardMode(container)).not.toBeNull();
    dispatch({ kind: "tabs", active: 2, tabs: [TAB, { ...TAB, id: 2, number: 2, label: "2 new" }] });
    dispatch({ kind: "arrive" });
    expect(inCardMode(container)).toBeNull();
  });

  it("the pane losing focus ends it", () => {
    const { container } = armedCard();
    dispatch({ kind: "pane_focus", focused: false });
    expect(inCardMode(container)).toBeNull();
  });

  it("Shift+Tab is refused, and cycles no mode", () => {
    const { container } = armedCard();
    expect(press("Tab", { shiftKey: true })).toBe(false);
    expect(band(container)).toBe(SHIFT_TAB_IN_CARD_FLASH);
    expect(posted.filter((m) => m.type === "cycle_mode")).toEqual([]);
    expect(inCardMode(container)).not.toBeNull();
    // It counts as a key: a digit at once is unarmed.
    press("1");
    afterHalf();
    expect(answers()).toEqual([]);
  });

  it("Ctrl+c leaves with the card unanswered and interrupts nothing", () => {
    const { container } = armedCard();
    expect(press("c", { ctrlKey: true })).toBe(false);
    expect(inCardMode(container)).toBeNull();
    expect(posted.filter((m) => m.type === "interrupt")).toEqual([]);
    expect(answers()).toEqual([]);
  });

  it("Esc goes back to BROWSE on the row it was entered from", () => {
    const { container } = started();
    press("k");
    wait(TYPING_GUARD_MS + 50);
    const origin = container.querySelector(".row-current")!.getAttribute("data-row-key");
    altN();
    expect(currentRowKind(container)).toContain("row-permission");
    arm();
    press("Escape");
    expect(inCardMode(container)).toBeNull();
    expect(modeOf(container)).toBe("browse");
    expect(container.querySelector(".row-current")!.getAttribute("data-row-key")).toBe(origin);
  });

  it("after an answer the band names the cards still waiting, and the cursor does not move to them", () => {
    const { container } = started({ second: true });
    const origin = container.querySelector(".row-current")!.getAttribute("data-row-key");
    press("Enter");
    arm();
    press("1");
    afterHalf();
    expect(answers()).toHaveLength(1);
    expect(inCardMode(container)).toBeNull();
    expect(band(container)).toBe(moreWaitingFlash(1));
    expect(container.querySelector(".row-current")!.getAttribute("data-row-key")).toBe(origin);
  });

  it("the row Alt+n entered it from is a jump: Ctrl+o returns there once card mode has ended in place", () => {
    const { container } = started({ second: true });
    press("k");
    wait(TYPING_GUARD_MS + 50);
    const origin = container.querySelector(".row-current")!.getAttribute("data-row-key");
    altN();
    const card = container.querySelector(".row-current")!.getAttribute("data-row-key");
    expect(card).not.toBe(origin);
    arm();
    // On to the other card: no second jump, the row first left stays the one to return to.
    altN();
    arm();
    altP();
    arm();
    press("F1");
    expect(inCardMode(container)).toBeNull();
    press("Escape");
    expect(container.querySelector(".keymap-overlay")).toBeNull();
    expect(container.querySelector(".row-current")!.getAttribute("data-row-key")).toBe(card);
    wait(TYPING_GUARD_MS + 50);
    press("o", { ctrlKey: true });
    expect(container.querySelector(".row-current")!.getAttribute("data-row-key")).toBe(origin);
    wait(TYPING_GUARD_MS + 50);
    press("i", { ctrlKey: true });
    expect(container.querySelector(".row-current")!.getAttribute("data-row-key")).toBe(card);
    wait(TYPING_GUARD_MS + 50);
    press("o", { ctrlKey: true });
    press("o", { ctrlKey: true });
    expect(container.querySelector(".row-current")!.getAttribute("data-row-key")).toBe(origin);
    expect(band(container)).toBe(NO_OLDER_JUMP_FLASH);
    expect(answers()).toEqual([]);
  });

  it("F1 opens the keys sheet at the card section and ends card mode", () => {
    const { container } = armedCard();
    press("F1");
    expect(inCardMode(container)).toBeNull();
    const sheet = container.querySelector(".keymap-overlay");
    expect(sheet).not.toBeNull();
    expect(Array.from(sheet!.querySelectorAll("h2")).map((h) => h.textContent)).toContain("Answering a card");
  });

  it("Alt+n from the question dock enters it, and Esc gives the keys back to the dock with the question untouched", () => {
    const { container } = started();
    dispatch({
      kind: "events", tab: 1, fromRevision: 5, throughRevision: 6,
      events: [{
        type: "question_asked", interaction_id: "q1", tool_use_id: null, unsupported_reason: null,
        questions: [{
          question: "Which database?", header: "Database", multi_select: false,
          options: [{ label: "Postgres", description: "Relational", preview: null }, { label: "SQLite", description: "One file", preview: null }],
        }],
      }],
    });
    wait(TYPING_GUARD_MS + 50);
    press("i");
    wait(TYPING_GUARD_MS + 50);
    const dock = container.querySelector<HTMLElement>(".question-dock")!;
    expect(dock.contains(document.activeElement)).toBe(true);
    expect(altN()).toBe(false);
    expect(inCardMode(container)).not.toBeNull();
    expect(document.activeElement).toBe(root(container));
    arm();
    press("Escape");
    wait(50);
    expect(inCardMode(container)).toBeNull();
    expect(container.querySelector(".question-dock")!.contains(document.activeElement)).toBe(true);
    expect(posted.filter((m) => m.type === "question_answer" || m.type === "question_decline")).toEqual([]);
    expect(answers()).toEqual([]);
  });

  it("Alt+n drops a leader sequence in flight and says so", () => {
    const { container } = started();
    dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: TABLE, newTabChord: "Ctrl+b c" });
    wait(TYPING_GUARD_MS + 50);
    press(" ");
    altN();
    expect(inCardMode(container)).not.toBeNull();
    expect(band(container)).toBe(pendingDroppedFlash("Space"));
    expect(container.querySelector(".which-key-box")).toBeNull();
    // The sequence is gone: a key that would have continued it is card mode's now.
    press("b");
    expect(posted.filter((m) => m.type === "tab_verb")).toEqual([]);
    expect(answers()).toEqual([]);
  });

  it("Alt+n with no card waiting says so", () => {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [TAB] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: { ...initialState(), status: { kind: "running" } } as AgentUiState });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "arrive" });
    act(() => widen(rendered.container));
    wait(TYPING_GUARD_MS + 50);
    altN();
    expect(inCardMode(rendered.container)).toBeNull();
    expect(band(rendered.container)).toBe(NO_CARD_WAITING_FLASH);
  });
});

describe("the band names card mode", () => {
  const bandText = (c: HTMLElement) => c.querySelector(".status-band")?.textContent ?? "";
  const activityText = (c: HTMLElement) => c.querySelector(".activity-card")?.textContent ?? "";
  const modeText = (c: HTMLElement) => c.querySelector('[data-testid="mode-block"]')?.textContent ?? "";

  it("the activity line offers the way in while a card waits; the band names the held card, its answers and CARD", () => {
    const { container } = started({ kind: "provider with a rule", second: true });
    expect(activityText(container)).toContain("card mode, then 1-3");
    expect(bandText(container)).not.toContain("card mode, then 1-3");
    expect(modeText(container)).toBe("BROWSE");
    const held = () => /card (\d)\/2 · 1-(\d) answer · h\/l Enter · Tab reason · Esc leaves/.exec(bandText(container));
    altN();
    arm();
    const first = held();
    expect(first).not.toBeNull();
    expect(modeText(container)).toBe("CARD");
    expect(activityText(container)).toMatch(/needs approval$/);
    altN();
    arm();
    const second = held();
    expect(second).not.toBeNull();
    // The two cards are 1/2 and 2/2; only the first has a saved-rule offer, so only it has three answers.
    expect(new Set([first![1], second![1]])).toEqual(new Set(["1", "2"]));
    expect(first![2]).toBe(first![1] === "1" ? "3" : "2");
    expect(second![2]).toBe(second![1] === "1" ? "3" : "2");
    press("Escape");
    expect(bandText(container)).not.toContain("Esc leaves");
    expect(modeText(container)).toBe("BROWSE");
    expect(activityText(container)).toContain("card mode, then 1-3");
  });

  it("counts a card's own two answers, and names Tab only where a deny takes a reason", () => {
    const { container } = armedCard({ kind: "ACP" });
    expect(bandText(container)).toContain("card 1/1 · 1-2 answer · h/l Enter · Esc leaves");
    expect(bandText(container)).not.toContain("Tab");
  });

  it("with the reason field open, says what Enter and Esc do there, and goes back on Esc", () => {
    const { container } = armedCard();
    press("Tab");
    expect(bandText(container)).toContain("card 1/1 · Enter denies with this reason · Esc back, text kept");
    expect(bandText(container)).not.toContain("1-2 answer");
    press("Escape");
    expect(inCardMode(container)).not.toBeNull();
    expect(bandText(container)).toContain("card 1/1 · 1-2 answer · h/l Enter · Tab reason · Esc leaves");
  });

  it("the activity line names Enter when the cursor is on the card, and Alt-n elsewhere", () => {
    const { container } = started();
    // `started` lands on the card's row.
    expect(activityText(container)).toContain("Enter or Alt-n: card mode, then 1-3");
    press("g");
    press("g");
    expect(activityText(container)).toContain("Alt-n: card mode, then 1-3");
    expect(activityText(container)).not.toContain("Enter or Alt-n");
  });
});
