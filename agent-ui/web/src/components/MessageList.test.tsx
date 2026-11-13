// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { MessageList } from "./MessageList";
import { noteUserScroll, resumeFollowing } from "../follow";
import { applyEvent, initialState } from "../reducer";
import type { AgentDomainEvent, AgentUiState, NoticeRecord } from "../types";
import { buildTimeline, UNSEEN_TURN_TEXT } from "../timeline";
import foregroundAgentFixture from "../fixtures/claude-scripted-c03_fg.json";
import backgroundAgentFixture from "../fixtures/claude-scripted-c03_bg.json";
import subagentFixtures from "../fixtures/subagent-frames.json";
import questionFixtures from "../fixtures/questions.json";
import { agentCard, buildDisplay } from "../display";
import cardRosterFixture from "../fixtures/claude-scripted-c01t.json";
import { primaryText } from "../copyText";

// See EmptyTab.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);

beforeAll(() => {
  // jsdom implements no layout, so `scrollIntoView` does not exist on Element at all -- the
  // auto-scroll effect would throw before a single assertion ran.
  Element.prototype.scrollIntoView = vi.fn();
});

function state(overrides: Partial<AgentUiState>): AgentUiState {
  return { ...initialState(), ...overrides };
}

/* Fixtures omit `seq` for readability; the helper assigns it to preserve their written order. */
function texts(...values: string[]) {
  return values.map((text, i) => ({ seq: i, text }));
}

/* `cursor` is a prop because the cursor must be
   visible, `.row-current`/`aria-current`). Every test above this line predates it and is not
   testing the cursor -- `cursor={-1}` is passed throughout them (never a real timeline index) so
   none of their rows render as current, preserving exactly the rendering these tests already
   assert on. The cursor-highlight behaviour itself is its own describe block, at the bottom. */

/* Rows share a sign-column grid and distinguish state by glyph as well as colour. Keep the
   finished calls apart: adjacent finished calls collapse into a run, hiding the separate glyphs
   this fixture must compare. */
describe("MessageList row structure", () => {
  it("gives every row a sign column, and distinguishes state by glyph not only colour", () => {
    render(
      <MessageList
        state={{
          ...initialState(),
          // `◐` is a call still running inside the active turn (an ended turn's is `·`).
          activeTurnId: "t1",
          userPrompts: [{ seq: 1, text: "do the thing" }],
          transcript: [
            { seq: 2, text: "on it" },
            { seq: 5, text: "now the other" },
          ],
          toolCalls: [
            { seq: 3, toolUseId: "a", name: "Read", input: {}, result: null, turnId: "t1" },
            { seq: 4, toolUseId: "b", name: "Read", input: {}, result: { content: "ok", isError: false } },
            { seq: 6, toolUseId: "c", name: "Read", input: {}, result: { content: "boom", isError: true } },
          ],
        }}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={() => {}}
      />,
    );
    const signs = Array.from(document.querySelectorAll<HTMLElement>(".row")).map((r) => r.dataset.sign);
    expect(signs).toEqual(["›", "", "◐", "✓", "", "✗"]);
  });
});

describe("MessageList transcript rendering", () => {
  it("renders assistant text as markdown, not as escaped source", () => {
    const { container } = render(
      <MessageList
        state={state({ transcript: texts("# Heading\n\nsome **bold** text") })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    expect(container.querySelector(".row-assistant h1")?.textContent).toBe("Heading");
    expect(container.querySelector(".row-assistant strong")?.textContent).toBe("bold");
  });

  it("renders one block per transcript entry, in order", () => {
    const { container } = render(
      <MessageList
        state={state({ transcript: texts("first", "second") })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    const messages = Array.from(container.querySelectorAll(".row-assistant"));
    expect(messages.map((m) => m.textContent?.trim())).toEqual(["first", "second"]);
  });
});

/* Rust's `seq` gives messages, tool calls and cards one order, shared by live events and snapshots.
   Rendering separate lists by kind would misrepresent turns with interleaved calls. */
describe("MessageList interleaves the conversation in its real order", () => {
  /** Every rendered row, in DOM order, as a short label. */
  function rendered(container: HTMLElement): string[] {
    return Array.from(container.querySelectorAll<HTMLElement>(".row")).map((el) => {
      if (el.classList.contains("row-assistant")) return `text:${el.textContent?.trim()}`;
      if (el.classList.contains("row-permission")) return "perm";
      if (el.classList.contains("row-tool")) return `tool:${el.querySelector(".tool-call")?.getAttribute("data-tool-name") ?? "?"}`;
      // This helper knows only text/permission/tool fixtures. Fail loudly for another row kind
      // rather than silently mislabelling it as "tool:?".
      throw new Error(`rendered(): row has none of the classes this helper knows about: "${el.className}"`);
    });
  }

  it("renders text, tool, text, tool in that order rather than both texts first", () => {
    const { container } = render(
      <MessageList
        state={state({
          transcript: [
            { seq: 1, text: "I'll check." },
            { seq: 3, text: "And now the other." },
            { seq: 6, text: "Done." },
          ],
          toolCalls: [
            { seq: 2, toolUseId: "toolu_1", name: "Bash", input: {}, result: null },
            { seq: 4, toolUseId: "toolu_2", name: "Read", input: {}, result: null },
          ],
        })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    expect(rendered(container)).toEqual([
      "text:I'll check.",
      "tool:Bash",
      "text:And now the other.",
      "tool:Read",
      "text:Done.",
    ]);
  });

  it("renders a linked permission card directly beneath the tool call it gates", () => {
    const { container } = render(
      <MessageList
        state={state({
          transcript: [
            { seq: 1, text: "before" },
            { seq: 5, text: "after" },
          ],
          toolCalls: [
            { seq: 2, toolUseId: "toolu_1", name: "Bash", input: {}, result: null },
            { seq: 3, toolUseId: "toolu_2", name: "Read", input: {}, result: null },
          ],
          pendingPermissions: [{ seq: 4, permissionId: "perm-1", toolUseId: "toolu_1", toolName: "Bash", input: {} }],
        })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    expect(rendered(container)).toEqual(["text:before", "tool:Bash", "perm", "tool:Read", "text:after"]);
  });

  /* The legacy backend sends no `tool_use_id`, so a card with no tool-call link is that backend's
     ordinary case, not an edge case. The card still has to land where it arrived. */
  it("still places a permission card with no tool-call link, by its own arrival position", () => {
    const { container } = render(
      <MessageList
        state={state({
          transcript: [
            { seq: 1, text: "before" },
            { seq: 4, text: "after" },
          ],
          toolCalls: [{ seq: 2, toolUseId: "toolu_1", name: "Bash", input: {}, result: null }],
          pendingPermissions: [{ seq: 3, permissionId: "perm-1", toolUseId: null, toolName: "Bash", input: {} }],
        })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    expect(rendered(container)).toEqual(["text:before", "tool:Bash", "perm", "text:after"]);
  });
});

/* Sanitization must keep model output from impersonating navigation controls. Model output is rendered with
   `dangerouslySetInnerHTML`, and model output is attacker-influenceable (a repository file, a web
   page the model fetched, a tool result). Script execution inside THIS document is not a contained
   XSS -- the panel holds `window.webkit.messageHandlers.eitriAgent`, the same bridge that relays
   permission decisions to Rust, so a script here can approve its own tool calls. These tests assert
   the stripping for real rather than trusting that the DOMPurify call is still in place. */
describe("MessageList sanitizes model output", () => {
  function html(markdown: string): string {
    const { container } = render(
      <MessageList
        state={state({ transcript: texts(markdown) })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    return container.querySelector(".row-assistant .row-body")!.innerHTML;
  }

  it("strips a raw <script> payload out of model output", () => {
    const rendered = html('Here you go:\n\n<script>window.__pwned = true;</script>\n');
    expect(rendered).not.toContain("<script");
    expect(rendered).not.toContain("__pwned");
    expect((window as unknown as { __pwned?: boolean }).__pwned).toBeUndefined();
  });

  it("strips an inline event handler, the payload that needs no <script> tag at all", () => {
    const rendered = html('<img src="x" onerror="window.__pwned = true">');
    expect(rendered.toLowerCase()).not.toContain("onerror");
  });

  it("strips a javascript: URL from a link the model wrote", () => {
    const rendered = html('[click me](javascript:window.__pwned=true)');
    expect(rendered.toLowerCase()).not.toContain("javascript:");
  });

  it("does not fire an injected handler even when the element survives", () => {
    const { container } = render(
      <MessageList
        state={state({ transcript: texts('<button onclick="window.__pwned = true">go</button>') })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    const button = container.querySelector(".row-assistant button");
    if (button) fireEvent.click(button);
    expect((window as unknown as { __pwned?: boolean }).__pwned).toBeUndefined();
  });

  it("still renders ordinary formatting -- sanitizing must not mean rendering nothing", () => {
    const rendered = html("a `code` span and a [link](https://example.com)");
    expect(rendered).toContain("<code>code</code>");
    expect(rendered).toContain('href="https://example.com"');
  });
});

describe("MessageList tool calls and permissions", () => {
  it("renders each tool call keyed on its own tool_use_id", () => {
    const { container } = render(
      <MessageList
        state={state({
          activeTurnId: "t1",
          toolCalls: [
            { seq: 0, toolUseId: "toolu_1", name: "Bash", input: { command: "echo one" }, result: null, turnId: "t1" },
            { seq: 1, toolUseId: "toolu_2", name: "Bash", input: { command: "echo two" }, result: { content: "two", isError: false } },
          ],
        })}
        sessionEnded={false}
        // t-0 is running and always expanded. Expand finished t-1 explicitly: this test checks
        // tool_use_id keying rather than the folded placeholder.
        expanded={{ "t-1": true }}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    const calls = Array.from(container.querySelectorAll(".tool-call"));
    expect(calls).toHaveLength(2);
    expect(calls[0].querySelector(".tool-result")!.getAttribute("data-state")).toBe("running");
    expect(calls[1].querySelector(".tool-result")!.getAttribute("data-state")).toBe("done");
  });

  /* A FINISHED call with no entry in `expanded` shows the folded placeholder,
     not its result. This once had zero coverage -- every other test here either
     leaves a call running (which always shows, folded or not) or opts out of the fold via
     `expanded`, so a regression that ignored `showResult` entirely, or a flipped `=== true` /
     `!== false` comparison at the call site, would pass all 206 tests unnoticed. */
  it("folds a finished call's result by default, per the empty `expanded` map", () => {
    const { container } = render(
      <MessageList
        state={state({
          toolCalls: [{ seq: 0, toolUseId: "toolu_1", name: "Bash", input: { command: "echo hi" }, result: { content: "hi", isError: false } }],
        })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    expect(container.querySelector('[data-folded="true"]')).not.toBeNull();
    expect(container.querySelector(".tool-result")).toBeNull();
  });

  /* The whole point of plumbing tool_use_id to the frontend: with two Bash calls in one turn, a card
     that only says "Bash" identifies neither of them. */
  it("marks the specific tool call a pending permission is waiting on", () => {
    const { container } = render(
      <MessageList
        state={state({
          toolCalls: [
            { seq: 0, toolUseId: "toolu_1", name: "Bash", input: { command: "echo one" }, result: null },
            { seq: 1, toolUseId: "toolu_2", name: "Bash", input: { command: "rm -rf /" }, result: null },
          ],
          pendingPermissions: [
            { seq: 2, permissionId: "perm-1", toolUseId: "toolu_2", toolName: "Bash", input: { command: "rm -rf /" } },
          ],
        })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    const awaiting = Array.from(container.querySelectorAll(".row-tool [data-awaiting-permission='true']"));
    expect(awaiting).toHaveLength(1);
    // A gated call says it is waiting rather than repeating the command a card below
    // it already shows.
    expect(awaiting[0].textContent).toContain("waiting for approval");
    expect(awaiting[0].textContent).not.toContain("rm -rf /");
  });

  it("a_call_on_an_editor_card_shows_waiting_for_approval", () => {
    const { container } = render(
      <MessageList
        state={state({
          toolCalls: [
            { seq: 0, toolUseId: "toolu_1", name: "mcp__eitri__open_file", input: { path: "a.rs" }, result: null },
            { seq: 1, toolUseId: "toolu_2", name: "mcp__eitri__open_file", input: { path: "/etc/hosts" }, result: null },
          ],
          editorCards: [
            {
              seq: 2, permissionId: "ec-0123456789abcdef-1", toolUseId: "toolu_2", toolName: "open_file", input: null,
              editorCard: { title: "Show /etc/hosts in your editor?", body: [{ kind: "code", text: "/etc/hosts" }] },
            },
          ],
        })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    const awaiting = Array.from(container.querySelectorAll(".row-tool [data-awaiting-permission='true']"));
    expect(awaiting).toHaveLength(1);
    expect(awaiting[0].textContent).toContain("waiting for approval");
    // The card is drawn under its call, with its own title.
    expect(container.querySelector(".permission-card")!.textContent).toContain("Show /etc/hosts in your editor?");
  });

  /* In the legacy hook-relay shape, permissionId and toolUseId are the same string from the
     PreToolUse payload. The waiting marker still matches by toolUseId; card keying must not affect it. */
  it("marks the right call when the permission's id is also its tool_use_id", () => {
    const { container } = render(
      <MessageList
        state={state({
          toolCalls: [
            { seq: 1, toolUseId: "toolu_first", name: "Bash", input: { command: "echo one" }, result: null },
            { seq: 2, toolUseId: "toolu_second", name: "Bash", input: { command: "rm -rf /" }, result: null },
          ],
          pendingPermissions: [
            { seq: 3, permissionId: "toolu_second", toolUseId: "toolu_second", toolName: "Bash", input: { command: "rm -rf /" } },
          ],
        })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    const awaiting = Array.from(container.querySelectorAll(".row-tool [data-awaiting-permission='true']"));
    expect(awaiting).toHaveLength(1);
    expect(awaiting[0].textContent).toContain("waiting for approval");
    expect(container.querySelector(".permission-card-tool")!.getAttribute("data-tool-use-id")).toBe("toolu_second");
  });

  it("marks nothing when the pending permission carries no tool_use_id", () => {
    const { container } = render(
      <MessageList
        state={state({
          toolCalls: [{ seq: 0, toolUseId: "toolu_1", name: "Bash", input: { command: "echo one" }, result: null }],
          pendingPermissions: [{ seq: 1, permissionId: "perm-1", toolUseId: null, toolName: "Bash", input: {} }],
        })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    expect(container.querySelector(".row-tool [data-awaiting-permission='true']")).toBeNull();
  });

  /* proto3 has no absent-string: an unset `tool_use_id` arrives as "" on both the permission and
     the tool call, so a null-only filter admitted "" into the awaiting set and then matched it
     against an unrelated call whose own id was also "". `PermissionCard` already guarded exactly
     this case, with its own test; this is the same hazard on the other side of the link. */
  it("does not cross-link a permission and a tool call that both carry a proto3 empty-string id", () => {
    const { container } = render(
      <MessageList
        state={state({
          toolCalls: [{ seq: 0, toolUseId: "", name: "Bash", input: { command: "echo one" }, result: null }],
          pendingPermissions: [{ seq: 1, permissionId: "perm-1", toolUseId: "", toolName: "Bash", input: {} }],
        })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    expect(container.querySelector(".row-tool [data-awaiting-permission='true']")).toBeNull();
    expect(container.querySelector(".tool-awaiting-permission")).toBeNull();
  });

  it("renders one card per pending permission and forwards a decision with its own id", () => {
    const onAnswerPermission = vi.fn();
    const { container } = render(
      <MessageList
        state={state({
          pendingPermissions: [
            { seq: 0, permissionId: "perm-1", toolUseId: "toolu_1", toolName: "Bash", input: {} },
            { seq: 1, permissionId: "perm-2", toolUseId: "toolu_2", toolName: "Write", input: {} },
          ],
        })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={onAnswerPermission}
      />,
    );
    const cards = Array.from(container.querySelectorAll(".permission-card"));
    expect(cards).toHaveLength(2);
    const approve = Array.from(cards[1].querySelectorAll("button")).find((b) => b.textContent === "Approve")!;
    // A click Enter or Space synthesised on a focused button (`detail` 0) answers nothing; a real one does.
    fireEvent.click(approve, { detail: 0 });
    expect(onAnswerPermission).not.toHaveBeenCalled();
    fireEvent.click(approve, { detail: 1 });
    expect(onAnswerPermission).toHaveBeenCalledWith("perm-2", "allow", undefined, undefined, expect.anything());
  });

  it("passes a dead session through to every card, so none can submit into it", () => {
    const onAnswerPermission = vi.fn();
    const { container } = render(
      <MessageList
        state={state({ pendingPermissions: [{ seq: 0, permissionId: "perm-1", toolUseId: null, toolName: "Bash", input: {} }] })}
        sessionEnded
        expanded={{}}
        cursor={-1}
        onAnswerPermission={onAnswerPermission}
      />,
    );
    const approve = Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Approve")!;
    expect(approve.disabled).toBe(true);
    fireEvent.click(approve);
    expect(onAnswerPermission).not.toHaveBeenCalled();
  });
});

/* A null result can mean abandoned rather than running. MessageList has the call's seq, the
   history boundary and sessionEnded together, so isAbandonedCall decides before ToolResult renders.
   Otherwise replay or an ended turn leaves a permanent spinner. */
describe("MessageList: an abandoned tool result never spins forever (sw-panel-render-6)", () => {
  const restoredTo = (uptoSeq: number) => ({
    source: "claude_transcript" as const,
    restoredItems: uptoSeq,
    omittedItems: 0,
    uptoSeq,
    sourcePath: "/claude/projects/p/sess.jsonl",
    attemptedTranscriptPath: null,
    fallbackReason: null,
    writerVersion: "2.1.272",
  });

  it("draws a static notice, not a spinner, for a null result that predates the restored-history boundary", () => {
    // With history.uptoSeq 3 and a call at seq 2 whose result is null, replay cannot deliver
    // a completion for a call that already existed when history was cut.
    const { container } = render(
      <MessageList
        state={state({
          history: restoredTo(3),
          toolCalls: [{ seq: 2, toolUseId: "toolu_1", name: "Bash", input: { command: "long-running" }, result: null }],
        })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    const result = container.querySelector(".tool-result")!;
    expect(result.getAttribute("data-state")).toBe("none");
    expect(result.getAttribute("aria-busy")).toBeNull();
    expect(result.textContent).toContain("no result recorded");
  });

  it("draws the same static notice once the session has ended, regardless of the history boundary", () => {
    const { container } = render(
      <MessageList
        state={state({
          toolCalls: [{ seq: 9, toolUseId: "toolu_1", name: "Bash", input: { command: "long-running" }, result: null }],
        })}
        sessionEnded={true}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    const result = container.querySelector(".tool-result")!;
    expect(result.getAttribute("data-state")).toBe("none");
  });

  /* An INTERRUPTED turn in a fresh session -- `tool_call_started`, then
     `turn_completed` with no result -- left `activeTurnId` null but `status` running and `history`
     null, so the call kept its `◐` and `Running…` forever. A call's result only ever arrives inside
     its own turn; once that turn is not the active one, it never will, and that must hold after
     the next turn starts too. Folded through the real reducer, not a hand-built state. */
  it("stops spinning once the call's own turn has ended, and stays stopped when the next turn starts", () => {
    const events: AgentDomainEvent[] = [
      { type: "session_opened", session_id: "s1", provider_session_id: "c1", model: "m", cwd: "/p" },
      { type: "user_prompt_submitted", text: "run it" },
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "sleep 100" } },
    ];
    const running = events.reduce(applyEvent, initialState());
    const rendered = (s: AgentUiState) =>
      render(<MessageList state={s} sessionEnded={false} expanded={{}} cursor={-1} onAnswerPermission={vi.fn()} />)
        .container;
    expect(rendered(running).querySelector(".tool-result")!.getAttribute("data-state")).toBe("running");
    cleanup();

    const interrupted = applyEvent(running, {
      type: "turn_completed",
      turn_id: "t1",
      outcome: "interrupted",
      result_text: "",
      stop_reason: null,
      usage: null,
      detail: { reason: null, api_error_status: null, message: null },
    });
    let result = rendered(interrupted).querySelector(".tool-result")!;
    expect(result.getAttribute("data-state")).toBe("none");
    expect(result.getAttribute("aria-busy")).toBeNull();
    cleanup();

    const nextTurn: AgentDomainEvent[] = [
      { type: "user_prompt_submitted", text: "again" },
      { type: "turn_started", turn_id: "t2" },
    ];
    const next = nextTurn.reduce(applyEvent, interrupted);
    result = rendered(next).querySelector(".tool-result")!;
    expect(result.getAttribute("data-state"), "the next turn does not bring the old call back to life").toBe("none");
  });

  it("keeps the running spinner for a null result still inside the live, unfinished session", () => {
    const { container } = render(
      <MessageList
        state={state({
          history: restoredTo(3),
          activeTurnId: "t1",
          toolCalls: [
            { seq: 5, toolUseId: "toolu_1", name: "Bash", input: { command: "long-running" }, result: null, turnId: "t1" },
          ],
        })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    const result = container.querySelector(".tool-result")!;
    expect(result.getAttribute("data-state")).toBe("running");
    expect(result.getAttribute("aria-busy")).toBe("true");
  });
});

/* New rows must not pull a parked reader away from j/k navigation. The cursor alone cannot say
   whether a mouse reader is following, and measuring after growth mistakes a tall new row for
   scrolling away. jsdom has no layout or automatic scroll events: set dimensions and dispatch them. */
function setScroll(list: HTMLElement, dims: { scrollHeight: number; clientHeight: number; scrollTop: number }) {
  Object.defineProperty(list, "scrollHeight", { value: dims.scrollHeight, configurable: true });
  Object.defineProperty(list, "clientHeight", { value: dims.clientHeight, configurable: true });
  Object.defineProperty(list, "scrollTop", { value: dims.scrollTop, configurable: true, writable: true });
}

/* Following snaps to the end: a smooth animation leaves a new row below the edge for 1-2 frames. */
describe("MessageList auto-follow", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: 0, onAnswerPermission: vi.fn() };

  it("still follows a new item to the bottom when the viewport was already near it", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("first") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    dims.grow(2080);

    rerender(<MessageList state={state({ transcript: texts("first", "second") })} {...props} />);

    expect(list.scrollTop).toBe(1680);
  });

  /* Measuring after a new row mounts mistakes growth beyond the 24px threshold for scrolling
     away. Grow by 600px without a scroll event while the reader is at the bottom. */
  it("still follows when the new row alone is taller than the threshold", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("first") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    dims.grow(2600);

    rerender(<MessageList state={state({ transcript: texts("first", "second") })} {...props} />);

    expect(list.scrollTop).toBe(2200);
  });

  /* Snap in the mounting commit, before paint: animation leaves the user's prompt as the last
     visible text for a frame or two when the reply's first row appears. */
  it("brings a new row into view in the same commit, by a snap and never a smooth scroll", () => {
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    const { container, rerender } = render(<MessageList state={state({ userPrompts: texts("why?") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    scrollIntoView.mockClear();
    dims.grow(2044); // the reply's first row mounts below the edge

    rerender(<MessageList state={state({ userPrompts: texts("why?"), transcript: [{ seq: 1, text: "Because" }] })} {...props} />);

    expect(list.scrollTop).toBe(1644);
    expect(scrollIntoView).not.toHaveBeenCalled();
  });

  it("does not fight a user reading further up: skips the follow once the user scrolled toward the top", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("first") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1600 });
    fireEvent.scroll(list);
    // A real wheel: intent first, then its scroll. Direction alone cannot identify user input.
    fireEvent.wheel(list, { deltaY: -100 });
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 800 });
    fireEvent.scroll(list);
    setScroll(list, { scrollHeight: 2300, clientHeight: 400, scrollTop: 800 });

    rerender(<MessageList state={state({ transcript: texts("first", "second") })} {...props} />);

    expect(list.scrollTop).toBe(800);
  });

  it("resumes following once the user scrolls back to the bottom", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("first") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1600 });
    fireEvent.scroll(list);
    fireEvent.wheel(list, { deltaY: -100 });
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 800 });
    fireEvent.scroll(list);
    fireEvent.wheel(list, { deltaY: 100 });
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1590 });
    fireEvent.scroll(list);
    setScroll(list, { scrollHeight: 2300, clientHeight: 400, scrollTop: 1590 });

    rerender(<MessageList state={state({ transcript: texts("first", "second") })} {...props} />);

    // `setScroll`'s `scrollTop` does not clamp, so the snap reads as `scrollHeight` itself.
    expect(list.scrollTop).toBe(2300);
  });
});

/* With the view at the bottom, a reply that grows
   while it streams stays in view -- before this, only a NEW row was ever followed. Growth is one
   transcript entry getting longer, so the four lengths do not change; the list's `scrollTop` is
   snapped to its end rather than smooth-scrolled. */
describe("MessageList follows a reply growing while it streams", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: 0, onAnswerPermission: vi.fn() };

  it("snaps to the end when the last reply grows and the view was at the bottom", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("par") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1600 });
    fireEvent.scroll(list);
    setScroll(list, { scrollHeight: 2300, clientHeight: 400, scrollTop: 1600 });

    rerender(<MessageList state={state({ transcript: texts("partial reply, longer now") })} {...props} />);

    expect(list.scrollTop).toBe(2300);
  });

  it("leaves the view alone when the user has scrolled up", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("par") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1600 });
    fireEvent.scroll(list);
    fireEvent.wheel(list, { deltaY: -100 });
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 800 });
    fireEvent.scroll(list);
    setScroll(list, { scrollHeight: 2300, clientHeight: 400, scrollTop: 800 });

    rerender(<MessageList state={state({ transcript: texts("partial reply, longer now") })} {...props} />);

    expect(list.scrollTop).toBe(800);
  });

  /* A browser delivers `scroll` at the next frame, so a `k` that scrolled up can still be
     unannounced when the next delta arrives. Since 2026-09-24 `k` announces itself first
     (`noteUserScroll`, as `App.tsx` does); the effect also still reads the position itself. */
  it("honours a scroll up whose scroll event has not arrived yet", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("par") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1600 });
    fireEvent.scroll(list);
    // Scrolled up by `k`, no scroll event dispatched; the content then grows.
    noteUserScroll(list, "up");
    setScroll(list, { scrollHeight: 2300, clientHeight: 400, scrollTop: 1540 });

    rerender(<MessageList state={state({ transcript: texts("partial reply, longer now") })} {...props} />);

    expect(list.scrollTop).toBe(1540);
  });
});

/* A list whose `scrollTop` clamps the way a
   browser's does, so the snap lands where a real one would. */
function clampedList(list: HTMLElement, clientHeight: number, start: { scrollHeight: number; scrollTop: number }) {
  let height = start.scrollHeight;
  let top = start.scrollTop;
  Object.defineProperty(list, "clientHeight", { value: clientHeight, configurable: true });
  Object.defineProperty(list, "scrollHeight", { configurable: true, get: () => height });
  Object.defineProperty(list, "scrollTop", {
    configurable: true,
    get: () => top,
    set: (v: number) => {
      top = Math.max(0, Math.min(height - clientHeight, v));
    },
  });
  return {
    grow(to: number) {
      height = to;
      top = Math.min(top, Math.max(0, height - clientHeight));
    },
  };
}

describe("MessageList follow guard: a scroll up always wins", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: 0, onAnswerPermission: vi.fn() };

  it("a k step that ends inside the 24px threshold still stops following", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("par") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    noteUserScroll(list, "up");
    list.scrollTop = 1590; // a 10px `k`: the rest of the row above was only 10px
    fireEvent.scroll(list);
    dims.grow(2300);

    rerender(<MessageList state={state({ transcript: texts("partial reply, longer now") })} {...props} />);

    expect(list.scrollTop).toBe(1590);
  });

  it("an update that moves nothing does not re-arm following near the bottom", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("par") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    noteUserScroll(list, "up");
    list.scrollTop = 1590;
    fireEvent.scroll(list);

    // The state changes but the height does not (text replaced in place): the effect's own
    // synchronous read finds the view 10px from the bottom and must not take that as "back".
    rerender(<MessageList state={state({ transcript: texts("pat") })} {...props} />);

    expect(list.scrollTop).toBe(1590);
  });

  it("a k pressed right after a snap, before the snap's scroll event, is seen as a scroll up", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("p") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    dims.grow(2300);
    rerender(<MessageList state={state({ transcript: texts("pa") })} {...props} />);
    expect(list.scrollTop).toBe(1900); // snapped
    noteUserScroll(list, "up");
    list.scrollTop -= 60; // `k`, with no scroll event yet for either scroll
    dims.grow(2400);

    rerender(<MessageList state={state({ transcript: texts("par") })} {...props} />);

    expect(list.scrollTop).toBe(1840);
  });

  it("content shrinking under a user at the bottom (scrollTop clamped down) keeps following", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("first") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    dims.grow(1900); // a row went away: the browser clamps scrollTop to 1500 and fires scroll
    fireEvent.scroll(list);
    dims.grow(2200);

    rerender(<MessageList state={state({ transcript: texts("first", "second") })} {...props} />);

    expect(list.scrollTop).toBe(1800);
  });
});

/* Following ends on user intent: an engine-originated scrollTop drop, including one on a
   status-clock tick, must not be mistaken for the reader leaving the bottom. */
describe("MessageList follows by intent, not by the direction of a scroll", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: 0, onAnswerPermission: vi.fn() };

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("keeps following through a drop nobody asked for, and the next delta puts the view back", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("p") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    // The engine moves the view up by itself: no wheel, no pointer, no key.
    list.scrollTop = 1100;
    fireEvent.scroll(list);
    dims.grow(2300);

    rerender(<MessageList state={state({ transcript: texts("pa") })} {...props} />);

    expect(list.scrollTop).toBe(1900);
  });

  it("follows a new row even after such a drop", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("first") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    list.scrollTop = 1100;
    fireEvent.scroll(list);
    dims.grow(2080);

    rerender(<MessageList state={state({ transcript: texts("first", "second") })} {...props} />);

    expect(list.scrollTop).toBe(1680);
  });

  // "any wheel up" that the list itself can take -- one that moves nothing is its own
  // block below.
  it("stops following on any wheel up the list can take, however small (deliberately)", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("p") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    // The wheel's own scroll has not arrived when the next delta does.
    fireEvent.wheel(list, { deltaY: -2 });
    dims.grow(2300);

    rerender(<MessageList state={state({ transcript: texts("pa") })} {...props} />);

    expect(list.scrollTop).toBe(1600);
  });

  it("takes a scroll made while a pointer is held on the list (its scrollbar) as the user's", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("p") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    fireEvent.pointerDown(list);
    list.scrollTop = 900;
    fireEvent.scroll(list);
    fireEvent.pointerUp(window);
    dims.grow(2300);

    rerender(<MessageList state={state({ transcript: texts("pa") })} {...props} />);

    expect(list.scrollTop).toBe(900);
  });

  it("does not take a drop long after the last wheel for the user's", () => {
    let now = 1000;
    vi.spyOn(performance, "now").mockImplementation(() => now);
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("p") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    fireEvent.wheel(list, { deltaY: 100 }); // a wheel DOWN at the bottom: steering, nothing to scroll
    now += 5000; // ...and a drop five seconds later, with nothing in between
    list.scrollTop = 1100;
    fireEvent.scroll(list);
    dims.grow(2300);

    rerender(<MessageList state={state({ transcript: texts("pa") })} {...props} />);

    expect(list.scrollTop).toBe(1900);
  });

  it("re-arms when the panel's own scroll down (G, j, Ctrl+d) ends near the bottom", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("p") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    noteUserScroll(list, "up");
    list.scrollTop = 800;
    fireEvent.scroll(list);
    noteUserScroll(list, "down");
    list.scrollTop = 1590; // 10px short: inside the follow threshold, going down
    fireEvent.scroll(list);
    dims.grow(2300);

    rerender(<MessageList state={state({ transcript: texts("pa") })} {...props} />);

    expect(list.scrollTop).toBe(1900);
  });
});

/* A wheel intent is not necessarily a successful scroll: an inner box or a sideways gesture
   can consume it while the list must keep following. */
describe("MessageList: a send resumes following", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: 0, onAnswerPermission: vi.fn() };

  it("snaps a reader who scrolled up to the end, and follows the reply that streams after", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("p") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    noteUserScroll(list, "up");
    list.scrollTop = 500;
    fireEvent.scroll(list);
    dims.grow(2100); // the new prompt
    rerender(<MessageList state={state({ transcript: texts("p"), userPrompts: [{ seq: 1, text: "q" }] })} {...props} />);
    expect(list.scrollTop).toBe(500); // parked: the prompt alone does not move a reader

    resumeFollowing(list);
    expect(list.scrollTop).toBe(1700);
    dims.grow(2400); // the reply streams in
    rerender(
      <MessageList
        state={state({ transcript: [{ seq: 0, text: "p" }, { seq: 2, text: "reply" }], userPrompts: [{ seq: 1, text: "q" }] })}
        {...props}
      />,
    );
    expect(list.scrollTop).toBe(2000);
  });

  it("still lets a scroll up after the send stop following", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("p") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 600 });
    resumeFollowing(list);
    expect(list.scrollTop).toBe(1600);
    noteUserScroll(list, "up");
    list.scrollTop = 900;
    fireEvent.scroll(list);
    dims.grow(2300);
    rerender(<MessageList state={state({ transcript: texts("pa") })} {...props} />);
    expect(list.scrollTop).toBe(900);
  });
});

describe("MessageList: every route a user scrolls by is theirs, and a gesture that moves nothing ends nothing", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: 0, onAnswerPermission: vi.fn() };

  afterEach(() => {
    vi.restoreAllMocks();
  });

  /** A clock the steering window reads, advanced by hand so no test depends on how fast it runs. */
  function fakeClock() {
    const clock = { now: 1000 };
    vi.spyOn(performance, "now").mockImplementation(() => clock.now);
    return clock;
  }

  function mount(start: { scrollHeight: number; scrollTop: number }) {
    const view = render(<MessageList state={state({ transcript: texts("p") })} {...props} />);
    const list = view.container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, start);
    fireEvent.scroll(list);
    const row = list.querySelector(".row") as HTMLElement;
    const delta = (text: string) => view.rerender(<MessageList state={state({ transcript: texts(text) })} {...props} />);
    return { list, dims, row, delta };
  }

  // --- (a) routes the panel does not own ------------------------------------------------------

  it("keeps a reader who scrolled up with the browser's own PageUp, a control inside the list focused", () => {
    fakeClock();
    const { list, dims, row, delta } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.keyDown(row, { key: "PageUp" });
    list.scrollTop = 1240; // the key's default action: `resolveKey` has none, so the browser scrolls
    fireEvent.scroll(list);
    dims.grow(2300);

    delta("pa");

    expect(list.scrollTop).toBe(1240);
  });

  it("does not snap over a PageUp whose (animated) scroll has not moved the list yet", () => {
    fakeClock();
    const { list, dims, row, delta } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.keyDown(row, { key: "PageUp" });
    dims.grow(2300);

    delta("pa");

    // 1900 would be a snap, which cancels the scroll the key just started.
    expect(list.scrollTop).toBe(1600);
  });

  it("takes an arrow-key scroll up the same way, and Shift+Space", () => {
    const clock = fakeClock();
    const { list, dims, row, delta } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.keyDown(row, { key: "ArrowUp" });
    list.scrollTop = 1560;
    fireEvent.scroll(list);
    dims.grow(2300);
    delta("pa");
    expect(list.scrollTop).toBe(1560);

    // Back to the bottom, then Shift+Space.
    clock.now += 1000;
    fireEvent.keyDown(row, { key: "End" });
    list.scrollTop = 1900;
    fireEvent.scroll(list);
    fireEvent.keyDown(row, { key: " ", shiftKey: true });
    list.scrollTop = 1540;
    fireEvent.scroll(list);
    dims.grow(2600);
    delta("par");
    expect(list.scrollTop).toBe(1540);
  });

  it("takes a scroll that focus landing inside the list causes (a Tab reveal) as the user's", () => {
    fakeClock();
    const { list, dims, row, delta } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.focusIn(row);
    list.scrollTop = 900;
    fireEvent.scroll(list);
    dims.grow(2300);

    delta("pa");

    expect(list.scrollTop).toBe(900);
  });

  it("guard: a key that scrolls nothing (a letter) is no licence for the engine to move the reader", () => {
    fakeClock();
    const { list, dims, row, delta } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.keyDown(row, { key: "x" });
    list.scrollTop = 1100;
    fireEvent.scroll(list);
    dims.grow(2300);

    delta("pa");

    expect(list.scrollTop).toBe(1900);
  });

  it("guard: an arrow key typed in a text box inside the list (a permission's reason) does not stop following", () => {
    fakeClock();
    const { list, dims, row, delta } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    const box = document.createElement("input");
    row.appendChild(box);
    fireEvent.keyDown(box, { key: "ArrowUp" });
    dims.grow(2300);

    delta("pa");

    expect(list.scrollTop).toBe(1900);
  });

  // --- (b) a wheel that never moves the list ----------------------------------------------------

  it("keeps following through a sideways swipe that carries a sub-pixel vertical jitter", () => {
    fakeClock();
    const { list, dims, row, delta } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(row, { deltaX: -40, deltaY: -0.5 });
    dims.grow(2300);

    delta("pa");

    expect(list.scrollTop).toBe(1900);
  });

  it("keeps following through a wheel up that a box inside the list (an expanded tool result) takes", () => {
    fakeClock();
    const { list, dims, row, delta } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    const box = document.createElement("div");
    box.style.overflowY = "auto";
    row.appendChild(box);
    const inner = document.createElement("pre");
    box.appendChild(inner);
    Object.defineProperty(box, "scrollTop", { value: 120, configurable: true, writable: true });
    fireEvent.wheel(inner, { deltaY: -100 });
    dims.grow(2300);

    delta("pa");

    expect(list.scrollTop).toBe(1900);
  });

  it("keeps following through a wheel up while the list has nothing above to scroll to", () => {
    fakeClock();
    const { list, dims, delta } = mount({ scrollHeight: 400, scrollTop: 0 });
    fireEvent.wheel(list, { deltaY: -100 });
    dims.grow(700);

    delta("pa");

    expect(list.scrollTop).toBe(300);
  });

  it("takes back a wheel's stop once the gesture is over and the list never went up", () => {
    const clock = fakeClock();
    const { list, dims, delta } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    // A wheel the list could have taken but did not (the engine latched the gesture elsewhere).
    fireEvent.wheel(list, { deltaY: -100 });
    dims.grow(2300);
    delta("pa");
    expect(list.scrollTop).toBe(1600); // inside the gesture: its scroll may still be on the way

    clock.now += 400;
    dims.grow(2600);
    delta("par");

    expect(list.scrollTop).toBe(2200);
  });

  it("guard: takes back a PageUp's stop the same way when the key moved nothing", () => {
    const clock = fakeClock();
    const { list, dims, row, delta } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.keyDown(row, { key: "PageUp" });
    clock.now += 400;
    dims.grow(2300);

    delta("pa");

    expect(list.scrollTop).toBe(1900);
  });

  it("guard: does not take back a wheel's stop when the list did go up", () => {
    const clock = fakeClock();
    const { list, dims, delta } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    list.scrollTop = 1200;
    fireEvent.scroll(list);
    clock.now += 5000;
    dims.grow(2300);

    delta("pa");

    expect(list.scrollTop).toBe(1200);
  });

  it("guard: does not take it back when the list went up but its scroll event never came", () => {
    const clock = fakeClock();
    const { list, dims, delta } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    list.scrollTop = 1200; // no scroll event
    clock.now += 5000;
    dims.grow(2300);

    delta("pa");

    expect(list.scrollTop).toBe(1200);
  });
});

/* Settle provisional stops even with no next state change: a card can arrive inside the
   gesture window and then leave the turn waiting, with nothing else to trigger a follow. */
describe("MessageList: a stop on a gesture that moved nothing is settled when the gesture ends", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: 0, onAnswerPermission: vi.fn() };
  const scrollIntoView = () => Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  /** The steering window reads `performance.now`; the take-back timer runs on `setTimeout`. One clock
   *  drives both, so a test never depends on how fast it runs. */
  function fakeTime() {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const clock = { now: 1000 };
    vi.spyOn(performance, "now").mockImplementation(() => clock.now);
    return (ms: number) => {
      clock.now += ms;
      act(() => {
        vi.advanceTimersByTime(ms);
      });
    };
  }

  function mount(start: { scrollHeight: number; scrollTop: number }) {
    const view = render(<MessageList state={state({ transcript: texts("p") })} {...props} />);
    const list = view.container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, start);
    fireEvent.scroll(list);
    const show = (...values: string[]) =>
      view.rerender(<MessageList state={state({ transcript: texts(...values) })} {...props} />);
    return { view, list, dims, show };
  }

  it("scrolls to a new row that arrived inside the gesture once it is over, with nothing after it", () => {
    const pass = fakeTime();
    const { list, dims, show } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 }); // the list could take it; the engine moved nothing
    pass(100);
    dims.grow(2300);
    show("p", "a new row"); // e.g. a permission card; the turn now waits for it
    expect(list.scrollTop).toBe(1600); // inside the gesture: its scroll may still come

    pass(5000); // silence: no state change at all

    expect(list.scrollTop).toBe(1900);
    expect(scrollIntoView()).not.toHaveBeenCalled();
  });

  it("snaps to the end of text that grew inside the gesture once it is over, with nothing after it", () => {
    const pass = fakeTime();
    const { list, dims, show } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    pass(100);
    dims.grow(2300);
    show("pa");
    expect(list.scrollTop).toBe(1600);

    pass(5000);

    expect(list.scrollTop).toBe(1900);
  });

  it("does the same for a PageUp from a focused control that moved nothing", () => {
    const pass = fakeTime();
    const { list, dims, show } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.keyDown(list.querySelector(".row") as HTMLElement, { key: "PageUp" });
    pass(100);
    dims.grow(2300);
    show("pa");

    pass(5000);

    expect(list.scrollTop).toBe(1900);
  });

  it("waits for a later wheel in the same gesture before it settles", () => {
    const pass = fakeTime();
    const { list, dims, show } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    pass(200);
    fireEvent.wheel(list, { deltaY: -100 }); // the window now runs to 300ms after THIS one
    dims.grow(2300);
    show("pa");

    pass(250); // past the first wheel's window, inside the second's
    expect(list.scrollTop).toBe(1600);

    pass(100);
    expect(list.scrollTop).toBe(1900);
  });

  it("waits while a pointer is held on the list, and settles once it is released and the window is over", () => {
    const pass = fakeTime();
    const { list, dims, show } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    fireEvent.pointerDown(list);
    dims.grow(2300);
    show("pa");

    pass(5000);
    expect(list.scrollTop).toBe(1600); // held: the user may be about to drag
    expect(vi.getTimerCount()).toBe(0); // and nothing polls while it is held; the release re-arms

    fireEvent.pointerUp(window);
    pass(250);
    expect(list.scrollTop).toBe(1600); // released, but scroll events from the drag may still land
    pass(100);
    expect(list.scrollTop).toBe(1900);
  });

  it("guard: leaves a reader whose wheel did move the list up where they are", () => {
    const pass = fakeTime();
    const { list, dims, show } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    list.scrollTop = 1200;
    fireEvent.scroll(list);
    dims.grow(2300);
    show("p", "a new row");

    pass(5000);

    expect(list.scrollTop).toBe(1200);
  });

  it("guard: leaves a reader whose wheel moved the list up with no scroll event yet where they are", () => {
    const pass = fakeTime();
    const { list } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    list.scrollTop = 1200; // its scroll event is late, and no state change reads it first

    pass(5000);

    expect(list.scrollTop).toBe(1200);
  });

  it("guard: the panel's own k during the gesture is a stop for good, not a provisional one", () => {
    const pass = fakeTime();
    const { list, dims, show } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    noteUserScroll(list, "up"); // `k`, whose own scroll has not landed yet
    dims.grow(2300);
    show("pa");

    pass(5000);

    expect(list.scrollTop).toBe(1600);
  });

  /* WebKitGTK can emit the first wheel scroll before anything moves; that event must not
     re-arm following and let the next delta cancel the wheel's animation. */
  it("keeps a wheel's stop through the wheel's own first scroll event when it reports no movement", () => {
    const pass = fakeTime();
    const { list, dims, show } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    fireEvent.scroll(list); // the wheel's first event: the list has not moved yet
    pass(10);
    dims.grow(2300);
    show("pa"); // the next delta, before the wheel's animation has moved anything

    // 1900 is the snap that cancels the wheel's scroll.
    expect(list.scrollTop).toBe(1600);
  });

  it("does the same when the next thing is a new row", () => {
    const pass = fakeTime();
    const { list, dims, show } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    fireEvent.scroll(list);
    pass(10);
    dims.grow(2300);
    show("p", "a new row");

    expect(list.scrollTop).toBe(1600);
  });

  it("guard: then lets the wheel's scroll, once it lands, stop following for good", () => {
    const pass = fakeTime();
    const { list, dims, show } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    fireEvent.scroll(list);
    pass(16);
    list.scrollTop = 1540; // the animation's first real step
    fireEvent.scroll(list);
    pass(5000);
    dims.grow(2300);
    show("pa");

    expect(list.scrollTop).toBe(1540);
  });

  it("guard: a wheel whose first event reports no movement and that then moves nothing is still taken back", () => {
    const pass = fakeTime();
    const { list, dims, show } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    fireEvent.scroll(list);
    dims.grow(2300);
    show("pa");

    pass(5000);

    expect(list.scrollTop).toBe(1900);
  });

  it("guard: content shrinking under a pending stop (the browser clamps scrollTop) still re-arms at the bottom", () => {
    const pass = fakeTime();
    const { list, dims, show } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    dims.grow(1900); // a row went away: scrollTop clamps from 1600 to 1500, and a scroll event says so
    fireEvent.scroll(list);
    pass(10);
    dims.grow(2200);
    show("p", "a new row");

    expect(list.scrollTop).toBe(1800);
  });

  it("guard: unmounting cancels the gesture's timer", () => {
    const pass = fakeTime();
    const { view, list, dims } = mount({ scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.wheel(list, { deltaY: -100 });
    expect(vi.getTimerCount()).toBe(1); // the gesture's own settle timer, and nothing else

    view.unmount();
    dims.grow(2300);

    expect(vi.getTimerCount()).toBe(0);
    pass(5000);
    expect(list.scrollTop).toBe(1600);
  });
});

describe("MessageList cursor highlight", () => {
  it("marks only the row at `cursor`, by position, with row-current and aria-current", () => {
    const { container } = render(
      <MessageList
        state={state({ userPrompts: texts("first", "second", "third") })}
        sessionEnded={false}
        expanded={{}}
        cursor={1}
        onAnswerPermission={vi.fn()}
      />,
    );
    const rows = Array.from(container.querySelectorAll(".row-prompt"));
    expect(rows).toHaveLength(3);
    expect(rows.map((r) => r.classList.contains("row-current"))).toEqual([false, true, false]);
    expect(rows.map((r) => r.getAttribute("aria-current"))).toEqual([null, "true", null]);
  });

  it("marks no row when cursor does not name a real index", () => {
    const { container } = render(
      <MessageList
        state={state({ userPrompts: texts("only one") })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={vi.fn()}
      />,
    );
    expect(container.querySelector(".row-current")).toBeNull();
  });
});

/* The restored-history notice lives at the head of the list rather than above
   it, because it describes the conversation below it. */
describe("the restored-history notice", () => {
  const notice = {
    source: "claude_transcript" as const,
    restoredItems: 2,
    omittedItems: 0,
    uptoSeq: 3,
    sourcePath: "/claude/projects/p/sess.jsonl",
    attemptedTranscriptPath: null,
    fallbackReason: null,
    writerVersion: "2.1.272",
  };

  function renderList(history: AgentUiState["history"]) {
    render(
      <MessageList
        state={state({ history, userPrompts: [{ seq: 1, text: "do the thing" }], transcript: texts("on it") })}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={() => {}}
      />,
    );
    return document.querySelector<HTMLElement>(".message-list")!;
  }

  it("is drawn whenever history was restored, at the top of the list", () => {
    const list = renderList(notice);
    expect(list.firstElementChild!.className).toBe("history-notice");
    expect(list.textContent).toContain("read from Claude's own transcript");
  });

  it("is absent for a fresh session, which is most of them", () => {
    expect(renderList(null).querySelector(".history-notice")).toBeNull();
  });

  /* The cursor is an index into `buildTimeline(state)`, and the notice is not in it. If the notice
     ever became a row, every index below it would be off by one and `Enter`/`y`/`a`/`d` would act
     on the wrong item. */
  it("leaves the row count -- and therefore every cursor index -- unchanged", () => {
    const withNotice = renderList(notice).querySelectorAll('[data-nav-stop="row"]').length;
    cleanup();
    const without = renderList(null).querySelectorAll('[data-nav-stop="row"]').length;
    expect(withNotice).toBe(without);
    expect(without).toBe(2);
  });
});

/** A `ResizeObserver` jsdom lacks: records what it observes, and `resize` delivers an entry to its
 *  callback the way the engine would after layout. */
type FakeObserver = { callback: ResizeObserverCallback; observed: Element[]; unobserved: Element[]; disconnected: boolean };
function stubResizeObserver(): FakeObserver[] {
  const observers: FakeObserver[] = [];
  class FakeResizeObserver {
    private record: FakeObserver;
    constructor(callback: ResizeObserverCallback) {
      this.record = { callback, observed: [], unobserved: [], disconnected: false };
      observers.push(this.record);
    }
    observe(el: Element) {
      this.record.observed.push(el);
    }
    unobserve(el: Element) {
      this.record.observed = this.record.observed.filter((o) => o !== el);
      this.record.unobserved.push(el);
    }
    disconnect() {
      this.record.disconnected = true;
    }
  }
  vi.stubGlobal("ResizeObserver", FakeResizeObserver);
  return observers;
}
const resize = (observer: FakeObserver, target: Element, width: number) =>
  observer.callback([{ target, contentRect: { width } } as unknown as ResizeObserverEntry], {} as ResizeObserver);

/* Make the observer once, watch the list and its rows, and do no extra work per streamed delta.
   Rows use no measured width; CSS size containers can make WebKitGTK reset scrollTop on clock ticks. */
describe("MessageList watches its own size without writing a width", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: -1, onAnswerPermission: () => {} };

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("observes the list and its rows once, writes no width, and adds nothing while a reply streams", () => {
    const observers = stubResizeObserver();
    const { container, rerender, unmount } = render(
      <MessageList state={state({ transcript: texts("the reply so far") })} {...props} />,
    );
    const list = container.querySelector<HTMLElement>(".message-list")!;
    expect(observers).toHaveLength(1);
    expect(observers[0].observed).toEqual([list, list.querySelector(".row")]);
    const setProperty = vi.spyOn(list.style, "setProperty");
    resize(observers[0], list, 537.5);
    expect(list.style.getPropertyValue("--list-inline-size")).toBe("");
    for (const text of ["the reply so far, and", "the reply so far, and more", "the reply so far, and more text"]) {
      rerender(<MessageList state={state({ transcript: texts(text) })} {...props} />);
    }
    expect(setProperty).not.toHaveBeenCalled();
    expect(observers).toHaveLength(1);
    expect(observers[0].observed).toHaveLength(2);
    unmount();
    expect(observers[0].disconnected).toBe(true);
  });
});

/* A row can mount with no new `state`: `Enter` on a collapsed run puts its calls in rows of their own, and
   `expanded` belongs to the caller. A row nobody watches changes size unseen -- `Enter` on the opened call's
   result then grew the list below a following view and nothing snapped it to the end. */
describe("MessageList watches rows that mount without a new state", () => {
  const props = { sessionEnded: false, cursor: -1, onAnswerPermission: () => {} };
  const call = (seq: number, id: string) => ({
    seq,
    toolUseId: id,
    name: "Read",
    input: {},
    result: { content: "ok", isError: false },
    turnId: "t1",
  });
  const conversation = state({
    userPrompts: [{ seq: 1, text: "read them" }],
    toolCalls: [call(2, "a"), call(3, "b"), call(4, "c")],
    transcript: [{ seq: 5, text: "done" }],
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("puts the rows of an opened run under the observer, and a row that left out of it", () => {
    const observers = stubResizeObserver();
    const { container, rerender } = render(<MessageList state={conversation} expanded={{}} {...props} />);
    const list = container.querySelector<HTMLElement>(".message-list")!;
    const rows = () => Array.from(list.querySelectorAll(".row"));
    const collapsed = rows();
    expect(collapsed).toHaveLength(3); // the prompt, the run of three reads, the reply
    expect(observers[0].observed).toEqual([list, ...collapsed]);

    rerender(<MessageList state={conversation} expanded={{ "r-2": true }} {...props} />);

    const opened = rows();
    expect(opened).toHaveLength(5); // the run's three calls took its place
    expect(new Set(observers[0].observed)).toEqual(new Set([list, ...opened]));
  });

  it("snaps a following view to the end when such a row's result is opened", () => {
    const observers = stubResizeObserver();
    const { container, rerender } = render(<MessageList state={conversation} expanded={{}} {...props} />);
    const list = container.querySelector<HTMLElement>(".message-list")!;
    const dims = { clientHeight: 400, scrollHeight: 2000, scrollTop: 1600 };
    for (const [name, value] of Object.entries(dims)) {
      Object.defineProperty(list, name, { configurable: true, writable: true, value });
    }
    fireEvent.scroll(list);
    rerender(<MessageList state={conversation} expanded={{ "r-2": true }} {...props} />);
    const third = list.querySelectorAll(".row")[3];
    expect(observers[0].observed).toContain(third);

    Object.defineProperty(list, "scrollHeight", { configurable: true, writable: true, value: 2200 }); // its result opened
    resize(observers[0], third, 500);

    expect(list.scrollTop).toBe(2200);
  });
});

/* Resizing without streaming can leave the view 482px short after a divider drag or 651px after
   unzooming (measured). The terminal or growing composer can shorten the list too; restyling rows
   alone can change content height without changing its box. Watch both list and rows and snap
   a following view to its end, even when no new state arrives. */
describe("MessageList keeps a following view at its end when the list or a row changes size", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: -1, onAnswerPermission: () => {} };

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  /** A list whose height and content both change on command, `scrollTop` clamped as a browser clamps it. */
  function resizableList(list: HTMLElement, start: { clientHeight: number; scrollHeight: number; scrollTop: number }) {
    let { clientHeight, scrollHeight, scrollTop } = start;
    const clamp = () => {
      scrollTop = Math.max(0, Math.min(scrollHeight - clientHeight, scrollTop));
    };
    Object.defineProperty(list, "clientHeight", { configurable: true, get: () => clientHeight });
    Object.defineProperty(list, "scrollHeight", { configurable: true, get: () => scrollHeight });
    Object.defineProperty(list, "scrollTop", {
      configurable: true,
      get: () => scrollTop,
      set: (v: number) => {
        scrollTop = v;
        clamp();
      },
    });
    return {
      /** Content reflowed or restyled; the view stays where it was, as it does in a browser. */
      content(to: number) {
        scrollHeight = to;
        clamp();
      },
      /** The list's own box got taller or shorter. */
      height(to: number) {
        clientHeight = to;
        clamp();
      },
    };
  }

  function mount() {
    const observers = stubResizeObserver();
    const view = render(<MessageList state={state({ transcript: texts("the reply, finished") })} {...props} />);
    const list = view.container.querySelector<HTMLElement>(".message-list")!;
    const dims = resizableList(list, { clientHeight: 400, scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    return { view, list, dims, observer: observers[0], row: list.querySelector(".row")! };
  }

  it("snaps to the end when the list narrows and its reply reflows taller, with nothing streaming", () => {
    const { list, dims, observer } = mount();
    dims.content(2482); // divider drag: 482px of reflow below the view

    resize(observer, list, 330);

    expect(list.scrollTop).toBe(2082);
  });

  it("snaps to the end when the list's own box gets shorter (the bottom terminal, a growing composer)", () => {
    const { list, dims, observer } = mount();
    dims.height(250);

    resize(observer, list, 536);

    expect(list.scrollTop).toBe(1750);
  });

  it("snaps to the end when a row changes size and the list's box does not (the prose measure)", () => {
    const { list, dims, observer, row } = mount();
    expect(observer.observed).toContain(row);
    dims.content(2600);

    resize(observer, row, 480);

    expect(list.scrollTop).toBe(2200);
  });

  it("guard: leaves a reader who scrolled up where they are", () => {
    const { list, dims, observer, row } = mount();
    fireEvent.wheel(list, { deltaY: -100 });
    list.scrollTop = 900;
    fireEvent.scroll(list);
    dims.content(2600);

    resize(observer, list, 330);
    resize(observer, row, 300);

    expect(list.scrollTop).toBe(900);
  });

  /* The follow effect's own reason for reading the position first holds here too: a scroll made with
     the pointer held on the list (its scrollbar) whose event has not been dispatched yet. Without that
     read, a resize in between would snap the drag away. */
  it("guard: honours a scrollbar drag whose scroll event has not arrived yet", () => {
    const { list, dims, observer } = mount();
    fireEvent.pointerDown(list);
    list.scrollTop = 900; // no scroll event yet
    dims.content(2600);

    resize(observer, list, 330);

    expect(list.scrollTop).toBe(900);
  });

  it("guard: leaves a pending provisional stop alone, and its gesture's end still catches up", () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const clock = { now: 1000 };
    vi.spyOn(performance, "now").mockImplementation(() => clock.now);
    const { list, dims, observer } = mount();
    fireEvent.wheel(list, { deltaY: -100 }); // its scroll may still be on the way
    dims.content(2300);

    resize(observer, list, 330);
    expect(list.scrollTop).toBe(1600);

    clock.now += 5000;
    act(() => {
      vi.advanceTimersByTime(5000);
    });
    expect(list.scrollTop).toBe(1900);
  });

  it("observes each row as it mounts, and lets go of one that leaves", () => {
    const { view, list, observer } = mount();
    const card = { seq: 2, permissionId: "p1", toolUseId: null, toolName: "Bash", input: { command: "ls" } };
    view.rerender(
      <MessageList state={state({ transcript: texts("the reply, finished"), pendingPermissions: [card] })} {...props} />,
    );
    const permissionRow = list.querySelector(".row-permission")!;
    expect(observer.observed).toContain(permissionRow);

    view.rerender(<MessageList state={state({ transcript: texts("the reply, finished") })} {...props} />);

    expect(observer.unobserved).toEqual([permissionRow]);
    expect(observer.observed).toEqual([list, list.querySelector(".row")]);
  });
});

/** Latest `(label, jump, afterSeq)` reported by `onUnreadChange`, as the status band holds it.
 *  `afterSeq` is the sequence threshold, not a position in the timeline. */
function lastUnread(mock: ReturnType<typeof vi.fn>): { label: string | null; jump: () => void; afterSeq: number | null } {
  const call = mock.mock.calls[mock.mock.calls.length - 1] as [string | null, () => void, number | null];
  return { label: call[0], jump: call[1], afterSeq: call[2] };
}

describe("MessageList: the jump-to-bottom pill (reported upward)", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: 0, onAnswerPermission: vi.fn() };

  it("counts rows that arrived while reading up, marks a card, and jumps to the end on a click", () => {
    const onUnreadChange = vi.fn();
    const { container, rerender } = render(
      <MessageList state={state({ transcript: texts("first") })} {...props} onUnreadChange={onUnreadChange} />,
    );
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    noteUserScroll(list, "up");
    list.scrollTop = 1000;
    fireEvent.scroll(list);
    // The band's compact form for "shown, nothing counted yet" -- the bare arrow, no "Jump to
    // bottom" (there is no room for words in a 24px band).
    expect(lastUnread(onUnreadChange).label).toBe("↓");
    dims.grow(2400);
    rerender(<MessageList state={state({ transcript: texts("first", "second", "third") })} {...props} onUnreadChange={onUnreadChange} />);
    expect(lastUnread(onUnreadChange).label).toBe("↓2");
    rerender(
      <MessageList
        state={state({
          transcript: texts("first", "second", "third"),
          pendingPermissions: [{ seq: 9, permissionId: "p", toolUseId: null, toolName: "Bash", input: {} }],
        })}
        {...props}
        onUnreadChange={onUnreadChange}
      />,
    );
    expect(lastUnread(onUnreadChange).label).toBe("↓ ⚑");
    lastUnread(onUnreadChange).jump();
    expect(list.scrollTop).toBe(2000);
    fireEvent.scroll(list);
    expect(lastUnread(onUnreadChange).label).toBeNull();
  });

  it("keeps approval after a row resizes -- the observer read the first render's timeline", () => {
    // The once-created observer needs the latest onScroll: a stale closure slices an old
    // timeline and replaces an approval label with an empty jump label when the card resizes.
    // jsdom has no observer, so deliver that resize through the fake.
    const observers = stubResizeObserver();
    const onUnreadChange = vi.fn();
    const { container, rerender } = render(
      <MessageList state={state({ transcript: texts("the reply") })} {...props} onUnreadChange={onUnreadChange} />,
    );
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    noteUserScroll(list, "up");
    list.scrollTop = 1000;
    fireEvent.scroll(list);
    dims.grow(2400);
    const call = { seq: 3, toolUseId: "toolu_x", name: "Bash", input: { command: "cargo build" }, result: null };
    rerender(
      <MessageList
        state={state({
          transcript: texts("the reply"),
          toolCalls: [call],
          pendingPermissions: [{ seq: 4, permissionId: "p", toolUseId: "toolu_x", toolName: "Bash", input: {} }],
        })}
        {...props}
        onUnreadChange={onUnreadChange}
      />,
    );
    expect(lastUnread(onUnreadChange).label).toBe("↓ ⚑");
    act(() => resize(observers[observers.length - 1], list.querySelector(".row-permission")!, 300));
    expect(lastUnread(onUnreadChange).label).toBe("↓ ⚑");
    vi.unstubAllGlobals();
  });

  it("is absent while following, however far a row grows", () => {
    const onUnreadChange = vi.fn();
    const { container, rerender } = render(
      <MessageList state={state({ transcript: texts("a") })} {...props} onUnreadChange={onUnreadChange} />,
    );
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    dims.grow(9000);
    rerender(<MessageList state={state({ transcript: texts("a, much longer") })} {...props} onUnreadChange={onUnreadChange} />);
    expect(onUnreadChange.mock.calls.every(([label]) => label === null)).toBe(true);
  });
});

/* An anchored permission can sit before older items in the display array. Count by arrival
   seq, not an array cutoff, so a newly arrived card is still unread wherever it is drawn. */
describe("MessageList: the unread threshold is a seq, not an index", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: 0, onAnswerPermission: vi.fn() };

  /** Parks the view (scrolled up and away from the bottom), the same way the unread-pill tests above do,
   *  so `unseenAfterSeqRef` in `MessageList` latches its threshold from `state`'s timeline. */
  function park(list: HTMLElement) {
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    noteUserScroll(list, "up");
    list.scrollTop = 1000;
    fireEvent.scroll(list);
    return dims;
  }

  it("counts a card anchored after an OLD tool call, even though it lands before the old length cutoff", () => {
    const onUnreadChange = vi.fn();
    // At park time: a tool call (seq 1) and a LATER message (seq 2) -- the max seq present is 2,
    // and the old, index-based threshold would have been `timeline.length` = 2.
    const { container, rerender } = render(
      <MessageList
        state={state({
          transcript: [{ seq: 2, text: "message B" }],
          toolCalls: [{ seq: 1, toolUseId: "toolu_a", name: "Bash", input: {}, result: null }],
        })}
        {...props}
        onUnreadChange={onUnreadChange}
      />,
    );
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = park(list);
    dims.grow(2400);
    // A permission request (seq 10 -- arrived long after the park) anchored to the OLD call
    // (`toolu_a`). `buildTimeline` splices it in right after that call, so it sits BEFORE `message B`
    // in the array even though its own `seq` is far newer: `timeline` is now
    // `[tool(1), permission(10), message(2)]`, length 3 -- one past the old index cutoff of 2, which
    // would slice off only `message(2)` and miss the card entirely.
    rerender(
      <MessageList
        state={state({
          transcript: [{ seq: 2, text: "message B" }],
          toolCalls: [{ seq: 1, toolUseId: "toolu_a", name: "Bash", input: {}, result: null }],
          pendingPermissions: [{ seq: 10, permissionId: "p1", toolUseId: "toolu_a", toolName: "Bash", input: {} }],
        })}
        {...props}
        onUnreadChange={onUnreadChange}
      />,
    );
    expect(lastUnread(onUnreadChange).label).toBe("↓ ⚑");
  });

  it("counts plain messages that arrive after the park, with no card below", () => {
    const onUnreadChange = vi.fn();
    const { container, rerender } = render(
      <MessageList state={state({ transcript: texts("first") })} {...props} onUnreadChange={onUnreadChange} />,
    );
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = park(list);
    dims.grow(2400);
    rerender(<MessageList state={state({ transcript: texts("first", "second", "third") })} {...props} onUnreadChange={onUnreadChange} />);
    expect(lastUnread(onUnreadChange).label).toBe("↓2");
  });

  it("seeds the threshold before the first updatePill on a restore, so a parked tab's own view is not lost", () => {
    const onUnreadChange = vi.fn();
    const { container, rerender } = render(
      <MessageList state={state({ transcript: texts("a", "b", "c") })} {...props} onUnreadChange={onUnreadChange} />,
    );
    const list = container.querySelector(".message-list") as HTMLElement;
    // `MessageList` is not remounted across a tab switch, so its unread threshold must track that switch,
    // so it carries whatever `followingRef` it already had. A tab switch's restore is only ever
    // seeded for a tab that was left PARKED (`App.tsx`'s `saveView` never saves a threshold for one
    // left `atBottom`) -- parking it here first is what stops the `[state]` effect's own `follow()`
    // from snapping the list back to the bottom before the seed effect below ever runs.
    setScroll(list, { scrollHeight: 900, clientHeight: 400, scrollTop: 500 });
    fireEvent.scroll(list);
    noteUserScroll(list, "up");
    // The restored scroll position: parked, far from the bottom -- the shape `App.tsx`'s own scroll
    // restore leaves the list in before `MessageList`'s seed effect runs (both are layout effects in
    // the same commit; jsdom does no layout, so nothing here resets what this sets).
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1000 });
    // Three items with `seq` > 2 (the seeded threshold) arrived while the tab was away.
    rerender(
      <MessageList
        state={state({
          transcript: [
            { seq: 0, text: "a" },
            { seq: 1, text: "b" },
            { seq: 2, text: "c" },
            { seq: 3, text: "d" },
            { seq: 4, text: "e" },
            { seq: 5, text: "f" },
          ],
        })}
        {...props}
        onUnreadChange={onUnreadChange}
        unseenSeed={{ afterSeq: 2, tick: 1 }}
      />,
    );
    expect(lastUnread(onUnreadChange).label).toBe("↓3");
    expect(lastUnread(onUnreadChange).afterSeq).toBe(2);
  });
});

/** A turn that did not complete says so where it stopped (`turnEnding.ts` words it). */
describe("turn ending rows", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: -1, onAnswerPermission: vi.fn() };
  const none = { reason: null, api_error_status: null, message: null };
  /** Prompt, a partial reply, the turn's end, then (optionally) a later prompt, folded by the real reducer. */
  const conversation = (end: AgentDomainEvent, later = false): AgentUiState => {
    const events: AgentDomainEvent[] = [
      { type: "turn_started", turn_id: "t1" },
      { type: "user_prompt_submitted", text: "first" },
      { type: "content_delta", turn_id: "t1", kind: "text", text: "partial reply" },
      end,
      ...(later ? ([{ type: "user_prompt_submitted", text: "second" }, { type: "turn_started", turn_id: "t2" }] as AgentDomainEvent[]) : []),
    ];
    return events.reduce(applyEvent, initialState());
  };
  const failed: AgentDomainEvent = {
    type: "turn_completed", turn_id: "t1", outcome: "failed", result_text: "", stop_reason: null, usage: null,
    detail: { reason: "api_error", api_error_status: 500, message: "boom\nsecond line" },
  };

  it("draws a muted `·` row where the turn ended, after the partial reply and before a later prompt", () => {
    const { container } = render(<MessageList state={conversation(failed, true)} {...props} />);
    const rows = Array.from(container.querySelectorAll(".row")).map((r) => r.className.split(" ").find((c) => c.startsWith("row-") && c !== "row-current"));
    expect(rows).toEqual(["row-prompt", "row-assistant", "row-ending", "row-prompt"]);
    const ending = container.querySelector<HTMLElement>(".row-ending")!;
    expect(ending.getAttribute("data-sign")).toBe("·");
    expect(ending.getAttribute("data-nav-stop")).toBe("row");
    expect(ending.querySelector(".row-body")!.textContent).toBe("the turn ended with an error (HTTP 500): boom\nsecond line");
  });

  it("a completed turn draws no such row, and an interrupted one says so in one word", () => {
    const completed: AgentDomainEvent = { ...failed, outcome: "completed", detail: none } as AgentDomainEvent;
    expect(render(<MessageList state={conversation(completed)} {...props} />).container.querySelector(".row-ending")).toBeNull();
    cleanup();
    const interrupted: AgentDomainEvent = { ...failed, outcome: "interrupted", detail: none } as AgentDomainEvent;
    expect(render(<MessageList state={conversation(interrupted)} {...props} />).container.querySelector(".row-ending .row-body")!.textContent).toBe("interrupted");
  });

  it("a session that ends under a running turn leaves the lost row", () => {
    const events: AgentDomainEvent[] = [
      { type: "turn_started", turn_id: "t1" },
      { type: "content_delta", turn_id: "t1", kind: "text", text: "partial" },
      { type: "session_unavailable", reason: "stream lost" },
    ];
    const { container } = render(<MessageList state={events.reduce(applyEvent, initialState())} {...props} sessionEnded />);
    expect(container.querySelector(".row-ending .row-body")!.textContent).toBe("the turn did not finish: the session ended");
  });
});

describe("notice rows: a prompt a hook stopped, and a turn this panel could not show", () => {
  const notice = (seq: number, kind: NoticeRecord["kind"], level: NoticeRecord["level"], content: string | null): NoticeRecord => ({
    seq,
    kind,
    level,
    content,
    scalars: {},
    taskId: null,
    toolUseId: null,
    taskStatus: null,
  });
  const hookWords = "Blocked by the deploy guard:\nproduction deploys need a ticket\u202e.";
  const withNotices = state({
    userPrompts: [{ seq: 0, text: "deploy to production" }],
    transcript: [{ seq: 3, text: "The tests passed in the background." }],
    notices: [
      notice(1, "hook_block", "warning", hookWords),
      notice(2, "unseen_turn", "notice", null),
      // A kind not yet drawn as a row stays out of the conversation.
      notice(4, "informational", "info", "not a row yet"),
    ],
  });

  it("draws both kinds in order, muted, each a j/k stop, the hook's words as plain text with hidden characters shown", () => {
    const { container } = render(
      <MessageList state={withNotices} sessionEnded={false} expanded={{}} cursor={-1} onAnswerPermission={vi.fn()} />,
    );
    const kinds = Array.from(container.querySelectorAll(".row")).map((row) =>
      Array.from(row.classList).find((name) => name.startsWith("row-") && name !== "row-current"),
    );
    expect(kinds).toEqual(["row-prompt", "row-notice", "row-notice", "row-assistant"]);
    const notices = Array.from(container.querySelectorAll<HTMLElement>(".row-notice"));
    expect(notices.map((row) => row.dataset.sign)).toEqual(["⚠", "·"]);
    // The sign alone carries the warning glyph: the row reads it once.
    expect(notices[0].textContent!.split("⚠").length - 1).toBe(1);
    expect(notices[0].querySelector(".notice-text")!.textContent).toBe(
      "UserPromptSubmit operation blocked by hook: Blocked by the deploy guard:\nproduction deploys need a ticket⟨U+202E⟩.",
    );
    expect(notices[0].querySelector(".permission-card-escape")).not.toBeNull();
    expect(notices[0].querySelector("a, strong, code")).toBeNull();
    expect(notices[1].textContent).toContain(UNSEEN_TURN_TEXT);
    expect(container.textContent).not.toContain("not a row yet");
  });

  it("y copies what each says, whole", () => {
    const rows = buildTimeline(withNotices).filter((item) => item.kind === "notice");
    expect(rows.map((item) => item.key)).toEqual(["n-1", "n-2"]);
    expect(rows.map(primaryText)).toEqual([`UserPromptSubmit operation blocked by hook: ${hookWords}`, UNSEEN_TURN_TEXT]);
  });
});

describe("wake rows and expandable lifecycle notices", () => {
  const cause = (over: Partial<import("../types").TurnCause>): import("../types").TurnCause => ({
    kind: "unknown", taskId: null, taskKind: null, taskStatus: null, description: null, summary: null,
    peerName: null, text: null, wakeupSource: null, scheduledForMs: null, scheduleReason: null, cronId: null, prompt: null, ...over,
  });
  const notices: NoticeRecord[] = [
    { seq: 3, kind: "compaction", level: "notice", content: "**summary** <b>plain</b>\u202e", scalars: { pre_tokens: 182000, post_tokens: 41000, trigger: "auto" }, taskId: null, toolUseId: null, taskStatus: null },
    { seq: 4, kind: "model_fallback", level: "warning", content: "**fallback** <b>plain</b>", scalars: {}, taskId: null, toolUseId: null, taskStatus: null },
    { seq: 5, kind: "compaction", level: "notice", content: null, scalars: {}, taskId: null, toolUseId: null, taskStatus: null },
  ];
  const conversation = state({
    turnStarts: [
      { seq: 1, turnId: "peer", origin: "cli", cause: cause({ kind: "peer", peerName: "worker", text: "**ready**\n<b>body</b>\u202e" }) },
      { seq: 2, turnId: "monitor", origin: "cli", cause: cause({ kind: "task", taskKind: "monitor", taskStatus: "running", description: "watch", summary: "event one\nevent two" }) },
    ], notices,
  });
  const props = { expanded: {}, cursor: 0, sessionEnded: false, onAnswerPermission: vi.fn() };

  it("makes each wake and notice a cursor stop, with full text available to y", () => {
    const { container, rerender } = render(<MessageList state={conversation} {...props} />);
    const rows = container.querySelectorAll('[data-nav-stop="row"]');
    expect(rows).toHaveLength(5);
    const timeline = buildTimeline(conversation);
    // j/k change the cursor index supplied by the panel's keyboard controller.
    for (const cursor of [0, 1, 2, 3, 4, 3, 2, 1, 0]) {
      rerender(<MessageList state={conversation} {...props} cursor={cursor} />);
      expect(container.querySelector(".row-current")).toBe(rows[cursor]);
      expect(primaryText(timeline[cursor])).toBeTruthy();
    }
    expect(primaryText(timeline[0])).toBe("› Message from @worker: **ready**\n**ready**\n<b>body</b>\u202e");
    expect(container.querySelector(".row-wake")!.getAttribute("data-sign")).toBe("●");
    expect(container.querySelector(".notice-prominent")!.textContent).toContain("**fallback** <b>plain</b>");
  });

  it("puts a wake row's fold marker at the end of its words, not on a line of its own", () => {
    const { container } = render(<MessageList state={conversation} {...props} />);
    const marker = container.querySelector(".row-wake .fold-marker")!;
    expect(marker.parentElement!.className).toBe("wake-head");
    expect(marker.previousElementSibling!.className).toBe("wake-text");
    expect(marker.previousElementSibling!.tagName).toBe("SPAN");
  });

  it("opens peer bodies and supplied summaries with the row's Enter fold, always as text", () => {
    const { container, rerender } = render(<MessageList state={conversation} {...props} />);
    expect(container.textContent).not.toContain("<b>body</b>");
    expect(container.textContent).not.toContain("**summary**");
    // Monitor output is an event, visible without opening a detail fold.
    expect(container.textContent).toContain('Monitor event: "watch"');
    expect(container.textContent).toContain("event one\nevent two");
    rerender(<MessageList state={conversation} {...props} expanded={{ "w-1": true, "n-3": true, "n-5": true }} />);
    expect(container.textContent).toContain("<b>body</b>⟨U+202E⟩");
    expect(container.textContent).toContain("**summary** <b>plain</b>⟨U+202E⟩");
    expect(container.querySelector("b, strong, a")).toBeNull();
    const compact = container.querySelectorAll(".notice-compaction");
    expect(compact[0].textContent).toContain("──── Conversation compacted · 182k → 41k tokens (auto) ────");
    expect(compact[1].querySelector(".fold-marker")).toBeNull();
    expect(compact[1].textContent).toContain("──── Conversation compacted ────");
  });
});

it("updates a memoized background card when only its roster status changes", () => {
  let s = initialState();
  for (const event of cardRosterFixture.domain_events) s = applyEvent(s, event as AgentDomainEvent);
  const task = s.backgroundTasks.rows.find((r) => r.tool_use_id === "toolu_lim")!;
  s.backgroundTasks = { query_start: false, rows: [{ ...task, status: "running", live: true }] };
  s.notices = [];
  const props = { sessionEnded: false, expanded: {}, cursor: -1, onAnswerPermission: () => {} };
  const { container, rerender } = render(<MessageList {...props} state={s} />);
  const card = () => container.querySelector('[data-tool-name="Bash"]')!;
  expect(card().textContent).toContain("Running in the background · /tasks");
  const ended = { ...s, backgroundTasks: { ...s.backgroundTasks, rows: [{ ...task, live: false, status: "completed" as const, summary: "completed", started_at: 1000, ended_at: 131000, usage: null }] } };
  rerender(<MessageList {...props} state={ended} />);
  expect(card().textContent).toContain("completed in background · 2m 10s");
  expect(card().textContent).not.toContain("Running in the background");
});

// Child frames and question settlements remain view data; rendering never changes their fold.
describe("subagent groups", () => {
  const child = (seq: number, name = "Read"): import("../types").ToolCallRecord => ({
    seq, toolUseId: `child-${seq}`, name, input: { file_path: "a.rs" }, result: null, turnId: "main",
  });
  const stream = (toolCalls: import("../types").ToolCallRecord[]) => ({
    toolCalls, transcript: [], thinking: [], assistantMessageOpen: false, thinkingOpen: false,
  });
  const parent = (done = false): import("../types").ToolCallRecord => ({
    seq: 1, toolUseId: "parent", name: "Agent", turnId: "main",
    input: { subagent_type: "Explore", description: "find\u202ethe\u200bbug" },
    children: stream([child(2), child(3, "Bash")]),
    result: done ? { content: [{ type: "text", text: "This agent switched to small because its earlier model could not be used.\nReport <img src=x>\n<usage>subagent_tokens: 18000\ntool_uses: 7\nduration_ms: 34000</usage>" }], isError: false } : null,
  });
  const draw = (call = parent(), expanded = {}, detailed = false) => render(<MessageList
    state={state({ activeTurnId: "main", toolCalls: [call] })} expanded={expanded} detailed={detailed}
    sessionEnded={false} cursor={0} onAnswerPermission={vi.fn()} />);

  it("folds the delegation by default and shows only the latest child call", () => {
    const { container } = draw();
    expect(container.querySelectorAll('[data-nav-stop="row"]')).toHaveLength(1);
    expect(container.querySelector(".subagent-heading")?.textContent).toContain("Explore(find⟨U+202E⟩the⟨U+200B⟩bug)");
    expect(container.querySelector(".subagent-status")?.textContent).toBe("⎿ Bash(a.rs) · 2 tool uses");
    expect(container.querySelector('.fold-marker')?.getAttribute("aria-label")).toBe("collapsed");
  });

  it("shows Done statistics and a separate warning before the folded report", () => {
    const call = parent(true);
    const { container, rerender } = draw(call);
    expect(container.querySelector(".subagent-status")?.textContent).toBe("⎿ Done (7 tool uses · 18k tokens · 0m 34s)");
    expect(container.querySelector(".subagent-warning")?.textContent).toContain("This agent switched to small");
    expect(container.querySelector(".subagent-report")).toBeNull();
    rerender(<MessageList state={state({ toolCalls: [call] })} expanded={{ "t-1": true }} sessionEnded={false} cursor={0} onAnswerPermission={vi.fn()} />);
    expect(container.querySelector(".subagent-report")?.textContent).toBe("Report <img src=x>");
    expect(container.querySelector(".subagent-report img")).toBeNull();
  });

  it("indents nested children again, keeping every child an ordinary navigation stop", () => {
    const nested = { ...child(2, "Agent"), children: stream([child(3)]) };
    const call = { ...parent(), children: stream([nested]) };
    const { container } = draw(call, { "t-1": true, "t-2": true });
    expect(container.querySelectorAll('[data-nav-stop="row"]')).toHaveLength(3);
    expect(container.querySelector('[data-depth="1"]')?.getAttribute("style")).toContain("1.5em");
    expect(container.querySelector('[data-depth="2"]')?.getAttribute("style")).toContain("3em");
    expect(container.querySelector(".row-tool-run")).toBeNull();
  });

  it("reads a background launch still in flight with the live line, never as background before the roster lists it", () => {
    const launching = { ...parent(), input: { subagent_type: "Explore", description: "find", run_in_background: true }, children: undefined };
    const live = draw(launching);
    expect(live.container.querySelector(".subagent-status")?.textContent).toBe("⎿ Running · 0 tool uses");
    live.unmount();
    const ended = render(<MessageList state={state({ activeTurnId: null, toolCalls: [launching] })} expanded={{}} detailed={false}
      sessionEnded={false} cursor={0} onAnswerPermission={vi.fn()} />);
    expect(ended.container.querySelector(".subagent-status")?.textContent).toBe("⎿ stopped with the session");
  });

  it("moves only the head of the report into the warning line", () => {
    const call = parent(true);
    call.result = { content: [{ type: "text", text: "Found it.\nThe maximum number of turns was never the cause.\nThe model fallback path is fine.\n<usage>tool_uses: 7</usage>" }], isError: false };
    const { container } = draw(call, { "t-1": true });
    expect(container.querySelector(".subagent-warning")?.textContent).toContain("maximum number of turns");
    expect(container.querySelector(".subagent-report")?.textContent).toBe("Found it.\nThe model fallback path is fine.");
  });

  it("never expands background children, including in the detailed view", () => {
    const { container } = draw({ ...parent(), background: true }, { "t-1": true }, true);
    expect(container.querySelectorAll('[data-nav-stop="row"]')).toHaveLength(1);
    expect(container.querySelector(".fold-marker")).toBeNull();
    expect(container.querySelector(".subagent-report")).toBeNull();
  });
});

describe("question transcript rows", () => {
  const question = (outcome?: import("../types").QuestionOutcome): import("../types").QuestionRecord => ({
    seq: 3, interactionId: "q", toolUseId: "ask", turnId: "main", unsupportedReason: null,
    questions: [{ header: "Target\u202e", question: "Which\u200b? <img src=x>", multiSelect: false, options: [] }],
    state: outcome ? { kind: "settled", outcome, answers: [{ questionIndex: 0, answer: "local\u200b", notes: "safe\u202e" }], reason: null } : { kind: "waiting" },
  });
  it.each([
    [undefined, "waiting for your answer"], ["answered", "→ local⟨U+200B⟩"],
    ["declined", "User declined to answer questions"], ["chat_requested", "→ chat about this"],
    ["interrupted", "(not answered: interrupted)"], ["expired", "(not answered: Claude Code withdrew the question)"],
    ["session_closed", "(not answered: session closed)"], ["provider_failed", "(not answered: provider failed)"],
    ["host_refused", "(not answered: host refused)"],
  ] as const)("renders the %s outcome with revealed text at the linked call", (outcome, status) => {
    const s = state({ questions: [question(outcome)], transcript: [{ seq: 2, text: "after the call" }],
      toolCalls: [{ seq: 1, name: "AskUserQuestion", toolUseId: "ask", input: {}, result: null }] });
    const { container } = render(<MessageList state={s} expanded={{}} cursor={0} sessionEnded={false} onAnswerPermission={vi.fn()} />);
    const row = container.querySelector(".row-question")!;
    expect(container.querySelectorAll('[data-nav-stop="row"]')[0]).toBe(row);
    expect(row.textContent).toContain("Target⟨U+202E⟩: Which⟨U+200B⟩? <img src=x>");
    expect(row.textContent).toContain(status);
    if (outcome) expect(row.textContent).toContain("→ notes: safe⟨U+202E⟩");
    expect(row.querySelector("img")).toBeNull();
    expect(container.querySelector(".row-tool")).toBeNull();
    expect(primaryText(buildTimeline(s)[0])).toContain("Which\u200b?");
  });

  it("keeps an old call's result as plain revealed text when there is no question record", () => {
    const { container } = render(<MessageList state={state({ toolCalls: [{
      seq: 1, name: "AskUserQuestion", toolUseId: "ask", input: {},
      result: { content: [{ type: "text", text: "refused\u202e\u200b <b>unsafe</b>" }], isError: true },
    }] })} expanded={{}} cursor={0} sessionEnded={false} onAnswerPermission={vi.fn()} />);
    expect(container.querySelector(".row-question")?.textContent).toContain("?Claude asked a questionrefused⟨U+202E⟩⟨U+200B⟩ <b>unsafe</b>");
    expect(container.querySelector(".row-question b")).toBeNull();
    expect(container.querySelector(".row-tool")).toBeNull();
  });
});


describe("structured transcript fixtures", () => {
  it.each(subagentFixtures.cases)("renders $name with children contained by their parent", (testCase) => {
    const s = testCase.events.reduce((s, event) => applyEvent(s, event as AgentDomainEvent), initialState());
    const base = buildTimeline(s);
    const childIds = s.toolCalls.flatMap((call) => call.children?.toolCalls.map((child) => child.toolUseId) ?? []);
    expect(base.filter((item) => item.kind === "tool" && childIds.includes(item.call.toolUseId))).toEqual([]);
    const folded = render(<MessageList state={s} expanded={{}} cursor={0} sessionEnded={false} onAnswerPermission={vi.fn()} />);
    expect(folded.container.querySelectorAll(".row-subagent")).toHaveLength(s.toolCalls.length);
    expect(folded.container.querySelector(".subagent-child")).toBeNull();
    folded.unmount();
    const expanded = Object.fromEntries(Array.from({ length: s.nextSeq }, (_, seq) => [`t-${seq}`, true]));
    const display = buildDisplay(base, { expanded, detailed: false, turnRunning: !!s.activeTurnId });
    const shown = render(<MessageList state={s} expanded={expanded} cursor={0} sessionEnded={false} onAnswerPermission={vi.fn()} />);
    expect(shown.container.querySelectorAll('[data-nav-stop="row"]')).toHaveLength(display.length);
    if (testCase.name.startsWith("background") || testCase.name === "unknown parent") {
      expect(shown.container.querySelector(".subagent-child")).toBeNull();
      expect(shown.container.querySelector(".subagent-status")?.textContent).not.toContain("Running in the background");
    }
  });

  it.each(questionFixtures.cases)("renders every recorded state of $name", (testCase) => {
    for (const step of testCase.steps) {
      const s = { ...initialState(), ...step.snapshot } as AgentUiState;
      const view = render(<MessageList state={s} expanded={{}} cursor={0} sessionEnded={false} onAnswerPermission={vi.fn()} />);
      expect(view.container.querySelectorAll(".row-question")).toHaveLength(s.questions.length);
      expect(view.container.querySelector(".row-tool")).toBeNull();
      const waiting = s.questions.filter((q) => q.state.kind !== "settled").length;
      expect(view.container.textContent?.match(/waiting for your answer/g)?.length ?? 0).toBe(waiting);
      view.unmount();
    }
  });
});


describe("subagent roster rendering", () => {
  it.each([["foreground", foregroundAgentFixture], ["background", backgroundAgentFixture]] as const)("uses live %s task statistics without confusing foreground and background", (kind, fixture) => {
    let s = initialState();
    for (const event of fixture.domain_events) {
      s = applyEvent(s, event as AgentDomainEvent);
      if (event.type !== "task_event" || event.edge !== "progress") continue;
      const view = render(<MessageList state={s} expanded={{}} cursor={0} sessionEnded={false} onAnswerPermission={vi.fn()} />);
      const row = view.container.querySelector(".row-subagent")!;
      if (kind === "background") {
        expect(row.querySelector(".subagent-status")?.textContent).toContain(`Running in the background · ${event.usage?.tool_uses} tool uses · /tasks`);
        expect(row.querySelector(".fold-marker")).toBeNull();
      } else {
        expect(row.querySelector(".subagent-status")?.textContent).toContain(`${event.usage?.tool_uses} tool uses · 0m 00s`);
        expect(row.querySelector(".subagent-status")?.textContent).not.toContain("background");
        expect(row.querySelector(".fold-marker")).not.toBeNull();
      }
      view.unmount();
    }
  });
});


it("uses a completed agent's final figures instead of its earlier progress figures", () => {
  let s = initialState();
  for (const event of foregroundAgentFixture.domain_events) {
    s = applyEvent(s, event as AgentDomainEvent);
    if (event.type === "task_event" && event.edge === "progress") break;
  }
  const parent = s.toolCalls.find((call) => call.name === "Agent")!;
  const completed = { ...parent, result: { content: "report\n<usage>tool_uses: 7\nsubagent_tokens: 18000\nduration_ms: 34000</usage>", isError: false } };
  expect(agentCard(completed, s).line).toBe("⎿ Done (7 tool uses · 18k tokens · 0m 34s)");
});

/* A foreground agent whose result can never arrive -- its turn was interrupted, the session ended, or
   history restored the call without its result -- must not keep its live "Running" line: the row
   reads as over, like any other call that ended without a result. */
describe("a foreground agent that ended without a result", () => {
  const agentStarted: AgentDomainEvent[] = [
    { type: "session_opened", session_id: "s1", provider_session_id: "c1", model: "m", cwd: "/p" },
    { type: "user_prompt_submitted", text: "look around" },
    { type: "turn_started", turn_id: "t1" },
    { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_a", name: "Agent",
      input: { subagent_type: "Explore", description: "survey the tree", prompt: "look" } },
  ];
  const shown = (s: AgentUiState, sessionEnded: boolean) => {
    const view = render(<MessageList state={s} sessionEnded={sessionEnded} expanded={{}} cursor={-1} onAnswerPermission={vi.fn()} />);
    const row = view.container.querySelector(".row-subagent")!;
    const seen = { line: row.querySelector(".subagent-status")!.textContent, sign: row.getAttribute("data-sign") };
    view.unmount();
    return seen;
  };
  const interrupted = () => applyEvent(agentStarted.reduce(applyEvent, initialState()), {
    type: "turn_completed", turn_id: "t1", outcome: "interrupted", result_text: "", stop_reason: null, usage: null,
    detail: { reason: null, api_error_status: null, message: null },
  });

  it("keeps the live line while its turn runs", () => {
    expect(shown(agentStarted.reduce(applyEvent, initialState()), false)).toEqual({ line: "⎿ Running · 0 tool uses", sign: "◐" });
  });

  it("says no result was recorded once its turn was interrupted", () => {
    expect(shown(interrupted(), false)).toEqual({ line: "⎿ no result recorded (0 tool uses)", sign: "·" });
  });

  it("says no result was recorded once the session has ended mid-call", () => {
    expect(shown(agentStarted.reduce(applyEvent, initialState()), true)).toEqual({ line: "⎿ no result recorded (0 tool uses)", sign: "·" });
  });

  it("says no result was recorded for a call history restored without its result", () => {
    const restored = state({
      history: { source: "claude_transcript", restoredItems: 3, omittedItems: 0, uptoSeq: 3, sourcePath: "/claude/projects/p/sess.jsonl",
        attemptedTranscriptPath: null, fallbackReason: null, writerVersion: "2.1.311" },
      toolCalls: [{ seq: 2, toolUseId: "toolu_a", name: "Agent", input: { subagent_type: "Explore", description: "survey the tree" }, result: null }],
    });
    expect(shown(restored, false)).toEqual({ line: "⎿ no result recorded (0 tool uses)", sign: "·" });
  });
});

describe("rows are children of one list", () => {
  const read = (seq: number) => ({
    seq, toolUseId: `use-${seq}`, name: "Read", input: { file_path: `/p/f${seq}.rs` },
    result: { content: "text", isError: false }, turnId: "t1",
  });
  const card = (seq: number) => ({ seq, permissionId: `perm-${seq}`, toolUseId: "unlinked", toolName: "Bash", input: { command: "ls" } });
  const list = (state: AgentUiState, expanded: Record<string, boolean>) => (
    <MessageList state={state} sessionEnded={false} expanded={expanded} cursor={0} onAnswerPermission={() => {}} />
  );

  it("a fold above a card leaves the card mounted, with what was typed into it", () => {
    const state = { ...initialState(), status: { kind: "running" }, activeTurnId: "t1", toolCalls: [read(63), read(64)], pendingPermissions: [card(65)] } as AgentUiState;
    const { container, rerender } = render(list(state, {}));
    expect(container.querySelectorAll(".row-tool-run")).toHaveLength(1);
    const reason = container.querySelector<HTMLInputElement>(".permission-card input")!;
    fireEvent.change(reason, { target: { value: "not that one" } });
    rerender(list(state, { "r-63": true }));
    expect(container.querySelectorAll(".row-tool-run")).toHaveLength(0);
    expect(container.querySelector(".permission-card input"), "the same element").toBe(reason);
    expect(reason.value).toBe("not that one");
  });

  describe("a card keeps what was typed into it, whatever else arrives", () => {
    const anchored = (seq: number, call: number) => ({ ...card(seq), toolUseId: `use-${call}` });
    const running = (over: Partial<AgentUiState>) => ({ ...initialState(), status: { kind: "running" }, activeTurnId: "t1", ...over }) as AgentUiState;
    /** Mounts `first`, types into its last card (the one at `_seq`), shows `then`, and checks the box is the same one, still holding it. */
    const typedThrough = (first: AgentUiState, _seq: number, then: AgentUiState) => {
      const view = (state: AgentUiState) => (
        <MessageList state={state} sessionEnded={false} expanded={{}} cursor={0} onAnswerPermission={() => {}} />
      );
      const { container, rerender } = render(view(first));
      const reason = Array.from(container.querySelectorAll<HTMLInputElement>(".permission-card input")).at(-1)!;
      fireEvent.change(reason, { target: { value: "not that one" } });
      rerender(view(then));
      expect(reason.isConnected, "the same box").toBe(true);
      expect(reason.value).toBe("not that one");
      return container;
    };

    it("a late card anchored to an older call", () => {
      const toolCalls = [read(1), read(65)];
      const container = typedThrough(
        running({ toolCalls, pendingPermissions: [anchored(66, 65)] }), 66,
        running({ toolCalls, pendingPermissions: [anchored(66, 65), anchored(130, 1)] }),
      );
      expect(container.querySelectorAll(".permission-card")).toHaveLength(2);
    });

    it("the call of an unlinked card arriving", () => {
      typedThrough(
        running({ toolCalls: [read(1)], pendingPermissions: [{ ...card(63), toolUseId: "use-64" }] }), 63,
        running({ toolCalls: [read(1), read(64)], pendingPermissions: [{ ...card(63), toolUseId: "use-64" }] }),
      );
    });

    it("a row arriving, and another card leaving, around it", () => {
      const toolCalls = [read(1), read(65)];
      const transcript = [{ seq: 70, text: "more" }];
      typedThrough(
        running({ toolCalls, pendingPermissions: [anchored(130, 1), anchored(66, 65)] }), 66,
        running({ toolCalls, transcript, pendingPermissions: [anchored(66, 65)] }),
      );
    });

    it("a resync: every record new, the same conversation", () => {
      const state = running({ toolCalls: [read(1), read(65)], transcript: [{ seq: 64, text: "reply" }], pendingPermissions: [anchored(66, 65)] });
      typedThrough(state, 66, JSON.parse(JSON.stringify(state)) as AgentUiState);
    });

    it("a snapshot putting rows ahead of it", () => {
      typedThrough(
        running({ toolCalls: [read(70)], pendingPermissions: [anchored(71, 70)] }), 71,
        running({ toolCalls: [read(1), read(70)], transcript: [{ seq: 2, text: "restored" }], pendingPermissions: [anchored(71, 70)] }),
      );
    });

    it("a broker card, which Rust numbers, renumbered", () => {
      const broker = (seq: number) => ({ seq, permissionId: "broker", toolUseId: null, toolName: "open_file", input: null, editorCard: { title: "Open", body: [] } });
      const view = (seq: number) => (
        <MessageList state={running({ toolCalls: [read(1)], editorCards: [broker(seq)] })} sessionEnded={false} expanded={{}} cursor={0} onAnswerPermission={() => {}} />
      );
      const { container, rerender } = render(view(63));
      const shown = container.querySelector(".permission-card");
      expect(shown).not.toBeNull();
      rerender(view(200));
      expect(container.querySelector(".permission-card")).toBe(shown);
    });
  });

  it("a late card leaves every row after it the element it was, in order", () => {
    const state = (pendingPermissions: AgentUiState["pendingPermissions"]) => ({
      ...initialState(), status: { kind: "running" }, activeTurnId: "t1",
      toolCalls: [{ ...read(1), result: { content: "text", isError: true } }, { ...read(3), result: { content: "text", isError: true } }],
      transcript: [{ seq: 2, text: "between" }, { seq: 4, text: "after" }], pendingPermissions,
    }) as AgentUiState;
    const error = vi.spyOn(console, "error");
    const { container, rerender } = render(list(state([]), {}));
    const rows = () => Array.from(container.querySelector(".message-list")!.children).filter((el) => el.classList.contains("row"));
    const before = rows();
    // By identity: `toEqual` compares DOM nodes by their markup, which a replacement with the same text passes.
    const sameElements = (now: Element[]) => {
      expect(now).toHaveLength(before.length);
      now.forEach((el, i) => expect(el, `row ${i}`).toBe(before[i]));
    };
    rerender(list(state([{ ...card(130), toolUseId: "use-1" }]), {}));
    expect(rows().map((el) => el.className.split(" ")[1])).toEqual(["row-tool", "row-permission", "row-assistant", "row-tool", "row-assistant"]);
    sameElements(rows().filter((el) => !el.classList.contains("row-permission")));
    rerender(list(state([]), {}));
    sameElements(rows());
    expect(error, "no duplicate key").not.toHaveBeenCalled();
    error.mockRestore();
  });

  it("keeps every row a direct child of the list, in the timeline's order, and keys unique", () => {
    const error = vi.spyOn(console, "error");
    const toolCalls = Array.from({ length: 200 }, (_, i) => ({ ...read(i * 3 + 1), result: { content: "text", isError: true } }));
    const transcript = Array.from({ length: 200 }, (_, i) => ({ seq: i * 3 + 2, text: `reply ${i}` }));
    const state = { ...initialState(), status: { kind: "running" }, toolCalls, transcript, pendingPermissions: [card(1000)] } as AgentUiState;
    const { container } = render(list(state, {}));
    const rows = Array.from(container.querySelector(".message-list")!.children).filter((el) => el.classList.contains("row"));
    expect(rows).toHaveLength(401);
    expect(rows[1].textContent).toContain("reply 0");
    expect(rows[400].querySelector(".permission-card")).not.toBeNull();
    expect(error).not.toHaveBeenCalled();
    error.mockRestore();
  });
});

for (const [test, name] of [["user_row_names_the_phone", "Pixel 8"], ["phone_name_shows_hidden_characters", "Pixel\u202e8"]]) {
  it(test, () => {
    const view = render(<MessageList state={state({userPrompts: [{seq: 1, text: "hello"}], sentFrom: {"1": {deviceId: "d1", name}}})} sessionEnded={false} expanded={{}} cursor={-1} onAnswerPermission={() => {}} />);
    expect(view.container.textContent).toContain(name === "Pixel 8" ? "from Pixel 8" : "U+202E");
  });
}
it("footer_names_the_phone_that_stopped", () => {
 const view = render(<MessageList state={state({turnEndings: [{seq: 2, turnId: "t1", kind: "interrupted", reason: null, apiErrorStatus: null, message: null}], stoppedFrom: {t1: {deviceId: "d1", name: "Pixel 8"}}})} sessionEnded={false} expanded={{}} cursor={-1} onAnswerPermission={() => {}} />);
 expect(view.container.textContent).toContain("stopped from Pixel 8");
});

/* A zoom or a divider drag changes the list's width and every row reflows; `scrollTop` kept in pixels put a reader
   deep in a long conversation pages away from where they were reading (measured in the sandbox, 2026-11-09). The
   reader's row is put back at its height in the view, following is not re-armed by a restore the engine clamped at
   the end, and zooming back returns to the same `scrollTop`. Fake layout: each reply's height is its characters over
   the width; the list's top is the viewport's top. */
describe("MessageList keeps a parked reader's place across a width change", () => {
  const replies = Array.from({ length: 12 }, (_, i) => `reply ${i} ` + "x".repeat(i % 2 === 0 ? 2400 : 40));
  const conversation = state({ transcript: texts(...replies) });
  let now = 10_000;

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  function mount(cursor: number) {
    const observers = stubResizeObserver();
    vi.spyOn(performance, "now").mockImplementation(() => now);
    const view = render(<MessageList state={conversation} sessionEnded={false} expanded={{}} cursor={cursor} onAnswerPermission={() => {}} />);
    const list = view.container.querySelector<HTMLElement>(".message-list")!;
    const rows = Array.from(list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]'));
    // Live: a commit can mount rows after this.
    const live = () => Array.from(list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]'));
    const geo = { width: 400, scrollTop: 0, height: 400 };
    const heights = () => live().map((row) => Math.ceil((row.textContent ?? "").length / (geo.width / 8)) * 20 + 16);
    const top = (i: number) => heights().slice(0, i).reduce((a, b) => a + b, 0);
    const total = () => heights().reduce((a, b) => a + b, 0);
    Object.defineProperty(list, "clientWidth", { configurable: true, get: () => geo.width });
    Object.defineProperty(list, "clientHeight", { configurable: true, get: () => geo.height });
    Object.defineProperty(list, "scrollHeight", { configurable: true, get: () => total() });
    // The engine clamps the position whenever layout shortens the list, and the clamped value is what stays.
    const clamp = () => (geo.scrollTop = Math.max(0, Math.min(geo.scrollTop, total() - geo.height)));
    Object.defineProperty(list, "scrollTop", {
      configurable: true,
      get: () => clamp(),
      set: (v: number) => {
        geo.scrollTop = v;
        clamp();
      },
    });
    const original = Element.prototype.getBoundingClientRect;
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
      if (this === list) return { top: 0, bottom: geo.height, left: 0, height: geo.height } as DOMRect;
      const i = live().indexOf(this as HTMLElement);
      if (i === -1) return original.call(this);
      const t = top(i) - clamp();
      return { top: t, bottom: t + heights()[i], left: 0, height: heights()[i] } as DOMRect;
    });
    const observer = observers[0];
    // The engine's first observation, at the starting width.
    act(() => resize(observer, list, geo.width));
    /** A streamed batch's commit: a new `state`, so the list's `[state]` layout effect runs. */
    const commit = (transcript = replies) =>
      view.rerender(
        <MessageList state={state({ transcript: texts(...transcript) })} sessionEnded={false} expanded={{}} cursor={cursor} onAnswerPermission={() => {}} />,
      );
    return { list, rows, geo, top, observer, commit };
  }

  /** The reader scrolls the view to `scrollTop`, then waits long enough that nothing counts as steering. */
  function park(list: HTMLElement, scrollTop: number) {
    act(() => {
      noteUserScroll(list, "up");
      list.scrollTop = scrollTop;
      fireEvent.scroll(list);
    });
    now += 1000;
  }

  function widen(list: HTMLElement, geo: { width: number }, observer: FakeObserver, width: number) {
    geo.width = width;
    act(() => {
      resize(observer, list, width);
      fireEvent.scroll(list); // the restore's own scroll event, a frame later
    });
    now += 1000;
  }

  it("puts the cursor's row back at its height after a zoom, and the same scrollTop after the unzoom", () => {
    const { list, rows, geo, top, observer } = mount(8);
    park(list, top(8) - 120);
    const before = geo.scrollTop;
    widen(list, geo, observer, 1000);
    expect(rows[8].getBoundingClientRect().top).toBe(120);
    widen(list, geo, observer, 400);
    expect(geo.scrollTop).toBe(before);
  });

  it("does not take a restore clamped at the end for following, so the unzoom returns", () => {
    const { list, rows, geo, top, observer } = mount(10);
    park(list, top(10) - 50);
    const before = geo.scrollTop;
    widen(list, geo, observer, 1600);
    // Clamped: the wide conversation cannot put row 10 that far down, so the view is at its end.
    expect(geo.scrollTop).toBe(list.scrollHeight - geo.height);
    expect(rows[10].getBoundingClientRect().top).toBeGreaterThan(0);
    widen(list, geo, observer, 400);
    expect(geo.scrollTop).toBe(before);
  });

  it("restores before a streamed batch's commit, which can run between the width change and the observer", () => {
    const { list, geo, top, observer, commit } = mount(10);
    park(list, top(10) - 50);
    const before = geo.scrollTop;
    geo.width = 1600; // the page has the new width; the engine clamps at the next layout
    act(() => commit());
    widen(list, geo, observer, 1600);
    widen(list, geo, observer, 400);
    expect(geo.scrollTop).toBe(before);
  });

  it("restores before a scroll event that arrives ahead of the observer", () => {
    const { list, geo, top, observer } = mount(10);
    park(list, top(10) - 50);
    const before = geo.scrollTop;
    geo.width = 1600;
    act(() => {
      fireEvent.scroll(list); // the clamp's own scroll event, dispatched before the observations
    });
    widen(list, geo, observer, 1600);
    widen(list, geo, observer, 400);
    expect(geo.scrollTop).toBe(before);
  });

  it("keeps the place when the zoom comes right after the reader's own scroll", () => {
    const { list, geo, top, observer } = mount(10);
    act(() => {
      noteUserScroll(list, "up");
      list.scrollTop = top(10) - 50;
      fireEvent.scroll(list);
    });
    const before = geo.scrollTop;
    // No time passes: the zoom key lands inside the scroll's steering window.
    geo.width = 1600;
    act(() => {
      resize(observer, list, 1600);
      fireEvent.scroll(list);
    });
    geo.width = 400;
    act(() => {
      resize(observer, list, 400);
      fireEvent.scroll(list);
    });
    expect(geo.scrollTop).toBe(before);
  });

  it("follows again after a wheel down at an end the restore left it at, with no scroll event", () => {
    const { list, geo, top, observer, commit } = mount(10);
    park(list, top(10) - 50);
    widen(list, geo, observer, 1600);
    expect(geo.scrollTop).toBe(list.scrollHeight - geo.height);
    act(() => {
      fireEvent.wheel(list, { deltaY: 120 }); // nothing below: the engine moves nothing and sends no scroll event
    });
    now += 1000;
    act(() => commit([...replies, "a new reply " + "y".repeat(800)]));
    expect(geo.scrollTop).toBe(list.scrollHeight - geo.height);
  });

  /** A reader parked one view above row 10's top, zoomed so the restore is clamped at the end, the observer done. */
  function heldAtEnd() {
    const m = mount(10);
    park(m.list, m.top(10) - 50);
    widen(m.list, m.geo, m.observer, 1600);
    expect(m.geo.scrollTop).toBe(m.list.scrollHeight - m.geo.height);
    return m;
  }
  const grown = [...replies, "a new reply " + "y".repeat(800)];
  const atEnd = (list: HTMLElement, geo: { scrollTop: number; height: number }) => geo.scrollTop === list.scrollHeight - geo.height;

  it("keeps the hold through a wheel up whose movement has not arrived when a batch commits", () => {
    const { list, geo, commit } = heldAtEnd();
    act(() => {
      fireEvent.wheel(list, { deltaY: -120 }); // the engine animates it; its first scroll event comes later
    });
    act(() => commit()); // a batch that changes nothing in height, before the wheel's movement
    act(() => commit(grown));
    expect(atEnd(list, geo)).toBe(false);
  });

  it("keeps the hold through a click (a text selection) that moves nothing", () => {
    const { list, geo, commit } = heldAtEnd();
    act(() => {
      fireEvent.pointerDown(list);
      fireEvent.pointerUp(window);
    });
    now += 1000;
    act(() => commit());
    act(() => commit(grown));
    expect(atEnd(list, geo)).toBe(false);
  });

  it("protects a wheel up at the end whose movement has not arrived, when following had already stopped", () => {
    const { list, geo, commit } = mount(11);
    act(() => {
      list.scrollTop = list.scrollHeight;
      fireEvent.scroll(list);
    });
    now += 1000;
    act(() => noteUserScroll(list, "up")); // `k` from the last row onto one on screen: nothing scrolls
    now += 1000;
    act(() => {
      fireEvent.wheel(list, { deltaY: -120 });
    });
    act(() => commit()); // before the wheel's movement
    act(() => commit(grown));
    expect(atEnd(list, geo)).toBe(false);
  });

  it("does not resume for a wheel down that a nested scroller takes (a tool output's own box)", () => {
    const { list, rows, geo, commit } = heldAtEnd();
    const box = rows[11];
    box.style.overflowY = "auto";
    Object.defineProperty(box, "scrollHeight", { configurable: true, value: 600 });
    Object.defineProperty(box, "clientHeight", { configurable: true, value: 200 });
    act(() => {
      fireEvent.wheel(box, { deltaY: 120 });
    });
    now += 1000;
    act(() => commit(grown));
    expect(atEnd(list, geo)).toBe(false);
  });

  it("does not resume for Space on a focused button, which presses it", () => {
    const { list, rows, geo, commit } = heldAtEnd();
    const button = document.createElement("button");
    rows[11].appendChild(button);
    act(() => {
      fireEvent.keyDown(button, { key: " " });
    });
    now += 1000;
    act(() => commit(grown));
    expect(atEnd(list, geo)).toBe(false);
  });

  it("does not resume for a panel key's downward move (a `j` stepping inside a box)", () => {
    const { list, geo, commit } = heldAtEnd();
    act(() => noteUserScroll(list, "down"));
    now += 1000;
    act(() => commit(grown));
    expect(atEnd(list, geo)).toBe(false);
  });

  it("resumes for End on the list and for an explicit resume (G, a send)", () => {
    const first = heldAtEnd();
    act(() => {
      fireEvent.keyDown(first.list, { key: "End" });
    });
    now += 1000;
    act(() => first.commit(grown));
    expect(atEnd(first.list, first.geo)).toBe(true);
    cleanup();
    vi.restoreAllMocks();
    const second = heldAtEnd();
    act(() => resumeFollowing(second.list));
    now += 1000;
    act(() => second.commit(grown));
    expect(atEnd(second.list, second.geo)).toBe(true);
  });

  /* Why the anchor keeps a character offset in its row, not only a text node: the streaming reply's open end is
     removed and drawn again on every batch, so a node the reader's point was found in is gone by the next zoom. */
  it("a streaming reply's open end is drawn anew on each batch: its text nodes do not survive", () => {
    const streamingState = (text: string) =>
      state({ activeTurnId: "t1", userPrompts: [{ seq: 1, text: "go" }], transcript: [{ seq: 2, text }] });
    const props = { sessionEnded: false, expanded: {}, cursor: -1, onAnswerPermission: () => {} };
    const view = render(<MessageList state={streamingState("First paragraph.\n\nThe second one, still being")} {...props} />);
    const walker = document.createTreeWalker(view.container, NodeFilter.SHOW_TEXT);
    let tail: Node | null = null;
    for (let n = walker.nextNode(); n !== null; n = walker.nextNode()) if (n.textContent?.includes("still being")) tail = n;
    expect(tail).not.toBeNull();
    act(() => view.rerender(<MessageList state={streamingState("First paragraph.\n\nThe second one, still being written")} {...props} />));
    expect(view.container.textContent).toContain("still being written");
    expect(tail!.isConnected).toBe(false);
  });

  it("still snaps a following view to the end", () => {
    const { list, geo, observer } = mount(11);
    act(() => {
      list.scrollTop = list.scrollHeight;
      fireEvent.scroll(list);
    });
    widen(list, geo, observer, 1000);
    widen(list, geo, observer, 400);
    expect(geo.scrollTop).toBe(list.scrollHeight - geo.height);
  });
});

it.each(["look at these", ""])("prompt %j shows image metadata as a muted text line", (text) => {
  const attachments = [
    { mediaType: "image/png", width: 1280, height: 720, bytes: 100 },
    { mediaType: "image/jpeg", width: 800, height: 600, bytes: 200 },
  ];
  const { container } = render(<MessageList state={state({ userPrompts: [{ seq: 1, text, attachments }] })}
    sessionEnded={false} expanded={{}} cursor={-1} onAnswerPermission={vi.fn()} />);
  const row = container.querySelector(".row-prompt")!;
  expect(row.textContent).toContain("[2 images: PNG 1280×720, JPEG 800×600]");
  expect(row.querySelector(".prompt-images")?.textContent).toBe("[2 images: PNG 1280×720, JPEG 800×600]");
  expect(row.querySelector("img")).toBeNull();
  expect(buildTimeline(state({ userPrompts: [{ seq: 1, text, attachments }] }))[0]).toMatchObject({
    text: [text, "[2 images: PNG 1280×720, JPEG 800×600]"].filter(Boolean).join("\n"),
  });
});
