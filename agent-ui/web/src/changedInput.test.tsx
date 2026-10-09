// @vitest-environment jsdom
/**
 * A call whose input, as the CLI's own prompt showed it, differs from the input the gate judged says so on its row. Rust
 * makes that comparison once (`AgentSessionProjection::derived_after`) and sends the result as a `tool_input_changed`
 * event, even when the host answered the gate request itself and the page never saw it; the page only applies it. The
 * events and the snapshot come from `fixtures/changed-input.json`, which Rust writes from its own serializer, with the
 * gate request left out as a deferral in an Auto tab leaves it.
 */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { applyEvent, applySnapshot, initialState, MAX_HELD_DENIALS } from "./reducer";
import { changedInputText, renderToolCall } from "./toolRegistry";
import type { AgentDomainEvent, AgentUiSnapshot, InputChange, ToolCallRecord } from "./types";
import fixture from "./fixtures/changed-input.json";

afterEach(cleanup);

const started = (id: string): AgentDomainEvent => ({ type: "tool_call_started", turn_id: "t", tool_use_id: id, name: "Bash", input: { command: "a" } });
const changed = (id: string, change: InputChange): AgentDomainEvent => ({ type: "tool_input_changed", tool_use_id: id, change });
const cmd = (command: string): InputChange => ({ fields: { command } });
const asked = (permission: string, id: string | null, input: unknown, fromCli: boolean): AgentDomainEvent =>
  ({
    type: "permission_requested",
    permission_id: permission,
    tool_use_id: id,
    tool_name: "Bash",
    input,
    ...(fromCli ? { provider_prompt: { reason: null, description: null, blocked_path: null, matched_ask_rule: null, unrecognized_origin: null } } : {}),
  }) as AgentDomainEvent;

describe("the changed input, applied", () => {
  it("the events Rust sends (no gate request among them) and its snapshot agree on the call's changed input", () => {
    const events = fixture.events.events as AgentDomainEvent[];
    expect(events.some((e) => e.type === "permission_requested" && !("provider_prompt" in e))).toBe(false);
    let state = initialState();
    for (const event of events) state = applyEvent(state, event);
    const rebuilt = applySnapshot(initialState(), fixture.snapshot.state as unknown as AgentUiSnapshot, fixture.snapshot.throughRevision);
    expect(state.toolCalls[0].changedInput).toEqual({ fields: { command: "rtk git push --force origin main‮" } });
    expect(rebuilt.toolCalls[0].changedInput).toEqual(state.toolCalls[0].changedInput);
  });

  it("the page compares nothing: a gate request and a different CLI prompt, with no event, mark nothing", () => {
    let state = applyEvent(initialState(), started("toolu_a"));
    state = applyEvent(state, asked("g", "toolu_a", { command: "x" }, false));
    state = applyEvent(state, asked("c", "toolu_a", { command: "y" }, true));
    expect(state.toolCalls[0].changedInput).toBeUndefined();
  });

  it("the row says so, muted, with the hidden character revealed", () => {
    const call = (fixture.snapshot.state as unknown as AgentUiSnapshot).toolCalls[0] as ToolCallRecord;
    const { container } = render(<>{renderToolCall(call)}</>);
    const note = container.querySelector(".tool-changed-input-note");
    expect(note?.classList.contains("tool-rule-note")).toBe(true);
    expect(note?.textContent).toContain("input differs from what the gate was asked");
    expect(note?.textContent).toContain("rtk git push --force origin main⟨U+202E⟩");
    expect(note?.textContent).not.toContain("‮");
    expect(note?.querySelector(".permission-card-escape")).not.toBeNull();
  });

  it("a call with no changed input says nothing", () => {
    const { container } = render(<>{renderToolCall({ seq: 0, toolUseId: "t", name: "Bash", input: { command: "a" }, result: null })}</>);
    expect(container.querySelector(".tool-changed-input-note")).toBeNull();
  });

  it("marks the first call with the id only, as Rust does", () => {
    let state = applyEvent(applyEvent(initialState(), started("toolu_a")), started("toolu_a"));
    state = applyEvent(state, changed("toolu_a", cmd("y")));
    expect(state.toolCalls.map((c) => c.changedInput)).toEqual([cmd("y"), undefined]);
  });

  it("a change that arrives ahead of its call is attached when it starts; a turn's end forgets", () => {
    let state = applyEvent(initialState(), changed("toolu_a", cmd("y")));
    expect(state.changedInputsBeforeTheirCall).toHaveLength(1);
    state = applyEvent(state, started("toolu_a"));
    expect(state.toolCalls[0].changedInput).toEqual(cmd("y"));
    expect(state.changedInputsBeforeTheirCall).toEqual([]);

    state = applyEvent(state, changed("toolu_b", cmd("y")));
    state = applyEvent(state, { type: "session_closed", reason: "bye" });
    expect(state.changedInputsBeforeTheirCall).toEqual([]);
    state = applyEvent(state, started("toolu_b"));
    expect(state.toolCalls[1].changedInput).toBeUndefined();
  });

  it("a snapshot carries the change Rust holds for a call that has not started, and the call takes it", () => {
    const snapshot = { ...(fixture.snapshot.state as unknown as AgentUiSnapshot), toolCalls: [], changedInputsBeforeTheirCall: [{ toolUseId: "toolu_1", change: cmd("rtk x") }] };
    let state = applySnapshot(initialState(), snapshot, 7);
    expect(state.changedInputsBeforeTheirCall).toHaveLength(1);
    state = applyEvent(state, started("toolu_1"));
    expect(state.toolCalls[0].changedInput).toEqual(cmd("rtk x"));
    // A snapshot without the key (nothing held) leaves nothing held.
    const bare = applySnapshot(state, fixture.snapshot.state as unknown as AgentUiSnapshot, 8);
    expect(bare.changedInputsBeforeTheirCall).toEqual([]);
  });

  it("a turn's end forgets a held change", () => {
    let state = applyEvent(initialState(), changed("toolu_a", cmd("y")));
    state = applyEvent(state, { type: "turn_completed", turn_id: "t", outcome: "completed", result_text: "", stop_reason: null, usage: null, detail: { reason: null, api_error_status: null, message: null } } as AgentDomainEvent);
    expect(state.changedInputsBeforeTheirCall).toEqual([]);
  });

  it("what is held is capped, the oldest dropped first", () => {
    let state = initialState();
    for (let n = 0; n < MAX_HELD_DENIALS + 6; n++) state = applyEvent(state, changed(`toolu_${n}`, cmd("x")));
    expect(state.changedInputsBeforeTheirCall).toHaveLength(MAX_HELD_DENIALS);
    expect(state.changedInputsBeforeTheirCall[0].toolUseId).toBe("toolu_6");
  });

  it("the note text: a lone command, the changed fields, the removed ones, and a bound", () => {
    expect(changedInputText("Bash", cmd("ls"))).toBe("ls");
    expect(changedInputText("Bash", { fields: { command: "ls", dangerouslyDisableSandbox: true } })).toBe('{"command":"ls","dangerouslyDisableSandbox":true}');
    expect(changedInputText("Bash", { fields: { command: "ls" }, removed: ["timeout"] })).toBe('{"command":"ls"} (no longer present: timeout)');
    expect(changedInputText("Write", { fields: {}, removed: ["content"] })).toBe("(no longer present: content)");
    expect(changedInputText("Write", { fields: { content: "b" }, clipped: true })).toBe('{"content":"b"} (cut)');
    const long = changedInputText("Write", { fields: { content: "z".repeat(10_000) } });
    expect(long.length).toBeLessThan(4100);
    expect(long).toContain("more)");
  });
});
