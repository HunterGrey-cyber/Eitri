// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { QueueLines } from "./QueueLines";

afterEach(cleanup);

it("offers to take queued messages back for editing without sending them", () => {
  const onEdit = vi.fn();
  const { getByRole, getByText } = render(
    <QueueLines items={[{ text: "Run tests", queuedAt: 1 }]} error={null} onEdit={onEdit} />,
  );
  expect(getByText("1 queued")).toBeTruthy();
  fireEvent.click(getByRole("button", { name: "Edit queued messages" }));
  expect(onEdit).toHaveBeenCalledOnce();
});

it("lists each queued message on one muted line, and says why a flush was refused", () => {
  const { container, rerender } = render(
    <QueueLines items={[{ text: "and the tests\nplease", queuedAt: 1 }, { text: "then commit", queuedAt: 2 }]} error={null} />,
  );
  const lines = Array.from(container.querySelectorAll(".queue-line")).map((l) => l.textContent);
  expect(lines).toEqual(["⧗ and the tests …", "⧗ then commit"]);
  rerender(<QueueLines items={[{ text: "x", queuedAt: 1 }]} error="the backend refused it" />);
  expect(container.querySelector(".queue-error")!.textContent).toBe("⚠ not sent: the backend refused it — Enter or Ctrl+Enter tries again");
  rerender(<QueueLines items={[]} error={null} />);
  expect(container.firstChild).toBeNull();
});
