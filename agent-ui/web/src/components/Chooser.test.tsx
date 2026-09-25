// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { Chooser } from "./Chooser";
import type { ChooserEnvelope } from "../types";

afterEach(cleanup);
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const ENVELOPE: ChooserEnvelope = {
  launch: false,
  open: [{ tab: 1, label: "1 fix-parser", marker: null, pending: 0, resumable: true }],
  records: [
    { providerSessionId: "held-0000", name: null, title: "held one", createdAt: "1", updatedAt: "2", heldElsewhere: true },
    { providerSessionId: "free-0000", name: null, title: "free one", createdAt: "1", updatedAt: "2", heldElsewhere: false },
  ],
};

function renderChooser(envelope = ENVELOPE) {
  const props = { envelope, onSwitch: vi.fn(), onResume: vi.fn(), onCloseTab: vi.fn(), onLeave: vi.fn() };
  const view = render(<Chooser {...props} />);
  const root = view.container.querySelector<HTMLElement>(".chooser")!;
  return { props, root, ...view };
}

describe("Chooser", () => {
  it("takes the keys when it opens; j/k move and Enter switches to an open tab", () => {
    const { root, props } = renderChooser();
    expect(root.contains(document.activeElement)).toBe(true);
    fireEvent.keyDown(root, { key: "Enter" });
    expect(props.onSwitch).toHaveBeenCalledWith(1);
  });
  it("a record held elsewhere cannot be chosen, and the next one can", () => {
    const { root, props } = renderChooser();
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "Enter" });
    expect(props.onResume).not.toHaveBeenCalled();
    fireEvent.keyDown(root, { key: "j" });
    fireEvent.keyDown(root, { key: "Enter" });
    expect(props.onResume).toHaveBeenCalledWith("free-0000");
  });
  it("x asks to close the open tab under the cursor", () => {
    const { root, props } = renderChooser();
    fireEvent.keyDown(root, { key: "x" });
    expect(props.onCloseTab).toHaveBeenCalledWith(1);
  });
  it("Esc and q leave, saying whether this was the launch chooser", () => {
    const first = renderChooser({ ...ENVELOPE, launch: true });
    fireEvent.keyDown(first.root, { key: "Escape" });
    expect(first.props.onLeave).toHaveBeenCalledWith(true);
    cleanup();
    const second = renderChooser();
    fireEvent.keyDown(second.root, { key: "q" });
    expect(second.props.onLeave).toHaveBeenCalledWith(false);
  });
  it("/ opens a filter; typing narrows the rows and Enter returns to the list", () => {
    const { root, container } = renderChooser();
    fireEvent.keyDown(root, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    expect(document.activeElement).toBe(filter);
    fireEvent.change(filter, { target: { value: "free" } });
    expect(container.querySelectorAll(".chooser-row").length).toBe(1);
    fireEvent.keyDown(filter, { key: "Enter" });
    expect(document.activeElement).not.toBe(filter);
  });
  /** Review focus 5. */
  it("the_filter_ignores_enter_while_composing", () => {
    const { root, container, props } = renderChooser();
    fireEvent.keyDown(root, { key: "/" });
    const filter = container.querySelector<HTMLInputElement>(".chooser-filter")!;
    fireEvent.keyDown(filter, { key: "Enter", isComposing: true });
    fireEvent.keyDown(filter, { key: "Escape", isComposing: true });
    expect(document.activeElement).toBe(filter);
    expect(props.onLeave).not.toHaveBeenCalled();
  });
});
