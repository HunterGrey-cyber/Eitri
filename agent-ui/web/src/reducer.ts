import { cleanCommands } from "./slashCommands";
import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState, ProviderPrompt, Seq, ToolCallRecord, TurnEnding, WireProviderPrompt } from "./types";

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
    usage: null,
    accountLimits: null,
    backgroundCalls: 0,
    availableCommands: null,
    turnEndings: [],
    lastTurnEnding: null,
    history: null,
    assistantMessageOpen: false,
    thinking: [],
    thinkingOpen: false,
    nextSeq: 0,
    turnThinking: false,
    ...NOTHING_HELD,
  };
}

/** What the state holds for calls that have not been folded: none of it on the wire, all of it
 * dropped where Rust's projection drops its own. */
const NOTHING_HELD = { denialsBeforeTheirCall: [], changedInputsBeforeTheirCall: [] } as const;

/** Appends `(toolUseId, change)`, replacing an earlier entry for the id, capped at `MAX_HELD_DENIALS`
 * with the oldest dropped first (Rust's `push_bounded`). */
function pushBounded<T extends { toolUseId: string }>(held: ReadonlyArray<T>, entry: T): T[] {
  return [...held.filter((h) => h.toolUseId !== entry.toolUseId), entry].slice(-MAX_HELD_DENIALS);
}

/** How many refusals that arrived ahead of their call a state holds at once; the same cap as
 * Rust's `MAX_HELD_DENIALS`, so a refusal naming a call that never comes cannot pile up. */
export const MAX_HELD_DENIALS = 64;

/** The "lost" ending, for an event that ends the session: when a turn was still running, no
 *  `turn_completed` is ever coming for it, so the transcript would show a reply that just stops. This
 *  pushes one turn-ending row for it, with the event's own `seq`, and the band's `lastTurnEnding` says
 *  so. With no turn running there is nothing to report (a failed turn already ended before the session
 *  did: that turn has its own row and this adds none). MUST stay identical to
 *  `AgentSessionProjection::apply`, which does the same on the same three events. */
function withLostTurn(state: AgentUiState, seq: Seq): AgentUiState {
  if (state.activeTurnId === null) return state;
  const ending: TurnEnding = {
    seq,
    turnId: state.activeTurnId,
    kind: "lost",
    reason: null,
    apiErrorStatus: null,
    message: null,
    resetsAt: state.accountLimits?.resetsAt ?? null,
  };
  return { ...state, turnEndings: [...state.turnEndings, ending], lastTurnEnding: "lost" };
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
 * Folds one delivered batch, in order: the same state `events.reduce(applyEvent, state)` gives.
 *
 * A run of consecutive text deltas of one turn is folded as one delta carrying their joined text,
 * and `nextSeq` is then moved on by the rest of the run, which is exactly what folding them one by
 * one does: the first either opens a message (at its own `seq`) or appends to the open one, and
 * every later one appends to that same message, which keeps the first's `seq`. Folding them one by
 * one copied the whole transcript once per delta; a batch of D deltas over M messages copied M x D
 * entries for one render.
 *
 * A run of thinking deltas is folded the same way, with one difference: a thinking delta with no text
 * opens no block, so the empty ones at the head of a run only move `nextSeq` on, and the block the
 * joined delta opens takes the `seq` of the first one that has text, as folding one by one gives it.
 */
export function applyEvents(state: AgentUiState, events: readonly AgentDomainEvent[]): AgentUiState {
  let folded = state;
  let i = 0;
  while (i < events.length) {
    const first = events[i];
    let end = i + 1;
    const kind = mergeableKind(first);
    if (kind !== null) {
      const turn = (first as ContentDelta).turn_id;
      while (end < events.length) {
        const next = events[end];
        if (mergeableKind(next) !== kind || (next as ContentDelta).turn_id !== turn) break;
        end += 1;
      }
    }
    if (end - i === 1) {
      folded = applyEvent(folded, first);
    } else {
      const run = events.slice(i, end) as ContentDelta[];
      // Only a thinking run skips its empty head (see above); a text delta with no text still opens a
      // message, as `AgentSessionProjection::apply` does.
      let lead = 0;
      if (kind === "thinking") while (lead < run.length - 1 && run[lead].text === "") lead += 1;
      const skipped = { ...folded, nextSeq: folded.nextSeq + lead };
      const merged = applyEvent(skipped, { ...run[lead], text: run.slice(lead).map((e) => e.text).join("") });
      folded = { ...merged, nextSeq: merged.nextSeq + (run.length - lead - 1) };
    }
    i = end;
  }
  return folded;
}

type ContentDelta = Extract<AgentDomainEvent, { type: "content_delta" }>;

/** The kind of a delta `applyEvents` may join with its neighbours, or `null` for any other event. */
function mergeableKind(event: AgentDomainEvent): "text" | "thinking" | null {
  if (event.type !== "content_delta" || typeof event.text !== "string") return null;
  return event.kind === "text" || event.kind === "thinking" ? event.kind : null;
}

let transcriptEntriesCopied = 0;
/** How many transcript entries text deltas have copied into new arrays in this page (the
 *  performance tests count it). */
export function transcriptEntriesCopiedCount(): number {
  return transcriptEntriesCopied;
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
 * There is no exception any more. `permission_mode_changed` used to return `incoming` and advance
 * nothing, and a repeated thinking delta did the same; both drifted the page's numbering one behind
 * Rust's per such event, which nothing noticed until turn footers compared a seq Rust named with the
 * page's own. A batch of thinking deltas still costs one fold (`applyEvents`) and one render.
 *
 * The events in one `{kind:"events"}` batch arrive in fold order -- `UiDelivery::Events` is
 * documented as "apply these events, in order", and `ConversationIngest` queues one entry per event
 * it folds -- so arrival order here IS the authoritative order, not a guess at it. The one case
 * where it queues nothing is while a resync is owed, and that ends in a snapshot, which re-seeds
 * this counter rather than continuing it.
 */
export function applyEvent(incoming: AgentUiState, event: AgentDomainEvent): AgentUiState {
  const seq = incoming.nextSeq;
  // Advanced once, here, so every `return state` below carries it -- including the arms that change
  // nothing else. An event with no visible effect is still a real, ordered occurrence.
  //
  // `turnThinking` defaults to false here too, for every event: it is cleared by the ABSENCE of a
  // special case rather than by a maintained list of event types that ought to clear it.
  // Only the `content_delta`/`"thinking"` arm below sets it back to true.
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
      };
    case "turn_started":
      // A new turn is newer news than however the last one ended: the band stops saying it. The row
      // already in the transcript stays where it is.
      return { ...state, activeTurnId: event.turn_id, assistantMessageOpen: false, thinkingOpen: false, lastTurnEnding: null };
    case "assistant_message_boundary":
      // Two messages with no tool call between them are two entries, as `AgentSessionProjection`
      // folds them (the phase-3 GUI pass, 2026-09-25).
      return { ...state, assistantMessageOpen: false, thinkingOpen: false };
    case "user_prompt_submitted":
      // `assistantMessageOpen: false` for the same reason `AgentSessionProjection::apply` does it:
      // a prompt can only occur between assistant messages, and leaving the run open appends the
      // next turn's reply to the last one.
      return {
        ...state,
        userPrompts: [...state.userPrompts, { seq, text: event.text }],
        assistantMessageOpen: false,
        thinkingOpen: false,
      };
    case "user_message_delivered":
      // The same row a submitted prompt is, at the point the agent read it -- not where Enter was pressed,
      // which is why it is not folded when it is sent -- and closing the run for the same reason as
      // `AgentSessionProjection::apply`'s arm. Marked delivered, as Rust marks it, so it reads as part
      // of the running turn rather than the start of a new one.
      return {
        ...state,
        userPrompts: [...state.userPrompts, { seq, text: event.text, delivered: true }],
        assistantMessageOpen: false,
        thinkingOpen: false,
      };
    case "user_message_dropped":
      // Nothing in the conversation: Rust holds the message again, and its queue envelope says why.
      return state;
    case "content_delta": {
      if (event.kind === "thinking") {
        // The agent's thinking, kept as `AgentSessionProjection::apply` keeps it: consecutive deltas
        // are one block, ordered by the delta that opened it, and a delta with no text opens nothing
        // and closes nothing. MUST stay identical to that fold. Text that is not a string (a shape
        // Rust never sends) is read as none rather than appended as "undefined".
        const text = typeof event.text === "string" ? event.text : "";
        if (text === "") return { ...state, turnThinking: true };
        if (state.thinkingOpen && state.thinking.length > 0) {
          const thinking = state.thinking.slice();
          const open = thinking[thinking.length - 1];
          thinking[thinking.length - 1] = { ...open, text: open.text + text };
          return { ...state, thinking, turnThinking: true };
        }
        return {
          ...state,
          thinking: [...state.thinking, { seq, turnId: event.turn_id, text }],
          thinkingOpen: true,
          turnThinking: true,
        };
      }
      // A RUNTIME defence against TS/Rust drift, not a type-level branch -- which is exactly why
      // it needs the cast. `ContentKind` is `{Text, Thinking}` today, so TS has already narrowed
      // `event.kind` to "text" by this line and the comparison is dead by the types. It is not
      // dead at runtime: a third variant added on the Rust side without a matching case here would
      // otherwise fall through into the text path below and append a transcript entry whose `text`
      // is `undefined`, which `MessageList` hands to `renderMarkdown`, which throws inside
      // `marked` -- and this app has no error boundary, so the whole panel unmounts. This arm used
      // to read `if (event.kind !== "text") return state;` and the in-flight motion change turned
      // it into `if (event.kind === "thinking")`, dropping the guard, which was reproduced as an
      // unmount on 2026-09-20. Same shape, and same reason, as the `default:` arm at
      // the bottom of this switch.
      if ((event.kind as string) !== "text") return state;
      // `event.kind` is narrowed to "text" here, the only other member of the union.
      // `transcript` holds assistant MESSAGES, not content events. Under partial streaming a single
      // 600-word reply arrives as 400+ deltas; pushing each as its own entry renders 400 separate
      // bubbles, each markdown-parsed in isolation -- and a fragment like "`eitri_" or "**bold"
      // is not valid standalone markdown, so every streamed reply's formatting breaks.
      //
      // MUST stay identical to `AgentSessionProjection::apply` in agent/src/projection.rs: the two
      // fold the same events and a snapshot from Rust has to be indistinguishable from this
      // reducer's own accumulation.
      transcriptEntriesCopied += state.transcript.length;
      if (state.assistantMessageOpen && state.transcript.length > 0) {
        const transcript = state.transcript.slice();
        const open = transcript[transcript.length - 1];
        // `seq` is untouched: a message is ordered by where it started, so a reply still streaming
        // does not slide below the tool call that already interrupted it.
        transcript[transcript.length - 1] = { seq: open.seq, text: open.text + event.text };
        // A reply ends the thought before it; the next thought is a block of its own.
        return { ...state, transcript, thinkingOpen: false };
      }
      return { ...state, transcript: [...state.transcript, { seq, text: event.text }], assistantMessageOpen: true, thinkingOpen: false };
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
      // A refusal that arrived before this call is attached now, as Rust's projection does.
      const held = state.denialsBeforeTheirCall.find((d) => d.toolUseId === event.tool_use_id);
      if (held !== undefined) record.denied = held.denied;
      const heldChange = state.changedInputsBeforeTheirCall.find((c) => c.toolUseId === event.tool_use_id);
      if (heldChange !== undefined) record.changedInput = heldChange.change;
      // A tool call only happens between assistant messages, so the streaming text ended here.
      return {
        ...state,
        toolCalls: [...state.toolCalls, record],
        assistantMessageOpen: false,
        thinkingOpen: false,
        ...(held === undefined
          ? {}
          : { denialsBeforeTheirCall: state.denialsBeforeTheirCall.filter((d) => d.toolUseId !== event.tool_use_id) }),
        ...(heldChange === undefined
          ? {}
          : { changedInputsBeforeTheirCall: state.changedInputsBeforeTheirCall.filter((c) => c.toolUseId !== event.tool_use_id) }),
      };
    }
    case "tool_call_completed": {
      // Only the first call with the id, as the Rust fold does: two calls can share an id (the sidecar's
      // empty one), and a result belongs to one of them. Found once, so a long session's completion
      // stays linear in its calls.
      const first = state.toolCalls.findIndex((c) => c.toolUseId === event.tool_use_id);
      return {
        ...state,
        toolCalls: state.toolCalls.map((call, i) =>
          i === first ? { ...call, result: { content: event.content, isError: event.is_error } } : call,
        ),
      };
    }
    case "permission_requested":
      return {
        ...state,
        assistantMessageOpen: false,
        thinkingOpen: false,
        pendingPermissions: [
          // Keyed by `permission_id` like the Rust map: a repeat replaces the card and takes the newer `seq`.
          ...state.pendingPermissions.filter((p) => p.permissionId !== event.permission_id),
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
      // The one authoritative source for "this permission card is gone".
      // Replaces the deleted markPermissionAnswered: this now arrives as a real event from Rust
      // the instant AgentSession::respond_permission or interrupt() resolves it, never as a
      // frontend-local guess.
      return { ...state, pendingPermissions: state.pendingPermissions.filter((p) => p.permissionId !== event.permission_id) };
    case "turn_completed": {
      // Clear the in-flight turn on `turn_completed`. Finishing a turn does not end the
      // conversation; `session_closed` and `session_unavailable` do. Match
      // `AgentSessionProjection::apply` when updating usage: replace the entire figure because the
      // SDK reports a running total that `/clear` can reset. A lower report is valid. A turn with
      // no usage report keeps the last figure; missing data must not erase it or become zero. A
      // failed, limited or interrupted turn also leaves a row with the provider's explanation and
      // updates the band until the next turn starts. A completed turn adds no ending row. Keep this
      // behavior identical to `AgentSessionProjection::apply`. Use a fallback for absent `detail`
      // so an event without it still produces a row.
      const done = {
        ...state,
        activeTurnId: null,
        assistantMessageOpen: false,
        thinkingOpen: false,
        usage: event.usage ?? state.usage,
        ...NOTHING_HELD,
      };
      if (event.outcome === "completed") return done;
      const detail = event.detail ?? { reason: null, api_error_status: null, message: null };
      const ending: TurnEnding = {
        seq,
        turnId: event.turn_id,
        kind: event.outcome,
        reason: detail.reason ?? null,
        apiErrorStatus: detail.api_error_status ?? null,
        message: detail.message ?? null,
        resetsAt: state.accountLimits?.resetsAt ?? null,
      };
      return { ...done, turnEndings: [...state.turnEndings, ending], lastTurnEnding: event.outcome };
    }
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
        ...withLostTurn(state, seq),
        activeTurnId: null,
        status: { kind: "unavailable", reason: describeFailedResume(event) },
        assistantMessageOpen: false,
        thinkingOpen: false,
        ...NOTHING_HELD,
      };
    }
    case "session_unavailable":
      return {
        ...withLostTurn(state, seq),
        activeTurnId: null,
        status: { kind: "unavailable", reason: event.reason },
        assistantMessageOpen: false,
        thinkingOpen: false,
        ...NOTHING_HELD,
      };
    case "session_closed":
      return {
        ...withLostTurn(state, seq),
        activeTurnId: null,
        status: { kind: "closed", reason: event.reason },
        assistantMessageOpen: false,
        thinkingOpen: false,
        ...NOTHING_HELD,
      };
    case "cli_permission_mode":
      // Rust's answer path reads it; the view draws nothing from it. `state`, so `nextSeq` keeps
      // step with Rust's revision, which this event bumps like any other.
      return state;
    case "permission_denied": {
      // On the call it refused, as `AgentSessionProjection::apply` puts it. A refusal naming no call
      // changes nothing. One whose call this state does not hold yet is kept by its id and attached
      // when the call starts (see `tool_call_started`), dropped when the turn or session ends, and
      // capped, oldest first, exactly as Rust's projection does -- without it the row would lack its
      // "blocked by auto" note until a snapshot.
      const id = event.tool_use_id;
      if (id === null || id === "") return state;
      const denied = { reasonType: event.reason_type, reason: event.reason };
      if (state.toolCalls.some((call) => call.toolUseId === id)) {
        return { ...state, toolCalls: state.toolCalls.map((call) => (call.toolUseId === id ? { ...call, denied } : call)) };
      }
      const held = [...state.denialsBeforeTheirCall.filter((d) => d.toolUseId !== id), { toolUseId: id, denied }];
      return { ...state, denialsBeforeTheirCall: held.slice(-MAX_HELD_DENIALS) };
    }
    case "policy_mismatch":
      // Rust fails the tab (`TabSet::pump`) and the failure reaches the page as the tab's own state;
      // there is nothing here to draw. `state`, so `nextSeq` keeps step with Rust's revision.
      return state;
    case "account_limits": {
      // The latest report wins, whole: the agent states the window each time. A slot it did not say
      // is `null`, not carried over from the earlier report. The latest turn's ending, if it did not
      // complete and knew no reset time, takes this one (the report explaining a stop can follow it);
      // one it had stays. A resumed session's past ending is never filled: Rust's seed carries no
      // `lastTurnEnding`. MUST stay identical to `AgentSessionProjection::apply`.
      const resetsAt = event.resets_at ?? null;
      const last = state.turnEndings[state.turnEndings.length - 1];
      const fill = state.lastTurnEnding !== null && last !== undefined && (last.resetsAt ?? null) === null;
      return {
        ...state,
        accountLimits: {
          usedPercent: event.used_percent,
          windowMinutes: event.window_minutes ?? null,
          resetsAt,
          plan: event.plan ?? null,
        },
        turnEndings: fill ? [...state.turnEndings.slice(0, -1), { ...last, resetsAt }] : state.turnEndings,
      };
    }
    case "background_calls":
      // The latest count wins, as in `AgentSessionProjection::apply`.
      return { ...state, backgroundCalls: event.running };
    case "commands_available":
      // The latest list replaces the whole earlier one, as in `AgentSessionProjection::apply`. The names
      // and descriptions are the agent's own text: bounded here again, whatever Rust let through.
      return { ...state, availableCommands: cleanCommands(event.commands) };
    case "tool_input_changed": {
      // Rust compared the CLI's prompt with the gate request (one it may have answered itself, so
      // the page never saw it) and says so here: applied, never recomputed. Like
      // `AgentSessionProjection::apply`: the first call with that id, or held until it starts.
      const id = event.tool_use_id;
      const at = state.toolCalls.findIndex((call) => call.toolUseId === id);
      if (at >= 0) {
        return { ...state, toolCalls: state.toolCalls.map((call, i) => (i === at ? { ...call, changedInput: event.change } : call)) };
      }
      return {
        ...state,
        changedInputsBeforeTheirCall: pushBounded(state.changedInputsBeforeTheirCall, { toolUseId: id, change: event.change }),
      };
    }
    case "permission_mode_changed":
      // The mode lives on the tab (`tabs` envelope), not the projection: `TabInfo.mode` is what the
      // band and the box read, and Rust's own `tabs` envelope already reflects an acknowledged
      // switch. Folding a second copy here would just be a value that can drift from it, so this
      // event changes nothing the page draws. It still advances `nextSeq`, as Rust bumps
      // `last_revision` for it: the seqs Rust names (a turn's footer anchor) must be the page's own.
      // `turnThinking` is left as it was: a mode switch says nothing about what the agent is doing.
      return { ...incoming, nextSeq: seq + 1 };
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
  /** Tool-use ids of a `Write`/`Edit`/`NotebookEdit` the acceptEdits fast path
   *  answered with no card. No message of its own, unlike `ruleNotes` -- there is only one fast
   *  path, so the row just says "allowed by auto" (`toolRegistry.tsx`). */
  autoNotes?: readonly string[];
  /** Tool-use ids of a `Write` answered with no card -- by the acceptEdits
   *  fast path, or in bypass -- over a path where nothing existed
   *  just before the answer was sent. `createsFile`'s own signal is keyed by permission id and only
   *  ever set for a call that raised a card (F22) -- a `Write` answered without one raises none, so
   *  without this its row kept `createsFile`'s absence and showed the overwrite warning for a file
   *  that never existed. */
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

/** The conversation a tab showed when its session failed under it, kept on screen as a lost
 *  session (`reason` in the lost-session row) rather than replaced by an empty failed tab. `null`
 *  when there is nothing to keep -- no restored history and nothing said yet -- so a session that
 *  never showed anything still fails the ordinary way. The cards go: nothing can answer them any
 *  more. */
export function keepAfterFailure(state: AgentUiState, reason: string): AgentUiState | null {
  const said =
    state.history !== null ||
    state.userPrompts.length > 0 ||
    state.transcript.length > 0 ||
    state.toolCalls.length > 0 ||
    state.thinking.length > 0;
  if (!said) return null;
  return {
    ...state,
    status: { kind: "unavailable", reason },
    activeTurnId: null,
    pendingPermissions: [],
    assistantMessageOpen: false,
    thinkingOpen: false,
    turnThinking: false,
    ...NOTHING_HELD,
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
  // The snapshot omits `nextSeq`, `turnThinking` and `denialsBeforeTheirCall`, which
  // `AgentUiSnapshot` excludes as reducer-internal fields. Supply them here. Preserve
  // `assistantMessageOpen` from the snapshot: Rust continues appending to an open reply, and
  // resetting it would split that reply into two rows with broken markdown. Reset `turnThinking`
  // because earlier thinking does not imply current activity. Resync or reload may degrade
  // `thinking` to `sent` but must not invent activity. Take `usage` from this tab's snapshot so a
  // tab with no report cannot retain the previous tab's figure. Default an absent key to `null`,
  // which `usageSegment` treats as unknown; `undefined` would throw. Restore `accountLimits` from
  // the snapshot, defaulting an absent key to `null` for payloads that predate it. Restore
  // `turnEndings` and `lastTurnEnding` from the snapshot, defaulting absent keys to `[]` and
  // `null`, so reload preserves ending rows and the band's status.
  return {
    ...snapshot,
    usage: snapshot.usage ?? null,
    accountLimits: snapshot.accountLimits ?? null,
    // A payload from a build that predates the count is a tab with none.
    backgroundCalls: snapshot.backgroundCalls ?? 0,
    // `null` is "no list reported"; a payload from a build that predates the key says the same.
    availableCommands: cleanCommands(snapshot.availableCommands),
    turnEndings: snapshot.turnEndings ?? [],
    lastTurnEnding: snapshot.lastTurnEnding ?? null,
    // Taken from the wire like `assistantMessageOpen`; a payload without them is a conversation with no
    // thinking.
    thinking: snapshot.thinking ?? [],
    thinkingOpen: snapshot.thinkingOpen ?? false,
    nextSeq: throughRevision,
    turnThinking: false,
    ...NOTHING_HELD,
    // The one held list Rust serializes: a change whose call has not started yet.
    changedInputsBeforeTheirCall: snapshot.changedInputsBeforeTheirCall ?? [],
  };
}

/** Whether a `resume_outcome` confirms the session that was actually asked for -- the same
 * predicate `applyEvent`'s own `resume_outcome` arm used to compute inline. Exported so `App.tsx`
 * can decide whether a resume outcome ends the in-flight-motion clock (`turnClock`)
 * without re-deriving this rule a second time; a forked attach is not a substitution because
 * forking legitimately returns a different id. */
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
