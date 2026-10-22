import { describe, expect, it } from "vitest";
import { applyEvent, applySnapshot, initialState } from "./reducer";
import { phaseOf, phaseWord } from "./turnPhase";

describe("phaseOf", () => {
  it("reads sent when nothing has come back yet", () => {
    const state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    expect(phaseOf(state)).toEqual({ kind: "sent" });
    expect(phaseWord(phaseOf(state))).toBe("working");
  });

  it("reads thinking once a thinking delta has arrived and nothing has landed since", () => {
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    expect(phaseOf(state)).toEqual({ kind: "thinking" });
    expect(phaseWord(phaseOf(state))).toBe("thinking");
  });

  it("reads replying once the last item is an assistant message", () => {
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "hello" });
    expect(phaseOf(state)).toEqual({ kind: "replying" });
  });

  it("reads running <Tool> while the last item is a tool call with no result", () => {
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    expect(phaseOf(state)).toEqual({ kind: "tool", toolName: "Bash" });
    expect(phaseWord(phaseOf(state))).toBe("running Bash");
  });

  it("reads blocked while a permission is pending, even mid-tool-call", () => {
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    state = applyEvent(state, { type: "permission_requested", permission_id: "p1", tool_use_id: "tu1", tool_name: "Bash", input: {} });
    expect(phaseOf(state)).toEqual({ kind: "blocked" });
    expect(phaseWord(phaseOf(state))).toBe("waiting for you");
  });

  it("rule 2: a denied tool call (result stays null forever) does not stick as 'tool' once text follows", () => {
    // `tool_call_started` clears `assistantMessageOpen`, so the model's next text opens a NEW,
    // higher-seq transcript entry -- exactly what a denial leaves behind, since a denied call's
    // `result` is never set (agent/src/projection.rs's PermissionResolved arm records no verdict).
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    // No tool_call_completed ever arrives for tu1 -- this is what a denial looks like on the wire.
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "never mind, let me try something else" });
    expect(phaseOf(state)).toEqual({ kind: "replying" });
  });

  /* An unfinished call stays current when a later sibling finishes
     Rule 2 used to require `openTool.seq === lastItemSeq`. The four tests below are the four cases
     that settle the amendment; the first three were each reproduced RED against that qualifier
     before it was replaced, and the fourth is the regression guard that the replacement did not
     break the ordinary path. */

  it("names the newest UNFINISHED call when a sibling that started later finishes first", () => {
    // Reproduced against the old qualifier: B's record was the highest-seq ITEM, so A -- genuinely
    // running -- failed `openTool.seq === lastItemSeq` and the phase read `sent`, whose own
    // definition is "nothing has come back yet". A sibling completing is not evidence about A.
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "a", name: "Bash", input: {} });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "b", name: "Read", input: {} });
    state = applyEvent(state, { type: "tool_call_completed", turn_id: "t1", tool_use_id: "b", content: "ok", is_error: false });
    expect(phaseOf(state)).toEqual({ kind: "tool", toolName: "Bash" });
    expect(phaseWord(phaseOf(state))).toBe("running Bash");
  });

  it("a newer tool call does not displace the phase, it renames it to the newest unfinished one", () => {
    // The deliberate asymmetry: TEXT and THINKING displace `tool`, a second tool call does not --
    // two calls in flight means two things are happening, not that the first one stopped.
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "a", name: "Bash", input: {} });
    expect(phaseOf(state)).toEqual({ kind: "tool", toolName: "Bash" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "b", name: "Read", input: {} });
    expect(phaseOf(state)).toEqual({ kind: "tool", toolName: "Read" });
  });

  it("a DENIED call leaves 'running <Tool>' as soon as the model thinks", () => {
    // Reproduced against the old qualifier: `permission_resolved` creates no item, so the tool call
    // was still `lastItemSeq` and rule 2 -- which sat ahead of rule 3 -- kept matching. The panel
    // animated "running Bash" for a call the user had just REFUSED, for as long as the model
    // thought about what to do instead.
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    state = applyEvent(state, { type: "permission_requested", permission_id: "p1", tool_use_id: "tu1", tool_name: "Bash", input: {} });
    expect(phaseOf(state)).toEqual({ kind: "blocked" });
    state = applyEvent(state, { type: "permission_resolved", permission_id: "p1", outcome: "denied" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "denied -- what else can I do" });
    expect(phaseOf(state)).toEqual({ kind: "thinking" });
  });

  it("a DENIED call leaves 'running <Tool>' as soon as the model answers", () => {
    // The other half of the same case. This one the old qualifier already handled (text opens a
    // new, higher-seq transcript entry), so it is a guard on the amendment rather than a repro:
    // the replacement clause must keep answering it the same way.
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    state = applyEvent(state, { type: "permission_requested", permission_id: "p1", tool_use_id: "tu1", tool_name: "Bash", input: {} });
    state = applyEvent(state, { type: "permission_resolved", permission_id: "p1", outcome: "denied" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "I cannot run that, so instead" });
    expect(phaseOf(state)).toEqual({ kind: "replying" });
  });

  it("an ordinary single call reads 'running <Tool>' from start to result, approval included", () => {
    // The regression guard for the amendment: the common path must be unchanged. Note the middle
    // two steps -- a permission asked and ALLOWED -- are events that create no item either, so
    // before and after them the phase has to be the same words on the screen.
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    expect(phaseOf(state)).toEqual({ kind: "tool", toolName: "Bash" });
    state = applyEvent(state, { type: "permission_requested", permission_id: "p1", tool_use_id: "tu1", tool_name: "Bash", input: {} });
    expect(phaseOf(state)).toEqual({ kind: "blocked" });
    state = applyEvent(state, { type: "permission_resolved", permission_id: "p1", outcome: "allowed" });
    expect(phaseOf(state)).toEqual({ kind: "tool", toolName: "Bash" });
    state = applyEvent(state, { type: "tool_call_completed", turn_id: "t1", tool_use_id: "tu1", content: "ok", is_error: false });
    expect(phaseOf(state).kind).not.toBe("tool");
  });

  /* A new PROMPT displaces the previous turn's tool call
     The first amendment dropped `lastItemSeq` outright, which left a call that never completed --
     an interrupt or a denial, both of which leave `result: null` FOREVER -- as "the newest
     unfinished call" into the following turn. `toolCalls` is never cleared between turns, and a
     user prompt is not assistant text, so nothing displaced it. The three tests below are that
     defect's three shapes; each was checked RED against the code before `lastPromptSeq` was added,
     reporting `{kind: "tool", toolName: ...}` where it now reports `sent`. */

  it("an INTERRUPTED call does not claim 'running <Tool>' into the next turn", () => {
    // After an interrupt the model may emit nothing further, so
    // no text ever lands after the call inside its own turn -- `lastMessageSeq` stays below it for
    // good, and only the new prompt can displace it. `ActivityLine` renders `running` from the
    // instant `turn_started` lands, so without this the meter animates "running NotebookEdit" for
    // the whole round trip between Enter and the model's first delta -- naming a tool the user
    // personally stopped, in exactly the window `sent` exists to name.
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "edit the notebook" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "let me look" });
    state = applyEvent(state, {
      type: "tool_call_started",
      turn_id: "t1",
      tool_use_id: "tu1",
      name: "NotebookEdit",
      input: {},
    });
    expect(phaseOf(state)).toEqual({ kind: "tool", toolName: "NotebookEdit" });
    // The interrupt: the turn ends and no result for tu1 ever arrives.
    state = applyEvent(state, {
      type: "turn_completed",
      turn_id: "t1",
      outcome: "interrupted",
      result_text: "",
      stop_reason: null,
      usage: null,
      detail: { reason: null, api_error_status: null, message: null },
    });
    state = applyEvent(state, { type: "user_prompt_submitted", text: "never mind, do X" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t2" });
    expect(phaseOf(state)).toEqual({ kind: "sent" });
    expect(phaseWord(phaseOf(state))).toBe("working");
  });

  it("a DENIED call does not claim 'running <Tool>' into the next turn", () => {
    // The same defect by the other route to `result: null`. Here not even a first delta precedes
    // the call, so `lastMessageSeq` is -1 throughout and the prompt is the only displacer there is.
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "run the build" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    state = applyEvent(state, {
      type: "permission_requested",
      permission_id: "p1",
      tool_use_id: "tu1",
      tool_name: "Bash",
      input: {},
    });
    state = applyEvent(state, { type: "permission_resolved", permission_id: "p1", outcome: "denied" });
    state = applyEvent(state, {
      type: "turn_completed",
      turn_id: "t1",
      outcome: "completed",
      result_text: "",
      stop_reason: null,
      usage: null,
      detail: { reason: null, api_error_status: null, message: null },
    });
    state = applyEvent(state, { type: "user_prompt_submitted", text: "do X instead" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t2" });
    expect(phaseOf(state)).toEqual({ kind: "sent" });
  });

  it("a resync landing in that stale state reads sent too, not the stale call", () => {
    // The seq comparison has to survive a snapshot, because a resync is where every ephemeral bit
    // is lost -- if `lastPromptSeq` needed anything reducer-internal this would be the test that
    // failed. It does not: `userPrompts` is a real projection collection carrying real seqs, so
    // the rule reads the same before and after.
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "run the build" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    state = applyEvent(state, {
      type: "turn_completed",
      turn_id: "t1",
      outcome: "interrupted",
      result_text: "",
      stop_reason: null,
      usage: null,
      detail: { reason: null, api_error_status: null, message: null },
    });
    state = applyEvent(state, { type: "user_prompt_submitted", text: "never mind, do X" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t2" });

    // `assistantMessageOpen` stays on `wire`, as Rust now sends it: `turn_started`
    // just above already closed it on `state` itself, so this is the wire's true value, not a
    // stripped one -- `phaseOf` does not read this field regardless.
    const { nextSeq: _n, turnThinking: _t, ...wire } = state;
    const resynced = applySnapshot(state, wire, state.nextSeq);
    expect(phaseOf(resynced)).toEqual({ kind: "sent" });
  });

  it("rule 3 before rule 4: thinking resumes after text has already streamed", () => {
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "text", text: "let me check" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    state = applyEvent(state, { type: "tool_call_completed", turn_id: "t1", tool_use_id: "tu1", content: "ok", is_error: false });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm, now what" });
    // The last item `phaseOf` reads is still the completed tool call (it reads prompts, replies and calls,
    // not thinking blocks), but the phase must
    // report thinking, not replying -- rule 3 runs before rule 4 for exactly this reason.
    expect(phaseOf(state)).toEqual({ kind: "thinking" });
  });

  it("a completed tool call whose result truly is the last item reads sent/replying via the fallthrough, not tool", () => {
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Read", input: {} });
    state = applyEvent(state, { type: "tool_call_completed", turn_id: "t1", tool_use_id: "tu1", content: "file contents", is_error: false });
    // The tool call is the last item, but it has a result now, so rule 2's "no result" clause
    // excludes it -- this must fall through past `tool` entirely.
    expect(phaseOf(state).kind).not.toBe("tool");
  });

  it("a resync mid-thought keeps thinking: the open thought is on the wire", () => {
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    expect(phaseOf(state)).toEqual({ kind: "thinking" });

    // The snapshot Rust would send: no `turnThinking` (reducer-internal), but `thinkingOpen`, which is
    // Rust's `thinking_open`, stays.
    const { nextSeq: _seq, turnThinking: _thinking, ...wire } = state;
    const resynced = applySnapshot(state, wire, state.nextSeq);
    expect(resynced.thinkingOpen).toBe(true);
    expect(phaseWord(phaseOf(resynced))).toBe("thinking");
  });

  it("any other event mid-thought keeps thinking (a list of commands, a count of background calls)", () => {
    let state = applyEvent(initialState(), { type: "turn_started", turn_id: "t1" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    state = applyEvent(state, { type: "background_calls", running: 0 });
    expect(state.turnThinking).toBe(false);
    expect(phaseOf(state)).toEqual({ kind: "thinking" });
    state = applyEvent(state, { type: "commands_available", commands: [{ name: "a", description: "" }] });
    expect(phaseOf(state)).toEqual({ kind: "thinking" });
    // Over an unfinished call too: the thought started after the call, so it is what is happening.
    let withCall = applyEvent(initialState(), { type: "turn_started", turn_id: "t1" });
    withCall = applyEvent(withCall, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    withCall = applyEvent(withCall, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    withCall = applyEvent(withCall, { type: "background_calls", running: 1 });
    expect(phaseOf(withCall)).toEqual({ kind: "thinking" });
    // What closes the thought ends it: a reply.
    withCall = applyEvent(withCall, { type: "content_delta", turn_id: "t1", kind: "text", text: "so" });
    expect(phaseOf(withCall)).toEqual({ kind: "replying" });
  });

  it("the resync degradation: a thinking delta with no text is lost to a resync, never invented", () => {
    // The invariant, written as a test: `turnThinking` can only ever be LOST. A delta
    // with no text opens no block, so nothing on the wire says thinking, and the phase degrades.
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "" });
    expect(phaseOf(state)).toEqual({ kind: "thinking" });
    expect(state.thinkingOpen).toBe(false);

    const { nextSeq: _seq, turnThinking: _thinking, ...wire } = state;
    const resynced = applySnapshot(state, wire, state.nextSeq);
    expect(phaseOf(resynced)).toEqual({ kind: "sent" });
  });

  it("the OTHER degraded path: a resync after an empty thinking delta over an unfinished call degrades to that call", () => {
    // A snapshot carries no record that a permission was DENIED -- a denied call and a running one are
    // both `result: null` -- so once the ephemeral `turnThinking` bit is gone, and no open thought says
    // otherwise, the phase reads the call again until the next thinking delta or the model's next
    // text arrives. The degradation is toward a LESS specific phase, never INTO `thinking`.
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "" });
    expect(phaseOf(state)).toEqual({ kind: "thinking" });

    const { nextSeq: _n, turnThinking: _t, ...wire } = state;
    const resynced = applySnapshot(state, wire, state.nextSeq);
    expect(phaseOf(resynced)).toEqual({ kind: "tool", toolName: "Bash" });
    expect(resynced.turnThinking).toBe(false);
  });
});

describe("phaseWord", () => {
  it("names all five phases, one word each", () => {
    expect(phaseWord({ kind: "sent" })).toBe("working");
    expect(phaseWord({ kind: "thinking" })).toBe("thinking");
    expect(phaseWord({ kind: "replying" })).toBe("replying");
    expect(phaseWord({ kind: "tool", toolName: "NotebookEdit" })).toBe("running NotebookEdit");
    expect(phaseWord({ kind: "blocked" })).toBe("waiting for you");
  });
});
