/** The ordering key every item in a conversation carries, shared across `TranscriptMessage`,
 * `ToolCallRecord` and `PermissionRequestRecord` so the three can be interleaved into the one
 * sequence they really formed.
 *
 * Authoritative, not derived here: Rust's `AgentSessionProjection::apply` assigns it from the same
 * counter as `last_revision`, and `serialize_snapshot_for_js` ships it. That is what makes the
 * order survive a reload or a `UiDelivery::Resync` -- both throw this whole state away and rebuild
 * it from a snapshot, so an order the frontend had accumulated for itself would vanish with it.
 *
 * `reducer.ts` continues the same numbering for events folded after a snapshot (see `nextSeq`),
 * using the wire's own delivery order rather than inventing one. */
export type Seq = number;
/** One assistant message. `seq` is where the message STARTED -- appending a streamed chunk never
 * moves it, or a reply still streaming would keep sliding below the tool call that interrupted it. */
export type TranscriptMessage = { seq: Seq; text: string };
export type ToolCallRecord = { seq: Seq; toolUseId: string; name: string; input: unknown; result: { content: unknown; isError: boolean } | null };
/** `toolUseId` is the link back to the `ToolCallRecord` this request gates -- the same id that
 * call is keyed on.
 *
 * Genuinely nullable rather than optional. Both backends do send a real id on the paths that
 * actually gate tools -- the sidecar's proto `PermissionRequested` since 2026-09-10, and the
 * legacy backend's `PreToolUse` hook relay since 2026-09-15 -- so `null` is the uncommon case
 * rather than the normal one. It is still a case: the Rust side forwards only an id its source
 * message really carried and substitutes nothing when there is none (`agent/src/session.rs`'s
 * `permission_requested_event`), and an empty string is normalised to absent before it gets here.
 *
 * So `null` means "this request arrived with no usable link", never "this backend cannot supply
 * one". Render it honestly -- never default it, never guess at the most recent call. */
export type PermissionRequestRecord = { seq: Seq; permissionId: string; toolUseId: string | null; toolName: string; input: unknown };
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
  transcript: TranscriptMessage[]; toolCalls: ToolCallRecord[]; status: SessionStatus;
  activeTurnId: string | null; pendingPermissions: PermissionRequestRecord[];
  capabilities: Capabilities;
  provider: ProviderInfo | null;
  /** Reducer-internal, never on the wire: true while the last folded event was assistant text, so
   * the next chunk continues the same message. See `reducer.ts`'s `content_delta` case. */
  assistantMessageOpen: boolean;
  /** Reducer-internal, never on the wire: the `seq` the next locally-folded item will take.
   *
   * Seeded from a snapshot's own `throughRevision`, which Rust guarantees is strictly greater than
   * every `seq` inside that snapshot -- so an item folded after a snapshot sorts after everything
   * the snapshot carried, and cannot collide with one of them. Incremented once per `applyEvent`
   * call, exactly as `AgentSessionProjection::apply` bumps `last_revision`, so on the healthy path
   * the numbers are the same numbers Rust would have assigned rather than a parallel scheme. */
  nextSeq: Seq;
};

/** A snapshot as it ACTUALLY arrives from Rust, which is not an `AgentUiState`.
 *
 * `serialize_snapshot_for_js` emits neither of the two reducer-internal fields above -- they are
 * marked "never on the wire" for a reason and Rust has no key for either. Typing the inbound
 * envelope as a full `AgentUiState` asserted both were present and `number`/`boolean` when both are
 * `undefined` at runtime; `applySnapshot` overrides them immediately so nothing broke, but anything
 * that read `payload.state.nextSeq` before that -- a resync-diffing path, say, which is exactly the
 * kind of thing this area attracts -- would have got `undefined` with the compiler insisting on a
 * number. Subtracting them is the honest shape: what is missing is now missing in the type too. */
export type AgentUiSnapshot = Omit<AgentUiState, "assistantMessageOpen" | "nextSeq">;

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

/** What a provider reported about a turn's cost. Mirrors Rust's `agent::UsageInfo`, and the
 *  snake_case field names are deliberate -- `AgentDomainEvent` reaches the frontend through
 *  serde's own derive, not through `serialize_snapshot_for_js`'s camelCase re-shaping.
 *
 *  `turn_completed.usage` is `null` whenever the backend reported nothing, which on the `sidecar`
 *  backend is EVERY turn: `verdandi.claude.runtime.v1`'s `TurnCompleted` message carries no usage
 *  fields at all. Anything that renders this must show `null` as unknown. Showing it as $0.00 would
 *  reinstate the bug that made the field nullable: this event used to arrive with
 *  `total_cost_usd: 0, num_turns: 0` hardcoded on that backend, so a cost readout would have been
 *  correct on `legacy` and confidently, permanently wrong on `sidecar`. Nothing renders it today. */
export type UsageInfo = { total_cost_usd: number; num_turns: number };
export type AgentDomainEvent =
  | { type: "session_opened"; session_id: string; provider_session_id: string; model: string; cwd: string }
  | { type: "turn_started"; turn_id: string }
  | { type: "content_delta"; turn_id: string; kind: "text" | "thinking"; text: string }
  | { type: "tool_call_started"; turn_id: string; tool_use_id: string; name: string; input: unknown }
  | { type: "tool_call_completed"; turn_id: string; tool_use_id: string; content: unknown; is_error: boolean }
  | { type: "permission_requested"; permission_id: string; tool_use_id: string | null; tool_name: string; input: unknown }
  | { type: "permission_resolved"; permission_id: string; outcome: "allowed" | "denied" | "cancelled_by_interrupt" | "cancelled_by_session_close" | "provider_failed" | "expired" }
  | { type: "turn_completed"; turn_id: string; outcome: TurnOutcome; result_text: string; stop_reason: string | null; usage: UsageInfo | null }
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
