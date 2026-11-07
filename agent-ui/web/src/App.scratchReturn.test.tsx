// @vitest-environment jsdom
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { initialState } from "./reducer";
import type { QuestionRecord } from "./types";

let posted: Record<string, unknown>[];
const tab = { id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, title: null, agent: "claude" };
const otherTab = { ...tab, id: 2, number: 2, label: "2 other" };
const question: QuestionRecord = { seq: 2, interactionId: "q1", toolUseId: null, turnId: "t1",
  unsupportedReason: null, state: { kind: "waiting" }, questions: [{ question: "Which database?",
    header: "Database", multiSelect: false, options: [{ label: "SQLite", description: "One file", preview: null }] }] };

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["Date", "performance", "setTimeout", "clearTimeout", "requestAnimationFrame", "cancelAnimationFrame"] });
  posted = [];
  window.webkit = { messageHandlers: { eitriAgent: { postMessage: (raw) => posted.push(JSON.parse(raw)) } } };
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.useRealTimers(); });
function dispatch(payload: unknown) { act(() => window.__eitriDispatch!(JSON.stringify(payload))); }
function pause() { act(() => vi.advanceTimersByTime(400)); }
function press(target: Element, key: string, props = {}) {
  fireEvent.keyDown(target, { key, ...props });
  fireEvent.keyUp(target, { key, ...props });
}
function setup(empty = false, questions: QuestionRecord[] = []) {
  const { container } = render(<App />);
  dispatch({ kind: "hello", backend: "sidecar", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"], resumableSessions: [], account: null });
  dispatch({ kind: "tabs", active: 1, tabs: [{ ...tab, state: empty ? "not_started" : "live" }, otherTab] });
  if (!empty) dispatch({ kind: "snapshot", tab: 1, throughRevision: 3, state: {
    ...initialState(), status: { kind: "running" }, transcript: [{ seq: 0, text: "Prior answer" }], questions,
  } });
  dispatch({ kind: "pane_focus", focused: true });
  dispatch({ kind: "enter_input" });
  pause();
  return container;
}
function composer(container: HTMLElement) {
  return container.querySelector<HTMLTextAreaElement>('[aria-label="Message the agent"]');
}
function mode(container: HTMLElement) {
  return container.querySelector('[data-testid="mode-block"]')?.getAttribute("data-mode");
}
function leaveForEdit(field: HTMLTextAreaElement) {
  press(field, "g", { ctrlKey: true });
  dispatch({ kind: "scratch", tab: 1, editing: true });
  act(() => field.blur());
  dispatch({ kind: "pane_focus", focused: false });
}
function finishEdit(text: string | null, questionEdit = false, arrive = true) {
  // finish_scratch_edit sends the returned text before clearing scratch; :q! sends no text.
  if (text !== null) dispatch(questionEdit
    ? { kind: "question_other_edited", tab: 1, interaction_id: "q1", question_index: 0, text }
    : { kind: "draft", tab: 1, text });
  dispatch({ kind: "scratch", tab: 1, editing: false });
  dispatch({ kind: "pane_focus", focused: true });
  // EditorDone uses focus_and_arrive; waiting questions count toward its card-first landing too.
  if (arrive) dispatch(questionEdit ? { kind: "focus_permission", tab: 1 } : { kind: "arrive", intent: "show" });
}
const sends = () => posted.filter((m) => m.type === "send_message");

describe("composer scratch return", () => {
  it.each([false, true])("saved draft returns to INPUT at its end and Enter sends (empty=%s)", (empty) => {
    const container = setup(empty);
    const field = composer(container)!;
    fireEvent.change(field, { target: { value: "original draft" } });
    field.setSelectionRange(2, 2); fireEvent.select(field);
    leaveForEdit(field);
    expect(posted.find((m) => m.type === "edit_draft")).toMatchObject({ tab: 1, text: "original draft" });
    finishEdit("edited draft\nlast line");
    expect(mode(container)).toBe("input");
    const returned = composer(container)!;
    expect(returned.value).toBe("edited draft\nlast line");
    expect(document.activeElement).toBe(returned);
    expect([returned.selectionStart, returned.selectionEnd]).toEqual([returned.value.length, returned.value.length]);
    expect(sends()).toHaveLength(0);
    press(returned, "Enter");
    expect(sends()).toEqual([expect.objectContaining({ tab: 1, text: "edited draft\nlast line" })]);
  });

  it.each([false, true])(":q! restores INPUT at the unchanged draft's end (empty=%s)", (empty) => {
    const container = setup(empty);
    const field = composer(container)!;
    fireEvent.change(field, { target: { value: "unchanged draft" } });
    field.setSelectionRange(2, 2); fireEvent.select(field);
    leaveForEdit(field);
    finishEdit(null);
    expect(mode(container)).toBe("input");
    const returned = composer(container)!;
    expect(returned.value).toBe("unchanged draft");
    expect(document.activeElement).toBe(returned);
    expect([returned.selectionStart, returned.selectionEnd]).toEqual([15, 15]);
    expect(sends()).toHaveLength(0);
    press(returned, "Enter");
    expect(sends()).toEqual([expect.objectContaining({ tab: 1, text: "unchanged draft" })]);
  });

  it.each([false, true])("a batched return restores INPUT (empty=%s)", (empty) => {
    const container = setup(empty);
    leaveForEdit(composer(container)!);
    act(() => finishEdit("batched draft"));
    expect(mode(container)).toBe("input");
    const field = composer(container)!;
    expect(field.value).toBe("batched draft");
    expect(document.activeElement).toBe(field);
    expect([field.selectionStart, field.selectionEnd]).toEqual([13, 13]);
    expect(sends()).toHaveLength(0);
  });

  it.each([false, true])("the held Enter of :wq, repeating into the box, sends nothing; a fresh Enter does (empty=%s)", (empty) => {
    const container = setup(empty);
    const field = composer(container)!;
    fireEvent.change(field, { target: { value: "a draft" } });
    leaveForEdit(field);
    finishEdit("a draft, edited");
    const returned = composer(container)!;
    expect(mode(container)).toBe("input");
    fireEvent.keyDown(returned, { key: "Enter", repeat: true });
    fireEvent.keyDown(returned, { key: "Enter", repeat: true });
    fireEvent.keyUp(returned, { key: "Enter" });
    expect(sends()).toHaveLength(0);
    expect(composer(container)!.value).toBe("a draft, edited");
    pause();
    press(composer(container)!, "Enter");
    expect(sends()).toEqual([expect.objectContaining({ tab: 1, text: "a draft, edited" })]);
  });

  it("the return is an arrival: a Ctrl+y right as the keys come back approves nothing", () => {
    const container = setup();
    leaveForEdit(composer(container)!);
    finishEdit("edited draft");
    expect(mode(container)).toBe("input");
    // A card that came while nvim had the draft, with no landing of its own.
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 4, events: [{
      type: "permission_requested", permission_id: "p1", tool_use_id: null, tool_name: "Bash", input: { command: "rm file" },
    }] });
    const field = composer(container)!;
    fireEvent.keyDown(field, { key: "y", code: "KeyY", ctrlKey: true });
    fireEvent.keyUp(field, { key: "y", code: "KeyY", ctrlKey: true });
    act(() => vi.advanceTimersByTime(1000));
    expect(posted.filter((m) => m.type === "permission_response")).toHaveLength(0);
  });

  it("a composer edit returning while a permission card waits lands on the card in BROWSE and sends nothing", () => {
    const container = setup();
    const field = composer(container)!;
    fireEvent.change(field, { target: { value: "a draft" } });
    leaveForEdit(field);
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 4, events: [{
      type: "permission_requested", permission_id: "p1", tool_use_id: null, tool_name: "Bash", input: { command: "rm file" },
    }] });
    dispatch({ kind: "draft", tab: 1, text: "a draft, edited" });
    dispatch({ kind: "scratch", tab: 1, editing: false });
    dispatch({ kind: "pane_focus", focused: true });
    // A key between the keys coming back and the card landing answers nothing either.
    fireEvent.keyDown(document.activeElement ?? container, { key: "Enter" });
    dispatch({ kind: "focus_permission", tab: 1 });
    act(() => vi.advanceTimersByTime(1000));
    expect(mode(container)).toBe("browse");
    expect(container.querySelector(".row-current")?.textContent ?? "").toContain("rm file");
    expect(posted.filter((m) => m.type === "permission_response" || m.type === "send_message")).toHaveLength(0);
    expect(composer(container)).toBeNull();
  });

  it("a composer edit returning while an editor card waits gives the return up and sends nothing", () => {
    const container = setup();
    const field = composer(container)!;
    fireEvent.change(field, { target: { value: "a draft" } });
    leaveForEdit(field);
    dispatch({ kind: "editor_cards", tab: 1, cards: [{
      id: "ec-0123456789abcdef-1", seq: 1, tool: "open_file", toolUseId: null, title: "Show /etc/hosts in your editor?",
      body: [{ kind: "code", text: "/etc/hosts" }],
    }] });
    dispatch({ kind: "draft", tab: 1, text: "a draft, edited" });
    dispatch({ kind: "scratch", tab: 1, editing: false });
    dispatch({ kind: "pane_focus", focused: true });
    expect(composer(container)).toBeNull();
    fireEvent.keyDown(document.activeElement ?? container, { key: "Enter" });
    dispatch({ kind: "focus_permission", tab: 1 });
    act(() => vi.advanceTimersByTime(1000));
    expect(mode(container)).toBe("browse");
    expect(posted.filter((m) => m.type === "send_message" || m.type === "editor_card_answer")).toHaveLength(0);
  });

  it("a composer edit returning while a question waits gives the return up and sends nothing", () => {
    const container = setup();
    const field = composer(container)!;
    fireEvent.change(field, { target: { value: "a draft" } });
    leaveForEdit(field);
    // The question came while nvim had the draft.
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 5, state: {
      ...initialState(), status: { kind: "running" }, transcript: [{ seq: 0, text: "Prior answer" }], questions: [question],
    } });
    dispatch({ kind: "draft", tab: 1, text: "a draft, edited" });
    dispatch({ kind: "scratch", tab: 1, editing: false });
    dispatch({ kind: "pane_focus", focused: true });
    expect(mode(container)).toBe("browse");
    fireEvent.keyDown(document.activeElement ?? container, { key: "Enter" });
    dispatch({ kind: "focus_permission", tab: 1 });
    act(() => vi.advanceTimersByTime(1000));
    expect(posted.filter((m) => m.type === "send_message" || m.type === "question_answer")).toHaveLength(0);
  });

  it("pane focus alone resumes INPUT and a later ordinary arrival still lands BROWSE", () => {
    const container = setup();
    leaveForEdit(composer(container)!);
    finishEdit("draft", false, false);
    expect(mode(container)).toBe("input");
    press(composer(container)!, "ArrowLeft");
    dispatch({ kind: "arrive", intent: "show" });
    expect(mode(container)).toBe("browse");
    expect(sends()).toHaveLength(0);
  });

  it.each([false, true])("a switch during the edit keeps the other tab in BROWSE (empty=%s)", (empty) => {
    const container = setup(empty);
    leaveForEdit(composer(container)!);
    dispatch({ kind: "tabs", active: 2, tabs: [tab, otherTab] });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 0, state: { ...initialState(), status: { kind: "running" } } });
    dispatch({ kind: "draft", tab: 2, text: "other draft" });
    finishEdit("edited first tab");
    expect(mode(container)).toBe("browse");
    expect(composer(container)).toBeNull();
    expect(sends()).toHaveLength(0);
  });

  it("an arrival during the edit keeps its BROWSE landing", () => {
    const container = setup();
    leaveForEdit(composer(container)!);
    dispatch({ kind: "arrive" });
    finishEdit("edited draft");
    expect(mode(container)).toBe("browse");
    expect(sends()).toHaveLength(0);
  });

  it("an overlay opened during the edit keeps its current mode", () => {
    const container = setup();
    leaveForEdit(composer(container)!);
    dispatch({ kind: "open_keymap", opening: 7 });
    expect(container.querySelector(".keymap-overlay")).not.toBeNull();
    finishEdit("edited draft");
    expect(container.querySelector(".keymap-overlay")).toBeNull();
    expect(mode(container)).toBe("browse");
    expect(composer(container)).toBeNull();
    expect(sends()).toHaveLength(0);
  });

  it("BROWSE's read-only row view returns to BROWSE", () => {
    const container = setup();
    press(composer(container)!, "Escape");
    pause();
    const root = container.querySelector(".agent-ui-conversation")!;
    press(root, "g", { ctrlKey: true });
    expect(posted.some((m) => m.type === "view_in_editor")).toBe(true);
    dispatch({ kind: "pane_focus", focused: false });
    dispatch({ kind: "pane_focus", focused: true });
    expect(mode(container)).toBe("browse");
    expect(sends()).toHaveLength(0);
  });
});

describe("question answer scratch return", () => {
  it.each(["edited answer", null])("returns to Type something at the end (%s)", (text) => {
    const container = setup(false, [question]);
    const dock = container.querySelector(".question-dock")!;
    press(dock, "2");
    const field = container.querySelector<HTMLTextAreaElement>('[aria-label="Type something"]')!;
    fireEvent.change(field, { target: { value: "original answer" } });
    act(() => field.focus());
    field.setSelectionRange(2, 2);
    leaveForEdit(field);
    expect(posted.find((m) => m.type === "edit_question_other")).toMatchObject({ tab: 1, question_index: 0, text: "original answer" });
    finishEdit(text, true);
    const returned = container.querySelector<HTMLTextAreaElement>('[aria-label="Type something"]')!;
    expect(mode(container)).toBe("input");
    expect(returned.value).toBe(text ?? "original answer");
    expect(document.activeElement).toBe(returned);
    expect([returned.selectionStart, returned.selectionEnd]).toEqual([returned.value.length, returned.value.length]);
    expect(posted.filter((m) => m.type === "question_answer")).toHaveLength(0);
    // The return is an arrival: an Enter still being pressed as the keys came back answers nothing.
    press(returned, "Enter");
    expect(posted.filter((m) => m.type === "question_answer")).toHaveLength(0);
    pause();
    press(returned, "Enter");
    expect(posted.filter((m) => m.type === "question_answer")).toEqual([
      expect.objectContaining({ tab: 1, answers: [{ question_index: 0, option_indexes: [], other_text: text ?? "original answer", notes: null }] }),
    ]);
  });

  it("a fresh Enter between the keys coming back and the landing answers nothing", () => {
    const container = setup(false, [question]);
    press(container.querySelector(".question-dock")!, "2");
    const field = container.querySelector<HTMLTextAreaElement>('[aria-label="Type something"]')!;
    fireEvent.change(field, { target: { value: "original answer" } });
    act(() => field.focus());
    leaveForEdit(field);
    dispatch({ kind: "question_other_edited", tab: 1, interaction_id: "q1", question_index: 0, text: "edited answer" });
    dispatch({ kind: "scratch", tab: 1, editing: false });
    dispatch({ kind: "pane_focus", focused: true });
    const returned = container.querySelector<HTMLTextAreaElement>('[aria-label="Type something"]')!;
    expect(document.activeElement).toBe(returned);
    fireEvent.keyDown(returned, { key: "Enter" });
    fireEvent.keyUp(returned, { key: "Enter" });
    dispatch({ kind: "focus_permission", tab: 1 });
    act(() => vi.advanceTimersByTime(1000));
    expect(posted.filter((m) => m.type === "question_answer")).toHaveLength(0);
  });

  it("a permission card waiting on return keeps its BROWSE landing", () => {
    const container = setup(false, [question]);
    press(container.querySelector(".question-dock")!, "2");
    leaveForEdit(container.querySelector<HTMLTextAreaElement>('[aria-label="Type something"]')!);
    dispatch({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 4, events: [{
      type: "permission_requested", permission_id: "p1", tool_use_id: null, tool_name: "Bash", input: { command: "rm file" },
    }] });
    finishEdit("edited answer", true);
    expect(mode(container)).toBe("browse");
    expect(posted.filter((m) => m.type === "question_answer" || m.type === "permission_response")).toHaveLength(0);
  });
});
