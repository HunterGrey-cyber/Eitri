import { afterEach, describe, expect, it, vi } from "vitest";
import { trace } from "./trace";

afterEach(() => { vi.restoreAllMocks(); });

describe("trace", () => {
  it("writes one console.info line per event, the data as JSON", () => {
    const info = vi.spyOn(console, "info").mockImplementation(() => {});
    trace("start", {});
    expect(info.mock.calls).toEqual([["[page-trace] start {}"]]);
    info.mockClear();
    trace("top", { line: 119 });
    expect(info.mock.calls).toEqual([["[page-trace] top {\"line\":119}"]]);
  });
});
