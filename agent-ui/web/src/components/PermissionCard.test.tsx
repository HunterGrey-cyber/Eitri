// @vitest-environment jsdom
/// <reference types="vite/client" />
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, render, cleanup, fireEvent } from "@testing-library/react";

// See EmptyTab.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);
import { PermissionCard, PermissionCardContext } from "./PermissionCard";
import type { BackgroundTaskRecord, PermissionRequestRecord, ToolCallRecord } from "../types";
import { applyEvent, initialState } from "../reducer";
import type { AgentDomainEvent } from "../types";
import css from "../index.css?raw";

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
  /** Rust refused a click's answer (an "Always allow" whose rule could
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
  /** In a tooltip on the card's tool line, never as visible text. */
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

describe("the Bash card", () => {
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

describe("the reason box and the remember button", () => {
  it("repeated Enter in the reason box is prevented without denying", () => {
    const onAnswer = vi.fn();
    const { container } = render(<PermissionCard request={REQUEST} sessionEnded={false} onAnswer={onAnswer} />);
    const reason = container.querySelector<HTMLInputElement>("input")!;
    fireEvent.change(reason, { target: { value: "keep this reason" } });
    expect(fireEvent.keyDown(reason, { key: "Enter", repeat: true })).toBe(false);
    expect(onAnswer).not.toHaveBeenCalled();
    fireEvent.keyDown(reason, { key: "Enter" });
    expect(onAnswer).toHaveBeenCalledWith("perm-1", "deny", "keep this reason");
  });

  it("denies with the reason on Enter in the reason box", () => {
    const onAnswer = vi.fn();
    const { container } = render(<PermissionCard request={REQUEST} sessionEnded={false} onAnswer={onAnswer} />);
    const reason = container.querySelector<HTMLInputElement>("input")!;
    fireEvent.change(reason, { target: { value: "not the whole disk" } });
    fireEvent.keyDown(reason, { key: "Enter" });
    expect(onAnswer).toHaveBeenCalledWith("perm-1", "deny", "not the whole disk");
  });

  /** An input method's Enter commits a reason without denying. */
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

/** A Write over no file (Rust looked when the card was raised) says it creates one;
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

/** The CLI's own prompt (it asked after the gate had answered) is drawn in the same
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

describe("PermissionCard shows everything it asks to approve", () => {
  it("lists a Bash call's other keys after the command", () => {
    const { container } = render(
      <PermissionCard
        request={{ ...REQUEST, input: { command: "make", dangerouslyDisableSandbox: true, timeout: 5000 } }}
        sessionEnded={false}
        onAnswer={() => {}}
      />,
    );
    const extra = container.querySelector(".permission-card-extra")!;
    expect(extra.textContent).toContain('"dangerouslyDisableSandbox": true');
    expect(extra.textContent).toContain('"timeout": 5000');
    expect(container.querySelector(".permission-card-command")!.textContent).toBe("$ make");
  });

  it("lists a description that is not a string, since it is not drawn as one", () => {
    const { container } = render(
      <PermissionCard request={{ ...REQUEST, input: { command: "make", description: { x: 1 } } }} sessionEnded={false} onAnswer={() => {}} />,
    );
    expect(container.querySelector(".permission-card-extra")!.textContent).toContain('"description": {"x":1}');
  });

  it("quotes a key's name, so a newline in it cannot lay out as another key", () => {
    const { container } = render(
      <PermissionCard
        request={{ ...REQUEST, input: { command: "make", "x\ntimeout": 5, "y\u2028z": 1 } }}
        sessionEnded={false}
        onAnswer={() => {}}
      />,
    );
    const extra = container.querySelector(".permission-card-extra")!;
    // Two keys, two lines: the newline inside the first key is drawn as JSON's escape.
    expect(extra.textContent!.split("\n")).toEqual(['"x\\ntimeout": 5', '"y⟨U+2028⟩z": 1']);
  });

  it("draws a NotebookEdit delete's cell id through the hidden-character escapes", () => {
    const { container } = render(
      <PermissionCard
        request={{ ...REQUEST, toolName: "NotebookEdit", input: { notebook_path: "/p/a.ipynb", cell_id: "c\u202e1", edit_mode: "delete" } }}
        sessionEnded={false}
        onAnswer={() => {}}
      />,
    );
    const note = container.querySelector(".permission-card-edit-note")!;
    expect(note.textContent).toBe("Deletes cell c⟨U+202E⟩1. The request carries no record of what it held.");
    expect(container.querySelector(".permission-card-warning")!.textContent).toContain("change holds 1 invisible");
  });

  it("lists nothing extra for a plain command and description", () => {
    const { container } = render(
      <PermissionCard
        request={{ ...REQUEST, input: { command: "make", description: "Build" } }}
        sessionEnded={false}
        onAnswer={() => {}}
      />,
    );
    expect(container.querySelector(".permission-card-extra")).toBeNull();
  });

  it("escapes hidden characters in the extra keys, the description, the tool name and the rule offer", () => {
    const { container } = render(
      <PermissionCard
        request={{
          ...REQUEST,
          input: { command: "make", description: "ok\u202E", "k\u200B": "v\u00A0" },
        }}
        ruleOffer={"git\u00A0status *"}
        sessionEnded={false}
        onAnswer={() => {}}
      />,
    );
    const other = render(
      <PermissionCard request={{ ...REQUEST, toolName: "mcp__a\u200Bb", input: {} }} sessionEnded={false} onAnswer={() => {}} />,
    );
    expect(other.container.querySelector(".permission-card-tool")!.textContent).toContain("⟨U+200B⟩");
    for (const sel of [".permission-card-extra", ".permission-card-description", "[data-nav-action=always]"]) {
      const el = container.querySelector(sel)!;
      expect(el.textContent, sel).toMatch(/⟨U\+[0-9A-F]{4}⟩/);
      expect(el.textContent, sel).not.toMatch(/[\u200B\u202E\u00A0]/);
    }
  });

  it("wraps what a person approves instead of scrolling it out of sight", () => {
    expect(css).toMatch(/\.permission-card-input\s*\{[^}]*white-space:\s*pre-wrap[^}]*overflow-wrap:\s*anywhere/);
    expect(css).toMatch(/\.permission-card \.diff-line\s*\{[^}]*white-space:\s*pre-wrap/);
    expect(css).toMatch(/\.permission-card \.diff-text\s*\{[^}]*min-width:\s*0/);
  });
});

it("names the key that answers it on Approve and Deny, as data the stylesheet draws", () => {
  const { container } = render(
    <PermissionCard request={REQUEST} sessionEnded={false} onAnswer={() => {}} />,
  );
  expect(container.querySelector('[data-nav-action="allow"]')!.getAttribute("data-key")).toBe("a");
  expect(container.querySelector('[data-nav-action="deny"]')!.getAttribute("data-key")).toBe("d");
  expect(container.querySelector('[data-nav-action="allow"]')!.textContent).toBe("Approve");
});

it("shows the last line of a Write above the diff cap", () => {
  const content = [...Array.from({ length: 400 }, (_, i) => `line ${i}`), "rm -rf important"].join("\n");
  const { container } = render(<PermissionCard request={{ ...REQUEST, toolName: "Write", input: { file_path: "/p/run.sh", content } }} sessionEnded={false} onAnswer={vi.fn()} />);
  expect(container.textContent).toContain("rm -rf important");
  expect(container.textContent).not.toContain("Too large to show here");
  expect(container.querySelectorAll(".diff-added")).toHaveLength(401);
});

it("shows every removed and added line of a large Edit", () => {
  const oldLines = Array.from({ length: 401 }, (_, i) => `old ${i}`);
  const newLines = Array.from({ length: 402 }, (_, i) => `new ${i}`);
  const { container } = render(<PermissionCard request={{ ...REQUEST, toolName: "Edit", input: { file_path: "/p/a", old_string: oldLines.join("\n"), new_string: newLines.join("\n") } }} sessionEnded={false} onAnswer={vi.fn()} />);
  expect(Array.from(container.querySelectorAll(".diff-removed .diff-text"), (line) => line.textContent)).toEqual(oldLines);
  expect(Array.from(container.querySelectorAll(".diff-added .diff-text"), (line) => line.textContent)).toEqual(newLines);
  expect(container.textContent).not.toContain("Too large to show here");
});


describe("Workflow permission view", () => {
  function workflow(input: unknown) {
    const onAnswer = vi.fn(), onViewScript = vi.fn();
    const view = render(<PermissionCardContext.Provider value={{ state: initialState(), onViewScript }}>
      <PermissionCard request={{ ...REQUEST, toolName: "Workflow", input }} sessionEnded={false} onAnswer={onAnswer} />
    </PermissionCardContext.Provider>);
    return { ...view, onAnswer, onViewScript };
  }

  it("draws the inline script as escaped monospace text without interpreting tags or phases", () => {
    const script = '<script>alert(1)</script>\n// phases: <b>prepare</b>\u0007\u202e\u200b';
    const view = workflow({ script, args: "<i>argument</i>\u202e" });
    expect(view.getByText("Run a dynamic workflow?")).toBeTruthy();
    expect(view.container.querySelector("pre.permission-card-script")!.textContent)
      .toContain('<script>alert(1)</script>\n// phases: <b>prepare</b>⟨U+0007⟩⟨U+202E⟩⟨U+200B⟩');
    expect(view.container.querySelector(".permission-card-input")!.textContent).toContain("<i>argument</i>⟨U+202E⟩");
    expect(view.container.querySelector("script, b, i, ol, ul")).toBeNull();
    fireEvent.click(view.getByRole("button", { name: "View raw script" }));
    expect(view.onViewScript).toHaveBeenCalledWith(script);
    expect(view.onAnswer).not.toHaveBeenCalled();
  });

  it.each([
    { name: "Named\u202e", args: ["one", "<b>two</b>"] },
    { scriptPath: "/work/flow\u200b.js", args: { target: "<img src=x>" } },
  ])("shows names, script paths and structured arguments as plain text: %j", (input) => {
    const view = workflow(input);
    expect(view.container.textContent).toContain("Arguments");
    expect(view.container.textContent).toContain("⟨U+");
    expect(view.container.textContent).toContain(JSON.stringify(input.args, null, 2));
    expect(view.container.querySelector("b, img")).toBeNull();
    expect(view.queryByRole("button", { name: "View raw script" })).toBeNull();
    expect(view.queryByText(/don't ask again|Always allow/i)).toBeNull();
    expect(view.container.querySelector('[data-nav-action="allow"]')!.getAttribute("data-key")).toBe("a");
    expect(view.container.querySelector('[data-nav-action="deny"]')!.getAttribute("data-key")).toBe("d");
    fireEvent.click(view.getByRole("button", { name: "Approve" }));
    expect(view.onAnswer).toHaveBeenCalledTimes(1);
    expect(view.onAnswer).toHaveBeenCalledWith("perm-1", "allow", undefined);
  });

  it.each(["resolvedScript", "resolved_script"])("shows and opens the supplied %s in preference to the original", (key) => {
    const view = workflow({ script: "original", [key]: "resolved\u202e" });
    expect(view.container.querySelector(".permission-card-script")!.textContent).toContain("resolved⟨U+202E⟩");
    fireEvent.click(view.getByRole("button", { name: "View raw script" }));
    expect(view.onViewScript).toHaveBeenCalledWith("resolved\u202e");
  });

  it("keeps unrecognized input fields visible and keeps denial's reason", () => {
    const view = workflow({ script: "run()", option: "<b>flag</b>\u202e" });
    expect(view.container.textContent).toContain('"option": "<b>flag</b>⟨U+202E⟩"');
    fireEvent.change(view.container.querySelector("input")!, { target: { value: "inspect first" } });
    fireEvent.click(view.getByRole("button", { name: "Deny" }));
    expect(view.onAnswer).toHaveBeenCalledWith("perm-1", "deny", "inspect first");
  });

  it("uses the read-only editor route and returns to the waiting card without editing or answering", async () => {
    const { default: App } = await import("../App");
    Element.prototype.scrollIntoView = vi.fn();
    const posted: Record<string, unknown>[] = [];
    const priorWebkit = window.webkit;
    window.webkit = { messageHandlers: { eitriAgent: { postMessage: (msg: string) => posted.push(JSON.parse(msg)) } } };
    try {
      const script = "// vim: set modifiable:\n<script>raw</script>\u202e";
      const request = { ...REQUEST, toolName: "Workflow", input: { script } };
      const view = render(<App />);
      const dispatch = (payload: unknown) => act(() => window.__eitriDispatch!(JSON.stringify(payload)));
      dispatch({ kind: "hello", backend: "legacy", projectDir: "/project", permissionModes: ["manual", "auto", "bypass"],
        resumableSessions: [], account: null });
      dispatch({ kind: "tabs", active: 1, tabs: [{ id: 1, number: 1, label: "1 claude", name: null,
        state: "live", mode: "manual", marker: null, pending: 1, resumable: false, failure: null, title: null }] });
      dispatch({ kind: "snapshot", tab: 1, throughRevision: 1,
        state: { ...initialState(), status: { kind: "running" }, pendingPermissions: [request] } });
      fireEvent.click(view.getByRole("button", { name: "View raw script" }));
      const scratch = posted.find((message) => message.type === "view_in_editor");
      expect(scratch).toEqual({ type: "view_in_editor", request_id: expect.any(String), title: "Workflow script", text: script });
      dispatch({ kind: "command_result", requestId: scratch!.request_id, ok: true });
      dispatch({ kind: "arrive" });
      expect(view.getByText("Run a dynamic workflow?")).toBeTruthy();
      expect(buttons(view.container).approve.disabled).toBe(false);
      expect(posted.some((message) => ["edit_draft", "permission_response", "send_message", "open_path"].includes(String(message.type)))).toBe(false);
    } finally {
      window.webkit = priorWebkit;
    }
  });
});

describe("the agent asking a permission card", () => {
  const child: ToolCallRecord = { seq: 2, toolUseId: REQUEST.toolUseId!, name: "Bash", input: REQUEST.input, result: null };
  const children = (calls: ToolCallRecord[]) => ({ transcript: [], thinking: [], toolCalls: calls,
    assistantMessageOpen: false, thinkingOpen: false });
  const parent = (name: string, input: unknown): ToolCallRecord => ({ seq: 1, toolUseId: "parent", name, input,
    result: null, children: children([child]) });
  function card(calls: ToolCallRecord[], workflows: Partial<BackgroundTaskRecord>[] = []) {
    const current = initialState();
    current.toolCalls = calls;
    current.backgroundTasks.rows = workflows.map((task, i) => ({ task_id: String(i), kind: "workflow", live: true,
      status: "running", ...task } as BackgroundTaskRecord));
    return render(<PermissionCardContext.Provider value={{ state: current, onViewScript: vi.fn() }}>
      <PermissionCard request={REQUEST} sessionEnded={false} onAnswer={vi.fn()} />
    </PermissionCardContext.Provider>);
  }

  it.each(["Agent", "Task"])("names a known %s parent in the main column with hidden characters revealed", (name) => {
    const view = card([parent(name, { description: "Review <b>files</b>\u202e" })]);
    expect(view.container.querySelector(".permission-card-provider")!.textContent).toBe("asked by Review <b>files</b>⟨U+202E⟩");
    expect(view.container.querySelector("b")).toBeNull();
  });

  it("follows nested parent chains to the nearest known agent description", () => {
    const inner = parent("Task", { description: "Inner" });
    const outer = { ...parent("Agent", { description: "Outer" }), children: children([inner]) };
    const view = card([outer]);
    expect(view.getByText("asked by Inner")).toBeTruthy();
  });

  it("traces through a placeholder to a known ancestor", () => {
    const unknown = parent("background agent", {});
    const outer = { ...parent("Agent", { description: "Known" }), children: children([unknown]) };
    expect(card([outer]).getByText("asked by Known")).toBeTruthy();
  });

  it.each([
    [[], "asked by a background agent"],
    [[{ workflow_name: "Flow <b>one</b>\u202e" }], "asked by a background agent (workflow Flow <b>one</b>⟨U+202E⟩ running)"],
    [[{ workflow_name: "One" }, { workflow_name: "Two" }], "asked by a background agent"],
    [[{ workflow_name: "Ended", live: false, status: "completed" }], "asked by a background agent"],
    [[{ workflow_name: "Ended", status: "failed" }, { workflow_name: "Live" }], "asked by a background agent (workflow Live running)"],
  ] as [Partial<BackgroundTaskRecord>[], string][])("labels unknown ancestry with the live workflow count: %j", (workflows, label) => {
    const view = card([parent("background agent", {})], workflows);
    expect(view.container.querySelector(".permission-card-provider")!.textContent).toBe(label);
    expect(view.container.querySelector("b")).toBeNull();
  });

  it("does not infer a child from an unrelated parent or a live workflow", () => {
    const view = card([child, parent("Agent", { description: "Other" })], [{ workflow_name: "Flow" }]);
    expect(view.container.querySelector(".permission-card-provider")).toBeNull();
  });
});

describe("the agent label at the moment a recorded card is drawn", () => {
  const twins = import.meta.glob<{ domain_events: AgentDomainEvent[] }>("../fixtures/claude-scripted-*.json",
    { eager: true, import: "default" });
  const events = (scenario: string) => twins[`../fixtures/claude-scripted-${scenario}.json`].domain_events;

  /** The card for `toolUseId` as the page draws it: the state folded through the event that raised it. */
  function cardAt(scenario: string, toolUseId: string) {
    const all = events(scenario);
    const at = all.findIndex((e) => e.type === "permission_requested" && e.tool_use_id === toolUseId);
    expect(at, `${toolUseId} is raised in ${scenario}`).toBeGreaterThanOrEqual(0);
    const state = all.slice(0, at + 1).reduce(applyEvent, initialState());
    const request = state.pendingPermissions.find((p) => p.toolUseId === toolUseId)!;
    return render(<PermissionCardContext.Provider value={{ state, onViewScript: vi.fn() }}>
      <PermissionCard request={request} sessionEnded={false} onAnswer={vi.fn()} />
    </PermissionCardContext.Provider>);
  }

  // The CLI raises a child's card before the parent call or any of the child's frames reach the page, so
  // nothing links the card to an agent yet and none is named.
  it.each(["toolu_sb_read", "toolu_sb_bash"])("draws a foreground subagent's card %s with no agent label", (id) => {
    expect(cardAt("c03_fg", id).container.querySelector(".permission-card-provider")).toBeNull();
  });

  it.each(["toolu_wa", "toolu_wb"])("draws a workflow agent's card %s with no agent label", (id) => {
    expect(cardAt("c16w", id).container.querySelector(".permission-card-provider")).toBeNull();
  });
});

describe("the Workflow card's script", () => {
  it("draws only the resolved script when the request carries one", () => {
    const request: PermissionRequestRecord = { ...REQUEST, toolName: "Workflow",
      input: { script: "ORIGINAL-SOURCE", resolvedScript: "RESOLVED-SOURCE", args: { a: 1 } } };
    const view = render(<PermissionCard request={request} sessionEnded={false} onAnswer={vi.fn()} />);
    expect(view.container.textContent).toContain("RESOLVED-SOURCE");
    expect(view.container.textContent).not.toContain("ORIGINAL-SOURCE");
  });
});

/* A card Eitri's own tool broker raised: its title and lines are plain text from a repository or the agent, each
   drawn through `revealHidden`; it takes no reason and offers no rule. */
describe("a broker card", () => {
  const BROKER: PermissionRequestRecord = {
    seq: 4,
    permissionId: "ec-0123456789abcdef-1",
    toolUseId: "toolu_01",
    toolName: "open_file",
    input: null,
    editorCard: { title: "Show /etc/hosts in your editor?", body: [] },
  };
  const withBody = (title: string, body: NonNullable<PermissionRequestRecord["editorCard"]>["body"]): PermissionRequestRecord => ({
    ...BROKER,
    editorCard: { title, body },
  });

  it("hidden_characters_in_a_card_are_revealed", () => {
    const { container } = render(
      <PermissionCard
        request={withBody("Show a\u202Etxt.exe in your editor?", [
          { kind: "code", text: "/tmp/a\u202Etxt.exe" },
          { kind: "plain", text: "from\u200Bhere" },
          { kind: "agent", text: "open it\u2066" },
          { kind: "warning", text: "outside\u202D" },
        ])}
        sessionEnded={false}
        onAnswer={vi.fn()}
      />,
    );
    const text = container.textContent ?? "";
    for (const hidden of ["\u202E", "\u200B", "\u2066", "\u202D"]) expect(text).not.toContain(hidden);
    expect(container.querySelector(".permission-card-tool")!.textContent).toBe("Show a⟨U+202E⟩txt.exe in your editor?");
    expect(container.querySelector('[data-line-kind="code"]')!.textContent).toBe("/tmp/a⟨U+202E⟩txt.exe");
    // Title (1) and four lines (1 each) all show their characters, and each line says so.
    expect(container.querySelectorAll(".permission-card-escape")).toHaveLength(5);
    expect(container.querySelectorAll(".permission-card-warning")).toHaveLength(4);
  });

  it("a_title_keeps_every_space_and_lays_out_left_to_right", () => {
    const { container } = render(<PermissionCard request={withBody("Load src/a  b.rs in your editor?", [])} sessionEnded={false} onAnswer={vi.fn()} />);
    const title = container.querySelector(".permission-card-tool")!;
    expect(title.textContent).toBe("Load src/a  b.rs in your editor?");
    expect(title.classList.contains("permission-card-tool-broker")).toBe(true);
  });

  it("markdown_in_a_card_stays_text", () => {
    const { container } = render(
      <PermissionCard
        request={withBody("**Open** `a.rs`?", [
          { kind: "plain", text: "# heading\n- item [link](https://example.com) <b>bold</b>" },
          { kind: "code", text: "`rm -rf`" },
          { kind: "agent", text: "*I* need `this`" },
        ])}
        sessionEnded={false}
        onAnswer={vi.fn()}
      />,
    );
    expect(container.querySelector(".permission-card-tool")!.textContent).toBe("**Open** `a.rs`?");
    expect(container.querySelector('[data-line-kind="plain"]')!.textContent).toBe("# heading\n- item [link](https://example.com) <b>bold</b>");
    expect(container.querySelector('[data-line-kind="code"]')!.textContent).toBe("`rm -rf`");
    expect(container.querySelector('[data-line-kind="agent"]')!.textContent).toBe("The agent says: *I* need `this`");
    // No element a Markdown or HTML renderer would make.
    for (const tag of ["a", "b", "em", "strong", "h1", "ul", "li", "code"]) expect(container.querySelector(tag), tag).toBeNull();
    // Monospace for code, a preformatted block.
    expect(container.querySelector('pre[data-line-kind="code"]')).not.toBeNull();
  });

  it("an_editor_card_has_no_reason_box_and_no_always", () => {
    const onAnswer = vi.fn();
    const { container } = render(
      <PermissionCard
        request={withBody("Show a in your editor?", [{ kind: "plain", text: "x" }])}
        sessionEnded={false}
        ruleOffer="open_file *"
        onAnswer={onAnswer}
      />,
    );
    expect(container.querySelector(".permission-card input")).toBeNull();
    expect(Array.from(container.querySelectorAll("button")).map((b) => b.textContent)).toEqual(["Approve", "Deny"]);
    expect(container.textContent).not.toContain("Always allow");
    fireEvent.click(buttons(container).approve);
    expect(onAnswer).toHaveBeenCalledWith("ec-0123456789abcdef-1", "allow", undefined);
  });
});
