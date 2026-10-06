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

describe("PermissionCard when the panel withdraws its answer", () => {
  /** Codex's whole-branch review: Rust refused a click's answer (an "Always allow" whose rule could
   *  not be saved) and left the card waiting, but the card's own `clicked` kept its buttons disabled
   *  for good. The panel takes its answer back (`alreadyAnswered` true -> false) and the click goes
   *  with it. */
  it("a click the panel took back leaves the buttons live again", () => {
    const onAnswer = vi.fn();
    const { container, rerender } = render(
      <PermissionCard request={REQUEST} sessionEnded={false} onAnswer={onAnswer} />,
    );
    fireEvent.click(buttons(container).approve);
    rerender(<PermissionCard request={REQUEST} sessionEnded={false} alreadyAnswered onAnswer={onAnswer} />);
    expect(buttons(container).approve.disabled).toBe(true);
    rerender(<PermissionCard request={REQUEST} sessionEnded={false} alreadyAnswered={false} onAnswer={onAnswer} />);
    expect(buttons(container).approve.disabled).toBe(false);
    fireEvent.click(buttons(container).approve);
    expect(onAnswer).toHaveBeenCalledTimes(2);
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

  it("shows bidi and invisible characters in the command as visible escapes", () => {
    const { container } = render(
      <PermissionCard
        request={{ ...REQUEST, input: { command: "cat a\u202Etxt.exe\u200B" } }}
        sessionEnded={false}
        onAnswer={vi.fn()}
      />,
    );
    const pre = container.querySelector("pre.permission-card-command")!;
    expect(pre.textContent).toBe("$ cat a⟨U+202E⟩txt.exe⟨U+200B⟩");
    expect(pre.textContent).not.toContain("\u202E");
    expect(pre.querySelectorAll(".permission-card-escape")).toHaveLength(2);
    expect(container.querySelector(".permission-card-warning")!.textContent).toContain("2 invisible or direction-changing");
  });

  it("shows them escaped in the JSON view too, and warns only when there are any", () => {
    const { container } = render(
      <PermissionCard request={{ ...REQUEST, toolName: "mcp__x__y", input: { k: "a\u202Eb" } }} sessionEnded={false} onAnswer={vi.fn()} />,
    );
    const pre = container.querySelector("pre.permission-card-input")!;
    expect(pre.textContent).toContain('"k": "a⟨U+202E⟩b"');
    expect(pre.textContent).not.toContain("\u202E");
    expect(container.querySelector(".permission-card-warning")!.textContent).toContain("1 invisible or direction-changing character,");
    cleanup();
    const plain = render(<PermissionCard request={REQUEST} sessionEnded={false} onAnswer={vi.fn()} />);
    expect(plain.container.querySelector(".permission-card-warning")).toBeNull();
    expect(plain.container.querySelector(".permission-card-escape")).toBeNull();
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

/** An edit card is drawn for the risky edits (outside the project, protected paths), so the file it
 *  names and the change it shows get the same escapes as a command: a right-to-left override in a
 *  file name could otherwise make it read as a different file. */
describe("PermissionCard for an Edit with hidden characters", () => {
  it("shows them escaped in the file path and in the diff, and warns", () => {
    const { container } = render(
      <PermissionCard
        request={{
          ...REQUEST,
          toolName: "Edit",
          input: { file_path: "/p/a\u202Etxt.sh", old_string: "x\u200By", new_string: "z" },
        }}
        sessionEnded={false}
        onAnswer={vi.fn()}
      />,
    );
    const path = container.querySelector(".permission-card-edit-path")!;
    expect(path.textContent).toBe("/p/a⟨U+202E⟩txt.sh");
    expect(path.getAttribute("data-path")).toBe("/p/a\u202Etxt.sh");
    const diff = container.querySelector("pre.permission-card-diff")!;
    expect(diff.textContent).toContain("x⟨U+200B⟩y");
    expect(container.textContent).not.toContain("\u202E");
    expect(container.textContent).not.toContain("\u200B");
    expect(container.querySelectorAll(".permission-card-escape")).toHaveLength(2);
    expect(container.querySelector(".permission-card-warning")!.textContent).toContain("2 invisible or direction-changing");
  });
});

/** O3 ruling 6: the CLI's own prompt (it asked after the gate had answered) is drawn in the same
 *  card, showing the CLI's own sentence and a small label saying whose question it is -- Claude
 *  Code's safety check, or the user's own ask rule when one forced it -- and it answers through the
 *  same buttons, keys and permission id as any card. */
describe("PermissionCard for the CLI's own prompt", () => {
  const REASON = "Claude requested permissions to edit /p/.git/probe which is a sensitive file.";
  const prompt: PermissionRequestRecord = {
    ...REQUEST,
    permissionId: "perm-cli",
    toolName: "Write",
    input: { file_path: "/p/.git/probe", content: "o3\n" },
    providerPrompt: { reason: REASON, description: ".git/probe", blockedPath: null, matchedAskRule: null, unrecognizedOrigin: null },
  };

  it("shows the CLI's own sentence under a Claude Code asked label", () => {
    const { container } = render(<PermissionCard request={prompt} sessionEnded={false} onAnswer={vi.fn()} />);
    const line = container.querySelector(".permission-card-provider")!;
    expect(line).not.toBeNull();
    expect(line.querySelector(".permission-card-provider-label")!.textContent).toBe("Claude Code asked");
    expect(line.querySelector(".permission-card-provider-reason")!.textContent).toBe(REASON);
  });

  it("names the user's own ask rule instead when one forced the prompt", () => {
    const forced: PermissionRequestRecord = {
      ...prompt,
      toolName: "Bash",
      input: { command: "cat notes.txt" },
      providerPrompt: {
        reason: null,
        description: null,
        blockedPath: null,
        matchedAskRule: { source: "projectSettings", toolName: "Bash", ruleContent: "cat:*" },
        unrecognizedOrigin: null,
      },
    };
    const { container } = render(<PermissionCard request={forced} sessionEnded={false} onAnswer={vi.fn()} />);
    const label = container.querySelector(".permission-card-provider-label")!;
    expect(label.textContent).toBe("your ask rule: Bash(cat:*)");
    expect(label.getAttribute("title")).toContain("projectSettings");
    expect(container.textContent).not.toContain("Claude Code asked");
    expect(container.querySelector(".permission-card-provider-reason")).toBeNull();
  });

  /* A prompt that says nothing about why could be the user's own content-scoped ask rule, which the
     CLI does not name, so it says so; one of a kind this build does not know is just "asked". */
  it("calls a prompt with no real reason possibly the user's own rule, and an unknown kind neutrally", () => {
    const silent: PermissionRequestRecord = { ...prompt, providerPrompt: { ...prompt.providerPrompt!, reason: null } };
    const first = render(<PermissionCard request={silent} sessionEnded={false} onAnswer={vi.fn()} />);
    expect(first.container.querySelector(".permission-card-provider-label")!.textContent).toBe(
      "Claude Code asked (maybe your ask rule)",
    );
    expect(first.container.querySelector(".permission-card-provider-reason")).toBeNull();
    first.unmount();
    /* A blocked path is not a reason, and neither is an empty or blank one (Rust agrees, so the label
       a card shows and the one its row note carries never differ). */
    const withoutReason = (reason: string | null, blockedPath: string | null): PermissionRequestRecord => ({
      ...prompt,
      providerPrompt: { ...prompt.providerPrompt!, reason, blockedPath },
    });
    for (const [reason, blockedPath] of [[null, "/p/.git/probe"], ["", "/p/.git/probe"], ["  \n", null]] as const) {
      const view = render(<PermissionCard request={withoutReason(reason, blockedPath)} sessionEnded={false} onAnswer={vi.fn()} />);
      expect(view.container.querySelector(".permission-card-provider-label")!.textContent).toBe(
        "Claude Code asked (maybe your ask rule)",
      );
      view.unmount();
    }
    const unknown: PermissionRequestRecord = { ...prompt, providerPrompt: { ...prompt.providerPrompt!, unrecognizedOrigin: 7 } };
    const { container } = render(<PermissionCard request={unknown} sessionEnded={false} onAnswer={vi.fn()} />);
    expect(container.querySelector(".permission-card-provider-label")!.textContent).toBe("Claude Code asked");
    expect(container.querySelector(".permission-card-provider-reason")!.textContent).toBe(REASON);
  });

  it("answers through the same buttons and permission id as any card", () => {
    const onAnswer = vi.fn();
    const { container } = render(<PermissionCard request={prompt} sessionEnded={false} onAnswer={onAnswer} />);
    const approve = container.querySelector('button[data-nav-action="allow"]') as HTMLButtonElement;
    expect(approve.textContent).toBe("Approve");
    expect(container.querySelector('button[data-nav-action="deny"]')).not.toBeNull();
    fireEvent.click(approve);
    expect(onAnswer).toHaveBeenCalledWith("perm-cli", "allow", undefined);
  });

  it("draws nothing extra on the gate's own card", () => {
    const { container } = render(<PermissionCard request={REQUEST} sessionEnded={false} onAnswer={vi.fn()} />);
    expect(container.querySelector(".permission-card-provider")).toBeNull();
  });
});
