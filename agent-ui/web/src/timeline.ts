import type { AgentUiState, PermissionRequestRecord, Seq, ToolCallRecord } from "./types";

/** One thing that happened in the conversation, ready to render in order.
 *
 * A view over the three collections rather than a fourth copy of them: `AgentUiState` stays the
 * shape Rust's `AgentSessionProjection` serializes, and nothing here can drift out of sync with it
 * because nothing here is stored. */
/** `key` is a React key, unique across the whole timeline and stable for as long as the item is.
 *
 * ALL THREE kinds key on `seq` rather than on their own identity, which is what makes uniqueness
 * real rather than hoped for. `seq` is unique by construction across all three collections:
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
  | { kind: "message"; seq: Seq; key: string; text: string }
  | { kind: "tool"; seq: Seq; key: string; call: ToolCallRecord }
  | { kind: "permission"; seq: Seq; key: string; request: PermissionRequestRecord };

/** Whether a `toolUseId` can identify a tool call at all.
 *
 * Two values cannot, and both are real:
 *   null -- the legacy backend, still the default, sends no id (`agent/src/session.rs`).
 *   ""   -- proto3 has no absent-string, so an unset `tool_use_id` arrives as "" from the sidecar.
 *           A `ToolCallRecord.toolUseId` crosses the same boundary and can be "" for the same
 *           reason, so admitting it would anchor an arbitrary card to an arbitrary call.
 * `MessageList`'s awaiting-permission marker and `PermissionCard` guard the identical case. */
function isUsableLink(toolUseId: string | null): toolUseId is string {
  return toolUseId !== null && toolUseId !== "";
}

/**
 * The three collections merged into the one sequence they actually formed.
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
 * guard, roughly half of what is below -- CANNOT RUN ON THE LEGACY BACKEND, which is the default
 * and the only backend this project's own machine can run today. Every legacy permission carries
 * `toolUseId: null` (`agent/src/session.rs` passes `tool_use_id: None`, deliberately, with its
 * reasons written out there), so `isUsableLink` rejects all of them and every card takes the
 * `unanchored` path. Only the sidecar backend sends a real id, and this host's `claude` 2.1.272 is
 * refused by that sidecar's CLI-version gate, so the anchoring half ships on unit tests alone --
 * `timeline.test.ts` and `MessageList.test.tsx` cover it, no live run ever has.
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
    ...state.transcript.map((message): TimelineItem => ({ kind: "message", seq: message.seq, key: `m-${message.seq}`, text: message.text })),
    ...state.toolCalls.map((call): TimelineItem => ({ kind: "tool", seq: call.seq, key: `t-${call.seq}`, call })),
    ...unanchored.map((request): TimelineItem => ({ kind: "permission", seq: request.seq, key: `p-${request.seq}`, request })),
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
