// @vitest-environment jsdom
import { afterEach, beforeAll, describe, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import App from "./App";
import { renderMarkdown } from "./markdown";
import { initialState } from "./reducer";

/* How the panel's page spends time on one long streamed reply, measured at three sizes. Opt-in
   (`LONG_REPLY_MEASURE=1`): it prints a table and asserts nothing, since jsdom's clock is not WebKitGTK's and a
   speed test names no absolute bound. What it is for is the shape: does the time of the last batch grow with the
   text already held (a reply drawn again from its first byte on every batch), and what does the end of the turn cost. */

afterEach(cleanup);
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const TAB = { id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null };
const BATCH_BYTES = Number(process.env.LONG_REPLY_BATCH ?? 1024);
const SIZES = [100_000, 400_000, 1_200_000];

function dispatch(payload: unknown) {
  window.__eitriDispatch!(JSON.stringify(payload));
}

function line(i: number): string {
  // One markdown line of about BATCH_BYTES, the way the scripted stand-in's lines run.
  const head = `line ${i} `;
  return head + "x".repeat(Math.max(0, BATCH_BYTES - head.length - 1)) + "\n";
}

const enabled = process.env.LONG_REPLY_MEASURE === "1";

describe.skipIf(!enabled)("a long streamed reply", () => {
  for (const size of SIZES) {
    it(`${size} bytes in batches of ${BATCH_BYTES}`, () => {
      (window as unknown as { webkit: unknown }).webkit = { messageHandlers: { eitriAgent: { postMessage: () => {} } } };
      render(<App />);
      act(() => {
        dispatch({ kind: "tabs", active: 1, tabs: [TAB] });
        dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: { ...initialState(), status: { kind: "running" }, activeTurnId: "t1", nextSeq: 1 } });
      });
      let revision = 1;
      const deliver = (events: unknown[]) => {
        act(() => dispatch({ kind: "events", tab: 1, fromRevision: revision, throughRevision: revision + events.length, events }));
        revision += events.length;
      };
      const batches = Math.ceil(size / BATCH_BYTES);
      const times: number[] = [];
      const started = performance.now();
      for (let i = 0; i < batches; i++) {
        const t0 = performance.now();
        deliver([{ type: "content_delta", kind: "text", text: line(i) }]);
        times.push(performance.now() - t0);
      }
      const streamed = performance.now() - started;
      const t0 = performance.now();
      deliver([
        {
          type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: "", stop_reason: null, usage: null,
          detail: { reason: null, api_error_status: null, message: null },
        },
      ]);
      const ended = performance.now() - t0;
      const mean = (xs: number[]) => xs.reduce((a, b) => a + b, 0) / Math.max(1, xs.length);
      const tenth = Math.max(1, Math.floor(batches / 10));
      const first = mean(times.slice(0, tenth));
      const last = mean(times.slice(-tenth));
      console.log(
        `[long-reply] page ${size} B: ${batches} batches, total ${streamed.toFixed(0)} ms, first-tenth batch ${first.toFixed(2)} ms, last-tenth batch ${last.toFixed(2)} ms (x${(last / first).toFixed(1)}), turn end ${ended.toFixed(0)} ms`,
      );
    }, 1_800_000);
  }

  it("one drawing of the whole text through markdown (marked, highlight, sanitize), cold", () => {
    for (const size of SIZES) {
      let text = "";
      for (let i = 0; text.length < size; i++) text += line(i);
      const t0 = performance.now();
      renderMarkdown(text);
      console.log(`[long-reply] markdown of ${size} B in one call: ${(performance.now() - t0).toFixed(0)} ms`);
    }
  }, 600_000);
});
