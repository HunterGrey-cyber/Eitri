export type ToolCallRecord = { toolUseId: string; name: string; input: unknown; result: { content: unknown; isError: boolean } | null };
export type PermissionRequestRecord = { permissionId: string; toolName: string; input: unknown };
export type SessionStatus =
  | { kind: "starting" }
  | { kind: "running" }
  | { kind: "unavailable"; reason: string }
  | { kind: "closed"; reason: string };
export type AgentUiState = {
  sessionId: string | null; model: string | null; cwd: string | null;
  transcript: string[]; toolCalls: ToolCallRecord[]; status: SessionStatus;
  activeTurnId: string | null; pendingPermissions: PermissionRequestRecord[];
};
export type TurnOutcome = "completed" | "interrupted" | "failed" | "limit_reached";
export type AgentDomainEvent =
  | { type: "session_opened"; session_id: string; model: string; cwd: string }
  | { type: "turn_started"; turn_id: string }
  | { type: "content_delta"; turn_id: string; kind: "text" | "thinking"; text: string }
  | { type: "tool_call_started"; turn_id: string; tool_use_id: string; name: string; input: unknown }
  | { type: "tool_call_completed"; turn_id: string; tool_use_id: string; content: unknown; is_error: boolean }
  | { type: "permission_requested"; permission_id: string; tool_name: string; input: unknown }
  | { type: "permission_resolved"; permission_id: string; outcome: "allowed" | "denied" | "cancelled_by_interrupt" | "cancelled_by_session_close" }
  | { type: "turn_completed"; turn_id: string; outcome: TurnOutcome; result_text: string; stop_reason: string | null; total_cost_usd: number; num_turns: number }
  | { type: "session_unavailable"; reason: string }
  | { type: "session_closed"; reason: string };
