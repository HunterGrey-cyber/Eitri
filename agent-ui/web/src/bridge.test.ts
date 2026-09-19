// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { installDispatch, nextRequestId } from "./bridge";

describe("nextRequestId", () => {
  it("returns distinct values on successive calls", () => {
    const a = nextRequestId();
    const b = nextRequestId();
    const c = nextRequestId();
    expect(new Set([a, b, c]).size).toBe(3);
  });
});

describe("installDispatch", () => {
  it("demuxes a command_result envelope with ok:true", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__neovibeDispatch!(JSON.stringify({ kind: "command_result", requestId: "req-1", ok: true }));
    expect(handler).toHaveBeenCalledWith({ kind: "command_result", requestId: "req-1", ok: true });
  });

  it("demuxes a command_result envelope with ok:false and an error message", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__neovibeDispatch!(JSON.stringify({ kind: "command_result", requestId: "req-2", ok: false, error: "boom" }));
    expect(handler).toHaveBeenCalledWith({ kind: "command_result", requestId: "req-2", ok: false, error: "boom" });
  });

  it("demuxes an events envelope, keeping fromRevision/throughRevision/events intact", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const events = [{ type: "turn_started", turn_id: "t1" }];
    window.__neovibeDispatch!(JSON.stringify({ kind: "events", fromRevision: 3, throughRevision: 4, events }));
    expect(handler).toHaveBeenCalledWith({ kind: "events", fromRevision: 3, throughRevision: 4, events });
  });

  it("demuxes a snapshot envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const state = {
      sessionId: "abc", model: "m", cwd: "/tmp", transcript: [], toolCalls: [],
      status: { kind: "running" }, activeTurnId: null, pendingPermissions: [],
    };
    window.__neovibeDispatch!(JSON.stringify({ kind: "snapshot", throughRevision: 7, state }));
    expect(handler).toHaveBeenCalledWith({ kind: "snapshot", throughRevision: 7, state });
  });

  /* The `handoff` envelope has to survive the kind whitelist: an envelope kind this build does not
     list is warned about and dropped, so a new one that is not added there arrives nowhere and
     nothing reports it -- the panel would simply never show the command after a real close. */
  it("demuxes a handoff envelope, keeping the command and its parts intact", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const envelope = {
      kind: "handoff",
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    };
    window.__neovibeDispatch!(JSON.stringify(envelope));
    expect(handler).toHaveBeenCalledWith(envelope);
  });

  it("demuxes a theme envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__neovibeDispatch!(JSON.stringify({ kind: "theme", vars: { "--nv-bg": "#faf4ed" } }));
    expect(handler).toHaveBeenCalledWith({ kind: "theme", vars: { "--nv-bg": "#faf4ed" } });
  });

  it("demuxes an enter_input envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__neovibeDispatch!(JSON.stringify({ kind: "enter_input" }));
    expect(handler).toHaveBeenCalledWith({ kind: "enter_input" });
  });

  it("demuxes a pane_focus envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__neovibeDispatch!(JSON.stringify({ kind: "pane_focus", focused: true }));
    expect(handler).toHaveBeenCalledWith({ kind: "pane_focus", focused: true });
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
