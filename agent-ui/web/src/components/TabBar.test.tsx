// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { TabBar } from "./TabBar";
import type { TabInfo } from "../types";

afterEach(cleanup);
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const tab = (id: number, over: Partial<TabInfo> = {}): TabInfo => ({
  id, number: id, label: `${id} new`, name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: true, failure: null, ...over,
});
const TABS = [tab(1, { label: "1 fix-parser", marker: "needs_input", pending: 1 }), tab(2, { label: "2 docs", marker: "unread" }), tab(3, { label: "3 new", marker: "ended", state: "ended" })];

function renderBar(over: Partial<Parameters<typeof TabBar>[0]> = {}) {
  const props = { tabs: TABS, active: 2, renaming: null, onSelect: vi.fn(), onRenameCommit: vi.fn(), onRenameCancel: vi.fn(), ...over };
  return { props, ...render(<TabBar {...props} />) };
}

describe("TabBar", () => {
  it("is one nav stop of tab buttons, labels and markers, the active one selected", () => {
    const { container } = renderBar();
    const bar = container.querySelector('[data-nav-stop="tabs"]')!;
    const buttons = bar.querySelectorAll<HTMLButtonElement>('[role="tab"]');
    expect(Array.from(buttons).map((b) => b.textContent)).toEqual(["1 fix-parser ⚑", "2 docs •", "3 new ✕"]);
    expect(buttons[1].getAttribute("aria-selected")).toBe("true");
    expect(buttons[2].classList.contains("tab-ended")).toBe(true);
    expect(Element.prototype.scrollIntoView).toHaveBeenCalled();
  });
  it("selects a tab on click (Enter on a focused tab button is its click)", () => {
    const { container, props } = renderBar();
    fireEvent.click(container.querySelectorAll('[role="tab"]')[0]);
    expect(props.onSelect).toHaveBeenCalledWith(1);
  });
  it("draws a working tab with the motion class, not a glyph", () => {
    const { container } = renderBar({ tabs: [tab(1, { marker: "working" }), tab(2)] });
    expect(container.querySelector(".tab-working")).not.toBeNull();
  });
  it("renames inline: prefilled and selected, Enter commits, Esc cancels", () => {
    const { container, props } = renderBar({ renaming: { tab: 2, initial: "docs" } });
    const input = container.querySelector<HTMLInputElement>(".tab-rename")!;
    expect(input.value).toBe("docs");
    expect(document.activeElement).toBe(input);
    expect([input.selectionStart, input.selectionEnd]).toEqual([0, 4]);
    fireEvent.change(input, { target: { value: "notes" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(props.onRenameCommit).toHaveBeenCalledWith("notes");
    fireEvent.keyDown(input, { key: "Escape" });
    expect(props.onRenameCancel).toHaveBeenCalled();
  });
  /** Review focus 5: an Enter or Esc the input method owns neither saves nor cancels. */
  it("rename_ignores_enter_while_composing", () => {
    const { container, props } = renderBar({ renaming: { tab: 2, initial: "" } });
    const input = container.querySelector<HTMLInputElement>(".tab-rename")!;
    fireEvent.keyDown(input, { key: "Enter", isComposing: true });
    fireEvent.keyDown(input, { key: "Escape", isComposing: true });
    fireEvent.keyDown(input, { key: "Enter", keyCode: 229 });
    expect(props.onRenameCommit).not.toHaveBeenCalled();
    expect(props.onRenameCancel).not.toHaveBeenCalled();
  });
});
