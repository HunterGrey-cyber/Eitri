import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState, Seq, SubagentStream, ToolCallRecord } from "../types";

type Fold = (state: AgentUiState, event: AgentDomainEvent, seq: Seq) => AgentUiState;
const empty = (): SubagentStream => ({ transcript: [], thinking: [], toolCalls: [], assistantMessageOpen: false, thinkingOpen: false });

/** Updates the first matching call at any depth without changing the incoming tree. */
export function updateCall(calls: ToolCallRecord[], id: string, update: (call: ToolCallRecord) => ToolCallRecord): ToolCallRecord[] | null {
  for (let at = 0; at < calls.length; at++) {
    const call = calls[at];
    if (call.toolUseId === id) return calls.map((entry, i) => i === at ? update(call) : entry);
    if (call.children) {
      const nested = updateCall(call.children.toolCalls, id, update);
      if (nested) return calls.map((entry, i) => i === at ? { ...call, children: { ...call.children!, toolCalls: nested } } : entry);
    }
  }
  return null;
}

function route(calls: ToolCallRecord[], id: string, frame: AgentDomainEvent, seq: Seq, active: string | null): ToolCallRecord[] {
  const update = (call: ToolCallRecord) => ({ ...call, children: foldStream(call.children ?? empty(), frame, seq, active) });
  return updateCall(calls, id, update) ?? [...calls, update({ seq, toolUseId: id, turnId: "", name: "background agent", input: {}, result: null, background: true })];
}

function foldStream(stream: SubagentStream, frame: AgentDomainEvent, seq: Seq, active: string | null): SubagentStream {
  switch (frame.type) {
    case "content_delta": {
      if (!frame.text) return stream;
      if (frame.kind === "text") {
        const transcript = stream.transcript.slice();
        if (stream.assistantMessageOpen && transcript.length > 0) {
          const last = transcript[transcript.length - 1];
          transcript[transcript.length - 1] = { ...last, text: last.text + frame.text };
        } else transcript.push({ seq, text: frame.text });
        return { ...stream, transcript, assistantMessageOpen: true };
      }
      const thinking = stream.thinking.slice();
      if (stream.thinkingOpen && thinking.length > 0) {
        const last = thinking[thinking.length - 1];
        thinking[thinking.length - 1] = { ...last, text: last.text + frame.text };
      } else thinking.push({ seq, turnId: frame.turn_id, text: frame.text });
      return { ...stream, thinking, thinkingOpen: true };
    }
    case "assistant_message_boundary": return { ...stream, assistantMessageOpen: false, thinkingOpen: false };
    case "tool_call_started": return { ...stream, assistantMessageOpen: false, thinkingOpen: false,
      toolCalls: [...stream.toolCalls, { seq, turnId: frame.turn_id, toolUseId: frame.tool_use_id, name: frame.name, input: frame.input, result: null,
        ...(active !== frame.turn_id ? { background: true } : {}) }] };
    case "tool_call_completed": return { ...stream, toolCalls: updateCall(stream.toolCalls, frame.tool_use_id,
      (call) => ({ ...call, result: { content: frame.content, isError: frame.is_error },
        ...(call.background || active !== frame.turn_id ? { background: true } : {}) })) ?? stream.toolCalls };
    case "subagent_frame": return { ...stream, toolCalls: route(stream.toolCalls, frame.parent_tool_use_id, frame.frame, seq, active) };
    default: return stream;
  }
}

export function fold(state: AgentUiState, event: Extract<AgentDomainEvent, { type: "subagent_frame" }>, _apply: Fold): AgentUiState {
  if (!["content_delta", "tool_call_started", "tool_call_completed", "assistant_message_boundary", "subagent_frame"].includes(event.frame.type)) return state;
  let leaf: AgentDomainEvent = event.frame;
  while (leaf.type === "subagent_frame") leaf = leaf.frame;
  if (!["content_delta", "tool_call_started", "tool_call_completed", "assistant_message_boundary"].includes(leaf.type)) return state;
  let toolCalls = route(state.toolCalls, event.parent_tool_use_id, event.frame, state.nextSeq - 1, state.activeTurnId);
  if (leaf.type !== "tool_call_started") return { ...state, toolCalls };
  const id = leaf.tool_use_id;
  const denied = state.denialsBeforeTheirCall.find((entry) => entry.toolUseId === id);
  const changed = state.changedInputsBeforeTheirCall.find((entry) => entry.toolUseId === id);
  toolCalls = updateCall(toolCalls, id, (call) => ({ ...call,
    ...(denied ? { denied: denied.denied } : {}), ...(changed ? { changedInput: changed.change } : {}) }))!;
  return { ...state, toolCalls,
    denialsBeforeTheirCall: state.denialsBeforeTheirCall.filter((entry) => entry.toolUseId !== id),
    changedInputsBeforeTheirCall: state.changedInputsBeforeTheirCall.filter((entry) => entry.toolUseId !== id) };
}

export function fromSnapshot(state: AgentUiState, snapshot: AgentUiSnapshot): AgentUiState {
  return { ...state, toolCalls: snapshot.toolCalls ?? [] };
}

export function isChildCall(calls: ToolCallRecord[], id: string | null): boolean {
  return id !== null && calls.some((call) => call.children && updateCall(call.children.toolCalls, id, (entry) => entry) !== null);
}
