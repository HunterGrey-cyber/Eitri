export type ToolCallRecord = { toolUseId: string; name: string; input: unknown; result: { content: unknown; isError: boolean } | null };
export type PermissionRequestRecord = { permissionId: string; toolName: string; input: unknown };
export type SessionStatus =
  | { kind: "starting" }
  | { kind: "running" }
  | { kind: "unavailable"; reason: string }
  | { kind: "closed"; reason: string };

export type BackendKind = "legacy" | "sidecar";
export type PermissionModeChoice = "auto" | "bypass";

/** What the provider can actually do, as the server advertised it, intersected with what this
 * client implements. Never inferred from a name or from an enum member existing. */
export type Capabilities = {
  resume: boolean;
  fork: boolean;
  interrupt: boolean;
  bypassPermissionMode: boolean;
};

/** Descriptive only -- for display and diagnostics, never for deciding whether a control is
 * available. That decision belongs to `Capabilities`. */
export type ProviderInfo = {
  sidecarVersion: string;
  claudeAgentSdkVersion: string;
  claudeCodeVersion: string;
  protocol: string;
  /** Non-fatal things the provider said on its way up: which Verdandi checkout/revision it was
   * built from, and any CLI version-skew warning. The first thing to check when behavior is odd. */
  startupDiagnostics: string[];
};

export type AgentUiState = {
  backend: BackendKind;
  /** Three identities, deliberately never collapsed into one field.
   *  conversationId    -- Neovibe's own, stable for a workspace (null on the legacy backend)
   *  sessionId         -- Verdandi's, what every RPC is addressed with
   *  providerSessionId -- Claude's, what `claude --resume <id>` takes (null until the provider
   *                       reports it, which is why a session that never took a turn has none) */
  conversationId: string | null;
  sessionId: string | null;
  providerSessionId: string | null;
  model: string | null; cwd: string | null;
  transcript: string[]; toolCalls: ToolCallRecord[]; status: SessionStatus;
  activeTurnId: string | null; pendingPermissions: PermissionRequestRecord[];
  capabilities: Capabilities;
  provider: ProviderInfo | null;
};

/** The handshake reply, before any session exists: which backend is behind the bridge and what it
 * genuinely offers. The start screen renders from this rather than hardcoding either backend's
 * shape. */
export type Hello = {
  backend: BackendKind;
  projectDir: string;
  /** Only modes with genuinely distinct runtime behavior. The sidecar backend offers `bypass`
   * alone in this milestone -- its `interactive` and `verdandi_rules` modes are confirmed to
   * behave identically, so offering them as choices would be a lie. */
  permissionModes: PermissionModeChoice[];
  /** Always false in this milestone. The single field a Resume control may ever be gated on, so
   * the control cannot appear before the path behind it exists. */
  resumeAvailable: boolean;
  expectedVerdandiRevision: string | null;
};

export type TurnOutcome = "completed" | "interrupted" | "failed" | "limit_reached";
export type AgentDomainEvent =
  | { type: "session_opened"; session_id: string; provider_session_id: string; model: string; cwd: string }
  | { type: "turn_started"; turn_id: string }
  | { type: "content_delta"; turn_id: string; kind: "text" | "thinking"; text: string }
  | { type: "tool_call_started"; turn_id: string; tool_use_id: string; name: string; input: unknown }
  | { type: "tool_call_completed"; turn_id: string; tool_use_id: string; content: unknown; is_error: boolean }
  | { type: "permission_requested"; permission_id: string; tool_name: string; input: unknown }
  | { type: "permission_resolved"; permission_id: string; outcome: "allowed" | "denied" | "cancelled_by_interrupt" | "cancelled_by_session_close" | "provider_failed" | "expired" }
  | { type: "turn_completed"; turn_id: string; outcome: TurnOutcome; result_text: string; stop_reason: string | null; total_cost_usd: number; num_turns: number }
  | { type: "session_unavailable"; reason: string }
  | { type: "session_closed"; reason: string };
