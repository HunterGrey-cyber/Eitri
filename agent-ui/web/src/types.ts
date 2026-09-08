export type PermissionSource = "hook_relay" | "can_use_tool";
export type ToolCallRecord = { id: string; name: string; input: unknown; result: { content: unknown; isError: boolean } | null };
export type PermissionRequestRecord = { requestId: string; toolName: string; input: unknown; source: PermissionSource };
export type SessionStatus = { kind: "starting" } | { kind: "running" } | { kind: "finished"; isError: boolean };
export type AgentUiState = {
  sessionId: string | null; model: string | null; cwd: string | null;
  transcript: string[]; toolCalls: ToolCallRecord[]; status: SessionStatus;
  turnInProgress: boolean; pendingPermissions: PermissionRequestRecord[];
};
export type AgentEvent =
  | { type: "session_started"; session_id: string; model: string; cwd: string }
  | { type: "assistant_text"; text: string }
  | { type: "thinking"; text: string }
  | { type: "tool_started"; id: string; name: string; input: unknown }
  | { type: "tool_result"; id: string; content: unknown; is_error: boolean }
  | { type: "turn_finished"; result_text: string; is_error: boolean; stop_reason: string | null; total_cost_usd: number; num_turns: number }
  | { type: "rate_limit"; raw: unknown }
  | { type: "unknown"; kind: string; subtype: string | null; raw: unknown }
  | { type: "process_stderr"; line: string }
  | { type: "process_exited"; success: boolean }
  | { type: "permission_request"; request_id: string; tool_name: string; input: unknown; source: PermissionSource }
  | { type: "control_response"; request_id: string; subtype: string; raw: unknown };
