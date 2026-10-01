// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { installDispatch, nextRequestId } from "./bridge";
import type { OutboundMessage } from "./bridge";

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
    window.__eitriDispatch!(JSON.stringify({ kind: "command_result", requestId: "req-1", ok: true }));
    expect(handler).toHaveBeenCalledWith({ kind: "command_result", requestId: "req-1", ok: true });
  });

  it("demuxes a command_result envelope with ok:false and an error message", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "command_result", requestId: "req-2", ok: false, error: "boom" }));
    expect(handler).toHaveBeenCalledWith({ kind: "command_result", requestId: "req-2", ok: false, error: "boom" });
  });

  it("demuxes an events envelope, keeping fromRevision/throughRevision/events intact", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const events = [{ type: "turn_started", turn_id: "t1" }];
    window.__eitriDispatch!(JSON.stringify({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 4, events }));
    expect(handler).toHaveBeenCalledWith({ kind: "events", tab: 1, fromRevision: 3, throughRevision: 4, events });
  });

  it("demuxes a snapshot envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const state = {
      sessionId: "abc", model: "m", cwd: "/tmp", transcript: [], toolCalls: [],
      status: { kind: "running" }, activeTurnId: null, pendingPermissions: [],
    };
    window.__eitriDispatch!(JSON.stringify({ kind: "snapshot", tab: 1, throughRevision: 7, state }));
    expect(handler).toHaveBeenCalledWith({ kind: "snapshot", tab: 1, throughRevision: 7, state });
  });

  /* The `handoff` envelope has to survive the kind whitelist: an envelope kind this build does not
     list is warned about and dropped, so a new one that is not added there arrives nowhere and
     nothing reports it -- the panel would simply never show the command after a real close. */
  it("demuxes a handoff envelope, keeping the command and its parts intact", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const envelope = {
      kind: "handoff",
      tab: 1,
      command: "cd /home/user/project && claude --resume 1857dcd5-973b-46a2",
      cwd: "/home/user/project",
      providerSessionId: "1857dcd5-973b-46a2",
    };
    window.__eitriDispatch!(JSON.stringify(envelope));
    expect(handler).toHaveBeenCalledWith(envelope);
  });

  it("demuxes a theme envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "theme", vars: { "--nv-bg": "#faf4ed" } }));
    expect(handler).toHaveBeenCalledWith({ kind: "theme", vars: { "--nv-bg": "#faf4ed" } });
  });

  it("demuxes an enter_input envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "enter_input" }));
    expect(handler).toHaveBeenCalledWith({ kind: "enter_input" });
  });

  it.each([
    { kind: "literal_key", key: "C-a" },
    { kind: "open_keymap" },
    { kind: "open_command_line" },
    { kind: "keymap", prefix: "Ctrl+b", window: [], prefixKeys: [] },
  ])("demuxes a $kind envelope", (payload) => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify(payload));
    expect(handler).toHaveBeenCalledWith(payload);
  });

  it("no longer accepts select_all", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "select_all" }));
    expect(handler).not.toHaveBeenCalledWith({ kind: "select_all" });
  });

  it("demuxes a pane_focus envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "pane_focus", focused: true }));
    expect(handler).toHaveBeenCalledWith({ kind: "pane_focus", focused: true });
  });

  it("demuxes an error envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "error", tab: 1, message: "boom" }));
    expect(handler).toHaveBeenCalledWith({ kind: "error", tab: 1, message: "boom" });
  });

  it("accepts the five tab envelopes", () => {
    const seen: string[] = [];
    installDispatch((p) => seen.push(p.kind));
    for (const kind of ["tabs", "tab_detail", "chooser", "confirm_close", "begin_rename"]) {
      window.__eitriDispatch!(JSON.stringify({ kind, tab: 1, active: 1, tabs: [], rows: [], launch: false, open: [], records: [], lines: [], current: null }));
    }
    expect(seen).toEqual(["tabs", "tab_detail", "chooser", "confirm_close", "begin_rename"]);
  });

  it("demuxes an editor_typing envelope, and drops nothing of it", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "editor_typing", typing: true, periodMs: 500 }));
    window.__eitriDispatch!(JSON.stringify({ kind: "editor_typing", typing: false, periodMs: 500 }));
    expect(handler).toHaveBeenNthCalledWith(1, { kind: "editor_typing", typing: true, periodMs: 500 });
    expect(handler).toHaveBeenNthCalledWith(2, { kind: "editor_typing", typing: false, periodMs: 500 });
  });

  it("demuxes a nav_key envelope, down and up", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "nav_key", direction: "down" }));
    window.__eitriDispatch!(JSON.stringify({ kind: "nav_key", direction: "up" }));
    expect(handler).toHaveBeenNthCalledWith(1, { kind: "nav_key", direction: "down" });
    expect(handler).toHaveBeenNthCalledWith(2, { kind: "nav_key", direction: "up" });
  });

  it("warns and does not throw on an unrecognized envelope kind", () => {
    const handler = vi.fn();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    installDispatch(handler);
    expect(() => window.__eitriDispatch!(JSON.stringify({ kind: "something_future" }))).not.toThrow();
    expect(handler).not.toHaveBeenCalled();
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });

  it("warns and does not throw on invalid JSON", () => {
    const handler = vi.fn();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    installDispatch(handler);
    expect(() => window.__eitriDispatch!("not json {{{")).not.toThrow();
    expect(handler).not.toHaveBeenCalled();
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });

  it("demuxes hint_collect envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "hint_collect", sessionId: 7 }));
    expect(handler).toHaveBeenCalledWith({ kind: "hint_collect", sessionId: 7 });
  });

  it("demuxes hint_show envelope with labels", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const envelope = { kind: "hint_show", sessionId: 7, labels: ["a", "s", "d"] };
    window.__eitriDispatch!(JSON.stringify(envelope));
    expect(handler).toHaveBeenCalledWith(envelope);
  });

  it("demuxes hint_prefix envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "hint_prefix", sessionId: 7, typed: "a" }));
    expect(handler).toHaveBeenCalledWith({ kind: "hint_prefix", sessionId: 7, typed: "a" });
  });

  it("demuxes hint_land envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "hint_land", sessionId: 7, index: 2 }));
    expect(handler).toHaveBeenCalledWith({ kind: "hint_land", sessionId: 7, index: 2 });
  });

  it("demuxes hint_end envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "hint_end", sessionId: 7 }));
    expect(handler).toHaveBeenCalledWith({ kind: "hint_end", sessionId: 7 });
  });

  it("demuxes a confirm_bypass envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const envelope = { kind: "confirm_bypass", tab: 3, scope: "tab", nonce: 17, lines: ["Switch to bypass and approve the 2 waiting cards? (y/n)"] };
    window.__eitriDispatch!(JSON.stringify(envelope));
    expect(handler).toHaveBeenCalledWith(envelope);
  });

  it("demuxes a confirm_bypass envelope with a null tab (window default scope)", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const envelope = { kind: "confirm_bypass", tab: null, scope: "default", nonce: 18, lines: ["Start new sessions in bypass? (y/n)"] };
    window.__eitriDispatch!(JSON.stringify(envelope));
    expect(handler).toHaveBeenCalledWith(envelope);
  });

  /* D7/D11: a `y` can only ever echo a real nonce back -- one this side never made up itself -- so
     an envelope with none is rejected up front, the same as invalid JSON or an unrecognized kind. */
  it("rejects a confirm_bypass envelope with no nonce, like other malformed envelopes", () => {
    const handler = vi.fn();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "confirm_bypass", tab: 3, scope: "tab", lines: ["?"] }));
    expect(handler).not.toHaveBeenCalled();
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });

  it("demuxes a confirm_restore envelope and rejects one with no nonce", () => {
    const handler = vi.fn();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    installDispatch(handler);
    const envelope = { kind: "confirm_restore", nonce: 4, lines: ["Restore 3 tabs (1 in bypass)? y/n"] };
    window.__eitriDispatch!(JSON.stringify(envelope));
    expect(handler).toHaveBeenCalledWith(envelope);
    handler.mockClear();
    window.__eitriDispatch!(JSON.stringify({ kind: "confirm_restore", lines: ["?"] }));
    expect(handler).not.toHaveBeenCalled();
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });

  it("passes every phase 3 envelope through the whitelist", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const envelopes = [
      { kind: "queue", tab: 1, items: [{ text: "later", queuedAt: 5 }], error: null },
      { kind: "draft", tab: 1, text: "half" },
      { kind: "queue_taken", tab: 1, texts: ["a"] },
      { kind: "history", entries: ["old"] },
      { kind: "rule_offers", tab: 1, offers: { "perm-1": "git push *" } },
      { kind: "editor_context", file: "src/a.rs", lines: [1, 2] },
      { kind: "scratch", tab: 1, editing: true },
      { kind: "notice", text: "no such file" },
    ];
    for (const envelope of envelopes) window.__eitriDispatch!(JSON.stringify(envelope));
    expect(handler.mock.calls.map((c) => c[0])).toEqual(envelopes);
  });
});

/* V1 §3.5, the plan's own "Interfaces" block: the exact wire strings `panel_keys`/`nav_fallthrough`
   serialise to, byte for byte -- Task 2 pins the same five in a core test, so the two sides can
   never silently drift apart on field order or spelling. */
describe("OutboundMessage: panel_keys and nav_fallthrough serialise exactly (Interfaces block)", () => {
  it.each<[OutboundMessage, string]>([
    [{ type: "panel_keys", request_id: "req-7", mode: "browse" }, '{"type":"panel_keys","request_id":"req-7","mode":"browse"}'],
    [{ type: "panel_keys", request_id: "req-8", mode: "input" }, '{"type":"panel_keys","request_id":"req-8","mode":"input"}'],
    [{ type: "panel_keys", request_id: "req-9", mode: "other" }, '{"type":"panel_keys","request_id":"req-9","mode":"other"}'],
    [{ type: "nav_fallthrough", request_id: "req-10", direction: "down" }, '{"type":"nav_fallthrough","request_id":"req-10","direction":"down"}'],
    [{ type: "nav_fallthrough", request_id: "req-11", direction: "up" }, '{"type":"nav_fallthrough","request_id":"req-11","direction":"up"}'],
  ])("%o -> %s", (message, wire) => {
    expect(JSON.stringify(message)).toBe(wire);
  });
});

/* v1 picks, Task 6 (R11): `pane_nav`'s exact wire strings, all four sides, field order included -- the core
   test `parses_pane_nav_in_all_four_directions_as_a_window_level_message` parses these same four
   strings, so the two sides cannot drift apart on spelling (the words are the sides' names, never the
   letters `h`/`j`/`k`/`l`). */
describe("OutboundMessage: pane_nav serialises exactly (v1 picks, Task 6)", () => {
  it.each<[OutboundMessage, string]>([
    [{ type: "pane_nav", request_id: "req-1", direction: "left" }, '{"type":"pane_nav","request_id":"req-1","direction":"left"}'],
    [{ type: "pane_nav", request_id: "req-2", direction: "down" }, '{"type":"pane_nav","request_id":"req-2","direction":"down"}'],
    [{ type: "pane_nav", request_id: "req-3", direction: "up" }, '{"type":"pane_nav","request_id":"req-3","direction":"up"}'],
    [{ type: "pane_nav", request_id: "req-4", direction: "right" }, '{"type":"pane_nav","request_id":"req-4","direction":"right"}'],
  ])("%o -> %s", (message, wire) => {
    expect(JSON.stringify(message)).toBe(wire);
  });
});

/* v1 picks, Task 8 (R6): `open_url`'s exact wire string, field order included -- the core test
   `parses_open_url_as_a_window_level_message` parses this same string, so the two sides cannot drift apart
   on spelling. The page sends the WHATWG-normalized `href` (`nav.ts#webUrl`), and Rust re-checks it
   (`agent_panel.rs#web_url`) before it reaches `UriLauncher`. */
describe("OutboundMessage: open_url serialises exactly (v1 picks, Task 8)", () => {
  it("names the type, the request id and the url, in that order", () => {
    const message: OutboundMessage = { type: "open_url", request_id: "req-1", url: "https://example.com/a" };
    expect(JSON.stringify(message)).toBe('{"type":"open_url","request_id":"req-1","url":"https://example.com/a"}');
  });
});
