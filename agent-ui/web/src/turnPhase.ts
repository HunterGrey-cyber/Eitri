import type { AgentUiState } from "./types";

/**
 * The agent's current phase for the motion indicator, derived only from `AgentUiState`.
 * The caller uses it while `status.kind === "running" && activeTurnId !== null`.
 * A pending permission produces `blocked`: the agent is waiting for the user,
 * so the caller must hide the motion indicator.
 */
export type TurnPhase =
  | { kind: "sent" }
  | { kind: "thinking" }
  | { kind: "replying" }
  | { kind: "tool"; toolName: string }
  | { kind: "blocked" };

/**
 * The order of these checks determines the visible phase:
 *
 * 1. A pending permission overrides everything else, regardless of sequence ordering.
 * 2. Use the newest unfinished tool call unless later assistant text, thinking, or a new
 *    user prompt displaces it.
 * 3. Check thinking before transcript messages because thinking can resume after text.
 * 4. If the last item is a transcript message, use `replying`.
 * 5. Otherwise use `sent`, including when only the user's prompt has arrived.
 *
 * A tool call need not be the last item. A parallel call can complete while it is still
 * running, and a denial creates no item, leaving the denied call's `result` null.
 * Later assistant text or thinking displaces a denied call. A newer tool call only
 * changes which unfinished call names the phase; it does not displace other live calls.
 *
 * `toolCalls` persists between turns. Interrupted and denied calls can retain null results,
 * so a later prompt must prevent an old call from naming the next turn's initial phase.
 * Only calls from earlier turns are displaced by a prompt. `user_message_delivered` can
 * add a prompt during a running turn without stopping that turn's tool calls.
 *
 * `turnThinking` is ephemeral and is cleared by snapshots and unrelated events.
 * `thinkingOpen` is persisted as `thinking_open` while the newest thought is being written.
 * Replies, calls, prompts, and turn endings close it in both folds. Either flag produces
 * `thinking`, so snapshots and unrelated events preserve a thought that is still open.
 * A thinking delta with no text opens no block and can still be lost: without an unfinished
 * call it degrades to `sent`; with one and no later text it degrades to `tool`.
 * A denied call cannot be distinguished from a live call in the snapshot. The next
 * thinking delta or assistant text displaces it.
 *
 * `permission_resolved` carries an outcome, but `toolCalls` stores no verdict in
 * `AgentSessionProjection::apply`. The reducer must match that snapshot shape; a verdict
 * stored only in the frontend would disappear on resync. Persisting it needs a wire change.
 *
 * Read `userPrompts`, `transcript`, and `toolCalls` directly. `buildTimeline` moves linked
 * permission cards under their calls for display, so its order is not chronology.
 */
export function phaseOf(state: AgentUiState): TurnPhase {
  if (state.pendingPermissions.length > 0) return { kind: "blocked" };

  // Four running maxima in one pass per collection, comparing strictly greater than what came
  // before -- correct regardless of collection order, since only the running maximum survives.
  //
  // `lastMessageSeq` and `lastPromptSeq` are separate from `lastItemSeq` on purpose: rule 2 asks
  // about two SPECIFIC displacers (assistant text, and the user's own next prompt), and a later
  // tool call is neither. Rules 4 and 5 still want the overall last item, which is what
  // `lastItemSeq`/`lastIsMessage` carry.
  let lastItemSeq = -1;
  let lastIsMessage = false;
  let lastMessageSeq = -1;
  let lastPromptSeq = -1;
  for (const prompt of state.userPrompts) {
    if (prompt.seq > lastPromptSeq) lastPromptSeq = prompt.seq;
    if (prompt.seq > lastItemSeq) {
      lastItemSeq = prompt.seq;
      lastIsMessage = false;
    }
  }
  for (const message of state.transcript) {
    if (message.seq > lastMessageSeq) lastMessageSeq = message.seq;
    if (message.seq > lastItemSeq) {
      lastItemSeq = message.seq;
      lastIsMessage = true;
    }
  }
  for (const call of state.toolCalls) {
    if (call.seq > lastItemSeq) {
      lastItemSeq = call.seq;
      lastIsMessage = false;
    }
  }

  // The NEWEST unfinished call: with several in flight, the phase is named after the one that
  // started last, and a sibling completing changes nothing about it.
  let openTool: { seq: number; name: string; turnId: string | undefined } | null = null;
  for (const call of state.toolCalls) {
    if (call.result === null && (openTool === null || call.seq > openTool.seq)) {
      openTool = { seq: call.seq, name: call.name, turnId: call.turnId };
    }
  }
  // The three displacers, in the order the doc comment lists them.
  //
  // `lastPromptSeq < openTool.seq` is the one that survives a turn boundary: an interrupted or
  // denied call keeps `result: null` forever and `toolCalls` is never cleared, so without this the
  // call would still be "the newest unfinished call" in the NEXT turn.
  //
  // `!thinking` is "no thought has started after it": `turnThinking` is set by a thinking delta and
  // cleared by EVERY other event (reducer.ts), and `thinkingOpen` stays true from a thought's first
  // text until something closes it -- a call starting among those things, so an open thought is
  // necessarily after the tool call. Rule 3 directly below then names that state, rather than this
  // clause silently dropping to `sent`.
  const thinking = state.turnThinking || state.thinkingOpen;
  //
  // A prompt after a call of the RUNNING turn displaces nothing: it was put into that turn while the call ran.
  const ofRunningTurn = openTool !== null && state.activeTurnId !== null && openTool.turnId === state.activeTurnId;
  if (
    openTool !== null &&
    lastMessageSeq < openTool.seq &&
    (lastPromptSeq < openTool.seq || ofRunningTurn) &&
    !thinking
  ) {
    return { kind: "tool", toolName: openTool.name };
  }

  if (thinking) return { kind: "thinking" };
  if (lastIsMessage) return { kind: "replying" };
  return { kind: "sent" };
}

/** The label `TurnActivity` renders for a phase, distinguishing activity states with words. */
export function phaseWord(phase: TurnPhase): string {
  switch (phase.kind) {
    case "sent":
      // Nothing has come back yet: the agent is at work, which is what Claude Code says too.
      return "working";
    case "thinking":
      return "thinking";
    case "replying":
      return "replying";
    case "tool":
      return `running ${phase.toolName}`;
    case "blocked":
      return "waiting for you";
  }
}
