// @vitest-environment jsdom
import { Profiler } from "react";
import { afterEach, beforeAll, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import { MARKDOWN_CACHE_LIMIT, isMarkdownCached, markdownParseCount } from "./markdown";
import { lineDiffCount } from "./diff";
import { timelineBuildCount } from "./timeline";

/* The panel's cost over a long conversation, counted rather than timed: jsdom's timing is not
   WebKitGTK's, and a count cannot be flaky. A conversation of more replies than the markdown cache
   holds, with 100 finished 400-line Edit calls in it, is the case each count below once grew with. */

afterEach(cleanup);
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const TAB = { id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null };
const REPLIES = MARKDOWN_CACHE_LIMIT + 1;
const EDITS = 100;

function dispatch(payload: unknown) {
  window.__eitriDispatch!(JSON.stringify(payload));
}

/** Mounts the panel on a long conversation; `commits()` counts the panel's React commits. */
function mountLong() {
  (window as unknown as { webkit: unknown }).webkit = { messageHandlers: { eitriAgent: { postMessage: () => {} } } };
  let commits = 0;
  const { container } = render(
    <Profiler id="app" onRender={() => (commits += 1)}>
      <App />
    </Profiler>,
  );
  const transcript = Array.from({ length: REPLIES }, (_, i) => ({ seq: i * 2 + 1, text: `reply **${i}** with \`code\`` }));
  const oldText = Array.from({ length: 400 }, (_, i) => `old line ${i}`).join("\n");
  const newText = Array.from({ length: 400 }, (_, i) => `new line ${i}`).join("\n");
  const toolCalls = Array.from({ length: EDITS }, (_, i) => ({
    seq: i * 2 + 2,
    toolUseId: `toolu_${i}`,
    name: "Edit",
    input: { file_path: `/p/f${i}.rs`, old_string: oldText, new_string: newText },
    result: { content: "ok", isError: false },
    turnId: "t0",
  }));
  const nextSeq = REPLIES * 2 + 2;
  act(() => {
    dispatch({ kind: "tabs", active: 1, tabs: [TAB] });
    dispatch({
      kind: "snapshot",
      tab: 1,
      throughRevision: nextSeq,
      state: { ...initialState(), status: { kind: "running" }, activeTurnId: "t1", transcript, toolCalls, nextSeq },
    });
  });
  let revision = nextSeq;
  const deliver = (events: unknown[]) => {
    act(() => dispatch({ kind: "events", tab: 1, fromRevision: revision, throughRevision: revision + events.length, events }));
    revision += events.length;
  };
  return {
    root: container.querySelector<HTMLElement>(".agent-ui-conversation")!,
    list: container.querySelector<HTMLElement>(".message-list")!,
    commits: () => commits,
    deliver,
  };
}

const textDeltas = (n: number) => Array.from({ length: n }, () => ({ type: "content_delta", kind: "text", text: "word " }));

it("a k press diffs no finished Edit row again", () => {
  const { root } = mountLong();
  const diffs = lineDiffCount();
  for (let i = 0; i < 10; i++) fireEvent.keyDown(root, { key: "k" });
  console.log(`[long] lineDiff calls per k press over ${EDITS} Edit rows: ${(lineDiffCount() - diffs) / 10}`);
  expect(lineDiffCount() - diffs).toBe(0);
});

it("a k press over more replies than the markdown cache holds parses none of them", () => {
  const { root } = mountLong();
  const parses = markdownParseCount();
  for (let i = 0; i < 10; i++) fireEvent.keyDown(root, { key: "k" });
  console.log(`[long] markdown parses per k press over ${REPLIES} replies: ${(markdownParseCount() - parses) / 10}`);
  expect(markdownParseCount() - parses).toBe(0);
});

it("a reply streamed in 200 batches leaves one cached rendering of itself, not one per batch", () => {
  const { deliver } = mountLong();
  for (let i = 0; i < 200; i++) deliver(textDeltas(1));
  const versions = Array.from({ length: 200 }, (_, i) => "word ".repeat(i + 1));
  const held = versions.filter(isMarkdownCached).length;
  console.log(`[long] versions of one streamed reply held in the markdown cache: ${held} of 200`);
  expect(isMarkdownCached(versions[199])).toBe(true);
  expect(held).toBe(1);
});

it("a scroll event that changes no unread label renders nothing", () => {
  const { list, commits } = mountLong();
  const before = commits();
  for (let i = 0; i < 10; i++) fireEvent.scroll(list);
  console.log(`[long] commits per scroll event: ${(commits() - before) / 10}`);
  expect(commits() - before).toBe(0);
});

it("a streamed batch renders once, builds the timeline once and parses only the reply that grew", () => {
  const { commits, deliver } = mountLong();
  deliver(textDeltas(1)); // opens the streamed reply
  const before = { commits: commits(), timelines: timelineBuildCount(), parses: markdownParseCount(), diffs: lineDiffCount() };
  for (let i = 0; i < 10; i++) deliver(textDeltas(20));
  const per = (n: number) => n / 10;
  console.log(
    `[long] per batch of 20 deltas: commits ${per(commits() - before.commits)}, timelines ${per(timelineBuildCount() - before.timelines)}, parses ${per(markdownParseCount() - before.parses)}`,
  );
  expect(commits() - before.commits).toBe(10);
  expect(timelineBuildCount() - before.timelines).toBe(10);
  expect(markdownParseCount() - before.parses).toBe(10);
  expect(lineDiffCount() - before.diffs).toBe(0);
});
