import type {
  AgentUiState,
  PermissionRequestRecord,
  Seq,
  ThinkingBlock,
  ToolCallRecord,
  TranscriptMessage,
  TurnEnding,
  UserPromptRecord,
} from "./types";

/** One thing that happened in the conversation, ready to render in order.
 *
 * A view over the collections rather than a fifth copy of them: `AgentUiState` stays the
 * shape Rust's `AgentSessionProjection` serializes, and nothing here can drift out of sync with it
 * because nothing here is stored but each record's own item, made from that record alone and held
 * only while the record is (`buildTimeline`'s caches). */
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
  /** `delivered`: a message the agent read inside its running turn (`UserPromptRecord.delivered`). */
  | { kind: "prompt"; seq: Seq; key: string; text: string; delivered?: true }
  | { kind: "message"; seq: Seq; key: string; text: string }
  | { kind: "tool"; seq: Seq; key: string; call: ToolCallRecord }
  | { kind: "permission"; seq: Seq; key: string; request: PermissionRequestRecord }
  /** Where a turn that did not complete stopped: failed, hit a limit, interrupted, or cut off by the
   *  session ending. Keyed `e-<seq>` like the others, by the `seq` of the event that ended the turn. */
  | { kind: "ending"; seq: Seq; key: string; ending: TurnEnding }
  /** One block of the agent's thinking, keyed `k-<seq>`. `open` while the agent is still writing it
   *  (the state's `thinkingOpen`, for its last block only). Whether and how it is drawn is
   *  `display.ts`'s and `MessageList`'s to decide, by the panel's `agent.thinking` level. */
  | { kind: "thinking"; seq: Seq; key: string; block: ThinkingBlock; open: boolean }
  /** A collapsed run of finished tool calls, drawn as one row by `display.ts`.
   *  `buildTimeline` preserves ordering by `seq`, with cards anchored after their calls.
   *  Folding is a display layer over that ordering. `key` is `r-<first seq>`. */
  | {
      kind: "run";
      seq: Seq;
      key: string;
      calls: ToolCallRecord[];
      /** Finished thinking blocks that sat between two of `calls`, folded away with them; absent when
       *  none did. Expanding the run puts them back as rows of their own. */
      thoughts?: ThinkingBlock[];
    };

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
 * Merge the collections by `seq`, assigned by Rust's `AgentSessionProjection::apply` and carried
 * in snapshots by `serialize_snapshot_for_js`. Reloads and `UiDelivery::Resync` rebuild from those
 * keys, preserving event order.
 *
 * A pending permission linked by `toolUseId` to a known call is placed immediately after that call,
 * so cards stay with their invocation even when several calls use the same tool. Unlinked cards
 * and cards naming an absent call use their own `seq`; missing links never discard a card.
 *
 * Legacy permissions have `toolUseId: null`, so `isUsableLink` rejects them and they all take the
 * unanchored path. Only the sidecar sends real ids for anchoring. The base merge still interleaves
 * transcript and tool entries on legacy. `timeline.test.ts` and `MessageList.test.tsx` cover the
 * linked-card behavior that legacy cannot exercise.
 */
let timelineBuilds = 0;
/** How many times `buildTimeline` has run in this page: the performance tests count it. */
export function timelineBuildCount(): number {
  return timelineBuilds;
}

/* Each item is made once per record and handed out again while that record is the same object.
   The reducer replaces only the records an event changed (the reply or thought being streamed, the
   call that completed), and every row is memoized on its item, so a batch of streamed deltas renders
   the one row whose record changed rather than every row of the conversation. A snapshot brings new
   records, and with them new items, once. Each cache also checks the fields its item copies, so a
   record changed in place (only ever a test) is never answered with a stale item. */
const promptItems = new WeakMap<UserPromptRecord, TimelineItem>();
const messageItems = new WeakMap<TranscriptMessage, TimelineItem>();
const toolItems = new WeakMap<ToolCallRecord, TimelineItem>();
const permissionItems = new WeakMap<PermissionRequestRecord, TimelineItem>();
const endingItems = new WeakMap<TurnEnding, TimelineItem>();
/** By block, one item for each value of `open`: a block's item changes when it closes. */
const thinkingItems = new WeakMap<ThinkingBlock, { open?: TimelineItem; closed?: TimelineItem }>();

function promptItem(prompt: UserPromptRecord): TimelineItem {
  const held = promptItems.get(prompt);
  if (held?.kind === "prompt" && held.seq === prompt.seq && held.text === prompt.text && held.delivered === prompt.delivered)
    return held;
  const item: TimelineItem = {
    kind: "prompt",
    seq: prompt.seq,
    key: `u-${prompt.seq}`,
    text: prompt.text,
    ...(prompt.delivered ? { delivered: true as const } : {}),
  };
  promptItems.set(prompt, item);
  return item;
}

function messageItem(message: TranscriptMessage): TimelineItem {
  const held = messageItems.get(message);
  if (held?.kind === "message" && held.seq === message.seq && held.text === message.text) return held;
  const item: TimelineItem = { kind: "message", seq: message.seq, key: `m-${message.seq}`, text: message.text };
  messageItems.set(message, item);
  return item;
}

function toolItem(call: ToolCallRecord): TimelineItem {
  const held = toolItems.get(call);
  if (held !== undefined && held.seq === call.seq) return held;
  const item: TimelineItem = { kind: "tool", seq: call.seq, key: `t-${call.seq}`, call };
  toolItems.set(call, item);
  return item;
}

function permissionItem(request: PermissionRequestRecord): TimelineItem {
  const held = permissionItems.get(request);
  if (held !== undefined && held.seq === request.seq) return held;
  const item: TimelineItem = { kind: "permission", seq: request.seq, key: `p-${request.seq}`, request };
  permissionItems.set(request, item);
  return item;
}

function endingItem(ending: TurnEnding): TimelineItem {
  const held = endingItems.get(ending);
  if (held !== undefined && held.seq === ending.seq) return held;
  const item: TimelineItem = { kind: "ending", seq: ending.seq, key: `e-${ending.seq}`, ending };
  endingItems.set(ending, item);
  return item;
}

function thinkingItem(block: ThinkingBlock, open: boolean): TimelineItem {
  let slots = thinkingItems.get(block);
  if (slots === undefined) {
    slots = {};
    thinkingItems.set(block, slots);
  }
  const held = open ? slots.open : slots.closed;
  if (held !== undefined && held.seq === block.seq) return held;
  const item: TimelineItem = { kind: "thinking", seq: block.seq, key: `k-${block.seq}`, block, open };
  if (open) slots.open = item;
  else slots.closed = item;
  return item;
}

export function buildTimeline(state: AgentUiState): TimelineItem[] {
  timelineBuilds += 1;
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
    ...state.userPrompts.map(promptItem),
    ...state.transcript.map(messageItem),
    ...state.toolCalls.map(toolItem),
    ...unanchored.map(permissionItem),
    ...state.turnEndings.map(endingItem),
    // A snapshot from a build before thinking was kept has none (`applySnapshot` fills `[]`); the
    // `?? []` covers a hand-built state in a test.
    ...(state.thinking ?? []).map((block, i, all) => thinkingItem(block, state.thinkingOpen === true && i === all.length - 1)),
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
    for (const request of siblings) out.push(permissionItem(request));
  }
  return out;
}

/**
 * Where `focus_permission` puts the cursor: the timeline index of the OLDEST
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

/** `[[`/`]]`: the next `prompt` row before (`-1`) or after (`1`) `from`, or `null` if none exists.
 *  Navigation stops at either end, matching `clampStep` in `nav.ts`. */
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

/** The card INPUT's `Ctrl+y` approves -- the timeline index of the OLDEST card
 *  still waiting (lowest `seq`), the same rule a card landing uses (`oldestPendingPermission`)
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

/** `]p` (+1) / `[p` (-1): the next waiting card after or before `from`, wrapping at either end.
 *  Cards already answered by this panel are excluded. A lone waiting card returns its own index,
 *  even when `from` is on it; no waiting cards returns `null`.
 *  `from` may be outside the timeline, so a cursor past a shortened list still finds a row. */
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

/** `{N}]p` / `{N}[p`: repeats `waitingCardIndex` from each preceding landing.
 *  After the first step, navigation cycles only through waiting cards. Reducing `times - 1` modulo
 *  that count preserves the landing while limiting work to O(waiting x rows), rather than O(times x rows).
 *  This avoids scanning a long conversation 9999 times for a single keydown. Returns `null` if no card waits. */
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
