import type { AgentUiState, PermissionRequestRecord, Seq, ToolCallRecord } from "./types";

/** One thing that happened in the conversation, ready to render in order.
 *
 * A view over the four collections rather than a fifth copy of them: `AgentUiState` stays the
 * shape Rust's `AgentSessionProjection` serializes, and nothing here can drift out of sync with it
 * because nothing here is stored. */
/** `key` is a React key, unique across the whole timeline and stable for as long as the item is.
 *
 * ALL FOUR kinds key on `seq` rather than on their own identity, which is what makes uniqueness
 * real rather than hoped for. `seq` is unique by construction across all four collections:
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
  | { kind: "permission"; seq: Seq; key: string; request: PermissionRequestRecord };

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
