import type { AgentUiState } from "./types";

/**
 * What the agent is doing right now, for the in-flight motion indicator
 * (`2026-09-20-in-flight-motion-design.md`). Five phases, read off `AgentUiState` alone -- this is
 * a pure function, called only while `ActivityLine`'s own `working` predicate (`status.kind ===
 * "running" && activeTurnId !== null`) is true (V2, session tabs Task 10; formerly `StatusLine`).
 *
 * `blocked` is not a variant of "in flight": while a permission card is up the agent is not
 * working, the user is, and the caller must not render the meter for it (design §2, §5.1).
 */
export type TurnPhase =
  | { kind: "sent" }
  | { kind: "thinking" }
  | { kind: "replying" }
  | { kind: "tool"; toolName: string }
  | { kind: "blocked" };

/**
 * Design doc §8.3, **as amended by the whole-branch review of 2026-09-20** -- each rule's place in
 * the list is load-bearing, not incidental:
 *
 * 1. A pending permission overrides everything else, regardless of `seq` ordering against the tool
 *    call it gates (unverified whether that ordering ever matters in practice; the override does
 *    not depend on it either way).
 * 2. `running <Tool>` holds when there is an unfinished tool call -- the NEWEST one, which is the
 *    one the phase is named after -- **and** none of its three displacers has arrived after it:
 *    assistant text, a thinking delta, or a new user prompt. See the block comment below: this
 *    clause replaced an `openTool.seq === lastItemSeq` qualifier that was wrong in two reproduced
 *    ways, and its own first version, which named only the first two displacers, was wrong in a
 *    third.
 * 3. Thinking, checked BEFORE the last-item-is-a-message rule: thinking can resume after text has
 *    already streamed, and rule 4 alone would misreport `replying` while it does.
 * 4. The last item being a transcript message reads `replying`.
 * 5. Otherwise (the last item is your own prompt, or nothing has landed at all) reads `sent`.
 *
 * ## Rule 2, and why it does not ask whether the tool call is the last ITEM
 *
 * It used to. The qualifier was `openTool.seq === lastItemSeq`, written to stop a DENIED call
 * (whose `result` stays `null` forever, `agent/src/projection.rs`'s `PermissionResolved` arm) from
 * reading `running <Tool>` for the rest of the turn. The whole-branch review reproduced two defects
 * in it, both of which come from asking the wrong question:
 *
 * - **Parallel calls.** `reducer.ts`'s own `permission_requested` comment says "a turn can have
 *   several of the same tool in flight". With A and B both started and B completing first, B's
 *   record is still the highest-`seq` ITEM, so A -- genuinely running -- failed the qualifier and
 *   the phase fell through to `sent`, whose own definition is "the last thing on the timeline is
 *   your prompt, nothing has come back yet". A sibling call completing is not evidence about this
 *   call.
 * - **A denial.** `permission_resolved` creates no item, so after a denial the tool call is still
 *   the last item and the qualifier still matched -- the panel animated "running Bash" for a call
 *   the user had just refused, and a thinking delta (which also creates no item) could not displace
 *   it, because rule 2 sits ahead of rule 3.
 *
 * So rule 2 stopped asking "is the tool call the latest item" and asks "is the tool still what is
 * happening". A denial is followed by the model thinking or answering, and **either** displaces it,
 * which is what closes the case the old qualifier was written for.
 *
 * ### The three displacers, and the asymmetry between them
 *
 * An open tool call stops being the phase when any of these has arrived after it:
 *
 * - **assistant TEXT** (`lastMessageSeq`) -- the model has moved on to answering;
 * - **a THINKING delta** (`state.turnThinking`) -- the model has moved on to deciding;
 * - **a new user PROMPT** (`lastPromptSeq`) -- the user has moved on, and whatever that call was
 *   doing belongs to a turn that is over.
 *
 * **A newer TOOL CALL is deliberately NOT a displacer: it only RENAMES the phase.** The difference
 * is what each event says about the call already in flight. A sibling call starting says nothing
 * about it -- `reducer.ts`'s own `permission_requested` comment records that "a turn can have
 * several of the same tool in flight" -- so two calls in flight means two things are happening, and
 * the phase moves to the newest unfinished one rather than falling through to `sent`. A new prompt
 * is the opposite: `toolCalls` is never cleared between turns, and the two shapes that leave
 * `result: null` FOREVER -- an interrupt, and a denial -- would otherwise carry their call into the
 * next turn as "the newest unfinished call" with nothing able to displace it, because a prompt is
 * not text and the interrupted model emits no further delta. `ActivityLine` renders `running` from
 * the instant `turn_started` lands, so that stale call would animate for the whole round trip
 * between Enter and the model's first delta -- naming a tool the user had just stopped or refused,
 * in exactly the window `sent` exists to name. Reproduced by the re-review of 2026-09-20; the
 * first version of this amendment dropped `lastItemSeq` without putting `lastPromptSeq` in its
 * place, and the three tests named "into the next turn" in `turnPhase.test.ts` are that gap.
 *
 * The prompt displacer cannot fire *inside* a turn and so cannot hide a genuinely running call:
 * `App.tsx:1380` disables the composer while `state.activeTurnId !== null`, so a `user_prompt_submitted`
 * only ever arrives at a turn boundary. If that ever changes -- a queued prompt, say -- this clause
 * becomes wrong in the other direction and has to be scoped to the turn rather than to the session.
 *
 * ## What a resync degrades this to
 *
 * `turnThinking` is ephemeral and **can only ever be lost, never invented** (design §8.2), and
 * `applySnapshot` clears it. So after a resync or a panel reload:
 *
 * - mid-thinking with no unfinished tool call, the phase degrades `thinking` -> `sent`, which after
 *   a resync is *true*: the last item on the timeline really is your prompt;
 * - mid-thinking **with** an unfinished tool call and no text after it, the phase degrades
 *   `thinking` -> `tool`. For a live call that is correct and is the common case. For a DENIED call
 *   it is the old defect for one event's worth of time, and it is not fixable on this side: the
 *   snapshot carries no record that a permission was denied (a denied call is indistinguishable
 *   from a running one -- `result: null` in both cases), so nothing here can tell them apart until
 *   the next thinking delta or the model's next text arrives. Both are the documented direction:
 *   less specific, never inventing `thinking`.
 *
 * A `permission_resolved` EVENT does carry an `outcome` (`"denied"` among them), and remembering it
 * per tool call would make rule 2 exact rather than displaceable. It is deliberately not done here:
 * `toolCalls` is a SNAPSHOT collection whose shape `reducer.ts` is required to fold identically to
 * `AgentSessionProjection::apply`, and Rust records no verdict there -- so a frontend-only field
 * would be dropped by the very resync this paragraph is about, buying nothing and costing the one
 * property that keeps the two folds honest. Closing it properly is a wire change.
 *
 * ## What it reads
 *
 * The raw `userPrompts`/`transcript`/`toolCalls` collections directly, not `buildTimeline` -- that
 * function re-anchors a linked permission card under its tool call for RENDERING order, which is
 * not the same thing as chronology.
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
  let openTool: { seq: number; name: string } | null = null;
  for (const call of state.toolCalls) {
    if (call.result === null && (openTool === null || call.seq > openTool.seq)) {
      openTool = { seq: call.seq, name: call.name };
    }
  }
  // The three displacers, in the order the doc comment lists them.
  //
  // `lastPromptSeq < openTool.seq` is the one that survives a turn boundary: an interrupted or
  // denied call keeps `result: null` forever and `toolCalls` is never cleared, so without this the
  // call would still be "the newest unfinished call" in the NEXT turn.
  //
  // `!state.turnThinking` is "no thinking delta has arrived after it": the bit is set by a thinking
  // delta and cleared by EVERY other event (reducer.ts), so while it is set the most recent thing
  // that happened is a thinking delta, which is necessarily after the tool call. Rule 3 directly
  // below then names that state, rather than this clause silently dropping to `sent`.
  if (openTool !== null && lastMessageSeq < openTool.seq && lastPromptSeq < openTool.seq && !state.turnThinking) {
    return { kind: "tool", toolName: openTool.name };
  }

  if (state.turnThinking) return { kind: "thinking" };
  if (lastIsMessage) return { kind: "replying" };
  return { kind: "sent" };
}

/** The exact word (or words) `TurnActivity` renders for a phase -- one word per phase, per design
 * §2's ruling that four distinguishable words replace four separate animations. */
export function phaseWord(phase: TurnPhase): string {
  switch (phase.kind) {
    case "sent":
      return "sent";
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
