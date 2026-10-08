// @vitest-environment jsdom
/**
 * The cards of a Codex tab (and an ACP agent's): what each shows of the call it gates, that everything the agent
 * wrote goes through `revealHidden`, and that no reason box is offered for a deny. The inputs come from
 * `fixtures/codex-cards.json`, which Rust writes from the provider's own card constructors.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { PermissionCard } from "./PermissionCard";
import fixture from "../fixtures/codex-cards.json";
import type { PermissionRequestRecord } from "../types";

afterEach(cleanup);

/** The pending request of one fixture case, as the snapshot carries it. */
function requestOf(name: string): PermissionRequestRecord {
  const found = fixture.cases.find((c) => c.name === name)!;
  return found.snapshot.state.pendingPermissions[0] as unknown as PermissionRequestRecord;
}

function card(request: PermissionRequestRecord, onAnswer = vi.fn()) {
  return render(<PermissionCard request={request} sessionEnded={false} onAnswer={onAnswer} />);
}

describe("a Codex command card", () => {
  it("command card shows the wrapper and hidden characters revealed", () => {
    const { container } = card(requestOf("hidden"));
    const command = container.querySelector(".permission-card-command")!;
    // The shell wrapper is part of what runs, so it is shown as Codex will run it.
    expect(command.textContent).toContain("/bin/bash -lc 'echo ");
    // The right-to-left override is drawn as an escape, not applied.
    expect(command.textContent).toContain("⟨U+202E⟩dcba");
    expect(command.textContent).not.toContain("\u202e");
    expect(container.querySelector(".permission-card-warning")!.textContent).toContain("command holds 1 invisible");
    expect(container.querySelector(".permission-card-wording")!.textContent).toBe("runs outside the sandbox with your full rights");
  });

  it("shows the working directory and Codex's reason, labelled and muted", () => {
    const { container } = card(requestOf("command"));
    expect(container.textContent).toContain("/bin/bash -lc 'git push origin main'");
    expect(container.textContent).toContain("in /project");
    const says = container.querySelector(".permission-card-says")!;
    expect(says.textContent).toBe("Codex says: The push needs the network");
    expect(says.classList.contains("permission-card-description")).toBe(true);
  });

  it("network wording names the host", () => {
    const { container } = card(requestOf("network"));
    expect(container.querySelector(".permission-card-wording")!.textContent).toBe(
      "asks for network access to registry.example.com (https); may run outside the sandbox with your full rights",
    );
    // A network request names no command, so no command line is drawn.
    expect(container.querySelector(".permission-card-command")).toBeNull();
  });

  it("subagent card names the task", () => {
    const { container } = card(requestOf("subagent"));
    expect(container.querySelector(".permission-card-provider")!.textContent).toBe("subagent/root/tests");
    expect(container.textContent).toContain("cargo test");
  });

  it("a subagent name with a hidden character is revealed too", () => {
    const request = requestOf("subagent");
    const { container } = card({ ...request, input: { ...(request.input as object), subagent: "/root/te\u202est" } });
    expect(container.querySelector(".permission-card-provider")!.textContent).toContain("⟨U+202E⟩");
  });
});

describe("a Codex file change card", () => {
  it("file change card shows each diff", () => {
    const { container } = card(requestOf("file_change"));
    const heads = Array.from(container.querySelectorAll(".permission-card-edit-path")).map((el) => el.textContent);
    expect(heads).toEqual(["/project/new.txt", "/project/src/lib.rs"]);
    const diffs = Array.from(container.querySelectorAll(".permission-card-diff"));
    expect(diffs).toHaveLength(2);
    const lines = (el: Element) => Array.from(el.querySelectorAll(".diff-line")).map((l) => `${l.className.replace("diff-line diff-", "")}:${l.querySelector(".diff-text")!.textContent}`);
    expect(lines(diffs[0])).toEqual(["added:one", "added:two"]);
    expect(lines(diffs[1])).toEqual(["context:@@ -1,2 +1,2 @@", "context:fn a() {}", "removed:fn b() {}", "added:fn c() {}"]);
    // What the change does to the path beyond its lines, and why Codex says it asks.
    expect(container.textContent).toContain("Creates a new file.");
    expect(container.textContent).toContain("Moves it to /project/src/main.rs.");
    expect(container.querySelector(".permission-card-says")!.textContent).toBe("Codex says: Rename b to c");
  });

  it("a large change is shown whole, never as counts beside an Approve", () => {
    const request = requestOf("file_change");
    const input = request.input as { changes: Array<Record<string, unknown>> };
    const big = Array.from({ length: 2500 }, (_, i) => `line ${i}`).join("\n");
    const { container } = card({ ...request, input: { ...input, changes: [{ ...input.changes[0], kind: "add", diff: big }] } });
    expect(container.textContent).not.toContain("Too large to show here");
    const shown = Array.from(container.querySelectorAll(".permission-card-diff .diff-line"));
    expect(shown).toHaveLength(2500);
    expect(shown[2499].querySelector(".diff-text")!.textContent).toBe("line 2499");
  });

  it("a path with a hidden character is revealed", () => {
    const request = requestOf("file_change");
    const input = request.input as { changes: Array<Record<string, unknown>> };
    const { container } = card({ ...request, input: { ...input, changes: [{ ...input.changes[0], path: "/project/a\u202eb.txt" }] } });
    expect(container.querySelector(".permission-card-edit-path")!.textContent).toContain("⟨U+202E⟩");
  });
});

describe("a Codex tool call card", () => {
  it("shows the server, the tool and the arguments as JSON", () => {
    const { container } = card(requestOf("mcp"));
    expect(container.querySelector(".permission-card-wording")!.textContent).toBe("Tool lookup on server docs");
    expect(container.querySelector(".permission-card-input")!.textContent).toContain('"query": "tokio"');
  });
});

describe("an ACP agent's card", () => {
  const request: PermissionRequestRecord = {
    seq: 1,
    permissionId: "p",
    toolUseId: "c",
    toolName: "acp:execute",
    input: {
      title: "Run `rm -rf build`",
      rawInput: { command: "rm -rf build" },
      locations: [{ path: "/project/build" }],
      options: [
        { name: "Yes\u202e", kind: "allow_once" },
        { name: "Always", kind: "allow_always" },
        { name: "No, and tell the agent what to do differently", kind: "reject_once" },
      ],
    },
  };

  it("shows the title and the option names verbatim, revealed, and never an always option", () => {
    const { container } = card(request);
    expect(container.querySelector(".permission-card-wording")!.textContent).toBe("Run `rm -rf build`");
    const options = container.querySelector(".permission-card-options")!.textContent!;
    expect(options).toContain("Approve answers “Yes⟨U+202E⟩”");
    expect(options).toContain("Deny answers “No, and tell the agent what to do differently”");
    expect(options).not.toContain("Always");
  });

  it("says when the agent offered no option to answer with", () => {
    const { container } = card({ ...request, input: { title: "t", options: [{ name: "Always", kind: "allow_always" }] } });
    const options = container.querySelector(".permission-card-options")!.textContent!;
    expect(options).toContain("Approve answers nothing: the agent offered no allow-once option");
    expect(options).toContain("Deny answers nothing: the agent offered no reject-once option");
  });
});

describe("the reason box", () => {
  it("reason box absent and D flashes", () => {
    // The box is absent for every card of another agent; `D`'s flash is `App.codexCards.test.tsx`.
    for (const name of ["command", "file_change", "mcp"]) {
      const { container, unmount } = card(requestOf(name));
      expect(container.querySelector("input")).toBeNull();
      unmount();
    }
    const { container } = card({ seq: 1, permissionId: "p", toolUseId: "c", toolName: "acp:edit", input: {} });
    expect(container.querySelector("input")).toBeNull();
  });

  it("a Claude card keeps its reason box", () => {
    const { container } = card({ seq: 1, permissionId: "p", toolUseId: "c", toolName: "Bash", input: { command: "ls" } });
    expect(container.querySelector("input")).not.toBeNull();
  });

  it("deny answers with no reason", () => {
    const onAnswer = vi.fn();
    const { container } = card(requestOf("command"), onAnswer);
    const deny = Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "Deny")!;
    deny.click();
    expect(onAnswer).toHaveBeenCalledWith("perm-command", "deny", undefined);
  });
});
