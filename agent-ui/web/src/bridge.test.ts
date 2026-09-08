// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { installDispatch } from "./bridge";

describe("installDispatch", () => {
  it("demuxes an event envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__neovibeDispatch!(JSON.stringify({ kind: "event", event: { type: "assistant_text", text: "hi" } }));
    expect(handler).toHaveBeenCalledWith({ kind: "event", event: { type: "assistant_text", text: "hi" } });
  });

  it("demuxes a snapshot envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const snapshot = { sessionId: "abc", model: "m", cwd: "/tmp", transcript: [], toolCalls: [], status: { kind: "running" }, turnInProgress: false, pendingPermissions: [] };
    window.__neovibeDispatch!(JSON.stringify({ kind: "snapshot", snapshot }));
    expect(handler).toHaveBeenCalledWith({ kind: "snapshot", snapshot });
  });

  it("demuxes an error envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__neovibeDispatch!(JSON.stringify({ kind: "error", message: "boom" }));
    expect(handler).toHaveBeenCalledWith({ kind: "error", message: "boom" });
  });

  it("warns and does not throw on an unrecognized envelope kind", () => {
    const handler = vi.fn();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    installDispatch(handler);
    expect(() => window.__neovibeDispatch!(JSON.stringify({ kind: "something_future" }))).not.toThrow();
    expect(handler).not.toHaveBeenCalled();
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });

  it("warns and does not throw on invalid JSON", () => {
    const handler = vi.fn();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    installDispatch(handler);
    expect(() => window.__neovibeDispatch!("not json {{{")).not.toThrow();
    expect(handler).not.toHaveBeenCalled();
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });
});
