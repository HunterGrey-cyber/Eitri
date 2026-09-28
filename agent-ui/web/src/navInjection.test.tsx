// @vitest-environment jsdom
/**
 * v1 hardening Task 3 (ruling R2; review `2026-09-27-v1-hardening/codex-sec-panel-content-verdicts.md`,
 * finding 1): a model reply that carries elements marked `data-nav-stop="row"` must not move which
 * row a key acts on. The rows are found by structure -- a stop is never inside another stop -- and
 * `a`/`d` answer the card under the cursor by its `permissionId`, never by clicking a button that a
 * DOM query found.
 *
 * Every case runs twice: once through the real sanitizer (`renderMarkdown`), and once with it
 * bypassed, rendering the reply's raw HTML, so the structural fix is shown to stand on its own
 * whatever Task 1's sanitizer does or does not strip.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { hintTargets } from "./nav";
import type { AgentDomainEvent, AgentUiState, Hello } from "./types";

const markdown = vi.hoisted(() => ({ raw: false }));
vi.mock("./markdown", async (importOriginal) => {
  const real = await importOriginal<typeof import("./markdown")>();
  return { ...real, renderMarkdown: (text: string) => (markdown.raw ? text : real.renderMarkdown(text)) };
});

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
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
  markdown.raw = false;
  delete (navigator as unknown as Record<string, unknown>).clipboard;
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
const LIVE_TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;
function snapshotState(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return { ...initialState(), status: { kind: "running" }, ...overrides };
}
/** Past v1 S1's 250ms typing guard, so a lone key is what acts. */
const wait = (ms = 300) => act(() => vi.advanceTimersByTime(ms));
const press = (key: string, init: Record<string, unknown> = {}) =>
  fireEvent.keyDown(document.activeElement ?? document.body, { key, ...init });
const answered = () => posted.filter((m) => m.type === "permission_response");
const text = (el: Element | null | undefined) => (el?.textContent ?? "").replace(/\s+/g, " ");
const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;

/** Two hidden elements that claim to be conversation rows, the verdict's probe. */
const TWO_HIDDEN_ROWS = '<div data-nav-stop="row" hidden></div><div data-nav-stop="row" hidden></div>';

/** A prompt, a reply carrying `injected`, then two gated Bash calls: `rm -rf important` first,
 *  `echo hello > note.txt` second. Timeline: 0 prompt, 1 reply, 2 tool rm, 3 card rm, 4 tool echo,
 *  5 card echo, then (when `after` is given) 6 a second reply. */
function setup(injected: string, after?: string) {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 0, state: snapshotState() });
  const list: AgentDomainEvent[] = [
    { type: "user_prompt_submitted", text: "tidy up" },
    { type: "turn_started", turn_id: "t1" },
    { type: "content_delta", turn_id: "t1", kind: "text", text: `I'll write a note.\n\n${injected}\n` },
    { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_rm", name: "Bash", input: { command: "rm -rf important" } },
    { type: "permission_requested", permission_id: "perm-rm", tool_use_id: "toolu_rm", tool_name: "Bash", input: { command: "rm -rf important" } },
    { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_echo", name: "Bash", input: { command: "echo hello > note.txt" } },
    { type: "permission_requested", permission_id: "perm-echo", tool_use_id: "toolu_echo", tool_name: "Bash", input: { command: "echo hello > note.txt" } },
    ...(after === undefined ? [] : [{ type: "content_delta", turn_id: "t1", kind: "text", text: after } as AgentDomainEvent]),
  ];
  dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
  dispatch({ kind: "pane_focus", focused: true });
  return rendered;
}

/** The row the panel draws its cursor on: the real conversation row, a direct child of the list. */
const cursorRow = (c: HTMLElement) => c.querySelector<HTMLElement>('.message-list > [data-nav-stop="row"].row-current');
const isEchoCard = (row: Element | null) => row !== null && row.classList.contains("row-permission") && text(row).includes("echo hello");
const isRmCard = (row: Element | null) => row !== null && row.classList.contains("row-permission") && text(row).includes("rm -rf");

/** `arrive` lands on the oldest card (rm); `j`, `j` walk to the echo card. */
function arriveThenTwoJ(c: HTMLElement) {
  dispatch({ kind: "arrive" });
  wait();
  expect(isRmCard(cursorRow(c))).toBe(true);
  press("j");
  wait();
  press("j");
  wait();
}

describe.each([
  { raw: false, via: "through the real sanitizer" },
  { raw: true, via: "with the sanitizer bypassed (raw HTML)" },
])("injected rows in a reply, $via", ({ raw }) => {
  beforeEach(() => {
    markdown.raw = raw;
  });

  it("the probe: arrive, j, j, a approves the echo card the cursor is on, never rm", () => {
    const { container } = setup(TWO_HIDDEN_ROWS);
    arriveThenTwoJ(container);
    expect(isEchoCard(cursorRow(container))).toBe(true);
    press("a");
    wait();
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-echo", decision: "allow" })]);
  });

  it("d on the echo card denies the echo card", () => {
    const { container } = setup(TWO_HIDDEN_ROWS);
    arriveThenTwoJ(container);
    press("d");
    wait();
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-echo", decision: "deny" })]);
  });

  it("d on the rm card itself denies rm (the probe's realism case answered nothing)", () => {
    const { container } = setup(TWO_HIDDEN_ROWS);
    dispatch({ kind: "arrive" });
    wait();
    expect(isRmCard(cursorRow(container))).toBe(true);
    press("d");
    wait();
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-rm", decision: "deny" })]);
  });

  it("D puts the keys in the echo card's own reason box", () => {
    const { container } = setup(TWO_HIDDEN_ROWS);
    arriveThenTwoJ(container);
    press("D", { shiftKey: true });
    wait();
    const box = document.activeElement as HTMLInputElement;
    expect(box.tagName).toBe("INPUT");
    expect(isEchoCard(box.closest(".row"))).toBe(true);
    fireEvent.change(box, { target: { value: "not now" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-echo", decision: "deny", reason: "not now" })]);
  });

  it("l on the echo card selects the echo card's own Approve", () => {
    const { container } = setup(TWO_HIDDEN_ROWS);
    arriveThenTwoJ(container);
    press("l");
    expect(document.activeElement?.textContent).toBe("Approve");
    expect(isEchoCard(document.activeElement!.closest(".row"))).toBe(true);
  });

  it("j and k walk the conversation's own rows, one per press, never an injected one", () => {
    const { container } = setup(TWO_HIDDEN_ROWS);
    press("g");
    press("g");
    wait();
    const seen: string[] = [];
    for (let n = 0; n < 5; n++) {
      press("j");
      wait();
      seen.push(cursorRow(container)?.className.match(/row-(prompt|assistant|tool|permission)\b/)?.[1] ?? "none");
    }
    expect(seen).toEqual(["assistant", "tool", "permission", "tool", "permission"]);
    expect(isEchoCard(cursorRow(container))).toBe(true);
    press("k");
    wait();
    // The echo call's own row: the fifth of the conversation's rows (a gated call's row does not
    // repeat its command, P4, so its text cannot tell the two calls apart).
    const rows = Array.from(container.querySelectorAll('.message-list > [data-nav-stop="row"]'));
    expect(rows.indexOf(cursorRow(container)!)).toBe(4);
    expect(cursorRow(container)!.classList.contains("row-tool")).toBe(true);
  });

  it("j and k from a card's own button step to the rows next to that card", () => {
    const { container } = setup(TWO_HIDDEN_ROWS);
    const rows = () => Array.from(container.querySelectorAll('.message-list > [data-nav-stop="row"]'));
    // arrive: the rm card (timeline 3); l: its Approve has the keys; j: the echo call (timeline 4).
    dispatch({ kind: "arrive" });
    wait();
    press("l");
    expect(isRmCard(document.activeElement!.closest(".row"))).toBe(true);
    press("j");
    wait();
    expect(rows().indexOf(cursorRow(container)!)).toBe(4);
    // Back on the rm card's Approve, k: the rm call (timeline 2).
    press("k");
    wait();
    press("l");
    expect(isRmCard(document.activeElement!.closest(".row"))).toBe(true);
    press("k");
    wait();
    expect(rows().indexOf(cursorRow(container)!)).toBe(2);
  });

  it("a HINT landing on the echo card puts the cursor on it, and a answers it", () => {
    const { container } = setup(TWO_HIDDEN_ROWS);
    // jsdom lays nothing out: give every stop, control and code block its own 10px box.
    const box = (el: Element, top: number, height = 10) => {
      (el as HTMLElement).getBoundingClientRect = () =>
        ({ top, bottom: top + height, left: 0, right: 100, width: 100, height, x: 0, y: top }) as DOMRect;
    };
    box(root(container), 0, 1000);
    box(container.querySelector(".message-list")!, 0, 1000);
    let y = 0;
    for (const el of container.querySelectorAll("[data-nav-stop], button, input, pre.code-block")) {
      box(el, y);
      y += 10;
    }
    const echoCard = Array.from(container.querySelectorAll(".row-permission")).find(isEchoCard)!;
    const index = hintTargets(root(container)).findIndex((t) => t.el === echoCard);
    expect(index).toBeGreaterThanOrEqual(0);
    dispatch({ kind: "hint_collect", sessionId: 1 });
    dispatch({ kind: "hint_land", sessionId: 1, index });
    expect(cursorRow(container)).toBe(echoCard);
    wait();
    press("a");
    wait();
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-echo", decision: "allow" })]);
  });

  it("y after a HINT on a code block below the injection copies that block", () => {
    const clipboard = { writeText: vi.fn() };
    Object.defineProperty(navigator, "clipboard", { value: clipboard, configurable: true });
    // With the sanitizer bypassed, `renderMarkdown` renders nothing: the reply is its own HTML.
    const reply = raw
      ? '<p>Now run:</p><pre class="code-block"><code>ls -la</code></pre><p>then look.</p>'
      : "Now run:\n\n```\nls -la\n```\n\nthen look.";
    const { container } = setup(TWO_HIDDEN_ROWS, reply);
    const box = (el: Element, top: number, height = 10) => {
      (el as HTMLElement).getBoundingClientRect = () =>
        ({ top, bottom: top + height, left: 0, right: 100, width: 100, height, x: 0, y: top }) as DOMRect;
    };
    box(root(container), 0, 1000);
    box(container.querySelector(".message-list")!, 0, 1000);
    let y = 0;
    for (const el of container.querySelectorAll("[data-nav-stop], button, input, pre.code-block")) {
      box(el, y);
      y += 10;
    }
    const code = container.querySelector<HTMLElement>(".row-assistant pre.code-block")!;
    expect(code).not.toBeNull();
    const index = hintTargets(root(container)).findIndex((t) => t.el === code);
    expect(index).toBeGreaterThanOrEqual(0);
    dispatch({ kind: "hint_collect", sessionId: 1 });
    dispatch({ kind: "hint_land", sessionId: 1, index });
    expect(cursorRow(container)?.contains(code)).toBe(true);
    press("y");
    expect(clipboard.writeText).toHaveBeenLastCalledWith("ls -la");
  });
});

describe("a key answers a card by its id, and only once", () => {
  it("a disables the card it answered, and neither a second a nor a click sends another answer", () => {
    const { container } = setup("(nothing)");
    dispatch({ kind: "arrive" });
    wait();
    press("a");
    wait();
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-rm", decision: "allow" })]);
    const card = cursorRow(container)!;
    const approve = Array.from(card.querySelectorAll("button")).find((b) => b.textContent === "Approve")!;
    expect(approve.disabled).toBe(true);
    press("a");
    wait();
    fireEvent.click(approve);
    expect(answered()).toHaveLength(1);
  });

  it("d sends the reason already typed into the card's box, as the card's own Deny does", () => {
    const { container } = setup("(nothing)");
    dispatch({ kind: "arrive" });
    wait();
    press("D", { shiftKey: true });
    wait();
    const box = document.activeElement as HTMLInputElement;
    expect(box.tagName).toBe("INPUT");
    fireEvent.change(box, { target: { value: "keep it" } });
    fireEvent.keyDown(box, { key: "Escape" });
    expect(document.activeElement).toBe(root(container));
    wait();
    press("d");
    wait();
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-rm", decision: "deny", reason: "keep it" })]);
  });

  it("a waiting a does nothing once the card was answered by a click in the meantime", () => {
    const { container } = setup("(nothing)");
    dispatch({ kind: "arrive" });
    wait();
    press("a");
    const card = cursorRow(container)!;
    fireEvent.click(Array.from(card.querySelectorAll("button")).find((b) => b.textContent === "Deny")!);
    wait();
    expect(answered()).toEqual([expect.objectContaining({ permission_id: "perm-rm", decision: "deny" })]);
  });

  it("a waiting a does nothing once the card was resolved in the meantime", () => {
    setup("(nothing)");
    dispatch({ kind: "arrive" });
    wait();
    press("a");
    dispatch({
      kind: "events",
      tab: 1,
      fromRevision: 7,
      throughRevision: 8,
      events: [{ type: "permission_resolved", permission_id: "perm-rm", outcome: "denied" }],
    });
    wait();
    expect(answered()).toEqual([]);
  });
});
