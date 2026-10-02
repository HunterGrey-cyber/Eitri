import type { AgentUiState, PermissionRequestRecord, Seq, ToolCallRecord, TurnEnding } from "./types";

/** One thing that happened in the conversation, ready to render in order.
 *
 * A view over the collections rather than a fifth copy of them: `AgentUiState` stays the
 * shape Rust's `AgentSessionProjection` serializes, and nothing here can drift out of sync with it
 * because nothing here is stored. */
/** `key` is a React key, unique across the whole timeline and stable for as long as the item is.
 *
 * Every kind keys on `seq` rather than on their own identity, which is what makes uniqueness
 * real rather than hoped for. `seq` is unique by construction across all the collections:
 * `AgentSessionProjection::apply` assigns it once per event and no event creates two items
 * (`agent/tests/projection.rs::no_single_event_ever_creates_more_than_one_item` pins that half),
 * and `reducer.ts` continues the same counter from a snapshot's `throughRevision`.
 *
 * The identities were tried first and each fails on the same proto3 hazard -- there is no
 * absent-string, so an unset id arrives as `""`. `MessageList` used to key a tool call on its
 * `toolUseId`, which can be `""` for several calls at once. A permission card keyed on
 * `permissionId` has exactly the same problem: Rust's `pending_permissions` HashMap collapses two
 * `""` ids into one entry so a snapshot can never carry two, but `reducer.ts` appends to a plain
 * array with no dedupe, so two live `permission_requested` events with `permission_id: ""` really
 * do coexist there. Two siblings with the key `p-` is a React error and a mis-reconciled DOM.
 *
 * What keying on `seq` does NOT fix, and is not claimed to: `reducer.ts`'s `permission_resolved`
 * still filters by `permissionId`, so resolving one of two `""` cards removes both. That is a
 * reducer-level consequence of the same hazard and wants its own change. */
export type TimelineItem =
  | { kind: "prompt"; seq: Seq; key: string; text: string }
  | { kind: "message"; seq: Seq; key: string; text: string }
  | { kind: "tool"; seq: Seq; key: string; call: ToolCallRecord }
  | { kind: "permission"; seq: Seq; key: string; request: PermissionRequestRecord }
  /** Where a turn that did not complete stopped: failed, hit a limit, interrupted, or cut off by the
   *  session ending. Keyed `e-<seq>` like the others, by the `seq` of the event that ended the turn. */
  | { kind: "ending"; seq: Seq; key: string; ending: TurnEnding }
  /** P2: a collapsed run of finished tool calls, drawn as one row. Made by `display.ts`, never by
   *  `buildTimeline` -- this function's own ordering (by `seq`, cards anchored after their call)
   *  stays exactly as it was, and folding several rows into one is a DISPLAY decision layered on
   *  top of it, not a change to what the conversation actually contains. `key` is `r-<first seq>`. */
  | { kind: "run"; seq: Seq; key: string; calls: ToolCallRecord[] };

/** Whether a `toolUseId` can identify a tool call at all.
 *
 * Two values cannot, and both are real:
 *   null -- the legacy backend, still the default for a source build, sends no id
 *           (`agent/src/session.rs`).
 *   ""   -- proto3 has no absent-string, so an unset `tool_use_id` arrives as "" from the sidecar.
 *           A `ToolCallRecord.toolUseId` crosses the same boundary and can be "" for the same
 *           reason, so admitting it would anchor an arbitrary card to an arbitrary call.
 *
 * **Exported because three places need exactly this question and all three used to answer it
 * themselves**: this function, `MessageList`'s awaiting-permission marker (an inline `id !== null
 * && id !== ""`) and `PermissionCard`'s "for tool call …" line (a truthiness test). Each carried a
 * paragraph saying the other two guarded the identical case, which is documentation standing in
 * for a shared definition -- the same duplicated-guard defect this branch already fixed once for
 * the colour guard. One predicate, three call sites, so a change to the rule cannot reach two of
 * them and miss the third. */
export function isUsableLink(toolUseId: string | null): toolUseId is string {
  return toolUseId !== null && toolUseId !== "";
}

/**
 * The collections merged into the one sequence they actually formed.
 *
 * Ordering comes from `seq`, which Rust's `AgentSessionProjection::apply` assigns and
 * `serialize_snapshot_for_js` ships -- this function sorts by an authoritative key, it does not
 * infer an order from the shape of the data. That is what makes the result survive a reload or a
 * `UiDelivery::Resync`: both discard the frontend's whole state and rebuild it from a snapshot,
 * and the snapshot carries these numbers.
 *
 * The one placement that is NOT by `seq`: a pending permission whose `toolUseId` names a tool call
 * present here is emitted immediately after that call, because the card is about that specific
 * invocation and a turn can have several of the same tool in flight. Everything else -- an unlinked
 * card, or one naming a call this state does not have -- falls back to its own `seq`, so nothing is
 * ever dropped for want of a link.
 *
 * SAY IT PLAINLY, because a reader will otherwise assume this function is uniformly exercised: that
 * anchoring branch -- `anchored`, `knownToolUseIds`, the per-call sibling sort, the `anchored.delete`
 * guard, roughly half of what is below -- CANNOT RUN ON THE LEGACY BACKEND, which is what a source
 * build still starts on. Every legacy permission carries `toolUseId: null` (`agent/src/session.rs`
 * passes `tool_use_id: None`, deliberately, with its reasons written out there), so `isUsableLink`
 * rejects all of them and every card takes the `unanchored` path.
 *
 * Only the sidecar backend sends a real id. That half is NOT unit-tests-only any more, and this
 * paragraph said it was for three days after it stopped being true: the sidecar was driven on a
 * screen on 2026-09-15 -- against this host's own `claude` 2.1.272, whose version the panel header
 * reported as `CLI 2.1.272 ⚠`, so the CLI-version gate this paragraph blamed had already moved --
 * and the linked-permission anchoring was exercised for real there (`shell/MANUAL_VERIFICATION.md`,
 * "the sidecar backend, verified on a screen for the first time"; `CLAUDE.md`'s own table carries
 * the same row). `timeline.test.ts` and `MessageList.test.tsx` still cover it, and remain the only
 * coverage of the branch on the DEFAULT backend, where it is unreachable by construction.
 *
 * The half that DOES run on legacy, and that fixes the reported defect, is the transcript-vs-tool
 * interleaving: the `base` merge and its sort.
 */
export function buildTimeline(state: AgentUiState): TimelineItem[] {
  // Which linked cards hang off which call. A call may gate more than one.
  const anchored = new Map<string, PermissionRequestRecord[]>();
  const unanchored: PermissionRequestRecord[] = [];
  const knownToolUseIds = new Set(state.toolCalls.map((call) => call.toolUseId).filter(isUsableLink));

  for (const request of state.pendingPermissions) {
    const link = request.toolUseId;
    if (isUsableLink(link) && knownToolUseIds.has(link)) {
      const siblings = anchored.get(link);
      if (siblings) siblings.push(request);
      else anchored.set(link, [request]);
    } else {
      unanchored.push(request);
    }
  }
  // Several cards on one call keep the order they were requested in.
  for (const siblings of anchored.values()) siblings.sort((a, b) => a.seq - b.seq);

  const base: TimelineItem[] = [
    ...state.userPrompts.map((prompt): TimelineItem => ({ kind: "prompt", seq: prompt.seq, key: `u-${prompt.seq}`, text: prompt.text })),
    ...state.transcript.map((message): TimelineItem => ({ kind: "message", seq: message.seq, key: `m-${message.seq}`, text: message.text })),
    ...state.toolCalls.map((call): TimelineItem => ({ kind: "tool", seq: call.seq, key: `t-${call.seq}`, call })),
    ...unanchored.map((request): TimelineItem => ({ kind: "permission", seq: request.seq, key: `p-${request.seq}`, request })),
    ...state.turnEndings.map((ending): TimelineItem => ({ kind: "ending", seq: ending.seq, key: `e-${ending.seq}`, ending })),
  ];
  base.sort((a, b) => a.seq - b.seq);

  const out: TimelineItem[] = [];
  for (const item of base) {
    out.push(item);
    if (item.kind !== "tool") continue;
    const siblings = anchored.get(item.call.toolUseId);
    if (!siblings) continue;
    // Deleted as it is consumed, so two calls sharing one id (which should not happen, and which
    // this function must not amplify if it does) cannot emit the same card twice.
    anchored.delete(item.call.toolUseId);
    for (const request of siblings) {
      out.push({ kind: "permission", seq: request.seq, key: `p-${request.seq}`, request });
    }
  }
  return out;
}

/**
 * Where `focus_permission` puts the cursor (modules spec §3.3): the timeline index of the OLDEST
 * pending card -- the lowest `seq`, the one the model has waited on longest, whatever row it is
 * drawn under -- or `null` when no card is pending.
 */
export function oldestPendingPermission(timeline: TimelineItem[]): number | null {
  let best: number | null = null;
  timeline.forEach((item, index) => {
    if (item.kind !== "permission") return;
    const current = best === null ? null : timeline[best];
    if (current === null || (current.kind === "permission" && item.seq < current.seq)) best = index;
  });
  return best;
}

/** R4's `[[`/`]]`: the next `prompt` row before (`-1`) or after (`1`) `from`, or `null` when there is
 *  none. Never wraps -- `[[` at the first prompt, or `]]` at the last, simply does nothing, the same
 *  way `clampStep` (`nav.ts`) stops `j`/`k` at either end rather than cycling. */
export function promptIndex(timeline: TimelineItem[], from: number, delta: 1 | -1): number | null {
  for (let i = from + delta; i >= 0 && i < timeline.length; i += delta) {
    if (timeline[i].kind === "prompt") return i;
  }
  return null;
}

/** Whether `item` is a card waiting for an answer from this panel: a permission item whose id is not in
 *  `answered` -- the ids this tab has answered, by a card's button or by `a`/`d`, which stay drawn (inert)
 *  until the provider resolves them. The one condition a timeline cannot know is whether the session still
 *  lives (a dead session's cards are inert, `PermissionCard`): that is the caller's. */
function isWaitingCard(item: TimelineItem, answered: ReadonlySet<string>): boolean {
  return item.kind === "permission" && !answered.has(item.request.permissionId);
}

/** Owner decision #39: the card INPUT's `Ctrl+y` approves -- the timeline index of the OLDEST card
 *  still waiting (lowest `seq`), the same rule a card landing uses (`oldestPendingPermission`, R11)
 *  with the cards this panel has already answered left out, since those wait only for the provider.
 *  `null` when none waits. `timeline` is the active tab's, so another tab's card is never a candidate. */
export function oldestWaitingPermission(timeline: TimelineItem[], answered: ReadonlySet<string>): number | null {
  let best: number | null = null;
  timeline.forEach((item, index) => {
    if (!isWaitingCard(item, answered)) return;
    const current = best === null ? null : timeline[best];
    if (current === null || (current.kind === "permission" && item.seq < current.seq)) best = index;
  });
  return best;
}

/** `]p` (+1) / `[p` (-1) (R7): the next waiting card after (before) `from`, wrapping as nvim's `]d`
 *  does (`vim.diagnostic.jump`'s default) -- unlike `[[`/`]]` (`promptIndex`), which stop at either end. A card
 *  this panel already answered is not waiting. With one card waiting, and `from` on it, the answer is `from`
 *  itself (a lone diagnostic is its own next); `null` when none waits at all. `from` may lie outside the
 *  timeline (it counts round the ring), so a cursor left one past a shortened list still finds a real row. */
export function waitingCardIndex(
  timeline: TimelineItem[],
  from: number,
  delta: 1 | -1,
  answered: ReadonlySet<string>,
): number | null {
  const n = timeline.length;
  for (let step = 1; step <= n; step++) {
    const i = (((from + delta * step) % n) + n) % n;
    if (isWaitingCard(timeline[i], answered)) return i;
  }
  return null;
}

/** `{N}]p` / `{N}[p`: `waitingCardIndex` taken `times` times, each step from the card the last one landed on
 *  (a count repeats it, R7). The first step reaches a waiting card and every step after it only goes round the
 *  ring of them, so a count is at most as many steps as there are waiting cards -- `times - 1` modulo that,
 *  plus the first -- however large it is: it costs O(waiting x rows), where repeating the single step `times`
 *  times cost O(times x rows), which for the panel's cap of 9999 over a long conversation with one card waiting
 *  is a whole scan of it 9999 times inside one keydown. The same landing either way. `null` when no card waits. */
export function waitingCardAfter(
  timeline: TimelineItem[],
  from: number,
  delta: 1 | -1,
  times: number,
  answered: ReadonlySet<string>,
): number | null {
  let waiting = 0;
  for (const item of timeline) if (isWaitingCard(item, answered)) waiting++;
  if (waiting === 0) return null;
  let target = from;
  const steps = ((Math.max(1, times) - 1) % waiting) + 1;
  for (let step = 0; step < steps; step++) target = waitingCardIndex(timeline, target, delta, answered) ?? target;
  return target;
}
