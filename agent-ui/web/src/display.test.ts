import { describe, expect, it } from "vitest";
import { buildDisplay, indexOfKey, runSummary } from "./display";
import type { TimelineItem } from "./timeline";
import type { ToolCallRecord } from "./types";

const call = (seq: number, name: string, done = true): ToolCallRecord => ({
  seq, toolUseId: `t${seq}`, name, input: {}, result: done ? { content: "ok", isError: false } : null,
});
const errorCall = (seq: number, name: string): ToolCallRecord => ({
  seq, toolUseId: `t${seq}`, name, input: {}, result: { content: "boom", isError: true },
});
const tool = (c: ToolCallRecord): TimelineItem => ({ kind: "tool", seq: c.seq, key: `t-${c.seq}`, call: c });
const msg = (seq: number): TimelineItem => ({ kind: "message", seq, key: `m-${seq}`, text: "m" });
const opts = { expanded: {}, detailed: false, turnRunning: false };

describe("P2 runs", () => {
  it("collapses two or more finished calls in a row into one summary row", () => {
    const items = [msg(1), tool(call(2, "Read")), tool(call(3, "Read")), tool(call(4, "Bash")), msg(5)];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["message", "run", "message"]);
    const run = shown[1] as Extract<TimelineItem, { kind: "run" }>;
    expect(run.key).toBe("r-2");
    expect(runSummary(run.calls)).toBe("Read ×2 · Bash ×1");
  });

  it("leaves a single call, a running call, the running turn's tail and an expanded run alone", () => {
    expect(buildDisplay([msg(1), tool(call(2, "Read")), msg(3)], opts).map((i) => i.kind)).toEqual(["message", "tool", "message"]);
    expect(buildDisplay([tool(call(1, "Read")), tool(call(2, "Bash", false))], opts).map((i) => i.kind)).toEqual(["tool", "tool"]);
    expect(
      buildDisplay([tool(call(1, "Read")), tool(call(2, "Read"))], { ...opts, turnRunning: true }).map((i) => i.kind),
      "the tail of a running turn is still arriving",
    ).toEqual(["tool", "tool"]);
    const items = [tool(call(1, "Read")), tool(call(2, "Read")), msg(3)];
    expect(buildDisplay(items, { ...opts, expanded: { "r-1": true } }).map((i) => i.kind)).toEqual(["tool", "tool", "message"]);
    expect(buildDisplay(items, { ...opts, detailed: true }).map((i) => i.kind), "Ctrl+o shows everything").toEqual(["tool", "tool", "message"]);
  });

  it("never swallows a call a card is waiting on", () => {
    const card: TimelineItem = { kind: "permission", seq: 3, key: "p-3", request: { seq: 3, permissionId: "p", toolUseId: "t2", toolName: "Bash", input: {} } };
    expect(buildDisplay([tool(call(1, "Read")), tool(call(2, "Bash")), card, msg(4)], opts).map((i) => i.kind)).toEqual(["tool", "tool", "permission", "message"]);
  });

  it("sw-panel-render-5: never folds a failing call behind a run's ✓ sign", () => {
    // Probe lifted from the verdict: `toolCalls [Bash isError:true, Read ok]` followed by a
    // message. Before the fix, buildDisplay's kinds were `['run', 'message']` -- the failing Bash
    // call folded into a run row that draws a fixed `✓` sign (`MessageList.tsx`) and a count-only
    // summary (`runSummary`), with no `✗` anywhere. The failing call must end the run instead, so
    // it keeps its own row -- exactly like the identical call shown alone (not folded) already does.
    const items = [tool(errorCall(1, "Bash")), tool(call(2, "Read")), msg(3)];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["tool", "tool", "message"]);
  });

  it("still folds a run of all-successful calls even with a failure elsewhere in the timeline", () => {
    // Regression guard alongside the fix above: excluding a failure from the run it would otherwise
    // join must not stop two SUCCESSFUL calls next to each other from folding into a run.
    const items = [tool(call(1, "Read")), tool(call(2, "Read")), tool(errorCall(3, "Bash")), msg(4)];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["run", "tool", "message"]);
  });

  it("v1 trial item 7: an edit between reads is never counted into a fold, on either side", () => {
    // 4A (the acceptEdits fast path) auto-allows in-project Write/Edit/NotebookEdit with no card,
    // so nothing else marks an edit as different from an ordinary finished, ungated call -- without
    // this rule it would have folded into "Read ×4 · Edit ×1" and the change would have vanished
    // behind a count. A run of other tools now stops at it instead, on both sides.
    const items = [tool(call(1, "Read")), tool(call(2, "Read")), tool(call(3, "Edit")), tool(call(4, "Read")), tool(call(5, "Read"))];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["run", "tool", "run"]);
    const [before, edit, after] = shown as [Extract<TimelineItem, { kind: "run" }>, Extract<TimelineItem, { kind: "tool" }>, Extract<TimelineItem, { kind: "run" }>];
    expect(runSummary(before.calls)).toBe("Read ×2");
    expect(edit.call.toolUseId).toBe("t3");
    expect(runSummary(after.calls)).toBe("Read ×2");
  });

  it("v1 trial item 7: Write, Edit and NotebookEdit never fold into a count-only run", () => {
    const items = [tool(call(1, "Edit")), tool(call(2, "Edit")), tool(call(3, "Edit")), tool(call(4, "Edit")), tool(call(5, "Edit"))];
    const shown = buildDisplay(items, opts);
    expect(shown.map((i) => i.kind)).toEqual(["tool", "tool", "tool", "tool", "tool"]);
    expect(shown.map((i) => (i as Extract<TimelineItem, { kind: "tool" }>).call.toolUseId)).toEqual(["t1", "t2", "t3", "t4", "t5"]);
  });

  it("v1 trial item 7: Write and NotebookEdit stop a fold the same way Edit does", () => {
    const items = [tool(call(1, "Read")), tool(call(2, "Write")), tool(call(3, "Read")), tool(call(4, "NotebookEdit")), tool(call(5, "Read"))];
    expect(buildDisplay(items, opts).map((i) => i.kind)).toEqual(["tool", "tool", "tool", "tool", "tool"]);
  });

  it("finds a row by key, inside a run too", () => {
    const shown = buildDisplay([msg(1), tool(call(2, "Read")), tool(call(3, "Read")), msg(4)], opts);
    expect(indexOfKey(shown, "m-4")).toBe(2);
    expect(indexOfKey(shown, "t-3")).toBe(1);
    expect(indexOfKey(shown, "x")).toBeNull();
  });
});
