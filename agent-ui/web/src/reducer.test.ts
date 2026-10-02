import { describe, expect, it } from "vitest";
import { applyCallNotes, applyEvent, applySnapshot, keepAfterSidecarStop, initialState, MAX_HELD_DENIALS, resetToStartScreen } from "./reducer";
import type { AgentDomainEvent, AgentUiSnapshot, TurnEndDetail, TurnOutcome, UsageInfo } from "./types";
/** What Rust really sends for the turn-ending cases; written from its serializer. */
import fixture from "./fixtures/turn-endings.json";

describe("applyEvent", () => {
  it("session_opened populates identity and sets status to running", () => {
    const event: AgentDomainEvent = { type: "session_opened", session_id: "abc-123", provider_session_id: "claude-abc-123", model: "claude-sonnet-5", cwd: "/tmp" };
    const state = applyEvent(initialState(), event);
    expect(state.sessionId).toBe("abc-123");
    expect(state.model).toBe("claude-sonnet-5");
    expect(state.status).toEqual({ kind: "running" });
  });

  it("turn_started sets activeTurnId", () => {
    const state = applyEvent(initialState(), { type: "turn_started", turn_id: "t1" });
    expect(state.activeTurnId).toBe("t1");
  });

  it("content_delta accumulates consecutive text into one message, ignoring thinking deltas", () => {
    // This asserted ["first", "second"] until partial streaming landed. `transcript` holds assistant
    // MESSAGES, and two consecutive text deltas with nothing between them are one message -- under
    // partial streaming they are two fragments of one sentence.
    let state = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "text", text: "first" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "pondering" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "second" });
    expect(state.transcript.map((m) => m.text)).toEqual(["firstsecond"]);
  });

  // The phase-3 GUI pass (2026-09-25): `After the table.TURN-1-DONE`. Mirrors the Rust projection.
  it("assistant_message_boundary ends the message, so the next text is its own entry", () => {
    let state = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "text", text: "After the table." });
    state = applyEvent(state, { type: "assistant_message_boundary", turn_id: "t1" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "TURN-1-DONE" });
    expect(state.transcript.map((m) => m.text)).toEqual(["After the table.", "TURN-1-DONE"]);
  });

  it("tool_call_started then tool_call_completed links by tool_use_id", () => {
    let state = applyEvent(initialState(), { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "echo hi" } });
    expect(state.toolCalls).toHaveLength(1);
    expect(state.toolCalls[0].result).toBeNull();
    state = applyEvent(state, { type: "tool_call_completed", turn_id: "t1", tool_use_id: "toolu_1", content: "hi", is_error: false });
    expect(state.toolCalls[0].result).toEqual({ content: "hi", isError: false });
  });

  it("turn_started sets activeTurnId, and a subsequent turn_completed clears it regardless of outcome", () => {
    // Parametrized over all four TurnOutcome values, mirroring Task 1's own Rust test of the same
    // property (turn_completed always clears the active turn, no matter how it ended).
    const outcomes: TurnOutcome[] = ["completed", "interrupted", "failed", "limit_reached"];
    for (const outcome of outcomes) {
      let state = applyEvent(initialState(), { type: "turn_started", turn_id: "t1" });
      expect(state.activeTurnId).toBe("t1");
      state = applyEvent(state, {
        type: "turn_completed", turn_id: "t1", outcome, result_text: "done",
        stop_reason: "end_turn", usage: { total_cost_usd: 0.01, num_turns: 1, tokens: null, model: null }, detail: { reason: null, api_error_status: null, message: null },
      });
      expect(state.activeTurnId).toBeNull();
    }
  });

  it("turn_completed does not change status (v2 semantics: only session_unavailable/session_closed do)", () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, {
      type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: "done",
      stop_reason: "end_turn", usage: { total_cost_usd: 0.01, num_turns: 1, tokens: null, model: null }, detail: { reason: null, api_error_status: null, message: null },
    });
    expect(state.status).toEqual({ kind: "running" });
  });

  /* R5 (v1 picks, Task 13): the tab's last reported usage. Each report REPLACES the figure whole
     (the SDK reports a running total and `/clear` resets it, so a lower later figure is the truth),
     and a turn that reports none -- interrupted, synthesized -- leaves the last figure standing:
     silence is not a measurement, and the band must never show an invented zero. Mirrors
     `AgentSessionProjection::apply`'s `TurnCompleted` arm. */
  describe("usage (R5)", () => {
    const report = (cost: number, tokens: number | null): UsageInfo => ({
      total_cost_usd: cost,
      num_turns: null,
      tokens: tokens === null ? null : { input: tokens, output: 0, cache_creation: 0, cache_read: 0 },
      model: null,
    });
    const done = (usage: UsageInfo | null, turn = "t1", outcome: TurnOutcome = "completed"): AgentDomainEvent => ({
      type: "turn_completed", turn_id: turn, outcome, result_text: "", stop_reason: null, usage, detail: { reason: null, api_error_status: null, message: null },
    });

    it("is unknown until a turn reports one", () => {
      expect(initialState().usage).toBeNull();
      expect(applyEvent(initialState(), { type: "turn_started", turn_id: "t1" }).usage).toBeNull();
      expect(applyEvent(initialState(), done(null)).usage).toBeNull();
    });

    it("a turn_completed with a usage sets state.usage, whole", () => {
      const usage = report(0.42, 1234);
      expect(applyEvent(initialState(), done(usage)).usage).toEqual(usage);
    });

    it("a later turn_completed with usage: null keeps the last figure", () => {
      const usage = report(0.42, 1234);
      let state = applyEvent(initialState(), done(usage));
      state = applyEvent(state, done(null, "t2"));
      expect(state.usage).toEqual(usage);
    });

    it("no outcome erases the figure: an interrupted, failed or limit-reached turn that reports none leaves it", () => {
      const usage = report(0.42, 1234);
      for (const outcome of ["interrupted", "failed", "limit_reached"] as const) {
        const state = applyEvent(applyEvent(initialState(), done(usage)), done(null, "t2", outcome));
        expect(state.usage, outcome).toEqual(usage);
        // ...and one of them that DID report is a report like any other.
        expect(applyEvent(state, done(report(0.5, 9), "t3", outcome)).usage, outcome).toEqual(report(0.5, 9));
      }
    });

    it("a lower later report replaces the figure -- /clear resets the running total, it is not summed or maxed", () => {
      let state = applyEvent(initialState(), done(report(0.5, 900_000)));
      state = applyEvent(state, done(report(0.02, 3000), "t2"));
      expect(state.usage).toEqual(report(0.02, 3000));
    });

    it("a legacy report (a cost and a turn count, no tokens) is kept as reported, tokens still unknown", () => {
      const legacy: UsageInfo = { total_cost_usd: 0.01, num_turns: 3, tokens: null, model: null };
      expect(applyEvent(initialState(), done(legacy)).usage).toEqual(legacy);
    });

    it("applySnapshot takes the snapshot's usage, and null when it carries none", () => {
      const usage = report(0.42, 1234);
      const reported = applyEvent(initialState(), done(usage));
      const { nextSeq: _seq, turnThinking: _think, ...wire } = reported;
      expect(applySnapshot(initialState(), wire, 5).usage).toEqual(usage);
      // Another tab's snapshot that has reported nothing is not this tab's figure: a snapshot
      // replaces the whole state, this field included.
      const { nextSeq: _s2, turnThinking: _t2, ...none } = initialState();
      expect(none.usage).toBeNull();
      expect(applySnapshot(reported, none, 5).usage).toBeNull();
    });

    it("a snapshot that omits the key altogether reads as unknown, never as undefined", () => {
      // Rust always sends the key (`null` or an object); this is the runtime defence for a payload
      // that does not, since `usageSegment` reads `null` and would throw on `undefined`.
      const { nextSeq: _seq, turnThinking: _think, usage: _usage, ...noKey } = initialState();
      expect(applySnapshot(initialState(), noKey as unknown as AgentUiSnapshot, 0).usage).toBeNull();
    });

    it("a start-over is a new session: resetToStartScreen forgets the figure", () => {
      const state = applyEvent(initialState(), done(report(0.42, 1234)));
      expect(resetToStartScreen(state).usage).toBeNull();
    });

    it("a session that ends keeps its figure on screen -- what was spent is still spent", () => {
      const usage = report(0.42, 1234);
      let state = applyEvent(initialState(), done(usage));
      state = applyEvent(state, { type: "session_unavailable", reason: "provider process exited unexpectedly" });
      expect(state.usage).toEqual(usage);
      // And the lost-session view a stopped sidecar leaves behind (v1 polish item 6).
      const said = applyEvent(applyEvent(initialState(), done(usage)), { type: "content_delta", turn_id: "t2", kind: "text", text: "hi" });
      expect(keepAfterSidecarStop(said, "the sidecar stopped")?.usage).toEqual(usage);
    });
  });

  it("session_unavailable sets status to unavailable with a reason", () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "session_unavailable", reason: "provider process exited unexpectedly" });
    expect(state.status).toEqual({ kind: "unavailable", reason: "provider process exited unexpectedly" });
  });

  it("session_closed sets status to closed with a reason", () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "session_closed", reason: "window closed" });
    expect(state.status).toEqual({ kind: "closed", reason: "window closed" });
  });

  /* The forbidden failure mode, at the reducer level: a session that dies mid-turn used to leave
     activeTurnId set forever, so App.tsx kept turnInProgress true, the composer stayed disabled
     with a spinner, and the truncated reply above it looked like a reply that had simply finished
     being short. */
  it("session_unavailable mid-turn clears activeTurnId, because no turn_completed is ever coming", () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "half an ans" });
    expect(state.activeTurnId).toBe("t1");

    state = applyEvent(state, { type: "session_unavailable", reason: "3 event(s) were never delivered" });

    expect(state.activeTurnId).toBeNull();
    expect(state.status).toEqual({ kind: "unavailable", reason: "3 event(s) were never delivered" });
    // And the half-written text is still there: clearing the turn must not also erase what did
    // arrive. The banner says it may be incomplete; the reducer does not quietly delete it.
    expect(state.transcript[state.transcript.length - 1].text).toBe("half an ans");
  });

  it("session_closed mid-turn clears activeTurnId too", () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "session_closed", reason: "closed_by_host" });
    expect(state.activeTurnId).toBeNull();
  });

  // Wave 5: the mode lives on the tab (`tabs` envelope), not this per-session projection, so the
  // reducer folds nothing here -- unlike a genuinely unrecognized shape, this is a KNOWN no-op, so
  // it must not reach the exhaustiveness guard's console.warn either.
  it("permission_mode_changed returns the state unchanged and does not warn", () => {
    const consoleWarn = console.warn;
    let warned = false;
    console.warn = () => { warned = true; };
    const before = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    const after = applyEvent(before, { type: "permission_mode_changed", mode: "bypass", provider_mode: "BYPASS_PERMISSIONS", floor_applied: false });
    expect(after).toBe(before);
    expect(warned).toBe(false);
    console.warn = consoleWarn;
  });

  it("permission_requested pushes onto pendingPermissions without clearing prior entries", () => {
    let state = applyEvent(initialState(), { type: "permission_requested", permission_id: "r1", tool_use_id: null, tool_name: "Bash", input: {} });
    state = applyEvent(state, { type: "permission_requested", permission_id: "r2", tool_use_id: null, tool_name: "Read", input: {} });
    expect(state.pendingPermissions.map((p) => p.permissionId)).toEqual(["r1", "r2"]);
  });

  /* The link back to the tool call a permission gates. Without it a card can only ever say "Bash
     wants to run", never WHICH Bash call -- and with several tool calls in one turn those are not
     the same question. `null` is what a request whose source supplied no such id looks like
     (today: the legacy backend's secondary `can_use_tool` path), never a fabricated id. */
  it("permission_requested carries the tool_use_id of the call it gates", () => {
    const state = applyEvent(initialState(), {
      type: "permission_requested",
      permission_id: "r1",
      tool_use_id: "toolu_01ABC",
      tool_name: "Bash",
      input: {},
    });
    expect(state.pendingPermissions[0].toolUseId).toBe("toolu_01ABC");
  });

  /* O3: the CLI's own prompt for a call arrives as a SECOND request carrying the same tool_use_id
     as the gate's (already answered) one, under a new permission id, with the CLI's own words in
     `provider_prompt` (Rust's snake_case event). The reducer keeps it as a card of its own and
     carries the words in the snapshot's camelCase shape, so an events-built card and a
     snapshot-built one read the same. A gate request carries no `providerPrompt` at all. */
  it("keeps the CLI's own prompt for an answered call as its own card, with its words", () => {
    let state = applyEvent(initialState(), {
      type: "permission_requested",
      permission_id: "perm-gate",
      tool_use_id: "toolu_probe",
      tool_name: "Write",
      input: {},
    });
    expect(state.pendingPermissions[0]).not.toHaveProperty("providerPrompt");
    state = applyEvent(state, { type: "permission_resolved", permission_id: "perm-gate", outcome: "allowed" });
    state = applyEvent(state, {
      type: "permission_requested",
      permission_id: "perm-cli",
      tool_use_id: "toolu_probe",
      tool_name: "Write",
      input: {},
      provider_prompt: {
        reason: "Claude requested permissions to edit /p/.git/probe which is a sensitive file.",
        description: ".git/probe",
        blocked_path: null,
        matched_ask_rule: { source: "projectSettings", tool_name: "Write", rule_content: null },
      },
    });
    expect(state.pendingPermissions).toHaveLength(1);
    expect(state.pendingPermissions[0].permissionId).toBe("perm-cli");
    expect(state.pendingPermissions[0].providerPrompt).toEqual({
      reason: "Claude requested permissions to edit /p/.git/probe which is a sensitive file.",
      description: ".git/probe",
      blockedPath: null,
      matchedAskRule: { source: "projectSettings", toolName: "Write", ruleContent: null },
      unrecognizedOrigin: null,
    });
    // O3 review #3: an origin this build does not know is carried, so the card calls it neutrally.
    state = applyEvent(state, {
      type: "permission_requested",
      permission_id: "perm-new",
      tool_use_id: "toolu_new",
      tool_name: "Write",
      input: {},
      provider_prompt: { reason: null, description: null, blocked_path: null, matched_ask_rule: null, unrecognized_origin: 7 },
    });
    expect(state.pendingPermissions[1].providerPrompt?.unrecognizedOrigin).toBe(7);
  });

  /* The legacy backend's own shape, new on 2026-09-15: its `PreToolUse` hook payload's
     `tool_use_id` is BOTH the identity of the gated call and the id the request is answered by, so
     the two arrive as one string. Nothing in the reducer may assume they differ -- the card is
     keyed on one and linked by the other, and if either were derived from the other this pair
     would be where it broke. */
  it("keeps both ids when a hook-relay request sends the same string as permission_id and tool_use_id", () => {
    const state = applyEvent(initialState(), {
      type: "permission_requested",
      permission_id: "toolu_01CtdezhmhUCrBaswxW5HYmC",
      tool_use_id: "toolu_01CtdezhmhUCrBaswxW5HYmC",
      tool_name: "Bash",
      input: { command: "echo hello" },
    });
    expect(state.pendingPermissions).toHaveLength(1);
    expect(state.pendingPermissions[0].permissionId).toBe("toolu_01CtdezhmhUCrBaswxW5HYmC");
    expect(state.pendingPermissions[0].toolUseId).toBe("toolu_01CtdezhmhUCrBaswxW5HYmC");
  });

  /* And it still resolves by permission_id afterwards: the id being shared with the tool call must
     not make the card outlive its own answer. */
  it("resolves a hook-relay request whose permission_id is also its tool_use_id", () => {
    let state = applyEvent(initialState(), {
      type: "permission_requested",
      permission_id: "toolu_01CtdezhmhUCrBaswxW5HYmC",
      tool_use_id: "toolu_01CtdezhmhUCrBaswxW5HYmC",
      tool_name: "Bash",
      input: {},
    });
    state = applyEvent(state, { type: "permission_resolved", permission_id: "toolu_01CtdezhmhUCrBaswxW5HYmC", outcome: "allowed" });
    expect(state.pendingPermissions).toEqual([]);
  });

  it("permission_requested keeps a backend's honest null rather than inventing an id", () => {
    const state = applyEvent(initialState(), {
      type: "permission_requested",
      permission_id: "r1",
      tool_use_id: null,
      tool_name: "Bash",
      input: {},
    });
    expect(state.pendingPermissions[0].toolUseId).toBeNull();
  });

  it("permission_resolved removes only the matching permission, leaving others pending", () => {
    // The direct replacement for the deleted markPermissionAnswered test -- this is now a real
    // event pushed from Rust the instant AgentSession::respond_permission (or interrupt())
    // resolves the request, not a frontend-local optimistic guess.
    let state = applyEvent(initialState(), { type: "permission_requested", permission_id: "r1", tool_use_id: null, tool_name: "Bash", input: {} });
    state = applyEvent(state, { type: "permission_requested", permission_id: "r2", tool_use_id: null, tool_name: "Read", input: {} });
    state = applyEvent(state, { type: "permission_resolved", permission_id: "r1", outcome: "allowed" });
    expect(state.pendingPermissions.map((p) => p.permissionId)).toEqual(["r2"]);
  });

  it("permission_resolved removes the matching permission regardless of outcome", () => {
    const outcomes: Array<"allowed" | "denied" | "cancelled_by_interrupt" | "cancelled_by_session_close"> = [
      "allowed", "denied", "cancelled_by_interrupt", "cancelled_by_session_close",
    ];
    for (const outcome of outcomes) {
      let state = applyEvent(initialState(), { type: "permission_requested", permission_id: "r1", tool_use_id: null, tool_name: "Bash", input: {} });
      state = applyEvent(state, { type: "permission_resolved", permission_id: "r1", outcome });
      expect(state.pendingPermissions).toEqual([]);
    }
  });

  it("folds a user prompt as its own item and closes the open assistant message", () => {
    let state = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "text", text: "first" });
    state = applyEvent(state, { type: "user_prompt_submitted", text: "and now this" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t2", kind: "text", text: "second" });

    expect(state.userPrompts.map((p) => p.text)).toEqual(["and now this"]);
    // Must match agent/src/projection.rs exactly: a snapshot from Rust and this reducer's own
    // accumulation have to be indistinguishable.
    expect(state.transcript.map((m) => m.text)).toEqual(["first", "second"]);
  });

  it("permission_resolved on an unknown permission id is a harmless no-op", () => {
    let state = applyEvent(initialState(), { type: "permission_requested", permission_id: "r1", tool_use_id: null, tool_name: "Bash", input: {} });
    state = applyEvent(state, { type: "permission_resolved", permission_id: "does-not-exist", outcome: "allowed" });
    expect(state.pendingPermissions.map((p) => p.permissionId)).toEqual(["r1"]);
  });

  it("unrecognized event shapes do not throw and are observable, not silently dropped", () => {
    const consoleWarn = console.warn;
    let warned = false;
    console.warn = () => { warned = true; };
    // @ts-expect-error -- deliberately testing a shape TypeScript wouldn't normally allow through
    const state = applyEvent(initialState(), { type: "some_future_event_type" });
    expect(state).toBeTruthy();
    expect(warned).toBe(true);
    console.warn = consoleWarn;
  });

  /* A restored history (design §4, §5.5) reaches this side only inside a snapshot -- there is no
   * event for it, because it is folded in Rust before the ingestion thread that produces events
   * even starts. So a resync or a panel reload is the ONLY path it travels, and it has to survive
   * one intact: a notice lost on a resync would leave a panel showing last session's messages with
   * nothing saying they are old. */
  it("a resync keeps the restored-history notice and the items it describes", () => {
    const restored = {
      ...initialState(),
      userPrompts: [{ seq: 0, text: "what did we say?" }],
      transcript: [{ seq: 1, text: "this much" }],
      history: {
        source: "claude_transcript" as const,
        restoredItems: 2,
        omittedItems: 3,
        uptoSeq: 2,
        sourcePath: "/claude/projects/p/sess.jsonl",
        attemptedTranscriptPath: null,
        fallbackReason: null,
        writerVersion: "2.1.272",
      },
    };
    // `throughRevision` is the seed for locally folded items, and it is what keeps a live item
    // sorting after every restored one across a resync.
    const state = applySnapshot(initialState(), restored, 2);
    expect(state.history).toEqual(restored.history);
    expect(state.nextSeq).toBe(2);
    // Closed, exactly as it is closed in Rust after a load: otherwise the first live delta appends
    // to the last restored assistant message and the two render as one bubble.
    expect(state.assistantMessageOpen).toBe(false);
    const live = applyEvent(state, { type: "content_delta", turn_id: "t-live", kind: "text", text: "a live answer" });
    expect(live.transcript.map((m) => m.text)).toEqual(["this much", "a live answer"]);
    expect(live.transcript[1].seq).toBeGreaterThanOrEqual(restored.history.uptoSeq);
    expect(live.history).toEqual(restored.history);
  });

  it("a fresh session carries no history notice", () => {
    expect(initialState().history).toBeNull();
    // `r` on an ended session starts over, and starting over restores nothing.
    expect(resetToStartScreen({ ...initialState(), history: { source: "eitri_copy", restoredItems: 1, omittedItems: 0, uptoSeq: 1, sourcePath: "/x", attemptedTranscriptPath: null, fallbackReason: "transcript file not found", writerVersion: null } }).history).toBeNull();
  });

  it("applySnapshot replaces the whole state wholesale", () => {
    const snapshot = { ...initialState(), sessionId: "replaced", transcript: [{ seq: 0, text: "from snapshot" }] };
    const state = applySnapshot(initialState(), snapshot, 1);
    // `nextSeq` is the one field a snapshot does not carry -- it comes from the envelope beside it.
    expect(state).toEqual({ ...snapshot, nextSeq: 1 });
  });
});

describe("identity handling", () => {
  it("keeps the Verdandi session id and the Claude provider session id in separate fields", () => {
    // The two are genuinely different values on the sidecar backend. Measured against a real
    // sidecar: CreateSessionResponse returned one UUID while SessionReady carried another. Storing
    // both in one field would eventually send Claude's id where Verdandi's belongs.
    const state = applyEvent(initialState(), {
      type: "session_opened",
      session_id: "8b3be45a-330b-429f-b1a9-573e73448e23",
      provider_session_id: "f6fabf0a-8be9-4916-879c-ceadc863620e",
      model: "claude-sonnet-5",
      cwd: "/tmp",
    });
    expect(state.sessionId).toBe("8b3be45a-330b-429f-b1a9-573e73448e23");
    expect(state.providerSessionId).toBe("f6fabf0a-8be9-4916-879c-ceadc863620e");
    expect(state.sessionId).not.toBe(state.providerSessionId);
  });

  it("a second session_opened on the same session is idempotent, not a new session", () => {
    // The Agent SDK emits system/init at the start of EVERY turn inside one streaming session, so
    // this event legitimately repeats. It must not reset accumulated state.
    const opened: AgentDomainEvent = {
      type: "session_opened",
      session_id: "sess-1",
      provider_session_id: "claude-1",
      model: "claude-sonnet-5",
      cwd: "/tmp",
    };
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "hello" });
    state = applyEvent(state, opened);
    expect(state.transcript.map((m) => m.text)).toEqual(["hello"]);
    expect(state.sessionId).toBe("sess-1");
    expect(state.providerSessionId).toBe("claude-1");
  });

  it("starts with every capability false, so a gated control cannot appear before the server says so", () => {
    const state = initialState();
    expect(state.capabilities).toEqual({ resume: false, fork: false, interrupt: false, bypassPermissionMode: false });
    expect(state.provider).toBeNull();
    expect(state.conversationId).toBeNull();
    expect(state.providerSessionId).toBeNull();
  });
});

describe("partial assistant streaming", () => {
  const opened: AgentDomainEvent = {
    type: "session_opened", session_id: "s", provider_session_id: "p", model: "m", cwd: "/tmp",
  };
  const delta = (text: string): AgentDomainEvent => ({ type: "content_delta", turn_id: "t1", kind: "text", text });

  it("accumulates a streamed reply into ONE transcript entry, not one per delta", () => {
    // The defect this pins: under partial streaming a 600-word reply arrives as 400+ deltas. One
    // transcript entry each renders 400 separate bubbles, every fragment markdown-parsed alone --
    // so "`eitri_" or "**bold" is not valid standalone markdown and the formatting breaks.
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    for (const chunk of ["The ", "quick ", "**brown** ", "fox"]) state = applyEvent(state, delta(chunk));
    expect(state.transcript.map((m) => m.text)).toEqual(["The quick **brown** fox"]);
  });

  it("starts a new entry after a tool call, because that ends the assistant message", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, delta("I'll check."));
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    state = applyEvent(state, delta("It printed hi."));
    expect(state.transcript.map((m) => m.text)).toEqual(["I'll check.", "It printed hi."]);
  });

  it("starts a new entry on the next turn", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, delta("first"));
    state = applyEvent(state, { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: "first", stop_reason: null, usage: null, detail: { reason: null, api_error_status: null, message: null } });
    state = applyEvent(state, { type: "turn_started", turn_id: "t2" });
    state = applyEvent(state, delta("second"));
    expect(state.transcript.map((m) => m.text)).toEqual(["first", "second"]);
  });

  it("a thinking delta does not split the text around it", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, delta("before "));
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    state = applyEvent(state, delta("after"));
    expect(state.transcript.map((m) => m.text)).toEqual(["before after"]);
  });

  it("a snapshot with a closed message starts a new entry, never appending into it", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, delta("streaming"));
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    expect(state.assistantMessageOpen).toBe(false);
    // Built as the WIRE now genuinely carries it (sw-panel-render-2's fix): `assistantMessageOpen`
    // is on `AgentUiSnapshot`, and here it is truthfully `false` -- the tool call closed the
    // message before this snapshot was ever taken, on both sides.
    const { nextSeq: _seq, turnThinking: _think, ...wire } = { ...state, transcript: [{ seq: 0, text: "an earlier reply" }] };
    const restored = applySnapshot(state, wire, 1);
    expect(restored.assistantMessageOpen).toBe(false);
    const next = applyEvent(restored, delta("new message"));
    expect(next.transcript.map((m) => m.text)).toEqual(["an earlier reply", "new message"]);
  });

  it("sw-panel-render-2: a mid-stream snapshot keeps the message open, so the next delta continues it rather than splitting it", () => {
    // The defect this pins: `applySnapshot` used to force `assistantMessageOpen: false`
    // unconditionally, on the theory a snapshot always meant "start fresh" -- but Rust's own
    // projection keeps appending to the open message underneath a snapshot taken mid-reply
    // (a tab switch back to a streaming reply, `prefix r`, a bounded-queue Resync), so forcing
    // `false` here split one streaming reply into two transcript rows with broken markdown at the
    // seam. Probe lifted from the verdict: fold `session_opened, turn_started, text 'Use **strong'`,
    // apply a snapshot mid-message, then fold `text ' emphasis** here.'`.
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, delta("Use **strong"));
    expect(state.assistantMessageOpen).toBe(true);
    // The wire as Rust now sends it mid-stream: `assistantMessageOpen` present and `true`, matching
    // the live projection this state was folded from.
    const { nextSeq: _seq, turnThinking: _think, ...wire } = state;
    const restored = applySnapshot(initialState(), wire, state.nextSeq);
    expect(restored.assistantMessageOpen).toBe(true);
    const next = applyEvent(restored, delta(" emphasis** here."));
    expect(next.transcript.map((m) => m.text)).toEqual(["Use **strong emphasis** here."]);
  });
});

/* `turnThinking` (2026-09-20-in-flight-motion-design.md §8.2): the one ephemeral bit `turnPhase.ts`
   reads that is not already sitting in the four collections. Invariant under test throughout: it
   can only ever be LOST, never invented -- see `turnPhase.test.ts`'s "resync degradation" test for
   the same invariant one layer up, on the phase it feeds. */
describe("turnThinking", () => {
  it("is set by a content_delta with kind thinking", () => {
    const state = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    expect(state.turnThinking).toBe(true);
  });

  it("a REPEATED thinking delta returns the exact same object -- the §5.4 re-render guard", () => {
    const first = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    const second = applyEvent(first, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "still going" });
    // `toBe`, not `toEqual`: a second render is exactly the cost this optimization exists to avoid
    // (design §5.4's "one re-render per transition, not per delta"), and `toEqual` would pass even
    // if this returned a fresh, merely value-equal object every time.
    expect(second).toBe(first);
  });

  it("a single (non-repeated) thinking delta still advances nextSeq like any other event", () => {
    // Only a REPEAT is free. The first thinking delta from a fresh state creates no item but is
    // still a real occurrence -- this is `reducer.test.ts`'s existing "seq assignment" invariant,
    // restated here so the two guards cannot silently disagree about the boundary between them.
    const before = initialState().nextSeq;
    const state = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    expect(state.nextSeq).toBe(before + 1);
  });

  it("a content_delta of an UNKNOWN kind is a no-op, not a transcript entry with no text", () => {
    // Whole-branch review, 2026-09-20. This arm used to open with `if (event.kind !== "text")
    // return state;` and the in-flight motion change replaced it with `if (event.kind ===
    // "thinking")`, so every other kind fell into the TEXT path. Reproduced there: folding a
    // `redacted_thinking` delta appended `{seq: 1, text: undefined}`, which `MessageList` hands to
    // `renderMarkdown`, which throws inside `marked` -- and this app has no error boundary, so the
    // panel unmounts whole.
    //
    // Unreachable against today's Rust (`ContentKind` is `{Text, Thinking}` and `projection.rs`
    // matches per variant), which is exactly the point: this is the same defence-against-drift the
    // `default:` arm at the bottom of that switch exists for, and the cast is needed because TS has
    // already narrowed the union by this line.
    const drifted = { type: "content_delta", turn_id: "t1", kind: "redacted_thinking" } as unknown as AgentDomainEvent;
    const state = applyEvent(initialState(), drifted);
    expect(state.transcript).toEqual([]);
    expect(state.assistantMessageOpen).toBe(false);
    // It is still a real, ordered occurrence, so it consumes a revision exactly like any other
    // event that creates nothing -- only a REPEATED thinking delta is free.
    expect(state.nextSeq).toBe(initialState().nextSeq + 1);
  });

  it("is cleared by every OTHER event, by the absence of a special case rather than a maintained list", () => {
    const opened: AgentDomainEvent = { type: "session_opened", session_id: "s", provider_session_id: "p", model: "m", cwd: "/tmp" };
    const events: AgentDomainEvent[] = [
      opened,
      { type: "turn_started", turn_id: "t1" },
      { type: "user_prompt_submitted", text: "go" },
      { type: "content_delta", turn_id: "t1", kind: "text", text: "ok" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} },
      { type: "tool_call_completed", turn_id: "t1", tool_use_id: "tu1", content: "hi", is_error: false },
      { type: "permission_requested", permission_id: "p1", tool_use_id: "tu1", tool_name: "Bash", input: {} },
      { type: "permission_resolved", permission_id: "p1", outcome: "allowed" },
      { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: "done", stop_reason: null, usage: null, detail: { reason: null, api_error_status: null, message: null } },
      { type: "session_unavailable", reason: "provider crashed" },
      { type: "session_closed", reason: "provider exited" },
    ];
    for (const event of events) {
      const thinking = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
      expect(thinking.turnThinking).toBe(true);
      const cleared = applyEvent(thinking, event);
      expect(cleared.turnThinking).toBe(false);
    }
  });

  it("applySnapshot clears it, even if the wire happened to carry a stale value", () => {
    const thinking = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    // The real wire never carries this key at all (`AgentUiSnapshot` omits it) -- spreading it in
    // anyway proves `applySnapshot` forces the reset itself rather than merely passing through
    // whatever a snapshot happens not to override.
    const wireWithStaleFlag = { ...thinking, turnThinking: true };
    const restored = applySnapshot(thinking, wireWithStaleFlag, thinking.nextSeq);
    expect(restored.turnThinking).toBe(false);
  });
});

describe("resume outcome", () => {
  function outcome(over: Partial<Extract<AgentDomainEvent, { type: "resume_outcome" }>> = {}) {
    return {
      type: "resume_outcome" as const,
      requested_provider_session_id: "claude-abc",
      status: "attached" as const,
      attached_provider_session_id: "claude-abc",
      forked: false,
      detail: null,
      ...over,
    };
  }

  it("a confirmed resume changes nothing", () => {
    const before = applyEvent(initialState(), { type: "session_opened", session_id: "s", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    const after = applyEvent(before, outcome());
    expect(after.status).toEqual({ kind: "running" });
  });

  /* The substitution the resume protocol exists to prevent. A conversation that looks continued
     while carrying none of its history must not be presented as a working one. */
  it("attaching to a different session ends the conversation and says which is which", () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "s", provider_session_id: "other", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, outcome({ attached_provider_session_id: "some-other-session" }));

    expect(state.status.kind).toBe("unavailable");
    const reason = state.status.kind === "unavailable" ? state.status.reason : "";
    expect(reason).toContain("claude-abc");
    expect(reason).toContain("some-other-session");
    expect(state.activeTurnId).toBeNull();
  });

  /* Forking legitimately returns a different id. Without reading `forked`, every successful fork
     would report as a substitution the day fork is enabled. */
  it("a fork attaching under a new id is not a substitution", () => {
    const state = applyEvent(initialState(), outcome({ attached_provider_session_id: "forked-copy", forked: true }));
    expect(state.status.kind).not.toBe("unavailable");
  });

  it("a refused resume names the session that is gone and what to do instead", () => {
    const state = applyEvent(initialState(), outcome({ status: "rejected", attached_provider_session_id: null }));
    expect(state.status.kind).toBe("unavailable");
    const reason = state.status.kind === "unavailable" ? state.status.reason : "";
    expect(reason).toContain("claude-abc");
    expect(reason).toContain("Start a new session");
  });

  it("a provider that failed to start is not reported as a missing conversation", () => {
    const state = applyEvent(initialState(), outcome({ status: "initialization_failed", attached_provider_session_id: null, detail: "spawn ENOENT" }));
    const reason = state.status.kind === "unavailable" ? state.status.reason : "";
    expect(reason).toContain("provider problem");
    expect(reason).toContain("spawn ENOENT");
    expect(reason).not.toContain("does not have session");
  });
});

/* Interleaved ordering, the reducer's half (2026-09-15).

   Rust's projection is where the order originates -- these tests pin that the live path continues
   the same numbering rather than running a scheme of its own, because the two have to describe the
   same conversation. A frontend-only ordering would look right until the first reload. */
describe("seq assignment", () => {
  const opened: AgentDomainEvent = {
    type: "session_opened", session_id: "s", provider_session_id: "p", model: "m", cwd: "/tmp",
  };

  it("numbers messages, tool calls and permissions on one counter, so they can be interleaved", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "I'll check." });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "Now this." });
    state = applyEvent(state, { type: "permission_requested", permission_id: "perm-1", tool_use_id: "tu1", tool_name: "Bash", input: {} });

    const merged = [
      ...state.transcript.map((m) => [m.seq, `text:${m.text}`] as const),
      ...state.toolCalls.map((c) => [c.seq, `tool:${c.toolUseId}`] as const),
      ...state.pendingPermissions.map((p) => [p.seq, `perm:${p.permissionId}`] as const),
    ].sort((a, b) => a[0] - b[0]);
    expect(merged.map(([, label]) => label)).toEqual(["text:I'll check.", "tool:tu1", "text:Now this.", "perm:perm-1"]);
  });

  it("advances the counter for an event that creates nothing, exactly as the Rust projection does", () => {
    // A thinking delta has no visible effect and still consumes a revision on the Rust side. If the
    // two counters diverged here, a locally-folded item would stop matching the number Rust would
    // have given it -- harmless for sorting, but the claim in `types.ts` would stop being true.
    let state = applyEvent(initialState(), opened);
    const before = state.nextSeq;
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    expect(state.nextSeq).toBe(before + 1);
    expect(state.transcript).toEqual([]);
  });

  it("keeps a streamed message at the seq of its first delta", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "The " });
    const first = state.transcript[0].seq;
    for (const chunk of ["quick ", "brown ", "fox"]) {
      state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: chunk });
    }
    expect(state.transcript).toEqual([{ seq: first, text: "The quick brown fox" }]);
  });

  /* The bug that would appear hours later. A `UiDelivery::Resync` or a panel reload throws this
     whole state away and rebuilds it from a snapshot; anything folded afterwards has to sort AFTER
     everything the snapshot carried. Rust guarantees `throughRevision` exceeds every `seq` in the
     snapshot, which is why it is the seed. */
  it("continues a snapshot's numbering, so items folded after one sort after everything in it", () => {
    const snapshot = {
      ...initialState(),
      transcript: [{ seq: 40, text: "from the snapshot" }],
      toolCalls: [{ seq: 41, toolUseId: "tu_old", name: "Bash", input: {}, result: null }],
    };
    let state = applySnapshot(initialState(), snapshot, 42);
    state = applyEvent(state, { type: "content_delta", turn_id: "t9", kind: "text", text: "after the snapshot" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t9", tool_use_id: "tu_new", name: "Read", input: {} });

    const highestInSnapshot = 41;
    expect(state.transcript[1].seq).toBeGreaterThan(highestInSnapshot);
    expect(state.toolCalls[1].seq).toBeGreaterThan(state.transcript[1].seq);
    // And no collision with anything the snapshot already held.
    const seqs = [...state.transcript.map((m) => m.seq), ...state.toolCalls.map((c) => c.seq)];
    expect(new Set(seqs).size).toBe(seqs.length);
  });
});

describe("resetToStartScreen", () => {
  /* `r` on an ended session must clear everything that belongs to the session that just ended, but
     keep what came from `hello` and describes the bridge rather than the session -- there is no
     fresh `hello` coming, so this is the only copy of it there is. */
  it("keeps backend/capabilities/provider but clears the rest of the session", () => {
    const providerInfo = {
      sidecarVersion: "1.2.3", claudeAgentSdkVersion: "0.1.0", claudeCodeVersion: "2.1.272",
      protocol: "v1", buildDescription: null, startupDiagnostics: [],
    };
    let state = applyEvent(initialState(), {
      type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp",
    });
    state = {
      ...state,
      backend: "sidecar",
      capabilities: { resume: true, fork: false, interrupt: true, bypassPermissionMode: false },
      provider: providerInfo,
    };
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "hello there" });
    state = applyEvent(state, { type: "session_closed", reason: "provider exited" });

    const reset = resetToStartScreen(state);

    expect(reset.backend).toBe("sidecar");
    expect(reset.capabilities).toEqual({ resume: true, fork: false, interrupt: true, bypassPermissionMode: false });
    expect(reset.provider).toEqual(providerInfo);
    expect(reset.transcript).toEqual([]);
    expect(reset.status).toEqual({ kind: "starting" });
    expect(reset.sessionId).toBeNull();
  });
});

describe("the CLI's own auto mode", () => {
  const started: AgentDomainEvent = {
    type: "tool_call_started",
    turn_id: "t",
    tool_use_id: "toolu_push",
    name: "Bash",
    input: { command: "git push --force origin main" },
  };
  const other: AgentDomainEvent = { ...started, tool_use_id: "toolu_ok" } as AgentDomainEvent;
  const denied: AgentDomainEvent = {
    type: "permission_denied",
    tool_use_id: "toolu_push",
    tool_name: "Bash",
    reason_type: "classifier",
    reason: "[Git Destructive]",
  };

  it("puts the CLI's refusal on the call it refused, and on no other", () => {
    let state = applyEvent(applyEvent(initialState(), other), started);
    state = applyEvent(state, denied);
    expect(state.toolCalls.map((c) => c.denied)).toEqual([undefined, { reasonType: "classifier", reason: "[Git Destructive]" }]);
    expect(state.nextSeq).toBe(3);
  });

  it("changes nothing for a refusal naming no call it holds, nor for the CLI's mode report, but keeps step", () => {
    const state = applyEvent(initialState(), started);
    for (const event of [
      { ...denied, tool_use_id: null },
      { ...denied, tool_use_id: "toolu_elsewhere" },
      { type: "cli_permission_mode", reported: "auto" },
    ] as AgentDomainEvent[]) {
      const next = applyEvent(state, event);
      expect(next.toolCalls).toEqual(state.toolCalls);
      expect(next.nextSeq).toBe(state.nextSeq + 1);
    }
  });
});

describe("a CLI refusal that arrives ahead of its call", () => {
  const started = (id: string): AgentDomainEvent => ({
    type: "tool_call_started",
    turn_id: "t",
    tool_use_id: id,
    name: "Bash",
    input: { command: "git push --force origin main" },
  });
  const denied = (id: string): AgentDomainEvent => ({
    type: "permission_denied",
    tool_use_id: id,
    tool_name: "Bash",
    reason_type: "classifier",
    reason: "[Git Destructive]",
  });
  const note = { reasonType: "classifier", reason: "[Git Destructive]" };

  it("is attached when its call starts, and held no longer", () => {
    let state = applyEvent(initialState(), denied("toolu_late"));
    expect(state.toolCalls).toEqual([]);
    state = applyEvent(state, started("toolu_late"));
    expect(state.toolCalls[0].denied).toEqual(note);
    expect(state.denialsBeforeTheirCall).toEqual([]);
    state = applyEvent(state, started("toolu_other"));
    expect(state.toolCalls[1].denied).toBeUndefined();
  });

  it("still lands at once on a call the state already holds", () => {
    let state = applyEvent(initialState(), started("toolu_a"));
    state = applyEvent(state, denied("toolu_a"));
    expect(state.toolCalls[0].denied).toEqual(note);
    expect(state.denialsBeforeTheirCall).toEqual([]);
  });

  it("is dropped when the turn or the session ends, so a later call of that id is not marked", () => {
    const ends: AgentDomainEvent[] = [
      { type: "turn_completed", turn_id: "t", outcome: "completed", result_text: "", stop_reason: null, usage: null, detail: { reason: null, api_error_status: null, message: null } } as AgentDomainEvent,
      { type: "session_closed", reason: "bye" },
      { type: "session_unavailable", reason: "gone" },
    ];
    for (const end of ends) {
      let state = applyEvent(initialState(), denied("toolu_never"));
      expect(state.denialsBeforeTheirCall).toHaveLength(1);
      state = applyEvent(state, end);
      expect(state.denialsBeforeTheirCall).toEqual([]);
      state = applyEvent(state, started("toolu_never"));
      expect(state.toolCalls[0].denied).toBeUndefined();
    }
  });

  it("is capped, the oldest dropped first, and a repeat for one id adds nothing", () => {
    let state = initialState();
    for (let n = 0; n < MAX_HELD_DENIALS + 6; n++) state = applyEvent(state, denied(`toolu_${n}`));
    expect(state.denialsBeforeTheirCall).toHaveLength(MAX_HELD_DENIALS);
    state = applyEvent(state, denied(`toolu_${MAX_HELD_DENIALS + 5}`));
    expect(state.denialsBeforeTheirCall).toHaveLength(MAX_HELD_DENIALS);
    state = applyEvent(applyEvent(applyEvent(state, started("toolu_0")), started("toolu_5")), started("toolu_6"));
    expect(state.toolCalls.map((c) => c.denied)).toEqual([undefined, undefined, note]);
  });
});

describe("applyCallNotes (v1 polish F18, F22)", () => {
  it("marks the named calls, leaves the rest and ignores a note for no call", () => {
    let state = initialState();
    for (const id of ["toolu_1", "toolu_2"]) {
      state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: id, name: "Bash", input: { command: "x" } });
    }
    const other = state.toolCalls[1];
    const next = applyCallNotes(state, {
      ruleNotes: [
        { toolUseId: "toolu_1", rule: "Bash(npm *)" },
        { toolUseId: "toolu_gone", rule: "Bash(ls *)" },
      ],
    });
    expect(next.toolCalls.map((c) => c.allowedByRule)).toEqual(["Bash(npm *)", undefined]);
    expect(next.toolCalls[1]).toBe(other);
    expect(applyCallNotes(state, {})).toBe(state);
    expect(applyCallNotes(state, { ruleNotes: [], createsFile: [] })).toBe(state);
  });

  it("puts the note for a call answered without a card on that call (O3 review item 7)", () => {
    let state = initialState();
    for (const id of ["toolu_1", "toolu_2"]) {
      state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: id, name: "Write", input: {} });
    }
    const next = applyCallNotes(state, {
      promptNotes: [
        { toolUseId: "toolu_2", note: "Claude Code safety check — allowed with your approval" },
        { toolUseId: "toolu_gone", note: "x" },
      ],
    });
    expect(next.toolCalls.map((c) => c.promptNote)).toEqual([undefined, "Claude Code safety check — allowed with your approval"]);
    expect(next.toolCalls[0]).toBe(state.toolCalls[0]);
    expect(applyCallNotes(state, { promptNotes: [] })).toBe(state);
  });

  /** v1 trial item 7: the acceptEdits fast path's own note, applied the same way as the rule note
   *  above -- named calls only, ignoring a note for no call, a no-op when nothing arrived. */
  it("marks the calls the acceptEdits fast path allowed, leaves the rest", () => {
    let state = initialState();
    for (const id of ["toolu_1", "toolu_2"]) {
      state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: id, name: "Edit", input: {} });
    }
    const other = state.toolCalls[1];
    const next = applyCallNotes(state, { autoNotes: ["toolu_1", "toolu_gone"] });
    expect(next.toolCalls.map((c) => c.allowedByAuto)).toEqual([true, undefined]);
    expect(next.toolCalls[1]).toBe(other);
    expect(applyCallNotes(state, {})).toBe(state);
    expect(applyCallNotes(state, { autoNotes: [] })).toBe(state);
  });

  it("marks a Write card raised over no file, and its call", () => {
    let state = initialState();
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_w", name: "Write", input: { file_path: "/p/a" } });
    for (const [perm, id] of [["perm-new", "toolu_w"], ["perm-old", "toolu_x"]]) {
      state = applyEvent(state, { type: "permission_requested", permission_id: perm, tool_use_id: id, tool_name: "Write", input: { file_path: "/p/a" } });
    }
    const next = applyCallNotes(state, { createsFile: [{ permissionId: "perm-new", toolUseId: "toolu_w" }] });
    expect(next.pendingPermissions.map((p) => p.createsFile)).toEqual([true, undefined]);
    expect(next.toolCalls[0].createsFile).toBe(true);
  });

  /** Fix round finding 1: a fast-path-allowed `Write` never raises a `PermissionRequested`, so
   *  `createsFile` above never learns of it (F22's own note is driven entirely by that event) --
   *  `autoCreatesFile` carries the identical signal for a call answered with no card at all. */
  it("marks a fast-path-allowed Write over no file as creating one, with no card ever raised", () => {
    let state = initialState();
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_w", name: "Write", input: { file_path: "/p/new.rs" } });
    const next = applyCallNotes(state, { autoCreatesFile: ["toolu_w"] });
    expect(next.toolCalls[0].createsFile).toBe(true);
    expect(applyCallNotes(state, { autoCreatesFile: [] })).toBe(state);
  });
});

describe("keepAfterSidecarStop (v1 polish item 6)", () => {
  it("keeps a conversation as lost, without its cards; keeps nothing when nothing was said", () => {
    expect(keepAfterSidecarStop(initialState(), "gone")).toBeNull();
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "permission_requested", permission_id: "p", tool_use_id: null, tool_name: "Bash", input: {} });
    const kept = keepAfterSidecarStop(state, "gone")!;
    expect(kept.status).toEqual({ kind: "unavailable", reason: "gone" });
    expect(kept.userPrompts).toEqual(state.userPrompts);
    expect(kept.pendingPermissions).toEqual([]);
    expect(kept.activeTurnId).toBeNull();
  });
});

/** Turns that do not complete: the fold must be the one `AgentSessionProjection::apply` does. */
describe("turn endings", () => {
  const NONE: TurnEndDetail = { reason: null, api_error_status: null, message: null };
  const ended = (outcome: TurnOutcome, detail = NONE, turn = "t1"): AgentDomainEvent => ({
    type: "turn_completed", turn_id: turn, outcome, result_text: "", stop_reason: null, usage: null, detail,
  });
  const running = () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "s", provider_session_id: "p", model: "m", cwd: "/w" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    return state;
  };

  it("a failed turn pushes one item with the detail's fields, at the event's own seq, and sets lastTurnEnding", () => {
    const before = running();
    const state = applyEvent(before, ended("failed", { reason: "api_error", api_error_status: 500, message: "boom" }));
    expect(state.turnEndings).toEqual([{ seq: before.nextSeq, turnId: "t1", kind: "failed", reason: "api_error", apiErrorStatus: 500, message: "boom" }]);
    expect(state.lastTurnEnding).toBe("failed");
    expect(state.activeTurnId).toBeNull();
  });

  it.each(["interrupted", "limit_reached"] as const)("%s folds to an item of that kind", (outcome) => {
    const state = applyEvent(running(), ended(outcome, { reason: "max_turns", api_error_status: null, message: null }));
    expect(state.turnEndings.map((e) => [e.kind, e.reason])).toEqual([[outcome, "max_turns"]]);
    expect(state.lastTurnEnding).toBe(outcome);
  });

  it("a completed turn adds nothing and leaves lastTurnEnding as it was", () => {
    const state = applyEvent(running(), ended("completed"));
    expect(state.turnEndings).toEqual([]);
    expect(state.lastTurnEnding).toBeNull();
  });

  it("an event without a detail still folds, to an item with nothing to say", () => {
    const bare = { ...ended("failed"), detail: undefined } as unknown as AgentDomainEvent;
    expect(applyEvent(running(), bare).turnEndings[0]).toMatchObject({ kind: "failed", reason: null, apiErrorStatus: null, message: null });
  });

  it("turn_started clears lastTurnEnding and keeps the item", () => {
    let state = applyEvent(running(), ended("failed"));
    state = applyEvent(state, { type: "turn_started", turn_id: "t2" });
    expect(state.lastTurnEnding).toBeNull();
    expect(state.turnEndings).toHaveLength(1);
  });

  it("nothing but turn_started clears it: later events leave a standing ending alone", () => {
    let state = applyEvent(running(), ended("limit_reached"));
    state = applyEvent(state, { type: "user_prompt_submitted", text: "again" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "x" });
    expect(state.lastTurnEnding).toBe("limit_reached");
  });

  describe("a session that ends under a running turn", () => {
    const resumeFailed: AgentDomainEvent = {
      type: "resume_outcome", requested_provider_session_id: "p", status: "rejected", attached_provider_session_id: null, forked: false, detail: null,
    };
    const events: [string, AgentDomainEvent][] = [
      ["session_unavailable", { type: "session_unavailable", reason: "gone" }],
      ["session_closed", { type: "session_closed", reason: "provider_failed" }],
      ["a non-attaching resume_outcome", resumeFailed],
    ];
    it.each(events)("%s pushes a lost item for the active turn", (_name, event) => {
      const before = running();
      const state = applyEvent(before, event);
      expect(state.turnEndings).toEqual([{ seq: before.nextSeq, turnId: "t1", kind: "lost", reason: null, apiErrorStatus: null, message: null }]);
      expect(state.lastTurnEnding).toBe("lost");
    });
    it.each(events)("%s with no turn running pushes nothing", (_name, event) => {
      const idle = applyEvent(running(), ended("completed"));
      const state = applyEvent(idle, event);
      expect(state.turnEndings).toEqual([]);
      expect(state.lastTurnEnding).toBeNull();
    });
    it("a resume_outcome that attached is not an ending", () => {
      const attached: AgentDomainEvent = { ...resumeFailed, status: "attached", attached_provider_session_id: "p" } as AgentDomainEvent;
      expect(applyEvent(running(), attached).turnEndings).toEqual([]);
    });
    it("a failed turn that then closes the session has one failed item and no lost item", () => {
      let state = applyEvent(running(), ended("failed"));
      state = applyEvent(state, { type: "session_closed", reason: "provider_failed" });
      expect(state.turnEndings.map((e) => e.kind)).toEqual(["failed"]);
      expect(state.lastTurnEnding).toBe("failed");
    });
  });

  it("applySnapshot takes turnEndings and lastTurnEnding from the wire, and defends against their absence", () => {
    const snapshot = {
      ...(initialState() as AgentUiSnapshot),
      turnEndings: [{ seq: 4, turnId: "t1", kind: "lost", reason: null, apiErrorStatus: null, message: null }],
      lastTurnEnding: "lost",
    } as AgentUiSnapshot;
    const state = applySnapshot(initialState(), snapshot, 9);
    expect(state.turnEndings).toEqual(snapshot.turnEndings);
    expect(state.lastTurnEnding).toBe("lost");
    const bare = { ...snapshot, turnEndings: undefined, lastTurnEnding: undefined } as unknown as AgentUiSnapshot;
    const defended = applySnapshot(state, bare, 9);
    expect(defended.turnEndings).toEqual([]);
    expect(defended.lastTurnEnding).toBeNull();
  });

  describe("the contract fixture (what Rust sends)", () => {
    for (const c of fixture.cases) {
      it(`${c.name}: folding the events gives what the snapshot carries`, () => {
        let state = initialState();
        for (const event of c.events.events as AgentDomainEvent[]) state = applyEvent(state, event);
        const snap = c.snapshot.state as unknown as AgentUiSnapshot;
        const loaded = applySnapshot(initialState(), snap, c.snapshot.throughRevision);
        expect(state.turnEndings).toEqual(snap.turnEndings);
        expect(state.lastTurnEnding).toBe(snap.lastTurnEnding);
        expect(state.transcript).toEqual(snap.transcript);
        expect(state.status).toEqual(snap.status);
        expect(state.activeTurnId).toBe(snap.activeTurnId);
        expect(loaded.turnEndings).toEqual(state.turnEndings);
        expect(loaded.nextSeq).toBe(state.nextSeq);
      });
    }
  });
});
