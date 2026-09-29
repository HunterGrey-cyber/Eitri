import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState, ProviderPrompt, ToolCallRecord, WireProviderPrompt } from "./types";

/** The state before any snapshot arrives. `backend` defaults to "legacy" only because something
 * must be written here -- the real value always arrives with `hello` (before any session can exist)
 * and again with every snapshot, so nothing renders a backend-dependent decision from this default.
 * Capabilities default to all-false, which is the safe direction: a control gated on a capability
 * stays hidden until the server has actually said the capability exists. */
export function initialState(): AgentUiState {
  return {
    backend: "legacy",
    conversationId: null,
    sessionId: null,
    providerSessionId: null,
    model: null,
    cwd: null,
    userPrompts: [],
    transcript: [],
    toolCalls: [],
    status: { kind: "starting" },
    activeTurnId: null,
    pendingPermissions: [],
    capabilities: { resume: false, fork: false, interrupt: false, bypassPermissionMode: false },
    provider: null,
    history: null,
    assistantMessageOpen: false,
    nextSeq: 0,
    turnThinking: false,
  };
}

/** `r` on an ended session (or any other in-panel "start over"): drops back to the start screen.
 *
 * `backend`, `capabilities` and `provider` are preserved rather than reset, because those came
 * from `hello` and describe the bridge this WebView is attached to, not the session that just
 * ended -- a fresh `hello` is not coming, so this is the only copy of them there is. Everything
 * else (transcript, tool calls, pending permissions, status, `nextSeq`, ...) genuinely belongs to
 * the session that ended, so it resets to exactly what a brand-new mount would show. */
export function resetToStartScreen(state: AgentUiState): AgentUiState {
  return { ...initialState(), backend: state.backend, capabilities: state.capabilities, provider: state.provider };
}

/**
 * Folds one event, assigning any item it creates the `seq` that orders it against everything else
 * in the conversation.
 *
 * `nextSeq` advances by exactly one per call -- including for an event that creates nothing, which
 * is what `AgentSessionProjection::apply` does with `last_revision` too. Keeping the two in step
 * means a locally-folded item gets the number Rust would have given it, so the live path and a
 * later snapshot describe the same order rather than two schemes that merely happen to agree.
 *
 * **There is exactly one exception, and it is the first thing this function does** (2026-09-20): a
 * REPEATED thinking delta returns `incoming` itself, advancing nothing, so that a turn that merely
 * keeps thinking costs no re-render. The two counters therefore drift apart by one per such delta.
 * The paragraph above is the rule and this is the whole of its exception -- see the early return's
 * own comment for why it is safe, and `types.ts`'s `nextSeq` doc for what was checked rather than
 * assumed. Nothing else in this function may take a second exception without the same check.
 *
 * The events in one `{kind:"events"}` batch arrive in fold order -- `UiDelivery::Events` is
 * documented as "apply these events, in order", and `ConversationIngest` queues one entry per event
 * it folds -- so arrival order here IS the authoritative order, not a guess at it. The one case
 * where it queues nothing is while a resync is owed, and that ends in a snapshot, which re-seeds
 * this counter rather than continuing it.
 */
export function applyEvent(incoming: AgentUiState, event: AgentDomainEvent): AgentUiState {
  // A repeated thinking delta changes nothing. Returning `incoming` itself lets `App.tsx` skip
  // a render on the 33ms pump while the turn keeps thinking. This also leaves `nextSeq` unchanged:
  // no item is created, so nothing downstream needs a new `seq`. The first thinking delta still
  // falls through and advances `nextSeq` like other events.
  if (event.type === "content_delta" && event.kind === "thinking" && incoming.turnThinking) {
    return incoming;
  }
  const seq = incoming.nextSeq;
  // Advance `nextSeq` once here so every `return state` below includes it, even when no visible
  // state changes. Each event is still an ordered occurrence. Clear `turnThinking` by default
  // rather than maintaining a list of events that clear it. Only a `content_delta` with kind
  // `thinking` sets it back to true.
  const state: AgentUiState = { ...incoming, nextSeq: seq + 1, turnThinking: false };
  switch (event.type) {
    case "session_opened":
      // Arrives once PER TURN on the sidecar backend, not once per session: the Agent SDK emits a
      // system/init at the start of each turn even inside one streaming session. Folding it is
      // idempotent, so that is harmless -- but it must never be read as "a new session began".
      // `session_id` is Verdandi's and `provider_session_id` is Claude's; they are different values
      // and are kept in different fields.
      return {
        ...state,
        sessionId: event.session_id,
        providerSessionId: event.provider_session_id,
        model: event.model,
        cwd: event.cwd,
        status: { kind: "running" },
        assistantMessageOpen: false,
      };
    case "turn_started":
      return { ...state, activeTurnId: event.turn_id, assistantMessageOpen: false };
    case "assistant_message_boundary":
      // Two messages with no tool call between them are two entries, as `AgentSessionProjection`
      // folds them (the phase-3 GUI pass, 2026-09-25).
      return { ...state, assistantMessageOpen: false };
    case "user_prompt_submitted":
      // `assistantMessageOpen: false` for the same reason `AgentSessionProjection::apply` does it:
      // a prompt can only occur between assistant messages, and leaving the run open appends the
      // next turn's reply to the last one.
      return {
        ...state,
        userPrompts: [...state.userPrompts, { seq, text: event.text }],
        assistantMessageOpen: false,
      };
    case "content_delta": {
      if (event.kind === "thinking") return { ...state, turnThinking: true };
      // Guard against TS/Rust variant drift at runtime. TypeScript has narrowed `event.kind` to
      // "text" after the thinking case, so a cast is needed to check for an unknown variant.
      // Without the guard, a new Rust variant could append a transcript entry with undefined text.
      // `renderMarkdown` would throw inside `marked`, unmounting the panel without an error boundary.
      // This protects the same boundary as the switch's `default` arm.
      if ((event.kind as string) !== "text") return state;
      // `event.kind` is narrowed to "text" here, the only other member of the union.
      // `transcript` holds assistant MESSAGES, not content events. Under partial streaming a single
      // 600-word reply arrives as 400+ deltas; pushing each as its own entry renders 400 separate
      // bubbles, each markdown-parsed in isolation -- and a fragment like "`neovibe_" or "**bold"
      // is not valid standalone markdown, so every streamed reply's formatting breaks.
      //
      // MUST stay identical to `AgentSessionProjection::apply` in agent/src/projection.rs: the two
      // fold the same events and a snapshot from Rust has to be indistinguishable from this
      // reducer's own accumulation.
      if (state.assistantMessageOpen && state.transcript.length > 0) {
        const transcript = state.transcript.slice();
        const open = transcript[transcript.length - 1];
        // `seq` is untouched: a message is ordered by where it started, so a reply still streaming
        // does not slide below the tool call that already interrupted it.
        transcript[transcript.length - 1] = { seq: open.seq, text: open.text + event.text };
        return { ...state, transcript };
      }
      return { ...state, transcript: [...state.transcript, { seq, text: event.text }], assistantMessageOpen: true };
    }
    case "tool_call_started": {
      const record: ToolCallRecord = {
        seq,
        toolUseId: event.tool_use_id,
        name: event.name,
        input: event.input,
        result: null,
        turnId: event.turn_id,
      };
      // A tool call only happens between assistant messages, so the streaming text ended here.
      return { ...state, toolCalls: [...state.toolCalls, record], assistantMessageOpen: false };
    }
    case "tool_call_completed":
      return {
        ...state,
        toolCalls: state.toolCalls.map((call) =>
          call.toolUseId === event.tool_use_id ? { ...call, result: { content: event.content, isError: event.is_error } } : call,
        ),
      };
    case "permission_requested":
      return {
        ...state,
        assistantMessageOpen: false,
        pendingPermissions: [
          ...state.pendingPermissions,
          // `tool_use_id` is carried through as-is, null included: it is what ties this card to the
          // exact tool call it gates, and a turn can have several of the same tool in flight. The
          // reducer takes no view on when a request arrives without one -- that is settled
          // upstream, at `agent/src/session.rs`'s `permission_requested_event` -- it only refuses
          // to invent one. Note the two ids may legitimately be equal: on the legacy backend's
          // hook-relay path `permission_id` and `tool_use_id` are the same string, so nothing here
          // may assume they differ.
          {
            seq,
            permissionId: event.permission_id,
            toolUseId: event.tool_use_id,
            toolName: event.tool_name,
            input: event.input,
            // O3: the CLI's own prompt, reshaped to the snapshot's camelCase so a card built from
            // events and one rebuilt from a snapshot read the same. Absent on the gate's request.
            ...(event.provider_prompt ? { providerPrompt: providerPromptFromWire(event.provider_prompt) } : {}),
          },
        ],
      };
    case "permission_resolved":
      // Rust sends `permission_resolved` when `AgentSession::respond_permission` or `interrupt()`
      // resolves the request. Remove the card from this event so its lifetime follows Rust state.
      return { ...state, pendingPermissions: state.pendingPermissions.filter((p) => p.permissionId !== event.permission_id) };
    case "turn_completed":
      // The one authoritative source for "no turn is in flight" -- replaces the deleted
      // markTurnStarted's matching clear. v2 semantics unchanged from v1: a finished turn does
      // NOT end the conversation, only session_closed/session_unavailable do.
      return { ...state, activeTurnId: null, assistantMessageOpen: false };
    // Both endings clear activeTurnId, mirroring AgentSessionProjection exactly: no turn_completed
    // is ever coming, so leaving it set leaves App.tsx's turnInProgress true forever -- a spinner
    // on a dead session, next to a reply that may be truncated. Clearing it is not a local guess at
    // a completion: no message is closed out, no outcome invented; the turn just stops being in
    // flight, which is the truth.
    case "resume_outcome": {
      // Mirrors AgentSessionProjection exactly. A verdict that does not confirm the requested
      // session ends the conversation's usefulness, whatever arrives afterwards: a session that
      // looks continued while carrying none of its history is the one outcome the resume protocol
      // exists to prevent. `forked` is consulted rather than ignored because forking legitimately
      // returns a different id.
      if (resumeAttached(event)) return state;
      return {
        ...state,
        activeTurnId: null,
        status: { kind: "unavailable", reason: describeFailedResume(event) },
        assistantMessageOpen: false,
      };
    }
    case "session_unavailable":
      return {
        ...state,
        activeTurnId: null,
        status: { kind: "unavailable", reason: event.reason },
        assistantMessageOpen: false,
      };
    case "session_closed":
      return {
        ...state,
        activeTurnId: null,
        status: { kind: "closed", reason: event.reason },
        assistantMessageOpen: false,
      };
    case "permission_mode_changed":
      // The mode lives on the tab (`tabs` envelope), not the projection: `TabInfo.mode` is what the
      // band and the box read, and Rust's own `tabs` envelope already reflects an acknowledged
      // switch. Folding a second copy here would just be a value that can drift from it, so this
      // event is a no-op for the reducer -- `incoming`, not `state`, so it advances neither `nextSeq`
      // nor `turnThinking`, the same as the repeated-thinking-delta exception above.
      return incoming;
    default: {
      // Exhaustiveness guard: a new AgentDomainEvent variant added on the Rust side without a
      // matching TS case lands here at runtime -- observable, never silently dropped.
      console.warn("agent-ui: completely unhandled event shape from Rust", event);
      return state;
    }
  }
}

/** What the events envelope says beyond its events (Rust's `CallNotes`): the saved rule that
 *  answered a call and the `Write` cards raised over no file. */
export type CallNotes = {
  ruleNotes?: readonly { toolUseId: string; rule: string }[];
  createsFile?: readonly { permissionId: string; toolUseId: string | null }[];
  /** Calls whose CLI prompt was answered without a card, with the note shown on each call's row. */
  promptNotes?: readonly { toolUseId: string; note: string }[];
  /** Tool-use ids for `Write`, `Edit` or `NotebookEdit` calls answered by the acceptEdits fast
   *  path without a card. Unlike `ruleNotes`, these have no separate message: `toolRegistry.tsx`
   *  labels their rows "allowed by auto". */
  autoNotes?: readonly string[];
  /** Tool-use ids for `Write` calls answered without a card, through acceptEdits or bypass, when
   *  the path did not exist just before the answer. `createsFile` is keyed by permission id and only
   *  covers calls with cards. This signal keeps a new-file write from showing an overwrite warning. */
  autoCreatesFile?: readonly string[];
};

/** Folds an events envelope's `CallNotes` into the state, after that envelope's events, so a call
 *  or card from the same batch is already there. A note for something this state does not hold is
 *  dropped; the next snapshot carries it anyway. */
export function applyCallNotes(state: AgentUiState, notes: CallNotes): AgentUiState {
  const rules = new Map((notes.ruleNotes ?? []).map((n) => [n.toolUseId, n.rule]));
  const newFileCards = new Set((notes.createsFile ?? []).map((n) => n.permissionId));
  const newFileCalls = new Set((notes.createsFile ?? []).flatMap((n) => (n.toolUseId === null ? [] : [n.toolUseId])));
  const autoNewFileCalls = new Set(notes.autoCreatesFile ?? []);
  const promptNotes = new Map((notes.promptNotes ?? []).map((n) => [n.toolUseId, n.note]));
  const autoNotes = new Set(notes.autoNotes ?? []);
  if (
    rules.size === 0 &&
    newFileCards.size === 0 &&
    autoNewFileCalls.size === 0 &&
    promptNotes.size === 0 &&
    autoNotes.size === 0
  )
    return state;
  return {
    ...state,
    toolCalls: state.toolCalls.map((call) => {
      const rule = rules.get(call.toolUseId);
      const creates = newFileCalls.has(call.toolUseId) || autoNewFileCalls.has(call.toolUseId);
      const promptNote = promptNotes.get(call.toolUseId);
      const auto = autoNotes.has(call.toolUseId);
      if (rule === undefined && !creates && promptNote === undefined && !auto) return call;
      return {
        ...call,
        ...(rule === undefined ? {} : { allowedByRule: rule }),
        ...(creates ? { createsFile: true } : {}),
        ...(promptNote === undefined ? {} : { promptNote }),
        ...(auto ? { allowedByAuto: true } : {}),
      };
    }),
    pendingPermissions: state.pendingPermissions.map((p) => (newFileCards.has(p.permissionId) ? { ...p, createsFile: true } : p)),
  };
}

/** The event's snake_case `provider_prompt` as the snapshot spells it (`agent_bridge.rs`). */
function providerPromptFromWire(wire: WireProviderPrompt): ProviderPrompt {
  const rule = wire.matched_ask_rule;
  return {
    reason: wire.reason ?? null,
    description: wire.description ?? null,
    blockedPath: wire.blocked_path ?? null,
    matchedAskRule: rule ? { source: rule.source, toolName: rule.tool_name, ruleContent: rule.rule_content ?? null } : null,
    unrecognizedOrigin: wire.unrecognized_origin ?? null,
  };
}

/** Keep the visible conversation after its sidecar stops, with `reason` in the lost-session row.
 *  Return `null` when there is no restored history or conversation content, allowing an empty
 *  session to fail normally. Remove permission cards because the stopped sidecar can no longer
 *  answer them. */
export function keepAfterSidecarStop(state: AgentUiState, reason: string): AgentUiState | null {
  const said = state.history !== null || state.userPrompts.length > 0 || state.transcript.length > 0 || state.toolCalls.length > 0;
  if (!said) return null;
  return {
    ...state,
    status: { kind: "unavailable", reason },
    activeTurnId: null,
    pendingPermissions: [],
    assistantMessageOpen: false,
    turnThinking: false,
  };
}

/**
 * A snapshot is a complete replacement of this state.
 *
 * `throughRevision` comes from the same envelope as `snapshot` and is Rust's `last_revision` for
 * exactly the state it carries. Rust reads each item's `seq` from that counter BEFORE bumping it,
 * so every `seq` inside the snapshot is strictly below this number -- which makes it the correct
 * seed for `nextSeq`: the next event folded here gets a number no item in the snapshot already
 * holds, and sorts after all of them. This is the whole reason the ordering survives a reload or a
 * `UiDelivery::Resync`, the two paths that throw this state away and rebuild it from here.
 */
export function applySnapshot(_state: AgentUiState, snapshot: AgentUiSnapshot, throughRevision: number): AgentUiState {
  // The snapshot omits `nextSeq` and `turnThinking`, which `AgentUiSnapshot` excludes as
  // reducer-internal fields. Supply them here. Preserve `assistantMessageOpen` from the snapshot:
  // Rust continues appending to an open reply, and resetting it would split that reply into two
  // rows with broken markdown. Reset `turnThinking` because earlier thinking does not imply current
  // activity. Resync or reload may degrade `thinking` to `sent` but must not invent activity.
  return { ...snapshot, nextSeq: throughRevision, turnThinking: false };
}

/** Whether `resume_outcome` confirms the requested session. Share this predicate with
 * `applyEvent` and `App.tsx`, which uses it to stop `turnClock`, so both interpret the outcome
 * alike. A forked attach may legitimately return a different session id. */
export function resumeAttached(event: Extract<AgentDomainEvent, { type: "resume_outcome" }>): boolean {
  return event.status === "attached" && (event.forked || event.attached_provider_session_id === event.requested_provider_session_id);
}

/** The user-facing reason a resume did not continue the session that was asked for.
 *
 * Deliberately a near-transcription of Rust's `describe_failed_resume`: the two run on the same
 * events and must not disagree about what happened. Kept as text rather than a code so the panel
 * has something to show without a second mapping table on this side. */
function describeFailedResume(event: Extract<AgentDomainEvent, { type: "resume_outcome" }>): string {
  const detail = event.detail !== null && event.detail.trim() !== "" ? ` (${event.detail})` : "";
  const requested = event.requested_provider_session_id;
  switch (event.status) {
    case "rejected":
      return `the provider does not have session ${requested} any more, so that conversation cannot be continued${detail}. Start a new session instead.`;
    case "initialization_failed":
      return `the provider failed to start while continuing session ${requested}${detail}. This is a provider problem rather than a missing conversation — the session may still exist. Try again, or start a new session.`;
    case "attached":
      return `asked to continue session ${requested}, but the provider attached to ${event.attached_provider_session_id ?? "an unnamed session"} instead, so this conversation carries none of the history that was asked for${detail}. Start a new session instead.`;
  }
}
