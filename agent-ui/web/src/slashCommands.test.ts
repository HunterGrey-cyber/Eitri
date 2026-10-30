import { describe, expect, it } from "vitest";
import {
  slashAction,
  barePickerCommand,
  builtInCompletions,
  cleanCommands,
  completionSource,
  heldBackSlashCommand,
  matchCompletions,
  MAX_COMPLETIONS,
  MAX_COMPLETION_HINT,
  MAX_COMPLETION_NAME,
  MAX_COMPLETION_TEXT,
  menuToken,
  startsMatch,
  SLASH_MENU_ROWS,
  slashMenuWindow,
  SLASH_COMMANDS,
  slashCommandFlashText,
  listedSlashCommands,
  worksSlashCommands,
} from "./slashCommands";
// The design doc is not in the public tree, so it is looked up by glob: an absent file gives an empty
// map (no import error) and the row-by-row check below is skipped.
const slashCommandsDoc = Object.values(
  import.meta.glob("../../../docs/canonical/2026-09-27-slash-commands.md", {
    query: "?raw",
    import: "default",
    eager: true,
  }),
)[0] as string | undefined;

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

  // Cross-checks the table against the doc's own classification row by row, so a hand-transcription
  // slip fails here rather than only being caught by someone re-reading both files side by side.
  // Four rows (`/model`, `/config`, `/resume`, `/login`) carry an extra "— and are held
  // back regardless (named ...)" annotation after the class word in the doc's own cell; only the
  // leading word(s) are the class, so this matches the whole cell and reads its prefix.
  it.skipIf(!slashCommandsDoc)("classifies every command exactly as the doc's table row says", () => {
    for (const entry of SLASH_COMMANDS) {
      const rowRe = new RegExp(`\\| \`/${entry.name}\` \\| ([^|]+) \\|`);
      const m = rowRe.exec(slashCommandsDoc!);
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
    expect(heldBackSlashCommand("/resume yesterday")).toBe("resume");
  });

  /* A bare /model now sends -- its reply is real, parseable text
   *  headless, and the panel opens a picker from it instead of refusing to send at all. */
  it("no longer holds back a bare /model -- it sends, same as /model <name>", () => {
    expect(heldBackSlashCommand("/model")).toBeNull();
    expect(heldBackSlashCommand("/model  ")).toBeNull();
    expect(heldBackSlashCommand("/model sonnet")).toBeNull();
  });

  it("never held back /effort, bare or with an argument -- unknown to SLASH_COMMANDS either way", () => {
    expect(heldBackSlashCommand("/effort")).toBeNull();
    expect(heldBackSlashCommand("/effort low")).toBeNull();
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
  it("matches the documented wording", () => {
    expect(slashCommandFlashText("login")).toBe("/login does not work in Eitri — ? lists the ones that do");
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

/* The overlay must list only what Enter sends, so its own discovery hint cannot recommend a refused command. */
describe("listedSlashCommands (the ? overlay's list, what Enter would send)", () => {
  /* A bare /model sends and opens a picker, so list it plainly like every other sendable row. */
  it("leaves out /config and lists /model plainly", () => {
    expect(listedSlashCommands()).toEqual([
      "clear",
      "compact",
      "cost",
      "context",
      "usage",
      "model",
      "mcp",
      "agents",
      "doctor",
      "output-style",
    ]);
  });

  it("names nothing Enter holds back", () => {
    for (const shown of listedSlashCommands()) {
      expect(heldBackSlashCommand(`/${shown}`), shown).toBeNull();
    }
  });
});

describe("barePickerCommand", () => {
  it("names a bare /model or /effort, trailing whitespace included", () => {
    expect(barePickerCommand("/model")).toBe("model");
    expect(barePickerCommand("/model  ")).toBe("model");
    expect(barePickerCommand("  /effort")).toBe("effort");
  });

  it("is null the moment an argument is present", () => {
    expect(barePickerCommand("/model sonnet")).toBeNull();
    expect(barePickerCommand("/effort low")).toBeNull();
  });

  it("is null for any other command, known or not, and for plain text", () => {
    expect(barePickerCommand("/compact")).toBeNull();
    expect(barePickerCommand("/unknown-thing")).toBeNull();
    expect(barePickerCommand("please add a test")).toBeNull();
    expect(barePickerCommand("")).toBeNull();
  });
});


describe("the `/` menu's rows", () => {
  it("lists the commands that do something here, in the table's order, with /effort after /model", () => {
    const names = builtInCompletions().map((c) => c.name);
    expect(names).toEqual([
      "help",
      "clear",
      "compact",
      "cost",
      "context",
      "usage",
      "status",
      "model",
      "effort",
      "memory",
      "init",
      "review",
      "security-review",
      "mcp",
      "agents",
      "resume",
      "rewind",
      "doctor",
      "output-style",
      "tasks",
      "bashes",
      "workflows",
    ]);
  });

  it("leaves out every command Enter would refuse, and every sent-as-text row that only answers 'not available'", () => {
    const names = new Set(builtInCompletions().map((c) => c.name));
    for (const name of names) expect(heldBackSlashCommand(`/${name}`), name).toBeNull();
    for (const refused of ["config", "login"]) expect(names.has(refused), refused).toBe(false);
    for (const useless of ["export", "hooks", "permissions", "add-dir", "logout", "release-notes", "pr-comments", "todos"]) {
      expect(names.has(useless), useless).toBe(false);
    }
  });

  it("describes every row in one plain line", () => {
    for (const row of builtInCompletions()) {
      expect(row.description, row.name).not.toBe("");
      expect(row.description, row.name).not.toMatch(/[\n<>]/);
    }
  });
});

describe("matching what is typed after the slash", () => {
  const rows = ["compact", "cost", "context", "recompute", "co", "clear"].map((name) => ({ name, description: "" }));
  const names = (query: string) => matchCompletions(rows, query).map((c) => c.name);

  it("puts an equal name first, then prefixes, then names that merely contain it, each in the source's order", () => {
    expect(names("co")).toEqual(["co", "compact", "cost", "context", "recompute"]);
    expect(names("CO")).toEqual(names("co"));
    expect(names("cl")).toEqual(["clear"]);
  });

  it("lists everything for a bare slash, and nothing when nothing matches", () => {
    expect(names("")).toHaveLength(rows.length);
    expect(names("zzz")).toEqual([]);
  });
});

describe("which part of the draft the menu is about", () => {
  it("is the first word, while the draft is one line and the caret is inside it", () => {
    expect(menuToken("/", 1)).toEqual({ query: "", end: 1 });
    expect(menuToken("/co", 3)).toEqual({ query: "co", end: 3 });
    expect(menuToken("/co", 2)).toEqual({ query: "co", end: 3 });
    expect(menuToken("/compact now", 4)).toEqual({ query: "compact", end: 8 });
  });
  it("is nothing once a space is typed, the caret leaves the word, there is a second line, or no slash", () => {
    expect(menuToken("/co ", 4)).toBeNull();
    expect(menuToken("/compact now", 10)).toBeNull();
    expect(menuToken("/co", 0)).toBeNull();
    expect(menuToken("/co\nmore", 3)).toBeNull();
    expect(menuToken("hello /co", 9)).toBeNull();
    expect(menuToken("", 0)).toBeNull();
    expect(menuToken(" /co", 4)).toBeNull();
  });
});

describe("the window of rows drawn", () => {
  it("keeps the selected row inside eight rows, moving the least, and never past the last full window", () => {
    expect(SLASH_MENU_ROWS).toBe(8);
    expect(slashMenuWindow(0, 0, 20)).toBe(0);
    expect(slashMenuWindow(7, 0, 20)).toBe(0);
    expect(slashMenuWindow(8, 0, 20)).toBe(1);
    expect(slashMenuWindow(19, 1, 20)).toBe(12);
    expect(slashMenuWindow(3, 12, 20)).toBe(3);
    expect(slashMenuWindow(0, 5, 5)).toBe(0);
    expect(slashMenuWindow(2, 9, 9)).toBe(1);
  });
});

describe("where a tab's menu rows come from", () => {
  const reported = [{ name: "web", description: "Search" }];
  it("is the agent's own list whenever it reported one, Claude's included", () => {
    expect(completionSource("acp:demo", reported)).toBe(reported);
    expect(completionSource("claude", reported)).toEqual(reported);
    expect(completionSource(undefined, reported)).toEqual(reported);
  });
  it("holds back, on a Claude tab, the reported commands Enter refuses there; another agent's list is whole", () => {
    const list = [
      { name: "my-skill", description: "A skill" },
      { name: "config", description: "Open settings" },
      { name: "login", description: "Sign in" },
    ];
    expect(completionSource("claude", list)?.map((c) => c.name)).toEqual(["my-skill"]);
    expect(completionSource(undefined, list)?.map((c) => c.name)).toEqual(["my-skill"]);
    expect(completionSource("acp:demo", list)).toBe(list);
  });
  it("is Claude's built-ins for a Claude tab with no list, and no menu at all for another agent", () => {
    expect(completionSource("claude", null)).toEqual(builtInCompletions());
    expect(completionSource(undefined, null)).toEqual(builtInCompletions());
    expect(completionSource("codex", null)).toBeNull();
    expect(completionSource("acp:demo", null)).toBeNull();
  });
});

describe("an agent's list as the page keeps it", () => {
  it("is not a list unless it is an array", () => {
    expect(cleanCommands(null)).toBeNull();
    expect(cleanCommands(undefined)).toBeNull();
    expect(cleanCommands("web")).toBeNull();
    expect(cleanCommands({ name: "web" })).toBeNull();
  });
  it("drops entries without a usable name and repeated names, tolerating one leading slash", () => {
    const out = cleanCommands([
      { name: "web", description: "Search" },
      { name: "/test", description: "Run" },
      { name: "web", description: "again" },
      { name: "two words" },
      { name: "tab\tname" },
      { name: "" },
      { name: 3 },
      { description: "nameless" },
      "bare string",
      null,
      { name: "x".repeat(MAX_COMPLETION_NAME + 1) },
    ]);
    expect(out?.map((c) => c.name)).toEqual(["web", "test"]);
  });
  it("folds a description to one line and cuts it, and keeps a hint only when it says something", () => {
    const out = cleanCommands([
      { name: "a", description: "one\ntwo\u0007   three", input: undefined, hint: "  " },
      { name: "b", description: "d".repeat(MAX_COMPLETION_TEXT + 40), hint: "query" },
      { name: "c" },
    ])!;
    expect(out[0]).toEqual({ name: "a", description: "one two three" });
    expect(Array.from(out[1].description)).toHaveLength(MAX_COMPLETION_TEXT);
    expect(out[1].description.endsWith("…")).toBe(true);
    expect(out[1].hint).toBe("query");
    expect(out[2]).toEqual({ name: "c", description: "" });
  });
  it("drops a name holding a format, bidi, zero-width, tag or blank character, as the Rust side does", () => {
    // The same characters as `HIDDEN_NAME_CHARS` in agent/src/providers/acp/wire.rs.
    const hidden = [
      "\u202E", "\u200B", "\u2066", "\u2067", "\u2068", "\u2069", "\u00AD", "\uFEFF",
      "\u{E0041}", "\u200D", "\u2060", "\u061C", "\u3164", "\uFE0F", "\u00A0",
    ];
    for (const c of hidden) {
      const out = cleanCommands([{ name: `te${c}st` }, { name: `${c}lead` }, { name: "ok" }]);
      expect(out?.map((e) => e.name), `U+${c.codePointAt(0)!.toString(16)}`).toEqual(["ok"]);
    }
    // Letters of a right-to-left script are ordinary text.
    expect(cleanCommands([{ name: "שלום" }])?.map((e) => e.name)).toEqual(["שלום"]);
  });
  it("strips one leading slash and drops a name that still starts with one, as the Rust side does", () => {
    expect(cleanCommands([{ name: "//x" }, { name: "/y" }, { name: "/" }])?.map((e) => e.name)).toEqual(["y"]);
  });
  it("cuts a hint at MAX_COMPLETION_HINT, shorter than a description", () => {
    const out = cleanCommands([{ name: "a", description: "d".repeat(100), hint: "h".repeat(100) }])!;
    expect(Array.from(out[0].hint!)).toHaveLength(MAX_COMPLETION_HINT);
    expect(out[0].hint!.endsWith("…")).toBe(true);
    expect(Array.from(out[0].description)).toHaveLength(100);
  });
  it("keeps at most MAX_COMPLETIONS", () => {
    const many = Array.from({ length: MAX_COMPLETIONS + 30 }, (_, i) => ({ name: `c${i}`, description: "" }));
    expect(cleanCommands(many)).toHaveLength(MAX_COMPLETIONS);
  });
});

describe("matchCompletions tiers", () => {
  it("lists exact, then prefix, then word-start (separators ignored), then contained names", () => {
    const list = ["xreview", "security-review", "review-all", "review", "adddir-x", "add-dir"].map((name) => ({
      name,
      description: "",
    }));
    expect(matchCompletions(list, "review").map((c) => c.name)).toEqual(["review", "review-all", "security-review", "xreview"]);
    expect(matchCompletions(list, "adddir").map((c) => c.name)).toEqual(["adddir-x", "add-dir"]);
  });
});

describe("startsMatch (Enter's rule)", () => {
  const cmd = (name: string) => ({ name, description: "" });
  it("matches from the start of the name, or of a word within it, ignoring the separators", () => {
    expect(startsMatch(cmd("review"), "rev")).toBe(true);
    expect(startsMatch(cmd("REVIEW"), "Rev")).toBe(true);
    expect(startsMatch(cmd("security-review"), "rev")).toBe(true);
    expect(startsMatch(cmd("security-review"), "sec")).toBe(true);
    expect(startsMatch(cmd("add-dir"), "adddir")).toBe(true);
    expect(startsMatch(cmd("output_style"), "style")).toBe(true);
    expect(startsMatch(cmd("plugin:skill"), "skill")).toBe(true);
  });
  it("does not match letters found only inside a word, or an empty query", () => {
    expect(startsMatch(cmd("review"), "view")).toBe(false);
    expect(startsMatch(cmd("doctor"), "oct")).toBe(false);
    expect(startsMatch(cmd("agents"), "gen")).toBe(false);
    expect(startsMatch(cmd("init"), "it")).toBe(false);
    expect(startsMatch(cmd("export"), "xp")).toBe(false);
    expect(startsMatch(cmd("review"), "")).toBe(false);
    expect(startsMatch(cmd("review"), "-")).toBe(false);
  });
});

describe("the built-in rows carry the reference's wording", () => {
  it("/model says it also saves the choice, and /compact and /model show their argument hints", () => {
    const rows = builtInCompletions();
    expect(rows.find((c) => c.name === "model")).toEqual({
      name: "model",
      description: "Switch the AI model and save it as your default for new sessions",
      hint: "[model]",
    });
    expect(rows.find((c) => c.name === "compact")?.hint).toBe("[instructions]");
    expect(rows.find((c) => c.name === "cost")?.hint).toBeUndefined();
  });
});

describe("Eitri slash actions", () => {
  it("the_five_words_map_to_eitri", () => {
    expect(slashAction("/help")).toEqual({ kind: "keymap" });
    expect(slashAction("/status")).toEqual({ kind: "tab-info" });
    expect(slashAction("/resume")).toEqual({ kind: "sessions" });
    expect(slashAction("/memory")).toEqual({ kind: "open-path", path: "CLAUDE.md" });
    expect(slashAction("/rewind")).toEqual({ kind: "pointer", text: "/rewind is not in Eitri — c reviews and reverts this turn's edits" });
    for (const text of ["/help me", "/statusbar", "/resume yesterday", "hello", ""]) expect(slashAction(text)).toBeNull();
    expect(slashAction("  /status ")).toEqual({ kind: "tab-info" });
  });

  it("the_completion_menu_still_lists_them", () => {
    const names = builtInCompletions().map((row) => row.name);
    for (const name of ["help", "status", "resume", "memory", "rewind"]) expect(names).toContain(name);
    expect(completionSource("claude", [{ name: "resume", description: "Resume" }])).toHaveLength(1);
  });
});
