export type ToolCallRecord = { toolUseId: string; name: string; input: unknown; result: { content: unknown; isError: boolean } | null };
export type PermissionRequestRecord = { permissionId: string; toolName: string; input: unknown };
export type SessionStatus =
  | { kind: "starting" }
  | { kind: "running" }
  | { kind: "unavailable"; reason: string }
  | { kind: "closed"; reason: string };

export type BackendKind = "legacy" | "sidecar";

/** A previous conversation offered for continuation. The key is the CLAUDE session id -- the only
 * identity that survives a resume. Resuming mints a new Verdandi session id, so storing that one
 * would point at something that stops existing the moment it is used. */
export type ResumableSession = {
  provider: string;
  providerSessionId: string;
  updatedAt: string;
};
export type PermissionModeChoice = "auto" | "bypass";

/** What the provider said about a resume. `attached` is the late verdict -- the provider only
 *  reports its session id at the start of a turn -- while the two failures arrive promptly. */
export type ResumeStatus = "attached" | "rejected" | "initialization_failed";

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
  /** Which build of the provider this is (Verdandi checkout and revision). Descriptive and
   * essentially always present -- deliberately NOT a diagnostic. */
  buildDescription: string | null;
  /** Only things worth warning about: an untested CLI version, a Verdandi checkout that has drifted
   * from the verified baseline. **Empty is the normal case**, which is what makes non-empty a real
   * signal worth putting a glyph on. */
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
  /** Reducer-internal, never on the wire: true while the last folded event was assistant text, so
   * the next chunk continues the same message. See `reducer.ts`'s `content_delta` case. */
  assistantMessageOpen: boolean;
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
  /** The previous conversation in this workspace, when there is one worth offering.
   *
   * Non-null only when ALL THREE hold: the provider advertised resume, this client implements it,
   * and this workspace has a persisted provider session id. The continue-previous control renders
   * on exactly this field and nothing else, so it cannot appear for a workspace with nothing to
   * continue. Null is the normal case for a fresh workspace. */
  resumableSession: ResumableSession | null;
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
  /** The provider's verdict on a resume, stated once for a session that asked for one. Mirrors the
   *  Rust `AgentDomainEvent::ResumeOutcome`; the reducer must fold it the same way
   *  `AgentSessionProjection` does, or the two states diverge on a failed resume. */
  | {
      type: "resume_outcome";
      requested_provider_session_id: string;
      status: ResumeStatus;
      attached_provider_session_id: string | null;
      forked: boolean;
      detail: string | null;
    }
  | { type: "session_unavailable"; reason: string }
  | { type: "session_closed"; reason: string };
