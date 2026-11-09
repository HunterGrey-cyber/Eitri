// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { installDispatch, nextRequestId } from "./bridge";
import type { OutboundMessage } from "./bridge";
import editorCards from "./fixtures/editor-cards.json";

describe("nextRequestId", () => {
  it("returns distinct values on successive calls", () => {
    const a = nextRequestId();
    const b = nextRequestId();
    const c = nextRequestId();
    expect(new Set([a, b, c]).size).toBe(3);
  });
});

describe("installDispatch", () => {
  it("demuxes the turn review's three envelopes, and the requests the overlay posts type-check", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const review = { kind: "review", requestId: "r1", tab: 3, scope: "turn", current: 7, turns: [], files: [], compared: true, pendingNoResult: 0, notes: [] };
    const diff = { kind: "review_diff", requestId: "r2", tab: 3, turn: 7, path: "a.rs", added: 1, removed: 0, hunks: null };
    const hint = { kind: "review_hint", tab: 3, turn: 7, files: 2 };
    for (const envelope of [review, diff, hint]) window.__eitriDispatch!(JSON.stringify(envelope));
    expect(handler.mock.calls.map((call) => call[0])).toEqual([review, diff, hint]);
    const sent: OutboundMessage[] = [
      { type: "review_request", request_id: "r1", tab: 3, turn: "latest", scope: "turn" },
      { type: "review_request", request_id: "r2", tab: 3, turn: 6, scope: "session" },
      { type: "review_diff_request", request_id: "r3", tab: 3, turn: 7, scope: "turn", path: "a.rs" },
    ];
    expect(sent).toHaveLength(3);
  });

  it("carries task requests and delivers the plain output reply with its correlation fields", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const reply = { kind: "task_output_result", requestId: "o1", tab: 3, taskId: "task", output: "�<b>plain</b>", totalBytes: 9000, truncated: true };
    window.__eitriDispatch!(JSON.stringify(reply));
    expect(handler).toHaveBeenCalledWith(reply);
    const requests: OutboundMessage[] = [
      { type: "stop_task", request_id: "s1", tab: 3, task_id: "task" },
      { type: "task_output", request_id: "o1", tab: 3, task_id: "task" },
    ];
    expect(requests.map((request) => JSON.parse(JSON.stringify(request)).task_id)).toEqual(["task", "task"]);
  });

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

  it("demuxes a review_settings envelope", () => {
    const handler = vi.fn();
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "review_settings", enabled: true, hint: false }));
    expect(handler).toHaveBeenCalledWith({ kind: "review_settings", enabled: true, hint: false });
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

  /* A `y` can only ever echo a real nonce back -- one this side never made up itself -- so
     an envelope with none is rejected up front, the same as invalid JSON or an unrecognized kind. */
  it("demuxes the editor_cards envelope Rust serialises, and rejects a list the card view cannot draw", () => {
    const handler = vi.fn();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    installDispatch(handler);
    const good = { kind: "editor_cards", tab: 1, cards: editorCards };
    window.__eitriDispatch!(JSON.stringify(good));
    expect(handler.mock.calls.map((call) => call[0])).toEqual([good]);
    for (const cards of [
      "nope",
      [{ ...editorCards[0], body: "text" }],
      [{ ...editorCards[0], body: [{ kind: "markdown", text: "x" }] }],
      [{ ...editorCards[0], id: 7 }],
    ]) window.__eitriDispatch!(JSON.stringify({ kind: "editor_cards", tab: 1, cards }));
    expect(handler).toHaveBeenCalledTimes(1);
    warn.mockRestore();
  });

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

  describe("the trust question", () => {
    const HEX = "a1".repeat(32);
    const prompt = (over: Record<string, unknown> = {}) => ({
      kind: "trust_prompt", tab: 3, nonce: 17, agent: "Claude", root: "/p/src", top: "/p", fingerprint: HEX, findingsDigest: HEX,
      state: "untrusted", remember: "yes", rememberNote: null, changed: null, items: [], ...over,
    });
    const dispatched = (envelope: Record<string, unknown>) => {
      const handler = vi.fn();
      const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
      installDispatch(handler);
      window.__eitriDispatch!(JSON.stringify(envelope));
      const warned = warn.mock.calls.length > 0;
      warn.mockRestore();
      return { handler, warned };
    };

    it("demuxes a well-formed trust_prompt, for each remember value", () => {
      for (const remember of ["yes", "window", "session"]) {
        const envelope = prompt({ remember, rememberNote: remember === "yes" ? null : "note" });
        const { handler, warned } = dispatched(envelope);
        expect(handler).toHaveBeenCalledWith(envelope);
        expect(warned).toBe(false);
      }
    });

    it("the_bridge_rejects_a_trust_prompt_without_a_numeric_nonce", () => {
      for (const nonce of [undefined, "17", null]) {
        const { handler, warned } = dispatched(prompt({ nonce }));
        expect(handler).not.toHaveBeenCalled();
        expect(warned).toBe(true);
      }
    });

    it("accepts absent or well-formed instruction texts without changing their order", () => {
      for (const texts of [undefined, [], [
        { kind: "diff", path: "CLAUDE.md", added: 1, removed: 1, skippedAfter: 2, hunks: [
          { skipped: 3, lines: [
            { kind: "context", text: "same" }, { kind: "removed", text: "old" },
            { kind: "added", text: "new", noNewline: true },
          ] },
        ] },
        { kind: "plain", path: "AGENTS.md", line: "no earlier copy" },
      ]]) {
        const envelope = prompt({ changed: { added: [], removed: [], changed: [], texts } });
        const { handler, warned } = dispatched(envelope);
        expect(handler).toHaveBeenCalledWith(JSON.parse(JSON.stringify(envelope)));
        expect(warned).toBe(false);
      }
    });

    it("drops an envelope with malformed instruction texts", () => {
      const line = { kind: "added", text: "new", noNewline: true };
      const hunk = { skipped: 1, lines: [line] };
      const diff = { kind: "diff", path: "CLAUDE.md", added: 1, removed: 0, skippedAfter: 1, hunks: [hunk] };
      const plain = { kind: "plain", path: "AGENTS.md", line: "no earlier copy" };
      const malformed: unknown[] = [null, {}, "text", 1, [null], ["text"], [1], [{}],
        [{ kind: "diff", path: "old wire", before: "a", after: "b" }],
        [{ ...diff, kind: "other" }], [{ ...plain, kind: undefined }],
      ];
      for (const entry of [diff, plain]) {
        for (const field of Object.keys(entry).filter((key) => key !== "kind")) {
          for (const value of [undefined, null, [], {}]) {
            if (field === "hunks" && Array.isArray(value)) continue;
            malformed.push([{ ...entry, [field]: value }]);
          }
          if (field === "path" || field === "line") {
            for (const value of [1, true]) malformed.push([{ ...entry, [field]: value }]);
          }
        }
      }
      for (const field of ["added", "removed", "skippedAfter"]) {
        for (const value of [-1, 0.5, "1", true]) malformed.push([{ ...diff, [field]: value }]);
      }
      for (const value of [undefined, null, -1, 0.5, "1", true, {}, []]) {
        malformed.push([{ ...diff, hunks: [{ ...hunk, skipped: value }] }]);
      }
      for (const value of [null, 1, "hunk", [], {}]) malformed.push([{ ...diff, hunks: [value] }]);
      for (const value of [undefined, null, 1, "lines", {}]) {
        malformed.push([{ ...diff, hunks: [{ ...hunk, lines: value }] }]);
      }
      const badLines = [null, 1, "line", [], {},
        ...[undefined, null, "other", 1, {}, []].map((kind) => ({ ...line, kind })),
        ...[undefined, null, 1, true, {}, []].map((text) => ({ ...line, text })),
        ...[null, false, 0, "true", {}, []].map((noNewline) => ({ ...line, noNewline })),
      ];
      for (const bad of badLines) malformed.push([{ ...diff, hunks: [{ ...hunk, lines: [bad] }] }]);
      for (const texts of malformed) {
        const { handler, warned } = dispatched(prompt({ changed: { added: [], removed: [], changed: [], texts } }));
        expect(handler, JSON.stringify(texts)).not.toHaveBeenCalled();
        expect(warned).toBe(true);
      }
      expect(dispatched(prompt({ changed: { added: [], removed: [], changed: [], texts: [diff, { ...plain, line: 1 }] } })).handler)
        .not.toHaveBeenCalled();
    });

    it("rejects a trust_prompt whose fingerprint or digest is not 64 hex digits", () => {
      const bad = [undefined, "", "a1".repeat(31), "A1".repeat(32), "zz".repeat(32), "a1".repeat(33), 5];
      for (const value of bad) {
        expect(dispatched(prompt({ fingerprint: value })).handler, `fingerprint ${String(value)}`).not.toHaveBeenCalled();
        expect(dispatched(prompt({ findingsDigest: value })).handler, `digest ${String(value)}`).not.toHaveBeenCalled();
      }
    });

    it("rejects a trust_prompt that names no agent", () => {
      for (const agent of [undefined, "", 1, null]) {
        expect(dispatched(prompt({ agent })).handler, String(agent)).not.toHaveBeenCalled();
      }
    });

    it("rejects a trust_prompt whose remember is not yes, window or session", () => {
      for (const remember of [undefined, "no", "Yes", "forever", 1]) {
        expect(dispatched(prompt({ remember })).handler, String(remember)).not.toHaveBeenCalled();
      }
    });

    it("rejects a trust_prompt with no tab or no item list", () => {
      expect(dispatched(prompt({ tab: undefined })).handler).not.toHaveBeenCalled();
      expect(dispatched(prompt({ items: undefined })).handler).not.toHaveBeenCalled();
    });

    it("type-checks the three requests the prompt and the command line post", () => {
      const sent: OutboundMessage[] = [
        { type: "trust_answer", request_id: "r1", tab: 3, nonce: 17, fingerprint: HEX, findings_digest: HEX, trust: true },
        { type: "trust_cancel", request_id: "r2", tab: 3, nonce: 17 },
        { type: "trust_command", request_id: "r3", action: "untrust", agent: "codex" },
      ];
      expect(sent.map((m) => m.type)).toEqual(["trust_answer", "trust_cancel", "trust_command"]);
    });
  });

  it("passes every queue, draft, history, rule-offer, editor and scratch envelope through the whitelist", () => {
    const handler = vi.fn();
    installDispatch(handler);
    const envelopes = [
      { kind: "queue", tab: 1, items: [{ text: "later", queuedAt: 5 }], error: null },
      { kind: "draft", tab: 1, text: "half" },
      { kind: "queue_taken", tab: 1, texts: ["a"] },
      { kind: "history", entries: ["old"] },
      { kind: "rule_offers", tab: 1, offers: { "perm-1": "git push *" } },
      { kind: "editor_context", file: "src/a.rs", lines: [1, 2] },
      { kind: "editor_link", state: "failed", text: "could not attach: no such socket" },
      { kind: "scratch", tab: 1, editing: true },
      { kind: "notice", text: "no such file" },
    ];
    for (const envelope of envelopes) window.__eitriDispatch!(JSON.stringify(envelope));
    expect(handler.mock.calls.map((c) => c[0])).toEqual(envelopes);
  });
});

/* The exact wire strings `panel_keys`/`nav_fallthrough`
   serialise to, byte for byte -- a core test pins the same five, so the two sides can
   never silently drift apart on field order or spelling. */
describe("OutboundMessage: panel_keys and nav_fallthrough serialise exactly", () => {
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

/* `pane_nav`'s exact wire strings, all four sides, field order included -- the core
   test `parses_pane_nav_in_all_four_directions_as_a_window_level_message` parses these same four
   strings, so the two sides cannot drift apart on spelling (the words are the sides' names, never the
   letters `h`/`j`/`k`/`l`). */
describe("OutboundMessage: pane_nav serialises exactly", () => {
  it.each<[OutboundMessage, string]>([
    [{ type: "pane_nav", request_id: "req-1", direction: "left" }, '{"type":"pane_nav","request_id":"req-1","direction":"left"}'],
    [{ type: "pane_nav", request_id: "req-2", direction: "down" }, '{"type":"pane_nav","request_id":"req-2","direction":"down"}'],
    [{ type: "pane_nav", request_id: "req-3", direction: "up" }, '{"type":"pane_nav","request_id":"req-3","direction":"up"}'],
    [{ type: "pane_nav", request_id: "req-4", direction: "right" }, '{"type":"pane_nav","request_id":"req-4","direction":"right"}'],
  ])("%o -> %s", (message, wire) => {
    expect(JSON.stringify(message)).toBe(wire);
  });
});

/* `open_url`'s exact wire string, field order included -- the core test
   `parses_open_url_as_a_window_level_message` parses this same string, so the two sides cannot drift apart
   on spelling. The page sends the WHATWG-normalized `href` (`nav.ts#webUrl`), and Rust re-checks it
   (`panel/src/links.rs#web_url`) before it reaches `UriLauncher`. */
describe("OutboundMessage: open_url serialises exactly", () => {
  it("names the type, the request id and the url, in that order", () => {
    const message: OutboundMessage = { type: "open_url", request_id: "req-1", url: "https://example.com/a" };
    expect(JSON.stringify(message)).toBe('{"type":"open_url","request_id":"req-1","url":"https://example.com/a"}');
  });
});

/* `keymap_closed`'s exact wire string, field order included -- the core test
   `keymap_closed_parses_with_both_return_keys_values` parses this same string. */
describe("OutboundMessage: keymap_closed serialises exactly", () => {
  it.each<[OutboundMessage, string]>([
    [
      { type: "keymap_closed", request_id: "req-1", opening: 7, return_keys: true },
      '{"type":"keymap_closed","request_id":"req-1","opening":7,"return_keys":true}',
    ],
    [
      { type: "keymap_closed", request_id: "req-2", opening: 7, return_keys: false },
      '{"type":"keymap_closed","request_id":"req-2","opening":7,"return_keys":false}',
    ],
  ])("%o -> %s", (message, wire) => {
    expect(JSON.stringify(message)).toBe(wire);
  });
});

describe("path completion envelopes", () => {
  it("accepts only numeric tab/sequence and an array consisting entirely of strings", () => {
    const handler = vi.fn();
    const warning = vi.spyOn(console, "warn").mockImplementation(() => {});
    installDispatch(handler);
    const valid = { kind: "path_completions", tab: 1, seq: 2, items: ["sub/", "sub/file"] };
    window.__eitriDispatch!(JSON.stringify(valid));
    expect(handler).toHaveBeenLastCalledWith(valid);
    for (const malformed of [
      { ...valid, items: ["okay", 3] }, { ...valid, items: "file" }, { ...valid, items: null },
      { ...valid, tab: "1" }, { ...valid, seq: "2" }, { ...valid, seq: -1 }, { ...valid, seq: 0.5 },
    ]) window.__eitriDispatch!(JSON.stringify(malformed));
    expect(handler).toHaveBeenCalledTimes(1);
    warning.mockRestore();
  });
});

it("demuxes window pages", () => {
  const handler = vi.fn();
  installDispatch(handler);
  const payload = { kind: "pages", pages: [{ id: "markdown", title: "Markdown", summary: null }] };
  window.__eitriDispatch!(JSON.stringify(payload));
  expect(handler).toHaveBeenCalledWith(payload);
});

describe("path_choices stopped reason", () => {
  it.each([undefined, null, "stopped after 1 s"])("accepts %s", (stopped) => {
    const handler = vi.fn();
    installDispatch(handler);
    const envelope = { kind: "path_choices", requestId: "p1", tab: 1, paths: [{ path: "src/a.rs", line: null }], stopped };
    window.__eitriDispatch!(JSON.stringify(envelope));
    expect(handler).toHaveBeenCalledWith(JSON.parse(JSON.stringify(envelope)));
  });
  it.each([12, false, {}, []])("rejects a malformed stopped reason %s", (stopped) => {
    const handler = vi.fn();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    installDispatch(handler);
    window.__eitriDispatch!(JSON.stringify({ kind: "path_choices", requestId: "p1", tab: 1,
      paths: [{ path: "src/a.rs", line: null }], stopped }));
    warn.mockRestore();
    expect(handler).not.toHaveBeenCalled();
  });
});
