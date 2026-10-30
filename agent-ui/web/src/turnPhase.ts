import type { AgentUiState } from "./types";

/**
 * What the agent is doing right now, for the in-flight motion indicator.
 * Five phases, read off `AgentUiState` alone -- this is
 * a pure function, called only while `ActivityLine`'s own `working` predicate (`status.kind ===
 * "running" && activeTurnId !== null`) is true.
 *
 * `blocked` is not a variant of "in flight": while a permission card is up the agent is not
 * working, the user is, and the caller must not render the meter for it.
 */
export type TurnPhase =
  | { kind: "sent" }
  | { kind: "thinking" }
  | { kind: "replying" }
  | { kind: "tool"; toolName: string }
  | { kind: "blocked" };

/**
 * The rules, in order; each rule's place in the list is load-bearing:
 *
 * 1. A pending permission blocks, whatever the `seq` ordering against the tool call it gates.
 * 2. `running <Tool>` holds while the newest tool call is unfinished and none of three displacers
 *    arrived after it: assistant text, a thinking delta, or a prompt of a later turn.
 * 3. Thinking, checked before the last-item-is-a-message rule: thinking can resume after text has
 *    streamed, and rule 4 alone would report `replying` meanwhile.
 * 4. A transcript message as the last item reads `replying`.
 * 5. Otherwise (the last item is the user's prompt, or nothing has landed) the phase is `sent`.
 *
 * Rule 2 does not ask whether the open call is the latest ITEM. That qualifier failed twice:
 * - Parallel calls: `reducer.ts` notes that a turn can have several of the same tool in flight. With
 *   A and B started and B finishing first, B's record is still the newest item, so A, genuinely
 *   running, fell through to `sent`. A sibling completing says nothing about another call.
 * - A denial creates no item and leaves `result: null`, so the call stayed the last item and kept
 *   reading `running` for a call the user had refused; a thinking delta could not displace it
 *   because rule 2 sits ahead of rule 3. Text or thinking displaces it now.
 *
 * A newer tool call is deliberately not a displacer; it only renames the phase to the newest
 * unfinished call. A new prompt is the opposite. `toolCalls` is never cleared between turns, and an
 * interrupted or denied call keeps `result: null` forever. Without a prompt displacer it would be
 * carried into the next turn as the newest unfinished call, and nothing could displace it: a prompt
 * is not text and the interrupted model emits no further delta. `ActivityLine` shows `running` from
 * `turn_started`, so the stale call would animate through the round trip between Enter and the
 * model's first delta, naming a tool the user just stopped or refused, in the window `sent` exists
 * to name. A prompt delivered inside the running turn (`user_message_delivered`) displaces none of
 * that turn's calls: they are still running.
 *
 * `turnThinking` is ephemeral and can only ever be lost, never invented: `applySnapshot` and any
 * other event clear it. `thinkingOpen` is on the wire and stays true until a reply, call, prompt or
 * turn end closes the thought in both folds, so a snapshot taken mid-thought keeps `thinking`.
 * A resync can still lose a textless thinking delta, which opens no block: `thinking` becomes `sent`
 * with no open call, or `tool` with one and no newer text. For a live call that is correct and is the
 * common case; a denied call can look running until another delta arrives, and this side cannot
 * tell them apart because the snapshot records no card's denial. Never invent thinking to hide that.
 *
 * A `permission_resolved` event does carry an `outcome` (`"denied"` among them), and remembering it
 * per tool call would make rule 2 exact rather than displaceable. It is deliberately not done here:
 * `toolCalls` is a SNAPSHOT collection that `reducer.ts` must fold identically to
 * `AgentSessionProjection::apply`, and Rust records no card's verdict there (a call's `denied` holds
 * only the CLI's own refusal), so a frontend-only field would
 * be dropped by the very resync described above and cost the property that keeps the two folds
 * honest. Closing it properly is a wire change.
 *
 * Read raw collections, not `buildTimeline`: its permission anchoring is display order, not chronology.
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
    if (call.background) continue;
    if (call.seq > lastItemSeq) {
      lastItemSeq = call.seq;
      lastIsMessage = false;
    }
  }

  // The NEWEST unfinished call: with several in flight, the phase is named after the one that
  // started last, and a sibling completing changes nothing about it.
  let openTool: { seq: number; name: string; turnId: string | undefined } | null = null;
  for (const call of state.toolCalls) {
    if (call.background) continue;
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

/** The exact word (or words) `TurnActivity` renders for a phase -- one word per phase: four
 * distinguishable words replace four separate animations, which read as noise rather than state. */
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

/** Transient CLI phases take the same activity slot as the ordinary turn phase. */
export function cliPhaseText(state: AgentUiState): string | null {
  if (state.apiRetry) {
    const s = state.apiRetry.scalars;
    const error = s.error_kind ?? s.error_status ?? state.apiRetry.content ?? "API error";
    const delay = typeof s.retry_delay_ms === "number" ? ` · Retrying in ${Math.ceil(s.retry_delay_ms / 1000)}s` : "";
    const attempt = typeof s.attempt === "number" ? ` · attempt ${s.attempt}${typeof s.max_retries === "number" ? `/${s.max_retries}` : ""}` : "";
    return `${error}${delay}${attempt}`;
  }
  if (state.compacting) return "Compacting conversation…";
  if (state.accountLimits && state.accountLimits.usedPercent >= 100 && state.accountLimits.resetsAt !== null) {
    const time = new Date(state.accountLimits.resetsAt * 1000).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", hour12: false });
    return `Usage limit reached · limit resets ${time} · continuing automatically at ${time}`;
  }
  return null;
}
