// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { MessageList } from "./MessageList";
import { initialState } from "../reducer";
import type { AgentUiState } from "../types";

// See ModeSelector.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
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
   the glyph in `data-sign` differs too. This is the one test asserting the glyphs themselves. */
describe("MessageList row structure", () => {
  it("gives every row a sign column, and distinguishes state by glyph not only colour", () => {
    render(
      <MessageList
        state={{
          ...initialState(),
          userPrompts: [{ seq: 1, text: "do the thing" }],
          transcript: [{ seq: 2, text: "on it" }],
          toolCalls: [
            { seq: 3, toolUseId: "a", name: "Read", input: {}, result: null },
            { seq: 4, toolUseId: "b", name: "Read", input: {}, result: { content: "ok", isError: false } },
            { seq: 5, toolUseId: "c", name: "Read", input: {}, result: { content: "boom", isError: true } },
          ],
        }}
        sessionEnded={false}
        expanded={{}}
        cursor={-1}
        onAnswerPermission={() => {}}
      />,
    );
    const signs = Array.from(document.querySelectorAll<HTMLElement>(".row")).map((r) => r.dataset.sign);
    expect(signs).toEqual(["›", "", "◐", "✓", "✗"]);
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
          toolCalls: [
            { seq: 0, toolUseId: "toolu_1", name: "Bash", input: { command: "echo one" }, result: null },
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
    expect(container.querySelector(".tool-result-folded")).not.toBeNull();
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
    expect(awaiting[0].textContent).toContain("rm -rf /");
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
    expect(awaiting[0].textContent).toContain("rm -rf /");
    expect(container.querySelector(".permission-card-tool-use-id")!.textContent).toContain("toolu_second");
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
