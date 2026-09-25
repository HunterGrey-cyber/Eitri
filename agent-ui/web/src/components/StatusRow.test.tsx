// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { StatusRow } from "./StatusRow";

afterEach(cleanup);

it("shows the row, a ⚠ only with a warning, and opens the popover", () => {
  const onOpenDetail = vi.fn();
  const { container, rerender } = render(<StatusRow text="m · sidecar · 1/2" warning={null} onOpenDetail={onOpenDetail} />);
  const row = container.querySelector<HTMLButtonElement>('[data-nav-stop="status-row"]')!;
  expect(row.textContent).toBe("m · sidecar · 1/2");
  fireEvent.click(row);
  expect(onOpenDetail).toHaveBeenCalledTimes(1);
  rerender(<StatusRow text="m · sidecar · 1/2" warning="Verdandi baseline drift: x" onOpenDetail={onOpenDetail} />);
  const warn = container.querySelector(".status-warning")!;
  expect(warn.textContent).toBe("⚠");
  expect(warn.getAttribute("title")).toBe("Verdandi baseline drift: x");
});
