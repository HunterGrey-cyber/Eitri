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

describe("MessageList transcript rendering", () => {
  it("renders assistant text as markdown, not as escaped source", () => {
    const { container } = render(
      <MessageList state={state({ transcript: texts("# Heading\n\nsome **bold** text") })} sessionEnded={false} onAnswerPermission={vi.fn()} />,
    );
    expect(container.querySelector(".assistant-message h1")?.textContent).toBe("Heading");
    expect(container.querySelector(".assistant-message strong")?.textContent).toBe("bold");
  });

  it("renders one block per transcript entry, in order", () => {
    const { container } = render(
      <MessageList state={state({ transcript: texts("first", "second") })} sessionEnded={false} onAnswerPermission={vi.fn()} />,
    );
    const messages = Array.from(container.querySelectorAll(".assistant-message"));
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
  /** Every rendered block, in DOM order, as a short label. */
  function rendered(container: HTMLElement): string[] {
    return Array.from(container.querySelectorAll(".assistant-message, .tool-message, .permission-card")).map((el) => {
      if (el.classList.contains("assistant-message")) return `text:${el.textContent?.trim()}`;
      if (el.classList.contains("permission-card")) return "perm";
      return `tool:${el.querySelector(".tool-call")?.getAttribute("data-tool-name") ?? "?"}`;
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
      <MessageList state={state({ transcript: texts(markdown) })} sessionEnded={false} onAnswerPermission={vi.fn()} />,
    );
    return container.querySelector(".assistant-message")!.innerHTML;
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
        onAnswerPermission={vi.fn()}
      />,
    );
    const button = container.querySelector(".assistant-message button");
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
        onAnswerPermission={vi.fn()}
      />,
    );
    const calls = Array.from(container.querySelectorAll(".tool-call"));
    expect(calls).toHaveLength(2);
    expect(calls[0].querySelector(".tool-result")!.getAttribute("data-state")).toBe("running");
    expect(calls[1].querySelector(".tool-result")!.getAttribute("data-state")).toBe("done");
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
        onAnswerPermission={vi.fn()}
      />,
    );
    const awaiting = Array.from(container.querySelectorAll(".tool-message[data-awaiting-permission='true']"));
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
        onAnswerPermission={vi.fn()}
      />,
    );
    const awaiting = Array.from(container.querySelectorAll(".tool-message[data-awaiting-permission='true']"));
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
        onAnswerPermission={vi.fn()}
      />,
    );
    expect(container.querySelector(".tool-message[data-awaiting-permission='true']")).toBeNull();
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
        onAnswerPermission={vi.fn()}
      />,
    );
    expect(container.querySelector(".tool-message[data-awaiting-permission='true']")).toBeNull();
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
        onAnswerPermission={onAnswerPermission}
      />,
    );
    const approve = Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Approve")!;
    expect(approve.disabled).toBe(true);
    fireEvent.click(approve);
    expect(onAnswerPermission).not.toHaveBeenCalled();
  });
});
