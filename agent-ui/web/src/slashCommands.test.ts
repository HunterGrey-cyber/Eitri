import { describe, expect, it } from "vitest";
import {
  heldBackSlashCommand,
  SLASH_COMMANDS,
  slashCommandFlashText,
  worksSlashCommands,
} from "./slashCommands";
import slashCommandsDoc from "../../../docs/2026-09-27-slash-commands.md?raw";

describe("SLASH_COMMANDS", () => {
  it("has exactly the fixed 28-command list, once each", () => {
    const names = SLASH_COMMANDS.map((e) => e.name);
    expect(new Set(names).size).toBe(names.length);
    expect(names).toEqual([
      "help",
      "clear",
      "compact",
      "cost",
      "context",
      "usage",
      "status",
      "model",
      "config",
      "memory",
      "init",
      "review",
      "security-review",
      "pr-comments",
      "todos",
      "export",
      "mcp",
      "agents",
      "hooks",
      "permissions",
      "add-dir",
      "resume",
      "rewind",
      "login",
      "logout",
      "doctor",
      "release-notes",
      "output-style",
    ]);
  });

  // Check each command's classification against its table cell to catch transcription errors.
  // Some cells have trailing annotations, so read the class from the leading words.
  it("classifies every command exactly as the doc's table row says", () => {
    for (const entry of SLASH_COMMANDS) {
      const rowRe = new RegExp(`\\| \`/${entry.name}\` \\| ([^|]+) \\|`);
      const m = rowRe.exec(slashCommandsDoc);
      expect(m, `no doc row found for /${entry.name}`).not.toBeNull();
      const cell = m![1].trim();
      const normalized = cell.startsWith("sent as text")
        ? "sent-as-text"
        : cell.startsWith("works")
          ? "works"
          : cell;
      expect(normalized, `/${entry.name}`).toBe(entry.class);
    }
  });
});

describe("heldBackSlashCommand", () => {
  it("holds back the three unconditional interactive-only commands", () => {
    expect(heldBackSlashCommand("/login")).toBe("login");
    expect(heldBackSlashCommand("/config")).toBe("config");
    expect(heldBackSlashCommand("/resume")).toBe("resume");
  });

  it("holds back /model without an argument, but not with one", () => {
    expect(heldBackSlashCommand("/model")).toBe("model");
    expect(heldBackSlashCommand("/model  ")).toBe("model");
    expect(heldBackSlashCommand("/model sonnet")).toBeNull();
  });

  it("sends a command classed works, e.g. /compact", () => {
    expect(heldBackSlashCommand("/compact")).toBeNull();
  });

  it("sends a command classed sent-as-text, e.g. /help", () => {
    expect(heldBackSlashCommand("/help")).toBeNull();
  });

  it("sends any unknown slash word", () => {
    expect(heldBackSlashCommand("/unknown-thing")).toBeNull();
  });

  it("leaves ordinary text (no leading slash) alone", () => {
    expect(heldBackSlashCommand("please add a test")).toBeNull();
    expect(heldBackSlashCommand("")).toBeNull();
  });

  it("reads the first word even with leading whitespace or a trailing argument", () => {
    expect(heldBackSlashCommand("  /login")).toBe("login");
    expect(heldBackSlashCommand("/login now please")).toBe("login");
  });
});

describe("slashCommandFlashText", () => {
  it("explains that /login is unavailable and points to ?", () => {
    expect(slashCommandFlashText("login")).toBe("/login does not work in neovibe — ? lists the ones that do");
  });
});

describe("worksSlashCommands (the ? overlay's list)", () => {
  it("lists exactly the works rows, in the table's own order, model and config included", () => {
    expect(worksSlashCommands()).toEqual([
      "clear",
      "compact",
      "cost",
      "context",
      "usage",
      "model",
      "config",
      "mcp",
      "agents",
      "doctor",
      "output-style",
    ]);
  });
});
