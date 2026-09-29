// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { SlashPicker } from "./SlashPicker";

afterEach(cleanup);
beforeAll(() => {
  // jsdom has no layout, so it never implements this -- `Chooser.test.tsx` mocks it the same way.
  Element.prototype.scrollIntoView = vi.fn();
});

function renderPicker(over: Record<string, unknown> = {}) {
  const props = {
    kind: "model" as const,
    options: ["sonnet", "opus", "haiku"],
    current: "haiku",
    focusRequest: 0,
    onChoose: vi.fn(),
    onCancel: vi.fn(),
    ...over,
  };
  const view = render(<SlashPicker {...props} />);
  const root = view.container.querySelector<HTMLElement>(".slash-picker")!;
  return { props, root, ...view };
}

describe("SlashPicker (owner trial item 2)", () => {
  it("starts the cursor on the current option and marks it", () => {
    const { container } = renderPicker();
    const current = container.querySelector(".slash-picker-row.current")!;
    expect(current.textContent).toContain("haiku");
    expect(current.textContent).toContain("(current)");
  });

  it("starts on row 0 when the reply named no current option (/effort)", () => {
    const { container } = renderPicker({ kind: "effort", options: ["low", "medium", "high"], current: null });
    const current = container.querySelector(".slash-picker-row.current")!;
    expect(current.textContent).toContain("low");
    expect(container.querySelectorAll(".slash-picker-current-marker")).toHaveLength(0);
  });

  it("j/k move the cursor, clamped at both ends", () => {
    const { root, container } = renderPicker();
    // current starts at "haiku" (index 2, the last option)
    fireEvent.keyDown(root, { key: "j" });
    expect(container.querySelector(".slash-picker-row.current")!.textContent).toContain("haiku");
    fireEvent.keyDown(root, { key: "k" });
    expect(container.querySelector(".slash-picker-row.current")!.textContent).toContain("opus");
    fireEvent.keyDown(root, { key: "k" });
    expect(container.querySelector(".slash-picker-row.current")!.textContent).toContain("sonnet");
    fireEvent.keyDown(root, { key: "k" });
    expect(container.querySelector(".slash-picker-row.current")!.textContent).toContain("sonnet");
  });

  it("Enter chooses the option under the cursor", () => {
    const { root, props } = renderPicker();
    fireEvent.keyDown(root, { key: "k" });
    fireEvent.keyDown(root, { key: "Enter" });
    expect(props.onChoose).toHaveBeenCalledWith("opus");
    expect(props.onCancel).not.toHaveBeenCalled();
  });

  it("Escape and q both cancel with nothing chosen", () => {
    for (const key of ["Escape", "q"]) {
      const { root, props } = renderPicker();
      fireEvent.keyDown(root, { key });
      expect(props.onChoose, key).not.toHaveBeenCalled();
      expect(props.onCancel, key).toHaveBeenCalledTimes(1);
      cleanup();
    }
  });

  it("a click on a row chooses it directly, regardless of the cursor", () => {
    const { container, props } = renderPicker();
    fireEvent.click(container.querySelectorAll(".slash-picker-row")[0]!);
    expect(props.onChoose).toHaveBeenCalledWith("sonnet");
  });

  it("focuses itself on mount, so a keydown reaches it with no click first", () => {
    const { root } = renderPicker();
    expect(document.activeElement).toBe(root);
  });

  it("names the command and the option under the cursor in its hint line", () => {
    const { container, root } = renderPicker();
    expect(container.querySelector(".slash-picker-hint")!.textContent).toContain("enter /model haiku");
    fireEvent.keyDown(root, { key: "k" });
    expect(container.querySelector(".slash-picker-hint")!.textContent).toContain("enter /model opus");
  });

  it("says /effort's level only applies to this session (the probe's own scope)", () => {
    const { container } = renderPicker({ kind: "effort", options: ["low", "high"], current: null });
    expect(container.querySelector(".slash-picker-hint")!.textContent).toContain("applies to this session only");
  });

  it("ignores an IME's own Enter/Escape (isComposing)", () => {
    const { root, props } = renderPicker();
    fireEvent.keyDown(root, { key: "Enter", isComposing: true });
    expect(props.onChoose).not.toHaveBeenCalled();
  });

  it("scrolls the cursor row into view when j/k moves it", () => {
    const { root, container } = renderPicker();
    const mock = container.querySelector(".slash-picker-row.current")!.scrollIntoView as ReturnType<typeof vi.fn>;
    mock.mockClear();
    fireEvent.keyDown(root, { key: "k" });
    expect(mock).toHaveBeenCalled();
  });

  /** Fix round (Codex review finding): a GTK focus round trip, `arrive` or a HINT landing used to
   *  hand the keys to the container root instead of an open picker, since `App.tsx`'s `takeKeys`
   *  had no branch for it -- mirrors `Chooser.test.tsx`'s identical `focusRequest` coverage. */
  describe("focusRequest (fix round)", () => {
    it("re-focuses the root after focus moved elsewhere", () => {
      const outside = document.createElement("button");
      document.body.appendChild(outside);
      const { root, props, rerender } = renderPicker();
      outside.focus();
      expect(document.activeElement).toBe(outside);
      rerender(<SlashPicker {...props} focusRequest={1} />);
      expect(document.activeElement).toBe(root);
      document.body.removeChild(outside);
    });
  });
});
