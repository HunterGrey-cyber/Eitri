import { describe, expect, it } from "vitest";
import { buildTimeline } from "./timeline";
import { initialState } from "./reducer";
import type { AgentUiState, PermissionRequestRecord, ToolCallRecord, TranscriptMessage } from "./types";

function msg(seq: number, text: string): TranscriptMessage {
  return { seq, text };
}
function tool(seq: number, toolUseId: string): ToolCallRecord {
  return { seq, toolUseId, name: "Bash", input: {}, result: null };
}
function perm(seq: number, permissionId: string, toolUseId: string | null): PermissionRequestRecord {
  return { seq, permissionId, toolUseId, toolName: "Bash", input: {} };
}
function state(overrides: Partial<AgentUiState>): AgentUiState {
  return { ...initialState(), ...overrides };
}

/** A compact label per item, so a test asserts a whole sequence rather than one index at a time. */
function labels(s: AgentUiState): string[] {
  return buildTimeline(s).map((item) => {
    switch (item.kind) {
      case "prompt":
        return `prompt:${item.text}`;
      case "message":
        return `text:${item.text}`;
      case "tool":
        return `tool:${item.call.toolUseId}`;
      case "permission":
        return `perm:${item.request.permissionId}`;
    }
  });
}

describe("buildTimeline", () => {
  /* The defect this closes. Rendering the three collections one after another put every tool card
     below every assistant message, so a turn that went text/tool/text/tool read as text/text/tool/tool
     -- which is not what happened, and is the panel's most visible wrong statement about a turn. */
  it("interleaves messages and tool calls into the order they actually happened", () => {
    expect(
      labels(
        state({
          transcript: [msg(1, "I'll check."), msg(3, "And now the other one."), msg(6, "Done.")],
          toolCalls: [tool(2, "toolu_1"), tool(4, "toolu_2")],
        }),
      ),
    ).toEqual(["text:I'll check.", "tool:toolu_1", "text:And now the other one.", "tool:toolu_2", "text:Done."]);
  });

  it("does not depend on the arrays arriving in any particular order themselves", () => {
    // Rust emits them sorted; nothing in the wire type PROMISES that, so the merge sorts rather
    // than trusting it.
    expect(
      labels(
        state({
          transcript: [msg(3, "second"), msg(1, "first")],
          toolCalls: [tool(4, "toolu_2"), tool(2, "toolu_1")],
        }),
      ),
    ).toEqual(["text:first", "tool:toolu_1", "text:second", "tool:toolu_2"]);
  });

  /* A permission card is about one specific tool call, and a turn can have several of the same tool
     in flight -- so when the link exists the card belongs against that call, not merely somewhere
     nearby in time. */
  it("puts a linked permission immediately after the tool call it gates, not at its own seq", () => {
    expect(
      labels(
        state({
          transcript: [msg(1, "before"), msg(5, "after")],
          toolCalls: [tool(2, "toolu_1"), tool(3, "toolu_2")],
          // seq 4 would place this last-but-one; the link to toolu_1 wins.
          pendingPermissions: [perm(4, "perm-1", "toolu_1")],
        }),
      ),
    ).toEqual(["text:before", "tool:toolu_1", "perm:perm-1", "tool:toolu_2", "text:after"]);
  });

  /* The legacy backend -- still the default -- sends no `tool_use_id` at all. A card with no link
     must still land somewhere truthful rather than being dropped or parked at the bottom. */
  it("places an unlinked permission by its own seq, which is where it arrived", () => {
    expect(
      labels(
        state({
          transcript: [msg(1, "before"), msg(4, "after")],
          toolCalls: [tool(2, "toolu_1")],
          pendingPermissions: [perm(3, "perm-1", null)],
        }),
      ),
    ).toEqual(["text:before", "tool:toolu_1", "perm:perm-1", "text:after"]);
  });

  /* proto3 has no absent-string, so an unset `tool_use_id` arrives as "" on BOTH a permission and a
     tool call. Treating "" as a link would anchor an arbitrary card to an arbitrary call.
     `MessageList`'s awaiting-permission marker guards the identical case on the identical grounds. */
  it("does not treat a proto3 empty-string id as a link", () => {
    expect(
      labels(
        state({
          toolCalls: [tool(1, "")],
          pendingPermissions: [perm(2, "perm-1", "")],
        }),
      ),
    ).toEqual(["tool:", "perm:perm-1"]);
  });

  /* A card whose link names a call this state does not have -- a tool call lost to a resync, say --
     must not vanish. It falls back to its own seq like any unlinked card. */
  it("falls back to seq order when the linked tool call is not present", () => {
    expect(
      labels(
        state({
          transcript: [msg(1, "before")],
          pendingPermissions: [perm(2, "perm-1", "toolu_missing")],
        }),
      ),
    ).toEqual(["text:before", "perm:perm-1"]);
  });

  it("keeps several permissions on one tool call in their own requested order", () => {
    expect(
      labels(
        state({
          toolCalls: [tool(1, "toolu_1")],
          pendingPermissions: [perm(3, "perm-b", "toolu_1"), perm(2, "perm-a", "toolu_1")],
        }),
      ),
    ).toEqual(["tool:toolu_1", "perm:perm-a", "perm:perm-b"]);
  });

  it("gives every item a key that is stable and unique across the whole timeline", () => {
    const s = state({
      transcript: [msg(1, "a"), msg(3, "b")],
      toolCalls: [tool(2, "toolu_1")],
      pendingPermissions: [perm(4, "perm-1", null)],
    });
    const keys = buildTimeline(s).map((item) => item.key);
    expect(new Set(keys).size).toBe(keys.length);
    // Stable: the same state builds the same keys, so React does not remount a streaming message.
    expect(buildTimeline(s).map((item) => item.key)).toEqual(keys);
  });

  /* The key is `seq`, not `permissionId`, and this is the case that forces the difference. proto3
     has no absent-string, so an unset `permission_id` arrives as "" -- and unlike Rust, whose
     `pending_permissions` HashMap collapses two "" ids into a single entry, `reducer.ts` appends to
     a plain array with no dedupe, so two of them genuinely coexist in frontend state. Keyed on
     `permissionId` both cards would be `key="p-"`: a React duplicate-key error and a
     mis-reconciled DOM, which is the exact failure the type's own doc claims to have eliminated. */
  it("gives two permissions sharing a proto3 empty-string id distinct keys", () => {
    const keys = buildTimeline(
      state({ pendingPermissions: [perm(1, "", null), perm(2, "", null)] }),
    ).map((item) => item.key);
    expect(keys).toEqual(["p-1", "p-2"]);
    expect(new Set(keys).size).toBe(2);
  });

  /* Two tool calls carrying ONE id -- which should not happen, and which this function must not
     amplify into a duplicate-key crash if it does. The card is emitted once, after the first call
     that claims it. Without the `anchored.delete` consumption guard both calls would emit it and
     the two copies would share a key. */
  it("emits a card once even if two tool calls claim the same id", () => {
    const items = buildTimeline(
      state({
        toolCalls: [tool(1, "toolu_dup"), tool(2, "toolu_dup")],
        pendingPermissions: [perm(3, "perm-1", "toolu_dup")],
      }),
    );
    expect(items.map((i) => i.kind)).toEqual(["tool", "permission", "tool"]);
    const keys = items.map((i) => i.key);
    expect(new Set(keys).size).toBe(keys.length);
  });

  it("is empty for a conversation that has produced nothing", () => {
    expect(buildTimeline(initialState())).toEqual([]);
  });

  /* The fourth source (Task 2 of panel-as-document): a user prompt is its own timeline item,
     ordered by `seq` exactly like the other three -- this is the merge's only new input, not a
     new merge rule.
     A second turn is included, and its prompt's `seq` (4) is neither the smallest nor the largest
     value in the fixture -- it must land BETWEEN the first tool call (3) and the second message
     (5). A fixture whose only prompt sits at the smallest `seq` cannot tell a real sort from a
     regression that special-cases prompts to always come first: `buildTimeline` spreads
     `userPrompts` into `base` before the other three collections, so "prompt first, by insertion
     order" and "prompt first, by seq" would coincide there. Landing this prompt in the middle
     requires the sort to actually run. */
  it("places a user prompt among the other three kinds by seq", () => {
    const items = buildTimeline(
      state({
        userPrompts: [{ seq: 1, text: "do the thing" }, { seq: 4, text: "and this too" }],
        transcript: [msg(2, "on it"), msg(5, "sure")],
        toolCalls: [tool(3, "toolu_1"), tool(6, "toolu_2")],
      }),
    );
    expect(items.map((i) => [i.kind, i.seq])).toEqual([
      ["prompt", 1],
      ["message", 2],
      ["tool", 3],
      ["prompt", 4],
      ["message", 5],
      ["tool", 6],
    ]);
    expect(new Set(items.map((i) => i.key)).size).toBe(6);
  });
});
