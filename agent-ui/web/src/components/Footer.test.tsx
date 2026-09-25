// @vitest-environment jsdom
import { afterEach, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { Footer } from "./Footer";

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
