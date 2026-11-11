// @vitest-environment jsdom
import { afterEach, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { initialState } from "./reducer";
import { MessageList } from "./components/MessageList";

afterEach(cleanup);
it("renders no eviction line in the compiled phone page, even without touch", () => {
  expect(import.meta.env.EITRI_PHONE_PAGE).toBe(true);
  const { container } = render(<MessageList state={{ ...initialState(), evicted: { throughSeq: 4, turns: 2 } }} expanded={{}} cursor={0} sessionEnded={false} onAnswerPermission={() => {}} />);
  expect(container.textContent).not.toContain("earlier turns not loaded");
});
