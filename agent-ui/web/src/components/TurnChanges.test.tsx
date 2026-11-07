// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { initialState } from "../reducer";
import type { AgentUiState, ReviewSettings, ToolCallRecord } from "../types";
import { REVIEW_SETTINGS_OFF } from "../types";
import { latestTurnChanges, latestTurnId, TurnChanges, turnChangesWanted } from "./TurnChanges";

const HINT_ON: ReviewSettings = { enabled: true, hint: "full" };

const edit = (seq: number, turnId: string, file: string, isError = false): ToolCallRecord => ({
  seq,
  toolUseId: `e${seq}`,
  name: "Edit",
  turnId,
  input: { file_path: file, old_string: "a\nb", new_string: "a\nc\nd" },
  result: { content: "ok", isError },
});

const withCalls = (calls: ToolCallRecord[], activeTurnId: string | null = null): AgentUiState => ({
  ...initialState(),
  toolCalls: calls,
  activeTurnId,
});

describe("latestTurnChanges", () => {
  it("counts files and lines of the latest turn's finished edits only", () => {
    const state = withCalls([edit(1, "t1", "/x"), edit(2, "t2", "/a"), edit(3, "t2", "/a"), edit(4, "t2", "/b")]);
    expect(latestTurnChanges(state)).toEqual({ files: 2, added: 6, removed: 3 });
  });

  it("leaves out a failed edit and says nothing for a turn with no edits", () => {
    expect(latestTurnChanges(withCalls([edit(1, "t1", "/a", true)]))).toBeNull();
    expect(latestTurnChanges(withCalls([]))).toBeNull();
  });

});

describe("the change line", () => {
  const running = withCalls([edit(1, "t1", "/a")], "t1");

  it("is off by default: no settings, or the hint off, draws nothing", () => {
    expect(render(<TurnChanges state={running} review={REVIEW_SETTINGS_OFF} />).container.textContent).toBe("");
    expect(render(<TurnChanges state={running} review={{ enabled: true, hint: "off" }} />).container.textContent).toBe("");
    expect(turnChangesWanted(REVIEW_SETTINGS_OFF)).toBe(false);
  });

  it("keeps the running change line hidden at the key level", () => {
    const key: ReviewSettings = { enabled: true, hint: "key" };
    expect(turnChangesWanted(key)).toBe(false);
    expect(render(<TurnChanges state={running} review={key} />).container.textContent).toBe("");
  });

  it("is drawn while the turn runs when review is on and the hint asked for", () => {
    expect(turnChangesWanted(HINT_ON)).toBe(true);
    expect(render(<TurnChanges state={running} review={HINT_ON} />).container.textContent).toBe("1 file changed+2−1");
  });

  it("draws nothing, and so offers no key, when review is off, even with the hint on", () => {
    const off: ReviewSettings = { enabled: false, hint: "full" };
    expect(turnChangesWanted(off)).toBe(false);
    const { container } = render(<TurnChanges state={running} review={off} />);
    expect(container.textContent).toBe("");
    expect(container.textContent).not.toContain("c to review");
  });

  it("leaves a finished turn to the status band and never offers the key itself", () => {
    const done = render(<TurnChanges state={withCalls([edit(1, "t1", "/a")])} review={HINT_ON} />);
    expect(done.container.textContent).toBe("");
    const live = render(<TurnChanges state={running} review={HINT_ON} />);
    expect(live.container.textContent).not.toContain("c to review");
  });
});

describe("the latest turn", () => {
  const call = (seq: number, turnId: string, name: string, input: unknown): ToolCallRecord => ({
    seq,
    toolUseId: `c${seq}`,
    name,
    turnId,
    input,
    result: { content: "ok", isError: false },
  });

  it("is the turn after the latest prompt: a tool-free latest turn shows nothing, and offers no review key", () => {
    const toolFree: AgentUiState = {
      ...initialState(),
      userPrompts: [
        { seq: 0, text: "edit it" },
        { seq: 3, text: "thanks, explain" },
      ],
      toolCalls: [edit(1, "t1", "/a")],
      transcript: [{ seq: 4, text: "It does X." }],
    };
    expect(latestTurnId(toolFree)).toBeNull();
    expect(latestTurnChanges(toolFree)).toBeNull();
    expect(render(<TurnChanges state={toolFree} review={HINT_ON} />).container.textContent).toBe("");
    // A thought names the latest turn, which still changed nothing.
    const thought = { ...toolFree, thinking: [{ seq: 5, turnId: "t2", text: "hm" }] };
    expect(latestTurnId(thought)).toBe("t2");
    expect(latestTurnChanges(thought)).toBeNull();
  });

  it("is the running turn while one runs", () => {
    const state: AgentUiState = { ...withCalls([edit(1, "t1", "/a")], "t2"), userPrompts: [{ seq: 0, text: "x" }] };
    expect(latestTurnId(state)).toBe("t2");
    expect(latestTurnChanges(state)).toBeNull();
  });

  it("is not changed by a message read inside the turn: it opens no turn", () => {
    const state: AgentUiState = {
      ...initialState(),
      userPrompts: [
        { seq: 0, text: "edit it" },
        { seq: 2, text: "and the tests", delivered: true },
      ],
      toolCalls: [edit(1, "t1", "/a")],
    };
    expect(latestTurnId(state)).toBe("t1");
    expect(latestTurnChanges(state)).toEqual({ files: 1, added: 2, removed: 1 });
  });

  it("counts a Codex fileChange, each of its files", () => {
    const state = withCalls([
      call(1, "t1", "fileChange", {
        changes: [
          { path: "/a", kind: "update", diff: "@@ -1,2 +1,2 @@\n keep\n-old\n+new" },
          { path: "/n", kind: "add", diff: "x\ny" },
        ],
      }),
    ]);
    expect(latestTurnChanges(state)).toEqual({ files: 2, added: 3, removed: 1 });
  });

  it("counts an ACP agent's edit, delete and move, and no other kind", () => {
    const state = withCalls([
      call(1, "t1", "acp:edit", { title: "edit", diffs: [{ path: "/p", oldText: "a\nb", newText: "a\nc" }] }),
      call(2, "t1", "acp:delete", { title: "rm", diffs: [{ path: "/q", oldText: "gone", newText: "" }] }),
      call(3, "t1", "acp:read", { title: "read", diffs: [{ path: "/r", oldText: "", newText: "x" }] }),
    ]);
    expect(latestTurnChanges(state)).toEqual({ files: 2, added: 1, removed: 2 });
  });
});
