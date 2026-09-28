// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { MessageList } from "./MessageList";
import { noteUserScroll, resumeFollowing } from "../follow";
import { applyEvent, initialState } from "../reducer";
import type { AgentDomainEvent, AgentUiState } from "../types";

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

/* Test fixtures were written before items carried a `seq`, so they are written here as they read
   best and given ascending seqs. Order-sensitive tests below assign their own seqs explicitly. */
function texts(...values: string[]) {
  return values.map((text, i) => ({ seq: i, text }));
}

/* `cursor` was added to `Props` in panel-as-document task 5's review round 1 (the cursor must be
   visible, `.row-current`/`aria-current`). Every test above this line predates it and is not
   testing the cursor -- `cursor={-1}` is passed throughout them (never a real timeline index) so
   none of their rows render as current, preserving exactly the rendering these tests already
   assert on. The cursor-highlight behaviour itself is its own describe block, at the bottom. */

/* Task 3 of the panel-as-document plan: every conversation item is now a row on one sign-column
   grid (`.row.row-<kind>`, `.row-sign`, `.row-body`), and the row's state is never colour alone --
   the glyph in `data-sign` differs too. This is the one test asserting the glyphs themselves.
   The two finished calls sit apart, each with something else between them (Task 13, P2): two
   ADJACENT finished calls collapse into one `run` row now, which would leave only one glyph on
   screen for both -- exactly the wrong fixture for a test about telling states apart by glyph. */
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

/* The defect this closes (2026-09-15). This component mapped `transcript`, then `toolCalls`, then
   `pendingPermissions` -- three sequential maps -- so however a turn really went, the DOM read as
   every message, then every tool card, then every permission card. A turn with several tool calls
   read in the wrong order, which is the panel's most visible untruth about what the agent did.

   The order now comes from `seq`, which Rust's projection assigns and its snapshot ships, so this
   renders the same sequence after a reload as it did live. */
describe("MessageList interleaves the conversation in its real order", () => {
  /** Every rendered row, in DOM order, as a short label. */
  function rendered(container: HTMLElement): string[] {
    return Array.from(container.querySelectorAll<HTMLElement>(".row")).map((el) => {
      if (el.classList.contains("row-assistant")) return `text:${el.textContent?.trim()}`;
      if (el.classList.contains("row-permission")) return "perm";
      if (el.classList.contains("row-tool")) return `tool:${el.querySelector(".tool-call")?.getAttribute("data-tool-name") ?? "?"}`;
      // This helper only knows the three kinds these tests mix (text/perm/tool). A silent
      // `else -> tool:` used to swallow anything else (a `row-prompt` fixture added later, say)
      // into a mislabelled "tool:?" instead of a failure -- fail loudly instead.
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

  /* The legacy backend -- still the default -- sends no `tool_use_id`, so this is the ordinary case
     on a default install, not an edge case. The card still has to land where it arrived. */
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

/* The Critical finding this sanitization closed: model output is rendered with
   `dangerouslySetInnerHTML`, and model output is attacker-influenceable (a repository file, a web
   page the model fetched, a tool result). Script execution inside THIS document is not a contained
   XSS -- the panel holds `window.webkit.messageHandlers.neovibeAgent`, the same bridge that relays
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
        // t-0 is still running, which always renders regardless of `expanded` -- see
        // `renderToolCall`. t-1 is finished, and this test is about tool_use_id keying, not about
        // the fold added in this task, so it is expanded here to keep asserting the state it always
        // asserted rather than the folded placeholder.
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

  /* Spec §3.2's default: a FINISHED call with no entry in `expanded` shows the folded placeholder,
     not its result. Round 1 review found this had zero coverage -- every other test here either
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
    // P4 (Task 13): a gated call says it is waiting rather than repeating the command a card below
    // it already shows.
    expect(awaiting[0].textContent).toContain("waiting for approval");
    expect(awaiting[0].textContent).not.toContain("rm -rf /");
  });

  /* The legacy backend's hook-relay shape specifically (new on 2026-09-15): permissionId and
     toolUseId are the same string, because the `PreToolUse` payload's own `tool_use_id` is both.
     The marker must still land on the right call -- it is matched on toolUseId, and the fact that
     the card is ALSO keyed on that value changes nothing. */
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
    fireEvent.click(Array.from(cards[1].querySelectorAll("button")).find((b) => b.textContent === "Approve")!);
    expect(onAnswerPermission).toHaveBeenCalledWith("perm-2", "allow", undefined);
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

/* sw-panel-render-6: a `null` result used to mean "still running" unconditionally, so a tool call
   abandoned by session end or a resume that restored a call from before its own history boundary
   rendered a permanent spinner. `MessageList` is what has both the call's `seq` and `state.history`
   /`sessionEnded` in scope, so it is where the "can this ever complete?" verdict is actually made
   (`isAbandonedCall`) before it reaches `renderToolCall`/`ToolResult`. */
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
    // Probe lifted from the verdict: `history.uptoSeq=3`, a tool call at seq 2 with `result:null` --
    // a resume/reload can never see a `tool_call_completed` for a call that already existed when
    // history was cut, so this can never complete.
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

  /* Codex, whole-branch review: an INTERRUPTED turn in a fresh session -- `tool_call_started`, then
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

/* The other half of "j无法在长输出内部下滑": before this, a new ROW unconditionally scrolled the
   view to the bottom, which fought the cursor the moment a user had used `j`/`k` to read something
   further up. (Not every streamed delta, as this comment first said: deltas merge into one
   transcript entry and never re-ran the effect -- see `MessageList.tsx`'s own comment.) The guard is
   driven by scroll events on `.message-list`, not by the cursor and not by a measurement taken after
   the new row is already in the DOM -- see that same comment for why each of those was wrong. jsdom
   implements no layout and fires no scroll events on its own, so these tests set the three
   dimensions and dispatch `scroll` themselves. */
function setScroll(list: HTMLElement, dims: { scrollHeight: number; clientHeight: number; scrollTop: number }) {
  Object.defineProperty(list, "scrollHeight", { value: dims.scrollHeight, configurable: true });
  Object.defineProperty(list, "clientHeight", { value: dims.clientHeight, configurable: true });
  Object.defineProperty(list, "scrollTop", { value: dims.scrollTop, configurable: true, writable: true });
}

/* Correction (the GUI pass, 2026-09-24, its finding F-c): the tests in this block asserted that a new
   row was followed by a call to `scrollIntoView` -- the smooth scroll a new row used to get. That scroll
   left the row below the edge for the frame or two its animation took, so following is a snap to the
   end now, new row or not, and these tests assert where the list ends up instead. */
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

  /* The review's finding: the first guard measured AFTER the new row was in the DOM, so a row taller
     than the 24px threshold made a user sitting exactly at the bottom look scrolled away. Here the
     user is at the very bottom, and the new row grows the list by 600px with no scroll event -- which
     is what real content growth does. Checked against the old guard: it fails this test. */
  it("still follows when the new row alone is taller than the threshold", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("first") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    dims.grow(2600);

    rerender(<MessageList state={state({ transcript: texts("first", "second") })} {...props} />);

    expect(list.scrollTop).toBe(2200);
  });

  /* The GUI pass's F-c, and red on `8e58403`: the new row is at the end in the SAME commit -- before
     paint -- rather than after a smooth scroll's animation, which is what left the user's own prompt
     the last row with text for a frame or two as a reply's first row mounted. */
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
    // A real wheel: the `wheel` event, then the scroll it causes (2026-09-24: the intent is what
    // counts, not the direction of the scroll alone).
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

/* The owner's panel design (2026-09-19), point D: with the view at the bottom, a reply that grows
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

/* Review findings on the follow guard (2026-09-19). A list whose `scrollTop` clamps the way a
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

/* Change B of the 2026-09-24 fix: leaving the bottom takes something the USER did. WebKitGTK once
   dropped `scrollTop` by itself on every clock tick mid-reply (see the dated record), and the old
   rule -- "a scroll event saw scrollTop go down" -- read each drop as the reader leaving. */
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

  // Fix round 1: "any wheel up" that the list itself can take -- one that moves nothing is its own
  // block below.
  it("stops following on any wheel up the list can take, however small (deliberately, since 2026-09-24)", () => {
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

/* Fix round 1 of the 2026-09-24 fix, from its adversarial review (findings 3a and 3b). Change B
   recognised a wheel, a held pointer, a touch drag and the panel's own keys, and read every other
   scroll as the engine's -- so (a) a reader who scrolled up by a route it did not list (the
   browser's own PageUp/arrow scroll with a control inside the list focused, a Tab reveal) was put
   back at the bottom by the next delta, a trap the base did not have; and (b) any wheel with a
   negative `deltaY` ended following even when the list never moved (a sideways touchpad swipe with
   a sub-pixel vertical jitter, a wheel an expanded tool result's own box took, a wheel with nothing
   to scroll up to), which the base did not do either. Each test below that is not marked as a
   guard fails on `b313411`. */
describe("MessageList: a send resumes following (the phase-3 GUI pass, 2026-09-25)", () => {
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

/* Fix round 2, from the fix's re-review (M1). Round 1 took a provisional stop back only inside the
   follow effect, which runs on a state change. A misprediction (a wheel the engine sent somewhere
   else) followed, inside its window, by a new row and then silence -- a permission card, after which
   the turn waits -- left that row below the view until something else arrived, which may be nothing.
   The take-back now also runs when the gesture's window runs out, and catches up with what the stop
   held back. The five tests below that are not marked as guards fail on `287cf9e`; so does the unmount
   guard, which counts the new timer itself. The other three guards pass on both.

   Correction (the GUI pass, 2026-09-24): the catch-up for a new row is a snap now, as every follow is,
   so the tests below read `scrollTop` where they read a call to `scrollIntoView`. */
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

  /* Found while building the real-engine test for the above (fix round 2): WebKitGTK performs the
     default scroll for a wheel event and animates it over about 200ms, and its FIRST scroll event,
     0-1ms after the wheel, can report the list exactly where the wheel found it -- seen in 2 of 8
     wheels at the tail. At the bottom, that event re-armed following and cleared the provisional
     stop, so the next delta (or a new row's smooth scroll) cancelled the wheel's scroll: the very
     case the stop exists for. The first two fail on `287cf9e`; the three after them are guards. */
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

/* The restored-history notice (design §5.5/§10). It lives at the head of the list rather than above
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

/* The rows' width comes from here, not from a CSS size container (2026-09-24). `.row` used to be
   `container-type: inline-size`, which in WebKitGTK made the list lose its reader whenever the
   status line's clock ticked during a streamed reply (`shell/tests/panel_stream_scroll.rs` is the
   real-engine test). The replacement writes the list's content-box width to `--list-inline-size`,
   and its whole safety argument is WHEN it writes: before the first paint, then only on a resize --
   never on a streamed delta, which would restyle the conversation 30 times a second. */
describe("MessageList measures its own width for the rows' escapes", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: -1, onAnswerPermission: () => {} };

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("writes --list-inline-size on the list before anything else reads it", () => {
    // jsdom lays nothing out, so the content box measures 0 -- which the stylesheet turns into "no
    // escape" (see index.css's `.row` rule). The point is that the property is there from the start.
    const { container } = render(<MessageList state={state({ transcript: texts("hi") })} {...props} />);
    const list = container.querySelector<HTMLElement>(".message-list")!;
    expect(list.style.getPropertyValue("--list-inline-size")).toBe("0px");
  });

  it("re-measures on a resize, and writes nothing while a reply streams", () => {
    const observers = stubResizeObserver();
    const { container, rerender, unmount } = render(
      <MessageList state={state({ transcript: texts("the reply so far") })} {...props} />,
    );
    const list = container.querySelector<HTMLElement>(".message-list")!;
    expect(observers).toHaveLength(1);
    // The list itself, and (since the GUI pass, 2026-09-24) the one row in it -- see the next block.
    expect(observers[0].observed).toEqual([list, list.querySelector(".row")]);
    const setProperty = vi.spyOn(list.style, "setProperty");

    // A resize reaches the rows, as the content box the observer reports.
    resize(observers[0], list, 537.5);
    expect(list.style.getPropertyValue("--list-inline-size")).toBe("537.5px");
    expect(setProperty).toHaveBeenCalledTimes(1);
    // The same size again changes nothing, so it costs no restyle.
    resize(observers[0], list, 537.5);
    expect(setProperty).toHaveBeenCalledTimes(1);
    // A ROW's entry is not the list's width, whatever its content box says.
    resize(observers[0], list.querySelector(".row")!, 120);
    expect(list.style.getPropertyValue("--list-inline-size")).toBe("537.5px");

    // Streaming: the open message grows on every delta (a new `state` each time). No write, no
    // second observer, and the growing row is not observed again.
    for (const text of ["the reply so far, and", "the reply so far, and more", "the reply so far, and more text"]) {
      rerender(<MessageList state={state({ transcript: texts(text) })} {...props} />);
    }
    expect(setProperty).toHaveBeenCalledTimes(1);
    expect(observers).toHaveLength(1);
    expect(observers[0].observed).toHaveLength(2);

    unmount();
    expect(observers[0].disconnected).toBe(true);
  });
});

/* The sandbox GUI pass (2026-09-24), its finding F-a: the follow snap ran only on a state change, so
   a resize with nothing streaming -- a divider drag, an unzoom, the bottom terminal shown, the composer
   growing -- left a following view short (482px and 651px, measured), for as long as a tool call ran,
   a permission card waited, or the turn was over. Rows can change size with the list's box unchanged
   too (`--prose-measure`; a font push, the investigation's F5, also shortens the list, since the
   status line and composer grow with it). The `ResizeObserver` watches the list and every row, and
   snaps a following view to its end. The four tests not marked
   as guards fail on `8e58403`, which observed the list alone and only ever wrote its width. */
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
    dims.content(2482); // the GUI pass's divider drag: 482px of reflow below the view

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

/** The last `(label, jump, afterSeq)` triple `onUnreadChange` was called with -- panel round 2 plan,
 *  Task 10: the pill no longer floats itself (`NewPill`, deleted); it reports upward instead, and the
 *  band (`App.tsx`) holds the latest value the way this reads it back for a test. `afterSeq` (wave 3,
 *  Task 3) is the `seq` threshold itself -- see `MessageList`'s own doc comment on the prop. */
function lastUnread(mock: ReturnType<typeof vi.fn>): { label: string | null; jump: () => void; afterSeq: number | null } {
  const call = mock.mock.calls[mock.mock.calls.length - 1] as [string | null, () => void, number | null];
  return { label: call[0], jump: call[1], afterSeq: call[2] };
}

describe("MessageList: the R2 pill (reported upward, panel round 2 plan Task 10)", () => {
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

  it("keeps approval after a row resizes -- the observer read the first render's timeline (GUI pass 2026-09-25)", () => {
    // Seen in the sandbox: a Bash card landed below a reader and the pill said "Jump to bottom". The
    // `ResizeObserver`, made once, called the FIRST render's `onScroll`, whose `updatePill` sliced the
    // first render's timeline -- nothing past the reader's mark -- and overwrote the right label the
    // moment the new rows' size settled. jsdom has no observer; the fake below delivers that resize.
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

/* Wave 3, Task 3. The old threshold was a timeline INDEX (the length at the moment following
   stopped), sliced off the CURRENT timeline. `buildTimeline` does not keep the array in `seq` order
   -- a card anchored to an older tool call is spliced in right after that call, which can land it
   BEFORE the old length cutoff even though it arrived (its own `seq`) long after. A `seq` threshold
   fixes that: what matters is when an item happened, never where `buildTimeline` chose to draw it. */
describe("MessageList: the unread threshold is a seq, not an index (wave 3, Task 3)", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: 0, onAnswerPermission: vi.fn() };

  /** Parks the view (scrolled up and away from the bottom), the same way the R2 pill tests above do,
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
    // `MessageList` is not remounted across a tab switch (that is the whole reason this task exists),
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
