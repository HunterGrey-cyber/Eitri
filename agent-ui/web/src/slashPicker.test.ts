import { describe, expect, it } from "vitest";
import { parseEffortReply, parseModelReply } from "./slashPicker";

// Verbatim probe text, CLI 2.1.283 under (owner trial item 2, 2026-09-28's probe
// section of the private review notes).
const MODEL_REPLY =
  "Current model: `Haiku 4.5` (effort: high)\n" +
  "Usage: /model <name>. Available: sonnet, opus, haiku, fable, best, sonnet[1m], opus[1m], " +
  "fable[1m], opusplan, default, or a full model ID.";

const EFFORT_USAGE_REPLY = "Usage: /effort <low|medium|high|xhigh|max|auto>";
const EFFORT_ERROR_REPLY = "Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max, auto";

describe("parseModelReply (owner trial item 2)", () => {
  // "Current model: `Haiku 4.5`" names the CLI's display name, not one of the /model <name> slugs
  // in `options` -- `current` must resolve to the matching SLUG ("haiku"), not the raw display
  // text, so `SlashPicker`'s own `option === current` marking and cursor-seeding find it.
  it("parses the verbatim probe reply: current model resolved to its slug, every option in order", () => {
    expect(parseModelReply(MODEL_REPLY)).toEqual({
      current: "haiku",
      options: ["sonnet", "opus", "haiku", "fable", "best", "sonnet[1m]", "opus[1m]", "fable[1m]", "opusplan", "default"],
    });
  });

  it("resolves the display name to its slug case-insensitively, for every option", () => {
    const text = (name: string) =>
      `Current model: \`${name}\`\nUsage: /model <name>. Available: sonnet, opus, haiku, or a full model ID.`;
    expect(parseModelReply(text("Sonnet 4.5"))?.current).toBe("sonnet");
    expect(parseModelReply(text("Opus 4.1"))?.current).toBe("opus");
    expect(parseModelReply(text("HAIKU 4.5"))?.current).toBe("haiku");
  });

  it("leaves current null when the display name matches no known option", () => {
    const text =
      "Current model: `Some Future Model`\nUsage: /model <name>. Available: sonnet, opus, or a full model ID.";
    expect(parseModelReply(text)?.current).toBeNull();
  });

  it("parses a reply naming no current model (an older CLI's shorter line)", () => {
    const text = "Usage: /model <name>. Available: sonnet, opus, or a full model ID.";
    expect(parseModelReply(text)).toEqual({ current: null, options: ["sonnet", "opus"] });
  });

  it("parses an extra model the probe never saw -- never a hard-coded list", () => {
    const text =
      "Current model: `Opus 4.1` (effort: high)\n" +
      "Usage: /model <name>. Available: sonnet, opus, haiku, brandnew, or a full model ID.";
    expect(parseModelReply(text)).toEqual({
      current: "opus",
      options: ["sonnet", "opus", "haiku", "brandnew"],
    });
  });

  it("is null for text that does not match this shape at all (a different CLI version)", () => {
    expect(parseModelReply("I don't understand that command.")).toBeNull();
    expect(parseModelReply("")).toBeNull();
    // The /model error reply's own shape (an unparseable "Available" list) must not fabricate a
    // picker with no real options in it.
    expect(parseModelReply("Available: , or a full model ID.")).toBeNull();
  });
});

describe("parseEffortReply (owner trial item 2)", () => {
  it("parses the verbatim probe reply's level list", () => {
    expect(parseEffortReply(EFFORT_USAGE_REPLY)).toEqual({
      options: ["low", "medium", "high", "xhigh", "max", "auto"],
    });
  });

  it("is null for the error reply (not the bare command's own usage text)", () => {
    expect(parseEffortReply(EFFORT_ERROR_REPLY)).toBeNull();
  });

  it("is null for text that does not match this shape at all", () => {
    expect(parseEffortReply("I don't understand that command.")).toBeNull();
    expect(parseEffortReply("")).toBeNull();
  });
});
