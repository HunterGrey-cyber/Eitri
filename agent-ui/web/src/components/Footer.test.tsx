// @vitest-environment jsdom
import { afterEach, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { Footer, INPUT_IDLE_HINT, INPUT_RUNNING_HINT } from "./Footer";

afterEach(cleanup);

it("is the mode block, the mode pill, then the which-key strip", () => {
  const { container, getByTestId, rerender } = render(
    <Footer mode="browse" paneFocused={false} pill="⏵⏵ auto on">
      <span className="which-key">strip</span>
    </Footer>,
  );
  const block = getByTestId("mode-block");
  expect(block.textContent).toBe("BROWSE");
  expect(block.getAttribute("data-focused")).toBe("false");
  const footer = container.querySelector(".panel-footer")!;
  expect(Array.from(footer.children).map((c) => c.className)).toEqual(["mode-block", "mode-pill", "which-key"]);
  expect(footer.querySelector(".mode-pill")!.textContent).toBe("⏵⏵ auto on");
  rerender(<Footer mode="input" paneFocused pill="⏵⏵ auto on" />);
  expect(getByTestId("mode-block").getAttribute("data-focused")).toBe("true");
});

it("shows the INPUT hints (F4), a flash over them, and the close prompt over both", () => {
  const { container, rerender } = render(<Footer mode="input" pill="p" hint={INPUT_IDLE_HINT} />);
  expect(container.querySelector(".footer-hint")!.textContent).toBe("Enter send · Shift+Enter newline · Esc browse · Ctrl+g nvim");
  rerender(<Footer mode="input" pill="p" hint={INPUT_RUNNING_HINT} flash="copied 3 chars" />);
  expect(container.querySelector(".footer-flash")!.textContent).toBe("copied 3 chars");
  expect(container.querySelector(".footer-hint")).toBeNull();
  rerender(
    <Footer mode="input" pill="p" hint={INPUT_RUNNING_HINT} flash="copied 3 chars">
      <span className="confirm-close">close 1? (y/n)</span>
    </Footer>,
  );
  expect(container.querySelector(".footer-flash")).toBeNull();
  expect(container.querySelector(".confirm-close")).not.toBeNull();
  expect(INPUT_RUNNING_HINT).toBe("Enter queue · Ctrl+Enter now · Ctrl+c interrupt");
});
