import { describe, expect, it } from "vitest";
import { applyEvent, applySnapshot, initialState } from "./reducer";
import { phaseOf, phaseWord } from "./turnPhase";

describe("phaseOf", () => {
  it("reads sent when nothing has come back yet", () => {
    const state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    expect(phaseOf(state)).toEqual({ kind: "sent" });
    expect(phaseWord(phaseOf(state))).toBe("sent");
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

  /* --- the amended rule 2 (whole-branch review, 2026-09-20) ------------------------------------
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

  /* --- rule 2's third displacer: a new PROMPT (re-review of 2026-09-20) -----------------------
     The first amendment dropped `lastItemSeq` outright, which left a call that never completed --
     an interrupt or a denial, both of which leave `result: null` FOREVER -- as "the newest
     unfinished call" into the following turn. `toolCalls` is never cleared between turns, and a
     user prompt is not assistant text, so nothing displaced it. The three tests below are that
     defect's three shapes; each was checked RED against the code before `lastPromptSeq` was added,
     reporting `{kind: "tool", toolName: ...}` where it now reports `sent`. */

  it("an INTERRUPTED call does not claim 'running <Tool>' into the next turn", () => {
    // The shape the re-review reproduced. After an interrupt the model emits nothing further, so
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
    });
    state = applyEvent(state, { type: "user_prompt_submitted", text: "never mind, do X" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t2" });
    expect(phaseOf(state)).toEqual({ kind: "sent" });
    expect(phaseWord(phaseOf(state))).toBe("sent");
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
    });
    state = applyEvent(state, { type: "user_prompt_submitted", text: "never mind, do X" });
    state = applyEvent(state, { type: "turn_started", turn_id: "t2" });

    // `assistantMessageOpen` stays on `wire`, as Rust now sends it (sw-panel-render-2): `turn_started`
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
    // The last ITEM is still the completed tool call (thinking creates no item), but the phase must
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

  it("the resync degradation: a resync mid-thinking degrades the phase to sent, never invents thinking", () => {
    // The invariant of design §8.2, written as a test: `turnThinking` can only ever be LOST.
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    expect(phaseOf(state)).toEqual({ kind: "thinking" });

    // Take the snapshot Rust would actually send for this state (no `turnThinking` key at all --
    // it is reducer-internal on both sides, unlike `assistantMessageOpen`, which IS on the wire
    // since sw-panel-render-2's fix and so stays in `wire` here) and resync onto it.
    const { nextSeq: _seq, turnThinking: _thinking, ...wire } = state;
    const resynced = applySnapshot(state, wire, state.nextSeq);
    expect(phaseOf(resynced)).toEqual({ kind: "sent" });
  });

  it("the OTHER degraded path: a resync mid-thinking over an unfinished call degrades to that call, not to thinking", () => {
    // The amended rule 2 has a second degradation the old one did not, and it is written down here
    // rather than only in `turnPhase.ts`'s prose. A snapshot carries no record that a permission
    // was DENIED -- a denied call and a running one are both `result: null` -- so once the
    // ephemeral `turnThinking` bit is gone there is nothing left on this side that can tell them
    // apart, and the phase reads the call again until the next thinking delta or the model's next
    // text arrives. For a live call that is correct and is the common case; for a denied one it is
    // one event's worth of the old defect, and it is not closable without a wire change.
    //
    // What this test really pins is the direction: the degradation is toward a LESS specific
    // phase, and nothing ever degrades INTO `thinking` (design §8.2's invariant).
    let state = applyEvent(initialState(), { type: "user_prompt_submitted", text: "hi" });
    state = applyEvent(state, { type: "tool_call_started", turn_id: "t1", tool_use_id: "tu1", name: "Bash", input: {} });
    state = applyEvent(state, { type: "content_delta", turn_id: "t1", kind: "thinking", text: "hmm" });
    expect(phaseOf(state)).toEqual({ kind: "thinking" });

    const { nextSeq: _n, turnThinking: _t, ...wire } = state;
    const resynced = applySnapshot(state, wire, state.nextSeq);
    expect(phaseOf(resynced)).toEqual({ kind: "tool", toolName: "Bash" });
    expect(resynced.turnThinking).toBe(false);
  });
});

describe("phaseWord", () => {
  it("names all five phases, one word each", () => {
    expect(phaseWord({ kind: "sent" })).toBe("sent");
    expect(phaseWord({ kind: "thinking" })).toBe("thinking");
    expect(phaseWord({ kind: "replying" })).toBe("replying");
    expect(phaseWord({ kind: "tool", toolName: "NotebookEdit" })).toBe("running NotebookEdit");
    expect(phaseWord({ kind: "blocked" })).toBe("waiting for you");
  });
});
