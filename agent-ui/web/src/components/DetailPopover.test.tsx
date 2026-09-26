// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { DetailPopover } from "./DetailPopover";

afterEach(cleanup);

it("lists every row, marks the current one, and closes on a backdrop click only", () => {
  const onClose = vi.fn();
  const rows = [{ label: "account", value: "work" }, { label: "cwd", value: "/p" }];
  const { container } = render(<DetailPopover rows={rows} current={1} onClose={onClose} />);
  const trs = container.querySelectorAll("tr");
  expect(trs.length).toBe(2);
  expect(trs[1].getAttribute("aria-current")).toBe("true");
  expect(trs[0].textContent).toContain("work");
  fireEvent.click(trs[0]);
  expect(onClose).not.toHaveBeenCalled();
  fireEvent.click(container.querySelector(".detail-popover")!);
  expect(onClose).toHaveBeenCalledTimes(1);
});

it("keeps the current row in view as j/k move it (the phase-3 GUI pass, 2026-09-25)", () => {
  // Seen in the sandbox: with phase 3's rows the table outgrew a short panel, and `j` walked the
  // highlight off the bottom with nothing scrolling after it.
  const calls: Element[] = [];
  const original = Element.prototype.scrollIntoView;
  Element.prototype.scrollIntoView = function (this: Element) {
    calls.push(this);
  };
  try {
    const rows = Array.from({ length: 20 }, (_, i) => ({ label: `row ${i}`, value: `${i}` }));
    const { container, rerender } = render(<DetailPopover rows={rows} current={0} onClose={() => {}} />);
    rerender(<DetailPopover rows={rows} current={17} onClose={() => {}} />);
    expect(calls[calls.length - 1]).toBe(container.querySelectorAll("tr")[17]);
  } finally {
    Element.prototype.scrollIntoView = original;
  }
});
