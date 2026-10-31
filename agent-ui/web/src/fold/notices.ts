import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState, NoticeRecord } from "../types";

type Notice = Extract<AgentDomainEvent, { type: "session_notice" }>;

export function recordsRow(event: Notice): boolean {
  switch (event.kind) {
    case "api_retry": case "compacting": case "notification": return false;
    case "informational": return event.level !== "info";
    case "peer_hold": return event.scalars?.hold_state === "held" || event.scalars?.hold_state === "dropped";
    case "rate_limit": return !["allowed", "allowed_warning", "rejected"].includes(String(event.scalars?.rate_status));
    default: return true;
  }
}

export function fold(state: AgentUiState, event: AgentDomainEvent): AgentUiState {
  if (event.type !== "session_notice") return state;
  const record: NoticeRecord = {
    seq: state.nextSeq - 1, kind: event.kind, level: event.level, content: event.content ?? null,
    scalars: event.scalars ?? {}, taskId: event.task_id ?? null, toolUseId: event.tool_use_id ?? null, taskStatus: event.task_status ?? null,
  };
  switch (event.kind) {
    case "api_retry": state = { ...state, apiRetry: record }; break;
    case "compacting": state = { ...state, compacting: true }; break;
    case "compaction": state = { ...state, compacting: false }; break;
    case "notification": state = { ...state, notification: record }; break;
    case "model_fallback":
      if (typeof record.scalars.to_model === "string") state = { ...state, fallbackModel: record.scalars.to_model };
      break;
    case "rate_limit": state = { ...state, rateLimitWarning: record.scalars.rate_status === "allowed_warning" ? record : null }; break;
  }
  return recordsRow(event) ? { ...state, assistantMessageOpen: false, thinkingOpen: false, notices: [...state.notices, record] } : state;
}

export function fromSnapshot(state: AgentUiState, snapshot: AgentUiSnapshot): AgentUiState {
  return { ...state, apiRetry: snapshot.apiRetry ?? null, compacting: snapshot.compacting ?? false,
    rateLimitWarning: snapshot.rateLimitWarning ?? null, fallbackModel: snapshot.fallbackModel ?? null,
    effort: snapshot.effort ?? null, notification: snapshot.notification ?? null };
}

export function onTurnEnd(state: AgentUiState, _event: AgentDomainEvent): AgentUiState { return { ...state, apiRetry: null }; }
export function onContent(state: AgentUiState, _event: AgentDomainEvent): AgentUiState { return { ...state, apiRetry: null }; }
export function isTransient(kind: string): boolean { return kind === "api_retry" || kind === "compacting" || kind === "notification"; }
