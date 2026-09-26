// @vitest-environment jsdom
import { afterEach, beforeAll, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { markdownParseCount } from "./markdown";

afterEach(cleanup);
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

/** V3 (ruling 33): the deterministic half of "measure first". A `j` press re-renders the whole
 *  panel; before the memo, every message was re-parsed (marked + highlight.js + DOMPurify) on every
 *  press. Counted, not timed: jsdom's timing is not WebKitGTK's, and a count cannot be flaky. */
it("parses no message again when j moves the cursor through 500 of them", () => {
  const posted: unknown[] = [];
  (window as unknown as { webkit: unknown }).webkit = { messageHandlers: { neovibeAgent: { postMessage: (m: string) => posted.push(m) } } };
  const { container } = render(<App />);
  const transcript = Array.from({ length: 500 }, (_, i) => ({ seq: i + 1, text: `message **${i}** with \`code\`` }));
  act(() => {
    window.__neovibeDispatch!(JSON.stringify({ kind: "tabs", active: 1, tabs: [{ id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null }] }));
    window.__neovibeDispatch!(JSON.stringify({ kind: "snapshot", tab: 1, throughRevision: 501, state: { ...initialState(), status: { kind: "running" }, transcript } }));
  });
  const root = container.querySelector(".agent-ui-conversation")!;
  const before = markdownParseCount();
  for (let i = 0; i < 20; i++) fireEvent.keyDown(root, { key: "j" });
  const perPress = (markdownParseCount() - before) / 20;
  console.log(`[V3] marked.parse calls per j press over 500 messages: ${perPress}`);
  expect(perPress).toBe(0);
});
