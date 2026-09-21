import { describe, expect, it } from "vitest";
import { applyEvent, applySnapshot, initialState, resetToStartScreen } from "./reducer";
import type { AgentDomainEvent, TurnOutcome } from "./types";

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
        stop_reason: "end_turn", usage: { total_cost_usd: 0.01, num_turns: 1 },
      });
      expect(state.activeTurnId).toBeNull();
    }
  });

  it("turn_completed does not change status (v2 semantics: only session_unavailable/session_closed do)", () => {
    let state = applyEvent(initialState(), { type: "session_opened", session_id: "abc", provider_session_id: "claude-abc", model: "m", cwd: "/tmp" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, {
      type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: "done",
      stop_reason: "end_turn", usage: { total_cost_usd: 0.01, num_turns: 1 },
    });
    expect(state.status).toEqual({ kind: "running" });
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
    expect(resetToStartScreen({ ...initialState(), history: { source: "neovibe_copy", restoredItems: 1, omittedItems: 0, uptoSeq: 1, sourcePath: "/x", attemptedTranscriptPath: null, fallbackReason: "transcript file not found", writerVersion: null } }).history).toBeNull();
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
    // so "`neovibe_" or "**bold" is not valid standalone markdown and the formatting breaks.
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
    state = applyEvent(state, { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: "first", stop_reason: null, usage: null });
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

  it("a snapshot resets the open-message flag rather than appending into a closed message", () => {
    let state = applyEvent(initialState(), opened);
    state = applyEvent(state, delta("streaming"));
    expect(state.assistantMessageOpen).toBe(true);
    // Built as the WIRE carries it: `serialize_snapshot_for_js` emits neither reducer-internal
    // field, so the two are stripped here rather than handed in. This test used to pass
    // `assistantMessageOpen: true` explicitly, which asserted a key Rust has never sent -- the
    // reset it checks is of the flag on `state`, not of one inside the snapshot.
    const { assistantMessageOpen: _open, nextSeq: _seq, ...wire } = { ...state, transcript: [{ seq: 0, text: "an earlier reply" }] };
    const restored = applySnapshot(state, wire, 1);
    expect(restored.assistantMessageOpen).toBe(false);
    const next = applyEvent(restored, delta("new message"));
    expect(next.transcript.map((m) => m.text)).toEqual(["an earlier reply", "new message"]);
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
      { type: "turn_completed", turn_id: "t1", outcome: "completed", result_text: "done", stop_reason: null, usage: null },
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
