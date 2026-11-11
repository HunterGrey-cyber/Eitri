// @vitest-environment jsdom
import { afterEach, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { initialState } from "./reducer";
import { MessageList } from "./components/MessageList";

afterEach(cleanup);
it("renders a plain eviction line in the compiled phone page, even without touch", () => {
  expect(import.meta.env.EITRI_PHONE_PAGE).toBe(true);
  const { container } = render(<MessageList state={{ ...initialState(), evicted: { throughSeq: 4, turns: 2 } }} expanded={{}} cursor={0} sessionEnded={false} onAnswerPermission={() => {}} />);
  expect(container.textContent).toContain("2 earlier turns not loaded.");
  expect(container.querySelector(".history-notice button, .history-notice[data-nav-stop]")).toBeNull();
});
