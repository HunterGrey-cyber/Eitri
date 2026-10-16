// @vitest-environment jsdom
/**
 * The "Worked for 1m 23s" line under a finished turn: chrome between rows, not a row. It is not a cursor
 * stop, VISUAL's copy leaves its text out, and a turn with no known time draws nothing.
 */
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { MessageList } from "./MessageList";
import { initialState } from "../reducer";
import { conversationRows } from "../nav";
import { isChromeNode } from "../visual";
import { durationsFromSnapshot } from "../turnTimes";
import type { AgentUiState, ThinkingDisplay, TurnTimeWire } from "../types";

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const time = (turnId: string, durationMs: number, anchorSeq: number | null): TurnTimeWire => ({
  turnId,
  startedAtMs: 1_000,
  endedAtMs: 1_000 + durationMs,
  anchorSeq,
});

/** Two completed turns: a prompt, a reply and a tool call each. */
function twoTurns(overrides: Partial<AgentUiState> = {}): AgentUiState {
  return {
    ...initialState(),
    userPrompts: [
      { seq: 1, text: "first" },
      { seq: 5, text: "second" },
    ],
    transcript: [
      { seq: 2, text: "reply one" },
      { seq: 6, text: "reply two" },
    ],
    toolCalls: [
      { seq: 3, toolUseId: "a", name: "Bash", input: { command: "ls" }, result: { content: "x", isError: false }, turnId: "t1" },
    ],
    ...overrides,
  };
}

function draw(
  state: AgentUiState,
  times: TurnTimeWire[] | undefined,
  thinkingDisplay?: ThinkingDisplay,
  expanded: Record<string, boolean> = {},
) {
  return render(
    <MessageList
      state={state}
      turnDurations={times === undefined ? undefined : durationsFromSnapshot(times)}
      sessionEnded={false}
      expanded={expanded}
      cursor={-1}
      thinkingDisplay={thinkingDisplay}
      onAnswerPermission={() => {}}
    />,
  );
}

const footers = (c: HTMLElement) => Array.from(c.querySelectorAll(".turn-time")).map((el) => el.textContent);

describe("the Worked for line", () => {
  it("stands after the last row of each finished turn, before the next turn's prompt", () => {
    const { container } = draw(twoTurns(), [time("t1", 83_000, 3), time("t2", 8_000, 6)]);
    expect(footers(container)).toEqual(["Worked for 1m 23s", "Worked for 8s"]);
    const first = container.querySelector(".turn-time")!;
    expect(first.previousElementSibling!.textContent).toContain("ls");
    expect(first.nextElementSibling!.textContent).toContain("second");
    const last = container.querySelectorAll(".turn-time")[1]!;
    expect(last.previousElementSibling!.textContent).toContain("reply two");
    expect(last.nextElementSibling!.classList.contains("cursor-band")).toBe(true);
  });

  it("stands under a folded run when the turn's last row is one of its calls", () => {
    const state = twoTurns({
      toolCalls: [
        { seq: 3, toolUseId: "a", name: "Read", input: {}, result: { content: "x", isError: false }, turnId: "t1" },
        { seq: 4, toolUseId: "b", name: "Read", input: {}, result: { content: "y", isError: false }, turnId: "t1" },
      ],
    });
    const { container } = draw(state, [time("t1", 5_000, 4)]);
    const footer = container.querySelector(".turn-time")!;
    expect(footer.previousElementSibling!.classList.contains("row-tool-run")).toBe(true);
  });

  it("draws nothing when the timing is unknown", () => {
    expect(footers(draw(twoTurns(), undefined).container)).toEqual([]);
    cleanup();
    expect(footers(draw(twoTurns(), []).container)).toEqual([]);
    cleanup();
    // A turn Rust stamped but could not place on a row.
    expect(footers(draw(twoTurns(), [time("t1", 5_000, null)]).container)).toEqual([]);
    cleanup();
    // A row that is not on the page.
    expect(footers(draw(twoTurns(), [time("t1", 5_000, 99)]).container)).toEqual([]);
  });

  it("draws nothing for the turn that is still running", () => {
    const { container } = draw(twoTurns({ activeTurnId: "t2" }), [time("t1", 4_000, 3), time("t2", 1_000, 6)]);
    expect(footers(container)).toEqual(["Worked for 4s"]);
  });

  it("is no cursor stop: the rows keep their order and count, and the line carries no stop attribute", () => {
    const withLines = draw(twoTurns(), [time("t1", 83_000, 3), time("t2", 8_000, 6)]).container;
    const list = withLines.querySelector<HTMLElement>(".message-list")!;
    const rows = conversationRows(list).map((row) => row.textContent);
    expect(rows).toHaveLength(5);
    expect(withLines.querySelectorAll("[data-nav-stop]")).toHaveLength(5);
    for (const line of Array.from(withLines.querySelectorAll(".turn-time"))) {
      expect(line.hasAttribute("data-nav-stop")).toBe(false);
      expect(line.closest("[data-nav-stop]")).toBeNull();
    }
    cleanup();
    const bare = draw(twoTurns(), undefined).container;
    expect(conversationRows(bare.querySelector<HTMLElement>(".message-list")!).map((row) => row.textContent)).toEqual(rows);
  });

  it("is chrome to VISUAL's copy: its text sits in an aria-hidden span, which isChromeNode refuses", () => {
    const { container } = draw(twoTurns(), [time("t1", 83_000, 3)]);
    const list = container.querySelector<HTMLElement>(".message-list")!;
    const span = container.querySelector(".turn-time > span")!;
    expect(span.getAttribute("aria-hidden")).toBe("true");
    expect(isChromeNode(span.firstChild!, list)).toBe(true);
    // A reply's text is not.
    const reply = Array.from(container.querySelectorAll(".row-assistant")).find((el) => el.textContent?.includes("reply one"))!;
    expect(isChromeNode(reply.querySelector("p")!.firstChild!, list)).toBe(false);
  });
});

describe("a turn that ended badly", () => {
  const ended = twoTurns({
    turnEndings: [{ seq: 4, turnId: "t1", kind: "interrupted", reason: null, apiErrorStatus: null, message: null }],
  });

  it("says how long on its ending row instead of a footer", () => {
    const { container } = draw(ended, [time("t1", 12_000, 4)]);
    expect(container.querySelector(".row-ending .row-body")!.textContent).toBe("interrupted after 12s");
    expect(footers(container)).toEqual([]);
  });

  it("keeps the ending row as it was when the time is unknown", () => {
    const { container } = draw(ended, []);
    expect(container.querySelector(".row-ending .row-body")!.textContent).toBe("interrupted");
  });
});

describe("a turn whose newest row is a thought", () => {
  const ok = { content: "x", isError: false };
  const LEVELS: ThinkingDisplay[] = ["collapsed", "preview", "expanded", "hidden"];

  // Rust's anchor is the newest row the agent wrote, a thought included: here the thought at 3.
  const endsOnThought: AgentUiState = {
    ...initialState(),
    userPrompts: [
      { seq: 0, text: "go" },
      { seq: 5, text: "next" },
    ],
    toolCalls: [{ seq: 2, toolUseId: "a", name: "Bash", input: { command: "ls" }, result: ok, turnId: "t1" }],
    thinking: [{ seq: 3, turnId: "t1", text: "done, nothing to say" }],
  };

  for (const level of LEVELS) {
    it(`keeps its line at level ${level}, under the turn's last drawn row and above the next prompt`, () => {
      const { container } = draw(endsOnThought, [time("t1", 5_000, 3)], level);
      expect(footers(container)).toEqual(["Worked for 5s"]);
      const line = container.querySelector(".turn-time")!;
      const above = line.previousElementSibling!;
      if (level === "hidden") expect(above.textContent).toContain("ls");
      else expect(above.classList.contains("row-thinking")).toBe(true);
      expect(line.nextElementSibling!.textContent).toContain("next");
    });
  }

  // Two turns with no prompt between them (a turn the agent opened itself): t1 ends on a thought that
  // sits between t1's calls and t2's, which a run of calls would otherwise fold away.
  const noPromptBetween: AgentUiState = {
    ...initialState(),
    userPrompts: [{ seq: 0, text: "go" }],
    toolCalls: [
      { seq: 2, toolUseId: "a", name: "Read", input: {}, result: ok, turnId: "t1" },
      { seq: 3, toolUseId: "b", name: "Read", input: {}, result: ok, turnId: "t1" },
      { seq: 6, toolUseId: "c", name: "Read", input: {}, result: ok, turnId: "t2" },
      { seq: 7, toolUseId: "d", name: "Read", input: {}, result: ok, turnId: "t2" },
    ],
    thinking: [{ seq: 4, turnId: "t1", text: "t1 last thought" }],
  };
  for (const level of LEVELS) {
    it(`gives each of two turns its own line at level ${level}, the first above the second turn's rows`, () => {
      const { container } = draw(noPromptBetween, [time("t1", 5_000, 4), time("t2", 7_000, 7)], level);
      expect(footers(container)).toEqual(["Worked for 5s", "Worked for 7s"]);
      const first = container.querySelector(".turn-time")!;
      // The second turn's run follows the first line.
      expect(first.nextElementSibling!.classList.contains("row-tool-run")).toBe(true);
    });
  }

  it("stands under the run that absorbed the thought it names", () => {
    // The anchor is a thought folded into a run between two calls (Rust may name it when the call after
    // it was folded in under another seq order, or when the line is placed by a stale anchor): the
    // line goes under the run that holds it.
    const absorbed: AgentUiState = {
      ...initialState(),
      userPrompts: [
        { seq: 0, text: "go" },
        { seq: 9, text: "next" },
      ],
      toolCalls: [
        { seq: 2, toolUseId: "a", name: "Read", input: {}, result: ok, turnId: "t1" },
        { seq: 4, toolUseId: "b", name: "Read", input: {}, result: ok, turnId: "t1" },
      ],
      thinking: [{ seq: 3, turnId: "t1", text: "between" }],
    };
    const { container } = draw(absorbed, [time("t1", 2_000, 3)], "collapsed");
    expect(footers(container)).toEqual(["Worked for 2s"]);
    const line = container.querySelector(".turn-time")!;
    expect(line.previousElementSibling!.classList.contains("row-tool-run")).toBe(true);
    expect(line.nextElementSibling!.textContent).toContain("next");
  });

  it("never stands under a later turn's prompt: a turn with nothing drawn but its prompt has the line under it", () => {
    const onlyThought: AgentUiState = {
      ...initialState(),
      userPrompts: [
        { seq: 0, text: "go" },
        { seq: 3, text: "next" },
      ],
      thinking: [{ seq: 1, turnId: "t1", text: "quiet" }],
      transcript: [{ seq: 4, text: "reply to next" }],
    };
    const { container } = draw(onlyThought, [time("t1", 1_000, 1)], "hidden");
    const line = container.querySelector(".turn-time")!;
    expect(line.previousElementSibling!.textContent).toContain("go");
    expect(line.nextElementSibling!.textContent).toContain("next");
  });
});
