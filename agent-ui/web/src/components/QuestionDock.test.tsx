// @vitest-environment jsdom
import { createRef, useState } from "react";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { QuestionDock, type QuestionDockHandle } from "./QuestionDock";
import type { QuestionRecord, QuestionSpec } from "../types";

const spec: QuestionSpec = { question: "Which database?", header: "Database", multiSelect: false, options: [
  { label: "Postgres", description: "Relational", preview: null }, { label: "SQLite", description: "One file", preview: null },
] };
const record = (questions = [spec]): QuestionRecord => ({ seq: 1, interactionId: "q1", toolUseId: "t", turnId: "turn", questions, state: { kind: "waiting" }, unsupportedReason: null });
let messages: Record<string, unknown>[];
beforeEach(() => {
  vi.useFakeTimers();
  vi.spyOn(performance, "now").mockImplementation(() => Date.now());
  messages = [];
  window.webkit = { messageHandlers: { eitriAgent: { postMessage: (m) => messages.push(JSON.parse(m)) } } };
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.useRealTimers(); });
function setup(questions = [spec], draft?: unknown, blocked = () => false) {
  const ref = createRef<QuestionDockHandle>();
  const result = render(<QuestionDock ref={ref} tab={1} record={record(questions)} active blocked={blocked} onActivate={() => {}} stillWaiting={() => true} draft={draft} />);
  const dock = result.getByRole("group");
  const press = (key: string, rest = {}) => fireEvent.keyDown(dock, { key, ...rest });
  return { ...result, ref, dock, press };
}
const answers = () => messages.filter((m) => m.type === "question_answer" || m.type === "question_decline");

describe("question dock", () => {
  it("activating from BROWSE keeps focus in the clicked textarea", () => {
    function Page() {
      const [mode, setMode] = useState("browse");
      return <QuestionDock tab={1} record={record()} active={mode === "input"} blocked={() => false}
        onActivate={() => setMode("input")} stillWaiting={() => true}
        draft={{ page: 0, focus: 2, answers: [{ options: [], other: true, text: "draft" }] }} />;
    }
    const { getByRole, getByLabelText } = render(<Page />);
    const dock = getByRole("group");
    const field = getByLabelText("Type something");
    expect(dock.dataset.active).toBe("false");
    act(() => field.focus());
    expect(dock.dataset.active).toBe("true");
    expect(document.activeElement).toBe(field);
    expect(answers()).toEqual([]);
  });

  it("shows the question, bold-ready labels, separate descriptions, first pointer and no initial selection", () => {
    const { container, getByText } = setup();
    expect(getByText("Which database?")).toBeTruthy();
    expect(getByText("Relational").closest(".question-description")).toBeTruthy();
    expect(container.querySelector('.question-option[data-focused="true"]')?.textContent).toContain("Postgres");
    expect(container.querySelectorAll('[aria-pressed="true"]')).toHaveLength(0);
    expect(container.querySelector(".question-head")?.textContent).toBe("Database1/1");
  });
  it("number picks never advance or submit, and a single selection never toggles off", () => {
    const { press, container } = setup();
    press("2"); press("2");
    expect(container.querySelectorAll('[aria-pressed="true"]')).toHaveLength(1);
    expect(container.querySelector('[aria-pressed="true"]')?.textContent).toContain("SQLite");
    expect(answers()).toEqual([]);
    press("Enter");
    expect(answers()[0]).toMatchObject({ type: "question_answer", answers: [{ question_index: 0, option_indexes: [1] }] });
  });
  it("Space selects on keydown without advancing", () => {
    const { press } = setup(); press(" ");
    expect(answers()).toHaveLength(0);
    press("Enter"); expect(answers()).toHaveLength(1);
  });
  it("multi-select toggles options, preserves their order and includes custom text", () => {
    const { press, getByLabelText, getByText } = setup([{ ...spec, multiSelect: true }]);
    expect(getByText("select all that apply")).toBeTruthy();
    press("2"); press("1"); press("2"); press("2"); press("3"); press("Enter");
    fireEvent.change(getByLabelText("Type something"), { target: { value: "MySQL, custom" } });
    press("Tab");
    expect(getByText("Review your answers")).toBeTruthy();
    expect(getByText('Postgres, SQLite, "MySQL, custom"')).toBeTruthy();
    press("Enter");
    expect(answers()[0]).toMatchObject({ answers: [{ option_indexes: [0, 1], other_text: "MySQL, custom" }] });
  });
  it("multi-select Enter toggles while Next advances to the following question", () => {
    const { press, container } = setup([{ ...spec, multiSelect: true }, { ...spec, header: "Second" }]);
    press("Enter"); expect(answers()).toHaveLength(0);
    expect(container.querySelector('[aria-pressed="true"]')).toBeTruthy();
    press("End"); press("Enter");
    expect(container.querySelector(".question-head")?.textContent).toContain("Second");
  });
  it("a multi-select Submit row sends the set once from the last question", () => {
    const { press, getByText } = setup([{ ...spec, multiSelect: true }]);
    press("1"); press("End"); press("Enter"); press("Enter");
    expect(getByText("sending…")).toBeTruthy();
    expect(answers()).toHaveLength(1);
    expect(answers()[0]).toMatchObject({ answers: [{ question_index: 0, option_indexes: [0] }] });
  });
  it("moves up and down with wrap, and switches pages without wrap while preserving picks", () => {
    const { press, container } = setup([spec, { ...spec, header: "Second" }]);
    press("k"); expect(container.querySelector('[data-focused="true"]')?.textContent).toContain("Chat about this");
    press("j"); press("1"); press("l"); press("2"); press("h"); press("h");
    expect(container.querySelector('[aria-pressed="true"]')?.textContent).toContain("Postgres");
    press("ArrowRight"); press("Tab"); press("Tab");
    expect(container.querySelector(".question-head")?.textContent).toContain("Review");
    press("Tab", { shiftKey: true });
    expect(container.querySelector('[aria-pressed="true"]')?.textContent).toContain("SQLite");
    expect(answers()).toHaveLength(0);
  });
  it("single-select Enter advances each page and Review submits the set once", () => {
    const { press, getByText } = setup([spec, { ...spec, header: "Second" }]);
    press("Enter"); press("Enter");
    expect(getByText("Review your answers")).toBeTruthy();
    expect(answers()).toHaveLength(0);
    press("Enter"); press("Enter");
    expect(answers()).toHaveLength(1);
    expect((answers()[0].answers as unknown[]).length).toBe(2);
    expect(getByText("sending…")).toBeTruthy();
  });
  it("Ctrl+Enter opens incomplete Review and its Submit permits missing answers", () => {
    const { press, getByText } = setup([spec, spec]);
    press("1"); press("Enter", { ctrlKey: true });
    expect(getByText("You have not answered all questions")).toBeTruthy();
    expect(answers()).toHaveLength(0);
    press("Enter");
    expect((answers()[0].answers as unknown[])).toHaveLength(1);
  });
  it("Ctrl+Enter on an unanswered single question opens Review without sending", () => {
    const { press, getByText } = setup();
    press("Enter", { ctrlKey: true });
    expect(getByText("Review your answers")).toBeTruthy();
    expect(getByText("You have not answered all questions")).toBeTruthy();
    expect(answers()).toHaveLength(0);
  });

  it("Ctrl+Enter submits a completed set directly", () => {
    const { press } = setup([spec, spec]);
    press("1"); press("Tab"); press("2"); press("Enter", { ctrlKey: true });
    expect(answers()).toHaveLength(1);
  });
  it("Type something focuses a real field; escaping leaves it before cancelling", () => {
    const { press, getByLabelText, dock } = setup();
    press("3"); press("Enter");
    const field = getByLabelText("Type something");
    expect(document.activeElement).toBe(field);
    fireEvent.keyDown(field, { key: "Escape" });
    expect(document.activeElement).toBe(dock);
    expect(answers()).toHaveLength(0);
    press("Escape"); expect(answers()[0]).toMatchObject({ type: "question_decline", kind: "cancel" });
  });
  it("custom text supports single-select replacement, multiline editing and Up/Down field focus", () => {
    const { press, getByLabelText, dock } = setup();
    press("1"); press("3"); press("Enter");
    const field = getByLabelText("Type something") as HTMLTextAreaElement;
    fireEvent.change(field, { target: { value: "custom\nanswer" } });
    fireEvent.keyDown(field, { key: "Enter", shiftKey: true });
    expect(answers()).toHaveLength(0);
    fireEvent.keyDown(field, { key: "ArrowDown" }); expect(document.activeElement).toBe(field);
    field.setSelectionRange(2, 2); fireEvent.keyDown(field, { key: "ArrowUp" });
    expect(document.activeElement).toBe(dock);
    fireEvent.keyDown(field, { key: "Enter" });
    expect(answers()[0]).toMatchObject({ answers: [{ option_indexes: [], other_text: "custom\nanswer" }] });
  });
  it("keeps custom text when selecting an option and restores it when returning", () => {
    const { press, getByLabelText } = setup(); press("3");
    fireEvent.change(getByLabelText("Type something"), { target: { value: "mine" } });
    press("1"); press("3");
    expect((getByLabelText("Type something") as HTMLTextAreaElement).value).toBe("mine");
  });
  it("focusing a retained custom answer selects it before Enter submits", () => {
    const { press, getByLabelText } = setup(); press("3");
    const field = getByLabelText("Type something") as HTMLTextAreaElement;
    fireEvent.change(field, { target: { value: "mine" } });
    press("1");
    act(() => field.focus());
    fireEvent.keyDown(field, { key: "Enter" });
    expect(answers()[0]).toMatchObject({ answers: [{ option_indexes: [], other_text: "mine" }] });
  });

  it("typing on the custom row starts typing; Ctrl+G edits and a scoped reply replaces only custom text", () => {
    const { press, ref, getByLabelText } = setup(); press("3"); press("z");
    expect((getByLabelText("Type something") as HTMLTextAreaElement).value).toBe("z");
    press("g", { ctrlKey: true });
    expect(messages.find((m) => m.type === "edit_question_other")).toMatchObject({ question_index: 0, text: "z", interaction_id: "q1" });
    act(() => { ref.current!.reply({ kind: "question_other_edited", tab: 1, interaction_id: "q1", question_index: 0, text: "edited" }); });
    expect((getByLabelText("Type something") as HTMLTextAreaElement).value).toBe("edited");
    expect(answers()).toHaveLength(0);
  });
  it("Chat declines with the answers so far; its digit alone never declines", () => {
    const { press } = setup(); press("1"); press("4");
    expect(answers()).toHaveLength(0);
    press("Enter"); expect(answers()[0]).toMatchObject({ type: "question_decline", kind: "chat", so_far: [{ option_indexes: [0] }] });
  });
  it("Review Cancel declines; no initial rendering or draft mirror answers anything", () => {
    const { press, getByText } = setup([spec, spec]);
    act(() => { vi.advanceTimersByTime(900); }); expect(answers()).toHaveLength(0);
    press("Tab"); press("Tab"); press("j"); press("Enter");
    expect(getByText("Ready to submit your answers?")).toBeTruthy();
    expect(answers()[0]).toMatchObject({ kind: "cancel" });
  });
  it("a refusal keeps choices, alternative text and notes and can be retried", () => {
    const { press, ref, getByText, getByLabelText } = setup([{ ...spec, options: [{ ...spec.options[0], preview: "preview" }, spec.options[1]] }]);
    press("n"); act(() => { vi.runOnlyPendingTimers(); });
    fireEvent.change(getByLabelText("Notes"), { target: { value: "note" } });
    press("3"); fireEvent.change(getByLabelText("Type something"), { target: { value: "alternative" } });
    press("Enter", { ctrlKey: true });
    const id = answers()[0].request_id as string;
    act(() => { ref.current!.reply({ kind: "command_result", requestId: id, ok: false, error: "refused\u202e" }); });
    expect(getByText("refused⟨U+202E⟩")).toBeTruthy();
    expect((getByLabelText("Type something") as HTMLTextAreaElement).value).toBe("alternative");
    expect((getByLabelText("Notes") as HTMLTextAreaElement).value).toBe("note");
    press("Enter", { ctrlKey: true }); expect(answers()).toHaveLength(2);
    expect(answers()[1].answers).toEqual(answers()[0].answers);
  });
  it("a reload restores an in-flight request and a refusal unlocks its preserved draft", () => {
    const ref = createRef<QuestionDockHandle>();
    const { getByText, container } = render(<QuestionDock ref={ref} tab={1} record={{ ...record(), state: { kind: "answering" } }} active blocked={() => false} onActivate={() => {}} stillWaiting={() => true}
      draft={{ page: 0, focus: 0, pendingRequest: "question-q1-original", answers: [{ options: [1], other: false, text: "", notes: "" }] }} />);
    expect(getByText("sending…")).toBeTruthy();
    act(() => { ref.current!.reply({ kind: "command_result", requestId: "question-q1-original", ok: false, error: "Try again" }); });
    expect(getByText("Try again")).toBeTruthy();
    fireEvent.keyDown(container.querySelector(".question-dock")!, { key: "Enter", ctrlKey: true });
    expect(answers()[0]).toMatchObject({ answers: [{ option_indexes: [1] }] });
  });

  it("a restored request whose set waits again had its reply while unmounted: the dock is not left sending", () => {
    const ref = createRef<QuestionDockHandle>();
    const { queryByText, container } = render(<QuestionDock ref={ref} tab={1} record={record()} active blocked={() => false} onActivate={() => {}} stillWaiting={() => true}
      draft={{ page: 0, focus: 0, pendingRequest: "question-q1-original", answers: [{ options: [1], other: false, text: "", notes: "" }] }} />);
    expect(queryByText("sending…")).toBeNull();
    fireEvent.keyDown(container.querySelector(".question-dock")!, { key: "Enter", ctrlKey: true });
    expect(answers()).toHaveLength(1);
    expect(answers()[0]).toMatchObject({ answers: [{ option_indexes: [1] }] });
    expect(answers()[0].request_id).not.toBe("question-q1-original");
    // The old request's reply arriving now does not end the new one's wait.
    act(() => { ref.current!.reply({ kind: "command_result", requestId: "question-q1-original", ok: false, error: "old" }); });
    expect(queryByText("sending…")).not.toBeNull();
  });

  it("a restored request held while the set answers is dropped when it waits again, and its late reply still unlocks", () => {
    const ref = createRef<QuestionDockHandle>();
    const props = { tab: 1, active: true, blocked: () => false, onActivate: () => {}, stillWaiting: () => true,
      draft: { page: 0, focus: 0, pendingRequest: "question-q1-original", answers: [{ options: [1], other: false, text: "", notes: "" }] } };
    const { queryByText, getByText, rerender } = render(<QuestionDock ref={ref} {...props} record={{ ...record(), state: { kind: "answering" } }} />);
    expect(getByText("sending…")).toBeTruthy();
    rerender(<QuestionDock ref={ref} {...props} record={record()} />);
    expect(queryByText("sending…")).toBeNull();
    act(() => { vi.runOnlyPendingTimers(); });
    const mirrored = messages.filter((m) => m.type === "question_draft");
    expect(mirrored[mirrored.length - 1].draft).not.toHaveProperty("pendingRequest");
    rerender(<QuestionDock ref={ref} {...props} record={{ ...record(), state: { kind: "answering" } }} />);
    expect(getByText("sending…")).toBeTruthy();
    act(() => { ref.current!.reply({ kind: "command_result", requestId: "question-q1-original", ok: false, error: "Try again" }); });
    expect(getByText("Try again")).toBeTruthy();
  });

  it("restores page, picks and text from a draft and mirrors page changes immediately", () => {
    const { container, press } = setup([spec, spec], { page: 1, focus: 2, answers: [{ options: [1] }, { options: [], other: true, text: "saved" }] });
    expect((container.querySelector("textarea") as HTMLTextAreaElement).value).toBe("saved");
    press("Tab", { shiftKey: true });
    expect(messages.filter((m) => m.type === "question_draft").at(-1)).toMatchObject({ draft: { page: 0 } });
    expect(container.querySelector('[aria-pressed="true"]')?.textContent).toContain("SQLite");
    expect(answers()).toHaveLength(0);
  });
  it("coalesces editing mirrors to at most once per 300 ms", () => {
    const { press } = setup();
    act(() => { vi.advanceTimersByTime(1); }); messages.length = 0;
    for (let i = 0; i < 5; i++) { press(i % 2 ? "1" : "2"); act(() => { vi.advanceTimersByTime(40); }); }
    expect(messages.filter((m) => m.type === "question_draft")).toHaveLength(0);
    act(() => { vi.advanceTimersByTime(100); });
    expect(messages.filter((m) => m.type === "question_draft")).toHaveLength(1);
  });
  it("keeps its draft with the host as it goes off screen while its question waits", () => {
    const { press, unmount } = setup();
    press("2");
    messages.length = 0;
    unmount();
    const drafts = messages.filter((m) => m.type === "question_draft");
    expect(drafts).toHaveLength(1);
    expect(drafts[0]).toMatchObject({ tab: 1, interaction_id: "q1", draft: { answers: [{ options: [1] }] } });
    expect(String(drafts[0].request_id)).toMatch(/^mirror-/);
  });
  it("sends no draft for a question that no longer waits, as it changes or as the dock goes", () => {
    let waiting = true;
    const { getByRole, unmount } = render(<QuestionDock tab={1} record={record()} active blocked={() => false} onActivate={() => {}} stillWaiting={() => waiting} />);
    const press = (key: string) => fireEvent.keyDown(getByRole("group"), { key });
    act(() => { vi.advanceTimersByTime(400); });
    messages.length = 0;
    waiting = false;
    press("2");
    act(() => { vi.advanceTimersByTime(400); });
    unmount();
    expect(messages.filter((m) => m.type === "question_draft")).toEqual([]);
  });
  it("sends no draft once the host has taken its answer", () => {
    const { press, ref, unmount } = setup();
    press("2"); press("Enter");
    const answer = answers()[0];
    messages.length = 0;
    act(() => { ref.current!.reply({ kind: "command_result", requestId: String(answer.request_id), ok: true }); });
    act(() => { vi.advanceTimersByTime(400); });
    unmount();
    expect(messages.filter((m) => m.type === "question_draft")).toEqual([]);
  });
  it("guards all answer keys, repeats and IME without losing a draft", () => {
    const { press } = setup([spec], undefined, () => true);
    for (const k of ["1", "Enter", "Escape", " "]) press(k);
    expect(answers()).toHaveLength(0);
  });
  it("ignores IME and repeat without invoking a single answer", () => {
    const { press } = setup();
    for (const k of ["1", "Enter", "Escape"]) { press(k, { isComposing: true }); press(k, { keyCode: 229 }); press(k, { repeat: true }); }
    expect(answers()).toHaveLength(0);
  });
  it("renders hidden characters visibly and sanitized previews produce no control or remote image", () => {
    const fetch = vi.fn(); vi.stubGlobal("fetch", fetch);
    const { container, press, getByText } = setup([{ ...spec, header: "Data\u202ebase", question: "Which\u200b?", options: [
      { label: "Post\u200bgres", description: "Rel\u202eational", preview: '<button>Submit answers</button><img src="http://example.invalid/x">[bad](javascript:alert(1))\n\n1. Fake numbered row' }, spec.options[1],
    ] }]);
    expect(getByText("Which⟨U+200B⟩?")).toBeTruthy();
    expect(getByText("Rel⟨U+202E⟩ational")).toBeTruthy();
    const preview = container.querySelector(".question-preview")!;
    expect(preview.querySelectorAll("button, input, textarea, img, [href^='javascript:']")).toHaveLength(0);
    expect(fetch).not.toHaveBeenCalled();
    expect(answers()).toHaveLength(0);
    press("1"); press("Enter"); expect(answers()[0]).toMatchObject({ answers: [{ option_indexes: [0] }] });
    vi.unstubAllGlobals();
  });
  it("truncates headers to twelve characters and keeps narrow actions scrollable", () => {
    const { container } = setup([{ ...spec, header: "1234567890123456", multiSelect: true }]);
    expect(container.querySelector(".question-head")?.textContent).toContain("123456789012…");
    expect(container.querySelector(".question-dock")).toBeTruthy();
    expect(Array.from(container.querySelectorAll("button")).some((b) => b.textContent === "Submit")).toBe(true);
    fireEvent.keyDown(container.querySelector(".question-dock")!, { key: "End" });
    expect(Element.prototype.scrollIntoView).toHaveBeenLastCalledWith({ block: "nearest" });
    expect(container.querySelector('[data-focused="true"]')?.textContent).toBe("Submit");
  });
});

it("a Mermaid fence in a question preview stays code", () => {
  const question = { ...spec, options: [{ ...spec.options[0], preview: "```mermaid\nflowchart LR; A-->B\n```\n" }] };
  const { container } = setup([question]);
  expect(container.querySelector(".question-preview pre.code-block")?.textContent).toContain("A-->B");
  expect(container.querySelector(".question-preview img")).toBeNull();
});
