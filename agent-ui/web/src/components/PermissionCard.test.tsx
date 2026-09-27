// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { render, cleanup, fireEvent } from "@testing-library/react";

// See EmptyTab.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);
import { PermissionCard } from "./PermissionCard";
import type { PermissionRequestRecord } from "../types";

const REQUEST: PermissionRequestRecord = {
  // `seq` places the card in the conversation; this component renders one card in isolation, so
  // the value is immaterial here. Placement itself is `timeline.ts`'s job and is tested there.
  seq: 0,
  permissionId: "perm-1",
  toolUseId: "toolu_01ABC",
  toolName: "Bash",
  input: { command: "rm -rf /" },
};

function buttons(container: HTMLElement) {
  const all = Array.from(container.querySelectorAll("button"));
  return {
    approve: all.find((b) => b.textContent === "Approve")!,
    deny: all.find((b) => b.textContent === "Deny")!,
  };
}

describe("PermissionCard decisions", () => {
  it("sends a typed allow, with no reason attached", () => {
    const onAnswer = vi.fn();
    const { container } = render(
      <PermissionCard request={REQUEST} sessionEnded={false} onAnswer={onAnswer} />,
    );
    fireEvent.change(container.querySelector("input")!, { target: { value: "typed but irrelevant" } });
    fireEvent.click(buttons(container).approve);
    // An approval never carries a reason: nothing downstream has a field to show it in, so sending
    // one would be inventing a channel that does not exist.
    expect(onAnswer).toHaveBeenCalledWith("perm-1", "allow", undefined);
  });

  it("sends a typed deny carrying the reason the model will be shown", () => {
    const onAnswer = vi.fn();
    const { container } = render(
      <PermissionCard request={REQUEST} sessionEnded={false} onAnswer={onAnswer} />,
    );
    fireEvent.change(container.querySelector("input")!, { target: { value: "not in this repo" } });
    fireEvent.click(buttons(container).deny);
    expect(onAnswer).toHaveBeenCalledWith("perm-1", "deny", "not in this repo");
  });

  it("does not send the same decision twice while the real resolution is still in flight", () => {
    const onAnswer = vi.fn();
    const { container } = render(
      <PermissionCard request={REQUEST} sessionEnded={false} onAnswer={onAnswer} />,
    );
    fireEvent.click(buttons(container).approve);
    fireEvent.click(buttons(container).approve);
    fireEvent.click(buttons(container).deny);
    expect(onAnswer).toHaveBeenCalledTimes(1);
    // The card is still on screen. Only a real PermissionResolved event removes it -- this component
    // never decides that its own request is finished.
    expect(container.querySelector(".permission-card")).not.toBeNull();
  });
});

/* A turn can have several tool calls in flight at once, so "Bash wants to run" does not identify
   anything on its own. The id is what ties the card to the exact call above it in the transcript --
   the same id `MessageList` keys that call's block on. */
describe("PermissionCard and the tool call it gates", () => {
  /** v1 polish F21: in a tooltip on the card's tool line, never as visible text. */
  it("names the tool call the request belongs to, in a tooltip only", () => {
    const { container } = render(
      <PermissionCard request={REQUEST} sessionEnded={false} onAnswer={vi.fn()} />,
    );
    const tool = container.querySelector(".permission-card-tool")!;
    expect(tool.getAttribute("title")).toBe("tool call toolu_01ABC");
    expect(tool.getAttribute("data-tool-use-id")).toBe("toolu_01ABC");
    expect(container.textContent).not.toContain("toolu_01ABC");
    expect(container.textContent).not.toContain("for tool call");
  });

  /* proto3 has no absent scalar: an unset `tool_use_id` reaches Rust as "" and is carried through
     as `Some("")`, so "no link" arrives in two shapes and both have to read the same. */
  it("treats a proto3 empty-string id as no link at all", () => {
    const { container } = render(
      <PermissionCard request={{ ...REQUEST, toolUseId: "" }} sessionEnded={false} onAnswer={vi.fn()} />,
    );
    expect(container.querySelector(".permission-card-tool")!.hasAttribute("title")).toBe(false);
    expect(container.querySelector("[data-tool-use-id]")).toBeNull();
  });

  it("says nothing at all rather than inventing a link when the backend sent none", () => {
    // The legacy backend's own PermissionRequest has no field to populate this from; a placeholder
    // like "unknown call" would read as a real, failed lookup instead of as an absent field.
    const { container } = render(
      <PermissionCard request={{ ...REQUEST, toolUseId: null }} sessionEnded={false} onAnswer={vi.fn()} />,
    );
    expect(container.querySelector(".permission-card-tool")!.hasAttribute("title")).toBe(false);
    expect(container.querySelector("[data-tool-use-id]")).toBeNull();
  });
});

describe("PermissionCard on a session that has ended", () => {
  /* The specific thing being prevented: a card left over from a session that died stays clickable,
     the user clicks Approve, and the decision is posted into a session that no longer exists. */
  it("cannot submit into a dead session", () => {
    const onAnswer = vi.fn();
    const { container } = render(
      <PermissionCard request={REQUEST} sessionEnded onAnswer={onAnswer} />,
    );
    const { approve, deny } = buttons(container);
    expect(approve.disabled).toBe(true);
    expect(deny.disabled).toBe(true);
    fireEvent.click(approve);
    fireEvent.click(deny);
    expect(onAnswer).not.toHaveBeenCalled();
  });

  it("stays visible and says why it is inert, rather than vanishing", () => {
    const { container } = render(
      <PermissionCard request={REQUEST} sessionEnded onAnswer={vi.fn()} />,
    );
    // Removing it would read as a resolution nobody made; the request really was left unanswered.
    expect(container.querySelector(".permission-card")).not.toBeNull();
    expect(container.querySelector(".permission-card-stale")?.textContent).toContain(
      "ended before the request was answered",
    );
  });
});

describe("P4: the Bash card", () => {
  it("shows the command as a shell line with real newlines, and its description", () => {
    const { container } = render(
      <PermissionCard
        request={{ ...REQUEST, input: { command: 'git commit -m "one\ntwo"', description: "Commit the fix" } }}
        sessionEnded={false}
        onAnswer={vi.fn()}
      />,
    );
    expect(container.querySelector("pre.permission-card-command")!.textContent).toBe('$ git commit -m "one\ntwo"');
    expect(container.querySelector(".permission-card-description")!.textContent).toBe("Commit the fix");
    expect(container.querySelector(".permission-card-input")).toBeNull();
  });

  it("keeps JSON for a tool it has no view for", () => {
    const { container } = render(<PermissionCard request={{ ...REQUEST, toolName: "mcp__x__y", input: { k: "v" } }} sessionEnded={false} onAnswer={vi.fn()} />);
    expect(container.querySelector(".permission-card-input")!.textContent).toContain('"k": "v"');
  });
});

describe("P5 and D7", () => {
  it("denies with the reason on Enter in the reason box", () => {
    const onAnswer = vi.fn();
    const { container } = render(<PermissionCard request={REQUEST} sessionEnded={false} onAnswer={onAnswer} />);
    const reason = container.querySelector<HTMLInputElement>("input")!;
    fireEvent.change(reason, { target: { value: "not the whole disk" } });
    fireEvent.keyDown(reason, { key: "Enter" });
    expect(onAnswer).toHaveBeenCalledWith("perm-1", "deny", "not the whole disk");
  });

  /** Review focus 3. */
  it("enter_in_the_reason_box_while_composing_does_not_deny", () => {
    const onAnswer = vi.fn();
    const { container } = render(<PermissionCard request={REQUEST} sessionEnded={false} onAnswer={onAnswer} />);
    const reason = container.querySelector<HTMLInputElement>("input")!;
    fireEvent.keyDown(reason, { key: "Enter", isComposing: true });
    fireEvent.keyDown(reason, { key: "Enter", keyCode: 229 });
    expect(onAnswer).not.toHaveBeenCalled();
  });

  it("offers Always allow only with a rule from Rust, third for l, and sends remember", () => {
    const onAnswer = vi.fn();
    const without = render(<PermissionCard request={REQUEST} sessionEnded={false} onAnswer={onAnswer} />);
    expect(without.container.textContent).not.toContain("Always allow");
    without.unmount();
    const { container } = render(<PermissionCard request={REQUEST} sessionEnded={false} onAnswer={onAnswer} ruleOffer="git push *" />);
    const always = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.startsWith("Always allow"))!;
    expect(always.textContent).toBe("Always allow git push * in this project");
    expect(always.getAttribute("data-nav-order")).toBe("3");
    expect(container.querySelector("input")!.getAttribute("data-nav-order")).toBe("4");
    fireEvent.click(always);
    expect(onAnswer).toHaveBeenCalledWith("perm-1", "allow", undefined, true);
  });
});

/** v1 polish F22: a Write over no file (Rust looked when the card was raised) says it creates one;
 *  over an existing file, or when nobody looked, the overwrite warning stays. */
describe("PermissionCard for a Write", () => {
  const write: PermissionRequestRecord = { ...REQUEST, toolName: "Write", input: { file_path: "/p/new.txt", content: "hi\n" } };
  const WARNING = "Writes the whole file. The request does not say what is there now.";

  it("says it creates a new file, without the overwrite warning", () => {
    const { container } = render(<PermissionCard request={{ ...write, createsFile: true }} sessionEnded={false} onAnswer={vi.fn()} />);
    expect(container.textContent).toContain("Creates a new file.");
    expect(container.textContent).not.toContain(WARNING);
  });

  it("keeps the warning over an existing file", () => {
    const { container } = render(<PermissionCard request={write} sessionEnded={false} onAnswer={vi.fn()} />);
    expect(container.textContent).toContain(WARNING);
    expect(container.textContent).not.toContain("Creates a new file.");
  });
});
