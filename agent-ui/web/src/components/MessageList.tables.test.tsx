// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import { MessageList } from "./MessageList";
import { initialState } from "../reducer";
import type { AgentUiState } from "../types";

/* A table in a reply is a grid or a list (`tableLayout.ts`): here, that the rows ask for it at the moments the rule says. */

afterEach(cleanup);
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const TABLE = "| 想了解 | 从这里读 |\n|---|---|\n| Agent 对话怎么跑 | `agent/src/providers/{claude_sidecar,codex,acp}/` |";
const props = { sessionEnded: false, expanded: {}, cursor: -1, onAnswerPermission: () => {} };
const reply = (text: string, running: boolean): AgentUiState => ({
  ...initialState(),
  activeTurnId: running ? "t1" : null,
  assistantMessageOpen: running,
  userPrompts: [{ seq: 1, text: "go" }],
  transcript: [{ seq: 2, text }],
});

/* jsdom lays nothing out: a 348px box whose table would take 600px. */
beforeEach(() => {
  // A slow drawing makes the stream rest before the next one (`SLOW_TAIL_MS`); on a loaded machine the first draw is slow.
  vi.spyOn(performance, "now").mockReturnValue(0);
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function (this: HTMLElement) {
    return this.classList.contains("table-scroll") ? 348 : 0;
  });
  vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockImplementation(function (this: HTMLElement) {
    return this.tagName === "TABLE" ? 600 : 0;
  });
  Range.prototype.getClientRects = () => [{ top: 0, bottom: 18, height: 18, width: 40 }] as unknown as DOMRectList;
});
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  delete (Range.prototype as Partial<Range>).getClientRects;
});

describe("a reply's table", () => {
  it("is a list at once when the reply is already finished and the grid does not fit", () => {
    const view = render(<MessageList state={reply(`intro\n\n${TABLE}\n`, false)} {...props} />);
    expect(view.container.querySelector<HTMLElement>(".table-scroll table")!.hidden).toBe(true);
    expect(view.container.querySelector(".table-list")?.textContent).toContain("想了解: Agent 对话怎么跑");
  });

  it("stays a grid while it is the block being written, and is decided once a block follows it", () => {
    const view = render(<MessageList state={reply(`intro\n\n${TABLE}`, true)} {...props} />);
    const table = () => view.container.querySelector<HTMLElement>(".table-scroll table")!;
    expect(table().hidden).toBe(false);
    expect(view.container.querySelector(".table-list")).toBeNull();
    act(() => view.rerender(<MessageList state={reply(`intro\n\n${TABLE}\n\nnext paragraph\n`, true)} {...props} />));
    expect(table().hidden).toBe(true);
    expect(view.container.querySelector(".table-list")).not.toBeNull();
  });

  it("is decided when the reply ends, if nothing came after it", () => {
    const view = render(<MessageList state={reply(`intro\n\n${TABLE}\n`, true)} {...props} />);
    expect(view.container.querySelector(".table-list")).toBeNull();
    act(() => view.rerender(<MessageList state={reply(`intro\n\n${TABLE}\n`, false)} {...props} />));
    expect(view.container.querySelector(".table-list")).not.toBeNull();
  });
});
