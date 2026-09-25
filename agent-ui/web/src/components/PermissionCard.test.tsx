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
  it("names the tool call the request belongs to", () => {
    const { container } = render(
      <PermissionCard request={REQUEST} sessionEnded={false} onAnswer={vi.fn()} />,
    );
    expect(container.querySelector(".permission-card-tool-use-id")?.textContent).toContain("toolu_01ABC");
  });

  /* proto3 has no absent scalar: an unset `tool_use_id` reaches Rust as "" and is carried through
     as `Some("")`, so "no link" arrives in two shapes and both have to read the same. */
  it("treats a proto3 empty-string id as no link at all", () => {
    const { container } = render(
      <PermissionCard request={{ ...REQUEST, toolUseId: "" }} sessionEnded={false} onAnswer={vi.fn()} />,
    );
    expect(container.querySelector(".permission-card-tool-use-id")).toBeNull();
  });

  it("says nothing at all rather than inventing a link when the backend sent none", () => {
    // The legacy backend's own PermissionRequest has no field to populate this from; a placeholder
    // like "unknown call" would read as a real, failed lookup instead of as an absent field.
    const { container } = render(
      <PermissionCard request={{ ...REQUEST, toolUseId: null }} sessionEnded={false} onAnswer={vi.fn()} />,
    );
    expect(container.querySelector(".permission-card-tool-use-id")).toBeNull();
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
