// @vitest-environment jsdom
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { initialState } from "./reducer";
import { bandLayout } from "./band";
import type { AgentUiState, QuestionRecord } from "./types";

const question: QuestionRecord = { seq: 2, interactionId: "q1", toolUseId: null, turnId: "t1", unsupportedReason: null, state: { kind: "waiting" }, questions: [{
  question: "Which database?", header: "Database", multiSelect: false,
  options: [{ label: "Postgres", description: "Relational", preview: null }, { label: "SQLite", description: "One file", preview: null }],
}] };
const tab = { id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: true, failure: null, title: null };
let messages: Record<string, unknown>[];
let now: number;
beforeEach(() => {
  vi.useFakeTimers();
  messages = []; now = 1000;
  vi.spyOn(performance, "now").mockImplementation(() => now);
  window.webkit = { messageHandlers: { eitriAgent: { postMessage: (m) => messages.push(JSON.parse(m)) } } };
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.useRealTimers(); });
function pause(ms: number) { act(() => { now += ms; vi.advanceTimersByTime(ms); }); }
function dispatch(payload: unknown) { act(() => window.__eitriDispatch!(JSON.stringify(payload))); }
function setup(overrides: Partial<AgentUiState> = {}, drafts?: unknown) {
  const view = render(<App />);
  dispatch({ kind: "hello", backend: "sidecar", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], expectedVerdandiRevision: null, account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [{ ...tab, questionDraft: drafts }] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 3, state: { ...initialState(), status: { kind: "running" }, transcript: [{ seq: 0, text: "Prior answer" }], ...overrides } });
  dispatch({ kind: "pane_focus", focused: true });
  const root = view.container.querySelector(".agent-ui-conversation")!;
  const press = (key: string, props = {}, target: Element = document.activeElement ?? root) => {
    fireEvent.keyDown(target, { key, ...props }); fireEvent.keyUp(target, { key, ...props });
  };
  return { ...view, root, press };
}
function ask() { dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 4, events: [{ type: "question_asked", interaction_id: "q1", tool_use_id: null, questions: question.questions.map((q) => ({ ...q, multi_select: q.multiSelect })), unsupported_reason: null }] }); }
const answers = () => messages.filter((m) => m.type === "question_answer" || m.type === "question_decline");

describe("questions in the page", () => {
  it("replaces the composer, then restores its text and caret after settlement", () => {
    const { container, press, root } = setup();
    press("i", {}, root);
    const composer = container.querySelector<HTMLTextAreaElement>('[aria-label="Message the agent"]')!;
    fireEvent.change(composer, { target: { value: "preserved draft" } });
    composer.setSelectionRange(4, 4); fireEvent.select(composer);
    ask();
    expect(container.querySelector(".question-dock")).toBeTruthy();
    expect(composer.closest("[hidden]")).toBeTruthy();
    pause(300); press("Enter");
    expect(answers()).toHaveLength(1);
    dispatch({ kind: "events", tab: 1, fromRevision: 4, throughRevision: 5, events: [{ type: "question_settled", interaction_id: "q1", outcome: "answered", answers: [], reason: null }] });
    const restored = container.querySelector<HTMLTextAreaElement>('[aria-label="Message the agent"]')!;
    expect(restored.value).toBe("preserved draft");
    expect(restored.selectionStart).toBe(4);
    expect(document.activeElement).toBe(restored);
    expect(container.querySelector(".question-dock")).toBeNull();
  });
  it("keeps BROWSE navigation and i enters the dock without answering", () => {
    const { container, press, root, getByText } = setup({ questions: [question] });
    expect(getByText("⚑ Claude has a question — i to answer")).toBeTruthy();
    press("j", {}, root); expect(answers()).toHaveLength(0);
    pause(300); press("i", {}, root);
    expect(document.activeElement?.closest(".question-dock")).toBeTruthy();
    expect(answers()).toHaveLength(0);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("input");
  });
  it("Enter on the question transcript row enters the dock", () => {
    const { container, press, root } = setup({ questions: [question] });
    press("G", {}, root); pause(600);
    press("Enter", {}, root);
    expect(container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode")).toBe("input");
    expect(answers()).toHaveLength(0);
  });
  it("swallows answer keys for 250 ms and until a key held before arrival is released", () => {
    const { container, press, root } = setup(); press("i", {}, root);
    const composer = container.querySelector("textarea")!;
    fireEvent.keyDown(composer, { key: "a", code: "KeyA" }); ask();
    const dock = container.querySelector(".question-dock")!;
    press("Enter", {}, dock); expect(answers()).toHaveLength(0);
    pause(300); press("Enter", {}, dock); expect(answers()).toHaveLength(0);
    fireEvent.keyUp(dock, { key: "a", code: "KeyA" });
    press("Enter", { repeat: true }, dock); expect(answers()).toHaveLength(0);
    press("Enter", {}, dock); expect(answers()).toHaveLength(1);
  });
  it("reload restores the dock, page and draft through the tab envelope and snapshot", () => {
    const { container, press, root } = setup({ questions: [question] }, { q1: { page: 0, focus: 2, answers: [{ options: [], other: true, text: "restored alternative" }] } });
    expect((container.querySelector('.question-dock textarea') as HTMLTextAreaElement).value).toBe("restored alternative");
    pause(300); press("i", {}, root); press("Enter", { ctrlKey: true });
    expect(answers()[0]).toMatchObject({ answers: [{ other_text: "restored alternative" }] });
  });
  it("a refused answer leaves the dock's pick and text for retry", () => {
    const { container, press, root, getByText } = setup({ questions: [question] });
    pause(300); press("i", {}, root); press("3");
    const field = container.querySelector('.question-dock textarea')!;
    fireEvent.change(field, { target: { value: "mine" } });
    press("Enter", { ctrlKey: true });
    dispatch({ kind: "command_result", requestId: answers()[0].request_id, ok: false, error: "Try again" });
    expect(getByText("Try again")).toBeTruthy();
    expect((field as HTMLTextAreaElement).value).toBe("mine");
    press("Enter", { ctrlKey: true }); expect(answers()).toHaveLength(2);
    expect(answers()[1].answers).toEqual(answers()[0].answers);
  });
  const drafts = (tabId: number) => messages.filter((m) => m.type === "question_draft" && m.tab === tabId);
  it("a move to the other window sends the question's unmirrored draft before its answer to view_export", () => {
    const { container, press, root } = setup({ questions: [question] });
    pause(300); press("i", {}, root);
    pause(400); messages.length = 0;
    press("2", {}, container.querySelector(".question-dock")!);
    // Asked inside the mirror's delay, as a move right after a choice is.
    dispatch({ kind: "view_export", request: 21, tab: 1 });
    const draft = messages.findIndex((m) => m.type === "question_draft" && m.tab === 1);
    const answer = messages.findIndex((m) => m.type === "view_exported" && m.request === 21);
    expect(draft).toBeGreaterThanOrEqual(0);
    expect(answer).toBeGreaterThan(draft);
    expect(messages[draft]).toMatchObject({ interaction_id: "q1", draft: { answers: [{ options: [1] }] } });
  });
  it("a question that settles after its answer sends no draft and leaves no error", () => {
    const { container, press, root, queryByText } = setup({ questions: [question] });
    pause(300); press("i", {}, root);
    const dock = container.querySelector(".question-dock")!;
    press("2", {}, dock); press("Enter", {}, dock);
    expect(answers()).toHaveLength(1);
    const sent = String(answers()[0].request_id);
    messages.length = 0;
    dispatch({ kind: "command_result", requestId: sent, ok: true });
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 4, events: [{ type: "question_settled", interaction_id: "q1", outcome: "answered", answers: [], reason: null }] });
    pause(400);
    expect(container.querySelector(".question-dock")).toBeNull();
    expect(drafts(1)).toEqual([]);
    // A draft mirrored just before the settlement reached the host is refused there; the conversation is fine.
    dispatch({ kind: "command_result", requestId: "mirror-9999", ok: false, error: "that question is no longer waiting" });
    expect(queryByText(/no longer waiting/)).toBeNull();
  });
  it("an interrupted question sends no draft as its dock goes", () => {
    const { container, press, root } = setup({ questions: [question] });
    pause(300); press("i", {}, root);
    press("2", {}, container.querySelector(".question-dock")!);
    pause(400); messages.length = 0;
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 4, events: [{ type: "question_settled", interaction_id: "q1", outcome: "interrupted", answers: [], reason: null }] });
    pause(400);
    expect(container.querySelector(".question-dock")).toBeNull();
    expect(drafts(1)).toEqual([]);
  });
  it("a switch away keeps the question's draft with the host; closing its tab sends none", () => {
    const other = { ...tab, id: 2, number: 2, label: "2 other" };
    const switched = setup({ questions: [question] });
    pause(300); switched.press("i", {}, switched.root);
    switched.press("2", {}, switched.container.querySelector(".question-dock")!);
    messages.length = 0;
    dispatch({ kind: "tabs", active: 2, tabs: [tab, other] });
    expect(drafts(1)).toHaveLength(1);
    expect(drafts(1)[0]).toMatchObject({ interaction_id: "q1", draft: { answers: [{ options: [1] }] } });
    cleanup();
    messages = [];
    const closed = setup({ questions: [question] });
    pause(300); closed.press("i", {}, closed.root);
    closed.press("2", {}, closed.container.querySelector(".question-dock")!);
    messages.length = 0;
    dispatch({ kind: "tabs", active: 2, tabs: [other] });
    expect(drafts(1)).toEqual([]);
    dispatch({ kind: "command_result", requestId: "mirror-9998", ok: false, error: "protocol: no tab 1" });
    expect(closed.queryByText(/no tab 1/)).toBeNull();
  });
  it("a draft the host cannot keep is said, so the text is not lost silently", () => {
    const { queryByText } = setup({ questions: [question] });
    dispatch({ kind: "command_result", requestId: "mirror-9997", ok: false, error: "the question draft exceeds 64 KiB" });
    expect(queryByText(/exceeds 64 KiB/)).not.toBeNull();
  });
  it("shows question attention in the band count and uses the host's tab marker", () => {
    const { container } = setup({ questions: [question] });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...tab, marker: "needs_input", pending: 1 }, { ...tab, id: 2, number: 2, label: "2 other" }] });
    expect(container.querySelector(".tab-bar")?.textContent).toContain("⚑");
    const segments = bandLayout({ mode: "browse", pill: "auto", showcmd: null, message: null, prompt: null, warn: null, unread: null, cards: 1, questions: 2, queued: 0, context: null, position: null, model: null, usage: null }, 2000, 8);
    expect(segments.find((s) => s.id === "cards")?.text).toBe("⚑3");
  });
  it.each(["a", "d"])("BROWSE %s answers only a card with a question waiting", async (key) => {
    const { root, press, container } = setup({ questions: [question], pendingPermissions: [{ seq: 4, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "ls" } }] });
    dispatch({ kind: "focus_permission", tab: 1 }); pause(1000);
    press(key, {}, root);
    pause(350);
    expect(answers()).toHaveLength(0);
    expect(container.querySelector(".question-dock")).toBeTruthy();
    expect(messages.some((m) => m.type === "permission_response")).toBe(true);
  });
  it("Ctrl+y in INPUT approves only the card while the dock holds the keys", () => {
    const { press, root } = setup({ questions: [question], pendingPermissions: [{ seq: 4, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "ls" } }] });
    pause(1000); press("i", {}, root); pause(1000); press("y", { ctrlKey: true }); pause(350);
    expect(answers()).toHaveLength(0);
    expect(messages.some((m) => m.type === "permission_response")).toBe(true);
  });
  it("Shift+Tab into Bypass never sends a question answer", () => {
    const { press, root, container } = setup({ questions: [question], pendingPermissions: [{ seq: 4, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "ls" } }] });
    pause(1000); press("Tab", { shiftKey: true }, root);
    expect(messages.some((m) => m.type === "cycle_mode")).toBe(true);
    dispatch({ kind: "confirm_bypass", tab: 1, scope: "tab", nonce: 7, lines: ["Bypass permissions? y/n"] });
    pause(1000); press("y", {}, root);
    expect(messages.some((m) => m.type === "confirm_bypass")).toBe(true);
    expect(answers()).toHaveLength(0);
    expect(container.querySelector(".question-dock")).toBeTruthy();
  });
});
