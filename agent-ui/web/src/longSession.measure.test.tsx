// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import App from "./App";
import { timelineRowRenderCount } from "./components/MessageList";
import { buildDisplay } from "./display";
import { longSessionState } from "./fixtures/longSession";
import { runLongSessionBench } from "./fixtures/longSessionBench";
import { markdownParseCount } from "./markdown";
import { buildTimeline } from "./timeline";

/* Measure page work at three conversation lengths when `LONG_SESSION_MEASURE=1`.
   Print scaling data without timing assertions because jsdom does not measure WebKitGTK performance.
   `tools/measure/panel_bench.py` runs the same operations in real WebKit. */

afterEach(cleanup);
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const SIZES = (process.env.LONG_SESSION_SIZES ?? "300,1500,6000").split(",").map(Number);
const enabled = process.env.LONG_SESSION_MEASURE === "1";

describe("the long-session fixture", () => {
  it("draws about as many rows as it was asked for", () => {
    const state = longSessionState(110);
    const rows = buildDisplay(buildTimeline(state), { expanded: {}, detailed: false, turnRunning: false, thinking: "collapsed" });
    expect(rows.length).toBe(110);
  });
});

describe.skipIf(!enabled)("a long session", () => {
  for (const size of SIZES) {
    it(`${size} rows`, async () => {
      (window as unknown as { webkit: unknown }).webkit = { messageHandlers: { eitriAgent: { postMessage: () => {} } } };
      render(<App />);
      const rowsBefore = timelineRowRenderCount();
      const parsesBefore = markdownParseCount();
      const result = await runLongSessionBench(size, {
        work: async (run) => {
          const t0 = performance.now();
          await act(async () => run());
          return performance.now() - t0;
        },
        pause: async () => {},
      });
      for (const [name, stat] of Object.entries(result.operations)) {
        console.log(
          `[long-session] ${size} rows: ${name}: median ${stat.median.toFixed(2)} ms, p90 ${stat.p90.toFixed(2)} ms, n ${stat.n}`,
        );
      }
      console.log(
        `[long-session] ${size} rows: drew ${result.drawnRows}, snapshot ${result.snapshotBytes} B, fold ${result.parts.foldOneDeltaMs.toFixed(3)} ms, ` +
          `timeline ${result.parts.timelineMs.toFixed(2)} ms, tail markdown ${result.parts.markdownOfTailMs.toFixed(2)} ms, ` +
          `row renders ${timelineRowRenderCount() - rowsBefore}, parses ${markdownParseCount() - parsesBefore}, heap ${(process.memoryUsage().heapUsed / 1e6).toFixed(0)} MB`,
      );
    }, 1_800_000);
  }
});
