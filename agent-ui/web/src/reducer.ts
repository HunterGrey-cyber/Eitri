import * as roster from "./fold/roster";
import * as notices from "./fold/notices";
import * as subagents from "./fold/subagents";
import * as questions from "./fold/questions";
import * as plans from "./fold/plans";
import * as reset from "./fold/reset";
import { cleanCommands } from "./slashCommands";
import { isEditorCardWire } from "./editorCardWire";
import type {
  AgentDomainEvent,
  AgentUiSnapshot,
  AgentUiState,
  EditorCardProposal,
  EditorCardWire,
  PermissionRequestRecord,
  ProviderPrompt,
  Seq,
  ToolCallRecord,
  TurnCause,
  TurnEnding,
  WireProviderPrompt,
  WireTurnCause,
} from "./types";

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
    effort: null,
    cwd: null,
    userPrompts: [],
    transcript: [],
    toolCalls: [],
    status: { kind: "starting" },
    activeTurnId: null,
    pendingPermissions: [],
    editorCards: [],
    capabilities: { resume: false, fork: false, interrupt: false, bypassPermissionMode: false },
    provider: null,
    usage: null,
    accountLimits: null,
    backgroundCalls: 0,
    backgroundTasks: roster.emptyRoster(),
    availableCommands: null,
    turnEndings: [],
    lastTurnEnding: null,
    history: null,
    evicted: null,
    assistantMessageOpen: false,
    thinking: [],
    thinkingOpen: false,
    turnStarts: [],
    notices: [],
    apiRetry: null,
    compacting: false,
    rateLimitWarning: null,
    fallbackModel: null,
    notification: null,
    questions: [],
    plans: [],
    planning: null,
    planMarks: [],
    cliUnseenBusy: false,
    cliUnseenEpisode: 0,
    openingPromptSeq: null,
    runningTurnCli: false,
    nextSeq: 0,
    turnThinking: false,
    ...NOTHING_HELD,
  };
}

/** The proposal a card names, when the wire gave one the page can use: an id and a list of paths. Anything else is
 *  left off, and the card is then viewed as plain text. */
function proposalOfCard(proposal: unknown): { proposal?: EditorCardProposal } {
  if (proposal === null || typeof proposal !== "object") return {};
  const { id, files } = proposal as { id?: unknown; files?: unknown };
  if (typeof id !== "string" || !Array.isArray(files) || files.length === 0 || !files.every((f) => typeof f === "string")) return {};
  return { proposal: { id, files: files as string[] } };
}

/** A broker card as the record the card view draws. `permissionId` is the card's own id, `toolName` the broker tool,
 *  `seq` the wire's: Rust stamps it, and nothing here takes one from `nextSeq`, so the page's numbering stays Rust's
 *  and a turn's footer keeps its anchor. `input` is `null`: no provider request stands behind it. */
export function editorCardRecord(card: EditorCardWire): PermissionRequestRecord {
  return {
    seq: card.seq,
    permissionId: card.id,
    toolUseId: card.toolUseId ?? null,
    toolName: card.tool,
    input: null,
    editorCard: { title: card.title, body: card.body, ...proposalOfCard(card.proposal) },
  };
}

/** A snapshot's card: the wire shape becomes a record, and a record (a state folded already) stays one. */
function asCardRecord(card: EditorCardWire | PermissionRequestRecord): PermissionRequestRecord {
  return "permissionId" in card ? card : editorCardRecord(card);
}

/** A snapshot's card list as records: an entry the card view cannot draw is dropped, as the envelope's is. */
function cardRecords(cards: unknown): PermissionRequestRecord[] {
  if (!Array.isArray(cards)) return [];
  return cards.filter((card) => (card !== null && typeof card === "object" && "permissionId" in card) || isEditorCardWire(card)).map(asCardRecord);
}

/** The `editor_cards` envelope: the tab's cards replaced whole. A card the broker no longer holds is gone from the
 *  list, and nothing else in the state moves. */
export function withEditorCards(state: AgentUiState, cards: readonly EditorCardWire[]): AgentUiState {
  return { ...state, editorCards: cards.map(editorCardRecord) };
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
 * the session that ended, so it resets to exactly what a brand-new mount would show, except the page's count of
 * stretches of unseen work (`cliUnseenEpisode`), which only goes up. */
export function resetToStartScreen(state: AgentUiState): AgentUiState {
  return {
    ...initialState(),
    backend: state.backend,
    capabilities: state.capabilities,
    provider: state.provider,
    // Never counted down: a stop question still up from the ended session must not match the next one's work.
    cliUnseenEpisode: state.cliUnseenEpisode,
  };
}

/**
 * Folds one delivered batch, in order: the same state `events.reduce(applyEvent, state)` gives.
 *
 * A run of consecutive text deltas of one turn is folded as one delta carrying their joined text,
 * and `nextSeq` is then moved on by the rest of the run, which is exactly what folding them one by
 * one does: the first either opens a message (at its own `seq`) or appends to the open one, and
 * every later one appends to that same message, which keeps the first's `seq`. Folding them one by
 * one copies the whole transcript once per delta; a batch of D deltas over M messages copies M x D
 * entries for one render.
 *
 * A run of thinking deltas is folded the same way, with one difference: a thinking delta with no text
 * opens no block, so the empty ones at the head of a run only move `nextSeq` on, and the block the
 * joined delta opens takes the `seq` of the first one that has text, as folding one by one gives it.
 */
export function applyEvents(
  state: AgentUiState, events: readonly AgentDomainEvent[], revisions?: readonly number[],
  onUnknown?: (event: AgentDomainEvent) => void,
): AgentUiState {
  let folded = state;
  let i = 0;
  while (i < events.length) {
    // A host-answered request still consumed a Rust revision even though no card was sent here.
    const revision = revisions?.[i];
    if (revision !== undefined && revision - 1 > folded.nextSeq) {
      folded = { ...folded, nextSeq: revision - 1 };
    }
    const first = events[i];
    let end = i + 1;
    const kind = mergeableKind(first);
    if (kind !== null) {
      const turn = (first as ContentDelta).turn_id;
      while (end < events.length) {
        if (revisions !== undefined &&
          (revisions[end - 1] === undefined || revisions[end] !== revisions[end - 1] + 1)) break;
        const next = events[end];
        if (mergeableKind(next) !== kind || (next as ContentDelta).turn_id !== turn) break;
        end += 1;
      }
    }
    if (end - i === 1) {
      folded = applyEvent(folded, first, onUnknown);
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
 * is what `AgentSessionProjection::apply` does with `last_revision` too. `applyEvents` advances
 * over omitted events using the envelope's per-event revisions before calling this fold. A locally
 * folded item gets the number Rust would have given it, so the live path and a later snapshot
 * describe the same order rather than two schemes that merely happen to agree.
 *
 * Skipping a revision for a mode change or a repeated thinking delta drifts the page's numbering
 * behind Rust's, so a turn footer cannot find the seq Rust names. Batching thinking deltas still
 * costs one fold (`applyEvents`) and one render.
 *
 * The events in one `{kind:"events"}` batch arrive in fold order -- `UiDelivery::Events` is
 * documented as "apply these events, in order", and `ConversationIngest` queues one entry per event
 * it folds. The host can omit requests it answered itself; their revisions remain gaps in the
 * delivered tags. A resync ends in a snapshot, which re-seeds this counter rather than continuing it.
 */
export function applyEvent(incoming: AgentUiState, event: AgentDomainEvent, onUnknown: ((event: AgentDomainEvent) => void) | undefined): AgentUiState;
export function applyEvent(incoming: AgentUiState, event: AgentDomainEvent): AgentUiState;
export function applyEvent(
  incoming: AgentUiState, event: AgentDomainEvent, onUnknown?: ((event: AgentDomainEvent) => void) | number,
): AgentUiState {
  const seq = incoming.nextSeq;
  // Advanced once, here, so every `return state` below carries it -- including the arms that change
  // nothing else. An event with no visible effect is still a real, ordered occurrence.
  //
  // `turnThinking` defaults to false here too, for every event: it is cleared by the ABSENCE of a
  // special case rather than by a maintained list of event types that ought to clear it.
  // Only the `content_delta`/`"thinking"` arm below sets it back to true.
  const preservesActivity = event.type === "history_evicted" || event.type === "turn_cause_updated" || event.type === "cli_cycle_started" || event.type === "effort_reported"
    || event.type === "background_tasks_changed" || event.type === "task_event" || event.type === "conversation_reset"
    || event.type === "subagent_frame" || (event.type === "session_notice" && !notices.recordsRow(event));
  let state: AgentUiState = { ...incoming, nextSeq: seq + 1, turnThinking: preservesActivity ? incoming.turnThinking : false };
  switch (event.type) {
    case "turn_completed":
      state = roster.onTurnCompleted(state, event);
      state = notices.onTurnEnd(state, event);
      break;
    case "content_delta":
      state = notices.onContent(state, event);
      break;
    case "tool_call_started":
      state = notices.onContent(state, event);
      state = questions.onCallStarted(state, event, seq);
      state = plans.onCallStarted(state, event, seq);
      break;
    case "resume_outcome":
      state = { ...roster.onSessionEnd(state, event), effort: null };
      break;
    case "session_closed":
    case "session_unavailable":
      state = roster.onSessionEnd(state, event);
      state = questions.onSessionEnd(state, event);
      state = plans.onSessionEnd(state);
      break;
  }
  if (event.type === "permission_mode_changed") return { ...incoming, nextSeq: seq + 1 };
  // The host's own records of a tool it serves: only a recorded twin holds them.
  if (event.type === "session_servers" || event.type === "client_tool_call" || event.type === "client_tool_call_settled") {
    return { ...incoming, nextSeq: seq + 1 };
  }
  const unknown = typeof onUnknown === "function" ? onUnknown : undefined;
  if (event.type === "subagent_frame") {
    // A nested kind this build knows but does not show is dropped on purpose; only one it has no
    // name for reaches the hook.
    if (unknown) {
      let leaf: unknown = event;
      while (typeof leaf === "object" && leaf !== null && (leaf as { type?: unknown }).type === "subagent_frame") {
        leaf = (leaf as { frame?: unknown }).frame;
      }
      const type = typeof leaf === "object" && leaf !== null ? (leaf as { type?: unknown }).type : undefined;
      if (typeof type !== "string" || !knownEventTypes.has(type)) unknown(leaf as AgentDomainEvent);
    }
    return subagents.fold({ ...state, turnThinking: incoming.turnThinking }, event, foldEvent);
  }
  return foldEvent(state, event, seq, unknown);
}

/** Every event type this build folds; the record's type makes the compiler keep it complete. */
const knownEventTypes: ReadonlySet<string> = new Set(Object.keys({
  history_evicted: true, account_limits: true, assistant_message_boundary: true, background_calls: true, background_tasks_changed: true,
  cli_cycle_started: true, client_tool_call: true, client_tool_call_settled: true, cli_permission_mode: true,
  cli_work_unseen: true, commands_available: true, content_delta: true, conversation_reset: true,
  effort_reported: true, model_reported: true, permission_mode_changed: true, permission_requested: true,
  permission_denied: true, permission_resolved: true, policy_mismatch: true, question_asked: true,
  question_settled: true, resume_outcome: true, plan_proposed: true, plan_settled: true, cli_plan_mode: true,
  plan_transition_started: true, plan_transition_undone: true,
  session_closed: true, session_notice: true, session_opened: true, session_servers: true,
  session_unavailable: true, subagent_frame: true, task_event: true, tool_call_completed: true,
  tool_call_started: true, tool_input_changed: true, turn_attributed: true, turn_cause_updated: true,
  turn_completed: true, turn_started: true, user_message_delivered: true, user_message_dropped: true,
  user_prompt_submitted: true, user_prompt_withdrawn: true, user_prompt_attachments: true,
} satisfies Record<AgentDomainEvent["type"], true>));

function foldEvent(state: AgentUiState, event: AgentDomainEvent, seq: Seq, onUnknown?: (event: AgentDomainEvent) => void): AgentUiState {
  switch (event.type) {
    case "history_evicted": {
      const lifted = new Set(event.lifted);
      const keep = (r: { seq: Seq }) => r.seq >= event.through_seq || lifted.has(r.seq);
      const next = { ...state,
        userPrompts: state.userPrompts.filter(keep), transcript: state.transcript.filter(keep),
        thinking: state.thinking.filter(keep), toolCalls: state.toolCalls.filter(keep),
        turnStarts: state.turnStarts.filter(keep), turnEndings: state.turnEndings.filter(keep),
        notices: state.notices.filter(keep), questions: state.questions.filter(keep), plans: state.plans.filter(keep),
        planMarks: state.planMarks.filter(keep),
        evicted: { throughSeq: event.through_seq, turns: event.turns },
      };
      const owners = new Set([
        ...next.thinking.map((r) => r.turnId), ...next.turnStarts.map((r) => r.turnId),
        ...next.turnEndings.map((r) => r.turnId), ...next.backgroundTasks.rows.map((r) => r.task_id),
      ]);
      if (next.activeTurnId !== null) owners.add(next.activeTurnId);
      const collect = (calls: AgentUiState["toolCalls"]) => {
        for (const call of calls) {
          if (call.turnId !== undefined) owners.add(call.turnId);
          owners.add(call.toolUseId);
          if (call.children) collect(call.children.toolCalls);
        }
      };
      collect(next.toolCalls);
      return { ...next,
        ...(state.sentFrom === undefined ? {} : { sentFrom: Object.fromEntries(
          Object.entries(state.sentFrom).filter(([seq]) => keep({ seq: Number(seq) })),
        ) }),
        ...(state.stoppedFrom === undefined ? {} : { stoppedFrom: Object.fromEntries(
          Object.entries(state.stoppedFrom).filter(([id]) => owners.has(id)),
        ) }),
      };
    }
    case "turn_cause_updated": {
      // The cause the turn's close named replaces what its start said; a turn with no start row keeps
      // none, and an update naming no cause is no news. MUST stay identical to `AgentSessionProjection::apply`.
      if (event.cause === null || event.cause === undefined) return state;
      let at = -1;
      for (let i = state.turnStarts.length - 1; i >= 0; i -= 1) {
        if (state.turnStarts[i].turnId === event.turn_id) {
          at = i;
          break;
        }
      }
      if (at < 0) return state;
      const cause = turnCauseFromWire(event.cause);
      return { ...state, turnStarts: state.turnStarts.map((start, i) => (i === at ? { ...start, cause } : start)) };
    }
    case "cli_cycle_started": {
      // Claude Code ran work of its own ahead of the running turn's prompt, which it has not read yet: the
      // prompt leaves the conversation (the tab holds it until its delivery folds it back where Claude Code
      // read it), and why the work started is a row. MUST stay identical to `AgentSessionProjection::apply`.
      if (state.activeTurnId !== event.turn_id) return state;
      const opening = state.openingPromptSeq;
      const userPrompts =
        opening === null ? state.userPrompts : state.userPrompts.filter((p) => p.seq !== opening || p.delivered === true);
      return {
        ...state,
        userPrompts,
        openingPromptSeq: null,
        assistantMessageOpen: false,
        thinkingOpen: false,
        turnStarts: [
          ...state.turnStarts,
          { seq, turnId: event.turn_id, origin: "cli", cause: event.cause ? turnCauseFromWire(event.cause) : null },
        ],
      };
    }
    case "background_tasks_changed":
    case "task_event":
      return roster.fold(state, event);
    case "conversation_reset":
      return reset.fold(state, event);
    case "subagent_frame":
      return state;
    case "effort_reported":
      return { ...state, effort: event.effort };
    case "model_reported":
      return { ...state, model: event.model };
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
      // Until its `turn_attributed` says otherwise, a turn is one this host sent.
      return {
        ...state,
        activeTurnId: event.turn_id,
        assistantMessageOpen: false,
        thinkingOpen: false,
        lastTurnEnding: null,
        runningTurnCli: false,
      };
    case "assistant_message_boundary":
      // Two messages with no tool call between them are two entries, as `AgentSessionProjection`
      // folds them.
      return { ...state, assistantMessageOpen: false, thinkingOpen: false };
    case "user_prompt_submitted":
      // `assistantMessageOpen: false` for the same reason `AgentSessionProjection::apply` does it:
      // a prompt can only occur between assistant messages, and leaving the run open appends the
      // next turn's reply to the last one.
      // A prompt folded with no turn running is what the next turn opens with.
      return {
        ...state,
        userPrompts: [...state.userPrompts, { seq, text: event.text }],
        assistantMessageOpen: false,
        thinkingOpen: false,
        openingPromptSeq: state.activeTurnId === null ? seq : state.openingPromptSeq,
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
      // `marked` -- and this app has no error boundary, so the whole panel unmounts. Same shape,
      // and same reason, as the `default:` arm at the bottom of this switch.
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
        assistantMessageOpen: subagents.isChildCall(state.toolCalls, event.tool_use_id) ? state.assistantMessageOpen : false,
        thinkingOpen: subagents.isChildCall(state.toolCalls, event.tool_use_id) ? state.thinkingOpen : false,
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
            // The CLI's own prompt, reshaped to the snapshot's camelCase so a card built from
            // events and one rebuilt from a snapshot read the same. Absent on the gate's request.
            ...(event.provider_prompt ? { providerPrompt: providerPromptFromWire(event.provider_prompt) } : {}),
          },
        ],
      };
    case "permission_resolved":
      // The authoritative source for "this permission card is gone": Rust sends it when the
      // request is resolved, never as a frontend-local guess.
      return { ...state, pendingPermissions: state.pendingPermissions.filter((p) => p.permissionId !== event.permission_id) };
    case "turn_completed": {
      // A completion naming a turn this page is not running (another runs, or none does) ends nothing
      // here: the running turn runs on, and a turn the panel never showed leaves no ending row. What it
      // said arrives as the events Rust derives after it. Its usage is still the CLI's latest report,
      // and it ends the note of unseen work. MUST stay identical to `AgentSessionProjection::apply`.
      if (state.activeTurnId !== event.turn_id) {
        return { ...state, usage: event.usage ?? state.usage, cliUnseenBusy: false };
      }
      // The authoritative source for "no turn is in flight": a finished turn does
      // NOT end the conversation, only session_closed/session_unavailable do.
      //
      // `usage`, mirroring `AgentSessionProjection::apply`: a report REPLACES the figure whole
      // -- the SDK reports a running total that `/clear` resets, so a lower later figure is the
      // truth, never summed or maxed -- and a turn that reported none (`null`: interrupted,
      // synthesized, a sidecar without `TurnUsage`) leaves the last one standing. Silence is not a
      // measurement, so it must neither erase a real figure nor become a zero.
      //
      // A turn that did not complete (failed, hit a limit, was interrupted) leaves a turn-ending row
      // where it stopped, with the provider's own words, and the band says it until the next turn
      // starts. A completed one adds nothing: it needs no explaining, and `lastTurnEnding` is already
      // null (only `turn_started` clears it, and only a non-completed end sets it). MUST stay identical
      // to `AgentSessionProjection::apply`. `detail` is read defensively: Rust always sends it, but an
      // event without one must still fold to a row rather than throw inside the reducer.
      // A turn Claude Code started that showed nothing and ended without a result leaves nothing: no row
      // saying why it started, and no failed ending for work nobody saw. MUST stay identical to Rust.
      let emptyStart = -1;
      if (state.runningTurnCli && event.detail?.reason === NO_RESULT_REASON) {
        for (let i = state.turnStarts.length - 1; i >= 0; i -= 1) {
          if (state.turnStarts[i].turnId === event.turn_id) {
            if (!holdsItemAfter(state, state.turnStarts[i].seq)) emptyStart = i;
            break;
          }
        }
      }
      const done = {
        ...state,
        cliUnseenBusy: false,
        activeTurnId: null,
        assistantMessageOpen: false,
        thinkingOpen: false,
        usage: event.usage ?? state.usage,
        openingPromptSeq: null,
        runningTurnCli: false,
        ...NOTHING_HELD,
      };
      if (emptyStart >= 0) return { ...done, turnStarts: state.turnStarts.filter((_, i) => i !== emptyStart) };
      if (event.outcome === "completed") return done;
      const detail = event.detail ?? { reason: null, api_error_status: null, message: null };
      const ending: TurnEnding = {
        seq,
        turnId: event.turn_id,
        kind: event.outcome,
        reason: detail.reason ?? null,
        apiErrorStatus: detail.api_error_status ?? null,
        message: detail.message ?? null,
        ...(detail.origin_kind === undefined ? {} : { originKind: detail.origin_kind }),
        ...(detail.num_turns === undefined ? {} : { numTurns: detail.num_turns }),
        resetsAt: state.accountLimits?.resetsAt ?? null,
      };
      return { ...done, turnEndings: [...state.turnEndings, ending], lastTurnEnding: event.outcome };
    }
    // Both endings clear activeTurnId and cliUnseenBusy, mirroring AgentSessionProjection exactly: no
    // turn_completed is ever coming, for this page's turn or for work it could not show, so leaving
    // activeTurnId set leaves App.tsx's turnInProgress true forever -- a spinner on a dead session,
    // next to a reply that may be truncated. Clearing it is not a local guess at
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
        openingPromptSeq: null,
        runningTurnCli: false,
        cliUnseenBusy: false,
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
        openingPromptSeq: null,
        runningTurnCli: false,
        cliUnseenBusy: false,
        status: { kind: "unavailable", reason: event.reason },
        assistantMessageOpen: false,
        thinkingOpen: false,
        ...NOTHING_HELD,
      };
    case "session_closed":
      return {
        ...withLostTurn(state, seq),
        activeTurnId: null,
        openingPromptSeq: null,
        runningTurnCli: false,
        cliUnseenBusy: false,
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
      const updated = subagents.updateCall(state.toolCalls, id, (call) => ({ ...call, denied }));
      if (updated) return { ...state, toolCalls: updated };
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
      const updated = subagents.updateCall(state.toolCalls, id, (call) => ({ ...call, changedInput: event.change }));
      if (updated) return { ...state, toolCalls: updated };
      return {
        ...state,
        changedInputsBeforeTheirCall: pushBounded(state.changedInputsBeforeTheirCall, { toolUseId: id, change: event.change }),
      };
    }
    case "turn_attributed": {
      // A row only for a turn the user did not send; the `turn_started` before it already closed what was
      // open. MUST stay identical to `AgentSessionProjection::apply`.
      if (state.activeTurnId === event.turn_id) state = { ...state, runningTurnCli: event.origin === "cli" };
      if (event.origin === "client") return state;
      return {
        ...state,
        turnStarts: [
          ...state.turnStarts,
          { seq, turnId: event.turn_id, origin: event.origin, cause: event.cause ? turnCauseFromWire(event.cause) : null },
        ],
      };
    }
    case "session_notice":
      return notices.fold(state, event);
    case "cli_work_unseen":
      // Work out of sight: noted, and nothing else -- no turn runs here because of it. Claude Code sends one for
      // every message it writes there, so only the first after none was under way begins a new stretch.
      if (state.cliUnseenBusy) return { ...state };
      return { ...state, cliUnseenBusy: true, cliUnseenEpisode: state.cliUnseenEpisode + 1 };
    case "user_prompt_withdrawn": {
      // The newest prompt not put into a running turn goes; a delivered one is part of its turn and
      // stays. Nothing to take back changes nothing. MUST stay identical to `AgentSessionProjection::apply`.
      let at = -1;
      for (let i = state.userPrompts.length - 1; i >= 0; i--) {
        if (state.userPrompts[i].delivered !== true) {
          at = i;
          break;
        }
      }
      if (at < 0) return state;
      const removed = state.userPrompts[at].seq;
      return {
        ...state,
        userPrompts: state.userPrompts.filter((_, i) => i !== at),
        openingPromptSeq: state.openingPromptSeq === removed ? null : state.openingPromptSeq,
      };
    }
    case "user_prompt_attachments": {
      // The images belong to the prompt they were sent with, the newest one not put into a turn; nothing to
      // attach them to changes nothing. MUST stay identical to `AgentSessionProjection::apply`. Rust's
      // `AttachmentMeta` is snake_case on the wire; the record keeps what the row draws.
      let at = -1;
      for (let i = state.userPrompts.length - 1; i >= 0; i--) {
        if (state.userPrompts[i].delivered !== true) {
          at = i;
          break;
        }
      }
      if (at < 0) return state;
      const attachments = event.attachments.map((image) => ({
        mediaType: image.media_type, width: image.width, height: image.height, bytes: image.bytes,
      }));
      return {
        ...state,
        userPrompts: state.userPrompts.map((prompt, i) => (i === at ? { ...prompt, attachments } : prompt)),
      };
    }
    case "question_asked":
    case "question_settled":
      return questions.fold(state, event);
    case "plan_proposed":
    case "plan_settled":
      return plans.fold(state, event);
    // The host's reading of plan mode: it reaches the page as `planningChanged` beside the events.
    case "cli_plan_mode":
    case "plan_transition_started":
    case "plan_transition_undone":
      return state;
    case "permission_mode_changed":
      // The mode lives on the tab (`tabs` envelope), not the projection: `TabInfo.mode` is what the
      // band and the box read, and Rust's own `tabs` envelope already reflects an acknowledged
      // switch. Folding a second copy here would just be a value that can drift from it, so this
      // event changes nothing the page draws. It still advances `nextSeq`, as Rust bumps
      // `last_revision` for it: the seqs Rust names (a turn's footer anchor) must be the page's own.
      // `turnThinking` is left as it was: a mode switch says nothing about what the agent is doing.
      return state;
    default: {
      // Exhaustiveness guard: a new AgentDomainEvent variant added on the Rust side without a
      // matching TS case lands here at runtime -- observable, never silently dropped.
      console.warn("agent-ui: completely unhandled event shape from Rust", event);
      onUnknown?.(event);
      return state;
    }
  }
}

/** What the events envelope says beyond its events (Rust's `CallNotes`): the saved rule that
 *  answered a call and the `Write` cards raised over no file. */
export type CallNotes = {
  sentFrom?: AgentUiState["sentFrom"];
  stoppedFrom?: AgentUiState["stoppedFrom"];
  ruleNotes?: readonly { toolUseId: string; rule: string }[];
  createsFile?: readonly { permissionId: string; toolUseId: string | null }[];
  /** Calls whose CLI prompt was answered without a card, with their row's note. */
  promptNotes?: readonly { toolUseId: string; note: string }[];
  /** Tool-use ids of a `Write`/`Edit`/`NotebookEdit` the acceptEdits fast path
   *  answered with no card. No message of its own, unlike `ruleNotes` -- there is only one fast
   *  path, so the row just says "allowed by auto" (`toolRegistry.tsx`). */
  autoNotes?: readonly string[];
  /** Tool-use ids of a `Write` answered with no card -- by the acceptEdits
   *  fast path, or in bypass -- over a path where nothing existed
   *  just before the answer was sent. `createsFile`'s own signal is keyed by permission id and only
   *  ever set for a call that raised a card -- a `Write` answered without one raises none, so
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
    notes.sentFrom === undefined && notes.stoppedFrom === undefined &&
    rules.size === 0 &&
    newFileCards.size === 0 &&
    autoNewFileCalls.size === 0 &&
    promptNotes.size === 0 &&
    autoNotes.size === 0
  )
    return state;
  return {
    ...state,
    ...(notes.sentFrom === undefined ? {} : { sentFrom: { ...state.sentFrom, ...notes.sentFrom } }),
    ...(notes.stoppedFrom === undefined ? {} : { stoppedFrom: { ...state.stoppedFrom, ...notes.stoppedFrom } }),
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

/** `TurnEndDetail.reason` of a turn Claude Code went idle in without a result (Rust's `NO_RESULT_REASON`). */
export const NO_RESULT_REASON = "no_result";

/** Whether any item of the conversation was folded after `seq` (Rust's `holds_item_after`). */
function holdsItemAfter(state: AgentUiState, seq: Seq): boolean {
  const after = (items: readonly { seq: Seq }[]) => items.some((item) => item.seq > seq);
  return (
    after(state.userPrompts) ||
    after(state.transcript) ||
    after(state.toolCalls) ||
    after(state.thinking) ||
    after(state.notices) ||
    after(state.questions) ||
    after(state.plans) ||
    after(state.turnEndings) ||
    after(state.turnStarts) ||
    after(state.pendingPermissions)
  );
}

/** The event's snake_case cause as the snapshot spells it (`agent_bridge.rs`). */
function turnCauseFromWire(wire: WireTurnCause): TurnCause {
  return {
    kind: wire.kind,
    ...(wire.tool_use_id == null ? {} : { toolUseId: wire.tool_use_id }),
    taskId: wire.task_id ?? null,
    taskKind: wire.task_kind ?? null,
    taskStatus: wire.task_status ?? null,
    description: wire.description ?? null,
    summary: wire.summary ?? null,
    peerName: wire.peer_name ?? null,
    text: wire.text ?? null,
    wakeupSource: wire.wakeup_source ?? null,
    scheduledForMs: wire.scheduled_for_ms ?? null,
    scheduleReason: wire.schedule_reason ?? null,
    cronId: wire.cron_id ?? null,
    prompt: wire.prompt ?? null,
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
    openingPromptSeq: null,
    runningTurnCli: false,
    pendingPermissions: [],
    editorCards: [],
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
export function applySnapshot(previous: AgentUiState, snapshot: AgentUiSnapshot, throughRevision: number): AgentUiState {
  // The snapshot carries neither `nextSeq` nor `turnThinking` (nor `denialsBeforeTheirCall`, which
  // Rust keeps in a field of its own that is not serialized) -- all reducer-internal on both
  // sides and deliberately not on the wire, which is why the parameter is typed `AgentUiSnapshot`
  // (the `Omit` of exactly those) rather than a full `AgentUiState` that would claim Rust sent
  // them. Supplying them here is the only reason this function exists.
  //
  // `assistantMessageOpen` comes from the snapshot: Rust keeps appending to the open message
  // underneath a mid-reply snapshot. Forcing it false would split that reply into two rows with
  // broken markdown at the seam on a tab switch, `prefix r` or bounded-queue Resync. See the field
  // on `AgentUiState`.
  //
  // Resetting `turnThinking` IS still forced: a thinking delta folded before the snapshot was taken
  // must not still read as "thinking now"; the invariant is that
  // this bit can only ever be LOST, and a resync/reload is one more way to lose it, degrading
  // `thinking` to `sent`. Unlike `assistantMessageOpen`, Rust never sends this one at
  // all, so there is nothing on `snapshot` to take it from.
  //
  // `usage` comes from the snapshot like every other field: it is per tab, so a tab that has
  // reported nothing must not keep the figure of the one shown before it. `?? null` is the runtime
  // defence for a payload without the key (Rust always sends it, `null` included): `usageSegment`
  // reads `null` as unknown and would throw on `undefined`.
  //
  // `accountLimits` is read the same way: the snapshot carries the latest report (`null` before any), and
  // `?? null` covers a payload from a build that predates the key.
  //
  // `turnEndings` and `lastTurnEnding` come from the wire like the other collections, with the same
  // defence for a payload without them (`[]`/`null`), so a reload keeps the rows and the band's word.
  let state: AgentUiState = {
    ...initialState(),
    ...snapshot,
    evicted: snapshot.evicted ?? null,
    usage: snapshot.usage ?? null,
    accountLimits: snapshot.accountLimits ?? null,
    // A payload from a build that predates the count is a tab with none.
    backgroundCalls: snapshot.backgroundCalls ?? 0,
    backgroundTasks: roster.emptyRoster(),
    // `null` is "no list reported"; a payload from a build that predates the key says the same.
    availableCommands: cleanCommands(snapshot.availableCommands),
    turnEndings: snapshot.turnEndings ?? [],
    lastTurnEnding: snapshot.lastTurnEnding ?? null,
    // Taken from the wire like `assistantMessageOpen`; a payload without them is a conversation with no
    // thinking.
    thinking: snapshot.thinking ?? [],
    thinkingOpen: snapshot.thinkingOpen ?? false,
    // Rust sends each only while it holds any: a payload without one is a conversation with none.
    turnStarts: snapshot.turnStarts ?? [],
    notices: snapshot.notices ?? [],
    questions: snapshot.questions ?? [],
    // Sent only while set: a payload without it is a tab with no unseen work noted.
    cliUnseenBusy: snapshot.cliUnseenBusy ?? false,
    // Not on the wire: the page cannot tell whether unseen work the snapshot shows is the stretch it knew, so it is
    // a new one.
    cliUnseenEpisode: previous.cliUnseenEpisode + 1,
    // Both sent only while set.
    openingPromptSeq: snapshot.openingPromptSeq ?? null,
    runningTurnCli: snapshot.runningTurnCli ?? false,
    nextSeq: throughRevision,
    turnThinking: false,
    // The broker's cards, wire shape in the snapshot; a payload without the key is a tab with none.
    editorCards: cardRecords(snapshot.editorCards),
    ...NOTHING_HELD,
    // The one held list Rust serializes: a change whose call has not started yet.
    changedInputsBeforeTheirCall: snapshot.changedInputsBeforeTheirCall ?? [],
  };
  state = roster.fromSnapshot(state, snapshot);
  state = notices.fromSnapshot(state, snapshot);
  state = subagents.fromSnapshot(state, snapshot);
  state = questions.fromSnapshot(state, snapshot);
  state = plans.fromSnapshot(state, snapshot);
  return reset.fromSnapshot(state, snapshot);
}

/** Whether a `resume_outcome` confirms the session that was actually asked for. Exported so `App.tsx`
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
