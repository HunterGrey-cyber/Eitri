// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { act, cleanup, renderHook } from "@testing-library/react";
import { useRef, useState } from "react";
import { initialState } from "../reducer";
import { buildTimeline } from "../timeline";
import { pruneExpanded, usePreserveCursorRow } from "./panelEffects";

afterEach(cleanup);
it("drops expansions of evicted rows and keeps surviving runs and folded children", () => {
  const call = (seq: number) => ({ seq, turnId: "turn", toolUseId: `call${seq}`, name: "Read", input: {}, result: { content: "ok", isError: false } });
  const state = { ...initialState(), toolCalls: [call(20), call(21), { ...call(30), name: "Agent", children: {
    transcript: [], thinking: [], toolCalls: [call(31)], assistantMessageOpen: false, thinkingOpen: false,
  } }] };
  expect(pruneExpanded(state, { "t-1": true, "r-2": true, "t-20": true, "r-20": true, "t-31": true }))
    .toEqual({ "t-20": true, "r-20": true, "t-31": true });
});

it("clamps a dropped cursor to the first surviving row and follows a surviving key", () => {
  const flash = vi.fn();
  const before = buildTimeline({ ...initialState(), userPrompts: [0, 2, 4, 6, 8].map((seq) => ({ seq, text: `${seq}` })) });
  const after = before.slice(3);
  const { result, rerender } = renderHook(({ rows, boundary }) => {
    const [cursor, setCursor] = useState(2);
    const previous = useRef<typeof rows | null>(null);
    const previousCursor = useRef(cursor);
    const key = useRef<string | null>(null);
    const landing = useRef<1 | -1 | 0 | "keep">(0);
    usePreserveCursorRow(previous, rows, previousCursor, cursor, key, landing, setCursor, boundary, 1, null, flash);
    return { cursor, setCursor };
  }, { initialProps: { rows: before, boundary: 0 } });
  rerender({ rows: after, boundary: 6 });
  expect(result.current.cursor).toBe(0);
  expect(flash).toHaveBeenCalledTimes(1);
  act(() => result.current.setCursor(1));
  rerender({ rows: after.slice(1), boundary: 8 });
  expect(result.current.cursor).toBe(0);
  expect(flash).toHaveBeenCalledTimes(1);
});

it("keeps a restored cursor when switching to another tab with an eviction boundary", () => {
  const rows = (base: number) => buildTimeline({ ...initialState(), userPrompts: [0, 2, 4, 6, 8].map((n) => ({ seq: base + n, text: `${base + n}` })) });
  const { result, rerender } = renderHook(({ timeline, tab, boundary }) => {
    const [cursor, setCursor] = useState(2);
    const previous = useRef<typeof timeline | null>(null);
    const previousCursor = useRef(cursor);
    const key = useRef<string | null>(null);
    const landing = useRef<1 | -1 | 0 | "keep">(0);
    usePreserveCursorRow(previous, timeline, previousCursor, cursor, key, landing, setCursor, boundary, tab);
    return cursor;
  }, { initialProps: { timeline: rows(0), tab: 1, boundary: 0 } });
  rerender({ timeline: rows(100), tab: 2, boundary: 100 });
  expect(result.current).toBe(2);
});

it("keeps an expanded run after a thought separates it from an earlier run in expanded thinking", () => {
  const call = (seq: number) => ({ seq, turnId: "turn", toolUseId: `call${seq}`, name: "Read", input: {}, result: { content: "ok", isError: false } });
  const state = { ...initialState(), toolCalls: [call(20), call(21), call(23), call(24)],
    thinking: [{ seq: 22, turnId: "turn", text: "thought" }] };
  expect(pruneExpanded(state, { "r-1": true, "r-23": true }, "expanded"))
    .toEqual({ "r-23": true });
});
