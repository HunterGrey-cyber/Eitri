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
  it("is Claude Code's fresh prompt: a live composer, the mode pill, at most 8 resume rows", () => {
    const { container, getByText } = renderEmpty();
    expect(container.querySelector("textarea")).not.toBeNull();
    expect(document.activeElement).toBe(container.querySelector("textarea"));
    getByText("⏵⏵ auto on (shift+tab to cycle)");
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
  it("says it is starting while the session connects, with the box disabled", () => {
    const { container, getByText } = renderEmpty({ tab: { ...TAB, state: "starting" } });
    getByText(/Starting the agent backend/);
    expect(container.querySelector("textarea")?.disabled).toBe(true);
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
