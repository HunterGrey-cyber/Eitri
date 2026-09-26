// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { EmptyTab } from "./EmptyTab";
import type { EmptyTabProps } from "./EmptyTab";
import type { Hello, ResumableSession, TabInfo } from "../types";

afterEach(cleanup);

const session = (n: number): ResumableSession => ({
  provider: "claude", providerSessionId: `id-${n}-0000000000`, createdAt: "1", updatedAt: "2", title: `about ${n}`, name: null,
});
const HELLO: Hello = {
  backend: "sidecar", projectDir: "/p", permissionModes: ["auto", "bypass"],
  resumableSessions: Array.from({ length: 10 }, (_, i) => session(i)), expectedVerdandiRevision: "28a5e4c",
};
const TAB: TabInfo = { id: 1, number: 1, label: "1 new", name: null, state: "not_started", mode: "auto", marker: null, pending: 0, resumable: true, failure: null };

function renderEmpty(over: Partial<EmptyTabProps> = {}) {
  const props: EmptyTabProps = {
    hello: HELLO, tab: TAB, handoff: null, failure: null, paneFocused: true, focusRequest: 0, restoredDraft: null,
    onSend: vi.fn(), onResume: vi.fn(), onCycleMode: vi.fn(), onReset: vi.fn(), onHint: vi.fn(), ...over,
  };
  return { props, ...render(<EmptyTab {...props} />) };
}

describe("EmptyTab (F3)", () => {
  it("is Claude Code's fresh prompt: a live composer, at most 8 resume rows", () => {
    // The mode pill moved into the footer `App.tsx` draws (Task 9); see `App.test.tsx`'s
    // "draws the empty tab's close prompt in the footer" for where it is covered now.
    const { container } = renderEmpty();
    expect(container.querySelector("textarea")).not.toBeNull();
    expect(document.activeElement).toBe(container.querySelector("textarea"));
    expect(container.querySelectorAll('[data-nav-stop="resume"]').length).toBe(8);
    expect(container.textContent).not.toContain("about 8");
  });
  it("sends what is typed, which starts the session lazily in Rust", () => {
    const { container, props } = renderEmpty();
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "fix the parser" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(props.onSend).toHaveBeenCalledWith("fix the parser");
  });
  it("cycles the mode on Shift+Tab, but not while an input method is composing", () => {
    const { container, props } = renderEmpty();
    const box = container.querySelector("textarea")!;
    fireEvent.keyDown(box, { key: "Tab", shiftKey: true, isComposing: true });
    expect(props.onCycleMode).not.toHaveBeenCalled();
    fireEvent.keyDown(box, { key: "Tab", shiftKey: true });
    expect(props.onCycleMode).toHaveBeenCalledTimes(1);
  });
  it("resumes the row that is activated (Enter on a focused row button is its click)", () => {
    const { container, props } = renderEmpty();
    const rows = container.querySelectorAll<HTMLButtonElement>('[data-nav-stop="resume"]');
    rows[2].focus();
    fireEvent.click(rows[2]);
    expect(props.onResume).toHaveBeenCalledWith("id-2-0000000000");
  });
  it("says it is starting while the session connects, with the box live, queueing", () => {
    const onQueue = vi.fn();
    const { container, getByText } = renderEmpty({ tab: { ...TAB, state: "starting" }, onQueue });
    getByText(/Starting the agent backend/);
    const box = container.querySelector("textarea")!;
    expect(box.disabled).toBe(false);
    fireEvent.change(box, { target: { value: "queue this" } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(onQueue).toHaveBeenCalledWith("queue this");
  });
  /** Fix round 1 (reviewer finding, blocking): a starting tab has no live turn to send-now to, so
   *  Ctrl+Enter used to fall through to `Composer`'s default no-op `onSendNow` while its `submit()`
   *  cleared the box anyway -- the typed text vanished with no send, no queue and no restore. */
  it("queues on Ctrl+Enter too, instead of silently discarding the draft (fix round 1)", () => {
    const onQueue = vi.fn();
    const { container } = renderEmpty({ tab: { ...TAB, state: "starting" }, onQueue });
    const box = container.querySelector("textarea")!;
    fireEvent.change(box, { target: { value: "important text" } });
    fireEvent.keyDown(box, { key: "Enter", ctrlKey: true });
    expect(onQueue).toHaveBeenCalledWith("important text");
  });
  it("does not queue a blank entry when Ctrl+Enter is pressed on an empty box", () => {
    const onQueue = vi.fn();
    const { container } = renderEmpty({ tab: { ...TAB, state: "starting" }, onQueue });
    const box = container.querySelector("textarea")!;
    fireEvent.keyDown(box, { key: "Enter", ctrlKey: true });
    expect(onQueue).not.toHaveBeenCalled();
  });
  it("shows why a failed tab failed, and r starts it over", () => {
    const { container, getByText, props } = renderEmpty({ tab: { ...TAB, state: "failed" }, failure: "the gate refused 2.1.999" });
    getByText("the gate refused 2.1.999");
    fireEvent.keyDown(container.querySelector(".empty-tab")!, { key: "r" });
    expect(props.onReset).toHaveBeenCalled();
  });
  it("shows the handoff card of a conversation handed to a terminal (ruling 13)", () => {
    const { getByText } = renderEmpty({
      handoff: { command: "cd /p && claude --resume abc", cwd: "/p", providerSessionId: "abc" },
    });
    getByText("cd /p && claude --resume abc");
  });
});
