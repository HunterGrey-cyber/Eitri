// @vitest-environment jsdom
/**
 * Claude's plan in the page: the dock in the composer's slot, the keys that move into and out of plan mode, what the
 * band says, and the routes that never decide a plan (Ctrl+y, card mode, Shift+Tab into Bypass).
 */
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { initialState } from "./reducer";
import { binding, TABLE } from "./testFixtures";
import type { PanelTable } from "./keymap";
import type { AgentUiState, PlanningPhase, PlanningState, PlanRecord, QuestionRecord, PermissionModeChoice } from "./types";

const SHA = "b".repeat(64);
const plan: PlanRecord = {
  seq: 2, interactionId: "p1", toolUseId: "toolu_exit", turnId: "t1", plan: "# Ship it\n\n1. Change `a.rs`\n2. Test",
  planSha256: SHA, allowedPrompts: [{ tool: "Bash", prompt: "run the tests" }], state: { kind: "waiting" }, unsupportedReason: null,
};
const question: QuestionRecord = { seq: 2, interactionId: "q1", toolUseId: null, turnId: "t1", unsupportedReason: null, state: { kind: "waiting" }, questions: [{
  question: "Which database?", header: "Database", multiSelect: false,
  options: [{ label: "Postgres", description: "", preview: null }, { label: "SQLite", description: "", preview: null }],
}] };
const tab = { id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: true, failure: null, title: null };
const PANEL: PanelTable = { ...TABLE, bindings: [...TABLE.bindings, binding(["<leader>", "p"], "plan.toggle", "plan mode")] };
const planning = (phase: PlanningPhase, origin: PermissionModeChoice | null = "auto"): PlanningState => ({ phase, origin });
let messages: Record<string, unknown>[];
let now: number;
/** The band lays out its row only once it is measured; jsdom measures nothing, so the test reports a wide band. */
let widen: (container: HTMLElement) => void;
function stubBandWidth(): (container: HTMLElement) => void {
  type Rec = { callback: ResizeObserverCallback; observed: Element[] };
  const observers: Rec[] = [];
  class FakeResizeObserver {
    private record: Rec;
    constructor(callback: ResizeObserverCallback) { this.record = { callback, observed: [] }; observers.push(this.record); }
    observe(el: Element) { this.record.observed.push(el); }
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
beforeEach(() => {
  vi.useFakeTimers({
    toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "setImmediate", "clearImmediate", "Date", "requestAnimationFrame", "cancelAnimationFrame"],
  });
  messages = []; now = 1000;
  vi.spyOn(performance, "now").mockImplementation(() => now);
  window.webkit = { messageHandlers: { eitriAgent: { postMessage: (m) => messages.push(JSON.parse(m)) } } };
  Element.prototype.scrollIntoView = vi.fn();
  widen = stubBandWidth();
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.useRealTimers(); vi.unstubAllGlobals(); });
function pause(ms: number) { act(() => { now += ms; vi.advanceTimersByTime(ms); }); }
function dispatch(payload: unknown) { act(() => window.__eitriDispatch!(JSON.stringify(payload))); }
function setup(overrides: Partial<AgentUiState> = {}, tabExtra: Record<string, unknown> = {}, live = true) {
  const view = render(<App />);
  dispatch({ kind: "hello", backend: "sidecar", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [{ ...tab, ...(live ? {} : { state: "not_started" }), ...tabExtra }] });
  if (live) {
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 3, state: {
      ...initialState(), status: { kind: "running" }, backend: "sidecar", transcript: [{ seq: 0, text: "Prior answer" }],
      capabilities: { ...initialState().capabilities, planMode: true }, planning: planning("executing", null), ...overrides,
    } });
  }
  dispatch({ kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [], panel: PANEL, newTabChord: "Ctrl+b c" });
  dispatch({ kind: "pane_focus", focused: true });
  act(() => widen(view.container));
  const root = view.container.querySelector<HTMLElement>(".agent-ui-conversation") ?? view.container.firstElementChild as HTMLElement;
  const press = (key: string, props = {}, target: Element = document.activeElement ?? root) => {
    fireEvent.keyDown(target, { key, ...props }); fireEvent.keyUp(target, { key, ...props });
  };
  return { ...view, root, press };
}
function propose(from = 3) {
  dispatch({ kind: "events", tab: 1, fromRevision: from, throughRevision: from + 1, events: [{
    type: "plan_proposed", interaction_id: "p1", tool_use_id: "toolu_exit", plan: plan.plan, plan_sha256: SHA,
    allowed_prompts: plan.allowedPrompts, unsupported_reason: null,
  }] });
}
const resolves = () => messages.filter((m) => m.type === "plan_resolve");
const switches = () => messages.filter((m) => m.type === "set_planning");
const pill = (c: HTMLElement) => c.querySelector('[data-testid="mode-pill"]')?.textContent;
const flash = (c: HTMLElement) => { act(() => widen(c)); return c.querySelector(".band-message")?.textContent; };

describe("plan mode in the page", () => {
  it("the dock replaces the composer and gives it back after settlement", () => {
    const { container, press, root } = setup({ planning: planning("planning") });
    press("i", {}, root);
    const composer = container.querySelector<HTMLTextAreaElement>('[aria-label="Message the agent"]')!;
    fireEvent.change(composer, { target: { value: "kept draft" } });
    propose();
    expect(container.querySelector(".plan-dock")).toBeTruthy();
    expect(composer.closest("[hidden]")).toBeTruthy();
    expect(document.activeElement?.closest(".plan-dock")).toBeTruthy();
    expect(container.querySelector(".plan-dock .plan-body h1")?.textContent).toBe("Ship it");
    expect(container.textContent).toContain("Claude says it will need");
    pause(300); press("l"); pause(300); press("Enter");
    expect(resolves()).toEqual([{ type: "plan_resolve", request_id: expect.any(String), tab: 1, interaction_id: "p1", choice: "approve_manual", plan_sha256: SHA }]);
    dispatch({ kind: "command_result", requestId: resolves()[0].request_id, ok: true });
    dispatch({ kind: "events", tab: 1, fromRevision: 4, throughRevision: 5, events: [{
      type: "plan_settled", interaction_id: "p1", outcome: "approved", mode_after: "manual", feedback: null, reason: null,
    }], planningChanged: planning("leaving") });
    expect(container.querySelector(".plan-dock")).toBeNull();
    const restored = container.querySelector<HTMLTextAreaElement>('[aria-label="Message the agent"]')!;
    expect(restored.closest("[hidden]")).toBeNull();
    expect(restored.value).toBe("kept draft");
    // The plan's row says how it ended.
    expect(container.textContent).toContain("approved · manual");
  });
  it("Ctrl+y, card-mode keys and Shift+Tab into Bypass never answer a plan", () => {
    const { container, press, root } = setup({ plans: [plan], planning: planning("planning", "bypass") }, { mode: "bypass" });
    pause(300); press("i", {}, root);
    const dock = container.querySelector(".plan-dock")!;
    expect(dock.querySelector('[data-choice="approve_bypass"]')).toBeTruthy();
    pause(300); press("y", { ctrlKey: true }, dock);
    pause(300); press("n", { altKey: true }, dock);
    pause(300); press("1"); press("a"); press("y");
    pause(300); press("Tab", { shiftKey: true }, dock);
    expect(messages.some((m) => m.type === "cycle_mode")).toBe(false);
    pause(300); press("Escape", {}, dock); pause(300); press("y", { ctrlKey: true }, root);
    // In BROWSE, Shift+Tab asks the host, which refuses while plan mode holds the tab; nothing answers the plan.
    pause(300); press("Tab", { shiftKey: true }, root);
    expect(resolves()).toEqual([]);
    expect(messages.some((m) => m.type === "permission_response" || m.type === "confirm_bypass_answer")).toBe(false);
    expect(container.querySelector(".plan-dock")).toBeTruthy();
  });
  it("Shift+Tab while planning flashes the host's refusal, not the banner", () => {
    const { container, press, root } = setup({ planning: planning("planning") });
    pause(300); press("Tab", { shiftKey: true }, root);
    const cycle = messages.find((m) => m.type === "cycle_mode");
    expect(cycle).toBeTruthy();
    dispatch({ kind: "command_result", requestId: cycle!.request_id, ok: false, error: "in plan mode: <leader>p leaves plan mode" });
    expect(flash(container)).toBe("in plan mode: <leader>p leaves plan mode");
    expect(container.querySelector(".command-notice, .agent-ui-notice")?.textContent ?? "").not.toContain("in plan mode");
  });
  it("<leader>p enters and leaves; /plan enters", () => {
    const { container, press, root } = setup();
    pause(300); press(" ", {}, root); press("p", {}, root);
    expect(switches()).toEqual([{ type: "set_planning", request_id: expect.any(String), tab: 1, enter: true }]);
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 3, events: [], planningChanged: planning("entering") });
    expect(pill(container)).toBe("⏸ entering plan…");
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 4, events: [{ type: "cli_plan_mode", source: "CliModeReported" }], planningChanged: planning("planning") });
    expect(container.textContent).toContain("entered plan mode");
    pause(300); press(" ", {}, root); press("p", {}, root);
    expect(switches()[1]).toMatchObject({ enter: false });
    dispatch({ kind: "events", tab: 1, fromRevision: 4, throughRevision: 4, events: [], planningChanged: planning("leaving") });
    dispatch({ kind: "events", tab: 1, fromRevision: 4, throughRevision: 5, events: [{ type: "cli_permission_mode", reported: "auto" }], planningChanged: planning("executing", null) });
    expect(container.textContent).toContain("left plan mode");
    expect(pill(container)).toBe("⏵⏵ auto");
    // A refusal is a flash.
    dispatch({ kind: "command_result", requestId: switches()[1].request_id, ok: false, error: "plan mode ends when the tab is idle" });
    expect(flash(container)).toBe("plan mode ends when the tab is idle");
    // `/plan` from the box.
    pause(300); press("i", {}, root);
    const composer = container.querySelector<HTMLTextAreaElement>('[aria-label="Message the agent"]')!;
    fireEvent.change(composer, { target: { value: "/plan" } });
    pause(300); press("Enter", {}, composer);
    expect(switches()[2]).toMatchObject({ enter: true });
    expect(messages.some((m) => m.type === "send_message")).toBe(false);
  });
  it("<leader>p without plan mode says why; an empty sidecar tab is asked to start in plan mode", () => {
    const { container, press, root, unmount } = setup({ capabilities: initialState().capabilities, planning: null });
    pause(300); press(" ", {}, root); press("p", {}, root);
    expect(switches()).toEqual([]);
    expect(flash(container)).toBe("plan mode needs a Claude tab on a sidecar that offers it");
    unmount();
    const empty = setup({}, {}, false);
    dispatch({ kind: "arrive" });
    const start = empty.container.querySelector(".empty-tab")!;
    pause(300);
    empty.press(" ", {}, start); empty.press("p", {}, start);
    expect(switches()).toEqual([{ type: "set_planning", request_id: expect.any(String), tab: 1, enter: true }]);
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...tab, state: "not_started", planOnStart: true }] });
    expect(pill(empty.container)).toBe("⏸ plan on start · from auto");
  });
  it("reload restores the dock and its feedback draft from the snapshot", () => {
    const { container, press, root, getByLabelText } = setup({ plans: [plan], planning: planning("planning") }, { planFeedbackDraft: { p1: "fewer files" } });
    expect(container.querySelector(".plan-dock")).toBeTruthy();
    expect((getByLabelText("What to change in the plan (optional)") as HTMLTextAreaElement).value).toBe("fewer files");
    pause(300); press("i", {}, root);
    press("Enter", { ctrlKey: true }, getByLabelText("What to change in the plan (optional)"));
    expect(resolves()[0]).toMatchObject({ choice: "keep_planning", feedback: "fewer files" });
    // The page-only rows of plan-mode moves are not in a snapshot.
    expect(container.textContent).not.toContain("entered plan mode");
  });
  it("the badge shows plan with its origin and the transition", () => {
    const { container } = setup({ planning: planning("planning", "auto") });
    expect(pill(container)).toBe("⏸ plan · from auto");
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 3, events: [], planningChanged: planning("leaving") });
    expect(pill(container)).toBe("⏸ leaving plan…");
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 3, events: [], planningChanged: planning("executing", null) });
    expect(pill(container)).toBe("⏵⏵ auto");
    // A session that stops offering plan mode.
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 3, events: [], planningChanged: null });
    expect(pill(container)).toBe("⏵⏵ auto");
  });
  it("the activity line says the plan needs review, never a card key", () => {
    const { container } = setup({ plans: [plan], planning: planning("planning") });
    const line = container.querySelector(".activity-line")?.textContent ?? "";
    expect(line).toContain("plan needs review");
    expect(line).not.toMatch(/ctrl\+y|Ctrl\+y/);
  });
  it("a question waiting first keeps the dock until it settles", () => {
    const { container } = setup({ questions: [question], plans: [{ ...plan, seq: 3 }], planning: planning("planning") });
    expect(container.querySelector(".question-dock")).toBeTruthy();
    expect(container.querySelector(".plan-dock")).toBeNull();
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 4, events: [{ type: "question_settled", interaction_id: "q1", outcome: "answered", answers: [], reason: null }] });
    expect(container.querySelector(".question-dock")).toBeNull();
    expect(container.querySelector(".plan-dock")).toBeTruthy();
  });
  it("a double Enter on the plan's row from BROWSE decides nothing, and the dock comes back on Keep planning", () => {
    const { container, press, root } = setup({ plans: [plan], planning: planning("planning", "bypass") }, { mode: "bypass" });
    pause(300); press("i", {}, root);
    for (let i = 0; i < 3; i++) { pause(300); press("l"); }
    expect(container.querySelector('.plan-action[data-focused="true"]')?.getAttribute("data-choice")).toBe("approve_bypass");
    pause(300); press("Escape");
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
    expect(container.querySelector('.plan-action[data-focused="true"]')?.getAttribute("data-choice")).toBe("keep_planning");
    pause(1000); press("G", {}, root); pause(600);
    press("Enter", {}, root); press("Enter");
    expect(resolves()).toEqual([]);
    expect(document.activeElement?.closest(".plan-dock")).toBeTruthy();
    // The same for `i` then Enter.
    pause(300); press("Escape"); pause(1000); press("i", {}, root); press("Enter");
    expect(resolves()).toEqual([]);
  });
  it("a key held as the plan arrives decides nothing until it is released", () => {
    const { container, press, root } = setup({ planning: planning("planning") });
    press("i", {}, root);
    const composer = container.querySelector("textarea")!;
    fireEvent.keyDown(composer, { key: "a", code: "KeyA" });
    propose();
    const dock = container.querySelector(".plan-dock")!;
    // Long past the dock's own paint-and-key guard (the paint lands on the first frame): only the held key holds it.
    pause(300); pause(300); press("Enter", {}, dock);
    expect(resolves()).toEqual([]);
    expect(container.querySelector(".plan-feedback")).toBeNull();
    fireEvent.keyUp(dock, { key: "a", code: "KeyA" });
    pause(300); press("Enter", {}, dock);
    expect(container.querySelector(".plan-feedback")).toBeTruthy();
  });
  it("an approval waiting for calls says so in the dock", () => {
    const { container, press, root } = setup({ plans: [plan], planning: planning("planning") });
    pause(300); press("i", {}, root); pause(300); press("Tab"); pause(300); press("Tab"); pause(300); press("Enter");
    const sent = resolves()[0];
    expect(sent).toMatchObject({ choice: "approve_auto" });
    dispatch({ kind: "plan_waiting", requestId: sent.request_id, tab: 1, calls: 1 });
    expect(container.querySelector(".plan-footer")?.textContent).toBe("waiting for 1 call to finish");
  });
});

it.each(["n", "p"])("Alt+%s in plan feedback enters card mode without answering", (key) => {
  const { container, press, root, getByLabelText } = setup({ plans: [plan], planning: planning("planning"),
    pendingPermissions: [{ seq: 3, permissionId: "perm", toolUseId: "call", toolName: "Bash", input: { command: "pwd" } }],
  }, { planFeedbackDraft: { p1: "fewer files" } });
  pause(300); press("i", {}, root);
  const field = getByLabelText("What to change in the plan (optional)");
  act(() => field.focus());
  pause(300); press(key, { altKey: true }, field);
  expect(container.querySelector(".permission-card-mode")).toBeTruthy();
  expect(resolves()).toEqual([]);
  expect(messages.some((m) => m.type === "permission_response")).toBe(false);
});

it("Ctrl+y in plan feedback explains the refusal", () => {
  const { container, press, root, getByLabelText } = setup({ plans: [plan], planning: planning("planning") },
    { planFeedbackDraft: { p1: "fewer files" } });
  pause(300); press("i", {}, root);
  const field = getByLabelText("What to change in the plan (optional)");
  act(() => field.focus());
  pause(300); press("y", { ctrlKey: true }, field);
  expect(flash(container)).toBe("Ctrl+y: cards are on Alt-n now");
  expect(resolves()).toEqual([]);
});

it("the hidden composer cannot change mode while a plan is docked", () => {
  const { container } = setup({ plans: [plan], planning: planning("planning") });
  const hint = container.querySelector(".composer-browse-hint")!;
  fireEvent.focus(hint);
  expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("browse");
});

it("the hidden composer does not query paths while a plan is docked", () => {
  const { container, press, root } = setup({ planning: planning("planning") });
  pause(300); press("i", {}, root);
  const composer = container.querySelector<HTMLTextAreaElement>('[aria-label="Message the agent"]')!;
  propose();
  fireEvent.change(composer, { target: { value: "@src" } });
  composer.setSelectionRange(4, 4);
  fireEvent.select(composer);
  pause(100);
  expect(messages.filter((m) => m.type === "path_complete")).toEqual([]);
  expect(container.querySelector(".at-menu")).toBeNull();
});
