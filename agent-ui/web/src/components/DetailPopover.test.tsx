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
