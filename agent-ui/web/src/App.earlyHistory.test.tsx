// @vitest-environment jsdom
/**
 * A resume's history drawn before its session is live (Rust's early snapshot, `serialize_early_snapshot_for_js`):
 * the starting tab shows the conversation, its composer queues behind the start and sends nothing, and a start
 * that fails takes the early history off the screen and shows the failure as for a start that drew nothing.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { AgentUiState, Hello } from "./types";

afterEach(cleanup);

let posted: Array<Record<string, unknown>>;

beforeEach(() => {
  posted = [];
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } },
  };
  Element.prototype.scrollIntoView = vi.fn();
});

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

const ofType = (type: string) => posted.filter((message) => message.type === type);

const HELLO: Hello = {
  backend: "sidecar",
  projectDir: "/home/user/project",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  account: "team",
};

const TAB = {
  id: 1, number: 1, label: "1 saved", name: null, mode: "auto",
  marker: null, pending: 0, resumable: true, failure: null, title: null, agent: "claude",
} as const;

const HISTORY = {
  ...initialState(),
  userPrompts: [{ seq: 1, text: "please fix the parser" }],
  transcript: [{ seq: 2, text: "restored reply from the agent" }],
} as Partial<AgentUiState>;

function resuming() {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, state: "starting" }] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 2, state: HISTORY, early: true });
  return rendered;
}

describe("a resume's history drawn while it starts", () => {
  it("shows the conversation under the starting tab", () => {
    const { container } = resuming();
    expect(container.querySelector(".empty-tab")).toBeNull();
    expect(container.textContent).toContain("restored reply from the agent");
    expect(container.textContent).toContain("please fix the parser");
  });

  it("queues what is typed behind the start and sends nothing", () => {
    const { container } = resuming();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "enter_input" });
    const box = container.querySelector<HTMLTextAreaElement>("textarea")!;
    fireEvent.change(box, { target: { value: "and the tests?" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(ofType("send_message")).toEqual([]);
    expect(ofType("queue_message")).toMatchObject([{ tab: 1, text: "and the tests?" }]);
    fireEvent.change(box, { target: { value: "right now" } });
    fireEvent.keyDown(box, { key: "Enter", ctrlKey: true });
    expect(ofType("send_now")).toEqual([]);
    expect(ofType("queue_message")).toHaveLength(2);
  });

  it("keeps what was typed in the starting screen when the conversation's composer replaces it", () => {
    const { container } = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, state: "starting" }] });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "enter_input" });
    fireEvent.change(container.querySelector<HTMLTextAreaElement>("textarea")!, { target: { value: "half typed" } });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 2, state: HISTORY, early: true });
    expect(container.textContent).toContain("restored reply from the agent");
    // The conversation lands in BROWSE; its box shows once INPUT is entered.
    dispatch({ kind: "enter_input" });
    expect(container.querySelector<HTMLTextAreaElement>("textarea")!.value).toBe("half typed");
  });

  it("goes when the start fails, which shows as a start that drew nothing", () => {
    const { container } = resuming();
    dispatch({ kind: "error", tab: 1, message: "could not continue the previous session: no such session" });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, state: "failed", failure: "could not continue" }] });
    expect(container.textContent).not.toContain("restored reply from the agent");
    expect(container.textContent).not.toContain("This session was lost.");
    expect(container.querySelector(".empty-tab")).not.toBeNull();
  });

  it("stays on screen when the session goes live, replaced by its own snapshot", () => {
    const { container } = resuming();
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, state: "live" }] });
    dispatch({ kind: "snapshot", tab: 1, throughRevision: 2, state: { ...HISTORY, status: { kind: "running" } } });
    expect(container.querySelector(".empty-tab")).toBeNull();
    expect(container.textContent).toContain("restored reply from the agent");
  });

  /** A refused restore hands the tab being typed in back empty (`starting` -> `not_started`, the same tab), and the
   *  host's draft can trail the box by the mirror's debounce. */
  function refusedWhileTyping(typed: string, hostDraft: string) {
    const rendered = resuming();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "enter_input" });
    const box = rendered.container.querySelector<HTMLTextAreaElement>("textarea")!;
    for (const key of typed) fireEvent.keyDown(box, { key });
    fireEvent.change(box, { target: { value: typed } });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, state: "not_started" }] });
    dispatch({ kind: "draft", tab: 1, text: hostDraft });
    return rendered;
  }

  const sent = () => posted.filter((m) => ["send_message", "queue_message", "send_now", "resume", "tab_verb"].includes(String(m.type)));

  it("leaves typing mode when a refused restore empties the tab being typed in, keeping the text unsent", () => {
    const { container } = refusedWhileTyping("r1", "");
    expect(container.querySelector("textarea")).toBeNull();
    expect(container.querySelector(".composer-draft")?.textContent).toContain("r1");
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "Enter" });
    expect(sent()).toEqual([]);
    expect(container.querySelector(".composer-draft")?.textContent).toContain("r1");
  });

  it("keeps what was queued and what was typed since, and an Enter sends neither", async () => {
    const rendered = resuming();
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "enter_input" });
    const box = rendered.container.querySelector<HTMLTextAreaElement>("textarea")!;
    fireEvent.change(box, { target: { value: "q1" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(posted.filter((m) => m.type === "queue_message")).toHaveLength(1);
    const queued = posted.length;
    const again = rendered.container.querySelector<HTMLTextAreaElement>("textarea")!;
    for (const key of "w2") fireEvent.keyDown(again, { key });
    fireEvent.change(again, { target: { value: "w2" } });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, state: "not_started" }] });
    dispatch({ kind: "draft", tab: 1, text: "q1" });
    expect(rendered.container.querySelector("textarea")).toBeNull();
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "Enter" });
    expect(posted.slice(queued).filter((m) => ["send_message", "queue_message", "send_now", "resume"].includes(String(m.type)))).toEqual([]);
    // The box shows two lines in BROWSE; the whole draft is what goes back to the host.
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 400)); });
    const drafts = posted.filter((m) => m.type === "draft");
    expect(drafts[drafts.length - 1]).toMatchObject({ tab: 1, text: "q1\n\nw2" });
  });

  /** The starting screen's own box (no history arrived yet), typed in when the refusal lands. */
  function refusedBeforeHistory(typed: string) {
    const rendered = render(<App />);
    dispatch({ kind: "hello", ...HELLO });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, state: "starting" }] });
    dispatch({ kind: "pane_focus", focused: true });
    dispatch({ kind: "enter_input" });
    const box = rendered.container.querySelector<HTMLTextAreaElement>("textarea")!;
    for (const key of typed) fireEvent.keyDown(box, { key });
    fireEvent.change(box, { target: { value: typed } });
    dispatch({ kind: "tabs", active: 1, tabs: [{ ...TAB, state: "not_started" }] });
    dispatch({ kind: "draft", tab: 1, text: "" });
    return rendered;
  }

  it("leaves typing mode on the starting screen too, before any history arrived", () => {
    const { container } = refusedBeforeHistory("r1");
    expect(container.querySelector("textarea")).toBeNull();
    expect(container.querySelector(".composer-draft")?.textContent).toContain("r1");
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "Enter" });
    expect(sent()).toEqual([]);
  });

  it("swallows an Enter right after a single letter that reads as a motion", () => {
    const { container } = refusedBeforeHistory("j");
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "Enter" });
    expect(sent()).toEqual([]);
    expect(container.querySelector("textarea")).toBeNull();
  });

  it("sends the kept text only after a deliberate i and Enter", async () => {
    const { container } = refusedWhileTyping("r1", "");
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 600)); });
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "i" });
    const box = container.querySelector<HTMLTextAreaElement>("textarea")!;
    expect(box.value).toBe("r1");
    fireEvent.keyDown(box, { key: "Enter" });
    expect(posted.filter((m) => m.type === "send_message")).toMatchObject([{ tab: 1, text: "r1" }]);
  });
});

