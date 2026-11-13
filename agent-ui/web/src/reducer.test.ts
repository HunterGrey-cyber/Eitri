import { phaseOf } from "./turnPhase";
import subagentFrames from "./fixtures/subagent-frames.json";
import sessionNotices from "./fixtures/session-notices.json";
import conversationReset from "./fixtures/conversation-reset.json";
import backgroundTasksFixture from "./fixtures/background-tasks.json";
import * as roster322 from "./fold/roster";
import * as notices322 from "./fold/notices";
import * as questions322 from "./fold/questions";
import * as subagents322 from "./fold/subagents";
import * as reset322 from "./fold/reset";
import protocol322 from "./fixtures/protocol-322.json";
import questionsFixture from "./fixtures/questions.json";
/** Plan records and the plan phase as Rust's projection folds them; written by `agent/src/testkit/plans_fixture.rs`. */
import plansFixture from "./fixtures/plans.json";
import * as plans326 from "./fold/plans";
import { describe, expect, it, vi } from "vitest";
import { buildTimeline } from "./timeline";
import { applyCallNotes, applyEvent, applyEvents, applySnapshot, keepAfterFailure, initialState, MAX_HELD_DENIALS, resetToStartScreen, withEditorCards } from "./reducer";
import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState, EditorCardWire, TurnEndDetail, TurnOutcome, UsageInfo } from "./types";
/** What Rust really sends for the turn-ending cases; written from its serializer. */
import fixture from "./fixtures/turn-endings.json";
/** Turn starts, notices and questions as Rust folds and sends them; written from its serializer. */
import historyRecords from "./fixtures/history-records.json";
/** Turn ends of turns this page did not run, hook blocks, unseen work and withdrawn prompts, as Rust
 *  folds and sends them; written from its serializer. */
import unseenTurns from "./fixtures/unseen-turns.json";
import cliTurns from "./fixtures/cli-turns.json";
/** The broker cards as Rust serialises them (`EditorCardWire`); `core/src/editor_tools/cards.rs` checks the same file byte for byte. */
import editorCardsFixture from "./fixtures/editor-cards.json";

it("desk_reducer_unchanged_without_the_hook", () => {
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  const before = initialState();
  const event = { type: "future_event" } as unknown as AgentDomainEvent;
  expect(applyEvent(before, event)).toEqual({ ...before, nextSeq: 1 });
  expect(warn).toHaveBeenCalledWith("agent-ui: completely unhandled event shape from Rust", event);
  const onUnknown = vi.fn();
  expect(applyEvent(before, event, onUnknown)).toEqual(applyEvent(before, event));
  expect(onUnknown).toHaveBeenCalledWith(event);
  expect([event].reduce(applyEvent, before)).toEqual(applyEvent(before, event));
  const nested = { type: "subagent_frame", parent_tool_use_id: "tool-1", frame: event } as unknown as AgentDomainEvent;
  const quiet = applyEvent(before, nested);
  onUnknown.mockClear();
  expect(applyEvent(before, nested, onUnknown)).toEqual(quiet);
  expect(onUnknown).toHaveBeenCalledTimes(1);
  expect(onUnknown).toHaveBeenCalledWith(event);
  const known = { type: "subagent_frame", parent_tool_use_id: "tool-1",
    frame: { type: "model_reported", model: "inner" } } as unknown as AgentDomainEvent;
  onUnknown.mockClear();
  applyEvent(before, known, onUnknown);
  expect(onUnknown).not.toHaveBeenCalled();
  warn.mockRestore();
});

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
    // `transcript` holds assistant messages, and consecutive text deltas are one message -- under
    // partial streaming they are two fragments of one sentence.
    let state = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "text", text: "first" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "pondering" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "second" });
    expect(state.transcript.map((m) => m.text)).toEqual(["firstsecond"]);
  });

  // `After the table.TURN-1-DONE` is two messages when a boundary separates the deltas, as in Rust.
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

  it("tool_call_completed completes only the first call with that id, as the Rust fold does", () => {
    let state = applyEvent(initialState(), { type: "tool_call_started", turn_id: "t1", tool_use_id: "", name: "Bash", input: { command: "a" } });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "", name: "Bash", input: { command: "b" } });
    state = applyEvent(state, { type: "tool_call_completed", turn_id: "t1", tool_use_id: "", content: "done", is_error: false });
    expect(state.toolCalls.map((c) => c.result)).toEqual([{ content: "done", isError: false }, null]);
  });

  it("turn_started sets activeTurnId, and a subsequent turn_completed clears it regardless of outcome", () => {
    // Parametrized over all four TurnOutcome values, matching the Rust test of the same
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

  it("keeps the effort the agent last stated, as AgentSessionProjection does", () => {
    const said = (effort: string | null): AgentDomainEvent => ({ type: "effort_reported", effort });
    let state = applyEvent(initialState(), said("high"));
    expect(state.effort).toBe("high");
    state = applyEvent(state, said(null));
    expect(state.effort).toBeNull();
    state = applyEvent(state, said("low"));
    state = applyEvent(state, { type: "conversation_reset", new_conversation_id: "n", reason: "clear" } as never);
    expect(state.effort, "/clear keeps the process, and what it runs at").toBe("low");
    state = applyEvent(state, { type: "resume_outcome", requested_provider_session_id: "c", status: "attached", attached_provider_session_id: "c", forked: false, detail: null } as never);
    expect(state.effort, "a resume replaces the process").toBeNull();
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

  /* The tab's last reported usage. Each report REPLACES the figure whole
     (the SDK reports a running total and `/clear` resets it, so a lower later figure is the truth),
     and a turn that reports none -- interrupted, synthesized -- leaves the last figure standing:
     silence is not a measurement, and the band must never show an invented zero. Mirrors
     `AgentSessionProjection::apply`'s `TurnCompleted` arm. */
  describe("the usage report", () => {
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
      // And the lost-session view a stopped sidecar leaves behind (with its recovery note).
      const said = applyEvent(applyEvent(initialState(), done(usage)), { type: "content_delta", turn_id: "t2", kind: "text", text: "hi" });
      expect(keepAfterFailure(said, "the sidecar stopped")?.usage).toEqual(usage);
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

  /* A session that dies mid-turn must clear activeTurnId; leaving it set keeps the composer
     disabled with a spinner and makes the truncated reply look complete. */
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

  // The mode lives on the tab (`tabs` envelope), not this per-session projection, so the
  // reducer folds nothing here -- unlike a genuinely unrecognized shape, this is a KNOWN event, so
  // it must not reach the exhaustiveness guard's console.warn either.
  it("permission_mode_changed changes nothing but nextSeq and does not warn", () => {
    const consoleWarn = console.warn;
    let warned = false;
    console.warn = () => { warned = true; };
    const before = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    const after = applyEvent(before, { type: "permission_mode_changed", mode: "bypass", provider_mode: "BYPASS_PERMISSIONS", floor_applied: false });
    expect(after).toEqual({ ...before, nextSeq: before.nextSeq + 1 });
    expect(warned).toBe(false);
    console.warn = consoleWarn;
  });

  // Rust's projection bumps `last_revision` once for every event, a mode change included, and a turn's
  // footer is placed by a seq Rust names: an item folded after a mode change must get Rust's number.
  it("an item after a permission_mode_changed takes the seq Rust gives it (one per event)", () => {
    const events: AgentDomainEvent[] = [
      { type: "turn_started", turn_id: "t1" },
      { type: "permission_mode_changed", mode: "bypass", provider_mode: "BYPASS_PERMISSIONS", floor_applied: false },
      { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" },
      { type: "permission_mode_changed", mode: "auto", provider_mode: "DEFAULT", floor_applied: false },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "c1", name: "Read", input: {} },
    ] as AgentDomainEvent[];
    const one = events.reduce(applyEvent, initialState());
    expect(one.thinking.map((t) => t.seq)).toEqual([2]);
    expect(one.toolCalls.map((c) => c.seq)).toEqual([4]);
    expect(one.nextSeq).toBe(events.length);
    expect(applyEvents(initialState(), events)).toEqual(one);
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
  it("a repeated permission_requested replaces the card with that id, as the Rust map does", () => {
    let state = applyEvent(initialState(), { type: "permission_requested", permission_id: "r1", tool_use_id: null, tool_name: "Bash", input: { a: 1 } });
    state = applyEvent(state, { type: "permission_requested", permission_id: "r2", tool_use_id: null, tool_name: "Read", input: {} });
    state = applyEvent(state, { type: "permission_requested", permission_id: "r1", tool_use_id: null, tool_name: "Bash", input: { a: 2 } });
    expect(state.pendingPermissions.map((p) => p.permissionId)).toEqual(["r2", "r1"]);
    expect(state.pendingPermissions[1].input).toEqual({ a: 2 });
    expect(state.pendingPermissions[1].seq).toBeGreaterThan(state.pendingPermissions[0].seq);
    state = applyEvent(state, { type: "permission_resolved", permission_id: "r1", outcome: "allowed" });
    expect(state.pendingPermissions.map((p) => p.permissionId)).toEqual(["r2"]);
  });

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

  /* The CLI's own prompt for a call arrives as a SECOND request carrying the same tool_use_id
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
    // An origin this build does not know is carried, so the card calls it neutrally.
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

  /* The legacy backend's `PreToolUse` hook payload's
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
    // Rust sends the event when AgentSession::respond_permission (or interrupt()) resolves the
    // request; the frontend must wait for it rather than guess optimistically.
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

  /* A restored history (from the host) reaches this side only inside a snapshot -- there is no
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
    // A snapshot carries neither `nextSeq`, which comes from the envelope beside it, nor `cliUnseenEpisode`, which
    // counts on from the page's own.
    expect(state).toEqual({ ...snapshot, nextSeq: 1, cliUnseenEpisode: 1 });
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

  it("a session_opened in the middle of a reply does not end the message, as the Rust fold does not", () => {
    const opened: AgentDomainEvent = { type: "session_opened", session_id: "s", provider_session_id: "p", model: "m", cwd: "/tmp" };
    let state = applyEvent(initialState(), { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "hi" });
    state = applyEvent(state, opened);
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: " there" });
    expect(state.transcript.map((m) => m.text)).toEqual(["hi there"]);
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
    // Under partial streaming a 600-word reply arrives as 400+ deltas. One transcript entry per
    // delta renders 400 separate bubbles, each fragment markdown-parsed alone -- so "`eitri_"
    // or "**bold" is not valid standalone markdown and the formatting breaks.
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
    // The wire carries `assistantMessageOpen` on `AgentUiSnapshot`, here `false`: the tool call closed the
    // message before this snapshot was ever taken, on both sides.
    const { nextSeq: _seq, turnThinking: _think, ...wire } = { ...state, transcript: [{ seq: 0, text: "an earlier reply" }] };
    const restored = applySnapshot(state, wire, 1);
    expect(restored.assistantMessageOpen).toBe(false);
    const next = applyEvent(restored, delta("new message"));
    expect(next.transcript.map((m) => m.text)).toEqual(["an earlier reply", "new message"]);
  });

  it("a mid-stream snapshot keeps the message open, so the next delta continues it rather than splitting it", () => {
    // Rust keeps appending to the open message underneath a mid-reply snapshot (a tab switch,
    // `prefix r`, a bounded-queue Resync). Forcing `assistantMessageOpen: false` would split the
    // reply into rows with broken markdown at the seam. Fold `session_opened, turn_started,
    // text 'Use **strong'`, apply a snapshot, then fold `text ' emphasis** here.'`.
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, delta("Use **strong"));
    expect(state.assistantMessageOpen).toBe(true);
    // The wire Rust sends mid-stream: `assistantMessageOpen` present and `true`, matching
    // the live projection this state was folded from.
    const { nextSeq: _seq, turnThinking: _think, ...wire } = state;
    const restored = applySnapshot(initialState(), wire, state.nextSeq);
    expect(restored.assistantMessageOpen).toBe(true);
    const next = applyEvent(restored, delta(" emphasis** here."));
    expect(next.transcript.map((m) => m.text)).toEqual(["Use **strong emphasis** here."]);
  });
});

/* `turnThinking` is the one ephemeral bit `turnPhase.ts`
   reads that is not already sitting in the four collections. Invariant under test throughout: it
   can only ever be LOST, never invented -- see `turnPhase.test.ts`'s "resync degradation" test for
   the same invariant one layer up, on the phase it feeds. */
describe("turnThinking", () => {
  it("is set by a content_delta with kind thinking", () => {
    const state = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    expect(state.turnThinking).toBe(true);
  });

  it("a repeated thinking delta keeps the bit and appends its text, advancing nextSeq like any other event", () => {
    // Thinking text is drawn, so the fold must carry it; `applyEvents` batches deltas into one fold
    // and one render.
    const first = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    const second = applyEvent(first, { type: "content_delta", turn_id: "t1", kind: "thinking", text: " still going" });
    expect(second.turnThinking).toBe(true);
    expect(second.thinking).toEqual([{ seq: 0, turnId: "t1", text: "hmm still going" }]);
    expect(second.nextSeq).toBe(first.nextSeq + 1);
  });

  it("a single thinking delta advances nextSeq like any other event", () => {
    const before = initialState().nextSeq;
    const state = applyEvent(initialState(), { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    expect(state.nextSeq).toBe(before + 1);
  });

  it("a content_delta of an UNKNOWN kind is a no-op, not a transcript entry with no text", () => {
    // An unknown kind such as `redacted_thinking` must not fall into the text path: its missing text
    // would reach `renderMarkdown` and throw inside `marked`, unmounting the panel without an error boundary.
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
    // event that creates nothing.
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

/* Interleaved ordering, the reducer's half.

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

describe("an agent's policy tripwire and account limits", () => {
  const limits = (usedPercent: number, rest: Partial<Extract<AgentDomainEvent, { type: "account_limits" }>> = {}): AgentDomainEvent => ({
    type: "account_limits",
    used_percent: usedPercent,
    window_minutes: 300,
    resets_at: 1790000000,
    plan: "pro",
    ...rest,
  });

  it("account_limits event stores limits; policy_mismatch is ignored", () => {
    let state = applyEvent(initialState(), limits(42.5));
    expect(state.accountLimits).toEqual({ usedPercent: 42.5, windowMinutes: 300, resetsAt: 1790000000, plan: "pro" });
    expect(state.nextSeq).toBe(1);

    const before = state;
    state = applyEvent(state, { type: "policy_mismatch", requested: "on-request", reported: "never" });
    expect(state.accountLimits).toEqual(before.accountLimits);
    expect(state.status).toEqual(before.status);
    expect(state.nextSeq).toBe(before.nextSeq + 1);
  });

  it("the latest report replaces the earlier one whole, and a slot it leaves out is null", () => {
    let state = applyEvent(initialState(), limits(10));
    state = applyEvent(state, limits(12.5, { window_minutes: null, resets_at: null, plan: null }));
    expect(state.accountLimits).toEqual({ usedPercent: 12.5, windowMinutes: null, resetsAt: null, plan: null });
  });

  it("a snapshot carries the limits, and one without the key (an older build) leaves none", () => {
    const withLimits = applyEvent(initialState(), limits(42.5));
    const { nextSeq: _n, turnThinking: _t, denialsBeforeTheirCall: _d, ...snapshot } = withLimits;
    expect(applySnapshot(initialState(), snapshot, 7).accountLimits).toEqual(withLimits.accountLimits);
    // A tab that has reported nothing must not keep the figure of the one shown before it.
    expect(applySnapshot(withLimits, { ...snapshot, accountLimits: null }, 7).accountLimits).toBeNull();
    const { accountLimits: _a, ...old } = snapshot;
    expect(applySnapshot(withLimits, old as typeof snapshot, 7).accountLimits).toBeNull();
  });
});

describe("the commands an agent left running", () => {
  it("background_calls replaces the count, as Rust's fold does, and a snapshot carries it", () => {
    let state = applyEvent(initialState(), { type: "background_calls", running: 2 });
    expect(state.backgroundCalls).toBe(2);
    expect(state.nextSeq).toBe(1);
    state = applyEvent(state, { type: "background_calls", running: 0 });
    expect(state.backgroundCalls).toBe(0);
    expect(initialState().backgroundCalls).toBe(0);

    const counted = applyEvent(initialState(), { type: "background_calls", running: 3 });
    const { nextSeq: _n, turnThinking: _t, denialsBeforeTheirCall: _d, ...snapshot } = counted;
    expect(applySnapshot(initialState(), snapshot, 4).backgroundCalls).toBe(3);
    // Another tab's snapshot without one is a tab with none.
    const { backgroundCalls: _b, ...old } = snapshot;
    expect(applySnapshot(counted, old as typeof snapshot, 4).backgroundCalls).toBe(0);
  });
});

describe("the slash commands an agent listed", () => {
  it("commands_available replaces the whole list, as Rust's fold does, and a snapshot carries it", () => {
    expect(initialState().availableCommands).toBeNull();
    let state = applyEvent(initialState(), {
      type: "commands_available",
      commands: [{ name: "web", description: "Search the web", hint: "query" }, { name: "test", description: "" }],
    });
    expect(state.availableCommands).toEqual([{ name: "web", description: "Search the web", hint: "query" }, { name: "test", description: "" }]);
    state = applyEvent(state, { type: "commands_available", commands: [{ name: "plan", description: "Plan" }] });
    expect(state.availableCommands).toEqual([{ name: "plan", description: "Plan" }]);
    const emptied = applyEvent(state, { type: "commands_available", commands: [] });
    expect(emptied.availableCommands, "an empty list withdraws them; it is not 'no report'").toEqual([]);

    const { nextSeq: _n, turnThinking: _t, denialsBeforeTheirCall: _d, ...snapshot } = state;
    expect(applySnapshot(initialState(), snapshot, 4).availableCommands).toEqual([{ name: "plan", description: "Plan" }]);
    // Another tab's snapshot without the key, or with a null, is a tab that listed none.
    const { availableCommands: _a, ...old } = snapshot;
    expect(applySnapshot(state, old as typeof snapshot, 4).availableCommands).toBeNull();
    expect(applySnapshot(state, { ...snapshot, availableCommands: null }, 4).availableCommands).toBeNull();
  });

  it("holds what an agent said to the page's bounds again", () => {
    const state = applyEvent(initialState(), {
      type: "commands_available",
      commands: [{ name: "ok", description: "line one\nline two" }, { name: "bad name", description: "x" }],
    });
    expect(state.availableCommands).toEqual([{ name: "ok", description: "line one line two" }]);
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
      // The end of the turn the refusal arrived in: an end naming another turn ends nothing here.
      let state = applyEvent(initialState(), { type: "turn_started", turn_id: "t" });
      state = applyEvent(state, denied("toolu_never"));
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

describe("applyCallNotes", () => {
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

  it("puts the note for a call answered without a card on that call", () => {
    let state = initialState();
    for (const id of ["toolu_1", "toolu_2"]) {
      state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: id, name: "Write", input: {} });
    }
    const next = applyCallNotes(state, {
      promptNotes: [
        { toolUseId: "toolu_2", note: "Claude Code asked — allowed with your approval" },
        { toolUseId: "toolu_gone", note: "x" },
      ],
    });
    expect(next.toolCalls.map((c) => c.promptNote)).toEqual([undefined, "Claude Code asked — allowed with your approval"]);
    expect(next.toolCalls[0]).toBe(state.toolCalls[0]);
    expect(applyCallNotes(state, { promptNotes: [] })).toBe(state);
  });

  /** The acceptEdits fast path's own note, applied the same way as the rule note
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

  /** A fast-path-allowed `Write` never raises a `PermissionRequested`, so
   *  `createsFile` above never learns of it (the creation note is driven entirely by that event) --
   *  `autoCreatesFile` carries the identical signal for a call answered with no card at all. */
  it("marks a fast-path-allowed Write over no file as creating one, with no card ever raised", () => {
    let state = initialState();
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_w", name: "Write", input: { file_path: "/p/new.rs" } });
    const next = applyCallNotes(state, { autoCreatesFile: ["toolu_w"] });
    expect(next.toolCalls[0].createsFile).toBe(true);
    expect(applyCallNotes(state, { autoCreatesFile: [] })).toBe(state);
  });
});

describe("keepAfterFailure", () => {
  it("keeps a conversation as lost, without its cards; keeps nothing when nothing was said", () => {
    expect(keepAfterFailure(initialState(), "gone")).toBeNull();
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "permission_requested", permission_id: "p", tool_use_id: null, tool_name: "Bash", input: {} });
    const kept = keepAfterFailure(state, "gone")!;
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
    expect(state.turnEndings).toEqual([{ seq: before.nextSeq, turnId: "t1", kind: "failed", reason: "api_error", apiErrorStatus: 500, message: "boom", resetsAt: null }]);
    expect(state.lastTurnEnding).toBe("failed");
    expect(state.activeTurnId).toBeNull();
  });

  it.each(["interrupted", "limit_reached"] as const)("%s folds to an item of that kind", (outcome) => {
    const state = applyEvent(running(), ended(outcome, { reason: "max_turns", api_error_status: null, message: null }));
    expect(state.turnEndings.map((e) => [e.kind, e.reason])).toEqual([[outcome, "max_turns"]]);
    expect(state.lastTurnEnding).toBe(outcome);
  });

  it("an ending keeps the reset time it had; a later report fills one that had none, until the next turn", () => {
    const limits = (resets_at: number) =>
      ({ type: "account_limits", used_percent: 100, window_minutes: 300, resets_at, plan: null }) as AgentDomainEvent;
    const usage = { reason: "usage_limit", api_error_status: null, message: null };
    let state = applyEvent(running(), limits(1000));
    state = applyEvent(state, ended("limit_reached", usage));
    state = applyEvent(state, limits(2000));
    expect(state.turnEndings[0].resetsAt).toBe(1000);
    expect(state.accountLimits?.resetsAt).toBe(2000);

    let late = applyEvent(running(), ended("limit_reached", usage));
    expect(late.turnEndings[0].resetsAt).toBeNull();
    late = applyEvent(late, limits(3000));
    late = applyEvent(late, limits(4000));
    expect(late.turnEndings[0].resetsAt).toBe(3000);
    late = applyEvent(late, { type: "turn_started", turn_id: "t2" } as AgentDomainEvent);
    late = applyEvent(late, ended("limit_reached", usage, "t2"));
    late = applyEvent(late, limits(5000));
    expect(late.turnEndings.map((e) => e.resetsAt)).toEqual([3000, 4000]);
  });

  it("a resumed session's past ending is not filled by a live report", () => {
    // What Rust sends for a resume whose last past turn hit the limit: the row, and no band word.
    const snapshot = {
      ...(initialState() as AgentUiSnapshot),
      turnEndings: [
        { seq: 1, turnId: "old-1", kind: "limit_reached", reason: "usage_limit", apiErrorStatus: null, message: null, resetsAt: null },
      ],
      lastTurnEnding: null,
    } as AgentUiSnapshot;
    let state = applySnapshot(initialState(), snapshot, 2);
    state = applyEvent(state, { type: "session_opened", session_id: "s", provider_session_id: "p", model: "m", cwd: "/p" } as AgentDomainEvent);
    state = applyEvent(state, { type: "account_limits", used_percent: 3, window_minutes: 300, resets_at: 1000, plan: null } as AgentDomainEvent);
    expect(state.accountLimits?.resetsAt).toBe(1000);
    expect(state.turnEndings.map((e) => e.resetsAt)).toEqual([null]);
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
      expect(state.turnEndings).toEqual([{ seq: before.nextSeq, turnId: "t1", kind: "lost", reason: null, apiErrorStatus: null, message: null, resetsAt: null }]);
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

describe("applyEvents folds a batch exactly as one event at a time, over every kind of event", () => {
  /** A small seeded generator, so a failure names its seed and reproduces. */
  function rng(seed: number) {
    let s = seed >>> 0;
    return () => {
      s = (s * 1664525 + 1013904223) >>> 0;
      return s / 2 ** 32;
    };
  }

  function randomEvents(r: () => number, n: number): AgentDomainEvent[] {
    const out: AgentDomainEvent[] = [];
    let turn = 0;
    let call = 0;
    let msg = 0;
    const pick = <T,>(xs: T[]) => xs[Math.floor(r() * xs.length)];
    for (let i = 0; i < n; i++) {
      const k = r();
      const tid = `t${turn}`;
      if (k < 0.25) out.push({ type: "content_delta", turn_id: pick([tid, `t${turn + 1}`]), kind: "thinking", text: pick(["", "a", "b\n"]) });
      else if (k < 0.45) out.push({ type: "content_delta", turn_id: pick([tid, `t${turn + 1}`]), kind: "text", text: pick(["", "x", "y "]) });
      else if (k < 0.5) out.push({ type: "user_message_delivered", client_message_id: `m${msg++}`, turn_id: tid, text: "steer" });
      else if (k < 0.55) out.push({ type: "user_message_dropped", client_message_id: `m${msg++}`, reason: "r" });
      else if (k < 0.58) out.push({ type: "commands_available", commands: [{ name: "a", description: "" }] });
      else if (k < 0.63) out.push({ type: "tool_call_started", turn_id: tid, tool_use_id: `c${call++}`, name: "Read", input: {} });
      else if (k < 0.68) out.push({ type: "tool_call_completed", turn_id: tid, tool_use_id: `c${Math.max(0, call - 1)}`, content: "ok", is_error: false });
      else if (k < 0.72) out.push({ type: "assistant_message_boundary", turn_id: tid });
      else if (k < 0.76) out.push({ type: "turn_started", turn_id: `t${++turn}` });
      else if (k < 0.8)
        out.push({
          type: "turn_completed",
          turn_id: tid,
          outcome: "completed",
          result_text: "",
          stop_reason: null,
          usage: null,
          detail: { reason: null, api_error_status: null, message: null },
        });
      else if (k < 0.84) out.push({ type: "user_prompt_submitted", text: "p" });
      else if (k < 0.88) out.push({ type: "background_calls", running: 1 });
      else if (k < 0.91) out.push({ type: "permission_mode_changed", mode: "auto", provider_mode: "default", floor_applied: false });
      else out.push({ type: "content_delta", turn_id: tid, kind: "thinking", text: "z" });
    }
    return out;
  }

  it("for 500 seeded sequences split into random batches", () => {
    for (let seed = 1; seed <= 500; seed++) {
      const r = rng(seed);
      const events = randomEvents(r, 40);
      const one = events.reduce(applyEvent, initialState());
      let batched = initialState();
      for (let i = 0; i < events.length; ) {
        const len = 1 + Math.floor(r() * 12);
        batched = applyEvents(batched, events.slice(i, i + len));
        i += len;
      }
      expect(batched, `seed ${seed}`).toEqual(one);
      // Rust numbers every event once: the page's counter is the number of events folded.
      expect(one.nextSeq, `seed ${seed}`).toBe(events.length);
    }
  });
});

describe("turn starts, notices and questions (history-records.json, what Rust sends)", () => {
  const events = historyRecords.events.events as unknown as AgentDomainEvent[];
  const final = historyRecords.snapshot.state as unknown as AgentUiSnapshot;
  const middle = historyRecords.middle.state as unknown as AgentUiSnapshot;
  const check = (state: AgentUiState) => {
    expect(state.turnStarts).toEqual(final.turnStarts);
    expect(state.notices).toEqual(final.notices);
    expect(state.questions).toEqual(final.questions);
    expect(state.activeTurnId).toBe(final.activeTurnId);
    expect(state.transcript).toEqual(final.transcript);
    expect(state.userPrompts).toEqual(final.userPrompts);
    expect(state.nextSeq).toBe(historyRecords.snapshot.throughRevision);
  };

  it("folding every event one by one gives what the last snapshot carries", () => {
    check(events.reduce(applyEvent, initialState()));
  });

  it("folding them as one batch gives the same", () => {
    check(applyEvents(initialState(), events));
  });

  it("the middle snapshot, with questions still waiting, and the events after it give the same", () => {
    const loaded = applySnapshot(initialState(), middle, historyRecords.middle.throughRevision);
    expect(loaded.questions.map((q) => q.state.kind)).toEqual(["waiting", "waiting"]);
    check(applyEvents(loaded, events.slice(historyRecords.split)));
  });

  it("the last snapshot loads as it was sent", () => {
    check(applySnapshot(initialState(), final, historyRecords.snapshot.throughRevision));
  });

  it("covers every cause, every notice kind and every way a question ends", () => {
    const loaded = applySnapshot(initialState(), final, historyRecords.snapshot.throughRevision);
    expect(loaded.turnStarts.map((r) => [r.origin, r.cause?.kind ?? null])).toEqual([
      ["cli", "task"],
      ["cli", "scheduled"],
      ["cli", "peer"],
      ["cli", "other"],
      ["cli", "unknown"],
      ["follow_up", null],
    ]);
    expect(new Set(loaded.notices.map((n) => n.kind)).size).toBe(9);
    const outcomes = loaded.questions.flatMap((q) => (q.state.kind === "settled" ? [q.state.outcome] : []));
    expect(new Set(outcomes)).toEqual(
      new Set(["answered", "declined", "chat_requested", "interrupted", "expired", "session_closed", "provider_failed", "host_refused"]),
    );
    expect(loaded.questions.filter((q) => q.state.kind === "waiting").map((q) => q.interactionId)).toEqual(["q-waiting", "q-last"]);
  });

  it("a snapshot without the three keys (a conversation with none, or an older build) holds none", () => {
    const { turnStarts: _s, notices: _n, questions: _q, ...older } = final;
    const loaded = applySnapshot(initialState(), older as AgentUiSnapshot, historyRecords.snapshot.throughRevision);
    expect(loaded.turnStarts).toEqual([]);
    expect(loaded.notices).toEqual([]);
    expect(loaded.questions).toEqual([]);
  });

  it("a turn the user sent leaves no start row, and a settlement for no question changes nothing", () => {
    let state = applyEvent(initialState(), { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "turn_attributed", turn_id: "t1", origin: "client", cause: null });
    state = applyEvent(state, { type: "question_settled", interaction_id: "nobody", outcome: "declined", answers: [], reason: null });
    expect(state.turnStarts).toEqual([]);
    expect(state.questions).toEqual([]);
    expect(state.nextSeq).toBe(3);
  });
});

describe("turn ends this page did not run, hook blocks, unseen work, withdrawn prompts (unseen-turns.json)", () => {
  const cases = unseenTurns.cases as unknown as {
    name: string;
    events: { events: AgentDomainEvent[]; throughRevision: number };
    snapshot: { state: AgentUiSnapshot; throughRevision: number };
  }[];
  const byName = (name: string) => {
    const found = cases.find((c) => c.name === name);
    if (found === undefined) throw new Error(`no case ${name}`);
    return found;
  };
  const folded = (name: string) => byName(name).events.events.reduce(applyEvent, initialState());

  for (const c of cases) {
    it(`${c.name}: folding the events, one by one or as a batch, gives what the snapshot carries`, () => {
      const loaded = applySnapshot(initialState(), c.snapshot.state, c.snapshot.throughRevision);
      for (const state of [c.events.events.reduce(applyEvent, initialState()), applyEvents(initialState(), c.events.events)]) {
        expect(state.activeTurnId).toBe(c.snapshot.state.activeTurnId);
        expect(state.userPrompts).toEqual(c.snapshot.state.userPrompts);
        expect(state.transcript).toEqual(c.snapshot.state.transcript);
        expect(state.notices).toEqual(c.snapshot.state.notices ?? []);
        expect(state.turnEndings).toEqual(c.snapshot.state.turnEndings);
        expect(state.lastTurnEnding).toBe(c.snapshot.state.lastTurnEnding);
        expect(state.assistantMessageOpen).toBe(c.snapshot.state.assistantMessageOpen);
        expect(state.cliUnseenBusy).toBe(c.snapshot.state.cliUnseenBusy ?? false);
        expect(state.nextSeq).toBe(c.snapshot.throughRevision);
        // The event-by-event page and the page loaded from Rust's snapshot are the same page.
        expect(loaded.notices).toEqual(state.notices);
        expect(loaded.transcript).toEqual(state.transcript);
        expect(loaded.userPrompts).toEqual(state.userPrompts);
        expect(loaded.turnEndings).toEqual(state.turnEndings);
        expect(loaded.activeTurnId).toBe(state.activeTurnId);
        expect(loaded.cliUnseenBusy).toBe(state.cliUnseenBusy);
        expect(loaded.nextSeq).toBe(state.nextSeq);
      }
    });
  }

  it("a turn this page did not run, while its own runs, leaves its own running and shows the answer apart", () => {
    const state = folded("foreign_while_running_with_text");
    expect(state.activeTurnId).toBe("t1");
    expect(state.notices.map((n) => n.kind)).toEqual(["unseen_turn"]);
    expect(state.transcript.map((m) => m.text)).toEqual(["Looking at it", "The tests passed in the background.", " and the fix is in"]);
    expect(state.notices[0].seq).toBeLessThan(state.transcript[1].seq);
  });

  it("with none running, such an end draws no failed row, and without text nothing at all", () => {
    for (const name of ["foreign_none_running_with_text", "foreign_none_running_without_text", "foreign_while_running_without_text"]) {
      const state = folded(name);
      expect(state.turnEndings, name).toEqual([]);
      expect(state.lastTurnEnding, name).toBeNull();
    }
    expect(folded("foreign_none_running_without_text").notices).toEqual([]);
    expect(folded("foreign_while_running_without_text").activeTurnId).toBe("t1");
  });

  it("a prompt a hook stopped shows the hook's words; /compact shows nothing new", () => {
    const blocked = folded("hook_blocked_prompt");
    expect(blocked.notices.map((n) => [n.kind, n.level])).toEqual([["hook_block", "warning"]]);
    expect(blocked.notices[0].content).toContain("production deploys need a ticket");
    expect(blocked.transcript).toEqual([]);
    const compact = folded("compact_answers_without_a_reply");
    expect(compact.notices).toEqual([]);
    expect(compact.transcript).toEqual([]);
  });

  it("unseen work is noted until the next completion, and no turn runs because of it", () => {
    const noted = folded("cli_work_unseen");
    expect(noted.cliUnseenBusy).toBe(true);
    expect(noted.activeTurnId).toBeNull();
    expect(folded("cli_work_unseen_then_its_end").cliUnseenBusy).toBe(false);
  });

  it("each stretch of unseen work gets a number of its own: more messages of one stretch keep it", () => {
    const unseen: AgentDomainEvent = { type: "cli_work_unseen" };
    const end: AgentDomainEvent = {
      type: "turn_completed", turn_id: "cli-a", outcome: "completed", result_text: "", stop_reason: null, usage: null,
      detail: { reason: null, api_error_status: null, message: null },
    };
    const first = applyEvent(initialState(), unseen);
    expect(first.cliUnseenEpisode).toBe(1);
    expect(applyEvents(first, [unseen, unseen]).cliUnseenEpisode).toBe(1);
    const ended = applyEvent(first, end);
    expect(ended.cliUnseenBusy).toBe(false);
    expect(ended.cliUnseenEpisode).toBe(1);
    const second = applyEvent(ended, unseen);
    expect(second.cliUnseenBusy).toBe(true);
    expect(second.cliUnseenEpisode).toBe(2);
    // A snapshot cannot say the work it shows is the stretch the page knew, so it is a new one; a start-over never
    // counts down.
    const loaded = applySnapshot(second, { ...initialState(), cliUnseenBusy: true }, 5);
    expect(loaded.cliUnseenEpisode).toBe(3);
    expect(resetToStartScreen(loaded).cliUnseenEpisode).toBe(3);
  });

  it("a session that ends ends the note of work out of sight: no completion for it is coming", () => {
    for (const name of [
      "cli_work_unseen_then_the_session_closes",
      "cli_work_unseen_then_the_session_is_lost",
      "cli_work_unseen_then_a_resume_that_did_not_attach",
    ]) {
      expect(folded(name).cliUnseenBusy, name).toBe(false);
    }
  });

  it("a withdrawn prompt takes back the newest undelivered one, and nothing else", () => {
    expect(folded("withdrawn_after_an_undelivered_prompt").userPrompts.map((p) => p.text)).toEqual(["fix the parser"]);
    expect(folded("withdrawn_after_a_delivered_one").userPrompts).toEqual([{ seq: 2, text: "and the docs", delivered: true }]);
    expect(folded("withdrawn_with_none").userPrompts).toEqual([]);
  });
});


describe("protocol 3.22 routing", () => {
  const events = protocol322.domain_events as AgentDomainEvent[];

  it("folds every event to the Rust snapshot, batched or separately", () => {
    const snapshot = protocol322.snapshot;
    const restored = applySnapshot(initialState(), snapshot.state as unknown as AgentUiSnapshot, snapshot.throughRevision);
    for (const state of [events.reduce(applyEvent, initialState()), applyEvents(initialState(), events)]) {
      for (const key of ["userPrompts", "transcript", "toolCalls", "status", "activeTurnId", "pendingPermissions", "thinking", "thinkingOpen", "assistantMessageOpen", "turnStarts", "notices", "questions", "turnEndings", "lastTurnEnding", "backgroundCalls", "nextSeq"] as const) {
        expect(state[key], key).toEqual(restored[key]);
      }
    }
    expect(protocol322.events.events).toEqual(protocol322.domain_events);
  });

  it("keeps child text outside the main reply through delta batching", () => {
    const state = applyEvents(initialState(), events.slice(0, 13));
    expect(state.transcript.find((m) => m.seq === 7)?.text).toBe("before after");
    expect(state.toolCalls.find((call) => call.toolUseId === "p")?.children?.transcript[0].text).toBe("child reply");
    expect(state.activeTurnId).toBe("main");
    expect(state.assistantMessageOpen).toBe(true);
    expect(state.nextSeq).toBe(13);
  });

  it("preserves existing fields for the empty folds", () => {
    const incoming = { ...initialState(), turnThinking: true };
    const quiet = new Set(["turn_cause_updated", "cli_cycle_started", "background_tasks_changed", "task_event"]);
    for (const event of events.filter((e) => quiet.has(e.type))) {
      expect(applyEvent(incoming, event)).toEqual({ ...incoming, nextSeq: incoming.nextSeq + 1 });
    }
  });

  it("calls the lifecycle and content hooks only for main events", () => {
    const rosterFold = vi.spyOn(roster322, "fold");
    const noticeFold = vi.spyOn(notices322, "fold");
    const questionFold = vi.spyOn(questions322, "fold");
    const resetFold = vi.spyOn(reset322, "fold");
    const childFold = vi.spyOn(subagents322, "fold");
    const content = vi.spyOn(notices322, "onContent");
    const end = vi.spyOn(notices322, "onTurnEnd");
    const completed = vi.spyOn(roster322, "onTurnCompleted");
    const rosterEnd = vi.spyOn(roster322, "onSessionEnd");
    const questionEnd = vi.spyOn(questions322, "onSessionEnd");
    try {
      let state = events.reduce(applyEvent, initialState());
      state = applyEvent(state, { type: "session_closed", reason: "closed" });
      applyEvent(state, { type: "session_unavailable", reason: "gone" });
      expect(rosterFold).toHaveBeenCalledTimes(2);
      expect(noticeFold).toHaveBeenCalledTimes(4);
      expect(questionFold).toHaveBeenCalledTimes(2);
      expect(resetFold).toHaveBeenCalledTimes(1);
      expect(childFold).toHaveBeenCalledTimes(4);
      expect(content).toHaveBeenCalledTimes(2);
      expect(end).toHaveBeenCalledTimes(1);
      expect(completed).toHaveBeenCalledTimes(1);
      expect(rosterEnd).toHaveBeenCalledTimes(2);
      expect(questionEnd).toHaveBeenCalledTimes(2);
    } finally { vi.restoreAllMocks(); }
  });

  it("replaces each concern through its snapshot hook", () => {
    const spies = [roster322, notices322, subagents322, questions322, reset322].map((module) => vi.spyOn(module, "fromSnapshot"));
    try {
      const before = events.reduce(applyEvent, initialState());
      const empty = initialState();
      const after = applySnapshot(before, empty, 0);
      for (const spy of spies) expect(spy).toHaveBeenCalledTimes(1);
      expect(after.transcript).toEqual([]);
      expect(after.notices).toEqual([]);
      expect(after.questions).toEqual([]);
    } finally { vi.restoreAllMocks(); }
  });
});

describe("session notices match the Rust snapshot", () => {
  const cases = sessionNotices.cases as unknown as {
    name: string; events: { events: AgentDomainEvent[] };
    snapshot: { state: AgentUiSnapshot; throughRevision: number };
  }[];
  for (const c of cases) {
    it(c.name, () => {
      const folded = c.events.events.reduce(applyEvent, initialState());
      const loaded = applySnapshot(initialState(), c.snapshot.state, c.snapshot.throughRevision);
      for (const key of ["notices", "apiRetry", "compacting", "rateLimitWarning", "fallbackModel", "notification",
        "transcript", "assistantMessageOpen", "thinkingOpen", "accountLimits", "turnEndings", "nextSeq"] as const) {
        expect(folded[key]).toEqual(loaded[key]);
      }
      if (["informational_info", "peer_released", "rate_allowed", "rate_allowed_warning", "retry", "notification", "compacting", "rate_rejected"].includes(c.name)) {
        expect(folded.notices).toEqual([]);
      }
      if (c.name === "rate_rejected") expect(folded.turnEndings[0].resetsAt).toBe(1800000123);
      if (c.name === "quiet_preserves_stream") expect(folded.transcript.map((m) => m.text)).toEqual(["beforeafter"]);
      if (c.name.startsWith("retry_cleared")) expect(folded.apiRetry).toBeNull();
      const replaced = applySnapshot(folded, initialState(), 0);
      expect(replaced.apiRetry).toBeNull();
      expect(replaced.compacting).toBe(false);
      expect(replaced.rateLimitWarning).toBeNull();
      expect(replaced.fallbackModel).toBeNull();
      expect(replaced.notification).toBeNull();
    });
  }
});

describe("/clear starts a new conversation in the same tab", () => {
  const cases = conversationReset.cases as unknown as {
    name: string; domain_events: AgentDomainEvent[]; events: { events: AgentDomainEvent[] };
    snapshot: { state: AgentUiSnapshot; throughRevision: number };
  }[];
  const byName = (name: string) => {
    const c = cases.find((c) => c.name === name);
    if (!c) throw new Error(`no case ${name}`);
    return c;
  };

  for (const c of cases) {
    it(`${c.name}: the event fold equals the Rust snapshot`, () => {
      const folded = c.events.events.reduce(applyEvent, initialState());
      const loaded = applySnapshot(initialState(), c.snapshot.state, c.snapshot.throughRevision);
      for (const key of ["providerSessionId", "userPrompts", "transcript", "toolCalls", "pendingPermissions", "activeTurnId",
        "status", "backgroundCalls", "turnEndings", "lastTurnEnding", "turnStarts", "notices", "questions", "usage",
        "history", "thinking", "thinkingOpen", "assistantMessageOpen", "nextSeq"] as const) {
        expect(folded[key], key).toEqual(loaded[key]);
      }
      expect(c.events.events).toEqual(c.domain_events);
    });
  }

  it("keeps the count of running commands and a waiting card, and nothing of the conversation", () => {
    const state = byName("shell_count_and_card_survive_the_reset").events.events.reduce(applyEvent, initialState());
    expect(state.providerSessionId).toBe("sess-new");
    expect(state.backgroundCalls).toBe(1);
    expect(state.pendingPermissions.map((p) => p.permissionId)).toEqual(["perm-1"]);
    expect(state.toolCalls.map((c) => c.toolUseId)).toEqual(["tu-card"]);
    expect(state.userPrompts).toEqual([]);
    expect(state.transcript).toEqual([]);
    expect(state.notices).toEqual([]);
    expect(state.activeTurnId).toBeNull();
  });

  it("shows only what came after the reset", () => {
    const state = byName("a_turn_after_the_reset_is_the_whole_conversation").events.events.reduce(applyEvent, initialState());
    expect(state.userPrompts.map((p) => p.text)).toEqual(["after"]);
    expect(state.transcript.map((m) => m.text)).toEqual(["after reply"]);
    expect(state.toolCalls).toEqual([]);
  });

  it("leaves the running turn and the held state to the process", () => {
    const events = byName("shell_count_and_card_survive_the_reset").domain_events;
    const at = events.findIndex((e) => e.type === "conversation_reset");
    const before = events.slice(0, at).reduce(applyEvent, initialState());
    const after = applyEvent(before, events[at]);
    expect(after.activeTurnId).toBe(before.activeTurnId);
    expect(after.activeTurnId).not.toBeNull();
    expect(after.sessionId).toBe(before.sessionId);
    expect(after.nextSeq).toBe(before.nextSeq + 1);
  });
});

describe("question states from Rust", () => {
  for (const testCase of questionsFixture.cases) {
    it(testCase.name, () => {
      let state = initialState();
      let reloaded = initialState();
      for (const step of testCase.steps) {
        const action = step.action as { event?: AgentDomainEvent; send?: string; interactionId?: string };
        const apply = (before: AgentUiState) => action.event ? applyEvent(before, action.event)
          : action.send === "answering" ? questions322.markQuestionAnswering(before, action.interactionId!)
          : questions322.questionSendFailed(before, action.interactionId!);
        state = apply(state);
        reloaded = apply(reloaded);
        expect(state.questions).toEqual(step.snapshot.questions ?? []);
        expect(reloaded.questions).toEqual(state.questions);
        expect(state.nextSeq).toBe(step.throughRevision);
        expect(questions322.waitingQuestions(state).map((q) => q.interactionId)).toEqual(step.waitingIds);
        reloaded = applySnapshot(initialState(), { ...initialState(), ...step.snapshot } as AgentUiSnapshot, step.throughRevision);
        expect(reloaded.questions).toEqual(state.questions);
      }
    });
  }
});

describe("plan states from Rust", () => {
  for (const testCase of plansFixture.cases) {
    it(testCase.name, () => {
      let state = initialState();
      for (const step of testCase.steps) {
        const action = step.action as { event?: AgentDomainEvent; send?: string; interactionId?: string };
        state = action.event ? applyEvent(state, action.event)
          : { ...state, plans: state.plans.map((record) => record.interactionId === action.interactionId
            && record.state.kind === (action.send === "answering" ? "waiting" : "answering")
            ? { ...record, state: { kind: action.send === "answering" ? "answering" as const : "waiting" as const } } : record) };
        // The records are the page's own fold; the phase is the host's, read from the snapshot.
        const snapshot = step.snapshot as { plans?: unknown[]; planning: { phase: string } };
        expect(state.plans).toEqual(snapshot.plans ?? []);
        expect(state.nextSeq).toBe(step.throughRevision);
        expect(plans326.waitingPlan(state)?.interactionId ?? null).toBe(step.waitingId);
        const reloaded = applySnapshot(initialState(), { ...initialState(), ...snapshot } as unknown as AgentUiSnapshot, step.throughRevision);
        expect(reloaded.plans).toEqual(state.plans);
        expect(reloaded.planning).toEqual({ phase: snapshot.planning.phase, origin: null });
        expect(reloaded.planning!.phase !== "executing").toBe(step.isPlanning);
      }
    });
  }
  it("the envelope's planningChanged sets plan mode and places page-only rows that a snapshot drops", () => {
    let state = applyEvent(initialState(), { type: "turn_started", turn_id: "t1" });
    state = plans326.withPlanning(state, { phase: "entering", origin: "auto" });
    expect(state.planMarks).toEqual([]);
    state = plans326.withPlanning(state, { phase: "planning", origin: "auto" });
    expect(state.planning).toEqual({ phase: "planning", origin: "auto" });
    expect(state.planMarks).toEqual([{ seq: 0.5, kind: "entered", key: "pm-1-entered" }]);
    expect(plans326.withPlanning(state, undefined)).toBe(state);
    state = plans326.withPlanning(state, { phase: "leaving", origin: "auto" });
    state = plans326.withPlanning(state, { phase: "executing", origin: null });
    expect(state.planMarks.map((m) => m.kind)).toEqual(["entered", "left"]);
    expect(plans326.withPlanning(state, null).planning).toBeNull();
    expect(applySnapshot(state, { ...initialState() } as unknown as AgentUiSnapshot, 1).planMarks).toEqual([]);
  });
});

describe("turns Claude Code started, a prompt read after work of its own, endings without a result (cli-turns.json)", () => {
  type Case = {
    name: string;
    split: number;
    events: { events: AgentDomainEvent[]; throughRevision: number };
    middle: { state: AgentUiSnapshot; throughRevision: number };
    snapshot: { state: AgentUiSnapshot; throughRevision: number };
  };
  const cases = cliTurns.cases as unknown as Case[];
  const byName = (name: string) => {
    const found = cases.find((c) => c.name === name);
    if (found === undefined) throw new Error(`no case ${name}`);
    return found;
  };
  const folded = (name: string) => byName(name).events.events.reduce(applyEvent, initialState());
  const keys = [
    "activeTurnId",
    "userPrompts",
    "transcript",
    "toolCalls",
    "turnStarts",
    "turnEndings",
    "lastTurnEnding",
    "notices",
    "openingPromptSeq",
    "runningTurnCli",
    "assistantMessageOpen",
    "nextSeq",
  ] as const;

  for (const c of cases) {
    it(`${c.name}: event by event, as a batch, from the middle snapshot and from the last one, the same page`, () => {
      const loaded = applySnapshot(initialState(), c.snapshot.state, c.snapshot.throughRevision);
      const fromMiddle = applyEvents(applySnapshot(initialState(), c.middle.state, c.middle.throughRevision), c.events.events.slice(c.split));
      for (const state of [c.events.events.reduce(applyEvent, initialState()), applyEvents(initialState(), c.events.events), fromMiddle]) {
        for (const key of keys) expect(state[key], `${c.name}: ${key}`).toEqual(loaded[key]);
      }
      expect(loaded.nextSeq).toBe(c.snapshot.throughRevision);
    });
  }

  it("a turn Claude Code started is a turn: it runs, and its start says why", () => {
    const c = byName("task_cause");
    const mid = applySnapshot(initialState(), c.middle.state, c.middle.throughRevision);
    expect(mid.activeTurnId).toBe("c1");
    expect(mid.runningTurnCli).toBe(true);
    const state = folded("task_cause");
    expect(state.turnStarts.map((r) => [r.turnId, r.origin, r.cause?.kind, r.cause?.description])).toEqual([["c1", "cli", "task", "cargo test"]]);
  });

  it("the cause the close named replaces the start's unknown one; an update with none changes nothing", () => {
    expect(folded("unknown_cause_updated_at_the_close").turnStarts.map((r) => r.cause?.kind)).toEqual(["task"]);
  });

  it("scheduled causes keep their reason or its absence, and three empty scheduled turns keep three rows", () => {
    expect(folded("scheduled_with_a_reason").turnStarts[0].cause?.scheduleReason).toBe("check the deploy");
    expect(folded("scheduled_without_a_reason").turnStarts[0].cause?.scheduleReason).toBeNull();
    const three = folded("three_empty_scheduled_turns");
    expect(three.turnStarts.map((r) => r.turnId)).toEqual(["c1", "c2", "c3"]);
    expect(three.turnEndings).toEqual([]);
  });

  it("a peer's cause carries its name and its body as given", () => {
    const cause = folded("peer_with_its_body").turnStarts[0].cause;
    expect(cause?.peerName).toBe("reviewer");
    expect(cause?.text).toBe("Please look at \u202eline 4\u200b");
  });

  it("a prompt read after work of its own leaves the conversation at the cycle and comes back where it was read", () => {
    const c = byName("prompt_read_after_work_of_its_own");
    const events = c.events.events;
    const cycle = events.findIndex((e) => e.type === "cli_cycle_started");
    const atCycle = events.slice(0, cycle + 1).reduce(applyEvent, initialState());
    expect(atCycle.userPrompts.map((p) => p.text)).toEqual(["fix the parser"]);
    expect(atCycle.openingPromptSeq).toBeNull();
    expect(atCycle.turnStarts.map((r) => [r.turnId, r.cause?.kind])).toEqual([["t2", "task"]]);
    const state = folded(c.name);
    expect(state.userPrompts.map((p) => p.text)).toEqual(["fix the parser", "and the docs"]);
    const woken = state.transcript.find((m) => m.text === "The build finished.");
    const read = state.userPrompts[1];
    expect(read.delivered).toBe(true);
    expect(read.seq).toBeGreaterThan(woken?.seq ?? Infinity);
  });

  it("a prompt dropped after work of its own is not in the conversation", () => {
    expect(folded("prompt_dropped_after_work_of_its_own").userPrompts.map((p) => p.text)).toEqual(["fix the parser"]);
  });

  it("an ending without a result keeps its row, its origin and its count; an empty one leaves nothing", () => {
    const kept = folded("no_result_after_a_reply");
    expect(kept.turnEndings.map((e) => [e.kind, e.reason, e.originKind, e.numTurns])).toEqual([["failed", "no_result", "task-notification", 1]]);
    expect(kept.turnStarts).toHaveLength(1);
    const empty = folded("empty_no_result");
    expect(empty.turnStarts).toEqual([]);
    expect(empty.turnEndings).toEqual([]);
    expect(empty.lastTurnEnding).toBeNull();
    expect(empty.activeTurnId).toBeNull();
  });
});

describe("background task reconciliation", () => {
  for (const c of backgroundTasksFixture.cases) {
    it(c.name, () => {
      const test = c as unknown as { events: { events: AgentDomainEvent[] }; batches: { events: AgentDomainEvent[] }[];
        snapshots: { state: AgentUiSnapshot; throughRevision: number }[]; expected_running: number[];
        expected_rows: Record<string, unknown>[]; expected_notice_count: number; expected_prefix_rows?: Record<string, Record<string, unknown>[]> };
      let state = initialState();
      for (const [index, batch] of test.batches.entries()) {
        const previous = state;
        state = applyEvents(state, batch.events);
        const snapshot = test.snapshots[index];
        const restored = applySnapshot(initialState(), snapshot.state, snapshot.throughRevision);
        expect(state.backgroundTasks).toEqual(restored.backgroundTasks);
        expect(state.backgroundCalls).toBe(test.expected_running[index]);
        expect(state.backgroundTasks.rows.filter((r) => r.live && !r.ambient).length).toBe(state.backgroundCalls);
        expect(state.notices).toEqual(restored.notices);
        test.expected_prefix_rows?.[index]?.forEach((row, i) => expect(state.backgroundTasks.rows[i]).toMatchObject(row));
        // Resync after every prefix must preserve the next fold, including the pending query reset.
        const resumed = applyEvents(applySnapshot(previous, index === 0 ? initialState() : test.snapshots[index - 1].state,
          index === 0 ? 0 : test.snapshots[index - 1].throughRevision), batch.events);
        for (const key of ["backgroundTasks", "backgroundCalls", "notices", "nextSeq"] as const) {
          expect(resumed[key], key).toEqual(state[key]);
        }
      }
      expect(applyEvents(initialState(), test.events.events)).toEqual(state);
      expect(test.events.events.reduce(applyEvent, initialState())).toEqual(state);
      expect(state.notices).toHaveLength(test.expected_notice_count);
      expect(state.backgroundTasks.rows).toHaveLength(test.expected_rows.length);
      test.expected_rows.forEach((row, index) => expect(state.backgroundTasks.rows[index]).toMatchObject(row));
      expect(applySnapshot(state, initialState(), 0).backgroundTasks.rows).toEqual([]);
    });
  }
});


describe("nested subagent streams", () => {
  for (const fixture of subagentFrames.cases) {
    it(fixture.name, () => {
      const events = fixture.events as AgentDomainEvent[];
      let state = initialState();
      for (const event of events) {
        const before = state;
        state = applyEvent(state, event);
        if (event.type === "subagent_frame") {
          const { toolCalls: _beforeCalls, nextSeq: _beforeSeq, denialsBeforeTheirCall: _beforeDenials,
            changedInputsBeforeTheirCall: _beforeChanges, ...beforeMain } = before;
          const { toolCalls: _afterCalls, nextSeq: _afterSeq, denialsBeforeTheirCall: _afterDenials,
            changedInputsBeforeTheirCall: _afterChanges, ...afterMain } = state;
          expect(afterMain).toEqual(beforeMain);
          expect(phaseOf(state)).toEqual(phaseOf(before));
        }
      }
      const snapshot = fixture.snapshot;
      const restored = applySnapshot(initialState(), snapshot.state as unknown as AgentUiSnapshot, snapshot.throughRevision);
      for (const folded of [state, applyEvents(initialState(), events)]) {
        for (const key of ["toolCalls", "transcript", "thinking", "activeTurnId", "assistantMessageOpen", "thinkingOpen", "pendingPermissions", "nextSeq"] as const) {
          expect(folded[key], key).toEqual(restored[key]);
        }
        const next: AgentDomainEvent[] = [
          { type: "content_delta", turn_id: state.activeTurnId ?? "main", kind: "text", text: " more main" },
          { type: "subagent_frame", parent_tool_use_id: state.toolCalls[0].toolUseId,
            frame: { type: "content_delta", turn_id: "main", kind: "text", text: " more child" } },
        ];
        const continued = applyEvents(folded, next);
        const reloaded = applyEvents(restored, next);
        expect(continued.toolCalls).toEqual(reloaded.toolCalls);
        expect(continued.transcript).toEqual(reloaded.transcript);
      }
    });
  }

  it("keeps adjacent main and child deltas separate during batching", () => {
    const fixture = subagentFrames.cases.find((entry) => entry.name === "foreground children")!;
    const state = applyEvents(initialState(), fixture.events as AgentDomainEvent[]);
    expect(state.transcript.map((row) => row.text)).toEqual(["main reply"]);
    const parent = state.toolCalls[0];
    expect(parent.children?.transcript.map((row) => row.text)).toEqual(["child reply"]);
    expect(parent.children?.toolCalls.map((call) => call.toolUseId)).toEqual(["child"]);
    expect(state.toolCalls).toHaveLength(1);
  });
});

describe("event fold revisions", () => {
  it("assigns a reply the Rust seq after a dropped request", () => {
    const events: AgentDomainEvent[] = [
      { type: "turn_started", turn_id: "t1" },
      { type: "content_delta", turn_id: "t1", kind: "text", text: "reply" },
    ];
    expect(applyEvents(initialState(), events, [1, 3]).transcript[0].seq).toBe(2);
    expect(applyEvents(initialState(), events).transcript[0].seq).toBe(1);
  });

  it("does not merge text or thinking deltas across a revision gap", () => {
    for (const kind of ["text", "thinking"] as const) {
      const events: AgentDomainEvent[] = [
        { type: "content_delta", turn_id: "t1", kind, text: "" },
        { type: "content_delta", turn_id: "t1", kind, text: "reply" },
      ];
      const state = applyEvents(initialState(), events, [2, 5]);
      expect(state.nextSeq).toBe(5);
      if (kind === "thinking") expect(state.thinking[0].seq).toBe(4);
      else expect(state.transcript[0].seq).toBe(1);
    }
  });

  it("never rewinds the sequence for stale tags", () => {
    const state = applyEvents({ ...initialState(), nextSeq: 10 }, [
      { type: "content_delta", turn_id: "t1", kind: "text", text: "reply" },
    ], [3]);
    expect(state.transcript[0].seq).toBe(10);
    expect(state.nextSeq).toBe(11);
  });
});

describe("broker cards", () => {
  const wires = editorCardsFixture as EditorCardWire[];

  it("the_card_fixture_folds_as_rust_serialises_it", () => {
    const snapshot = { ...initialState(), editorCards: wires } as unknown as AgentUiSnapshot;
    const state = applySnapshot(initialState(), snapshot, 50);
    expect(state.editorCards.map((card) => [card.permissionId, card.seq, card.toolName, card.toolUseId])).toEqual([
      ["ec-0123456789abcdef-1", 41, "open_file", "toolu_01"],
      ["ec-0123456789abcdef-2", 42, "open_file", "toolu_02"],
      ["ec-0123456789abcdef-3", 43, "open_file", null],
      ["ec-0123456789abcdef-4", 44, "open_file", null],
      ["ec-0123456789abcdef-5", 45, "apply_proposal", "toolu_05"],
    ]);
    expect(state.editorCards.map((card) => card.editorCard)).toEqual(
      wires.map((wire) => ({ title: wire.title, body: wire.body, ...(wire.proposal ? { proposal: wire.proposal } : {}) })),
    );
    expect(state.editorCards.map((card) => card.editorCard!.body[0].kind)).toEqual(["plain", "code", "agent", "warning", "plain"]);
    // A card is never a provider's request and never takes the page's counter.
    expect(state.pendingPermissions).toEqual([]);
    expect(state.nextSeq).toBe(50);
    expect(withEditorCards(state, []).editorCards).toEqual([]);
    expect(withEditorCards(state, wires.slice(1)).nextSeq).toBe(50);
  });

  it("a snapshot without the key is a tab with none, and one that cannot be a list is too", () => {
    expect(applySnapshot(initialState(), { ...initialState() } as unknown as AgentUiSnapshot, 0).editorCards).toEqual([]);
    expect(applySnapshot(initialState(), { ...initialState(), editorCards: "x" } as unknown as AgentUiSnapshot, 0).editorCards).toEqual([]);
  });

  it("a snapshot drops a card the view cannot draw and keeps the rest", () => {
    const good = wires[0];
    const bad = { ...wires[1], body: "not a list" };
    const state = applySnapshot(initialState(), { ...initialState(), editorCards: [good, bad, null, 7] } as unknown as AgentUiSnapshot, 0);
    expect(state.editorCards.map((c) => c.permissionId)).toEqual([good.id]);
  });

  it("a conversation reset keeps a waiting broker card, and the call goes as Rust's projection drops it", () => {
    const card = wires[0];
    let state = withEditorCards(initialState(), [card]);
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: card.toolUseId!, name: "mcp__eitri__open_file", input: {} });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_other", name: "Read", input: {} });
    state = applyEvent(state, { type: "conversation_reset", new_conversation_id: "c2", trigger: null });
    expect(state.editorCards.map((c) => c.permissionId)).toEqual([card.id]);
    expect(state.toolCalls).toEqual([]);
  });

  it("a lost session keeps no card, and the start screen none", () => {
    const state = withEditorCards({ ...initialState(), userPrompts: [{ seq: 0, text: "hi" }] }, wires);
    expect(keepAfterFailure(state, "gone")!.editorCards).toEqual([]);
    expect(resetToStartScreen(state).editorCards).toEqual([]);
  });
});

it("call_notes_reducer_keeps_sent_from", () => {
  const phone = { deviceId: "d1", name: "Pixel 8" };
  const state = applyCallNotes(initialState(), { sentFrom: { "3": phone }, stoppedFrom: { t1: phone } });
  expect(state.sentFrom).toEqual({ "3": phone });
  expect(applyCallNotes(state, {}).stoppedFrom).toEqual({ t1: phone });
  expect(applySnapshot(state, initialState(), 0).sentFrom ?? {}).toEqual({});
});

it("conversation_reset_clears_phone_notes", () => {
  const phone = { deviceId: "d1", name: "Pixel 8" };
  const state = applyCallNotes(initialState(), { sentFrom: { "3": phone }, stoppedFrom: { t1: phone } });
  const cleared = applyEvent(state, { type: "conversation_reset", new_conversation_id: "new", trigger: null });
  expect(cleared.sentFrom).toEqual({});
  expect(cleared.stoppedFrom).toEqual({});
});

it("the newest unsettled plan is docked regardless of array order", () => {
  let state = initialState();
  for (const interaction_id of ["old", "new"]) state = applyEvent(state, {
    type: "plan_proposed", interaction_id, tool_use_id: null, plan: "# Plan", plan_sha256: "a".repeat(64), allowed_prompts: [], unsupported_reason: null,
  });
  expect(plans326.waitingPlan(state)?.interactionId).toBe("new");
  expect(plans326.waitingPlan({ plans: [...state.plans].reverse() })?.interactionId).toBe("new");
  state = applyEvent(state, { type: "plan_settled", interaction_id: "new", outcome: "interrupted", mode_after: null, feedback: null, reason: null });
  expect(plans326.waitingPlan(state)?.interactionId).toBe("old");
});

it("images sent with a prompt fold onto the newest prompt not put into a turn, as Rust folds them", () => {
  const png = { media_type: "image/png", bytes: 302, width: 48, height: 32, sha256: "ab".repeat(32) };
  const events: AgentDomainEvent[] = [
    { type: "user_prompt_submitted", text: "first" },
    { type: "user_prompt_submitted", text: "" },
    { type: "user_prompt_attachments", attachments: [png, { ...png, media_type: "image/jpeg", width: 1429, height: 804 }] },
  ];
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  const onUnknown = vi.fn();
  const state = events.reduce((s, e) => applyEvent(s, e, onUnknown), initialState());
  expect(onUnknown).not.toHaveBeenCalled();
  expect(warn).not.toHaveBeenCalled();
  warn.mockRestore();
  expect(state.userPrompts[0]).toEqual({ seq: 0, text: "first" });
  expect(state.userPrompts[1]).toEqual({
    seq: 1, text: "",
    attachments: [
      { mediaType: "image/png", width: 48, height: 32, bytes: 302 },
      { mediaType: "image/jpeg", width: 1429, height: 804, bytes: 302 },
    ],
  });
  expect(buildTimeline(state).find((item) => item.kind === "prompt" && item.seq === 1)).toMatchObject({
    images: "[2 images: PNG 48×32, JPEG 1429×804]",
  });
  // With no prompt to take them, nothing changes.
  const empty = applyEvent(initialState(), events[2]);
  expect(empty.userPrompts).toEqual([]);
});
