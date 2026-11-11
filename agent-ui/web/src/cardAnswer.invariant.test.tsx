// @vitest-environment jsdom
/// <reference types="vite/client" />
/* A key answers a card only through card mode: armed, a fresh press on the root, its after-half quiet, by the card's
   own id (`answerByKey` with a key proof). A card button answers only a real pointer click (`pointerAnswerProof`).
   These cases hold that from the outside, on the page, and a text check holds who may build a proof. */
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
vi.unmock("./afterPaint");
import App from "./App";
import { initialState } from "./reducer";
import { TYPING_GUARD_MS } from "./typingGuard";
import { CARD_ARM_MS } from "./cardMode";
import { REPLAYED_KEY_FLASH, cardArmingFlash } from "./app/flashes";

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
function setup(kind: "provider" | "broker" = "provider") {
  const widen = stubBandWidth();
  const view = render(<App />);
  dispatch({ kind: "hello", backend: "legacy", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [TAB] });
  const pending = kind === "provider" ? [{ seq: 1, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "rm build" } }] : [];
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 3, state: { ...initialState(), status: { kind: "running" }, pendingPermissions: pending } });
  if (kind === "provider") dispatch({ kind: "rule_offers", tab: 1, offers: { p1: "rm *" } });
  else {
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

const LABELS = ["Approve", "Always allow", "Deny"] as const;
const ACTION: Record<string, string> = { Approve: "allow", Deny: "deny", "Always allow": "always" };
/** By its place, not its text: in card mode each button's text starts with its digit. */
function button(container: HTMLElement, label: string): HTMLButtonElement {
  return container.querySelector<HTMLButtonElement>(`.permission-card button[data-nav-action="${ACTION[label]}"]`)!;
}
const reasonBox = (container: HTMLElement) => container.querySelector<HTMLInputElement>(".permission-card input")!;
const inCardMode = (container: HTMLElement) => container.querySelector(".permission-card-mode") !== null;

/** A keydown on `el` as a browser would carry it on: a focused button activates on Enter or Space whose default
 *  survived, and that activation is a click with `detail` 0. Returns whether the default survived. */
function keyOn(el: HTMLElement, init: Record<string, unknown>): boolean {
  const survived = fireEvent.keyDown(el, init);
  fireEvent.keyUp(el, init);
  if (survived && el.tagName === "BUTTON" && (init.key === "Enter" || init.key === " ")) fireEvent.click(el, { detail: 0 });
  return survived;
}

/** Card mode on the card the cursor is on (a lone Enter on its row), then past its paint and its arming. */
function enterArmed(root: HTMLElement) {
  fireEvent.keyDown(root, { key: "Enter", code: "Enter" });
  fireEvent.keyUp(root, { key: "Enter", code: "Enter" });
  wait(CARD_ARM_MS + 100);
}

describe("(a) a card button's Enter and Space never reach its activation", () => {
  it.each(LABELS)("out of card mode, Enter and Space on a focused %s are prevented and post nothing", (label) => {
    const { container } = setup();
    const target = button(container, label);
    for (const key of ["Enter", " "]) {
      act(() => target.focus()); // a mouse press dragged off it: the one way a card button holds focus
      wait(TYPING_GUARD_MS + 50);
      expect(fireEvent.keyDown(target, { key }), JSON.stringify(key)).toBe(false);
      fireEvent.keyUp(target, { key });
      // A browser that still synthesised its click (a Space released over a button the mouse holds :active) makes one
      // with `detail` 0, which proves no pointer.
      fireEvent.click(target, { detail: 0 });
      wait(1000);
      if (inCardMode(container)) fireEvent.keyDown(container.querySelector<HTMLElement>(".agent-ui-conversation")!, { key: "Escape" });
    }
    expect(answers()).toEqual([]);
  });

  it.each(LABELS)("in card mode, Enter and Space on %s are prevented and post nothing, the reason box shut or open", (label) => {
    const { container, root } = setup();
    enterArmed(root);
    expect(inCardMode(container)).toBe(true);
    const target = button(container, label);
    for (const key of ["Enter", " "]) {
      expect(keyOn(target, { key }), `shut ${JSON.stringify(key)}`).toBe(false);
      wait(CARD_ARM_MS + TYPING_GUARD_MS + 100);
    }
    // The box open: a key typed there is its text, but on a button it is still claimed.
    fireEvent.keyDown(root, { key: "Tab" });
    expect(document.activeElement).toBe(reasonBox(container));
    for (const key of ["Enter", " "]) {
      expect(keyOn(target, { key }), `open ${JSON.stringify(key)}`).toBe(false);
      wait(CARD_ARM_MS + TYPING_GUARD_MS + 100);
    }
    expect(answers()).toEqual([]);
  });
});

describe("outside card mode, no key on a card's controls answers it", () => {
  const KEYS: Record<string, unknown>[] = [
    { key: "a" }, { key: "d" }, { key: "D", shiftKey: true }, { key: "Enter" }, { key: " " },
  ];
  it.each(KEYS)("%j on each card button and in the reason box posts no answer, at once or after a pause", (init) => {
    const { container } = setup();
    for (const target of [...LABELS.map((l) => button(container, l)), reasonBox(container)]) {
      act(() => target.focus());
      wait(TYPING_GUARD_MS + 50);
      keyOn(target, init);
      wait(1000);
      keyOn(target, init);
      wait(1000);
      // An Enter on a focused button is card mode for its card (no answer): leave it before the next target.
      if (inCardMode(container)) fireEvent.keyDown(container.querySelector<HTMLElement>(".agent-ui-conversation")!, { key: "Escape" });
      wait(1000);
    }
    expect(answers()).toEqual([]);
  });

  it("a broker card's buttons the same: no key on them posts an editor_card_answer", () => {
    const { container } = setup("broker");
    for (const label of ["Approve", "Deny"]) {
      const target = button(container, label);
      for (const init of KEYS) {
        act(() => target.focus());
        wait(TYPING_GUARD_MS + 50);
        keyOn(target, init);
        wait(1000);
        if (inCardMode(container)) fireEvent.keyDown(container.querySelector<HTMLElement>(".agent-ui-conversation")!, { key: "Escape" });
      }
    }
    expect(answers()).toEqual([]);
  });

  /* BROWSE Tab and `l` walk nothing onto them: the controls are `tabIndex -1` and off `controlsOf`. The Tab is
     modelled as a browser's: the next element in document order that takes focus, only if its default survived. */
  it("neither BROWSE l nor a Tab walk from the root lands on a card control", () => {
    const { container, root } = setup();
    for (let i = 0; i < 4; i++) {
      fireEvent.keyDown(document.activeElement ?? root, { key: "l" });
      wait(TYPING_GUARD_MS + 50);
      expect(document.activeElement?.closest("[data-card-control]") ?? null).toBeNull();
    }
    act(() => root.focus());
    for (let i = 0; i < 6; i++) {
      const from = document.activeElement as HTMLElement;
      if (!fireEvent.keyDown(from, { key: "Tab" })) break;
      const next = Array.from(container.querySelectorAll<HTMLElement>("button, input, textarea, [tabindex]")).find(
        (el) =>
          !(el as HTMLButtonElement).disabled &&
          el.tabIndex >= 0 &&
          (from.compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0,
      );
      if (next === undefined) break;
      act(() => next.focus());
      expect(document.activeElement?.closest("[data-card-control]") ?? null).toBeNull();
    }
    for (const el of container.querySelectorAll<HTMLElement>(".permission-card [data-card-control]")) expect(el.tabIndex).toBe(-1);
  });
});

describe("a card button answers only a real pointer click", () => {
  it.each(LABELS)("a click with detail 0 on %s posts nothing; detail 1 answers it", (label) => {
    const { container } = setup();
    const target = button(container, label);
    fireEvent.click(target, { detail: 0 });
    act(() => target.click());
    expect(answers()).toEqual([]);
    fireEvent.click(target, { detail: 1 });
    expect(answers()).toEqual([
      expect.objectContaining({
        type: "permission_response",
        permission_id: "p1",
        decision: label === "Deny" ? "deny" : "allow",
        ...(label === "Always allow" ? { remember: true } : {}),
      }),
    ]);
  });

  it.each(["Approve", "Deny"])("a broker card's %s: detail 0 posts nothing; detail 1 answers with origin click", (label) => {
    const { container } = setup("broker");
    fireEvent.click(button(container, label), { detail: 0 });
    expect(answers()).toEqual([]);
    fireEvent.click(button(container, label), { detail: 1 });
    expect(answers()).toEqual([
      expect.objectContaining({ type: "editor_card_answer", card_id: BROKER_ID, decision: label === "Deny" ? "deny" : "allow", origin: "click" }),
    ]);
  });
});

describe("(b) only a fresh, armed press on the root answers", () => {
  it("a 1 that lands on <body> after a focus loss is replayed and refused; nothing is answered", () => {
    const { container, root } = setup();
    enterArmed(root);
    act(() => (document.activeElement as HTMLElement).blur());
    expect(document.activeElement).toBe(document.body);
    fireEvent.keyDown(document.body, { key: "1", code: "Digit1" });
    expect(band(container)).toBe(REPLAYED_KEY_FLASH);
    fireEvent.keyUp(root, { key: "1", code: "Digit1" });
    wait(CARD_ARM_MS);
    wait(1000);
    expect(answers()).toEqual([]);
    expect(document.activeElement).toBe(root);
  });

  /* The positive control for the case above: the same flow with the digit pressed on the root answers, so the
     refusal there is the replay's and not the set-up's. */
  it("the same flow with a fresh 1 on the root answers the card", () => {
    const { root } = setup();
    enterArmed(root);
    fireEvent.keyDown(root, { key: "1", code: "Digit1" });
    fireEvent.keyUp(root, { key: "1", code: "Digit1" });
    wait(TYPING_GUARD_MS + 50);
    expect(answers()).toEqual([expect.objectContaining({ type: "permission_response", permission_id: "p1", decision: "allow" })]);
  });

  it("a 1 before card mode is painted answers nothing and says it is arming", () => {
    const { container, root } = setup();
    let frame: FrameRequestCallback | undefined;
    vi.stubGlobal("requestAnimationFrame", (run: FrameRequestCallback) => { frame = run; return 1; });
    fireEvent.keyDown(root, { key: "n", code: "KeyN", altKey: true });
    fireEvent.keyUp(root, { key: "n", code: "KeyN", altKey: true });
    expect(inCardMode(container)).toBe(true);
    wait(CARD_ARM_MS + 1000);
    fireEvent.keyDown(root, { key: "1", code: "Digit1" });
    expect(band(container)).toBe(cardArmingFlash("1"));
    fireEvent.keyUp(root, { key: "1", code: "Digit1" });
    wait(CARD_ARM_MS + 1000);
    expect(answers()).toEqual([]);
    act(() => frame!(performance.now()));
    act(() => frame!(performance.now()));
    wait(CARD_ARM_MS + 100);
    fireEvent.keyDown(root, { key: "1", code: "Digit1" });
    fireEvent.keyUp(root, { key: "1", code: "Digit1" });
    wait(TYPING_GUARD_MS + 50);
    expect(answers()).toHaveLength(1);
  });
});

/* (c) Who may build a proof, read from the source as text: one pattern over each file's whole text, so it does not
   depend on where a line ends. Test files are left out (they build proofs to test the constructors). */
describe("(c) the proofs' constructors and their callers", () => {
  const sources = Object.fromEntries(
    Object.entries(
      import.meta.glob(["./**/*.ts", "./**/*.tsx"], { query: "?raw", import: "default", eager: true }) as Record<string, string>,
    ).filter(([path]) => !/\.test\.tsx?$/.test(path) && !path.includes("/fixtures/")),
  );
  const count = (text: string, re: RegExp) => (text.match(new RegExp(re.source, "g")) ?? []).length;
  const holders = (re: RegExp) => Object.keys(sources).filter((path) => re.test(sources[path])).sort();

  it("reads the panel's sources", () => {
    expect(Object.keys(sources)).toEqual(expect.arrayContaining(["./App.tsx", "./cardMode.ts", "./answerProof.ts", "./components/PermissionCard.tsx"]));
  });

  it("keyAnswerProof( is only in cardMode.ts and once in App.tsx, inside answerByKey", () => {
    expect(holders(/\bkeyAnswerProof\(/)).toEqual(["./App.tsx", "./cardMode.ts"]);
    const app = sources["./App.tsx"];
    expect(count(app, /\bkeyAnswerProof\(/)).toBe(1);
    const start = app.indexOf("function answerByKey(");
    const end = app.indexOf("\n  function ", start + 1);
    const at = app.indexOf("keyAnswerProof(");
    expect(start).toBeGreaterThan(-1);
    expect(at > start && at < end).toBe(true);
  });

  it("pointerAnswerProof( is only its definition and PermissionCard's onClick handlers, on their own event", () => {
    expect(holders(/\bpointerAnswerProof\(/)).toEqual(["./answerProof.ts", "./components/PermissionCard.tsx"]);
    expect(count(sources["./answerProof.ts"], /\bpointerAnswerProof\(/)).toBe(1);
    const card = sources["./components/PermissionCard.tsx"];
    const calls = count(card, /\bpointerAnswerProof\(/);
    const inOnClick = count(card, /onClick=\{\((\w+)\)\s*=>\s*\{\s*const proof = pointerAnswerProof\(\1\);/);
    expect(calls).toBe(3);
    expect(inOnClick).toBe(calls);
  });

  it("no cast makes a proof, and no answer call casts its way past the types", () => {
    const casts = /\bas\s+(KeyAnswerProof|PointerAnswerProof)\b/;
    expect(holders(casts)).toEqual(["./answerProof.ts", "./cardMode.ts"]);
    // Every call of the three names an answer passes through, and the definition of `answerPermission`: the argument
    // list, up to its matching parenthesis, holds no `as` cast of any kind (`as never` too) and no `<Type>` one.
    const callee = /\b(answerPermission|onAnswerPermission|onAnswer)\(/g;
    let seen = 0;
    for (const [path, text] of Object.entries(sources)) {
      for (const match of text.matchAll(callee)) {
        seen++;
        let depth = 0;
        let end = match.index! + match[1].length;
        for (; end < text.length; end++) {
          if (text[end] === "(") depth++;
          else if (text[end] === ")" && --depth === 0) break;
        }
        const args = text.slice(match.index!, end + 1);
        expect(args, `${path}: ${args}`).not.toMatch(/\bas\s/);
        expect(args, `${path}: ${args}`).not.toMatch(/<\s*[A-Za-z_$][\w$.]*(\s*[|&,]\s*[A-Za-z_$][\w$.]*)*\s*>\s*[\w$(]/);
      }
    }
    // answerPermission's definition and its call in App.tsx, PermissionCard's onAnswer, MessageList's pass-through.
    expect(seen).toBeGreaterThanOrEqual(4);
  });

  it("the cast check sees the casts it is there to refuse", () => {
    const castPatterns = [/\bas\s/, /<\s*[A-Za-z_$][\w$.]*(\s*[|&,]\s*[A-Za-z_$][\w$.]*)*\s*>\s*[\w$(]/];
    const refused = (args: string) => castPatterns.some((re) => re.test(args));
    expect(refused("answerPermission(id, d, r, m, proof as never)")).toBe(true);
    expect(refused("onAnswer(id, d, r, m, {} as KeyAnswerProof)")).toBe(true);
    expect(refused("onAnswerPermission(id, d, r, m, <KeyAnswerProof>fake)")).toBe(true);
    expect(refused("onAnswer(id, d, r, m, <KeyAnswerProof | PointerAnswerProof>(fake))")).toBe(true);
    expect(refused("answerPermission(card.permissionId, proof.decision, proof.reason || undefined, proof.remember, proof)")).toBe(false);
  });
});
