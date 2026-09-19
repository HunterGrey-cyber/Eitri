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

describe("MessageList auto-follow", () => {
  const props = { sessionEnded: false, expanded: {}, cursor: 0, onAnswerPermission: vi.fn() };

  it("still follows a new item to the bottom when the viewport was already near it", () => {
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    const { rerender } = render(<MessageList state={state({ transcript: texts("first") })} {...props} />);
    scrollIntoView.mockClear();

    rerender(<MessageList state={state({ transcript: texts("first", "second") })} {...props} />);

    expect(scrollIntoView).toHaveBeenCalled();
  });

  /* The review's finding: the first guard measured AFTER the new row was in the DOM, so a row taller
     than the 24px threshold made a user sitting exactly at the bottom look scrolled away. Here the
     user is at the very bottom, and the new row grows the list by 600px with no scroll event -- which
     is what real content growth does. Checked against the old guard: it fails this test. */
  it("still follows when the new row alone is taller than the threshold", () => {
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("first") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1600 });
    fireEvent.scroll(list);
    setScroll(list, { scrollHeight: 2600, clientHeight: 400, scrollTop: 1600 });
    scrollIntoView.mockClear();

    rerender(<MessageList state={state({ transcript: texts("first", "second") })} {...props} />);

    expect(scrollIntoView).toHaveBeenCalled();
  });

  it("does not fight a user reading further up: skips the follow once the user scrolled toward the top", () => {
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("first") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1600 });
    fireEvent.scroll(list);
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 800 });
    fireEvent.scroll(list);
    scrollIntoView.mockClear();

    rerender(<MessageList state={state({ transcript: texts("first", "second") })} {...props} />);

    expect(scrollIntoView).not.toHaveBeenCalled();
  });

  it("resumes following once the user scrolls back to the bottom", () => {
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("first") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1600 });
    fireEvent.scroll(list);
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 800 });
    fireEvent.scroll(list);
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1590 });
    fireEvent.scroll(list);
    scrollIntoView.mockClear();

    rerender(<MessageList state={state({ transcript: texts("first", "second") })} {...props} />);

    expect(scrollIntoView).toHaveBeenCalled();
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
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 800 });
    fireEvent.scroll(list);
    setScroll(list, { scrollHeight: 2300, clientHeight: 400, scrollTop: 800 });

    rerender(<MessageList state={state({ transcript: texts("partial reply, longer now") })} {...props} />);

    expect(list.scrollTop).toBe(800);
  });

  /* A browser delivers `scroll` at the next frame, so a `k` that scrolled up can still be
     unannounced when the next delta arrives. The effect reads the position itself first; without
     that, this delta would snap the user straight back down. */
  it("honours a scroll up whose scroll event has not arrived yet", () => {
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("par") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    setScroll(list, { scrollHeight: 2000, clientHeight: 400, scrollTop: 1600 });
    fireEvent.scroll(list);
    // Scrolled up, no event dispatched; the content then grows.
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
    list.scrollTop -= 60; // `k`, with no scroll event yet for either scroll
    dims.grow(2400);

    rerender(<MessageList state={state({ transcript: texts("par") })} {...props} />);

    expect(list.scrollTop).toBe(1840);
  });

  it("content shrinking under a user at the bottom (scrollTop clamped down) keeps following", () => {
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    const { container, rerender } = render(<MessageList state={state({ transcript: texts("first") })} {...props} />);
    const list = container.querySelector(".message-list") as HTMLElement;
    const dims = clampedList(list, 400, { scrollHeight: 2000, scrollTop: 1600 });
    fireEvent.scroll(list);
    dims.grow(1900); // a row went away: the browser clamps scrollTop to 1500 and fires scroll
    fireEvent.scroll(list);
    scrollIntoView.mockClear();

    rerender(<MessageList state={state({ transcript: texts("first", "second") })} {...props} />);

    expect(scrollIntoView).toHaveBeenCalled();
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
