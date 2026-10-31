import type { AgentDomainEvent, AgentUiSnapshot, AgentUiState } from "../types";

/* `/clear`: Claude Code starts a new conversation in the same process. The view keeps nothing of the
 * conversation before it and keeps what the process still holds, as Rust's `projection::reset` does:
 * the session's identity and status, the running turn (the `/clear` turn ends after the reset), cards
 * still waiting with the calls they ask about, questions still waiting, the count of commands left
 * running, the account's limits, the command list, the live band fields and the held bookkeeping. */

/** The conversation's fields, as a reset leaves them. */
const EMPTIED = {
  userPrompts: [],
  transcript: [],
  turnEndings: [],
  lastTurnEnding: null,
  turnStarts: [],
  // The prompt row it points at goes with the conversation.
  openingPromptSeq: null,
  notices: [],
  // The old conversation's context; the next report is the new one's.
  usage: null,
  assistantMessageOpen: false,
  thinking: [],
  thinkingOpen: false,
  // What was restored belonged to the old conversation.
  history: null,
} satisfies Partial<AgentUiState>;

/** The fields the process still holds, which a reset leaves as they are. */
const KEPT = [
  "backend", "conversationId", "sessionId", "model", "cwd", "status", "activeTurnId", "pendingPermissions",
  "capabilities", "provider", "accountLimits", "backgroundCalls", "backgroundTasks", "availableCommands", "apiRetry",
  "compacting", "rateLimitWarning", "fallbackModel", "effort", "notification", "cliUnseenBusy", "nextSeq", "turnThinking",
  "denialsBeforeTheirCall", "changedInputsBeforeTheirCall", "runningTurnCli",
] as const satisfies readonly (keyof AgentUiState)[];

/** Kept in part (`fold` below says which part). */
type Filtered = "providerSessionId" | "toolCalls" | "questions";

// A field added to the state does not compile here until it is placed in one of the three lists.
type Unplaced = Exclude<keyof AgentUiState, keyof typeof EMPTIED | (typeof KEPT)[number] | Filtered>;
const everyFieldPlaced: [Unplaced] extends [never] ? true : Unplaced = true;
void everyFieldPlaced;

export function fold(state: AgentUiState, event: AgentDomainEvent): AgentUiState {
  if (event.type !== "conversation_reset") return state;
  const waitingCalls = new Set(state.pendingPermissions.map((card) => card.toolUseId));
  return {
    ...state,
    ...EMPTIED,
    // The id the CLI names for the new conversation, until its first `session_opened` states it.
    providerSessionId: event.new_conversation_id || state.providerSessionId,
    // A waiting card's call stays with it: its answer and its result still land on that row.
    toolCalls: state.toolCalls.filter((call) => waitingCalls.has(call.toolUseId)),
    questions: state.questions.filter((set) => set.state.kind !== "settled"),
  };
}

export function fromSnapshot(state: AgentUiState, _snapshot: AgentUiSnapshot): AgentUiState { return state; }
