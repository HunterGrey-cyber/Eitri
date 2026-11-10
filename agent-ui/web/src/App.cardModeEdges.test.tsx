// @vitest-environment jsdom
/// <reference types="vite/client" />
/* Card mode's edges on the page: the reason field never answers by itself, leaving takes the keys back from it, a held
   leaving key stays inside, an answer cancelled by something that is not a key says so, a deny by digit carries the
   field's kept text, a changed offer ends card mode, a held Alt-n moves nothing, and what an arrival, a focus move and
   a tab switch do to it. */
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { initialState } from "./reducer";
import { TYPING_GUARD_MS } from "./typingGuard";
import { CARD_ARM_MS } from "./cardMode";
import { ANSWER_INTERRUPTED_FLASH, cardArmingFlash, CARD_CHANGED_FLASH, REASON_ENTER_OUTSIDE_FLASH } from "./app/flashes";

let messages: Record<string, unknown>[];
beforeEach(() => {
  vi.useFakeTimers({
    toFake: ["Date", "performance", "setTimeout", "clearTimeout", "setInterval", "clearInterval", "requestAnimationFrame", "cancelAnimationFrame"],
  });
  messages = [];
  window.webkit = { messageHandlers: { eitriAgent: { postMessage: (m) => messages.push(JSON.parse(m)) } } };
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

/** jsdom has no layout, so the band shows only mode and pill until something reports a width (the same
 *  `ResizeObserver` stub `App.test.tsx`'s `stubBandWidth` uses). Call before the render. */
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
        const width = el === band ? 1400 : el === measure ? 7.2 : null;
        if (width !== null) o.callback([{ target: el, contentRect: { width } } as unknown as ResizeObserverEntry], {} as ResizeObserver);
      }
    }
  };
}

function dispatch(payload: unknown) {
  act(() => window.__eitriDispatch!(JSON.stringify(payload)));
}
const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));
/** Every answer the page posted, of either kind: a provider's and a broker card's. */
const answers = () => messages.filter((m) => m.type === "permission_response" || m.type === "editor_card_answer");
const band = (container: HTMLElement) => container.querySelector(".band-message")?.textContent ?? null;

const TAB = { id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 1, resumable: false, failure: null, title: null };
const BROKER_ID = "ec-0123456789abcdef-1";

/** A provider card waiting (`p1`, with Rust's rule offer, so it has all three buttons and a reason box), or a broker
 *  card (two buttons, no box), the keys arrived on it in BROWSE. */
function setup(kind: "provider" | "broker" | "two" = "provider") {
  const widen = stubBandWidth();
  const view = render(<App />);
  dispatch({ kind: "hello", backend: "legacy", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [TAB] });
  const first = { seq: 1, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "rm build" } };
  const second = { seq: 2, permissionId: "p2", toolUseId: null, toolName: "Bash", input: { command: "rm dist" } };
  const pending = kind === "provider" ? [first] : kind === "two" ? [first, second] : [];
  dispatch({
    kind: "snapshot", tab: 1, throughRevision: 3,
    state: { ...initialState(), status: { kind: "running" }, activeTurnId: "t1", pendingPermissions: pending },
  });
  if (kind !== "broker") dispatch({ kind: "rule_offers", tab: 1, offers: { p1: "rm *" } });
  if (kind === "broker") {
    dispatch({
      kind: "editor_cards", tab: 1,
      cards: [{ id: BROKER_ID, seq: 1, tool: "open_file", toolUseId: null, title: "Show /etc/hosts in your editor?", body: [{ kind: "code", text: "/etc/hosts" }] }],
    });
  }
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "focus_permission", tab: 1 });
  act(() => widen(view.container));
  wait(TYPING_GUARD_MS + 50);
  const root = view.container.querySelector<HTMLElement>(".agent-ui-conversation")!;
  expect(view.container.querySelector(".row-current .permission-card")).not.toBeNull();
  return { ...view, root };
}

const reasonBox = (container: HTMLElement) => container.querySelector<HTMLInputElement>(".permission-card input")!;
const inCardMode = (container: HTMLElement) => container.querySelector(".permission-card-mode") !== null;

/** Card mode on the card the cursor is on (a lone Enter on its row), then past its paint and its arming. */
function enterArmed(root: HTMLElement) {
  fireEvent.keyDown(root, { key: "Enter", code: "Enter" });
  fireEvent.keyUp(root, { key: "Enter", code: "Enter" });
  wait(CARD_ARM_MS + 100);
}

const root = (container: HTMLElement) => container.querySelector<HTMLElement>(".agent-ui-conversation")!;
const posted = (type: string) => messages.filter((m) => m.type === type);
/** Card mode armed, then Tab: the reason field open and holding the keys, `text` typed into it. */
function reasonOpen(container: HTMLElement, text = "because") {
  enterArmed(root(container));
  fireEvent.keyDown(root(container), { key: "Tab" });
  const box = reasonBox(container);
  expect(document.activeElement).toBe(box);
  fireEvent.change(box, { target: { value: text } });
  return box;
}

describe("the reason field never answers by itself", () => {
  it.each([{ shiftKey: true }, { ctrlKey: true }, { altKey: true }])("%j Enter in the open field, unarmed and armed, posts nothing", (mods) => {
    const { container } = setup();
    const box = reasonOpen(container);
    for (const pause of [0, CARD_ARM_MS + 100]) {
      wait(pause);
      expect(fireEvent.keyDown(box, { key: "Enter", ...mods })).toBe(false);
      fireEvent.keyUp(box, { key: "Enter", ...mods });
      expect(band(container)).toMatch(/Enter: Enter alone denies with the reason$/);
      wait(1000);
    }
    expect(answers()).toEqual([]);
    expect(inCardMode(container)).toBe(true);
    expect(document.activeElement).toBe(box);
  });

  it("a plain Enter in the open field, once armed, denies with its text through the after-half", () => {
    const { container } = setup();
    const box = reasonOpen(container, "too broad");
    wait(CARD_ARM_MS + 100);
    fireEvent.keyDown(box, { key: "Enter" });
    fireEvent.keyUp(box, { key: "Enter" });
    wait(TYPING_GUARD_MS + 50);
    expect(answers()).toEqual([expect.objectContaining({ permission_id: "p1", decision: "deny", reason: "too broad" })]);
  });
});

describe("card mode ending takes the keys back from the reason field", () => {
  it.each([
    ["the pane losing focus and getting it back", () => {
      dispatch({ kind: "pane_focus", focused: false });
      dispatch({ kind: "pane_focus", focused: true });
    }],
    ["the chooser covering the page", () => dispatch({ kind: "chooser", open: [], records: [] })],
    ["HINT starting", () => dispatch({ kind: "hint_collect", sessionId: 1 })],
  ])("after %s the field is unfocused, and its Enter then denies nothing", (_name, end) => {
    const { container } = setup();
    const box = reasonOpen(container);
    end();
    expect(inCardMode(container)).toBe(false);
    expect(document.activeElement).not.toBe(box);
    wait(1000);
    fireEvent.keyDown(box, { key: "Enter" });
    wait(1000);
    expect(answers()).toEqual([]);
  });

  it("F1 ends card mode, opens the sheet, and leaves the field unfocused", () => {
    const { container } = setup();
    const box = reasonOpen(container);
    fireEvent.keyDown(box, { key: "F1" });
    expect(inCardMode(container)).toBe(false);
    expect(document.activeElement).not.toBe(box);
    expect(answers()).toEqual([]);
  });
});

describe("the key that leaves card mode stays inside while held", () => {
  it("a held Ctrl+c leaves card mode and interrupts nothing; a fresh Ctrl+c after its release interrupts", () => {
    const { container } = setup();
    enterArmed(root(container));
    fireEvent.keyDown(root(container), { key: "c", code: "KeyC", ctrlKey: true });
    expect(inCardMode(container)).toBe(false);
    for (let i = 0; i < 3; i++) {
      wait(30);
      expect(fireEvent.keyDown(document.activeElement ?? root(container), { key: "c", code: "KeyC", ctrlKey: true, repeat: true })).toBe(false);
    }
    wait(1000);
    expect(posted("interrupt")).toEqual([]);
    fireEvent.keyUp(document.activeElement ?? root(container), { key: "c", code: "KeyC", ctrlKey: true });
    wait(1000);
    fireEvent.keyDown(document.activeElement ?? root(container), { key: "c", code: "KeyC", ctrlKey: true });
    expect(posted("interrupt")).toHaveLength(1);
    expect(answers()).toEqual([]);
  });

  it("a held Esc that left card mode is swallowed until its release", () => {
    const { container } = setup();
    enterArmed(root(container));
    fireEvent.keyDown(root(container), { key: "Escape" });
    expect(inCardMode(container)).toBe(false);
    expect(fireEvent.keyDown(root(container), { key: "Escape", repeat: true })).toBe(false);
    fireEvent.keyUp(root(container), { key: "Escape" });
    expect(answers()).toEqual([]);
  });
});

describe("an answer cancelled by something that is not a key says so", () => {
  it.each([
    ["an arrival of its own tab", () => dispatch({ kind: "arrive" })],
    ["the panel taking the keys again (pane_focus)", () => dispatch({ kind: "pane_focus", focused: true })],
  ])("an armed 1, then %s in its after-half: nothing answered, the band says so, and a later armed 1 answers", (_name, interrupt) => {
    const { container } = setup();
    enterArmed(root(container));
    fireEvent.keyDown(root(container), { key: "1", code: "Digit1" });
    fireEvent.keyUp(root(container), { key: "1", code: "Digit1" });
    wait(100);
    interrupt();
    expect(band(container)).toBe(ANSWER_INTERRUPTED_FLASH);
    wait(1000);
    expect(answers()).toEqual([]);
    expect(inCardMode(container)).toBe(true);
    fireEvent.keyDown(root(container), { key: "1", code: "Digit1" });
    fireEvent.keyUp(root(container), { key: "1", code: "Digit1" });
    wait(TYPING_GUARD_MS + 50);
    expect(answers()).toEqual([expect.objectContaining({ permission_id: "p1", decision: "allow" })]);
  });
});

describe("a deny by digit carries the field's kept text", () => {
  it("Tab, a reason, Esc (kept), then an armed 3 (Deny beside Always allow) denies with it", () => {
    const { container } = setup();
    const box = reasonOpen(container, "not now");
    fireEvent.keyDown(box, { key: "Escape" });
    expect(document.activeElement).toBe(root(container));
    wait(CARD_ARM_MS + 100);
    fireEvent.keyDown(root(container), { key: "3", code: "Digit3" });
    fireEvent.keyUp(root(container), { key: "3", code: "Digit3" });
    wait(TYPING_GUARD_MS + 50);
    expect(answers()).toEqual([expect.objectContaining({ permission_id: "p1", decision: "deny", reason: "not now" })]);
  });

  it("an armed 1 with text kept in the field sends no reason", () => {
    const { container } = setup();
    const box = reasonOpen(container, "not now");
    fireEvent.keyDown(box, { key: "Escape" });
    wait(CARD_ARM_MS + 100);
    fireEvent.keyDown(root(container), { key: "1", code: "Digit1" });
    wait(TYPING_GUARD_MS + 50);
    expect(answers()).toHaveLength(1);
    expect(answers()[0].reason).toBeUndefined();
  });
});

describe("a change of the held card's offer ends card mode", () => {
  it("the rule offer withdrawn under card mode: card mode ends with a flash, and 2 (Always allow) then sends nothing", () => {
    const { container } = setup();
    enterArmed(root(container));
    dispatch({ kind: "rule_offers", tab: 1, offers: {} });
    expect(inCardMode(container)).toBe(false);
    expect(band(container)).toBe(CARD_CHANGED_FLASH);
    wait(1000);
    fireEvent.keyDown(root(container), { key: "2", code: "Digit2" });
    wait(1000);
    expect(answers()).toEqual([]);
  });

  it("the rule replaced by another during 2's after-half: card mode ends, nothing is saved, and the new rule answers", () => {
    const { container } = setup();
    enterArmed(root(container));
    fireEvent.keyDown(root(container), { key: "2", code: "Digit2" });
    fireEvent.keyUp(root(container), { key: "2", code: "Digit2" });
    wait(100);
    dispatch({ kind: "rule_offers", tab: 1, offers: { p1: "rm -rf *" } });
    expect(inCardMode(container)).toBe(false);
    expect(band(container)).toBe(CARD_CHANGED_FLASH);
    wait(1000);
    expect(answers()).toEqual([]);
    // Entered again, the card shows the new rule, and 2 grants that one.
    fireEvent.keyDown(root(container), { key: "n", code: "KeyN", altKey: true });
    wait(CARD_ARM_MS + 100);
    fireEvent.keyDown(root(container), { key: "2", code: "Digit2" });
    wait(TYPING_GUARD_MS + 50);
    expect(answers()).toEqual([expect.objectContaining({ permission_id: "p1", decision: "allow", remember: true })]);
  });

  it("the rule replaced in the same task as the after-half's end: nothing is saved", () => {
    const { container } = setup();
    enterArmed(root(container));
    fireEvent.keyDown(root(container), { key: "2", code: "Digit2" });
    fireEvent.keyUp(root(container), { key: "2", code: "Digit2" });
    wait(100);
    // The envelope and the answer's timer in one task, with no render between them.
    act(() => {
      window.__eitriDispatch!(JSON.stringify({ kind: "rule_offers", tab: 1, offers: { p1: "rm -rf *" } }));
      vi.advanceTimersByTime(TYPING_GUARD_MS);
    });
    wait(1000);
    expect(answers()).toEqual([]);
    expect(inCardMode(container)).toBe(false);
  });

  it("the same offer sent again changes nothing: card mode stays and 2 answers with remember", () => {
    const { container } = setup();
    enterArmed(root(container));
    dispatch({ kind: "rule_offers", tab: 1, offers: { p1: "rm *" } });
    expect(inCardMode(container)).toBe(true);
    wait(CARD_ARM_MS + 100);
    fireEvent.keyDown(root(container), { key: "2", code: "Digit2" });
    wait(TYPING_GUARD_MS + 50);
    expect(answers()).toEqual([expect.objectContaining({ permission_id: "p1", decision: "allow", remember: true })]);
  });
});

describe("a held Alt-n moves to no card", () => {
  it("outside card mode, a repeated Alt+n enters nothing and says so; a fresh one enters", () => {
    const { container } = setup();
    fireEvent.keyDown(root(container), { key: "n", code: "KeyN", altKey: true, repeat: true });
    expect(inCardMode(container)).toBe(false);
    expect(band(container)).toBe("Alt-n: a held key moves to no card — press it again");
    fireEvent.keyDown(root(container), { key: "n", code: "KeyN", altKey: true });
    expect(inCardMode(container)).toBe(true);
  });

  it("in card mode, a repeated Alt+n keeps the card it holds", () => {
    const { container } = setup("two");
    enterArmed(root(container));
    const held = () => container.querySelector(".permission-card-mode")!.closest('[data-nav-stop="row"]');
    const before = held();
    fireEvent.keyDown(root(container), { key: "n", code: "KeyN", altKey: true, repeat: true });
    expect(held()).toBe(before);
    expect(band(container)).toBe("Alt-n: a held key moves to no card — press it again");
    fireEvent.keyDown(root(container), { key: "n", code: "KeyN", altKey: true });
    expect(held()).not.toBe(before);
  });
});

describe("what an arrival, a focus move and a tab switch do to card mode", () => {
  it("a digit 270 ms after an arrival of the card's tab is refused as arming (past the guard, inside the re-arm)", () => {
    const { container } = setup();
    enterArmed(root(container));
    dispatch({ kind: "arrive" });
    wait(270);
    fireEvent.keyDown(root(container), { key: "1", code: "Digit1" });
    expect(band(container)).toBe(cardArmingFlash("1"));
    wait(1000);
    expect(answers()).toEqual([]);
    expect(inCardMode(container)).toBe(true);
  });

  it("an arrival during card mode leaves card mode and the cursor on the card it holds", () => {
    const { container } = setup("two");
    // The second card: Alt-n from the first (the arrival's landing), then armed.
    fireEvent.keyDown(root(container), { key: "n", code: "KeyN", altKey: true });
    wait(CARD_ARM_MS + 100);
    const held = container.querySelector(".permission-card-mode")!;
    expect(held.closest(".row-current")).not.toBeNull();
    dispatch({ kind: "arrive" });
    expect(container.querySelector(".permission-card-mode")).toBe(held);
    expect(held.closest(".row-current")).not.toBeNull();
  });

  it("a focus that lands on another card's button ends card mode, answering nothing", () => {
    const { container } = setup("two");
    enterArmed(root(container));
    const other = Array.from(container.querySelectorAll<HTMLElement>(".permission-card")).find(
      (card) => !card.classList.contains("permission-card-mode"),
    )!;
    act(() => other.querySelector<HTMLButtonElement>('[data-nav-action="allow"]')!.focus()); // a mouse press dragged off it
    expect(inCardMode(container)).toBe(false);
    wait(1000);
    expect(answers()).toEqual([]);
  });

  it("a tab switch ends card mode and shows no flash on the new tab", () => {
    const { container } = setup();
    enterArmed(root(container));
    dispatch({ kind: "tabs", active: 2, tabs: [TAB, { ...TAB, id: 2, number: 2, label: "2 new", pending: 0 }] });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: { ...initialState(), status: { kind: "running" } } });
    expect(inCardMode(container)).toBe(false);
    expect(band(container)).toBeNull();
    expect(answers()).toEqual([]);
  });

  it("a tab switch during an answer's after-half sends nothing and says nothing on the new tab", () => {
    const { container } = setup();
    enterArmed(root(container));
    fireEvent.keyDown(root(container), { key: "1", code: "Digit1" });
    fireEvent.keyUp(root(container), { key: "1", code: "Digit1" });
    wait(100);
    dispatch({ kind: "tabs", active: 2, tabs: [TAB, { ...TAB, id: 2, number: 2, label: "2 new", pending: 0 }] });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: { ...initialState(), status: { kind: "running" } } });
    expect(inCardMode(container)).toBe(false);
    expect(band(container)).toBeNull();
    wait(1000);
    expect(band(container)).toBeNull();
    expect(answers()).toEqual([]);
  });

  it("Alt+n in a card's reason field outside card mode enters card mode, as the field's flash says", () => {
    const { container } = setup();
    const box = reasonBox(container);
    act(() => box.focus()); // a click into it
    fireEvent.keyDown(box, { key: "Enter" });
    expect(band(container)).toBe(REASON_ENTER_OUTSIDE_FLASH);
    wait(TYPING_GUARD_MS + 50);
    expect(fireEvent.keyDown(box, { key: "n", code: "KeyN", altKey: true })).toBe(false);
    expect(inCardMode(container)).toBe(true);
    expect(document.activeElement).toBe(root(container));
    expect(box.value).toBe("");
    wait(CARD_ARM_MS + 100);
    fireEvent.keyDown(root(container), { key: "3", code: "Digit3" });
    wait(TYPING_GUARD_MS + 50);
    expect(answers()).toEqual([expect.objectContaining({ permission_id: "p1", decision: "deny" })]);
  });

  it("outside card mode the field's Enter says card mode first", () => {
    const { container } = setup();
    const box = reasonBox(container);
    act(() => box.focus()); // a click into it
    expect(fireEvent.keyDown(box, { key: "Enter" })).toBe(false);
    expect(band(container)).toBe(REASON_ENTER_OUTSIDE_FLASH);
    wait(1000);
    expect(answers()).toEqual([]);
  });
});
