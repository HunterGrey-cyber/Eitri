// @vitest-environment jsdom
/**
 * The trust question in the panel: how `App` draws a `trust_prompt`, which keys answer it and behind which
 * guards, what an answer echoes back to Rust, where the prompt ends, and the `:trust`/`:untrust` command line.
 * The overlay's own drawing is `components/TrustPrompt.test.tsx` and its key table `trust.test.ts`.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { EMPTY_PANEL_TABLE } from "./keymap";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { AgentDomainEvent, AgentUiState, Hello } from "./types";

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
  // `performance` is faked too: the guards read it, and `wait` has to move it.
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date", "performance"] });
  widen = stubBandWidth();
  Object.defineProperty(navigator, "clipboard", { value: { writeText: vi.fn() }, configurable: true });
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
let widen: (container: HTMLElement) => void;

const wait = (ms: number) => act(() => vi.advanceTimersByTime(ms));
const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
const press = (c: HTMLElement, k: string, init: Record<string, unknown> = {}) => fireEvent.keyDown(root(c), { key: k, ...init });
const requests = (type: string) => posted.filter((m) => m.type === type);

/** A running turn in tab 1 with one card waiting, so a stray `a` would be seen answering it. */
function cardEvents(): AgentDomainEvent[] {
  return [
    { type: "user_prompt_submitted", text: "tidy up" },
    { type: "turn_started", turn_id: "t1" },
    { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "rm build" } },
    { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: { command: "rm build" } },
    { type: "content_delta", turn_id: "t1", kind: "text", text: "meanwhile" },
  ];
}

/** Tab 1 live, the pane focused and the keys arrived, in BROWSE; `withCard` leaves a card waiting. */
function started({ withCard = false, tabs = [TAB1] as readonly Record<string, unknown>[] } = {}) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState() });
  if (withCard) {
    const list = cardEvents();
    dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  }
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  wait(TYPING_GUARD_MS + 50);
  return rendered;
}

const HEX_A = "a".repeat(64);
const HEX_B = "b".repeat(64);
const HEX_C = "c".repeat(64);
const HEX_D = "d".repeat(64);

function trustPrompt(over: Record<string, unknown> = {}) {
  return {
    kind: "trust_prompt", tab: 1, nonce: 17, root: "/home/u/p/src", top: "/home/u/p",
    fingerprint: HEX_A, findingsDigest: HEX_B, state: "untrusted", remember: "yes", rememberNote: null, changed: null,
    items: [{ what: "hook", file: ".claude/settings.json", label: "SessionStart", value: "touch /tmp/m", outside: false }],
    ...over,
  };
}

const prompt = (c: HTMLElement) => c.querySelector<HTMLElement>(".trust-prompt");
const bandPrompt = (c: HTMLElement) => c.querySelector(".band-prompt")?.textContent ?? null;
/** What the band says while the question is up: a flash is drawn after the prompt, which hides the message. */
const promptLine = (c: HTMLElement) => bandPrompt(c) ?? "";

/** Tab 1 live, the pane focused and the keys arrived, in BROWSE, then a trust question for it. */
function asked(over: Record<string, unknown> = {}, options: { tabs?: readonly Record<string, unknown>[] } = {}) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: options.tabs ?? [TAB1] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState() });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  wait(TYPING_GUARD_MS + 50);
  dispatch(trustPrompt(over));
  return rendered;
}

describe("the trust question", () => {
  it("draws the overlay over the conversation and the band's prompt", () => {
    const { container } = asked();
    expect(prompt(container)).not.toBeNull();
    expect(prompt(container)!.textContent).toContain("Trust this project's Claude configuration?");
    expect(prompt(container)!.textContent).toContain("SessionStart: touch /tmp/m");
    expect(bandPrompt(container)).toBe("trust? y/n");
  });

  it("y_and_n_past_the_guard_post_trust_answer_with_tab_nonce_and_the_shown_fingerprint", () => {
    for (const [key, trust] of [["y", true], ["Y", true], ["n", false]] as const) {
      posted = [];
      const { container, unmount } = asked();
      wait(TYPING_GUARD_MS + 50);
      press(container, key);
      expect(requests("trust_answer")).toEqual([
        expect.objectContaining({ tab: 1, nonce: 17, fingerprint: HEX_A, findings_digest: HEX_B, trust }),
      ]);
      expect(typeof requests("trust_answer")[0].request_id).toBe("string");
      expect(prompt(container)).toBeNull();
      expect(bandPrompt(container)).toBeNull();
      unmount();
    }
  });

  it("echoes a second prompt's own fingerprint and digest after it replaces the first", () => {
    const { container } = asked();
    wait(TYPING_GUARD_MS + 50);
    dispatch(trustPrompt({ nonce: 18, fingerprint: HEX_C, findingsDigest: HEX_D }));
    wait(TYPING_GUARD_MS + 50);
    press(container, "y");
    expect(requests("trust_answer")).toEqual([
      expect.objectContaining({ nonce: 18, fingerprint: HEX_C, findings_digest: HEX_D, trust: true }),
    ]);
  });

  it("a_second_prompt_restarts_the_wait", () => {
    const { container } = asked();
    wait(TYPING_GUARD_MS + 50);
    dispatch(trustPrompt({ nonce: 18, fingerprint: HEX_C, findingsDigest: HEX_D }));
    press(container, "y");
    expect(requests("trust_answer")).toEqual([]);
    expect(promptLine(container)).toBe("trust? y/n · wait a moment, then y or n");
    // The prompt is still up: a later y, past the wait, counts.
    expect(prompt(container)).not.toBeNull();
    wait(TYPING_GUARD_MS + 50);
    press(container, "y");
    expect(requests("trust_answer")).toHaveLength(1);
  });

  it("y_and_n_inside_the_typing_guard_post_nothing", () => {
    const { container } = asked();
    for (const key of ["y", "n"]) {
      press(container, key);
      expect(promptLine(container)).toBe("trust? y/n · wait a moment, then y or n");
    }
    expect(requests("trust_answer")).toEqual([]);
    // A letter typed just before the answer is typing too.
    wait(TYPING_GUARD_MS + 50);
    press(container, "x");
    press(container, "y");
    expect(requests("trust_answer")).toEqual([]);
  });

  it("y_with_ctrl_alt_meta_super_or_composing_posts_nothing", () => {
    const { container } = asked();
    wait(TYPING_GUARD_MS + 50);
    for (const key of ["y", "n"]) {
      for (const init of [{ ctrlKey: true }, { altKey: true }, { metaKey: true }, { isComposing: true }, { keyCode: 229 }]) {
        wait(TYPING_GUARD_MS + 50);
        press(container, key, init);
      }
    }
    expect(requests("trust_answer")).toEqual([]);
    expect(prompt(container)).not.toBeNull();
  });

  it("an_autorepeated_y_posts_nothing", () => {
    const { container } = asked();
    wait(TYPING_GUARD_MS + 50);
    press(container, "y", { repeat: true });
    press(container, "n", { repeat: true });
    expect(requests("trust_answer")).toEqual([]);
    // After a real answer, the held key's repeats are swallowed and never fall through as a BROWSE `y`.
    wait(TYPING_GUARD_MS + 50);
    press(container, "y");
    expect(requests("trust_answer")).toHaveLength(1);
    press(container, "y", { repeat: true });
    press(container, "y", { repeat: true });
    expect(requests("trust_answer")).toHaveLength(1);
    expect(container.textContent).not.toContain("copied");
  });

  it("escape_posts_trust_cancel", () => {
    const { container } = asked();
    // No wait: Escape starts nothing.
    press(container, "Escape");
    expect(requests("trust_cancel")).toEqual([expect.objectContaining({ tab: 1, nonce: 17 })]);
    expect(requests("trust_answer")).toEqual([]);
    expect(prompt(container)).toBeNull();
  });

  it("swallows every other key and says what the prompt takes", () => {
    const { container } = asked();
    wait(TYPING_GUARD_MS + 50);
    for (const key of ["Enter", "q", "a", "d", "i", "c", "Tab"]) {
      press(container, key);
      expect(promptLine(container), key).toBe("trust? y/n · y trusts and loads it · n starts without it · Esc puts this off");
      wait(TYPING_GUARD_MS + 50);
    }
    expect(prompt(container)).not.toBeNull();
    expect(posted.filter((m) => !["ready", "panel_keys"].includes(String(m.type)))).toEqual([]);
  });

  it("a session prompt's flash also says y covers this start only", () => {
    const { container } = asked({ remember: "session", rememberNote: "y trusts this start only; the next one asks again" });
    wait(TYPING_GUARD_MS + 50);
    press(container, "x");
    expect(promptLine(container)).toContain("y trusts this start only");
  });

  it("scrolls with j, k, Ctrl+d, Ctrl+u, gg and G, and stays open", () => {
    const { container } = asked();
    const box = prompt(container)!;
    Object.defineProperty(box, "scrollHeight", { value: 2000, configurable: true });
    Object.defineProperty(box, "clientHeight", { value: 400, configurable: true });
    wait(TYPING_GUARD_MS + 50);
    press(container, "j");
    const afterJ = box.scrollTop;
    expect(afterJ).toBeGreaterThan(0);
    press(container, "k");
    expect(box.scrollTop).toBeLessThan(afterJ);
    press(container, "d", { ctrlKey: true });
    expect(box.scrollTop).toBe(200);
    press(container, "u", { ctrlKey: true });
    expect(box.scrollTop).toBe(0);
    press(container, "G", { shiftKey: true });
    expect(box.scrollTop).toBe(1600);
    press(container, "g");
    press(container, "g");
    expect(box.scrollTop).toBe(0);
    // A lone g followed by another key is no gg.
    press(container, "G", { shiftKey: true });
    press(container, "g");
    press(container, "x");
    press(container, "g");
    expect(box.scrollTop).toBe(1600);
    expect(prompt(container)).not.toBeNull();
    expect(requests("trust_answer")).toEqual([]);
  });

  it("takes a key typed into a text field and never lets it answer", () => {
    const { container } = asked();
    wait(TYPING_GUARD_MS + 50);
    const field = document.createElement("input");
    container.querySelector(".agent-ui-conversation")!.appendChild(field);
    field.focus();
    wait(TYPING_GUARD_MS + 50);
    const proceeded = fireEvent.keyDown(field, { key: "y" });
    expect(proceeded).toBe(false);
    fireEvent.keyDown(field, { key: "n" });
    expect(requests("trust_answer")).toEqual([]);
    expect(prompt(container)).not.toBeNull();
    // Escape still puts the start off from inside the field.
    fireEvent.keyDown(field, { key: "Escape" });
    expect(requests("trust_cancel")).toHaveLength(1);
  });

  it("a_prompt_for_another_tab_is_dropped", () => {
    const { container } = asked({ tab: 2 }, { tabs: [TAB1, TAB2] });
    expect(prompt(container)).toBeNull();
    expect(bandPrompt(container)).toBeNull();
    wait(TYPING_GUARD_MS + 50);
    press(container, "y");
    expect(requests("trust_answer")).toEqual([]);
  });

  it("a_tab_switch_clears_the_prompt", () => {
    const { container } = asked({}, { tabs: [TAB1, TAB2] });
    expect(prompt(container)).not.toBeNull();
    dispatch({ kind: "tabs", active: 2, tabs: [TAB1, TAB2] });
    expect(prompt(container)).toBeNull();
    expect(bandPrompt(container)).toBeNull();
    wait(TYPING_GUARD_MS + 50);
    press(container, "y");
    expect(requests("trust_answer")).toEqual([]);
  });

  it("losing the pane's focus clears the prompt, as it does a bypass question", () => {
    const { container } = asked();
    dispatch({ kind: "pane_focus", focused: false });
    expect(prompt(container)).toBeNull();
  });

  /** Rust asks again the moment a tab waiting for the answer is on screen without its question, so a prompt
   *  arrives right after a close prompt, a rename field or the chooser took the keys. It must wait for them:
   *  drawn over them, it took the `y` meant for the close prompt, or the first letter of a name. */
  it("a_prompt_arriving_over_a_close_prompt_waits_and_y_closes_the_tab", () => {
    const { container } = asked();
    dispatch({ kind: "confirm_close", tab: 1, lines: ["close tab 1? y/n"] });
    expect(bandPrompt(container)).toBe("close tab 1? y/n");
    dispatch(trustPrompt({ nonce: 18 }));
    expect(bandPrompt(container)).toBe("close tab 1? y/n");
    expect(prompt(container)).toBeNull();
    wait(500);
    press(container, "y");
    expect(requests("close_tab")).toEqual([expect.objectContaining({ tab: 1 })]);
    expect(requests("trust_answer")).toEqual([]);
  });

  it("a_held_prompt_opens_once_the_close_prompt_is_answered_n_with_its_own_wait", () => {
    const { container } = asked();
    for (const lines of [["close tab 1? y/n"], ["close 2 other tabs? (y/n)"]]) {
      dispatch(lines[0].startsWith("close tab") ? { kind: "confirm_close", tab: 1, lines } : { kind: "confirm_close_others", tabs: [2], lines });
      dispatch(trustPrompt({ nonce: 18 }));
      wait(500);
      press(container, "n");
      expect(requests("close_tab")).toEqual([]);
      expect(requests("close_others")).toEqual([]);
      // The trust question is up now, and the key that ended the other prompt does not answer it.
      expect(prompt(container)).not.toBeNull();
      expect(bandPrompt(container)).toBe("trust? y/n");
      press(container, "y");
      expect(requests("trust_answer")).toEqual([]);
      wait(TYPING_GUARD_MS + 50);
      press(container, "Escape");
      expect(requests("trust_cancel")).toHaveLength(1);
      posted = [];
    }
  });

  it("a_prompt_arriving_over_a_bypass_question_waits", () => {
    const { container } = asked();
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 5, lines: ["bypass? y/n"] });
    dispatch(trustPrompt({ nonce: 18 }));
    expect(prompt(container)).toBeNull();
    wait(500);
    press(container, "y");
    expect(requests("trust_answer")).toEqual([]);
    expect(requests("confirm_bypass")).toHaveLength(1);
  });

  it("a_prompt_arriving_over_the_rename_field_leaves_the_name_to_the_field", () => {
    const { container } = asked();
    dispatch({ kind: "begin_rename", tab: 1, current: null });
    const input = container.querySelector<HTMLInputElement>(".tab-rename")!;
    input.focus();
    dispatch(trustPrompt({ nonce: 18 }));
    expect(prompt(container)).toBeNull();
    expect(document.activeElement).toBe(input);
    wait(500);
    fireEvent.keyDown(input, { key: "y" });
    fireEvent.change(input, { target: { value: "yarn" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(requests("trust_answer")).toEqual([]);
    expect(requests("rename_tab")).toEqual([expect.objectContaining({ tab: 1, name: "yarn" })]);
    // The field is gone, so the question opens, and only a later y past its own wait answers it.
    expect(prompt(container)).not.toBeNull();
    wait(TYPING_GUARD_MS + 50);
    press(container, "y");
    expect(requests("trust_answer")).toEqual([expect.objectContaining({ nonce: 18, trust: true })]);
  });

  it("a_route_away_drops_a_held_prompt", () => {
    const { container } = asked({}, { tabs: [TAB1, TAB2] });
    dispatch({ kind: "confirm_close", tab: 1, lines: ["close tab 1? y/n"] });
    dispatch(trustPrompt({ nonce: 18 }));
    dispatch({ kind: "tabs", active: 2, tabs: [TAB1, TAB2] });
    dispatch({ kind: "tabs", active: 1, tabs: [TAB1, TAB2] });
    expect(prompt(container)).toBeNull();
    wait(TYPING_GUARD_MS + 50);
    press(container, "y");
    expect(requests("trust_answer")).toEqual([]);
  });

  it("a_refused_answer_is_not_a_conversation_banner", () => {
    const { container } = asked();
    wait(TYPING_GUARD_MS + 50);
    press(container, "y");
    const [sent] = requests("trust_answer");
    dispatch({ kind: "command_result", requestId: sent.request_id, ok: false, error: "that prompt is no longer current" });
    expect(container.querySelector(".command-notice")).toBeNull();
  });

  it("drops a malformed prompt at the bridge: nothing is drawn", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { container } = asked({ fingerprint: "short" });
    expect(prompt(container)).toBeNull();
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });
});

const EMPTY_TAB1 = { ...TAB1, state: "not_started", label: "1 new" } as const;
const key = (el: Element | null, k: string, init: Record<string, unknown> = {}) => fireEvent.keyDown(el!, { key: k, ...init });
const emptyTab = (c: HTMLElement) => c.querySelector<HTMLElement>(".empty-tab[tabindex]");

/** The start screen (no session yet), the keys landed on it in BROWSE, then a trust question for the first send. */
function askedOnStartScreen(
  over: Record<string, unknown> = {},
  tab: Record<string, unknown> = EMPTY_TAB1,
  { typing = false }: { typing?: boolean } = {},
) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [tab] });
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: EMPTY_PANEL_TABLE, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "arrive" });
  act(() => widen(rendered.container));
  wait(TYPING_GUARD_MS + 50);
  if (typing) {
    // A first send: the keys are in the composer, a message typed, when the question opens.
    key(document.activeElement, "i");
    wait(TYPING_GUARD_MS + 50);
    fireEvent.change(rendered.container.querySelector("textarea")!, { target: { value: "hello" } });
  }
  dispatch(trustPrompt(over));
  return rendered;
}

describe("the trust question on the start screen", () => {
  it("leaves the status band outside the overlay's box, so the band's prompt and flashes can be read", () => {
    const { container } = askedOnStartScreen();
    const box = prompt(container)!;
    const band = container.querySelector(".status-band")!;
    expect(box).not.toBeNull();
    expect(box.parentElement!.classList.contains("empty-stage")).toBe(true);
    expect(box.parentElement!.contains(band)).toBe(false);
    expect(bandPrompt(container)).toBe("trust? y/n");
    // A flash made while it is up reads in the band, after the prompt.
    key(document.activeElement, "j");
    key(document.activeElement, "y");
    expect(promptLine(container)).toContain("trust? y/n");
    expect(promptLine(container)).toContain("wait a moment, then y or n");
  });

  it("takes no key from behind the overlay: the keys sit on the start screen's root", () => {
    const { container } = askedOnStartScreen();
    expect(document.activeElement).toBe(container.querySelector(".agent-ui-root"));
    key(document.activeElement, "i");
    expect(container.querySelector("textarea:focus")).toBeNull();
    expect(posted.filter((m) => String(m.type).startsWith("trust_"))).toEqual([]);
  });

  it("opening over a typed first send leaves no field holding the keys", () => {
    const { container } = askedOnStartScreen({}, EMPTY_TAB1, { typing: true });
    wait(TYPING_GUARD_MS + 50);
    // The composer is gone from behind the overlay, so no key can be typed into it and no key of the
    // question's own is lost to it.
    expect(container.querySelector("textarea")).toBeNull();
    expect(document.activeElement).toBe(container.querySelector(".agent-ui-root"));
    key(document.activeElement, "y");
    expect(requests("trust_answer")).toEqual([expect.objectContaining({ trust: true })]);
  });

  it("escape_hands_the_keys_back_to_the_start_screen_so_i_enters_INPUT", () => {
    const { container } = askedOnStartScreen();
    key(document.activeElement, "Escape");
    expect(requests("trust_cancel")).toHaveLength(1);
    expect(prompt(container)).toBeNull();
    // The keys must be on the element that handles them, not on the layout root above it.
    expect(document.activeElement).toBe(emptyTab(container));
    wait(TYPING_GUARD_MS + 50);
    key(document.activeElement, "i");
    expect(container.querySelector("textarea:focus")).not.toBeNull();
  });

  it("escape_over_a_typed_first_send_leaves_working_keys", () => {
    const { container } = askedOnStartScreen({}, EMPTY_TAB1, { typing: true });
    key(document.activeElement, "Escape");
    expect(prompt(container)).toBeNull();
    // Whichever mode the screen is in, the next key reaches its handler: Escape and `i` get to INPUT.
    key(document.activeElement, "Escape");
    wait(TYPING_GUARD_MS + 50);
    key(document.activeElement, "i");
    expect(container.querySelector("textarea:focus")).not.toBeNull();
  });

  it("n_hands_the_keys_back_too", () => {
    const { container } = askedOnStartScreen();
    wait(TYPING_GUARD_MS + 50);
    key(document.activeElement, "n");
    expect(requests("trust_answer")).toEqual([expect.objectContaining({ trust: false })]);
    expect(prompt(container)).toBeNull();
    expect(document.activeElement).toBe(emptyTab(container));
  });

  it("a key after the question closed answers nothing and posts no second answer", () => {
    const { container } = askedOnStartScreen();
    key(document.activeElement, "Escape");
    posted = [];
    key(document.activeElement, "y");
    key(document.activeElement, "n");
    key(document.activeElement, "a");
    expect(posted.filter((m) => String(m.type).startsWith("trust_") || String(m.type).includes("permission"))).toEqual([]);
    expect(prompt(container)).toBeNull();
  });

  it("a tab waiting for the answer says so, and never that a backend is starting", () => {
    const waiting = { ...TAB1, state: "awaiting_trust" } as const;
    const { container } = askedOnStartScreen({}, waiting);
    const text = container.querySelector(".empty-tab")!.textContent!;
    expect(text).toContain("Waiting for your answer to the trust question");
    expect(text).not.toContain("Starting the agent backend");
    wait(15_000);
    expect(container.querySelector(".empty-tab")!.textContent).not.toContain("a first start from a Verdandi checkout");
  });

  it("a starting tab still says the backend is starting", () => {
    const starting = { ...TAB1, state: "starting" } as const;
    const { container } = askedOnStartScreen({}, starting);
    expect(container.querySelector(".empty-tab")!.textContent).toContain("Starting the agent backend");
    expect(container.querySelector(".empty-tab")!.textContent).not.toContain("trust question");
  });
});

describe("the : line's trust commands", () => {
  function typed(container: HTMLElement, text: string) {
    dispatch({ kind: "open_command_line" });
    const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
    fireEvent.change(input, { target: { value: text } });
    fireEvent.keyDown(input, { key: "Enter" });
  }

  it("the_ex_line_sends_trust_and_untrust_only_on_an_exact_match", () => {
    const { container } = started();
    typed(container, "trust");
    expect(requests("trust_command")).toEqual([expect.objectContaining({ action: "trust" })]);
    typed(container, "untrust");
    expect(requests("trust_command").map((m) => m.action)).toEqual(["trust", "untrust"]);
    for (const line of ["trust x", "trusted", "Trust", "ls"]) {
      typed(container, line);
      expect(container.querySelector(".band-message")!.textContent).toBe(
        `:${line} — no ex commands here; ? lists this panel's keys`,
      );
    }
    expect(requests("trust_command")).toHaveLength(2);
  });

  it("a_trust_command_that_ends_untrusted_raises_no_banner", () => {
    const { container } = started();
    typed(container, "trust");
    const [sent] = requests("trust_command");
    dispatch({ kind: "command_result", requestId: sent.request_id, ok: false, error: "not trusted" });
    expect(container.querySelector(".command-notice")).toBeNull();
  });

  it("carries no tab: the command is the window's", () => {
    const { container } = started();
    typed(container, "trust");
    expect(requests("trust_command")[0]).not.toHaveProperty("tab");
  });
});
