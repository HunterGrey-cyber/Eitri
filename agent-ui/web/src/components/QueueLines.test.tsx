// @vitest-environment jsdom
import { afterEach, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { QueueLines } from "./QueueLines";

afterEach(cleanup);

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
